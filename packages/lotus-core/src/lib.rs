use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::{env, fs, path::PathBuf, process::Command};
use std::collections::HashMap;
use std::time::Instant;

use once_cell::sync::OnceCell;

use napi_derive::napi;
use napi::threadsafe_function::{ThreadsafeFunction, ErrorStrategy, ThreadsafeFunctionCallMode};
use serde::{Deserialize, Serialize};
use serde_json;
use uuid::Uuid;
use log::{info, error, debug, trace};

mod window_state;
mod platform;

use window_state::WindowStateManager;

use winit::event::{WindowEvent, MouseScrollDelta};
use winit::event_loop::{EventLoopProxy, ActiveEventLoop};
use winit::application::ApplicationHandler;
use winit::window::{Window, WindowId, CursorIcon};
use winit::dpi::PhysicalSize as WinitPhysicalSize;
use winit::raw_window_handle::{HasWindowHandle, HasDisplayHandle};
use dpi::PhysicalSize as ServoPhysicalSize;
#[cfg(target_os = "macos")]
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
#[cfg(target_os = "windows")]
use window_vibrancy::{apply_blur, apply_mica};
#[cfg(target_os = "linux")]
use winit::platform::x11::WindowAttributesExtX11;
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;

// Servo Imports
use servo::{
    ServoBuilder, WebViewDelegate, 
    WebViewBuilder, WindowRenderingContext, RenderingContext,
    resources::{self, Resource},
    InputEvent, MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    WheelEvent, WheelDelta, WheelMode,
    LoadStatus,
    ConsoleLogLevel,
    WebResourceLoad,
    UserContentManager, UserScript,

};
use euclid::{Point2D, Scale};
use servo::{DeviceIndependentPixel, DevicePixel};
use servo::WebResourceResponse;
use http::header::{HeaderMap, HeaderValue, CONTENT_TYPE};

use http::StatusCode;
use dark_light;


// IPC Message structure - Removed! process raw bytes.

// Global event loop proxy - initialized once
static EVENT_LOOP_PROXY: OnceCell<EventLoopProxy<EngineCommand>> = OnceCell::new();

// Global app state (thread-safe metadata only, no Rc types)
static APP_STATE: OnceCell<Arc<Mutex<AppState>>> = OnceCell::new();

#[cfg(target_os = "linux")]
fn detect_linux_theme_robust() -> dark_light::Mode {
    // 1. Try standard crate
    let mode = dark_light::detect();
    if mode == dark_light::Mode::Dark {
        return mode;
    }

    // 2. Try gsettings for color-scheme (Modern GNOME)
    // gsettings get org.gnome.desktop.interface color-scheme
    if let Ok(output) = Command::new("gsettings")
        .args(&["get", "org.gnome.desktop.interface", "color-scheme"])
        .output() 
    {
        let stdout = String::from_utf8_lossy(&output.stdout).to_lowercase();
        // Output is usually "'prefer-dark'\n"
        if stdout.contains("prefer-dark") {
            return dark_light::Mode::Dark;
        }
    }

    // 3. Try gsettings for gtk-theme (Legacy / Fallback)
    // gsettings get org.gnome.desktop.interface gtk-theme
    if let Ok(output) = Command::new("gsettings")
        .args(&["get", "org.gnome.desktop.interface", "gtk-theme"])
        .output() 
    {
        let stdout = String::from_utf8_lossy(&output.stdout).to_lowercase();
        // Check for "-dark" suffix
        if stdout.contains("-dark") || stdout.contains(":dark") {
            return dark_light::Mode::Dark;
        }
    }

    // Default to what we found initially (Light/Default)
    mode
}

struct AppState {
    window_metadata: HashMap<String, WindowMetadata>,
    window_states: WindowStateManager,
    ipc_server_port: u16,
    ipc_server_token: String,
    msgpackr_source: String,
    profiling: bool,
    start_time: Instant,
    window_start_times: HashMap<String, Instant>,
}

// IPC bootstrap script injected into every page
const IPC_BOOTSTRAP_BASE: &str = r#"
window.lotus = {
    handlers: {},
    // Buffer for batching
    _batch: [],
    _batchTimeout: null,
    port: null, // Will be set by init script
    token: null, // Will be set by init script
    id: null,    // Will be set by init script

    send: (channel, data) => {
        const port = window.lotus.port;
        const token = window.lotus.token;
        const windowId = window.lotus.id;
        if (!port) {
            console.error("Lotus IPC port not initialized");
            return;
        }

        // Universal API: Check for binary
        if (data instanceof Blob || data instanceof ArrayBuffer || ArrayBuffer.isView(data)) {
            // Send binary directly via POST to localhost server
            fetch(`http://127.0.0.1:${port}/ipc/${encodeURIComponent(channel)}`, {
                method: 'POST',
                headers: {
                    'X-Lotus-Auth': token,
                    'X-Lotus-Window-Id': windowId
                },
                body: data
            }).catch(e => console.error("IPC Binary Post Failed", e));
        } else {
            // JSON/Object -> Batch -> MsgPack
            window.lotus._batch.push([channel, data]);
            
            if (!window.lotus._batchTimeout) {
                queueMicrotask(() => {
                    const batch = window.lotus._batch;
                    window.lotus._batch = [];
                    window.lotus._batchTimeout = null;
                    
                    if (window.msgpackr) {
                        try {
                            const packed = window.msgpackr.pack(batch);
                            fetch(`http://127.0.0.1:${port}/batch`, {
                                method: 'POST',
                                headers: {
                                    'X-Lotus-Auth': token,
                                    'X-Lotus-Window-Id': windowId
                                },
                                body: packed
                            }).catch(e => console.error("IPC Batch Post Failed", e));
                        } catch (e) {
                            console.error("Failed to pack batch", e);
                        }
                    } else {
                        console.error("msgpackr not loaded");
                    }
                });
                window.lotus._batchTimeout = true; // Any truthy value
            }
        }
    },
    on: (channel, handler) => {
        if (!window.lotus.handlers[channel]) window.lotus.handlers[channel] = [];
        window.lotus.handlers[channel].push(handler);
    },
    emit: (channel, data) => {
        (window.lotus.handlers[channel] || []).forEach(h => h(data));
    }
};
"#;

