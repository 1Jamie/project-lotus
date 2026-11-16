//! Read-only gamepad watcher for the Start+Select overlay chord.
//!
//! Pads are opened `O_RDONLY`. Events are copies, so the game keeps its own fd.
//! Never `EVIOCGRAB` or `write()` — exclusive grab on Steam's virtual Xbox pad
//! destroys that node and the game does not see input again until replug.
//! Overlay navigation is mouse, keyboard, and touch. This thread only emits
//! `ToggleOverlay`.

#![cfg(target_os = "linux")]

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use inotify::{EventMask, Inotify, WatchMask};
use log::{debug, info, warn};
use winit::event_loop::EventLoopProxy;

use crate::app::EngineCommand;

const BUS_VIRTUAL: u16 = 0x06;
const VENDOR_VALVE: u16 = 0x28de;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

const BTN_SOUTH: u16 = 0x130; // A
const BTN_EAST: u16 = 0x131; // B
const BTN_NORTH: u16 = 0x133; // X
const BTN_WEST: u16 = 0x134; // Y
const BTN_TL: u16 = 0x136; // LB
const BTN_TR: u16 = 0x137; // RB
const BTN_TL2: u16 = 0x138;
const BTN_TR2: u16 = 0x139;
const BTN_SELECT: u16 = 0x13a;
const BTN_START: u16 = 0x13b;
const BTN_MODE: u16 = 0x13c;
const BTN_THUMBL: u16 = 0x13d;
const BTN_THUMBR: u16 = 0x13e;
const BTN_DPAD_UP: u16 = 0x220;
const BTN_DPAD_DOWN: u16 = 0x221;
const BTN_DPAD_LEFT: u16 = 0x222;
const BTN_DPAD_RIGHT: u16 = 0x223;
const BTN_GAMEPAD: u16 = 0x130;

// Linux keycodes for pad→key when an external pane is focused.
const KEY_UP: u32 = 103;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_DOWN: u32 = 108;
const KEY_ENTER: u32 = 28;
const KEY_ESC: u32 = 1;

const EVIOCGRAB_CODE: libc::c_ulong = 0x4004_4590; // _IOW('E', 0x90, int)

#[repr(C)]
#[derive(Clone, Copy)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    time_sec: libc::time_t,
    time_usec: libc::suseconds_t,
    type_: u16,
    code: u16,
    value: i32,
}

fn eviocgname() -> libc::c_ulong {
    // _IOC(_IOC_READ, 'E', 0x06, 256)
    0x80ff_4506
}

fn eviocgid() -> libc::c_ulong {
    // _IOR('E', 0x02, struct input_id) size 8
    0x8008_4502
}

fn eviocgbit(ev: u32, len: u32) -> libc::c_ulong {
    // _IOC(_IOC_READ=2, 'E', 0x20 + ev, len) — NOT (0x8<<30); that made EVIOCGBIT EINVAL
    // and every TrackedPad::open failed, so Start+Select never saw a device.
    const IOC_READ: u64 = 2;
    let nr = 0x20 + ev;
    let size = (len as u64) & 0x1fff;
    (IOC_READ << 30) | (size << 16) | ((b'E' as u64) << 8) | (nr as u64)
}

fn eviocgabs(axis: u16) -> libc::c_ulong {
    // _IOR('E', 0x40 + axis, struct input_absinfo) size 24
    let nr = 0x40 + axis as u32;
    0x8018_4500 | (nr as libc::c_ulong)
}

fn bit_test(bits: &[u8], bit: u16) -> bool {
    let idx = (bit / 8) as usize;
    let mask = 1u8 << (bit % 8);
    bits.get(idx).map(|b| b & mask != 0).unwrap_or(false)
}

fn ioctl_grab(fd: RawFd, grab: bool) -> io::Result<()> {
    let v: libc::c_int = if grab { 1 } else { 0 };
    let rc = unsafe { libc::ioctl(fd, EVIOCGRAB_CODE, &v as *const libc::c_int) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn read_id(fd: RawFd) -> io::Result<InputId> {
    let mut id = InputId {
        bustype: 0,
        vendor: 0,
        product: 0,
        version: 0,
    };
    let rc = unsafe { libc::ioctl(fd, eviocgid(), &mut id as *mut InputId) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(id)
    }
}

fn read_name(fd: RawFd) -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::ioctl(fd, eviocgname(), buf.as_mut_ptr()) };
    if rc < 0 {
        return String::new();
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).to_string()
}

