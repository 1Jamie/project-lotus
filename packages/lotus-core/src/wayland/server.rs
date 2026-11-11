//! Embedded Wayland server state and protocol handlers using Smithay.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use crossbeam_channel::Sender;
use log::{debug, info, warn};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::Buffer;
use smithay::delegate_compositor;
use smithay::delegate_cursor_shape;
use smithay::delegate_dmabuf;
use smithay::delegate_output;
use smithay::delegate_pointer_constraints;
use smithay::delegate_presentation;
use smithay::delegate_relative_pointer;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_viewporter;
use smithay::delegate_xdg_shell;
use smithay::delegate_xwayland_shell;
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::utils::{Logical, Point, Serial, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state};
use smithay::wayland::compositor::{
    with_states, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
};
use smithay::wayland::dmabuf::{
    DmabufFeedback, DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{X11Surface, X11Wm, XwmHandler, XwmId};
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::pointer_constraints::{
    with_pointer_constraint, PointerConstraintsHandler, PointerConstraintsState,
};
use smithay::wayland::presentation::{
    PresentationFeedbackCachedState, PresentationFeedbackCallback, PresentationState, Refresh,
};
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::seat::CURSOR_IMAGE_ROLE;
use smithay::wayland::tablet_manager::TabletSeatHandler;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server;
use wayland_server::protocol::wl_buffer::WlBuffer;
use wayland_server::protocol::wl_callback::WlCallback;
use wayland_server::protocol::wl_seat::WlSeat;
use wayland_server::protocol::wl_surface::WlSurface;
use wayland_server::{Client, DisplayHandle, Resource};

use crate::wayland::dmabuf::DmabufFrameDescriptor;

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    pub pane_id: Option<String>,
}
impl wayland_server::backend::ClientData for ClientState {}

/// Nested client cursor request, forwarded to the host winit window.
#[derive(Debug, Clone)]
pub enum NestedCursor {
    Hidden,
    /// CSS / wp_cursor_shape name (e.g. `"pointer"`, `"text"`).
    Named(String),
    /// RGBA8 (non-premultiplied) bitmap from a cursor `wl_surface` / Xcursor theme.
    Custom {
        rgba: Vec<u8>,
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
    },
}

#[derive(Debug)]
pub enum WaylandEvent {
    FrameCommitted(DmabufFrameDescriptor),
    ShmFrameCommitted {
        surface_id: u32,
        width: u32,
        height: u32,
        stride: u32,
        data: Vec<u8>,
    },
    SurfaceConfigured { width: u32, height: u32 },
    XwaylandReady { display_number: u32 },
    ClientConnected,
    ClientDisconnected,
    WindowCreated {
        pane_id: String,
        surface_id: u32,
        title: String,
        app_id: String,
        role: String,
        width: u32,
        height: u32,
    },
    WindowUpdated {
        pane_id: String,
        surface_id: u32,
        title: String,
        app_id: String,
        width: u32,
        height: u32,
    },
    WindowClosed {
        pane_id: String,
        surface_id: u32,
    },
    CursorChanged(NestedCursor),
}

pub struct LotusWaylandState {
    pub display_handle: DisplayHandle,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: DmabufGlobal,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,
    #[allow(dead_code)] // retained so the wp_cursor_shape global stays alive
    pub cursor_shape_manager_state: CursorShapeManagerState,
    pub output_manager_state: OutputManagerState,
    pub output: Output,
    pub viewporter_state: ViewporterState,
    /// wp_relative_pointer_manager_v1 — FPS relative aim for nested clients.
    #[allow(dead_code)]
    pub relative_pointer_manager_state: RelativePointerManagerState,
    /// zwp_pointer_constraints_v1 — lock/confine requests from games.
    #[allow(dead_code)]
    pub pointer_constraints_state: PointerConstraintsState,
    /// wp_presentation — DXVK/VKD3D presentation_feedback pacing.
    #[allow(dead_code)]
    pub presentation_state: PresentationState,
    /// Feedback objects drained from commits; marked presented after host swap.
    pub pending_presentation_feedbacks: Vec<PresentationFeedbackCallback>,
    /// Presentation sequence counter for wp_presentation_feedback.presented.
    pub presentation_seq: u64,
    /// Last cursor surface used by `wl_pointer.set_cursor` (for commit updates).
    pub current_cursor_surface: Option<WlSurface>,
    /// Last surface we gave pointer focus — used to tag cursor events without
    /// re-entering `PointerHandle` (which deadlocks inside `cursor_image`).
    pub last_pointer_focus: Option<WlSurface>,
    pub toplevel: Option<ToplevelSurface>,
    pub toplevels: Vec<ToplevelSurface>,
    pub toplevel_surfaces: HashMap<u32, ToplevelSurface>,
    pub popup_surfaces: HashMap<u32, PopupSurface>,
    pub x11_surfaces: HashMap<u32, X11Surface>,
    pub pane_toplevel_stacks: HashMap<String, Vec<u32>>,
    pub focused_surface: Option<WlSurface>,
    pub pending_frame_callbacks: Arc<Mutex<Vec<WlCallback>>>,
    pub active_dmabuf: Option<Dmabuf>,
    pub previous_dmabuf: Option<Dmabuf>,
    pub event_sender: Sender<WaylandEvent>,
    pub target_size: (u32, u32),
    pub xwayland_shell_state: XWaylandShellState,
    pub xwm: Option<X11Wm>,
    pub window_id: String,
    pub pane_registrations: Arc<parking_lot::Mutex<Vec<crate::wayland::PaneRegistration>>>,
    pub surface_to_pane: Arc<parking_lot::Mutex<HashMap<u32, String>>>,
    pub known_surfaces: HashMap<u32, WlSurface>,
    /// Host monitor refresh in millihertz (e.g. 60000 = 60 Hz). Advertised on wl_output.
    pub refresh_rate_millihertz: u32,
    /// True while atrium is in game-mode (inputs → primary). Overlay clients must
    /// not receive xdg Activated — Discord suppresses desktop Notify while focused.
    pub host_game_mode: bool,
    /// Last NestedPointerLock state we told the host about (debounce).
    pub last_emitted_pointer_lock: bool,
}

/// CLOCK_MONOTONIC timestamp for wp_presentation (clk_id = 1).
fn monotonic_now() -> std::time::Duration {
    #[cfg(target_os = "linux")]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime with stack timespec; CLOCK_MONOTONIC is valid.
        let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        if rc == 0 {
            return std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32);
        }
    }
    std::time::Duration::from_millis(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    )
}

fn get_toplevel_title_and_app_id(surface: &ToplevelSurface) -> (String, String) {
    with_states(surface.wl_surface(), |states| {
        states.data_map.get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>().and_then(|data| {
            let lock = data.lock().unwrap();
            Some((lock.title.clone().unwrap_or_default(), lock.app_id.clone().unwrap_or_default()))
        })
    }).unwrap_or_default()
}