const DRAG_REGION_SCRIPT: &str = r#"
(function() {
    let updateTimeout = null;
    function updateDragRegions() {
        if (updateTimeout) clearTimeout(updateTimeout);
        updateTimeout = setTimeout(() => {
            const dragElements = document.querySelectorAll('[style*="-webkit-app-region: drag"], [data-lotus-drag="true"]');
            const noDragElements = document.querySelectorAll('[style*="-webkit-app-region: no-drag"], [data-lotus-drag="false"]');
            
            const dragRects = [];
            const noDragRects = [];
            const dpr = window.devicePixelRatio || 1;
            
            dragElements.forEach(el => {
                const rect = el.getBoundingClientRect();
                dragRects.push({
                    x: rect.x * dpr,
                    y: rect.y * dpr,
                    width: rect.width * dpr,
                    height: rect.height * dpr
                });
            });
            
            noDragElements.forEach(el => {
                const rect = el.getBoundingClientRect();
                noDragRects.push({
                    x: rect.x * dpr,
                    y: rect.y * dpr,
                    width: rect.width * dpr,
                    height: rect.height * dpr
                });
            });
            
            if (window.lotus && window.lotus.send) {
                // console.log("[DRAG] Sending rects to Rust:", { dragRects, noDragRects });
                window.lotus.send('lotus:set-drag-regions', { drag: dragRects, noDrag: noDragRects });
            }
        }, 50); // Debounce for 50ms
    }

    function initObservers() {
        if (!document.body) return;
        const observer = new MutationObserver(updateDragRegions);
        observer.observe(document.body, { childList: true, subtree: true, attributes: true, attributeFilter: ['style', 'data-lotus-drag'] });

        const resizeObserver = new ResizeObserver(updateDragRegions);
        resizeObserver.observe(document.body);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', () => {
            updateDragRegions();
            initObservers();
        });
    } else {
        updateDragRegions();
        initObservers();
    }
})();
"#;

struct WindowMetadata {
    root_path: Option<PathBuf>,
}

struct WindowInstance {
    webview: servo::WebView,
    rendering_context: Rc<WindowRenderingContext>,
    window: Arc<Window>,
    last_mouse_pos: Point2D<f32, servo::DevicePixel>,
    is_mouse_down: bool,
    frameless: bool,
    drag_regions: Vec<euclid::Rect<f32, servo::DevicePixel>>,
    no_drag_regions: Vec<euclid::Rect<f32, servo::DevicePixel>>,
    in_resize_border: bool, // tracks whether cursor is currently in the resize border zone
}


// Window options for creation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[napi(object)]
pub struct WindowOptions {
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
    pub fullscreen: bool,
    pub title: String,
    pub resizable: bool,
    pub frameless: bool,
    pub always_on_top: bool,
    pub initial_url: Option<String>,
    pub restore_state: bool,
    pub root: Option<String>,
    pub transparent: bool,
    pub visible: bool,
    pub id: Option<String>,
    pub wm_class: Option<String>,
}

impl Default for WindowOptions {
    fn default() -> Self {
        WindowOptions {
            width: 1024,
            height: 768,
            maximized: false,
            fullscreen: false,
            title: "Lotus App".to_string(),
            resizable: true,
            frameless: false,
            always_on_top: false,
            initial_url: None,
            restore_state: true,
            root: None,
            transparent: false,
            visible: true,
            id: None,
            wm_class: None,
        }
    }
}

// Commands sent from Node.js -> Rust
#[derive(Debug)]
pub enum EngineCommand {
    Wake,
    // App-level commands
    CreateWindow(WindowOptions, String), // options, window_id
    Quit,
    
    // Window-specific commands (all take window ID)
    LoadUrl(String, String), // window_id, url
    SendToRenderer(String, String, serde_json::Value), // window_id, channel, data
    IpcMessage(String, Vec<u8>), // window_id, raw_bytes
    Resize(String, winit::dpi::PhysicalSize<u32>), // window_id, size
    SetPosition(String, winit::dpi::PhysicalPosition<i32>), // window_id, position
    SetAlwaysOnTop(String, bool), // window_id, flag
    RequestAttention(String), // window_id
    SetTitle(String, String), // window_id, title
    CloseWindow(String), // window_id
    SetDecorations(String, bool), // window_id, decorations
    ExecuteScript(String, String), // window_id, script
    ShowWindow(String), // window_id
    HideWindow(String), // window_id
    UpdateDragRegions(String, Vec<euclid::Rect<f32, servo::DevicePixel>>, Vec<euclid::Rect<f32, servo::DevicePixel>>), // window_id, drag_regions, no_drag_regions
    MinimizeWindow(String), // window_id
    UnminimizeWindow(String), // window_id
    MaximizeWindow(String), // window_id
    UnmaximizeWindow(String), // window_id
    FocusWindow(String), // window_id
}


#[napi]
pub struct WindowHandle {
    id: String,
}

#[napi]
impl WindowHandle {
    #[napi]
    pub fn get_id(&self) -> String {
        self.id.clone()
    }

