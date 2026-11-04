//! Allocation-free, fail-closed file descriptor sanitization and verification.
//!
//! Post-fork code must never allocate on the heap (no malloc, no String/Vec/Box),
//! as allocator locks may have been held by other threads prior to fork.
//! This module uses raw stack buffers and direct Linux syscalls (close_range, SYS_getdents64).

use std::os::unix::io::RawFd;

/// Parse an ASCII decimal string from bytes into a RawFd without heap allocation.
pub fn parse_fd(bytes: &[u8]) -> Option<RawFd> {
    if bytes.is_empty() {
        return None;
    }
    let mut val: RawFd = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add((b - b'0') as RawFd)?;
    }
    Some(val)
}

/// Fallback for older kernels: enumerate `/proc/self/fd` using raw `SYS_getdents64` into a stack buffer
/// and close any open descriptor not present in `allowlist` (excluding the enumeration dir_fd).
///
/// Guaranteed completely allocation-free (zero heap allocations).
pub unsafe fn close_unlisted_via_proc_self_fd(allowlist: &[RawFd]) -> Result<(), i32> {
    let dir_fd = libc::open(
        b"/proc/self/fd\0".as_ptr() as *const libc::c_char,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
    );
    if dir_fd < 0 {
        return Err(*libc::__errno_location());
    }

    let mut buf = [0u8; 1024];
    loop {
        let nread = libc::syscall(libc::SYS_getdents64, dir_fd, buf.as_mut_ptr(), buf.len());
        if nread < 0 {
            let err = *libc::__errno_location();
            libc::close(dir_fd);
            return Err(err);
        }
        if nread == 0 {
            break;
        }

        let mut bpos: usize = 0;
        let nread = nread as usize;
        while bpos < nread {
            if bpos + 18 >= nread {
                libc::close(dir_fd);
                return Err(libc::EINVAL);
            }
            let d_reclen = u16::from_ne_bytes([buf[bpos + 16], buf[bpos + 17]]) as usize;
            if d_reclen == 0 || bpos + d_reclen > nread {
                libc::close(dir_fd);
                return Err(libc::EINVAL);
            }

            let name_bytes = &buf[bpos + 19..bpos + d_reclen];
            let name_len = name_bytes.iter().position(|&c| c == 0).unwrap_or(name_bytes.len());
            let name = &name_bytes[..name_len];

            if name != b"." && name != b".." {
                if let Some(fd) = parse_fd(name) {
                    if fd != dir_fd && !allowlist.contains(&fd) {
                        if libc::close(fd) != 0 {
                            libc::close(dir_fd);
                            return Err(*libc::__errno_location());
                        }
                    }
                }
            }

            bpos += d_reclen;
        }
    }

    libc::close(dir_fd);
    Ok(())
}

/// Perform fail-closed gap-closing sanitization against a sorted allowlist of file descriptors.
/// Uses `close_range(2)` where supported, falling back to stack-buffer `/proc/self/fd` enumeration on ENOSYS.
/// Fails closed (aborts process) on any unexpected syscall error.
pub fn sanitize_descriptors_fail_closed(allowlist: &[RawFd]) {
    let mut use_fallback = false;

    for i in 0..(allowlist.len().saturating_sub(1)) {
        let gap_start = allowlist[i] + 1;
        let gap_end = allowlist[i + 1] - 1;
        if gap_start <= gap_end {
            let ret = unsafe { libc::syscall(libc::SYS_close_range, gap_start as libc::c_uint, gap_end as libc::c_uint, 0 as libc::c_uint) };
            if ret != 0 {
                let err = unsafe { *libc::__errno_location() };
                if err == libc::ENOSYS {
                    use_fallback = true;
                    break;
                } else {
                    eprintln!("[lotus-supervisor] Fatal: close_range({}, {}) failed with errno {}", gap_start, gap_end, err);
                    std::process::abort();
                }
            }
        }
    }

    if !use_fallback {
        if let Some(&highest_fd) = allowlist.last() {
            if (highest_fd as u32) < u32::MAX {
                let ret = unsafe { libc::syscall(libc::SYS_close_range, (highest_fd + 1) as libc::c_uint, u32::MAX, 0 as libc::c_uint) };
                if ret != 0 {
                    let err = unsafe { *libc::__errno_location() };
                    if err == libc::ENOSYS {
                        use_fallback = true;
                    } else {
                        eprintln!("[lotus-supervisor] Fatal: close_range({}, ~0U) failed with errno {}", highest_fd + 1, err);
                        std::process::abort();
                    }
                }
            }
        }
    }

    if use_fallback {
        unsafe {
            if let Err(err) = close_unlisted_via_proc_self_fd(allowlist) {
                eprintln!("[lotus-supervisor] Fatal: close_unlisted_via_proc_self_fd failed with errno {}", err);
                std::process::abort();
            }
        }
    }
}

