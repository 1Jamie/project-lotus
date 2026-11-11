//! Host-session D-Bus notification bridge.
//!
//! External apps (Discord, …) keep talking to the *real* session bus. We
//! `BecomeMonitor` on that bus, watch `org.freedesktop.Notifications.Notify`,
//! and mirror matching calls into atrium's toast bus (`atrium-notify` →
//! `shell.notify`). No nested dbus-daemon.
//!
//! Modes (per registered app filter):
//! - `mirror`  — toast + leave the host daemon alone
//! - `capture` — toast + `CloseNotification` the host banner when we see the id

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use futures_util::StreamExt;
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use zbus::fdo::{DBusProxy, MonitoringProxy};
use zbus::message::Type as MsgType;
use zbus::names::{BusName, WellKnownName};
use zbus::zvariant::{ObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, Message};

use crate::server::WS_SENDERS;

const MSG_TYPE_DATA: u8 = 0x02;
const PENDING_TIMEOUT_MS: u64 = 500;
const QUEUE_CAP: usize = 32;
const BACKOFF_START_MS: u64 = 250;
const BACKOFF_CAP_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyMode {
    Off,
    Mirror,
    Capture,
}

impl NotifyMode {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "mirror" => Self::Mirror,
            "capture" => Self::Capture,
            _ => Self::Off,
        }
    }
}

#[derive(Clone)]
struct ToastDraft {
    app_name: String,
    summary: String,
    body: String,
    app_icon: String,
    timeout_ms: i32,
    replaces_id: u32,
    actions: Vec<String>,
    urgency: String,
    mode: NotifyMode,
    /// Filter keys that matched (for quiet-pane lookup).
    match_keys: Vec<String>,
}

struct PendingNotify {
    draft: ToastDraft,
    /// true once toast has been delivered (timeout fallback or return).
    emitted: bool,
}

#[derive(Default)]
struct BridgeState {
    /// lowercase app_name / desktop-entry → mode
    filters: HashMap<String, NotifyMode>,
    /// filter key → source pane id (for overlay quieting)
    app_to_pane: HashMap<String, String>,
    /// pane_id → currently visible in atrium overlay
    pane_visible: HashMap<String, bool>,
    window_id: Option<String>,
    /// Queued WS payloads when no HUD sockets are connected yet.
    outbound: VecDeque<(String, serde_json::Value)>,
}

static BRIDGE: Lazy<Arc<Mutex<BridgeState>>> =
    Lazy::new(|| Arc::new(Mutex::new(BridgeState::default())));
static STARTED: AtomicBool = AtomicBool::new(false);
static PENDING: Lazy<Mutex<HashMap<u32, PendingNotify>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn diag_enabled() -> bool {
    matches!(
        std::env::var("ATRIUM_DBUS_DIAG").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes")
    )
}

fn diag(msg: impl AsRef<str>) {
    if diag_enabled() {
        info!("[lotus-nest] dbus-diag {}", msg.as_ref());
    }
}

/// Register notification filters for an external pane and ensure the monitor runs.
pub fn register_external_notify(
    window_id: &str,
    mode: NotifyMode,
    apps: &[String],
    pane_id: Option<&str>,
) {
    if mode == NotifyMode::Off {
        return;
    }
    {
        let mut st = BRIDGE.lock().unwrap_or_else(|e| e.into_inner());
        st.window_id = Some(window_id.to_string());
        if apps.is_empty() {
            warn!(
                "[lotus-nest] dbus.notifications={:?} with empty notificationApps — ignored",
                mode
            );
            return;
        }
        for a in apps {
            let key = a.trim().to_ascii_lowercase();
            if key.is_empty() {
                continue;
            }
            let prev = st.filters.get(&key).copied();
            let next = match prev {
                Some(NotifyMode::Capture) => NotifyMode::Capture,
                _ if mode == NotifyMode::Capture => NotifyMode::Capture,
                _ => mode,
            };
            st.filters.insert(key.clone(), next);
            if let Some(pid) = pane_id {
                st.app_to_pane.insert(key, pid.to_string());
            }
        }
        info!(
            "[lotus-nest] dbus notify bridge filters={:?} window={} pane={:?}",
            st.filters.keys().collect::<Vec<_>>(),
            window_id,
            pane_id
        );
    }
    flush_outbound_queue();
    ensure_started();
}