fn read_bits(fd: RawFd, ev: u32, bytes: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; bytes];
    let rc = unsafe { libc::ioctl(fd, eviocgbit(ev, bytes as u32), buf.as_mut_ptr()) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(buf)
    }
}

fn read_absinfo(fd: RawFd, axis: u16) -> Option<InputAbsinfo> {
    let mut info = InputAbsinfo::default();
    let rc = unsafe { libc::ioctl(fd, eviocgabs(axis), &mut info as *mut InputAbsinfo) };
    if rc < 0 {
        None
    } else {
        Some(info)
    }
}

fn abs_center(info: &InputAbsinfo) -> i32 {
    info.minimum.saturating_add(info.maximum) / 2
}

fn write_events(fd: RawFd, events: &[InputEvent]) -> io::Result<()> {
    let bytes = unsafe {
        std::slice::from_raw_parts(
            events.as_ptr() as *const u8,
            events.len() * std::mem::size_of::<InputEvent>(),
        )
    };
    let mut written = 0;
    while written < bytes.len() {
        match unsafe {
            libc::write(
                fd,
                bytes[written..].as_ptr() as *const libc::c_void,
                bytes.len() - written,
            )
        } {
            n if n < 0 => {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            n => written += n as usize,
        }
    }
    Ok(())
}

fn make_event(type_: u16, code: u16, value: i32) -> InputEvent {
    InputEvent {
        time_sec: 0,
        time_usec: 0,
        type_,
        code,
        value,
    }
}

/// write() neutrals then immediate EVIOCGRAB — Steam Input must see the SYN before grab latches.
fn write_neutral_then_grab(fd: RawFd, axes: &[(u16, i32)], keys: &[u16]) -> io::Result<()> {
    let mut evs: Vec<InputEvent> = Vec::with_capacity(axes.len() + keys.len() + 1);
    for &(axis, center) in axes {
        evs.push(make_event(EV_ABS, axis, center));
    }
    for &code in keys {
        evs.push(make_event(EV_KEY, code, 0));
    }
    evs.push(make_event(EV_SYN, SYN_REPORT, 0));
    write_events(fd, &evs)?;
    ioctl_grab(fd, true)?;
    Ok(())
}

fn write_neutral(fd: RawFd, axes: &[(u16, i32)], keys: &[u16]) -> io::Result<()> {
    let mut evs: Vec<InputEvent> = Vec::with_capacity(axes.len() + keys.len() + 1);
    for &(axis, center) in axes {
        evs.push(make_event(EV_ABS, axis, center));
    }
    for &code in keys {
        evs.push(make_event(EV_KEY, code, 0));
    }
    evs.push(make_event(EV_SYN, SYN_REPORT, 0));
    write_events(fd, &evs)
}

/// Our Guide+X summon uinput (and friends) — never open these.
fn is_summon_or_noise_pad(id: &InputId, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.contains("atrium osk")
        || lower.contains("edge gestures steam")
        || lower.contains("steamos-manager")
        || lower.contains("extest fake")
    {
        return true;
    }
    // Valve vendor but not a gamepad-shaped Steam Input virtual pad.
    if id.vendor == VENDOR_VALVE && id.product == 0 {
        return true;
    }
    false
}

/// Steam Input's synthetic Xbox pad (vendor 0x28DE, often "Microsoft X-Box 360 pad").
/// Physical hidraw may be mode-000 / InputPlumber-locked; this is what games read.
fn is_steam_input_virtual_pad(id: &InputId, name: &str) -> bool {
    if id.vendor != VENDOR_VALVE {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    lower.contains("x-box")
        || lower.contains("xbox")
        || lower.contains("steam virtual")
        || id.product == 0x11ff
}

fn looks_like_gamepad(key_bits: &[u8], abs_bits: &[u8]) -> bool {
    if !bit_test(key_bits, BTN_GAMEPAD) && !bit_test(key_bits, BTN_SOUTH) {
        return false;
    }
    // Need at least one stick or hat / dpad.
    bit_test(abs_bits, ABS_X)
        || bit_test(abs_bits, ABS_HAT0X)
        || bit_test(key_bits, BTN_DPAD_UP)
}

struct TrackedPad {
    path: PathBuf,
    file: File,
    #[allow(dead_code)]
    name: String,
    /// False when we only got O_RDONLY (still enough for Start+Select chords).
    can_grab: bool,
    /// Steam Input virtual Xbox pad — preferred watch target when physical is locked.
    steam_virtual: bool,
    grabbed: bool,
    axes: Vec<(u16, i32)>,
    keys: Vec<u16>,
    select_down: bool,
    start_down: bool,
    chord_latched: bool,
    /// Last ABS_HAT0X / ABS_HAT0Y — Nav only on edges so a held hat cannot
    /// re-queue "up" every poll after coalesced drain.
    hat_x: i32,
    hat_y: i32,
}

impl TrackedPad {
    fn open(path: &Path) -> io::Result<Option<Self>> {
        // Read-only. A writeable fd is what makes EVIOCGRAB possible; we never
        // want that. Evdev delivers a copy to every reader.
        thread::sleep(Duration::from_millis(50));
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        let can_grab = false;
        let fd = file.as_raw_fd();
        let id = read_id(fd)?;
        let name = read_name(fd);
        if is_summon_or_noise_pad(&id, &name) {
            debug!(
                "[evdev-pad] skip summon/noise {} vendor={:#x} name={:?}",
                path.display(),
                id.vendor,
                name
            );
            return Ok(None);
        }
        let steam_virtual = is_steam_input_virtual_pad(&id, &name);
        // Generic BUS_VIRTUAL non-gamepad nodes (not Steam's Xbox synthesizer).
        if id.bustype == BUS_VIRTUAL && !steam_virtual {
            debug!(
                "[evdev-pad] skip bus_virtual {} name={:?}",
                path.display(),
                name
            );
            return Ok(None);
        }
        // KEY_MAX≈0x2ff → ~96 bytes; keep a little headroom.
        let key_bits = match read_bits(fd, EV_KEY as u32, 128) {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    "[evdev-pad] EVIOCGBIT(EV_KEY) failed on {}: {} — skipping",
                    path.display(),
                    e
                );
                return Ok(None);
            }
        };
        let abs_bits = read_bits(fd, EV_ABS as u32, 64).unwrap_or_default();
        if !looks_like_gamepad(&key_bits, &abs_bits) {
            debug!(
                "[evdev-pad] not a gamepad {} ({:?})",
                path.display(),
                name
            );
            return Ok(None);
        }

        let mut axes = Vec::new();
        // Sticks + hats. Hats were missing from the neutral flush — overlay Nav
        // via ABS_HAT0Y left the axis at -1 after ungrab, so the game saw stuck
        // "up" and ignored the rest of the pad until restart.
        for axis in [
            ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ, ABS_HAT0X, ABS_HAT0Y,
        ] {
            if bit_test(&abs_bits, axis) {
                if let Some(info) = read_absinfo(fd, axis) {
                    // Hats are typically -1/0/1; abs_center → 0.
                    axes.push((axis, abs_center(&info)));
                }
            }
        }

        let mut keys = Vec::new();
        for code in [
            BTN_SOUTH,
            BTN_EAST,
            BTN_NORTH,
            BTN_WEST,
            BTN_TL,
            BTN_TR,
            BTN_TL2,
            BTN_TR2,
            BTN_SELECT,
            BTN_START,
            BTN_MODE,
            BTN_THUMBL,
            BTN_THUMBR,
            BTN_DPAD_UP,
            BTN_DPAD_DOWN,
            BTN_DPAD_LEFT,
            BTN_DPAD_RIGHT,
        ] {
            if bit_test(&key_bits, code) {
                keys.push(code);
            }
        }

        info!(
            "[evdev-pad] tracking {} ({:?}) vendor={:#x} product={:#x} grab={} steam_virt={}",
            path.display(),
            name,
            id.vendor,
            id.product,
            can_grab,
            steam_virtual
        );

        Ok(Some(Self {
            path: path.to_path_buf(),
            file,
            name,
            can_grab,
            steam_virtual,
            grabbed: false,
            axes,
            keys,
            select_down: false,
            start_down: false,
            chord_latched: false,
            hat_x: 0,
            hat_y: 0,
        }))
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    fn enter_overlay_grab(&mut self) {
        if self.grabbed || !self.can_grab {
            return;
        }
        match write_neutral_then_grab(self.fd(), &self.axes, &self.keys) {
            Ok(()) => {
                self.grabbed = true;
                self.hat_x = 0;
                self.hat_y = 0;
                self.select_down = false;
                self.start_down = false;
                info!(
                    "[evdev-pad] grabbed {} (steam_virt={}) for overlay exclusive",
                    self.path.display(),
                    self.steam_virtual
                );
            }
            Err(e) => warn!(
                "[evdev-pad] grab failed on {}: {}",
                self.path.display(),
                e
            ),
        }
    }

    fn leave_grab(&mut self) {
        // Always attempt EVIOCGRAB(0) when we can — a desynced `grabbed=false`
        // while the fd is still exclusive leaves the game with no pad until restart.
        let was = self.grabbed;
        if self.can_grab {
            // Neutral while still exclusive, then drop grab, then neutral again so
            // Steam/Proton definitely observe sticks/hats/buttons released (open
            // overlay → Nav up → close must not leave ABS_HAT0Y=-1 latched).
            let _ = write_neutral(self.fd(), &self.axes, &self.keys);
            let _ = ioctl_grab(self.fd(), false);
            self.grabbed = false;
            if let Err(e) = write_neutral(self.fd(), &self.axes, &self.keys) {
                warn!(
                    "[evdev-pad] post-ungrab neutral flush failed on {}: {}",
                    self.path.display(),
                    e
                );
            }
        } else {
            self.grabbed = false;
        }
        self.hat_x = 0;
        self.hat_y = 0;
        self.select_down = false;
        self.start_down = false;
        self.chord_latched = false;
        if was {
            info!(
                "[evdev-pad] ungrabbed {} (steam_virt={}) — game may read pads",
                self.path.display(),
                self.steam_virtual
            );
        }
    }

    /// Force a full neutral report without changing grab (overlay open path /
    /// mode swap hygiene).
    fn flush_neutral(&mut self) {
        if !self.can_grab {
            return;
        }
        if let Err(e) = write_neutral(self.fd(), &self.axes, &self.keys) {
            warn!(
                "[evdev-pad] neutral flush failed on {}: {}",
                self.path.display(),
                e
            );
        }
        self.hat_x = 0;
        self.hat_y = 0;
    }
}

#[derive(Default)]
struct SharedPadState {
    /// true while atrium overlay is open (!game_mode).
    overlay: bool,
    /// true while Steam/DE OSK is up — forces ungrab.
    osk_active: bool,
    /// Re-run grab policy even when overlay/osk flags are unchanged.
    force_grab_refresh: bool,
    /// External nest pane that should receive pad→key (arrows/enter/esc).
    key_forward_pane: Option<String>,
    /// Servo plugin tab pane for direct AtriumFocus inject (no Node hop).
    spatial_focus_pane: Option<String>,
}

struct PadHub {
    window_id: String,
    proxy: EventLoopProxy<EngineCommand>,
    state: Arc<Mutex<SharedPadState>>,
    stop: Arc<AtomicBool>,
}

static HUB: OnceLock<Mutex<Option<PadHub>>> = OnceLock::new();

fn hub_mutex() -> &'static Mutex<Option<PadHub>> {
    HUB.get_or_init(|| Mutex::new(None))
}

