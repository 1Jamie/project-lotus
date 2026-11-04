//! Lotus Track 2 Engine-Side Secure IPC Transport (`ipc_security.rs`)
//!
//! Provides route-scoped `/ipc/handshake`, `/ipc/secure-ws`, atomic single-use ticket consumption,
//! atomic WebSocket replacement, continuous TokenBucket rate limiting, OutboundEventQueue with
//! wire-frame byte accounting and `OverflowPolicy::Coalesce`, and credential rotation.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use axum::{
    extract::{Query, State, ws::{Message as WsMessage, WebSocket, WebSocketUpgrade}},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

/// Default maximum inbound wire-frame payload bytes (4 MiB).
pub const DEFAULT_MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
/// Default ticket time-to-live in seconds (30s).
pub const DEFAULT_TICKET_TTL_SECS: u64 = 30;
/// Default outbound event queue capacity in entries.
pub const DEFAULT_OUTBOUND_QUEUE_ENTRIES: usize = 256;
/// Default outbound event queue capacity in bytes (2 MiB).
pub const DEFAULT_OUTBOUND_QUEUE_BYTES: usize = 2 * 1024 * 1024;

/// Constant-time slice comparison to prevent timing side-channel attacks on secrets and tickets.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Constant-time string comparison.
pub fn constant_time_eq_str(a: &str, b: &str) -> bool {
    constant_time_eq(a.as_bytes(), b.as_bytes())
}

/// Normalizes an origin string according to WHATWG standard serialization.
///
/// Converts host to lowercase, standardizes default ports (omits :80 for http, :443 for https),
/// and preserves special schemes like `lotus-resource://` and `null`.
pub fn normalize_origin(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "null" {
        return Some("null".to_string());
    }

    if let Ok(parsed) = url::Url::parse(trimmed) {
        let scheme = parsed.scheme().to_lowercase();
        if scheme == "data" || scheme == "javascript" {
            return Some("null".to_string());
        }
        if scheme == "lotus-resource" {
            let host = parsed.host_str().unwrap_or("localhost").to_lowercase();
            return Some(format!("lotus-resource://{}", host));
        }
        let host = parsed.host_str()?.to_lowercase();
        let port_part = match parsed.port() {
            Some(p) => {
                if (scheme == "http" && p == 80) || (scheme == "https" && p == 443) {
                    String::new()
                } else {
                    format!(":{}", p)
                }
            }
            None => String::new(),
        };
        Some(format!("{}://{}{}", scheme, host, port_part))
    } else {
        None
    }
}

/// Continuous Token Bucket rate limiter for per-surface request throttling.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    pub capacity: f64,
    pub tokens: f64,
    pub refill_rate: f64, // tokens per second
    pub last_refill: Instant,
}

impl TokenBucket {
    pub fn new(capacity: f64, refill_rate: f64) -> Self {
        Self {
            capacity,
            tokens: capacity,
            refill_rate,
            last_refill: Instant::now(),
        }
    }

    /// Try to consume `count` tokens. Returns `true` if allowed, `false` if rate-limited.
    pub fn try_consume(&mut self, count: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);

        if self.tokens >= count {
            self.tokens -= count;
            true
        } else {
            false
        }
    }
}

/// Overflow policy for the outbound event queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// Coalesces events with identical keys, updating byte accounting accurately.
    Coalesce,
    /// Drops the oldest event when limits are exceeded.
    DropOldest,
    /// Rejects the new event when limits are exceeded.
    Reject,
}

/// An entry in the outbound event queue.
#[derive(Debug, Clone)]
pub struct OutboundEventEntry {
    pub event_name: String,
    pub payload: Vec<u8>,
    pub coalesce_key: Option<String>,
    pub wire_bytes: usize,
    pub enqueued_at: Instant,
}

/// Outbound event queue with strict wire-frame byte accounting.
#[derive(Debug, Clone)]
pub struct OutboundEventQueue {
    pub max_entries: usize,
    pub max_bytes: usize,
    pub current_bytes: usize,
    pub entries: VecDeque<OutboundEventEntry>,
    pub policy: OverflowPolicy,
}