impl LotusWaylandState {
    pub fn new(
        window_id: String,
        display_handle: DisplayHandle,
        event_sender: Sender<WaylandEvent>,
        target_size: (u32, u32),
        refresh_rate_millihertz: u32,
        pane_registrations: Arc<parking_lot::Mutex<Vec<crate::wayland::PaneRegistration>>>,
        surface_to_pane: Arc<parking_lot::Mutex<HashMap<u32, String>>>,
    ) -> Self {
        let refresh_rate_millihertz = refresh_rate_millihertz.max(1);
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let mut dmabuf_state = DmabufState::new();

        // Advertise supported DMA-BUF formats with GPU render node default feedback
        let main_device = std::fs::metadata("/dev/dri/renderD128")
            .or_else(|_| std::fs::metadata("/dev/dri/card1"))
            .map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.rdev()
            })
            .unwrap_or(0);

        // Advertise Linear + Invalid (all modifiers) for the common 8-bit formats.
        // Xwayland/GBM often picks a tiled modifier; Linear-only feedback leaves it
        // unable to present → nested X11 clients hang until XIO timeout.
        let formats = {
            let codes = [
                drm_fourcc::DrmFourcc::Argb8888,
                drm_fourcc::DrmFourcc::Xrgb8888,
                drm_fourcc::DrmFourcc::Abgr8888,
                drm_fourcc::DrmFourcc::Xbgr8888,
                drm_fourcc::DrmFourcc::Abgr2101010,
                drm_fourcc::DrmFourcc::Argb2101010,
            ];
            let modifiers = [
                drm_fourcc::DrmModifier::Linear,
                drm_fourcc::DrmModifier::Invalid,
            ];
            let mut out = Vec::with_capacity(codes.len() * modifiers.len());
            for code in codes {
                for modifier in modifiers {
                    out.push(drm_fourcc::DrmFormat { code, modifier });
                }
            }
            out
        };

        let default_feedback = DmabufFeedbackBuilder::new(main_device, formats)
            .build()
            .unwrap();

        let dmabuf_global = dmabuf_state.create_global_with_default_feedback::<Self>(
            &display_handle,
            &default_feedback,
        );

        let shm_state = ShmState::new::<Self>(&display_handle, vec![]);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "lotus-nest-seat");

        let _ = seat.add_keyboard(Default::default(), 200, 25);
        let _ = seat.add_pointer();

        // wp_cursor_shape_manager_v1 — Chromium/Discord prefer named shapes over bitmaps.
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&display_handle);

        let xwayland_shell_state = XWaylandShellState::new::<Self>(&display_handle);

        // 1. Advertise wl_output with zxdg_output_manager_v1
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let output = Output::new(
            "lotus-nest-0".into(),
            PhysicalProperties {
                size: (target_size.0 as i32, target_size.1 as i32).into(),
                subpixel: Subpixel::HorizontalRgb,
                make: "Lotus".into(),
                model: "Atrium Virtual Screen".into(),
            },
        );
        let _ = output.create_global::<Self>(&display_handle);
        let mode = Mode {
            size: (target_size.0 as i32, target_size.1 as i32).into(),
            refresh: refresh_rate_millihertz as i32,
        };
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);

        // 2. Advertise wp_viewporter for pixel-perfect source/dest scaling
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);

        // Relative pointer + constraints so nested games (Overwatch etc.) can lock
        // the pointer and receive raw deltas for mouse-look.
        let relative_pointer_manager_state =
            RelativePointerManagerState::new::<Self>(&display_handle);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&display_handle);
        // CLOCK_MONOTONIC = 1 — matches what DXVK/VKD3D expect for presentation_feedback.
        let presentation_state = PresentationState::new::<Self>(&display_handle, 1);

        let refresh_hz = refresh_rate_millihertz as f32 / 1000.0;
        info!(
            "[lotus-nest] initialized nested sub-compositor: wl_output ({}x{}@{:.0}Hz) + wp_viewporter + relative_pointer + pointer_constraints + wp_presentation",
            target_size.0, target_size.1, refresh_hz
        );

        Self {
            display_handle,
            compositor_state,
            xdg_shell_state,
            dmabuf_state,
            dmabuf_global,
            shm_state,
            seat_state,
            seat,
            cursor_shape_manager_state,
            output_manager_state,
            output,
            viewporter_state,
            relative_pointer_manager_state,
            pointer_constraints_state,
            presentation_state,
            pending_presentation_feedbacks: Vec::new(),
            presentation_seq: 0,
            current_cursor_surface: None,
            last_pointer_focus: None,
            toplevel: None,
            toplevels: Vec::new(),
            toplevel_surfaces: HashMap::new(),
            popup_surfaces: HashMap::new(),
            x11_surfaces: HashMap::new(),
            pane_toplevel_stacks: HashMap::new(),
            focused_surface: None,
            pending_frame_callbacks: Arc::new(Mutex::new(Vec::new())),
            active_dmabuf: None,
            previous_dmabuf: None,
            event_sender,
            target_size,
            xwayland_shell_state,
            xwm: None,
            window_id,
            pane_registrations,
            surface_to_pane,
            known_surfaces: HashMap::new(),
            refresh_rate_millihertz,
            // Atrium windows start in game mode.
            host_game_mode: false,
            last_emitted_pointer_lock: false,
        }
    }

    /// Keep nested `wl_output` refresh in lockstep with the host monitor.
    pub fn set_output_refresh(&mut self, millihertz: u32) {
        let millihertz = millihertz.max(1);
        if millihertz == self.refresh_rate_millihertz {
            return;
        }
        self.refresh_rate_millihertz = millihertz;
        let mode = Mode {
            size: (self.target_size.0 as i32, self.target_size.1 as i32).into(),
            refresh: millihertz as i32,
        };
        self.output.change_current_state(
            Some(mode),
            None,
            None,
            None,
        );
        self.output.set_preferred(mode);
        info!(
            "[lotus-nest] nested wl_output refresh → {:.0}Hz",
            millihertz as f32 / 1000.0
        );
    }

    /// Dispatch frame done callbacks (`wl_callback.done`) to unfreeze client swapchain.
    pub fn send_frame_callbacks(&self, timestamp_ms: u32) {
        let mut cbs = self.pending_frame_callbacks.lock().unwrap();
        for cb in cbs.drain(..) {
            cb.done(timestamp_ms);
        }
    }

    /// Mark pending wp_presentation_feedback objects as presented (after host swap).
    /// `timestamp_ms` is ignored for the protocol clock — we use CLOCK_MONOTONIC.
    pub fn send_presentation_feedback(&mut self, _timestamp_ms: u32) {
        let time = monotonic_now();
        let min_period = std::time::Duration::from_nanos(
            1_000_000_000_000u64 / self.refresh_rate_millihertz.max(1) as u64,
        );
        // Advertise variable refresh so VKD3D/DXVK can pace to VRR.
        let refresh = Refresh::variable(min_period);
        self.presentation_seq = self.presentation_seq.wrapping_add(1);
        let seq = self.presentation_seq;
        let feedbacks = std::mem::take(&mut self.pending_presentation_feedbacks);
        for feedback in feedbacks {
            feedback.presented(
                &self.output,
                time,
                refresh,
                seq,
                wp_presentation_feedback::Kind::Vsync,
            );
        }
    }

    fn surface_is_primary(&self, surface: &WlSurface) -> bool {
        let sid = surface.id().protocol_id();
        let Some(pane_id) = self.surface_to_pane.lock().get(&sid).cloned() else {
            return false;
        };
        self.pane_registrations
            .lock()
            .iter()
            .find(|p| p.pane_id == pane_id)
            .map(|p| p.is_primary)
            .unwrap_or(false)
    }

    fn emit_cursor(&self, cursor: NestedCursor) {
        // NEVER call seat.get_pointer() here — `cursor_image` runs while smithay
        // already holds the pointer lock; re-entering deadlocks the nested loop
        // (Xwayland freezes → Overwatch never maps / XIO timeouts).
        let from_primary = self
            .last_pointer_focus
            .as_ref()
            .map(|s| self.surface_is_primary(s))
            .unwrap_or(false);
        let _ = self.event_sender.send(WaylandEvent::CursorChanged(cursor.clone()));
        if let Some(proxy) = crate::app::EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(crate::app::EngineCommand::NestedCursor {
                window_id: self.window_id.clone(),
                cursor,
                from_primary,
            });
        }
    }

    fn emit_pointer_lock(&mut self, active: bool) {
        if self.last_emitted_pointer_lock == active {
            return;
        }
        self.last_emitted_pointer_lock = active;
        if let Some(proxy) = crate::app::EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(crate::app::EngineCommand::NestedPointerLock {
                window_id: self.window_id.clone(),
                active,
            });
        }
    }

    fn sync_pointer_lock_for_surface(&mut self, surface: &WlSurface) {
        let active = if let Some(pointer) = self.seat.get_pointer() {
            with_pointer_constraint(surface, &pointer, |constraint| {
                constraint.map(|c| c.is_active()).unwrap_or(false)
            })
        } else {
            false
        };
        if active {
            self.emit_pointer_lock(true);
        } else if self.host_game_mode {
            // Only clear the sticky shoe while game mode owns the seat. Overlay must
            // not wipe lock state when Discord steals transient pointer focus.
            self.emit_pointer_lock(false);
        }
    }

    /// Read SHM cursor pixels + hotspot into a host-ready `NestedCursor::Custom`.
    fn cursor_from_surface(surface: &WlSurface) -> Option<NestedCursor> {
        let hotspot = with_states(surface, |states| {
            states
                .data_map
                .get::<CursorImageSurfaceData>()
                .and_then(|m| m.lock().ok())
                .map(|attrs| attrs.hotspot)
                .unwrap_or_else(|| (0, 0).into())
        });

        with_renderer_surface_state(surface, |state| {
            let wl_buf = state.buffer()?;
            let shm = smithay::wayland::shm::with_buffer_contents(wl_buf, |ptr, _len, data| {
                use wayland_server::protocol::wl_shm;
                let width = data.width.max(0) as u32;
                let height = data.height.max(0) as u32;
                if width == 0 || height == 0 || width > 2048 || height > 2048 {
                    return None;
                }
                let stride = data.stride.max(0) as usize;
                let src = unsafe {
                    std::slice::from_raw_parts(
                        ptr.offset(data.offset as isize),
                        stride.saturating_mul(height as usize),
                    )
                };
                // wl_shm Argb8888/Xrgb8888 are little-endian → memory order B,G,R,A.
                let mut rgba = Vec::with_capacity((width * height * 4) as usize);
                for y in 0..height as usize {
                    let row = &src[y * stride..];
                    for x in 0..width as usize {
                        let i = x * 4;
                        if i + 3 >= row.len() {
                            break;
                        }
                        let (b, g, r, a) = (row[i], row[i + 1], row[i + 2], row[i + 3]);
                        let a = match data.format {
                            wl_shm::Format::Xrgb8888 => 255,
                            _ => a,
                        };
                        rgba.extend_from_slice(&[r, g, b, a]);
                    }
                }
                Some((rgba, width as u16, height as u16))
            })
            .ok()
            .flatten()?;

            let (rgba, width, height) = shm;
            // 1×1 or fully transparent = Wine/Xwayland "hide cursor" without set_cursor(None).
            if width <= 1 && height <= 1 {
                return Some(NestedCursor::Hidden);
            }
            if rgba.len() >= 4 && rgba.chunks_exact(4).all(|px| px[3] == 0) {
                return Some(NestedCursor::Hidden);
            }
            let hotspot_x = (hotspot.x.max(0) as u16).min(width.saturating_sub(1));
            let hotspot_y = (hotspot.y.max(0) as u16).min(height.saturating_sub(1));
            Some(NestedCursor::Custom {
                rgba,
                width,
                height,
                hotspot_x,
                hotspot_y,
            })
        })
        .flatten()
    }

    /// Set input focus to a specific Wayland surface.
    pub fn set_focus_surface(&mut self, surface: Option<WlSurface>) {
        self.focused_surface = surface.clone();
        // Before pointer.motion — cursor_image must not re-enter PointerHandle.
        self.last_pointer_focus = surface.clone();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, surface.clone(), serial);
        }
        if let Some(pointer) = self.seat.get_pointer() {
            let target = surface.map(|s| (s, Point::from((0.0, 0.0))));
            pointer.motion(self, target, &smithay::input::pointer::MotionEvent {
                location: Point::from((0.0, 0.0)),
                serial,
                time: 0,
            });
        }
    }

    pub fn clear_keyboard_focus(&mut self) {
        self.focused_surface = None;
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, None, serial);
        }
    }

    pub fn clear_pointer_focus(&mut self) {
        self.last_pointer_focus = None;
        if let Some(pointer) = self.seat.get_pointer() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            let location = pointer.current_location();
            let now_ms = self.now_ms();
            pointer.motion(
                self,
                None,
                &smithay::input::pointer::MotionEvent {
                    location,
                    serial,
                    time: now_ms,
                },
            );
            pointer.frame(self);
        }
        // Do NOT emit NestedPointerLock(false) here — host-initiated leave must not
        // wipe the game's sticky lock shoe while the user is in overlay.
    }

    /// Drop seat focus if it currently points at a surface owned by `pane_id`.
    pub fn clear_focus_if_pane(&mut self, pane_id: &str) {
        let focused_on_pane = self.focused_surface.as_ref().and_then(|s| {
            let sid = s.id().protocol_id();
            self.surface_to_pane
                .lock()
                .get(&sid)
                .map(|p| p.as_str() == pane_id)
        });
        if focused_on_pane == Some(true) {
            self.clear_keyboard_focus();
            self.clear_pointer_focus();
        }
    }

    /// Map/unmap + Activated bits for a pane. Overlay panes stay deactivated while
    /// `host_game_mode` so Chromium/Discord keep emitting session-bus Notify.
    pub fn set_pane_visible(&mut self, pane_id: &str, visible: bool) {
        let is_primary = {
            let panes = self.pane_registrations.lock();
            panes
                .iter()
                .find(|p| p.pane_id == pane_id)
                .map(|p| p.is_primary)
                .unwrap_or(false)
        };
        let allow_activated = visible && (is_primary || !self.host_game_mode);
        if let Some(stack) = self.pane_toplevel_stacks.get(pane_id) {
            for &top_id in stack {
                if let Some(top) = self.toplevel_surfaces.get(&top_id) {
                    top.with_pending_state(|s| {
                        if visible {
                            s.states.unset(xdg_toplevel::State::Suspended);
                        } else {
                            s.states.set(xdg_toplevel::State::Suspended);
                        }
                        if allow_activated {
                            s.states.set(xdg_toplevel::State::Activated);
                        } else {
                            s.states.unset(xdg_toplevel::State::Activated);
                        }
                    });
                    top.send_configure();
                }
            }
        }
        // X11 activated bit (Wine); Suspended has no direct X11 equivalent here.
        let s2p = self.surface_to_pane.lock();
        let x11_ids: Vec<u32> = self
            .x11_surfaces
            .keys()
            .filter(|id| s2p.get(id).map(|p| p.as_str()) == Some(pane_id))
            .copied()
            .collect();
        drop(s2p);
        for sid in x11_ids {
            if let Some(window) = self.x11_surfaces.get(&sid) {
                let _ = window.set_activated(allow_activated);
            }
        }
        if !visible {
            self.clear_focus_if_pane(pane_id);
        }
    }

    /// Clear xdg Suspended without changing Activated — used when leaving game mode
    /// so Chromium/Discord resume their compositor after a long throttled stretch.
    pub fn unsuspend_pane(&mut self, pane_id: &str) {
        if let Some(stack) = self.pane_toplevel_stacks.get(pane_id) {
            for &top_id in stack {
                if let Some(top) = self.toplevel_surfaces.get(&top_id) {
                    top.with_pending_state(|s| {
                        s.states.unset(xdg_toplevel::State::Suspended);
                        s.states.set(xdg_toplevel::State::TiledLeft);
                        s.states.set(xdg_toplevel::State::TiledRight);
                        s.states.set(xdg_toplevel::State::TiledTop);
                        s.states.set(xdg_toplevel::State::TiledBottom);
                    });
                    top.send_configure();
                }
            }
        }
    }

    /// Tell a pane's clients they are (or are not) the activated / focused window.
    /// Discord gates desktop notifications on this.
    pub fn set_pane_activated(&mut self, pane_id: &str, activated: bool) {
        if let Some(stack) = self.pane_toplevel_stacks.get(pane_id) {
            for &top_id in stack {
                if let Some(top) = self.toplevel_surfaces.get(&top_id) {
                    top.with_pending_state(|s| {
                        // Keep tiled bits sticky so Discord never re-enables CSD resize.
                        s.states.set(xdg_toplevel::State::TiledLeft);
                        s.states.set(xdg_toplevel::State::TiledRight);
                        s.states.set(xdg_toplevel::State::TiledTop);
                        s.states.set(xdg_toplevel::State::TiledBottom);
                        if activated {
                            s.states.set(xdg_toplevel::State::Activated);
                            s.states.unset(xdg_toplevel::State::Suspended);
                        } else {
                            s.states.unset(xdg_toplevel::State::Activated);
                        }
                    });
                    top.send_configure();
                }
            }
        }
        let s2p = self.surface_to_pane.lock();
        let x11_ids: Vec<u32> = self
            .x11_surfaces
            .keys()
            .filter(|id| s2p.get(id).map(|p| p.as_str()) == Some(pane_id))
            .copied()
            .collect();
        drop(s2p);
        for sid in x11_ids {
            if let Some(window) = self.x11_surfaces.get(&sid) {
                let _ = window.set_activated(activated);
            }
        }
    }

    fn surface_for_pane(&self, pane_id: &str) -> Option<WlSurface> {
        let s2p = self.surface_to_pane.lock();
        // Prefer the newest popup/menu surface for this pane when present.
        let mut popup_ids: Vec<u32> = self
            .popup_surfaces
            .keys()
            .filter(|id| s2p.get(id).map(|p| p.as_str()) == Some(pane_id))
            .copied()
            .collect();
        popup_ids.sort_unstable();
        if let Some(&top_popup) = popup_ids.last() {
            if let Some(surf) = self.known_surfaces.get(&top_popup) {
                return Some(surf.clone());
            }
        }
        drop(s2p);
        if let Some(stack) = self.pane_toplevel_stacks.get(pane_id) {
            if let Some(&top_id) = stack.last() {
                if let Some(surf) = self.known_surfaces.get(&top_id) {
                    return Some(surf.clone());
                }
            }
        }
        let panes = self.pane_registrations.lock();
        let reg = panes.iter().find(|p| p.pane_id == pane_id)?;
        let sid = reg.active_surface_id.load(Ordering::SeqCst);
        let sid = if sid != u32::MAX {
            sid
        } else {
            reg.surface_id.load(Ordering::SeqCst)
        };
        if sid == u32::MAX {
            return None;
        }
        self.known_surfaces.get(&sid).cloned()
    }

    fn now_ms(&self) -> u32 {
        (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() & 0xFFFFFFFF) as u32
    }

    /// Docked slot configure: size hint + fully tiled so Chromium/Electron disable
    /// CSD resize borders (otherwise edge/stale clicks become nwse-resize grabs).
    /// Never Maximized — Chromium treats that as fill-wl_output.
    fn set_docked_toplevel_states(
        states: &mut smithay::wayland::shell::xdg::ToplevelState,
        size: (u32, u32),
        activated: bool,
    ) {
        states.size = Some((size.0 as i32, size.1 as i32).into());
        states.states.set(xdg_toplevel::State::TiledLeft);
        states.states.set(xdg_toplevel::State::TiledRight);
        states.states.set(xdg_toplevel::State::TiledTop);
        states.states.set(xdg_toplevel::State::TiledBottom);
        if activated {
            states.states.set(xdg_toplevel::State::Activated);
        } else {
            states.states.unset(xdg_toplevel::State::Activated);
        }
    }

    /// Push an xdg_toplevel configure for a pane so the client matches `target_size`.
    pub fn reconfigure_pane_size(&mut self, pane_id: &str, width: u32, height: u32) {
        if let Some(stack) = self.pane_toplevel_stacks.get(pane_id) {
            for &top_id in stack {
                if let Some(top) = self.toplevel_surfaces.get(&top_id) {
                    top.with_pending_state(|s| {
                        Self::set_docked_toplevel_states(s, (width, height), true);
                    });
                    top.send_configure();
                }
            }
        }
        if let Some(ref _xwm) = self.xwm {
            let s2p = self.surface_to_pane.lock();
            for (&sid, surface) in &self.x11_surfaces {
                if s2p.get(&sid) == Some(&pane_id.to_string()) {
                    let _ = surface.configure(Some(smithay::utils::Rectangle::new(
                        (0, 0).into(),
                        (width as i32, height as i32).into(),
                    )));
                }
            }
        }
    }

    /// Focus a pane's active surface and deliver pointer motion in one event-loop tick.
    ///
    /// Smithay's focus tuple is `(surface, surface_origin_in_global_space)`. Pane-local
    /// input treats each surface as originating at (0,0); surface-local pointer coords are
    /// then `location - origin`. Passing the pointer position as the origin (as we used to)
    /// made clients see ~`(0,0)` or a fixed enter-point offset — matching Discord hover
    /// appearing far up/left of the real cursor.
    pub fn dispatch_pointer_motion_to_pane(&mut self, pane_id: &str, x: f64, y: f64) {
        let Some(surface) = self.surface_for_pane(pane_id) else {
            return;
        };
        // Record before motion — cursor_image may fire during enter and must not
        // call back into PointerHandle.
        self.last_pointer_focus = Some(surface.clone());
        // Pointer focus only — keyboard focus is sticky (set on click / key route).
        // Hover must not steal seat keyboard focus from the click-focused surface.
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let now_ms = self.now_ms();
        let location = Point::from((x, y));
        let surface_origin = Point::from((0.0, 0.0));
        if let Some(pointer) = self.seat.get_pointer() {
            let focus = Some((surface.clone(), surface_origin));
            pointer.motion(
                self,
                focus,
                &smithay::input::pointer::MotionEvent {
                    location,
                    serial,
                    time: now_ms,
                },
            );
            // Re-activate lock/confine after enter (leave deactivates oneshot/persistent).
            with_pointer_constraint(&surface, &pointer, |constraint| {
                if let Some(c) = constraint {
                    if !c.is_active() {
                        c.activate();
                    }
                }
            });
            pointer.frame(self);
        }
        self.sync_pointer_lock_for_surface(&surface);
    }

    pub fn dispatch_pointer_button_to_pane(
        &mut self,
        pane_id: &str,
        x: f64,
        y: f64,
        button: u32,
        pressed: bool,
    ) {
        // Skip absolute re-motion only while the game lock shoe is active — otherwise
        // Chromium/Discord CSD hit-tests a stale enter point (often 0,0 → resize grip).
        let already_focused = self
            .surface_for_pane(pane_id)
            .is_some_and(|s| self.last_pointer_focus.as_ref() == Some(&s));
        let skip_motion = already_focused && self.last_emitted_pointer_lock;
        if !skip_motion {
            self.dispatch_pointer_motion_to_pane(pane_id, x, y);
        }
        // Click acquires keyboard focus (normal WM behavior).
        if pressed {
            if let Some(surface) = self.surface_for_pane(pane_id) {
                self.focused_surface = Some(surface.clone());
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.set_focus(self, Some(surface), serial);
                }
            }
        }
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let now_ms = self.now_ms();
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.button(
                self,
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
            pointer.frame(self);
        }
    }

    pub fn dispatch_pointer_axis_to_pane(
        &mut self,
        pane_id: &str,
        x: f64,
        y: f64,
        horizontal: f64,
        vertical: f64,
    ) {
        let already_focused = self
            .surface_for_pane(pane_id)
            .is_some_and(|s| self.last_pointer_focus.as_ref() == Some(&s));
        if !already_focused {
            self.dispatch_pointer_motion_to_pane(pane_id, x, y);
        }
        let now_ms = self.now_ms();
        if let Some(pointer) = self.seat.get_pointer() {
            let mut frame = smithay::input::pointer::AxisFrame::new(now_ms)
                .source(smithay::backend::input::AxisSource::Wheel);
            if horizontal.abs() > 0.0 {
                frame = frame.value(smithay::backend::input::Axis::Horizontal, horizontal);
            }
            if vertical.abs() > 0.0 {
                frame = frame.value(smithay::backend::input::Axis::Vertical, vertical);
            }
            pointer.axis(self, frame);
            pointer.frame(self);
        }
    }

    pub fn dispatch_keyboard_to_pane(&mut self, pane_id: &str, keycode: u32, pressed: bool) {
        let Some(surface) = self.surface_for_pane(pane_id) else {
            return;
        };
        let focus_changed = self
            .focused_surface
            .as_ref()
            .map(|s| s != &surface)
            .unwrap_or(true);
        self.focused_surface = Some(surface.clone());
        let now_ms = self.now_ms();
        if let Some(keyboard) = self.seat.get_keyboard() {
            // Only (re)enter when the focused surface changes — per-key set_focus
            // churns enter/leave for Electron and feels like delayed/duped typing.
            if focus_changed {
                let focus_serial = smithay::utils::SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, Some(surface), focus_serial);
            }
            let key_serial = smithay::utils::SERIAL_COUNTER.next_serial();
            let xkb_keycode = smithay::input::keyboard::Keycode::new(keycode);
            let key_state = if pressed {
                smithay::backend::input::KeyState::Pressed
            } else {
                smithay::backend::input::KeyState::Released
            };
            keyboard.input(
                self,
                xkb_keycode,
                key_state,
                key_serial,
                now_ms,
                |_, _, _| smithay::input::keyboard::FilterResult::<()>::Forward,
            );
        }
    }
}