pub fn ensure_started(window_id: String, proxy: EventLoopProxy<EngineCommand>) {
    let mut guard = hub_mutex().lock().unwrap();
    if let Some(hub) = guard.as_ref() {
        if hub.window_id == window_id {
            return;
        }
        hub.stop.store(true, Ordering::SeqCst);
    }
    let state = Arc::new(Mutex::new(SharedPadState::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let hub = PadHub {
        window_id: window_id.clone(),
        proxy: proxy.clone(),
        state: state.clone(),
        stop: stop.clone(),
    };
    *guard = Some(hub);
    drop(guard);

    thread::Builder::new()
        .name("atrium-evdev-pad".into())
        .spawn(move || pad_thread(window_id, proxy, state, stop))
        .expect("spawn atrium-evdev-pad");
    info!("[evdev-pad] watcher started");
}

pub fn set_overlay_mode(overlay: bool) {
    if let Ok(guard) = hub_mutex().lock() {
        if let Some(hub) = guard.as_ref() {
            if let Ok(mut st) = hub.state.lock() {
                let changed = st.overlay != overlay;
                st.overlay = overlay;
                if !overlay {
                    // Closing overlay: drop forwards so we never keep eating
                    // pad→key / spatial inject while game_mode is restored.
                    st.key_forward_pane = None;
                    st.spatial_focus_pane = None;
                }
                // Always refresh grab policy. Closing with force=false skipped
                // apply when `overlay` was already false after a chord-grab race,
                // leaving EVIOCGRAB held — overlay dismissed, game got no pads.
                st.force_grab_refresh = true;
                if changed {
                    // Drop coalesced Nav/Confirm so a held hat "up" cannot keep
                    // scrolling atrium (or look stuck) across the mode swap.
                    clear_pending_overlay_focus();
                }
            }
            let _ = hub.proxy.send_event(EngineCommand::Wake);
        }
    }
}

/// Clear overlay focus steps queued while chrome was open / about to close.
fn clear_pending_overlay_focus() {
    if let Ok(mut st) = pending_pad_state().lock() {
        st.nav = None;
        st.confirm = None;
    }
}

/// Re-attempt EVIOCGRAB on all RW pads while overlay is open (Steam Input race).
pub fn retry_overlay_grabs() {
    if let Ok(guard) = hub_mutex().lock() {
        if let Some(hub) = guard.as_ref() {
            if let Ok(mut st) = hub.state.lock() {
                if st.overlay {
                    st.force_grab_refresh = true;
                }
            }
            let _ = hub.proxy.send_event(EngineCommand::Wake);
        }
    }
}

pub fn set_osk_active(active: bool) {
    if let Ok(guard) = hub_mutex().lock() {
        if let Some(hub) = guard.as_ref() {
            if let Ok(mut st) = hub.state.lock() {
                st.osk_active = active;
            }
            let _ = hub.proxy.send_event(EngineCommand::Wake);
        }
    }
}

pub fn set_key_forward_pane(pane_id: Option<String>) {
    if let Ok(guard) = hub_mutex().lock() {
        if let Some(hub) = guard.as_ref() {
            if let Ok(mut st) = hub.state.lock() {
                st.key_forward_pane = pane_id;
                // Raw key-forward and spatial focus are mutually exclusive.
                if st.key_forward_pane.is_some() {
                    st.spatial_focus_pane = None;
                }
            }
        }
    }
}

pub fn set_spatial_focus_pane(pane_id: Option<String>) {
    if let Ok(guard) = hub_mutex().lock() {
        if let Some(hub) = guard.as_ref() {
            if let Ok(mut st) = hub.state.lock() {
                st.spatial_focus_pane = pane_id;
                if st.spatial_focus_pane.is_some() {
                    st.key_forward_pane = None;
                }
            }
        }
    }
}

pub fn spatial_focus_pane() -> Option<String> {
    let Ok(guard) = hub_mutex().lock() else {
        return None;
    };
    let hub = guard.as_ref()?;
    let Ok(st) = hub.state.lock() else {
        return None;
    };
    st.spatial_focus_pane.clone()
}

/// Pad input bypasses the winit `EngineCommand` FIFO. Nest RedrawWindow /
/// ExecuteScript storms drain that FIFO before RedrawRequested; a deep Nav
/// backlog then snap-catch-up. Nav uses a single latest-dir slot; discrete
/// actions (Confirm/Tab/Toggle) use a short edge queue.
#[derive(Debug, Clone)]
pub enum PendingPadInput {
    Action {
        window_id: String,
        action: String,
        dir: Option<String>,
    },
    Key {
        window_id: String,
        pane_id: String,
        keycode: u32,
        pressed: bool,
    },
}

const PENDING_EDGE_CAP: usize = 8;

struct PendingPadState {
    /// Latest overlay Nav only — overwrite, never replay a stall backlog.
    nav: Option<(String, String)>, // window_id, dir
    /// At most one pending Confirm (latest window wins).
    confirm: Option<String>, // window_id
    /// Tab / Toggle / Back / Osk / pad→key edges (not Nav/Confirm).
    edges: VecDeque<PendingPadInput>,
}

fn pending_pad_state() -> &'static Mutex<PendingPadState> {
    static S: OnceLock<Mutex<PendingPadState>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(PendingPadState {
            nav: None,
            confirm: None,
            edges: VecDeque::with_capacity(PENDING_EDGE_CAP),
        })
    })
}

