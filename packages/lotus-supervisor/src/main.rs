//! lotus-supervisor: Dedicated Long-Lived Supervisor Process for Lotus GUI
//!
//! Operates as a minimal process-management component of the TCB.
//! Free of async runtimes (no tokio, no thread pools), GUI, GPU, or Servo dependencies.

use lotus_supervisor::caps;
use lotus_supervisor::fd;
use lotus_supervisor::landlock;
use lotus_supervisor::ns;
use lotus_supervisor::seccomp;

use std::env;
use std::ffi::CString;
use std::fs;
use std::process;

/// Assert that the supervisor is strictly single-threaded prior to any fork operation.
pub fn assert_single_threaded() {
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("Threads:") {
                let count = line.split_whitespace().nth(1).unwrap_or("0");
                if count != "1" {
                    eprintln!("[lotus-supervisor] Fatal: Expected 1 thread, found {}", count);
                    process::abort();
                }
                return;
            }
        }
    }
}

pub fn pidfd_send_signal(pidfd: i32, sig: libc::c_int) -> i32 {
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            sig,
            std::ptr::null::<libc::c_void>(),
            0,
        ) as i32
    }
}

struct SupervisorConfig {
    supervisor_fd: i32,
    control_fd: i32,
    engine_bin: String,
    engine_args: Vec<String>,
    cache_dir: Option<String>,
    temp_dir: Option<String>,
    pass_fds: Vec<i32>,
}

fn parse_args() -> Result<SupervisorConfig, String> {
    let args: Vec<String> = env::args().collect();
    let mut supervisor_fd: Option<i32> = env::var("LOTUS_SUPERVISOR_FD")
        .ok()
        .and_then(|v| v.parse().ok());
    let mut control_fd: Option<i32> = env::var("LOTUS_CONTROL_FD")
        .ok()
        .and_then(|v| v.parse().ok());
    let mut engine_bin: Option<String> = env::var("LOTUS_ENGINE_BIN").ok();
    let mut cache_dir: Option<String> = env::var("LOTUS_CACHE_DIR").ok();
    let mut temp_dir: Option<String> = env::var("LOTUS_TEMP_DIR").ok();
    let mut pass_fds: Vec<i32> = Vec::new();
    let mut engine_args = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--supervisor-fd" => {
                i += 1;
                if i < args.len() {
                    supervisor_fd = args[i].parse().ok();
                }
            }
            "--control-fd" => {
                i += 1;
                if i < args.len() {
                    control_fd = args[i].parse().ok();
                }
            }
            "--engine-bin" => {
                i += 1;
                if i < args.len() {
                    engine_bin = Some(args[i].clone());
                }
            }
            "--cache-dir" => {
                i += 1;
                if i < args.len() {
                    cache_dir = Some(args[i].clone());
                }
            }
            "--temp-dir" => {
                i += 1;
                if i < args.len() {
                    temp_dir = Some(args[i].clone());
                }
            }
            "--pass-fds" => {
                i += 1;
                if i < args.len() {
                    for part in args[i].split(',') {
                        if let Ok(fd) = part.trim().parse::<i32>() {
                            pass_fds.push(fd);
                        }
                    }
                }
            }
            "--" => {
                i += 1;
                while i < args.len() {
                    engine_args.push(args[i].clone());
                    i += 1;
                }
                break;
            }
            other => {
                if engine_bin.is_none() && !other.starts_with('-') {
                    engine_bin = Some(other.to_string());
                } else {
                    engine_args.push(other.to_string());
                }
            }
        }
        i += 1;
    }

    let supervisor_fd = supervisor_fd.ok_or("Missing --supervisor-fd or LOTUS_SUPERVISOR_FD")?;
    let control_fd = control_fd.ok_or("Missing --control-fd or LOTUS_CONTROL_FD")?;
    let engine_bin = engine_bin.ok_or("Missing --engine-bin or LOTUS_ENGINE_BIN")?;

    Ok(SupervisorConfig {
        supervisor_fd,
        control_fd,
        engine_bin,
        engine_args,
        cache_dir,
        temp_dir,
        pass_fds,
    })
}

