//! Byte-stream framing for the host ↔ sandbox GPU socket (`LOTUS_GPU_FD`).
//!
//! Each message: **24-byte little-endian header** then **payload**:
//! - `u32` `body_len` — bytes of payload after the header (not including header).
//! - `u64` `corr` — correlation id (engine → host request flow).
//! - `u64` `context_id` — WebGL context id for attribution (oversized / context-lost paths).
//! - `u32` `kind` — message kind (wire enum; v1 host only validates framing).
//!
//! Policy (v1):
//! - `body_len` ≤ [`MAX_BODY_BYTES`]: read payload in full.
//! - `body_len` > [`MAX_BODY_BYTES`] but ≤ [`CLOSE_THRESHOLD_BYTES`]: discard `body_len` bytes (bounded
//!   reads), log/signal context failure; **connection stays open**.
//! - `body_len` > [`CLOSE_THRESHOLD_BYTES`]: treat as attack; caller should **close** the socket
//!   (do not spin on a huge discard).

use std::os::unix::io::RawFd;

/// Maximum payload size the host will buffer in v1 (e.g. one 4096² RGBA upload).
pub const HEADER_BYTES: usize = 24;
pub const MAX_BODY_BYTES: u64 = 80 * 1024 * 1024;
/// Above this, the host closes the connection instead of discarding (2 × max body).
pub const CLOSE_THRESHOLD_BYTES: u64 = MAX_BODY_BYTES.saturating_mul(2);

const DISCARD_CHUNK: usize = 64 * 1024;

/// v1 wire kinds: engine → host `Ping`, host → engine `Pong` (broker handshake).
pub const KIND_PING: u32 = 1;
pub const KIND_PONG: u32 = 2;
/// Engine → host: stripped [`WebGLMsg`] body (MessagePack).
pub const KIND_COMMAND: u32 = 3;
/// Host → engine: reply payload for a prior command correlation id.
pub const KIND_REPLY: u32 = 4;
/// Engine → host: abandon a context after executing-phase timeout / disconnect.
pub const KIND_CONTEXT_ABANDONED: u32 = 5;
/// Host → engine: canvas memfd slot advanced for `context_id` (body empty or tiny).
pub const KIND_CANVAS_PRESENT: u32 = 6;
/// Engine → host: canvas device-pixel bounds for hole-punch (body = 4×i32 x,y,w,h).
pub const KIND_CANVAS_BOUNDS: u32 = 7;

/// Parsed header fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuFrameHeader {
    pub body_len: u32,
    pub corr: u64,
    pub context_id: u64,
    pub kind: u32,
}

impl GpuFrameHeader {
    pub fn decode(bytes: &[u8; HEADER_BYTES]) -> Self {
        Self {
            body_len: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            corr: u64::from_le_bytes(bytes[4..12].try_into().unwrap()),
            context_id: u64::from_le_bytes(bytes[12..20].try_into().unwrap()),
            kind: u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
        }
    }

    pub fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut b = [0u8; HEADER_BYTES];
        b[0..4].copy_from_slice(&self.body_len.to_le_bytes());
        b[4..12].copy_from_slice(&self.corr.to_le_bytes());
        b[12..20].copy_from_slice(&self.context_id.to_le_bytes());
        b[20..24].copy_from_slice(&self.kind.to_le_bytes());
        b
    }

    /// What to do after reading the header bytes.
    pub fn routing(&self) -> FrameRouting {
        let n = u64::from(self.body_len);
        if n > CLOSE_THRESHOLD_BYTES {
            FrameRouting::CloseConnection
        } else if n > MAX_BODY_BYTES {
            FrameRouting::DiscardBody { len: n }
        } else {
            FrameRouting::ReadBody { len: n as usize }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRouting {
    ReadBody { len: usize },
    DiscardBody { len: u64 },
    CloseConnection,
}

/// Read exactly `buf.len()` bytes from `fd`, or return an error on EOF / syscall failure.
pub fn read_exact_fd(fd: RawFd, buf: &mut [u8]) -> Result<(), &'static str> {
    let mut off = 0;
    while off < buf.len() {
        let n = unsafe {
            libc::read(
                fd,
                buf[off..].as_mut_ptr() as *mut libc::c_void,
                buf.len() - off,
            )
        };
        if n < 0 {
            return Err("E_READ");
        }
        if n == 0 {
            return Err("E_EOF");
        }
        off += n as usize;
    }
    Ok(())
}

/// Write all of `buf` to `fd` (handles partial writes).
pub fn write_exact_fd(fd: RawFd, buf: &[u8]) -> Result<(), &'static str> {
    let mut off = 0;
    while off < buf.len() {
        let n = unsafe {
            libc::write(
                fd,
                buf[off..].as_ptr() as *const libc::c_void,
                buf.len() - off,
            )
        };
        if n < 0 {
            return Err("E_WRITE");
        }
        if n == 0 {
            return Err("E_WRITE_ZERO");
        }
        off += n as usize;
    }
    Ok(())
}

