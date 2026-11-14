//! Output-scoped presentation clock (VRR vs fixed VSync) and game vs UI activity hysteresis.

#[cfg(target_os = "linux")]
use crate::app::is_gamescope_session_pub;
#[cfg(target_os = "linux")]
use crate::window::WindowInstance;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
pub(crate) static NESTED_LAST_PRIMARY_COMMIT_NS: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "linux")]
fn nest_monotonic_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_nanos() as u64
}

#[cfg(target_os = "linux")]
pub(crate) fn note_primary_game_commit() {
    NESTED_LAST_PRIMARY_COMMIT_NS.store(nest_monotonic_ns(), Ordering::SeqCst);
}

#[cfg(target_os = "linux")]
pub(crate) fn game_commit_fresh(max_age: Duration) -> bool {
    let last = NESTED_LAST_PRIMARY_COMMIT_NS.load(Ordering::SeqCst);
    if last == 0 {
        return false;
    }
    nest_monotonic_ns().saturating_sub(last) < max_age.as_nanos() as u64
}

#[cfg(target_os = "linux")]
pub fn window_is_viewable(instance: &WindowInstance) -> bool {
    if instance.zero_size {
        return false;
    }
    if is_gamescope_session_pub() {
        return true;
    }
    if instance.os_minimized {
        return false;
    }
    !instance.os_occluded
}

/// Milliseconds without a primary commit before overlay-driven presents are allowed.
#[cfg(target_os = "linux")]
pub const PACING_IDLE_MS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingPolicy {
    /// Mailbox. The scanning CRTC has variable refresh enabled, or
    /// `ATRIUM_PACING=vrr`. A frame slower than the max refresh stays up
    /// until the next one; the panel does not insert a spare vblank.
    VrrPassthrough,
    /// FIFO. The scanning CRTC is fixed, or `ATRIUM_PACING=fixed`. A frame
    /// slower than the refresh is repeated for the spare vblanks. Mailbox
    /// here reorders that spare vblank back onto the previous image.
    /// The nested compositor latches client commits in order and keeps the
    /// scanned-out wl_buffer until a strictly newer frame has been presented,
    /// so a late frame holds instead of rewriting the image still on screen.
    /// Under gamescope this is interval 0 + one software pageflip per refresh
    /// (Xwayland cannot do real eglSwapInterval(1) FIFO).
    FixedVsyncLocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentActivity {
    /// Primary has committed within `PACING_IDLE_MS`.
    GameActive,
    /// Cold boot, loading, or no primary commit for ≥ `PACING_IDLE_MS`.
    UiIdle,
}

#[cfg(target_os = "linux")]
fn pacing_override() -> Option<PacingPolicy> {
    let Ok(v) = std::env::var("ATRIUM_PACING") else {
        return None;
    };
    match v.to_ascii_lowercase().as_str() {
        "vrr" | "mailbox" | "0" => Some(PacingPolicy::VrrPassthrough),
        "fixed" | "vsync" | "1" => Some(PacingPolicy::FixedVsyncLocked),
        _ => None,
    }
}

/// `VRR_ENABLED` on every CRTC that is actually scanning a connected output.
///
/// Steam and gamescope advertise adaptive sync to the client even when the
/// docked panel is locked. The CRTC flag is the scanout. `None` means DRM
/// could not be read; callers keep the gamescope-session fallback.
#[cfg(target_os = "linux")]
fn output_vrr_enabled() -> Option<bool> {
    use std::sync::Mutex;
    use std::time::Instant;
    static CACHE: Mutex<Option<(Instant, Option<bool>)>> = Mutex::new(None);
    let now = Instant::now();
    if let Ok(guard) = CACHE.lock() {
        if let Some((at, value)) = *guard {
            if now.duration_since(at) < Duration::from_millis(250) {
                return value;
            }
        }
    }
    let value = probe_output_vrr();
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let prev = guard.map(|(_, v)| v);
    if prev != Some(value) {
        log::info!(
            "[lotus-pacing] scanout VRR_ENABLED={}",
            match value {
                Some(true) => "1",
                Some(false) => "0",
                None => "unreadable",
            }
        );
    }
    *guard = Some((now, value));
    value
}

#[cfg(target_os = "linux")]
fn probe_output_vrr() -> Option<bool> {
    let dir = std::fs::read_dir("/dev/dri").ok()?;
    let mut enabled = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        if let Some(card) = scan_drm_card(&entry.path()) {
            enabled.extend(card);
        }
    }
    if enabled.is_empty() {
        return None;
    }
    // Mixed outputs (one varying, one locked) stay FIFO. A locked panel
    // in the mix is the one that flashes the previous frame.
    Some(enabled.iter().all(|on| *on))
}