fn main() {
    assert_single_threaded();

    let config = match parse_args() {
        Ok(c) => c,
        Err(err) => {
            eprintln!("[lotus-supervisor] Usage error: {}", err);
            process::exit(1);
        }
    };

    // 1. Prepare Landlock descriptors BEFORE unsharing/forking
    let mut landlock_fds = Vec::with_capacity(32);
    let ro_system_paths = [
        "/usr",
        "/lib",
        "/lib64",
        "/etc/fonts",
        "/etc/ld.so.cache",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
        // System CA certificates: Required by Servo ResourceManager TLS connector initialization
        // (Panics with "No CA certificates were loaded from the system" if omitted; recorded exception in plan §2.3)
        "/etc/ssl",
        "/etc/pki",
    ];
    for p in &ro_system_paths {
        if let Ok(d) = landlock::open_landlock_dir(p, false) {
            landlock_fds.push(d);
        }
    }
    // Allow engine binary directory
    if let Some(parent) = std::path::Path::new(&config.engine_bin).parent() {
        if let Some(parent_str) = parent.to_str() {
            if let Ok(d) = landlock::open_landlock_dir(parent_str, false) {
                landlock_fds.push(d);
            }
        }
    }
    // Allow current working directory (for assets/resources)
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(cwd_str) = cwd.to_str() {
            if let Ok(d) = landlock::open_landlock_dir(cwd_str, false) {
                landlock_fds.push(d);
            }
        }
    }
    // Allow explicit resources path
    if let Ok(res_path) = env::var("LOTUS_RESOURCES_PATH") {
        if let Ok(d) = landlock::open_landlock_dir(&res_path, false) {
            landlock_fds.push(d);
        }
    }
    if let Some(ref cache) = config.cache_dir {
        let _ = fs::create_dir_all(cache);
        if let Ok(d) = landlock::open_landlock_dir(cache, true) {
            landlock_fds.push(d);
        }
    }
    // Allow /tmp for temporary files (or explicit temp_dir)
    let temp_path = config.temp_dir.as_deref().unwrap_or("/tmp");
    let _ = fs::create_dir_all(temp_path);
    if let Ok(d) = landlock::open_landlock_dir(temp_path, true) {
        landlock_fds.push(d);
    }

    // 2. Create coordination pipes
    let mut map_pipe = [0i32; 2];
    if unsafe { libc::pipe2(map_pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        eprintln!("[lotus-supervisor] Failed to create map pipe");
        process::abort();
    }
    let map_read_fd = map_pipe[0];
    let map_write_fd = map_pipe[1];

    let mut port_pipe = [0i32; 2];
    if unsafe { libc::pipe2(port_pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        eprintln!("[lotus-supervisor] Failed to create port pipe");
        process::abort();
    }
    let port_read_fd = port_pipe[0];
    let port_write_fd = port_pipe[1];

    let mut liveness_pipe = [0i32; 2];
    if unsafe { libc::pipe2(liveness_pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        eprintln!("[lotus-supervisor] Failed to create liveness pipe");
        process::abort();
    }
    let liveness_read_fd = liveness_pipe[0];
    let liveness_write_fd = liveness_pipe[1];

    // 3. Pre-allocate all CStrings and raw pointers before clone (zero allocations after clone)
    let engine_bin_c = CString::new(config.engine_bin.as_bytes()).unwrap();
    let mut args_c = Vec::new();
    args_c.push(engine_bin_c.clone());
    for arg in &config.engine_args {
        args_c.push(CString::new(arg.as_bytes()).unwrap());
    }
    let args_ptrs: Vec<*const libc::c_char> = args_c
        .iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    let mut envs_c = Vec::new();
    envs_c.push(CString::new(format!("LOTUS_CONTROL_FD={}", config.control_fd)).unwrap());
    envs_c.push(CString::new(format!("LOTUS_LIVENESS_FD={}", liveness_read_fd)).unwrap());
    for (k, v) in env::vars() {
        if k != "LOTUS_CONTROL_FD"
            && k != "LOTUS_LIVENESS_FD"
            && k != "LOTUS_IPC_LISTENER_FD"
            && k != "LOTUS_SUPERVISOR_FD"
        {
            if let Ok(c) = CString::new(format!("{}={}", k, v)) {
                envs_c.push(c);
            }
        }
    }
    let envs_ptrs: Vec<*const libc::c_char> = envs_c
        .iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    // Pre-allocate descriptor allowlists before clone (zero allocations after clone)
    landlock_fds.reserve(8);
    let mut child_bootstrap_allowlist: Vec<i32> = Vec::with_capacity(64);
    child_bootstrap_allowlist.push(0);
    child_bootstrap_allowlist.push(1);
    child_bootstrap_allowlist.push(2);
    child_bootstrap_allowlist.push(config.control_fd);
    child_bootstrap_allowlist.push(liveness_read_fd);
    for &pfd in &config.pass_fds {
        child_bootstrap_allowlist.push(pfd);
    }
    for d in &landlock_fds {
        child_bootstrap_allowlist.push(d.fd);
    }

    let mut child_final_allowlist: Vec<i32> = Vec::with_capacity(64);
    child_final_allowlist.push(0);
    child_final_allowlist.push(1);
    child_final_allowlist.push(2);
    child_final_allowlist.push(config.control_fd);
    child_final_allowlist.push(liveness_read_fd);
    for &pfd in &config.pass_fds {
        child_final_allowlist.push(pfd);
    }

    // 4. Verify single-threaded immediately before clone
    assert_single_threaded();

    // 5. Clone child into new user & PID namespace
    let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWPID | libc::SIGCHLD;
    let pid = unsafe {
        libc::syscall(
            libc::SYS_clone,
            flags as libc::c_ulong,
            0 as *mut libc::c_void,
            0 as *mut libc::c_int,
            0 as *mut libc::c_int,
            0 as libc::c_ulong,
        ) as i32
    };

    if pid < 0 {
        eprintln!("[lotus-supervisor] Clone failed: {}", unsafe { *libc::__errno_location() });
        process::abort();
    }

    if pid == 0 {
        // =========================================================================
        // CHILD PROCESS (PID 1 in child PID & user namespace) - STAGE A LOCKDOWN
        // ZERO heap allocations permitted here!
        // =========================================================================

        // Close supervisor-side descriptors
        unsafe {
            libc::close(map_write_fd);
            libc::close(port_read_fd);
            libc::close(config.supervisor_fd);
            libc::close(liveness_write_fd);
        }

        // Wait for parent to write UID/GID mappings
        let mut sync_buf = [0u8; 1];
        let n = unsafe { libc::read(map_read_fd, sync_buf.as_mut_ptr() as *mut _, 1) };
        unsafe { libc::close(map_read_fd) };
        if n != 1 {
            unsafe { libc::_exit(101) };
        }

        // Child is now mapped to container root (UID 0) with user namespace capabilities!
        // Unshare network, mount, and IPC namespaces
        if ns::setup_child_isolated_namespaces().is_err() {
            unsafe { libc::_exit(102) };
        }

        // Bring up loopback interface in child netns
        if ns::bring_up_loopback().is_err() {
            unsafe { libc::_exit(103) };
        }

        // Bind internal IPC listener
        let (listener_fd, bound_port) = match ns::bind_netns_listener() {
            Ok(res) => res,
            Err(_) => unsafe { libc::_exit(104) },
        };

        // Send bound port to supervisor via port pipe
        let port_bytes = bound_port.to_ne_bytes();
        let wn = unsafe { libc::write(port_write_fd, port_bytes.as_ptr() as *const _, 2) };
        unsafe { libc::close(port_write_fd) };
        if wn != 2 {
            unsafe { libc::_exit(105) };
        }

        // Establish parent-death signal
        unsafe {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                libc::_exit(106);
            }
        }

        // Immediate poll check on liveness pipe (detect race if supervisor died before prctl)
        let mut pfd = libc::pollfd {
            fd: liveness_read_fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };
        let pret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if pret > 0 && (pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0) {
            unsafe { libc::_exit(107); }
        }

        // Mount fresh /proc (hidepid=2, fallback hidepid=0)
        let target = b"/proc\0".as_ptr() as *const libc::c_char;
        let fstype = b"proc\0".as_ptr() as *const libc::c_char;
        let mflags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        let opts = b"hidepid=2\0".as_ptr() as *const libc::c_void;
        let mret = unsafe { libc::mount(fstype, target, fstype, mflags, opts) };
        if mret != 0 {
            let opts_fallback = b"\0".as_ptr() as *const libc::c_void;
            let mret_fallback = unsafe { libc::mount(fstype, target, fstype, mflags, opts_fallback) };
            if mret_fallback != 0 {
                unsafe { libc::_exit(108); }
            }
        }

        // Open newly mounted /proc for Landlock
        if let Ok(d) = landlock::open_landlock_dir("/proc", false) {
            child_bootstrap_allowlist.push(d.fd);
            landlock_fds.push(d);
        }

        // Clear FD_CLOEXEC on runtime descriptors so they survive execve
        unsafe {
            libc::fcntl(config.control_fd, libc::F_SETFD, 0);
            libc::fcntl(liveness_read_fd, libc::F_SETFD, 0);
            libc::fcntl(listener_fd, libc::F_SETFD, 0);
            for &pfd in &config.pass_fds {
                libc::fcntl(pfd, libc::F_SETFD, 0);
            }
        }

        // Drop all capabilities
        caps::drop_all_capabilities();

        // Lock down privileges
        caps::set_no_new_privs();

        // Finalize bootstrap allowlist (pre-allocated in parent, zero heap allocations)
        child_bootstrap_allowlist.push(listener_fd);
        child_bootstrap_allowlist.sort_unstable();
        child_bootstrap_allowlist.dedup();

        // Close unlisted descriptors against bootstrap allowlist
        fd::sanitize_descriptors_fail_closed(&child_bootstrap_allowlist);

        // Apply Landlock ruleset and close Landlock descriptors (ABI >= 4 with network port containment)
        landlock::apply_landlock_and_close_descriptors(&landlock_fds, Some(bound_port));

        // Finalize final allowlist and verify descriptor table matches strictly before execve
        child_final_allowlist.push(listener_fd);
        child_final_allowlist.sort_unstable();
        child_final_allowlist.dedup();
        fd::sanitize_descriptors_fail_closed(&child_final_allowlist);
        fd::verify_exact_descriptor_table(&child_final_allowlist);

        // Execute engine binary
        unsafe {
            libc::execve(
                engine_bin_c.as_ptr(),
                args_ptrs.as_ptr(),
                envs_ptrs.as_ptr(),
            );
            let err = *libc::__errno_location();
            eprintln!("[lotus-supervisor] execve failed with errno {}", err);
            libc::_exit(109);
        }
    }

    // =========================================================================
    // PARENT PROCESS (lotus-supervisor) - RESTRICTED SUPERVISION PHASE
    // =========================================================================
    let engine_host_pid = pid;

    // Close child-only descriptors
    unsafe {
        libc::close(map_read_fd);
        libc::close(port_write_fd);
        libc::close(config.control_fd);
        libc::close(liveness_read_fd);
        for &pfd in &config.pass_fds {
            libc::close(pfd);
        }
        for d in landlock_fds {
            libc::close(d.fd);
        }
    }

    // Configure user namespace for child (parent is in parent userns, so this succeeds unprivileged!)
    if let Err(e) = ns::configure_child_user_namespace(engine_host_pid) {
        eprintln!("[lotus-supervisor] Failed to configure child user namespace: {}", e);
        process::abort();
    }

    // Notify child that user namespace configuration is complete
    let w_map = unsafe { libc::write(map_write_fd, b"1".as_ptr() as *const _, 1) };
    unsafe { libc::close(map_write_fd) };
    if w_map != 1 {
        eprintln!("[lotus-supervisor] Failed to write map sync byte");
        process::abort();
    }

    // Read bound port from child
    let mut port_bytes = [0u8; 2];
    let rn = unsafe { libc::read(port_read_fd, port_bytes.as_mut_ptr() as *mut _, 2) };
    unsafe { libc::close(port_read_fd) };
    if rn != 2 {
        eprintln!("[lotus-supervisor] Failed to read port from child");
        process::abort();
    }
    let engine_port = u16::from_ne_bytes(port_bytes);

    // Open pidfd for engine child
    let engine_pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, engine_host_pid, 0) as i32 };
    if engine_pidfd < 0 {
        eprintln!("[lotus-supervisor] Failed to open engine pidfd");
        process::abort();
    }

    // Inform Node Host about spawned engine PID and network port
    let spawn_msg = format!(
        "{{\"type\":\"SPAWNED\",\"engine_pid\":{},\"engine_port\":{}}}\n",
        engine_host_pid, engine_port
    );
    let wret = unsafe {
        libc::write(
            config.supervisor_fd,
            spawn_msg.as_ptr() as *const _,
            spawn_msg.len(),
        )
    };
    if wret < 0 {
        eprintln!("[lotus-supervisor] Failed to write SPAWNED to supervisor socket");
        process::abort();
    }

    // Sanitize descriptor table for Restricted Supervision Phase
    let mut sup_allowlist = [
        0,
        1,
        2,
        config.supervisor_fd,
        liveness_write_fd,
        engine_pidfd,
    ];
    sup_allowlist.sort_unstable();

    fd::sanitize_descriptors_fail_closed(&sup_allowlist);
    fd::verify_exact_descriptor_table(&sup_allowlist);

    // Drop all capabilities
    caps::drop_all_capabilities();

    // Set PR_SET_NO_NEW_PRIVS
    caps::set_no_new_privs();

    // Apply supervisor Seccomp BPF filter
    if let Err(e) = seccomp::apply_supervisor_seccomp(engine_pidfd) {
        eprintln!("[lotus-supervisor] Failed to apply seccomp filter: {}", e);
        process::abort();
    }

    // Enter restricted supervision loop
    run_supervision_loop(
        config.supervisor_fd,
        liveness_write_fd,
        engine_host_pid,
        engine_pidfd,
    );
}

