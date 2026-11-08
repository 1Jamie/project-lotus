//! packages/lotus-engine/src/main.rs
//! Standalone Headless Servo Engine Process for Secure Panes.
//!
//! Executes inside a sandboxed Linux container (Stage A namespaces + Stage B seccomp).
//! Communicates with the Host process (Node.js / lotus-core) via:
//! - Control socket / pipe (LOTUS_CONTROL_FD): bidirectional JSON-RPC control & input events.
//! - GPU broker socket (LOTUS_GPU_FD, optional): `SOCK_STREAM` Unix socket for the host WebGL broker (fd 5 when passed from the supervisor).
//! - Liveness pipe (LOTUS_LIVENESS_FD): parent-death detection via POLLHUP.
//! - Shared Memory (RemotePaneShm): zero-copy triple-buffered RGBA8 frame publication.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use euclid::Point2D;
use servo::resources::{self, Resource};
use gleam::gl::Gl;
use glow::Context as GlowContext;
use paint_api::rendering_context::{
    Error as RenderingContextError, RenderingContext, SoftwareRenderingContext,
};
use servo::{
    Code, ConsoleLogLevel, DeviceIntPoint, DeviceIntRect, DeviceIntSize, EventLoopWaker, InputEvent as ServoInputEvent,
    Key, KeyboardEvent as ServoKeyboardEvent, KeyState, LoadStatus, Location, Modifiers,
    MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    NamedKey, RefreshDriver, RgbaImage, ServoBuilder, UserContentManager, UserScript, WebViewBuilder,
    WebViewDelegate, WheelDelta, WheelEvent, WheelMode,
};
use surfman::{Connection, Surface, SurfaceTexture};
use url::Url;

use lotus_supervisor::shm::{
    calculate_shm_geometry, publish_slot, reserve_slot_for_writing, ConsumerAckHeader,
    ProducerControlHeader, ShmGeometry, STATE_FREE,
};
use lotus_supervisor::stage_b::apply_stage_b_seccomp;

mod protocol;
use protocol::{ControlMessage, EngineEvent, InputEvent};

use std::io::Write;

/// Host-authoritative display pace for sandboxed Servo.
///
/// Without this, paint falls back to Servo's `TimerRefreshDriver` (hardcoded 120 Hz).
/// The host sends `SetDisplayTiming` (monitor millihertz) and `Vsync` ticks on each
/// compositor frame; we fire pending `observe_next_frame` callbacks on those ticks.
struct HostRefreshDriver {
    pending: Mutex<Vec<Box<dyn Fn() + Send + 'static>>>,
    millihertz: std::sync::atomic::AtomicU32,
    waiting_since: Mutex<Option<Instant>>,
}

impl HostRefreshDriver {
    fn new(default_millihertz: u32) -> Rc<Self> {
        Rc::new(Self {
            pending: Mutex::new(Vec::new()),
            millihertz: std::sync::atomic::AtomicU32::new(default_millihertz.max(1)),
            waiting_since: Mutex::new(None),
        })
    }

    fn set_millihertz(&self, millihertz: u32) {
        // 0 = graphics paused (host minimized/occluded). Do not coerce to 1.
        self.millihertz.store(millihertz, Ordering::Release);
    }

    fn millihertz(&self) -> u32 {
        self.millihertz.load(Ordering::Acquire)
    }

    fn graphics_paused(&self) -> bool {
        self.millihertz() == 0
    }

    fn frame_period(&self) -> Duration {
        let mhz = u64::from(self.millihertz());
        if mhz == 0 {
            // Timer/IPC pump only while graphics are paused.
            return Duration::from_millis(250);
        }
        Duration::from_nanos(1_000_000_000_000u64 / mhz)
    }

    fn fire_pending(&self) {
        if self.graphics_paused() {
            return;
        }
        let cbs = {
            let mut pending = self.pending.lock().unwrap();
            std::mem::take(&mut *pending)
        };
        *self.waiting_since.lock().unwrap() = None;
        for cb in cbs {
            cb();
        }
    }

    /// Host compositor frame tick.
    fn vsync(&self, _presentation_time_ns: u64) {
        self.fire_pending();
    }

    /// If the host stops sending Vsync (background / unfocused), advance at the
    /// host-configured rate after one period so rAF tracks SetDisplayTiming.
    /// No-op while graphics are paused (0 mHz).
    fn poll_fallback(&self) {
        if self.graphics_paused() {
            return;
        }
        let waiting = *self.waiting_since.lock().unwrap();
        let Some(since) = waiting else { return };
        if since.elapsed() >= self.frame_period() {
            self.fire_pending();
        }
    }

    fn is_waiting_for_frame(&self) -> bool {
        self.waiting_since
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
            || !self
                .pending
                .lock()
                .map(|g| g.is_empty())
                .unwrap_or(true)
    }
}

impl RefreshDriver for HostRefreshDriver {
    fn observe_next_frame(&self, start_frame_callback: Box<dyn Fn() + Send + 'static>) {
        let mut pending = self.pending.lock().unwrap();
        if pending.is_empty() {
            *self.waiting_since.lock().unwrap() = Some(Instant::now());
        }
        pending.push(start_frame_callback);
    }
}

/// Page compositing still uses the jail's [`SoftwareRenderingContext`] (software WebRender path).
/// When [`LOTUS_GPU_FD`] is set, WebGL is brokered on the host; reporting `connection() == None`
/// prevents Servo from registering a surfman GPU adapter for this pane inside the sandbox.
/// Always exposes [`HostRefreshDriver`] so Painter does not fall back to the 120 Hz timer.
struct SandboxSoftwareRenderingContext {
    inner: Rc<SoftwareRenderingContext>,
    refresh_driver: Rc<HostRefreshDriver>,
    /// When true (GPU broker), hide the jail GL connection from Servo.
    hide_connection: bool,
}

impl RenderingContext for SandboxSoftwareRenderingContext {
    fn prepare_for_rendering(&self) {
        self.inner.prepare_for_rendering();
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        self.inner.read_to_image(source_rectangle)
    }

    fn size(&self) -> PhysicalSize<u32> {
        self.inner.size()
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        self.inner.resize(size);
    }

    fn present(&self) {
        self.inner.present();
    }

    fn make_current(&self) -> Result<(), RenderingContextError> {
        self.inner.make_current()
    }

    fn gleam_gl_api(&self) -> Rc<dyn Gl> {
        self.inner.gleam_gl_api()
    }

    fn glow_gl_api(&self) -> Arc<GlowContext> {
        self.inner.glow_gl_api()
    }

    fn create_texture(
        &self,
        surface: Surface,
    ) -> Option<(SurfaceTexture, u32, euclid::default::Size2D<i32>)> {
        self.inner.create_texture(surface)
    }

    fn destroy_texture(&self, surface_texture: SurfaceTexture) -> Option<Surface> {
        self.inner.destroy_texture(surface_texture)
    }

