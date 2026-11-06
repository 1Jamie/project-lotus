use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use std::{env, fs};

use dpi::PhysicalSize;
use servo::resources::{self, Resource};
use servo::{
    DeviceIntPoint, DeviceIntRect, DeviceIntSize, EventLoopWaker, LoadStatus,
    RenderingContext, ServoBuilder, SoftwareRenderingContext, WebViewBuilder, WebViewDelegate,
};
use url::Url;

use lotus_supervisor::shm::{
    calculate_shm_geometry, claim_and_verify_slot, publish_slot, release_slot_claim,
    reserve_slot_for_writing, validate_frame_metadata, ConsumerAckHeader, ProducerControlHeader,
    SealedMemfdPlane, CLAIM_UNCLAIMED,
};
use lotus_supervisor::stage_b::apply_stage_b_seccomp;

struct SpikeResourceReader;

impl resources::ResourceReaderMethods for SpikeResourceReader {
    fn read(&self, file: Resource) -> Vec<u8> {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let res_dir = manifest_dir.join("../lotus-core/resources");
        let path = res_dir.join(file.filename());
        match fs::read(&path) {
            Ok(data) => data,
            Err(e) => {
                eprintln!("Warning: could not read resource {:?}: {}", path, e);
                Vec::new()
            }
        }
    }
    fn sandbox_access_files_dirs(&self) -> Vec<PathBuf> {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        vec![manifest_dir.join("../lotus-core/resources")]
    }
    fn sandbox_access_files(&self) -> Vec<PathBuf> {
        vec![]
    }
}

static RESOURCE_READER: SpikeResourceReader = SpikeResourceReader;
servo::submit_resource_reader!(&RESOURCE_READER);

#[derive(Clone)]
struct SpikeWaker(mpsc::Sender<()>);

impl EventLoopWaker for SpikeWaker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(SpikeWaker(self.0.clone()))
    }
    fn wake(&self) {
        let _ = self.0.send(());
    }
}

struct SpikeDelegate {
    load_complete: Arc<AtomicBool>,
    frame_ready: Arc<AtomicBool>,
}

impl WebViewDelegate for SpikeDelegate {
    fn notify_load_status_changed(&self, _webview: servo::WebView, status: LoadStatus) {
        println!("  [Delegate] LoadStatus: {:?}", status);
        if status == LoadStatus::Complete {
            self.load_complete.store(true, Ordering::SeqCst);
        }
    }

    fn notify_new_frame_ready(&self, _webview: servo::WebView) {
        println!("  [Delegate] New frame ready");
        self.frame_ready.store(true, Ordering::SeqCst);
    }
}