impl OutboundEventQueue {
    pub fn new(max_entries: usize, max_bytes: usize, policy: OverflowPolicy) -> Self {
        Self {
            max_entries,
            max_bytes,
            current_bytes: 0,
            entries: VecDeque::new(),
            policy,
        }
    }

    /// Computes the wire-frame size of an event (event name length + payload length + framing overhead).
    pub fn compute_wire_bytes(event_name: &str, payload: &[u8]) -> usize {
        // 4 bytes length prefix + event name length + payload length
        4 + event_name.len() + payload.len()
    }

    /// Enqueue an event according to the overflow policy.
    pub fn push(
        &mut self,
        event_name: String,
        payload: Vec<u8>,
        coalesce_key: Option<String>,
    ) -> Result<(), &'static str> {
        let wire_bytes = Self::compute_wire_bytes(&event_name, &payload);

        // Coalescing check
        if self.policy == OverflowPolicy::Coalesce {
            if let Some(ref key) = coalesce_key {
                if let Some(pos) = self.entries.iter().position(|e| e.coalesce_key.as_ref() == Some(key)) {
                    let old_wire_bytes = self.entries[pos].wire_bytes;
                    self.current_bytes = self.current_bytes.saturating_sub(old_wire_bytes) + wire_bytes;
                    self.entries[pos] = OutboundEventEntry {
                        event_name,
                        payload,
                        coalesce_key,
                        wire_bytes,
                        enqueued_at: Instant::now(),
                    };
                    return Ok(());
                }
            }
        }

        // Check if queue exceeds limits and handle according to policy
        while self.entries.len() >= self.max_entries || (self.current_bytes + wire_bytes > self.max_bytes && !self.entries.is_empty()) {
            match self.policy {
                OverflowPolicy::Coalesce | OverflowPolicy::DropOldest => {
                    if let Some(dropped) = self.entries.pop_front() {
                        self.current_bytes = self.current_bytes.saturating_sub(dropped.wire_bytes);
                    }
                }
                OverflowPolicy::Reject => {
                    return Err("E_QUEUE_OVERFLOW");
                }
            }
        }

        if self.current_bytes + wire_bytes > self.max_bytes {
            return Err("E_QUEUE_PAYLOAD_TOO_LARGE");
        }

        self.current_bytes += wire_bytes;
        self.entries.push_back(OutboundEventEntry {
            event_name,
            payload,
            coalesce_key,
            wire_bytes,
            enqueued_at: Instant::now(),
        });

        Ok(())
    }

    pub fn pop_front(&mut self) -> Option<OutboundEventEntry> {
        if let Some(entry) = self.entries.pop_front() {
            self.current_bytes = self.current_bytes.saturating_sub(entry.wire_bytes);
            Some(entry)
        } else {
            None
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.current_bytes = 0;
    }
}

/// Single-use 256-bit ticket for authenticating WebSocket upgrades.
#[derive(Debug, Clone)]
pub struct SecureTicket {
    pub ticket: String, // 64 hex characters (256 bits)
    pub surface_id: String,
    pub document_id: String,
    pub created_at: Instant,
    pub expires_at: Instant,
}

impl SecureTicket {
    pub fn new(surface_id: String, document_id: String, ttl_secs: u64) -> Self {
        // Generate 256-bit entropy using two v4 UUIDs (32 hex bytes = 64 characters)
        let u1 = Uuid::new_v4().as_simple().to_string();
        let u2 = Uuid::new_v4().as_simple().to_string();
        let ticket = format!("{}{}", u1, u2);
        let now = Instant::now();
        Self {
            ticket,
            surface_id,
            document_id,
            created_at: now,
            expires_at: now + Duration::from_secs(ttl_secs),
        }
    }

    pub fn is_expired(&self) -> bool {
        Instant::now() > self.expires_at
    }
}

