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
pub mod origin;
pub mod ipc_security;
pub mod vfs;
pub mod layout;
pub mod window;
pub mod server;
pub mod delegate;
pub mod gl;
pub mod userscripts;
pub mod app;
#[cfg(target_os = "linux")]
pub mod wayland;
#[cfg(target_os = "linux")]
pub mod dbus_notify;

use crate::window_state::WindowStateManager;
pub use app::*;
pub use origin::{
    is_trusted_origin, normalize_origin_broker, normalize_origin_ipc, origin_grants_trust,
    NormalizeMode,
};
pub(crate) use vfs::*;
pub use layout::{PaneAnchor, StagedPaneLayout, StagedLayout};
pub use server::{dispatch_to_renderer, start_ipc_server, WS_SENDERS, WS_PENDING};
pub use delegate::LotusPaneDelegate;
pub use window::{
    WindowOptions, PaneOptions, ScrollbarConfig, WindowMetadata,
    ActiveSelectPrompt, SelectPromptWrapper, DragRect, DragRegionPayload,
    PaneInstance, WindowInstance,
    map_winit_modifiers, map_winit_key, map_winit_code,
};

/// Ensure the process-wide host WebGL GL thread is running.
#[cfg(target_os = "linux")]
#[napi(js_name = "ensureWebglBroker")]
pub fn ensure_webgl_broker() -> napi::Result<()> {
    crate::gl::webgl_broker::ensure_webgl_broker()
        .map_err(|e| napi::Error::from_reason(e))
}

#[cfg(not(target_os = "linux"))]
#[napi(js_name = "ensureWebglBroker")]
pub fn ensure_webgl_broker() -> napi::Result<()> {
    Err(napi::Error::from_reason("WebGL broker is only supported on Linux"))
}

/// Register the host end of the GPU `socketpair` (from [`create_gpu_socket_pair`]) and start the feeder thread.
/// Optional `pane_id` scopes the GPU feeder (and experimental hole-punch) to that host pane.
#[cfg(target_os = "linux")]
#[napi(js_name = "registerGpuWebglBroker")]
pub fn register_gpu_webgl_broker(fd: i32, pane_id: Option<String>) -> napi::Result<()> {
    crate::gl::webgl_broker::register_gpu_fd(fd, pane_id)
        .map_err(|e| napi::Error::from_reason(e))
}

#[cfg(not(target_os = "linux"))]
#[napi(js_name = "registerGpuWebglBroker")]
pub fn register_gpu_webgl_broker(_fd: i32, _pane_id: Option<String>) -> napi::Result<()> {
    Err(napi::Error::from_reason("WebGL broker is only supported on Linux"))
}