#[cfg(target_os = "linux")]
fn scan_drm_card(path: &std::path::Path) -> Option<Vec<bool>> {
    use std::os::unix::ffi::OsStrExt;
    let mut c_path = path.as_os_str().as_bytes().to_vec();
    c_path.push(0);
    let fd = unsafe { libc::open(c_path.as_ptr() as *const libc::c_char, libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let result = unsafe { scan_drm_fd(fd) };
    unsafe { libc::close(fd) };
    result
}

#[cfg(target_os = "linux")]
const DRM_IOCTL_MODE_GETRESOURCES: libc::c_ulong = 0xc04064a0;
#[cfg(target_os = "linux")]
const DRM_IOCTL_MODE_GETCONNECTOR: libc::c_ulong = 0xc05064a7;
#[cfg(target_os = "linux")]
const DRM_IOCTL_MODE_GETENCODER: libc::c_ulong = 0xc01464a6;
#[cfg(target_os = "linux")]
const DRM_IOCTL_MODE_GETPROPERTY: libc::c_ulong = 0xc04064aa;
#[cfg(target_os = "linux")]
const DRM_IOCTL_MODE_OBJ_GETPROPERTIES: libc::c_ulong = 0xc02064b9;
#[cfg(target_os = "linux")]
const DRM_MODE_OBJECT_CRTC: u32 = 0xcccc_cccc;
#[cfg(target_os = "linux")]
const DRM_MODE_CONNECTED: u32 = 1;

#[cfg(target_os = "linux")]
#[repr(C)]
struct DrmModeCardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct DrmModeGetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct DrmModeGetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct DrmModeGetProperty {
    values_ptr: u64,
    enum_blob_ptr: u64,
    prop_id: u32,
    flags: u32,
    name: [u8; 32],
    count_values: u32,
    count_enum_blobs: u32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct DrmModeObjGetProperties {
    props_ptr: u64,
    prop_values_ptr: u64,
    count_props: u32,
    obj_id: u32,
    obj_type: u32,
    pad: u32,
}

#[cfg(target_os = "linux")]
unsafe fn scan_drm_fd(fd: libc::c_int) -> Option<Vec<bool>> {
    let mut res = std::mem::zeroed::<DrmModeCardRes>();
    if libc::ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res) != 0 || res.count_connectors == 0 {
        return None;
    }
    let mut fbs = vec![0u32; res.count_fbs.max(1) as usize];
    let mut crtcs = vec![0u32; res.count_crtcs.max(1) as usize];
    let mut conns = vec![0u32; res.count_connectors as usize];
    let mut encoders = vec![0u32; res.count_encoders.max(1) as usize];
    res.fb_id_ptr = fbs.as_mut_ptr() as u64;
    res.crtc_id_ptr = crtcs.as_mut_ptr() as u64;
    res.connector_id_ptr = conns.as_mut_ptr() as u64;
    res.encoder_id_ptr = encoders.as_mut_ptr() as u64;
    if libc::ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res) != 0 {
        return None;
    }

    let mut names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    let mut enabled = Vec::new();
    for &connector_id in conns.iter().take(res.count_connectors as usize) {
        let mut conn = std::mem::zeroed::<DrmModeGetConnector>();
        conn.connector_id = connector_id;
        if libc::ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &mut conn) != 0 {
            continue;
        }
        let mut modes = vec![0u8; conn.count_modes.max(1) as usize * 68];
        let mut props = vec![0u32; conn.count_props.max(1) as usize];
        let mut vals = vec![0u64; conn.count_props.max(1) as usize];
        let mut encs = vec![0u32; conn.count_encoders.max(1) as usize];
        conn.modes_ptr = modes.as_mut_ptr() as u64;
        conn.props_ptr = props.as_mut_ptr() as u64;
        conn.prop_values_ptr = vals.as_mut_ptr() as u64;
        conn.encoders_ptr = encs.as_mut_ptr() as u64;
        if libc::ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &mut conn) != 0 || conn.connection != DRM_MODE_CONNECTED || conn.encoder_id == 0 {
            continue;
        }
        let mut enc = std::mem::zeroed::<DrmModeGetEncoder>();
        enc.encoder_id = conn.encoder_id;
        if libc::ioctl(fd, DRM_IOCTL_MODE_GETENCODER, &mut enc) != 0 || enc.crtc_id == 0 {
            continue;
        }
        if let Some(on) = crtc_vrr_enabled(fd, enc.crtc_id, &mut names) {
            enabled.push(on);
        }
    }
    Some(enabled)
}

