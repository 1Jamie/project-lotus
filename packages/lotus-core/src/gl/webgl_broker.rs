//! Host-side WebGL GPU socket feeder + process-wide GL thread + canvas memfd planes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;

use log::{debug, error, info, trace, warn};
use lotus_supervisor::canvas_plane::{create_canvas_plane, publish_frame};
use lotus_supervisor::gpu_frame::{
    discard_exact_fd, read_exact_fd, read_header, write_frame, write_frame_with_fd, FrameRouting,
    GpuFrameHeader, KIND_CANVAS_BOUNDS, KIND_CANVAS_PRESENT, KIND_COMMAND, KIND_PING, KIND_PONG,
    KIND_REPLY,
};
use lotus_supervisor::shm::SealedMemfdPlane;
use paint_api::rendering_context::RenderingContext;
use paint_api::{
    CrossProcessPaintApi, PainterSurfmanDetails, PainterSurfmanDetailsMap,
    WebRenderExternalImageIdManager,
};
use parking_lot::Mutex as ParkingMutex;
use servo_base::generic_channel::GenericSender;
use servo_base::id::TEST_PAINTER_ID;
use servo_canvas_traits::webgl::{webgl_channel, WebGLContextId, WebGLCreateContextResult, WebGLMsg};
use surfman::chains::{SwapChainAPI, SwapChains, SwapChainsAPI};
use surfman::{Adapter, Connection, Device, SurfaceTexture};
use webgl::broker_msg::{WireCreateContextOk, WireReply, WireWebGLMsg};
use webgl::broker_wire::rehydrate_webgl_command;
use webgl::webgl_presented_frames::WebGLPresentedBufferMap;
use webgl::webgl_thread::{WebGLContextBusyMap, WebGLThread, WebGLThreadInit};

static BROKER_THREADS: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

static GL_READY: AtomicBool = AtomicBool::new(false);
static GL_SENDER: Mutex<Option<GenericSender<WebGLMsg>>> = Mutex::new(None);

/// Window compositor Connection/Adapter — broker uses this (not `Connection::new()`)
/// so Surfaces stay shareable with the UI-thread Device (required for the GPU broker).
static WINDOW_SURFMAN: Mutex<Option<(Connection, Adapter)>> = Mutex::new(None);

/// Shared swap chains for the host WebGLThread (reliable present) and experimental punch.
static BROKER_SWAP_CHAINS: Mutex<Option<SwapChains<WebGLContextId, Device>>> = Mutex::new(None);
/// Serializes experimental UI Surface bounce vs GL swap_buffers.
static SWAP_CHAIN_GATE: LazyLock<Arc<Mutex<()>>> = LazyLock::new(|| Arc::new(Mutex::new(())));
/// EXPERIMENTAL: last imported front buffer per context (UI thread only).
struct PunchFront {
    surface_texture: SurfaceTexture,
    gl_tex: u32,
    width: i32,
    height: i32,
}
thread_local! {
    static PUNCH_FRONTS: RefCell<HashMap<u64, PunchFront>> = RefCell::new(HashMap::new());
}

/// Host GPU socket ends registered before the window surfman Connection exists.
/// Drained by [`bind_window_surfman`] once the GL thread can start.
static PENDING_GPU_FDS: Mutex<Vec<RawFd>> = Mutex::new(Vec::new());

/// Composite ownership key: a context is only legal on the feeder that created it.
#[derive(Hash, PartialEq, Eq, Clone, Copy, Debug)]
pub struct BrokerContextHandle {
    pub feeder_fd: RawFd,
    pub context_id: u64,
}