/// Long-lived supervision wait loop under Seccomp restriction.
fn run_supervision_loop(
    supervisor_fd: i32,
    liveness_write_fd: i32,
    engine_host_pid: i32,
    engine_pidfd: i32,
) {
    let mut buf = [0u8; 512];

    loop {
        // Poll supervisor_fd for commands or EOF
        let mut pfd = libc::pollfd {
            fd: supervisor_fd,
            events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
            revents: 0,
        };

        // Poll with 100ms timeout so we can periodically check child status
        let poll_ret = unsafe { libc::poll(&mut pfd, 1, 100) };

        // Check if child exited spontaneously
        let mut status: libc::c_int = 0;
        let wait_ret = unsafe { libc::waitpid(engine_host_pid, &mut status, libc::WNOHANG) };
        if wait_ret == engine_host_pid {
            // Child exited! Close liveness write end
            unsafe { libc::close(liveness_write_fd) };

            let exit_code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else if libc::WIFSIGNALED(status) {
                128 + libc::WTERMSIG(status)
            } else {
                1
            };

            let exit_msg = format!("{{\"type\":\"EXITED\",\"code\":{}}}\n", exit_code);
            unsafe {
                libc::write(
                    supervisor_fd,
                    exit_msg.as_ptr() as *const _,
                    exit_msg.len(),
                );
                libc::close(supervisor_fd);
            }
            process::exit(0);
        }

        if poll_ret > 0 {
            if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                // Node host died or closed socket -> initiate graceful shutdown
                execute_graceful_shutdown(
                    supervisor_fd,
                    liveness_write_fd,
                    engine_host_pid,
                    engine_pidfd,
                );
                return;
            }

            if pfd.revents & libc::POLLIN != 0 {
                let n = unsafe { libc::read(supervisor_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
                if n <= 0 {
                    // EOF from Node -> graceful shutdown
                    execute_graceful_shutdown(
                        supervisor_fd,
                        liveness_write_fd,
                        engine_host_pid,
                        engine_pidfd,
                    );
                    return;
                }

                // Check for shutdown command
                let text = std::str::from_utf8(&buf[..n as usize]).unwrap_or("");
                if text.contains("SHUTDOWN") || text.contains("QUIT") {
                    execute_graceful_shutdown(
                        supervisor_fd,
                        liveness_write_fd,
                        engine_host_pid,
                        engine_pidfd,
                    );
                    return;
                }
            }
        }
    }
}

/// Graceful shutdown sequence:
/// 1. Maintain liveness_write_fd open throughout.
/// 2. Send SIGTERM via pidfd_send_signal.
/// 3. Wait up to 3 seconds for exit (waitpid WNOHANG).
/// 4. If alive after 3 seconds, escalate to SIGKILL.
/// 5. Once child is reaped, close liveness_write_fd and exit.
fn execute_graceful_shutdown(
    supervisor_fd: i32,
    liveness_write_fd: i32,
    engine_host_pid: i32,
    engine_pidfd: i32,
) {
    // 1. Issue SIGTERM via pidfd
    pidfd_send_signal(engine_pidfd, libc::SIGTERM);

    let mut status: libc::c_int = 0;
    let mut reaped = false;

    // Wait up to 3000ms (3 seconds)
    for _ in 0..60 {
        let ret = unsafe { libc::waitpid(engine_host_pid, &mut status, libc::WNOHANG) };
        if ret == engine_host_pid {
            reaped = true;
            break;
        }

        // Sleep 50ms
        let req = libc::timespec {
            tv_sec: 0,
            tv_nsec: 50_000_000,
        };
        unsafe { libc::nanosleep(&req, std::ptr::null_mut()) };
    }

    if !reaped {
        // Escalate to SIGKILL
        pidfd_send_signal(engine_pidfd, libc::SIGKILL);
        unsafe { libc::waitpid(engine_host_pid, &mut status, 0) };
    }

    // 2. Only after child is fully reaped do we close liveness_write_fd
    unsafe { libc::close(liveness_write_fd) };

    let exit_code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        0
    };

    let done_msg = format!("{{\"type\":\"EXITED\",\"code\":{}}}\n", exit_code);
    unsafe {
        libc::write(
            supervisor_fd,
            done_msg.as_ptr() as *const _,
            done_msg.len(),
        );
        libc::close(supervisor_fd);
    }

    process::exit(0);
}