// 1. Compositor protocol handler
impl CompositorHandler for LotusWaylandState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(state) = client.get_data::<ClientState>() {
            &state.compositor_state
        } else if let Some(state) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            &state.compositor_state
        } else {
            panic!("Unknown client state type connected to sub-compositor");
        }
    }

    fn commit(&mut self, surface: &WlSurface) {
        let surface_id = surface.id().protocol_id();
        self.known_surfaces.insert(surface_id, surface.clone());

        // Track buffer state the way smithay desktop helpers expect. Harmless for our
        // own import path; required for correct Xwayland buffer lifecycle.
        on_commit_buffer_handler::<Self>(surface);

        // Check surface role: cursor surfaces must NEVER be assigned to any pane,
        // nor should their buffer updates be treated as application window frames.
        let role = smithay::wayland::compositor::get_role(surface);
        let is_cursor = role == Some(CURSOR_IMAGE_ROLE);

        // Cursor bitmap updates — Wine/Xwayland often hides via NULL/transparent buffer
        // commit and restores by attaching a real buffer + commit *without* set_cursor.
        let is_active_cursor = is_cursor
            || self
                .current_cursor_surface
                .as_ref()
                .is_some_and(|s| s == surface);
        if is_active_cursor {
            with_states(surface, |states| {
                let mut attrs = states.cached_state.get::<SurfaceAttributes>();
                let current = attrs.current();
                if !current.frame_callbacks.is_empty() {
                    let mut cbs = self.pending_frame_callbacks.lock().unwrap();
                    cbs.extend(std::mem::take(&mut current.frame_callbacks));
                }
            });
            self.current_cursor_surface = Some(surface.clone());
            match Self::cursor_from_surface(surface) {
                Some(cursor) => self.emit_cursor(cursor),
                // NULL / empty / fully-transparent buffer → hidden (OW load→hero-select).
                None => self.emit_cursor(NestedCursor::Hidden),
            }
            // Lock may have been destroyed without a motion — re-sync shoes.
            if let Some(focus) = self.last_pointer_focus.clone() {
                self.sync_pointer_lock_for_surface(&focus);
            }
            return;
        }

        // Dynamic surface-to-pane mapping if not yet registered and not a cursor
        if !is_cursor {
            let mut s2p = self.surface_to_pane.lock();
            if !s2p.contains_key(&surface_id) {
                // Check if surface is a subsurface of an existing parent
                let parent_pane = smithay::wayland::compositor::get_parent(surface)
                    .and_then(|p| s2p.get(&p.id().protocol_id()).cloned());

                // Or check connection client tag
                let client_pane = surface.client().and_then(|c| {
                    c.get_data::<ClientState>().and_then(|cs| cs.pane_id.clone())
                });

                // Tagged clients (Discord's dedicated socket) stay on their pane.
                // Untagged (Xwayland) always go to primary — never steal Discord's slot.
                let pane_id_to_assign = parent_pane.or(client_pane);

                let mut panes = self.pane_registrations.lock();
                let matched_pos = if let Some(ref target_id) = pane_id_to_assign {
                    panes.iter().position(|p| &p.pane_id == target_id)
                } else {
                    panes.iter().position(|p| p.is_primary)
                };
                if let Some(pos) = matched_pos {
                    let pane = &mut panes[pos];
                    if pane.surface_id.load(Ordering::SeqCst) == u32::MAX {
                        pane.surface_id.store(surface_id, Ordering::SeqCst);
                    }
                    // Don't flip active on every untagged commit — Steam helpers would win.
                    // Size-based promotion happens in pump_frames; only seed if empty.
                    if pane.active_surface_id.load(Ordering::SeqCst) == u32::MAX {
                        pane.active_surface_id.store(surface_id, Ordering::SeqCst);
                    }
                    s2p.insert(surface_id, pane.pane_id.clone());
                    debug!("[lotus-nest] dynamically assigned committed surface {} to pane '{}'", surface_id, pane.pane_id);
                }
            }
        }

        if !is_cursor && self.focused_surface.is_none() {
            self.set_focus_surface(Some(surface.clone()));
        }

        let mut new_presentation_feedbacks = Vec::new();
        with_states(surface, |states| {
            let mut attrs = states.cached_state.get::<SurfaceAttributes>();
            let current = attrs.current();

            // Collect any pending frame callbacks requested by the client
            if !current.frame_callbacks.is_empty() {
                let mut cbs = self.pending_frame_callbacks.lock().unwrap();
                cbs.extend(std::mem::take(&mut current.frame_callbacks));
            }

            // Drain wp_presentation_feedback — presented only after host eglSwapBuffers.
            let mut feedback_state = states
                .cached_state
                .get::<PresentationFeedbackCachedState>();
            let callbacks = std::mem::take(&mut feedback_state.current().callbacks);
            if !callbacks.is_empty() {
                new_presentation_feedbacks = callbacks;
            }
        });
        if !new_presentation_feedbacks.is_empty() {
            self.pending_presentation_feedbacks
                .extend(new_presentation_feedbacks);
        }

        // IMPORTANT: `on_commit_buffer_handler` above *takes* BufferAssignment::NewBuffer
        // out of SurfaceAttributes. Reading attrs.buffer afterwards always looks like
        // "no buffer change" — which is why Discord/game panes went blank. Pull the
        // live buffer from RendererSurfaceState instead (works for first attach AND
        // later damage-only commits that keep the same wl_buffer).
        if !is_cursor {
            let imported = with_renderer_surface_state(surface, |state| {
                let Some(wl_buf) = state.buffer() else {
                    return false;
                };
                match smithay::wayland::dmabuf::get_dmabuf(wl_buf) {
                    Ok(dmabuf) => {
                        {
                            use std::sync::atomic::{AtomicU64, Ordering};
                            static DMA_COMMITS: AtomicU64 = AtomicU64::new(0);
                            let n = DMA_COMMITS.fetch_add(1, Ordering::Relaxed) + 1;
                            if n <= 12 || n % 240 == 0 {
                                info!(
                                    "[lotus-nest] surface {} committed DMA-BUF #{} ({}x{})",
                                    surface_id,
                                    n,
                                    dmabuf.width(),
                                    dmabuf.height()
                                );
                            }
                        }
                        let desc = DmabufFrameDescriptor::new(surface_id, dmabuf.clone());
                        let _ = self.event_sender.send(WaylandEvent::FrameCommitted(desc));
                        // Keep only the live buffer + one previous. Drop anything older
                        // immediately so a stalled host cannot pin the whole swapchain.
                        let old = self.active_dmabuf.replace(dmabuf.clone());
                        self.previous_dmabuf = old;
                        true
                    }
                    Err(dma_err) => {
                        let shm_res = smithay::wayland::shm::with_buffer_contents(
                            wl_buf,
                            |ptr, _len, data| {
                                let slice = unsafe {
                                    std::slice::from_raw_parts(
                                        ptr.offset(data.offset as isize),
                                        (data.stride * data.height) as usize,
                                    )
                                };
                                (
                                    data.width as u32,
                                    data.height as u32,
                                    data.stride as u32,
                                    slice.to_vec(),
                                )
                            },
                        );
                        match shm_res {
                            Ok((width, height, stride, bytes)) => {
                                log::trace!(
                                    "[lotus-nest] surface {} committed SHM buffer ({}x{})",
                                    surface_id, width, height
                                );
                                let _ = self.event_sender.send(WaylandEvent::ShmFrameCommitted {
                                    surface_id,
                                    width,
                                    height,
                                    stride,
                                    data: bytes,
                                });
                                true
                            }
                            Err(shm_err) => {
                                warn!(
                                    "[lotus-nest] surface {} buffer could not be imported as DMA-BUF ({:?}) nor SHM ({:?})",
                                    surface_id, dma_err, shm_err
                                );
                                false
                            }
                        }
                    }
                }
            });
            if imported == Some(true) {
                // Game-driven present: primary commit wakes host for composite+swap.
                // Overlay commits piggyback (no independent swap) while the game is live.
                // FrameDone / wp_presentation fire only AFTER that swap (not here).
                use std::sync::atomic::Ordering;
                crate::app::NESTED_REDRAW_GEN.fetch_add(1, Ordering::SeqCst);
                let is_primary = {
                    let s2p = self.surface_to_pane.lock();
                    let panes = self.pane_registrations.lock();
                    s2p.get(&surface_id)
                        .and_then(|pid| panes.iter().find(|p| &p.pane_id == pid))
                        .map(|p| p.is_primary)
                        .unwrap_or(false)
                };
                let wake = if is_primary {
                    crate::app::nest_note_primary_game_commit();
                    // Constraint may have been destroyed without motion — refresh shoes.
                    if let Some(focus) = self.last_pointer_focus.clone() {
                        self.sync_pointer_lock_for_surface(&focus);
                    }
                    true
                } else if !self.host_game_mode {
                    // Overlay mode: Discord/HUD must drive their own presents — piggybacking
                    // on a live game leaves them FrameDone-starved and visually frozen.
                    true
                } else {
                    // Game mode: piggyback on primary commits (VRR); wake only if game stalled.
                    !crate::app::nest_game_commit_fresh(std::time::Duration::from_millis(50))
                };
                if wake && !crate::app::NESTED_REDRAW_PENDING.swap(true, Ordering::SeqCst) {
                    if let Some(proxy) = crate::app::EVENT_LOOP_PROXY.get() {
                        let _ = proxy.send_event(crate::app::EngineCommand::RedrawWindow(
                            self.window_id.clone(),
                        ));
                    } else {
                        crate::app::NESTED_REDRAW_PENDING.store(false, Ordering::SeqCst);
                    }
                }
            }
        }
        // Intentionally do NOT send_frame_callbacks here. Host acks FrameDone only
        // after a real swap so clients (Xwayland/Discord) pace to vsync / VRR.
    }
}
delegate_compositor!(LotusWaylandState);

