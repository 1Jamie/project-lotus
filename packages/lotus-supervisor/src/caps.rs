//! Capability dropping and privilege revocation utilities for lotus-supervisor.

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CapUserHeader {
    pub version: u32,
    pub pid: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CapUserData {
    pub effective: u32,
    pub permitted: u32,
    pub inheritable: u32,
}

pub const _LINUX_CAPABILITY_VERSION_3: u32 = 0x20080522;
pub const PR_CAP_AMBIENT: i32 = 47;
pub const PR_CAP_AMBIENT_CLEAR_ALL: libc::c_ulong = 4;
pub const PR_CAPBSET_DROP: i32 = 24;

/// Drop all ambient, bounding, effective, permitted, and inheritable capabilities.
/// Fails closed (aborts process) on unexpected error.
pub fn drop_all_capabilities() {
    unsafe {
        // 1. Clear all ambient capabilities
        let _ = libc::prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0);

        // 2. Drop bounding capability set (iterate up to 63; EINVAL is normal for unassigned caps)
        for cap in 0..64 {
            let _ = libc::prctl(PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0);
        }

        // 3. Clear effective, permitted, and inheritable capability sets via capset
        let hdr = CapUserHeader {
            version: _LINUX_CAPABILITY_VERSION_3,
            pid: 0, // 0 means current calling thread/process
        };
        let data = [
            CapUserData {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            },
            CapUserData {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            },
        ];

        let ret = libc::syscall(libc::SYS_capset, &hdr as *const _, &data as *const _);
        if ret != 0 {
            libc::abort();
        }
    }
}

/// Set PR_SET_NO_NEW_PRIVS. Aborts on failure.
pub fn set_no_new_privs() {
    unsafe {
        let ret = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        if ret != 0 {
            libc::abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_drop_all_capabilities_and_no_new_privs() {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");

        if pid == 0 {
            // Child: set no new privs and drop all capabilities
            set_no_new_privs();
            drop_all_capabilities();

            // Verify /proc/self/status has CapEff = 0000000000000000
            let status = fs::read_to_string("/proc/self/status").expect("read status");
            for line in status.lines() {
                if line.starts_with("CapEff:") {
                    let val = line.split_whitespace().nth(1).unwrap();
                    if val != "0000000000000000" {
                        unsafe { libc::_exit(1) };
                    }
                }
                if line.starts_with("NoNewPrivs:") {
                    let val = line.split_whitespace().nth(1).unwrap();
                    if val != "1" {
                        unsafe { libc::_exit(2) };
                    }
                }
            }
            unsafe { libc::_exit(0) };
        } else {
            let mut status: libc::c_int = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
            assert!(
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                "Child failed capability/no_new_privs check: exit status = {}",
                libc::WEXITSTATUS(status)
            );
        }
    }
}