#[cfg(target_os = "linux")]
unsafe fn crtc_vrr_enabled(
    fd: libc::c_int,
    crtc_id: u32,
    names: &mut std::collections::HashMap<u32, String>,
) -> Option<bool> {
    let mut obj = std::mem::zeroed::<DrmModeObjGetProperties>();
    obj.obj_id = crtc_id;
    obj.obj_type = DRM_MODE_OBJECT_CRTC;
    if libc::ioctl(fd, DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut obj) != 0 || obj.count_props == 0 {
        return None;
    }
    let mut props = vec![0u32; obj.count_props as usize];
    let mut vals = vec![0u64; obj.count_props as usize];
    obj.props_ptr = props.as_mut_ptr() as u64;
    obj.prop_values_ptr = vals.as_mut_ptr() as u64;
    if libc::ioctl(fd, DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut obj) != 0 {
        return None;
    }
    for i in 0..obj.count_props as usize {
        if prop_name(fd, props[i], names).as_deref() == Some("VRR_ENABLED") {
            return Some(vals[i] != 0);
        }
    }
    None
}

#[cfg(target_os = "linux")]
unsafe fn prop_name(
    fd: libc::c_int,
    prop_id: u32,
    names: &mut std::collections::HashMap<u32, String>,
) -> Option<String> {
    if let Some(name) = names.get(&prop_id) {
        return Some(name.clone());
    }
    let mut prop = std::mem::zeroed::<DrmModeGetProperty>();
    prop.prop_id = prop_id;
    if libc::ioctl(fd, DRM_IOCTL_MODE_GETPROPERTY, &mut prop) != 0 {
        return None;
    }
    let len = prop.name.iter().position(|b| *b == 0).unwrap_or(prop.name.len());
    let name = String::from_utf8_lossy(&prop.name[..len]).into_owned();
    names.insert(prop_id, name.clone());
    Some(name)
}