    fn connection(&self) -> Option<Connection> {
        if self.hide_connection {
            None
        } else {
            self.inner.connection()
        }
    }

    fn refresh_driver(&self) -> Option<Rc<dyn RefreshDriver>> {
        Some(self.refresh_driver.clone())
    }
}

fn is_verbose() -> bool {
    static VERBOSE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VERBOSE.get_or_init(|| {
        std::env::var("LOTUS_ENGINE_DEBUG").map(|v| v == "1" || v == "true").unwrap_or(false)
    })
}

macro_rules! log_debug {
    ($($arg:tt)*) => {{
        if is_verbose() {
            eprintln!($($arg)*);
            let _ = std::io::stderr().flush();
        }
    }};
}

macro_rules! log_engine {
    ($($arg:tt)*) => {{
        eprintln!($($arg)*);
        let _ = std::io::stderr().flush();
    }};
}

// =========================================================================
// Resource Reader for Embedded Servo Assets
// =========================================================================

struct EngineResourceReader;

impl resources::ResourceReaderMethods for EngineResourceReader {
    fn read(&self, file: Resource) -> Vec<u8> {
        if let Ok(res_path) = env::var("LOTUS_RESOURCES_PATH") {
            let path = PathBuf::from(&res_path).join(file.filename());
            if let Ok(data) = fs::read(&path) {
                return data;
            }
        }

        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let res_dir = manifest_dir.join("../lotus-core/resources");
        let path = res_dir.join(file.filename());
        if let Ok(data) = fs::read(&path) {
            return data;
        }

        if let Ok(exe) = env::current_exe() {
            if let Some(parent) = exe.parent() {
                let candidates = [
                    parent.join("resources").join(file.filename()),
                    parent.join("../lotus-core/resources").join(file.filename()),
                    parent.join("../../packages/lotus-core/resources").join(file.filename()),
                ];
                for cand in &candidates {
                    if let Ok(data) = fs::read(cand) {
                        return data;
                    }
                }
            }
        }

        eprintln!("[lotus-engine] Warning: Could not locate resource {:?}", file.filename());
        Vec::new()
    }

    fn sandbox_access_files_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Ok(res_path) = env::var("LOTUS_RESOURCES_PATH") {
            dirs.push(PathBuf::from(res_path));
        }
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        dirs.push(manifest_dir.join("../lotus-core/resources"));
        dirs
    }

    fn sandbox_access_files(&self) -> Vec<PathBuf> {
        vec![]
    }
}

static RESOURCE_READER: EngineResourceReader = EngineResourceReader;
servo::submit_resource_reader!(&RESOURCE_READER);

// =========================================================================
// Servo Event Loop Waker & Delegate
// =========================================================================

#[derive(Clone)]
struct EngineWaker(mpsc::Sender<()>);

impl EventLoopWaker for EngineWaker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(EngineWaker(self.0.clone()))
    }
    fn wake(&self) {
        let _ = self.0.send(());
    }
}

struct EngineDelegate {
    surface_id: String,
    load_complete: Arc<AtomicBool>,
    frame_ready: Arc<AtomicBool>,
    last_url: Arc<Mutex<Option<String>>>,
    last_title: Arc<Mutex<Option<String>>>,
}

impl WebViewDelegate for EngineDelegate {
    fn notify_load_status_changed(&self, webview: servo::WebView, status: LoadStatus) {
        log_debug!("[lotus-engine] notify_load_status_changed: status={:?}, url={:?}", status, webview.url());
        if status == LoadStatus::Complete {
            if let Some(u) = webview.url() {
                *self.last_url.lock().unwrap() = Some(u.to_string());
            }
            self.load_complete.store(true, Ordering::SeqCst);
            self.frame_ready.store(true, Ordering::SeqCst);
        }
    }

    fn notify_new_frame_ready(&self, _webview: servo::WebView) {
        log_debug!("[lotus-engine] notify_new_frame_ready for {}", self.surface_id);
        self.frame_ready.store(true, Ordering::SeqCst);
    }

    fn show_console_message(&self, _webview: servo::WebView, level: ConsoleLogLevel, message: String) {
        if is_verbose() || matches!(level, ConsoleLogLevel::Error | ConsoleLogLevel::Warn) {
            log_engine!("[lotus-engine] Console [{:?}]: {}", level, message);
        }
    }

    fn notify_crashed(&self, _webview: servo::WebView, reason: String, backtrace: Option<String>) {
        log_engine!("[lotus-engine] CRASH in webview: {} {:?}", reason, backtrace);
    }

    fn notify_page_title_changed(&self, _webview: servo::WebView, title: Option<String>) {
        if let Some(t) = title {
            *self.last_title.lock().unwrap() = Some(t);
        }
    }
}

struct SurfaceState {
    surface_id: String,
    width: u32,
    height: u32,
    geom: ShmGeometry,
    rw_ptr: *mut u8,
    shm_size: usize,
    consumer_ack_ptr: *const ConsumerAckHeader,
    rendering_ctx: Rc<SoftwareRenderingContext>,
    webview: servo::WebView,
    last_pub_gen: [u64; 3],
    last_pub_seq: [u64; 3],
    current_slot: usize,
    frame_ready: Arc<AtomicBool>,
    load_complete: Arc<AtomicBool>,
    last_url: Arc<Mutex<Option<String>>>,
    last_title: Arc<Mutex<Option<String>>>,
    paint_needed: bool,
}

impl Drop for SurfaceState {
    fn drop(&mut self) {
        if !self.rw_ptr.is_null() && self.shm_size > 0 {
            unsafe {
                libc::munmap(self.rw_ptr as *mut libc::c_void, self.shm_size);
            }
        }
        if !self.consumer_ack_ptr.is_null() {
            unsafe {
                libc::munmap(
                    self.consumer_ack_ptr as *mut libc::c_void,
                    std::mem::size_of::<ConsumerAckHeader>(),
                );
            }
        }
    }
}