    #[napi]
    pub fn load_url(&self, url: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::LoadUrl(self.id.clone(), url));
        }
    }

    #[napi]
    pub fn send_to_renderer(&self, channel: String, data: String) -> napi::Result<()> {
        let json_data: serde_json::Value = serde_json::from_str(&data)
            .map_err(|e| napi::Error::from_reason(format!("Invalid JSON: {}", e)))?;
        
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            proxy.send_event(EngineCommand::SendToRenderer(self.id.clone(), channel, json_data))
                .map_err(|e| napi::Error::from_reason(format!("Failed to send event: {}", e)))?;
        }
        
        Ok(())
    }

    #[napi]
    pub fn resize(&self, width: u32, height: u32) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::Resize(
                self.id.clone(),
                winit::dpi::PhysicalSize::new(width, height)
            ));
        }
    }

    #[napi]
    pub fn set_position(&self, x: i32, y: i32) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetPosition(
                self.id.clone(),
                winit::dpi::PhysicalPosition::new(x, y)
            ));
        }
    }

    #[napi]
    pub fn show(&self) {
         if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::ShowWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn hide(&self) {
         if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::HideWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn set_always_on_top(&self, always_on_top: bool) {

        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetAlwaysOnTop(self.id.clone(), always_on_top));
        }
    }

    #[napi]
    pub fn request_attention(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::RequestAttention(self.id.clone()));
        }
    }

    #[napi]
    pub fn set_title(&self, title: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetTitle(self.id.clone(), title));
        }
    }

    #[napi]
    pub fn set_decorations(&self, decorations: bool) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetDecorations(self.id.clone(), decorations));
        }
    }

    #[napi]
    pub fn close(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::CloseWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn update_drag_regions(&self, rects_json: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&rects_json) {
                let mut drag_regions = Vec::new();
                let mut no_drag_regions = Vec::new();
                
                if let Some(drag_arr) = data.get("drag").and_then(|v| v.as_array()) {
                    for r in drag_arr {
                        if let (Some(x), Some(y), Some(w), Some(h)) = (
                            r.get("x").and_then(|v| v.as_f64()),
                            r.get("y").and_then(|v| v.as_f64()),
                            r.get("width").and_then(|v| v.as_f64()),
                            r.get("height").and_then(|v| v.as_f64())
                        ) {
                            drag_regions.push(euclid::Rect::new(
                                euclid::Point2D::new(x as f32, y as f32),
                                euclid::Size2D::new(w as f32, h as f32)
                            ));
                        }
                    }
                }
                
                if let Some(no_drag_arr) = data.get("noDrag").and_then(|v| v.as_array()) {
                    for r in no_drag_arr {
                        if let (Some(x), Some(y), Some(w), Some(h)) = (
                            r.get("x").and_then(|v| v.as_f64()),
                            r.get("y").and_then(|v| v.as_f64()),
                            r.get("width").and_then(|v| v.as_f64()),
                            r.get("height").and_then(|v| v.as_f64())
                        ) {
                            no_drag_regions.push(euclid::Rect::new(
                                euclid::Point2D::new(x as f32, y as f32),
                                euclid::Size2D::new(w as f32, h as f32)
                            ));
                        }
                    }
                }
                
                info!("Rust: Updated drag regions for window {}: drag: {}, no_drag: {}", self.id, drag_regions.len(), no_drag_regions.len());
                let _ = proxy.send_event(EngineCommand::UpdateDragRegions(self.id.clone(), drag_regions, no_drag_regions));
            }
        }
    }

    #[napi]
    pub fn execute_script(&self, script: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::ExecuteScript(self.id.clone(), script));
        }
    }

    #[napi]
    pub fn minimize(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::MinimizeWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn unminimize(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::UnminimizeWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn maximize(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::MaximizeWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn unmaximize(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::UnmaximizeWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn focus(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::FocusWindow(self.id.clone()));
        }
    }
}




// ------------------------------------------------------------------
// RESOURCES IMPLEMENTATION
// ------------------------------------------------------------------

struct ResourceReader;

impl resources::ResourceReaderMethods for ResourceReader {
    fn read(&self, file: Resource) -> Vec<u8> {
        let mut path = resources_dir_path();
        path.push(file.filename());
        // debug!("Rust: Reading resource: {:?}", path); 
        match fs::read(&path) {
            Ok(data) => data,
            Err(e) => {
                eprintln!("Rust Warning: Missing resource: {:?} ({})", path, e);
                Vec::new()
            }
        }
    }
    fn sandbox_access_files_dirs(&self) -> Vec<PathBuf> {
        vec![resources_dir_path()]
    }
    fn sandbox_access_files(&self) -> Vec<PathBuf> {
        vec![]
    }
}

fn resources_dir_path() -> PathBuf {
    // Try ./resources relative to current working directory first
    let mut path = env::current_dir().unwrap();
    path.push("resources");
    if path.exists() {
        return path;
    }
    // Fallback?
    path.pop();
    path
}

fn init_resources() {
    resources::set(Box::new(ResourceReader));
}

// ------------------------------------------------------------------
// DELEGATE IMPLEMENTATIONS
// ------------------------------------------------------------------


struct LotusWebViewDelegate {
    window: Arc<Window>,
    window_id: String,
}

impl WebViewDelegate for LotusWebViewDelegate {
    fn notify_load_status_changed(&self, _webview: servo::WebView, status: LoadStatus) {
        let (profiling, window_start) = if let Some(state) = APP_STATE.get() {
            if let Ok(s) = state.lock() {
                (s.profiling, s.window_start_times.get(&self.window_id).cloned())
            } else {
                (false, None)
            }
        } else {
            (false, None)
        };

        if profiling {
            if let Some(start) = window_start {
                let elapsed = start.elapsed();
                eprintln!("[PROFILE] Window {} status {:?} reached in {:?}", self.window_id, status, elapsed);
            }
        }

        info!("Rust: LoadStatus changed to {:?} for {}", status, self.window_id);
        
        let status_str = match status {
            LoadStatus::Started => "started",
            LoadStatus::HeadParsed => "head-parsed",
            LoadStatus::Complete => "complete",
        };

        if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
            "event": "load-status",
            "window_id": self.window_id,
            "status": status_str
        })) {
            if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                let _ = proxy.send_event(EngineCommand::IpcMessage(self.window_id.clone(), msg));
            }
        }
    }
    
    fn notify_new_frame_ready(&self, _webview: servo::WebView) {
        trace!("Rust: NewFrameReady - Requesting Redraw");
        self.window.request_redraw();
        
        if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
            "event": "frame-ready",
            "window_id": self.window_id
        })) {
             if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                let _ = proxy.send_event(EngineCommand::IpcMessage(self.window_id.clone(), msg));
            }
        }
    }
    
    fn notify_page_title_changed(&self, _webview: servo::WebView, title: Option<String>) {
         info!("Rust: Title changed to {:?}", title);
    }

    fn show_console_message(&self, _webview: servo::WebView, _level: ConsoleLogLevel, message: String) {
        info!("Rust Console: {}", message);
    }

    fn load_web_resource(&self, _webview: servo::WebView, load: WebResourceLoad) {
        let url = load.request().url.clone();
        let url_str = url.as_str();
        
        if url_str.starts_with("lotus-resource://") {
             // 1. Get root path for this window
             let root_path = if let Some(state) = APP_STATE.get() {
                 if let Ok(s) = state.lock() {
                     s.window_metadata.get(&self.window_id)
                        .and_then(|m| m.root_path.clone())
                 } else {
                     None
                 }
             } else {
                 None
             };

             if let Some(root) = root_path {
                 // 2. Resolve file path
                 // Handle "lotus-resource://localhost/path/to/file"
                 // The path() method returns "/path/to/file"
                 let path_in_url = url.path();
                 // Remove leading slash safely
                 let relative_path = path_in_url.trim_start_matches('/');
                 
                 // Prevent directory traversal (basic check)
                 // Start with root
                 let full_path = root.join(relative_path);
                 
                 debug!("Rust: Loading resource: {:?}", full_path);
                 
                 match fs::read(&full_path) {
                    Ok(data) => {
                        let mime = mime_guess::from_path(&full_path).first_or_octet_stream();
                        let mime_str = mime.to_string();
                        
                        let mut headers = HeaderMap::new();
                        if let Ok(val) = HeaderValue::from_str(&mime_str) {
                             headers.insert(CONTENT_TYPE, val);
                        }

                        let response = WebResourceResponse::new(url)
                            .headers(headers)
                            .status_code(StatusCode::OK);

                        let mut intercepted = load.intercept(response);
                        intercepted.send_body_data(data);
                        intercepted.finish();
                    },
                    Err(e) => {
                        error!("Failed to read file {:?}: {}", full_path, e);
                         // Return 404
                        let response = WebResourceResponse::new(url)
                            .status_code(StatusCode::NOT_FOUND);
                        let intercepted = load.intercept(response);
                        intercepted.finish();
                    }
                 }
                 return;
             }
        }
        
        // For all other URLs, don't intercept (let default handling occur)
    }

    fn notify_cursor_changed(&self, _webview: servo::WebView, cursor: servo::Cursor) {
        let winit_cursor = match cursor {
            servo::Cursor::Default => CursorIcon::Default,
            servo::Cursor::Pointer => CursorIcon::Pointer,
            servo::Cursor::ContextMenu => CursorIcon::ContextMenu,
            servo::Cursor::Help => CursorIcon::Help,
            servo::Cursor::Progress => CursorIcon::Progress,
            servo::Cursor::Wait => CursorIcon::Wait,
            servo::Cursor::Cell => CursorIcon::Cell,
            servo::Cursor::Crosshair => CursorIcon::Crosshair,
            servo::Cursor::Text => CursorIcon::Text,
            servo::Cursor::VerticalText => CursorIcon::VerticalText,
            servo::Cursor::Alias => CursorIcon::Alias,
            servo::Cursor::Copy => CursorIcon::Copy,
            servo::Cursor::Move => CursorIcon::Move,
            servo::Cursor::NoDrop => CursorIcon::NoDrop,
            servo::Cursor::NotAllowed => CursorIcon::NotAllowed,
            servo::Cursor::Grab => CursorIcon::Grab,
            servo::Cursor::Grabbing => CursorIcon::Grabbing,
            servo::Cursor::EResize => CursorIcon::EResize,
            servo::Cursor::NResize => CursorIcon::NResize,
            servo::Cursor::NeResize => CursorIcon::NeResize,
            servo::Cursor::NwResize => CursorIcon::NwResize,
            servo::Cursor::SResize => CursorIcon::SResize,
            servo::Cursor::SeResize => CursorIcon::SeResize,
            servo::Cursor::SwResize => CursorIcon::SwResize,
            servo::Cursor::WResize => CursorIcon::WResize,
            servo::Cursor::EwResize => CursorIcon::EwResize,
            servo::Cursor::NsResize => CursorIcon::NsResize,
            servo::Cursor::NeswResize => CursorIcon::NeswResize,
            servo::Cursor::NwseResize => CursorIcon::NwseResize,
            servo::Cursor::ColResize => CursorIcon::ColResize,
            servo::Cursor::RowResize => CursorIcon::RowResize,
            servo::Cursor::AllScroll => CursorIcon::AllScroll,
            servo::Cursor::ZoomIn => CursorIcon::ZoomIn,
            servo::Cursor::ZoomOut => CursorIcon::ZoomOut,
            _ => CursorIcon::Default,
        };
        self.window.set_cursor(winit_cursor);
    }
}