#[cfg(target_os = "linux")]
pub fn resolve_pacing_policy() -> PacingPolicy {
    if let Some(policy) = pacing_override() {
        return policy;
    }
    match output_vrr_enabled() {
        Some(true) => PacingPolicy::VrrPassthrough,
        Some(false) => PacingPolicy::FixedVsyncLocked,
        None => {
            if is_gamescope_session_pub() {
                PacingPolicy::VrrPassthrough
            } else {
                PacingPolicy::FixedVsyncLocked
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn resolve_pacing_policy() -> PacingPolicy {
    PacingPolicy::FixedVsyncLocked
}

#[cfg(target_os = "linux")]
pub fn present_activity(instance: &WindowInstance) -> PresentActivity {
    if instance.nested_compositor.is_none() {
        return PresentActivity::UiIdle;
    }
    if game_commit_fresh(Duration::from_millis(PACING_IDLE_MS)) {
        PresentActivity::GameActive
    } else {
        PresentActivity::UiIdle
    }
}

/// Interval 0 (mailbox / non-blocking swap).
///
/// Real VRR wants mailbox. Gamescope Xwayland also needs interval 0: interval 1
/// either deadlocks on buffer release or returns without waiting, so Fixed under
/// gamescope phase-aligns in software (`fixed_present_ready`) instead.
#[cfg(target_os = "linux")]
pub fn wants_immediate_swap(instance: &WindowInstance) -> bool {
    instance.pacing_policy == PacingPolicy::VrrPassthrough || is_gamescope_session_pub()
}

/// Software FIFO for Fixed + interval 0 (gamescope): at most one host pageflip per
/// refresh period so the mailbox never holds a previous image for a spare vblank.
#[cfg(target_os = "linux")]
pub fn fixed_software_fifo(instance: &WindowInstance) -> bool {
    instance.pacing_policy == PacingPolicy::FixedVsyncLocked && wants_immediate_swap(instance)
}

/// Hold nest `previous_dmabuf` across the spare vblank under gamescope Fixed.
/// Releasing on the non-blocking swap return lets DXVK rewrite a buffer that may
/// still be the mailbox "previous" on the next outer scan.
#[cfg(target_os = "linux")]
pub fn defer_nest_buffer_release(instance: &WindowInstance) -> bool {
    fixed_software_fifo(instance) && instance.nested_compositor.is_some()
}

/// Guard A: never block the main thread on interval=1 when the host window is hidden.
#[cfg(target_os = "linux")]
pub fn host_should_skip_blocking_present(instance: &WindowInstance) -> bool {
    !window_is_viewable(instance)
        && instance.pacing_policy == PacingPolicy::FixedVsyncLocked
        && instance.nested_compositor.is_some()
}

/// Fixed SoftFifo under gamescope must keep presenting the held frame on every
/// refresh while the game is live. Game-only wakes leave spare vblanks to the
/// outer mailbox, which reorders the previous image ("one frame rewind") when
/// the game sits much below the panel rate.
#[cfg(target_os = "linux")]
pub fn wants_fixed_hold_cadence(instance: &WindowInstance) -> bool {
    // Keep SoftFifo spare-vblank holds while the nest is live — including under
    // the overlay — so opening chrome does not free-run the mailbox at panel Hz.
    fixed_software_fifo(instance)
        && window_is_viewable(instance)
        && present_activity(instance) == PresentActivity::GameActive
}

/// True when the gamescope backup cadence thread should kick even though a
/// primary commit landed recently. Fixed SoftFifo needs spare-vblank holds;
/// VRR must never free-run at panel max.
#[cfg(target_os = "linux")]
pub fn cadence_fills_spare_vblanks() -> bool {
    resolve_pacing_policy() == PacingPolicy::FixedVsyncLocked && is_gamescope_session_pub()
}

#[cfg(target_os = "linux")]
pub fn wants_stall_present_timer(instance: &WindowInstance) -> bool {
    if instance.nested_compositor.is_none() {
        return is_gamescope_session_pub();
    }
    let activity = present_activity(instance);
    match instance.pacing_policy {
        // Idle only — GameActive must stay game-commit driven so adaptive sync
        // can drop to the real frame rate (passthrough).
        PacingPolicy::VrrPassthrough => activity == PresentActivity::UiIdle,
        // UiIdle stall + GameActive SoftFifo holds (spare vblank fill).
        PacingPolicy::FixedVsyncLocked => {
            activity == PresentActivity::UiIdle || wants_fixed_hold_cadence(instance)
        }
    }
}

/// VRR GameActive: host must not swap unless the primary nest buffer advanced.
/// X11 expose / overlay-only wakes would otherwise free-run at panel max and
/// defeat passthrough (gamescope FPS pegged while the game sits at 40–50).
#[cfg(target_os = "linux")]
pub fn vrr_requires_primary_damage(instance: &WindowInstance) -> bool {
    instance.pacing_policy == PacingPolicy::VrrPassthrough
        && present_activity(instance) == PresentActivity::GameActive
}

#[cfg(target_os = "linux")]
pub fn primary_has_unpresented_frame(instance: &WindowInstance) -> bool {
    let Some(id) = instance.primary_game_pane.as_ref() else {
        return false;
    };
    let Some(pane) = instance.panes.get(id) else {
        return false;
    };
    pane.frame_seq != pane.last_presented_seq
}

#[cfg(target_os = "linux")]
pub fn nest_game_driven_present(instance: &WindowInstance) -> bool {
    instance.nested_compositor.is_some()
        && instance.game_mode
        && present_activity(instance) == PresentActivity::GameActive
}

#[cfg(target_os = "linux")]
pub fn policy_label(policy: PacingPolicy) -> &'static str {
    match policy {
        PacingPolicy::VrrPassthrough => "VrrPassthrough",
        PacingPolicy::FixedVsyncLocked => "FixedVsyncLocked",
    }
}

/// Absolute CLOCK_MONOTONIC timestamp (ns). Same epoch as wp_presentation / DRM UST.
#[cfg(target_os = "linux")]
pub fn clock_monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64).saturating_mul(1_000_000_000) + ts.tv_nsec as u64
}

#[cfg(target_os = "linux")]
fn clock_nanosleep_abs(deadline_ns: u64) {
    let ts = libc::timespec {
        tv_sec: (deadline_ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (deadline_ns % 1_000_000_000) as libc::c_long,
    };
    unsafe {
        // TIMER_ABSTIME — ignore EINTR; a short early wake just means a slightly
        // longer spin into the deadline on the next check (we only sleep once).
        let _ = libc::clock_nanosleep(
            libc::CLOCK_MONOTONIC,
            libc::TIMER_ABSTIME,
            &ts,
            std::ptr::null_mut(),
        );
    }
}

/// Lead time before `next_deadline` at which we issue eglSwapBuffers so the
/// DMA-BUF lands in gamescope's CVBlankTimer latch window.
#[cfg(target_os = "linux")]
pub const SWAP_LEAD_NS: u64 = 1_250_000; // 1.25 ms

/// Minimum SoftFifo budget (ms) required to run Servo/HUD paint on a game-driven
/// present. Below this we composite the last overlay FBOs and keep `needs_repaint`
/// so a later tick (or UiIdle) can catch up without blowing the gate.
#[cfg(target_os = "linux")]
pub const OVERLAY_PAINT_MIN_BUDGET_MS: f32 = 4.0;

/// Next SoftFifo sleep-until (CLOCK_MONOTONIC ns) published for overlay budgeting.
#[cfg(target_os = "linux")]
static SOFT_FIFO_NEXT_GATE_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "linux")]
static SOFT_FIFO_GATE_ARMED: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "linux")]
pub fn clear_soft_fifo_gate_publish() {
    SOFT_FIFO_GATE_ARMED.store(0, Ordering::Release);
}