fn run_headless_render_pipeline(test_html_path: &PathBuf, apply_lockdown: bool) {
    let width = 400u32;
    let height = 300u32;

    println!("\n=======================================================");
    println!("Running Headless Render Pipeline (apply_lockdown: {})", apply_lockdown);
    println!("=======================================================");

    // 1. Initialize Servo and Runtime Infrastructure FIRST so opts are registered
    println!("[Step 1] Initializing Servo singleton and waker...");
    let (wake_tx, wake_rx) = mpsc::channel();
    let waker = SpikeWaker(wake_tx);
    let mut prefs = servo::prefs::Preferences::default();
    prefs.shell_background_color_rgba = [0.0, 0.0, 0.0, 1.0];

    let servo = ServoBuilder::default()
        .event_loop_waker(Box::new(waker))
        .preferences(prefs)
        .build();

    // 2. Initialize Headless Software / Offscreen Rendering Context
    println!("[Step 2] Initializing SoftwareRenderingContext ({}x{})...", width, height);
    let rendering_ctx = Rc::new(
        SoftwareRenderingContext::new(PhysicalSize::new(width, height))
            .expect("Failed to initialize SoftwareRenderingContext"),
    );

    // 3. Map Shared Memory Region (RemotePaneShm)
    println!("[Step 3] Calculating SHM geometry and creating SealedMemfdPlane...");
    let geom = calculate_shm_geometry(width, height).expect("Failed to calculate geometry");
    let total_shm_size = 256 + geom.total_pixel_bytes;
    let plane = SealedMemfdPlane::new("headless_spike_shm", total_shm_size)
        .expect("Failed to create SealedMemfdPlane");

    // Initialize producer control header
    let prod_hdr = unsafe { &mut *(plane.rw_ptr as *mut ProducerControlHeader) };
    prod_hdr.pub_seq.store(0, Ordering::Relaxed);
    prod_hdr.prod_resize_gen.store(1, Ordering::Relaxed);
    prod_hdr.active_slots = 3;

    // Track producer publication history
    let mut last_pub_gen = [0u64; 3];
    let mut last_pub_seq = [0u64; 3];

    // Consumer ack state
    let consumer_ack = ConsumerAckHeader {
        consumer_gen_id: AtomicU64::new(1),
        claimed_generation: AtomicU64::new(0),
        claimed_seq: AtomicU64::new(0),
        free_seq: AtomicU64::new(0),
        claimed_slot: AtomicU32::new(CLAIM_UNCLAIMED),
        _pad: [0; 28],
    };

    let load_complete = Arc::new(AtomicBool::new(false));
    let frame_ready = Arc::new(AtomicBool::new(false));
    let delegate = SpikeDelegate {
        load_complete: load_complete.clone(),
        frame_ready: frame_ready.clone(),
    };

    println!("[Step 4] Creating WebView with test HTML document...");
    let webview = WebViewBuilder::new(&servo, rendering_ctx.clone())
        .delegate(Rc::new(delegate))
        .build();

    let doc_url = Url::from_file_path(test_html_path.canonicalize().unwrap()).unwrap();
    println!("  Target document URL: {}", doc_url);
    webview.load(doc_url);

    // If lockdown is requested, apply Stage B seccomp now (at the verified transition boundary)
    if apply_lockdown {
        println!("[Step 5] Applying Stage B Seccomp-BPF lockdown to execution thread...");
        apply_stage_b_seccomp().expect("Failed to apply Stage B seccomp");
        println!("  Stage B Seccomp-BPF filter active!");
    } else {
        println!("[Step 5] Skipping Stage B (baseline run)...");
    }

    // 6. Pump Servo Event Loop until document completes loading and layout
    println!("[Step 6] Pumping Servo event loop for load & layout...");
    let start = Instant::now();
    while !load_complete.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(10) {
        servo.spin_event_loop();
        let _ = wake_rx.recv_timeout(Duration::from_millis(50));
    }
    assert!(load_complete.load(Ordering::SeqCst), "Document load timed out!");
    println!("  Document load completed in {:?}", start.elapsed());

    // 7. Paint and read back image
    println!("[Step 7] Painting frame and reading back pixels...");
    let _ = rendering_ctx.make_current();
    webview.paint();

    let rect = DeviceIntRect::from_origin_and_size(
        DeviceIntPoint::new(0, 0),
        DeviceIntSize::new(width as i32, height as i32),
    );
    let image = rendering_ctx
        .read_to_image(rect)
        .expect("Failed to read image from headless rendering context");
    let raw_pixels = image.into_raw();
    println!("  Successfully read back {} bytes of RGBA pixels", raw_pixels.len());

    // Verify non-empty / non-zero frame
    let has_non_zero = raw_pixels.iter().any(|&b| b != 0);
    assert!(has_non_zero, "Rendered frame was completely empty / all zeros!");
    println!("  Verified rendered frame contains non-zero pixel data.");

    // 8. Publish to Shared Memory (Producer)
    println!("[Step 8] Publishing frame to RemotePaneShm slot...");
    let slot_idx = 0usize;
    reserve_slot_for_writing(slot_idx, prod_hdr, &consumer_ack, &last_pub_gen, &last_pub_seq)
        .expect("Failed to reserve slot");

    let slot_offset = 256 + slot_idx * geom.slot_stride_bytes;
    let slot_ptr = unsafe { plane.rw_ptr.add(slot_offset) };

    // Copy rows with stride alignment
    let row_bytes = (width as usize) * 4;
    for y in 0..(height as usize) {
        let src_row = &raw_pixels[y * row_bytes..(y + 1) * row_bytes];
        let dst_row = unsafe { slot_ptr.add(y * geom.stride) };
        unsafe {
            std::ptr::copy_nonoverlapping(src_row.as_ptr(), dst_row, row_bytes);
        }
    }

    let pub_seq = publish_slot(
        slot_idx,
        prod_hdr,
        width,
        height,
        geom.stride as u32,
        &mut last_pub_gen,
        &mut last_pub_seq,
    )
    .expect("Failed to publish slot");
    println!("  Published frame to slot {} with pub_seq = {}", slot_idx, pub_seq);

    // 9. Consume and Verify via Read-Only Consumer Mapping (Consumer)
    println!("[Step 9] Simulating Host Compositor reading frame via read-only descriptor...");
    let ro_ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            plane.size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            plane.ro_fd,
            0,
        ) as *const u8
    };
    assert!(ro_ptr != libc::MAP_FAILED as *const u8, "Consumer mmap failed");

    let ro_header = unsafe { &*(ro_ptr as *const ProducerControlHeader) };

    let (_claimed_gen, claimed_seq) = claim_and_verify_slot(
        slot_idx,
        ro_header,
        &consumer_ack,
        1,
    )
    .expect("Consumer claim failed");

    assert_eq!(claimed_seq, pub_seq);

    let validated = validate_frame_metadata(
        slot_idx,
        &ro_header.slots[slot_idx],
        ro_header,
        &consumer_ack,
        1,
        width,
        height,
        geom.slot_stride_bytes,
        geom.total_pixel_bytes,
    )
    .expect("Consumer 15-point frame validation failed");

    assert_eq!(validated.width, width);
    assert_eq!(validated.height, height);
    assert_eq!(validated.stride, geom.stride);
    assert_eq!(validated.pub_seq, pub_seq);

    let consumer_slot_ptr = unsafe { ro_ptr.add(256 + validated.slot_offset) };
    for y in 0..(height as usize) {
        let expected_row = &raw_pixels[y * row_bytes..(y + 1) * row_bytes];
        let actual_row = unsafe {
            std::slice::from_raw_parts(consumer_slot_ptr.add(y * validated.stride), row_bytes)
        };
        assert_eq!(actual_row, expected_row, "Row {} pixel mismatch between producer and consumer!", y);
    }

    release_slot_claim(&consumer_ack, pub_seq);
    unsafe {
        libc::munmap(ro_ptr as *mut libc::c_void, plane.size);
    }
    println!("  Consumer verification confirmed 100% pixel matching and released slot.");
    println!("Pipeline finished successfully!");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let test_app_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../lotus-core/test_apps/sandbox_test_app/ui/sandboxed.html");

    assert!(
        test_app_path.exists(),
        "Test HTML file does not exist at {:?}",
        test_app_path
    );

    if args.len() > 1 && args[1] == "--stage-b" {
        println!("=== Running in Subprocess: Stage B Seccomp Mode ===");
        run_headless_render_pipeline(&test_app_path, true);
        println!("[OK] Stage B Headless Pipeline completed successfully!");
        return;
    }

    if args.len() > 1 && args[1] == "--baseline" {
        println!("=== Running in Subprocess: Baseline Mode ===");
        run_headless_render_pipeline(&test_app_path, false);
        println!("[OK] Baseline Headless Pipeline completed successfully!");
        return;
    }

    println!("=== Lotus Headless Servo Feasibility Spike ===");
    println!("Target test document: {:?}", test_app_path);

    // Run Stage 1: Baseline Headless Execution in fresh subprocess
    println!("\n>>> Phase A: Baseline Headless Pipeline (Pre-Lockdown) <<<");
    let current_exe = std::env::current_exe().expect("Failed to get current_exe");
    let status_a = std::process::Command::new(&current_exe)
        .arg("--baseline")
        .status()
        .expect("Failed to spawn baseline subprocess");
    assert!(status_a.success(), "Baseline run failed: {:?}", status_a);
    println!("\n[OK] Phase A Passed.");

    // Run Stage 2: Sandboxed Execution Under Stage B Seccomp in fresh subprocess
    println!("\n>>> Phase B: Sandboxed Headless Pipeline Under Stage B Seccomp <<<");
    let status_b = std::process::Command::new(&current_exe)
        .arg("--stage-b")
        .status()
        .expect("Failed to spawn Stage B subprocess");
    assert!(status_b.success(), "Stage B run failed: {:?}", status_b);
    println!("\n[OK] Phase B Passed.");

    println!("\n=======================================================");
    println!("ALL FEASIBILITY SPIKE CRITERIA MET AND VERIFIED!");
    println!("=======================================================");
}