/// Bind a previously registered GPU feeder to a host pane (prewarm → first surface).
#[cfg(target_os = "linux")]
#[napi(js_name = "associateGpuWebglBrokerPane")]
pub fn associate_gpu_webgl_broker_pane(fd: i32, pane_id: String) -> napi::Result<()> {
    crate::gl::webgl_broker::associate_feeder_pane(fd, &pane_id);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
#[napi(js_name = "associateGpuWebglBrokerPane")]
pub fn associate_gpu_webgl_broker_pane(_fd: i32, _pane_id: String) -> napi::Result<()> {
    Err(napi::Error::from_reason("WebGL broker is only supported on Linux"))
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
    pub fn request_redraw(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::RedrawWindow(self.id.clone()));
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
    pub fn create_remote_pane(
        &self,
        pane_id: String,
        ro_fd: i32,
        shm_size: i64,
        ack_rw_fd: Option<i32>,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        z_index: i32,
        anchor: Option<u32>,
        dock_order: Option<u32>,
        visible: Option<bool>,
        max_shm_width: Option<u32>,
        max_shm_height: Option<u32>,
    ) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let rect: euclid::Rect<f32, servo::DeviceIndependentPixel> = euclid::Rect::new(
                euclid::Point2D::new(x as f32, y as f32),
                euclid::Size2D::new(width as f32, height as f32),
            );
            let _ = proxy.send_event(EngineCommand::CreateRemotePane {
                window_id: self.id.clone(),
                pane_id,
                ro_fd,
                shm_size: shm_size as usize,
                ack_rw_fd: ack_rw_fd.unwrap_or(-1),
                rect,
                z_index,
                anchor: PaneAnchor::from(anchor.unwrap_or(0)),
                dock_order: dock_order.unwrap_or(0),
                visible: visible.unwrap_or(true),
                max_shm_width,
                max_shm_height,
            });
        }
    }

    #[cfg(target_os = "linux")]
    #[napi]
    pub fn create_external_app_pane(
        &self,
        pane_id: String,
        command: String,
        args: Vec<String>,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        z_index: i32,
        anchor: Option<u32>,
        dock_order: Option<u32>,
        visible: Option<bool>,
        dbus_notifications: Option<String>,
        dbus_notification_apps: Option<Vec<String>>,
    ) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let rect: euclid::Rect<f32, servo::DeviceIndependentPixel> = euclid::Rect::new(
                euclid::Point2D::new(x as f32, y as f32),
                euclid::Size2D::new(width as f32, height as f32),
            );
            let _ = proxy.send_event(EngineCommand::CreateExternalAppPane {
                window_id: self.id.clone(),
                pane_id,
                command,
                args,
                rect,
                z_index,
                anchor: PaneAnchor::from(anchor.unwrap_or(0)),
                dock_order: dock_order.unwrap_or(0),
                visible: visible.unwrap_or(true),
                dbus_notifications: dbus_notifications.unwrap_or_else(|| "off".into()),
                dbus_notification_apps: dbus_notification_apps.unwrap_or_default(),
            });
        }
    }

    #[cfg(target_os = "linux")]
    #[napi]
    pub fn set_game_mode(&self, enabled: bool) {
        // Do not guard nest readiness here — CreateExternalAppPane and SetGameMode
        // share the winit FIFO queue (EC4). Guard lives in the event handler.
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetGameMode(self.id.clone(), enabled));
        }
    }

    /// Register a window-scoped host hotkey (PhysicalKey / KeyboardEvent.code).
    /// Works with or without a nested compositor when the table is non-empty (EC3).
    #[cfg(target_os = "linux")]
    #[napi]
    pub fn register_host_hotkey(&self, id: String, key: String, modifiers: Option<u32>) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::RegisterHostHotkey {
                window_id: self.id.clone(),
                id,
                key,
                modifiers: modifiers.unwrap_or(0),
            });
        }
    }

    #[cfg(target_os = "linux")]
    #[napi]
    pub fn unregister_host_hotkey(&self, id: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::UnregisterHostHotkey {
                window_id: self.id.clone(),
                id,
            });
        }
    }

    #[cfg(target_os = "linux")]
    #[napi]
    pub fn clear_host_hotkeys(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::ClearHostHotkeys(self.id.clone()));
        }
    }

    /// Borderless fullscreen for the lotus host window (not the nested game).
    #[napi]
    pub fn set_fullscreen(&self, enabled: bool) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetFullscreen(self.id.clone(), enabled));
        }
    }

    #[napi]
    pub fn set_pane_z_index(&self, pane_id: String, z_index: i32) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetPaneZIndex(self.id.clone(), pane_id, z_index));
        }
    }

    #[napi]
    pub fn attach_remote_pane_input(&self, pane_id: String, input_fd: i32) -> napi::Result<()> {
        let dup_fd = unsafe { libc::dup(input_fd) };
        if dup_fd < 0 {
            let err = unsafe { *libc::__errno_location() };
            return Err(napi::Error::from_reason(format!("dup failed for input_fd: errno {}", err)));
        }
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::AttachRemotePaneInput(self.id.clone(), pane_id, dup_fd));
        } else {
            unsafe { libc::close(dup_fd); };
        }
        Ok(())
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

    /// Update dock thickness for an anchored pane without clearing its anchor.
    /// Left/Right use `width`; Top/Bottom use `height`; Fill/None are ignored.
    #[napi]
    pub fn set_pane_size(&self, pane_id: String, width: f64, height: f64) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetPaneSize(
                self.id.clone(),
                pane_id,
                width as f32,
                height as f32,
            ));
        }
    }

    #[napi]
    pub fn set_pane_weight(&self, pane_id: String, weight: f64) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetPaneWeight(
                self.id.clone(),
                pane_id,
                weight as f32,
            ));
        }
    }

    #[napi]
    pub fn set_split_weights(&self, split_id: String, payload_json: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetSplitWeights(
                self.id.clone(),
                split_id,
                payload_json,
            ));
        }
    }

    #[napi]
    pub fn set_split_sizes(&self, split_id: String, payload_json: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetSplitSizes(
                self.id.clone(),
                split_id,
                payload_json,
            ));
        }
    }

    #[napi]
    pub fn begin_layout_batch(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::BeginLayoutBatch(self.id.clone()));
        }
    }

    #[napi]
    pub fn end_layout_batch(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::EndLayoutBatch(self.id.clone()));
        }
    }

    #[napi]
    pub fn set_pane_visible(&self, pane_id: String, visible: bool) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetPaneVisible(self.id.clone(), pane_id, visible));
        }
    }

    #[napi]
    pub fn set_pane_passthrough(&self, pane_id: String, passthrough: bool) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetPanePassthrough(
                self.id.clone(),
                pane_id,
                passthrough,
            ));
        }
    }

    /// Cover a docked remote with host crash chrome as a high-z overlay while keeping the
    /// remote visible so it continues to receive compositor vsync / SHM pull.
    #[napi]
    pub fn cover_remote_with_crash_chrome(&self, remote_pane_id: String, crash_pane_id: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::CoverRemoteWithCrashChrome {
                window_id: self.id.clone(),
                remote_pane_id,
                crash_pane_id,
            });
        }
    }

    /// After attaching a successor engine: wait for host GL upload before lifting chrome.
    #[napi]
    pub fn arm_crash_cover_fresh_frame(&self, remote_pane_id: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::ArmCrashCoverFreshFrame {
                window_id: self.id.clone(),
                remote_pane_id,
            });
        }
    }

    /// Clear under-cover / fresh-frame gates after crash chrome is dismissed.
    #[napi]
    pub fn clear_remote_crash_cover(&self, remote_pane_id: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::ClearRemoteCrashCover {
                window_id: self.id.clone(),
                remote_pane_id,
            });
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