/// Milliseconds remaining until SoftFifo would sleep/submit. `Some(0)` = already late.
#[cfg(target_os = "linux")]
pub fn soft_fifo_budget_ms() -> Option<f32> {
    if SOFT_FIFO_GATE_ARMED.load(Ordering::Acquire) == 0 {
        return None;
    }
    let until = SOFT_FIFO_NEXT_GATE_NS.load(Ordering::Acquire);
    if until == 0 {
        return None;
    }
    let now = clock_monotonic_ns();
    if until <= now {
        Some(0.0)
    } else {
        Some((until - now) as f32 / 1_000_000.0)
    }
}

/// Wall `Instant` for winit `WaitUntil` aligned to the SoftFifo gate.
/// Keeps the host event loop awake for DeviceEvent mouse between presents.
#[cfg(target_os = "linux")]
pub fn soft_fifo_gate_instant() -> Option<std::time::Instant> {
    if SOFT_FIFO_GATE_ARMED.load(Ordering::Acquire) == 0 {
        return None;
    }
    let until = SOFT_FIFO_NEXT_GATE_NS.load(Ordering::Acquire);
    if until == 0 {
        return None;
    }
    let now = clock_monotonic_ns();
    if until <= now {
        return Some(std::time::Instant::now());
    }
    Some(std::time::Instant::now() + Duration::from_nanos(until - now))
}

/// SoftFifo gate is due (or within the inline phase-polish window).
/// Used by `about_to_wait` so DeviceEvent wakes only re-arm `WaitUntil` and do
/// **not** `request_redraw` / present early.
#[cfg(target_os = "linux")]
pub fn soft_fifo_gate_due() -> bool {
    match soft_fifo_budget_ms() {
        None => true, // unarmed — allow a present to start the grid
        Some(ms) => ms <= SOFT_FIFO_INLINE_WAIT_MS,
    }
}

/// Max in-thread sleep inside present (phase polish only). Longer waits use WaitUntil.
#[cfg(target_os = "linux")]
pub const SOFT_FIFO_INLINE_WAIT_MS: f32 = 1.5;

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
pub enum SoftFifoWait {
    /// Gate reached — caller may swap (after optional micro-sleep already done).
    Ready { gate_ms: f32 },
    /// Still early — skip swap; `about_to_wait` holds `WaitUntil(gate)`.
    Deferred { remaining_ms: f32 },
}

/// Game-driven SoftFifo: optionally skip Servo overlay paint when the gate is
/// still approaching (budget thin but not yet the present slot).
///
/// Once [`soft_fifo_gate_due`] is true this RedrawRequested *is* the present
/// slot — never defer here. The old `budget < 4ms` check starved HUD/shell
/// forever (due window is ≤1.5 ms, so every real present skipped Servo; F12
/// chrome and perf-hud froze until a rare UiIdle catch-up).
///
/// Overlay input mode (`!game_mode`) never defers — atrium shell/plugin UI must
/// paint on the next redraw or D-pad/toggles sit 1–2s behind the action while
/// the nest game is still `GameActive` underneath.
#[cfg(target_os = "linux")]
pub fn should_defer_overlay_servo_paint(instance: &WindowInstance) -> bool {
    if !instance.game_mode {
        return false;
    }
    if !fixed_software_fifo(instance) {
        return false;
    }
    if present_activity(instance) != PresentActivity::GameActive {
        return false;
    }
    // Present slot — paint overlays (shell opacity, HUD) with the flip.
    if soft_fifo_gate_due() {
        return false;
    }
    match soft_fifo_budget_ms() {
        Some(ms) => ms < OVERLAY_PAINT_MIN_BUDGET_MS,
        None => false,
    }
}

/// Guard B: piggyback Servo/HUD frames on the next game present instead of
/// waking an overlay-only redraw. Only while the user is in game input mode —
/// overlay/edit must wake immediately even if the nest is still committing.
#[cfg(target_os = "linux")]
pub fn defer_overlay_servo_wake(instance: &WindowInstance) -> bool {
    instance.game_mode
        && instance.nested_compositor.is_some()
        && present_activity(instance) == PresentActivity::GameActive
}

/// Phase-locked software FIFO for Fixed + interval 0 (gamescope).
///
/// Absolute `next_deadline` grid + optional CRTC presentation feedback PLL.
/// Sleeping relative to post-swap completion free-runs at ~17.0 ms on a 16.67 ms
/// panel (~1.2 phase slips/sec); this pacer does not accumulate that jitter.
#[cfg(target_os = "linux")]
pub struct SoftFifoPacer {
    pub next_deadline_ns: u64,
    pub period_ns: u64,
    pub swap_lead_ns: u64,
    /// Last signed phase error applied (ns), for diag.
    pub last_phase_err_ns: i64,
    pub phase_slips: u64,
}

#[cfg(target_os = "linux")]
impl SoftFifoPacer {
    pub fn new(period_ns: u64) -> Self {
        let period_ns = period_ns.max(1_000_000); // ≥1 ms
        Self {
            next_deadline_ns: 0,
            period_ns,
            swap_lead_ns: SWAP_LEAD_NS.min(period_ns / 4),
            last_phase_err_ns: 0,
            phase_slips: 0,
        }
    }