fn create_surface(
    surface_id: String,
    width: u32,
    height: u32,
    max_width: Option<u32>,
    max_height: Option<u32>,
    scale_factor: Option<f32>,
    shm_fd: RawFd,
    ack_fd: RawFd,
    initial_url: Option<String>,
    bridge_script: Option<String>,
    host_gpu_broker: bool,
    refresh_driver: Rc<HostRefreshDriver>,
    servo: &servo::Servo,
) -> Result<SurfaceState, String> {
    log_debug!("[lotus-engine] create_surface entered: sid={}, width={}, height={}, max_w={:?}, max_h={:?}, shm_fd={}, ack_fd={}", surface_id, width, height, max_width, max_height, shm_fd, ack_fd);
    let mut geom_w = max_width.unwrap_or(width).max(width);
    let mut geom_h = max_height.unwrap_or(height).max(height);
    let mut geom = calculate_shm_geometry(geom_w, geom_h).map_err(|e| format!("{:?}", e))?;

    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(shm_fd, &mut stat) } == 0 && stat.st_size > 0 {
        let actual_size = stat.st_size as usize;
        if 256 + geom.total_pixel_bytes > actual_size {
            geom_w = width;
            geom_h = height;
            geom = calculate_shm_geometry(geom_w, geom_h).map_err(|e| format!("{:?}", e))?;
        }
    }
    let total_shm_size = 256 + geom.total_pixel_bytes;

    let rw_ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            total_shm_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            shm_fd,
            0,
        ) as *mut u8
    };
    if rw_ptr == libc::MAP_FAILED as *mut u8 {
        let err = unsafe { *libc::__errno_location() };
        unsafe {
            libc::close(shm_fd);
            libc::close(ack_fd);
        };
        log_engine!("[lotus-engine] mmap shm_fd failed for {}: errno {}", surface_id, err);
        return Err(format!("mmap shm_fd failed: errno {}", err));
    }

    let prod_hdr = unsafe { &mut *(rw_ptr as *mut ProducerControlHeader) };
    prod_hdr.pub_seq.store(0, Ordering::Relaxed);
    prod_hdr.prod_resize_gen.store(1, Ordering::Relaxed);
    prod_hdr.active_slots = 3;
    // Drop any READY slots left by a prior engine generation on this SHM — otherwise
    // the host consumer can re-upload the crash-stale frame after pub_seq reset.
    for slot in &prod_hdr.slots {
        slot.state.store(STATE_FREE, Ordering::Release);
        slot.pub_seq.store(0, Ordering::Release);
    }

    let consumer_ack_ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            std::mem::size_of::<ConsumerAckHeader>(),
            libc::PROT_READ,
            libc::MAP_SHARED,
            ack_fd,
            0,
        ) as *const ConsumerAckHeader
    };
    if consumer_ack_ptr == libc::MAP_FAILED as *const ConsumerAckHeader {
        let err = unsafe { *libc::__errno_location() };
        unsafe {
            libc::munmap(rw_ptr as *mut libc::c_void, total_shm_size);
            libc::close(shm_fd);
            libc::close(ack_fd);
        };
        log_engine!("[lotus-engine] mmap ack_fd failed for {}: errno {}", surface_id, err);
        return Err(format!("mmap ack_fd failed: errno {}", err));
    }

    // Close descriptor numbers after mmap succeeds; kernel retains mapping references (I7 fix)
    unsafe {
        libc::close(shm_fd);
        libc::close(ack_fd);
    }

    let load_complete = Arc::new(AtomicBool::new(false));
    let frame_ready = Arc::new(AtomicBool::new(false));
    let last_url = Arc::new(Mutex::new(None));
    let last_title = Arc::new(Mutex::new(None));

    let delegate = EngineDelegate {
        surface_id: surface_id.clone(),
        load_complete: load_complete.clone(),
        frame_ready: frame_ready.clone(),
        last_url: last_url.clone(),
        last_title: last_title.clone(),
    };

    let rendering_ctx = Rc::new(
        SoftwareRenderingContext::new(PhysicalSize::new(width, height))
            .map_err(|e| format!("SoftwareRenderingContext::new ({}x{}) failed: {:?}", width, height, e))?,
    );

    // Always wrap so Painter uses HostRefreshDriver instead of the 120 Hz timer fallback.
    let for_webview: Rc<dyn RenderingContext> = Rc::new(SandboxSoftwareRenderingContext {
        inner: rendering_ctx.clone(),
        refresh_driver,
        hide_connection: host_gpu_broker,
    });

    let sf = scale_factor.unwrap_or(1.0);
    let mut builder = WebViewBuilder::new(servo, for_webview)
        .hidpi_scale_factor(euclid::Scale::new(sf))
        .delegate(Rc::new(delegate));

    if let Some(ref script) = bridge_script {
        let ucm = UserContentManager::new(servo);
        ucm.add_script(Rc::new(UserScript::from(script.as_str())));
        builder = builder.user_content_manager(Rc::new(ucm));
    }

    if let Some(ref u_str) = initial_url {
        match Url::parse(u_str) {
            Ok(u) => {
                log_debug!("[lotus-engine] Initializing WebView with URL {}", u);
                builder = builder.url(u);
            }
            Err(e) => {
                log_engine!("[lotus-engine] Url::parse failed for {:?}: {}", u_str, e);
            }
        }
    }

    rendering_ctx.resize(PhysicalSize::new(width, height));
    log_debug!("[lotus-engine] calling builder.build() for {}", surface_id);
    let webview = builder.build();
    log_debug!("[lotus-engine] webview built ok for {}", surface_id);
    webview.resize(PhysicalSize::new(width, height));

    Ok(SurfaceState {
        surface_id,
        width,
        height,
        geom,
        rw_ptr,
        shm_size: total_shm_size,
        consumer_ack_ptr,
        rendering_ctx,
        webview,
        last_pub_gen: [0u64; 3],
        last_pub_seq: [0u64; 3],
        current_slot: 0,
        frame_ready,
        load_complete,
        last_url,
        last_title,
        paint_needed: true,
    })
}

fn recv_control_msg_with_fds(
    fd: RawFd,
    buf: &mut Vec<u8>,
) -> Result<(Vec<String>, Vec<RawFd>), io::Error> {
    let mut received_fds = Vec::new();
    let mut chunk = [0u8; 8192];

    loop {
        let mut iov = libc::iovec {
            iov_base: chunk.as_mut_ptr() as *mut libc::c_void,
            iov_len: chunk.len(),
        };

        let cmsg_space = unsafe { libc::CMSG_SPACE((std::mem::size_of::<RawFd>() * 4) as u32) as usize };
        let mut cmsg_buf = vec![0u8; cmsg_space];

        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = cmsg_space;

        let r = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_DONTWAIT) };
        if r < 0 {
            let err = unsafe { *libc::__errno_location() };
            if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
                break;
            }
            return Err(io::Error::from_raw_os_error(err));
        }
        if r == 0 {
            if buf.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "control_fd EOF"));
            }
            break;
        }

        buf.extend_from_slice(&chunk[..r as usize]);

        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let data_ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
                    let data_len = (*cmsg).cmsg_len - libc::CMSG_LEN(0) as usize;
                    let fd_count = data_len / std::mem::size_of::<RawFd>();
                    for i in 0..fd_count {
                        received_fds.push(*data_ptr.add(i));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }

        if (r as usize) < chunk.len() {
            break;
        }
    }

    let mut lines = Vec::new();
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
        let s = String::from_utf8_lossy(&line_bytes[..line_bytes.len().saturating_sub(1)]).to_string();
        if !s.trim().is_empty() {
            lines.push(s);
        }
    }

    Ok((lines, received_fds))
}

// =========================================================================
// Engine Config & Argument Parsing
// =========================================================================

