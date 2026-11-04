use std::env;
use std::sync::Arc;
use std::thread;

use axum::{
    body::Body,
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use log::{error, info, warn};
use once_cell::sync::OnceCell;
use tokio::sync::mpsc;
use tower_http::cors::{Any, CorsLayer};
use winit::event_loop::EventLoopProxy;

use crate::{get_trusted_origins, normalize_origin, APP_STATE, EngineCommand};

pub static WS_SENDERS: OnceCell<Arc<DashMap<String, mpsc::UnboundedSender<WsMessage>>>> = OnceCell::new();
pub static WS_PENDING: OnceCell<Arc<DashMap<String, std::collections::VecDeque<Vec<u8>>>>> = OnceCell::new();

pub const WS_PENDING_MAX_FRAMES: usize = 1024;

pub fn dispatch_to_renderer(window_id: String, pane_id: Option<String>, data: axum::body::Bytes) {
    let msg = axum::extract::ws::Message::Binary(data.to_vec());
    if let Some(senders) = WS_SENDERS.get() {
        match pane_id {
            Some(p) => {
                let client_id = format!("{}:{}", window_id, p);
                let alt_client_id = if p == "main" { Some(window_id.clone()) } else { None };

                let maybe_tx = senders.get(&client_id)
                    .or_else(|| alt_client_id.as_ref().and_then(|id| senders.get(id)));

                if let Some(tx) = maybe_tx {
                    if tx.send(msg).is_err() {
                        if let Some(pending) = WS_PENDING.get() {
                            let mut q = pending.entry(client_id).or_insert_with(std::collections::VecDeque::new);
                            if q.len() < WS_PENDING_MAX_FRAMES {
                                q.push_back(data.to_vec());
                            }
                        }
                    }
                } else {
                    // Not connected yet — queue it
                    if let Some(pending) = WS_PENDING.get() {
                        let mut q = pending.entry(client_id).or_insert_with(std::collections::VecDeque::new);
                        if q.len() < WS_PENDING_MAX_FRAMES {
                            q.push_back(data.to_vec());
                        }
                    }
                }
            },
            None => {
                // Broadcast to all panes of this window
                let mut found_any = false;
                for entry in senders.iter() {
                    if entry.key().starts_with(&format!("{}:", window_id)) || entry.key() == &window_id {
                        let _ = entry.value().send(msg.clone());
                        found_any = true;
                    }
                }

                if !found_any {
                    // Fallback to window-level queue if no panes are connected
                    if let Some(pending) = WS_PENDING.get() {
                        let mut q = pending.entry(window_id).or_insert_with(std::collections::VecDeque::new);
                        if q.len() < WS_PENDING_MAX_FRAMES {
                            q.push_back(data.to_vec());
                        }
                    }
                }
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct WsQuery {
    token: String,
    id: String,
    #[serde(rename = "paneId")]
    pane_id: Option<String>,
}

#[derive(Clone)]
struct ServerState {
    proxy: EventLoopProxy<EngineCommand>,
    token: String,
    ws_senders: Arc<DashMap<String, mpsc::UnboundedSender<WsMessage>>>,
}

pub fn start_ipc_server(
    server_proxy: EventLoopProxy<EngineCommand>,
    server_token: String,
    port_tx: std::sync::mpsc::Sender<u16>,
) {
    thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                error!("Failed to build tokio runtime: {}", e);
                return;
            }
        };

        rt.block_on(async move {
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!("Failed to bind Tokio TCP Listener: {}", e);
                    let _ = port_tx.send(0);
                    return;
                }
            };

            let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
            info!("Rust: Tokio/Axum IPC Server listening on port {}", actual_port);

            // Update port in state
            if let Some(state) = APP_STATE.get() {
                if let Ok(mut s) = state.lock() {
                    s.ipc_server_port = actual_port;
                }
            }

            let _ = port_tx.send(actual_port);

            let ws_senders_map: Arc<DashMap<String, mpsc::UnboundedSender<WsMessage>>> = Arc::new(DashMap::new());
            WS_SENDERS.set(ws_senders_map.clone()).ok();

            let ws_pending_map: Arc<DashMap<String, std::collections::VecDeque<Vec<u8>>>> = Arc::new(DashMap::new());
            WS_PENDING.set(ws_pending_map.clone()).ok();

            let state = ServerState {
                proxy: server_proxy,
                token: server_token,
                ws_senders: ws_senders_map,
            };

            let cors = CorsLayer::new()
                .allow_origin([
                    "http://127.0.0.1".parse::<HeaderValue>().unwrap(),
                    "http://localhost".parse::<HeaderValue>().unwrap(),
                    "null".parse::<HeaderValue>().unwrap(),
                ])
                .allow_methods(Any)
                .allow_headers(vec![
                    header::CONTENT_TYPE,
                    header::ACCEPT,
                    header::ACCEPT_LANGUAGE,
                    header::HeaderName::from_static("x-lotus-auth"),
                    header::HeaderName::from_static("x-lotus-window-id"),
                ]);

            let app = Router::new()
                .route("/batch", post(handle_batch))
                .route("/ipc/:channel", post(handle_ipc))
                .route("/resource/*path", get(handle_resource))
                .route("/ws", get(handle_ws_upgrade))
                .layer(cors)
                .with_state(state);

            if let Err(e) = axum::serve(listener, app).await {
                error!("Axum server error: {}", e);
            }
        });
    });
}