    pub fn set_period_ns(&mut self, period_ns: u64) {
        let period_ns = period_ns.max(1_000_000);
        if self.period_ns != period_ns {
            self.period_ns = period_ns;
            self.swap_lead_ns = SWAP_LEAD_NS.min(period_ns / 4);
        }
    }

    fn publish_next_gate(&self) {
        if self.next_deadline_ns == 0 {
            clear_soft_fifo_gate_publish();
            return;
        }
        let sleep_until = self.next_deadline_ns.saturating_sub(self.swap_lead_ns);
        SOFT_FIFO_NEXT_GATE_NS.store(sleep_until, Ordering::Release);
        SOFT_FIFO_GATE_ARMED.store(1, Ordering::Release);
    }

    /// Gate check for SoftFifo present.
    ///
    /// - Due / late → [`SoftFifoWait::Ready`] (optional ≤1.5 ms phase polish sleep).
    /// - Early by more than that → [`SoftFifoWait::Deferred`] — **do not swap**.
    ///   `about_to_wait` parks on `WaitUntil` so DeviceEvents flow without
    ///   free-running presents (the old 2 ms-cap-then-flip path hit 300+ Hz).
    pub fn wait_for_swap_slot(&mut self) -> SoftFifoWait {
        let now = clock_monotonic_ns();
        if self.next_deadline_ns == 0 {
            // First frame: aim one period ahead so we don't immediately miss.
            self.next_deadline_ns = now.saturating_add(self.period_ns);
            self.publish_next_gate();
            return SoftFifoWait::Ready { gate_ms: 0.0 };
        }
        let sleep_until = self.next_deadline_ns.saturating_sub(self.swap_lead_ns);
        self.publish_next_gate();
        if sleep_until <= now {
            return SoftFifoWait::Ready { gate_ms: 0.0 };
        }
        let remaining_ns = sleep_until - now;
        let remaining_ms = remaining_ns as f32 / 1_000_000.0;
        let max_inline_ns = (SOFT_FIFO_INLINE_WAIT_MS * 1_000_000.0) as u64;
        if remaining_ns > max_inline_ns {
            return SoftFifoWait::Deferred { remaining_ms };
        }
        clock_nanosleep_abs(sleep_until);
        SoftFifoWait::Ready {
            gate_ms: remaining_ms,
        }
    }

    /// Advance the absolute grid after swap. Snap forward on overrun (no burst catch-up).
    pub fn advance_after_swap(&mut self, post_swap_ns: u64) {
        if self.next_deadline_ns == 0 {
            self.next_deadline_ns = post_swap_ns.saturating_add(self.period_ns);
            self.publish_next_gate();
            return;
        }
        self.next_deadline_ns = self.next_deadline_ns.saturating_add(self.period_ns);
        if self.next_deadline_ns <= post_swap_ns {
            let behind = post_swap_ns - self.next_deadline_ns;
            let missed = behind / self.period_ns + 1;
            self.next_deadline_ns = self.next_deadline_ns.saturating_add(missed * self.period_ns);
            self.phase_slips = self.phase_slips.saturating_add(missed);
        }
        self.publish_next_gate();
    }

    /// Adopt CRTC `refresh_ns` when non-zero (fixes 59.94 vs 60.00 crystal skew).
    pub fn adopt_refresh_ns(&mut self, refresh_ns: u64) {
        if refresh_ns >= 1_000_000 && refresh_ns <= 100_000_000 {
            self.set_period_ns(refresh_ns);
        }
    }

    /// Low-gain PLL nudge from a presentation timestamp (wp_presentation or DRM UST).
    /// `t_presented_ns` is CLOCK_MONOTONIC absolute.
    pub fn apply_presented(&mut self, t_presented_ns: u64, refresh_ns: u64) {
        if refresh_ns > 0 {
            self.adopt_refresh_ns(refresh_ns);
        }
        if self.next_deadline_ns == 0 || self.period_ns == 0 {
            return;
        }
        let period = self.period_ns as i64;
        let raw = (t_presented_ns as i64).wrapping_sub(self.next_deadline_ns as i64) % period;
        let phase_err = if raw > period / 2 {
            raw - period
        } else if raw < -(period / 2) {
            raw + period
        } else {
            raw
        };
        self.last_phase_err_ns = phase_err;
        // ~2% per frame — locks smoothly without stepping.
        let nudge = phase_err / 50;
        if nudge != 0 {
            let next = self.next_deadline_ns as i64 + nudge;
            self.next_deadline_ns = if next < 0 { 0 } else { next as u64 };
            self.publish_next_gate();
        }
    }
}

