use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::path::PathBuf;

use once_cell::sync::OnceCell;
use log::{info, error, debug, trace, warn};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize as WinitPhysicalSize;
use dpi::PhysicalSize as ServoPhysicalSize;
use winit::event::{WindowEvent, MouseScrollDelta};
#[cfg(target_os = "linux")]
use winit::event::{DeviceEvent, DeviceId};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy, ControlFlow};
#[cfg(target_os = "linux")]
use winit::event_loop::DeviceEvents;
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
    /// Nested Wayland/X11 client cursor (shape or bitmap) → host winit cursor.
    #[cfg(target_os = "linux")]
    NestedCursor {
        window_id: String,
        cursor: crate::wayland::NestedCursor,
        /// True when the seat cursor came from the primary/game pane.
        from_primary: bool,
    },
    /// Primary game activated/released zwp_pointer_constraints lock/confine.
    #[cfg(target_os = "linux")]
    NestedPointerLock {
        window_id: String,
        active: bool,
    },
    SetMinInnerSize(String, Option<winit::dpi::PhysicalSize<u32>>), // window_id, size (None = remove constraint)
    SetMaxInnerSize(String, Option<winit::dpi::PhysicalSize<u32>>), // window_id, size (None = remove constraint)

    // Pane-specific commands
    CreatePane(String, String, String, euclid::Rect<f32, servo::DeviceIndependentPixel>, i32, PaneAnchor, u32, bool), // window_id, pane_id, url, rect, z_index, anchor, dock_order, visible
    /// Linux sandbox remote pane (SHM + supervisor). Not available on Windows.
    #[cfg(target_os = "linux")]
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
    /// Move dock quad without Servo/external configure — texture scales into the new rect.
    /// Final size/position commit still uses SetPaneRect.
    SetPaneRectVisual(String, String, euclid::Rect<f32, servo::DeviceIndependentPixel>),
    /// Update dock thickness without clearing the pane's anchor.
    /// Left/Right consume `width`; Top/Bottom consume `height`; Fill/None are no-ops.
    SetPaneSize(String, String, f32, f32), // window_id, pane_id, width, height
    SetPaneWeight(String, String, f32), // window_id, pane_id, weight
    SetSplitWeights(String, String, String), // window_id, split_id, JSON map|array
    SetSplitSizes(String, String, String), // window_id, split_id, JSON map|array
    BeginLayoutBatch(String), // window_id
    EndLayoutBatch(String), // window_id — flush one recalculate_layout
    SetPaneVisible(String, String, bool), // window_id, pane_id, visible
    SetPanePassthrough(String, String, bool), // window_id, pane_id, pointer_passthrough
    FocusPane(String, String), // window_id, pane_id
    #[cfg(target_os = "linux")]
    AttachRemotePaneInput(String, String, std::os::unix::io::RawFd), // window_id, pane_id, fd
    /// Keep a remote pane docked+visible (so it receives compositor vsync) while a host
    /// crash-chrome pane covers it as a high-z absolute overlay. Used during crash UI and
    /// respawn so we never gate first-frame production on revealing the remote early.
    #[cfg(target_os = "linux")]
    CoverRemoteWithCrashChrome {
        window_id: String,
        remote_pane_id: String,
        crash_pane_id: String,
    },
    /// After a successor engine is attached: invalidate stale GL/SHM cache and arm the
    /// gate that emits `remote-frame-composited` once a fresh frame is uploaded.
    #[cfg(target_os = "linux")]
    ArmCrashCoverFreshFrame {
        window_id: String,
        remote_pane_id: String,
    },
    /// Lift cover bookkeeping after JS hides crash chrome.
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
    CreateExternalAppPane {
        window_id: String,
        pane_id: String,
        command: String,
        args: Vec<String>,
        rect: euclid::Rect<f32, servo::DeviceIndependentPixel>,
        z_index: i32,
        anchor: PaneAnchor,
        dock_order: u32,
        visible: bool,
        /// `off` | `mirror` | `capture` — host-bus Notify → atrium toast bridge.
        dbus_notifications: String,
        /// App names / desktop-ids to match (case-insensitive), e.g. Discord.
        dbus_notification_apps: Vec<String>,
        /// Extra child env (merged into Command before spawn). Prefer this over
        /// `env KEY=VAL` argv so secrets never appear in /proc/<pid>/cmdline.
        env: std::collections::HashMap<String, String>,
        /// Overlay client buffer tracks dock rect (xdg configure on setRect).
        follow_dock: bool,
        client_width: Option<u32>,
        client_height: Option<u32>,
        client_upscale_sgsr: bool,
        resize_edges: u32,
        resize_anchor: u8,
        resize_min_w: f32,
        resize_min_h: f32,
        resize_max_w: Option<f32>,
        resize_max_h: Option<f32>,
    },
    #[cfg(target_os = "linux")]
    SetExternalClientSize {
        window_id: String,
        pane_id: String,
        width: u32,
        height: u32,
    },
    #[cfg(target_os = "linux")]
    SetGameMode(String, bool),
    /// Semantic gamepad action from [`crate::evdev_pad`] → nest IPC / mode toggle.
    #[cfg(target_os = "linux")]
    ControllerPadAction {
        window_id: String,
        action: String,
        dir: Option<String>,
    },
    /// Pad→key inject for the active external dock pane (discordo TUI, etc.).
    #[cfg(target_os = "linux")]
    ControllerPadKey {
        window_id: String,
        pane_id: String,
        keycode: u32,
        pressed: bool,
    },
    #[cfg(target_os = "linux")]
    SetOskActive(String, bool),
    #[cfg(target_os = "linux")]
    SetPadKeyForwardPane(String, Option<String>),
    /// Servo tab pane id for direct AtriumFocus inject (Nav/Confirm, no Node hop).
    #[cfg(target_os = "linux")]
    SetPadSpatialFocusPane(String, Option<String>),
    /// Apply host upscaler/sharpen config and ensure shaders (GL thread).
    #[cfg(target_os = "linux")]
    ApplyHostUpscale {
        window_id: String,
        config_json: String,
    },
    #[cfg(target_os = "linux")]
    RegisterHostHotkey {
        window_id: String,
        id: String,
        /// W3C KeyboardEvent.code (e.g. "F12", "KeyD").
        key: String,
        /// Bitmask: Ctrl=1 Alt=2 Shift=4 Super=8 (lock keys ignored at match).
        modifiers: u32,
    },
    #[cfg(target_os = "linux")]
    UnregisterHostHotkey {
        window_id: String,
        id: String,
    },
    #[cfg(target_os = "linux")]
    ClearHostHotkeys(String),
    /// Borderless fullscreen for the lotus host window itself.
    SetFullscreen(String, bool),
    SetPaneZIndex(String, String, i32),
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

    /// Apply pad input from the dedicated queue (not the winit EngineCommand FIFO).
    /// `allow_focus_step`: take at most one Nav/Confirm (once per about_to_wait /
    /// present). UserEvent drains edges only so RedrawWindow storms cannot burst Nav.
    /// `event_loop` is required so ToggleOverlay can re-wear game cursor shoes.
    #[cfg(target_os = "linux")]
    fn drain_pad_input(
        &mut self,
        allow_focus_step: bool,
        event_loop: Option<&ActiveEventLoop>,
    ) {
        let pending = crate::evdev_pad::drain_pending_pad(allow_focus_step);
        if pending.is_empty() {
            return;
        }
        let mut injected = false;
        for item in pending {
            match item {
                crate::evdev_pad::PendingPadInput::Action {
                    window_id,
                    action,
                    dir,
                } => {
                    if self.apply_controller_pad_action(
                        &window_id,
                        &action,
                        dir,
                        event_loop,
                    ) {
                        injected = true;
                    }
                }
                crate::evdev_pad::PendingPadInput::Key {
                    window_id,
                    pane_id,
                    keycode,
                    pressed,
                } => {
                    self.apply_controller_pad_key(&window_id, &pane_id, keycode, pressed);
                }
            }
        }
        // One spin for the whole batch so AtriumFocus scripts run without
        // per-event NewFrameReady amplification inside a user-event flood.
        if injected {
            if let Some(servo) = &self.servo {
                servo.spin_event_loop();
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn apply_controller_pad_key(
        &mut self,
        window_id: &str,
        pane_id: &str,
        keycode: u32,
        pressed: bool,
    ) {
        let Some(instance) = self.windows.get(window_id) else {
            return;
        };
        let Some(pane) = instance.panes.get(pane_id) else {
            return;
        };
        if let Some(ref ws) = pane.external_wayland_server {
            ws.lock()
                .send_keyboard_to_pane(pane_id, keycode, pressed);
        }
    }

    /// Returns true when a spatial AtriumFocus script was queued.
    #[cfg(target_os = "linux")]
    fn apply_controller_pad_action(
        &mut self,
        window_id: &str,
        action: &str,
        dir: Option<String>,
        event_loop: Option<&ActiveEventLoop>,
    ) -> bool {
        // Mode-local ToggleOverlay: flip Rust/evdev first, Node only mirrors state.
        // Must pass event_loop so nest_wear_game_cursor_shoe can restore custom cursors.
        if action == "ToggleOverlay" {
            let new_game_mode = if let Some(instance) = self.windows.get_mut(window_id) {
                let next = !instance.game_mode;
                if next && instance.nested_compositor.is_none() {
                    warn!(
                        "[lotus-nest] ToggleOverlay→game ignored: nested compositor not started"
                    );
                    return false;
                }
                nest_apply_game_mode(instance, next, event_loop);
                if next {
                    // Re-assert shoes after mode flip (cursor visual + grab sync).
                    nest_wear_game_pointer_shoes(instance, event_loop);
                }
                if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                    crate::evdev_pad::ensure_started(window_id.to_string(), proxy.clone());
                }
                // overlay = !game_mode. Closing must force EVIOCGRAB release so
                // pad→game resumes; opening grabs for shell Nav.
                crate::evdev_pad::set_overlay_mode(!next);
                if next {
                    crate::evdev_pad::set_key_forward_pane(None);
                }
                push_renderer_ipc(
                    window_id,
                    "nest:game-mode",
                    serde_json::json!({ "game_mode": next }),
                );
                instance.overlay_dirty = true;
                if window_is_viewable(instance) {
                    instance.window.request_redraw();
                }
                info!("[lotus-nest] ToggleOverlay → game_mode={}", next);
                next
            } else {
                return false;
            };
            push_host_ipc(
                &self.callback,
                window_id,
                "nest-controller-action",
                serde_json::json!({
                    "action": "ToggleOverlay",
                    "modeHandled": true,
                    "gameMode": new_game_mode,
                }),
            );
            return false;
        }

        #[cfg(target_os = "linux")]
        if let Some(instance) = self.windows.get_mut(window_id) {
            nest_note_input_device(instance, &self.callback, window_id, "controller");
        }

        let spatial = crate::evdev_pad::spatial_focus_pane();
        let handled_spatial = if matches!(action, "Nav" | "Confirm") {
            if let Some(ref pane_id) = spatial {
                if let Some(instance) = self.windows.get_mut(window_id) {
                    let pid = if pane_id == "main" && !instance.panes.contains_key("main") {
                        instance.primary_pane_id.clone()
                    } else {
                        pane_id.clone()
                    };
                    if let Some(pane) = instance.panes.get(&pid) {
                        let script = if action == "Nav" {
                            let d = dir.as_deref().unwrap_or("down");
                            format!(
                                "window.AtriumFocus&&window.AtriumFocus.move({});",
                                serde_json::to_string(d)
                                    .unwrap_or_else(|_| "\"down\"".into())
                            )
                        } else {
                            "window.AtriumFocus&&window.AtriumFocus.confirm();".to_string()
                        };
                        pane.webview.evaluate_javascript(&script, |_| {});
                        instance.overlay_dirty = true;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        if handled_spatial {
            let mut payload =
                serde_json::json!({ "action": action, "spatialHandled": true });
            if let Some(ref d) = dir {
                payload["dir"] = serde_json::Value::String(d.clone());
            }
            push_host_ipc(
                &self.callback,
                window_id,
                "nest-controller-action",
                payload,
            );
            true
        } else {
            let mut payload = serde_json::json!({ "action": action });
            if let Some(ref d) = dir {
                payload["dir"] = serde_json::Value::String(d.clone());
            }
            // Never Blocking-stall the GL thread on Node for pad actions.
            push_host_ipc(
                &self.callback,
                window_id,
                "nest-controller-action",
                payload,
            );
            false
        }
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

#[cfg(target_os = "linux")]
pub(crate) fn forward_remote_input(fd: std::os::unix::io::RawFd, json: &str) {
    let mut bytes = json.as_bytes().to_vec();
    bytes.push(b'\n');
    unsafe {
        let _ = libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn forward_remote_input(_fd: i32, _json: &str) {}

/// Push a Track-1 IPC batch to renderer WebSockets. Renderer `_processRaw` requires the
/// leading `MSG_TYPE_DATA` (0x02) byte — bare msgpack is silently dropped.
fn push_renderer_ipc(window_id: &str, channel: &str, payload: serde_json::Value) {
    let Some(senders) = WS_SENDERS.get() else {
        return;
    };
    let Ok(packed) = rmp_serde::encode::to_vec_named(&vec![(channel, payload)]) else {
        return;
    };
    let mut wrapped = Vec::with_capacity(packed.len() + 1);
    wrapped.push(MSG_TYPE_DATA);
    wrapped.extend(packed);
    let msg = axum::extract::ws::Message::Binary(wrapped.into());
    for entry in senders.iter() {
        if entry.key().starts_with(&format!("{}:", window_id)) || entry.key() == window_id {
            let _ = entry.value().send(msg.clone());
        }
    }
}

/// Push a Track-1 IPC batch straight to the Node host callback (no Servo WS hop).
/// Use for latency-sensitive atrium input (pad Nav / Tab / ToggleOverlay / hotkeys).
fn push_host_ipc(
    callback: &ThreadsafeFunction<(String, Vec<napi::bindgen_prelude::Buffer>), ErrorStrategy::Fatal>,
    window_id: &str,
    channel: &str,
    payload: serde_json::Value,
) {
    push_host_ipc_mode(
        callback,
        window_id,
        channel,
        payload,
        ThreadsafeFunctionCallMode::NonBlocking,
    );
}

fn push_host_ipc_mode(
    callback: &ThreadsafeFunction<(String, Vec<napi::bindgen_prelude::Buffer>), ErrorStrategy::Fatal>,
    window_id: &str,
    channel: &str,
    payload: serde_json::Value,
    mode: ThreadsafeFunctionCallMode,
) {
    let Ok(packed) = rmp_serde::encode::to_vec_named(&vec![(channel, payload)]) else {
        return;
    };
    let mut wrapped = Vec::with_capacity(packed.len() + 1);
    wrapped.push(MSG_TYPE_DATA);
    wrapped.extend(packed);
    // NonBlocking can drop under Node load (looks like controller-only lag spikes);
    // Blocking keeps pad Tab/ToggleOverlay ordered with the UI thread.
    let _ = callback.call((window_id.to_string(), vec![wrapped.into()]), mode);
}

#[cfg(target_os = "linux")]
/// Linux `BTN_*` codes for nested wl_pointer / Xwayland.
///
/// winit collapses BTN_SIDE|BTN_BACK → `Back` and BTN_EXTRA|BTN_FORWARD → `Forward`.
/// Games expect mouse4/mouse5 as SIDE/EXTRA (0x113/0x114). Mapping Back→FORWARD and
/// Forward→BACK (the old values) dropped Corsair side-button melee binds.
fn winit_mouse_button_evdev(button: winit::event::MouseButton) -> u32 {
    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;
    const BTN_MIDDLE: u32 = 0x112;
    const BTN_SIDE: u32 = 0x113;
    const BTN_EXTRA: u32 = 0x114;
    match button {
        winit::event::MouseButton::Left => BTN_LEFT,
        winit::event::MouseButton::Right => BTN_RIGHT,
        winit::event::MouseButton::Middle => BTN_MIDDLE,
        winit::event::MouseButton::Back => BTN_SIDE,
        winit::event::MouseButton::Forward => BTN_EXTRA,
        // Wayland `Other` is already an evdev code; X11 `Other` is a button number.
        winit::event::MouseButton::Other(v) if (v as u32) >= BTN_LEFT => v as u32,
        winit::event::MouseButton::Other(8) => BTN_SIDE,
        winit::event::MouseButton::Other(9) => BTN_EXTRA,
        winit::event::MouseButton::Other(v) => BTN_LEFT.saturating_add(v.saturating_sub(1) as u32),
    }
}

/// Kill nested Proton/Discord children, then tear down the nested compositor.
#[cfg(target_os = "linux")]
fn nest_teardown_window(instance: &mut WindowInstance) {
    // Children first while the nested display is still alive.
    for pane in instance.panes.values_mut() {
        if let Some(pid) = pane.external_child_pid.take() {
            crate::window::terminate_external_child(pid);
        }
        pane.external_wayland_server = None;
        pane.external_surface_slot = None;
    }
    if instance.nested_compositor.take().is_some() {
        info!("[lotus-nest] nested compositor dropped");
    }
}

fn nest_set_pane_visible(instance: &mut WindowInstance, pane_id: &str, visible: bool) {
    instance.layout_root.set_leaf_visible(pane_id, visible);
    #[cfg(target_os = "linux")]
    crate::dbus_notify::set_source_pane_visible(pane_id, visible);
    if let Some(pane) = instance.panes.get_mut(pane_id) {
        pane.pending_visible = visible;
        pane.is_dirty = true;
        pane.frame_seq = pane.frame_seq.wrapping_add(1);

        // Keep Servo painting while the host just stops compositing this pane.
        // webview.hide()/show() throttles the document and clears rAF; unthrottle
        // does not re-schedule callbacks, so reveal stayed blank until a resize
        // forced a Webrender frame. Overlay toggles need the paint loop left alone.
        //
        // Also exit compositor freeze: visibility dirty used to set is_resizing
        // without a size change, which locked LocalFreeze on a ghost forever.
        pane.ghost_valid = false;
        pane.is_resizing = false;

        #[cfg(target_os = "linux")]
        if pane.external_wayland_server.is_some() {
            // Nested panes have no Servo resize transaction — commit visibility now so
            // composite + hit-testing match pending_visible without a window resize.
            // Client buffer size stays fixed (game output / overlay client size);
            // dock geometry is composite-only (cover/contain scale).
            pane.is_visible = visible;
        }
    }
    if let Some(active) = instance.active_layout.panes.get_mut(pane_id) {
        active.visible = visible;
    }
    if let Some(staged) = instance.staged_layout.panes.get_mut(pane_id) {
        staged.visible = visible;
    }
    #[cfg(target_os = "linux")]
    if let Some(ref ws) = instance.nested_compositor {
        ws.lock().send_pane_visible(pane_id, visible);
    }
    instance.force_scene_repaint = true;
    instance.window.request_redraw();
}

/// Physical rect for overlay resize grips: staged layout + external contain-fit.
///
/// No CSD shadow inset — Ptyxis launches `--fullscreen` (flush content edge). The
/// old ~12 CSS-px inset stacked on top of that and pushed left/bottom grips into
/// the terminal. Drag math already uses `pane.rect`; hit-test must match the same
/// visible edge.
fn overlay_resize_pane_rect(
    instance: &WindowInstance,
    pane: &PaneInstance,
    scale_factor: f32,
) -> (f32, f32, f32, f32) {
    let (px, py, pw, ph) = nest_pane_hit_rect(instance, pane, scale_factor);
    #[cfg(target_os = "linux")]
    if let Some(ref slot) = pane.external_surface_slot {
        let guard = slot.lock();
        if guard.width > 0 && guard.height > 0 {
            return crate::overlay_resize::contain_fit_content_rect(
                px,
                py,
                pw,
                ph,
                guard.width,
                guard.height,
            );
        }
    }
    let _ = scale_factor;
    (px, py, pw, ph)
}

/// Physical rect for pointer hit-testing; matches staged compositor placement in atrium.
fn nest_pane_hit_rect(
    instance: &WindowInstance,
    pane: &PaneInstance,
    scale_factor: f32,
) -> (f32, f32, f32, f32) {
    #[cfg(target_os = "linux")]
    if instance.nested_compositor.is_some() {
        if let Some(staged) = instance.staged_layout.panes.get(&pane.id) {
            if staged.rect.size.width > 0 && staged.rect.size.height > 0 {
                let r = staged.rect;
                return (
                    r.origin.x as f32,
                    r.origin.y as f32,
                    r.size.width as f32,
                    r.size.height as f32,
                );
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = instance;
    pane.input_physical_rect(scale_factor)
}

/// Host overlay edge resize: cursor feedback + drag reflow (no visible chrome).
fn overlay_resize_handle_cursor(
    instance: &mut WindowInstance,
    point: Point2D<f32, servo::DevicePixel>,
    scale_factor: f32,
) -> bool {
    use crate::overlay_resize::{
        compute_resized_rect, cursor_for_edge, hit_test_overlay_resize,
    };

    if let Some(ref drag) = instance.active_overlay_resize.clone() {
        let Some(pane) = instance.panes.get(&drag.pane_id) else {
            instance.active_overlay_resize = None;
            return true;
        };
        let Some(policy) = pane.overlay_resize.clone() else {
            instance.active_overlay_resize = None;
            return true;
        };
        let window = instance.window.inner_size();
        let window_w = window.width as f32 / scale_factor;
        let window_h = window.height as f32 / scale_factor;
        let cursor_x = point.x / scale_factor;
        let cursor_y = point.y / scale_factor;
        let next = compute_resized_rect(&policy, drag, cursor_x, cursor_y, window_w, window_h);
        let edge = drag.edge;
        let pane_id = drag.pane_id.clone();
        instance.apply_floating_pane_rect(&pane_id, next, scale_factor);
        // Host owns the cursor for the drag — don't let a stale nested flag block restore.
        #[cfg(target_os = "linux")]
        {
            instance.nested_cursor_applied = false;
        }
        instance.window.set_cursor(cursor_for_edge(edge));
        return true;
    }

    if !instance.is_mouse_down {
        if let Some((pid, edge)) = hit_test_overlay_resize(
            instance.panes.values(),
            point,
            scale_factor,
            pane_accepts_pointer_input,
            |p| overlay_resize_pane_rect(instance, p, scale_factor),
        ) {
            let entering = instance.overlay_resize_hover.as_ref().map(|(p, e)| (p.as_str(), *e))
                != Some((pid.as_str(), edge));
            instance.overlay_resize_hover = Some((pid.clone(), edge));
            // Host ColResize/RowResize must beat any nested client arrow.
            #[cfg(target_os = "linux")]
            {
                instance.nested_cursor_applied = false;
                if entering {
                    // Same flash class as overlay-open: wallpaper reseat leaves the
                    // nest seat on primary, so the game soft-cursor keeps painting
                    // inside the upscaled Fill (full-host tall) while we wear the
                    // grip shoe. Clear primary, then seat the overlay pane under
                    // the grip so leave→content never re-enters the game for a frame.
                    if let Some(ref ws) = instance.nested_compositor {
                        ws.lock().clear_pointer_focus();
                    }
                    if let Some(pane) = instance.panes.get(&pid) {
                        if let Some(ref ws) = pane.external_wayland_server.clone() {
                            let (tx, ty) =
                                nest_pointer_coords(instance, pane, point, scale_factor);
                            ws.lock().send_pointer_motion_to_pane(&pid, tx, ty);
                            instance.hover_pane_id = Some(pid.clone());
                            instance.last_overlay_pointer = Some((pid.clone(), tx, ty));
                        }
                    }
                }
            }
            instance.window.set_cursor(cursor_for_edge(edge));
            return true;
        }
        if instance.overlay_resize_hover.take().is_some() {
            // Leave grip → same shoe swap as nest_apply_game_mode(overlay).
            #[cfg(target_os = "linux")]
            {
                nest_clear_nested_cursor(instance);
                instance.nested_cursor = None;
                instance.nested_cursor_applied = false;
            }
            instance.window.set_cursor_visible(true);
            instance.window.set_cursor(winit::window::CursorIcon::Default);
        }
    }
    false
}

fn overlay_resize_handle_button(
    instance: &mut WindowInstance,
    point: Point2D<f32, servo::DevicePixel>,
    scale_factor: f32,
    is_pressed: bool,
    button: winit::event::MouseButton,
) -> bool {
    use crate::overlay_resize::{hit_test_overlay_resize, ActiveOverlayResizeDrag};

    if button != winit::event::MouseButton::Left {
        return false;
    }
    if !is_pressed {
        if instance.active_overlay_resize.take().is_some() {
            return true;
        }
        return false;
    }
    let Some((pid, edge)) = hit_test_overlay_resize(
        instance.panes.values(),
        point,
        scale_factor,
        pane_accepts_pointer_input,
        |p| overlay_resize_pane_rect(instance, p, scale_factor),
    ) else {
        return false;
    };
    let Some(start_rect) = instance.panes.get(&pid).map(|p| p.rect) else {
        return false;
    };
    instance.active_overlay_resize = Some(ActiveOverlayResizeDrag {
        pane_id: pid,
        edge,
        start_rect,
    });
    instance.last_mouse_down_pane_id = None;
    true
}

/// Topmost pane under the cursor (z-order), same as standard lotus overlay routing.
///
/// When `exclude_primary_game` is set (overlay input mode), the base game surface is
/// wallpaper-only: it never wins hit-test, so hover/click cannot steal focus back to it.
#[cfg(target_os = "linux")]
fn hit_test_pointer_pane<'a>(
    instance: &'a WindowInstance,
    point: Point2D<f32, servo::DevicePixel>,
    scale_factor: f32,
    exclude_primary_game: bool,
) -> Option<&'a PaneInstance> {
    let mut sorted: Vec<_> = instance.panes.values().collect();
    sorted.sort_by_key(|p| -p.z_index);
    for pane in sorted {
        if exclude_primary_game
            && instance.primary_game_pane.as_deref() == Some(pane.id.as_str())
        {
            continue;
        }
        if !pane_accepts_pointer_input(pane) {
            continue;
        }
        let (px, py, pw, ph) = nest_pane_hit_rect(instance, pane, scale_factor);
        let rect = euclid::Rect::new(
            euclid::Point2D::new(px, py),
            euclid::Size2D::new(pw, ph),
        );
        if rect.contains(point) {
            return Some(pane);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn nest_is_primary_game(instance: &WindowInstance, pane_id: &str) -> bool {
    instance.primary_game_pane.as_deref() == Some(pane_id)
}

/// Sticky keyboard/click focus for overlay surfaces. Does not follow the mouse.
#[cfg(target_os = "linux")]
fn nest_set_keyboard_focus(instance: &mut WindowInstance, pane_id: &str) {
    instance.active_pane_id = pane_id.to_string();
    if let Some(pane) = instance.panes.get(pane_id) {
        if let Some(ref ws) = pane.external_wayland_server {
            ws.lock().focus_pane(pane_id);
        }
    }
}

/// Notify atrium chrome when the active input device class changes (footer hints /
/// hide controller focus ring). Only emits on transition to avoid IPC spam.
#[cfg(target_os = "linux")]
fn nest_note_input_device(
    instance: &mut WindowInstance,
    callback: &ThreadsafeFunction<(String, Vec<napi::bindgen_prelude::Buffer>), ErrorStrategy::Fatal>,
    window_id: &str,
    device: &str,
) {
    if instance.last_input_device.as_deref() == Some(device) {
        return;
    }
    instance.last_input_device = Some(device.to_string());
    push_host_ipc(
        callback,
        window_id,
        "nest-input-device",
        serde_json::json!({ "device": device }),
    );
}

/// Enter/leave game-only *host* input. Two independent shoe sets:
/// - Game nest seat stays live (pointer focus + constraints + cursor) so OW/menu
///   state keeps updating `game_*` shoes while chrome is open.
/// - Host window wears either game shoes or overlay shoes; swap only the host.
/// Overlay plugins (Discord) are the ones that lose xdg Activated in game mode
/// so desktop Notify still fires — the game can keep thinking it is focused.
#[cfg(target_os = "linux")]
fn nest_apply_game_mode(
    instance: &mut WindowInstance,
    mode: bool,
    event_loop: Option<&ActiveEventLoop>,
) {
    let leaving_game = instance.game_mode && !mode;
    let entering_game = !instance.game_mode && mode;
    instance.game_mode = mode;
    if entering_game {
        // Fresh enter — no stale host-consumed press set.
        instance.host_hotkey_pressed.clear();
    }
    if let Some(ref ws) = instance.nested_compositor {
        // Nested compositor tracks host game-mode so new Discord toplevels never
        // grab Activated (which would silence session-bus Notify).
        ws.lock().set_host_game_mode(mode);
    }
    // Leaving game: drop nest keyboard so overlay/Servo can type. Pointer seat
    // on the primary stays put — tearing it down desynced OW cursor/lock shoes.
    if leaving_game {
        if let Some(ref ws) = instance.nested_compositor {
            ws.lock().clear_keyboard_focus();
        }
        instance.host_hotkey_pressed.clear();
    }
    if mode {
        // Overlay externals (Discord, …) must look unfocused so they emit desktop
        // Notify calls — we mirror those into atrium toasts via the dbus bridge.
        let game_id = instance.primary_game_pane.clone();
        let overlay_ids: Vec<String> = instance
            .panes
            .iter()
            .filter(|(id, p)| {
                p.external_wayland_server.is_some()
                    && game_id.as_deref() != Some(id.as_str())
            })
            .map(|(id, _)| id.clone())
            .collect();
        if let Some(ref ws) = instance.nested_compositor {
            let server = ws.lock();
            for id in &overlay_ids {
                server.set_pane_activated(id, false);
            }
            // Keyboard off overlays before re-seating the game.
            server.clear_keyboard_focus();
        }
        if let Some(ref gid) = game_id {
            if instance.active_pane_id != *gid
                && overlay_ids.iter().any(|id| id == &instance.active_pane_id)
            {
                instance.active_pane_id.clear();
            }
            nest_set_keyboard_focus(instance, gid);
            if let Some(ref ws) = instance.nested_compositor {
                // Affirm primary pointer seat (may already be there from overlay).
                ws.lock().focus_pane(gid);
            }
        }
        instance.hover_pane_id = None;
        // Overlay visual cache must not leak back onto the game host shoes.
        instance.nested_cursor = None;
        for id in &overlay_ids {
            crate::dbus_notify::set_source_pane_visible(id, false);
        }
        // Wear the live game shoes on the host (nest seat never went cold).
        nest_wear_game_pointer_shoes(instance, event_loop);
    } else {
        let game_id = instance.primary_game_pane.clone();
        let overlay_ids: Vec<String> = instance
            .panes
            .iter()
            .filter(|(id, p)| {
                p.external_wayland_server.is_some()
                    && game_id.as_deref() != Some(id.as_str())
            })
            .map(|(id, _)| id.clone())
            .collect();
        if nest_is_primary_game(instance, &instance.active_pane_id) {
            instance.active_pane_id.clear();
        }
        // Activate *visible* overlays so Chromium resumes painting. Do NOT clear
        // primary pointer focus or deactivate game constraints — game shoes stay
        // authoritative on the nest seat while host wears overlay shoes.
        if let Some(ref ws) = instance.nested_compositor {
            let server = ws.lock();
            for id in &overlay_ids {
                let visible = instance
                    .panes
                    .get(id)
                    .map(|p| p.is_visible || p.pending_visible)
                    .unwrap_or(false);
                server.set_pane_activated(id, visible);
            }
        }
        // Host overlay shoes only — nest game seat untouched.
        nest_release_host_pointer_grab(instance);
        nest_clear_nested_cursor(instance);
        instance.nested_cursor = None;
        instance.window.set_cursor_visible(true);
        instance.window.set_cursor(CursorIcon::Default);
        instance.nested_cursor_applied = false;
        instance.force_scene_repaint = true;
        instance.window.request_redraw();
        // Route host cursor onto overlay chrome / Discord. Primary nest seat is
        // restored after leaving a Discord hit so the game stays "focused".
        if let Some(el) = event_loop {
            let scale = instance.window.scale_factor() as f32;
            let point = instance.last_mouse_pos;
            let wid = instance.window_id.clone();
            nest_route_cursor_moved(instance, el, &wid, point, scale);
            let overlay_seated = instance.hover_pane_id.as_ref().is_some_and(|id| {
                instance.primary_game_pane.as_deref() != Some(id.as_str())
                    && instance
                        .panes
                        .get(id)
                        .is_some_and(|p| p.external_wayland_server.is_some())
            });
            if !overlay_seated {
                if let Some((pane_id, x, y)) = instance.last_overlay_pointer.clone() {
                    let still_overlay = instance
                        .panes
                        .get(&pane_id)
                        .is_some_and(|p| {
                            p.external_wayland_server.is_some()
                                && (p.is_visible || p.pending_visible)
                        });
                    if still_overlay {
                        if let Some(ref ws) = instance.nested_compositor {
                            ws.lock().send_pointer_motion_to_pane(&pane_id, x, y);
                        }
                        instance.hover_pane_id = Some(pane_id.clone());
                        nest_set_keyboard_focus(instance, &pane_id);
                        nest_sync_nested_cursor_for_hover(instance, Some(el), true);
                    } else if let Some(ref ws) = instance.nested_compositor {
                        ws.lock().reseat_primary_pointer_focus();
                    }
                } else if let Some(ref ws) = instance.nested_compositor {
                    ws.lock().reseat_primary_pointer_focus();
                }
            } else if let Some(ref id) = instance.hover_pane_id.clone() {
                nest_set_keyboard_focus(instance, id);
            }
        }
    }
}

/// Host Locked/Confined grab for FPS relative-only aim.
///
/// Driven solely by a real `zwp_pointer_constraints` lock/confine shoe — **not**
/// by a hidden nest cursor. Unity/Wine soft-cursor UIs (RoR2 menus, etc.) hide
/// the OS cursor and still need absolute `wl_pointer.motion`; treating hide as
/// lock starved absolute motion and only teleported on click.
#[cfg(target_os = "linux")]
fn nest_want_host_pointer_grab(instance: &WindowInstance) -> bool {
    instance.game_mode
        && instance.nested_compositor.is_some()
        && instance.game_pointer_constraint_active
}

#[cfg(target_os = "linux")]
fn nest_release_host_pointer_grab(instance: &mut WindowInstance) {
    use winit::window::CursorGrabMode;
    if instance.host_pointer_grabbed {
        let _ = instance.window.set_cursor_grab(CursorGrabMode::None);
        instance.host_pointer_grabbed = false;
        info!("[lotus-nest] host pointer grab off (overlay / unlocked)");
    }
}

/// Match host CursorGrab to game shoes while in game mode.
#[cfg(target_os = "linux")]
fn nest_sync_pointer_grab(instance: &mut WindowInstance) {
    use winit::window::CursorGrabMode;
    let want = nest_want_host_pointer_grab(instance);
    if want == instance.host_pointer_grabbed {
        if want {
            instance.window.set_cursor_visible(false);
        }
        return;
    }
    if want {
        let grabbed = instance
            .window
            .set_cursor_grab(CursorGrabMode::Locked)
            .or_else(|_| instance.window.set_cursor_grab(CursorGrabMode::Confined));
        match grabbed {
            Ok(()) => {
                instance.window.set_cursor_visible(false);
                instance.host_pointer_grabbed = true;
                info!(
                    "[lotus-nest] host pointer grab on (constraint={} cursor_hidden={})",
                    instance.game_pointer_constraint_active, instance.game_cursor_hidden
                );
            }
            Err(err) => {
                warn!(
                    "[lotus-nest] host pointer grab failed (relative aim may be soft): {:?}",
                    err
                );
                instance.host_pointer_grabbed = false;
            }
        }
    } else {
        nest_release_host_pointer_grab(instance);
    }
}

/// Record primary-game cursor into sticky shoes (never touches overlay Discord state).
///
/// Cursor visual and pointer-constraint shoes are independent. Overwatch's flow is
/// documented (Xwayland: confine + hidden cursor → relative lock): custom → lock
/// (load) → custom buffer again (hero select) → lock + hide (match). Clearing the
/// lock shoe on a visible custom cursor desynced host grab from nest: match then
/// hid without a fresh NestedPointerLock, leaving hidden+unlocked (no aim).
#[cfg(target_os = "linux")]
fn nest_record_game_cursor(instance: &mut WindowInstance, cursor: &crate::wayland::NestedCursor) {
    instance.game_cursor = Some(cursor.clone());
    instance.game_cursor_hidden = matches!(cursor, crate::wayland::NestedCursor::Hidden);
}

/// Apply only the game cursor visual (no host grab). Used on F12 enter before
/// nest seat re-enter completes.
#[cfg(target_os = "linux")]
fn nest_wear_game_cursor_shoe(
    instance: &mut WindowInstance,
    event_loop: Option<&ActiveEventLoop>,
) {
    if instance.game_cursor_hidden {
        instance.window.set_cursor_visible(false);
        instance.nested_cursor_applied = true;
        return;
    }
    if let (Some(el), Some(cursor)) = (event_loop, instance.game_cursor.clone()) {
        nest_paint_host_cursor(instance, el, &cursor);
    } else {
        nest_clear_nested_cursor(instance);
    }
}

/// Apply the game's sticky shoes to the host (grab + cursor visual).
///
/// Hide-without-lock: suppress the host cursor chrome but keep absolute
/// CursorMoved flowing (soft-cursor games). Lock shoe: grab + relative-only.
///
/// Xwayland locks relative motion when confined *and* the cursor is hidden —
/// always hide the host cursor while the constraint shoe is on (OW match /
/// load). Unlocked + custom paints the hero-select / menu cursor.
#[cfg(target_os = "linux")]
fn nest_wear_game_pointer_shoes(
    instance: &mut WindowInstance,
    event_loop: Option<&ActiveEventLoop>,
) {
    nest_sync_pointer_grab(instance);
    if instance.host_pointer_grabbed || instance.game_pointer_constraint_active {
        // Constraint shoe owns visibility: do not paint a stale Custom over a
        // lock (that undoes Xwayland's hide→relative upgrade).
        instance.window.set_cursor_visible(false);
        instance.nested_cursor_applied = true;
        return;
    }
    nest_wear_game_cursor_shoe(instance, event_loop);
}

#[cfg(target_os = "linux")]
fn nest_pointer_coords(
    instance: &WindowInstance,
    pane: &PaneInstance,
    point: Point2D<f32, servo::DevicePixel>,
    scale_factor: f32,
) -> (f64, f64) {
    let (px, py, pw, ph) = nest_pane_hit_rect(instance, pane, scale_factor);
    let lx = (point.x - px) as f64;
    let ly = (point.y - py) as f64;
    // Map host pane pixels → client buffer using the same contain-fit as composite.
    if let Some(ref slot) = pane.external_surface_slot {
        let guard = slot.lock();
        let bw = guard.width.max(1) as f64;
        let bh = guard.height.max(1) as f64;
        if pw > 1.0 && ph > 1.0 {
            let pane_ar = pw as f64 / ph as f64;
            let buf_ar = bw / bh;
            if (pane_ar - buf_ar).abs() < 0.02 {
                return (lx * bw / pw as f64, ly * bh / ph as f64);
            }
            if buf_ar > pane_ar {
                let content_h = pw as f64 / buf_ar;
                let y0 = (ph as f64 - content_h) * 0.5;
                return (lx * bw / pw as f64, (ly - y0) * bh / content_h);
            }
            let content_w = ph as f64 * buf_ar;
            let x0 = (pw as f64 - content_w) * 0.5;
            return ((lx - x0) * bw / content_w, ly * bh / ph as f64);
        }
    }
    (lx, ly)
}

#[cfg(target_os = "linux")]
fn nest_in_overlay_input_mode(instance: &WindowInstance) -> bool {
    instance.nested_compositor.is_some() && !instance.game_mode
}

/// Nested atrium/gamescope host: outer window chrome resize is wrong (and fights
/// Discord/Chromium CSD). Use host frameless edge resize only for plain lotus windows.
fn nest_owns_host_window(instance: &WindowInstance) -> bool {
    #[cfg(target_os = "linux")]
    {
        instance.nested_compositor.is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = instance;
        false
    }
}

/// Nested-client resize shapes must not paint onto the host.
/// Atrium owns overlay/gutter grips via `set_cursor(cursor_for_edge)` directly;
/// GTK/Chromium CSD (incl. ColResize from `sb_h_double_arrow`) would otherwise
/// stick after leaving a host grip when `nest_sync` re-applies a stale bag.
fn nest_is_window_chrome_resize_cursor(icon: CursorIcon) -> bool {
    matches!(
        icon,
        CursorIcon::NwResize
            | CursorIcon::NeResize
            | CursorIcon::SwResize
            | CursorIcon::SeResize
            | CursorIcon::NResize
            | CursorIcon::SResize
            | CursorIcon::EResize
            | CursorIcon::WResize
            | CursorIcon::ColResize
            | CursorIcon::RowResize
            | CursorIcon::NeswResize
            | CursorIcon::NwseResize
    )
}

#[cfg(target_os = "linux")]
fn nest_named_cursor_icon(name: &str) -> CursorIcon {
    let icon = name.parse::<CursorIcon>().unwrap_or_else(|_| match name {
        "dnd-ask" => CursorIcon::Copy,
        "all-resize" => CursorIcon::Move,
        // Xcursor legacy aliases commonly used by Wine/Proton themes.
        "left_ptr" | "arrow" | "top_left_arrow" => CursorIcon::Default,
        "hand2" | "hand1" | "hand" | "pointing_hand" => CursorIcon::Pointer,
        "xterm" | "ibeam" => CursorIcon::Text,
        "watch" => CursorIcon::Wait,
        "left_ptr_watch" | "half-busy" => CursorIcon::Progress,
        "sb_h_double_arrow" | "h_double_arrow" => CursorIcon::ColResize,
        "sb_v_double_arrow" | "v_double_arrow" => CursorIcon::RowResize,
        "fleur" | "size_all" => CursorIcon::Move,
        "pirate" | "crossed_circle" => CursorIcon::NotAllowed,
        // Chromium CSD grip names that may not parse as CursorIcon.
        "nw-resize" | "ne-resize" | "sw-resize" | "se-resize" | "n-resize" | "s-resize"
        | "e-resize" | "w-resize" | "nwse-resize" | "nesw-resize"
        | "top_left_corner" | "top_right_corner" | "bottom_left_corner" | "bottom_right_corner"
        | "left_side" | "right_side" | "top_side" | "bottom_side" => CursorIcon::Default,
        _ => CursorIcon::Default,
    });
    if nest_is_window_chrome_resize_cursor(icon) {
        CursorIcon::Default
    } else {
        icon
    }
}

/// Paint a nested cursor onto the host window (visual only — no game-shoe updates).
#[cfg(target_os = "linux")]
fn nest_paint_host_cursor(
    instance: &mut WindowInstance,
    event_loop: &ActiveEventLoop,
    cursor: &crate::wayland::NestedCursor,
) {
    use winit::window::{Cursor, CustomCursor};
    instance.nested_cursor = Some(cursor.clone());
    match cursor {
        crate::wayland::NestedCursor::Hidden => {
            instance.window.set_cursor_visible(false);
            instance.nested_cursor_applied = true;
        }
        crate::wayland::NestedCursor::Named(name) => {
            instance.window.set_cursor_visible(true);
            instance.window.set_cursor(nest_named_cursor_icon(name));
            instance.nested_cursor_applied = true;
        }
        crate::wayland::NestedCursor::Custom {
            rgba,
            width,
            height,
            hotspot_x,
            hotspot_y,
        } => {
            if *width == 0 || *height == 0 {
                instance.window.set_cursor_visible(true);
                instance.window.set_cursor(CursorIcon::Default);
                instance.nested_cursor_applied = true;
                return;
            }
            match CustomCursor::from_rgba(
                rgba.clone(),
                *width,
                *height,
                *hotspot_x,
                *hotspot_y,
            ) {
                Ok(source) => {
                    let custom = event_loop.create_custom_cursor(source);
                    instance.window.set_cursor_visible(true);
                    instance.window.set_cursor(Cursor::Custom(custom));
                    instance.nested_cursor_applied = true;
                }
                Err(err) => {
                    warn!("[lotus-nest] nested custom cursor rejected: {:?}", err);
                    instance.window.set_cursor_visible(true);
                    instance.window.set_cursor(CursorIcon::Default);
                    instance.nested_cursor_applied = true;
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn nest_clear_nested_cursor(instance: &mut WindowInstance) {
    if instance.nested_cursor_applied {
        instance.window.set_cursor_visible(true);
        instance.window.set_cursor(CursorIcon::Default);
        instance.nested_cursor_applied = false;
    }
}

/// Re-apply the last nested host visual when the pointer is over an external pane.
#[cfg(target_os = "linux")]
fn nest_sync_nested_cursor_for_hover(
    instance: &mut WindowInstance,
    event_loop: Option<&ActiveEventLoop>,
    over_external: bool,
) {
    if over_external {
        if instance.nested_cursor_applied {
            return;
        }
        if let (Some(el), Some(cursor)) = (event_loop, instance.nested_cursor.clone()) {
            nest_paint_host_cursor(instance, el, &cursor);
        }
    } else {
        nest_clear_nested_cursor(instance);
    }
}

/// Route cursor movement in atrium HUD/overlay mode (z-ordered hit test).
/// Updates hover + pointer only — never steals sticky keyboard focus (`active_pane_id`).
/// Never feeds the primary game: overlay owns the seat until game mode returns.
#[cfg(target_os = "linux")]
fn nest_route_cursor_moved(
    instance: &mut WindowInstance,
    event_loop: &ActiveEventLoop,
    uuid: &str,
    point: Point2D<f32, servo::DevicePixel>,
    scale_factor: f32,
) {
    let prev_hover = instance.hover_pane_id.clone();
    let prev_was_external = prev_hover.as_ref().is_some_and(|id| {
        instance
            .panes
            .get(id)
            .map(|p| p.external_wayland_server.is_some())
            .unwrap_or(false)
    });
    let Some(hover_id) =
        hit_test_pointer_pane(instance, point, scale_factor, true).map(|p| p.id.clone())
    else {
        instance.hover_pane_id = None;
        nest_clear_nested_cursor(instance);
        // Over empty/game wallpaper — leave Discord, reseat primary so the game
        // nest seat stays live (independent game shoes keep syncing).
        if prev_was_external {
            if let Some(ref ws) = instance.nested_compositor {
                ws.lock().reseat_primary_pointer_focus();
            }
        }
        return;
    };
    let entered = prev_hover.as_deref() != Some(hover_id.as_str());
    instance.hover_pane_id = Some(hover_id.clone());
    let hovering_primary = instance.primary_game_pane.as_deref() == Some(hover_id.as_str());
    let external_motion = instance.panes.get(&hover_id).and_then(|pane| {
        if pane.external_wayland_server.is_none() {
            return None;
        }
        let ws = pane.external_wayland_server.clone()?;
        let (tx, ty) = nest_pointer_coords(instance, pane, point, scale_factor);
        Some((ws, tx, ty))
    });
    if let Some((ws, tx, ty)) = external_motion {
        // Overlay must never feed host motion into the primary game — nest seat
        // stays on primary unless Discord (or another overlay external) needs it.
        if hovering_primary {
            instance.hover_pane_id = None;
            nest_clear_nested_cursor(instance);
            if let Some(ref nest) = instance.nested_compositor {
                nest.lock().reseat_primary_pointer_focus();
            }
            return;
        }
        // Seat WL keyboard when the pointer enters an external TUI (Ghostty) so
        // typing works without an extra click after dock focus / F9.
        if entered {
            nest_set_keyboard_focus(instance, &hover_id);
        }
        ws.lock().send_pointer_motion_to_pane(&hover_id, tx, ty);
        instance.last_overlay_pointer = Some((hover_id.clone(), tx, ty));
        nest_sync_nested_cursor_for_hover(instance, Some(event_loop), true);
        return;
    }
    // Left nested overlay surface → Servo/HUD owns the host cursor. Reseat the
    // game nest pointer so OW cursor/lock shoes keep updating under chrome.
    nest_clear_nested_cursor(instance);
    if prev_was_external {
        if let Some(ref ws) = instance.nested_compositor {
            ws.lock().reseat_primary_pointer_focus();
        }
    }
    let (px, py, pw, ph, is_external, wayland, translated) =
        if let Some(pane) = instance.panes.get(&hover_id) {
            let (px, py, pw, ph) = nest_pane_hit_rect(instance, pane, scale_factor);
            let mut translated = Point2D::new(point.x - px, point.y - py);
            let is_external = pane.external_wayland_server.is_some();
            let wayland = pane.external_wayland_server.clone();
            if pane.is_remote {
                if let Some(ref consumer) = pane.remote_consumer {
                    if consumer.tex_width > 0 && consumer.tex_height > 0 && pw > 0.0 && ph > 0.0 {
                        translated.x *= consumer.tex_width as f32 / pw;
                        translated.y *= consumer.tex_height as f32 / ph;
                    }
                }
            }
            (px, py, pw, ph, is_external, wayland, translated)
        } else {
            return;
        };
    let _ = (px, py, pw, ph, is_external, wayland);
    if let Some(pane) = instance.panes.get(&hover_id) {
        if pane.is_remote {
            if let Some(fd) = pane.remote_input_fd {
                let surface_id = format!("{}:{}", uuid, pane.id);
                let msg = serde_json::json!({
                    "type": "Input",
                    "event": {
                        "kind": "MouseMove",
                        "surface_id": surface_id,
                        "generation": 1,
                        "x": translated.x,
                        "y": translated.y,
                        "modifiers": instance.modifiers.bits(),
                    }
                })
                .to_string();
                forward_remote_input(fd, &msg);
            }
        } else {
            pane.webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
                servo::WebViewPoint::Device(translated),
            )));
        }
    }
}

#[cfg(target_os = "linux")]
fn nest_resolve_button_target(
    instance: &WindowInstance,
    is_pressed: bool,
    scale_factor: f32,
) -> Option<String> {
    if is_pressed {
        hit_test_pointer_pane(instance, instance.last_mouse_pos, scale_factor, true)
            .map(|p| p.id.clone())
    } else {
        instance.last_mouse_down_pane_id.clone()
    }
}

#[cfg(target_os = "linux")]
fn nest_route_mouse_button(
    instance: &mut WindowInstance,
    uuid: &str,
    button: winit::event::MouseButton,
    is_pressed: bool,
    scale_factor: f32,
) {
    let Some(target_id) = nest_resolve_button_target(instance, is_pressed, scale_factor) else {
        return;
    };
    if is_pressed {
        instance.last_mouse_down_pane_id = Some(target_id.clone());
        // Click-to-focus: sticky until the next overlay click (or return to game mode).
        nest_set_keyboard_focus(instance, &target_id);
    } else {
        instance.last_mouse_down_pane_id = None;
    }
    let pointer_plan = instance.panes.get(&target_id).map(|pane| {
        let (px, py, pw, ph) = nest_pane_hit_rect(instance, pane, scale_factor);
        let translated = Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
        let evdev = winit_mouse_button_evdev(button);
        let (wx, wy) = nest_pointer_coords(instance, pane, instance.last_mouse_pos, scale_factor);
        (
            pane.external_wayland_server.clone(),
            pane.is_remote,
            pane.remote_input_fd,
            pane.id.clone(),
            translated,
            pw,
            ph,
            evdev,
            wx,
            wy,
        )
    });
    let Some((wayland, is_remote, remote_fd, pane_id, mut translated, pw, ph, evdev, wx, wy)) =
        pointer_plan
    else {
        return;
    };
    if let Some(ws) = wayland {
        ws.lock()
            .send_pointer_button_to_pane(&pane_id, wx, wy, evdev, is_pressed);
        return;
    }
    let action = if is_pressed {
        MouseButtonAction::Down
    } else {
        MouseButtonAction::Up
    };
    let servo_button = match button {
        winit::event::MouseButton::Left => ServoMouseButton::Primary,
        winit::event::MouseButton::Right => ServoMouseButton::Secondary,
        winit::event::MouseButton::Middle => ServoMouseButton::Auxiliary,
        winit::event::MouseButton::Back => ServoMouseButton::Back,
        winit::event::MouseButton::Forward => ServoMouseButton::Forward,
        winit::event::MouseButton::Other(v) => ServoMouseButton::Other(v),
    };
    if is_remote {
        if let Some(fd) = remote_fd {
            if let Some(pane) = instance.panes.get(&target_id) {
                if let Some(ref consumer) = pane.remote_consumer {
                    if consumer.tex_width > 0 && consumer.tex_height > 0 && pw > 0.0 && ph > 0.0 {
                        translated.x *= consumer.tex_width as f32 / pw;
                        translated.y *= consumer.tex_height as f32 / ph;
                    }
                }
            }
            let kind = if is_pressed { "MouseDown" } else { "MouseUp" };
            let btn_num = match servo_button {
                ServoMouseButton::Primary => 0,
                ServoMouseButton::Auxiliary => 1,
                ServoMouseButton::Secondary => 2,
                _ => 0,
            };
            let surface_id = format!("{}:{}", uuid, pane_id);
            let msg = serde_json::json!({
                "type": "Input",
                "event": {
                    "kind": kind,
                    "surface_id": surface_id,
                    "generation": 1,
                    "button": btn_num,
                    "x": translated.x,
                    "y": translated.y,
                    "modifiers": instance.modifiers.bits(),
                }
            })
            .to_string();
            forward_remote_input(fd, &msg);
        }
    } else if let Some(pane) = instance.panes.get_mut(&target_id) {
        pane.webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
            action,
            servo_button,
            servo::WebViewPoint::Device(translated),
        )));
    }
}

/// Whether a pane should receive mouse hit-testing.
/// Crash-covered remotes never get input (chrome overlay owns the slot). Host recovery
/// chrome (`__lotus_crash_*`) is hittable as soon as `pending_visible` is set — do not
/// wait for a layout-commit flip of `is_visible` or clicks fall through to the dead remote.
fn pane_accepts_pointer_input(pane: &PaneInstance) -> bool {
    if pane.under_crash_cover {
        return false;
    }
    if pane.pointer_passthrough {
        return false;
    }
    if pane.pending_visible {
        return true;
    }
    // Hide in flight: still committed visible but pending_visible already false.
    if pane.is_visible {
        return false;
    }
    false
}

/// True floating overlays beat gutters. Tree-backed leaves (including sandboxed
/// remotes with `Anchor::None`) must **not** — they are layout participants, not modals.
fn is_gutter_blocking_overlay(instance: &WindowInstance, pane: &PaneInstance) -> bool {
    if !pane_accepts_pointer_input(pane) || pane.anchor != PaneAnchor::None {
        return false;
    }
    if pane.id.starts_with("__lotus_crash_") {
        return true; // crash chrome must beat gutters for Respawn clicks
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
    if instance.gutter_hover.take().is_some() {
        // Same sticky-cursor trap as overlay resize: host Col/RowResize must be
        // dropped so the next pane hit can re-apply the nested client cursor.
        #[cfg(target_os = "linux")]
        {
            instance.nested_cursor_applied = false;
        }
        instance.window.set_cursor(winit::window::CursorIcon::Default);
    }
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
    #[cfg(target_os = "linux")]
    {
        instance.nested_cursor_applied = false;
    }
    instance.gutter_hover = Some(key);
}

/// Read the window's current monitor refresh rate in millihertz (fallback 60 Hz).
/// `ATRIUM_REFRESH` (Hz) overrides when set — gamescope-style `-r`.
fn monitor_refresh_rate_millihertz(window: &winit::window::Window) -> u32 {
    if let Ok(hz) = std::env::var("ATRIUM_REFRESH") {
        if let Ok(n) = hz.parse::<u32>() {
            if n > 0 {
                return n.saturating_mul(1000);
            }
        }
    }
    window
        .current_monitor()
        .and_then(|m| m.refresh_rate_millihertz())
        .filter(|&mhz| mhz > 0)
        .unwrap_or(60_000)
}

#[cfg(target_os = "linux")]
fn x11_root_size(display: &str) -> Option<(u32, u32)> {
    let out = std::process::Command::new("xwininfo")
        .args(["-display", display, "-root"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut w = None;
    let mut h = None;
    for line in s.lines() {
        if let Some(rest) = line.trim().strip_prefix("Width:") {
            w = rest.trim().parse().ok();
        } else if let Some(rest) = line.trim().strip_prefix("Height:") {
            h = rest.trim().parse().ok();
        }
    }
    match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
        _ => None,
    }
}

/// eglSwapInterval on the current EGL display.
/// Gamescope fifo present deadlocks with interval=1 — use 0 there and phase-align in software.
#[cfg(target_os = "linux")]
pub(crate) fn nest_set_egl_swap_interval_pub(interval: i32) {
    nest_set_egl_swap_interval(interval);
}

#[cfg(target_os = "linux")]
fn nest_set_egl_swap_interval(interval: i32) {
    use std::os::raw::{c_int, c_void};
    // Mesa eglSwapInterval does Wayland/X11 round-trips — only call on change.
    if crate::present_feedback::cached_swap_interval() == Some(interval) {
        return;
    }
    unsafe extern "C" {
        fn eglGetCurrentDisplay() -> *mut c_void;
        fn eglSwapInterval(dpy: *mut c_void, interval: c_int) -> u32;
    }
    unsafe {
        let dpy = eglGetCurrentDisplay();
        if dpy.is_null() {
            nest_host_diag("eglSwapInterval: no current EGL display");
            return;
        }
        let ok = eglSwapInterval(dpy, interval as c_int);
        if ok != 0 {
            crate::present_feedback::set_cached_swap_interval(interval);
        }
        if std::env::var_os("ATRIUM_PACING_DIAG").is_some() {
            nest_host_diag(&format!("eglSwapInterval({interval}) ok={ok}"));
        }
    }
}

/// Append a line to the atrium host log (always on — not gated by RUST_LOG).
#[cfg(target_os = "linux")]
pub(crate) fn nest_host_diag_pub(msg: &str) {
    nest_host_diag(msg);
}

#[cfg(target_os = "linux")]
fn nest_host_diag(msg: &str) {
    use std::io::Write;
    let line = format!(
        "[lotus-nest] {}\n",
        msg
    );
    // Mirror to tracing + durable log Steam/Game Mode can collect.
    info!("{}", line.trim_end());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/lotus-nest-host.log")
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// True when running under Bazzite/SteamOS Game Mode (embedded gamescope).
#[cfg(target_os = "linux")]
pub(crate) fn is_gamescope_session_pub() -> bool {
    is_gamescope_session()
}

#[cfg(target_os = "linux")]
fn is_gamescope_session() -> bool {
    if std::env::var_os("ATRIUM_GAMEMODE").is_some_and(|v| v != "0") {
        return true;
    }
    if std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some() {
        return true;
    }
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|d| d.to_ascii_lowercase().contains("gamescope"))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn steam_app_id_for_gamescope() -> String {
    std::env::var("SteamAppId")
        .or_else(|_| std::env::var("SteamGameId"))
        .or_else(|_| std::env::var("STEAM_COMPAT_APP_ID"))
        .unwrap_or_else(|_| "1".into())
}

/// X11 display Steam/gamescope focused for this game launch.
#[cfg(target_os = "linux")]
fn gamescope_x11_display() -> String {
    std::env::var("STEAM_GAME_DISPLAY_0")
        .or_else(|_| std::env::var("DISPLAY"))
        .unwrap_or_else(|_| ":0".into())
}

#[cfg(target_os = "linux")]
fn x11_window_xid(window: &winit::window::Window) -> Option<u64> {
    use raw_window_handle::RawWindowHandle;
    match window.window_handle().ok().map(|h| h.as_raw()) {
        Some(RawWindowHandle::Xlib(h)) => Some(h.window as u64),
        Some(RawWindowHandle::Xcb(h)) => Some(h.window.get() as u64),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn run_xprop(args: &[&str]) -> bool {
    match std::process::Command::new("xprop").args(args).status() {
        Ok(st) if st.success() => true,
        Ok(st) => {
            warn!("[lotus-nest] xprop {:?} exited {}", args, st);
            false
        }
        Err(e) => {
            warn!("[lotus-nest] xprop {:?} failed: {}", args, e);
            false
        }
    }
}

/// Make the lotus host window look like a Steam-launched game to embedded gamescope.
/// Without these atoms Game Mode sits on a black/empty frame waiting forever.
/// Mirrors ChimeraOS `gamescope-fg`: STEAM_GAME on the window + BASELAYER focus on root.
#[cfg(target_os = "linux")]
fn tag_window_for_gamescope(window: &winit::window::Window) {
    if !is_gamescope_session() {
        return;
    }
    let Some(xid) = x11_window_xid(window) else {
        warn!(
            "[lotus-nest] Game Mode: host window is not X11 — cannot set STEAM_GAME \
             (WINIT_UNIX_BACKEND=x11 required; bin/atrium sets this)"
        );
        return;
    };

    let app_id = steam_app_id_for_gamescope();
    let display = gamescope_x11_display();
    let id_arg = format!("0x{:x}", xid);

    // Nested compositor model (like nested gamescope): atrium's host window IS the
    // Steam game surface on STEAM_GAME_DISPLAY_0. Inner game+HUD live in our smithay
    // panes — we are NOT STEAM_OVERLAY / GAMESCOPE_EXTERNAL_OVERLAY (mangoapp).
    let steam_ok = run_xprop(&[
        "-display",
        &display,
        "-id",
        &id_arg,
        "-f",
        "STEAM_GAME",
        "32c",
        "-set",
        "STEAM_GAME",
        &app_id,
    ]);

    // Force fully opaque — transparent hosts often composite as "nothing" in gamescope.
    let _ = run_xprop(&[
        "-display",
        &display,
        "-id",
        &id_arg,
        "-f",
        "_NET_WM_WINDOW_OPACITY",
        "32c",
        "-set",
        "_NET_WM_WINDOW_OPACITY",
        "4294967295",
    ]);

    // steamcompmgr prefers a fullscreen game window; logs showed _NET_WM_STATE missing.
    let _ = run_xprop(&[
        "-display",
        &display,
        "-id",
        &id_arg,
        "-f",
        "_NET_WM_STATE",
        "32a",
        "-set",
        "_NET_WM_STATE",
        "_NET_WM_STATE_FULLSCREEN",
    ]);

    // Only our appid. Merging Steam's list lets 413091/769 float first and we
    // never win pick_primary_focus even after we have commits.
    let baselayer = app_id.clone();

    let focus_ok = run_xprop(&[
        "-display",
        &display,
        "-root",
        "-format",
        "GAMESCOPECTRL_BASELAYER_APPID",
        "32co",
        "-set",
        "GAMESCOPECTRL_BASELAYER_APPID",
        &baselayer,
    ]);

    // Do NOT pin GAMESCOPECTRL_BASELAYER_WINDOW until steamcompmgr has commits for
    // us. steamcompmgr.cpp: pick selects the WINDOW first, then only sets
    // focusWindow if window_has_commits(); otherwise FOCUSED_APP stays on Steam (769)
    // and input never reaches atrium (F9/Discord invisible).

    // Also nudge the UI display root (:0) — steamcompmgr often reads focus there.
    if display != ":0" {
        let _ = run_xprop(&[
            "-display",
            ":0",
            "-root",
            "-format",
            "GAMESCOPECTRL_BASELAYER_APPID",
            "32co",
            "-set",
            "GAMESCOPECTRL_BASELAYER_APPID",
            &baselayer,
        ]);
        let _ = run_xprop(&[
            "-display",
            ":0",
            "-root",
            "-remove",
            "GAMESCOPECTRL_BASELAYER_WINDOW",
        ]);
    }
    let _ = run_xprop(&[
        "-display",
        &display,
        "-root",
        "-remove",
        "GAMESCOPECTRL_BASELAYER_WINDOW",
    ]);

    if steam_ok {
        info!(
            "[lotus-nest] Game Mode: STEAM_GAME={} on {} (display {}), baselayer focus={}",
            app_id, id_arg, display, focus_ok
        );
    }

    // Retry a few times — window may not be mapped on the first ShowWindow tick.
    let retry_display = display.clone();
    let retry_id = id_arg.clone();
    let retry_app = app_id.clone();
    let retry_baselayer = baselayer.clone();
    let _ = std::thread::Builder::new()
        .name("atrium-gamescope-tag".into())
        .spawn(move || {
            for i in 1..=8 {
                std::thread::sleep(std::time::Duration::from_millis(150 * i as u64));
                let _ = run_xprop(&[
                    "-display",
                    &retry_display,
                    "-id",
                    &retry_id,
                    "-f",
                    "STEAM_GAME",
                    "32c",
                    "-set",
                    "STEAM_GAME",
                    &retry_app,
                ]);
                let _ = run_xprop(&[
                    "-display",
                    &retry_display,
                    "-root",
                    "-format",
                    "GAMESCOPECTRL_BASELAYER_APPID",
                    "32co",
                    "-set",
                    "GAMESCOPECTRL_BASELAYER_APPID",
                    &retry_baselayer,
                ]);
                if retry_display != ":0" {
                    let _ = run_xprop(&[
                        "-display",
                        ":0",
                        "-root",
                        "-format",
                        "GAMESCOPECTRL_BASELAYER_APPID",
                        "32co",
                        "-set",
                        "GAMESCOPECTRL_BASELAYER_APPID",
                        &retry_baselayer,
                    ]);
                }
            }
        });
}

/// Coalesce gamescope present kicks — a flood of RedrawWindow+Wake into winit's
/// user-event drain livelocks with spin_event_loop → NewFrameReady → more events,
/// so RedrawRequested never runs (stuck on black present #1).
#[cfg(target_os = "linux")]
pub(crate) static GAMESCOPE_CADENCE_PENDING: AtomicBool = AtomicBool::new(false);

/// Coalesce nested DMA-BUF/SHM commit wakes — one in-flight RedrawWindow drains
/// pump_frames; commit spam must not flood the user-event queue.
#[cfg(target_os = "linux")]
pub(crate) static NESTED_REDRAW_PENDING: AtomicBool = AtomicBool::new(false);
/// Bumped on every nested buffer import (coalesce + mid-present follow-up).
#[cfg(target_os = "linux")]
pub(crate) static NESTED_REDRAW_GEN: AtomicU64 = AtomicU64::new(0);
/// After present: allow the next commit wake; if a newer buffer arrived mid-frame,
/// queue one immediate follow-up (game-driven coalesce, not a refresh timer).
#[cfg(target_os = "linux")]
fn nest_finish_nested_redraw(instance: &WindowInstance, gen_at_start: u64) {
    let newer = NESTED_REDRAW_GEN.load(Ordering::SeqCst) != gen_at_start;
    NESTED_REDRAW_PENDING.store(false, Ordering::SeqCst);
    if !newer {
        return;
    }
    if !NESTED_REDRAW_PENDING.swap(true, Ordering::SeqCst) {
        instance.window.request_redraw();
    }
}

/// Kick one RedrawWindow per refresh period so gamescope X11 keeps scanning us out.
#[cfg(target_os = "linux")]
fn start_gamescope_present_cadence(window_id: String, refresh_mhz: u32) {
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, AtomicOrdering::SeqCst) {
        return;
    }
    let period_ms = (1_000_000u64 / refresh_mhz.max(1) as u64).max(1);
    let _ = std::thread::Builder::new()
        .name("atrium-gs-cadence".into())
        .spawn(move || {
            nest_host_diag(&format!(
                "gamescope present cadence thread start period={}ms window={}",
                period_ms, window_id
            ));
            loop {
                std::thread::sleep(std::time::Duration::from_millis(period_ms));
                // VRR / game-driven: never kick while primary commits are fresh —
                // free-running at panel max defeats adaptive-sync passthrough.
                // Fixed SoftFifo: keep kicking so spare vblanks hold the last frame
                // (mailbox rewind when the game sits well below refresh).
                let game_fresh = crate::pacing::game_commit_fresh(std::time::Duration::from_millis(
                    crate::pacing::PACING_IDLE_MS,
                ));
                if game_fresh && !crate::pacing::cadence_fills_spare_vblanks() {
                    continue;
                }
                // One in-flight kick only — send_event already wakes the X11 loop.
                if GAMESCOPE_CADENCE_PENDING.swap(true, AtomicOrdering::SeqCst) {
                    continue;
                }
                let Some(proxy) = EVENT_LOOP_PROXY.get() else {
                    GAMESCOPE_CADENCE_PENDING.store(false, AtomicOrdering::SeqCst);
                    continue;
                };
                if proxy
                    .send_event(EngineCommand::RedrawWindow(window_id.clone()))
                    .is_err()
                {
                    GAMESCOPE_CADENCE_PENDING.store(false, AtomicOrdering::SeqCst);
                }
            }
        });
}

/// Nested atrium Xwayland is not gamescope. Gamescope WSI (VkLayer_FROG_gamescope_wsi)
/// keys off GAMESCOPE_WAYLAND_DISPLAY and will try to resolve our :N as a gamescope
/// xwayland server id — that fails / stalls (XIO on nested :2). Strip outer session
/// hints the way nested gamescope isolates its own children from a parent compositor.
///
/// `restore_steam_overlay`: only for the primary game pane. Host LD_PRELOAD has
/// gameoverlayrenderer stripped (bin/atrium); Proton needs ATRIUM_SAVED_LD_PRELOAD.
/// Never restore overlay onto Discord or other overlay panes.
#[cfg(target_os = "linux")]
/// Gamescope often ships `LANG=C` / no session bus. Terminal TUIs need UTF-8
/// (discordo QR block chars) and the user bus for libsecret / oo7 keyring tokens.
fn nest_apply_session_env(cmd: &mut std::process::Command) {
    let utf8 = std::env::var("LANG")
        .ok()
        .filter(|l| {
            let u = l.to_ascii_uppercase();
            u.contains("UTF-8") || u.contains("UTF8")
        })
        .unwrap_or_else(|| "C.UTF-8".into());
    cmd.env("LANG", &utf8);
    cmd.env("LC_CTYPE", &utf8);
    cmd.env("LC_MESSAGES", &utf8);
    // LC_ALL=C from gamescope would override the UTF-8 LANG above.
    match std::env::var("LC_ALL") {
        Ok(v) => {
            let u = v.to_ascii_uppercase();
            if v.is_empty() || v == "C" || v == "POSIX" || !(u.contains("UTF-8") || u.contains("UTF8"))
            {
                cmd.env_remove("LC_ALL");
            }
        }
        Err(_) => {}
    }
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        let bus_path = std::path::Path::new(&runtime).join("bus");
        if bus_path.exists() {
            cmd.env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", bus_path.display()),
            );
        }
    }
}

fn sanitize_nested_gamescope_env(cmd: &mut std::process::Command, restore_steam_overlay: bool) {
    cmd.env("ENABLE_GAMESCOPE_WSI", "0");
    cmd.env("DISABLE_GAMESCOPE_WSI", "1");
    cmd.env_remove("GAMESCOPE_WAYLAND_DISPLAY");
    // Children talk to atrium's nested Xwayland, not Steam's game display.
    // gamescope main.cpp: STEAM_GAME_DISPLAY_0 is the *outer* game xwayland (:1).
    cmd.env_remove("STEAM_GAME_DISPLAY_0");
    cmd.env_remove("STEAM_GAME_DISPLAY_1");
    // Drop Gamescope-only WSI/Xwayland hints that break atrium's nested X11.
    cmd.env_remove("vk_xwayland_wait_ready");
    cmd.env_remove("vk_khr_present_wait");
    cmd.env_remove("ENABLE_HDR_WSI");
    cmd.env_remove("GAMESCOPE_LIMITER_FILE");
    cmd.env_remove("LIBEI_SOCKET");
    nest_apply_session_env(cmd);

    if restore_steam_overlay {
        match std::env::var("ATRIUM_SAVED_LD_PRELOAD") {
            Ok(saved) if !saved.is_empty() => {
                cmd.env("LD_PRELOAD", &saved);
                info!(
                    "[lotus-nest] restored ATRIUM_SAVED_LD_PRELOAD for game child (len={})",
                    saved.len()
                );
            }
            _ => {
                cmd.env_remove("LD_PRELOAD");
            }
        }
    } else {
        // Discord / non-game: inherit host (overlay already stripped) and scrub leftovers.
        if let Ok(preload) = std::env::var("LD_PRELOAD") {
            let filtered: Vec<&str> = preload
                .split(':')
                .filter(|p| {
                    let l = p.to_ascii_lowercase();
                    !l.contains("gamescope") && !l.contains("gameoverlayrenderer")
                })
                .collect();
            if filtered.is_empty() {
                cmd.env_remove("LD_PRELOAD");
            } else if filtered.len() != preload.split(':').count() {
                cmd.env("LD_PRELOAD", filtered.join(":"));
            }
        }
    }
}

/// Unfocused (and on Wayland, typically minimized too — no reliable signal): 10 Hz.
const BACKGROUND_REFRESH_MILLIHERTZ: u32 = 10_000; // 10 Hz
/// Pause sandboxed graphics when the platform *explicitly* reports hidden
/// (X11 occlude/minimize, zero-size). Wayland does not provide this via winit today.
const HIDDEN_REFRESH_MILLIHERTZ: u32 = 0;

fn window_is_viewable(instance: &WindowInstance) -> bool {
    #[cfg(target_os = "linux")]
    {
        return crate::pacing::window_is_viewable(instance);
    }
    #[cfg(not(target_os = "linux"))]
    {
        if instance.zero_size {
            return false;
        }
        if instance.os_minimized {
            return false;
        }
        !instance.os_occluded
    }
}

fn effective_refresh_millihertz(instance: &WindowInstance, monitor_mhz: u32) -> u32 {
    let monitor = monitor_mhz.max(1);
    // Embedded gamescope (Bazzite Game Mode) lies about focus/occlusion for X11
    // clients — winit often sees unfocused+occluded even while steamcompmgr is
    // showing us. Pausing paints → permanent black screen with Steam's B-to-exit
    // bar, while nested audio (Discord/game) still works. Always run full rate.
    #[cfg(target_os = "linux")]
    if is_gamescope_session() {
        if instance.zero_size {
            return HIDDEN_REFRESH_MILLIHERTZ;
        }
        return monitor;
    }
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
        #[cfg(target_os = "linux")]
        if let Some(ref ws) = instance.nested_compositor {
            ws.lock().set_output_refresh(next);
        }
    }
    #[cfg(target_os = "linux")]
    apply_pacing_policy(instance);
}

#[cfg(target_os = "linux")]
fn apply_pacing_policy(instance: &mut WindowInstance) {
    if instance.nested_compositor.is_none() {
        return;
    }
    let resolved = crate::pacing::resolve_pacing_policy();
    if resolved == instance.pacing_policy {
        return;
    }
    let prev = instance.pacing_policy;
    instance.pacing_policy = resolved;
    instance.pacing_policy_generation = instance.pacing_policy_generation.wrapping_add(1);
    info!(
        "[lotus-pacing] policy {:?} → {:?} @ {} mHz (gen={})",
        prev,
        resolved,
        instance.refresh_rate_millihertz,
        instance.pacing_policy_generation,
    );
    if let Some(ref ws) = instance.nested_compositor {
        ws.lock().set_pacing_policy(resolved);
    }
    instance.force_scene_repaint = true;
    instance.window.request_redraw();
}

#[cfg(target_os = "linux")]
fn send_remote_display_timing(fd: std::os::unix::io::RawFd, millihertz: u32) {
    // 0 = host-authoritative graphics pause (explicit hide only).
    let msg = format!(
        "{{\"type\":\"SetDisplayTiming\",\"refresh_rate_millihertz\":{}}}",
        millihertz
    );
    forward_remote_input(fd, &msg);
}

#[cfg(not(target_os = "linux"))]
fn send_remote_display_timing(_fd: i32, _millihertz: u32) {}

#[cfg(target_os = "linux")]
fn send_remote_vsync(fd: std::os::unix::io::RawFd, surface_id: &str, presentation_time_ns: u64) {
    let msg = format!(
        "{{\"type\":\"Vsync\",\"surface_id\":{},\"presentation_time_ns\":{}}}",
        serde_json::to_string(surface_id).unwrap_or_else(|_| "\"\"".into()),
        presentation_time_ns
    );
    forward_remote_input(fd, &msg);
}

#[cfg(not(target_os = "linux"))]
fn send_remote_vsync(_fd: i32, _surface_id: &str, _presentation_time_ns: u64) {}

impl ApplicationHandler<EngineCommand> for LotusApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // App is ready to create windows
        info!("Rust: Application Resumed");
    }

    #[cfg(target_os = "linux")]
    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device_id: DeviceId,
        event: DeviceEvent,
    ) {
        // Raw relative deltas while the host cursor is grabbed in game mode.
        // Absolute CursorMoved is suppressed while grabbed (avoids aim doubling).
        let DeviceEvent::MouseMotion { delta: (dx, dy) } = event else {
            return;
        };
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        for instance in self.windows.values() {
            if !instance.game_mode || !instance.host_pointer_grabbed {
                continue;
            }
            let Some(ref ws) = instance.nested_compositor else {
                continue;
            };
            ws.lock().send_pointer_relative_motion(dx, dy);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Pad focus step once per loop turn (before spin/paint decisions).
        #[cfg(target_os = "linux")]
        self.drain_pad_input(true, Some(event_loop));

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
        //
        // VRR / game-driven: primary commits wake RedrawWindow. Do NOT run a fixed
        // present timer while the game is actively committing — that spoils adaptive sync.
        // Stall fallback (>50ms since last primary commit) or overlay mode may pace.
        let mut pending_remote_work = false;
        let mut pace_to_refresh = false;
        let mut pace_occluded = false;
        let mut pace_refresh_mhz: u32 = 60000;
        #[cfg(target_os = "linux")]
        let mut soft_fifo_wake: Option<Instant> = None;
        for window in self.windows.values_mut() {
            sync_minimized_from_os(window);
            apply_refresh_policy(window);

            let viewable = window_is_viewable(window);
            let mut any_needs_repaint = false;
            let mut reason = String::new();
            let mhz = window.refresh_rate_millihertz.max(1);
            pace_refresh_mhz = mhz;

            // SoftFifo while nest is GameActive — including under the overlay —
            // so chrome open does not free-run at panel Hz. Pad Wake still breaks
            // WaitUntil; Nav is one-per-turn so gate defer is fine.
            #[cfg(target_os = "linux")]
            let soft_fifo_active = viewable
                && crate::pacing::fixed_software_fifo(window)
                && crate::pacing::present_activity(window)
                    == crate::pacing::PresentActivity::GameActive;
            #[cfg(target_os = "linux")]
            if soft_fifo_active {
                if let Some(until) = crate::pacing::soft_fifo_gate_instant() {
                    soft_fifo_wake = Some(match soft_fifo_wake {
                        Some(prev) => prev.min(until),
                        None => until,
                    });
                }
                if crate::pacing::soft_fifo_gate_due() {
                    any_needs_repaint = true;
                    reason = "soft_fifo gate due".into();
                }
            } else if viewable && crate::pacing::wants_stall_present_timer(window) {
                // UiIdle stall / non-SoftFifo hold — safe to pace-redraw.
                pace_to_refresh = true;
                any_needs_repaint = true;
                reason = if crate::pacing::wants_fixed_hold_cadence(window) {
                    "fixed SoftFifo hold cadence".into()
                } else {
                    "atrium stall/overlay present fallback".into()
                };
            }
            // Guard A: keep a slow FrameDone while the host is hidden so Proton
            // does not deadlock, without calling eglSwapInterval(1).
            #[cfg(target_os = "linux")]
            if !viewable && window.nested_compositor.is_some() {
                pace_occluded = true;
                let due = window
                    .last_occluded_frame_done
                    .map(|t| t.elapsed() >= Duration::from_millis(150))
                    .unwrap_or(true);
                if due {
                    any_needs_repaint = true;
                    reason = "occluded nested FrameDone".into();
                }
            }

            for pane in window.panes.values() {
                if pane.is_remote {
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
                // HUD/Servo: game input + GameActive → defer to next game present.
                // Overlay/edit wakes immediately even if the nest is still live.
                if pane.needs_repaint && viewable {
                    #[cfg(target_os = "linux")]
                    if crate::pacing::defer_overlay_servo_wake(window) {
                        window.overlay_dirty = true;
                        continue;
                    }
                    any_needs_repaint = true;
                    reason = format!("Pane '{}' needs repaint", pane.id);
                }
            }
            #[cfg(target_os = "linux")]
            if window.force_scene_repaint && viewable {
                // Host upscale / mode changes piggyback on SoftFifo gate — never
                // free-run presents between deadlines (mailbox hitch rewind).
                if soft_fifo_active {
                    if crate::pacing::soft_fifo_gate_due() {
                        any_needs_repaint = true;
                        reason = "force_scene_repaint (soft_fifo gate)".into();
                    }
                } else {
                    any_needs_repaint = true;
                    reason = "force_scene_repaint".into();
                }
            } else if window.overlay_dirty && viewable {
                // Under SoftFifo+GameActive, piggyback on the gate — do not let
                // overlay_dirty free-run presents at panel Hz (60→120 regression).
                if soft_fifo_active {
                    if crate::pacing::soft_fifo_gate_due() {
                        any_needs_repaint = true;
                        reason = "overlay dirty (soft_fifo gate)".into();
                    }
                } else if !window.game_mode
                    || crate::pacing::present_activity(window)
                        == crate::pacing::PresentActivity::UiIdle
                {
                    any_needs_repaint = true;
                    reason = if window.game_mode {
                        "overlay dirty (UiIdle)".into()
                    } else {
                        "overlay dirty (overlay mode)".into()
                    };
                }
            }
            if any_needs_repaint {
                trace!("Rust: [Lifecycle] Requesting Redraw for window (Reason: {})", reason);
                window.window.request_redraw();
            }
        }

        #[cfg(target_os = "linux")]
        if let Some(until) = soft_fifo_wake {
            // Prefer SoftFifo gate over generic refresh WaitUntil — same cadence,
            // but aligned to the PLL deadline and non-blocking for DeviceEvents.
            event_loop.set_control_flow(ControlFlow::WaitUntil(until));
        } else if pace_occluded {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(150),
            ));
        } else if pace_to_refresh || pending_remote_work {
            let period =
                Duration::from_nanos(1_000_000_000_000u64 / pace_refresh_mhz.max(1) as u64);
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + period));
        } else {
            // Game-driven or idle: sleep until commit / input wakes us.
            event_loop.set_control_flow(ControlFlow::Wait);
        }
        #[cfg(not(target_os = "linux"))]
        if pace_occluded {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(150),
            ));
        } else if pace_to_refresh || pending_remote_work {
            let period =
                Duration::from_nanos(1_000_000_000_000u64 / pace_refresh_mhz.max(1) as u64);
            event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + period));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: EngineCommand) {
        // Edges only (Tab/Toggle) — Nav/Confirm wait for about_to_wait so a
        // RedrawWindow flood cannot burst-apply focus steps.
        #[cfg(target_os = "linux")]
        self.drain_pad_input(false, Some(event_loop));

        // High-frequency user events must NOT spin Servo here. winit drains the
        // entire user-event queue before RedrawRequested; spin → NewFrameReady →
        // more queue entries livelocks paint for 1–2s, then pad Nav snaps catch-up.
        // about_to_wait spins once per loop turn instead.
        let skip_spin = match &event {
            EngineCommand::Wake
            | EngineCommand::RedrawWindow(_)
            | EngineCommand::NewFrameReady(_, _)
            | EngineCommand::ExecuteScript(_, _, _)
            | EngineCommand::ApplyHostUpscale { .. } => true,
            #[cfg(target_os = "linux")]
            EngineCommand::NestedCursor { .. }
            | EngineCommand::NestedPointerLock { .. }
            | EngineCommand::ControllerPadAction { .. }
            | EngineCommand::ControllerPadKey { .. } => true,
            _ => false,
        };
        if !skip_spin {
            if let Some(servo) = &self.servo {
                servo.spin_event_loop();
            }
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

                // Game Mode: create unmapped, set STEAM_GAME, then map, then EGL.
                // Tagging after map+present lets steamcompmgr hold the first two
                // commits unretired and wedges Present #3. Map still happens before
                // EGL so steamcompmgr gets commits (unmapped swaps never retire).
                #[cfg(target_os = "linux")]
                let gamescope_host = is_gamescope_session();
                #[cfg(not(target_os = "linux"))]
                let gamescope_host = false;

                #[cfg(target_os = "linux")]
                let (create_w, create_h) = if gamescope_host {
                    x11_root_size(&gamescope_x11_display())
                        .filter(|(w, h)| *w > 1 && *h > 1)
                        .unwrap_or((options.width, options.height))
                } else {
                    (options.width, options.height)
                };
                #[cfg(not(target_os = "linux"))]
                let (create_w, create_h) = (options.width, options.height);

                info!(
                    "Rust: Creating window with visible=false (pre-tag), transparent={}, size={}x{} gamescope={}",
                    options.transparent, create_w, create_h, gamescope_host
                );

                let mut window_attrs = Window::default_attributes()
                    .with_title(options.title.clone())
                    .with_inner_size(WinitPhysicalSize::new(create_w, create_h))
                    .with_decorations(!options.frameless)
                    .with_visible(false)
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

                // Relative mouse deltas for FPS lock (DeviceEvent::MouseMotion).
                // Always (not WhenFocused): gamescope focus reports are flaky, and we
                // still want raw deltas whenever host grab is on.
                #[cfg(target_os = "linux")]
                event_loop.listen_device_events(DeviceEvents::Always);

                // STEAM_GAME before map + before Present #1 (gamescope only).
                #[cfg(target_os = "linux")]
                if gamescope_host {
                    tag_window_for_gamescope(&window);
                    window.set_visible(true);
                    nest_host_diag(
                        "gamescope: STEAM_GAME tagged then mapped before EGL surface",
                    );
                }

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

                // Surfman Connection::from_display_handle(winit) — same X11 Display*
                // as winit so Mesa DRI3 PRESENT IDLE/COMPLETE events are pumped.
                #[cfg(target_os = "linux")]
                nest_host_diag(
                    "host EGL via WindowRenderingContext::from_display_handle (shared winit Display)",
                );

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

                // GPU broker must share the window surfman Connection so
                // swap-chain Surfaces can become UI-thread SurfaceTextures.
                #[cfg(target_os = "linux")]
                {
                    if let Some(connection) = RenderingContext::connection(rendering_context.as_ref()) {
                        match connection.create_adapter() {
                            Ok(adapter) => {
                                if let Err(e) =
                                    crate::gl::webgl_broker::bind_window_surfman(connection, adapter)
                                {
                                    error!("[webgl-broker] bind_window_surfman failed: {e}");
                                }
                            },
                            Err(e) => {
                                error!("[webgl-broker] create_adapter for bind failed: {e:?}");
                            },
                        }
                    } else {
                        error!("[webgl-broker] window rendering context has no Connection");
                    }
                }
                
                let _ = rendering_context.make_current();

                // Gamescope Xwayland: interval 1 deadlocks (buffers never retire on
                // vblank). Interval 0 + Fixed software FIFO in atrium_host_present.
                #[cfg(target_os = "linux")]
                if gamescope_host {
                    nest_set_egl_swap_interval(0);
                }

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
                        pointer_passthrough: false,
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
                        ghost_valid: false,
                        frame_seq: 0,
                        last_presented_seq: 0,
                        is_remote: false,
                        remote_consumer: None,
                        remote_input_fd: None,
                        resize_generation: 0,
                        under_crash_cover: false,
                        await_host_composited_frame: false,
                        #[cfg(target_os = "linux")]
                        external_surface_slot: None,
                        #[cfg(target_os = "linux")]
                        external_wayland_server: None,
                        #[cfg(target_os = "linux")]
                        external_child_pid: None,
                        #[cfg(target_os = "linux")]
                        external_follow_dock: false,
                        #[cfg(target_os = "linux")]
                        external_client_upscale_sgsr: false,
                        overlay_resize: None,
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
                    comp_program_oes: None,
                    #[cfg(target_os = "linux")]
                    sgsr_program: None,
                    #[cfg(target_os = "linux")]
                    sgsr_program_oes: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_size_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_radius_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_flip_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_texel_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_src_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_sharp_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_threshold_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_use_edge_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_oes_size_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_oes_radius_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_oes_flip_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_oes_texel_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_oes_src_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_oes_sharp_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_oes_threshold_loc: None,
                    #[cfg(target_os = "linux")]
                    u_sgsr_oes_use_edge_loc: None,
                    #[cfg(target_os = "linux")]
                    comp_vs: None,
                    #[cfg(target_os = "linux")]
                    fsr1_easu_program: None,
                    #[cfg(target_os = "linux")]
                    fsr1_easu_program_oes: None,
                    #[cfg(target_os = "linux")]
                    fsr1_rcas_program: None,
                    #[cfg(target_os = "linux")]
                    fsr1_locs: crate::gl::fsr1::Fsr1Locations::default(),
                    #[cfg(target_os = "linux")]
                    upscale_temp_fbo: None,
                    #[cfg(target_os = "linux")]
                    upscale_temp_tex: None,
                    #[cfg(target_os = "linux")]
                    upscale_temp_size: None,
                    comp_fbo: None,
                    comp_tex: None,
                    comp_vao: None,
                    comp_vbo: None,
                    u_comp_size_loc: None,
                    u_comp_radius_loc: None,
                    u_comp_flip_loc: None,
                    u_comp_oes_size_loc: None,
                    u_comp_oes_radius_loc: None,
                    u_comp_oes_flip_loc: None,
                    comp_tex_size: None,
                    scene_fbo: None,
                    scene_tex: None,
                    scene_tex_size: None,
                    committed_layout: None,
                    active_window_size: winit::dpi::PhysicalSize::new(size.width, size.height),
                    committed_window_size: None,
                    last_presented_window_size: None,
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
                    active_overlay_resize: None,
                    overlay_resize_hover: None,
                    pending_gutter_delta: None,
                    gutter_hover: None,
                    hover_pane_id: None,
                    game_mode: false,
                    #[cfg(target_os = "linux")]
                    host_hotkeys: std::collections::HashMap::new(),
                    #[cfg(target_os = "linux")]
                    host_hotkey_pressed: std::collections::HashSet::new(),
                    #[cfg(target_os = "linux")]
                    host_pointer_grabbed: false,
                    #[cfg(target_os = "linux")]
                    game_pointer_constraint_active: false,
                    #[cfg(target_os = "linux")]
                    game_cursor_hidden: false,
                    #[cfg(target_os = "linux")]
                    game_cursor: None,
                    #[cfg(target_os = "linux")]
                    last_overlay_pointer: None,
                    primary_game_pane: None,
                    last_stats_send: None,
                    force_scene_repaint: false,
                    #[cfg(target_os = "linux")]
                    nested_cursor: None,
                    #[cfg(target_os = "linux")]
                    nested_cursor_applied: false,
                    #[cfg(target_os = "linux")]
                    last_input_device: None,
                    #[cfg(target_os = "linux")]
                    nested_compositor: None,
                    #[cfg(target_os = "linux")]
                    pacing_policy: crate::pacing::resolve_pacing_policy(),
                    #[cfg(target_os = "linux")]
                    pacing_policy_generation: 0,
                    #[cfg(target_os = "linux")]
                    overlay_dirty: false,
                    #[cfg(target_os = "linux")]
                    last_occluded_frame_done: None,
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

                // Gamescope X11 often drops coalesced request_redraw after the first
                // frames. Drive RedrawWindow + Wake from a side thread at refresh.
                #[cfg(target_os = "linux")]
                if gamescope_host {
                    start_gamescope_present_cadence(window_id.clone(), monitor_refresh_rate_millihertz(&window));
                }

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
                if let Some(mut instance) = self.windows.remove(&window_id) {
                    #[cfg(target_os = "linux")]
                    nest_teardown_window(&mut instance);
                }
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
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    let pid = if pane_id == "main" && !instance.panes.contains_key("main") {
                        instance.primary_pane_id.clone()
                    } else {
                        pane_id.clone()
                    };

                    if let Some(pane) = instance.panes.get(&pid) {
                        // Hot path (pad Nav / chrome / per-frame telemetry) — keep off
                        // info to avoid log I/O lag.
                        log::debug!(
                            "Rust: [IPC] ExecuteScript window='{}' pane='{}' len={}",
                            window_id,
                            pid,
                            script.len()
                        );
                        pane.webview.evaluate_javascript(&script, |_| {});
                    } else {
                        warn!("Rust: ExecuteScript failed - pane '{}' not found in window '{}'", pid, window_id);
                    }
                    // Do not spin or request_redraw here — perf-hud stats alone can be
                    // 60–120 ExecuteScripts/sec; each spin+redraw starved RedrawRequested
                    // inside winit's user-event drain (1–2s freeze then snap). Queue the
                    // script and mark dirty; about_to_wait spins once and wakes paint.
                    #[cfg(target_os = "linux")]
                    {
                        instance.overlay_dirty = true;
                    }
                    #[cfg(not(target_os = "linux"))]
                    if window_is_viewable(instance) {
                        instance.window.request_redraw();
                    }
                }
            },            EngineCommand::ShowWindow(window_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    instance.window.set_visible(true);
                    #[cfg(target_os = "linux")]
                    {
                        // surfman set_window() asserts (EGL_NO_SURFACE) under gamescope
                        // Xwayland — never rebuild the widget surface that way. Just map,
                        // resize the bound surface to the root, tag, and keep presenting.
                        if is_gamescope_session() {
                            if let Some((rw, rh)) = x11_root_size(&gamescope_x11_display()) {
                                if rw > 1 && rh > 1 {
                                    let size = winit::dpi::PhysicalSize::new(rw, rh);
                                    let _ = instance.window.request_inner_size(size);
                                    instance.rendering_context.resize(size);
                                    let _ = instance.rendering_context.make_current();
                                    // Present path re-applies policy; keep 0 here so a
                                    // sync resize present cannot wedge the 2-buffer ring.
                                    nest_set_egl_swap_interval(0);
                                    nest_host_diag(&format!(
                                        "gamescope show: resized bound surface to root {}x{}",
                                        rw, rh
                                    ));
                                }
                            }
                        }
                        tag_window_for_gamescope(&instance.window);
                    }
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
                        // Servo notes that animation ticks are driven by repaint; if we
                        // miss a notify after resize the loop stalls until an external wake.
                        if animating && window_is_viewable(instance) {
                            #[cfg(target_os = "linux")]
                            if crate::pacing::defer_overlay_servo_wake(instance) {
                                instance.overlay_dirty = true;
                            } else {
                                instance.window.request_redraw();
                            }
                            #[cfg(not(target_os = "linux"))]
                            instance.window.request_redraw();
                        }
                    }
                }
            },
            EngineCommand::NewFrameReady(window_id, pane_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        trace!("Rust: [Lifecycle] EngineCommand::NewFrameReady for window {} pane {}", window_id, pane_id);

                        // Queue-of-1: keep ready_to_repaint false until RedrawRequested
                        // paints. Re-arming here + spin_event_loop livelocks the user-event
                        // drain under gamescope cadence (never reaches present #2).
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
                        // Guard B: in game input mode + GameActive, piggyback on the
                        // next game present. Overlay/edit must wake immediately —
                        // otherwise shell/plugin UI lags 1–2s while the nest still commits.
                        #[cfg(target_os = "linux")]
                        let defer_wake = crate::pacing::defer_overlay_servo_wake(instance);
                        #[cfg(not(target_os = "linux"))]
                        let defer_wake = false;
                        if defer_wake {
                            #[cfg(target_os = "linux")]
                            {
                                instance.overlay_dirty = true;
                            }
                        } else {
                            instance.window.request_redraw();
                        }

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
                    }
                }
            },
            #[cfg(target_os = "linux")]
            EngineCommand::NestedCursor {
                window_id,
                cursor,
                from_primary,
            } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if from_primary {
                        // Snapshot game shoes. Overlay must not paint these onto the host.
                        nest_record_game_cursor(instance, &cursor);
                        if instance.game_mode {
                            nest_wear_game_pointer_shoes(instance, Some(event_loop));
                        }
                    } else if !instance.game_mode
                        && instance.overlay_resize_hover.is_none()
                        && instance.active_overlay_resize.is_none()
                    {
                        nest_paint_host_cursor(instance, event_loop, &cursor);
                    }
                }
            }
            #[cfg(target_os = "linux")]
            EngineCommand::NestedPointerLock { window_id, active } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    // Lock shoe is independent of cursor visual (OW: custom under
                    // load lock, then hide for match). Host grab only in game mode.
                    instance.game_pointer_constraint_active = active;
                    if instance.game_mode {
                        nest_wear_game_pointer_shoes(instance, Some(event_loop));
                    }
                }
            }
            EngineCommand::RedrawWindow(window_id) => {
                #[cfg(target_os = "linux")]
                {
                    // Do not clear NESTED_REDRAW_PENDING here — RedrawRequested clears
                    // after present so mid-frame commits stay coalesced.
                    GAMESCOPE_CADENCE_PENDING.store(false, Ordering::SeqCst);
                }
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
                        pointer_passthrough: false,
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
                        ghost_valid: false,
                        frame_seq: 0,
                        last_presented_seq: 0,
                        is_remote: false,
                        remote_consumer: None,
                        remote_input_fd: None,
                        resize_generation: 0,
                        under_crash_cover: false,
                        await_host_composited_frame: false,
                        #[cfg(target_os = "linux")]
                        external_surface_slot: None,
                        #[cfg(target_os = "linux")]
                        external_wayland_server: None,
                        #[cfg(target_os = "linux")]
                        external_child_pid: None,
                        #[cfg(target_os = "linux")]
                        external_follow_dock: false,
                        #[cfg(target_os = "linux")]
                        external_client_upscale_sgsr: false,
                        overlay_resize: None,
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
            #[cfg(target_os = "linux")]
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
                        pointer_passthrough: false,
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
                        ghost_valid: false,
                        frame_seq: 0,
                        last_presented_seq: 0,
                        is_remote: true,
                        remote_consumer: consumer,
                        remote_input_fd: None,
                        resize_generation: 0,
                        under_crash_cover: false,
                        await_host_composited_frame: false,
                        #[cfg(target_os = "linux")]
                        external_surface_slot: None,
                        #[cfg(target_os = "linux")]
                        external_wayland_server: None,
                        #[cfg(target_os = "linux")]
                        external_child_pid: None,
                        #[cfg(target_os = "linux")]
                        external_follow_dock: false,
                        #[cfg(target_os = "linux")]
                        external_client_upscale_sgsr: false,
                        overlay_resize: None,
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
            #[cfg(target_os = "linux")]
            EngineCommand::CreateExternalAppPane {
                window_id,
                pane_id,
                command,
                args,
                rect,
                z_index,
                anchor,
                dock_order,
                visible,
                dbus_notifications,
                dbus_notification_apps,
                env: child_env,
                follow_dock,
                client_width,
                client_height,
                client_upscale_sgsr,
                resize_edges,
                resize_anchor,
                resize_min_w,
                resize_min_h,
                resize_max_w,
                resize_max_h,
            } => {
                let servo = self.ensure_servo().clone();
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if pane_id == "main" {
                        instance.auto_resize_main = false;
                    }

                    // First external app created is designated the Base Game
                    let is_primary = instance.primary_game_pane.is_none();
                    if is_primary {
                        instance.primary_game_pane = Some(pane_id.clone());
                        info!("[lotus-nest] designated primary base game pane: '{}'", pane_id);
                    }

                    // Host-bus Notify → atrium toast (Discord etc.). Primary game skips.
                    if !is_primary {
                        let mode = crate::dbus_notify::NotifyMode::parse(&dbus_notifications);
                        crate::dbus_notify::register_external_notify(
                            &window_id,
                            mode,
                            &dbus_notification_apps,
                            Some(&pane_id),
                        );
                    }

                    let hidpi_scale_factor_val = instance.window.scale_factor() as f32;
                    let width = (rect.size.width * hidpi_scale_factor_val).round() as u32;
                    let height = (rect.size.height * hidpi_scale_factor_val).round() as u32;
                    let servo_size = ServoPhysicalSize::new(width.max(1), height.max(1));
                    let offscreen_ctx = Rc::new(crate::gl::LotusPaneContext::new(
                        instance.rendering_context.clone(),
                        servo_size,
                    ));
                    let dummy_webview = WebViewBuilder::new(&servo, offscreen_ctx.clone()).build();

                    let wayland_server = if let Some(ref ws) = instance.nested_compositor {
                        ws.clone()
                    } else {
                        // Fixed nested output size (game render res), independent of pane layout —
                        // same idea as gamescope -W/-H. Buffer is letterboxed into the pane.
                        let (out_w, out_h) = crate::wayland::nest_game_output_size();
                        let refresh_mhz = instance.refresh_rate_millihertz.max(1);
                        match crate::wayland::LotusWaylandServer::start(
                            window_id.clone(),
                            out_w,
                            out_h,
                            refresh_mhz,
                        ) {
                            Ok(s) => {
                                let arc = Arc::new(parking_lot::Mutex::new(s));
                                instance.nested_compositor = Some(arc.clone());
                                instance.pacing_policy = crate::pacing::resolve_pacing_policy();
                                arc.lock()
                                    .set_pacing_policy(instance.pacing_policy);
                                // Backup display clock if WaitUntil is dropped (esp. X11/gamescope).
                                start_gamescope_present_cadence(
                                    window_id.clone(),
                                    instance.refresh_rate_millihertz.max(1),
                                );
                                arc
                            }
                            Err(e) => {
                                error!("[lotus-nest] failed to start nested Wayland server: {}", e);
                                return;
                            }
                        }
                    };

                    let surface_slot = Arc::new(parking_lot::Mutex::new(crate::wayland::ExternalSurfaceSlot::default()));

                    let overlay_follow = follow_dock && !is_primary;
                    let configured_client = if overlay_follow || is_primary {
                        None
                    } else {
                        Some((
                            client_width.unwrap_or(width).max(32),
                            client_height.unwrap_or(height).max(32),
                        ))
                    };
                    let overlay_resize = crate::overlay_resize::OverlayResizePolicy::from_parts(
                        resize_edges,
                        resize_anchor,
                        resize_min_w,
                        resize_min_h,
                        resize_max_w,
                        resize_max_h,
                    );
                    let pane_socket_name = wayland_server.lock().register_pane(
                        pane_id.clone(),
                        is_primary,
                        follow_dock,
                        configured_client,
                        (width, height),
                        surface_slot.clone(),
                        None,
                    );

                    let mut child_pid: Option<u32> = None;
                    {
                        let isolated_runtime = wayland_server.lock().runtime_dir.clone();
                        let wayland_display = pane_socket_name;
                        // Poll Xwayland readiness without holding the server mutex across
                        // sleeps — otherwise pump_frames cannot import DMA-BUFs.
                        let x11_display = {
                            let start = Instant::now();
                            let timeout = Duration::from_secs(8);
                            let mut got = None;
                            while start.elapsed() < timeout {
                                if let Some(d) = wayland_server.lock().get_x11_display() {
                                    got = Some(d);
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            got
                        };

                        let mut effective_args = args.clone();
                        if command == "flatpak" {
                            if let Some(pos) = effective_args.iter().position(|a| a == "run") {
                                let mut to_insert = Vec::new();
                                // Note: Do NOT use --nosocket=wayland or --nosocket=x11 as Flatpak's runner
                                // forcefully unsets WAYLAND_DISPLAY and DISPLAY in the sandbox when those are present.
                                // Complete sandbox isolation is achieved by redirecting XDG_RUNTIME_DIR to our
                                // private session directory, so the app cannot see or reach /run/user/<uid>/wayland-0.
                                to_insert.push(format!("--filesystem={}", isolated_runtime.display()));
                                to_insert.push(format!("--env=XDG_RUNTIME_DIR={}", isolated_runtime.display()));
                                to_insert.push(format!("--env=WAYLAND_DISPLAY={}", wayland_display));
                                if let Some(d) = x11_display {
                                    to_insert.push(format!("--env=DISPLAY=:{}", d));
                                }
                                to_insert.push("--filesystem=/tmp".to_string());

                                for arg in to_insert.into_iter().rev() {
                                    if !effective_args.contains(&arg) {
                                        effective_args.insert(pos + 1, arg);
                                    }
                                }
                            }
                        }

                        let mut cmd = std::process::Command::new(&command);
                        cmd.args(&effective_args);
                        // Spawn-env bag (e.g. DISCORDO_TOKEN) — never put secrets on argv.
                        for (k, v) in &child_env {
                            cmd.env(k, v);
                        }
                        // Own process group so teardown can SIGTERM/SIGKILL the whole tree
                        // (Proton → wineserver → game), not just the launcher PID.
                        #[cfg(unix)]
                        {
                            use std::os::unix::process::CommandExt;
                            unsafe {
                                cmd.pre_exec(|| {
                                    if libc::setpgid(0, 0) != 0 {
                                        return Err(std::io::Error::last_os_error());
                                    }
                                    Ok(())
                                });
                            }
                        }

                        let log_path = format!("/tmp/lotus-nest-child-{}.log", pane_id);
                        if let Ok(file) = std::fs::File::create(&log_path) {
                            if let Ok(err_file) = file.try_clone() {
                                cmd.stdout(file);
                                cmd.stderr(err_file);
                            }
                        }

                        // Flatpak stays in the hermetic runtime. Native/Proton children keep the
                        // host XDG_RUNTIME_DIR (Steam IPC) and get a symlink to the nested
                        // Wayland socket under that dir — absolute WAYLAND_DISPLAY paths are
                        // flaky in Wine, and reusing the name "wayland-0" would hit the host.
                        // Primary stays on nested X11 for Proton. Every other client keeps
                        // its own pane socket; a single shared symlink would retarget them.
                        let nested_wl_abs = isolated_runtime.join(&wayland_display);
                        let is_flatpak = command == "flatpak";
                        let wayland_client = is_flatpak || !is_primary;
                        if is_flatpak {
                            cmd.env("XDG_RUNTIME_DIR", &isolated_runtime);
                            cmd.env("WAYLAND_DISPLAY", &wayland_display);
                        } else if let Ok(host_runtime) = std::env::var("XDG_RUNTIME_DIR") {
                            let host_dir = std::path::PathBuf::from(&host_runtime);
                            let child_wl_name = if is_primary {
                                format!(
                                    "wayland-nest-{}",
                                    isolated_runtime
                                        .file_name()
                                        .and_then(|s| s.to_str())
                                        .unwrap_or("session")
                                )
                            } else {
                                wayland_display.clone()
                            };
                            let link_path = host_dir.join(&child_wl_name);
                            let _ = std::fs::remove_file(&link_path);
                            if let Err(e) = std::os::unix::fs::symlink(&nested_wl_abs, &link_path) {
                                warn!(
                                    "[lotus-nest] failed to symlink nested Wayland into host runtime: {}",
                                    e
                                );
                                // Fallback: absolute path (libwayland accepts it).
                                cmd.env("WAYLAND_DISPLAY", nested_wl_abs.as_os_str());
                            } else {
                                debug!(
                                    "[lotus-nest] nested Wayland exposed to host runtime as {}",
                                    child_wl_name
                                );
                                cmd.env("WAYLAND_DISPLAY", &child_wl_name);
                            }
                        } else {
                            cmd.env("WAYLAND_DISPLAY", nested_wl_abs.as_os_str());
                        }
                        cmd.env_remove("WAYLAND_SOCKET");
                        // Nested compositor (gamescope-shaped): Proton/Wine speak X11 into
                        // *our* Xwayland. Outer gamescope only sees atrium's host window on
                        // STEAM_GAME_DISPLAY_0 (:1). Never launch the game onto :1 directly.
                        if let Some(d) = x11_display {
                            if wayland_client {
                                if is_flatpak {
                                    info!(
                                        "[lotus-nest] nested Xwayland ready — flatpak child DISPLAY=:{} \
                                         and WAYLAND_DISPLAY={} (host stays on gamescope game display)",
                                        d, wayland_display
                                    );
                                    cmd.env("DISPLAY", format!(":{}", d));
                                } else {
                                    info!(
                                        "[lotus-nest] pane '{}' on nested Wayland ({})",
                                        pane_id, wayland_display
                                    );
                                    // DISPLAY set would let GTK/Qt open an X11 window, and
                                    // every X11 window is assigned to the primary pane.
                                    cmd.env_remove("DISPLAY");
                                }
                                cmd.env("SDL_VIDEODRIVER", "wayland");
                                cmd.env("QT_QPA_PLATFORM", "wayland");
                                cmd.env("GDK_BACKEND", "wayland");
                                cmd.env_remove("PROTON_ENABLE_WAYLAND");
                                sanitize_nested_gamescope_env(&mut cmd, false);
                            } else {
                                info!(
                                    "[lotus-nest] nested Xwayland ready — child DISPLAY=:{} \
                                     (host stays on gamescope game display)",
                                    d
                                );
                                cmd.env("DISPLAY", format!(":{}", d));
                                // Nested X11 only — leave Steam/Proton/Pulse env alone
                                // (STEAM_COMPAT_*, SteamAppId, audio, etc. inherit).
                                cmd.env_remove("WAYLAND_DISPLAY");
                                cmd.env("SDL_VIDEODRIVER", "x11");
                                cmd.env("QT_QPA_PLATFORM", "xcb");
                                cmd.env("GDK_BACKEND", "x11");
                                cmd.env_remove("PROTON_ENABLE_WAYLAND");
                                // Restore Steam overlay preload only for the primary game pane.
                                sanitize_nested_gamescope_env(&mut cmd, is_primary);
                            }
                        } else {
                            warn!(
                                "[lotus-nest] nested Xwayland not ready in time — \
                                 falling back to Wayland-only child (Proton may fail)"
                            );
                            cmd.env_remove("DISPLAY");
                            cmd.env("SDL_VIDEODRIVER", "wayland");
                            cmd.env("QT_QPA_PLATFORM", "wayland");
                            cmd.env("GDK_BACKEND", "wayland");
                            if !is_flatpak {
                                cmd.env("PROTON_ENABLE_WAYLAND", "1");
                                sanitize_nested_gamescope_env(&mut cmd, is_primary);
                            } else {
                                sanitize_nested_gamescope_env(&mut cmd, false);
                            }
                        }

                        match cmd.spawn() {
                            Ok(mut child) => {
                                let pid = child.id();
                                info!("[lotus-nest] spawned '{}' with PID {}", command, pid);
                                child_pid = Some(pid);
                                wayland_server.lock().update_pane_pid(&pane_id, pid);
                                // Reap exit; if the primary game dies (Alt+F4 etc.), tear down atrium.
                                let exit_window_id = window_id.clone();
                                let exit_pane_id = pane_id.clone();
                                let close_on_exit = is_primary;
                                let spawned_at = Instant::now();
                                let _ = std::thread::Builder::new()
                                    .name(format!("atrium-wait-{}", pid))
                                    .spawn(move || {
                                        // Steam's launch wrapper can exit as soon as it hands off.
                                        // Stay up for that. If Proton was in the process group,
                                        // wait until the group is gone, then tear the host down.
                                        let pgid = pid as i32;
                                        let mut direct = Some(child);
                                        let mut status = None;
                                        loop {
                                            if let Some(ch) = direct.as_mut() {
                                                match ch.try_wait() {
                                                    Ok(Some(st)) => {
                                                        status = Some(st);
                                                        direct = None;
                                                    }
                                                    Ok(None) => {}
                                                    Err(_) => direct = None,
                                                }
                                            }
                                            let group_alive = unsafe { libc::kill(-pgid, 0) == 0 };
                                            if direct.is_none() && !group_alive {
                                                break;
                                            }
                                            std::thread::sleep(Duration::from_millis(200));
                                        }
                                        let code = status.as_ref().and_then(|s| s.code()).unwrap_or(-1);
                                        let lifetime_ms = spawned_at.elapsed().as_millis() as u64;
                                        info!(
                                            "[lotus-nest] external child pid {} group gone ({:?}) pane='{}' primary={} lifetime_ms={}",
                                            pid,
                                            status,
                                            exit_pane_id,
                                            close_on_exit,
                                            lifetime_ms
                                        );
                                        if close_on_exit && lifetime_ms >= 3000 {
                                            if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                                                let _ = proxy.send_event(
                                                    EngineCommand::CloseWindow(exit_window_id.clone()),
                                                );
                                            }
                                        } else if close_on_exit {
                                            info!(
                                                "[lotus-nest] primary child exited after {}ms — leaving host up",
                                                lifetime_ms
                                            );
                                        } else {
                                            push_renderer_ipc(
                                                &exit_window_id,
                                                "nest:external-exit",
                                                serde_json::json!({
                                                    "pane_id": exit_pane_id,
                                                    "pid": pid,
                                                    "code": code,
                                                    "lifetime_ms": lifetime_ms,
                                                }),
                                            );
                                        }
                                    });
                            }
                            Err(e) => {
                                error!("[lotus-nest] failed to spawn '{}': {}", command, e);
                            }
                        }
                    }

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
                        needs_repaint: false,
                        is_visible: visible,
                        pointer_passthrough: false,
                        drag_regions: Vec::new(),
                        no_drag_regions: Vec::new(),
                        offscreen_ctx,
                        servo_busy: false,
                        pending_servo_size: None,
                        requested_servo_size: servo_size,
                        current_servo_size: servo_size,
                        ready_to_repaint: Arc::new(AtomicBool::new(true)),
                        ready_frame_size: None,
                        first_frame_painted: true,
                        first_frame_painted_time: None,
                        pending_physical_rect: None,
                        is_resizing: false,
                        pending_visible: visible,
                        ghost_tex: None,
                        ghost_tex_size: None,
                        ghost_valid: false,
                        frame_seq: 0,
                        last_presented_seq: 0,
                        is_remote: false,
                        remote_consumer: None,
                        remote_input_fd: None,
                        resize_generation: 0,
                        under_crash_cover: false,
                        await_host_composited_frame: false,
                        external_surface_slot: Some(surface_slot),
                        external_wayland_server: Some(wayland_server),
                        external_child_pid: child_pid,
                        external_follow_dock: overlay_follow,
                        external_client_upscale_sgsr: client_upscale_sgsr,
                        overlay_resize: overlay_resize.clone(),
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
                    // Do not grab here — wait for the game's cursor/lock requests.
                    instance.window.request_redraw();
                }
            },
            #[cfg(target_os = "linux")]
            EngineCommand::SetGameMode(window_id, mode) => {
                // Nest-ready guard lives here (winit loop), not on the N-API thread,
                // so FIFO CreateExternalAppPane → SetGameMode(true) works (EC4).
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if mode && instance.nested_compositor.is_none() {
                        warn!(
                            "[lotus-nest] setGameMode(true) ignored: nested compositor not started yet"
                        );
                    } else {
                        nest_apply_game_mode(instance, mode, Some(event_loop));
                        info!("[lotus-nest] game mode set: {}", mode);
                        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                            crate::evdev_pad::ensure_started(window_id.clone(), proxy.clone());
                        }
                        // overlay = !game_mode — always re-assert so a JS mode
                        // desync cannot leave EVIOCGRAB stuck after "return to game".
                        crate::evdev_pad::set_overlay_mode(!mode);
                        if !mode {
                            // Overlay: retry EVIOCGRAB so Proton/SDL cannot keep
                            // reading pads if the first grab raced Steam Input.
                            crate::evdev_pad::retry_overlay_grabs();
                        }
                        if mode {
                            crate::evdev_pad::set_key_forward_pane(None);
                        }
                        push_renderer_ipc(
                            &window_id,
                            "nest:game-mode",
                            serde_json::json!({ "game_mode": mode }),
                        );
                    }
                }
            },
            #[cfg(target_os = "linux")]
            EngineCommand::ControllerPadAction {
                window_id,
                action,
                dir,
            } => {
                // Legacy FIFO path — live pad traffic uses drain_pending_pad().
                let injected = self.apply_controller_pad_action(
                    &window_id,
                    &action,
                    dir,
                    Some(event_loop),
                );
                if injected {
                    if let Some(servo) = &self.servo {
                        servo.spin_event_loop();
                    }
                }
            },
            #[cfg(target_os = "linux")]
            EngineCommand::ControllerPadKey {
                window_id,
                pane_id,
                keycode,
                pressed,
            } => {
                self.apply_controller_pad_key(&window_id, &pane_id, keycode, pressed);
            },
            #[cfg(target_os = "linux")]
            EngineCommand::SetOskActive(_window_id, active) => {
                crate::evdev_pad::set_osk_active(active);
            },
            #[cfg(target_os = "linux")]
            EngineCommand::SetPadKeyForwardPane(_window_id, pane_id) => {
                crate::evdev_pad::set_key_forward_pane(pane_id);
            },
            #[cfg(target_os = "linux")]
            EngineCommand::SetPadSpatialFocusPane(_window_id, pane_id) => {
                crate::evdev_pad::set_spatial_focus_pane(pane_id);
            },
            #[cfg(target_os = "linux")]
            EngineCommand::ApplyHostUpscale {
                window_id,
                config_json,
            } => {
                match serde_json::from_str::<crate::host_upscale::HostUpscaleConfig>(&config_json) {
                    Ok(cfg) => {
                        let applied = crate::window::set_host_upscale_config(cfg);
                        info!(
                            "[lotus-upscale] apply mode={} rcas={} sgsr_thr={:.5} sgsr_sharp={:.3} edge_dir={} rcas_stops={:.3}",
                            applied.mode.as_str(),
                            applied.sharpen_rcas,
                            applied.sgsr_edge_threshold,
                            applied.sgsr_edge_sharpness,
                            applied.sgsr_use_edge_direction,
                            applied.rcas_sharpness_stops
                        );
                        if let Some(instance) = self.windows.get_mut(&window_id) {
                            // GL must be current to link FSR/SGSR/RCAS; SoftFifo
                            // early-return must not drop this mode change.
                            let _ = instance.rendering_context.make_current();
                            instance.ensure_host_upscale_shaders();
                            instance.force_scene_repaint = true;
                            #[cfg(target_os = "linux")]
                            {
                                instance.overlay_dirty = true;
                            }
                            // Bump primary seq so scene_needs_present cannot idle-park
                            // on the previous upscale path.
                            if let Some(pid) = instance.primary_game_pane.clone() {
                                if let Some(pane) = instance.panes.get_mut(&pid) {
                                    pane.frame_seq = pane.frame_seq.wrapping_add(1);
                                }
                            }
                            if window_is_viewable(instance) {
                                instance.window.request_redraw();
                            }
                        }
                        push_renderer_ipc(
                            &window_id,
                            "nest:host-upscale",
                            serde_json::json!({
                                "mode": applied.mode.as_str(),
                                "sharpenRcas": applied.sharpen_rcas,
                                "sgsrEdgeThreshold": applied.sgsr_edge_threshold,
                                "sgsrEdgeSharpness": applied.sgsr_edge_sharpness,
                                "sgsrUseEdgeDirection": applied.sgsr_use_edge_direction,
                                "rcasSharpnessStops": applied.rcas_sharpness_stops,
                            }),
                        );
                    }
                    Err(e) => {
                        warn!("[lotus-upscale] ApplyHostUpscale bad json: {}", e);
                    }
                }
            },
            #[cfg(target_os = "linux")]
            EngineCommand::RegisterHostHotkey {
                window_id,
                id,
                key,
                modifiers,
            } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    match crate::window::parse_keyboard_event_code(&key) {
                        Some(keycode) => {
                            instance.host_hotkeys.insert(
                                id.clone(),
                                crate::window::HostHotkeyBinding {
                                    id: id.clone(),
                                    key: keycode,
                                    modifiers,
                                },
                            );
                            info!(
                                "[lotus-nest] host hotkey registered id={} key={} mods={:#x}",
                                id, key, modifiers
                            );
                        }
                        None => {
                            warn!(
                                "[lotus-nest] registerHostHotkey: unknown KeyboardEvent.code '{}'",
                                key
                            );
                        }
                    }
                }
            },
            #[cfg(target_os = "linux")]
            EngineCommand::UnregisterHostHotkey { window_id, id } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    instance.host_hotkeys.remove(&id);
                }
            },
            #[cfg(target_os = "linux")]
            EngineCommand::ClearHostHotkeys(window_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    instance.host_hotkeys.clear();
                    instance.host_hotkey_pressed.clear();
                }
            },
            EngineCommand::SetFullscreen(window_id, enabled) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if enabled {
                        instance.window.set_fullscreen(Some(
                            winit::window::Fullscreen::Borderless(None),
                        ));
                    } else {
                        instance.window.set_fullscreen(None);
                    }
                    info!("[lotus-nest] lotus window fullscreen={}", enabled);
                    #[cfg(target_os = "linux")]
                    tag_window_for_gamescope(&instance.window);
                    push_renderer_ipc(
                        &window_id,
                        "nest:fullscreen",
                        serde_json::json!({ "fullscreen": enabled }),
                    );
                    apply_refresh_policy(instance);
                    #[cfg(target_os = "linux")]
                    if let Some(ref ws) = instance.nested_compositor {
                        ws.lock().set_output_refresh(instance.refresh_rate_millihertz);
                    }
                }
            },
            EngineCommand::SetPaneZIndex(window_id, pane_id, z_index) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        pane.z_index = z_index;
                        instance.window.request_redraw();
                    }
                }
            },
            EngineCommand::RemovePane(window_id, pane_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    crate::gl::compositor::destroy_local_ghost(instance, &pane_id);
                    let _ = instance.panes.remove(&pane_id);
                    // Focus fallback: If the removed pane was active, move focus to primary.
                    if instance.active_pane_id == pane_id {
                        instance.active_pane_id = instance.primary_pane_id.clone();
                    }
                }
            },
            #[cfg(target_os = "linux")]
            EngineCommand::SetExternalClientSize {
                window_id,
                pane_id,
                width,
                height,
            } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(ref ws) = instance.nested_compositor {
                        ws.lock()
                            .set_external_client_size(&pane_id, width, height);
                    }
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        pane.is_dirty = true;
                    }
                    instance.window.request_redraw();
                }
            }
            EngineCommand::SetPaneRectVisual(window_id, pane_id, rect) => {
                // Dock-only move: scale existing buffer into the new quad. No Servo
                // resize and no nested xdg configure until a later SetPaneRect.
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if pane_id == "main" {
                        instance.auto_resize_main = false;
                    }
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        if !pane.is_dirty && pane.last_notified_rect == Some(rect) {
                            return;
                        }
                        let scale_factor = instance.window.scale_factor() as f32;
                        pane.rect = rect;
                        pane.last_notified_rect = Some(rect);
                        let pane_phys = euclid::default::Rect::new(
                            euclid::default::Point2D::new(
                                (rect.origin.x * scale_factor).round() as i32,
                                (rect.origin.y * scale_factor).round() as i32,
                            ),
                            euclid::default::Size2D::new(
                                (rect.size.width * scale_factor).round() as i32,
                                (rect.size.height * scale_factor).round() as i32,
                            ),
                        );
                        // Keep the previous buffer; compositor scales into last_physical_rect.
                        pane.last_physical_rect = pane_phys;
                        pane.pending_physical_rect = None;
                        pane.pending_servo_size = None;
                        pane.is_resizing = false;
                        pane.anchor = PaneAnchor::None;
                        pane.is_dirty = true;
                    }
                    instance.layout_root.set_leaf_detached(&pane_id, true);
                    let physical_size = instance.window.inner_size();
                    let scale_factor = instance.window.scale_factor() as f32;
                    instance.recalculate_layout(physical_size, scale_factor);
                    instance.window.request_redraw();
                }
            }
            EngineCommand::SetPaneRect(window_id, pane_id, rect) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if pane_id == "main" {
                        instance.auto_resize_main = false;
                    }
                    let mut external_resize: Option<(u32, u32)> = None;
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
                        pane.last_physical_rect = pane_phys;
                        pane.anchor = PaneAnchor::None;

                        let servo_size = ServoPhysicalSize::new(
                            ((rect.size.width * scale_factor).round() as u32).max(1),
                            ((rect.size.height * scale_factor).round() as u32).max(1)
                        );

                        #[cfg(target_os = "linux")]
                        if pane.external_wayland_server.is_some() {
                            external_resize = Some((servo_size.width, servo_size.height));
                        }
                        
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
                            pane.ghost_valid = false;
                            pane.pending_physical_rect = None;
                        }
                    }
                    #[cfg(target_os = "linux")]
                    if let Some((w, h)) = external_resize {
                        if let Some(ref ws) = instance.nested_compositor {
                            ws.lock().send_resize_pane(&pane_id, w, h);
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
                    nest_set_pane_visible(instance, &pane_id, visible);
                    let physical_size = instance.window.inner_size();
                    let scale_factor = instance.window.scale_factor() as f32;
                    instance.recalculate_layout(physical_size, scale_factor);
                    instance.window.request_redraw();
                }
            },
            EngineCommand::SetPanePassthrough(window_id, pane_id, passthrough) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        pane.pointer_passthrough = passthrough;
                    }
                }
            },
            #[cfg(target_os = "linux")]
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
                            crash.last_physical_rect = phys;
                            crash.anchor = PaneAnchor::None;
                            // Below select overlay (i32::MAX) but above normal content.
                            crash.z_index = 2_000_000;
                            // Eager visibility: hit-testing uses is_visible; waiting for a
                            // layout flip left crash chrome painted (staged) but unclickable.
                            crash.pending_visible = true;
                            crash.is_visible = true;
                            crash.is_dirty = true;
                            crash.pending_physical_rect = Some(phys);
                        }
                    } else if let Some(crash) = instance.panes.get_mut(&crash_pane_id) {
                        crash.anchor = PaneAnchor::None;
                        crash.z_index = 2_000_000;
                        crash.pending_visible = true;
                        crash.is_visible = true;
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
            #[cfg(target_os = "linux")]
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
            #[cfg(target_os = "linux")]
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
                    // External nest panes (Ghostty/Discordo) need WL keyboard seat
                    // focus, not just sticky active_pane_id — otherwise clicks work
                    // but keystrokes never reach the client after dock F9 / focus().
                    #[cfg(target_os = "linux")]
                    nest_set_keyboard_focus(instance, &pane_id);
                    #[cfg(not(target_os = "linux"))]
                    {
                        instance.active_pane_id = pane_id;
                    }
                }
            },
            #[cfg(target_os = "linux")]
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
        
        // Mirror leading skip_spin — trailing spin on RedrawWindow/ExecuteScript/pad
        // re-queued NewFrameReady into the same drain and starved paint.
        if !skip_spin {
            if let Some(servo) = &self.servo {
                servo.spin_event_loop();
            }
        }
        // Wait (or WaitUntil from about_to_wait). Poll fought phase-align and bunched frames.
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

        // Edges only here; focus steps are applied in about_to_wait (one per turn).
        #[cfg(target_os = "linux")]
        self.drain_pad_input(false, Some(event_loop));

        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }
        // about_to_wait upgrades to WaitUntil(refresh) under gamescope — keep Wait here.
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
                        // Use live inner_size — active_window_size lags until nuclear flip
                        // and would stage layout for the wrong physical size on maximize /
                        // monitor moves that fire ScaleFactorChanged with Resized.
                        let live = instance.window.inner_size();
                        instance.recalculate_layout(live, sf);
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
                        if let Some(mut closing) = self.windows.remove(&uuid) {
                            #[cfg(target_os = "linux")]
                            nest_teardown_window(&mut closing);
                        }
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
                        #[cfg(target_os = "linux")]
                        let nested_gen_at_start = NESTED_REDRAW_GEN.load(Ordering::SeqCst);
                        #[cfg(target_os = "linux")]
                        {
                            GAMESCOPE_CADENCE_PENDING.store(false, Ordering::SeqCst);
                            // Keep NESTED_REDRAW_PENDING set until after present so commits
                            // mid-frame coalesce instead of queuing a short follow-up present.
                            if is_gamescope_session() {
                                static REDRAW_N: AtomicU64 = AtomicU64::new(0);
                                let n = REDRAW_N.fetch_add(1, Ordering::Relaxed) + 1;
                                if n <= 8 || n % 120 == 0 {
                                    nest_host_diag(&format!(
                                        "RedrawRequested #{n} focused={} occluded={} nested={}",
                                        instance.os_focused,
                                        instance.os_occluded,
                                        instance.nested_compositor.is_some()
                                    ));
                                }
                            }
                        }
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
                            trace!("Rust: [Lifecycle] RedrawRequested occluded (Guard A — no blocking swap)");
                            #[cfg(target_os = "linux")]
                            let slow_fd = {
                                let due = instance
                                    .last_occluded_frame_done
                                    .map(|t| t.elapsed() >= Duration::from_millis(150))
                                    .unwrap_or(true);
                                if due {
                                    instance.last_occluded_frame_done = Some(Instant::now());
                                    if let Some(ref ws) = instance.nested_compositor {
                                        let now_ms = (std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_millis()
                                            & 0xFFFFFFFF) as u32;
                                        let server = ws.lock();
                                        server.notify_frame_done(now_ms);
                                        server.release_previous_buffer();
                                    }
                                }
                                due
                            };
                            #[cfg(not(target_os = "linux"))]
                            {
                                instance.window.pre_present_notify();
                                let _ = instance.rendering_context.make_current();
                                let _ = gl::atrium_host_present(instance);
                            }
                            #[cfg(target_os = "linux")]
                            {
                                let _ = slow_fd;
                            }
                            #[cfg(target_os = "linux")]
                            nest_finish_nested_redraw(instance, nested_gen_at_start);
                        } else {
                        // SoftFifo not due yet: skip paint/composite/swap entirely.
                        // Leave NESTED_REDRAW_PENDING set so commits coalesce; about_to_wait
                        // WaitUntil requests redraw when the gate is due. force_scene_repaint
                        // / overlay_dirty piggyback on that same gate — bypassing it here
                        // free-ran the mailbox (hitch / one-frame rewind).
                        #[cfg(target_os = "linux")]
                        if crate::pacing::fixed_software_fifo(instance)
                            && crate::pacing::present_activity(instance)
                                == crate::pacing::PresentActivity::GameActive
                            && !crate::pacing::soft_fifo_gate_due()
                        {
                            // ATRIUM_NO_SOFT_SPIN=1 — leak A/B: skip Servo spin on
                            // early SoftFifo wakes (MozJS arenas climbed ~11MB/min
                            // with no plugins when spinning every spare redraw).
                            if std::env::var_os("ATRIUM_NO_SOFT_SPIN").is_none() {
                                if let Some(servo) = &self.servo {
                                    servo.spin_event_loop();
                                }
                            }
                            return;
                        }

                        let _ = instance.rendering_context.make_current();
                        let window_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;
                        #[cfg(target_os = "linux")]
                        let pacing_diag = std::env::var_os("ATRIUM_PACING_DIAG").is_some();
                        #[cfg(target_os = "linux")]
                        let frame_t0 = pacing_diag.then(std::time::Instant::now);

                        // Coalesce gutter drag to once per frame before paint/layout flush.
                        if instance.flush_pending_gutter_drag(scale_factor) {
                            instance.recalculate_layout(window_size, scale_factor);
                        }

                        #[cfg(target_os = "linux")]
                        let poll_ms = frame_t0
                            .map(|t| t.elapsed().as_secs_f32() * 1000.0)
                            .unwrap_or(0.0);
                        #[cfg(target_os = "linux")]
                        let servo_t0 = pacing_diag.then(std::time::Instant::now);

                        trace!("Rust: [Lifecycle] RedrawRequested (FBO) for window size {}x{}", window_size.width, window_size.height);

                        // SoftFifo GameActive: if the gate budget is thin, skip Servo/HUD
                        // paint and composite last FBOs so overlay work cannot delay the
                        // game pageflip. needs_repaint stays set for a later catch-up.
                        #[cfg(target_os = "linux")]
                        let defer_overlay_servo =
                            crate::pacing::should_defer_overlay_servo_paint(instance);
                        #[cfg(not(target_os = "linux"))]
                        let defer_overlay_servo = false;
                        #[cfg(target_os = "linux")]
                        if defer_overlay_servo {
                            instance.overlay_dirty = true;
                        }

                        unsafe {
                            // 1. UNIVERSAL PAINT BLOCK
                            // Paint any panes that have new rendered frames ready in Servo
                            for p in instance.panes.values_mut() {
                                if p.is_remote {
                                    continue;
                                }
                                // frame_seq>0 ⇒ we already have an FBO to composite.
                                if defer_overlay_servo && p.needs_repaint && p.frame_seq > 0 {
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
                                            // Re-arm so Servo can notify for the post-resize frame.
                                            p.ready_to_repaint.store(true, Ordering::SeqCst);
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
                                                p.ready_to_repaint.store(true, Ordering::SeqCst);
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
                                            p.frame_seq = p.frame_seq.wrapping_add(1);
                                        }
                                    }
                                    p.needs_repaint = false;
                                    // Re-arm after paint so Servo can notify the next frame
                                    // (NewFrameReady no longer re-arms — avoids user-event livelock).
                                    p.ready_to_repaint.store(true, Ordering::SeqCst);
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
                                        p.frame_seq = p.frame_seq.wrapping_add(1);
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

                            // Always import nest DMA-BUFs before composite. Skipping while
                            // overlay_dirty left the mailbox on a stale sample then jumped
                            // on the next gate (hitch / one-frame rewind feel).
                            #[cfg(target_os = "linux")]
                            if let Some(ref ws) = instance.nested_compositor {
                                let mut server = ws.lock();
                                let dirty_panes = server.pump_frames(&instance.gl);
                                for pid in dirty_panes {
                                    if let Some(p) = instance.panes.get_mut(&pid) {
                                        p.is_dirty = true;
                                        // scene_needs_present keys off frame_seq — without
                                        // this, imported DMA-BUFs update the slot texture but
                                        // the host skips composite/present until a resize.
                                        p.frame_seq = p.frame_seq.wrapping_add(1);
                                    }
                                }
                            }

                            #[cfg(target_os = "linux")]
                            {
                                // ATRIUM_NO_GAME_STATS=1 — leak A/B: skip host stats push.
                                // (ATRIUM_NO_STATS_IPC is JS-only: skips plugin executeScript.)
                                let stats_disabled =
                                    std::env::var_os("ATRIUM_NO_GAME_STATS").is_some();
                                // 250ms matches plugin HUD flush; 100ms host push was
                                // enough traffic to climb anon RSS on its own (~13MB/min).
                                let stats_interval_ms: u64 = std::env::var("ATRIUM_STATS_INTERVAL_MS")
                                    .ok()
                                    .and_then(|v| v.parse().ok())
                                    .unwrap_or(250);
                                let should_send_stats = !stats_disabled
                                    && instance.last_stats_send.map(|t| {
                                        t.elapsed()
                                            >= std::time::Duration::from_millis(stats_interval_ms)
                                    })
                                    .unwrap_or(true);
                                if should_send_stats {
                                    instance.last_stats_send = Some(std::time::Instant::now());
                                    let game_stats = instance.primary_game_pane.as_ref()
                                        .and_then(|id| instance.panes.get(id))
                                        .and_then(|p| p.external_surface_slot.as_ref())
                                        .map(|slot| {
                                            let guard = slot.lock();
                                            let mut sorted = guard.rolling_present_ms.clone();
                                            if sorted.is_empty() {
                                                sorted = guard.rolling_frametimes.clone();
                                            }
                                            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                                            let pct_low = |p: f32| -> Option<f32> {
                                                if sorted.is_empty() {
                                                    return None;
                                                }
                                                let idx = ((1.0 - p) * (sorted.len() as f32 - 1.0)).round() as usize;
                                                let ft = sorted[idx.min(sorted.len() - 1)];
                                                if ft > 0.0 { Some(1000.0 / ft) } else { None }
                                            };
                                            let commits_per_swap = guard.last_commits_per_swap;
                                            (
                                                guard.present_fps.max(guard.current_fps),
                                                guard.last_frame_time_ms,
                                                guard.frame_counter,
                                                guard.width,
                                                guard.height,
                                                pct_low(0.01),
                                                pct_low(0.001),
                                                guard.ingest_fps,
                                                commits_per_swap,
                                            )
                                        });
                                    if let Some((
                                        fps,
                                        frametime_ms,
                                        total_frames,
                                        width,
                                        height,
                                        fps_1_low,
                                        fps_01_low,
                                        fps_ingest,
                                        commits_per_swap,
                                    )) = game_stats
                                    {
                                        // Always push — even fps==0 — so the HUD can show surface size
                                        // once frames exist (frame_counter > 0).
                                        if total_frames > 0 || fps > 0.0 || width > 0 {
                                            let refresh_hz =
                                                instance.refresh_rate_millihertz as f32 / 1000.0;
                                            let activity =
                                                crate::pacing::present_activity(instance);
                                            // Host IPC — not Servo WS. Routing stats through
                                            // the shell renderer (~10Hz) climbed anon RSS
                                            // (~15MB/min) via MozJS; nest-hotkey already
                                            // uses this path. Node listens on
                                            // atrium-forward-stats (same as the old shell hop).
                                            push_host_ipc(
                                                &self.callback,
                                                &uuid,
                                                "atrium-forward-stats",
                                                serde_json::json!({
                                                    "fps": fps,
                                                    "frametime_ms": frametime_ms,
                                                    "total_frames": total_frames,
                                                    "width": width,
                                                    "height": height,
                                                    "fps_1_low": fps_1_low,
                                                    "fps_01_low": fps_01_low,
                                                    "fps_ingest": fps_ingest,
                                                    "commits_per_swap": commits_per_swap,
                                                    "refresh_hz": refresh_hz,
                                                    "pacing_policy": crate::pacing::policy_label(instance.pacing_policy),
                                                    "pacing_activity": crate::pacing::activity_label(activity),
                                                    "soft_fifo": crate::pacing::fixed_software_fifo(instance),
                                                    "hold_cadence": crate::pacing::wants_fixed_hold_cadence(instance),
                                                }),
                                            );
                                        }
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
                                            #[cfg(target_os = "linux")]
                                            if pane.external_wayland_server.is_some() {
                                                continue;
                                            }
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
                                                // Next resize edge must re-seed from a live FBO.
                                                p.ghost_valid = false;
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

                            // Idle parking: skip scene clear/bind + swapchain present when
                            // pane pixels and layout are unchanged. Skipping present is
                            // intentional — presenting would re-arm the Wayland frame
                            // callback and pin the host at display rate with no damage.
                            // (Hidden windows still present-only above to release callbacks.)
                            // Game-driven: present when panes damaged (commit → frame_seq).
                            // Gamescope stall fallback: still force present so steamcompmgr
                            // keeps scanning us out while the game is quiet.
                            #[cfg(target_os = "linux")]
                            let servo_ms = servo_t0
                                .map(|t| t.elapsed().as_secs_f32() * 1000.0)
                                .unwrap_or(0.0);
                            #[cfg(target_os = "linux")]
                            let needs_present = gl::scene_needs_present(
                                instance,
                                window_size,
                                _flip_occurred,
                                &pending_hides,
                            );
                            #[cfg(not(target_os = "linux"))]
                            let needs_present = gl::scene_needs_present(
                                instance,
                                window_size,
                                _flip_occurred,
                                &pending_hides,
                            );

                            if needs_present {
                                let active_w = instance.active_window_size.width.max(1);
                                let active_h = instance.active_window_size.height.max(1);

                                #[cfg(target_os = "linux")]
                                let draw_t0 = pacing_diag.then(std::time::Instant::now);
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

                                #[cfg(target_os = "linux")]
                                if pacing_diag {
                                    crate::pacing::set_frame_stages(crate::pacing::FrameStageMs {
                                        poll_ms,
                                        servo_ms,
                                        draw_ms: draw_t0
                                            .map(|t| t.elapsed().as_secs_f32() * 1000.0)
                                            .unwrap_or(0.0),
                                    });
                                }

                                let live_w = window_size.width.max(1) as i32;
                                let live_h = window_size.height.max(1) as i32;
                                let swapped =
                                    gl::blit_scene_to_window(instance, live_w, live_h, scale_factor);
                                // Throttled gamescope skips must not ack — otherwise Servo/panes
                                // think the frame scanned out and we stall on black forever.
                                if swapped {
                                    gl::ack_presented_scene(instance, window_size);
                                    // Nested clients pace to real host presents only.
                                    #[cfg(target_os = "linux")]
                                    if let Some(ref ws) = instance.nested_compositor {
                                        let now_ms = (std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_millis()
                                            & 0xFFFFFFFF) as u32;
                                        let mut server = ws.lock();
                                        // A new seq retires every older wl_buffer in this
                                        // present, after the fence that covers its last
                                        // sample. Hold presents (nothing awaiting) send a
                                        // frame callback and keep the one pinned buffer.
                                        // Releases are queued before the frame callback so
                                        // a double-buffered client has a free buffer when
                                        // wl_callback.done runs.
                                        let presented = server.take_frames_awaiting_present();
                                        let retire = !presented.is_empty();
                                        if !retire || gl::wait_retired_sample_fence(&instance.gl) {
                                            for (surface_id, seq) in presented {
                                                server.note_frame_presented(surface_id, seq);
                                            }
                                        } else {
                                            server.restore_frames_awaiting_present(presented);
                                        }
                                        server.notify_frame_done(now_ms);
                                        // Gamescope Fixed: keep previous dmabuf through the
                                        // spare vblank. Next commit retires it via the slot.
                                        if !crate::pacing::defer_nest_buffer_release(instance) {
                                            server.release_previous_buffer();
                                        }
                                    }
                                }

                                // Gamescope steamcompmgr only focuses windows with commits
                                // (window_has_commits). Log presents so we can tell "painting
                                // but not scanned out" vs "not painting at all".
                                #[cfg(target_os = "linux")]
                                if swapped && is_gamescope_session() {
                                    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
                                    static PRESENT_COUNT: AtomicU64 = AtomicU64::new(0);
                                    let n = PRESENT_COUNT.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                                    if n <= 5 || n % 120 == 0 {
                                        nest_host_diag(&format!(
                                            "host present #{n} size={}x{} focused={} occluded={}",
                                            live_w,
                                            live_h,
                                            instance.os_focused,
                                            instance.os_occluded
                                        ));
                                    }
                                }

                                // Timing already applied at RedrawRequested entry; keep Vsync
                                // pull-model while focused + viewable, or while awaiting respawn.
                                // Gamescope rarely delivers Focused(true) — still drive nested vsync.
                                if window_is_viewable(instance) {
                                    let presentation_time_ns = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_nanos() as u64)
                                        .unwrap_or(0);
                                    #[cfg(target_os = "linux")]
                                    let force_vsync = is_gamescope_session();
                                    #[cfg(not(target_os = "linux"))]
                                    let force_vsync = false;
                                    for pane_id in &remotes_updated {
                                        if let Some(pane) = instance.panes.get(pane_id) {
                                            if force_vsync
                                                || instance.os_focused
                                                || pane.await_host_composited_frame
                                            {
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
                            } else {
                                trace!("Rust: [Lifecycle] RedrawRequested skip present (no damage)");
                                // Do NOT FrameDone on skip — that free-runs nested clients.
                                #[cfg(target_os = "linux")]
                                if pacing_diag
                                    && crate::pacing::vrr_requires_primary_damage(instance)
                                {
                                    if let Some(line) = crate::pacing::note_vrr_skip_present() {
                                        nest_host_diag(line);
                                    }
                                }
                            }

                            #[cfg(target_os = "linux")]
                            nest_finish_nested_redraw(instance, nested_gen_at_start);
                            #[cfg(target_os = "linux")]
                            {
                                let more_vsync = instance
                                    .nested_compositor
                                    .as_ref()
                                    .is_some_and(|ws| ws.lock().vsync_frames_waiting());
                                if more_vsync {
                                    instance.window.request_redraw();
                                }
                            }

                            // LOCK STEP: clear busy only after the nuclear flip lands.
                            // Always re-run layout dispatch afterward: paint may have just
                            // cleared `needs_repaint`, which was blocking `webview.resize()`
                            // when the transaction opened. Without this, dock toggles against
                            // a continuously animating WebGL pane freeze on a stretched ghost
                            // until some other IPC (e.g. palette) calls recalculate_layout.
                            if instance.committed_layout.is_none() {
                                for pane in instance.panes.values_mut() {
                                    pane.servo_busy = false;
                                }
                            }
                            let window_size = instance.window.inner_size();
                            let scale_factor = instance.window.scale_factor() as f32;
                            instance.recalculate_layout(window_size, scale_factor);

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

                // Never call surfman set_window under gamescope — CreatePlatformWindowSurface
                // returns EGL_NO_SURFACE and asserts. resize_bound_surface is safe.
                instance.rendering_context.resize(size);
                #[cfg(target_os = "linux")]
                if is_gamescope_session() {
                    // No sync present on resize — extra DRI3 swaps outside RedrawRequested
                    // flood the 2-buffer ring. Re-assert interval and wake for a paced redraw.
                    let _ = instance.rendering_context.make_current();
                    nest_set_egl_swap_interval(0);
                    nest_host_diag(&format!(
                        "gamescope resize: bound surface {}x{} (redraw only, no sync present)",
                        size.width, size.height
                    ));
                } else {
                    // Desktop: stretch last scene immediately to avoid resize flash.
                    let live_w = size.width.max(1) as i32;
                    let live_h = size.height.max(1) as i32;
                    gl::blit_scene_to_window(instance, live_w, live_h, scale_factor);
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let live_w = size.width.max(1) as i32;
                    let live_h = size.height.max(1) as i32;
                    gl::blit_scene_to_window(instance, live_w, live_h, scale_factor);
                }

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

                        if overlay_resize_handle_cursor(instance, point, scale_factor) {
                            return;
                        }

                        #[cfg(target_os = "linux")]
                        if instance.game_mode {
                            // While host-grabbed, only DeviceEvent::MouseMotion (relative) feeds
                            // the game. Forwarding absolute CursorMoved too doubles aim / jelly.
                            if instance.host_pointer_grabbed {
                                return;
                            }
                            let game_motion = instance.primary_game_pane.as_ref().and_then(|game_id| {
                                let pane = instance.panes.get(game_id)?;
                                let ws = pane.external_wayland_server.as_ref()?;
                                let (tx, ty) =
                                    nest_pointer_coords(instance, pane, point, scale_factor);
                                Some((game_id.clone(), ws.clone(), tx, ty))
                            });
                            if let Some((game_id, ws, tx, ty)) = game_motion {
                                ws.lock().send_pointer_motion_to_pane(&game_id, tx, ty);
                                // Re-wear game shoes when a host grip cleared nested_cursor_applied
                                // (nest_sync on nested_cursor would restore stale Discord ColResize).
                                if !instance.nested_cursor_applied {
                                    nest_wear_game_pointer_shoes(instance, Some(event_loop));
                                }
                                return;
                            }
                        }

                        #[cfg(target_os = "linux")]
                        if nest_in_overlay_input_mode(instance) {
                            nest_note_input_device(
                                instance,
                                &self.callback,
                                &uuid,
                                "mouse",
                            );
                            nest_route_cursor_moved(
                                instance,
                                event_loop,
                                &uuid,
                                point,
                                scale_factor,
                            );
                            return;
                        }
                        
                        // If mouse is down, route exclusively to the pane that received the Down event
                        if instance.is_mouse_down {
                            if let Some(pane_id) = &instance.last_mouse_down_pane_id {
                                #[cfg(target_os = "linux")]
                                let drag_layout = instance.panes.get(pane_id).map(|p| {
                                    (
                                        nest_pane_hit_rect(instance, p, scale_factor),
                                        p.external_wayland_server.as_ref().map(|ws| {
                                            let (tx, ty) =
                                                nest_pointer_coords(instance, p, point, scale_factor);
                                            (ws.clone(), tx, ty)
                                        }),
                                    )
                                });
                                #[cfg(target_os = "linux")]
                                if let Some((_, Some((ws, tx, ty)))) = drag_layout.as_ref() {
                                    ws.lock().send_pointer_motion_to_pane(pane_id, *tx, *ty);
                                    return;
                                }
                                if let Some(pane) = instance.panes.get_mut(pane_id) {
                                    #[cfg(target_os = "linux")]
                                    let (px, py, pw, ph) = drag_layout
                                        .as_ref()
                                        .map(|(r, _)| *r)
                                        .unwrap_or_else(|| pane.input_physical_rect(scale_factor));
                                    #[cfg(not(target_os = "linux"))]
                                    let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                                    let mut translated_point: Point2D<f32, DevicePixel> =
                                        Point2D::new(point.x - px, point.y - py);
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
                        }

                        // Hit-test resize directions first
                        if instance.frameless {
                            let mut hovered_pane = None;
                            let mut sorted_panes: Vec<_> = instance.panes.values().collect();
                            sorted_panes.sort_by_key(|p| -p.z_index); // Reverse Z for hit-testing

                            for pane in sorted_panes {
                                if !pane_accepts_pointer_input(pane) { continue; }
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
                                let translated_point: Point2D<f32, DevicePixel> =
                                    Point2D::new(point.x - px, point.y - py);

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
                                
                                // Frameless host edge-resize — never when nest owns the window
                                // (gamescope/atrium); that path is what resurfaced the corner cursor.
                                let resize_cursor = if instance.frameless
                                    && !nest_owns_host_window(instance)
                                {
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
                                let mut translated_point: Point2D<f32, DevicePixel> =
                                    Point2D::new(point.x - px, point.y - py);

                                #[cfg(target_os = "linux")]
                                if let Some(ref ws) = pane.external_wayland_server {
                                    let server = ws.lock();
                                    server.send_pointer_motion_to_pane(
                                        &pane.id,
                                        translated_point.x as f64,
                                        translated_point.y as f64,
                                    );
                                } else if pane.is_remote {
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
                                #[cfg(not(target_os = "linux"))]
                                {
                                    let _ = (pw, ph);
                                    pane.webview.notify_input_event(InputEvent::MouseMove(
                                        MouseMoveEvent::new(servo::WebViewPoint::Device(
                                            translated_point,
                                        )),
                                    ));
                                }
                            }
                        } else {
                            let hovered_pane = {
                                #[cfg(target_os = "linux")]
                                {
                                    hit_test_pointer_pane(instance, point, scale_factor, false)
                                }
                                #[cfg(not(target_os = "linux"))]
                                {
                                    let mut sorted_panes: Vec<_> = instance.panes.values().collect();
                                    sorted_panes.sort_by_key(|p| -p.z_index);
                                    sorted_panes.into_iter().find(|pane| {
                                        if !pane_accepts_pointer_input(pane) {
                                            return false;
                                        }
                                        let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                                        let rect = euclid::Rect::new(
                                            euclid::Point2D::new(px, py),
                                            euclid::Size2D::new(pw, ph),
                                        );
                                        rect.contains(point)
                                    })
                                }
                            };
                            if let Some(hover_id) =
                                hovered_pane.map(|p| p.id.clone())
                            {
                                #[cfg(target_os = "linux")]
                                {
                                    let external_motion = instance.panes.get(&hover_id).and_then(|pane| {
                                        pane.external_wayland_server.as_ref().map(|ws| {
                                            let (tx, ty) =
                                                nest_pointer_coords(instance, pane, point, scale_factor);
                                            (ws.clone(), tx, ty)
                                        })
                                    });
                                    if let Some((ws, tx, ty)) = external_motion {
                                        ws.lock().send_pointer_motion_to_pane(&hover_id, tx, ty);
                                        instance.hover_pane_id = Some(hover_id.clone());
                                        instance.active_pane_id = hover_id;
                                        return;
                                    }
                                }
                                if let Some(pane) = instance.panes.get(&hover_id) {
                                    let (px, py, pw, ph) =
                                        nest_pane_hit_rect(instance, pane, scale_factor);
                                    let mut translated_point =
                                        Point2D::new(point.x - px, point.y - py);
                                    if pane.is_remote {
                                        if let Some(ref consumer) = pane.remote_consumer {
                                            if consumer.tex_width > 0
                                                && consumer.tex_height > 0
                                                && pw > 0.0
                                                && ph > 0.0
                                            {
                                                translated_point.x *=
                                                    consumer.tex_width as f32 / pw;
                                                translated_point.y *=
                                                    consumer.tex_height as f32 / ph;
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
                                            })
                                            .to_string();
                                            forward_remote_input(fd, &msg);
                                        }
                                    } else {
                                        pane.webview.notify_input_event(InputEvent::MouseMove(
                                            MouseMoveEvent::new(servo::WebViewPoint::Device(
                                                translated_point,
                                            )),
                                        ));
                                    }
                                }
                                instance.hover_pane_id = Some(hover_id);
                            }
                        }
                    },
                    WindowEvent::MouseInput { state, button, .. } => {
                        let is_pressed = state == winit::event::ElementState::Pressed;
                        instance.is_mouse_down = is_pressed;
                        let scale_factor = instance.window.scale_factor() as f32;

                        #[cfg(target_os = "linux")]
                        if instance.game_mode {
                            if let Some(ref game_id) = instance.primary_game_pane {
                                if let Some(pane) = instance.panes.get(game_id) {
                                    if let Some(ref ws) = pane.external_wayland_server {
                                        let (x, y) = nest_pointer_coords(
                                            instance,
                                            pane,
                                            instance.last_mouse_pos,
                                            scale_factor,
                                        );
                                        ws.lock().send_pointer_button_to_pane(
                                            game_id,
                                            x,
                                            y,
                                            winit_mouse_button_evdev(button),
                                            is_pressed,
                                        );
                                        return;
                                    }
                                }
                            }
                        }

                        #[cfg(target_os = "linux")]
                        if nest_in_overlay_input_mode(instance) {
                            if overlay_resize_handle_button(
                                instance,
                                instance.last_mouse_pos,
                                scale_factor,
                                is_pressed,
                                button,
                            ) {
                                return;
                            }
                            nest_route_mouse_button(
                                instance,
                                &uuid,
                                button,
                                is_pressed,
                                scale_factor,
                            );
                            return;
                        }

                        if overlay_resize_handle_button(
                            instance,
                            instance.last_mouse_pos,
                            scale_factor,
                            is_pressed,
                            button,
                        ) {
                            return;
                        }

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
                                    if !pane_accepts_pointer_input(p) { return false; }
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
                            let found_id = {
                                #[cfg(target_os = "linux")]
                                {
                                    hit_test_pointer_pane(
                                        instance,
                                        instance.last_mouse_pos,
                                        scale_factor,
                                        false,
                                    )
                                    .map(|p| p.id.clone())
                                }
                                #[cfg(not(target_os = "linux"))]
                                {
                                    let mut sorted_panes: Vec<_> = instance.panes.values().collect();
                                    sorted_panes.sort_by_key(|p| -p.z_index);
                                    sorted_panes.into_iter().find_map(|pane| {
                                        if !pane_accepts_pointer_input(pane) {
                                            return None;
                                        }
                                        let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                                        let rect = euclid::Rect::new(
                                            euclid::Point2D::new(px, py),
                                            euclid::Size2D::new(pw, ph),
                                        );
                                        if rect.contains(instance.last_mouse_pos) {
                                            Some(pane.id.clone())
                                        } else {
                                            None
                                        }
                                    })
                                }
                            };
                            if is_pressed {
                                instance.last_mouse_down_pane_id = found_id.clone();
                            }
                            found_id
                        };

                        if let Some(pane_id) = target_pane_id {
                            let nest_owns = nest_owns_host_window(instance);
                            #[cfg(target_os = "linux")]
                            let pointer_layout = instance.panes.get(&pane_id).map(|p| {
                                (
                                    nest_pane_hit_rect(instance, p, scale_factor),
                                    p.external_wayland_server.as_ref().map(|ws| {
                                        let (x, y) = nest_pointer_coords(
                                            instance,
                                            p,
                                            instance.last_mouse_pos,
                                            scale_factor,
                                        );
                                        (ws.clone(), x, y)
                                    }),
                                )
                            });
                            if let Some(pane) = instance.panes.get_mut(&pane_id) {
                                    // Unified physical coordinate translation
                                    #[cfg(target_os = "linux")]
                                    let (px, py, pw, ph) = pointer_layout
                                        .as_ref()
                                        .map(|(r, _)| *r)
                                        .unwrap_or_else(|| pane.input_physical_rect(scale_factor));
                                    #[cfg(not(target_os = "linux"))]
                                    let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                                    let mut translated_point: Point2D<f32, DevicePixel> =
                                        Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
                                    if pane.is_remote {
                                        if let Some(ref consumer) = pane.remote_consumer {
                                            if consumer.tex_width > 0 && consumer.tex_height > 0 && pw > 0.0 && ph > 0.0 {
                                                translated_point.x *= consumer.tex_width as f32 / pw;
                                                translated_point.y *= consumer.tex_height as f32 / ph;
                                            }
                                        }
                                    }

                                    if is_pressed
                                        && button == winit::event::MouseButton::Left
                                        && instance.frameless
                                        && !nest_owns
                                    {
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
                                            let translated_point: Point2D<f32, DevicePixel> =
                                                Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
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
                                            let translated_point: Point2D<f32, DevicePixel> =
                                                Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
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

                                #[cfg(target_os = "linux")]
                                if let Some((ws, x, y)) =
                                    pointer_layout.as_ref().and_then(|(_, ext)| ext.clone())
                                {
                                    ws.lock().send_pointer_button_to_pane(
                                        &pane_id,
                                        x,
                                        y,
                                        winit_mouse_button_evdev(button),
                                        is_pressed,
                                    );
                                    return;
                                } else if pane.is_remote {
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
                                #[cfg(not(target_os = "linux"))]
                                {
                                    pane.webview.notify_input_event(InputEvent::MouseButton(
                                        MouseButtonEvent::new(
                                            action,
                                            servo_button,
                                            servo::WebViewPoint::Device(translated_point),
                                        ),
                                    ));
                                }
                            }
                        }
                    },
                    WindowEvent::MouseWheel { delta, .. } => {
                        let scale_factor = instance.window.scale_factor() as f32;
                        // Servo path: ~100 CSS px per wheel notch as device pixels.
                        // Nest path must NOT reuse these — Wayland wheel clients treat
                        // classic axis as ~10 units/detent (then ×120), so feeding
                        // 100×DPR made Discord/Electron jump ~10–20× too far.
                        let line_scroll = 100.0 * scale_factor as f64;
                        let (x, y) = match delta {
                            MouseScrollDelta::LineDelta(lx, ly) => {
                                (lx as f64 * line_scroll, ly as f64 * line_scroll)
                            }
                            MouseScrollDelta::PixelDelta(pos) => (pos.x, pos.y),
                        };
                        let wheel_delta = WheelDelta {
                            x,
                            y,
                            z: 0.0,
                            mode: WheelMode::DeltaPixel,
                        };
                        // Nest: smithay/winit convention — LineDelta → v120 (±120/detent)
                        // + legacy value (±10/detent); PixelDelta → Continuous pixels.
                        #[cfg(target_os = "linux")]
                        let (nest_h, nest_v, nest_v120_h, nest_v120_v, nest_wheel) = match delta {
                            MouseScrollDelta::LineDelta(lx, ly) => {
                                let lx = lx as f64;
                                let ly = ly as f64;
                                (
                                    -lx * 10.0,
                                    -ly * 10.0,
                                    Some((-lx * 120.0).round() as i32),
                                    Some((-ly * 120.0).round() as i32),
                                    true,
                                )
                            }
                            MouseScrollDelta::PixelDelta(pos) => {
                                (-pos.x, -pos.y, None, None, false)
                            }
                        };

                        #[cfg(target_os = "linux")]
                        if instance.game_mode {
                            if let Some(ref game_id) = instance.primary_game_pane {
                                if let Some(pane) = instance.panes.get(game_id) {
                                    if let Some(ref ws) = pane.external_wayland_server {
                                        let (mx, my) = nest_pointer_coords(
                                            instance,
                                            pane,
                                            instance.last_mouse_pos,
                                            scale_factor,
                                        );
                                        ws.lock().send_pointer_axis_to_pane(
                                            game_id,
                                            mx,
                                            my,
                                            nest_h,
                                            nest_v,
                                            nest_v120_h,
                                            nest_v120_v,
                                            nest_wheel,
                                        );
                                        return;
                                    }
                                }
                            }
                        }

                        #[cfg(target_os = "linux")]
                        if nest_in_overlay_input_mode(instance) {
                            if let Some(target_id) = hit_test_pointer_pane(
                                instance,
                                instance.last_mouse_pos,
                                scale_factor,
                                true,
                            )
                            .map(|p| p.id.clone())
                            {
                                if let Some(pane) = instance.panes.get(&target_id) {
                                    if let Some(ref ws) = pane.external_wayland_server {
                                        let (mx, my) = nest_pointer_coords(
                                            instance,
                                            pane,
                                            instance.last_mouse_pos,
                                            scale_factor,
                                        );
                                        ws.lock().send_pointer_axis_to_pane(
                                            &target_id,
                                            mx,
                                            my,
                                            nest_h,
                                            nest_v,
                                            nest_v120_h,
                                            nest_v120_v,
                                            nest_wheel,
                                        );
                                        instance.window.request_redraw();
                                        return;
                                    }
                                }
                            }
                        }

                        if let Some(_) = &instance.active_select {
                            let overlay_pane = instance.panes.get("__lotus_select_overlay");
                            let is_inside_overlay = overlay_pane.map(|p| {
                                if !pane_accepts_pointer_input(p) { return false; }
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

                        let wheel_pane = {
                            #[cfg(target_os = "linux")]
                            {
                                hit_test_pointer_pane(
                                    instance,
                                    instance.last_mouse_pos,
                                    scale_factor,
                                    false,
                                )
                                .map(|p| p.id.clone())
                            }
                            #[cfg(not(target_os = "linux"))]
                            {
                                let mut sorted_panes: Vec<_> = instance.panes.values().collect();
                                sorted_panes.sort_by_key(|p| -p.z_index);
                                sorted_panes.into_iter().find_map(|pane| {
                                    if !pane_accepts_pointer_input(pane) {
                                        return None;
                                    }
                                    let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                                    let rect = euclid::Rect::new(
                                        euclid::Point2D::new(px, py),
                                        euclid::Size2D::new(pw, ph),
                                    );
                                    if rect.contains(instance.last_mouse_pos) {
                                        Some(pane.id.clone())
                                    } else {
                                        None
                                    }
                                })
                            }
                        };
                        if let Some(pane_id) = wheel_pane {
                            #[cfg(target_os = "linux")]
                            let wheel_layout = instance.panes.get(&pane_id).map(|p| {
                                (
                                    nest_pane_hit_rect(instance, p, scale_factor),
                                    p.external_wayland_server.as_ref().map(|ws| {
                                        let (mx, my) = nest_pointer_coords(
                                            instance,
                                            p,
                                            instance.last_mouse_pos,
                                            scale_factor,
                                        );
                                        (ws.clone(), mx, my)
                                    }),
                                )
                            });
                            if let Some(pane) = instance.panes.get_mut(&pane_id) {
                                #[cfg(target_os = "linux")]
                                let (px, py, pw, ph) = wheel_layout
                                    .as_ref()
                                    .map(|(r, _)| *r)
                                    .unwrap_or_else(|| pane.input_physical_rect(scale_factor));
                                #[cfg(not(target_os = "linux"))]
                                let (px, py, pw, ph) = pane.input_physical_rect(scale_factor);
                                let mut translated_point: Point2D<f32, DevicePixel> =
                                    Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
                                #[cfg(target_os = "linux")]
                                if let Some((ws, mx, my)) =
                                    wheel_layout.as_ref().and_then(|(_, ext)| ext.clone())
                                {
                                    ws.lock().send_pointer_axis_to_pane(
                                        &pane_id,
                                        mx,
                                        my,
                                        nest_h,
                                        nest_v,
                                        nest_v120_h,
                                        nest_v120_v,
                                        nest_wheel,
                                    );
                                    instance.window.request_redraw();
                                } else if pane.is_remote {
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
                                #[cfg(not(target_os = "linux"))]
                                {
                                    let _ = (pw, ph);
                                    pane.webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(
                                        wheel_delta,
                                        servo::WebViewPoint::Device(translated_point),
                                    )));
                                    pane.needs_repaint = true;
                                }
                                instance.window.request_redraw();
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
                            // Under gamescope, Occluded is unreliable — keep painting.
                            #[cfg(target_os = "linux")]
                            let keep_painting = is_gamescope_session() || !occluded;
                            #[cfg(not(target_os = "linux"))]
                            let keep_painting = !occluded;
                            if keep_painting {
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
                            // Gamescope often never delivers Focused(true) to X11 clients.
                            #[cfg(target_os = "linux")]
                            let keep_painting = is_gamescope_session() || focused;
                            #[cfg(not(target_os = "linux"))]
                            let keep_painting = focused;
                            if keep_painting {
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
                            #[cfg(target_os = "linux")]
                            nest_clear_nested_cursor(instance);
                            instance.window.set_cursor(winit::window::CursorIcon::Default);
                        }
                    },
                    WindowEvent::KeyboardInput {
                        event,
                        is_synthetic,
                        ..
                    } => {
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

                        // Focus-sync synthetics (winit) must not reach clients — they
                        // re-press already-held keys and look like duplicates.
                        if is_synthetic {
                            return;
                        }

                        #[cfg(target_os = "linux")]
                        {
                            // EC1: Released for keys whose Pressed was host-consumed — never forward.
                            if event.state == winit::event::ElementState::Released
                                && instance.host_hotkey_pressed.remove(&event.physical_key)
                            {
                                return;
                            }

                            // EC3: any window with a non-empty host hotkey table.
                            // Match PhysicalKey::Code + Ctrl|Alt|Shift|Super (ignore lock keys).
                            if !instance.host_hotkeys.is_empty()
                                && event.state == winit::event::ElementState::Pressed
                                && !event.repeat
                            {
                                if let winit::keyboard::PhysicalKey::Code(code) = event.physical_key
                                {
                                    let pressed_mods =
                                        crate::window::host_hotkey_mod_mask(instance.modifiers);
                                    let matched_id = instance
                                        .host_hotkeys
                                        .values()
                                        .find(|b| {
                                            b.key == code && b.modifiers == pressed_mods
                                        })
                                        .map(|b| b.id.clone());
                                    if let Some(id) = matched_id {
                                        // Game input: only the mode toggle may steal
                                        // keys from the nest. Overlay binds (Discord,
                                        // HUD, …) must fall through so F9/F10 reach
                                        // the game. Atrium only registers toggle-mode
                                        // while game_mode; this is the safety net.
                                        if instance.game_mode && id != "toggle-mode" {
                                            // unmatched — forward below
                                        } else {
                                            instance.host_hotkey_pressed.insert(event.physical_key);
                                            push_host_ipc(
                                                &self.callback,
                                                &uuid,
                                                "nest-hotkey",
                                                serde_json::json!({ "id": id }),
                                            );
                                            return;
                                        }
                                    }
                                }
                            }
                        }

                        #[cfg(target_os = "linux")]
                        if instance.game_mode {
                            // Wayland clients repeat locally via wl_keyboard.repeat_info.
                            // Forwarding winit repeats → duplicated characters.
                            if event.repeat {
                                return;
                            }
                            if let Some(ref game_id) = instance.primary_game_pane {
                                if let Some(pane) = instance.panes.get(game_id) {
                                    if let Some(ref ws) = pane.external_wayland_server {
                                        use winit::platform::scancode::PhysicalKeyExtScancode;
                                        if let Some(scancode) = event.physical_key.to_scancode() {
                                            let is_pressed = event.state == winit::event::ElementState::Pressed;
                                            let server = ws.lock();
                                            server.send_keyboard_to_pane(
                                                game_id,
                                                scancode + 8,
                                                is_pressed,
                                            );
                                        }
                                        return;
                                    }
                                }
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
                        
                        // Overlay / normal lotus: sticky click focus only. Never follow
                        // hover onto the game (that was sending keys into the game whenever
                        // the cursor left Discord onto the wallpaper).
                        #[cfg(target_os = "linux")]
                        if nest_in_overlay_input_mode(instance) {
                            nest_note_input_device(
                                instance,
                                &self.callback,
                                &uuid,
                                "keyboard",
                            );
                        }
                        #[cfg(target_os = "linux")]
                        let keyboard_pane_id = {
                            let id = instance.active_pane_id.clone();
                            if id.is_empty()
                                || (nest_in_overlay_input_mode(instance)
                                    && nest_is_primary_game(instance, &id))
                            {
                                None
                            } else {
                                Some(id)
                            }
                        };
                        #[cfg(not(target_os = "linux"))]
                        let keyboard_pane_id = Some(instance.active_pane_id.clone());

                        if let Some(ref keyboard_pane_id) = keyboard_pane_id {
                            if let Some(pane) = instance.panes.get(keyboard_pane_id) {
                                #[cfg(target_os = "linux")]
                                if let Some(ref ws) = pane.external_wayland_server {
                                    // Same as game mode: client-side repeat only.
                                    if event.repeat {
                                        return;
                                    }
                                    use winit::platform::scancode::PhysicalKeyExtScancode;
                                    if let Some(scancode) = event.physical_key.to_scancode() {
                                        let is_pressed =
                                            event.state == winit::event::ElementState::Pressed;
                                        let server = ws.lock();
                                        server.send_keyboard_to_pane(
                                            &pane.id,
                                            scancode + 8,
                                            is_pressed,
                                        );
                                    }
                                    return;
                                }
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
                                        })
                                        .to_string();
                                        forward_remote_input(fd, &msg);
                                    }
                                } else {
                                    pane.webview
                                        .notify_input_event(InputEvent::Keyboard(keyboard_event));
                                }
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
