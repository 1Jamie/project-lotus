//! Embedded nested Wayland sub-compositor for Lotus (nest).

pub mod dmabuf;
pub mod server;

use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use crossbeam_channel::{unbounded, Receiver, Sender};
use log::{debug, error, info, warn};
use parking_lot::Mutex as ParkingMutex;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode};
use smithay::reexports::wayland_server::ListeningSocket;
use smithay::reexports::wayland_server::Display;
use smithay::xwayland::{XWayland, XWaylandEvent};

pub use dmabuf::{DmabufFrameDescriptor, EglDmaBufImporter, ImportedTexture, GL_TEXTURE_EXTERNAL_OES};
pub use server::{LotusWaylandState, NestedCursor, WaylandEvent};

pub enum WaylandCommand {
    FrameDone { timestamp_ms: u32 },
    ReleasePreviousBuffer,
    Resize { width: u32, height: u32 },
    /// Update nested wl_output Mode.refresh (millihertz) to match host monitor.
    SetOutputRefresh { millihertz: u32 },
    ResizePane { pane_id: String, width: u32, height: u32 },
    SetPaneVisible { pane_id: String, visible: bool },
    KeyboardKey { keycode: u32, pressed: bool },
    PointerMotion { x: f64, y: f64 },
    PointerMotionForPane { pane_id: String, x: f64, y: f64 },
    /// Raw relative deltas (mouse lock / FPS aiming). Forwarded via wp_relative_pointer.
    PointerRelativeMotion { dx: f64, dy: f64 },
    PointerButton { button: u32, pressed: bool },
    PointerButtonForPane {
        pane_id: String,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
    },
    PointerAxis { horizontal: f64, vertical: f64 },
    PointerAxisForPane {
        pane_id: String,
        x: f64,
        y: f64,
        horizontal: f64,
        vertical: f64,
    },
    KeyboardKeyForPane {
        pane_id: String,
        keycode: u32,
        pressed: bool,
    },
    SetFocusSurface { surface_id: u32 },
    /// Drop seat keyboard focus (game-mode: overlay apps must look unfocused).
    ClearKeyboardFocus,
    /// Drop pointer focus so overlay clients get wl_pointer.leave (Electron blur).
    ClearPointerFocus,
    /// xdg_toplevel Activated / X11 set_activated for a pane's surfaces.
    SetPaneActivated { pane_id: String, activated: bool },
    /// Host atrium game-mode flag — nested clients must not steal Activated while true.
    SetHostGameMode { enabled: bool },
    AddPaneListener { pane_id: String, socket: ListeningSocket },
    Shutdown,
}

#[derive(Clone)]
pub struct PaneRegistration {
    pub pane_id: String,
    pub is_primary: bool,
    pub target_size: (u32, u32),
    /// Last size we pushed via xdg_toplevel configure (skip redundant resizes).
    pub last_configured_size: Arc<ParkingMutex<(u32, u32)>>,
    pub slot: Arc<ParkingMutex<ExternalSurfaceSlot>>,
    pub child_pid: Option<u32>,
    pub surface_id: Arc<AtomicU32>,
    pub active_surface_id: Arc<AtomicU32>,
    pub shm_texture: Arc<ParkingMutex<Option<glow::Texture>>>,
}

/// Mailbox slot holding the latest committed game frame.
/// Decouples game frametimes from Lotus DOM layout transactions.
pub struct ExternalSurfaceSlot {
    pub current_texture: Option<glow::Texture>,
    pub target: u32,
    pub is_external_oes: bool,
    pub width: u32,
    pub height: u32,
    pub y_inverted: bool,
    pub frame_counter: u64,
    pub last_frame_time_ms: f32,
    pub current_fps: f32,
    pub rolling_frametimes: Vec<f32>,
    pub last_frame_instant: Option<std::time::Instant>,
    pub stats_updated: bool,
}

impl Default for ExternalSurfaceSlot {
    fn default() -> Self {
        Self {
            current_texture: None,
            target: glow::TEXTURE_2D,
            is_external_oes: false,
            width: 0,
            height: 0,
            y_inverted: false,
            frame_counter: 0,
            last_frame_time_ms: 0.0,
            current_fps: 0.0,
            rolling_frametimes: Vec::new(),
            last_frame_instant: None,
            stats_updated: false,
        }
    }
}

pub struct LotusWaylandServer {
    pub socket_name: String,
    pub socket_path: Option<PathBuf>,
    pub runtime_dir: PathBuf,
    /// Host atrium window id — used to CloseWindow when the primary game exits.
    pub window_id: String,
    /// Fixed nested wl_output / X11 screen size (game render resolution).
    /// Pane layout size is independent — we letterbox this buffer into the pane.
    pub output_size: (u32, u32),
    /// Host monitor refresh in millihertz — drives nested wl_output + present pacing.
    pub refresh_rate_millihertz: Arc<AtomicU32>,
    pub x11_display: Arc<AtomicU32>,
    pub event_receiver: Receiver<WaylandEvent>,
    pub command_sender: Sender<WaylandCommand>,
    pub importer: EglDmaBufImporter,
    pub slot: Arc<ParkingMutex<ExternalSurfaceSlot>>,
    pub shm_texture: Option<glow::Texture>,
    pub panes: Arc<ParkingMutex<Vec<PaneRegistration>>>,
    pub surface_to_pane: Arc<ParkingMutex<std::collections::HashMap<u32, String>>>,
    /// True once we have presented a real primary/game frame (not just Steam helpers).
    primary_ever_presented: Arc<AtomicBool>,
    /// When primary surfaces last went empty after a real present (debounce exit).
    primary_empty_since: ParkingMutex<Option<std::time::Instant>>,
    /// CloseWindow already queued for primary-game-gone.
    primary_exit_requested: Arc<AtomicBool>,
    thread_handle: Option<thread::JoinHandle<()>>,
}