/// Overlay quieting: when a filtered app's atrium pane is visible, skip toasts
/// (host Notify still reaches the session daemon unchanged).
pub fn set_source_pane_visible(pane_id: &str, visible: bool) {
    let mut st = BRIDGE.lock().unwrap_or_else(|e| e.into_inner());
    st.pane_visible.insert(pane_id.to_string(), visible);
    diag(format!("pane_visible {pane_id}={visible}"));
}

fn ensure_started() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = thread::Builder::new()
        .name("atrium-dbus-notify".into())
        .spawn(|| {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    error!("[lotus-nest] dbus notify runtime failed: {e}");
                    STARTED.store(false, Ordering::SeqCst);
                    return;
                }
            };
            rt.block_on(supervisor_loop());
            STARTED.store(false, Ordering::SeqCst);
        });
}

async fn supervisor_loop() {
    let mut backoff_ms = BACKOFF_START_MS;
    loop {
        match run_monitor_once().await {
            Ok(()) => {
                warn!("[lotus-nest] dbus notify monitor stream ended; reconnecting");
                backoff_ms = BACKOFF_START_MS;
            }
            Err(e) => {
                error!("[lotus-nest] dbus notify monitor error (will retry): {e}");
            }
        }
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms.saturating_mul(2)).min(BACKOFF_CAP_MS);
    }
}

fn filter_key_names() -> Vec<String> {
    let st = BRIDGE.lock().unwrap_or_else(|e| e.into_inner());
    st.filters
        .iter()
        .filter(|(_, m)| **m != NotifyMode::Off)
        .map(|(k, _)| k.clone())
        .collect()
}

/// Substring / equality match against registered filter keys.
fn lookup_mode(app_name: &str, desktop_entry: Option<&str>) -> Option<(NotifyMode, Vec<String>)> {
    let st = BRIDGE.lock().unwrap_or_else(|e| e.into_inner());
    if st.filters.is_empty() {
        return None;
    }
    let mut candidates = Vec::new();
    let app_l = app_name.to_ascii_lowercase();
    if !app_l.is_empty() {
        candidates.push(app_l);
    }
    if let Some(d) = desktop_entry {
        let dl = d.to_ascii_lowercase();
        candidates.push(dl.clone());
        if let Some(last) = dl.rsplit('.').next() {
            if !last.is_empty() {
                candidates.push(last.to_string());
            }
        }
    }

    let mut best: Option<(NotifyMode, Vec<String>)> = None;
    for (filter, mode) in &st.filters {
        if *mode == NotifyMode::Off {
            continue;
        }
        let hit = candidates
            .iter()
            .any(|c| c == filter || c.contains(filter) || filter.contains(c.as_str()));
        if !hit {
            continue;
        }
        match &mut best {
            None => best = Some((*mode, vec![filter.clone()])),
            Some((m, keys)) => {
                if *mode == NotifyMode::Capture {
                    *m = NotifyMode::Capture;
                }
                if !keys.iter().any(|k| k == filter) {
                    keys.push(filter.clone());
                }
            }
        }
    }
    best
}

