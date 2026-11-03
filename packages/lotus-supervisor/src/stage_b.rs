//! Stage B Seccomp-BPF Policy for lotus-engine and lotus-sandbox-probe.
//!
//! Executed at line 1 of engine main before threads/runtimes are initialized.
//! - Verifies AUDIT_ARCH_X86_64, blocks x32 ABI.
//! - SYS_clone3 returns ENOSYS to force fallback to SYS_clone.
//! - Enforces thread-only SYS_clone (CLONE_THREAD required, CSIGNAL == 0).
//! - Blocks subprocess creation (fork, vfork, execve, execveat).
//! - Blocks container/namespace escapes (unshare, setns, mount, umount2, pivot_root, chroot).
//! - Blocks cross-process inspection/injection (ptrace, process_vm_*, pidfd_*).
//! - Blocks SYS_bind and SYS_listen unconditionally; restricts SYS_socket to AF_INET SOCK_STREAM.
//! - Allows all other standard application syscalls.

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
const BPF_ALU: u16 = 0x04;
const BPF_AND: u16 = 0x50;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_JGE: u16 = 0x30;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

const SECCOMP_RET_ALLOW: u32 = 0x7fff0000;
const SECCOMP_RET_ERRNO: u32 = 0x00050000;

const AUDIT_ARCH_X86_64: u32 = 0xc000003e;

const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
const PR_SET_SECCOMP: libc::c_int = 22;
const SECCOMP_MODE_FILTER: libc::c_ulong = 2;

const SECCOMP_DATA_NR_OFFSET: u32 = 0;
const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;
const SECCOMP_DATA_ARG0_LO_OFFSET: u32 = 16;
const SECCOMP_DATA_ARG1_LO_OFFSET: u32 = 24;

const CLONE_THREAD: u32 = 0x00010000;