/// Ownership + present routing: `context_id` → creating `feeder_fd`.
/// Lookups for commands must authorize `(feeder_fd, context_id)` — never trust wire
/// `context_id` alone (cross-engine id abuse).
static CONTEXT_GPU_FD: LazyLock<Mutex<HashMap<u64, RawFd>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Feeder → host pane id (scopes experimental punch; also used for association). Empty = unknown.
static FEEDER_PANE_ID: LazyLock<Mutex<HashMap<RawFd, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Context → pane id (copied from feeder at CreateContext).
static CONTEXT_PANE_ID: LazyLock<Mutex<HashMap<u64, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Latched punch slots (constraint 2 seq gate).
static CANVAS_FRAME_SLOTS: LazyLock<Mutex<HashMap<u64, CanvasFrameSlot>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Last SHM seq observed per pane (updated by compositor each pass).
static PANE_LAST_SHM_SEQ: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// One write mutex per GPU socket (feeder replies + GL-thread presents on that fd).
/// Never acquire `CANVAS_PLANES` while holding a write lock (drop planes first).
static GPU_FD_WRITE_LOCKS: LazyLock<Mutex<HashMap<RawFd, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CANVAS_PLANES: LazyLock<Mutex<HashMap<u64, SealedMemfdPlane>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn gpu_fd_write_lock(fd: RawFd) -> Arc<Mutex<()>> {
    GPU_FD_WRITE_LOCKS
        .lock()
        .expect("poisoned")
        .entry(fd)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Authorize a wire `context_id` against the feeder that is speaking.
/// Returns `Err(())` on unknown id or cross-socket abuse — caller must drop the socket.
fn authorize_context(feeder_fd: RawFd, context_id: WebGLContextId) -> Result<BrokerContextHandle, ()> {
    let map = CONTEXT_GPU_FD.lock().expect("poisoned");
    match map.get(&context_id.0).copied() {
        Some(owner) if owner == feeder_fd => Ok(BrokerContextHandle {
            feeder_fd,
            context_id: context_id.0,
        }),
        Some(owner) => {
            error!(
                "[webgl-broker] SECURITY: cross-socket context abuse \
                 ctx={} from feeder_fd={} (owned by feeder_fd={}); dropping socket",
                context_id.0, feeder_fd, owner
            );
            Err(())
        },
        None => {
            error!(
                "[webgl-broker] SECURITY: unknown context_id={} on feeder_fd={}; dropping socket",
                context_id.0, feeder_fd
            );
            Err(())
        },
    }
}

fn authorize_contexts(feeder_fd: RawFd, ids: &[WebGLContextId]) -> Result<(), ()> {
    for id in ids {
        authorize_context(feeder_fd, *id)?;
    }
    Ok(())
}

/// Clears only state owned by one engine feeder when that socket EOF's.
struct BrokerThreadExitGuard {
    fd: RawFd,
}

impl Drop for BrokerThreadExitGuard {
    fn drop(&mut self) {
        let fd = self.fd;
        let removed: Vec<u64> = {
            let mut map = CONTEXT_GPU_FD.lock().expect("poisoned");
            let ids: Vec<u64> = map
                .iter()
                .filter(|(_, &mapped)| mapped == fd)
                .map(|(&id, _)| id)
                .collect();
            for id in &ids {
                map.remove(id);
            }
            ids
        };
        {
            let mut planes = CANVAS_PLANES.lock().expect("poisoned");
            let mut panes = CONTEXT_PANE_ID.lock().expect("poisoned");
            let mut slots = CANVAS_FRAME_SLOTS.lock().expect("poisoned");
            for id in removed {
                planes.remove(&id);
                panes.remove(&id);
                slots.remove(&id);
            }
        }
        FEEDER_PANE_ID.lock().expect("poisoned").remove(&fd);
        GPU_FD_WRITE_LOCKS.lock().expect("poisoned").remove(&fd);
    }
}

struct FdCloser(RawFd);

impl Drop for FdCloser {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

/// Bind the host window's surfman Connection/Adapter before starting the GL thread.
/// Must be called from the UI thread after `WindowRenderingContext::new`.
/// Rebinding after the GL thread has started is rejected (would strand live contexts).
///
/// Launcher may call `register_gpu_fd` *before* the winit Resumed path creates the
/// window — those fds sit in [`PENDING_GPU_FDS`] until this bind drains them.
pub fn bind_window_surfman(connection: Connection, adapter: Adapter) -> Result<(), String> {
    if GL_READY.load(Ordering::SeqCst) {
        let bound = WINDOW_SURFMAN.lock().expect("poisoned").is_some();
        if bound {
            // Already running on window surfman — idempotent OK for multi-window later.
            return Ok(());
        }
        return Err(
            "WebGL broker already started with a non-window Connection; restart required".into(),
        );
    }
    *WINDOW_SURFMAN.lock().expect("poisoned") = Some((connection, adapter));
    info!("[webgl-broker] window surfman Connection/Adapter bound");
    start_host_gl_thread()?;
    drain_pending_gpu_fds();
    Ok(())
}

fn drain_pending_gpu_fds() {
    let pending: Vec<RawFd> = std::mem::take(&mut *PENDING_GPU_FDS.lock().expect("poisoned"));
    for fd in pending {
        info!("[webgl-broker] starting deferred GPU feeder fd={}", fd);
        spawn_gpu_feeder(fd);
    }
}

fn spawn_gpu_feeder(host_fd: RawFd) {
    let jh = match std::thread::Builder::new()
        .name("lotus-gpu-broker".into())
        .spawn(move || run_gpu_feeder(host_fd))
    {
        Ok(jh) => jh,
        Err(e) => {
            error!("[webgl-broker] failed to spawn feeder for fd={host_fd}: {e}");
            unsafe {
                libc::close(host_fd);
            }
            return;
        },
    };
    BROKER_THREADS.lock().expect("poisoned").push(jh);
}

/// Shared swap chains for UI-thread SurfaceTexture bounce (Spike 2+). None until GL starts.
pub fn broker_swap_chains() -> Option<SwapChains<WebGLContextId, Device>> {
    BROKER_SWAP_CHAINS.lock().expect("poisoned").clone()
}

/// Latched punch frame for compositor (Spike 2/3). Drawn only when
/// `pane.last_shm_seq >= target_seq` (constraint 2 — no tear/flash).
#[derive(Clone, Debug)]
pub struct CanvasFrameSlot {
    pub handle: BrokerContextHandle,
    pub pane_id: String,
    /// Device-pixel rect in pane space (Spike 3: full-pane until jail sends real rects).
    pub canvas_rect_x: i32,
    pub canvas_rect_y: i32,
    pub canvas_rect_w: i32,
    pub canvas_rect_h: i32,
    pub target_seq: u64,
    pub opaque: bool,
}

/// Compositor → broker: latest consumed SHM `pub_seq` per pane (seq latch).
pub fn note_pane_shm_seq(pane_id: &str, seq: u64) {
    PANE_LAST_SHM_SEQ
        .lock()
        .expect("poisoned")
        .insert(pane_id.to_string(), seq);
}

/// Apply jail-reported canvas device-pixel bounds (Spike 3b). Keeps existing
/// `target_seq` / pane_id if the slot already exists.
fn apply_canvas_bounds(context_id: u64, x: i32, y: i32, w: i32, h: i32) {
    if w <= 0 || h <= 0 {
        return;
    }
    let feeder = CONTEXT_GPU_FD
        .lock()
        .expect("poisoned")
        .get(&context_id)
        .copied();
    let Some(feeder_fd) = feeder else {
        return;
    };
    let pane_id = CONTEXT_PANE_ID
        .lock()
        .expect("poisoned")
        .get(&context_id)
        .cloned()
        .unwrap_or_default();
    let mut slots = CANVAS_FRAME_SLOTS.lock().expect("poisoned");
    match slots.get_mut(&context_id) {
        Some(slot) => {
            slot.canvas_rect_x = x;
            slot.canvas_rect_y = y;
            slot.canvas_rect_w = w;
            slot.canvas_rect_h = h;
        },
        None => {
            slots.insert(
                context_id,
                CanvasFrameSlot {
                    handle: BrokerContextHandle {
                        feeder_fd,
                        context_id,
                    },
                    pane_id,
                    canvas_rect_x: x,
                    canvas_rect_y: y,
                    canvas_rect_w: w,
                    canvas_rect_h: h,
                    // Bounds alone must not enable punch — wait for opaque present latch.
                    target_seq: 0,
                    opaque: false,
                },
            );
        },
    }
    debug!(
        "[webgl-broker] canvas bounds ctx={} rect=({},{} {}x{})",
        context_id, x, y, w, h
    );
}

/// Mark a context as presented (call from swap path when hole-punch skips readback).
/// Latches `target_seq` to the current pane SHM seq (not +1) so punch does not stall
/// when the jail stops dirtying WR every frame.
pub fn note_context_presented(
    context_id: WebGLContextId,
    width: i32,
    height: i32,
    opaque: bool,
) {
    let feeder = CONTEXT_GPU_FD
        .lock()
        .expect("poisoned")
        .get(&context_id.0)
        .copied();
    let Some(feeder_fd) = feeder else {
        return;
    };
    let pane_id = CONTEXT_PANE_ID
        .lock()
        .expect("poisoned")
        .get(&context_id.0)
        .cloned()
        .unwrap_or_default();
    let shm_seq = if pane_id.is_empty() {
        0
    } else {
        PANE_LAST_SHM_SEQ
            .lock()
            .expect("poisoned")
            .get(&pane_id)
            .copied()
            .unwrap_or(0)
    };
    let mut slots = CANVAS_FRAME_SLOTS.lock().expect("poisoned");
    let target_seq = shm_seq;
    match slots.get_mut(&context_id.0) {
        Some(slot) => {
            slot.target_seq = target_seq;
            slot.opaque = opaque;
            if !pane_id.is_empty() {
                slot.pane_id = pane_id;
            }
            // Keep jail-reported device rect if present; else fall back to texture size.
            if slot.canvas_rect_w <= 0 || slot.canvas_rect_h <= 0 {
                slot.canvas_rect_x = 0;
                slot.canvas_rect_y = 0;
                slot.canvas_rect_w = width.max(0);
                slot.canvas_rect_h = height.max(0);
            }
        },
        None => {
            slots.insert(
                context_id.0,
                CanvasFrameSlot {
                    handle: BrokerContextHandle {
                        feeder_fd,
                        context_id: context_id.0,
                    },
                    pane_id,
                    canvas_rect_x: 0,
                    canvas_rect_y: 0,
                    canvas_rect_w: width.max(0),
                    canvas_rect_h: height.max(0),
                    target_seq,
                    opaque,
                },
            );
        },
    }
}

/// EXPERIMENTAL (`LOTUS_WEBGL_HOLE_PUNCH`): skip readback and sample Surfaces in the
/// compositor. Off by default. The reliable path is readback → memfd → jail WR → pane SHM.
/// Do not call punch APIs unless this returns true — keeps experimental work off the
/// default composite/present hot path.
pub fn experimental_hole_punch_enabled() -> bool {
    static CACHED: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("LOTUS_WEBGL_HOLE_PUNCH").is_some());
    *CACHED
}

#[deprecated(note = "use experimental_hole_punch_enabled")]
pub fn hole_punch_enabled() -> bool {
    experimental_hole_punch_enabled()
}

/// EXPERIMENTAL: bounce Surfaces for contexts owned by `pane_id` onto `draw`.
/// Seq latch: skip while `shm_seq < slot.target_seq`.
/// `draw(gl_tex, tex_w, tex_h, rect_x, rect_y, rect_w, rect_h)` — rect in pane space.
pub fn draw_hole_punch_quads_for_pane(
    rendering_context: &dyn RenderingContext,
    pane_id: &str,
    shm_seq: u64,
    mut draw: impl FnMut(u32, i32, i32, i32, i32, i32, i32),
) -> usize {
    if !experimental_hole_punch_enabled() || pane_id.is_empty() {
        return 0;
    }
    let Some(chains) = broker_swap_chains() else {
        return 0;
    };
    let ids: Vec<u64> = {
        let panes = CONTEXT_PANE_ID.lock().expect("poisoned");
        let slots = CANVAS_FRAME_SLOTS.lock().expect("poisoned");
        let feeders = CONTEXT_GPU_FD.lock().expect("poisoned");
        let feeder_panes = FEEDER_PANE_ID.lock().expect("poisoned");
        // Own this pane by CONTEXT_PANE_ID, or (pre-associate race) by feeder→pane map.
        // Never fall back to "any unbound" — that lets aux_sandbox steal sandboxed Surfaces.
        feeders
            .iter()
            .filter_map(|(&id, &fd)| {
                let owned = panes
                    .get(&id)
                    .map(|p| p == pane_id)
                    .unwrap_or_else(|| {
                        feeder_panes
                            .get(&fd)
                            .map(|p| p == pane_id)
                            .unwrap_or(false)
                    });
                if !owned {
                    return None;
                }
                let Some(slot) = slots.get(&id) else {
                    return None;
                };
                // Opaque-only: require an opaque present latch (bounds alone is not enough).
                if !slot.opaque {
                    return None;
                }
                (shm_seq >= slot.target_seq).then_some(id)
            })
            .collect()
    };
    let candidate_count = ids.len();
    let mut drawn = 0;
    for id in ids {
        let ctx = WebGLContextId(id);
        let (rx, ry, rw, rh) = {
            let slots = CANVAS_FRAME_SLOTS.lock().expect("poisoned");
            match slots.get(&id) {
                Some(s) => (s.canvas_rect_x, s.canvas_rect_y, s.canvas_rect_w, s.canvas_rect_h),
                None => (0, 0, 0, 0),
            }
        };
        let Some(chain) = chains.get(ctx) else {
            continue;
        };

        // Prefer a new front buffer; if the GL thread holds the gate (swap) or the
        // chain is empty, redraw the cached import instead of blocking / thrashing EGL.
        let new_surface = {
            match SWAP_CHAIN_GATE.try_lock() {
                Ok(_gate) => chain.take_surface(),
                Err(_) => None,
            }
        };

        if let Some(surface) = new_surface {
            let Some((surface_texture, gl_tex, size)) =
                rendering_context.create_texture(surface)
            else {
                error!(
                    "[webgl-broker] hole-punch create_texture failed ctx={}; surface dropped",
                    id
                );
                continue;
            };
            if gl_tex == 0 {
                if let Some(surface) = rendering_context.destroy_texture(surface_texture) {
                    let _gate = SWAP_CHAIN_GATE
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    chain.recycle_surface(surface);
                }
                continue;
            }
            // Recycle the previous front (if any) now that we have a replacement.
            let prev = PUNCH_FRONTS.with(|m| m.borrow_mut().remove(&id));
            if let Some(prev) = prev {
                if let Some(surface) = rendering_context.destroy_texture(prev.surface_texture) {
                    let _gate = SWAP_CHAIN_GATE
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    chain.recycle_surface(surface);
                }
            }
            let rect_w = if rw > 0 { rw } else { size.width };
            let rect_h = if rh > 0 { rh } else { size.height };
            draw(gl_tex, size.width, size.height, rx, ry, rect_w, rect_h);
            drawn += 1;
            PUNCH_FRONTS.with(|m| {
                m.borrow_mut().insert(
                    id,
                    PunchFront {
                        surface_texture,
                        gl_tex,
                        width: size.width,
                        height: size.height,
                    },
                );
            });
        } else {
            let cached = PUNCH_FRONTS.with(|m| {
                m.borrow().get(&id).map(|f| (f.gl_tex, f.width, f.height))
            });
            if let Some((gl_tex, width, height)) = cached {
                let rect_w = if rw > 0 { rw } else { width };
                let rect_h = if rh > 0 { rh } else { height };
                draw(gl_tex, width, height, rx, ry, rect_w, rect_h);
                drawn += 1;
            }
        }
    }
    {
        static LOGGED_OK: AtomicBool = AtomicBool::new(false);
        if drawn > 0 && !LOGGED_OK.swap(true, Ordering::Relaxed) {
            info!(
                "[webgl-broker] experimental hole-punch drew {drawn} quad(s) on pane '{pane_id}' \
                 shm_seq={shm_seq}"
            );
        } else if drawn > 0 {
            debug!(
                "[webgl-broker] experimental hole-punch drew {drawn} on '{pane_id}' shm_seq={shm_seq}"
            );
        } else if candidate_count > 0 {
            debug!(
                "[webgl-broker] experimental hole-punch miss pane '{pane_id}' \
                 candidates={candidate_count} shm_seq={shm_seq}"
            );
        }
    }
    drawn
}

pub fn ensure_webgl_broker() -> Result<(), String> {
    if GL_READY.load(Ordering::SeqCst) {
        return Ok(());
    }
    if WINDOW_SURFMAN.lock().expect("poisoned").is_none() {
        // Window not created yet (common: constellation spawn races Resumed).
        // register_gpu_fd will queue; bind_window_surfman starts the thread.
        debug!("[webgl-broker] ensure deferred until window surfman bind");
        return Ok(());
    }
    start_host_gl_thread()?;
    Ok(())
}

fn notify_canvas_present(context_id: WebGLContextId, canvas_epoch: Option<servo_base::Epoch>) {
    let fd = match CONTEXT_GPU_FD.lock().expect("poisoned").get(&context_id.0).copied() {
        Some(fd) => fd,
        None => return,
    };
    let hdr = GpuFrameHeader {
        body_len: 0,
        corr: canvas_epoch.map(|e| u64::from(e.0) + 1).unwrap_or(0),
        context_id: context_id.0,
        kind: KIND_CANVAS_PRESENT,
    };
    let wr_lock = gpu_fd_write_lock(fd);
    let _guard = wr_lock.lock().expect("poisoned");
    if let Err(e) = write_frame(fd, &hdr, &[]) {
        warn!("[webgl-broker] canvas present notify failed ({e})");
    }
}

fn on_host_present(
    context_id: WebGLContextId,
    size: euclid::default::Size2D<i32>,
    pixels: &[u8],
    canvas_epoch: Option<servo_base::Epoch>,
) {
    if experimental_hole_punch_enabled() && pixels.is_empty() {
        // EXPERIMENTAL: empty pixels ⇒ opaque punch (GL skipped readback).
        note_context_presented(context_id, size.width, size.height, true);
        notify_canvas_present(context_id, canvas_epoch);
        return;
    }
    let id = context_id.0;
    let mut planes = CANVAS_PLANES.lock().expect("poisoned");
    let Some(plane) = planes.get_mut(&id) else {
        return;
    };
    // Resize of GL surface without ResizeContext is rare; skip mismatched sizes.
    let expect = (size.width.max(0) as usize) * (size.height.max(0) as usize) * 4;
    if pixels.len() < expect {
        return;
    }
    unsafe {
        if let Err(e) = publish_frame(plane.rw_ptr, &pixels[..expect]) {
            warn!("[webgl-broker] canvas publish failed ({e})");
            return;
        }
    }
    drop(planes);
    notify_canvas_present(context_id, canvas_epoch);
}

fn start_host_gl_thread() -> Result<(), String> {
    if GL_READY
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(());
    }

    let (connection, adapter) = match WINDOW_SURFMAN.lock().expect("poisoned").clone() {
        Some(pair) => pair,
        None => {
            // Roll back the ready bit so a later bind + ensure can succeed.
            GL_READY.store(false, Ordering::SeqCst);
            return Err(
                "window surfman not bound; create a Lotus window before enabling the WebGL broker"
                    .into(),
            );
        },
    };

    let painter_surfman_details_map = PainterSurfmanDetailsMap::default();
    painter_surfman_details_map.insert(
        TEST_PAINTER_ID,
        PainterSurfmanDetails {
            connection,
            adapter,
        },
    );

    let default_painter_id_slot = Arc::new(ParkingMutex::new(Some(TEST_PAINTER_ID)));
    let presented_buffer_map: WebGLPresentedBufferMap =
        Arc::new(parking_lot::RwLock::new(rustc_hash::FxHashMap::default()));
    let busy_webgl_context_map = WebGLContextBusyMap::default();
    let (sender, receiver) = webgl_channel::<WebGLMsg>().ok_or("webgl_channel failed")?;
    let swap_chains = SwapChains::new();
    *BROKER_SWAP_CHAINS.lock().expect("poisoned") = Some(swap_chains.clone());

    let on_present: Arc<
        dyn Fn(WebGLContextId, euclid::default::Size2D<i32>, &[u8], Option<servo_base::Epoch>)
            + Send
            + Sync,
    > = Arc::new(|ctx, size, pixels, epoch| on_host_present(ctx, size, pixels, epoch));

    let init = WebGLThreadInit {
        paint_api: CrossProcessPaintApi::dummy(),
        external_image_id_manager: WebRenderExternalImageIdManager::default(),
        sender: sender.clone(),
        receiver,
        webrender_swap_chains: swap_chains,
        painter_surfman_details_map,
        busy_webgl_context_map,
        presented_buffer_map,
        default_painter_id_slot,
        on_command_start: None,
        on_command_end: None,
        // Reliable path: readback → on_present → memfd. Experimental punch may skip
        // readback and take SWAP_CHAIN_GATE only when LOTUS_WEBGL_HOLE_PUNCH is set.
        on_present: Some(on_present),
        swap_chain_gate: Some(Arc::clone(&SWAP_CHAIN_GATE)),
    };

    let _jh = WebGLThread::run_on_own_thread(init);
    *GL_SENDER.lock().expect("poisoned") = Some(sender);
    debug!(
        "[webgl-broker] host WebGLThread started on window surfman (painter={TEST_PAINTER_ID})"
    );
    Ok(())
}