fn source_pane_quiet(match_keys: &[String]) -> bool {
    let st = BRIDGE.lock().unwrap_or_else(|e| e.into_inner());
    for key in match_keys {
        if let Some(pane) = st.app_to_pane.get(key) {
            if st.pane_visible.get(pane).copied().unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

fn window_id() -> Option<String> {
    BRIDGE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .window_id
        .clone()
}

fn hint_desktop_entry(hints: &HashMap<String, OwnedValue>) -> Option<String> {
    for key in ["desktop-entry", "desktop_entry"] {
        if let Some(v) = hints.get(key) {
            if let Ok(s) = String::try_from(v.clone()) {
                return Some(s);
            }
        }
    }
    None
}

fn hint_urgency(hints: &HashMap<String, OwnedValue>) -> String {
    if let Some(v) = hints.get("urgency") {
        if let Ok(b) = u8::try_from(v.clone()) {
            return match b {
                0 => "low".into(),
                2 => "critical".into(),
                _ => "normal".into(),
            };
        }
        if let Ok(i) = i32::try_from(v.clone()) {
            return match i {
                0 => "low".into(),
                2 => "critical".into(),
                _ => "normal".into(),
            };
        }
    }
    "normal".into()
}

fn draft_to_payload(draft: &ToastDraft, dbus_id: Option<u32>) -> serde_json::Value {
    let timeout = if draft.timeout_ms > 0 {
        draft.timeout_ms as u64
    } else if draft.timeout_ms == 0 {
        0
    } else {
        4500
    };
    let mut actions_obj = Vec::new();
    for pair in draft.actions.chunks_exact(2) {
        actions_obj.push(serde_json::json!({
            "id": pair[0],
            "label": pair[1],
        }));
    }
    serde_json::json!({
        "title": if draft.summary.is_empty() { &draft.app_name } else { &draft.summary },
        "body": draft.body,
        "icon": if draft.app_icon.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!(draft.app_icon)
        },
        "urgency": draft.urgency,
        "timeoutMs": timeout,
        "source": draft.app_name,
        "dbusId": dbus_id,
        "replacesId": draft.replaces_id,
        "actions": actions_obj,
    })
}

fn send_ws(window_id: &str, channel: &str, payload: serde_json::Value) -> bool {
    let Some(senders) = WS_SENDERS.get() else {
        return false;
    };
    let Ok(packed) = rmp_serde::encode::to_vec_named(&vec![(channel, payload)]) else {
        return false;
    };
    let mut wrapped = Vec::with_capacity(packed.len() + 1);
    wrapped.push(MSG_TYPE_DATA);
    wrapped.extend(packed);
    let msg = axum::extract::ws::Message::Binary(wrapped.into());
    let prefix = format!("{window_id}:");
    let mut sent = false;
    for entry in senders.iter() {
        if entry.key().starts_with(&prefix) || entry.key() == window_id {
            if entry.value().send(msg.clone()).is_ok() {
                sent = true;
            }
        }
    }
    sent
}

fn enqueue_or_send(channel: &str, payload: serde_json::Value) {
    let Some(wid) = window_id() else {
        return;
    };
    if send_ws(&wid, channel, payload.clone()) {
        return;
    }
    let mut st = BRIDGE.lock().unwrap_or_else(|e| e.into_inner());
    if st.outbound.len() >= QUEUE_CAP {
        st.outbound.pop_front();
    }
    st.outbound.push_back((channel.to_string(), payload));
    diag(format!(
        "queued {channel} (outbound={})",
        st.outbound.len()
    ));
}

fn flush_outbound_queue() {
    let Some(wid) = window_id() else {
        return;
    };
    let queued = {
        let mut st = BRIDGE.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut st.outbound)
    };
    if queued.is_empty() {
        return;
    }
    let mut kept = VecDeque::new();
    for (ch, payload) in queued {
        if !send_ws(&wid, &ch, payload.clone()) {
            kept.push_back((ch, payload));
        }
    }
    if !kept.is_empty() {
        let mut st = BRIDGE.lock().unwrap_or_else(|e| e.into_inner());
        for item in kept.into_iter().rev() {
            st.outbound.push_front(item);
        }
        while st.outbound.len() > QUEUE_CAP {
            st.outbound.pop_back();
        }
    }
}

fn emit_toast(draft: &ToastDraft, dbus_id: Option<u32>) {
    if source_pane_quiet(&draft.match_keys) {
        diag(format!(
            "suppress toast app={:?} (source pane visible)",
            draft.app_name
        ));
        return;
    }
    let payload = draft_to_payload(draft, dbus_id);
    info!(
        "[lotus-nest] dbus Notify → atrium-notify app={:?} summary={:?} id={:?} mode={:?}",
        draft.app_name, draft.summary, dbus_id, draft.mode
    );
    enqueue_or_send("atrium-notify", payload);
}

fn emit_closed(id: u32, reason: u32) {
    enqueue_or_send(
        "atrium-notify-closed",
        serde_json::json!({ "dbusId": id, "reason": reason }),
    );
}

fn emit_action(id: u32, action_key: &str) {
    enqueue_or_send(
        "atrium-notify-action",
        serde_json::json!({ "dbusId": id, "action": action_key }),
    );
}

async fn close_host_notification(id: u32) {
    let Ok(conn) = Connection::session().await else {
        return;
    };
    let Ok(path) = ObjectPath::try_from("/org/freedesktop/Notifications") else {
        return;
    };
    let _ = conn
        .call_method(
            Some("org.freedesktop.Notifications"),
            &path,
            Some("org.freedesktop.Notifications"),
            "CloseNotification",
            &(id,),
        )
        .await;
}

async fn resolve_notifications_owner(conn: &Connection) -> Option<String> {
    let dbus = DBusProxy::new(conn).await.ok()?;
    let name = WellKnownName::try_from("org.freedesktop.Notifications").ok()?;
    match dbus.get_name_owner(BusName::WellKnown(name)).await {
        Ok(owner) => Some(owner.as_str().to_string()),
        Err(e) => {
            warn!(
                "[lotus-nest] dbus notify: org.freedesktop.Notifications has no owner yet ({e})"
            );
            None
        }
    }
}

fn build_monitor_rules() -> zbus::Result<Vec<MatchRule<'static>>> {
    let notify_call = MatchRule::builder()
        .msg_type(MsgType::MethodCall)
        .interface("org.freedesktop.Notifications")?
        .member("Notify")?
        .build();

    // Flatpak/Electron often use the desktop portal instead of (or before) Notify.
    let portal_add = MatchRule::builder()
        .msg_type(MsgType::MethodCall)
        .interface("org.freedesktop.portal.Notification")?
        .member("AddNotification")?
        .build();

    // Broad MethodReturn: BecomeMonitor rules are fixed for the connection lifetime.
    // When the notifications unique name changes we still need returns; filter by
    // pending reply_serial (+ optional sender check) in-process instead.
    let rule_ret = MatchRule::builder().msg_type(MsgType::MethodReturn).build();

    let closed = MatchRule::builder()
        .msg_type(MsgType::Signal)
        .interface("org.freedesktop.Notifications")?
        .member("NotificationClosed")?
        .build();

    let action = MatchRule::builder()
        .msg_type(MsgType::Signal)
        .interface("org.freedesktop.Notifications")?
        .member("ActionInvoked")?
        .build();

    let name_owner = MatchRule::builder()
        .msg_type(MsgType::Signal)
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .build();

    Ok(vec![
        notify_call,
        portal_add,
        rule_ret,
        closed,
        action,
        name_owner,
    ])
}

