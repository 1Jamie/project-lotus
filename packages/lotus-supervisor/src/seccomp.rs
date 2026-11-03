//! Supervisor-specific seccomp-bpf filter.
//!
//! In Phase II (Restricted Supervision Phase), the supervisor drops setup privileges
//! and installs this minimal filter:
//! - Blocks SYS_kill, SYS_tkill, SYS_tgkill unconditionally to prevent signaling host processes.
//! - Restricts signaling exclusively to `SYS_pidfd_send_signal` targeting `engine_pidfd` with [SIGTERM, SIGKILL, 0].
//! - Unconditionally blocks execve, fork, clone, unshare, setns, mount, socket, connect, bind, open.
//! - Allows only supervision wait-loop syscalls: read, write, writev, poll/ppoll, close, pidfd_send_signal, wait4/waitid, rt_sig*, exit.

use std::os::unix::io::RawFd;

#[repr(C)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_JGE: u16 = 0x30;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

const SECCOMP_RET_ALLOW: u32 = 0x7fff0000;
const SECCOMP_RET_ERRNO: u32 = 0x00050000;

const AUDIT_ARCH_X86_64: u32 = 0xc000003e;
#[allow(dead_code)]
const AUDIT_ARCH_AARCH64: u32 = 0xc00000b7;

const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
const PR_SET_SECCOMP: libc::c_int = 22;
const SECCOMP_MODE_FILTER: libc::c_ulong = 2;

// Offset of fields in seccomp_data:
const SECCOMP_DATA_NR_OFFSET: u32 = 0;
const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;
const SECCOMP_DATA_ARG0_LO_OFFSET: u32 = 16;
const SECCOMP_DATA_ARG0_HI_OFFSET: u32 = 20;
const SECCOMP_DATA_ARG1_LO_OFFSET: u32 = 24;
const SECCOMP_DATA_ARG2_LO_OFFSET: u32 = 32;
const SECCOMP_DATA_ARG2_HI_OFFSET: u32 = 36;
const SECCOMP_DATA_ARG3_LO_OFFSET: u32 = 40;
const SECCOMP_DATA_ARG3_HI_OFFSET: u32 = 44;