fn clone_gl_sender() -> Result<GenericSender<WebGLMsg>, String> {
    ensure_webgl_broker()?;
    GL_SENDER
        .lock()
        .expect("poisoned")
        .as_ref()
        .cloned()
        .ok_or_else(|| "host WebGLThread sender missing".to_string())
}

/// Associate (or re-associate) a feeder with a host pane id. Used when the GPU
/// socket was registered during prewarm before the surface existed.
pub fn associate_feeder_pane(host_fd: RawFd, pane_id: &str) {
    if pane_id.is_empty() {
        return;
    }
    FEEDER_PANE_ID
        .lock()
        .expect("poisoned")
        .insert(host_fd, pane_id.to_string());
    // Retroactively tag contexts already created on this feeder.
    let owned: Vec<u64> = CONTEXT_GPU_FD
        .lock()
        .expect("poisoned")
        .iter()
        .filter(|(_, &fd)| fd == host_fd)
        .map(|(&id, _)| id)
        .collect();
    let mut panes = CONTEXT_PANE_ID.lock().expect("poisoned");
    let mut slots = CANVAS_FRAME_SLOTS.lock().expect("poisoned");
    for id in owned {
        panes.insert(id, pane_id.to_string());
        if let Some(slot) = slots.get_mut(&id) {
            slot.pane_id = pane_id.to_string();
        }
    }
    info!(
        "[webgl-broker] associated feeder fd={} → pane '{}'",
        host_fd, pane_id
    );
}