async fn run_monitor_once() -> zbus::Result<()> {
    let conn = Connection::session().await.map_err(|e| {
        error!(
            "[lotus-nest] dbus notify: cannot connect to session bus ({e}) — is DBUS_SESSION_BUS_ADDRESS set?"
        );
        e
    })?;

    let owner = resolve_notifications_owner(&conn).await;
    let rules = build_monitor_rules()?;

    let mon = MonitoringProxy::new(&conn).await?;
    mon.become_monitor(&rules, 0).await.map_err(|e| {
        error!(
            "[lotus-nest] dbus notify BecomeMonitor failed ({e}) — Monitoring interface denied?"
        );
        e
    })?;

    info!(
        "[lotus-nest] dbus notify BecomeMonitor active (owner={:?}, filters={:?})",
        owner,
        filter_key_names()
    );

    // Clear stale pairings across reconnects.
    if let Ok(mut g) = PENDING.lock() {
        g.clear();
    }

    let mut notif_owner = owner;
    let mut stream = zbus::MessageStream::from(&conn);

    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!("[lotus-nest] dbus notify stream error: {e}");
                return Err(e.into());
            }
        };

        match msg.message_type() {
            MsgType::MethodCall => handle_method_call(&msg),
            MsgType::MethodReturn => handle_notify_return(&msg, &notif_owner),
            MsgType::Signal => handle_signal(&msg, &mut notif_owner),
            _ => {}
        }
    }
    Ok(())
}