// ------------------------------------------------------------------
// WAKER STRATEGY
// ------------------------------------------------------------------

#[derive(Clone)]
struct LotusWaker(EventLoopProxy<EngineCommand>);

impl servo::EventLoopWaker for LotusWaker {
    fn clone_box(&self) -> Box<dyn servo::EventLoopWaker> {
        Box::new(self.clone())
    }
    fn wake(&self) {
        let _ = self.0.send_event(EngineCommand::Wake);
    }
}

// ------------------------------------------------------------------
// INTERNAL APP HANDLER (Winit 0.30)
// ------------------------------------------------------------------

struct LotusApp {
    servo: Option<servo::Servo>,
    windows: HashMap<String, WindowInstance>,
    winit_id_to_uuid: HashMap<WindowId, String>,
    proxy: EventLoopProxy<EngineCommand>,
    callback: ThreadsafeFunction<Vec<u8>, ErrorStrategy::Fatal>,
}

impl LotusApp {
    fn new(
        proxy: EventLoopProxy<EngineCommand>,
        callback: ThreadsafeFunction<Vec<u8>, ErrorStrategy::Fatal>,
    ) -> Self {
        let mut app = Self {
            servo: None,
            windows: HashMap::new(),
            winit_id_to_uuid: HashMap::new(),
            proxy,
            callback,
        };
        app.ensure_servo();
        app
    }
    