/// Per-frame stage wall times (ms) for pacing-anomaly breakdown.
///
/// - `poll_ms`: RedrawRequested entry → servo paint (layout/gutter/sync)
/// - `servo_ms`: webview.paint + remote SHM consume
/// - `draw_ms`: scene composite + blit (excludes soft-fifo gate + swap)
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStageMs {
    pub poll_ms: f32,
    pub servo_ms: f32,
    pub draw_ms: f32,
}

/// Stash stages from RedrawRequested; consumed once at `atrium_host_present`.
#[cfg(target_os = "linux")]
std::thread_local! {
    static FRAME_STAGES: std::cell::Cell<FrameStageMs> = const { std::cell::Cell::new(FrameStageMs {
        poll_ms: 0.0,
        servo_ms: 0.0,
        draw_ms: 0.0,
    }) };
}

#[cfg(target_os = "linux")]
pub fn set_frame_stages(stages: FrameStageMs) {
    FRAME_STAGES.with(|c| c.set(stages));
}

#[cfg(target_os = "linux")]
pub fn bump_frame_draw_ms(extra_ms: f32) {
    FRAME_STAGES.with(|c| {
        let mut s = c.get();
        s.draw_ms += extra_ms;
        c.set(s);
    });
}

#[cfg(target_os = "linux")]
pub fn take_frame_stages() -> FrameStageMs {
    FRAME_STAGES.with(|c| c.replace(FrameStageMs::default()))
}

/// Window counters for present-path classification (reset every pacing-summary).
#[cfg(target_os = "linux")]
static DIAG_HOLD_PRESENTS: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "linux")]
static DIAG_VRR_PRIMARY_PRESENTS: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "linux")]
static DIAG_VRR_SKIPS: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "linux")]
static DIAG_SKIP_LOGGED: AtomicU64 = AtomicU64::new(0);

/// Three-clock snapshot for pacing-summary / clocks lines.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default)]
pub struct PresentClockSnap {
    pub ingest_fps: f32,
    pub present_fps: f32,
    pub commits_per_swap: f32,
    pub refresh_hz: f32,
}

#[cfg(target_os = "linux")]
pub fn note_hold_present() {
    DIAG_HOLD_PRESENTS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(target_os = "linux")]
pub fn note_vrr_primary_present() {
    DIAG_VRR_PRIMARY_PRESENTS.fetch_add(1, Ordering::Relaxed);
}

/// RedrawRequested that did not swap under VRR GameActive (no primary damage).
#[cfg(target_os = "linux")]
pub fn note_vrr_skip_present() -> Option<&'static str> {
    let n = DIAG_VRR_SKIPS.fetch_add(1, Ordering::Relaxed) + 1;
    let logged = DIAG_SKIP_LOGGED.fetch_add(1, Ordering::Relaxed);
    if logged < 8 || n % 120 == 0 {
        Some("pacing-vrr-skip (no primary damage — correct for passthrough)")
    } else {
        None
    }
}

/// Hot-path pacing telemetry: in-memory percentiles + anomaly lines only.
#[cfg(target_os = "linux")]
pub struct PacingDiag {
    samples: Vec<f32>,
    phase_errs_ms: Vec<f32>,
    frames: u64,
    phase_slips: u64,
    anomalies: u64,
    /// Window counters (reset every 120-frame summary).
    gate_sleeps: u64,
    gate_lates: u64,
    drm_ok: u64,
    drm_fail: u64,
    pll_applies: u64,
    last_policy: &'static str,
    last_activity: &'static str,
    last_soft_fifo: bool,
    last_clocks: PresentClockSnap,
}

#[cfg(target_os = "linux")]
impl PacingDiag {
    pub fn new() -> Self {
        Self {
            samples: Vec::with_capacity(120),
            phase_errs_ms: Vec::with_capacity(120),
            frames: 0,
            phase_slips: 0,
            anomalies: 0,
            gate_sleeps: 0,
            gate_lates: 0,
            drm_ok: 0,
            drm_fail: 0,
            pll_applies: 0,
            last_policy: "?",
            last_activity: "?",
            last_soft_fifo: false,
            last_clocks: PresentClockSnap::default(),
        }
    }