fn handle_method_call(msg: &Message) {
    let iface = msg
        .header()
        .interface()
        .map(|i| i.as_str().to_string())
        .unwrap_or_default();
    let member = msg
        .header()
        .member()
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();

    if iface == "org.freedesktop.Notifications" && member == "Notify" {
        handle_notify_call(msg);
    } else if iface == "org.freedesktop.portal.Notification" && member == "AddNotification" {
        handle_portal_add(msg);
    }
}

/// Flatpak Discord often calls the notification portal; mirror those into toasts too.
fn handle_portal_add(msg: &Message) {
    // AddNotification(id: s, notification: a{sv})
    let deserialized = msg
        .body()
        .deserialize::<(String, HashMap<String, OwnedValue>)>();
    let Ok((portal_id, notif)) = deserialized else {
        diag("portal AddNotification deserialize failed");
        return;
    };
    if window_id().is_none() || filter_key_names().is_empty() {
        return;
    }

    let title = notif
        .get("title")
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_default();
    let body = notif
        .get("body")
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_default();
    let icon = notif
        .get("icon")
        .and_then(|v| String::try_from(v.clone()).ok())
        .unwrap_or_default();

    // Portal requests don't carry app_name; accept when any filter is registered
    // and the payload looks like a user notification (has title or body).
    if title.is_empty() && body.is_empty() {
        return;
    }

    let filters = filter_key_names();
    let hay = format!("{title} {body} {icon} {portal_id}").to_ascii_lowercase();
    let matched: Vec<String> = filters
        .iter()
        .filter(|f| hay.contains(f.as_str()) || f.contains("discord"))
        .cloned()
        .collect();
    // If filters include discord (typical atrium), accept all portal adds — Flatpak
    // Discord is the usual portal notifier in this shell. Otherwise require a hit.
    let discord_filter = filters.iter().any(|f| f.contains("discord"));
    let match_keys = if !matched.is_empty() {
        matched
    } else if discord_filter {
        filters
            .iter()
            .filter(|f| f.contains("discord"))
            .cloned()
            .collect()
    } else {
        diag(format!(
            "ignored portal AddNotification id={portal_id:?} title={title:?}"
        ));
        return;
    };

    let mode = {
        let st = BRIDGE.lock().unwrap_or_else(|e| e.into_inner());
        match_keys
            .iter()
            .filter_map(|k| st.filters.get(k).copied())
            .find(|m| *m != NotifyMode::Off)
            .unwrap_or(NotifyMode::Mirror)
    };

    let draft = ToastDraft {
        app_name: "Discord".into(),
        summary: title,
        body,
        app_icon: icon,
        timeout_ms: -1,
        replaces_id: 0,
        actions: Vec::new(),
        urgency: "normal".into(),
        mode,
        match_keys,
    };
    info!(
        "[lotus-nest] dbus portal AddNotification → toast summary={:?}",
        draft.summary
    );
    emit_toast(&draft, None);
}

