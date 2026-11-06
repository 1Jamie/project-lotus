//! Adversarial check: with Stage B applied, can a holder of the read-only
//! descriptor obtain a writable shared mapping? Every attempt must fail.
use lotus_supervisor::shm::SealedMemfdPlane;
use lotus_supervisor::stage_b::apply_stage_b_seccomp;

fn errno() -> i32 { unsafe { *libc::__errno_location() } }

fn main() {
    let plane = SealedMemfdPlane::new("ro_escalation", 4096).expect("plane");
    let ro = plane.ro_fd;
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        apply_stage_b_seccomp().expect("stage b");
        let mut fails = 0;
        // 1. Direct writable mmap of the RO description.
        let m = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, ro, 0) };
        println!("mmap(RW, ro_fd): {}", if m == libc::MAP_FAILED { format!("denied errno={}", errno()) } else { fails += 1; "ALLOWED".into() });
        // 2. mprotect a read-only shared mapping up to write.
        let r = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ, libc::MAP_SHARED, ro, 0) };
        let p = unsafe { libc::mprotect(r, 4096, libc::PROT_READ | libc::PROT_WRITE) };
        println!("mprotect(RW) on RO map: {}", if p != 0 { format!("denied errno={}", errno()) } else { fails += 1; "ALLOWED".into() });
        // 3. Reopen via /proc/self/fd as O_RDWR.
        let path = std::ffi::CString::new(format!("/proc/self/fd/{}", ro)).unwrap();
        let o = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
        println!("open(/proc/self/fd/ro, O_RDWR): {}", if o < 0 { format!("denied errno={}", errno()) } else { fails += 1; "ALLOWED".into() });
        // 4. pwrite / write through the RO fd.
        let w = unsafe { libc::pwrite(ro, b"X".as_ptr() as *const _, 1, 0) };
        println!("pwrite(ro_fd): {}", if w < 0 { format!("denied errno={}", errno()) } else { fails += 1; "ALLOWED".into() });
        // 5. fchmod must be killed by seccomp (run last: SIGSYS terminates).
        println!("fchmod(ro_fd, 0600): expecting SIGSYS");
        unsafe { libc::syscall(libc::SYS_fchmod, ro, 0o600) };
        fails += 1;
        println!("fchmod: ALLOWED");
        unsafe { libc::_exit(fails) };
    }
    let mut st = 0;
    unsafe { libc::waitpid(pid, &mut st, 0) };
    if libc::WIFSIGNALED(st) {
        println!("child terminated by signal {} (SIGSYS={})", libc::WTERMSIG(st), libc::SIGSYS);
    } else {
        println!("child exited {} (non-zero = escalation succeeded)", libc::WEXITSTATUS(st));
    }
}