// ------------------------------------------------------------------
// REMOTE PANE SHARED MEMORY ALLOCATION
// ------------------------------------------------------------------

#[napi(object)]
pub struct ShmPlaneAllocation {
    pub pixel_rw_fd: i32,
    pub pixel_ro_fd: i32,
    pub ack_rw_fd: i32,
    pub ack_ro_fd: i32,
    pub rw_fd: i32,
    pub ro_fd: i32,
    pub total_size: i64,
    pub width: u32,
    pub height: u32,
}

#[napi]
pub fn allocate_remote_pane_shm(name: String, width: u32, height: u32) -> napi::Result<ShmPlaneAllocation> {
    let geom = lotus_supervisor::shm::calculate_shm_geometry(width, height)
        .map_err(|e| napi::Error::from_reason(format!("Invalid SHM geometry: {}", e)))?;
    let total_size = 256 + geom.total_pixel_bytes;
    let pixel_plane = lotus_supervisor::shm::SealedMemfdPlane::new(&format!("{}_pixel", name), total_size)
        .map_err(|e| napi::Error::from_reason(format!("Failed to create SealedMemfdPlane for pixels: {}", e)))?;
    let ack_plane = lotus_supervisor::shm::SealedMemfdPlane::new(&format!("{}_ack", name), 64)
        .map_err(|e| napi::Error::from_reason(format!("Failed to create SealedMemfdPlane for ack: {}", e)))?;

    // Initialize ConsumerAckHeader before dropping SealedMemfdPlane
    unsafe {
        let ack_hdr = &mut *(ack_plane.rw_ptr as *mut lotus_supervisor::shm::ConsumerAckHeader);
        ack_hdr.consumer_gen_id.store(1, std::sync::atomic::Ordering::Relaxed);
        ack_hdr.claimed_generation.store(0, std::sync::atomic::Ordering::Relaxed);
        ack_hdr.claimed_seq.store(0, std::sync::atomic::Ordering::Relaxed);
        ack_hdr.free_seq.store(0, std::sync::atomic::Ordering::Relaxed);
        ack_hdr.claimed_slot.store(lotus_supervisor::shm::CLAIM_UNCLAIMED, std::sync::atomic::Ordering::Relaxed);
    }

    // Duplicate descriptors so dropping plane doesn't close them prematurely
    let pixel_rw_fd = unsafe { libc::dup(pixel_plane.rw_fd) };
    let pixel_ro_fd = unsafe { libc::dup(pixel_plane.ro_fd) };
    let ack_rw_fd = unsafe { libc::dup(ack_plane.rw_fd) };
    let ack_ro_fd = unsafe { libc::dup(ack_plane.ro_fd) };
    if pixel_rw_fd < 0 || pixel_ro_fd < 0 || ack_rw_fd < 0 || ack_ro_fd < 0 {
        return Err(napi::Error::from_reason("dup failed"));
    }

    Ok(ShmPlaneAllocation {
        pixel_rw_fd,
        pixel_ro_fd,
        ack_rw_fd,
        ack_ro_fd,
        rw_fd: pixel_rw_fd,
        ro_fd: pixel_ro_fd,
        total_size: total_size as i64,
        width,
        height,
    })
}