static PAD_WAKE_PENDING: AtomicBool = AtomicBool::new(false);

fn wake_pad(proxy: &EventLoopProxy<EngineCommand>) {
    if !PAD_WAKE_PENDING.swap(true, Ordering::SeqCst) {
        let _ = proxy.send_event(EngineCommand::Wake);
    }
}

fn push_pending_pad(proxy: &EventLoopProxy<EngineCommand>, item: PendingPadInput) {
    if let Ok(mut st) = pending_pad_state().lock() {
        match &item {
            PendingPadInput::Action {
                window_id,
                action,
                dir,
            } if action == "Nav" => {
                let d = dir.as_deref().unwrap_or("down").to_string();
                st.nav = Some((window_id.clone(), d));
            }
            PendingPadInput::Action {
                window_id,
                action,
                ..
            } if action == "Confirm" => {
                st.confirm = Some(window_id.clone());
            }
            _ => {
                while st.edges.len() >= PENDING_EDGE_CAP {
                    st.edges.pop_front();
                }
                st.edges.push_back(item);
            }
        }
    }
    wake_pad(proxy);
}

/// Drain pad input for the GL thread.
///
/// - Always takes Tab/Toggle/Back/Osk/key edges.
/// - When `allow_focus_step`, also takes **at most one** Nav/Confirm (Confirm wins).
///   Pass `true` once per present/`about_to_wait`, not on every UserEvent, or a
///   RedrawWindow flood would still burst-apply focus steps.
/// - ToggleOverlay in the same batch drops Nav/Confirm — a stale coalesced "up"
///   from an earlier overlay visit must not apply after Start+Select alone.
pub fn drain_pending_pad(allow_focus_step: bool) -> Vec<PendingPadInput> {
    PAD_WAKE_PENDING.store(false, Ordering::SeqCst);
    let Ok(mut st) = pending_pad_state().lock() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(st.edges.len() + 1);
    while let Some(edge) = st.edges.pop_front() {
        out.push(edge);
    }
    let toggling = out.iter().any(|e| {
        matches!(
            e,
            PendingPadInput::Action { action, .. } if action == "ToggleOverlay"
        )
    });
    if toggling {
        st.nav = None;
        st.confirm = None;
    } else if allow_focus_step {
        if let Some(window_id) = st.confirm.take() {
            st.nav = None;
            out.push(PendingPadInput::Action {
                window_id,
                action: "Confirm".to_string(),
                dir: None,
            });
        } else if let Some((window_id, dir)) = st.nav.take() {
            out.push(PendingPadInput::Action {
                window_id,
                action: "Nav".to_string(),
                dir: Some(dir),
            });
        }
    }
    out
}