// 2. Output and Viewporter protocol handlers
impl OutputHandler for LotusWaylandState {}
delegate_output!(LotusWaylandState);
delegate_viewporter!(LotusWaylandState);

// 3. XDG Shell protocol handler
impl XdgShellHandler for LotusWaylandState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let surface_id = surface.wl_surface().id().protocol_id();
        self.known_surfaces.insert(surface_id, surface.wl_surface().clone());
        self.toplevel_surfaces.insert(surface_id, surface.clone());

        // 1. Determine owning pane:
        // First priority: dedicated connection pane tag (ClientState.pane_id)
        let client_pane_id = surface.wl_surface().client().and_then(|c| {
            c.get_data::<ClientState>().and_then(|cs| cs.pane_id.clone())
        });

        let mut assigned_size = self.target_size;
        let mut owning_pane_id: Option<String> = None;
        let mut prev_toplevel_to_deactivate: Option<ToplevelSurface> = None;

        {
            let mut panes = self.pane_registrations.lock();
            let mut s2p = self.surface_to_pane.lock();

            let matched_pos = if let Some(ref target_pid) = client_pane_id {
                panes.iter().position(|p| &p.pane_id == target_pid)
            } else {
                panes.iter().position(|p| p.is_primary)
            };

            if let Some(pos) = matched_pos {
                let pane = &mut panes[pos];
                let p_id = pane.pane_id.clone();
                owning_pane_id = Some(p_id.clone());
                pane.surface_id.store(surface_id, Ordering::SeqCst);
                pane.active_surface_id.store(surface_id, Ordering::SeqCst);
                s2p.insert(surface_id, p_id.clone());
                assigned_size = pane.target_size;

                // Deactivate previous top of stack if any
                let stack = self.pane_toplevel_stacks.entry(p_id.clone()).or_default();
                if let Some(&prev_id) = stack.last() {
                    if let Some(prev) = self.toplevel_surfaces.get(&prev_id) {
                        prev_toplevel_to_deactivate = Some(prev.clone());
                    }
                }
                stack.push(surface_id);

                debug!("[lotus-nest] assigned Wayland toplevel surface {} to pane '{}' (target size: {}x{})", surface_id, p_id, assigned_size.0, assigned_size.1);
            } else {
                debug!("[lotus-nest] new XDG toplevel surface {} registered (fallback {}x{})", surface_id, assigned_size.0, assigned_size.1);
            }
        }

        if let Some(prev) = prev_toplevel_to_deactivate {
            prev.with_pending_state(|s| {
                s.states.unset(xdg_toplevel::State::Activated);
            });
            prev.send_configure();
        }

        // Activated: primary always; overlay only when host is NOT in game mode.
        // Otherwise Discord thinks it is focused and suppresses desktop Notify.
        let is_primary_owner = owning_pane_id.as_ref().and_then(|pid| {
            self.pane_registrations
                .lock()
                .iter()
                .find(|p| &p.pane_id == pid)
                .map(|p| p.is_primary)
        });
        let grant_activated = match is_primary_owner {
            Some(true) => true,
            Some(false) => !self.host_game_mode,
            None => !self.host_game_mode,
        };
        surface.with_pending_state(|state| {
            Self::set_docked_toplevel_states(state, assigned_size, grant_activated);
        });
        surface.send_configure();
        if let Some(ref pid) = owning_pane_id {
            if let Some(p) = self
                .pane_registrations
                .lock()
                .iter()
                .find(|p| &p.pane_id == pid)
            {
                *p.last_configured_size.lock() = assigned_size;
            }
        }

        if grant_activated {
            self.set_focus_surface(Some(surface.wl_surface().clone()));
        }
        self.toplevels.push(surface.clone());
        self.toplevel = Some(surface.clone());

        if let Some(p_id) = owning_pane_id {
            let (title, app_id) = get_toplevel_title_and_app_id(&surface);
            let _ = self.event_sender.send(WaylandEvent::WindowCreated {
                pane_id: p_id,
                surface_id,
                title,
                app_id,
                role: "toplevel".into(),
                width: assigned_size.0,
                height: assigned_size.1,
            });
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let surface_id = surface.wl_surface().id().protocol_id();
        self.known_surfaces.remove(&surface_id);
        self.toplevel_surfaces.remove(&surface_id);
        self.toplevels.retain(|t| t.wl_surface().id().protocol_id() != surface_id);

        let mut pane_to_notify: Option<(String, u32)> = None;
        let mut new_active_top: Option<ToplevelSurface> = None;

        for (pane_id, stack) in self.pane_toplevel_stacks.iter_mut() {
            if let Some(pos) = stack.iter().position(|&id| id == surface_id) {
                stack.remove(pos);
                debug!("[lotus-nest] toplevel surface {} destroyed on pane '{}'", surface_id, pane_id);
                if let Some(&top_id) = stack.last() {
                    pane_to_notify = Some((pane_id.clone(), top_id));
                    if let Some(top) = self.toplevel_surfaces.get(&top_id) {
                        new_active_top = Some(top.clone());
                    }
                }
                let _ = self.event_sender.send(WaylandEvent::WindowClosed {
                    pane_id: pane_id.clone(),
                    surface_id,
                });
                break;
            }
        }

        if let Some((pane_id, next_id)) = pane_to_notify {
            {
                let panes = self.pane_registrations.lock();
                if let Some(p) = panes.iter().find(|p| p.pane_id == pane_id) {
                    p.active_surface_id.store(next_id, Ordering::SeqCst);
                    p.surface_id.store(next_id, Ordering::SeqCst);
                }
            }
            if let Some(top) = new_active_top {
                let pane_is_primary = {
                    let panes = self.pane_registrations.lock();
                    panes
                        .iter()
                        .find(|p| p.pane_id == pane_id)
                        .map(|p| p.is_primary)
                        .unwrap_or(false)
                };
                let grant = pane_is_primary || !self.host_game_mode;
                top.with_pending_state(|s| {
                    if grant {
                        s.states.set(xdg_toplevel::State::Activated);
                    } else {
                        s.states.unset(xdg_toplevel::State::Activated);
                    }
                });
                top.send_configure();
                if grant {
                    self.set_focus_surface(Some(top.wl_surface().clone()));
                }
            }
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let surface_id = surface.wl_surface().id().protocol_id();
        let (title, app_id) = get_toplevel_title_and_app_id(&surface);
        let s2p = self.surface_to_pane.lock();
        if let Some(pane_id) = s2p.get(&surface_id) {
            let _ = self.event_sender.send(WaylandEvent::WindowUpdated {
                pane_id: pane_id.clone(),
                surface_id,
                title,
                app_id,
                width: 0,
                height: 0,
            });
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.title_changed(surface);
    }

    fn move_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: WlSeat,
        _serial: Serial,
    ) {
        // Atrium owns dock geometry — ignore Chromium CSD move grabs.
    }

    fn resize_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: WlSeat,
        _serial: Serial,
        _edges: xdg_toplevel::ResizeEdge,
    ) {
        // Atrium owns dock geometry — ignore Chromium CSD resize grabs.
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let surface_id = surface.wl_surface().id().protocol_id();
        self.known_surfaces.insert(surface_id, surface.wl_surface().clone());
        self.popup_surfaces.insert(surface_id, surface.clone());

        if let Some(parent) = surface.get_parent_surface() {
            let parent_id = parent.id().protocol_id();
            let mut s2p = self.surface_to_pane.lock();
            if let Some(pane_id) = s2p.get(&parent_id).cloned() {
                s2p.insert(surface_id, pane_id.clone());
                debug!("[lotus-nest] popup surface {} associated with parent {} on pane '{}'", surface_id, parent_id, pane_id);
            }
        }

        let _ = surface.send_configure();
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}
    fn reposition_request(&mut self, _surface: PopupSurface, _positioner: PositionerState, _token: u32) {}
}
delegate_xdg_shell!(LotusWaylandState);