    fn ensure_servo(&mut self) -> &servo::Servo {
        if self.servo.is_none() {
            let mut prefs = servo::prefs::Preferences::default();
            prefs.shell_background_color_rgba = [0.0, 0.0, 0.0, 0.0]; // Transparent

            let waker = LotusWaker(self.proxy.clone());
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

impl ApplicationHandler<EngineCommand> for LotusApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // App is ready to create windows
        info!("Rust: Application Resumed");
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, cmd: EngineCommand) {
        match cmd {
            EngineCommand::Wake => {
                trace!("Rust: Wake received");
                if let Some(servo) = &self.servo {
                    servo.spin_event_loop();
                }
            },
            EngineCommand::CreateWindow(options, window_id) => {
                info!("Creating window: {}", window_id);
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
                
                let display_handle = window.display_handle().expect("Failed to get display handle");
                let window_handle = window.window_handle().expect("Failed to get window handle");
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

                // Apply Vibrancy/Transparency effects
                if options.transparent {
                    #[cfg(target_os = "macos")]
                    {
                        // Use HUD window or standard vibrancy
                        let _ = apply_vibrancy(&window, NSVisualEffectMaterial::HudWindow, None, None);
                    }
                    
                    #[cfg(target_os = "windows")]
                    {
                        // Try Mica first, fall back to blur
                         let _ = apply_mica(&window, None)
                            .or_else(|_| apply_blur(&window, None));
                    }
                }
                
                let servo = self.ensure_servo().clone();
                let delegate = Rc::new(LotusWebViewDelegate {
                    window: window.clone(),
                    window_id: window_id.clone(),
                });
                
                let hidpi_scale_factor_val = window.scale_factor() as f32;
                let hidpi_scale_factor = Scale::<f32, DeviceIndependentPixel, DevicePixel>::new(hidpi_scale_factor_val);
                debug!("Rust: Creating WebView with scale factor: {}", hidpi_scale_factor_val);
                
                let user_content_manager = Rc::new(UserContentManager::new(&servo));
                
                // Get msgpackr source, port and token from state
                let (msgpackr_source, port, token) = if let Some(state) = APP_STATE.get() {
                    if let Ok(s) = state.lock() {
                        (s.msgpackr_source.clone(), s.ipc_server_port, s.ipc_server_token.clone())
                    } else {
                        ("".to_string(), 0, "".to_string())
                    }
                } else {
                    ("".to_string(), 0, "".to_string())
                };

                // Inject scripts
                user_content_manager.add_script(Rc::new(UserScript::from(msgpackr_source.as_str())));
                user_content_manager.add_script(Rc::new(UserScript::from(IPC_BOOTSTRAP_BASE)));
                user_content_manager.add_script(Rc::new(UserScript::from(DRAG_REGION_SCRIPT)));
                
                let port_script = format!("window.lotus.port = {}; window.lotus.token = '{}'; window.lotus.id = '{}';", port, token, window_id);
                user_content_manager.add_script(Rc::new(UserScript::from(port_script.as_str())));

                // Inject Theme
                // Use the explicitly detected theme since we trust it more than winit's initial state on some linux WMs
                let theme_str = match mode {
                    dark_light::Mode::Dark => "dark",
                    dark_light::Mode::Light => "light",
                    _ => "light",
                };
                let theme_script = format!(r#"
                    window.lotus.theme = '{}';
                    try {{
                        document.documentElement.dataset.theme = window.lotus.theme;
                    }} catch(e) {{}}
                "#, theme_str);
                user_content_manager.add_script(Rc::new(UserScript::from(theme_script.as_str())));

                let mut webview_builder = WebViewBuilder::new(&servo, rendering_context.clone())
                    .delegate(delegate)
                    .hidpi_scale_factor(hidpi_scale_factor)
                    .user_content_manager(user_content_manager);

                if let Some(ref url_str) = options.initial_url {
                    debug!("Rust: Setting initial URL in builder: {}", url_str);
                    if let Ok(u) = url::Url::parse(url_str) {
                        webview_builder = webview_builder.url(u);
                    } else {
                        error!("Rust: Failed to parse initial URL: {}", url_str);
                    }
                }

                let webview = webview_builder.build();

                let instance = WindowInstance {
                    webview,
                    rendering_context,
                    window: window.clone(),
                    last_mouse_pos: Point2D::new(0.0, 0.0),
                    is_mouse_down: false,
                    frameless: options.frameless,
                    drag_regions: Vec::new(),
                    no_drag_regions: Vec::new(),
                    in_resize_border: false,
                };

                self.windows.insert(window_id.clone(), instance);
                self.winit_id_to_uuid.insert(winit_id, window_id.clone());

                if let Some(state) = APP_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        s.window_metadata.insert(window_id.clone(), WindowMetadata {
                            root_path: options.root.map(PathBuf::from),
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

                if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                    "event": "ready",
                    "window_id": window_id
                })) {
                    self.callback.call(msg, ThreadsafeFunctionCallMode::NonBlocking);
                }

                info!("Window created successfully: {}", window_id);
            },
            EngineCommand::Quit => {
                event_loop.exit();
            },
            EngineCommand::IpcMessage(_window_id, raw_bytes) => {
                self.callback.call(raw_bytes, ThreadsafeFunctionCallMode::NonBlocking);
            },
            EngineCommand::LoadUrl(window_id, url) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    if let Ok(u) = url::Url::parse(&url) {
                        instance.webview.load(u);
                    }
                }
            },
            EngineCommand::SendToRenderer(window_id, channel, data) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    let data_json = serde_json::to_string(&data).unwrap_or_else(|_| "null".to_string());
                    let script = format!("if (window.lotus) {{ window.lotus.emit('{}', {}); }}", channel, data_json);
                    instance.webview.evaluate_javascript(&script, |_| {});
                }
            },
            EngineCommand::Resize(window_id, size) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.webview.resize(size);
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
                self.windows.remove(&window_id);
                info!("Closed window: {}", window_id);
            },
            EngineCommand::SetDecorations(window_id, decorations) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_decorations(decorations);
                }
            },
            EngineCommand::ExecuteScript(window_id, script) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.webview.evaluate_javascript(&script, |_| {});
                }
            },
            EngineCommand::ShowWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_visible(true);
                }
            },
            EngineCommand::HideWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_visible(false);
                }
            },
            EngineCommand::UpdateDragRegions(window_id, drag_regions, no_drag_regions) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    instance.drag_regions = drag_regions;
                    instance.no_drag_regions = no_drag_regions;
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
        }
        
        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
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
                         instance.webview.evaluate_javascript(&script, |_| {});
                    }
                }
            },
            _ => {}
        }

        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }

        if let Some(uuid) = self.winit_id_to_uuid.get(&window_id).cloned() {
            if let Some(instance) = self.windows.get_mut(&uuid) {
                match event {
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
                            self.callback.call(close_msg, ThreadsafeFunctionCallMode::NonBlocking);
                        }
                        self.windows.remove(&uuid);
                        self.winit_id_to_uuid.remove(&window_id);
                        if self.windows.is_empty() {
                            event_loop.exit();
                        }
                    },
                    WindowEvent::RedrawRequested => {
                        let size = instance.window.inner_size();
                        trace!("Rust: RedrawRequested for {}, size={}x{}, scale={}, visible={}", 
                            uuid, size.width, size.height, instance.window.scale_factor(), instance.window.is_visible().unwrap_or(true));
                        
                        if let Err(e) = instance.rendering_context.make_current() {
                            error!("Rust: Failed to make context current: {:?}", e);
                        }
                        
                        instance.rendering_context.prepare_for_rendering();
                        instance.webview.paint();
                        instance.rendering_context.present();
                    },
            WindowEvent::Resized(size) => {
                info!("Rust: Resized to {}x{}", size.width, size.height);
                let servo_size = ServoPhysicalSize::new(size.width, size.height);
                instance.webview.resize(servo_size);
                
                // ALSO resize the surfman/opengl rendering context! 
                // This fixes the clipping/scroll bounding bug.
                instance.rendering_context.resize(size);

                let mut msg = Vec::new();
                if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                    "event": "resized",
                    "window_id": uuid,
                    "width": size.width,
                    "height": size.height
                })).is_ok() {
                    self.callback.call(msg, ThreadsafeFunctionCallMode::NonBlocking);
                }
            },
                    WindowEvent::CursorMoved { position, .. } => {
                        let point = Point2D::new(position.x as f32, position.y as f32);
                        instance.last_mouse_pos = point;
                        
                        // Hit-test resize directions first
                        if instance.frameless {
                            let mut hit_no_drag = false;
                            for no_drag_region in &instance.no_drag_regions {
                                if no_drag_region.contains(point) {
                                    hit_no_drag = true;
                                    break;
                                }
                            }
                            
                            if hit_no_drag {
                                if instance.in_resize_border {
                                    instance.window.set_cursor(CursorIcon::Default);
                                    instance.in_resize_border = false;
                                }
                            } else {
                                let size = instance.window.inner_size();
                                let x = position.x;
                                let y = position.y;
                                let w = size.width as f64;
                                let h = size.height as f64;
                                let border = 8.0;
                                
                                // Only override cursor when we're in a resize border zone.
                                // Otherwise let Servo drive the cursor via notify_cursor_changed.
                                let resize_cursor = if x < border && y < border {
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
                                    None // Let Servo handle the cursor
                                };
                                
                                if let Some(icon) = resize_cursor {
                                    instance.window.set_cursor(icon);
                                    instance.in_resize_border = true;
                                } else {
                                    // Only reset to Default when TRANSITIONING OUT of the border zone.
                                    // After that, let Servo drive cursor via notify_cursor_changed.
                                    if instance.in_resize_border {
                                        instance.window.set_cursor(CursorIcon::Default);
                                    }
                                    instance.in_resize_border = false;
                                }
                            }
                        }

                        instance.webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
                            servo::WebViewPoint::Device(point)
                        )));
                    },
                    WindowEvent::MouseInput { state, button, .. } => {
                        let is_pressed = state == winit::event::ElementState::Pressed;
                        instance.is_mouse_down = is_pressed;
                        
                        let action = match state {
                            winit::event::ElementState::Pressed => MouseButtonAction::Down,
                            winit::event::ElementState::Released => MouseButtonAction::Up,
                        };
                        let servo_button = match button {
                            winit::event::MouseButton::Left => ServoMouseButton::Left,
                            winit::event::MouseButton::Right => ServoMouseButton::Right,
                            winit::event::MouseButton::Middle => ServoMouseButton::Middle,
                            winit::event::MouseButton::Back => ServoMouseButton::Back,
                            winit::event::MouseButton::Forward => ServoMouseButton::Forward,
                            winit::event::MouseButton::Other(v) => ServoMouseButton::Other(v),
                        };

                        if is_pressed && button == winit::event::MouseButton::Left && instance.frameless {
                            let mut hit_no_drag = false;
                            for no_drag_region in &instance.no_drag_regions {
                                if no_drag_region.contains(instance.last_mouse_pos) {
                                    hit_no_drag = true;
                                    break;
                                }
                            }
                            
                            if !hit_no_drag {
                                let size = instance.window.inner_size();
                                let x = instance.last_mouse_pos.x as f64;
                                let y = instance.last_mouse_pos.y as f64;
                                let w = size.width as f64;
                                let h = size.height as f64;
                                let border = 8.0;
                                
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
                                    // Important: We inject an Up event before handing control to the OS
                                    // because the OS block the event loop and eats the MouseUp.
                                    instance.webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                                        MouseButtonAction::Up,
                                        servo_button,
                                        servo::WebViewPoint::Device(instance.last_mouse_pos)
                                    )));
                                    instance.is_mouse_down = false;
                                    let _ = instance.window.drag_resize_window(dir);
                                    return; // Stop processing and don't forward to servo
                                }
                            }
                            
                            // Check Drag Regions
                            let mut hit_drag = false;
                            for region in &instance.drag_regions {
                                if region.contains(instance.last_mouse_pos) {
                                    // Check no target regions first!
                                    let mut hit_no_drag = false;
                                    for no_drag_region in &instance.no_drag_regions {
                                        if no_drag_region.contains(instance.last_mouse_pos) {
                                            hit_no_drag = true;
                                            break;
                                        }
                                    }
                                    
                                    if !hit_no_drag {
                                        hit_drag = true;
                                        break;
                                    }
                                }
                            }
                            
                            if hit_drag {
                                // Inject Up event to clear dragging state in web platform
                                instance.webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                                    MouseButtonAction::Up,
                                    servo_button,
                                    servo::WebViewPoint::Device(instance.last_mouse_pos)
                                )));
                                instance.is_mouse_down = false;
                                let _ = instance.window.drag_window();
                                return;
                            }
                        }

                        instance.webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                            action,
                            servo_button,
                            servo::WebViewPoint::Device(instance.last_mouse_pos)
                        )));
                    },
                    WindowEvent::MouseWheel { delta, .. } => {
                        let scroll_multiplier = 20.0;
                        let (x, y) = match delta {
                            MouseScrollDelta::LineDelta(x, y) => (x as f64 * scroll_multiplier, y as f64 * scroll_multiplier),
                            MouseScrollDelta::PixelDelta(pos) => (pos.x * scroll_multiplier, pos.y * scroll_multiplier),
                        };
                        let wheel_delta = WheelDelta {
                            x,
                            y,
                            z: 0.0,
                            mode: WheelMode::DeltaPixel,
                        };
                        instance.webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(
                            wheel_delta,
                            servo::WebViewPoint::Device(instance.last_mouse_pos)
                        )));
                    },
                    WindowEvent::Moved(position) => {
                        let mut msg = Vec::new();
                        if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                            "event": "moved",
                            "window_id": uuid,
                            "x": position.x,
                            "y": position.y
                        })).is_ok() {
                            self.callback.call(msg, ThreadsafeFunctionCallMode::NonBlocking);
                        }
                    },
                    WindowEvent::Focused(focused) => {
                        let event_name = if focused { "focused" } else { "unfocused" };
                        let mut msg = Vec::new();
                        if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                            "event": event_name,
                            "window_id": uuid
                        })).is_ok() {
                            self.callback.call(msg, ThreadsafeFunctionCallMode::NonBlocking);
                        }
                    },
                    _ => {}
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }
    }
}

