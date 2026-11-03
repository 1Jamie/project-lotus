//! lotus-sandbox-probe: Native helper binary to verify Stage-A + Stage-B sandbox enforcement.
//!
//! Executed as the child process under lotus-supervisor to test sandbox guarantees
//! without booting the full Servo renderer.

use lotus_supervisor::stage_b;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::process;
use std::thread;

fn assert_single_threaded() {
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("Threads:") {
                let count = line.split_whitespace().nth(1).unwrap_or("0");
                if count != "1" {
                    eprintln!("[probe] Fatal: Expected 1 thread, found {}", count);
                    process::abort();
                }
                return;
            }
        }
    }
}

fn main() {
    // 1. Pre-main audit: assert single threaded before stage B
    assert_single_threaded();

    // 2. Apply Stage B seccomp filter
    if let Err(e) = stage_b::apply_stage_b_seccomp() {
        eprintln!("[probe] Failed to apply Stage B seccomp: {}", e);
        process::exit(1);
    }

    // 3. Obtain control descriptor
    let control_fd: i32 = match env::var("LOTUS_CONTROL_FD").ok().and_then(|v| v.parse().ok()) {
        Some(fd) => fd,
        None => {
            eprintln!("[probe] Missing LOTUS_CONTROL_FD");
            process::exit(2);
        }
    };

    let mut control_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(control_fd) };

    // 4. Dual-process handshake gate: Write STAGE_B_LOCKED and wait for PROCEED_TO_INIT
    if let Err(e) = control_stream.write_all(b"STAGE_B_LOCKED\n") {
        eprintln!("[probe] Failed to write STAGE_B_LOCKED: {}", e);
        process::exit(3);
    }
    let _ = control_stream.flush();

    let mut buf = [0u8; 64];
    let n = match control_stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => {
            eprintln!("[probe] Failed to receive PROCEED_TO_INIT from host");
            process::exit(4);
        }
    };

    let msg = std::str::from_utf8(&buf[..n]).unwrap_or("");
    if !msg.contains("PROCEED_TO_INIT") {
        eprintln!("[probe] Unexpected message: {}", msg);
        process::exit(5);
    }

    // 5. Execute sandbox security verifications
    let mut results = Vec::new();

    // Check 1: Verify child PID == 1 in private PID namespace
    let pid = unsafe { libc::getpid() };
    results.push(format!("PID_IS_1:{}", pid == 1));

    // Check 2: Verify /proc hiding host processes (only PID 1 present in /proc)
    let mut proc_pids = Vec::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Ok(file_name) = entry.file_name().into_string() {
                if file_name.chars().all(|c| c.is_ascii_digit()) {
                    proc_pids.push(file_name);
                }
            }
        }
    }
    results.push(format!("PROC_ONLY_PID_1:{}", proc_pids.len() == 1 && proc_pids.contains(&"1".to_string())));

    // Check 3: Verify Capabilities are completely zeroed
    let mut cap_eff_zero = false;
    let mut no_new_privs = false;
    let mut seccomp_mode_2 = false;
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("CapEff:") {
                let val = line.split_whitespace().nth(1).unwrap_or("");
                cap_eff_zero = val == "0000000000000000";
            }
            if line.starts_with("NoNewPrivs:") {
                let val = line.split_whitespace().nth(1).unwrap_or("");
                no_new_privs = val == "1";
            }
            if line.starts_with("Seccomp:") {
                let val = line.split_whitespace().nth(1).unwrap_or("");
                seccomp_mode_2 = val == "2";
            }
        }
    }
    results.push(format!("CAP_EFF_ZERO:{}", cap_eff_zero));
    results.push(format!("NO_NEW_PRIVS:{}", no_new_privs));
    results.push(format!("SECCOMP_MODE_2:{}", seccomp_mode_2));

    // Check 4: Verify thread creation succeeds under seccomp
    let thread_ok = thread::spawn(|| 123).join().unwrap_or(0) == 123;
    results.push(format!("THREAD_SPAWN_OK:{}", thread_ok));

    // Check 5: Verify SYS_fork fails with EPERM
    let fork_res = unsafe { libc::syscall(libc::SYS_fork) };
    let fork_blocked = fork_res == -1 && unsafe { *libc::__errno_location() } == libc::EPERM;
    results.push(format!("FORK_BLOCKED:{}", fork_blocked));

    // Check 6: Verify SYS_mount fails with EPERM
    let mount_res = unsafe { libc::mount(std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, std::ptr::null()) };
    let mount_blocked = mount_res == -1 && unsafe { *libc::__errno_location() } == libc::EPERM;
    results.push(format!("MOUNT_BLOCKED:{}", mount_blocked));

    // Check 7: Verify SYS_unshare fails with EPERM
    let unshare_res = unsafe { libc::syscall(libc::SYS_unshare, libc::CLONE_NEWUSER) };
    let unshare_blocked = unshare_res == -1 && unsafe { *libc::__errno_location() } == libc::EPERM;
    results.push(format!("UNSHARE_BLOCKED:{}", unshare_blocked));

    // Check 8: Verify SYS_bind fails with EPERM
    let bind_res = unsafe { libc::syscall(libc::SYS_bind, 0, std::ptr::null::<libc::c_void>(), 0) };
    let bind_blocked = bind_res == -1 && unsafe { *libc::__errno_location() } == libc::EPERM;
    results.push(format!("BIND_BLOCKED:{}", bind_blocked));

    // Check 9: Verify external IPv4 connect fails (loopback only)
    // Connecting to an external IP (e.g. 8.8.8.8) should fail in isolated netns (ENETUNREACH)
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    let mut connect_blocked = false;
    if sock >= 0 {
        let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sa.sin_family = libc::AF_INET as libc::sa_family_t;
        sa.sin_port = 80u16.to_be();
        sa.sin_addr.s_addr = 0x08080808u32.to_be(); // 8.8.8.8
        let cret = unsafe {
            libc::connect(
                sock,
                &sa as *const libc::sockaddr_in as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if cret != 0 {
            connect_blocked = true;
        }
        unsafe { libc::close(sock) };
    }
    results.push(format!("EXTERNAL_NET_BLOCKED:{}", connect_blocked));

    // Send results report to Node Host
    let report = format!("VERIFICATION_REPORT:{}\n", results.join(","));
    let _ = control_stream.write_all(report.as_bytes());
    let _ = control_stream.write_all(b"ENGINE_READY\n");
    let _ = control_stream.flush();

    // 6. Enter wait loop: keep liveness read end open and monitor for EOF or exit command
    let liveness_fd: i32 = env::var("LOTUS_LIVENESS_FD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(-1);

    let mut poll_fds = [
        libc::pollfd {
            fd: control_stream.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        },
        libc::pollfd {
            fd: liveness_fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        },
    ];

    loop {
        let count = if liveness_fd >= 0 { 2 } else { 1 };
        let ret = unsafe { libc::poll(poll_fds.as_mut_ptr(), count, 500) };
        if ret > 0 {
            // Check liveness pipe for supervisor death
            if count == 2 && (poll_fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0) {
                // Supervisor died unexpectedly!
                eprintln!("[probe] Liveness pipe EOF: supervisor terminated");
                process::exit(99);
            }

            // Check control stream
            if poll_fds[0].revents & libc::POLLIN != 0 {
                let mut cmd_buf = [0u8; 64];
                let n = match control_stream.read(&mut cmd_buf) {
                    Ok(n) if n > 0 => n,
                    _ => break,
                };
                let cmd = std::str::from_utf8(&cmd_buf[..n]).unwrap_or("");
                if cmd.contains("EXIT") || cmd.contains("SHUTDOWN") {
                    break;
                }
            } else if poll_fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                break;
            }
        }
    }

    // Forget ownership of control_stream so it does not double-close on exit
    std::mem::forget(control_stream);
    process::exit(0);
}