/// Synchronously sanitize both the producer control header and consumer ack header
/// of a remote pane SHM allocation before spawning a successor engine. This guarantees
/// that no stale `STATE_READY` frames or non-zero `pub_seq` from a dead engine
/// can be consumed by the host compositor during crash recovery.
#[napi]
pub fn reset_remote_pane_shm(pixel_rw_fd: i32, ack_rw_fd: i32) -> napi::Result<()> {
    if pixel_rw_fd >= 0 {
        let map_len = std::mem::size_of::<lotus_supervisor::shm::ProducerControlHeader>();
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                pixel_rw_fd,
                0,
            )
        };
        if ptr != libc::MAP_FAILED && !ptr.is_null() {
            unsafe {
                let prod_hdr = &mut *(ptr as *mut lotus_supervisor::shm::ProducerControlHeader);
                prod_hdr.pub_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                prod_hdr.prod_resize_gen.store(1, std::sync::atomic::Ordering::SeqCst);
                prod_hdr.active_slots = 3;
                for slot in &prod_hdr.slots {
                    slot.state.store(lotus_supervisor::shm::STATE_FREE, std::sync::atomic::Ordering::SeqCst);
                    slot.pub_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                    slot.generation.store(0, std::sync::atomic::Ordering::SeqCst);
                    slot.width.store(0, std::sync::atomic::Ordering::SeqCst);
                    slot.height.store(0, std::sync::atomic::Ordering::SeqCst);
                    slot.stride.store(0, std::sync::atomic::Ordering::SeqCst);
                }
                libc::munmap(ptr, map_len);
            }
        }
    }

    if ack_rw_fd >= 0 {
        let map_len = std::mem::size_of::<lotus_supervisor::shm::ConsumerAckHeader>();
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                ack_rw_fd,
                0,
            )
        };
        if ptr != libc::MAP_FAILED && !ptr.is_null() {
            unsafe {
                let ack_hdr = &mut *(ptr as *mut lotus_supervisor::shm::ConsumerAckHeader);
                ack_hdr.claimed_generation.store(0, std::sync::atomic::Ordering::SeqCst);
                ack_hdr.claimed_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                ack_hdr.free_seq.store(0, std::sync::atomic::Ordering::SeqCst);
                ack_hdr.claimed_slot.store(
                    lotus_supervisor::shm::CLAIM_UNCLAIMED,
                    std::sync::atomic::Ordering::SeqCst,
                );
                libc::munmap(ptr, map_len);
            }
        }
    }

    Ok(())
}