fn handle_notify_call(msg: &Message) {
    let deserialized = msg.body().deserialize::<(
        String,
        u32,
        String,
        String,
        String,
        Vec<String>,
        HashMap<String, OwnedValue>,
        i32,
    )>();
    let Ok((app_name, replaces_id, app_icon, summary, body, actions, hints, timeout)) =
        deserialized
    else {
        // Always log deserialize failures — usually means a different Notify shape.
        warn!("[lotus-nest] dbus Notify deserialize failed (wrong signature?)");
        return;
    };
    let desktop = hint_desktop_entry(&hints);
    // Log every Notify we see so missing filters are obvious without ATRIUM_DBUS_DIAG.
    info!(
        "[lotus-nest] dbus Notify seen app={app_name:?} desktop={desktop:?} summary={summary:?}"
    );
    let Some((mode, match_keys)) = lookup_mode(&app_name, desktop.as_deref()) else {
        info!(
            "[lotus-nest] dbus Notify ignored (no filter match) app={app_name:?} desktop={desktop:?} filters={:?}",
            filter_key_names()
        );
        return;
    };
    if window_id().is_none() {
        warn!("[lotus-nest] dbus Notify matched but no window_id registered");
        return;
    }

    let serial = msg.primary_header().serial_num().get();
    let draft = ToastDraft {
        app_name,
        summary,
        body,
        app_icon,
        timeout_ms: timeout,
        replaces_id,
        actions,
        urgency: hint_urgency(&hints),
        mode,
        match_keys,
    };

    diag(format!(
        "matched Notify serial={serial} app={:?} desktop={desktop:?} mode={mode:?}",
        draft.app_name
    ));

    // Emit immediately so toasts don't depend on MethodReturn pairing / timeout.
    // Return handler still attaches capture CloseNotification when needed.
    emit_toast(&draft, None);

    if let Ok(mut g) = PENDING.lock() {
        if g.len() > 256 {
            g.retain(|_, p| !p.emitted);
        }
        g.insert(
            serial,
            PendingNotify {
                draft,
                emitted: true, // already toasted
            },
        );
    }

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(PENDING_TIMEOUT_MS)).await;
        // Reap pending if no return arrived (avoid unbounded growth).
        if let Ok(mut g) = PENDING.lock() {
            g.remove(&serial);
        }
    });
}

fn handle_notify_return(msg: &Message, notif_owner: &Option<String>) {
    if let Some(owner) = notif_owner {
        if let Some(sender) = msg.header().sender() {
            if sender.as_str() != owner.as_str() {
                return;
            }
        }
    }
    let Some(reply_serial) = msg.header().reply_serial() else {
        return;
    };
    let serial = reply_serial.get();
    let mut pending = {
        let Ok(mut g) = PENDING.lock() else {
            return;
        };
        match g.remove(&serial) {
            Some(p) => p,
            None => return,
        }
    };

    let id = match msg.body().deserialize::<u32>() {
        Ok(id) => id,
        Err(_) => {
            diag(format!("Notify return serial={serial} not u32"));
            if !pending.emitted {
                emit_toast(&pending.draft, None);
            }
            return;
        }
    };
    diag(format!(
        "Notify return serial={serial} dbusId={id} mode={:?}",
        pending.draft.mode
    ));
    if !pending.emitted {
        pending.emitted = true;
        emit_toast(&pending.draft, Some(id));
    } else {
        // Timeout already toasted without id — still capture-close if needed.
        diag(format!(
            "return after timeout toast; dbusId={id} (no second toast)"
        ));
    }
    if pending.draft.mode == NotifyMode::Capture {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            debug!("[lotus-nest] dbus notify capture → CloseNotification({id})");
            close_host_notification(id).await;
        });
    }
}

fn handle_signal(msg: &Message, notif_owner: &mut Option<String>) {
    let header = msg.header();
    let iface = header.interface().map(|i| i.as_str()).unwrap_or("");
    let member = header.member().map(|m| m.as_str()).unwrap_or("");

    if iface == "org.freedesktop.DBus" && member == "NameOwnerChanged" {
        if let Ok((name, _old, new)) = msg.body().deserialize::<(String, String, String)>() {
            if name == "org.freedesktop.Notifications" {
                *notif_owner = if new.is_empty() { None } else { Some(new) };
                info!(
                    "[lotus-nest] dbus notify NameOwnerChanged → {:?}",
                    notif_owner
                );
            }
        }
        return;
    }

    if iface != "org.freedesktop.Notifications" {
        return;
    }
    match member {
        "NotificationClosed" => {
            if let Ok((id, reason)) = msg.body().deserialize::<(u32, u32)>() {
                diag(format!("NotificationClosed id={id} reason={reason}"));
                emit_closed(id, reason);
            }
        }
        "ActionInvoked" => {
            if let Ok((id, action)) = msg.body().deserialize::<(u32, String)>() {
                diag(format!("ActionInvoked id={id} action={action}"));
                emit_action(id, &action);
            }
        }
        _ => {}
    }
}