/// Gamescope-style nested render size. Defaults 1920×1080; override with
/// `ATRIUM_GAME_WIDTH` / `ATRIUM_GAME_HEIGHT`.
pub fn nest_game_output_size() -> (u32, u32) {
    let w = std::env::var("ATRIUM_GAME_WIDTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1920);
    let h = std::env::var("ATRIUM_GAME_HEIGHT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1080);
    (w.max(1), h.max(1))
}

/// Client buffer size for overlay apps (Discord, …).
///
/// Not the dock rect and not a hard-coded WxH. Mirrors `rightDrawerRect` against a
/// ≥1080p reference canvas so a 900p game still gets a hi-res Discord buffer that
/// the host contain-scales into the smaller dock. Override with
/// `ATRIUM_OVERLAY_WIDTH` / `ATRIUM_OVERLAY_HEIGHT` when both are set.
pub fn nest_overlay_client_size() -> (u32, u32) {
    let env_w = std::env::var("ATRIUM_OVERLAY_WIDTH")
        .ok()
        .and_then(|s| s.parse::<u32>().ok());
    let env_h = std::env::var("ATRIUM_OVERLAY_HEIGHT")
        .ok()
        .and_then(|s| s.parse::<u32>().ok());
    if let (Some(w), Some(h)) = (env_w, env_h) {
        return (w.max(1), h.max(1));
    }

    // Same drawer policy as lib/atrium/bounds.js `rightDrawerRect`.
    const TOP: u32 = 56;
    const MAX_WIDTH: u32 = 900;
    const MIN_WIDTH: u32 = 280;
    const FRACTION: f32 = 0.38;

    let (game_w, game_h) = nest_game_output_size();
    // Always size against at least a 1080p canvas so 900p games don't shrink Discord.
    let ref_w = game_w.max(1920);
    let ref_h = game_h.max(1080);
    let preferred = ((ref_w as f32) * FRACTION).floor() as u32;
    let width = preferred
        .min(MAX_WIDTH)
        .min(ref_w)
        .max(MIN_WIDTH.min(ref_w))
        .max(1);
    let height = ref_h.saturating_sub(TOP).max(1);
    (env_w.unwrap_or(width).max(1), env_h.unwrap_or(height).max(1))
}

impl Drop for LotusWaylandServer {
    fn drop(&mut self) {
        info!("[lotus-nest] tearing down nested Wayland session");
        let _ = self.command_sender.send(WaylandCommand::Shutdown);
        if let Some(handle) = self.thread_handle.take() {
            // Xwayland/clients should exit once the socket closes; don't hang forever.
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = handle.join();
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
        // Host-runtime symlink created for Proton (wayland-nest-<session>).
        if let Ok(host_runtime) = std::env::var("XDG_RUNTIME_DIR") {
            if let Some(name) = self.runtime_dir.file_name().and_then(|s| s.to_str()) {
                let link = PathBuf::from(host_runtime).join(format!("wayland-nest-{}", name));
                let _ = std::fs::remove_file(&link);
            }
        }
        if self.runtime_dir.exists() {
            let _ = std::fs::remove_dir_all(&self.runtime_dir);
        }
    }
}

/// Apply coalesced absolute + relative pointer cmds. Absolute uses the latest sample;
/// relative sums deltas so aim distance is preserved across a drain batch.
fn flush_coalesced_pointer(
    state: &mut server::LotusWaylandState,
    pending_motion_global: &mut Option<(f64, f64)>,
    pending_motion_pane: &mut Option<(String, f64, f64)>,
    pending_rel_dx: &mut f64,
    pending_rel_dy: &mut f64,
) {
    if let Some((x, y)) = pending_motion_global.take() {
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let now_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            & 0xFFFFFFFF) as u32;
        // Stamp before motion — cursor_image must not re-enter PointerHandle.
        state.last_pointer_focus = state.focused_surface.clone();
        if let Some(pointer) = state.seat.get_pointer() {
            let location = (x, y).into();
            let focus = state
                .focused_surface
                .clone()
                .map(|s| (s, (0.0, 0.0).into()));
            pointer.motion(
                state,
                focus,
                &smithay::input::pointer::MotionEvent {
                    location,
                    serial,
                    time: now_ms,
                },
            );
            pointer.frame(state);
        }
    }
    if let Some((pane_id, x, y)) = pending_motion_pane.take() {
        state.dispatch_pointer_motion_to_pane(&pane_id, x, y);
    }
    if *pending_rel_dx != 0.0 || *pending_rel_dy != 0.0 {
        let dx = std::mem::take(pending_rel_dx);
        let dy = std::mem::take(pending_rel_dy);
        let now_us = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros()
            & 0xFFFFFFFFFFFFFFFF) as u64;
        if let Some(pointer) = state.seat.get_pointer() {
            // Prefer last pointer focus (enter target) over keyboard-sticky focused_surface.
            let focus = state
                .last_pointer_focus
                .clone()
                .or_else(|| state.focused_surface.clone())
                .map(|s| (s, (0.0, 0.0).into()));
            pointer.relative_motion(
                state,
                focus,
                &smithay::input::pointer::RelativeMotionEvent {
                    delta: (dx, dy).into(),
                    delta_unaccel: (dx, dy).into(),
                    utime: now_us,
                },
            );
            pointer.frame(state);
        }
    }
}

impl LotusWaylandServer {
    /// Start the nested Wayland sub-compositor and Xwayland server in a background thread.
    ///
    /// `target_width`×`target_height` is the **virtual output** size the game renders at
    /// (like gamescope `-W -H`), not the lotus pane/window size.
    /// `refresh_rate_millihertz` should match the host monitor (e.g. 60000 for 60 Hz).
    pub fn start(
        window_id: String,
        target_width: u32,
        target_height: u32,
        refresh_rate_millihertz: u32,
    ) -> Result<Self, String> {
        let (event_tx, event_rx) = unbounded::<WaylandEvent>();
        let (cmd_tx, cmd_rx) = unbounded::<WaylandCommand>();
        let output_size = (target_width.max(1), target_height.max(1));
        let refresh_rate_millihertz = refresh_rate_millihertz.max(1);
        info!(
            "[lotus-nest] nested output (game render size) {}x{} @ {:.0}Hz",
            output_size.0,
            output_size.1,
            refresh_rate_millihertz as f32 / 1000.0
        );

        // Isolated hermetic runtime directory for this nested sub-compositor session
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let session_token = format!("{}-{}", std::process::id(), now_ns);
        let runtime_dir = std::env::temp_dir().join(format!("lotus-nest-{}", session_token));
        let _ = std::fs::create_dir_all(&runtime_dir);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700));
        }

        // Symlink audio / IPC sockets from host XDG_RUNTIME_DIR if present so child apps have audio
        if let Ok(host_runtime) = std::env::var("XDG_RUNTIME_DIR") {
            let host_dir = PathBuf::from(host_runtime);
            let host_pw = host_dir.join("pipewire-0");
            if host_pw.exists() {
                let _ = std::os::unix::fs::symlink(&host_pw, runtime_dir.join("pipewire-0"));
            }
            let host_pulse = host_dir.join("pulse");
            if host_pulse.exists() {
                let _ = std::os::unix::fs::symlink(&host_pulse, runtime_dir.join("pulse"));
            }
        }

        let socket_path = runtime_dir.join("wayland-0");
        let socket = ListeningSocket::bind_absolute(socket_path.clone())
            .map_err(|e| format!("Failed to bind isolated Wayland socket at {:?}: {:?}", socket_path, e))?;

        let socket_name = "wayland-0".to_string();

        let importer = EglDmaBufImporter::new()
            .map_err(|e| format!("Failed to initialize EglDmaBufImporter: {}", e))?;

        let x11_display = Arc::new(AtomicU32::new(u32::MAX));
        let x11_display_thread = x11_display.clone();
        let refresh_rate = Arc::new(AtomicU32::new(refresh_rate_millihertz));
        let slot = Arc::new(ParkingMutex::new(ExternalSurfaceSlot::default()));

        let panes = Arc::new(ParkingMutex::new(Vec::new()));
        let panes_thread = panes.clone();
        let surface_to_pane = Arc::new(ParkingMutex::new(std::collections::HashMap::new()));
        let surface_to_pane_thread = surface_to_pane.clone();

        info!("[lotus-nest] nested Wayland listening on {:?} (WAYLAND_DISPLAY={})", socket_path, socket_name);

        let host_window_id = window_id.clone();
        let thread_handle = thread::Builder::new()
            .name("lotus-wayland-server".to_string())
            .spawn(move || {
                Self::run_server_loop(
                    window_id,
                    socket,
                    event_tx,
                    cmd_rx,
                    x11_display_thread,
                    output_size,
                    refresh_rate_millihertz,
                    panes_thread,
                    surface_to_pane_thread,
                );
            })
            .map_err(|e| format!("Failed to spawn Wayland server thread: {}", e))?;

        Ok(Self {
            socket_name,
            socket_path: Some(socket_path),
            runtime_dir,
            window_id: host_window_id,
            output_size,
            refresh_rate_millihertz: refresh_rate,
            x11_display,
            event_receiver: event_rx,
            command_sender: cmd_tx,
            importer,
            slot,
            shm_texture: None,
            panes,
            surface_to_pane,
            primary_ever_presented: Arc::new(AtomicBool::new(false)),
            primary_empty_since: ParkingMutex::new(None),
            primary_exit_requested: Arc::new(AtomicBool::new(false)),
            thread_handle: Some(thread_handle),
        })
    }

    fn run_server_loop(
        window_id: String,
        socket: ListeningSocket,
        event_sender: Sender<WaylandEvent>,
        cmd_receiver: Receiver<WaylandCommand>,
        x11_display: Arc<AtomicU32>,
        target_size: (u32, u32),
        refresh_rate_millihertz: u32,
        pane_registrations: Arc<ParkingMutex<Vec<PaneRegistration>>>,
        surface_to_pane: Arc<ParkingMutex<std::collections::HashMap<u32, String>>>,
    ) {
        // Init smithay's SHM drop thread *before* Proton/Discord exhaust the Game Mode
        // cgroup PID budget (otherwise Pool LazyLock panics and nested Xwayland dies).
        warm_smithay_shm_drop_thread();

        let mut display = Display::<LotusWaylandState>::new()
            .expect("Failed to create Wayland Display");
        let display_handle = display.handle();

        let mut state = LotusWaylandState::new(
            window_id,
            display_handle.clone(),
            event_sender.clone(),
            target_size,
            refresh_rate_millihertz,
            pane_registrations,
            surface_to_pane,
        );

        let mut event_loop: EventLoop<LotusWaylandState> = EventLoop::try_new()
            .expect("Failed to create calloop EventLoop");

        let loop_handle = event_loop.handle();

        // 1. Register incoming Wayland connection listener
        let raw_socket_fd = socket.as_raw_fd();
        let socket_source = Generic::new(
            unsafe { smithay::reexports::calloop::generic::FdWrapper::new(raw_socket_fd) },
            Interest::READ,
            Mode::Level,
        );
        loop_handle
            .insert_source(socket_source, move |_event, _fd, state| {
                if let Ok(Some(stream)) = socket.accept() {
                    debug!("[lotus-nest] accepted new client connection");
                    let _ = state.display_handle.insert_client(stream, Arc::new(server::ClientState::default()));
                    let _ = state.event_sender.send(WaylandEvent::ClientConnected);
                }
                Ok(smithay::reexports::calloop::PostAction::Continue)
            })
            .expect("Failed to insert Wayland socket into event loop");

        // 2. Launch nested Xwayland server for Proton/Wine/X11 compatibility
        let mut _xwayland_client = None;
        // smithay env_clear()s then sets WAYLAND_SOCKET. Do NOT set WAYLAND_DISPLAY=""
        // afterward — an empty value can make Xwayland ignore WAYLAND_SOCKET and attach
        // to the host/gamescope socket under XDG_RUNTIME_DIR (wrong display).
        let xwayland_log = std::fs::File::create("/tmp/lotus-nest-xwayland.log")
            .map(|f| {
                f.try_clone()
                    .map(|err| (Stdio::from(f), Stdio::from(err)))
                    .unwrap_or_else(|_| (Stdio::null(), Stdio::null()))
            })
            .unwrap_or_else(|_| (Stdio::null(), Stdio::null()));
        match XWayland::spawn(
            &display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            true,
            xwayland_log.0,
            xwayland_log.1,
            |_| (),
        ) {
            Ok((xwayland, client)) => {
                let client_clone = client.clone();
                let loop_handle_clone = loop_handle.clone();
                _xwayland_client = Some(client);
                let x11_display_ev = x11_display.clone();
                let event_tx_clone = event_sender.clone();
                if let Err(e) = loop_handle.insert_source(xwayland, move |event, _, state| {
                    match event {
                        XWaylandEvent::Ready { x11_socket, display_number } => {
                            info!("[lotus-nest] nested Xwayland ready on DISPLAY=:{}", display_number);
                            x11_display_ev.store(display_number, Ordering::SeqCst);
                            let _ = event_tx_clone.send(WaylandEvent::XwaylandReady { display_number });

                            match smithay::xwayland::xwm::X11Wm::start_wm(
                                loop_handle_clone.clone(),
                                x11_socket,
                                client_clone.clone(),
                            ) {
                                Ok(wm) => {
                                    info!("[lotus-nest] attached X11 window manager to nested Xwayland");
                                    state.xwm = Some(wm);
                                }
                                Err(e) => {
                                    warn!("[lotus-nest] failed to attach X11Wm to nested Xwayland: {:?}", e);
                                }
                            }
                        }
                        XWaylandEvent::Error => {
                            warn!("[lotus-nest] Xwayland exited unexpectedly");
                        }
                    }
                }) {
                    warn!("[lotus-nest] failed to insert Xwayland source into event loop: {:?}", e);
                }
            }
            Err(e) => {
                warn!("[lotus-nest] Xwayland unavailable or failed to launch: {:?}", e);
            }
        }

        info!("[lotus-nest] Wayland event loop running");

        // Nested clients pace via host present → FrameDone only. A stalled-host
        // watchdog (below) covers gamescope suppressing exposes so Xwayland does
        // not deadlock waiting forever for wl_callback.done.
        let mut last_host_frame_done = std::time::Instant::now();

        loop {
            // Coalesce pointer motion across a drain batch so a 1kHz mouse cannot
            // backlog the unbounded cmd channel (jello / delayed FrameDone).
            let mut pending_rel_dx = 0.0f64;
            let mut pending_rel_dy = 0.0f64;
            let mut pending_motion_pane: Option<(String, f64, f64)> = None;
            let mut pending_motion_global: Option<(f64, f64)> = None;

            // Process incoming commands from the host compositor thread
            while let Ok(cmd) = cmd_receiver.try_recv() {
                match cmd {
                    WaylandCommand::PointerMotion { x, y } => {
                        pending_motion_global = Some((x, y));
                        continue;
                    }
                    WaylandCommand::PointerMotionForPane { pane_id, x, y } => {
                        pending_motion_pane = Some((pane_id, x, y));
                        continue;
                    }
                    WaylandCommand::PointerRelativeMotion { dx, dy } => {
                        pending_rel_dx += dx;
                        pending_rel_dy += dy;
                        continue;
                    }
                    other => {
                        flush_coalesced_pointer(
                            &mut state,
                            &mut pending_motion_global,
                            &mut pending_motion_pane,
                            &mut pending_rel_dx,
                            &mut pending_rel_dy,
                        );
                        match other {
                    WaylandCommand::FrameDone { timestamp_ms } => {
                        state.send_frame_callbacks(timestamp_ms);
                        state.send_presentation_feedback(timestamp_ms);
                        last_host_frame_done = std::time::Instant::now();
                    }
                    WaylandCommand::ReleasePreviousBuffer => {
                        state.previous_dmabuf = None;
                    }
                    WaylandCommand::SetOutputRefresh { millihertz } => {
                        state.set_output_refresh(millihertz);
                    }
                    WaylandCommand::Resize { width, height } => {
                        state.target_size = (width, height);
                        if let Some(ref toplevel) = state.toplevel {
                            toplevel.with_pending_state(|s| {
                                s.size = Some((width as i32, height as i32).into());
                            });
                            toplevel.send_configure();
                        }
                    }
                    WaylandCommand::ResizePane { pane_id, width, height } => {
                        let mut should_configure = true;
                        {
                            let mut panes = state.pane_registrations.lock();
                            if let Some(p) = panes.iter_mut().find(|p| p.pane_id == pane_id) {
                                p.target_size = (width, height);
                                let mut last = p.last_configured_size.lock();
                                if *last == (width, height) {
                                    should_configure = false;
                                } else {
                                    *last = (width, height);
                                }
                            }
                        }
                        if should_configure {
                            state.reconfigure_pane_size(&pane_id, width, height);
                        }
                    }
                    WaylandCommand::SetPaneVisible { pane_id, visible } => {
                        state.set_pane_visible(&pane_id, visible);
                    }
                    WaylandCommand::SetHostGameMode { enabled } => {
                        state.host_game_mode = enabled;
                        let overlay_ids: Vec<String> = {
                            let panes = state.pane_registrations.lock();
                            panes
                                .iter()
                                .filter(|p| !p.is_primary)
                                .map(|p| p.pane_id.clone())
                                .collect()
                        };
                        if enabled {
                            // Deactivate overlays so Discord emits session-bus Notify.
                            for id in &overlay_ids {
                                state.set_pane_activated(id, false);
                            }
                            state.clear_keyboard_focus();
                            state.clear_pointer_focus();
                        } else {
                            // Leaving game mode: clear Suspended so Chromium can paint again.
                            // App activates visible overlays; we only unstick Suspended here.
                            for id in &overlay_ids {
                                state.unsuspend_pane(id);
                            }
                        }
                    }
                    WaylandCommand::AddPaneListener { pane_id, socket } => {
                        let raw_fd = socket.as_raw_fd();
                        let socket_source = Generic::new(
                            unsafe { smithay::reexports::calloop::generic::FdWrapper::new(raw_fd) },
                            Interest::READ,
                            Mode::Level,
                        );
                        let pane_id_clone = pane_id.clone();
                        let _ = loop_handle.insert_source(socket_source, move |_event, _fd, state| {
                            if let Ok(Some(stream)) = socket.accept() {
                                debug!("[lotus-nest] accepted client on dedicated socket for pane '{}'", pane_id_clone);
                                let _ = state.display_handle.insert_client(
                                    stream,
                                    Arc::new(server::ClientState {
                                        compositor_state: smithay::wayland::compositor::CompositorClientState::default(),
                                        pane_id: Some(pane_id_clone.clone()),
                                    }),
                                );
                                let _ = state.event_sender.send(WaylandEvent::ClientConnected);
                            }
                            Ok(smithay::reexports::calloop::PostAction::Continue)
                        });
                    }
                    WaylandCommand::KeyboardKey { keycode, pressed } => {
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        let now_ms = (std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() & 0xFFFFFFFF) as u32;
                        if let Some(keyboard) = state.seat.get_keyboard() {
                            let xkb_keycode = smithay::input::keyboard::Keycode::new(keycode);
                            let key_state = if pressed {
                                smithay::backend::input::KeyState::Pressed
                            } else {
                                smithay::backend::input::KeyState::Released
                            };
                            keyboard.input(
                                &mut state,
                                xkb_keycode,
                                key_state,
                                serial,
                                now_ms,
                                |_, _, _| smithay::input::keyboard::FilterResult::<()>::Forward,
                            );
                        }
                    }
                    WaylandCommand::PointerButton { button, pressed } => {
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        let now_ms = (std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() & 0xFFFFFFFF) as u32;
                        if let Some(pointer) = state.seat.get_pointer() {
                            pointer.button(
                                &mut state,
                                &smithay::input::pointer::ButtonEvent {
                                    serial,
                                    time: now_ms,
                                    button,
                                    state: if pressed {
                                        smithay::backend::input::ButtonState::Pressed
                                    } else {
                                        smithay::backend::input::ButtonState::Released
                                    },
                                },
                            );
                            pointer.frame(&mut state);
                        }
                    }
                    WaylandCommand::PointerButtonForPane {
                        pane_id,
                        x,
                        y,
                        button,
                        pressed,
                    } => {
                        state.dispatch_pointer_button_to_pane(&pane_id, x, y, button, pressed);
                    }
                    WaylandCommand::PointerAxis { horizontal, vertical } => {
                        let now_ms = (std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() & 0xFFFFFFFF) as u32;
                        if let Some(pointer) = state.seat.get_pointer() {
                            let mut frame = smithay::input::pointer::AxisFrame::new(now_ms)
                                .source(smithay::backend::input::AxisSource::Wheel);
                            if horizontal.abs() > 0.0 {
                                frame = frame.value(smithay::backend::input::Axis::Horizontal, horizontal);
                            }
                            if vertical.abs() > 0.0 {
                                frame = frame.value(smithay::backend::input::Axis::Vertical, vertical);
                            }
                            pointer.axis(&mut state, frame);
                            pointer.frame(&mut state);
                        }
                    }
                    WaylandCommand::PointerAxisForPane {
                        pane_id,
                        x,
                        y,
                        horizontal,
                        vertical,
                    } => {
                        state.dispatch_pointer_axis_to_pane(&pane_id, x, y, horizontal, vertical);
                    }
                    WaylandCommand::KeyboardKeyForPane {
                        pane_id,
                        keycode,
                        pressed,
                    } => {
                        state.dispatch_keyboard_to_pane(&pane_id, keycode, pressed);
                    }
                    WaylandCommand::SetFocusSurface { surface_id } => {
                        if let Some(surf) = state.known_surfaces.get(&surface_id) {
                            state.set_focus_surface(Some(surf.clone()));
                        }
                    }
                    WaylandCommand::ClearKeyboardFocus => {
                        state.clear_keyboard_focus();
                    }
                    WaylandCommand::ClearPointerFocus => {
                        state.clear_pointer_focus();
                    }
                    WaylandCommand::SetPaneActivated { pane_id, activated } => {
                        state.set_pane_activated(&pane_id, activated);
                    }
                    WaylandCommand::Shutdown => {
                        info!("[lotus-nest] shutting down Wayland event loop");
                        return;
                    }
                    // Motion arms are drained above into coalesced pending.
                    WaylandCommand::PointerMotion { .. }
                    | WaylandCommand::PointerMotionForPane { .. }
                    | WaylandCommand::PointerRelativeMotion { .. } => unreachable!(),
                        }
                    }
                }
            }
            flush_coalesced_pointer(
                &mut state,
                &mut pending_motion_global,
                &mut pending_motion_pane,
                &mut pending_rel_dx,
                &mut pending_rel_dy,
            );

            // Dispatch incoming requests from clients and flush outgoing events
            let _ = display.dispatch_clients(&mut state);
            let _ = display.flush_clients();

            // Dispatch calloop events with a short timeout to handle commands responsively
            let dispatch_res = event_loop.dispatch(Some(Duration::from_millis(1)), &mut state);
            if let Err(e) = dispatch_res {
                error!("[lotus-nest] event loop dispatch error: {:?}", e);
            }

            // Stalled-host watchdog only — do NOT ack frames on a free-running 8ms
            // clock (that raced nested clients to ~125Hz then stalled at 100ms+).
            let refresh_mhz = state.refresh_rate_millihertz.max(1);
            let period = Duration::from_nanos(1_000_000_000_000u64 / refresh_mhz as u64);
            let stall_limit = period.saturating_mul(2).max(Duration::from_millis(32));
            if last_host_frame_done.elapsed() >= stall_limit {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u32)
                    .unwrap_or(0);
                state.send_frame_callbacks(ms);
                state.send_presentation_feedback(ms);
                // Drop the previous DMA-BUF so double-buffered Xwayland can recycle
                // when gamescope stops delivering host RedrawRequested.
                state.previous_dmabuf = None;
                let _ = display.flush_clients();
                last_host_frame_done = std::time::Instant::now();
            }
        }
    }

    pub fn register_pane(
        &self,
        pane_id: String,
        is_primary: bool,
        target_size: (u32, u32),
        slot: Arc<ParkingMutex<ExternalSurfaceSlot>>,
        child_pid: Option<u32>,
    ) -> String {
        // sun_path is ~108 bytes; colons confuse Flatpak/XDG WAYLAND_DISPLAY parsers.
        // Sanitize plugin:<id>:<local> → plugin_<id>_<local> for the socket filename only.
        let socket_name = if is_primary {
            "wayland-0".to_string()
        } else {
            let safe: String = pane_id
                .chars()
                .map(|c| if c == ':' || c == '/' { '_' } else { c })
                .collect();
            format!("wayland-{}", safe)
        };
        let socket_path = self.runtime_dir.join(&socket_name);
        if !is_primary {
            match ListeningSocket::bind_absolute(socket_path.clone()) {
                Ok(socket) => {
                    info!("[lotus-nest] bound dedicated socket for pane '{}' at {:?}", pane_id, socket_path);
                    let _ = self.command_sender.send(WaylandCommand::AddPaneListener {
                        pane_id: pane_id.clone(),
                        socket,
                    });
                }
                Err(e) => {
                    warn!("[lotus-nest] failed to bind dedicated socket for pane '{}' ({:?}), using default", pane_id, e);
                }
            }
        }

        // Primary/game → fixed nested output. Overlays → fixed high-res client size
        // (not the dock); host cover-scales the buffer into the dock on composite.
        let effective_size = if is_primary {
            self.output_size
        } else {
            let overlay = nest_overlay_client_size();
            if target_size != overlay {
                info!(
                    "[lotus-nest] overlay '{}' ignoring dock hint {}x{}; client stays {}x{}",
                    pane_id, target_size.0, target_size.1, overlay.0, overlay.1
                );
            }
            overlay
        };
        info!(
            "[lotus-nest] register pane '{}' primary={} client_size={}x{} (pane hint was {}x{})",
            pane_id, is_primary, effective_size.0, effective_size.1, target_size.0, target_size.1
        );

        let mut panes = self.panes.lock();
        panes.push(PaneRegistration {
            pane_id,
            is_primary,
            target_size: effective_size,
            last_configured_size: Arc::new(ParkingMutex::new((0, 0))),
            slot,
            child_pid,
            surface_id: Arc::new(AtomicU32::new(u32::MAX)),
            active_surface_id: Arc::new(AtomicU32::new(u32::MAX)),
            shm_texture: Arc::new(ParkingMutex::new(None)),
        });

        socket_name
    }

    pub fn send_resize_pane(&self, pane_id: &str, _width: u32, _height: u32) {
        // Nested clients keep a fixed client size (game: output_size, overlays:
        // nest_overlay_client_size). Dock/pane layout changes are composite-only —
        // never push xdg configures from lotus layout ticks (squashes Discord/Electron).
        let _ = pane_id;
    }

    pub fn send_pane_visible(&self, pane_id: &str, visible: bool) {
        let _ = self.command_sender.send(WaylandCommand::SetPaneVisible {
            pane_id: pane_id.to_string(),
            visible,
        });
    }

    pub fn update_pane_pid(&self, pane_id: &str, child_pid: u32) {
        let mut panes = self.panes.lock();
        if let Some(p) = panes.iter_mut().find(|p| p.pane_id == pane_id) {
            p.child_pid = Some(child_pid);
        }
    }

    pub fn focus_pane(&self, pane_id: &str) {
        let panes = self.panes.lock();
        if let Some(p) = panes.iter().find(|p| p.pane_id == pane_id) {
            let sid = p.active_surface_id.load(Ordering::SeqCst);
            let sid = if sid != u32::MAX {
                sid
            } else {
                p.surface_id.load(Ordering::SeqCst)
            };
            if sid != u32::MAX {
                let _ = self.command_sender.send(WaylandCommand::SetFocusSurface { surface_id: sid });
            }
        }
        let _ = self.command_sender.send(WaylandCommand::SetPaneActivated {
            pane_id: pane_id.to_string(),
            activated: true,
        });
    }

    pub fn clear_keyboard_focus(&self) {
        let _ = self.command_sender.send(WaylandCommand::ClearKeyboardFocus);
    }

    pub fn clear_pointer_focus(&self) {
        let _ = self.command_sender.send(WaylandCommand::ClearPointerFocus);
    }

    pub fn set_host_game_mode(&self, enabled: bool) {
        let _ = self.command_sender.send(WaylandCommand::SetHostGameMode { enabled });
    }

    pub fn set_pane_activated(&self, pane_id: &str, activated: bool) {
        let _ = self.command_sender.send(WaylandCommand::SetPaneActivated {
            pane_id: pane_id.to_string(),
            activated,
        });
    }

    pub fn focus_primary_pane(&self) -> Option<String> {
        let panes = self.panes.lock();
        if let Some(p) = panes.iter().find(|p| p.is_primary) {
            let sid = p.active_surface_id.load(Ordering::SeqCst);
            let sid = if sid != u32::MAX {
                sid
            } else {
                p.surface_id.load(Ordering::SeqCst)
            };
            if sid != u32::MAX {
                let _ = self.command_sender.send(WaylandCommand::SetFocusSurface { surface_id: sid });
            }
            return Some(p.pane_id.clone());
        }
        None
    }

    pub fn send_pointer_motion_to_pane(&self, pane_id: &str, x: f64, y: f64) {
        let _ = self.command_sender.send(WaylandCommand::PointerMotionForPane {
            pane_id: pane_id.to_string(),
            x,
            y,
        });
    }

    pub fn send_pointer_button_to_pane(
        &self,
        pane_id: &str,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
    ) {
        let _ = self.command_sender.send(WaylandCommand::PointerButtonForPane {
            pane_id: pane_id.to_string(),
            x,
            y,
            button,
            pressed,
        });
    }

    pub fn send_pointer_axis_to_pane(
        &self,
        pane_id: &str,
        x: f64,
        y: f64,
        horizontal: f64,
        vertical: f64,
    ) {
        let _ = self.command_sender.send(WaylandCommand::PointerAxisForPane {
            pane_id: pane_id.to_string(),
            x,
            y,
            horizontal,
            vertical,
        });
    }

    pub fn send_keyboard_to_pane(&self, pane_id: &str, keycode: u32, pressed: bool) {
        let _ = self.command_sender.send(WaylandCommand::KeyboardKeyForPane {
            pane_id: pane_id.to_string(),
            keycode,
            pressed,
        });
    }

    pub fn pump_game_frames(&mut self, gl: &glow::Context) -> bool {
        let dirty = self.pump_frames(gl);
        !dirty.is_empty()
    }

    pub fn pump_frames(&mut self, gl: &glow::Context) -> Vec<String> {
        let mut dirty_panes = Vec::new();
        // Keep only the latest buffer per surface — intermediate commits are discarded
        // so we present once per host redraw (monitor-paced), not once per spam commit.
        let mut latest_descs: std::collections::HashMap<u32, DmabufFrameDescriptor> =
            std::collections::HashMap::new();
        let mut latest_shms: std::collections::HashMap<u32, (u32, u32, u32, Vec<u8>)> =
            std::collections::HashMap::new();
        // Track which panes got a *presented* texture this pump (for stats once each).
        let mut presented_slots: Vec<(String, bool, Arc<ParkingMutex<ExternalSurfaceSlot>>)> =
            Vec::new();

        while let Ok(event) = self.event_receiver.try_recv() {
            match event {
                WaylandEvent::FrameCommitted(desc) => {
                    latest_descs.insert(desc.surface_id, desc);
                }
                WaylandEvent::ShmFrameCommitted { surface_id, width, height, stride, data } => {
                    latest_shms.insert(surface_id, (width, height, stride, data));
                }
                WaylandEvent::XwaylandReady { display_number } => {
                    debug!("[lotus-nest] host notified Xwayland DISPLAY=:{}", display_number);
                }
                WaylandEvent::ClientConnected => {
                    debug!("[lotus-nest] client connected to nested sub-compositor");
                }
                WaylandEvent::ClientDisconnected => {
                    debug!("[lotus-nest] client disconnected from nested sub-compositor");
                }
                WaylandEvent::SurfaceConfigured { .. } => {}
                WaylandEvent::WindowCreated { pane_id, surface_id, title, app_id: _, role, width, height } => {
                    debug!("[lotus-nest] window created on pane '{}': id={}, role={}, title={:?}, {}x{}", pane_id, surface_id, role, title, width, height);
                }
                WaylandEvent::WindowUpdated { pane_id, surface_id, title, app_id: _, width: _, height: _ } => {
                    debug!("[lotus-nest] window updated on pane '{}': id={}, title={:?}", pane_id, surface_id, title);
                }
                WaylandEvent::WindowClosed { pane_id, surface_id } => {
                    debug!("[lotus-nest] window closed on pane '{}': id={}", pane_id, surface_id);
                    self.on_primary_surface_closed(&pane_id, surface_id);
                }
                WaylandEvent::CursorChanged(_) => {
                    // Applied immediately via EngineCommand::NestedCursor; ignore here.
                }
            }
        }

        let refresh_mhz = self.refresh_rate_millihertz.load(Ordering::SeqCst).max(1);
        let refresh_hz = refresh_mhz as f32 / 1000.0;
        let min_ft_ms = 1000.0 / refresh_hz.max(1.0);

        for (_surface_id, desc) in latest_descs {
            let surface_id = desc.surface_id;
            match self.importer.import_texture(gl, &desc) {
                Ok(imported) => {
                    let pane_info = {
                        let s2p = self.surface_to_pane.lock();
                        let panes = self.panes.lock();
                        if let Some(p_id) = s2p.get(&surface_id).cloned() {
                            panes.iter().find(|p| p.pane_id == p_id).map(|p| {
                                (
                                    p.pane_id.clone(),
                                    p.is_primary,
                                    p.slot.clone(),
                                    p.active_surface_id.clone(),
                                )
                            })
                        } else {
                            None
                        }
                    };

                    if let Some((pane_id, is_primary, slot, active_sid)) = pane_info {
                        let mut apply = true;
                        if is_primary {
                            let active = active_sid.load(Ordering::SeqCst);
                            let px = imported.width.saturating_mul(imported.height);
                            let tiny = imported.width <= 64 && imported.height <= 64;
                            if tiny && active != u32::MAX && active != surface_id {
                                apply = false;
                            } else if !tiny && (active == u32::MAX || active != surface_id) {
                                let cur = slot.lock();
                                let cur_px = cur.width.saturating_mul(cur.height);
                                drop(cur);
                                if px >= cur_px || active == u32::MAX {
                                    active_sid.store(surface_id, Ordering::SeqCst);
                                } else {
                                    apply = false;
                                }
                            } else if !tiny && active != u32::MAX && active != surface_id {
                                apply = false;
                            }
                        }

                        if apply {
                            let mut guard = slot.lock();
                            guard.current_texture = Some(imported.texture);
                            guard.target = imported.target;
                            guard.is_external_oes = imported.is_external_oes;
                            guard.width = imported.width;
                            guard.height = imported.height;
                            guard.y_inverted = imported.y_inverted;

                            if is_primary {
                                let mut def_guard = self.slot.lock();
                                def_guard.current_texture = Some(imported.texture);
                                def_guard.target = imported.target;
                                def_guard.is_external_oes = imported.is_external_oes;
                                def_guard.width = imported.width;
                                def_guard.height = imported.height;
                                def_guard.y_inverted = imported.y_inverted;
                                if imported.width > 64 && imported.height > 64 {
                                    self.note_primary_presented();
                                }
                            }

                            if !dirty_panes.contains(&pane_id) {
                                dirty_panes.push(pane_id.clone());
                                presented_slots.push((pane_id, is_primary, slot.clone()));
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("[lotus-nest] failed to import DMA-BUF for surface {}: {}", surface_id, e);
                    self.release_previous_buffer();
                }
            }
        }

        for (surface_id, (width, height, _stride, data)) in latest_shms {
            let pane_info = {
                let s2p = self.surface_to_pane.lock();
                let panes = self.panes.lock();
                if let Some(p_id) = s2p.get(&surface_id).cloned() {
                    panes.iter().find(|p| p.pane_id == p_id).map(|p| {
                        (
                            p.pane_id.clone(),
                            p.is_primary,
                            p.slot.clone(),
                            p.shm_texture.clone(),
                        )
                    })
                } else {
                    None
                }
            };

            if let Some((pane_id, is_primary, slot, shm_tex_arc)) = pane_info {
                unsafe {
                    use glow::HasContext;
                    let mut tex_guard = shm_tex_arc.lock();
                    let tex = tex_guard.get_or_insert_with(|| {
                        let t = gl.create_texture().expect("Failed to create SHM GL texture");
                        gl.bind_texture(glow::TEXTURE_2D, Some(t));
                        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                        gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
                        t
                    });

                    gl.bind_texture(glow::TEXTURE_2D, Some(*tex));
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        glow::RGBA as i32,
                        width as i32,
                        height as i32,
                        0,
                        glow::BGRA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(&data)),
                    );
                    gl.bind_texture(glow::TEXTURE_2D, None);

                    let mut guard = slot.lock();
                    guard.current_texture = Some(*tex);
                    guard.target = glow::TEXTURE_2D;
                    guard.is_external_oes = false;
                    guard.width = width;
                    guard.height = height;
                    guard.y_inverted = true;

                    if is_primary {
                        let mut def_guard = self.slot.lock();
                        def_guard.current_texture = Some(*tex);
                        def_guard.target = glow::TEXTURE_2D;
                        def_guard.width = width;
                        def_guard.height = height;
                        if width > 64 && height > 64 {
                            self.note_primary_presented();
                        }
                    }

                    if !dirty_panes.contains(&pane_id) {
                        dirty_panes.push(pane_id.clone());
                        presented_slots.push((pane_id, is_primary, slot.clone()));
                    }
                }
            }
        }

        // One presentation sample per pane per host redraw — FPS tracks vsync, not commit spam.
        let now = std::time::Instant::now();
        for (_pane_id, is_primary, slot) in presented_slots {
            let mut guard = slot.lock();
            guard.frame_counter = guard.frame_counter.wrapping_add(1);
            let frametime_ms = if let Some(last) = guard.last_frame_instant {
                now.duration_since(last).as_secs_f32() * 1000.0
            } else {
                min_ft_ms
            };
            // Floor near the monitor period so tiny Instant deltas can't report 100k+ FPS.
            let frametime_ms = frametime_ms.max(min_ft_ms * 0.5);
            guard.last_frame_instant = Some(now);
            guard.last_frame_time_ms = frametime_ms;
            let instant_fps = (1000.0 / frametime_ms).min(refresh_hz * 1.05);
            if guard.current_fps == 0.0 {
                guard.current_fps = instant_fps;
            } else {
                guard.current_fps = guard.current_fps * 0.85 + instant_fps * 0.15;
            }
            guard.rolling_frametimes.push(frametime_ms);
            if guard.rolling_frametimes.len() > 60 {
                guard.rolling_frametimes.remove(0);
            }
            guard.stats_updated = true;
            if is_primary {
                let mut def_guard = self.slot.lock();
                def_guard.frame_counter = guard.frame_counter;
                def_guard.last_frame_time_ms = frametime_ms;
                def_guard.current_fps = guard.current_fps;
                def_guard.stats_updated = true;
            }
        }

        // FrameDone + buffer release happen AFTER the host swap (app.rs), not here.
        // Acking before present free-runs the game against a late scanout clock.

        self.maybe_request_exit_after_primary_gone();

        dirty_panes
    }

    fn note_primary_presented(&self) {
        self.primary_ever_presented.store(true, Ordering::SeqCst);
        *self.primary_empty_since.lock() = None;
    }

    fn primary_pane_id(&self) -> Option<String> {
        self.panes
            .lock()
            .iter()
            .find(|p| p.is_primary)
            .map(|p| p.pane_id.clone())
    }

    fn count_primary_surfaces(&self) -> usize {
        let Some(primary_id) = self.primary_pane_id() else {
            return 0;
        };
        self.surface_to_pane
            .lock()
            .values()
            .filter(|id| id.as_str() == primary_id)
            .count()
    }

    fn on_primary_surface_closed(&self, pane_id: &str, surface_id: u32) {
        let mut was_active = false;
        {
            let mut s2p = self.surface_to_pane.lock();
            s2p.remove(&surface_id);
        }
        {
            let panes = self.panes.lock();
            if let Some(p) = panes.iter().find(|p| p.pane_id == pane_id) {
                if p.surface_id.load(Ordering::SeqCst) == surface_id {
                    p.surface_id.store(u32::MAX, Ordering::SeqCst);
                }
                if p.active_surface_id.load(Ordering::SeqCst) == surface_id {
                    p.active_surface_id.store(u32::MAX, Ordering::SeqCst);
                    was_active = true;
                }
            }
        }

        let Some(primary_id) = self.primary_pane_id() else {
            return;
        };
        if pane_id != primary_id {
            return;
        }
        if !self.primary_ever_presented.load(Ordering::SeqCst) {
            return;
        }
        // Alt+F4 kills the real game window; Steam/Proton helpers (1×1) may linger.
        // Arm exit when the active game surface dies or nothing remains mapped.
        let remaining = self.count_primary_surfaces();
        if was_active || remaining == 0 {
            let mut empty = self.primary_empty_since.lock();
            if empty.is_none() {
                info!(
                    "[lotus-nest] primary game window gone (active={}, remaining={}) — arming exit debounce",
                    was_active, remaining
                );
                *empty = Some(std::time::Instant::now());
            }
        }
    }

    /// Proton/Steam often leave the launcher PID alive after Alt+F4; tear down when
    /// the primary game window is gone (after a short debounce for remount races).
    fn maybe_request_exit_after_primary_gone(&self) {
        if self.primary_exit_requested.load(Ordering::SeqCst) {
            return;
        }
        if !self.primary_ever_presented.load(Ordering::SeqCst) {
            return;
        }
        let since = {
            let empty = self.primary_empty_since.lock();
            match *empty {
                Some(t) => t,
                None => return,
            }
        };
        const DEBOUNCE: Duration = Duration::from_millis(1200);
        if since.elapsed() < DEBOUNCE {
            return;
        }
        // A new real present clears empty_since via note_primary_presented.
        if self
            .primary_exit_requested
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        info!(
            "[lotus-nest] primary game gone — closing atrium window '{}'",
            self.window_id
        );
        if let Some(proxy) = crate::app::EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(crate::app::EngineCommand::CloseWindow(
                self.window_id.clone(),
            ));
        }
    }

    /// Forward a keyboard key event to the sub-compositor.
    pub fn send_key(&self, keycode: u32, pressed: bool) {
        let _ = self.command_sender.send(WaylandCommand::KeyboardKey { keycode, pressed });
    }

    /// Forward a pointer motion event to the sub-compositor.
    pub fn send_pointer_motion(&self, x: f64, y: f64) {
        let _ = self.command_sender.send(WaylandCommand::PointerMotion { x, y });
    }

    pub fn send_pointer_relative_motion(&self, dx: f64, dy: f64) {
        let _ = self
            .command_sender
            .send(WaylandCommand::PointerRelativeMotion { dx, dy });
    }

    /// Forward a pointer button event to the sub-compositor.
    pub fn send_pointer_button(&self, button: u32, pressed: bool) {
        let _ = self.command_sender.send(WaylandCommand::PointerButton { button, pressed });
    }

    /// Forward a pointer scroll/axis event to the sub-compositor.
    pub fn send_pointer_axis(&self, horizontal: f64, vertical: f64) {
        let _ = self.command_sender.send(WaylandCommand::PointerAxis { horizontal, vertical });
    }

    /// Resize the sub-compositor's target window geometry.
    pub fn send_resize(&self, width: u32, height: u32) {
        let _ = self.command_sender.send(WaylandCommand::Resize { width, height });
    }

    /// Notify client frame callbacks that a frame has presented to the screen.
    pub fn notify_frame_done(&self, timestamp_ms: u32) {
        let _ = self.command_sender.send(WaylandCommand::FrameDone { timestamp_ms });
    }

    /// Keep nested `wl_output` refresh rate matched to the host monitor.
    pub fn set_output_refresh(&self, millihertz: u32) {
        let millihertz = millihertz.max(1);
        let prev = self.refresh_rate_millihertz.swap(millihertz, Ordering::SeqCst);
        if prev != millihertz {
            let _ = self.command_sender.send(WaylandCommand::SetOutputRefresh { millihertz });
        }
    }

    /// Release previous buffer reference so client can reuse the swapchain image.
    pub fn release_previous_buffer(&self) {
        let _ = self.command_sender.send(WaylandCommand::ReleasePreviousBuffer);
    }

    /// Get active X11 display number if nested Xwayland is running.
    pub fn get_x11_display(&self) -> Option<u32> {
        let d = self.x11_display.load(Ordering::SeqCst);
        if d != u32::MAX { Some(d) } else { None }
    }

}