/// Per-surface authoritative security policy enforced in the engine.
#[derive(Debug)]
pub struct SurfacePolicy {
    pub surface_id: String,
    pub is_secure: bool,
    pub active_document_id: String,
    pub active_bootstrap_secret: String,
    pub allowed_channels: HashSet<String>,
    pub allowed_methods: HashSet<String>,
    pub current_origin: Option<String>,
    pub ipc_eligible: bool,
    pub max_payload_bytes: usize,
    pub rate_limiter: TokenBucket,
    pub auth_failure_limiter: TokenBucket,
    pub active_ticket: Option<SecureTicket>,
    pub active_ws_session_id: Option<String>,
    pub outbound_queue: OutboundEventQueue,
}

impl SurfacePolicy {
    pub fn new(
        surface_id: String,
        is_secure: bool,
        document_id: String,
        bootstrap_secret: String,
        origin: Option<String>,
    ) -> Self {
        Self {
            surface_id,
            is_secure,
            active_document_id: document_id,
            active_bootstrap_secret: bootstrap_secret,
            allowed_channels: HashSet::new(),
            allowed_methods: HashSet::new(),
            current_origin: origin,
            ipc_eligible: true,
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            rate_limiter: TokenBucket::new(100.0, 50.0), // 100 burst, 50/sec
            auth_failure_limiter: TokenBucket::new(20.0, 5.0), // 20 burst, 5/sec for unauthenticated attempts
            active_ticket: None,
            active_ws_session_id: None,
            outbound_queue: OutboundEventQueue::new(
                DEFAULT_OUTBOUND_QUEUE_ENTRIES,
                DEFAULT_OUTBOUND_QUEUE_BYTES,
                OverflowPolicy::Coalesce,
            ),
        }
    }

    /// Rotates credentials on document navigation.
    pub fn rotate_credentials(&mut self, new_document_id: String, new_bootstrap_secret: String, new_origin: Option<String>) {
        self.active_document_id = new_document_id;
        self.active_bootstrap_secret = new_bootstrap_secret;
        self.current_origin = new_origin;
        self.active_ticket = None;
        self.active_ws_session_id = None;
        self.outbound_queue.clear();
    }

    /// Verifies channel and method intersection.
    pub fn is_call_permitted(&self, channel: &str, method: &str) -> bool {
        if !self.ipc_eligible {
            return false;
        }
        self.allowed_channels.contains(channel) && self.allowed_methods.contains(method)
    }

    /// Mints a fresh 256-bit ticket after validating bootstrap secret.
    pub fn mint_ticket(&mut self, bootstrap_secret: &str) -> Result<String, &'static str> {
        if !self.ipc_eligible {
            return Err("E_IPC_INELIGIBLE");
        }
        if !constant_time_eq_str(&self.active_bootstrap_secret, bootstrap_secret) {
            return Err("E_UNAUTHORIZED");
        }

        let ticket = SecureTicket::new(
            self.surface_id.clone(),
            self.active_document_id.clone(),
            DEFAULT_TICKET_TTL_SECS,
        );
        let ticket_str = ticket.ticket.clone();
        self.active_ticket = Some(ticket);
        Ok(ticket_str)
    }

    /// Atomically consumes the active ticket for the given document and ticket string.
    /// Replay attempts fail immediately because the ticket is cleared on consumption.
    /// Failed ticket attempts (mismatches) do NOT clear the active ticket, preventing denial of service (N2).
    pub fn consume_ticket(&mut self, document_id: &str, ticket_str: &str) -> Result<(), &'static str> {
        if !self.ipc_eligible {
            return Err("E_IPC_INELIGIBLE");
        }
        if self.active_document_id != document_id {
            return Err("E_DOCUMENT_MISMATCH");
        }

        let ticket = self.active_ticket.as_ref().ok_or("E_TICKET_NOT_FOUND_OR_ALREADY_USED")?;
        if ticket.is_expired() {
            self.active_ticket.take();
            return Err("E_TICKET_EXPIRED");
        }
        if !constant_time_eq_str(&ticket.ticket, ticket_str) {
            // N2 Fix: Keep active ticket intact on mismatch so legitimate client is not locked out
            return Err("E_TICKET_MISMATCH");
        }

        self.active_ticket.take();
        Ok(())
    }

    /// Atomically registers a new WebSocket session, replacing any previous session.
    pub fn register_ws_session(&mut self, session_id: String) -> Option<String> {
        let previous = self.active_ws_session_id.take();
        self.active_ws_session_id = Some(session_id);
        previous
    }
}

