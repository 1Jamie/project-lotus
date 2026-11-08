use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use http::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use http::StatusCode;
use log::{debug, error, info, trace, warn};
use servo::{
    ConsoleLogLevel, LoadStatus, WebResourceLoad, WebResourceResponse, WebViewDelegate,
};
use winit::event_loop::EventLoopProxy;
use winit::window::{CursorIcon, Window};

use crate::server::WS_PENDING;
use crate::window::SelectPromptWrapper;
use crate::{APP_STATE, EVENT_LOOP_PROXY, EngineCommand};

pub const SELECT_MENU_HTML: &str = include_str!("../resources/select-menu.html");
pub const SURFACE_CRASH_HTML: &str = include_str!("../resources/surface-crash.html");

pub struct LotusPaneDelegate {
    pub window: Arc<Window>,
    pub window_id: String,
    pub pane_id: String,
    pub proxy: EventLoopProxy<EngineCommand>,
    pub ready_to_repaint: Arc<AtomicBool>,
}

impl WebViewDelegate for LotusPaneDelegate {
    fn show_embedder_control(&self, _webview: servo::WebView, embedder_control: servo::EmbedderControl) {
        if let servo::EmbedderControl::SelectElement(select_element) = embedder_control {
            let _ = self.proxy.send_event(EngineCommand::ShowSelectControl {
                window_id: self.window_id.clone(),
                pane_id: self.pane_id.clone(),
                prompt: SelectPromptWrapper(select_element),
            });
        }
    }

    fn hide_embedder_control(&self, _webview: servo::WebView, control_id: servo::EmbedderControlId) {
        let _ = self.proxy.send_event(EngineCommand::HideSelectControl {
            window_id: self.window_id.clone(),
            control_id,
        });
    }