/// Verifies that the open file descriptor table strictly matches `allowlist`:
/// - Zero unintended descriptors survive.
/// - Zero required allowlisted descriptors are missing.
///
/// Guaranteed allocation-free. Aborts startup immediately on any discrepancy.
pub fn verify_exact_descriptor_table(allowlist: &[RawFd]) {
    assert!(allowlist.len() <= 64, "allowlist exceeds stack capacity of 64 fds");
    let dir_fd = unsafe {
        libc::open(
            b"/proc/self/fd\0".as_ptr() as *const libc::c_char,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if dir_fd < 0 {
        eprintln!("[lotus-supervisor] Fatal: verify_exact_descriptor_table failed to open /proc/self/fd");
        std::process::abort();
    }

    let mut seen = [false; 64];
    let mut buf = [0u8; 1024];

    loop {
        let nread = unsafe { libc::syscall(libc::SYS_getdents64, dir_fd, buf.as_mut_ptr(), buf.len()) };
        if nread < 0 {
            unsafe { libc::close(dir_fd) };
            eprintln!("[lotus-supervisor] Fatal: SYS_getdents64 failed during descriptor verification");
            std::process::abort();
        }
        if nread == 0 {
            break;
        }

        let mut bpos: usize = 0;
        let nread = nread as usize;
        while bpos < nread {
            if bpos + 18 >= nread {
                unsafe { libc::close(dir_fd) };
                std::process::abort();
            }
            let d_reclen = u16::from_ne_bytes([buf[bpos + 16], buf[bpos + 17]]) as usize;
            if d_reclen == 0 || bpos + d_reclen > nread {
                unsafe { libc::close(dir_fd) };
                std::process::abort();
            }

            let name_bytes = &buf[bpos + 19..bpos + d_reclen];
            let name_len = name_bytes.iter().position(|&c| c == 0).unwrap_or(name_bytes.len());
            let name = &name_bytes[..name_len];

            if name != b"." && name != b".." {
                if let Some(fd) = parse_fd(name) {
                    if fd != dir_fd {
                        if let Some(idx) = allowlist.iter().position(|&allowed| allowed == fd) {
                            seen[idx] = true;
                        } else {
                            unsafe { libc::close(dir_fd) };
                            eprintln!("[lotus-supervisor] Fatal: Unlisted descriptor {} survived sanitization", fd);
                            std::process::abort();
                        }
                    }
                }
            }

            bpos += d_reclen;
        }
    }

    unsafe { libc::close(dir_fd) };

    for (idx, &allowed_fd) in allowlist.iter().enumerate() {
        if !seen[idx] {
            eprintln!("[lotus-supervisor] Fatal: Required allowlisted descriptor {} missing", allowed_fd);
            std::process::abort();
        }
    }
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
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
        }
    }

    #[test]
    fn test_parse_fd() {
        assert_eq!(parse_fd(b"0"), Some(0));
        assert_eq!(parse_fd(b"123"), Some(123));
        assert_eq!(parse_fd(b""), None);
        assert_eq!(parse_fd(b"abc"), None);
        assert_eq!(parse_fd(b"12a3"), None);
    }

    #[test]
    fn test_sanitize_descriptors_gap_closing() {
        run_in_fork(|| {
            let mut pipe1 = [0; 2];
            let mut pipe2 = [0; 2];
            assert_eq!(unsafe { libc::pipe(pipe1.as_mut_ptr()) }, 0);
            assert_eq!(unsafe { libc::pipe(pipe2.as_mut_ptr()) }, 0);

            let mut allowlist = [0, 1, 2, pipe1[0], pipe2[1]];
            allowlist.sort_unstable();

            sanitize_descriptors_fail_closed(&allowlist);

            // Verify allowlisted descriptors survived
            assert!(unsafe { libc::fcntl(pipe1[0], libc::F_GETFD) } >= 0);
            assert!(unsafe { libc::fcntl(pipe2[1], libc::F_GETFD) } >= 0);

            // Verify unlisted descriptors were closed
            assert_eq!(unsafe { libc::fcntl(pipe1[1], libc::F_GETFD) }, -1);
            assert_eq!(unsafe { *libc::__errno_location() }, libc::EBADF);
            assert_eq!(unsafe { libc::fcntl(pipe2[0], libc::F_GETFD) }, -1);
            assert_eq!(unsafe { *libc::__errno_location() }, libc::EBADF);

            unsafe {
                libc::close(pipe1[0]);
                libc::close(pipe2[1]);
            }
        });
    }

    #[test]
    fn test_proc_self_fd_fallback_sanitization() {
        run_in_fork(|| {
            let mut pipe1 = [0; 2];
            let mut pipe2 = [0; 2];
            assert_eq!(unsafe { libc::pipe(pipe1.as_mut_ptr()) }, 0);
            assert_eq!(unsafe { libc::pipe(pipe2.as_mut_ptr()) }, 0);

            let mut allowlist = [0, 1, 2, pipe1[0], pipe2[1]];
            allowlist.sort_unstable();

            unsafe {
                assert!(close_unlisted_via_proc_self_fd(&allowlist).is_ok());
            }

            // Verify allowlisted descriptors survived
            assert!(unsafe { libc::fcntl(pipe1[0], libc::F_GETFD) } >= 0);
            assert!(unsafe { libc::fcntl(pipe2[1], libc::F_GETFD) } >= 0);

            // Verify unlisted descriptors were closed
            assert_eq!(unsafe { libc::fcntl(pipe1[1], libc::F_GETFD) }, -1);
            assert_eq!(unsafe { *libc::__errno_location() }, libc::EBADF);
            assert_eq!(unsafe { libc::fcntl(pipe2[0], libc::F_GETFD) }, -1);
            assert_eq!(unsafe { *libc::__errno_location() }, libc::EBADF);

            unsafe {
                libc::close(pipe1[0]);
                libc::close(pipe2[1]);
            }
        });
    }

    #[test]
    fn test_verify_exact_descriptor_table_matching() {
        run_in_fork(|| {
            let mut pipe1 = [0; 2];
            assert_eq!(unsafe { libc::pipe(pipe1.as_mut_ptr()) }, 0);

            let mut allowlist = [0, 1, 2, pipe1[0], pipe1[1]];
            allowlist.sort_unstable();

            sanitize_descriptors_fail_closed(&allowlist);
            verify_exact_descriptor_table(&allowlist);

            unsafe {
                libc::close(pipe1[0]);
                libc::close(pipe1[1]);
            }
        });
    }
}