async fn handle_batch(
    State(state): State<ServerState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if headers.get("x-lotus-auth").and_then(|h| h.to_str().ok()) != Some(&state.token) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    if let Some(origin_val) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        let trusted_set = get_trusted_origins();
        let is_trusted = if origin_val == "null" {
            trusted_set.contains("lotus-resource://localhost")
        } else if let Some(norm) = normalize_origin(origin_val) {
            trusted_set.contains(&norm)
        } else {
            false
        };

        if !is_trusted {
            warn!("Rust: Rejected batch IPC request from untrusted origin: {}", origin_val);
            return (StatusCode::FORBIDDEN, "Forbidden").into_response();
        }
    }

    let window_id = headers.get("x-lotus-window-id")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    let _ = state.proxy.send_event(EngineCommand::IpcMessage(window_id, body.to_vec()));
    (StatusCode::OK, "ok").into_response()
}

async fn handle_ipc(
    State(state): State<ServerState>,
    Path(channel): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if headers.get("x-lotus-auth").and_then(|h| h.to_str().ok()) != Some(&state.token) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    if let Some(origin_val) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        let trusted_set = get_trusted_origins();
        let is_trusted = if origin_val == "null" {
            trusted_set.contains("lotus-resource://localhost")
        } else if let Some(norm) = normalize_origin(origin_val) {
            trusted_set.contains(&norm)
        } else {
            false
        };

        if !is_trusted {
            warn!("Rust: Rejected IPC request from untrusted origin: {}", origin_val);
            return (StatusCode::FORBIDDEN, "Forbidden").into_response();
        }
    }

    let window_id = headers.get("x-lotus-window-id")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    let channel_decoded = urlencoding::decode(&channel).unwrap_or(std::borrow::Cow::Borrowed(&channel)).into_owned();
    let mut msg = Vec::new();
    if let Ok(_) = rmp_serde::encode::write(&mut msg, &vec![(channel_decoded, body.to_vec())]) {
        let _ = state.proxy.send_event(EngineCommand::IpcMessage(window_id, msg));
    }

    (StatusCode::OK, "ok").into_response()
}

async fn handle_resource(
    Path(path): Path<String>,
) -> impl IntoResponse {
    let root = env::current_dir().unwrap_or_default();
    let mut full_path = root.clone();
    full_path.push(path.trim_start_matches('/'));

    match (full_path.canonicalize(), root.canonicalize()) {
        (Ok(canonical_full), Ok(canonical_root)) => {
            if !canonical_full.starts_with(&canonical_root) {
                warn!("Blocked directory traversal attempt for {:?}", full_path);
                return (StatusCode::FORBIDDEN, "Forbidden").into_response();
            }

            if canonical_full.is_file() {
                match tokio::fs::read(&canonical_full).await {
                    Ok(content) => {
                        let mime = mime_guess::from_path(&canonical_full).first_or_octet_stream();
                        let mut resp = Response::new(Body::from(content));
                        resp.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_str(mime.as_ref())
                                .unwrap_or(HeaderValue::from_static("application/octet-stream")),
                        );
                        resp
                    }
                    Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Error reading file").into_response(),
                }
            } else {
                (StatusCode::NOT_FOUND, "Not Found").into_response()
            }
        },
        _ => {
            (StatusCode::NOT_FOUND, "Not Found").into_response()
        }
    }
}

