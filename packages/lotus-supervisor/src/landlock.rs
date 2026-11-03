//! Landlock Linux Security Module integration for lotus-supervisor.

pub const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
pub const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
pub const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

pub const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
pub const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

pub const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
pub const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
pub const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
pub const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
pub const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
pub const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
pub const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
pub const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
pub const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
pub const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
pub const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
pub const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
pub const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
pub const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
pub const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;
pub const LANDLOCK_ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LandlockRulesetAttr {
    pub handled_access_fs: u64,
    pub handled_access_net: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LandlockPathBeneathAttr {
    pub allowed_access: u64,
    pub parent_fd: i32,
}

#[derive(Debug, Clone, Copy)]
pub struct LandlockFd {
    pub fd: i32,
    pub writeable: bool,
}

/// Query the Landlock ABI version supported by the kernel. Returns negative on error/unsupported.
pub fn get_landlock_abi_version() -> i32 {
    unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<LandlockRulesetAttr>(),
            0,
            LANDLOCK_CREATE_RULESET_VERSION,
        ) as i32
    }
}

pub const FS_READ_ACCESS: u64 = LANDLOCK_ACCESS_FS_EXECUTE
    | LANDLOCK_ACCESS_FS_READ_FILE
    | LANDLOCK_ACCESS_FS_READ_DIR;

pub const FS_WRITE_ACCESS: u64 = FS_READ_ACCESS
    | LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_REMOVE_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_TRUNCATE;

/// Open a directory as an O_PATH descriptor suitable for Landlock.
/// Returns Ok(fd) or Err if directory doesn't exist.
pub fn open_landlock_dir(path: &str, writeable: bool) -> Result<LandlockFd, ()> {
    use std::ffi::CString;
    let c_path = CString::new(path).map_err(|_| ())?;
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(())
    } else {
        Ok(LandlockFd { fd, writeable })
    }
}

/// Apply Landlock ruleset to the current thread/process and close the rule descriptors.
/// Must be allocation-free and fail-closed.
pub fn apply_landlock_and_close_descriptors(descriptors: &[LandlockFd]) {
    let abi = get_landlock_abi_version();
    if abi < 1 {
        // If Landlock is unsupported by the host kernel, close descriptors and abort (fail-closed)
        for d in descriptors {
            unsafe { libc::close(d.fd) };
        }
        std::process::abort();
    }

    let ruleset_attr = LandlockRulesetAttr {
        handled_access_fs: FS_WRITE_ACCESS,
        handled_access_net: 0,
    };

    let ruleset_fd = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &ruleset_attr as *const _,
            std::mem::size_of::<LandlockRulesetAttr>(),
            0,
        ) as i32
    };

    if ruleset_fd < 0 {
        for d in descriptors {
            unsafe { libc::close(d.fd) };
        }
        std::process::abort();
    }

    for d in descriptors {
        let access = if d.writeable {
            FS_WRITE_ACCESS
        } else {
            FS_READ_ACCESS
        };
        let path_beneath = LandlockPathBeneathAttr {
            allowed_access: access,
            parent_fd: d.fd,
        };
        let ret = unsafe {
            libc::syscall(
                SYS_LANDLOCK_ADD_RULE,
                ruleset_fd,
                LANDLOCK_RULE_PATH_BENEATH,
                &path_beneath as *const _,
                0,
            )
        };
        // Always close descriptor
        unsafe { libc::close(d.fd) };
        if ret != 0 {
            unsafe { libc::close(ruleset_fd) };
            std::process::abort();
        }
    }

    // Apply self-restriction
    let ret = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd, 0) };
    unsafe { libc::close(ruleset_fd) };
    if ret != 0 {
        std::process::abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_landlock_abi_probe() {
        let abi = get_landlock_abi_version();
        assert!(abi >= 1, "Expected Landlock ABI >= 1, got {}", abi);
    }

    #[test]
    fn test_landlock_application() {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // Child: set no_new_privs first (required by Landlock)
            unsafe {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    libc::_exit(1);
                }
            }

            let mut fds = Vec::new();
            if let Ok(d) = open_landlock_dir("/usr", false) {
                fds.push(d);
            }
            if let Ok(d) = open_landlock_dir("/tmp", true) {
                fds.push(d);
            }

            apply_landlock_and_close_descriptors(&fds);

            // Attempting to read /root or a restricted path should fail with EACCES
            let c_root = std::ffi::CString::new("/etc/shadow").unwrap();
            let fd = unsafe { libc::open(c_root.as_ptr(), libc::O_RDONLY) };
            if fd >= 0 {
                // If by some chance shadow opened, exit with failure
                unsafe {
                    libc::close(fd);
                    libc::_exit(2);
                }
            }

            unsafe { libc::_exit(0) };
        } else {
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
            assert!(
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                "Child failed: status = {}",
                libc::WEXITSTATUS(status)
            );
        }
    }
}