pub fn pending_pad_len() -> usize {
    pending_pad_state()
        .lock()
        .map(|st| {
            st.edges.len()
                + usize::from(st.nav.is_some())
                + usize::from(st.confirm.is_some())
        })
        .unwrap_or(0)
}

fn emit_action(
    proxy: &EventLoopProxy<EngineCommand>,
    window_id: &str,
    action: &str,
    dir: Option<&str>,
) {
    push_pending_pad(
        proxy,
        PendingPadInput::Action {
            window_id: window_id.to_string(),
            action: action.to_string(),
            dir: dir.map(|s| s.to_string()),
        },
    );
}

/// Cross-device debounce: Steam virtual + physical often see the same chord and
/// would ToggleOverlay twice (open then immediately close).
struct ActionDebouncer {
    last_toggle: Option<Instant>,
    last_bumper: Option<Instant>,
    last_back: Option<Instant>,
    /// Chord held on any tracked pad — suppress re-fire until fully released.
    chord_held: bool,
}

impl ActionDebouncer {
    fn new() -> Self {
        Self {
            last_toggle: None,
            last_bumper: None,
            last_back: None,
            chord_held: false,
        }
    }

    fn allow_toggle(&mut self) -> bool {
        const COOLDOWN: Duration = Duration::from_millis(550);
        if self.chord_held {
            return false;
        }
        if let Some(t) = self.last_toggle {
            if t.elapsed() < COOLDOWN {
                return false;
            }
        }
        self.chord_held = true;
        self.last_toggle = Some(Instant::now());
        true
    }

