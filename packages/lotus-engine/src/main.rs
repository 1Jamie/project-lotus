//! packages/lotus-engine/src/main.rs
//! Standalone Headless Servo Engine Process for Secure Panes.
//!
//! Executes inside a sandboxed Linux container (Stage A namespaces + Stage B seccomp).
//! Communicates with the Host process (Node.js / lotus-core) via:
//! - Control socket / pipe (LOTUS_CONTROL_FD): bidirectional JSON-RPC control & input events.
//! - Liveness pipe (LOTUS_LIVENESS_FD): parent-death detection via POLLHUP.
//! - Shared Memory (RemotePaneShm): zero-copy triple-buffered RGBA8 frame publication.

use std::env;
use std::fs;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use dpi::PhysicalSize;
use euclid::Point2D;
use servo::resources::{self, Resource};
use servo::{
    DeviceIntPoint, DeviceIntRect, DeviceIntSize, EventLoopWaker, InputEvent as ServoInputEvent,
    LoadStatus, MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    RenderingContext, ServoBuilder, SoftwareRenderingContext, WebViewBuilder, WebViewDelegate,
    WheelDelta, WheelEvent, WheelMode,
};
use url::Url;

use lotus_supervisor::shm::{
    calculate_shm_geometry, publish_slot, reserve_slot_for_writing, ConsumerAckHeader,
    ProducerControlHeader, SealedMemfdPlane, CLAIM_UNCLAIMED,
};
use lotus_supervisor::stage_b::apply_stage_b_seccomp;

mod protocol;
use protocol::{ControlMessage, EngineEvent, InputEvent};

// =========================================================================
// Resource Reader for Embedded Servo Assets
// =========================================================================

struct EngineResourceReader;

impl resources::ResourceReaderMethods for EngineResourceReader {
    fn read(&self, file: Resource) -> Vec<u8> {
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
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        vec![manifest_dir.join("../lotus-core/resources")]
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
    load_complete: Arc<AtomicBool>,
    frame_ready: Arc<AtomicBool>,
    last_url: Arc<Mutex<Option<String>>>,
    last_title: Arc<Mutex<Option<String>>>,
}

impl WebViewDelegate for EngineDelegate {
    fn notify_load_status_changed(&self, webview: servo::WebView, status: LoadStatus) {
        if status == LoadStatus::Complete {
            if let Some(u) = webview.url() {
                *self.last_url.lock().unwrap() = Some(u.to_string());
            }
            self.load_complete.store(true, Ordering::SeqCst);
        }
    }

    fn notify_new_frame_ready(&self, _webview: servo::WebView) {
        self.frame_ready.store(true, Ordering::SeqCst);
    }

