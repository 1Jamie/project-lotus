//! Host-side WebGL GPU socket feeder + process-wide GL thread + canvas memfd planes.

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;

use log::{debug, error, trace, warn};
use lotus_supervisor::canvas_plane::{create_canvas_plane, publish_frame};
use lotus_supervisor::gpu_frame::{
    discard_exact_fd, read_exact_fd, read_header, write_frame, write_frame_with_fd, FrameRouting,
    GpuFrameHeader, KIND_CANVAS_PRESENT, KIND_COMMAND, KIND_PING, KIND_PONG, KIND_REPLY,
};
use lotus_supervisor::shm::SealedMemfdPlane;
use paint_api::{
    CrossProcessPaintApi, PainterSurfmanDetails, PainterSurfmanDetailsMap,
    WebRenderExternalImageIdManager,
};
use parking_lot::Mutex as ParkingMutex;
use servo_base::generic_channel::GenericSender;
use servo_base::id::TEST_PAINTER_ID;
use servo_canvas_traits::webgl::{webgl_channel, WebGLContextId, WebGLCreateContextResult, WebGLMsg};
use surfman::Connection;
use webgl::broker_msg::{WireCreateContextOk, WireReply, WireWebGLMsg};
use webgl::broker_wire::rehydrate_webgl_command;
use webgl::webgl_presented_frames::WebGLPresentedBufferMap;
use webgl::webgl_thread::{WebGLContextBusyMap, WebGLThread, WebGLThreadInit};

static BROKER_STARTED: AtomicBool = AtomicBool::new(false);
static BROKER_THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

static GL_READY: AtomicBool = AtomicBool::new(false);
static GL_SENDER: Mutex<Option<GenericSender<WebGLMsg>>> = Mutex::new(None);

/// Host end of the current engine GPU socket (for GL-thread present notifies).
static GPU_OUT_FD: Mutex<Option<RawFd>> = Mutex::new(None);
/// Serialize all writes on the GPU socket (feeder replies + GL-thread presents).
static GPU_WRITE_LOCK: Mutex<()> = Mutex::new(());
static CANVAS_PLANES: LazyLock<Mutex<HashMap<u64, SealedMemfdPlane>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct BrokerThreadExitGuard;

impl Drop for BrokerThreadExitGuard {
    fn drop(&mut self) {
        BROKER_STARTED.store(false, Ordering::SeqCst);
        *GPU_OUT_FD.lock().expect("poisoned") = None;
        CANVAS_PLANES.lock().expect("poisoned").clear();
        if let Ok(mut slot) = BROKER_THREAD.lock() {
            *slot = None;
        }
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

pub fn ensure_webgl_broker() -> Result<(), String> {
    if GL_READY.load(Ordering::SeqCst) {
        return Ok(());
    }
    start_host_gl_thread()?;
    Ok(())
}

fn notify_canvas_present(context_id: WebGLContextId, canvas_epoch: Option<servo_base::Epoch>) {
    let fd = match *GPU_OUT_FD.lock().expect("poisoned") {
        Some(fd) => fd,
        None => return,
    };
    let hdr = GpuFrameHeader {
        body_len: 0,
        corr: canvas_epoch.map(|e| u64::from(e.0) + 1).unwrap_or(0),
        context_id: context_id.0,
        kind: KIND_CANVAS_PRESENT,
    };
    let _guard = GPU_WRITE_LOCK.lock().expect("poisoned");
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

    let connection = Connection::new().map_err(|e| format!("surfman Connection::new: {e:?}"))?;
    let adapter = connection
        .create_adapter()
        .map_err(|e| format!("create_adapter: {e:?}"))?;

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
        webrender_swap_chains: surfman::chains::SwapChains::new(),
        painter_surfman_details_map,
        busy_webgl_context_map,
        presented_buffer_map,
        default_painter_id_slot,
        on_command_start: None,
        on_command_end: None,
        on_present: Some(on_present),
    };

    let _jh = WebGLThread::run_on_own_thread(init);
    *GL_SENDER.lock().expect("poisoned") = Some(sender);
    debug!("[webgl-broker] host WebGLThread started (painter={TEST_PAINTER_ID})");
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

pub fn register_gpu_fd(host_fd: RawFd) -> Result<(), String> {
    ensure_webgl_broker()?;

    if BROKER_STARTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("WebGL GPU broker already registered for this process".to_string());
    }

    *GPU_OUT_FD.lock().expect("poisoned") = Some(host_fd);

    let jh = std::thread::Builder::new()
        .name("lotus-gpu-broker".into())
        .spawn(move || run_gpu_feeder(host_fd))
        .map_err(|e| {
            BROKER_STARTED.store(false, Ordering::SeqCst);
            *GPU_OUT_FD.lock().expect("poisoned") = None;
            e.to_string()
        })?;

    *BROKER_THREAD.lock().expect("poisoned") = Some(jh);
    Ok(())
}

fn run_gpu_feeder(fd: RawFd) {
    let _exit = BrokerThreadExitGuard;
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
                            let _guard = GPU_WRITE_LOCK.lock().expect("poisoned");
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
                            error!("[webgl-broker] command handling failed; feeder exiting");
                            break;
                        }
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
    let _guard = GPU_WRITE_LOCK.lock().expect("poisoned");
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
    let _guard = GPU_WRITE_LOCK.lock().expect("poisoned");
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
            let _ = sender.send(WebGLMsg::SetImageKey(id, key));
            Ok(())
        },
        WireWebGLMsg::ResizeContext {
            context_id,
            size,
            corr,
        } => {
            let (tx, rx) = webgl_channel().ok_or(())?;
            sender
                .send(WebGLMsg::ResizeContext(context_id, size, tx))
                .map_err(|_| ())?;
            let result = rx.recv().unwrap_or(Err("resize recv failed".into()));
            match result {
                Ok(()) => {
                    // Replace plane generation.
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
            CANVAS_PLANES.lock().expect("poisoned").remove(&id.0);
            let _ = sender.send(WebGLMsg::RemoveContext(id));
            Ok(())
        },
        WireWebGLMsg::WebGLCommand(id, wire_cmd, bt) => {
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
            let _ = sender.send(WebGLMsg::SwapBuffers(ids, epoch, t));
            Ok(())
        },
        WireWebGLMsg::FinishedRenderingToContext(id) => {
            let _ = sender.send(WebGLMsg::FinishedRenderingToContext(id));
            Ok(())
        },
        WireWebGLMsg::ClearPainterResources { painter_id, corr } => {
            let (tx, rx) = webgl_channel().ok_or(())?;
            sender
                .send(WebGLMsg::ClearPainterResources(painter_id, tx))
                .map_err(|_| ())?;
            let _ = rx.recv();
            write_reply(fd, corr, &WireReply::ClearPainterResources)
        },
        WireWebGLMsg::Exit => {
            let _ = sender.send(WebGLMsg::Exit);
            Err(())
        },
    }
}