    fn chord_released_all(&mut self, any_chord_still_down: bool) {
        if !any_chord_still_down {
            self.chord_held = false;
        }
    }

    fn allow_bumper(&mut self) -> bool {
        // Keep short enough that LB/RB tab swaps feel immediate; long enough to
        // collapse Steam-virtual + physical double-fire.
        const COOLDOWN: Duration = Duration::from_millis(120);
        if let Some(t) = self.last_bumper {
            if t.elapsed() < COOLDOWN {
                return false;
            }
        }
        self.last_bumper = Some(Instant::now());
        true
    }

    fn allow_back(&mut self) -> bool {
        const COOLDOWN: Duration = Duration::from_millis(350);
        if let Some(t) = self.last_back {
            if t.elapsed() < COOLDOWN {
                return false;
            }
        }
        self.last_back = Some(Instant::now());
        true
    }
}

fn emit_key(
    proxy: &EventLoopProxy<EngineCommand>,
    window_id: &str,
    pane_id: &str,
    keycode: u32,
    pressed: bool,
) {
    push_pending_pad(
        proxy,
        PendingPadInput::Key {
            window_id: window_id.to_string(),
            pane_id: pane_id.to_string(),
            keycode,
            pressed,
        },
    );
}

fn enumerate_pads(pads: &mut HashMap<PathBuf, TrackedPad>) {
    let Ok(entries) = fs::read_dir("/dev/input") else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("event") {
            continue;
        }
        if pads.contains_key(&path) {
            continue;
        }
        match TrackedPad::open(&path) {
            Ok(Some(pad)) => {
                pads.insert(path, pad);
            }
            Ok(None) => {}
            Err(e) => {
                debug!("[evdev-pad] open {}: {}", path.display(), e);
            }
        }
    }
}