pub fn register_gpu_fd(host_fd: RawFd, pane_id: Option<String>) -> Result<(), String> {
    ensure_webgl_broker()?;

    if let Some(pid) = pane_id.filter(|s| !s.is_empty()) {
        FEEDER_PANE_ID
            .lock()
            .expect("poisoned")
            .insert(host_fd, pid);
    }

    if !GL_READY.load(Ordering::SeqCst) {
        // Window surfman not bound yet — keep the host end alive for bind drain.
        let mut pending = PENDING_GPU_FDS.lock().expect("poisoned");
        pending.push(host_fd);
        info!(
            "[webgl-broker] queued GPU fd={} until window surfman bind ({} pending)",
            host_fd,
            pending.len()
        );
        return Ok(());
    }

    // One feeder per sandboxed engine socket. Presents route via CONTEXT_GPU_FD.
    spawn_gpu_feeder(host_fd);
    Ok(())
}

fn run_gpu_feeder(fd: RawFd) {
    let _exit = BrokerThreadExitGuard { fd };
    let _closer = FdCloser(fd);

    loop {
        let hdr: GpuFrameHeader = match read_header(fd) {
            Ok(h) => h,
            Err(e) => {
                if e != "E_EOF" {
                    warn!("[webgl-broker] header read ended ({e})");
                }
                break;
            },
        };

        match hdr.routing() {
            FrameRouting::ReadBody { len } => {
                let mut buf = vec![0u8; len];
                if let Err(e) = read_exact_fd(fd, &mut buf) {
                    warn!("[webgl-broker] body read failed ({e})");
                    break;
                }
                trace!(
                    "[webgl-broker] frame corr={} ctx={} kind={} body_len={}",
                    hdr.corr,
                    hdr.context_id,
                    hdr.kind,
                    len
                );

                match hdr.kind {
                    KIND_PING => {
                        let pong = GpuFrameHeader {
                            body_len: 0,
                            corr: hdr.corr,
                            context_id: 0,
                            kind: KIND_PONG,
                        };
                        let wr = {
                            let wr_lock = gpu_fd_write_lock(fd);
                            let _guard = wr_lock.lock().expect("poisoned");
                            write_frame(fd, &pong, &[])
                        };
                        if let Err(e) = wr {
                            warn!("[webgl-broker] pong send failed ({e})");
                            break;
                        }
                        debug!("[webgl-broker] pong corr={}", hdr.corr);
                    },
                    KIND_COMMAND => {
                        let Ok(sender) = clone_gl_sender() else {
                            error!("[webgl-broker] no GL sender; feeder exiting");
                            break;
                        };
                        if handle_command(fd, &sender, &buf).is_err() {
                            error!(
                                "[webgl-broker] command rejected or failed on feeder_fd={}; \
                                 dropping GPU socket",
                                fd
                            );
                            break;
                        }
                    },
                    KIND_CANVAS_BOUNDS => {
                        // Jail → host: device-pixel canvas rect for hole-punch clip.
                        if buf.len() < 16 {
                            warn!(
                                "[webgl-broker] canvas bounds short body len={} ctx={}",
                                buf.len(),
                                hdr.context_id
                            );
                            continue;
                        }
                        if authorize_context(fd, WebGLContextId(hdr.context_id)).is_err() {
                            error!(
                                "[webgl-broker] canvas bounds auth failed on feeder_fd={}; \
                                 dropping GPU socket",
                                fd
                            );
                            break;
                        }
                        let x = i32::from_le_bytes(buf[0..4].try_into().unwrap());
                        let y = i32::from_le_bytes(buf[4..8].try_into().unwrap());
                        let w = i32::from_le_bytes(buf[8..12].try_into().unwrap());
                        let h = i32::from_le_bytes(buf[12..16].try_into().unwrap());
                        apply_canvas_bounds(hdr.context_id, x, y, w, h);
                    },
                    other => {
                        warn!("[webgl-broker] ignoring unexpected kind {other}");
                    },
                }
            },
            FrameRouting::DiscardBody { len } => {
                warn!(
                    "[webgl-broker] oversized frame body_len={} ctx={}; discarding",
                    len, hdr.context_id
                );
                if let Err(e) = discard_exact_fd(fd, len) {
                    warn!("[webgl-broker] discard failed ({e})");
                    break;
                }
            },
            FrameRouting::CloseConnection => {
                error!(
                    "[webgl-broker] absurd frame length (ctx={}); closing host fd",
                    hdr.context_id
                );
                break;
            },
        }
    }
}