/// Install supervisor seccomp-bpf filter restricting signaling strictly to `engine_pidfd`.
pub fn apply_supervisor_seccomp(engine_pidfd: RawFd) -> Result<(), i32> {
    // 1. Mandatory PR_SET_NO_NEW_PRIVS before installing unprivileged seccomp filter
    if unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(unsafe { *libc::__errno_location() });
    }

    #[cfg(target_arch = "x86_64")]
    let expected_arch = AUDIT_ARCH_X86_64;
    #[cfg(target_arch = "aarch64")]
    let expected_arch = AUDIT_ARCH_AARCH64;

    let eperm = SECCOMP_RET_ERRNO | (libc::EPERM as u32);
    let allow = SECCOMP_RET_ALLOW;

    let mut filter: Vec<SockFilter> = Vec::with_capacity(64);

    // Arch check
    filter.push(SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: SECCOMP_DATA_ARCH_OFFSET });
    filter.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 1, jf: 0, k: expected_arch });
    filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: eperm });

    // x32 check
    filter.push(SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: SECCOMP_DATA_NR_OFFSET });
    filter.push(SockFilter { code: BPF_JMP | BPF_JGE | BPF_K, jt: 0, jf: 1, k: 0x40000000 });
    filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: eperm });

    // Allowed supervision wait-loop syscalls
    let allowed_syscalls = [
        libc::SYS_read as u32,
        libc::SYS_write as u32,
        libc::SYS_writev as u32,
        libc::SYS_poll as u32,
        libc::SYS_ppoll as u32,
        libc::SYS_close as u32,
        libc::SYS_wait4 as u32,
        libc::SYS_waitid as u32,
        libc::SYS_clock_gettime as u32,
        libc::SYS_clock_nanosleep as u32,
        libc::SYS_nanosleep as u32,
        libc::SYS_rt_sigaction as u32,
        libc::SYS_rt_sigprocmask as u32,
        libc::SYS_rt_sigreturn as u32,
        libc::SYS_sigaltstack as u32,
        libc::SYS_restart_syscall as u32,
        libc::SYS_futex as u32,
        libc::SYS_exit as u32,
        libc::SYS_exit_group as u32,
    ];

    for &sc in &allowed_syscalls {
        filter.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 0, jf: 1, k: sc });
        filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: allow });
    }

    // Check if syscall is SYS_pidfd_send_signal
    filter.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 1, jf: 0, k: libc::SYS_pidfd_send_signal as u32 });
    filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: eperm });

    // Inspect arg0_lo (fd must match engine_pidfd)
    filter.push(SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: SECCOMP_DATA_ARG0_LO_OFFSET });
    filter.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 1, jf: 0, k: engine_pidfd as u32 });
    filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: eperm });

    // Inspect arg0_hi (must be 0)
    filter.push(SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: SECCOMP_DATA_ARG0_HI_OFFSET });
    filter.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 1, jf: 0, k: 0 });
    filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: eperm });

    // Inspect arg1_lo (signal): must be SIGTERM, SIGKILL, or 0
    filter.push(SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: SECCOMP_DATA_ARG1_LO_OFFSET });
    filter.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 3, jf: 0, k: libc::SIGTERM as u32 });
    filter.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 2, jf: 0, k: libc::SIGKILL as u32 });
    filter.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 1, jf: 0, k: 0 });
    filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: eperm });

    // Inspect arg2 (siginfo_t pointer must be NULL)
    filter.push(SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: SECCOMP_DATA_ARG2_LO_OFFSET });
    filter.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 1, jf: 0, k: 0 });
    filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: eperm });
    filter.push(SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: SECCOMP_DATA_ARG2_HI_OFFSET });
    filter.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 1, jf: 0, k: 0 });
    filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: eperm });

    // Inspect arg3 (flags must be 0)
    filter.push(SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: SECCOMP_DATA_ARG3_LO_OFFSET });
    filter.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 1, jf: 0, k: 0 });
    filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: eperm });
    filter.push(SockFilter { code: BPF_LD | BPF_W | BPF_ABS, jt: 0, jf: 0, k: SECCOMP_DATA_ARG3_HI_OFFSET });
    filter.push(SockFilter { code: BPF_JMP | BPF_JEQ | BPF_K, jt: 1, jf: 0, k: 0 });
    filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: eperm });

    // Allowed pidfd_send_signal
    filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: allow });

    // Default deny
    filter.push(SockFilter { code: BPF_RET | BPF_K, jt: 0, jf: 0, k: eperm });

    let prog = SockFprog {
        len: filter.len() as u16,
        filter: filter.as_ptr(),
    };

    if unsafe { libc::prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog as *const SockFprog as libc::c_ulong, 0, 0) } != 0 {
        return Err(unsafe { *libc::__errno_location() });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_in_fork<F: FnOnce()>(f: F) {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            f();
            unsafe { libc::_exit(0) };
        } else {
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            if libc::WIFSIGNALED(status) {
                panic!("Child killed by signal {}", libc::WTERMSIG(status));
            }
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
        }
    }

    #[test]
    fn test_supervisor_seccomp_policy() {
        run_in_fork(|| {
            let self_pid = unsafe { libc::getpid() };
            let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, self_pid, 0) } as RawFd;
            assert!(pidfd >= 0);

            // Install supervisor seccomp filter restricted to pidfd
            assert_eq!(apply_supervisor_seccomp(pidfd), Ok(()));

            // 1. SYS_kill must be unconditionally BLOCKED with EPERM
            let kill_ret = unsafe { libc::kill(self_pid, 0) };
            assert_eq!(kill_ret, -1);
            assert_eq!(unsafe { *libc::__errno_location() }, libc::EPERM);

            // 2. SYS_pidfd_send_signal targeting pidfd with signal 0 must SUCCEED
            let sig_ret = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd, 0, std::ptr::null::<()>(), 0) };
            assert_eq!(sig_ret, 0);

            // 3. SYS_pidfd_send_signal targeting wrong descriptor must be BLOCKED with EPERM
            let wrong_fd = pidfd + 99;
            let wrong_ret = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, wrong_fd, 0, std::ptr::null::<()>(), 0) };
            assert_eq!(wrong_ret, -1);
            assert_eq!(unsafe { *libc::__errno_location() }, libc::EPERM);

            // 4. SYS_pidfd_send_signal with unauthorized signal (e.g. SIGHUP = 1) must be BLOCKED with EPERM
            let sighup_ret = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd, libc::SIGHUP, std::ptr::null::<()>(), 0) };
            assert_eq!(sighup_ret, -1);
            assert_eq!(unsafe { *libc::__errno_location() }, libc::EPERM);

            // 5. SYS_pidfd_send_signal with non-NULL siginfo (arg2 != NULL) must be BLOCKED with EPERM
            let dummy_val = 1234usize;
            let info_ret = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd, 0, &dummy_val as *const _ as *const libc::c_void, 0) };
            assert_eq!(info_ret, -1);
            assert_eq!(unsafe { *libc::__errno_location() }, libc::EPERM);

            // 6. SYS_pidfd_send_signal with non-zero flags (arg3 != 0) must be BLOCKED with EPERM
            let flags_ret = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd, 0, std::ptr::null::<()>(), 1) };
            assert_eq!(flags_ret, -1);
            assert_eq!(unsafe { *libc::__errno_location() }, libc::EPERM);

            unsafe { libc::close(pidfd) };
        });
    }
}