/// Create+drop a tiny SHM pool so smithay's drop-thread LazyLock initializes while
/// we still have headroom to spawn threads (before Proton/Discord).
fn warm_smithay_shm_drop_thread() {
    use std::io::Write;
    use std::num::NonZeroUsize;
    use std::os::fd::OwnedFd;

    let path = std::env::temp_dir().join(format!(
        "lotus-nest-shm-warm-{}",
        std::process::id()
    ));
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            warn!("[lotus-nest] shm warm: open failed: {e}");
            return;
        }
    };
    let _ = file.write_all(&[0u8; 4096]);
    let _ = std::fs::remove_file(&path);
    let fd = OwnedFd::from(file);
    let size = NonZeroUsize::new(4096).unwrap();
    match smithay::wayland::shm::pool::Pool::new(fd, size) {
        Ok(pool) => {
            drop(pool);
            info!("[lotus-nest] warmed smithay SHM drop thread before nesting clients");
        }
        Err(_) => {
            warn!("[lotus-nest] shm warm: Pool::new failed");
        }
    }
}

impl LotusWaylandServer {
    /// Wait up to `timeout` for nested Xwayland to initialize and announce its display number.
    pub fn wait_for_x11_ready(&self, timeout: Duration) -> Option<u32> {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            let d = self.x11_display.load(Ordering::SeqCst);
            if d != u32::MAX {
                return Some(d);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

/// Map winit `KeyCode` to Linux evdev scancode for Wayland keyboard forwarding.
pub fn winit_key_to_evdev(key: winit::keyboard::KeyCode) -> u32 {
    use winit::keyboard::KeyCode;
    match key {
        KeyCode::Escape => 1,
        KeyCode::Digit1 => 2,
        KeyCode::Digit2 => 3,
        KeyCode::Digit3 => 4,
        KeyCode::Digit4 => 5,
        KeyCode::Digit5 => 6,
        KeyCode::Digit6 => 7,
        KeyCode::Digit7 => 8,
        KeyCode::Digit8 => 9,
        KeyCode::Digit9 => 10,
        KeyCode::Digit0 => 11,
        KeyCode::Minus => 12,
        KeyCode::Equal => 13,
        KeyCode::Backspace => 14,
        KeyCode::Tab => 15,
        KeyCode::KeyQ => 16,
        KeyCode::KeyW => 17,
        KeyCode::KeyE => 18,
        KeyCode::KeyR => 19,
        KeyCode::KeyT => 20,
        KeyCode::KeyY => 21,
        KeyCode::KeyU => 22,
        KeyCode::KeyI => 23,
        KeyCode::KeyO => 24,
        KeyCode::KeyP => 25,
        KeyCode::BracketLeft => 26,
        KeyCode::BracketRight => 27,
        KeyCode::Enter => 28,
        KeyCode::ControlLeft => 29,
        KeyCode::KeyA => 30,
        KeyCode::KeyS => 31,
        KeyCode::KeyD => 32,
        KeyCode::KeyF => 33,
        KeyCode::KeyG => 34,
        KeyCode::KeyH => 35,
        KeyCode::KeyJ => 36,
        KeyCode::KeyK => 37,
        KeyCode::KeyL => 38,
        KeyCode::Semicolon => 39,
        KeyCode::Quote => 40,
        KeyCode::Backquote => 41,
        KeyCode::ShiftLeft => 42,
        KeyCode::Backslash => 43,
        KeyCode::KeyZ => 44,
        KeyCode::KeyX => 45,
        KeyCode::KeyC => 46,
        KeyCode::KeyV => 47,
        KeyCode::KeyB => 48,
        KeyCode::KeyN => 49,
        KeyCode::KeyM => 50,
        KeyCode::Comma => 51,
        KeyCode::Period => 52,
        KeyCode::Slash => 53,
        KeyCode::ShiftRight => 54,
        KeyCode::NumpadMultiply => 55,
        KeyCode::AltLeft => 56,
        KeyCode::Space => 57,
        KeyCode::CapsLock => 58,
        KeyCode::F1 => 59,
        KeyCode::F2 => 60,
        KeyCode::F3 => 61,
        KeyCode::F4 => 62,
        KeyCode::F5 => 63,
        KeyCode::F6 => 64,
        KeyCode::F7 => 65,
        KeyCode::F8 => 66,
        KeyCode::F9 => 67,
        KeyCode::F10 => 68,
        KeyCode::NumLock => 69,
        KeyCode::ScrollLock => 70,
        KeyCode::F11 => 87,
        KeyCode::F12 => 88,
        KeyCode::ControlRight => 97,
        KeyCode::AltRight => 100,
        KeyCode::Home => 102,
        KeyCode::ArrowUp => 103,
        KeyCode::PageUp => 104,
        KeyCode::ArrowLeft => 105,
        KeyCode::ArrowRight => 106,
        KeyCode::End => 107,
        KeyCode::ArrowDown => 108,
        KeyCode::PageDown => 109,
        KeyCode::Insert => 110,
        KeyCode::Delete => 111,
        _ => 0,
    }
}