fn write_reply_bytes(fd: RawFd, corr: u64, body: &[u8]) -> Result<(), ()> {
    let hdr = GpuFrameHeader {
        body_len: body.len() as u32,
        corr,
        context_id: 0,
        kind: KIND_REPLY,
    };
    let wr_lock = gpu_fd_write_lock(fd);
    let _guard = wr_lock.lock().expect("poisoned");
    write_frame(fd, &hdr, body).map_err(|e| {
        warn!("[webgl-broker] reply write failed: {e}");
    })
}

fn write_reply(fd: RawFd, corr: u64, reply: &WireReply) -> Result<(), ()> {
    let body = rmp_serde::to_vec(reply).map_err(|e| {
        warn!("[webgl-broker] reply encode failed: {e}");
    })?;
    write_reply_bytes(fd, corr, &body)
}

fn write_reply_with_canvas_fd(
    fd: RawFd,
    corr: u64,
    reply: &WireReply,
    canvas_ro: RawFd,
) -> Result<(), ()> {
    let body = rmp_serde::to_vec(reply).map_err(|e| {
        warn!("[webgl-broker] reply encode failed: {e}");
    })?;
    let hdr = GpuFrameHeader {
        body_len: body.len() as u32,
        corr,
        context_id: 0,
        kind: KIND_REPLY,
    };
    let wr_lock = gpu_fd_write_lock(fd);
    let _guard = wr_lock.lock().expect("poisoned");
    write_frame_with_fd(fd, &hdr, &body, canvas_ro).map_err(|e| {
        warn!("[webgl-broker] reply+fd write failed: {e}");
    })
}