    fn notify_page_title_changed(&self, _webview: servo::WebView, title: Option<String>) {
        if let Some(t) = title {
            *self.last_title.lock().unwrap() = Some(t);
        }
    }
}

// =========================================================================
// Engine Config & Argument Parsing
// =========================================================================

struct EngineConfig {
    control_fd: Option<RawFd>,
    liveness_fd: Option<RawFd>,
    shm_fd: Option<RawFd>,
    width: u32,
    height: u32,
    surface_id: String,
    initial_url: String,
}

fn parse_config() -> EngineConfig {
    let mut control_fd: Option<RawFd> = env::var("LOTUS_CONTROL_FD").ok().and_then(|v| v.parse().ok());
    let mut liveness_fd: Option<RawFd> = env::var("LOTUS_LIVENESS_FD").ok().and_then(|v| v.parse().ok());
    let mut shm_fd: Option<RawFd> = env::var("LOTUS_SHM_FD").ok().and_then(|v| v.parse().ok());
    let mut width = 800u32;
    let mut height = 600u32;
    let mut surface_id = "default".to_string();
    let mut initial_url = "about:blank".to_string();

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
            "--liveness-fd" => {
                i += 1;
                if i < args.len() {
                    liveness_fd = args[i].parse().ok();
                }
            }
            "--shm-fd" => {
                i += 1;
                if i < args.len() {
                    shm_fd = args[i].parse().ok();
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
            _ => {}
        }
        i += 1;
    }

    EngineConfig {
        control_fd,
        liveness_fd,
        shm_fd,
        width,
        height,
        surface_id,
        initial_url,
    }
}

// =========================================================================
// Main Entrypoint & State Machine
// =========================================================================

fn main() {
    let config = parse_config();
    let width = config.width;
    let height = config.height;

    // 1. Initialize Servo Singleton & Waker FIRST (Opts must be initialized before anything calls opts::get())
    let (wake_tx, wake_rx) = mpsc::channel();
    let waker = EngineWaker(wake_tx);
    let mut prefs = servo::prefs::Preferences::default();
    prefs.shell_background_color_rgba = [0.0, 0.0, 0.0, 1.0];

    let servo = ServoBuilder::default()
        .event_loop_waker(Box::new(waker))
        .preferences(prefs)
        .build();

    // 2. Initialize Headless Software / Offscreen Rendering Context
    let rendering_ctx = Rc::new(
        SoftwareRenderingContext::new(PhysicalSize::new(width, height))
            .expect("Failed to initialize SoftwareRenderingContext"),
    );

    // 3. Map Shared Memory Region (RemotePaneShm)
    let geom = calculate_shm_geometry(width, height).expect("calculate_shm_geometry");
    let total_shm_size = 256 + geom.total_pixel_bytes;

    let (_owned_plane, rw_ptr) = if let Some(fd) = config.shm_fd {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                total_shm_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            ) as *mut u8
        };
        if ptr != libc::MAP_FAILED as *mut u8 {
            (None, ptr)
        } else {
            let plane = SealedMemfdPlane::new("lotus_engine_shm", total_shm_size)
                .expect("Failed to create SealedMemfdPlane");
            let ptr = plane.rw_ptr;
            (Some(plane), ptr)
        }
    } else {
        let plane = SealedMemfdPlane::new("lotus_engine_shm", total_shm_size)
            .expect("Failed to create SealedMemfdPlane");
        let ptr = plane.rw_ptr;
        (Some(plane), ptr)
    };

    // Initialize producer control header
    let prod_hdr = unsafe { &mut *(rw_ptr as *mut ProducerControlHeader) };
    prod_hdr.pub_seq.store(0, Ordering::Relaxed);
    prod_hdr.prod_resize_gen.store(1, Ordering::Relaxed);
    prod_hdr.active_slots = 3;

    // Local consumer ack simulation for standalone tracking
    let consumer_ack = ConsumerAckHeader {
        consumer_gen_id: AtomicU64::new(1),
        claimed_generation: AtomicU64::new(0),
        claimed_seq: AtomicU64::new(0),
        free_seq: AtomicU64::new(0),
        claimed_slot: AtomicU32::new(CLAIM_UNCLAIMED),
        _pad: [0; 28],
    };

    let mut last_pub_gen = [0u64; 3];
    let mut last_pub_seq = [0u64; 3];
    let mut current_slot = 0usize;

    // 4. Create WebView with initial URL
    let load_complete = Arc::new(AtomicBool::new(false));
    let frame_ready = Arc::new(AtomicBool::new(false));
    let last_url = Arc::new(Mutex::new(None));
    let last_title = Arc::new(Mutex::new(None));

    let delegate = EngineDelegate {
        load_complete: load_complete.clone(),
        frame_ready: frame_ready.clone(),
        last_url: last_url.clone(),
        last_title: last_title.clone(),
    };

    let webview = WebViewBuilder::new(&servo, rendering_ctx.clone())
        .delegate(Rc::new(delegate))
        .build();

    if let Ok(u) = Url::parse(&config.initial_url) {
        webview.load(u);
    }

    // 5. Apply Stage B Seccomp-BPF Lockdown
    apply_stage_b_seccomp().expect("Failed to apply Stage B seccomp");

    // 6. Verify Lockdown Invariants (/proc/self/status Seccomp == 2 && NoNewPrivs == 1)
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

    // 7. Attestation Gate Handshake over LOTUS_CONTROL_FD
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

        // Send ENGINE_READY
        let ready_msg = b"ENGINE_READY\n";
        let _ = unsafe { libc::write(ctrl_fd, ready_msg.as_ptr() as *const _, ready_msg.len()) };
    }

    // Set control_fd non-blocking for event loop
    if let Some(ctrl_fd) = config.control_fd {
        unsafe {
            let flags = libc::fcntl(ctrl_fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(ctrl_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
    }

    // 8. Steady-State Event & Render Loop
    let mut control_read_buf = Vec::new();
    let mut running = true;
    let mut paint_needed = true;

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

        // Read Control Messages from LOTUS_CONTROL_FD
        if let Some(ctrl_fd) = config.control_fd {
            let mut chunk = [0u8; 512];
            loop {
                let r = unsafe { libc::read(ctrl_fd, chunk.as_mut_ptr() as *mut _, chunk.len()) };
                if r > 0 {
                    control_read_buf.extend_from_slice(&chunk[..r as usize]);
                } else {
                    break;
                }
            }

            // Process complete lines
            while let Some(idx) = control_read_buf.iter().position(|&b| b == b'\n') {
                let line_bytes: Vec<u8> = control_read_buf.drain(..=idx).collect();
                let line_str = String::from_utf8_lossy(&line_bytes[..line_bytes.len().saturating_sub(1)]);
                let trimmed = line_str.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if trimmed == "SHUTDOWN" {
                    running = false;
                    break;
                }

                if let Ok(msg) = serde_json::from_str::<ControlMessage>(trimmed) {
                    match msg {
                        ControlMessage::Navigate { url, .. } => {
                            if let Ok(u) = Url::parse(&url) {
                                webview.load(u);
                                paint_needed = true;
                            }
                        }
                        ControlMessage::Resize { width: new_w, height: new_h, .. } => {
                            if new_w > 0 && new_h > 0 {
                                webview.resize(PhysicalSize::new(new_w, new_h));
                                paint_needed = true;
                            }
                        }
                        ControlMessage::Destroy { .. } => {
                            running = false;
                            break;
                        }
                        ControlMessage::Input { event } => {
                            handle_input_event(&webview, event);
                            paint_needed = true;
                        }
                        ControlMessage::Ping => {
                            send_engine_event(ctrl_fd, &EngineEvent::Pong);
                        }
                    }
                }
            }
        }

        // Spin Servo Event Loop
        servo.spin_event_loop();
        let _ = wake_rx.recv_timeout(Duration::from_millis(5));

        // Paint and Publish Frame if ready
        if frame_ready.swap(false, Ordering::SeqCst) || paint_needed {
            paint_needed = false;
            let _ = rendering_ctx.make_current();
            webview.paint();

            let rect = DeviceIntRect::from_origin_and_size(
                DeviceIntPoint::new(0, 0),
                DeviceIntSize::new(width as i32, height as i32),
            );

            if let Some(image) = rendering_ctx.read_to_image(rect) {
                let raw_pixels = image.into_raw();
                if raw_pixels.len() == (width as usize) * (height as usize) * 4 {
                    let slot_idx = current_slot;
                    if reserve_slot_for_writing(slot_idx, prod_hdr, &consumer_ack, &last_pub_gen, &last_pub_seq).is_ok() {
                        let slot_offset = 256 + slot_idx * geom.slot_stride_bytes;
                        let slot_ptr = unsafe { rw_ptr.add(slot_offset) };
                        let row_bytes = (width as usize) * 4;

                        for y in 0..(height as usize) {
                            let src_row = &raw_pixels[y * row_bytes..(y + 1) * row_bytes];
                            let dst_row = unsafe { slot_ptr.add(y * geom.stride) };
                            unsafe {
                                std::ptr::copy_nonoverlapping(src_row.as_ptr(), dst_row, row_bytes);
                            }
                        }

                        if let Ok(pub_seq) = publish_slot(
                            slot_idx,
                            prod_hdr,
                            width,
                            height,
                            geom.stride as u32,
                            &mut last_pub_gen,
                            &mut last_pub_seq,
                        ) {
                            if let Some(ctrl_fd) = config.control_fd {
                                send_engine_event(
                                    ctrl_fd,
                                    &EngineEvent::FramePublished {
                                        surface_id: config.surface_id.clone(),
                                        slot_idx,
                                        pub_seq,
                                    },
                                );
                            }
                            current_slot = (current_slot + 1) % 3;
                        }
                    }
                }
            }
        }
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
        InputEvent::MouseWheel { dx, dy, .. } => {
            let delta = WheelDelta {
                x: dx,
                y: dy,
                z: 0.0,
                mode: WheelMode::DeltaPixel,
            };
            webview.notify_input_event(ServoInputEvent::Wheel(WheelEvent::new(
                delta,
                servo::WebViewPoint::Device(Point2D::new(0.0, 0.0)),
            )));
        }
        _ => {}
    }
}
