//! Host presentation feedback for SoftFifoPacer PLL.
//!
//! Under gamescope atrium is an X11 client (`WINIT_UNIX_BACKEND=x11`), so outer
//! `wp_presentation` is unavailable. We sample the DRM CRTC's last vblank UST
//! (CLOCK_MONOTONIC when `DRM_CAP_TIMESTAMP_MONOTONIC` is set) — same phase
//! signal `wp_presentation.presented` would give on a Wayland host.
//!
//! When the host window *is* Wayland, [`note_wayland_presented`] can feed the
//! same atomics from a `wp_presentation_feedback.presented` handler.

use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::Mutex;

/// Latest CRTC presentation sample: (t_presented_ns, refresh_ns). refresh may be 0.
static LAST_PRESENTED_NS: AtomicU64 = AtomicU64::new(0);
static LAST_REFRESH_NS: AtomicU64 = AtomicU64::new(0);
static FEEDBACK_SEQ: AtomicU64 = AtomicU64::new(0);

/// Cached eglSwapInterval — avoid Mesa Wayland/X11 round-trips every frame.
static CACHED_SWAP_INTERVAL: AtomicI32 = AtomicI32::new(-1);

pub fn cached_swap_interval() -> Option<i32> {
    let v = CACHED_SWAP_INTERVAL.load(Ordering::Relaxed);
    if v < 0 {
        None
    } else {
        Some(v)
    }
}

pub fn set_cached_swap_interval(interval: i32) {
    CACHED_SWAP_INTERVAL.store(interval, Ordering::Relaxed);
}

pub fn clear_cached_swap_interval() {
    CACHED_SWAP_INTERVAL.store(-1, Ordering::Relaxed);
}

/// Publish a presentation sample for the SoftFifoPacer PLL.
pub fn note_presented(t_presented_ns: u64, refresh_ns: u64) {
    if t_presented_ns == 0 {
        return;
    }
    LAST_PRESENTED_NS.store(t_presented_ns, Ordering::Release);
    if refresh_ns > 0 {
        LAST_REFRESH_NS.store(refresh_ns, Ordering::Release);
    }
    FEEDBACK_SEQ.fetch_add(1, Ordering::Release);
}

/// Wayland `wp_presentation_feedback.presented` entry point (outer wl_surface).
#[allow(dead_code)]
pub fn note_wayland_presented(tv_sec: u64, tv_nsec: u32, refresh_ns: u64) {
    let t = tv_sec.saturating_mul(1_000_000_000).saturating_add(tv_nsec as u64);
    note_presented(t, refresh_ns);
}

/// Take the latest unread presentation sample, if any since `last_seq`.
pub fn take_presented_since(last_seq: &mut u64) -> Option<(u64, u64)> {
    let seq = FEEDBACK_SEQ.load(Ordering::Acquire);
    if seq == *last_seq {
        return None;
    }
    *last_seq = seq;
    let t = LAST_PRESENTED_NS.load(Ordering::Acquire);
    let refresh = LAST_REFRESH_NS.load(Ordering::Acquire);
    if t == 0 {
        return None;
    }
    Some((t, refresh))
}

struct DrmVblankState {
    fd: libc::c_int,
    pipe: u32,
    monotonic: bool,
}

static DRM_STATE: Mutex<Option<DrmVblankState>> = Mutex::new(None);

const DRM_IOCTL_WAIT_VBLANK: libc::c_ulong = 0xc018643a;
const DRM_IOCTL_GET_CAP: libc::c_ulong = 0xc010640c;
const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;
const DRM_VBLANK_RELATIVE: u32 = 0x1;
const DRM_VBLANK_EVENT: u32 = 0x4000000; // unused; we use relative query
#[allow(dead_code)]
const _DRM_VBLANK_EVENT_HOLD: u32 = DRM_VBLANK_EVENT;

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmWaitVblankRequest {
    typ: u32,
    sequence: u32,
    signal: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmWaitVblankReply {
    typ: u32,
    sequence: u32,
    tval_sec: i64,
    tval_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
union DrmWaitVblank {
    request: DrmWaitVblankRequest,
    reply: DrmWaitVblankReply,
}

#[repr(C)]
struct DrmGetCap {
    capability: u64,
    value: u64,
}

fn open_drm_card() -> Option<(libc::c_int, bool)> {
    let dir = std::fs::read_dir("/dev/dri").ok()?;
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        use std::os::unix::ffi::OsStrExt;
        let mut c_path = entry.path().as_os_str().as_bytes().to_vec();
        c_path.push(0);
        let fd = unsafe {
            libc::open(
                c_path.as_ptr() as *const libc::c_char,
                libc::O_RDWR | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            continue;
        }
        let mut cap = DrmGetCap {
            capability: DRM_CAP_TIMESTAMP_MONOTONIC,
            value: 0,
        };
        let mono = unsafe { libc::ioctl(fd, DRM_IOCTL_GET_CAP, &mut cap) } == 0 && cap.value != 0;
        return Some((fd, mono));
    }
    None
}

fn ensure_drm() -> bool {
    let mut guard = DRM_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_some() {
        return true;
    }
    let Some((fd, mono)) = open_drm_card() else {
        return false;
    };
    *guard = Some(DrmVblankState {
        fd,
        pipe: 0,
        monotonic: mono,
    });
    log::info!(
        "[lotus-pacing] DRM vblank feedback armed (timestamp_monotonic={mono})"
    );
    true
}

/// Sample last CRTC vblank UST into the presentation atomics.
/// Call after host eglSwapBuffers under gamescope Fixed soft-FIFO.
/// Returns `true` when a usable monotonic UST was published.
pub fn sample_drm_vblank_feedback(refresh_ns_hint: u64) -> bool {
    if !ensure_drm() {
        return false;
    }
    let mut guard = DRM_STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(state) = guard.as_mut() else {
        return false;
    };
    let mut vbl = DrmWaitVblank {
        request: DrmWaitVblankRequest {
            typ: DRM_VBLANK_RELATIVE | (state.pipe << 1),
            sequence: 0, // query current / last
            signal: 0,
        },
    };
    let rc = unsafe { libc::ioctl(state.fd, DRM_IOCTL_WAIT_VBLANK, &mut vbl) };
    if rc != 0 {
        return false;
    }
    let reply = unsafe { vbl.reply };
    if !state.monotonic {
        // Fall back: skip PLL rather than mix CLOCK_REALTIME into MONOTONIC deadlines.
        return false;
    }
    let t_ns = (reply.tval_sec as u64).saturating_mul(1_000_000_000)
        + (reply.tval_usec as u64).saturating_mul(1_000);
    // Defensive: reject zero / absurd stamps.
    if t_ns < 1_000_000 {
        return false;
    }
    note_presented(t_ns, refresh_ns_hint);
    true
}