#[allow(dead_code)]
fn apply_grab_policy(pads: &mut HashMap<PathBuf, TrackedPad>, overlay: bool, osk_active: bool) {
    let want_grab = overlay && !osk_active;
    // Games (Proton/SDL) read Steam Input's virtual Xbox node — NOT the physical
    // hidraw. The old policy grabbed physical only and left the virtual free, so
    // overlay chords hit atrium while the nest kept seeing the same presses
    // (laggy dual-path). Grab every RW pad, including steam_virtual, and neutral
    // flush so the game sees sticks/buttons/hats released.
    for pad in pads.values_mut() {
        if !want_grab {
            pad.leave_grab();
            continue;
        }
        // Already exclusive: still flush neutrals so a mid-overlay rescan cannot
        // leave a latched hat from before the grab.
        if pad.grabbed {
            pad.flush_neutral();
            continue;
        }
        pad.enter_overlay_grab();
    }
}

/// Single source for overlay semantic actions — avoid physical + Steam virtual
/// both firing TabNext/Nav for one press.
#[allow(dead_code)]
fn overlay_action_source(pads: &HashMap<PathBuf, TrackedPad>) -> Option<PathBuf> {
    let mut best: Option<(&PathBuf, i32)> = None;
    for (path, pad) in pads {
        // Prefer grabbed physical, then grabbed steam virt, then any tracked.
        let score = if pad.grabbed && !pad.steam_virtual {
            300
        } else if pad.grabbed && pad.steam_virtual {
            200
        } else if !pad.steam_virtual {
            100
        } else {
            50
        };
        if best.map(|(_, s)| score > s).unwrap_or(true) {
            best = Some((path, score));
        }
    }
    best.map(|(p, _)| p.clone())
}

#[allow(dead_code)]
fn map_overlay_button(
    code: u16,
    pressed: bool,
    proxy: &EventLoopProxy<EngineCommand>,
    window_id: &str,
    key_pane: Option<&str>,
    debounce: &mut ActionDebouncer,
) {
    if !pressed {
        // Release: only pad→key needs releases.
        if let Some(pane) = key_pane {
            let key = match code {
                BTN_DPAD_UP => Some(KEY_UP),
                BTN_DPAD_DOWN => Some(KEY_DOWN),
                BTN_DPAD_LEFT => Some(KEY_LEFT),
                BTN_DPAD_RIGHT => Some(KEY_RIGHT),
                BTN_SOUTH => Some(KEY_ENTER),
                BTN_EAST => Some(KEY_ESC),
                _ => None,
            };
            if let Some(k) = key {
                emit_key(proxy, window_id, pane, k, false);
            }
        }
        return;
    }

    match code {
        BTN_TL => {
            if debounce.allow_bumper() {
                emit_action(proxy, window_id, "TabPrev", None);
            }
        }
        BTN_TR => {
            if debounce.allow_bumper() {
                emit_action(proxy, window_id, "TabNext", None);
            }
        }
        BTN_NORTH => emit_action(proxy, window_id, "OskToggle", None),
        BTN_EAST => {
            if let Some(pane) = key_pane {
                emit_key(proxy, window_id, pane, KEY_ESC, true);
            } else if debounce.allow_back() {
                emit_action(proxy, window_id, "Back", None);
            }
        }
        BTN_SOUTH => {
            if let Some(pane) = key_pane {
                emit_key(proxy, window_id, pane, KEY_ENTER, true);
            } else {
                emit_action(proxy, window_id, "Confirm", None);
            }
        }
        BTN_DPAD_UP => {
            if let Some(pane) = key_pane {
                emit_key(proxy, window_id, pane, KEY_UP, true);
            } else {
                emit_action(proxy, window_id, "Nav", Some("up"));
            }
        }
        BTN_DPAD_DOWN => {
            if let Some(pane) = key_pane {
                emit_key(proxy, window_id, pane, KEY_DOWN, true);
            } else {
                emit_action(proxy, window_id, "Nav", Some("down"));
            }
        }
        BTN_DPAD_LEFT => {
            if let Some(pane) = key_pane {
                emit_key(proxy, window_id, pane, KEY_LEFT, true);
            } else {
                emit_action(proxy, window_id, "Nav", Some("left"));
            }
        }
        BTN_DPAD_RIGHT => {
            if let Some(pane) = key_pane {
                emit_key(proxy, window_id, pane, KEY_RIGHT, true);
            } else {
                emit_action(proxy, window_id, "Nav", Some("right"));
            }
        }
        _ => {}
    }
}