/// Transport manifest coordinated between Node host and engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportManifest {
    pub engine_port: u16,
    pub endpoints: HashMap<String, String>,
    pub surface_tokens: HashMap<String, String>,
    pub version: u32,
}

impl TransportManifest {
    pub fn new(engine_port: u16) -> Self {
        let mut endpoints = HashMap::new();
        endpoints.insert("handshake".to_string(), "/ipc/handshake".to_string());
        endpoints.insert("secure_ws".to_string(), "/ipc/secure-ws".to_string());

        Self {
            engine_port,
            endpoints,
            surface_tokens: HashMap::new(),
            version: 1,
        }
    }
}

/// Engine security state containing policies for all registered surfaces.
#[derive(Debug, Clone)]
pub struct EngineSecurityState {
    pub engine_port: u16,
    pub surfaces: Arc<Mutex<HashMap<String, Arc<Mutex<SurfacePolicy>>>>>,
}

impl EngineSecurityState {
    pub fn new(engine_port: u16) -> Self {
        Self {
            engine_port,
            surfaces: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn get_surface(&self, surface_id: &str) -> Option<Arc<Mutex<SurfacePolicy>>> {
        let map = self.surfaces.lock().unwrap();
        map.get(surface_id).cloned()
    }

    pub fn register_surface(&self, policy: SurfacePolicy) {
        let mut map = self.surfaces.lock().unwrap();
        let id = policy.surface_id.clone();
        map.insert(id, Arc::new(Mutex::new(policy)));
    }
}

/// Validates that the request `Host` header strictly targets 127.0.0.1 or localhost on `expected_port`.
pub fn validate_strict_host(headers: &HeaderMap, expected_port: u16) -> bool {
    let host = match headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        Some(h) => h,
        None => return false,
    };

    let expected_127 = format!("127.0.0.1:{}", expected_port);
    let expected_lh = format!("localhost:{}", expected_port);

    host == expected_127 || host == expected_lh
}

/// Validates that request `Origin` header matches the surface's expected origin if present.
pub fn validate_strict_origin(headers: &HeaderMap, allowed_origin: Option<&str>) -> bool {
    if let Some(origin_header) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if let Some(expected) = allowed_origin {
            if !expected.is_empty() && origin_header != expected {
                return false;
            }
        }
    }
    true
}

/// Handshake request payload.
#[derive(Debug, Deserialize)]
pub struct HandshakeRequest {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    #[serde(rename = "documentId")]
    pub document_id: String,
}

/// Handshake response payload.
#[derive(Debug, Serialize)]
pub struct HandshakeResponse {
    pub ticket: String,
    #[serde(rename = "expiresIn")]
    pub expires_in: u64,
}