    fn percentile(sorted: &[f32], p: f32) -> f32 {
        if sorted.is_empty() {
            return 0.0;
        }
        let idx = ((p / 100.0) * (sorted.len() - 1) as f32).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    /// Record one present. Returns a log line when a summary or anomaly should flush.
    ///
    /// Under FixedVsyncLocked, `swap_ms ≈ 13` on 60 Hz is healthy vsync wait — not a stall.
    /// Anomalies are real misses only: swap/dt past ~1.15× period, or SoftFifo phase slips.
    /// (`gate==0` alone is tracked in the summary — it is not an anomaly by itself.)
    pub fn observe(
        &mut self,
        present_dt_ms: f32,
        swap_ms: f32,
        gate_ms: f32,
        phase_err_ms: f32,
        phase_slips_total: u64,
        period_ms: f32,
        stages: FrameStageMs,
        soft_fifo: bool,
        drm_sample_ok: bool,
        pll_applied: bool,
        policy: &'static str,
        activity: &'static str,
        clocks: PresentClockSnap,
    ) -> Option<String> {
        self.frames = self.frames.saturating_add(1);
        self.samples.push(present_dt_ms);
        self.phase_errs_ms.push(phase_err_ms.abs());
        self.last_policy = policy;
        self.last_activity = activity;
        self.last_soft_fifo = soft_fifo;
        self.last_clocks = clocks;
        let slips_delta = phase_slips_total.saturating_sub(self.phase_slips);
        self.phase_slips = phase_slips_total;

        if soft_fifo {
            if gate_ms > 0.0 {
                self.gate_sleeps = self.gate_sleeps.saturating_add(1);
            } else {
                self.gate_lates = self.gate_lates.saturating_add(1);
            }
            if drm_sample_ok {
                self.drm_ok = self.drm_ok.saturating_add(1);
            } else {
                self.drm_fail = self.drm_fail.saturating_add(1);
            }
            if pll_applied {
                self.pll_applies = self.pll_applies.saturating_add(1);
            }
        }

        let miss_thresh = period_ms * 1.15;
        let anomaly =
            swap_ms > miss_thresh || present_dt_ms > miss_thresh || slips_delta > 0;
        let mut out = None;
        if anomaly {
            self.anomalies = self.anomalies.saturating_add(1);
            out = Some(format!(
                "pacing-anomaly #{n} dt={present_dt_ms:.2}ms swap={swap_ms:.2}ms gate={gate_ms:.2}ms phase_err={phase_err_ms:.2}ms slips=+{slips_delta} poll={poll:.2}ms servo={servo:.2}ms draw={draw:.2}ms policy={policy} act={activity}",
                n = self.frames,
                poll = stages.poll_ms,
                servo = stages.servo_ms,
                draw = stages.draw_ms,
            ));
        }

        // ~2s at 60 Hz — fast enough to compare ingest vs host vs panel OSD.
        if self.samples.len() >= 120 {
            let window = self.samples.len() as u64;
            let mut sorted = self.samples.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mut pe = self.phase_errs_ms.clone();
            pe.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let hold = DIAG_HOLD_PRESENTS.swap(0, Ordering::Relaxed);
            let vrr_pri = DIAG_VRR_PRIMARY_PRESENTS.swap(0, Ordering::Relaxed);
            let vrr_skip = DIAG_VRR_SKIPS.swap(0, Ordering::Relaxed);
            let c = self.last_clocks;
            let host_hz = if present_dt_ms > 0.0 {
                // window median host present rate from dt_p50
                1000.0 / Self::percentile(&sorted, 50.0).max(0.001)
            } else {
                c.present_fps
            };
            let summary = format!(
                "pacing-summary frames={} policy={} act={} soft_fifo={} refresh={:.1}Hz \
                 ingest={:.1}fps host={:.1}fps (dt_p50→{:.1}) cps={:.2} \
                 period={period_ms:.2}ms dt_p50={:.2} p95={:.2} p99={:.2} phase_abs_p95={:.2}ms \
                 slips={} anomalies={} gate_sleep={}/{} gate_late={} drm_ok={} drm_fail={} pll={} \
                 hold={} vrr_pri={} vrr_skip={}",
                self.frames,
                self.last_policy,
                self.last_activity,
                self.last_soft_fifo,
                c.refresh_hz,
                c.ingest_fps,
                c.present_fps,
                host_hz,
                c.commits_per_swap,
                Self::percentile(&sorted, 50.0),
                Self::percentile(&sorted, 95.0),
                Self::percentile(&sorted, 99.0),
                Self::percentile(&pe, 95.0),
                self.phase_slips,
                self.anomalies,
                self.gate_sleeps,
                window,
                self.gate_lates,
                self.drm_ok,
                self.drm_fail,
                self.pll_applies,
                hold,
                vrr_pri,
                vrr_skip,
            );
            self.samples.clear();
            self.phase_errs_ms.clear();
            self.anomalies = 0;
            self.gate_sleeps = 0;
            self.gate_lates = 0;
            self.drm_ok = 0;
            self.drm_fail = 0;
            self.pll_applies = 0;
            // Prefer summary; anomaly already logged this frame if any.
            out = Some(match out {
                Some(a) => format!("{a} | {summary}"),
                None => summary,
            });
        }
        out
    }
}

#[cfg(target_os = "linux")]
pub fn activity_label(activity: PresentActivity) -> &'static str {
    match activity {
        PresentActivity::GameActive => "GameActive",
        PresentActivity::UiIdle => "UiIdle",
    }
}
