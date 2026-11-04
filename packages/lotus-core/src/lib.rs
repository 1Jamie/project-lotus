use std::sync::{Arc, Mutex};
use std::thread;
use std::env;
use std::collections::HashMap;
use std::time::Instant;

use napi_derive::napi;
use napi::threadsafe_function::{ThreadsafeFunction, ErrorStrategy, ThreadsafeFunctionCallMode};
use serde_json;
use uuid::Uuid;
use log::{info, error, debug};

pub(crate) mod window_state;
pub(crate) mod platform;
pub mod ipc_security;
pub mod vfs;
pub mod layout;
pub mod window;
pub mod server;
pub mod delegate;
pub mod gl;
pub mod app;

use crate::window_state::WindowStateManager;
pub use app::*;
use vfs::{ByteLimitedLruCache, EncryptedVfs};
pub use layout::{PaneAnchor, StagedPaneLayout, StagedLayout};
pub use server::{dispatch_to_renderer, start_ipc_server, WS_SENDERS, WS_PENDING};
pub use delegate::LotusPaneDelegate;
pub use window::{
    WindowOptions, PaneOptions, ScrollbarConfig, WindowMetadata,
    ActiveSelectPrompt, SelectPromptWrapper, DragRect, DragRegionPayload,
    PaneInstance, WindowInstance,
    map_winit_modifiers, map_winit_key, map_winit_code,
};


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
            let _ = proxy.send_event(EngineCommand::LoadUrl(self.id.clone(), "main".to_string(), url));
        }
    }

    #[napi]
    pub fn load_url_in_pane(&self, pane_id: String, url: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::LoadUrl(self.id.clone(), pane_id, url));
        }
    }

    #[napi]
    pub fn send_to_renderer(&self, data: napi::bindgen_prelude::Buffer) -> napi::Result<()> {
        dispatch_to_renderer(self.id.clone(), None, axum::body::Bytes::from(data.to_vec()));
        Ok(())
    }

    #[napi]
    pub fn send_to_pane_renderer(&self, pane_id: String, data: napi::bindgen_prelude::Buffer) -> napi::Result<()> {
        dispatch_to_renderer(self.id.clone(), Some(pane_id), axum::body::Bytes::from(data.to_vec()));
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
            if let Ok(data) = serde_json::from_str::<DragRegionPayload>(&rects_json) {
                let drag_regions = data.drag.unwrap_or_default().into_iter().map(|r| {
                    euclid::Rect::new(
                        euclid::Point2D::new(r.x, r.y),
                        euclid::Size2D::new(r.width, r.height)
                    )
                }).collect::<Vec<_>>();

                let no_drag_regions = data.no_drag.unwrap_or_default().into_iter().map(|r| {
                    euclid::Rect::new(
                        euclid::Point2D::new(r.x, r.y),
                        euclid::Size2D::new(r.width, r.height)
                    )
                }).collect::<Vec<_>>();

                debug!("Rust: Updated drag regions for window {}: drag: {}, no_drag: {}", self.id, drag_regions.len(), no_drag_regions.len());
                let _ = proxy.send_event(EngineCommand::UpdateDragRegions(self.id.clone(), "main".to_string(), drag_regions, no_drag_regions));
            }
        }
    }

    #[napi]
    pub fn update_pane_drag_regions(&self, pane_id: String, rects_json: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            if let Ok(data) = serde_json::from_str::<DragRegionPayload>(&rects_json) {
                let drag_regions = data.drag.unwrap_or_default().into_iter().map(|r| {
                    euclid::Rect::new(
                        euclid::Point2D::new(r.x, r.y),
                        euclid::Size2D::new(r.width, r.height)
                    )
                }).collect::<Vec<_>>();

                let no_drag_regions = data.no_drag.unwrap_or_default().into_iter().map(|r| {
                    euclid::Rect::new(
                        euclid::Point2D::new(r.x, r.y),
                        euclid::Size2D::new(r.width, r.height)
                    )
                }).collect::<Vec<_>>();

                let _ = proxy.send_event(EngineCommand::UpdateDragRegions(self.id.clone(), pane_id, drag_regions, no_drag_regions));
            }
        }
    }

    #[napi]
    pub fn execute_script(&self, script: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::ExecuteScript(self.id.clone(), "main".to_string(), script));
        }
    }

    #[napi]
    pub fn execute_script_in_pane(&self, pane_id: String, script: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::ExecuteScript(self.id.clone(), pane_id, script));
        }
    }

    #[napi]
    pub fn create_pane(&self, pane_id: String, url: String, x: f64, y: f64, width: f64, height: f64, z_index: i32, anchor: Option<u32>, dock_order: Option<u32>, visible: Option<bool>) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let rect: euclid::Rect<f32, servo::DeviceIndependentPixel> = euclid::Rect::new(euclid::Point2D::new(x as f32, y as f32), euclid::Size2D::new(width as f32, height as f32));
            let _ = proxy.send_event(EngineCommand::CreatePane(
                self.id.clone(), 
                pane_id, 
                url, 
                rect, 
                z_index, 
                PaneAnchor::from(anchor.unwrap_or(0)), 
                dock_order.unwrap_or(0),
                visible.unwrap_or(true),
            ));
        }
    }

    #[napi]
    pub fn update_select_control(&self, control_id: String, selected_indices: Vec<u32>) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::UpdateSelectControl {
                window_id: self.id.clone(),
                control_id,
                selected_indices,
            });
        }
    }

    #[napi]
    pub fn submit_select_control(&self, control_id: String, selected_indices: Vec<u32>) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SubmitSelectControl {
                window_id: self.id.clone(),
                control_id,
                selected_indices,
            });
        }
    }

    #[napi]
    pub fn dismiss_select_control(&self, control_id: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::DismissSelectControl {
                window_id: self.id.clone(),
                control_id,
            });
        }
    }

    #[napi]
    pub fn remove_pane(&self, pane_id: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::RemovePane(self.id.clone(), pane_id));
        }
    }

    #[napi]
    pub fn set_pane_rect(&self, pane_id: String, x: f64, y: f64, width: f64, height: f64) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let rect: euclid::Rect<f32, servo::DeviceIndependentPixel> = euclid::Rect::new(euclid::Point2D::new(x as f32, y as f32), euclid::Size2D::new(width as f32, height as f32));
            let _ = proxy.send_event(EngineCommand::SetPaneRect(self.id.clone(), pane_id, rect));
        }
    }

    #[napi]
    pub fn set_pane_visible(&self, pane_id: String, visible: bool) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetPaneVisible(self.id.clone(), pane_id, visible));
        }
    }

    #[napi]
    pub fn focus_pane(&self, pane_id: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::FocusPane(self.id.clone(), pane_id));
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

    /// Set the minimum inner size the user can resize the window to.
    /// Pass 0 for both width and height to remove the constraint.
    #[napi]
    pub fn set_min_size(&self, width: u32, height: u32) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let size = if width == 0 && height == 0 {
                None
            } else {
                Some(winit::dpi::PhysicalSize::new(width, height))
            };
            let _ = proxy.send_event(EngineCommand::SetMinInnerSize(self.id.clone(), size));
        }
    }

    /// Set the maximum inner size the user can resize the window to.
    /// Pass 0 for both width and height to remove the constraint.
    #[napi]
    pub fn set_max_size(&self, width: u32, height: u32) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let size = if width == 0 && height == 0 {
                None
            } else {
                Some(winit::dpi::PhysicalSize::new(width, height))
            };
            let _ = proxy.send_event(EngineCommand::SetMaxInnerSize(self.id.clone(), size));
        }
    }
}