// 4. DMA-BUF protocol handler
impl BufferHandler for LotusWaylandState {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl DmabufHandler for LotusWaylandState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, _dmabuf: Dmabuf, notifier: ImportNotifier) {
        let _ = notifier.successful::<LotusWaylandState>();
    }

    fn new_surface_feedback(&mut self, _surface: &WlSurface, _global: &DmabufGlobal) -> Option<DmabufFeedback> {
        None
    }
}
delegate_dmabuf!(LotusWaylandState);

// 5. SHM protocol handler (fallback)
impl ShmHandler for LotusWaylandState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
delegate_shm!(LotusWaylandState);

// 6. Seat protocol handler
impl SeatHandler for LotusWaylandState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        match image {
            CursorImageStatus::Hidden => {
                self.current_cursor_surface = None;
                self.emit_cursor(NestedCursor::Hidden);
            }
            CursorImageStatus::Named(icon) => {
                self.current_cursor_surface = None;
                self.emit_cursor(NestedCursor::Named(icon.name().to_string()));
            }
            CursorImageStatus::Surface(surface) => {
                self.current_cursor_surface = Some(surface.clone());
                // Buffer may arrive on a later commit — emit when pixels are ready.
                if let Some(cursor) = Self::cursor_from_surface(&surface) {
                    self.emit_cursor(cursor);
                }
            }
        }
    }
}
delegate_seat!(LotusWaylandState);