struct EngineConfig {
    control_fd: Option<RawFd>,
    gpu_fd: Option<RawFd>,
    liveness_fd: Option<RawFd>,
    listener_fd: Option<RawFd>,
    broker_socket: Option<String>,
    shm_fd: Option<RawFd>,
    ack_fd: Option<RawFd>,
    width: u32,
    height: u32,
    surface_id: String,
    initial_url: String,
    bridge_script: Option<String>,
}

fn parse_config() -> EngineConfig {
    let mut control_fd: Option<RawFd> = env::var("LOTUS_CONTROL_FD").ok().and_then(|v| v.parse().ok());
    let mut gpu_fd: Option<RawFd> = env::var("LOTUS_GPU_FD").ok().and_then(|v| v.parse().ok());
    let mut liveness_fd: Option<RawFd> = env::var("LOTUS_LIVENESS_FD").ok().and_then(|v| v.parse().ok());
    let mut listener_fd: Option<RawFd> = env::var("LOTUS_IPC_LISTENER_FD").ok().and_then(|v| v.parse().ok());
    let mut broker_socket: Option<String> = env::var("LOTUS_BROKER_SOCKET").ok();
    let mut shm_fd: Option<RawFd> = env::var("LOTUS_SHM_FD").ok().and_then(|v| v.parse().ok());
    let mut ack_fd: Option<RawFd> = env::var("LOTUS_ACK_FD").ok().and_then(|v| v.parse().ok());
    let mut width = 800u32;
    let mut height = 600u32;
    let mut surface_id = "default".to_string();
    let mut initial_url = "about:blank".to_string();
    let mut bridge_script: Option<String> = None;

    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--control-fd" => {
                i += 1;
                if i < args.len() {
                    control_fd = args[i].parse().ok();
                }
            }
            "--gpu-fd" => {
                i += 1;
                if i < args.len() {
                    gpu_fd = args[i].parse().ok();
                }
            }
            "--liveness-fd" => {
                i += 1;
                if i < args.len() {
                    liveness_fd = args[i].parse().ok();
                }
            }
            "--listener-fd" => {
                i += 1;
                if i < args.len() {
                    listener_fd = args[i].parse().ok();
                }
            }
            "--broker-socket" => {
                i += 1;
                if i < args.len() {
                    broker_socket = Some(args[i].clone());
                }
            }
            "--shm-fd" => {
                i += 1;
                if i < args.len() {
                    shm_fd = args[i].parse().ok();
                }
            }
            "--ack-fd" => {
                i += 1;
                if i < args.len() {
                    ack_fd = args[i].parse().ok();
                }
            }
            "--width" => {
                i += 1;
                if i < args.len() {
                    width = args[i].parse().unwrap_or(800);
                }
            }
            "--height" => {
                i += 1;
                if i < args.len() {
                    height = args[i].parse().unwrap_or(600);
                }
            }
            "--surface-id" => {
                i += 1;
                if i < args.len() {
                    surface_id = args[i].clone();
                }
            }
            "--url" => {
                i += 1;
                if i < args.len() {
                    initial_url = args[i].clone();
                }
            }
            "--bridge-script" => {
                i += 1;
                if i < args.len() {
                    bridge_script = Some(args[i].clone());
                }
            }
            _ => {}
        }
        i += 1;
    }

    EngineConfig {
        control_fd,
        gpu_fd,
        liveness_fd,
        listener_fd,
        broker_socket,
        shm_fd,
        ack_fd,
        width,
        height,
        surface_id,
        initial_url,
        bridge_script,
    }
}

// =========================================================================
// Main Entrypoint & State Machine
// =========================================================================

