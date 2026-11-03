//! Linux namespace creation, UID/GID mapping, loopback bringup, and socket binding.

use std::fs;
use std::net::TcpListener;
use std::os::unix::io::{IntoRawFd, RawFd};

/// Configure single-identity UID/GID mapping for a target child PID.
/// Written by the parent process in the parent user namespace.
pub fn configure_child_user_namespace(child_pid: i32) -> Result<(), i32> {
    let setgroups_path = format!("/proc/{}/setgroups", child_pid);
    if fs::write(&setgroups_path, "deny").is_err() {
        return Err(unsafe { *libc::__errno_location() });
    }

    let host_uid = unsafe { libc::getuid() };
    let host_gid = unsafe { libc::getgid() };

    let uid_map = format!("0 {} 1\n", host_uid);
    let gid_map = format!("0 {} 1\n", host_gid);

    let uid_path = format!("/proc/{}/uid_map", child_pid);
    let gid_path = format!("/proc/{}/gid_map", child_pid);

    if fs::write(&uid_path, uid_map).is_err() {
        return Err(unsafe { *libc::__errno_location() });
    }
    if fs::write(&gid_path, gid_map).is_err() {
        return Err(unsafe { *libc::__errno_location() });
    }

    Ok(())
}

/// Unshares network, mount, and IPC namespaces in the child, and sets private mount propagation.
pub fn setup_child_isolated_namespaces() -> Result<(), i32> {
    let flags = libc::CLONE_NEWNET | libc::CLONE_NEWNS | libc::CLONE_NEWIPC;
    if unsafe { libc::unshare(flags) } != 0 {
        return Err(unsafe { *libc::__errno_location() });
    }

    // Configure MS_REC | MS_PRIVATE mount propagation to isolate future mounts (e.g. /proc)
    if unsafe {
        libc::mount(
            std::ptr::null(),
            b"/\0".as_ptr() as *const libc::c_char,
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    } != 0
    {
        return Err(unsafe { *libc::__errno_location() });
    }

    Ok(())
}

/// Bring up the loopback interface (`lo`) inside the current network namespace.
pub fn bring_up_loopback() -> Result<(), i32> {
    #[repr(C)]
    struct IfreqFlags {
        ifr_name: [u8; libc::IFNAMSIZ],
        ifr_flags: libc::c_short,
        _padding: [u8; 22],
    }

    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        return Err(unsafe { *libc::__errno_location() });
    }

    let mut ifr: IfreqFlags = unsafe { std::mem::zeroed() };
    ifr.ifr_name[0] = b'l';
    ifr.ifr_name[1] = b'o';

    // 1. Get current flags
    if unsafe { libc::ioctl(sock, libc::SIOCGIFFLAGS, &mut ifr as *mut _) } < 0 {
        let err = unsafe { *libc::__errno_location() };
        unsafe { libc::close(sock) };
        return Err(err);
    }

    // 2. Set IFF_UP | IFF_RUNNING
    ifr.ifr_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
    if unsafe { libc::ioctl(sock, libc::SIOCSIFFLAGS, &ifr as *const _) } < 0 {
        let err = unsafe { *libc::__errno_location() };
        unsafe { libc::close(sock) };
        return Err(err);
    }

    unsafe { libc::close(sock) };
    Ok(())
}

/// Create and pre-bind an AF_INET TCP listener on 127.0.0.1:0 inside the current network namespace.
pub fn bind_netns_listener() -> Result<(RawFd, u16), i32> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.raw_os_error().unwrap_or(libc::EADDRNOTAVAIL))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| e.raw_os_error().unwrap_or(libc::EADDRNOTAVAIL))?;
    let bound_port = local_addr.port();
    let fd = listener.into_raw_fd();
    Ok((fd, bound_port))
}