fn install_canvas_plane(context_id: WebGLContextId, width: u32, height: u32) -> Option<RawFd> {
    let name = format!("lotus-webgl-{}", context_id.0);
    match create_canvas_plane(&name, width, height) {
        Ok(plane) => {
            let ro = plane.ro_fd;
            // Dup RO for the reply; SealedMemfdPlane keeps its own ro_fd.
            let pass = unsafe { libc::fcntl(ro, libc::F_DUPFD_CLOEXEC, 0) };
            if pass < 0 {
                warn!("[webgl-broker] dup canvas ro fd failed");
                return None;
            }
            CANVAS_PLANES
                .lock()
                .expect("poisoned")
                .insert(context_id.0, plane);
            Some(pass)
        },
        Err(e) => {
            warn!("[webgl-broker] create_canvas_plane failed ({e})");
            None
        },
    }
}

fn handle_command(
    fd: RawFd,
    sender: &GenericSender<WebGLMsg>,
    body: &[u8],
) -> Result<(), ()> {
    let wire: WireWebGLMsg = rmp_serde::from_slice(body).map_err(|e| {
        warn!("[webgl-broker] command decode failed: {e}");
    })?;

    match wire {
        WireWebGLMsg::CreateContext {
            painter_id,
            version,
            size,
            attrs,
            corr,
        } => {
            let (tx, rx) = webgl_channel().ok_or(())?;
            let msg = WebGLMsg::CreateContext(painter_id, version, size, attrs, tx);
            sender.send(msg).map_err(|_| ())?;
            let result = rx.recv().map_err(|_| ())?;
            match result {
                Ok(WebGLCreateContextResult {
                    sender: msg_sender,
                    limits,
                    glsl_version,
                    api_type,
                }) => {
                    let context_id = msg_sender.context_id();
                    let _ = msg_sender;
                    CONTEXT_GPU_FD
                        .lock()
                        .expect("poisoned")
                        .insert(context_id.0, fd);
                    if let Some(pane) = FEEDER_PANE_ID.lock().expect("poisoned").get(&fd).cloned() {
                        CONTEXT_PANE_ID
                            .lock()
                            .expect("poisoned")
                            .insert(context_id.0, pane);
                    }
                    let reply = WireReply::CreateContext(Ok(WireCreateContextOk {
                        context_id,
                        limits,
                        glsl_version,
                        api_type,
                    }));
                    if let Some(ro) = install_canvas_plane(context_id, size.width, size.height) {
                        let r = write_reply_with_canvas_fd(fd, corr, &reply, ro);
                        unsafe {
                            libc::close(ro);
                        }
                        r
                    } else {
                        // Refuse context without a sealed plane (plan: no silent degrade).
                        CONTEXT_GPU_FD.lock().expect("poisoned").remove(&context_id.0);
                        CONTEXT_PANE_ID.lock().expect("poisoned").remove(&context_id.0);
                        CANVAS_FRAME_SLOTS.lock().expect("poisoned").remove(&context_id.0);
                        let fail = WireReply::CreateContext(Err(
                            "canvas memfd seal/alloc failed".into(),
                        ));
                        let _ = sender.send(WebGLMsg::RemoveContext(context_id));
                        write_reply(fd, corr, &fail)
                    }
                },
                Err(e) => write_reply(fd, corr, &WireReply::CreateContext(Err(e))),
            }
        },
        WireWebGLMsg::SetImageKey(id, key) => {
            authorize_context(fd, id)?;
            let _ = sender.send(WebGLMsg::SetImageKey(id, key));
            Ok(())
        },
        WireWebGLMsg::ResizeContext {
            context_id,
            size,
            corr,
        } => {
            authorize_context(fd, context_id)?;
            let (tx, rx) = webgl_channel().ok_or(())?;
            sender
                .send(WebGLMsg::ResizeContext(context_id, size, tx))
                .map_err(|_| ())?;
            let result = rx.recv().unwrap_or(Err("resize recv failed".into()));
            match result {
                Ok(()) => {
                    // Replace plane generation; keep CONTEXT_GPU_FD mapping (same feeder).
                    CANVAS_PLANES.lock().expect("poisoned").remove(&context_id.0);
                    let reply = WireReply::ResizeContext(Ok(()));
                    if let Some(ro) = install_canvas_plane(context_id, size.width, size.height) {
                        let r = write_reply_with_canvas_fd(fd, corr, &reply, ro);
                        unsafe {
                            libc::close(ro);
                        }
                        r
                    } else {
                        write_reply(
                            fd,
                            corr,
                            &WireReply::ResizeContext(Err("canvas memfd alloc failed".into())),
                        )
                    }
                },
                Err(e) => write_reply(fd, corr, &WireReply::ResizeContext(Err(e))),
            }
        },
        WireWebGLMsg::RemoveContext(id) => {
            authorize_context(fd, id)?;
            CONTEXT_GPU_FD.lock().expect("poisoned").remove(&id.0);
            CONTEXT_PANE_ID.lock().expect("poisoned").remove(&id.0);
            CANVAS_FRAME_SLOTS.lock().expect("poisoned").remove(&id.0);
            CANVAS_PLANES.lock().expect("poisoned").remove(&id.0);
            let _ = sender.send(WebGLMsg::RemoveContext(id));
            Ok(())
        },
        WireWebGLMsg::WebGLCommand(id, wire_cmd, bt) => {
            authorize_context(fd, id)?;
            let (cmd, wait) = rehydrate_webgl_command(wire_cmd);
            sender
                .send(WebGLMsg::WebGLCommand(id, cmd, bt))
                .map_err(|_| ())?;
            if let Some((corr, waiter)) = wait {
                let bytes = waiter().unwrap_or_default();
                write_reply_bytes(fd, corr, &bytes)
            } else {
                Ok(())
            }
        },
        WireWebGLMsg::SwapBuffers(ids, epoch, t) => {
            authorize_contexts(fd, &ids)?;
            let _ = sender.send(WebGLMsg::SwapBuffers(ids, epoch, t));
            Ok(())
        },
        WireWebGLMsg::FinishedRenderingToContext(id) => {
            authorize_context(fd, id)?;
            let _ = sender.send(WebGLMsg::FinishedRenderingToContext(id));
            Ok(())
        },
        WireWebGLMsg::ClearPainterResources { painter_id: _, corr } => {
            // All sandboxed feeders share one WebGLThread + TEST_PAINTER_ID device.
            // Forwarding this would let one engine tear down GPU state for every
            // other. Reply OK without mutating host GL (contexts stay owned until
            // RemoveContext / feeder EOF).
            warn!(
                "[webgl-broker] ClearPainterResources ignored on feeder_fd={} \
                 (shared painter; refuse cross-engine teardown)",
                fd
            );
            write_reply(fd, corr, &WireReply::ClearPainterResources)
        },
        WireWebGLMsg::Exit => {
            // Shared host GL thread must outlive other engine feeders — only
            // tear down this feeder's socket loop, not the process-wide WebGLThread.
            Err(())
        },
    }
}