impl TabletSeatHandler for LotusWaylandState {}
delegate_cursor_shape!(LotusWaylandState);

impl PointerConstraintsHandler for LotusWaylandState {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // Activate immediately when the constrained surface already has pointer focus
        // (games typically lock on match start while still focused).
        // Use last_pointer_focus — do not call pointer.current_focus() while smithay
        // may already hold the PointerHandle (same deadlock class as cursor_image).
        let focused = self.last_pointer_focus.as_ref() == Some(surface);
        if focused {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(c) = constraint {
                    c.activate();
                }
            });
            self.emit_pointer_lock(true);
        }
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: Point<f64, Logical>,
    ) {
    }
}
delegate_pointer_constraints!(LotusWaylandState);
delegate_relative_pointer!(LotusWaylandState);
delegate_presentation!(LotusWaylandState);

impl LotusWaylandState {
    /// Bind an X11 window's wl_surface to the **primary/game** pane.
    ///
    /// All X11 (Proton/Wine) windows belong on the game pane at the fixed nested
    /// output size — never on Discord's dock. Steam helper 1×1 windows used to
    /// "claim" primary, which then dumped the real game window onto the Discord
    /// pane at the wrong size (transparent atrium + broken overlay).
    fn assign_x11_window_to_pane(&mut self, window: &X11Surface) -> (u32, u32) {
        // Always the virtual output (ATRIUM_GAME_*), not the lotus pane layout size.
        let w = self.target_size.0;
        let h = self.target_size.1;
        let title = window.title();

        let Some(wl_surface) = window.wl_surface() else {
            return (w, h);
        };

        let surface_id = wl_surface.id().protocol_id();
        self.known_surfaces.insert(surface_id, wl_surface.clone());
        self.x11_surfaces.insert(surface_id, window.clone());

        {
            let panes = self.pane_registrations.lock();
            let mut s2p = self.surface_to_pane.lock();
            if let Some(pane) = panes.iter().find(|p| p.is_primary) {
                // Keep active_surface on the largest/most recent real window when possible.
                let prev = pane.active_surface_id.load(Ordering::SeqCst);
                let promote = prev == u32::MAX
                    || !title.is_empty()
                    || window.is_fullscreen();
                if pane.surface_id.load(Ordering::SeqCst) == u32::MAX {
                    pane.surface_id.store(surface_id, Ordering::SeqCst);
                }
                if promote {
                    pane.active_surface_id.store(surface_id, Ordering::SeqCst);
                }
                s2p.insert(surface_id, pane.pane_id.clone());
                debug!(
                    "[lotus-nest] assigned X11 surface {} ({:?}) → primary '{}' at output {}x{}",
                    surface_id, title, pane.pane_id, w, h
                );
            }
        }

        if self.focused_surface.is_none() || window.is_fullscreen() {
            self.set_focus_surface(Some(wl_surface));
        }
        (w, h)
    }

