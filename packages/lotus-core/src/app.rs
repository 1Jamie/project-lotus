use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::path::PathBuf;

use once_cell::sync::OnceCell;
use log::{info, error, debug, trace, warn};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize as WinitPhysicalSize;
use dpi::PhysicalSize as ServoPhysicalSize;
use winit::event::{WindowEvent, MouseScrollDelta};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy, ControlFlow};
use winit::window::{Window, WindowId, CursorIcon};
use winit::raw_window_handle::{HasWindowHandle, HasDisplayHandle};

#[cfg(target_os = "macos")]
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
#[cfg(target_os = "linux")]
use winit::platform::x11::WindowAttributesExtX11;
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;

use servo::{
    ServoBuilder,
    WebViewBuilder, WindowRenderingContext, RenderingContext,
    InputEvent, KeyboardEvent as ServoKeyboardEvent,
    KeyState, Location, Modifiers,
    MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    MouseLeftViewportEvent,
    WheelEvent, WheelDelta, WheelMode,
    UserContentManager,
    DeviceIndependentPixel, DevicePixel,
};
use euclid::{Point2D, Scale};
use dark_light;

use crate::window::{
    WindowInstance, PaneInstance, WindowMetadata, DragRegionPayload, SelectPromptWrapper,
    ActiveSelectPrompt, WindowOptions, PaneOptions, ScrollbarConfig,
    map_winit_key, map_winit_code, map_winit_modifiers,
};
use crate::layout::{
    begin_gutter_drag, hit_test_gutters, Axis, PaneAnchor, StagedLayout, DOCK_INNER_ID,
};
use crate::server::{WS_SENDERS, WS_PENDING};
use crate::delegate::LotusPaneDelegate;
use crate::gl::{self, RemotePaneConsumer};
use crate::vfs::{ByteLimitedLruCache, EncryptedVfs};
use crate::window_state::{self, WindowStateManager};
use crate::platform;

use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};

pub(crate) static EVENT_LOOP_PROXY: OnceCell<EventLoopProxy<EngineCommand>> = OnceCell::new();

// Global app state (thread-safe metadata only, no Rc types)
pub(crate) static APP_STATE: OnceCell<Arc<Mutex<AppState>>> = OnceCell::new();

pub const MSG_TYPE_CONTROL: u8 = 0x01;
pub const MSG_TYPE_DATA: u8 = 0x02;

static TRUSTED_ORIGINS: OnceCell<Arc<dashmap::DashSet<String>>> = OnceCell::new();

pub(crate) fn get_trusted_origins() -> Arc<dashmap::DashSet<String>> {
    TRUSTED_ORIGINS.get_or_init(|| {
        let set = Arc::new(dashmap::DashSet::new());
        set.insert("lotus-resource://localhost".to_string());
        set
    }).clone()
}

/// Track-1 / Axum trust-list normalization (WHATWG-aligned IpcTrust).
/// Delegates to [`crate::origin`] — see `testdata/origin_golden.json`.
pub(crate) fn normalize_origin(raw: &str) -> Option<String> {
    crate::origin::normalize_origin_ipc(raw)
}

#[inline]
pub(crate) fn fast_exit(code: i32) -> ! {
    #[cfg(unix)]
    unsafe {
        libc::_exit(code);
    }
    #[cfg(not(unix))]
    std::process::exit(code);
}

#[cfg(target_os = "linux")]
pub fn detect_linux_theme_robust() -> dark_light::Mode {
    // 1. Check environment variables populated by the Node Host launcher
    if let Ok(theme) = std::env::var("LOTUS_SYSTEM_THEME") {
        if theme.eq_ignore_ascii_case("dark") {
            return dark_light::Mode::Dark;
        } else if theme.eq_ignore_ascii_case("light") {
            return dark_light::Mode::Light;
        }
    }
    if let Ok(gtk) = std::env::var("GTK_THEME") {
        let lower = gtk.to_lowercase();
        if lower.contains("-dark") || lower.contains(":dark") {
            return dark_light::Mode::Dark;
        }
    }

    // 2. Delegate to the standard dark_light crate without invoking any external subprocesses (e.g. gsettings)
    dark_light::detect()
}

pub(crate) struct AppState {
    pub(crate) window_metadata: HashMap<String, WindowMetadata>,
    pub(crate) window_states: WindowStateManager,
    pub(crate) ipc_server_port: u16,
    pub(crate) ipc_server_token: String,
    pub(crate) msgpackr_source: String,
    pub(crate) profiling: bool,
    pub(crate) _start_time: Instant,
    pub(crate) window_start_times: HashMap<String, Instant>,
    pub(crate) vfs: Option<Arc<EncryptedVfs>>,
    pub(crate) resource_cache: ByteLimitedLruCache,
}

// Track-1 userscripts live in `crate::userscripts` (include_str! embedded).
use crate::userscripts::{classify_pane_role, populate_track1_ucm, PaneScriptEnv};

#[derive(Debug)]
pub enum EngineCommand {
    Wake,
    // App-level commands
    CreateWindow(WindowOptions, String), // options, window_id
    Quit,

    // Window-specific commands (all take window ID)
    LoadUrl(String, String, String), // window_id, pane_id, url
    IpcMessage(String, Vec<u8>), // window_id, raw_bytes
    IpcMessages(String, Vec<Vec<u8>>),
    Resize(String, winit::dpi::PhysicalSize<u32>), // window_id, size
    SetPosition(String, winit::dpi::PhysicalPosition<i32>), // window_id, position
    SetAlwaysOnTop(String, bool), // window_id, flag
    RequestAttention(String), // window_id
    SetTitle(String, String), // window_id, title
    CloseWindow(String), // window_id
    SetDecorations(String, bool), // window_id, decorations
    ExecuteScript(String, String, String), // window_id, pane_id, script
    ShowWindow(String), // window_id
    HideWindow(String), // window_id
    UpdateDragRegions(String, String, Vec<euclid::Rect<f32, servo::DevicePixel>>, Vec<euclid::Rect<f32, servo::DevicePixel>>), // window_id, pane_id, drag_regions, no_drag_regions
    MinimizeWindow(String), // window_id
    UnminimizeWindow(String), // window_id
    MaximizeWindow(String), // window_id
    UnmaximizeWindow(String), // window_id
    FocusWindow(String), // window_id
    AnimatingChanged(String, String, bool), // window_id, pane_id, animating
    NewFrameReady(String, String), // window_id, pane_id
    RedrawWindow(String), // window_id
    SetMinInnerSize(String, Option<winit::dpi::PhysicalSize<u32>>), // window_id, size (None = remove constraint)
    SetMaxInnerSize(String, Option<winit::dpi::PhysicalSize<u32>>), // window_id, size (None = remove constraint)

    // Pane-specific commands
    CreatePane(String, String, String, euclid::Rect<f32, servo::DeviceIndependentPixel>, i32, PaneAnchor, u32, bool), // window_id, pane_id, url, rect, z_index, anchor, dock_order, visible
    CreateRemotePane {
        window_id: String,
        pane_id: String,
        ro_fd: std::os::unix::io::RawFd,
        shm_size: usize,
        ack_rw_fd: std::os::unix::io::RawFd,
        rect: euclid::Rect<f32, servo::DeviceIndependentPixel>,
        z_index: i32,
        anchor: PaneAnchor,
        dock_order: u32,
        visible: bool,
        max_shm_width: Option<u32>,
        max_shm_height: Option<u32>,
    },
    RemovePane(String, String), // window_id, pane_id
    SetPaneRect(String, String, euclid::Rect<f32, servo::DeviceIndependentPixel>), // window_id, pane_id, rect
    /// Update dock thickness without clearing the pane's anchor.
    /// Left/Right consume `width`; Top/Bottom consume `height`; Fill/None are no-ops.
    SetPaneSize(String, String, f32, f32), // window_id, pane_id, width, height
    SetPaneWeight(String, String, f32), // window_id, pane_id, weight
    SetSplitWeights(String, String, String), // window_id, split_id, JSON map|array
    SetSplitSizes(String, String, String), // window_id, split_id, JSON map|array
    BeginLayoutBatch(String), // window_id
    EndLayoutBatch(String), // window_id — flush one recalculate_layout
    SetPaneVisible(String, String, bool), // window_id, pane_id, visible
    FocusPane(String, String), // window_id, pane_id
    AttachRemotePaneInput(String, String, std::os::unix::io::RawFd), // window_id, pane_id, fd
    /// Keep a remote pane docked+visible (so it receives compositor vsync) while a host
    /// crash-chrome pane covers it as a high-z absolute overlay. Used during crash UI and
    /// respawn so we never gate first-frame production on revealing the remote early.
    CoverRemoteWithCrashChrome {
        window_id: String,
        remote_pane_id: String,
        crash_pane_id: String,
    },
    /// After a successor engine is attached: invalidate stale GL/SHM cache and arm the
    /// gate that emits `remote-frame-composited` once a fresh frame is uploaded.
    ArmCrashCoverFreshFrame {
        window_id: String,
        remote_pane_id: String,
    },
    /// Lift cover bookkeeping after JS hides crash chrome.
    ClearRemoteCrashCover {
        window_id: String,
        remote_pane_id: String,
    },

    // Embedder control commands
    ShowSelectControl {
        window_id: String,
        pane_id: String,
        prompt: SelectPromptWrapper,
    },
    HideSelectControl {
        window_id: String,
        control_id: servo::EmbedderControlId,
    },
    UpdateSelectControl {
        window_id: String,
        control_id: String,
        selected_indices: Vec<u32>,
    },
    SubmitSelectControl {
        window_id: String,
        control_id: String,
        selected_indices: Vec<u32>,
    },
    DismissSelectControl {
        window_id: String,
        control_id: String,
    },
}