/// Apply the Stage B seccomp filter to the current process.
pub fn apply_stage_b_seccomp() -> Result<(), i32> {
    if unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(unsafe { *libc::__errno_location() });
    }

    #[cfg(target_arch = "x86_64")]
    {
        let mut filter: Vec<SockFilter> = Vec::with_capacity(128);

        // 1. Verify Architecture
        filter.push(SockFilter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: SECCOMP_DATA_ARCH_OFFSET,
        });
        filter.push(SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 1,
            jf: 0,
            k: AUDIT_ARCH_X86_64,
        });
        filter.push(SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | (libc::ENOSYS as u32),
        });

        // 2. Load Syscall Number
        filter.push(SockFilter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: SECCOMP_DATA_NR_OFFSET,
        });

        // 3. Reject x32 ABI (nr >= 0x40000000)
        filter.push(SockFilter {
            code: BPF_JMP | BPF_JGE | BPF_K,
            jt: 0,
            jf: 1,
            k: 0x40000000,
        });
        filter.push(SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | (libc::ENOSYS as u32),
        });

        // 4. SYS_clone3 returns ENOSYS to force glibc fallback to SYS_clone
        filter.push(SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 0,
            jf: 1,
            k: libc::SYS_clone3 as u32,
        });
        filter.push(SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | (libc::ENOSYS as u32),
        });

        // 5. Syscalls unconditionally blocked with EPERM
        let denied_syscalls = [
            // Process creation / replacement
            libc::SYS_fork as u32,
            libc::SYS_vfork as u32,
            libc::SYS_execve as u32,
            libc::SYS_execveat as u32,
            // Namespace / Container escapes
            libc::SYS_unshare as u32,
            libc::SYS_setns as u32,
            libc::SYS_mount as u32,
            libc::SYS_umount2 as u32,
            libc::SYS_pivot_root as u32,
            libc::SYS_chroot as u32,
            // Cross-process inspection / injection
            libc::SYS_ptrace as u32,
            libc::SYS_process_vm_readv as u32,
            libc::SYS_process_vm_writev as u32,
            libc::SYS_pidfd_open as u32,
            libc::SYS_pidfd_getfd as u32,
            libc::SYS_pidfd_send_signal as u32,
            // Network binding / listening
            libc::SYS_bind as u32,
            libc::SYS_listen as u32,
        ];

        for &nr in &denied_syscalls {
            filter.push(SockFilter {
                code: BPF_JMP | BPF_JEQ | BPF_K,
                jt: 0,
                jf: 1,
                k: nr,
            });
            filter.push(SockFilter {
                code: BPF_RET | BPF_K,
                jt: 0,
                jf: 0,
                k: SECCOMP_RET_ERRNO | (libc::EPERM as u32),
            });
        }

        // 6. Inspect SYS_clone: Must have CLONE_THREAD and CSIGNAL == 0
        // Jump past clone check if syscall != SYS_clone
        let clone_check_len = 10;
        filter.push(SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 0,
            jf: clone_check_len,
            k: libc::SYS_clone as u32,
        });

        // Check CSIGNAL (flags & 0xFF must be 0)
        filter.push(SockFilter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: SECCOMP_DATA_ARG0_LO_OFFSET,
        });
        filter.push(SockFilter {
            code: BPF_ALU | BPF_AND | BPF_K,
            jt: 0,
            jf: 0,
            k: 0xFF,
        });
        filter.push(SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 1,
            jf: 0,
            k: 0,
        });
        filter.push(SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | (libc::EPERM as u32),
        });

        // Check CLONE_THREAD (flags & CLONE_THREAD must be non-zero)
        filter.push(SockFilter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: SECCOMP_DATA_ARG0_LO_OFFSET,
        });
        filter.push(SockFilter {
            code: BPF_ALU | BPF_AND | BPF_K,
            jt: 0,
            jf: 0,
            k: CLONE_THREAD,
        });
        filter.push(SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 1,
            jf: 0,
            k: CLONE_THREAD,
        });
        filter.push(SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | (libc::EPERM as u32),
        });
        // Allowed clone
        filter.push(SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ALLOW,
        });

        // Reload syscall nr for subsequent checks
        filter.push(SockFilter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: SECCOMP_DATA_NR_OFFSET,
        });

        // 7. Inspect SYS_socket: domain must be AF_INET (2), type must be SOCK_STREAM (1)
        let sock_check_len = 8;
        filter.push(SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 0,
            jf: sock_check_len,
            k: libc::SYS_socket as u32,
        });

        // Check domain == AF_INET (2)
        filter.push(SockFilter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: SECCOMP_DATA_ARG0_LO_OFFSET,
        });
        filter.push(SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 1,
            jf: 0,
            k: libc::AF_INET as u32,
        });
        filter.push(SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | (libc::EPERM as u32),
        });

        // Check type & 0xFF == SOCK_STREAM (1)
        filter.push(SockFilter {
            code: BPF_LD | BPF_W | BPF_ABS,
            jt: 0,
            jf: 0,
            k: SECCOMP_DATA_ARG1_LO_OFFSET,
        });
        filter.push(SockFilter {
            code: BPF_ALU | BPF_AND | BPF_K,
            jt: 0,
            jf: 0,
            k: 0xFF,
        });
        filter.push(SockFilter {
            code: BPF_JMP | BPF_JEQ | BPF_K,
            jt: 1,
            jf: 0,
            k: libc::SOCK_STREAM as u32,
        });
        filter.push(SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ERRNO | (libc::EPERM as u32),
        });
        // Allowed socket
        filter.push(SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ALLOW,
        });

        // 8. Default: Allow all other standard syscalls
        filter.push(SockFilter {
            code: BPF_RET | BPF_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ALLOW,
        });

        let prog = SockFprog {
            len: filter.len() as u16,
            filter: filter.as_ptr(),
        };

        let ret = unsafe {
            libc::prctl(
                PR_SET_SECCOMP,
                SECCOMP_MODE_FILTER,
                &prog as *const SockFprog as libc::c_ulong,
                0,
                0,
            )
        };

        if ret != 0 {
            return Err(unsafe { *libc::__errno_location() });
        }

        Ok(())
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        Err(libc::ENOSYS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_stage_b_seccomp_policy() {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);

        if pid == 0 {
            // Child: apply Stage B filter
            if let Err(e) = apply_stage_b_seccomp() {
                eprintln!("Stage B apply failed: errno {}", e);
                unsafe { libc::_exit(1) };
            }

            // 1. Thread spawn must SUCCEED (tests CLONE_THREAD policy)
            let handle = thread::spawn(|| 42);
            let val = handle.join().expect("thread join");
            if val != 42 {
                unsafe { libc::_exit(2) };
            }

            // 2. SYS_clone3 must return ENOSYS
            let ret = unsafe { libc::syscall(libc::SYS_clone3, 0, 0) };
            if ret != -1 || unsafe { *libc::__errno_location() } != libc::ENOSYS {
                unsafe { libc::_exit(3) };
            }

            // 3. SYS_fork / process clone must fail with EPERM
            // Calling raw fork or clone without CLONE_THREAD
            let ret = unsafe { libc::syscall(libc::SYS_fork) };
            if ret != -1 || unsafe { *libc::__errno_location() } != libc::EPERM {
                unsafe { libc::_exit(4) };
            }

            // 4. SYS_mount must fail with EPERM
            let ret = unsafe { libc::mount(std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, std::ptr::null()) };
            if ret != -1 || unsafe { *libc::__errno_location() } != libc::EPERM {
                unsafe { libc::_exit(5) };
            }

            // 5. SYS_unshare must fail with EPERM
            let ret = unsafe { libc::syscall(libc::SYS_unshare, libc::CLONE_NEWUSER) };
            if ret != -1 || unsafe { *libc::__errno_location() } != libc::EPERM {
                unsafe { libc::_exit(6) };
            }

            // 6. SYS_bind must fail with EPERM
            let ret = unsafe { libc::syscall(libc::SYS_bind, 0, std::ptr::null::<libc::c_void>(), 0) };
            if ret != -1 || unsafe { *libc::__errno_location() } != libc::EPERM {
                unsafe { libc::_exit(7) };
            }

            // 7. Non-TCP or non-IPv4 socket must fail with EPERM
            let ret = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_STREAM, 0) };
            if ret != -1 || unsafe { *libc::__errno_location() } != libc::EPERM {
                unsafe { libc::_exit(8) };
            }

            let ret = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
            if ret != -1 || unsafe { *libc::__errno_location() } != libc::EPERM {
                unsafe { libc::_exit(9) };
            }

            // 8. AF_INET SOCK_STREAM socket creation must SUCCEED (or return valid error like EAFNOSUPPORT / EMFILE if restricted by netns, but NOT EPERM from seccomp)
            let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
            if sock >= 0 {
                unsafe { libc::close(sock) };
            } else if unsafe { *libc::__errno_location() } == libc::EPERM {
                unsafe { libc::_exit(10) };
            }

            unsafe { libc::_exit(0) };
        } else {
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
            assert!(
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                "Child failed Stage B assertion: exit status = {}",
                libc::WEXITSTATUS(status)
            );
        }
    }
}