#[napi(object)]
pub struct ControlSocketPair {
    pub host_fd: i32,
    pub child_fd: i32,
}

#[napi(object)]
pub struct GpuSocketPair {
    pub host_fd: i32,
    pub child_fd: i32,
}

#[napi]
pub fn create_control_socket_pair() -> napi::Result<ControlSocketPair> {
    let mut fds = [0i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } != 0 {
        let err = unsafe { *libc::__errno_location() };
        return Err(napi::Error::from_reason(format!("socketpair failed: errno {}", err)));
    }
    Ok(ControlSocketPair {
        host_fd: fds[0],
        child_fd: fds[1],
    })
}

#[napi]
pub fn create_gpu_socket_pair() -> napi::Result<GpuSocketPair> {
    let mut fds = [0i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } != 0 {
        let err = unsafe { *libc::__errno_location() };
        return Err(napi::Error::from_reason(format!("socketpair (gpu) failed: errno {}", err)));
    }
    Ok(GpuSocketPair {
        host_fd: fds[0],
        child_fd: fds[1],
    })
}

#[napi]
pub fn send_fds_over_socket(socket_fd: i32, fds_to_send: Vec<i32>, payload: String) -> napi::Result<()> {
    if fds_to_send.is_empty() {
        let bytes = payload.as_bytes();
        let ret = unsafe { libc::send(socket_fd, bytes.as_ptr() as *const _, bytes.len(), 0) };
        if ret < 0 {
            let err = unsafe { *libc::__errno_location() };
            return Err(napi::Error::from_reason(format!("send failed: errno {}", err)));
        }
        return Ok(());
    }

    let payload_bytes = payload.as_bytes();
    let mut iov = libc::iovec {
        iov_base: payload_bytes.as_ptr() as *mut libc::c_void,
        iov_len: payload_bytes.len(),
    };

    let fds_bytes_len = std::mem::size_of::<i32>() * fds_to_send.len();
    let cmsg_space = unsafe { libc::CMSG_SPACE(fds_bytes_len as u32) as usize };
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(napi::Error::from_reason("CMSG_FIRSTHDR returned null"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fds_bytes_len as u32) as usize;
        let data_ptr = libc::CMSG_DATA(cmsg) as *mut i32;
        std::ptr::copy_nonoverlapping(fds_to_send.as_ptr(), data_ptr, fds_to_send.len());
    }

    let ret = unsafe { libc::sendmsg(socket_fd, &msg, 0) };
    if ret < 0 {
        let err = unsafe { *libc::__errno_location() };
        return Err(napi::Error::from_reason(format!("sendmsg with SCM_RIGHTS failed: errno {}", err)));
    }

    Ok(())
}

#[napi]
pub fn register_surface_policy(
    surface_id: String,
    is_secure: bool,
    bootstrap_secret: String,
    active_document_id: String,
    allowed_channels: Vec<String>,
    allowed_methods: Vec<String>,
    origin: Option<String>,
) -> napi::Result<()> {
    let security_state = server::get_security_state();
    let mut policy = ipc_security::SurfacePolicy::new(
        surface_id.clone(),
        is_secure,
        active_document_id,
        bootstrap_secret,
        origin,
    );
    for ch in allowed_channels {
        policy.allowed_channels.insert(ch);
    }
    for method in allowed_methods {
        policy.allowed_methods.insert(method);
    }
    security_state.register_surface(policy);
    Ok(())
}

#[napi]
pub fn dispatch_to_surface(surface_id: String, data: napi::bindgen_prelude::Buffer) -> napi::Result<()> {
    dispatch_to_renderer(surface_id, None, axum::body::Bytes::from(data.to_vec()));
    Ok(())
}