#[napi]
pub struct App {
    // App doesn't hold the proxy directly - it's in the global static
}

#[napi]
impl App {
    #[napi(constructor)]
    pub fn new(callback: ThreadsafeFunction<(String, Vec<napi::bindgen_prelude::Buffer>), ErrorStrategy::Fatal>, profiling: bool, app_identifier: Option<String>, msgpackr_source: String) -> napi::Result<Self> {
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
            _start_time: start_time,
            window_start_times: HashMap::new(),
            vfs: None,
            resource_cache: ByteLimitedLruCache::new(128 * 1024 * 1024), // 128MB limit
        }));
        APP_STATE.set(app_state.clone()).ok();

        let token = match app_state.lock() {
            Ok(s) => s.ipc_server_token.clone(),
            Err(e) => {
                error!("AppState lock poisoned during init: {}", e);
                return Err(napi::Error::from_reason(format!("Internal state lock failed: {}", e)));
            }
        };
        
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
            
            let event_loop = match builder.build() {
                Ok(el) => el,
                Err(e) => {
                    error!("Failed to build Winit event loop: {}", e);
                    return;
                }
            };
            let proxy = event_loop.create_proxy();
            
            // Store proxy in global static and signal main thread
            EVENT_LOOP_PROXY.set(proxy.clone()).ok();
            let _ = proxy_tx.send(proxy.clone());

            info!("Rust: Starting Event Loop (LotusApp)");
            let mut lotus_app = LotusApp::new(proxy, event_callback);
            let _ = event_loop.run_app(&mut lotus_app);
            
            info!("Rust: Event loop stopped naturally.");
            fast_exit(0);
        });

        // 3. Wait for proxy to be ready
        let proxy = match proxy_rx.recv() {
            Ok(p) => p,
            Err(e) => {
                error!("Event loop thread failed to start, cannot receive proxy: {}", e);
                return Err(napi::Error::from_reason(
                    format!("Winit event loop failed to start: {}", e)
                ));
            }
        };

        // 4. Start IPC Server (Tokio + Axum)
        let server_proxy = proxy.clone();
        let server_token = token.clone();
        
        // Channel to communicate chosen port back to main thread
        let (port_tx, port_rx) = std::sync::mpsc::channel();
        
        start_ipc_server(server_proxy, server_token, port_tx);

        // 5. Send Initial ready event to Node.js
        let ready_token = token.clone();
        let ready_callback = callback.clone();
        
        // Synchronously wait up to 1s for port to be assigned from axum.
        // This safely pauses the V8 event loop for the ~1ms it takes to bind the port,
        // guaranteeing that APP_STATE is populated before any CreateWindow commands arrive.
        let port = port_rx.recv_timeout(std::time::Duration::from_millis(1000)).unwrap_or(0);

        thread::spawn(move || {
            let mode = {
                #[cfg(target_os = "linux")]
                { detect_linux_theme_robust() }
                #[cfg(not(target_os = "linux"))]
                { dark_light::detect() }
            };
            let theme_str = match mode { dark_light::Mode::Dark => "dark", _ => "light" };

            let mut ready_msg = Vec::new();
            if rmp_serde::encode::write(&mut ready_msg, &serde_json::json!({
                "event": "app-ready", 
                "ipc_port": port, 
                "ipc_token": ready_token,
                "system_theme": theme_str
            })).is_ok() {
                let mut wrapped = Vec::with_capacity(ready_msg.len() + 1);
                wrapped.push(MSG_TYPE_DATA);
                wrapped.extend(ready_msg);
                ready_callback.call(("global".to_string(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
            }
        });

        Ok(App {})
    }

    #[napi]
    pub fn init_vfs(&self) -> napi::Result<bool> {
        if let Some(state) = APP_STATE.get() {
            if let Ok(mut s) = state.lock() {
                if s.vfs.is_some() {
                    return Ok(true);
                }
                match EncryptedVfs::init() {
                    Ok(Some(vfs)) => {
                        s.vfs = Some(Arc::new(vfs));
                        info!("Rust: Encrypted VFS initialized successfully.");
                        return Ok(true);
                    }
                    Ok(None) => {
                        info!("Rust: Encrypted VFS initialization skipped (no VFS or shards found).");
                        return Ok(false);
                    }
                    Err(e) => {
                        error!("Rust: Fatal error during Encrypted VFS initialization: {}", e);
                        return Err(napi::Error::from_reason(format!("Encrypted VFS initialization failed: {}", e)));
                    }
                }
            }
        }
        Ok(false)
    }

    #[napi]
    pub fn quit(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::Quit);
        }
    }

    #[napi]
    pub fn exit(&self, code: Option<i32>) {
        fast_exit(code.unwrap_or(0));
    }

    #[napi]
    pub fn get_system_theme(&self) -> String {
        let mode = {
            #[cfg(target_os = "linux")]
            { detect_linux_theme_robust() }
            #[cfg(not(target_os = "linux"))]
            { dark_light::detect() }
        };
        match mode {
            dark_light::Mode::Dark => "dark".to_string(),
            _ => "light".to_string(),
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