fn main() {
    // Force headless software rendering without attempting X11/Wayland/DRI connection
    std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
    std::env::set_var("GALLIUM_DRIVER", "llvmpipe");
    std::env::set_var("EGL_PLATFORM", "surfaceless");
    std::env::set_var("GST_REGISTRY_FORK", "no");
    std::env::set_var("GST_REGISTRY_DISABLE", "yes");
    std::env::set_var("PULSE_SERVER", "");
    std::env::set_var("PULSE_AUTOSPAWN", "0");
    std::env::remove_var("DISPLAY");
    std::env::remove_var("WAYLAND_DISPLAY");
    std::env::remove_var("XAUTHORITY");
    if std::env::var("XDG_RUNTIME_DIR").is_err() {
        if let Ok(tmp) = std::env::var("TMPDIR") {
            std::env::set_var("XDG_RUNTIME_DIR", tmp);
        }
    }

    let config = parse_config();
    let width = config.width;
    let height = config.height;

    // 1. Line 1: Apply Stage B Seccomp-BPF Lockdown before creating any threads
    apply_stage_b_seccomp().expect("Failed to apply Stage B seccomp");

    // 2. Verify Lockdown Invariants (/proc/self/status Seccomp == 2 && NoNewPrivs == 1)
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        let mut seccomp_ok = false;
        let mut no_new_privs_ok = false;
        for line in status.lines() {
            if line.starts_with("Seccomp:") {
                let v = line.split_whitespace().nth(1).unwrap_or("");
                seccomp_ok = v == "2";
            }
            if line.starts_with("NoNewPrivs:") {
                let v = line.split_whitespace().nth(1).unwrap_or("");
                no_new_privs_ok = v == "1";
            }
        }
        if !seccomp_ok || !no_new_privs_ok {
            eprintln!("[lotus-engine] Fatal: Lockdown verification failed (seccomp: {}, no_new_privs: {})", seccomp_ok, no_new_privs_ok);
            std::process::abort();
        }
    }

    // 3. Attestation Gate Handshake over LOTUS_CONTROL_FD (Stage B locked, single-threaded)
    if let Some(ctrl_fd) = config.control_fd {
        // Send STAGE_B_LOCKED
        let locked_msg = b"STAGE_B_LOCKED\n";
        let n = unsafe { libc::write(ctrl_fd, locked_msg.as_ptr() as *const _, locked_msg.len()) };
        if n < 0 {
            eprintln!("[lotus-engine] Failed to write STAGE_B_LOCKED");
            std::process::exit(1);
        }

        // Wait for PROCEED_TO_INIT
        let mut line_buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let r = unsafe { libc::read(ctrl_fd, byte.as_mut_ptr() as *mut _, 1) };
            if r <= 0 {
                eprintln!("[lotus-engine] Failed to read PROCEED_TO_INIT");
                std::process::exit(2);
            }
            if byte[0] == b'\n' {
                let s = String::from_utf8_lossy(&line_buf);
                if s.contains("PROCEED_TO_INIT") {
                    break;
                }
                line_buf.clear();
            } else {
                line_buf.push(byte[0]);
            }
        }
    }

    // 4. Initialize Servo Singleton & Waker (Opts registered; any worker threads inherit Stage B seccomp)
    let (wake_tx, wake_rx) = mpsc::channel();
    let waker = EngineWaker(wake_tx);
    let mut prefs = servo::prefs::Preferences::default();
    prefs.shell_background_color_rgba = [0.0, 0.0, 0.0, 1.0];

    // Initialize local proxy forwarder to host network broker if listener_fd and broker_socket are present
    log_debug!("[lotus-engine] Broker config: listener_fd={:?}, broker_socket={:?}", config.listener_fd, config.broker_socket);
    if let (Some(l_fd), Some(broker_path)) = (config.listener_fd, config.broker_socket.clone()) {
        let listener = unsafe { std::net::TcpListener::from_raw_fd(l_fd) };
        log_debug!("[lotus-engine] listener.local_addr() = {:?}", listener.local_addr());
        if let Ok(addr) = listener.local_addr() {
            let bound_port = addr.port();
            let proxy_uri = format!("http://127.0.0.1:{}", bound_port);
            prefs.network_http_proxy_uri = proxy_uri.clone();
            prefs.network_https_proxy_uri = proxy_uri;

            let path = broker_path.clone();
            let active_conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            std::thread::Builder::new()
                .name("lotus-net-proxy".into())
                .spawn(move || {
                    log_debug!("[lotus-engine] lotus-net-proxy thread started, listening on {:?}", listener.local_addr());
                    for incoming in listener.incoming() {
                        log_debug!("[lotus-engine] lotus-net-proxy incoming connection: {:?}", incoming);
                        match incoming {
                            Ok(client_stream) => {
                                if active_conns.load(Ordering::Relaxed) >= 32 {
                                    eprintln!("[lotus-engine] Proxy connection limit reached (32), rejecting connection");
                                    drop(client_stream);
                                    continue;
                                }
                                active_conns.fetch_add(1, Ordering::SeqCst);
                                let conns_clone = active_conns.clone();
                                let p = path.clone();
                                std::thread::spawn(move || {
                                    struct ConnGuard(Arc<std::sync::atomic::AtomicUsize>);
                                    impl Drop for ConnGuard {
                                        fn drop(&mut self) {
                                            self.0.fetch_sub(1, Ordering::SeqCst);
                                        }
                                    }
                                    let _guard = ConnGuard(conns_clone);

                                    match std::os::unix::net::UnixStream::connect(&p) {
                                        Ok(broker_stream) => {
                                            if let (Ok(mut client_reader), Ok(mut broker_reader)) = (client_stream.try_clone(), broker_stream.try_clone()) {
                                                let mut client_writer = client_stream;
                                                let mut broker_writer = broker_stream;

                                                let t1 = std::thread::spawn(move || {
                                                    let _ = std::io::copy(&mut client_reader, &mut broker_writer);
                                                    let _ = broker_writer.shutdown(std::net::Shutdown::Write);
                                                });
                                                let _ = std::io::copy(&mut broker_reader, &mut client_writer);
                                                let _ = client_writer.shutdown(std::net::Shutdown::Write);
                                                let _ = t1.join();
                                            }
                                        }
                                        Err(e) => {
                                            eprintln!("[lotus-engine] Failed to connect to broker socket {}: {}", p, e);
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                eprintln!("[lotus-engine] Proxy listener accept error: {}", e);
                                break;
                            }
                        }
                    }
                })
                .expect("Failed to spawn proxy forwarder thread");
        }
    }

    let servo = ServoBuilder::default()
        .event_loop_waker(Box::new(waker))
        .preferences(prefs)
        .build();
    servo.setup_logging();

    let host_gpu_broker = config.gpu_fd.is_some();
    // Default 60 Hz until host sends SetDisplayTiming with the real monitor rate.
    let refresh_driver = HostRefreshDriver::new(60_000);

    // 5. Initialize Surfaces Map & Optional Initial Surface from CLI
    let mut surfaces: HashMap<String, SurfaceState> = HashMap::new();
    if let (Some(sfd), Some(afd)) = (config.shm_fd, config.ack_fd) {
        match create_surface(
            config.surface_id.clone(),
            width,
            height,
            None,
            None,
            None,
            sfd,
            afd,
            Some(config.initial_url.clone()),
            config.bridge_script.clone(),
            host_gpu_broker,
            refresh_driver.clone(),
            &servo,
        ) {
            Ok(initial_surface) => {
                surfaces.insert(config.surface_id.clone(), initial_surface);
            }
            Err(e) => {
                eprintln!("[lotus-engine] Fatal: Failed to initialize initial surface: {}", e);
                std::process::abort();
            }
        }
    }

    // 6. Attestation Gate Ready Handshake over LOTUS_CONTROL_FD
    if let Some(ctrl_fd) = config.control_fd {
        let ready_msg = b"ENGINE_READY\n";
        let _ = unsafe { libc::write(ctrl_fd, ready_msg.as_ptr() as *const _, ready_msg.len()) };

        // Set control_fd non-blocking for event loop
        unsafe {
            let flags = libc::fcntl(ctrl_fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(ctrl_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
    }

    // GPU broker handshake runs on the WebGL bridge thread (LOTUS_GPU_FD), not here.

    // 7. Steady-State Event & Multi-Surface Render Loop
    // Sandboxed Servo has no compositor vsync. Cap SHM publish (and the expensive
    // read_to_image) to the host display cadence. Override with LOTUS_FRAME_HZ.
    let target_hz: u32 = env::var("LOTUS_FRAME_HZ")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&hz| hz > 0 && hz <= 240)
        .unwrap_or(60);
    let frame_interval = Duration::from_nanos(1_000_000_000u64 / u64::from(target_hz));
    log_debug!(
        "[lotus-engine] Frame pacing: {} Hz ({:?} interval)",
        target_hz,
        frame_interval
    );

    let mut control_read_buf = Vec::new();
    let mut pending_fds = Vec::new();
    let mut running = true;

    while running {
        // Check Liveness FD
        if let Some(live_fd) = config.liveness_fd {
            let mut pfd = libc::pollfd {
                fd: live_fd,
                events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
                revents: 0,
            };
            let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ret > 0 && (pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0) {
                // Supervisor has terminated -> exit cleanly
                break;
            }
        }

        // Read Control Messages & Passed Descriptors from LOTUS_CONTROL_FD
        if let Some(ctrl_fd) = config.control_fd {
            match recv_control_msg_with_fds(ctrl_fd, &mut control_read_buf) {
                Ok((lines, fds)) => {
                    if !lines.is_empty() || !fds.is_empty() {
                        log_debug!("[lotus-engine] recv_control: lines={:?}, fds={:?}", lines, fds);
                    }
                    if control_read_buf.len() > 64 * 1024 {
                        control_read_buf.clear();
                    }
                    pending_fds.extend(fds);
                    while pending_fds.len() > 16 {
                        let excess = pending_fds.pop().unwrap();
                        unsafe { libc::close(excess); }
                    }
                    for trimmed in lines {
                        if trimmed == "SHUTDOWN" {
                            running = false;
                            break;
                        }

                        match serde_json::from_str::<ControlMessage>(&trimmed) {
                            Ok(msg) => {
                                match msg {
                                    ControlMessage::CreateSurface { surface_id, width: s_w, height: s_h, max_width, max_height, scale_factor, url, bridge_script } => {
                                        if pending_fds.len() >= 2 {
                                            let sfd = pending_fds.remove(0);
                                            let afd = pending_fds.remove(0);
                                            match create_surface(surface_id.clone(), s_w, s_h, max_width, max_height, scale_factor, sfd, afd, url, bridge_script, host_gpu_broker, refresh_driver.clone(), &servo) {
                                                Ok(surf) => {
                                                    surfaces.insert(surface_id, surf);
                                                }
                                                Err(e) => {
                                                    eprintln!("[lotus-engine] Failed to create surface {}: {}", surface_id, e);
                                                    unsafe {
                                                        libc::close(sfd);
                                                        libc::close(afd);
                                                    }
                                                }
                                            }
                                        } else {
                                            eprintln!("[lotus-engine] CreateSurface missing passed FDs for {}", surface_id);
                                        }
                                    }
                                    ControlMessage::DestroySurface { surface_id } | ControlMessage::Destroy { surface_id } => {
                                        surfaces.remove(&surface_id);
                                    }
                                    ControlMessage::Navigate { surface_id, url, .. } => {
                                        log_debug!("[lotus-engine] Navigate received: sid={}, url={}", surface_id, url);
                                        if let Some(surf) = surfaces.get_mut(&surface_id) {
                                            match Url::parse(&url) {
                                                Ok(u) => {
                                                    log_debug!("[lotus-engine] Navigate loading url: {}", u);
                                                    surf.webview.load(u);
                                                    surf.paint_needed = false;
                                                }
                                                Err(e) => {
                                                    eprintln!("[lotus-engine] Navigate Url::parse failed for {}: {}", url, e);
                                                }
                                            }
                                        } else {
                                            eprintln!("[lotus-engine] Navigate surface not found: {}", surface_id);
                                        }
                                    }
                                    ControlMessage::EvaluateScript { surface_id, script } => {
                                        log_debug!("[lotus-engine] EvaluateScript received: sid={}, len={}", surface_id, script.len());
                                        if let Some(surf) = surfaces.get(&surface_id) {
                                            surf.webview.evaluate_javascript(script, |_| {});
                                        } else {
                                            eprintln!("[lotus-engine] EvaluateScript surface not found: {}", surface_id);
                                        }
                                    }
                                    ControlMessage::Resize { surface_id, generation, width: new_w, height: new_h, scale_factor } => {
                                        if let Some(surf) = surfaces.get_mut(&surface_id) {
                                            if new_w > 0 && new_h > 0 {
                                                if (new_w as usize) * 4 <= surf.geom.stride && (new_h as usize) * surf.geom.stride <= surf.geom.slot_stride_bytes {
                                                    let prod_hdr = unsafe { &mut *(surf.rw_ptr as *mut ProducerControlHeader) };
                                                    prod_hdr.prod_resize_gen.store(generation, Ordering::Release);
                                                    if scale_factor > 0.0 {
                                                        surf.webview.set_hidpi_scale_factor(euclid::Scale::new(scale_factor));
                                                    }
                                                    surf.width = new_w;
                                                    surf.height = new_h;
                                                    // IMPORTANT: Do NOT call surf.rendering_ctx.resize() before
                                                    // surf.webview.resize()! Servo's painter resizes the
                                                    // rendering_context itself and updates layout/WebRender;
                                                    // resizing the ctx first makes Servo early-return and
                                                    // leaves the pane stuck at the old layout size.
                                                    surf.webview.resize(PhysicalSize::new(new_w, new_h));
                                                    surf.paint_needed = true;
                                                } else {
                                                    eprintln!("[lotus-engine] Resize rejected: {}x{} exceeds SHM bounds (stride={}, slot_stride={})",
                                                        new_w, new_h, surf.geom.stride, surf.geom.slot_stride_bytes);
                                                }
                                            }
                                        }
                                    }
                                    ControlMessage::Input { event } => {
                                        let sid = event.surface_id().to_string();
                                        if let Some(surf) = surfaces.get_mut(&sid) {
                                            handle_input_event(&surf.webview, event);
                                            surf.paint_needed = true;
                                        }
                                    }
                                    ControlMessage::SetDisplayTiming { refresh_rate_millihertz } => {
                                        log_debug!(
                                            "[lotus-engine] SetDisplayTiming: {} mHz",
                                            refresh_rate_millihertz
                                        );
                                        refresh_driver.set_millihertz(refresh_rate_millihertz);
                                    }
                                    ControlMessage::Vsync { surface_id: _, presentation_time_ns } => {
                                        refresh_driver.vsync(presentation_time_ns);
                                    }
                                    ControlMessage::Ping => {
                                        send_engine_event(ctrl_fd, &EngineEvent::Pong);
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("[lotus-engine] Failed to parse control message '{}': {}", trimmed, e);
                            }
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    break;
                }
                Err(e) => {
                    eprintln!("[lotus-engine] recv_control_msg error: {}", e);
                }
            }
        }

        // Advance animation frames if host Vsync stalled (unfocused), at host-set rate.
        refresh_driver.poll_fallback();

        // Spin Servo Event Loop
        servo.spin_event_loop();

        let mut any_frame_work = false;
        // Multi-Surface LoadStatus, Title, and Frame Publication
        for (surface_id, surf) in surfaces.iter_mut() {
            // 1. Emit LoadStatus if load completed
            if surf.load_complete.swap(false, Ordering::SeqCst) {
                if let Some(ctrl_fd) = config.control_fd {
                    let u = surf.last_url.lock().unwrap().clone();
                    send_engine_event(
                        ctrl_fd,
                        &EngineEvent::LoadStatus {
                            surface_id: surface_id.clone(),
                            status: "complete".to_string(),
                            url: u,
                        },
                    );
                }
            }

            // 2. Emit TitleChanged if page title changed
            if let Some(title) = surf.last_title.lock().unwrap().take() {
                if let Some(ctrl_fd) = config.control_fd {
                    send_engine_event(
                        ctrl_fd,
                        &EngineEvent::TitleChanged {
                            surface_id: surface_id.clone(),
                            title,
                        },
                    );
                }
            }

            // 3. Paint and Publish Frame if ready
            // While host-paused (minimized/occluded), skip animation paints entirely.
            // Resize still forces a paint via paint_needed.
            if refresh_driver.graphics_paused() && !surf.paint_needed {
                continue;
            }
            let ready = surf.frame_ready.swap(false, Ordering::SeqCst);
            let force = surf.paint_needed;
            if !(ready || force) {
                continue;
            }
            surf.paint_needed = false;

            // Reserve a free SHM slot *before* expensive paint/readback so we do not
            // burn a full software frame when the host has not drained yet.
            let prod_hdr = unsafe { &mut *(surf.rw_ptr as *mut ProducerControlHeader) };
            let consumer_ack = unsafe { &*surf.consumer_ack_ptr };
            let mut reserved_slot: Option<usize> = None;
            for _ in 0..3 {
                let slot_idx = surf.current_slot;
                match reserve_slot_for_writing(
                    slot_idx,
                    prod_hdr,
                    consumer_ack,
                    &surf.last_pub_gen,
                    &surf.last_pub_seq,
                ) {
                    Ok(()) => {
                        reserved_slot = Some(slot_idx);
                        break;
                    }
                    Err("E_SHM_SLOT_CLAIMED_BY_CONSUMER") => {
                        surf.current_slot = (surf.current_slot + 1) % 3;
                    }
                    Err(e) => {
                        log_engine!(
                            "[lotus-engine] reserve_slot_for_writing failed for {}: {}",
                            surface_id,
                            e
                        );
                        break;
                    }
                }
            }
            let Some(slot_idx) = reserved_slot else {
                surf.frame_ready.store(true, Ordering::SeqCst);
                continue;
            };
            any_frame_work = true;

            let release_reservation = |hdr: &ProducerControlHeader, idx: usize| {
                // Put the slot back to READY only if we published; FREE if we abort.
                // Callers that abort before publish should use STATE_FREE — see below.
                hdr.slots[idx].state.store(STATE_FREE, Ordering::Release);
            };

            let ctx_size = surf.rendering_ctx.size();
            if ctx_size.width != surf.width || ctx_size.height != surf.height {
                surf.rendering_ctx.resize(PhysicalSize::new(surf.width, surf.height));
            }
            let ctx_size = surf.rendering_ctx.size();
            // Never read back a rect that disagrees with the FB — Webrender panics when
            // upload/read buffer length disagrees with the declared rect (+ unpack stride).
            if ctx_size.width != surf.width || ctx_size.height != surf.height {
                log_engine!(
                    "[lotus-engine] paint size mismatch for {}: surf={}x{} ctx={}x{} — skipping frame",
                    surface_id, surf.width, surf.height, ctx_size.width, ctx_size.height
                );
                release_reservation(prod_hdr, slot_idx);
                surf.frame_ready.store(true, Ordering::SeqCst);
                continue;
            }
            if let Err(e) = surf.rendering_ctx.make_current() {
                log_engine!("[lotus-engine] make_current error for {}: {:?}", surface_id, e);
            }
            surf.webview.paint();

            let rect = DeviceIntRect::from_origin_and_size(
                DeviceIntPoint::new(0, 0),
                DeviceIntSize::new(surf.width as i32, surf.height as i32),
            );

            // Single readback — present after read so we capture the painted buffer.
            // (Direct scratch resize tripped seccomp SIGSYS in the jail on large panes.)
            let image = surf.rendering_ctx.read_to_image(rect);
            surf.rendering_ctx.present();

            let Some(image) = image else {
                log_engine!("[lotus-engine] read_to_image returned None for {}", surface_id);
                release_reservation(prod_hdr, slot_idx);
                continue;
            };
            let raw_pixels = image.into_raw();
            let expected_len = (surf.width as usize) * (surf.height as usize) * 4;
            if raw_pixels.len() != expected_len {
                log_engine!(
                    "[lotus-engine] raw_pixels.len() mismatch for {}: got {}, expected {}",
                    surface_id,
                    raw_pixels.len(),
                    expected_len
                );
                release_reservation(prod_hdr, slot_idx);
                continue;
            }

            let slot_offset = 256 + slot_idx * surf.geom.slot_stride_bytes;
            let slot_ptr = unsafe { surf.rw_ptr.add(slot_offset) };
            let row_bytes = (surf.width as usize) * 4;

            if row_bytes > surf.geom.stride
                || (surf.height as usize) * surf.geom.stride > surf.geom.slot_stride_bytes
            {
                log_engine!(
                    "[lotus-engine] Skip write: dimensions {}x{} exceed SHM plane geometry",
                    surf.width,
                    surf.height
                );
                release_reservation(prod_hdr, slot_idx);
                continue;
            }

            if row_bytes == surf.geom.stride {
                unsafe {
                    std::ptr::copy_nonoverlapping(raw_pixels.as_ptr(), slot_ptr, expected_len);
                }
            } else {
                for y in 0..(surf.height as usize) {
                    let src_row = &raw_pixels[y * row_bytes..(y + 1) * row_bytes];
                    let dst_row = unsafe { slot_ptr.add(y * surf.geom.stride) };
                    unsafe {
                        std::ptr::copy_nonoverlapping(src_row.as_ptr(), dst_row, row_bytes);
                    }
                }
            }

            match publish_slot(
                slot_idx,
                prod_hdr,
                surf.width,
                surf.height,
                surf.geom.stride as u32,
                &mut surf.last_pub_gen,
                &mut surf.last_pub_seq,
            ) {
                Ok(pub_seq) => {
                    log_debug!(
                        "[lotus-engine] publish_slot OK for {}: slot={}, pub_seq={}",
                        surface_id,
                        slot_idx,
                        pub_seq
                    );
                    if let Some(ctrl_fd) = config.control_fd {
                        send_engine_event(
                            ctrl_fd,
                            &EngineEvent::FramePublished {
                                surface_id: surface_id.clone(),
                                slot_idx,
                                pub_seq,
                            },
                        );
                    }
                    surf.current_slot = (surf.current_slot + 1) % 3;
                }
                Err(e) => {
                    log_engine!("[lotus-engine] publish_slot failed for {}: {}", surface_id, e);
                    release_reservation(prod_hdr, slot_idx);
                }
            }
        }

        // Sleep according to host display timing. Do NOT clamp to 16 ms — that kept the
        // loop spinning at ~60 Hz even after SetDisplayTiming dropped to background/hidden
        // rates (continuous rAF keeps is_waiting_for_frame true). Cap only so timers /
        // control IPC still pump while heavily throttled (watchdog heartbeats, etc.).
        let wake_timeout = if refresh_driver.graphics_paused() {
            Duration::from_millis(250)
        } else if any_frame_work || refresh_driver.is_waiting_for_frame() {
            refresh_driver
                .frame_period()
                .min(Duration::from_millis(250))
                .max(Duration::from_millis(1))
        } else {
            Duration::from_millis(50)
        };
        let _ = wake_rx.recv_timeout(wake_timeout);
    }
}

fn send_engine_event(fd: RawFd, event: &EngineEvent) {
    if let Ok(json) = serde_json::to_string(event) {
        let mut msg = json.into_bytes();
        msg.push(b'\n');
        let _ = unsafe { libc::write(fd, msg.as_ptr() as *const _, msg.len()) };
    }
}

fn handle_input_event(webview: &servo::WebView, event: InputEvent) {
    match event {
        InputEvent::MouseMove { x, y, .. } => {
            let pt = Point2D::new(x as f32, y as f32);
            webview.notify_input_event(ServoInputEvent::MouseMove(MouseMoveEvent::new(
                servo::WebViewPoint::Device(pt),
            )));
        }
        InputEvent::MouseDown { button, x, y, .. } => {
            let pt = Point2D::new(x as f32, y as f32);
            let btn = match button {
                0 => ServoMouseButton::Primary,
                1 => ServoMouseButton::Auxiliary,
                2 => ServoMouseButton::Secondary,
                _ => ServoMouseButton::Primary,
            };
            webview.notify_input_event(ServoInputEvent::MouseButton(MouseButtonEvent::new(
                MouseButtonAction::Down,
                btn,
                servo::WebViewPoint::Device(pt),
            )));
        }
        InputEvent::MouseUp { button, x, y, .. } => {
            let pt = Point2D::new(x as f32, y as f32);
            let btn = match button {
                0 => ServoMouseButton::Primary,
                1 => ServoMouseButton::Auxiliary,
                2 => ServoMouseButton::Secondary,
                _ => ServoMouseButton::Primary,
            };
            webview.notify_input_event(ServoInputEvent::MouseButton(MouseButtonEvent::new(
                MouseButtonAction::Up,
                btn,
                servo::WebViewPoint::Device(pt),
            )));
        }
        InputEvent::MouseWheel { x, y, dx, dy, .. } => {
            let delta = WheelDelta {
                x: dx,
                y: dy,
                z: 0.0,
                mode: WheelMode::DeltaPixel,
            };
            let pt = Point2D::new(x as f32, y as f32);
            webview.notify_input_event(ServoInputEvent::Wheel(WheelEvent::new(
                delta,
                servo::WebViewPoint::Device(pt),
            )));
        }
        InputEvent::KeyDown { key, code, keycode, modifiers, repeat, .. } => {
            let s_key = if let Some(ref k) = key {
                map_str_to_key(k)
            } else if keycode > 0 {
                if let Some(ch) = char::from_u32(keycode) {
                    Key::Character(ch.to_string())
                } else {
                    Key::Named(NamedKey::Unidentified)
                }
            } else {
                Key::Named(NamedKey::Unidentified)
            };
            let s_code = if let Some(ref c) = code {
                map_str_to_code(c)
            } else {
                Code::Unidentified
            };
            let s_mods = Modifiers::from_bits_truncate(modifiers);
            let kb_event = ServoKeyboardEvent::new_without_event(
                KeyState::Down,
                s_key,
                s_code,
                Location::Standard,
                s_mods,
                repeat,
                false,
            );
            webview.notify_input_event(ServoInputEvent::Keyboard(kb_event));
        }
        InputEvent::KeyUp { key, code, keycode, modifiers, .. } => {
            let s_key = if let Some(ref k) = key {
                map_str_to_key(k)
            } else if keycode > 0 {
                if let Some(ch) = char::from_u32(keycode) {
                    Key::Character(ch.to_string())
                } else {
                    Key::Named(NamedKey::Unidentified)
                }
            } else {
                Key::Named(NamedKey::Unidentified)
            };
            let s_code = if let Some(ref c) = code {
                map_str_to_code(c)
            } else {
                Code::Unidentified
            };
            let s_mods = Modifiers::from_bits_truncate(modifiers);
            let kb_event = ServoKeyboardEvent::new_without_event(
                KeyState::Up,
                s_key,
                s_code,
                Location::Standard,
                s_mods,
                false,
                false,
            );
            webview.notify_input_event(ServoInputEvent::Keyboard(kb_event));
        }
        _ => {}
    }
}

fn map_str_to_key(s: &str) -> Key {
    match s {
        "Backspace" => Key::Named(NamedKey::Backspace),
        "Tab" => Key::Named(NamedKey::Tab),
        "Enter" => Key::Named(NamedKey::Enter),
        "Escape" => Key::Named(NamedKey::Escape),
        "ArrowLeft" => Key::Named(NamedKey::ArrowLeft),
        "ArrowRight" => Key::Named(NamedKey::ArrowRight),
        "ArrowUp" => Key::Named(NamedKey::ArrowUp),
        "ArrowDown" => Key::Named(NamedKey::ArrowDown),
        "PageUp" => Key::Named(NamedKey::PageUp),
        "PageDown" => Key::Named(NamedKey::PageDown),
        "Home" => Key::Named(NamedKey::Home),
        "End" => Key::Named(NamedKey::End),
        "Insert" => Key::Named(NamedKey::Insert),
        "Delete" => Key::Named(NamedKey::Delete),
        "Shift" => Key::Named(NamedKey::Shift),
        "Control" => Key::Named(NamedKey::Control),
        "Alt" => Key::Named(NamedKey::Alt),
        "Meta" | "Super" => Key::Named(NamedKey::Meta),
        other => {
            if other.chars().count() == 1 {
                Key::Character(other.to_string())
            } else {
                Key::Named(NamedKey::Unidentified)
            }
        }
    }
}

fn map_str_to_code(s: &str) -> Code {
    match s {
        "KeyA" => Code::KeyA,
        "KeyB" => Code::KeyB,
        "KeyC" => Code::KeyC,
        "KeyD" => Code::KeyD,
        "KeyE" => Code::KeyE,
        "KeyF" => Code::KeyF,
        "KeyG" => Code::KeyG,
        "KeyH" => Code::KeyH,
        "KeyI" => Code::KeyI,
        "KeyJ" => Code::KeyJ,
        "KeyK" => Code::KeyK,
        "KeyL" => Code::KeyL,
        "KeyM" => Code::KeyM,
        "KeyN" => Code::KeyN,
        "KeyO" => Code::KeyO,
        "KeyP" => Code::KeyP,
        "KeyQ" => Code::KeyQ,
        "KeyR" => Code::KeyR,
        "KeyS" => Code::KeyS,
        "KeyT" => Code::KeyT,
        "KeyU" => Code::KeyU,
        "KeyV" => Code::KeyV,
        "KeyW" => Code::KeyW,
        "KeyX" => Code::KeyX,
        "KeyY" => Code::KeyY,
        "KeyZ" => Code::KeyZ,
        "Digit0" => Code::Digit0,
        "Digit1" => Code::Digit1,
        "Digit2" => Code::Digit2,
        "Digit3" => Code::Digit3,
        "Digit4" => Code::Digit4,
        "Digit5" => Code::Digit5,
        "Digit6" => Code::Digit6,
        "Digit7" => Code::Digit7,
        "Digit8" => Code::Digit8,
        "Digit9" => Code::Digit9,
        "Enter" => Code::Enter,
        "Escape" => Code::Escape,
        "Backspace" => Code::Backspace,
        "Tab" => Code::Tab,
        "Space" => Code::Space,
        _ => Code::Unidentified,
    }
}