fn pad_thread(
    window_id: String,
    proxy: EventLoopProxy<EngineCommand>,
    state: Arc<Mutex<SharedPadState>>,
    stop: Arc<AtomicBool>,
) {
    let mut pads: HashMap<PathBuf, TrackedPad> = HashMap::new();
    enumerate_pads(&mut pads);

    let mut inotify = Inotify::init().ok();
    if let Some(ref mut ino) = inotify {
        if let Err(e) = ino.watches().add(
            "/dev/input",
            WatchMask::CREATE | WatchMask::ATTRIB | WatchMask::DELETE,
        ) {
            warn!("[evdev-pad] inotify watch failed: {e}");
            inotify = None;
        }
    } else {
        warn!("[evdev-pad] inotify init failed — periodic rescan only");
    }

    let mut inotify_buf = [0u8; 1024];
    let mut last_scan = Instant::now();
    let mut debounce = ActionDebouncer::new();

    while !stop.load(Ordering::SeqCst) {
        // Drop grab-refresh requests. This thread never takes the device.
        {
            let mut st = state.lock().unwrap();
            st.force_grab_refresh = false;
        }

        // Hotplug events.
        if let Some(ref mut ino) = inotify {
            if let Ok(events) = ino.read_events(&mut inotify_buf) {
                let mut need_scan = false;
                for ev in events {
                    if ev.mask.contains(EventMask::CREATE)
                        || ev.mask.contains(EventMask::ATTRIB)
                        || ev.mask.contains(EventMask::DELETE)
                    {
                        need_scan = true;
                    }
                    if ev.mask.contains(EventMask::DELETE) {
                        if let Some(name) = ev.name {
                            let path = PathBuf::from("/dev/input").join(name);
                            pads.remove(&path);
                        }
                    }
                }
                if need_scan {
                    thread::sleep(Duration::from_millis(80));
                    enumerate_pads(&mut pads);
                }
            }
        }

        if last_scan.elapsed() > Duration::from_secs(3) {
            enumerate_pads(&mut pads);
            last_scan = Instant::now();
        }

        // Start+Select on any pad. No other buttons are consumed or forwarded.
        let paths: Vec<PathBuf> = pads.keys().cloned().collect();
        for path in paths {
            let Some(pad) = pads.get_mut(&path) else {
                continue;
            };
            let mut buf = [0u8; 24 * 32];
            loop {
                match pad.file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let sz = std::mem::size_of::<InputEvent>();
                        let count = n / sz;
                        for i in 0..count {
                            let off = i * sz;
                            let ev = unsafe {
                                std::ptr::read_unaligned(
                                    buf[off..].as_ptr() as *const InputEvent,
                                )
                            };
                            if ev.type_ != EV_KEY {
                                continue;
                            }
                            let pressed = ev.value != 0;
                            match ev.code {
                                BTN_SELECT => pad.select_down = pressed,
                                BTN_START => pad.start_down = pressed,
                                _ => continue,
                            }

                            // Start+Select → ToggleOverlay. Read-only: the game
                            // still receives the buttons. Debounced so a Steam
                            // virtual pad and a USB pad don't toggle twice.
                            if pad.select_down && pad.start_down {
                                pad.chord_latched = true;
                                if debounce.allow_toggle() {
                                    clear_pending_overlay_focus();
                                    emit_action(&proxy, &window_id, "ToggleOverlay", None);
                                }
                                continue;
                            }
                            if !pad.select_down && !pad.start_down {
                                pad.chord_latched = false;
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        warn!("[evdev-pad] read {}: {}", path.display(), e);
                        pads.remove(&path);
                        break;
                    }
                }
            }
        }

        let any_chord = pads
            .values()
            .any(|p| p.select_down && p.start_down);
        debounce.chord_released_all(any_chord);

        thread::sleep(Duration::from_millis(8));
    }

    info!("[evdev-pad] watcher stopped");
}