    fn notify_load_status_changed(&self, webview: servo::WebView, status: LoadStatus) {
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
                eprintln!("[PROFILE] Window {} Pane {} status {:?} reached in {:?}", self.window_id, self.pane_id, status, elapsed);
            }
        }

        info!("Rust: LoadStatus changed to {:?} for {} pane {}", status, self.window_id, self.pane_id);

        let status_str = match status {
            LoadStatus::Started => "started",
            LoadStatus::HeadParsed => "head-parsed",
            LoadStatus::Complete => "complete",
        };

        if status == LoadStatus::Started {
            let client_id = format!("{}:{}", self.window_id, self.pane_id);
            if let Some(pending) = WS_PENDING.get() {
                if pending.contains_key(&client_id) {
                    info!("Rust: Purging WS_PENDING for {} due to LoadStatus::Started", client_id);
                    pending.remove(&client_id);
                }
            }
        }

        let current_url = webview.url().map(|u| u.to_string());
        let client_id = format!("{}:{}", self.window_id, self.pane_id);
        if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
            "event": "load-status",
            "window_id": self.window_id,
            "pane_id": self.pane_id,
            "status": status_str,
            "url": current_url
        })) {
            if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                let _ = proxy.send_event(EngineCommand::IpcMessage(client_id, msg));
            }
        }
    }

    fn notify_new_frame_ready(&self, _webview: servo::WebView) {
        trace!("Rust: [Lifecycle] notify_new_frame_ready for window {} pane {}", self.window_id, self.pane_id);
        if self.ready_to_repaint.swap(false, Ordering::SeqCst) {
            let _ = self.proxy.send_event(EngineCommand::NewFrameReady(self.window_id.clone(), self.pane_id.clone()));
        }
    }

    fn notify_animating_changed(&self, _webview: servo::WebView, animating: bool) {
        trace!("Rust: Animating changed for {} pane {} -> {}", self.window_id, self.pane_id, animating);
        let _ = self.proxy.send_event(EngineCommand::AnimatingChanged(self.window_id.clone(), self.pane_id.clone(), animating));
    }

    fn notify_page_title_changed(&self, _webview: servo::WebView, title: Option<String>) {
         info!("Rust: Title changed for {} pane {} to {:?}", self.window_id, self.pane_id, title);
         let client_id = format!("{}:{}", self.window_id, self.pane_id);
         if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
            "event": "title-changed",
            "window_id": self.window_id,
            "pane_id": self.pane_id,
            "title": title
        })) {
            if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                let _ = proxy.send_event(EngineCommand::IpcMessage(client_id, msg));
            }
        }
    }

    fn show_console_message(&self, _webview: servo::WebView, _level: ConsoleLogLevel, message: String) {
        info!("Rust Console [{}|{}]: {}", self.window_id, self.pane_id, message);
    }

    fn load_web_resource(&self, _webview: servo::WebView, load: WebResourceLoad) {
        let url = load.request().url.clone();
        let url_str = url.as_str();

        if url_str.starts_with("lotus-resource://") {
            let path_in_url = url.path();
            let relative_path = path_in_url.trim_start_matches('/');
            let path_buf = PathBuf::from(relative_path);

            if relative_path == "select-menu.html" || relative_path == "__lotus_select_menu.html" {
                let mut headers = HeaderMap::new();
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
                let response = WebResourceResponse::new(url)
                    .headers(headers)
                    .status_code(StatusCode::OK);
                let mut intercepted = load.intercept(response);
                intercepted.send_body_data(SELECT_MENU_HTML.as_bytes().to_vec());
                intercepted.finish();
                return;
            }

            if relative_path == "surface-crash.html" || relative_path == "__lotus_surface_crash.html" {
                let mut headers = HeaderMap::new();
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
                let response = WebResourceResponse::new(url)
                    .headers(headers)
                    .status_code(StatusCode::OK);
                let mut intercepted = load.intercept(response);
                intercepted.send_body_data(SURFACE_CRASH_HTML.as_bytes().to_vec());
                intercepted.finish();
                return;
            }

            // 1. Check LRU Cache
            if let Some(state) = APP_STATE.get() {
                if let Ok(mut s) = state.lock() {
                    if let Some((data, mime_str)) = s.resource_cache.get(&path_buf) {
                        debug!("Rust: Cache hit for {:?}", path_buf);
                        let mut headers = HeaderMap::new();
                        if let Ok(val) = HeaderValue::from_str(mime_str) {
                             headers.insert(CONTENT_TYPE, val);
                        }
                        let response = WebResourceResponse::new(url)
                            .headers(headers)
                            .status_code(StatusCode::OK);
                        let mut intercepted = load.intercept(response);
                        intercepted.send_body_data(data.clone());
                        intercepted.finish();
                        return;
                    }
                }
            }

            // 2. Check VFS
            let mut vfs_data = None;
            if let Some(state) = APP_STATE.get() {
                if let Ok(s) = state.lock() {
                    if let Some(vfs) = &s.vfs {
                        vfs_data = vfs.read_file(relative_path);
                    }
                }
            }

            if let Some(data) = vfs_data {
                debug!("Rust: Loaded from VFS: {}", relative_path);
                let mime = mime_guess::from_path(relative_path).first_or_octet_stream();
                let mime_str = mime.to_string();

                // Cache it
                if let Some(state) = APP_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        s.resource_cache.put(path_buf.clone(), (data.clone(), mime_str.clone()));
                    }
                }

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
                return;
            }

            // 3. Fallback to physical filesystem
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
                let full_path = root.join(relative_path);

                // Security: Prevent directory traversal attacks.
                match (full_path.canonicalize(), root.canonicalize()) {
                    (Ok(canonical_full), Ok(canonical_root)) => {
                        if !canonical_full.starts_with(&canonical_root) {
                            warn!("Rust: Blocked directory traversal attempt for {:?}", full_path);
                            let response = WebResourceResponse::new(url)
                                .status_code(StatusCode::FORBIDDEN);
                            load.intercept(response).finish();
                            return;
                        }
                        // Path is safe — serve it
                        debug!("Rust: Loading resource from disk: {:?}", canonical_full);
                        match fs::read(&canonical_full) {
                            Ok(data) => {
                                let mime = mime_guess::from_path(&canonical_full).first_or_octet_stream();
                                let mime_str = mime.to_string();

                                // Cache it
                                if let Some(state) = APP_STATE.get() {
                                    if let Ok(mut s) = state.lock() {
                                        s.resource_cache.put(path_buf.clone(), (data.clone(), mime_str.clone()));
                                    }
                                }

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
                                error!("Failed to read file {:?}: {}", canonical_full, e);
                                let response = WebResourceResponse::new(url)
                                    .status_code(StatusCode::NOT_FOUND);
                                load.intercept(response).finish();
                            }
                        }
                    },
                    _ => {
                        debug!("Rust: Resource not found: {:?}", full_path);
                        let response = WebResourceResponse::new(url)
                            .status_code(StatusCode::NOT_FOUND);
                        load.intercept(response).finish();
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

        trace!("Rust: [UI] Cursor changed to {:?} for {} pane {}", winit_cursor, self.window_id, self.pane_id);
        self.window.set_cursor(winit_cursor);
    }
}