fn intercept_drag_regions(raw_bytes: &[u8], window_id: String) {
    let mut cursor = std::io::Cursor::new(raw_bytes);
    match rmpv::decode::value::read_value(&mut cursor) {
        Ok(batch_val) => {
            trace!("Rust: Intercepted IPC batch value: {:?}", batch_val);
            if let Some(batch) = batch_val.as_array() {
                if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                    // Extract true window_id if it's a composite clientId (window_id:pane_id)
                    let true_window_id = window_id.split(':').next().unwrap_or(&window_id).to_string();

                    for msg in batch {
                        if let Some(pair) = msg.as_array() {
                            if pair.len() >= 2 {
                                if let Some(channel) = pair[0].as_str() {
                                    if channel == "lotus:set-drag-regions" {
                                        match rmpv::ext::from_value::<DragRegionPayload>(pair[1].clone()) {
                                            Ok(payload) => {
                                                let drag_regions = payload.drag.unwrap_or_default().into_iter().map(|r| {
                                                    euclid::Rect::new(
                                                        euclid::Point2D::new(r.x, r.y),
                                                        euclid::Size2D::new(r.width, r.height)
                                                    )
                                                }).collect::<Vec<_>>();
                                                
                                                let no_drag_regions = payload.no_drag.unwrap_or_default().into_iter().map(|r| {
                                                    euclid::Rect::new(
                                                        euclid::Point2D::new(r.x, r.y),
                                                        euclid::Size2D::new(r.width, r.height)
                                                    )
                                                }).collect::<Vec<_>>();
                                                
                                                let pid = payload.pane_id.unwrap_or_else(|| "main".to_string());
                                                info!("Rust: Intercepted {} drag regions for pane '{}' in window '{}': {:?}", drag_regions.len(), pid, true_window_id, drag_regions);
                                                
                                                let _ = proxy.send_event(EngineCommand::UpdateDragRegions(
                                                    true_window_id.clone(),
                                                    pid,
                                                    drag_regions,
                                                    no_drag_regions
                                                ));
                                            },
                                            Err(e) => {
                                                debug!("Rust: Failed to deserialize DragRegionPayload: {:?}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
        Err(e) => {
            trace!("Rust: Failed to decode IPC batch as rmpv::Value: {:?}", e);
        }
    }
}


// ------------------------------------------------------------------
// WAKER STRATEGY
// ------------------------------------------------------------------

#[derive(Clone)]
pub struct LotusWaker(EventLoopProxy<EngineCommand>, Arc<AtomicBool>);

impl servo::EventLoopWaker for LotusWaker {
    fn clone_box(&self) -> Box<dyn servo::EventLoopWaker> {
        Box::new(self.clone())
    }
    fn wake(&self) {
        if self.1.swap(true, Ordering::SeqCst) == false {
            let _ = self.0.send_event(EngineCommand::Wake);
        }
    }
}

// ------------------------------------------------------------------
// INTERNAL APP HANDLER (Winit 0.30)
// ------------------------------------------------------------------

static WEBVIEW_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub struct LotusApp {
    servo: Option<servo::Servo>,
    windows: HashMap<String, WindowInstance>,
    winit_id_to_uuid: HashMap<WindowId, String>,
    proxy: EventLoopProxy<EngineCommand>,
    callback: ThreadsafeFunction<(String, Vec<napi::bindgen_prelude::Buffer>), ErrorStrategy::Fatal>,
    pending_wake: Arc<AtomicBool>,
}

impl Drop for LotusApp {
    fn drop(&mut self) {
        info!("Rust: [Teardown] LotusApp dropping, clearing windows first");
        // Drop all WebViews before shutting down Servo to avoid hangs
        self.windows.clear();
        info!("Rust: [Teardown] Windows cleared, shutting down Servo");
        self.servo = None;
    }
}

impl LotusApp {
    pub fn new(
        proxy: EventLoopProxy<EngineCommand>,
        callback: ThreadsafeFunction<(String, Vec<napi::bindgen_prelude::Buffer>), ErrorStrategy::Fatal>,
    ) -> Self {
        let mut app = Self {
            servo: None,
            windows: HashMap::new(),
            winit_id_to_uuid: HashMap::new(),
            proxy,
            callback,
            pending_wake: Arc::new(AtomicBool::new(false)),
        };
        app.ensure_servo();
        app
    }
    
    fn ensure_servo(&mut self) -> &servo::Servo {
        if self.servo.is_none() {
            info!("Rust: Initializing Servo Singleton");
            let mut prefs = servo::prefs::Preferences::default();
            prefs.shell_background_color_rgba = [0.0, 0.0, 0.0, 0.0]; // Transparent
            // removed prefs.gfx_precache_shaders = true so that shaders compile lazily (Option A)
            // prefs.gfx_precache_shaders = true;

            let waker = LotusWaker(self.proxy.clone(), self.pending_wake.clone());
            let servo = ServoBuilder::default()
                .event_loop_waker(Box::new(waker))
                .preferences(prefs)
                .build();
            servo.setup_logging();
            self.servo = Some(servo);
        }
        self.servo.as_ref().unwrap()
    }
}

fn map_winit_logical_key(k: &winit::keyboard::Key) -> String {
    match k {
        winit::keyboard::Key::Named(named) => format!("{:?}", named),
        winit::keyboard::Key::Character(s) => s.as_str().to_string(),
        _ => "Unidentified".to_string(),
    }
}

fn map_winit_physical_key(k: winit::keyboard::PhysicalKey) -> String {
    match k {
        winit::keyboard::PhysicalKey::Code(code) => format!("{:?}", code),
        _ => "Unidentified".to_string(),
    }
}

pub(crate) fn forward_remote_input(fd: std::os::unix::io::RawFd, json: &str) {
    let mut bytes = json.as_bytes().to_vec();
    bytes.push(b'\n');
    unsafe {
        let _ = libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }
}

/// True floating overlays beat gutters. Tree-backed leaves (including sandboxed
/// remotes with `Anchor::None`) must **not** — they are layout participants, not modals.
fn is_gutter_blocking_overlay(instance: &WindowInstance, pane: &PaneInstance) -> bool {
    if !pane.is_visible || pane.anchor != PaneAnchor::None {
        return false;
    }
    if pane.id.starts_with("__lotus_crash_") {
        return false;
    }
    // Participating layout-tree leaf (dock or split) → allow gutters through.
    if instance.layout_root.child_participating(&pane.id) {
        return false;
    }
    true
}

/// Scrollbar beats gutter only for pixels that are in the trailing scrollbar band
/// **and outside** the gutter strip (gutter strip already biased into the far neighbor).
fn point_in_trailing_scrollbar(
    instance: &WindowInstance,
    gutter: &crate::layout::HitTestGutter,
    point: Point2D<f32, DevicePixel>,
    scale_factor: f32,
) -> bool {
    if !instance.scrollbar_config.enabled {
        return false;
    }
    // Remotes use in-page scroll; host overlay scrollbars are in-process only.
    let neighbor_id = if gutter.left_id == DOCK_INNER_ID {
        return false;
    } else {
        gutter.left_id.as_str()
    };
    let Some(pane) = instance.panes.get(neighbor_id) else {
        return false;
    };
    if !pane.is_visible || pane.is_remote {
        return false;
    }
    let sb = (instance.scrollbar_config.width as f32 * scale_factor)
        .round()
        .max(1.0);
    let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
    let in_scrollbar = match gutter.axis {
        Axis::Horizontal => {
            let x0 = px + pw - sb;
            point.x >= x0 && point.x < px + pw && point.y >= py && point.y <= py + ph
        }
        Axis::Vertical => {
            let y0 = py + ph - sb;
            point.y >= y0 && point.y < py + ph && point.x >= px && point.x <= px + pw
        }
    };
    if !in_scrollbar {
        return false;
    }
    // Inside the gutter strip itself → gutter wins (strip is the interactive target).
    let gx = gutter.rect.origin.x as f32;
    let gy = gutter.rect.origin.y as f32;
    let gw = gutter.rect.size.width as f32;
    let gh = gutter.rect.size.height as f32;
    let in_gutter =
        point.x >= gx && point.x < gx + gw && point.y >= gy && point.y < gy + gh;
    !in_gutter
}

fn send_pane_mouse_leave(window_id: &str, pane: &PaneInstance) {
    if pane.is_remote {
        if let Some(fd) = pane.remote_input_fd {
            let surface_id = format!("{}:{}", window_id, pane.id);
            let msg = format!(
                "{{\"type\":\"Input\",\"event\":{{\"kind\":\"MouseMove\",\"surface_id\":\"{}\",\"generation\":1,\"x\":-1,\"y\":-1,\"modifiers\":0}}}}",
                surface_id
            );
            forward_remote_input(fd, &msg);
        }
        return;
    }
    pane.webview.notify_input_event(InputEvent::MouseLeftViewport(
        MouseLeftViewportEvent {
            focus_moving_to_another_iframe: false,
        },
    ));
}

fn clear_gutter_hover_state(instance: &mut WindowInstance) {
    instance.gutter_hover = None;
}

fn enter_gutter_hover(
    instance: &mut WindowInstance,
    window_id: &str,
    left_id: &str,
    right_id: &str,
) {
    let key = (left_id.to_string(), right_id.to_string());
    if instance.gutter_hover.as_ref() == Some(&key) {
        return;
    }
    // Synthetic leave so CSS/Servo hover does not stick under the strip.
    if let Some(pid) = instance.hover_pane_id.take() {
        if let Some(pane) = instance.panes.get(&pid) {
            send_pane_mouse_leave(window_id, pane);
        }
    }
    instance.gutter_hover = Some(key);
}

/// Read the window's current monitor refresh rate in millihertz (fallback 60 Hz).
fn monitor_refresh_rate_millihertz(window: &winit::window::Window) -> u32 {
    window
        .current_monitor()
        .and_then(|m| m.refresh_rate_millihertz())
        .filter(|&mhz| mhz > 0)
        .unwrap_or(60_000)
}

/// Unfocused (and on Wayland, typically minimized too — no reliable signal): 10 Hz.
const BACKGROUND_REFRESH_MILLIHERTZ: u32 = 10_000; // 10 Hz
/// Pause sandboxed graphics when the platform *explicitly* reports hidden
/// (X11 occlude/minimize, zero-size). Wayland does not provide this via winit today.
const HIDDEN_REFRESH_MILLIHERTZ: u32 = 0;

fn window_is_viewable(instance: &WindowInstance) -> bool {
    !instance.os_occluded && !instance.os_minimized && !instance.zero_size
}

fn effective_refresh_millihertz(instance: &WindowInstance, monitor_mhz: u32) -> u32 {
    let monitor = monitor_mhz.max(1);
    if !window_is_viewable(instance) {
        HIDDEN_REFRESH_MILLIHERTZ // 0 = engine graphics pause
    } else if instance.os_focused {
        monitor
    } else {
        BACKGROUND_REFRESH_MILLIHERTZ.min(monitor)
    }
}

fn push_display_timing_to_remotes(instance: &WindowInstance) {
    for pane in instance.panes.values() {
        if let Some(fd) = pane.remote_input_fd {
            send_remote_display_timing(fd, instance.refresh_rate_millihertz);
        }
    }
}

fn sync_minimized_from_os(instance: &mut WindowInstance) {
    // Wayland always returns None — leave os_minimized unchanged there.
    if let Some(minimized) = instance.window.is_minimized() {
        instance.os_minimized = minimized;
    }
}

fn apply_refresh_policy(instance: &mut WindowInstance) {
    instance.monitor_refresh_rate_millihertz =
        monitor_refresh_rate_millihertz(&instance.window);
    let next = effective_refresh_millihertz(
        instance,
        instance.monitor_refresh_rate_millihertz,
    );
    if next != instance.refresh_rate_millihertz {
        info!(
            "Rust: refresh policy {}→{} mHz (focused={} viewable={} minimized={} occluded={} zero={})",
            instance.refresh_rate_millihertz,
            next,
            instance.os_focused,
            window_is_viewable(instance),
            instance.os_minimized,
            instance.os_occluded,
            instance.zero_size,
        );
        instance.refresh_rate_millihertz = next;
        push_display_timing_to_remotes(instance);
    }
}

fn send_remote_display_timing(fd: std::os::unix::io::RawFd, millihertz: u32) {
    // 0 = host-authoritative graphics pause (explicit hide only).
    let msg = format!(
        "{{\"type\":\"SetDisplayTiming\",\"refresh_rate_millihertz\":{}}}",
        millihertz
    );
    forward_remote_input(fd, &msg);
}

fn send_remote_vsync(fd: std::os::unix::io::RawFd, surface_id: &str, presentation_time_ns: u64) {
    let msg = format!(
        "{{\"type\":\"Vsync\",\"surface_id\":{},\"presentation_time_ns\":{}}}",
        serde_json::to_string(surface_id).unwrap_or_else(|_| "\"\"".into()),
        presentation_time_ns
    );
    forward_remote_input(fd, &msg);
}

impl ApplicationHandler<EngineCommand> for LotusApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // App is ready to create windows
        info!("Rust: Application Resumed");
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Servo needs to process any queued internal events before the event loop sleeps.
        // This mirrors servoshell's `pump_servo_event_loop` call structure.
        // Without this, Servo's internal work (like deferred layout/paint) doesn't get
        // picked up until an external event wakes the loop.
        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }

        // Consolidated Redraw Request — only when something actually needs paint.
        // Do NOT force-redraw every vsync merely because remotes exist: FramePublished
        // already calls requestRedraw, and blind redraws re-emit Vsync → double-drive
        // sandboxed paint/readback. Poll SHM for unread frames so unfocused drain still works.
        let mut pending_remote_work = false;
        let mut remote_period = Duration::from_millis(16);
        for window in self.windows.values_mut() {
            sync_minimized_from_os(window);
            apply_refresh_policy(window);

            let viewable = window_is_viewable(window);
            let mut any_needs_repaint = false;
            let mut reason = String::new();
            let mhz = window.refresh_rate_millihertz.max(1) as u64;
            for pane in window.panes.values() {
                if pane.is_remote {
                    remote_period = Duration::from_nanos(1_000_000_000_000u64 / mhz);
                    // Hidden windows: do not wake the compositor for unread SHM —
                    // engines are already paused / backpressured.
                    if viewable {
                        if let Some(consumer) = pane.remote_consumer.as_ref() {
                            if consumer.has_unread_frame() {
                                pending_remote_work = true;
                                any_needs_repaint = true;
                                reason = format!("Remote pane '{}' has unread SHM frame", pane.id);
                            }
                        }
                    }
                }
                if pane.needs_repaint && viewable {
                    any_needs_repaint = true;
                    reason = format!("Pane '{}' needs repaint", pane.id);
                }
            }
            if any_needs_repaint {
                trace!("Rust: [Lifecycle] Requesting Redraw for window (Reason: {})", reason);
                window.window.request_redraw();
            }
        }

        // When remotes may publish soon, wake at display period to poll SHM; otherwise sleep.
        if pending_remote_work {
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + remote_period));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: EngineCommand) {
        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }
        match event {
            EngineCommand::Wake => {
                self.pending_wake.store(false, Ordering::SeqCst);
                // trace!("Rust: Wake received");
            },
            EngineCommand::CreateWindow(options, window_id) => {
                info!("Rust: CreateWindow received for {}", window_id);
                let creation_start = Instant::now();
                
                if let Some(state) = APP_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        s.window_start_times.insert(window_id.clone(), creation_start);
                    }
                }

                
                // Detect Theme explicitly
                #[cfg(target_os = "linux")]
                let mode = detect_linux_theme_robust();
                #[cfg(not(target_os = "linux"))]
                let mode = dark_light::detect();

                info!("Rust: Detected system theme mode: {:?}", mode);
                
                let theme = match mode {
                    dark_light::Mode::Dark => Some(winit::window::Theme::Dark),
                    dark_light::Mode::Light => Some(winit::window::Theme::Light),
                    dark_light::Mode::Default => None,
                };
                
                if let Some(t) = theme {
                     info!("Rust: Setting winit window theme to: {:?}", t);
                } else {
                     info!("Rust: Winit window theme set to None (Default)");
                }

                info!("Rust: Creating window with visible=false, transparent={}", options.transparent);

                let mut window_attrs = Window::default_attributes()
                    .with_title(options.title.clone())
                    .with_inner_size(WinitPhysicalSize::new(
                        options.width,
                        options.height
                    ))
                    .with_decorations(!options.frameless)
                    .with_visible(false) // Always start hidden
                    .with_transparent(options.transparent)
                    .with_theme(theme);

                // NOTE: We intentionally do NOT set no_redirection_bitmap here.
                // WS_EX_NOREDIRECTIONBITMAP requires a DirectComposition visual tree to
                // display content. ANGLE/surfman create a regular DXGI swap chain for the
                // HWND which is NOT compatible with that flag — the window becomes fully
                // transparent/invisible rather than showing the rendered content.

                #[cfg(target_os = "linux")]
                if let Some(class) = &options.wm_class {
                    // Use the class name for both instance and class for simplicity, or split it if needed.
                    // X11
                    // X11
                    window_attrs = WindowAttributesExtX11::with_name(window_attrs, class.clone(), class.clone());
                    // Wayland
                    window_attrs = WindowAttributesExtWayland::with_name(window_attrs, class.clone(), class.clone());
                }

                if options.restore_state {
                    if let Some(state) = APP_STATE.get() {
                        if let Ok(s) = state.lock() {
                            if let Some(ws) = s.window_states.get_window_state(&window_id) {
                                window_attrs = window_attrs
                                    .with_inner_size(WinitPhysicalSize::new(ws.width, ws.height))
                                    .with_position(winit::dpi::PhysicalPosition::new(ws.x, ws.y));
                            }
                        }
                    }
                }

                let window = match event_loop.create_window(window_attrs) {
                    Ok(w) => Arc::new(w),
                    Err(e) => {
                        error!("Failed to create window: {}", e);
                        return;
                    }
                };

                let winit_id = window.id();
                
                let display_handle = match window.display_handle() {
                    Ok(h) => h,
                    Err(e) => {
                        error!("Failed to get display handle: {}", e);
                        return;
                    }
                };
                let window_handle = match window.window_handle() {
                    Ok(h) => h,
                    Err(e) => {
                        error!("Failed to get window handle: {}", e);
                        return;
                    }
                };
                let size = window.inner_size();
                
                let rendering_context = match WindowRenderingContext::new(
                    display_handle,
                    window_handle,
                    size
                ) {
                    Ok(ctx) => Rc::new(ctx),
                    Err(e) => {
                        error!("Failed to create rendering context: {:?}", e);
                        return;
                    }
                };
                
                let _ = rendering_context.make_current();

                let gl = rendering_context.glow_gl_api();

                // Apply Vibrancy/Transparency effects
                if options.transparent {
                    #[cfg(target_os = "macos")]
                    {
                        let _ = apply_vibrancy(&window, NSVisualEffectMaterial::HudWindow, None, None);
                    }
                    
                    #[cfg(target_os = "windows")]
                    {
                        //there might be an issue with the blur on windows through egl/angle so commenting out for now
                        // Try Mica first, fall back to blur
                        // let _ = apply_mica(&window, None)
                        //    .or_else(|_| apply_blur(&window, None));
                    }
                }
                
                let servo = self.ensure_servo().clone();
                
                // Get msgpackr source, port and token from state
                let mut panes = HashMap::new();
                let mut primary_pane_id = "main".to_string();
                
                // 1. Setup shared components
                let hidpi_scale_factor_val = window.scale_factor() as f32;
                let hidpi_scale_factor = Scale::<f32, DeviceIndependentPixel, DevicePixel>::new(hidpi_scale_factor_val);
                
                let (msgpackr_source, port, token) = if let Some(state) = APP_STATE.get() {
                    if let Ok(s) = state.lock() {
                        (s.msgpackr_source.clone(), s.ipc_server_port, s.ipc_server_token.clone())
                    } else { ("".to_string(), 0, "".to_string()) }
                } else { ("".to_string(), 0, "".to_string()) };

                let scrollbar_config = ScrollbarConfig {
                    enabled: options.scrollbars_enabled.unwrap_or(true),
                    theme: options.scrollbar_theme.unwrap_or_else(|| "auto".to_string()),
                    auto_hide: options.scrollbar_auto_hide.unwrap_or(true),
                    width: options.scrollbar_width.unwrap_or(8),
                };

                let mut effective_trusted_origins: Vec<String> = Vec::new();
                if let Some(list) = &options.trusted_origins {
                    for o in list {
                        if let Some(norm) = normalize_origin(o) {
                            if !effective_trusted_origins.contains(&norm) {
                                effective_trusted_origins.push(norm);
                            }
                        }
                    }
                    if options.root.is_some() && !effective_trusted_origins.contains(&"lotus-resource://localhost".to_string()) {
                        effective_trusted_origins.push("lotus-resource://localhost".to_string());
                    }
                } else {
                    effective_trusted_origins.push("lotus-resource://localhost".to_string());
                    if let Some(initial) = &options.initial_url {
                        if let Some(norm) = normalize_origin(initial) {
                            if !effective_trusted_origins.contains(&norm) {
                                effective_trusted_origins.push(norm);
                            }
                        }
                    }
                }

                let trusted_set = get_trusted_origins();
                for o in &effective_trusted_origins {
                    // Never insert opaque "null" — it must not grant trust via membership.
                    if crate::origin::origin_grants_trust(o) {
                        trusted_set.insert(o.clone());
                    }
                }

                let origins_json = serde_json::to_string(&effective_trusted_origins)
                    .unwrap_or_else(|_| "[\"lotus-resource://localhost\"]".to_string());

                // UNIFIED PIPELINE: Every window is a multi-pane layout.
                // If options.panes is empty, synthesize a single 'main' fill pane.
                let mut effective_panes = options.panes.clone();
                if effective_panes.is_empty() {
                    effective_panes.push(PaneOptions {
                        id: "main".to_string(),
                        url: options.initial_url.clone().unwrap_or_else(|| "about:blank".to_string()),
                        x: 0.0,
                        y: 0.0,
                        width: size.width as f64 / hidpi_scale_factor_val as f64,
                        height: size.height as f64 / hidpi_scale_factor_val as f64,
                        z_index: 0,
                        visible: true,
                        anchor: Some(if options.auto_resize_main { 1 } else { 0 }),
                        dock_order: Some(0),
                    });
                }

                for (i, pane_opt) in effective_panes.iter().enumerate() {
                    if i == 0 { primary_pane_id = pane_opt.id.clone(); }
                    
                    let p_rect = euclid::Rect::new(
                        euclid::Point2D::new(pane_opt.x as f32, pane_opt.y as f32),
                        euclid::Size2D::new(pane_opt.width as f32, pane_opt.height as f32)
                    );
                    
                    let p_physical_w = (p_rect.width() * hidpi_scale_factor_val).round() as u32;
                    let p_physical_h = (p_rect.height() * hidpi_scale_factor_val).round() as u32;
                    // OpenGL requires at least 1x1 for framebuffer completeness
                    let p_physical_w = p_physical_w.max(1);
                    let p_physical_h = p_physical_h.max(1);

                    let p_offscreen = Rc::new(crate::gl::LotusPaneContext::new(
                        rendering_context.clone(),
                        ServoPhysicalSize::new(p_physical_w, p_physical_h),
                    ));
                    let p_ready_to_repaint = Arc::new(AtomicBool::new(true));
                    let p_delegate = Rc::new(LotusPaneDelegate {
                        window: window.clone(),
                        window_id: window_id.clone(),
                        pane_id: pane_opt.id.clone(),
                        proxy: self.proxy.clone(),
                        ready_to_repaint: p_ready_to_repaint.clone(),
                    });
                    
                    let p_ucm = Rc::new(UserContentManager::new(&servo));
                    let theme_str = match mode { dark_light::Mode::Dark => "dark", _ => "light" };
                    populate_track1_ucm(&p_ucm, &PaneScriptEnv {
                        role: classify_pane_role(&pane_opt.id, false),
                        msgpackr_source: msgpackr_source.as_str(),
                        origins_json: &origins_json,
                        port,
                        token: &token,
                        window_id: &window_id,
                        pane_id: &pane_opt.id,
                        theme: Some(theme_str),
                        scrollbar: Some(&scrollbar_config),
                    });
                    
                    let mut p_builder = WebViewBuilder::new(&servo, p_offscreen.clone())
                        .delegate(p_delegate)
                        .hidpi_scale_factor(hidpi_scale_factor)
                        .user_content_manager(p_ucm);
                    
                    if let Ok(u) = url::Url::parse(&pane_opt.url) {
                        p_builder = p_builder.url(u);
                    }

                    let p_initial_size = ServoPhysicalSize::new(p_physical_w, p_physical_h);
                    let pane = PaneInstance {
                        id: pane_opt.id.clone(),
                        webview: p_builder.build(),
                        rect: p_rect,
                        last_notified_rect: None,
                        last_physical_rect: euclid::default::Rect::new(
                            euclid::default::Point2D::new(0, 0),
                            euclid::default::Size2D::new(p_physical_w as i32, p_physical_h as i32)
                        ),
                        z_index: pane_opt.z_index,
                        anchor: PaneAnchor::from(pane_opt.anchor.unwrap_or(0)),
                        dock_order: pane_opt.dock_order.unwrap_or(0),
                        animating: false,
                        is_dirty: true,
                        needs_repaint: false,
                        is_visible: false,
                        drag_regions: Vec::new(),
                        no_drag_regions: Vec::new(),
                        offscreen_ctx: p_offscreen,
                        servo_busy: false,
                        pending_servo_size: None,
                        requested_servo_size: p_initial_size,
                        current_servo_size: p_initial_size,
                        ready_to_repaint: p_ready_to_repaint,
                        ready_frame_size: None,
                        first_frame_painted: false,
                        first_frame_painted_time: None,
                        pending_physical_rect: None,
                        is_resizing: false,
                        pending_visible: pane_opt.visible,
                        ghost_tex: None,
                        ghost_tex_size: None,
                        is_remote: false,
                        remote_consumer: None,
                        remote_input_fd: None,
                        resize_generation: 0,
                        under_crash_cover: false,
                        await_host_composited_frame: false,
                    };
                    panes.insert(pane_opt.id.clone(), pane);
                }

                let mut id_shifters = Vec::new();
                let shifter = WebViewBuilder::new(&servo, rendering_context.clone()).build();
                let shifter_count = WEBVIEW_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                info!("Rust: Created Shifter WebView #{}", shifter_count);
                id_shifters.push(shifter);

                let initial_layout = StagedLayout {
                    panes: HashMap::new(),
                    width: size.width,
                    height: size.height,
                    scale_factor: hidpi_scale_factor_val,
                };

                let layout_root = {
                    let synth = || {
                        let layout_synth: Vec<(String, crate::layout::PaneAnchor, u32, f32, f32)> = panes
                            .iter()
                            .map(|(id, p)| {
                                (id.clone(), p.anchor, p.dock_order, p.rect.size.width, p.rect.size.height)
                            })
                            .collect();
                        crate::layout::synthesize_dock_root(&layout_synth)
                    };
                    if let Some(ref layout_str) = options.layout {
                        match serde_json::from_str::<serde_json::Value>(layout_str) {
                            Ok(layout_json) => match crate::layout::parse_layout_json(&layout_json) {
                                Ok(tree) => {
                                    if let Err(dup) = tree.validate_unique_ids() {
                                        error!("Rust: duplicate layout id '{}'; synthesizing", dup);
                                        synth()
                                    } else {
                                        tree
                                    }
                                }
                                Err(e) => {
                                    error!("Rust: failed to parse layout tree ({}); synthesizing", e);
                                    synth()
                                }
                            },
                            Err(e) => {
                                error!("Rust: layout JSON invalid ({}); synthesizing", e);
                                synth()
                            }
                        }
                    } else {
                        synth()
                    }
                };

                let mut instance = WindowInstance {
                    window_id: window_id.clone(),
                    panes,
                    active_layout: initial_layout.clone(),
                    staged_layout: initial_layout,
                    active_pane_id: primary_pane_id.clone(),
                    primary_pane_id,
                    last_mouse_down_pane_id: None,
                    rendering_context,
                    gl,
                    window: window.clone(),
                    last_mouse_pos: Point2D::new(0.0, 0.0),
                    is_mouse_down: false,
                    modifiers: Modifiers::default(),
                    frameless: options.frameless,
                    transparent: options.transparent,
                    in_resize_border: false,
                    auto_resize_main: options.auto_resize_main,
                    corner_radius: options.corner_radius,
                    id_shifters,
                    emitted_ready_to_show: false,
                    pending_stabilization: false,
                    stencil_program: None,
                    stencil_vao: None,
                    u_stencil_size_loc: None,
                    u_stencil_radius_loc: None,
                    comp_program: None,
                    comp_fbo: None,
                    comp_tex: None,
                    comp_vao: None,
                    comp_vbo: None,
                    u_comp_size_loc: None,
                    u_comp_radius_loc: None,
                    u_comp_flip_loc: None,
                    comp_tex_size: None,
                    scene_fbo: None,
                    scene_tex: None,
                    scene_tex_size: None,
                    committed_layout: None,
                    active_window_size: winit::dpi::PhysicalSize::new(size.width, size.height),
                    committed_window_size: None,
                    active_select: None,
                    scrollbar_config,
                    trusted_origins: effective_trusted_origins,
                    monitor_refresh_rate_millihertz: monitor_refresh_rate_millihertz(&window),
                    refresh_rate_millihertz: monitor_refresh_rate_millihertz(&window),
                    os_focused: true,
                    os_occluded: false,
                    os_minimized: window.is_minimized().unwrap_or(false),
                    zero_size: size.width == 0 || size.height == 0,
                    layout_root,
                    layout_batch_depth: 0,
                    gutters: Vec::new(),
                    active_gutter_drag: None,
                    pending_gutter_delta: None,
                    gutter_hover: None,
                    hover_pane_id: None,
                };

                instance.init_stencil_program();
                instance.init_composition_resources();

                instance.recalculate_layout(winit::dpi::PhysicalSize::new(size.width, size.height), hidpi_scale_factor_val);

                self.windows.insert(window_id.clone(), instance);
                self.winit_id_to_uuid.insert(winit_id, window_id.clone());

                if let Some(state) = APP_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        s.window_metadata.insert(window_id.clone(), WindowMetadata {
                            root_path: options.root.clone().map(PathBuf::from),
                            last_window_size: Some(size),
                        });

                    }
                }

                if options.visible {
                    window.set_visible(true);
                }
                window.request_redraw();

                if let Some(state) = APP_STATE.get() {
                    if let Ok(s) = state.lock() {
                        if s.profiling {
                            eprintln!("[PROFILE] Window {} ready in {:?}", window_id, creation_start.elapsed());
                        }
                    }
                }

                let actual_size = window.inner_size();
                let actual_scale = window.scale_factor() as f32;
                if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                    "event": "_internal-created",
                    "window_id": window_id,
                    "width": actual_size.width,
                    "height": actual_size.height,
                    "scale_factor": actual_scale,
                    "logicalWidth": actual_size.width as f32 / actual_scale,
                    "logicalHeight": actual_size.height as f32 / actual_scale,
                    "maximized": window.is_maximized()
                })) {
                    let mut wrapped = Vec::with_capacity(msg.len() + 1);
                    wrapped.push(MSG_TYPE_DATA);
                    wrapped.extend(msg);
                    self.callback.call((window_id.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                }

                info!("Window created successfully: {}", window_id);
            },
            EngineCommand::Quit => {
                info!("Rust: [Teardown] Quit command received. Starting 1s safety watchdog.");
                // Start a watchdog immediately in case the event loop hangs during teardown
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                    eprintln!("Rust: [Teardown] Watchdog timeout. Forcing fast exit.");
                    fast_exit(0);
                });
                event_loop.exit();
            },
            EngineCommand::IpcMessage(window_id, raw_bytes) => {
                // For singular messages (mostly from internal Rust sources),
                // we ensure the MSG_TYPE_DATA header is present so JS doesn't have to guess.
                let (msg_type, payload) = if !raw_bytes.is_empty() {
                    (raw_bytes[0], &raw_bytes[1..])
                } else {
                    (MSG_TYPE_DATA, &raw_bytes[..])
                };

                if msg_type == MSG_TYPE_CONTROL {
                    intercept_drag_regions(payload, window_id.clone());
                }

                if !raw_bytes.is_empty() && (raw_bytes[0] == MSG_TYPE_CONTROL || raw_bytes[0] == MSG_TYPE_DATA || raw_bytes[0] == 0x03 || raw_bytes[0] == b'{') {
                    self.callback.call((window_id, vec![raw_bytes.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                } else {
                    let mut wrapped = Vec::with_capacity(raw_bytes.len() + 1);
                    wrapped.push(MSG_TYPE_DATA);
                    wrapped.extend(raw_bytes);
                    self.callback.call((window_id, vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                }
            },
            EngineCommand::IpcMessages(window_id, messages) => {
                let mut processed_messages = Vec::with_capacity(messages.len());
                for raw_bytes in messages {
                    // Pre-process messages to intercept known internal Lotus commands.
                    // The 1-byte header allows us to skip O(N) scanning for almost all messages.
                    let (msg_type, payload) = if raw_bytes.len() > 0 {
                        (raw_bytes[0], &raw_bytes[1..])
                    } else {
                        (MSG_TYPE_DATA, &raw_bytes[..])
                    };

                    info!("Rust: Processing IPC message from {}. Type: 0x{:02x}, Payload len: {}", window_id, msg_type, payload.len());

                    if msg_type == MSG_TYPE_CONTROL {
                        intercept_drag_regions(payload, window_id.clone());
                    }

                    // Forward the raw bytes (including the 1-byte header) to Node.js.
                    // This allows JS to handle both standard and chunked (0x03) messages.
                    processed_messages.push(raw_bytes.into());
                }
                
                if !processed_messages.is_empty() {
                    self.callback.call((window_id, processed_messages), ThreadsafeFunctionCallMode::NonBlocking);
                }
            },
            EngineCommand::LoadUrl(window_id, pane_id, url) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    let pid = if pane_id == "main" && !instance.panes.contains_key("main") {
                        &instance.primary_pane_id
                    } else {
                        &pane_id
                    };
                    
                    if let Some(pane) = instance.panes.get(pid) {
                        if let Ok(u) = url::Url::parse(&url) {
                            pane.webview.load(u);
                        }
                    }
                }
            },
            EngineCommand::Resize(window_id, size) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    let _ = instance.window.request_inner_size(size);
                    instance.rendering_context.resize(size);
                }
            },
            EngineCommand::SetPosition(window_id, position) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_outer_position(position);
                }
            },
            EngineCommand::SetAlwaysOnTop(window_id, flag) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    platform::set_always_on_top(&instance.window, flag);
                }
            },
            EngineCommand::RequestAttention(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    platform::request_attention(&instance.window);
                }
            },
            EngineCommand::SetTitle(window_id, title) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_title(&title);
                }
            },
            EngineCommand::CloseWindow(window_id) => {
                self.winit_id_to_uuid.retain(|_, v| v != &window_id);
                self.windows.remove(&window_id);
                // Clean up per-window metadata so long-running apps don't leak.
                if let Some(state) = APP_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        s.window_metadata.remove(&window_id);
                        s.window_start_times.remove(&window_id);
                    }
                }
                // Drop any buffered outgoing frames for this window.
                if let Some(p) = WS_PENDING.get() { p.remove(&window_id); }
                info!("Closed window: {}", window_id);

                let mut close_msg = Vec::new();
                if rmp_serde::encode::write(&mut close_msg, &serde_json::json!({"event": "window-closed", "window_id": window_id})).is_ok() {
                    let mut wrapped = Vec::with_capacity(close_msg.len() + 1);
                    wrapped.push(MSG_TYPE_DATA);
                    wrapped.extend(close_msg);
                    self.callback.call((window_id.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                }

                if self.windows.is_empty() {
                    info!("Rust: Last window closed via CloseWindow command, exiting event loop");
                    std::thread::spawn(|| {
                        std::thread::sleep(std::time::Duration::from_millis(1000));
                        fast_exit(0);
                    });
                    event_loop.exit();
                }
            },
            EngineCommand::SetDecorations(window_id, decorations) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_decorations(decorations);
                }
            },
            EngineCommand::ExecuteScript(window_id, pane_id, script) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    let pid = if pane_id == "main" && !instance.panes.contains_key("main") {
                        instance.primary_pane_id.clone()
                    } else {
                        pane_id.clone()
                    };

                    if let Some(pane) = instance.panes.get(&pid) {
                        info!("Rust: [IPC] Executing script in window '{}' pane '{}' (len: {})", window_id, pid, script.len());
                        pane.webview.evaluate_javascript(&script, |_| {});
                    } else {
                        warn!("Rust: ExecuteScript failed - pane '{}' not found in window '{}'", pid, window_id);
                    }
                }
            },            EngineCommand::ShowWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_visible(true);
                    instance.window.request_redraw();
                }
            },
            EngineCommand::HideWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_visible(false);
                }
            },
            EngineCommand::UpdateDragRegions(window_id, pane_id, drag_regions, no_drag_regions) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        pane.drag_regions = drag_regions;
                        pane.no_drag_regions = no_drag_regions;
                    }
                }
            },
            EngineCommand::MinimizeWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_minimized(true);
                }
            },
            EngineCommand::UnminimizeWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_minimized(false);
                }
            },
            EngineCommand::MaximizeWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_maximized(true);
                }
            },
            EngineCommand::UnmaximizeWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_maximized(false);
                }
            },
            EngineCommand::FocusWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.focus_window();
                }
            },
            EngineCommand::AnimatingChanged(window_id, pane_id, animating) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        pane.animating = animating;
                        trace!("Rust: Window {} Pane {} animating={}", window_id, pane_id, animating);
                    }
                }
            },
            EngineCommand::NewFrameReady(window_id, pane_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        trace!("Rust: [Lifecycle] EngineCommand::NewFrameReady for window {} pane {}", window_id, pane_id);
                        
                        // Unlock BEFORE painting to allow Servo to begin preparing the NEXT frame
                        pane.ready_to_repaint.store(true, Ordering::SeqCst);
                        
                        // Accept the frame Servo just spent time rendering!
                        pane.current_servo_size = pane.requested_servo_size;
                        pane.ready_frame_size = Some(pane.current_servo_size);
                        pane.needs_repaint = true;
                        
                        let was_first_frame = !pane.first_frame_painted;
                        if was_first_frame {
                            pane.first_frame_painted_time = Some(std::time::Instant::now());
                        }
                        pane.first_frame_painted = true;
                        pane.is_dirty = false;

                        // LOCK STEP: Do not dispatch webview.resize here!
                        // RedrawRequested will paint this frame into the GPU FBO, flip layout, and present.
                        // Only after presentation is complete will the next queued resize step be dispatched.
                        instance.window.request_redraw();

                        // AGGREGATION FIX: Only emit when all visible panes have painted
                        if !instance.emitted_ready_to_show {
                                let all_ready = instance.panes.values().all(|p| !p.pending_visible || p.first_frame_painted);
                                
                                if all_ready {
                                    instance.emitted_ready_to_show = true;
                                    
                                    if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                                        "event": "ready-to-show",
                                        "window_id": window_id,
                                        "pane_id": pane_id
                                    })) {
                                        let mut wrapped = Vec::with_capacity(msg.len() + 1);
                                        wrapped.push(MSG_TYPE_DATA);
                                        wrapped.extend(msg);
                                        self.callback.call((window_id.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                                    }
                                }
                            }
                        instance.window.request_redraw();
                    }
                }
            },
            EngineCommand::RedrawWindow(window_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    sync_minimized_from_os(instance);
                    apply_refresh_policy(instance);
                    // FramePublished → requestRedraw must not wake the compositor when
                    // hidden: skipping RedrawRequested without present busy-spins Wayland.
                    if window_is_viewable(instance) {
                        instance.window.request_redraw();
                    }
                }
            },
            EngineCommand::SetMinInnerSize(window_id, size) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_min_inner_size(size);
                }
            },
            EngineCommand::SetMaxInnerSize(window_id, size) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_max_inner_size(size);
                }
            },
            EngineCommand::CreatePane(window_id, pane_id, url, rect, z_index, anchor, dock_order, visible) => {
                let servo = self.ensure_servo().clone();
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if pane_id == "main" {
                        instance.auto_resize_main = false;
                    }

                    let ready_to_repaint = Arc::new(AtomicBool::new(true));
                    let delegate = Rc::new(LotusPaneDelegate {
                        window: instance.window.clone(),
                        window_id: window_id.clone(),
                        pane_id: pane_id.clone(),
                        proxy: self.proxy.clone(),
                        ready_to_repaint: ready_to_repaint.clone(),
                    });                    
                    let hidpi_scale_factor_val = instance.window.scale_factor() as f32;
                    let hidpi_scale_factor = Scale::<f32, DeviceIndependentPixel, DevicePixel>::new(hidpi_scale_factor_val);
                    
                    let user_content_manager = Rc::new(UserContentManager::new(&servo));
                    
                    let (msgpackr_source, port, token) = if let Some(state) = APP_STATE.get() {
                        if let Ok(s) = state.lock() {
                            (s.msgpackr_source.clone(), s.ipc_server_port, s.ipc_server_token.clone())
                        } else {
                            ("".to_string(), 0, "".to_string())
                        }
                    } else {
                        ("".to_string(), 0, "".to_string())
                    };

                    let origins_json = serde_json::to_string(&instance.trusted_origins)
                        .unwrap_or_else(|_| "[\"lotus-resource://localhost\"]".to_string());

                    populate_track1_ucm(&user_content_manager, &PaneScriptEnv {
                        role: classify_pane_role(&pane_id, false),
                        msgpackr_source: msgpackr_source.as_str(),
                        origins_json: &origins_json,
                        port,
                        token: &token,
                        window_id: &window_id,
                        pane_id: &pane_id,
                        theme: None,
                        scrollbar: Some(&instance.scrollbar_config),
                    });

                    let servo_size = ServoPhysicalSize::new(
                        (rect.size.width * hidpi_scale_factor_val).round() as u32,
                        (rect.size.height * hidpi_scale_factor_val).round() as u32
                    );
                    // OpenGL requires at least 1x1 for framebuffer completeness
                    let servo_size = ServoPhysicalSize::new(
                        servo_size.width.max(1),
                        servo_size.height.max(1)
                    );
                    let offscreen_ctx = Rc::new(crate::gl::LotusPaneContext::new(
                        instance.rendering_context.clone(),
                        servo_size,
                    ));

                    let mut webview_builder = WebViewBuilder::new(&servo, offscreen_ctx.clone())
                        .delegate(delegate)
                        .hidpi_scale_factor(hidpi_scale_factor)
                        .user_content_manager(user_content_manager);

                    // Force unique ID by keeping a shifter alive
                    let shifter = WebViewBuilder::new(&servo, instance.rendering_context.clone()).build();
                    let shifter_count = WEBVIEW_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    info!("Rust: Created Shifter WebView #{}", shifter_count);
                    instance.id_shifters.push(shifter);

                    if let Ok(u) = url::Url::parse(&url) {
                        webview_builder = webview_builder.url(u);
                    }

                    let webview = webview_builder.build();
                    let count = WEBVIEW_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    info!("Rust: Created additional WebView #{} for pane '{}'", count, pane_id);
                    
                    let pane = PaneInstance {
                        id: pane_id.clone(),
                        webview,
                        rect,
                        last_notified_rect: None,
                        last_physical_rect: euclid::default::Rect::new(
                            euclid::default::Point2D::new(0, 0),
                            euclid::default::Size2D::new(servo_size.width as i32, servo_size.height as i32)
                        ),
                        z_index,
                        anchor,
                        dock_order,
                        animating: false,
                        is_dirty: true,
                        needs_repaint: false,
                        is_visible: false,
                        drag_regions: Vec::new(),

                        no_drag_regions: Vec::new(),
                        offscreen_ctx,
                        servo_busy: false,
                        pending_servo_size: None,
                        requested_servo_size: servo_size,
                        current_servo_size: servo_size,
                        ready_to_repaint,
                        ready_frame_size: None,
                        first_frame_painted: false,
                        first_frame_painted_time: None,
                        pending_physical_rect: None,
                        is_resizing: false,
                        pending_visible: visible,
                        ghost_tex: None,
                        ghost_tex_size: None,
                        is_remote: false,
                        remote_consumer: None,
                        remote_input_fd: None,
                        resize_generation: 0,
                        under_crash_cover: false,
                        await_host_composited_frame: false,
                    };
                    instance.panes.insert(pane_id, pane);
                    let physical_size = instance.window.inner_size();
                    let scale_factor = instance.window.scale_factor() as f32;
                    instance.recalculate_layout(physical_size, scale_factor);
                    instance.window.request_redraw();
                }
            },
            EngineCommand::CreateRemotePane {
                window_id,
                pane_id,
                ro_fd,
                shm_size,
                ack_rw_fd,
                rect,
                z_index,
                anchor,
                dock_order,
                visible,
                max_shm_width,
                max_shm_height,
            } => {
                let servo = self.ensure_servo().clone();
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if pane_id == "main" {
                        instance.auto_resize_main = false;
                    }

                    let hidpi_scale_factor_val = instance.window.scale_factor() as f32;
                    let width = (rect.size.width * hidpi_scale_factor_val).round() as u32;
                    let height = (rect.size.height * hidpi_scale_factor_val).round() as u32;
                    let max_w = max_shm_width.unwrap_or(width).max(1);
                    let max_h = max_shm_height.unwrap_or(height).max(1);
                    let servo_size = ServoPhysicalSize::new(width.max(1), height.max(1));
                    let offscreen_ctx = Rc::new(crate::gl::LotusPaneContext::new(
                        instance.rendering_context.clone(),
                        servo_size,
                    ));

                    let consumer = match RemotePaneConsumer::new(ro_fd, shm_size, ack_rw_fd, max_w, max_h) {
                        Ok(c) => Some(c),
                        Err(e) => {
                            error!("Rust: Failed to create RemotePaneConsumer for pane '{}': {:?}", pane_id, e);
                            None
                        }
                    };

                    let dummy_webview = WebViewBuilder::new(&servo, offscreen_ctx.clone()).build();

                    let pane = PaneInstance {
                        id: pane_id.clone(),
                        webview: dummy_webview,
                        rect,
                        last_notified_rect: None,
                        last_physical_rect: euclid::default::Rect::new(
                            euclid::default::Point2D::new((rect.origin.x * hidpi_scale_factor_val).round() as i32, (rect.origin.y * hidpi_scale_factor_val).round() as i32),
                            euclid::default::Size2D::new(servo_size.width as i32, servo_size.height as i32),
                        ),
                        z_index,
                        anchor,
                        dock_order,
                        animating: false,
                        is_dirty: true,
                        // Remotes never take the local paint path; leave needs_repaint
                        // false so lock-step dispatch is not permanently blocked.
                        needs_repaint: false,
                        is_visible: visible,
                        drag_regions: Vec::new(),
                        no_drag_regions: Vec::new(),
                        offscreen_ctx,
                        servo_busy: false,
                        pending_servo_size: None,
                        requested_servo_size: servo_size,
                        current_servo_size: ServoPhysicalSize::new(0, 0),
                        ready_to_repaint: Arc::new(AtomicBool::new(true)),
                        ready_frame_size: None,
                        // Wait for first matching SHM frame via lock-step (same as locals).
                        first_frame_painted: false,
                        first_frame_painted_time: None,
                        pending_physical_rect: None,
                        is_resizing: false,
                        pending_visible: visible,
                        ghost_tex: None,
                        ghost_tex_size: None,
                        is_remote: true,
                        remote_consumer: consumer,
                        remote_input_fd: None,
                        resize_generation: 0,
                        under_crash_cover: false,
                        await_host_composited_frame: false,
                    };
                    let fixed = match anchor {
                        PaneAnchor::Left | PaneAnchor::Right => rect.size.width,
                        PaneAnchor::Top | PaneAnchor::Bottom => rect.size.height,
                        _ => 0.0,
                    };
                    crate::layout::ensure_dock_leaf(
                        &mut instance.layout_root,
                        &pane_id,
                        anchor,
                        fixed,
                        dock_order as usize,
                    );
                    instance.panes.insert(pane_id, pane);
                    let physical_size = instance.window.inner_size();
                    let scale_factor = instance.window.scale_factor() as f32;
                    instance.recalculate_layout(physical_size, scale_factor);
                    instance.window.request_redraw();
                }
            },
            EngineCommand::RemovePane(window_id, pane_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    let _ = instance.panes.remove(&pane_id);
                    // Focus fallback: If the removed pane was active, move focus to primary.
                    if instance.active_pane_id == pane_id {
                        instance.active_pane_id = instance.primary_pane_id.clone();
                    }
                }
            },
            EngineCommand::SetPaneRect(window_id, pane_id, rect) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if pane_id == "main" {
                        instance.auto_resize_main = false;
                    }
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        let scale_factor = instance.window.scale_factor() as f32;
                        let size_changed = pane.last_notified_rect.map(|r| r.size != rect.size).unwrap_or(true);
                        
                        if !size_changed && !pane.is_dirty && pane.last_notified_rect == Some(rect) {
                            return;
                        }
                        
                        pane.rect = rect;
                        pane.last_notified_rect = Some(rect);
                        
                        let pane_phys = euclid::default::Rect::new(
                            euclid::default::Point2D::new((rect.origin.x * scale_factor).round() as i32, (rect.origin.y * scale_factor).round() as i32),
                            euclid::default::Size2D::new((rect.size.width * scale_factor).round() as i32, (rect.size.height * scale_factor).round() as i32)
                        );
                        pane.pending_physical_rect = Some(pane_phys);
                        pane.anchor = PaneAnchor::None;

                        let servo_size = ServoPhysicalSize::new(
                            ((rect.size.width * scale_factor).round() as u32).max(1),
                            ((rect.size.height * scale_factor).round() as u32).max(1)
                        );
                        
                        debug!("Rust: SetPaneRect for '{}' -> {:?} (Physical: {}x{})", pane_id, rect, servo_size.width, servo_size.height);
                        
                        if size_changed || pane.is_dirty {
                            pane.is_resizing = true;
                            if size_changed {
                                // Physical resize: Hold the Nuclear Flip until Servo responds
                                pane.pending_servo_size = Some(servo_size);
                            }
                        } else {
                            // No change needed, but ensure we don't leave it in resizing state
                            pane.is_resizing = false;
                            pane.pending_physical_rect = None;
                        }
                    }
                    instance.layout_root.set_leaf_detached(&pane_id, true);
                    let physical_size = instance.window.inner_size();
                    let scale_factor = instance.window.scale_factor() as f32;
                    instance.recalculate_layout(physical_size, scale_factor);
                    instance.window.request_redraw();
                }
            },
            EngineCommand::SetPaneSize(window_id, pane_id, width, height) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    let size = if width.is_finite() && width > 0.0 {
                        width
                    } else if height.is_finite() && height > 0.0 {
                        height
                    } else {
                        1.0
                    };
                    let tree_changed = instance.layout_root.set_leaf_fixed_size(&pane_id, size);
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        match pane.anchor {
                            PaneAnchor::Left | PaneAnchor::Right => {
                                if width.is_finite() {
                                    pane.rect.size.width = width.max(1.0);
                                }
                            }
                            PaneAnchor::Top | PaneAnchor::Bottom => {
                                if height.is_finite() {
                                    pane.rect.size.height = height.max(1.0);
                                }
                            }
                            PaneAnchor::Fill | PaneAnchor::None => {}
                        }
                        if tree_changed {
                            pane.is_dirty = true;
                        }
                    }
                    if tree_changed && instance.layout_batch_depth == 0 {
                        let physical_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;
                        debug!("Rust: SetPaneSize for '{}' -> tree Fixed({})", pane_id, size);
                        instance.recalculate_layout(physical_size, scale_factor);
                        instance.window.request_redraw();
                    }
                }
            },
            EngineCommand::SetPaneWeight(window_id, pane_id, weight) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if instance.layout_root.set_leaf_weight(&pane_id, weight) {
                        if let Some(pane) = instance.panes.get_mut(&pane_id) {
                            pane.is_dirty = true;
                        }
                        if instance.layout_batch_depth == 0 {
                            let physical_size = instance.window.inner_size();
                            let scale_factor = instance.window.scale_factor() as f32;
                            instance.recalculate_layout(physical_size, scale_factor);
                            instance.window.request_redraw();
                        }
                    }
                }
            },
            EngineCommand::SetSplitWeights(window_id, split_id, payload_json) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    let ok = match serde_json::from_str::<serde_json::Value>(&payload_json) {
                        Ok(payload) => {
                            if let Some(arr) = payload.as_array() {
                                let vals: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|x| x as f32)).collect();
                                instance.layout_root.set_split_weights_list(&split_id, &vals)
                            } else if let Some(obj) = payload.as_object() {
                                let mut map = std::collections::HashMap::new();
                                for (k, v) in obj {
                                    if let Some(n) = v.as_f64() {
                                        map.insert(k.clone(), n as f32);
                                    }
                                }
                                instance.layout_root.set_split_weights_map(&split_id, &map)
                            } else {
                                false
                            }
                        }
                        Err(_) => false,
                    };
                    if ok && instance.layout_batch_depth == 0 {
                        let physical_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;
                        instance.recalculate_layout(physical_size, scale_factor);
                        instance.window.request_redraw();
                    }
                }
            },
            EngineCommand::SetSplitSizes(window_id, split_id, payload_json) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    let ok = match serde_json::from_str::<serde_json::Value>(&payload_json) {
                        Ok(payload) => {
                            if let Some(arr) = payload.as_array() {
                                let vals: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|x| x as f32)).collect();
                                instance.layout_root.set_split_sizes_list(&split_id, &vals)
                            } else if let Some(obj) = payload.as_object() {
                                let mut map = std::collections::HashMap::new();
                                for (k, v) in obj {
                                    if let Some(n) = v.as_f64() {
                                        map.insert(k.clone(), n as f32);
                                    }
                                }
                                instance.layout_root.set_split_sizes_map(&split_id, &map)
                            } else {
                                false
                            }
                        }
                        Err(_) => false,
                    };
                    if ok && instance.layout_batch_depth == 0 {
                        let physical_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;
                        instance.recalculate_layout(physical_size, scale_factor);
                        instance.window.request_redraw();
                    }
                }
            },
            EngineCommand::BeginLayoutBatch(window_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    instance.layout_batch_depth = instance.layout_batch_depth.saturating_add(1);
                }
            },
            EngineCommand::EndLayoutBatch(window_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    instance.layout_batch_depth = instance.layout_batch_depth.saturating_sub(1);
                    if instance.layout_batch_depth == 0 {
                        let physical_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;
                        instance.recalculate_layout(physical_size, scale_factor);
                        instance.window.request_redraw();
                    }
                }
            },
            EngineCommand::SetPaneVisible(window_id, pane_id, visible) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    instance.layout_root.set_leaf_visible(&pane_id, visible);
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        pane.pending_visible = visible;
                        pane.is_dirty = true;
                    }
                    let physical_size = instance.window.inner_size();
                    let scale_factor = instance.window.scale_factor() as f32;
                    instance.recalculate_layout(physical_size, scale_factor);
                    instance.window.request_redraw();
                }
            },
            EngineCommand::CoverRemoteWithCrashChrome {
                window_id,
                remote_pane_id,
                crash_pane_id,
            } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    let scale_factor = instance.window.scale_factor() as f32;
                    // Prefer the remote's laid-out slot; if it was hidden/zero, fall back to
                    // the crash pane's current rect (it may already be holding the dock slot).
                    let cover_phys = {
                        let remote_phys = instance
                            .panes
                            .get(&remote_pane_id)
                            .map(|p| p.last_physical_rect)
                            .filter(|r| r.size.width > 0 && r.size.height > 0);
                        let crash_phys = instance
                            .panes
                            .get(&crash_pane_id)
                            .map(|p| p.last_physical_rect)
                            .filter(|r| r.size.width > 0 && r.size.height > 0);
                        remote_phys.or(crash_phys)
                    };

                    if let Some(phys) = cover_phys {
                        let logical = euclid::Rect::new(
                            euclid::Point2D::new(
                                phys.origin.x as f32 / scale_factor,
                                phys.origin.y as f32 / scale_factor,
                            ),
                            euclid::Size2D::new(
                                phys.size.width as f32 / scale_factor,
                                phys.size.height as f32 / scale_factor,
                            ),
                        );
                        if let Some(crash) = instance.panes.get_mut(&crash_pane_id) {
                            crash.rect = logical;
                            crash.last_notified_rect = Some(logical);
                            crash.anchor = PaneAnchor::None;
                            // Below select overlay (i32::MAX) but above normal content.
                            crash.z_index = 2_000_000;
                            crash.pending_visible = true;
                            crash.is_dirty = true;
                            crash.pending_physical_rect = Some(phys);
                        }
                    } else if let Some(crash) = instance.panes.get_mut(&crash_pane_id) {
                        crash.anchor = PaneAnchor::None;
                        crash.z_index = 2_000_000;
                        crash.pending_visible = true;
                        crash.is_dirty = true;
                    }

                    let _ = instance.rendering_context.make_current();
                    if let Some(remote) = instance.panes.get_mut(&remote_pane_id) {
                        // Stay in the dock / compositor so the successor engine gets vsync.
                        remote.pending_visible = true;
                        remote.is_dirty = true;
                        remote.under_crash_cover = true;
                        if let Some(consumer) = remote.remote_consumer.as_mut() {
                            consumer.invalidate_cached_frame(&instance.gl);
                        }
                    }

                    let physical_size = instance.window.inner_size();
                    instance.recalculate_layout(physical_size, scale_factor);
                    instance.window.request_redraw();
                }
            },
            EngineCommand::ArmCrashCoverFreshFrame {
                window_id,
                remote_pane_id,
            } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    let _ = instance.rendering_context.make_current();
                    if let Some(remote) = instance.panes.get_mut(&remote_pane_id) {
                        if let Some(old_fd) = remote.remote_input_fd.take() {
                            if old_fd >= 0 {
                                unsafe { libc::close(old_fd) };
                            }
                        }
                        // Do not force under_crash_cover here — silent autoRespawn has no
                        // chrome and must keep drawing; cover mode already set the flag.
                        remote.await_host_composited_frame = true;
                        remote.current_servo_size = ServoPhysicalSize::new(0, 0);
                        remote.resize_generation = 1;
                        remote.servo_busy = false;
                        remote.ready_frame_size = None;
                        remote.first_frame_painted = false;
                        let target_w = remote.last_physical_rect.size.width.max(1) as u32;
                        let target_h = remote.last_physical_rect.size.height.max(1) as u32;
                        remote.requested_servo_size = ServoPhysicalSize::new(target_w, target_h);
                        if let Some(consumer) = remote.remote_consumer.as_mut() {
                            // CreateSurface resets pub_seq to 0 — align so we only accept
                            // post-respawn publishes, never prior-life READY slots.
                            consumer.invalidate_cached_frame(&instance.gl);
                            consumer.last_rendered_seq = 0;
                            consumer.update_active_resize_gen(1);
                        }
                    }
                    instance.window.request_redraw();
                }
            },
            EngineCommand::ClearRemoteCrashCover {
                window_id,
                remote_pane_id,
            } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(remote) = instance.panes.get_mut(&remote_pane_id) {
                        remote.under_crash_cover = false;
                        remote.await_host_composited_frame = false;
                    }
                    instance.window.request_redraw();
                }
            },
            EngineCommand::FocusPane(window_id, pane_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    instance.active_pane_id = pane_id;
                }
            },
            EngineCommand::AttachRemotePaneInput(window_id, pane_id, fd) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    let mut attached_fd: Option<std::os::unix::io::RawFd> = None;
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        if let Some(old_fd) = pane.remote_input_fd.replace(fd) {
                            if old_fd >= 0 {
                                unsafe { libc::close(old_fd) };
                            }
                        }
                        pane.resize_generation = 1;
                        pane.servo_busy = false;
                        let target_w = pane.last_physical_rect.size.width.max(1) as u32;
                        let target_h = pane.last_physical_rect.size.height.max(1) as u32;
                        pane.requested_servo_size = ServoPhysicalSize::new(target_w, target_h);
                        if !pane.first_frame_painted {
                            pane.current_servo_size = ServoPhysicalSize::new(0, 0);
                            pane.ready_frame_size = None;
                            if let Some(consumer) = pane.remote_consumer.as_mut() {
                                consumer.update_active_resize_gen(1);
                                consumer.last_rendered_seq = 0;
                            }
                        } else if let Some(consumer) = pane.remote_consumer.as_mut() {
                            consumer.update_active_resize_gen(1);
                        }
                        attached_fd = Some(fd);
                    } else {
                        unsafe { libc::close(fd); }
                    }
                    if let Some(ctrl_fd) = attached_fd {
                        sync_minimized_from_os(instance);
                        apply_refresh_policy(instance);
                        send_remote_display_timing(ctrl_fd, instance.refresh_rate_millihertz);
                        let presentation_time_ns = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_nanos() as u64)
                            .unwrap_or(0);
                        let surface_id = format!("{}:{}", window_id, pane_id);
                        send_remote_vsync(ctrl_fd, &surface_id, presentation_time_ns);
                        let physical_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;
                        instance.recalculate_layout(physical_size, scale_factor);
                        instance.window.request_redraw();
                    }
                } else {
                    unsafe { libc::close(fd); }
                }
            },
            EngineCommand::ShowSelectControl { window_id, pane_id, prompt } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    // If an existing select control was open, submit or dismiss it
                    if let Some(mut prev_active) = instance.active_select.take() {
                        instance.active_pane_id = prev_active.previous_active_pane_id;
                        if prev_active.allow_multiple {
                            prev_active.prompt.submit();
                        } else {
                            prev_active.prompt.select(prev_active.initial_selected_options.clone());
                            prev_active.prompt.submit();
                        }
                    }

                    let scale_factor = instance.window.scale_factor() as f32;
                    let (pane_px, pane_py) = if let Some(pane) = instance.panes.get(&pane_id) {
                        let (x, y, _, _) = pane.input_physical_rect(scale_factor);
                        (x, y)
                    } else {
                        (0.0, 0.0)
                    };

                    let control_id = prompt.0.id();
                    let control_id_str = format!("{:?}", control_id);
                    let pos = prompt.0.position();
                    let window_phys_x = pane_px + pos.min.x as f32;
                    let window_phys_y = pane_py + pos.min.y as f32;
                    let window_phys_w = pos.width() as f32;
                    let window_phys_h = pos.height() as f32;

                    let logical_x = window_phys_x / scale_factor;
                    let logical_y = window_phys_y / scale_factor;
                    let logical_w = window_phys_w / scale_factor;
                    let logical_h = window_phys_h / scale_factor;

                    let formatted_options: Vec<serde_json::Value> = prompt.0.options().iter().map(|item| {
                        match item {
                            servo::SelectElementOptionOrOptgroup::Option(opt) => {
                                serde_json::json!({
                                    "type": "option",
                                    "id": opt.id,
                                    "label": opt.label,
                                    "is_disabled": opt.is_disabled,
                                })
                            },
                            servo::SelectElementOptionOrOptgroup::Optgroup { label, options } => {
                                let opts: Vec<serde_json::Value> = options.iter().map(|opt| {
                                    serde_json::json!({
                                        "id": opt.id,
                                        "label": opt.label,
                                        "is_disabled": opt.is_disabled,
                                    })
                                }).collect();
                                serde_json::json!({
                                    "type": "optgroup",
                                    "label": label,
                                    "options": opts,
                                })
                            }
                        }
                    }).collect();

                    let selected_options = prompt.0.selected_options();
                    let allow_multiple = prompt.0.allow_select_multiple();

                    let previous_active_pane_id = instance.active_pane_id.clone();
                    instance.active_pane_id = "__lotus_select_overlay".to_string();

                    instance.active_select = Some(ActiveSelectPrompt {
                        control_id,
                        control_id_str: control_id_str.clone(),
                        pane_id: pane_id.clone(),
                        previous_active_pane_id,
                        initial_selected_options: selected_options.clone(),
                        prompt: prompt.0,
                        allow_multiple,
                    });

                    let payload = serde_json::json!({
                        "event": "select-element-open",
                        "window_id": window_id,
                        "pane_id": pane_id,
                        "id": control_id_str,
                        "rect": {
                            "x": logical_x,
                            "y": logical_y,
                            "width": logical_w,
                            "height": logical_h
                        },
                        "options": formatted_options,
                        "selectedOptions": selected_options,
                        "allowMultiple": allow_multiple
                    });

                    if let Ok(msg) = rmp_serde::encode::to_vec(&payload) {
                        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                            let _ = proxy.send_event(EngineCommand::IpcMessage(window_id.clone(), msg));
                        }
                    }
                }
            },
            EngineCommand::HideSelectControl { window_id, control_id } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(active) = &instance.active_select {
                        if active.control_id == control_id {
                            let mut active = instance.active_select.take().unwrap();
                            instance.active_pane_id = active.previous_active_pane_id;
                            if active.allow_multiple {
                                active.prompt.submit();
                            } else {
                                active.prompt.select(active.initial_selected_options.clone());
                                active.prompt.submit();
                            }
                            if let Some(overlay) = instance.panes.get_mut("__lotus_select_overlay") {
                                overlay.pending_visible = false;
                                overlay.is_dirty = true;
                                let physical_size = instance.window.inner_size();
                                let scale_factor = instance.window.scale_factor() as f32;
                                instance.recalculate_layout(physical_size, scale_factor);
                                instance.window.request_redraw();
                            }
                            if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                                "event": "select-element-close",
                                "window_id": window_id,
                            })) {
                                if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                                    let _ = proxy.send_event(EngineCommand::IpcMessage(window_id.clone(), msg));
                                }
                            }
                        }
                    }
                }
            },
            EngineCommand::UpdateSelectControl { window_id, control_id, selected_indices } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(active) = &mut instance.active_select {
                        if active.control_id_str == control_id || control_id.is_empty() {
                            let indices: Vec<usize> = selected_indices.into_iter().map(|i| i as usize).collect();
                            active.prompt.select(indices);
                        }
                    }
                }
            },
            EngineCommand::SubmitSelectControl { window_id, control_id: _, selected_indices } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(mut active) = instance.active_select.take() {
                        instance.active_pane_id = active.previous_active_pane_id;
                        let indices: Vec<usize> = selected_indices.into_iter().map(|i| i as usize).collect();
                        active.prompt.select(indices);
                        active.prompt.submit();
                    }
                    if let Some(overlay) = instance.panes.get_mut("__lotus_select_overlay") {
                        overlay.pending_visible = false;
                        overlay.is_dirty = true;
                        let physical_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;
                        instance.recalculate_layout(physical_size, scale_factor);
                        instance.window.request_redraw();
                    }
                    if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                        "event": "select-element-close",
                        "window_id": window_id,
                    })) {
                        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                            let _ = proxy.send_event(EngineCommand::IpcMessage(window_id.clone(), msg));
                        }
                    }
                }
            },
            EngineCommand::DismissSelectControl { window_id, control_id: _ } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(mut active) = instance.active_select.take() {
                        instance.active_pane_id = active.previous_active_pane_id;
                        active.prompt.select(active.initial_selected_options.clone());
                        active.prompt.submit();
                    }
                    if let Some(overlay) = instance.panes.get_mut("__lotus_select_overlay") {
                        overlay.pending_visible = false;
                        overlay.is_dirty = true;
                        let physical_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;
                        instance.recalculate_layout(physical_size, scale_factor);
                        instance.window.request_redraw();
                    }
                    if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                        "event": "select-element-close",
                        "window_id": window_id,
                    })) {
                        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                            let _ = proxy.send_event(EngineCommand::IpcMessage(window_id.clone(), msg));
                        }
                    }
                }
            },
        }
        
        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }
        event_loop.set_control_flow(ControlFlow::Wait);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        // Log every RedrawRequested BEFORE any guard, so we know if winit is dispatching it at all
        if matches!(event, WindowEvent::RedrawRequested) {
            // info!("Rust: [RAW] RedrawRequested fired for winit id {:?}, known windows: {}", 
            //    window_id, self.winit_id_to_uuid.len());
        }

        match event {
            WindowEvent::ThemeChanged(theme) => {
                if let Some(uuid) = self.winit_id_to_uuid.get(&window_id) {
                     let theme_str = match theme {
                        winit::window::Theme::Dark => "dark",
                        winit::window::Theme::Light => "light",
                    };
                    info!("Theme changed to {} for window {}", theme_str, uuid);
                    
                    if let Some(instance) = self.windows.get(uuid) {
                         let script = format!(r#"
                            if (window.lotus) {{
                                window.lotus.theme = '{}';
                                window.lotus.emit('theme-changed', '{}');
                                try {{ document.documentElement.dataset.theme = '{}'; }} catch(e) {{}}
                            }}
                        "#, theme_str, theme_str, theme_str);
                         for pane in instance.panes.values() {
                            pane.webview.evaluate_javascript(&script, |_| {});
                         }
                    }

                    let mut theme_msg = Vec::new();
                    if rmp_serde::encode::write(&mut theme_msg, &serde_json::json!({
                        "event": "theme-changed",
                        "theme": theme_str
                    })).is_ok() {
                        let mut wrapped = Vec::with_capacity(theme_msg.len() + 1);
                        wrapped.push(MSG_TYPE_DATA);
                        wrapped.extend(theme_msg);
                        self.callback.call(("global".to_string(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                    }
                }
            },
            _ => {}
        }

        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }
        event_loop.set_control_flow(ControlFlow::Wait);

        if let Some(uuid) = self.winit_id_to_uuid.get(&window_id).cloned() {
            if let Some(instance) = self.windows.get_mut(&uuid) {
                match event {
                    WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                        info!("Rust: Scale factor changed to {}", scale_factor);
                        let sf = scale_factor as f32;
                        for pane in instance.panes.values_mut() {
                            pane.webview.set_hidpi_scale_factor(euclid::Scale::new(sf));
                        }
                        instance.recalculate_layout(instance.active_window_size, sf);
                        instance.window.request_redraw();
                    },
                    WindowEvent::CloseRequested => {
                        info!("Rust: Window close requested");
                        
                        // Save window state before closing
                        if let Some(state) = APP_STATE.get() {
                            if let Ok(mut s) = state.lock() {
                                let position = instance.window.outer_position().ok();
                                let size = instance.window.inner_size();
                                let window_state = window_state::WindowState {
                                    x: position.map(|p| p.x).unwrap_or(0),
                                    y: position.map(|p| p.y).unwrap_or(0),
                                    width: size.width,
                                    height: size.height,
                                    maximized: instance.window.is_maximized(),
                                    fullscreen: false, // Default for now
                                };
                                s.window_states.save_window_state(&uuid, window_state);
                            }
                        }
                        let mut close_msg = Vec::new();
                        if rmp_serde::encode::write(&mut close_msg, &serde_json::json!({"event": "window-closed", "window_id": uuid})).is_ok() {
                            let mut wrapped = Vec::with_capacity(close_msg.len() + 1);
                            wrapped.push(MSG_TYPE_DATA);
                            wrapped.extend(close_msg);
                            self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                        self.windows.remove(&uuid);
                        self.winit_id_to_uuid.remove(&window_id);
                        info!("Rust: Window '{}' closed. {} windows remaining", uuid, self.windows.len());

                        // Clean up per-window metadata to avoid unbounded growth
                        // across many open/close cycles.
                        if let Some(state) = APP_STATE.get() {
                            if let Ok(mut s) = state.lock() {
                                s.window_metadata.remove(&uuid);
                                s.window_start_times.remove(&uuid);
                            }
                        }
                        // Drop any buffered outgoing frames for this window.
                        if let Some(p) = WS_PENDING.get() { p.remove(&uuid); }
                        if self.windows.is_empty() {
                            info!("Rust: Last window closed, exiting event loop");
                            std::thread::spawn(|| {
                                std::thread::sleep(std::time::Duration::from_millis(1000));
                                fast_exit(0);
                            });
                            event_loop.exit();
                        }
                    },
                    WindowEvent::RedrawRequested => {
                        sync_minimized_from_os(instance);
                        instance.zero_size = {
                            let s = instance.window.inner_size();
                            s.width == 0 || s.height == 0
                        };
                        apply_refresh_policy(instance);

                        // Explicitly hidden (minimize/occlude/0×0): skip paint/composite, but
                        // still present so Wayland frame callbacks are released (otherwise
                        // RedrawRequested busy-loops and pegs the host/Node event loop).
                        if !window_is_viewable(instance) {
                            trace!("Rust: [Lifecycle] RedrawRequested present-only (window not viewable)");
                            instance.window.pre_present_notify();
                            let _ = instance.rendering_context.make_current();
                            instance.rendering_context.present();
                        } else {
                        let _ = instance.rendering_context.make_current();
                        let window_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;

                        // Coalesce gutter drag to once per frame before paint/layout flush.
                        if instance.flush_pending_gutter_drag(scale_factor) {
                            instance.recalculate_layout(window_size, scale_factor);
                        }

                        trace!("Rust: [Lifecycle] RedrawRequested (FBO) for window size {}x{}", window_size.width, window_size.height);

                        unsafe {
                            // 1. UNIVERSAL PAINT BLOCK
                            // Paint any panes that have new rendered frames ready in Servo
                            for p in instance.panes.values_mut() {
                                if p.is_remote {
                                    continue;
                                }
                                if p.needs_repaint {
                                    if let Some(target_size) = p.ready_frame_size {
                                        let safe_size = ServoPhysicalSize::new(target_size.width.max(1), target_size.height.max(1));
                                        // Stale frame: a newer webview.resize() already moved
                                        // Webrender/FB past this ready size. Painting it asserts
                                        // in webrender (src_size <= len) / leaves inside_frame.
                                        if p.requested_servo_size != safe_size {
                                            p.needs_repaint = false;
                                            continue;
                                        }
                                        if p.offscreen_ctx.size() != safe_size {
                                            let _ = p.offscreen_ctx.make_current();
                                            p.offscreen_ctx.resize(safe_size);
                                        }
                                    }

                                    let current_size = p.offscreen_ctx.size();
                                    if current_size.width > 0 && current_size.height > 0 {
                                        if let Some(target_size) = p.ready_frame_size {
                                            let safe_size = ServoPhysicalSize::new(target_size.width.max(1), target_size.height.max(1));
                                            if current_size != safe_size {
                                                p.needs_repaint = false;
                                                continue;
                                            }
                                        }
                                        let _ = p.offscreen_ctx.make_current();
                                        use glow::HasContext;
                                        // Shared window GL (SHM / compositor). Reset unpack so
                                        // Webrender Immediate uploads see default pixel-store state.
                                        instance.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
                                        instance.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
                                        instance.gl.bind_buffer(glow::PIXEL_UNPACK_BUFFER, None);
                                        while instance.gl.get_error() != glow::NO_ERROR {}
                                        // Webrender asserts are process-fatal; isolate one bad
                                        // frame so a resize race cannot take down the host.
                                        let paint_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                            p.webview.paint();
                                        }));
                                        if let Err(payload) = paint_result {
                                            let msg = payload
                                                .downcast_ref::<&str>()
                                                .map(|s| (*s).to_string())
                                                .or_else(|| payload.downcast_ref::<String>().cloned())
                                                .unwrap_or_else(|| "unknown panic".into());
                                            warn!(
                                                "Rust: webview.paint panicked for pane '{}' (size {}x{}): {msg}",
                                                p.id, current_size.width, current_size.height
                                            );
                                        } else {
                                            p.first_frame_painted = true;
                                        }
                                    }
                                    p.needs_repaint = false;
                                }
                            }

                            // 2. Pull remote SHM before flip integrity — remotes mark
                            // ready_frame_size here so they participate in queue-of-1.
                            let mut preflight_remote_frames: Vec<String> = Vec::new();
                            let mut preflight_cover_ready: Vec<(String, u64)> = Vec::new();
                            for p in instance.panes.values_mut() {
                                if !p.is_remote {
                                    continue;
                                }
                                let Some(consumer) = p.remote_consumer.as_mut() else {
                                    continue;
                                };
                                // While Resize is in flight, upload to staging and keep
                                // compositing the last-good display texture (stretch interim).
                                let preserve_display = p.servo_busy && p.first_frame_painted;
                                match consumer.consume_latest_frame(&instance.gl, preserve_display) {
                                    Ok(true) => {
                                        preflight_remote_frames.push(p.id.clone());
                                        let (tw, th) = consumer.latest_upload_size();
                                        let want = p.requested_servo_size;
                                        // First post-Resize SHM frame is usually a cleared FB —
                                        // do not mark ready/flip on it. Wait for a painted frame
                                        // (2nd+ staging, or direct display upload) so we step
                                        // stretch→next without flashing black.
                                        let painted = !preserve_display
                                            || consumer.staging_frames_since_resize() >= 2
                                            || !p.first_frame_painted;
                                        if painted
                                            && ((tw == want.width && th == want.height && tw > 0 && th > 0)
                                                || !p.first_frame_painted)
                                        {
                                            p.current_servo_size = ServoPhysicalSize::new(tw, th);
                                            p.ready_frame_size = Some(ServoPhysicalSize::new(tw, th));
                                            p.first_frame_painted = true;
                                        }
                                        if p.await_host_composited_frame {
                                            preflight_cover_ready.push((p.id.clone(), consumer.last_rendered_seq));
                                        }
                                    }
                                    Ok(false) => {}
                                    Err(e) => {
                                        warn!(
                                            "Rust: pre-flip remote consume failed for '{}': {}",
                                            p.id, e
                                        );
                                    }
                                }
                            }

                            // AGGREGATION: If any pane completed its initial painted frame,
                            // emit ready-to-show if all visible panes are now painted.
                            if !instance.emitted_ready_to_show {
                                let all_ready = instance.panes.values().all(|p| !p.pending_visible || p.first_frame_painted);
                                if all_ready {
                                    instance.emitted_ready_to_show = true;
                                    if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                                        "event": "ready-to-show",
                                        "window_id": uuid,
                                    })) {
                                        let mut wrapped = Vec::with_capacity(msg.len() + 1);
                                        wrapped.push(MSG_TYPE_DATA);
                                        wrapped.extend(msg);
                                        self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                                    }
                                }
                            }

                            // 3. Evaluate Integrity against the COMMITTED target
                            let is_transaction_pending = instance.committed_layout.is_some();
                            let mut transaction_complete = false;

                            if let Some(committed) = &instance.committed_layout {
                                transaction_complete = true;
                                for (id, staged_pane) in &committed.panes {
                                    if staged_pane.visible {
                                        if let Some(pane) = instance.panes.get(id) {
                                            let target_size = ServoPhysicalSize::new(
                                                (staged_pane.rect.size.width as u32).max(1), 
                                                (staged_pane.rect.size.height as u32).max(1)
                                            );
                                            let current_ready = pane.ready_frame_size.unwrap_or(ServoPhysicalSize::new(0, 0));
                                            
                                            // Remotes + locals: flip only when every visible
                                            // pane has a frame at the committed size.
                                            if current_ready.width != target_size.width || current_ready.height != target_size.height {
                                                transaction_complete = false;
                                                break;
                                            }
                                            // For the very first spawn of a pane, hold the lock for 150ms 
                                            // to allow WebRender async texture and display list generation to finish.
                                            if !pane.is_remote {
                                                if let Some(first_time) = pane.first_frame_painted_time {
                                                    if first_time.elapsed() < std::time::Duration::from_millis(150) {
                                                        transaction_complete = false;
                                                        instance.window.request_redraw(); // Keep pumping the event loop
                                                        break;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            let mut _flip_occurred = false;

                            // 4. THE NUCLEAR GEOMETRY FLIP
                            if is_transaction_pending && transaction_complete {
                                if let Some(committed) = instance.committed_layout.take() {
                                    instance.active_layout = committed;
                                    if let Some(c_size) = instance.committed_window_size.take() {
                                        instance.active_window_size = c_size;
                                    }

                                    // Snapshot staged-ahead rects before we mutate panes
                                    // (continuous gutter may already be past this commit).
                                    let ahead: Vec<(String, euclid::default::Rect<i32>)> = instance
                                        .staged_layout
                                        .panes
                                        .iter()
                                        .filter_map(|(id, sp)| {
                                            let committed_rect = instance
                                                .active_layout
                                                .panes
                                                .get(id)
                                                .map(|c| c.rect)?;
                                            if sp.rect != committed_rect {
                                                Some((id.clone(), sp.rect))
                                            } else {
                                                None
                                            }
                                        })
                                        .collect();

                                    for (id, staged) in &instance.active_layout.panes {
                                        if let Some(p) = instance.panes.get_mut(id) {
                                            p.is_visible = staged.visible;
                                            p.last_physical_rect = staged.rect;
                                            p.first_frame_painted_time = None;
                                            if let Some(consumer) = p.remote_consumer.as_mut() {
                                                // Promote only painted staging (2nd+ frame);
                                                // discard a lone cleared post-Resize buffer.
                                                consumer.promote_painted_or_discard_staging(
                                                    &instance.gl,
                                                );
                                            }
                                            if let Some((_, ahead_rect)) =
                                                ahead.iter().find(|(aid, _)| aid == id)
                                            {
                                                // Layout already moved past this flip — keep
                                                // stretching into the newer pending slot.
                                                p.pending_physical_rect = Some(*ahead_rect);
                                                p.is_resizing = true;
                                            } else {
                                                p.pending_physical_rect = None;
                                                p.is_resizing = false;
                                            }
                                        }
                                    }
                                    trace!("Rust: [Transaction] Nuclear Layout Swap Executed");
                                    _flip_occurred = true;
                                }
                            }

                            let is_transaction_pending = instance.committed_layout.is_some();
                            let mut pending_hides = std::collections::HashSet::new();
                            if let Some(committed) = &instance.committed_layout {
                                for (id, staged) in &committed.panes {
                                    if !staged.visible {
                                        pending_hides.insert(id.clone());
                                    }
                                }
                            }

                            let mut sorted_panes: Vec<_> = instance.panes.values_mut().collect();
                            sorted_panes.sort_by_key(|p| p.z_index);

                            let active_w = instance.active_window_size.width.max(1);
                            let active_h = instance.active_window_size.height.max(1);

                            gl::ensure_textures(instance, active_w, active_h);
                            let composite_remotes = gl::composite_panes_to_scene(
                                instance,
                                active_w,
                                active_h,
                                scale_factor,
                                is_transaction_pending,
                                transaction_complete,
                                &pending_hides,
                            );
                            let mut remotes_updated = composite_remotes.remotes_with_new_frames;
                            for id in preflight_remote_frames {
                                if !remotes_updated.contains(&id) {
                                    remotes_updated.push(id);
                                }
                            }
                            let mut cover_ready: Vec<(String, u64)> = composite_remotes
                                .cover_ready_panes
                                .into_iter()
                                .map(|id| (id, 0u64))
                                .collect();
                            for (id, seq) in preflight_cover_ready {
                                if !cover_ready.iter().any(|(existing, _)| existing == &id) {
                                    cover_ready.push((id, seq));
                                }
                            }

                            let live_w = window_size.width.max(1) as i32;
                            let live_h = window_size.height.max(1) as i32;
                            gl::blit_scene_to_window(instance, live_w, live_h, scale_factor);

                            // Timing already applied at RedrawRequested entry; keep Vsync
                            // pull-model while focused + viewable, or while awaiting respawn frame.
                            if window_is_viewable(instance) {
                                let presentation_time_ns = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_nanos() as u64)
                                    .unwrap_or(0);
                                for pane_id in &remotes_updated {
                                    if let Some(pane) = instance.panes.get(pane_id) {
                                        if instance.os_focused || pane.await_host_composited_frame {
                                            if let Some(fd) = pane.remote_input_fd {
                                                let surface_id = format!("{}:{}", uuid, pane_id);
                                                send_remote_vsync(
                                                    fd,
                                                    &surface_id,
                                                    presentation_time_ns,
                                                );
                                            }
                                        }
                                    }
                                }
                            }

                            // Notify JS only after the host has the fresh pixels in GL —
                            // FramePublished alone still leaves the prior life's texture up.
                            for (pane_id, pub_seq) in &cover_ready {
                                if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                                    "event": "remote-frame-composited",
                                    "window_id": uuid,
                                    "pane_id": pane_id,
                                    "pub_seq": pub_seq,
                                })) {
                                    let mut wrapped = Vec::with_capacity(msg.len() + 1);
                                    wrapped.push(MSG_TYPE_DATA);
                                    wrapped.extend(msg);
                                    self.callback.call(
                                        (uuid.clone(), vec![wrapped.into()]),
                                        ThreadsafeFunctionCallMode::NonBlocking,
                                    );
                                }
                            }

                            // LOCK STEP: The current frame is now fully presented to the GPU and screen.
                            if instance.committed_layout.is_none() {
                                for pane in instance.panes.values_mut() {
                                    pane.servo_busy = false;
                                }
                                let window_size = instance.window.inner_size();
                                let scale_factor = instance.window.scale_factor() as f32;
                                instance.recalculate_layout(window_size, scale_factor);
                            }

                            trace!("Rust: [Lifecycle] RedrawRequested DONE");
                        }
                        } // viewable
                    },            WindowEvent::Resized(size) => {
                let scale_factor = instance.window.scale_factor() as f32;
                instance.zero_size = size.width == 0 || size.height == 0;
                sync_minimized_from_os(instance);
                apply_refresh_policy(instance);

                if let Some(mut active) = instance.active_select.take() {
                    instance.active_pane_id = active.previous_active_pane_id;
                    if active.allow_multiple {
                        active.prompt.submit();
                    } else {
                        active.prompt.select(active.initial_selected_options.clone());
                        active.prompt.submit();
                    }
                    if let Some(overlay) = instance.panes.get_mut("__lotus_select_overlay") {
                        overlay.pending_visible = false;
                        overlay.is_dirty = true;
                    }
                    if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                        "event": "select-element-close",
                        "window_id": uuid,
                    })) {
                        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                            let _ = proxy.send_event(EngineCommand::IpcMessage(uuid.clone(), msg));
                        }
                    }
                }
                
                // 1. Guard against feedback loops: only process if size actually changed
                // (Using a small threshold for floating point stability)
                if let Some(state) = APP_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        if let Some(metadata) = s.window_metadata.get_mut(&uuid) {
                            let last_size = metadata.last_window_size.unwrap_or(winit::dpi::PhysicalSize::new(0, 0));
                            if last_size == size {
                                return;
                            }
                            metadata.last_window_size = Some(size);
                        }
                    }
                }

                info!("Rust: Resized to {}x{}", size.width, size.height);

                instance.recalculate_layout(size, scale_factor);
                
                instance.rendering_context.resize(size);
                // SYNCHRONOUS PRESENTATION ON RESIZE:
                // Instantly stretch the last known good scene quad across the entire expanded window surface.
                // This eliminates any asynchronous gap between Mutter/Xwayland surface resize and Winit RedrawRequested.
                let live_w = size.width.max(1) as i32;
                let live_h = size.height.max(1) as i32;
                gl::blit_scene_to_window(instance, live_w, live_h, scale_factor);

                instance.window.request_redraw();

                let mut msg = Vec::new();
                let logical_w = size.width as f32 / scale_factor;
                let logical_h = size.height as f32 / scale_factor;
                
                info!("Rust: Sending 'resized' event to JS: {}x{} (Logical: {}x{})", size.width, size.height, logical_w, logical_h);
                
                if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                    "event": "resized",
                    "window_id": uuid,
                    "width": size.width,
                    "height": size.height,
                    "scale_factor": scale_factor,
                    "logicalWidth": logical_w,
                    "logicalHeight": logical_h,
                    "maximized": instance.window.is_maximized()
                })).is_ok() {
                        let mut wrapped = Vec::with_capacity(msg.len() + 1);
                        wrapped.push(MSG_TYPE_DATA);
                        wrapped.extend(msg);
                        self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                }
            },
                    WindowEvent::CursorMoved { position, .. } => {
                        let point = Point2D::new(position.x as f32, position.y as f32);
                        instance.last_mouse_pos = point;
                        let scale_factor = instance.window.scale_factor() as f32;

                        // Active gutter drag: stash total Δ for the redraw tick; no pane routing.
                        if let Some(ref drag) = instance.active_gutter_drag {
                            let main = match drag.axis {
                                Axis::Horizontal => point.x,
                                Axis::Vertical => point.y,
                            };
                            let raw = (main - drag.start_cursor_main).round() as i32;
                            instance.pending_gutter_delta = Some(raw);
                            let icon = match drag.axis {
                                Axis::Horizontal => winit::window::CursorIcon::ColResize,
                                Axis::Vertical => winit::window::CursorIcon::RowResize,
                            };
                            instance.window.set_cursor(icon);
                            instance.window.request_redraw();
                            return;
                        }
                        
                        // If mouse is down, route exclusively to the pane that received the Down event
                        if instance.is_mouse_down {
                            if let Some(pane_id) = &instance.last_mouse_down_pane_id {
                                if let Some(pane) = instance.panes.get_mut(pane_id) {
                                    let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                                    let mut translated_point = Point2D::new(point.x - px, point.y - py);
                                    if pane.is_remote {
                                        if let Some(ref consumer) = pane.remote_consumer {
                                            if consumer.tex_width > 0 && consumer.tex_height > 0 && pw > 0.0 && ph > 0.0 {
                                                translated_point.x *= consumer.tex_width as f32 / pw;
                                                translated_point.y *= consumer.tex_height as f32 / ph;
                                            }
                                        }
                                        if let Some(fd) = pane.remote_input_fd {
                                            let surface_id = format!("{}:{}", uuid, pane.id);
                                            let msg = serde_json::json!({
                                                "type": "Input",
                                                "event": {
                                                    "kind": "MouseMove",
                                                    "surface_id": surface_id,
                                                    "generation": 1,
                                                    "x": translated_point.x,
                                                    "y": translated_point.y,
                                                    "modifiers": instance.modifiers.bits(),
                                                }
                                            }).to_string();
                                            forward_remote_input(fd, &msg);
                                        }
                                    } else {
                                        pane.webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
                                            servo::WebViewPoint::Device(translated_point)
                                        )));
                                    }
                                    return;
                                }
                            }
                        }

                        // Overlay / scrollbar beat gutters; then gutter hover; else pane hit.
                        let overlay_blocks = {
                            let mut sorted: Vec<_> = instance.panes.values().collect();
                            sorted.sort_by_key(|p| -p.z_index);
                            sorted.into_iter().any(|p| {
                                if !is_gutter_blocking_overlay(instance, p) {
                                    return false;
                                }
                                let (px, py, pw, ph) = p.input_physical_rect(scale_factor);
                                let rect = euclid::Rect::new(
                                    euclid::Point2D::new(px, py),
                                    euclid::Size2D::new(pw, ph),
                                );
                                rect.contains(point)
                            })
                        };

                        if !overlay_blocks {
                            if let Some(g) = hit_test_gutters(
                                &instance.gutters,
                                euclid::default::Point2D::new(point.x, point.y),
                            ) {
                                if !point_in_trailing_scrollbar(instance, g, point, scale_factor) {
                                    let left_id = g.left_id.clone();
                                    let right_id = g.right_id.clone();
                                    let axis = g.axis;
                                    enter_gutter_hover(instance, &uuid, &left_id, &right_id);
                                    let icon = match axis {
                                        Axis::Horizontal => winit::window::CursorIcon::ColResize,
                                        Axis::Vertical => winit::window::CursorIcon::RowResize,
                                    };
                                    instance.window.set_cursor(icon);
                                    instance.in_resize_border = false;
                                    return;
                                }
                            }
                        }

                        if instance.gutter_hover.is_some() {
                            clear_gutter_hover_state(instance);
                            instance.window.set_cursor(winit::window::CursorIcon::Default);
                        }

                        // Hit-test resize directions first
                        if instance.frameless {
                            let mut hovered_pane = None;
                            let mut sorted_panes: Vec<_> = instance.panes.values().collect();
                            sorted_panes.sort_by_key(|p| -p.z_index); // Reverse Z for hit-testing

                            for pane in sorted_panes {
                                if !pane.is_visible { continue; }
                                let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                                let rect = euclid::Rect::new(euclid::Point2D::new(px, py), euclid::Size2D::new(pw, ph));

                                if rect.contains(point) {
                                    hovered_pane = Some(pane);
                                    break;
                                }
                            }
                            
                            if let Some(pane) = hovered_pane {
                                instance.hover_pane_id = Some(pane.id.clone());
                                // Unified physical coordinate translation
                                let (px, py, _pw, _ph) = pane.input_physical_rect(scale_factor);
                                let translated_point = Point2D::new(point.x - px, point.y - py);

                                 // SHADOWING: Check no-drag regions first.
                                 let mut hit_no_drag = false;
                                 for no_drag_region in &pane.no_drag_regions {
                                     if no_drag_region.contains(translated_point) {
                                         hit_no_drag = true;
                                         break;
                                     }
                                 }
                                 
                                 let mut hit_drag = false;
                                 if !hit_no_drag {
                                     for region in &pane.drag_regions {
                                         if region.contains(translated_point) {
                                             hit_drag = true;
                                             break;
                                         }
                                     }
                                 }
                                
                                // 1. In frameless windows, check window resize borders first
                                let resize_cursor = if instance.frameless {
                                    let size = instance.window.inner_size();
                                    let x = position.x;
                                    let y = position.y;
                                    let w = size.width as f64;
                                    let h = size.height as f64;
                                    let border = (6.0_f64 * f64::from(scale_factor)).round().max(4.0);
                                    
                                    if x < border && y < border {
                                        Some(CursorIcon::NwResize)
                                    } else if x > w - border && y < border {
                                        Some(CursorIcon::NeResize)
                                    } else if x < border && y > h - border {
                                        Some(CursorIcon::SwResize)
                                    } else if x > w - border && y > h - border {
                                        Some(CursorIcon::SeResize)
                                    } else if x < border {
                                        Some(CursorIcon::WResize)
                                    } else if x > w - border {
                                        Some(CursorIcon::EResize)
                                    } else if y < border {
                                        Some(CursorIcon::NResize)
                                    } else if y > h - border {
                                        Some(CursorIcon::SResize)
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };
                                
                                if let Some(icon) = resize_cursor {
                                    instance.window.set_cursor(icon);
                                    instance.in_resize_border = true;
                                } else {
                                    if instance.in_resize_border {
                                        instance.window.set_cursor(winit::window::CursorIcon::Default);
                                        instance.in_resize_border = false;
                                    }
                                    if hit_drag {
                                        instance.window.set_cursor(winit::window::CursorIcon::Move);
                                    }
                                }

                                // Route MouseMove to the hovered pane with translated physical coordinates
                                let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                                let mut translated_point = Point2D::new(point.x - px, point.y - py);

                                if pane.is_remote {
                                    if let Some(ref consumer) = pane.remote_consumer {
                                        if consumer.tex_width > 0 && consumer.tex_height > 0 && pw > 0.0 && ph > 0.0 {
                                            translated_point.x *= consumer.tex_width as f32 / pw;
                                            translated_point.y *= consumer.tex_height as f32 / ph;
                                        }
                                    }
                                    if let Some(fd) = pane.remote_input_fd {
                                        let surface_id = format!("{}:{}", uuid, pane.id);
                                        let msg = format!(
                                            "{{\"type\":\"Input\",\"event\":{{\"kind\":\"MouseMove\",\"surface_id\":\"{}\",\"generation\":1,\"x\":{},\"y\":{},\"modifiers\":0}}}}",
                                            surface_id, translated_point.x, translated_point.y
                                        );
                                        forward_remote_input(fd, &msg);
                                    }
                                } else {
                                    pane.webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
                                        servo::WebViewPoint::Device(translated_point)
                                    )));
                                }
                            }
                        } else {
                            // Standard window: just find hovered pane and route
                            let mut sorted_panes: Vec<_> = instance.panes.values().collect();
                            sorted_panes.sort_by_key(|p| -p.z_index);
                            for pane in sorted_panes {
                                if !pane.is_visible { continue; }
                                let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                                let rect = euclid::Rect::new(euclid::Point2D::new(px, py), euclid::Size2D::new(pw, ph));

                                if rect.contains(point) {
                                    let hover_id = pane.id.clone();
                                    let mut translated_point = Point2D::new(point.x - px, point.y - py);
                                    if pane.is_remote {
                                        if let Some(ref consumer) = pane.remote_consumer {
                                            if consumer.tex_width > 0 && consumer.tex_height > 0 && pw > 0.0 && ph > 0.0 {
                                                translated_point.x *= consumer.tex_width as f32 / pw;
                                                translated_point.y *= consumer.tex_height as f32 / ph;
                                            }
                                        }
                                        if let Some(fd) = pane.remote_input_fd {
                                            let surface_id = format!("{}:{}", uuid, pane.id);
                                            let msg = serde_json::json!({
                                                "type": "Input",
                                                "event": {
                                                    "kind": "MouseMove",
                                                    "surface_id": surface_id,
                                                    "generation": 1,
                                                    "x": translated_point.x,
                                                    "y": translated_point.y,
                                                    "modifiers": instance.modifiers.bits(),
                                                }
                                            }).to_string();
                                            forward_remote_input(fd, &msg);
                                        }
                                    } else {
                                        pane.webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
                                            servo::WebViewPoint::Device(translated_point)
                                        )));
                                    }
                                    instance.hover_pane_id = Some(hover_id);
                                    break;
                                }
                            }
                        }
                    },
                    WindowEvent::MouseInput { state, button, .. } => {
                        let is_pressed = state == winit::event::ElementState::Pressed;
                        instance.is_mouse_down = is_pressed;
                        let scale_factor = instance.window.scale_factor() as f32;

                        // Gutter release — keep current geometry (do not revert).
                        if !is_pressed && instance.active_gutter_drag.is_some() {
                            if instance.flush_pending_gutter_drag(scale_factor) {
                                let physical_size = instance.window.inner_size();
                                instance.recalculate_layout(physical_size, scale_factor);
                            }
                            instance.active_gutter_drag = None;
                            instance.pending_gutter_delta = None;
                            return;
                        }

                        // Gutter press — start drag from snapshot; overlays/scrollbars win.
                        if is_pressed && button == winit::event::MouseButton::Left {
                            let point = instance.last_mouse_pos;
                            let overlay_blocks = {
                                let mut sorted: Vec<_> = instance.panes.values().collect();
                                sorted.sort_by_key(|p| -p.z_index);
                                sorted.into_iter().any(|p| {
                                    if !is_gutter_blocking_overlay(instance, p) {
                                        return false;
                                    }
                                    let (px, py, pw, ph) = p.input_physical_rect(scale_factor);
                                    let rect = euclid::Rect::new(
                                        euclid::Point2D::new(px, py),
                                        euclid::Size2D::new(pw, ph),
                                    );
                                    rect.contains(point)
                                })
                            };
                            if !overlay_blocks {
                                let gutter_hit = hit_test_gutters(
                                    &instance.gutters,
                                    euclid::default::Point2D::new(point.x, point.y),
                                )
                                .filter(|g| {
                                    !point_in_trailing_scrollbar(instance, g, point, scale_factor)
                                })
                                .cloned();
                                if let Some(g) = gutter_hit {
                                    let cursor_main = match g.axis {
                                        Axis::Horizontal => point.x,
                                        Axis::Vertical => point.y,
                                    };
                                    if let Some(drag) =
                                        begin_gutter_drag(&instance.layout_root, &g, cursor_main)
                                    {
                                        let axis = g.axis;
                                        enter_gutter_hover(
                                            instance,
                                            &uuid,
                                            &drag.left_id,
                                            &drag.right_id,
                                        );
                                        instance.last_mouse_down_pane_id = None;
                                        // Drop any in-flight cleared Resize staging so a
                                        // mid-drag nuclear flip cannot flash black on 2|3.
                                        for p in instance.panes.values_mut() {
                                            if let Some(c) = p.remote_consumer.as_mut() {
                                                c.discard_staging(&instance.gl);
                                            }
                                        }
                                        instance.active_gutter_drag = Some(drag);
                                        instance.pending_gutter_delta = Some(0);
                                        let icon = match axis {
                                            Axis::Horizontal => {
                                                winit::window::CursorIcon::ColResize
                                            }
                                            Axis::Vertical => {
                                                winit::window::CursorIcon::RowResize
                                            }
                                        };
                                        instance.window.set_cursor(icon);
                                        return;
                                    }
                                }
                            }
                        }

                        if is_pressed {
                            if let Some(_) = &instance.active_select {
                                let overlay_pane = instance.panes.get("__lotus_select_overlay");
                                let is_inside_overlay = overlay_pane.map(|p| {
                                    if !p.is_visible { return false; }
                                    let (px, py, pw, ph) = p.input_physical_rect(scale_factor);
                                    let rect = euclid::Rect::new(euclid::Point2D::new(px, py), euclid::Size2D::new(pw, ph));
                                    rect.contains(instance.last_mouse_pos)
                                }).unwrap_or(false);

                                if !is_inside_overlay {
                                    let mut active = instance.active_select.take().unwrap();
                                    instance.active_pane_id = active.previous_active_pane_id;
                                    if active.allow_multiple {
                                        active.prompt.submit();
                                    } else {
                                        active.prompt.select(active.initial_selected_options.clone());
                                        active.prompt.submit();
                                    }
                                    if let Some(overlay) = instance.panes.get_mut("__lotus_select_overlay") {
                                        overlay.pending_visible = false;
                                        overlay.is_dirty = true;
                                        let physical_size = instance.window.inner_size();
                                        instance.recalculate_layout(physical_size, scale_factor);
                                        instance.window.request_redraw();
                                    }
                                    if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                                        "event": "select-element-close",
                                        "window_id": uuid,
                                    })) {
                                        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                                            let _ = proxy.send_event(EngineCommand::IpcMessage(uuid.clone(), msg));
                                        }
                                    }
                                    return;
                                }
                            }
                        }
                        
                        let action = match state {
                            winit::event::ElementState::Pressed => MouseButtonAction::Down,
                            winit::event::ElementState::Released => MouseButtonAction::Up,
                        };
                        let servo_button = match button {
                            winit::event::MouseButton::Left => ServoMouseButton::Primary,
                            winit::event::MouseButton::Right => ServoMouseButton::Secondary,
                            winit::event::MouseButton::Middle => ServoMouseButton::Auxiliary,
                            winit::event::MouseButton::Back => ServoMouseButton::Back,
                            winit::event::MouseButton::Forward => ServoMouseButton::Forward,
                            winit::event::MouseButton::Other(v) => ServoMouseButton::Other(v),
                        };

                        let target_pane_id = if !is_pressed {
                            instance.last_mouse_down_pane_id.take()
                        } else {
                            let mut sorted_panes: Vec<_> = instance.panes.values().collect();
                            sorted_panes.sort_by_key(|p| -p.z_index);
                            
                            let mut found_id = None;
                            for pane in sorted_panes {
                                if !pane.is_visible { continue; }
                                let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                                let rect = euclid::Rect::new(euclid::Point2D::new(px, py), euclid::Size2D::new(pw, ph));

                                if rect.contains(instance.last_mouse_pos) {
                                    found_id = Some(pane.id.clone());
                                    break;
                                }
                            }
                            if is_pressed {
                                instance.last_mouse_down_pane_id = found_id.clone();
                            }
                            found_id
                        };

                        if let Some(pane_id) = target_pane_id {
                            if let Some(pane) = instance.panes.get_mut(&pane_id) {
                                    // Unified physical coordinate translation
                                    let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                                    let mut translated_point = Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
                                    if pane.is_remote {
                                        if let Some(ref consumer) = pane.remote_consumer {
                                            if consumer.tex_width > 0 && consumer.tex_height > 0 && pw > 0.0 && ph > 0.0 {
                                                translated_point.x *= consumer.tex_width as f32 / pw;
                                                translated_point.y *= consumer.tex_height as f32 / ph;
                                            }
                                        }
                                    }

                                    if is_pressed && button == winit::event::MouseButton::Left && instance.frameless {
                                        let size = instance.window.inner_size();
                                        let x = instance.last_mouse_pos.x as f64;
                                        let y = instance.last_mouse_pos.y as f64;
                                        let w = size.width as f64;
                                        let h = size.height as f64;
                                        let border = (6.0_f64 * f64::from(scale_factor)).round().max(4.0);
                                        
                                        let mut resize_dir = None;
                                        if x < border && y < border {
                                            resize_dir = Some(winit::window::ResizeDirection::NorthWest);
                                        } else if x > w - border && y < border {
                                            resize_dir = Some(winit::window::ResizeDirection::NorthEast);
                                        } else if x < border && y > h - border {
                                            resize_dir = Some(winit::window::ResizeDirection::SouthWest);
                                        } else if x > w - border && y > h - border {
                                            resize_dir = Some(winit::window::ResizeDirection::SouthEast);
                                        } else if x < border {
                                            resize_dir = Some(winit::window::ResizeDirection::West);
                                        } else if x > w - border {
                                            resize_dir = Some(winit::window::ResizeDirection::East);
                                        } else if y < border {
                                            resize_dir = Some(winit::window::ResizeDirection::North);
                                        } else if y > h - border {
                                            resize_dir = Some(winit::window::ResizeDirection::South);
                                        }
                                        
                                        if let Some(dir) = resize_dir {
                                            let translated_point = Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
                                            pane.webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                                                MouseButtonAction::Up,
                                                servo_button,
                                                servo::WebViewPoint::Device(translated_point)
                                            )));
                                            instance.is_mouse_down = false;
                                            instance.last_mouse_down_pane_id = None;
                                            let _ = instance.window.drag_resize_window(dir);
                                            return;
                                        }

                                        let mut hit_no_drag = false;
                                        for no_drag_region in &pane.no_drag_regions {
                                            if no_drag_region.contains(translated_point) {
                                                hit_no_drag = true;
                                                break;
                                            }
                                        }

                                        let mut hit_drag = false;
                                        if !hit_no_drag {
                                            for region in &pane.drag_regions {
                                                if region.contains(translated_point) {
                                                    hit_drag = true;
                                                    break;
                                                }
                                            }
                                        }
                                        
                                        if hit_drag {
                                            info!("Rust: Hit Drag Region at {:?} in pane '{}'", translated_point, pane.id);
                                            let translated_point = Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
                                            pane.webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                                                MouseButtonAction::Up,
                                                servo_button,
                                                servo::WebViewPoint::Device(translated_point)
                                            )));
                                            instance.is_mouse_down = false;
                                            instance.last_mouse_down_pane_id = None;
                                            let _ = instance.window.drag_window();
                                            return;
                                        } else {
                                            trace!("Rust: Drag check FAILED at {:?} for pane '{}'. Drag regions count: {}. No-drag: {}", 
                                                translated_point, pane.id, pane.drag_regions.len(), pane.no_drag_regions.len());
                                        }
                                    }

                                // Focus on click
                                if is_pressed {
                                    instance.active_pane_id = pane.id.clone();
                                }

                                if pane.is_remote {
                                    if let Some(fd) = pane.remote_input_fd {
                                        let surface_id = format!("{}:{}", uuid, pane.id);
                                        let kind = match action {
                                            MouseButtonAction::Down => "MouseDown",
                                            MouseButtonAction::Up => "MouseUp",
                                        };
                                        let btn_num = match servo_button {
                                            ServoMouseButton::Primary => 0,
                                            ServoMouseButton::Auxiliary => 1,
                                            ServoMouseButton::Secondary => 2,
                                            _ => 0,
                                        };
                                        let msg = serde_json::json!({
                                            "type": "Input",
                                            "event": {
                                                "kind": kind,
                                                "surface_id": surface_id,
                                                "generation": 1,
                                                "button": btn_num,
                                                "x": translated_point.x,
                                                "y": translated_point.y,
                                                "modifiers": instance.modifiers.bits(),
                                            }
                                        }).to_string();
                                        forward_remote_input(fd, &msg);
                                    }
                                } else {
                                    pane.webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                                        action,
                                        servo_button,
                                        servo::WebViewPoint::Device(translated_point)
                                    )));
                                }
                            }
                        }
                    },
                    WindowEvent::MouseWheel { delta, .. } => {
                        let scale_factor = instance.window.scale_factor() as f32;
                        // Standard browsers (Chromium/Firefox) scroll ~100 CSS px per wheel notch (approx. 4-5 text lines).
                        // Scale by scale_factor so HiDPI displays maintain the intended CSS scroll distance.
                        // Precision touchpads and smooth wheels emit PixelDelta, which is already in physical pixels.
                        let line_scroll = 100.0 * scale_factor as f64;
                        let (x, y) = match delta {
                            MouseScrollDelta::LineDelta(x, y) => (x as f64 * line_scroll, y as f64 * line_scroll),
                            MouseScrollDelta::PixelDelta(pos) => (pos.x, pos.y),
                        };
                        let wheel_delta = WheelDelta {
                            x,
                            y,
                            z: 0.0,
                            mode: WheelMode::DeltaPixel,
                        };

                        if let Some(_) = &instance.active_select {
                            let overlay_pane = instance.panes.get("__lotus_select_overlay");
                            let is_inside_overlay = overlay_pane.map(|p| {
                                if !p.is_visible { return false; }
                                let (px, py, pw, ph) = p.input_physical_rect(scale_factor);
                                let rect = euclid::Rect::new(euclid::Point2D::new(px, py), euclid::Size2D::new(pw, ph));
                                rect.contains(instance.last_mouse_pos)
                            }).unwrap_or(false);

                            if !is_inside_overlay {
                                let mut active = instance.active_select.take().unwrap();
                                instance.active_pane_id = active.previous_active_pane_id;
                                if active.allow_multiple {
                                    active.prompt.submit();
                                } else {
                                    active.prompt.select(active.initial_selected_options.clone());
                                    active.prompt.submit();
                                }
                                if let Some(overlay) = instance.panes.get_mut("__lotus_select_overlay") {
                                    overlay.pending_visible = false;
                                    overlay.is_dirty = true;
                                    let physical_size = instance.window.inner_size();
                                    instance.recalculate_layout(physical_size, scale_factor);
                                    instance.window.request_redraw();
                                }
                                if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                                    "event": "select-element-close",
                                    "window_id": uuid,
                                })) {
                                    if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                                        let _ = proxy.send_event(EngineCommand::IpcMessage(uuid.clone(), msg));
                                    }
                                }
                            }
                        }

                        let mut sorted_panes: Vec<_> = instance.panes.values_mut().collect();
                        sorted_panes.sort_by_key(|p| -p.z_index);
                        for pane in sorted_panes {
                            if !pane.is_visible { continue; }
                            let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                            let rect = euclid::Rect::new(euclid::Point2D::new(px, py), euclid::Size2D::new(pw, ph));

                            if rect.contains(instance.last_mouse_pos) {
                                let mut translated_point = Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
                                if pane.is_remote {
                                    if let Some(ref consumer) = pane.remote_consumer {
                                        if consumer.tex_width > 0 && consumer.tex_height > 0 && pw > 0.0 && ph > 0.0 {
                                            translated_point.x *= consumer.tex_width as f32 / pw;
                                            translated_point.y *= consumer.tex_height as f32 / ph;
                                        }
                                    }
                                    if let Some(fd) = pane.remote_input_fd {
                                        let surface_id = format!("{}:{}", uuid, pane.id);
                                        let msg = serde_json::json!({
                                            "type": "Input",
                                            "event": {
                                                "kind": "MouseWheel",
                                                "surface_id": surface_id,
                                                "generation": 1,
                                                "x": translated_point.x,
                                                "y": translated_point.y,
                                                "dx": x,
                                                "dy": y,
                                                "modifiers": instance.modifiers.bits(),
                                            }
                                        }).to_string();
                                        forward_remote_input(fd, &msg);
                                    }
                                } else {
                                    pane.webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(
                                        wheel_delta,
                                        servo::WebViewPoint::Device(translated_point)
                                    )));
                                    pane.needs_repaint = true;
                                }
                                instance.window.request_redraw();
                                break;
                            }
                        }
                    },
                    WindowEvent::Moved(position) => {
                        let mut msg = Vec::new();
                        if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                            "event": "moved",
                            "window_id": uuid,
                            "x": position.x,
                            "y": position.y
                        })).is_ok() {
                            let mut wrapped = Vec::with_capacity(msg.len() + 1);
                            wrapped.push(MSG_TYPE_DATA);
                            wrapped.extend(msg);
                            self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                    },
                    WindowEvent::Occluded(occluded) => {
                        if let Some(instance) = self.windows.get_mut(&uuid) {
                            instance.os_occluded = occluded;
                            sync_minimized_from_os(instance);
                            apply_refresh_policy(instance);
                            if !occluded {
                                instance.window.request_redraw();
                            }
                        }
                    },
                    WindowEvent::Focused(focused) => {
                        if let Some(instance) = self.windows.get_mut(&uuid) {
                            instance.os_focused = focused;
                            sync_minimized_from_os(instance);
                            apply_refresh_policy(instance);
                            if !focused {
                                // Keep current dragged geometry; drop capture so a missed
                                // mouse-up cannot leave the seam stuck.
                                instance.cancel_gutter_drag_keep();
                            }
                            // Kick a redraw so focused remotes resume at full rate immediately.
                            if focused {
                                instance.window.request_redraw();
                            }
                        }
                        let event_name = if focused { "focused" } else { "unfocused" };
                        let mut msg = Vec::new();
                        if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                            "event": event_name,
                            "window_id": uuid
                        })).is_ok() {
                            let mut wrapped = Vec::with_capacity(msg.len() + 1);
                            wrapped.push(MSG_TYPE_DATA);
                            wrapped.extend(msg);
                            self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                    },
                    WindowEvent::HoveredFile(path) => {
                        let path_str = path.to_string_lossy().into_owned();
                        // Notify Node.js
                        let mut msg = Vec::new();
                        if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                            "event": "file-hover", "window_id": uuid, "path": path_str
                        })).is_ok() {
                            let mut wrapped = Vec::with_capacity(msg.len() + 1);
                            wrapped.push(MSG_TYPE_DATA);
                            wrapped.extend(msg);
                            self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                        // Push to renderer — standard [[channel, payload]] msgpack batch.
                        // to_vec_named is used intentionally (same as to_vec but with
                        // named struct fields) to produce human-readable msgpack maps;
                        // consistent with the serde_json::Value payloads used here.
                        if let Some(senders) = WS_SENDERS.get() {
                            if let Ok(packed) = rmp_serde::encode::to_vec_named(
                                &vec![("file-hover", serde_json::json!({ "path": path_str }))]
                            ) {
                                let msg = axum::extract::ws::Message::Binary(packed.into());
                                for entry in senders.iter() {
                                    if entry.key().starts_with(&format!("{}:", uuid)) || entry.key() == &uuid {
                                        let _ = entry.value().send(msg.clone());
                                    }
                                }
                            }
                        }
                    },
                    WindowEvent::HoveredFileCancelled => {
                        let mut msg = Vec::new();
                        if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                            "event": "file-hover-cancelled", "window_id": uuid
                        })).is_ok() {
                            let mut wrapped = Vec::with_capacity(msg.len() + 1);
                            wrapped.push(MSG_TYPE_DATA);
                            wrapped.extend(msg);
                            self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                        if let Some(senders) = WS_SENDERS.get() {
                            if let Ok(packed) = rmp_serde::encode::to_vec_named(
                                &vec![("file-hover-cancelled", serde_json::json!(null))]
                            ) {
                                let msg = axum::extract::ws::Message::Binary(packed.into());
                                for entry in senders.iter() {
                                    if entry.key().starts_with(&format!("{}:", uuid)) || entry.key() == &uuid {
                                        let _ = entry.value().send(msg.clone());
                                    }
                                }
                            }
                        }
                    },
                    WindowEvent::DroppedFile(path) => {
                        let path_str = path.to_string_lossy().into_owned();
                        // Notify Node.js
                        let mut msg = Vec::new();
                        if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                            "event": "file-drop", "window_id": uuid, "path": path_str
                        })).is_ok() {
                            let mut wrapped = Vec::with_capacity(msg.len() + 1);
                            wrapped.push(MSG_TYPE_DATA);
                            wrapped.extend(msg);
                            self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                        // Push to renderer
                        if let Some(senders) = WS_SENDERS.get() {
                            if let Ok(packed) = rmp_serde::encode::to_vec_named(
                                &vec![("file-drop", serde_json::json!({ "path": path_str }))]
                            ) {
                                let msg = axum::extract::ws::Message::Binary(packed.into());
                                for entry in senders.iter() {
                                    if entry.key().starts_with(&format!("{}:", uuid)) || entry.key() == &uuid {
                                        let _ = entry.value().send(msg.clone());
                                    }
                                }
                            }
                        }
                    },
                    WindowEvent::ModifiersChanged(modifiers) => {
                        instance.modifiers = map_winit_modifiers(modifiers);
                    },
                    WindowEvent::CursorLeft { .. } => {
                        // Without a button: drop gutter capture (keep geometry) and hover.
                        if !instance.is_mouse_down {
                            instance.cancel_gutter_drag_keep();
                            clear_gutter_hover_state(instance);
                            instance.hover_pane_id = None;
                            instance.window.set_cursor(winit::window::CursorIcon::Default);
                        }
                    },
                    WindowEvent::KeyboardInput { event, .. } => {
                        // Escape during gutter drag: revert to start snapshot.
                        if event.state == winit::event::ElementState::Pressed
                            && instance.active_gutter_drag.is_some()
                        {
                            let is_escape = matches!(
                                event.logical_key,
                                winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape)
                            );
                            if is_escape {
                                let scale_factor = instance.window.scale_factor() as f32;
                                if instance.cancel_gutter_drag_revert(scale_factor) {
                                    let physical_size = instance.window.inner_size();
                                    instance.recalculate_layout(physical_size, scale_factor);
                                    instance.window.request_redraw();
                                }
                                instance.window.set_cursor(winit::window::CursorIcon::Default);
                                return;
                            }
                        }

                        let state = match event.state {
                            winit::event::ElementState::Pressed => KeyState::Down,
                            winit::event::ElementState::Released => KeyState::Up,
                        };
                        
                        let key = map_winit_key(&event.logical_key);
                        let code = map_winit_code(event.physical_key);
                        
                        let keyboard_event = ServoKeyboardEvent::new_without_event(
                            state,
                            key,
                            code,
                            Location::Standard,
                            instance.modifiers,
                            event.repeat,
                            false, // is_composing
                        );
                        
                        if let Some(pane) = instance.panes.get(&instance.active_pane_id) {
                            if pane.is_remote {
                                if let Some(fd) = pane.remote_input_fd {
                                    let surface_id = format!("{}:{}", uuid, pane.id);
                                    let kind = match event.state {
                                        winit::event::ElementState::Pressed => "KeyDown",
                                        winit::event::ElementState::Released => "KeyUp",
                                    };
                                    let key_str = map_winit_logical_key(&event.logical_key);
                                    let code_str = map_winit_physical_key(event.physical_key);
                                    let repeat = event.repeat;
                                    let msg = serde_json::json!({
                                        "type": "Input",
                                        "event": {
                                            "kind": kind,
                                            "surface_id": surface_id,
                                            "generation": 1,
                                            "key": key_str,
                                            "code": code_str,
                                            "keycode": 0,
                                            "modifiers": instance.modifiers.bits(),
                                            "repeat": repeat,
                                        }
                                    }).to_string();
                                    forward_remote_input(fd, &msg);
                                }
                            } else {
                                pane.webview.notify_input_event(InputEvent::Keyboard(keyboard_event));
                            }
                        }
                    },
                    _ => {}
                }
            }
        }

        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }
    }
}

// ------------------------------------------------------------------
// APP STRUCT - Manages global event loop
// ------------------------------------------------------------------