/// Write one framed message: 24-byte header then `body` (must match `header.body_len`).
pub fn write_frame(fd: RawFd, header: &GpuFrameHeader, body: &[u8]) -> Result<(), &'static str> {
    if u32::try_from(body.len()).ok() != Some(header.body_len) {
        return Err("E_HEADER_BODY_MISMATCH");
    }
    write_exact_fd(fd, &header.encode())?;
    if !body.is_empty() {
        write_exact_fd(fd, body)?;
    }
    Ok(())
}

/// Write header (+ optional body) with one SCM_RIGHTS fd attached to the header sendmsg.
pub fn write_frame_with_fd(
    fd: RawFd,
    header: &GpuFrameHeader,
    body: &[u8],
    pass_fd: RawFd,
) -> Result<(), &'static str> {
    if u32::try_from(body.len()).ok() != Some(header.body_len) {
        return Err("E_HEADER_BODY_MISMATCH");
    }
    let hdr_bytes = header.encode();
    let mut iov = libc::iovec {
        iov_base: hdr_bytes.as_ptr() as *mut libc::c_void,
        iov_len: HEADER_BYTES,
    };
    let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize };
    let mut cmsg_buf = vec![0u8; cmsg_space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err("E_CMSG");
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        let data_ptr = libc::CMSG_DATA(cmsg) as *mut RawFd;
        *data_ptr = pass_fd;
    }

    let n = unsafe { libc::sendmsg(fd, &msg, 0) };
    if n < 0 {
        return Err("E_SENDMSG");
    }
    if !body.is_empty() {
        write_exact_fd(fd, body)?;
    }
    Ok(())
}

/// Discard exactly `len` bytes from `fd` (bounded chunked reads).
pub fn discard_exact_fd(fd: RawFd, mut len: u64) -> Result<(), &'static str> {
    let mut chunk = [0u8; DISCARD_CHUNK];
    while len > 0 {
        let take = (len as usize).min(DISCARD_CHUNK);
        read_exact_fd(fd, &mut chunk[..take])?;
        len -= take as u64;
    }
    Ok(())
}

/// Read a full header from the stream.
pub fn read_header(fd: RawFd) -> Result<GpuFrameHeader, &'static str> {
    let mut raw = [0u8; HEADER_BYTES];
    read_exact_fd(fd, &mut raw)?;
    Ok(GpuFrameHeader::decode(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_roundtrip_fields() {
        let mut b = [0u8; HEADER_BYTES];
        b[0..4].copy_from_slice(&42u32.to_le_bytes());
        b[4..12].copy_from_slice(&7u64.to_le_bytes());
        b[12..20].copy_from_slice(&99u64.to_le_bytes());
        b[20..24].copy_from_slice(&3u32.to_le_bytes());
        let h = GpuFrameHeader::decode(&b);
        assert_eq!(h.body_len, 42);
        assert_eq!(h.corr, 7);
        assert_eq!(h.context_id, 99);
        assert_eq!(h.kind, 3);
        let enc = h.encode();
        assert_eq!(enc, b);
    }

    #[test]
    fn routing_read_small() {
        let h = GpuFrameHeader {
            body_len: 100,
            corr: 0,
            context_id: 1,
            kind: 0,
        };
        assert!(matches!(h.routing(), FrameRouting::ReadBody { len: 100 }));
    }

    #[test]
    fn routing_discard_over_cap() {
        let h = GpuFrameHeader {
            body_len: (MAX_BODY_BYTES + 1) as u32,
            corr: 0,
            context_id: 2,
            kind: 0,
        };
        assert!(matches!(
            h.routing(),
            FrameRouting::DiscardBody { len: _ }
        ));
    }

    #[test]
    fn routing_close_u32_max() {
        let h = GpuFrameHeader {
            body_len: u32::MAX,
            corr: 0,
            context_id: 0,
            kind: 0,
        };
        assert_eq!(h.routing(), FrameRouting::CloseConnection);
    }
}