async fn handle_ws_upgrade(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<ServerState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if query.token != state.token {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let host_ok = headers.get("host")
        .and_then(|v| v.to_str().ok())
        .map(|h| h == "127.0.0.1" || h.starts_with("127.0.0.1:") || h == "localhost" || h.starts_with("localhost:"))
        .unwrap_or(false);

    if !host_ok {
        warn!("Rust: Rejected WebSocket upgrade with invalid Host header");
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }

    if let Some(origin_val) = headers.get("origin").and_then(|v| v.to_str().ok()) {
        let trusted_set = get_trusted_origins();
        let is_trusted = if origin_val == "null" {
            trusted_set.contains("lotus-resource://localhost")
        } else if let Some(norm) = normalize_origin(origin_val) {
            trusted_set.contains(&norm)
        } else {
            false
        };

        if !is_trusted {
            warn!("Rust: Rejected WebSocket upgrade from untrusted origin: {}", origin_val);
            return (StatusCode::FORBIDDEN, "Forbidden").into_response();
        }
    }

    let full_id = match query.pane_id {
        Some(p) => format!("{}:{}", query.id, p),
        None => query.id.clone(),
    };

    ws.on_upgrade(move |socket| handle_ws_client(socket, full_id, state))
}

async fn handle_ws_client(socket: WebSocket, client_id: String, state: ServerState) {
    info!("Rust: WebSocket client connected for client {}", client_id);
    let (mut sender, mut receiver) = socket.split();

    let (tx, mut rx) = mpsc::unbounded_channel();
    state.ws_senders.insert(client_id.clone(), tx.clone());

    if let Some(pending) = WS_PENDING.get() {
        if let Some(mut entry) = pending.get_mut(&client_id) {
            while let Some(frame) = entry.pop_front() {
                let _ = tx.send(WsMessage::Binary(frame.into()));
            }
        }

        let parts: Vec<&str> = client_id.split(':').collect();
        if parts.len() > 1 && (parts[1] == "main" || parts[1] == parts[0]) {
            if let Some(mut entry) = pending.get_mut(parts[0]) {
                while let Some(frame) = entry.pop_front() {
                    let _ = tx.send(WsMessage::Binary(frame.into()));
                }
            }
        }
    }

    let send_task = async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(msg).await.is_err() {
                break;
            }
        }
    };

    let proxy = state.proxy.clone();
    let client_id_clone = client_id.clone();
    let recv_task = async move {
        let mut batch_buffer: Vec<Vec<u8>> = Vec::with_capacity(32);
        let mut batch_bytes: usize = 0;
        const BATCH_FLUSH_SIZE_BYTES: usize = 5 * 1024 * 1024; // 5 MB

        loop {
            tokio::select! {
                msg_opt = receiver.next() => {
                    match msg_opt {
                        Some(Ok(WsMessage::Binary(bin))) => {
                            batch_bytes += bin.len();
                            batch_buffer.push(bin);
                        }
                        Some(Ok(WsMessage::Text(txt))) => {
                            batch_bytes += txt.len();
                            batch_buffer.push(txt.into_bytes());
                        }
                        Some(Err(_)) | None => {
                            if !batch_buffer.is_empty() {
                                let msgs = std::mem::take(&mut batch_buffer);
                                let _ = proxy.send_event(EngineCommand::IpcMessages(client_id_clone.clone(), msgs));
                            }
                            break;
                        }
                        _ => {}
                    }

                    if batch_buffer.len() >= 32 || batch_bytes >= BATCH_FLUSH_SIZE_BYTES {
                        batch_bytes = 0;
                        let msgs = std::mem::take(&mut batch_buffer);
                        let _ = proxy.send_event(EngineCommand::IpcMessages(client_id_clone.clone(), msgs));
                    }
                }

                _ = tokio::time::sleep(std::time::Duration::from_micros(50)), if !batch_buffer.is_empty() => {
                    batch_bytes = 0;
                    let msgs = std::mem::take(&mut batch_buffer);
                    let _ = proxy.send_event(EngineCommand::IpcMessages(client_id_clone.clone(), msgs));
                }
            }
        }
    };

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    info!("Rust: WebSocket client disconnected for client {}", client_id);
    state.ws_senders.remove(&client_id);
}