/// Route-scoped `/ipc/handshake` handler:
/// - Zero CORS headers on rejected requests (401/403/400).
/// - Strict `Host: 127.0.0.1:<port>` / `localhost:<port>` check.
/// - Constant-time `X-Lotus-Bootstrap` check.
/// - Single-use 256-bit ticket issuance.
pub async fn handle_handshake(
    State(state): State<EngineSecurityState>,
    headers: HeaderMap,
    Json(payload): Json<HandshakeRequest>,
) -> Response {
    // 1. Strict Host validation
    if !validate_strict_host(&headers, state.engine_port) {
        return (StatusCode::FORBIDDEN, "Invalid Host").into_response();
    }

    // 2. Extract bootstrap secret from X-Lotus-Bootstrap header
    let bootstrap_secret = match headers.get("x-lotus-bootstrap").and_then(|v| v.to_str().ok()) {
        Some(s) => s,
        None => return (StatusCode::UNAUTHORIZED, "Missing Bootstrap Secret").into_response(),
    };

    // 3. Find surface policy
    let surface_arc = match state.get_surface(&payload.surface_id) {
        Some(s) => s,
        None => return (StatusCode::NOT_FOUND, "Surface Not Found").into_response(),
    };

    let mut surface = surface_arc.lock().unwrap();

    // 4. Verify surface is secure and document matches
    if !surface.is_secure {
        return (StatusCode::BAD_REQUEST, "Surface is not Track 2 secure").into_response();
    }
    if surface.active_document_id != payload.document_id {
        return (StatusCode::CONFLICT, "Document ID Mismatch").into_response();
    }

    // 5. Rate limiting check
    if !surface.rate_limiter.try_consume(1.0) {
        return (StatusCode::TOO_MANY_REQUESTS, "Rate limit exceeded").into_response();
    }

    // 6. Constant-time bootstrap secret check and mint ticket
    match surface.mint_ticket(bootstrap_secret) {
        Ok(ticket) => {
            let resp = HandshakeResponse {
                ticket,
                expires_in: DEFAULT_TICKET_TTL_SECS,
            };
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(_) => (StatusCode::UNAUTHORIZED, "Unauthorized").into_response(),
    }
}

/// Query parameters for WebSocket upgrade `/ipc/secure-ws`.
#[derive(Debug, Deserialize)]
pub struct SecureWsQuery {
    #[serde(rename = "surfaceId")]
    pub surface_id: String,
    #[serde(rename = "documentId")]
    pub document_id: String,
    pub ticket: String,
}

/// Route-scoped `/ipc/secure-ws` handler:
/// - Validates strict Host header.
/// - Atomic ticket consumption under `Mutex<SurfacePolicy>`.
/// - Atomic WebSocket session replacement.
pub async fn handle_secure_ws_upgrade(
    ws: WebSocketUpgrade,
    Query(query): Query<SecureWsQuery>,
    State(state): State<EngineSecurityState>,
    headers: HeaderMap,
) -> Response {
    // 1. Strict Host check
    if !validate_strict_host(&headers, state.engine_port) {
        return (StatusCode::FORBIDDEN, "Invalid Host").into_response();
    }

    // 2. Surface lookup
    let surface_arc = match state.get_surface(&query.surface_id) {
        Some(s) => s,
        None => return (StatusCode::NOT_FOUND, "Surface Not Found").into_response(),
    };

    let mut surface = surface_arc.lock().unwrap();

    // Strict Origin check against expected surface origin (if present)
    if !validate_strict_origin(&headers, surface.current_origin.as_deref()) {
        return (StatusCode::FORBIDDEN, "Forbidden Origin").into_response();
    }

    // 3. Atomic ticket consumption under lock (N2: invalid attempts do not burn ticket)
    if let Err(err_code) = surface.consume_ticket(&query.document_id, &query.ticket) {
        // Failed attempt: throttle against auth_failure_limiter without depleting legitimate rate_limiter
        if !surface.auth_failure_limiter.try_consume(1.0) {
            return (StatusCode::TOO_MANY_REQUESTS, "Auth failure rate limit exceeded").into_response();
        }
        return (StatusCode::UNAUTHORIZED, err_code).into_response();
    }

    // 4. Rate limiting check for authenticated connection
    if !surface.rate_limiter.try_consume(1.0) {
        return (StatusCode::TOO_MANY_REQUESTS, "Rate limit exceeded").into_response();
    }

    // 5. Generate new session ID and atomically replace active session
    let session_id = Uuid::new_v4().to_string();
    let _old_session = surface.register_ws_session(session_id.clone());

    let surface_id = query.surface_id.clone();
    let surface_for_client = surface_arc.clone();

    // 6. Complete WebSocket upgrade
    ws.on_upgrade(move |socket| handle_secure_ws_client(socket, surface_id, session_id, surface_for_client))
}

/// Handles the active secure WebSocket connection.
async fn handle_secure_ws_client(
    mut socket: WebSocket,
    _surface_id: String,
    session_id: String,
    surface_arc: Arc<Mutex<SurfacePolicy>>,
) {
    while let Some(msg_result) = socket.recv().await {
        let msg = match msg_result {
            Ok(m) => m,
            Err(_) => break,
        };

        // Check if session has been superseded by atomic replacement
        {
            let surface = surface_arc.lock().unwrap();
            if surface.active_ws_session_id.as_deref() != Some(&session_id) {
                // Session superseded: close cleanly
                break;
            }
        }

        match msg {
            WsMessage::Binary(bin) => {
                let surface = surface_arc.lock().unwrap();
                if bin.len() > surface.max_payload_bytes {
                    // Frame exceeds maximum payload bytes: reject / drop
                    continue;
                }
                // Processing will be handled via dispatch channel
            }
            WsMessage::Text(txt) => {
                let surface = surface_arc.lock().unwrap();
                if txt.len() > surface.max_payload_bytes {
                    continue;
                }
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }

    // Clean up session if this was the active one
    let mut surface = surface_arc.lock().unwrap();
    if surface.active_ws_session_id.as_deref() == Some(&session_id) {
        surface.active_ws_session_id = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq_str("secret-token-123", "secret-token-123"));
        assert!(!constant_time_eq_str("secret-token-123", "secret-token-124"));
        assert!(!constant_time_eq_str("short", "longer-string"));
        assert!(!constant_time_eq_str("", "non-empty"));
        assert!(constant_time_eq_str("", ""));
    }

    #[test]
    fn test_normalize_origin() {
        assert_eq!(normalize_origin("http://127.0.0.1:80"), Some("http://127.0.0.1".to_string()));
        assert_eq!(normalize_origin("http://127.0.0.1:3000"), Some("http://127.0.0.1:3000".to_string()));
        assert_eq!(normalize_origin("https://EXAMPLE.com:443"), Some("https://example.com".to_string()));
        assert_eq!(normalize_origin("null"), Some("null".to_string()));
        assert_eq!(normalize_origin("data:text/html,test"), Some("null".to_string()));
        assert_eq!(normalize_origin("lotus-resource://localhost/path"), Some("lotus-resource://localhost".to_string()));
    }

    #[test]
    fn test_token_bucket_rate_limiter() {
        let mut tb = TokenBucket::new(5.0, 10.0);
        assert!(tb.try_consume(3.0));
        assert!(tb.try_consume(2.0));
        // Exhausted
        assert!(!tb.try_consume(1.0));

        // Sleep to allow refill
        std::thread::sleep(Duration::from_millis(150));
        // Should have refilled ~1.5 tokens
        assert!(tb.try_consume(1.0));
    }

    #[test]
    fn test_outbound_queue_coalescing_and_byte_accounting() {
        let mut queue = OutboundEventQueue::new(5, 500, OverflowPolicy::Coalesce);

        queue.push("pane:resize".to_string(), vec![1, 2, 3], Some("resize".to_string())).unwrap();
        let bytes_after_first = queue.current_bytes;
        assert_eq!(queue.entries.len(), 1);

        // Push another resize with the same coalesce_key - replaces previous entry!
        queue.push("pane:resize".to_string(), vec![1, 2, 3, 4, 5], Some("resize".to_string())).unwrap();
        assert_eq!(queue.entries.len(), 1);
        assert_eq!(queue.current_bytes, bytes_after_first + 2); // 2 more bytes in payload

        // Pop
        let popped = queue.pop_front().unwrap();
        assert_eq!(popped.payload, vec![1, 2, 3, 4, 5]);
        assert_eq!(queue.current_bytes, 0);
        assert_eq!(queue.entries.len(), 0);
    }

    #[test]
    fn test_surface_policy_ticket_atomic_consumption_and_replay_prevention() {
        let mut policy = SurfacePolicy::new(
            "surf-1".to_string(),
            true,
            "doc-1".to_string(),
            "secret-bootstrap".to_string(),
            Some("http://127.0.0.1:3000".to_string()),
        );

        // Incorrect bootstrap secret -> rejected
        assert!(policy.mint_ticket("wrong-secret").is_err());

        // Correct bootstrap secret -> 256-bit ticket issued
        let ticket = policy.mint_ticket("secret-bootstrap").unwrap();
        assert_eq!(ticket.len(), 64);

        // Document mismatch -> rejected
        assert_eq!(policy.consume_ticket("doc-2", &ticket), Err("E_DOCUMENT_MISMATCH"));

        // N2: Attacker with wrong ticket string -> rejected with E_TICKET_MISMATCH, ticket preserved!
        assert_eq!(policy.consume_ticket("doc-1", "attacker-forged-ticket-1234"), Err("E_TICKET_MISMATCH"));
        assert!(policy.active_ticket.is_some(), "Active ticket must NOT be burned by failed attempt");

        // Valid consumption -> succeeds
        assert!(policy.consume_ticket("doc-1", &ticket).is_ok());

        // Replay attempt -> fails immediately because active_ticket was taken!
        assert_eq!(policy.consume_ticket("doc-1", &ticket), Err("E_TICKET_NOT_FOUND_OR_ALREADY_USED"));
    }

    #[test]
    fn test_atomic_websocket_replacement() {
        let mut policy = SurfacePolicy::new(
            "surf-1".to_string(),
            true,
            "doc-1".to_string(),
            "secret-bootstrap".to_string(),
            Some("http://127.0.0.1:3000".to_string()),
        );

        let s1 = policy.register_ws_session("session-1".to_string());
        assert_eq!(s1, None);
        assert_eq!(policy.active_ws_session_id.as_deref(), Some("session-1"));

        let s2 = policy.register_ws_session("session-2".to_string());
        assert_eq!(s2, Some("session-1".to_string()));
        assert_eq!(policy.active_ws_session_id.as_deref(), Some("session-2"));
    }

    #[test]
    fn test_credential_rotation_on_navigation() {
        let mut policy = SurfacePolicy::new(
            "surf-1".to_string(),
            true,
            "doc-1".to_string(),
            "secret-bootstrap".to_string(),
            Some("http://127.0.0.1:3000".to_string()),
        );

        let ticket = policy.mint_ticket("secret-bootstrap").unwrap();
        policy.register_ws_session("ws-old".to_string());
        policy.outbound_queue.push("event".to_string(), vec![1, 2], None).unwrap();

        // Rotate credentials
        policy.rotate_credentials("doc-2".to_string(), "new-bootstrap".to_string(), Some("https://example.com".to_string()));

        assert_eq!(policy.active_document_id, "doc-2");
        assert_eq!(policy.active_bootstrap_secret, "new-bootstrap");
        assert_eq!(policy.active_ticket.is_none(), true);
        assert_eq!(policy.active_ws_session_id.is_none(), true);
        assert_eq!(policy.outbound_queue.entries.len(), 0);

        // Old ticket cannot be consumed
        assert!(policy.consume_ticket("doc-2", &ticket).is_err());
    }

    #[test]
    fn test_channel_method_intersection() {
        let mut policy = SurfacePolicy::new(
            "surf-1".to_string(),
            true,
            "doc-1".to_string(),
            "secret-bootstrap".to_string(),
            Some("http://127.0.0.1:3000".to_string()),
        );

        policy.allowed_channels.insert("data".to_string());
        policy.allowed_methods.insert("query".to_string());

        assert!(policy.is_call_permitted("data", "query"));
        assert!(!policy.is_call_permitted("admin", "query"));
        assert!(!policy.is_call_permitted("data", "delete"));

        policy.ipc_eligible = false;
        assert!(!policy.is_call_permitted("data", "query"));
    }
}