// ------------------------------------------------------------------
// APP STRUCT - Manages global event loop
// ------------------------------------------------------------------


#[napi]
pub struct App {
    // App doesn't hold the proxy directly - it's in the global static
}

#[napi]
impl App {
    #[napi(constructor)]
    pub fn new(callback: ThreadsafeFunction<Vec<u8>, ErrorStrategy::Fatal>, profiling: bool, app_identifier: Option<String>, msgpackr_source: String) -> Self {
        let (proxy_tx, proxy_rx) = crossbeam_channel::bounded(1);
        
        let start_time = Instant::now();
        if profiling {
            eprintln!("[PROFILE] Application starting...");
        }

        // Determine app identifier (default to "lotus" if not provided)
        let app_id = app_identifier.unwrap_or_else(|| "lotus".to_string());

        #[cfg(target_os = "linux")]
        {
            let mode = detect_linux_theme_robust();
            eprintln!("[PROFILE] App::new - Linux Theme Detection Result (Robust): {:?}", mode);
            if mode == dark_light::Mode::Dark {
               env::set_var("GTK_THEME", "Adwaita:dark");
               eprintln!("[PROFILE] Set GTK_THEME=Adwaita:dark");
            } else {
               eprintln!("[PROFILE] Did NOT set GTK_THEME (mode was not Dark)");
            }
        }

        // 1. Initialize global app state
        let app_state = Arc::new(Mutex::new(AppState {
            window_metadata: HashMap::new(),
            window_states: WindowStateManager::new(&app_id),
            ipc_server_port: 0,
            ipc_server_token: Uuid::new_v4().to_string(),
            msgpackr_source,
            profiling,
            start_time,
            window_start_times: HashMap::new(),
        }));
        APP_STATE.set(app_state.clone()).ok();

        let token = app_state.lock().unwrap().ipc_server_token.clone();
        
        // 2. Start Event Loop in dedicated thread
        let event_callback = callback.clone();
        thread::spawn(move || {
            let mut builder = winit::event_loop::EventLoop::<EngineCommand>::with_user_event();
            #[cfg(target_os = "linux")]
            {
                winit::platform::x11::EventLoopBuilderExtX11::with_any_thread(&mut builder, true);
                winit::platform::wayland::EventLoopBuilderExtWayland::with_any_thread(&mut builder, true);
            }
            #[cfg(target_os = "windows")]
            {
                use winit::platform::windows::EventLoopBuilderExtWindows;
                builder.with_any_thread(true);
            }
            
            let event_loop = builder.build().unwrap();
            let proxy = event_loop.create_proxy();
            
            // Store proxy in global static and signal main thread
            EVENT_LOOP_PROXY.set(proxy.clone()).ok();
            let _ = proxy_tx.send(proxy.clone());

            info!("Rust: Starting Event Loop (LotusApp)");
            let mut lotus_app = LotusApp::new(proxy, event_callback);
            let _ = event_loop.run_app(&mut lotus_app);
            
            info!("Rust: Event loop successfully stopped. Hard exit via _exit(0) now.");
            unsafe { libc::_exit(0); }
        });

        // 3. Wait for proxy to be ready
        let proxy = proxy_rx.recv().expect("Failed to receive proxy");

        // 4. Start IPC Server
        let server_proxy = proxy.clone();
        let server_token = token.clone();
        thread::spawn(move || {
            let server = match tiny_http::Server::http("127.0.0.1:0") {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to start IPC server: {}", e);
                    return;
                }
            };
            let actual_port = match server.server_addr() {
                tiny_http::ListenAddr::IP(addr) => addr.port(),
                _ => 0,
            };
            info!("Rust: IPC Server listening on port {}", actual_port);
            
            // Update port in state
            if let Some(state) = APP_STATE.get() {
                if let Ok(mut s) = state.lock() {
                    s.ipc_server_port = actual_port;
                }
            }

            for mut request in server.incoming_requests() {
                let url_str = request.url().to_string();
                
                // Handle CORS preflight
                if request.method() == &tiny_http::Method::Options {
                    let response = tiny_http::Response::from_string("OK")
                        .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap())
                        .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Methods"[..], &b"GET, POST, OPTIONS"[..]).unwrap())
                        .with_header(tiny_http::Header::from_bytes(&b"Access-Control-Allow-Headers"[..], &b"Content-Type, X-Lotus-Auth, X-Lotus-Window-Id"[..]).unwrap());
                    let _ = request.respond(response);
                    continue;
                }

                let auth_token = request.headers().iter()
                    .find(|h| h.field.as_str().to_ascii_lowercase() == "x-lotus-auth")
                    .map(|h| h.value.as_str().to_string());
                
                if auth_token != Some(server_token.clone()) {
                    let _ = request.respond(tiny_http::Response::from_string("Unauthorized").with_status_code(401));
                    continue;
                }

                let window_id = request.headers().iter()
                    .find(|h| h.field.as_str().to_ascii_lowercase() == "x-lotus-window-id")
                    .map(|h| h.value.as_str().to_string())
                    .unwrap_or_else(|| "unknown".to_string());

                let cors_header = tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap();

                if url_str.starts_with("/ipc/") {
                    let channel = url_str.trim_start_matches("/ipc/");
                    let channel = urlencoding::decode(channel).unwrap_or(std::borrow::Cow::Borrowed(channel)).to_string();

                    let mut content = Vec::new();
                    let _ = request.as_reader().read_to_end(&mut content);
                    
                    let mut msg = Vec::new();
                    // Wrap single binary message in a list for consistency with batch format: [[channel, content]]
                    if let Ok(_) = rmp_serde::encode::write(&mut msg, &vec![(channel, content)]) {
                        let _ = server_proxy.send_event(EngineCommand::IpcMessage(window_id, msg));
                    }
                    let _ = request.respond(tiny_http::Response::from_string("ok").with_header(cors_header));
                } else if url_str == "/batch" {
                    let mut content = Vec::new();
                    let _ = request.as_reader().read_to_end(&mut content);

                    // Before forwarding to Node.js, intercept internal Lotus IPC channels.
                    // This avoids a Node.js round-trip for performance-critical messages.
                    if let Ok(batch) = rmp_serde::decode::from_slice::<Vec<(String, serde_json::Value)>>(&content) {
                        for (channel, data) in &batch {
                            if channel == "lotus:set-drag-regions" {
                                let mut drag_regions = Vec::new();
                                let mut no_drag_regions = Vec::new();

                                if let Some(drag_arr) = data.get("drag").and_then(|v| v.as_array()) {
                                    for r in drag_arr {
                                        if let (Some(x), Some(y), Some(w), Some(h)) = (
                                            r.get("x").and_then(|v| v.as_f64()),
                                            r.get("y").and_then(|v| v.as_f64()),
                                            r.get("width").and_then(|v| v.as_f64()),
                                            r.get("height").and_then(|v| v.as_f64())
                                        ) {
                                            drag_regions.push(euclid::Rect::new(
                                                euclid::Point2D::new(x as f32, y as f32),
                                                euclid::Size2D::new(w as f32, h as f32)
                                            ));
                                        }
                                    }
                                }

                                if let Some(no_drag_arr) = data.get("noDrag").and_then(|v| v.as_array()) {
                                    for r in no_drag_arr {
                                        if let (Some(x), Some(y), Some(w), Some(h)) = (
                                            r.get("x").and_then(|v| v.as_f64()),
                                            r.get("y").and_then(|v| v.as_f64()),
                                            r.get("width").and_then(|v| v.as_f64()),
                                            r.get("height").and_then(|v| v.as_f64())
                                        ) {
                                            no_drag_regions.push(euclid::Rect::new(
                                                euclid::Point2D::new(x as f32, y as f32),
                                                euclid::Size2D::new(w as f32, h as f32)
                                            ));
                                        }
                                    }
                                }
                                
                                info!("Rust: Intercepted drag regions from batch for {}: drag: {}, no_drag: {}", window_id, drag_regions.len(), no_drag_regions.len());
                                let _ = server_proxy.send_event(EngineCommand::UpdateDragRegions(window_id.clone(), drag_regions, no_drag_regions));
                                
                                // Don't forward lotus:* internal channels to Node.js
                                continue;
                            }
                        }
                        // Forward the full batch to Node.js (non-internal messages will still be routed there)
                    }

                    // Always forward raw bytes to Node.js for normal IPC channels
                    let _ = server_proxy.send_event(EngineCommand::IpcMessage(window_id, content));
                    let _ = request.respond(tiny_http::Response::from_string("ok").with_header(cors_header));
                } else if url_str.starts_with("/resource/") {
                    let path = url_str.trim_start_matches("/resource/");
                    let mut full_path = env::current_dir().unwrap();
                    full_path.push(path.trim_start_matches('/'));
                    
                    if full_path.exists() && full_path.is_file() {
                        if let Ok(file) = fs::File::open(full_path) {
                            let mut file_response = tiny_http::Response::from_file(file);
                            file_response.add_header(cors_header.clone());
                            let _ = request.respond(file_response);
                        } else {
                            let _ = request.respond(tiny_http::Response::from_string("Error reading file").with_status_code(500));
                        }
                    } else {
                        let _ = request.respond(tiny_http::Response::from_string("Not Found").with_status_code(404));
                    }
                } else {
                    let _ = request.respond(tiny_http::Response::from_string("Not Found").with_status_code(404));
                }
            }
        });

        // 5. Send Initial ready event to Node.js
        let ready_token = token.clone();
        let ready_callback = callback.clone();
        thread::spawn(move || {
            init_resources();
            
            let mut port = 0;
            for _ in 0..100 {
                if let Some(state) = APP_STATE.get() {
                    if let Ok(s) = state.lock() {
                        if s.ipc_server_port != 0 {
                            port = s.ipc_server_port;
                            break;
                        }
                    }
                }
                thread::sleep(std::time::Duration::from_millis(10));
            }

            let mut ready_msg = Vec::new();
            if rmp_serde::encode::write(&mut ready_msg, &serde_json::json!({
                "event": "app-ready", 
                "ipc_port": port, 
                "ipc_token": ready_token
            })).is_ok() {
                ready_callback.call(ready_msg, ThreadsafeFunctionCallMode::NonBlocking);
            }
        });

        App {}
    }

    #[napi]
    pub fn quit(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::Quit);
        }
    }
}

// ------------------------------------------------------------------
// CREATE WINDOW FUNCTION
// ------------------------------------------------------------------

#[napi]
pub fn create_window(options: WindowOptions) -> WindowHandle {
    let window_id = options.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    
    if let Some(proxy) = EVENT_LOOP_PROXY.get() {
        let _ = proxy.send_event(EngineCommand::CreateWindow(options, window_id.clone()));
    }
    
    WindowHandle {
        id: window_id,
    }
}