    fn configure_x11_to_pane(&mut self, window: &X11Surface) {
        let (w, h) = self.assign_x11_window_to_pane(window);
        let _ = window.configure(Some(smithay::utils::Rectangle::new(
            (0, 0).into(),
            (w as i32, h as i32).into(),
        )));
        let _ = window.set_activated(true);
    }
}

// 7. XWayland shell protocol handler
impl XWaylandShellHandler for LotusWaylandState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(
        &mut self,
        _xwm: XwmId,
        wl_surface: WlSurface,
        window: X11Surface,
    ) {
        debug!(
            "[lotus-nest] X11↔wl_surface associated (wl={})",
            wl_surface.id().protocol_id()
        );
        self.configure_x11_to_pane(&window);
    }
}
delegate_xwayland_shell!(LotusWaylandState);

// 8. X11 Window Manager handler
impl XwmHandler for LotusWaylandState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().unwrap()
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        debug!("[lotus-nest] X11 window created: {:?}", window.title());
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        debug!("[lotus-nest] X11 override-redirect window: {:?}", window.title());
    }

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let title = window.title();
        let class = window.class();
        let client_pid = window.get_client_pid().ok().or_else(|| window.pid());
        debug!(
            "[lotus-nest] mapping X11 window: {:?}, class: {:?}, pid: {:?}",
            title, class, client_pid
        );
        let _ = window.set_mapped(true);
        // wl_surface may not exist yet — surface_associated / map_window_notify retry.
        self.configure_x11_to_pane(&window);
    }

    fn map_window_notify(&mut self, _xwm: XwmId, window: X11Surface) {
        debug!(
            "[lotus-nest] X11 window mapped: {:?} (wl={})",
            window.title(),
            window
                .wl_surface()
                .map(|s| s.id().protocol_id())
                .unwrap_or(0)
        );
        self.configure_x11_to_pane(&window);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        debug!(
            "[lotus-nest] X11 override-redirect mapped: {:?}",
            window.title()
        );
        self.configure_x11_to_pane(&window);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(wl_surface) = window.wl_surface() {
            let surface_id = wl_surface.id().protocol_id();
            self.x11_surfaces.remove(&surface_id);
            let s2p = self.surface_to_pane.lock();
            if let Some(pane_id) = s2p.get(&surface_id) {
                let _ = self.event_sender.send(WaylandEvent::WindowClosed {
                    pane_id: pane_id.clone(),
                    surface_id,
                });
            }
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.unmapped_window(_xwm, window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        _w: Option<u32>,
        _h: Option<u32>,
        _reorder: Option<smithay::xwayland::xwm::Reorder>,
    ) {
        self.configure_x11_to_pane(&window);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: smithay::utils::Rectangle<i32, smithay::utils::Logical>,
        _above: Option<u32>,
    ) {}

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        debug!("[lotus-nest] X11 fullscreen_request: {:?}", window.title());
        let _ = window.set_fullscreen(true);
        self.configure_x11_to_pane(&window);
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_fullscreen(false);
        self.configure_x11_to_pane(&window);
    }

    fn maximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_maximized(true);
        self.configure_x11_to_pane(&window);
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_maximized(false);
        self.configure_x11_to_pane(&window);
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _edges: smithay::xwayland::xwm::ResizeEdge,
    ) {}

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}
}
