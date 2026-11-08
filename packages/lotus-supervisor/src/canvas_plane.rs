//! Double-buffered WebGL canvas plane in a sealed memfd (host writer / jail reader).
//!
//! Layout (little-endian, natural align):
//! - `u64` active_slot (0 or 1)
//! - `u32` width, `u32` height, `u32` stride_bytes, `u32` reserved
//! - `u64` slot0_seq, `u64` slot1_seq  (odd = writer in progress, even = stable)
//! - padding to 128 bytes
//! - slot0 pixels (stride * height)
//! - slot1 pixels (stride * height)
//!
//! Keep in sync with `servo-webgl` `broker_canvas.rs`.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::shm::SealedMemfdPlane;

pub const CANVAS_HEADER_BYTES: usize = 128;

#[repr(C)]
pub struct CanvasPlaneHeader {
    pub active_slot: AtomicU64,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub _reserved: u32,
    pub slot_seq: [AtomicU64; 2],
    _pad: [u8; 128 - 8 - 16 - 16],
}

const _: () = assert!(std::mem::size_of::<CanvasPlaneHeader>() == CANVAS_HEADER_BYTES);

pub fn stride_for_width(width: u32) -> u32 {
    width.saturating_mul(4)
}

pub fn plane_bytes(width: u32, height: u32) -> usize {
    let stride = stride_for_width(width) as usize;
    let slot = stride.saturating_mul(height as usize);
    CANVAS_HEADER_BYTES + slot.saturating_mul(2)
}

pub fn create_canvas_plane(
    name: &str,
    width: u32,
    height: u32,
) -> Result<SealedMemfdPlane, &'static str> {
    let size = plane_bytes(width, height);
    if size == CANVAS_HEADER_BYTES {
        return Err("E_CANVAS_EMPTY");
    }
    let plane = SealedMemfdPlane::new(name, size)?;
    unsafe {
        init_header(plane.rw_ptr, width, height);
    }
    Ok(plane)
}

/// # Safety
/// `ptr` must point at a mapping of at least [`plane_bytes`] bytes.
pub unsafe fn init_header(ptr: *mut u8, width: u32, height: u32) {
    let hdr = &mut *(ptr as *mut CanvasPlaneHeader);
    hdr.active_slot.store(0, Ordering::Relaxed);
    hdr.width = width;
    hdr.height = height;
    hdr.stride_bytes = stride_for_width(width);
    hdr._reserved = 0;
    hdr.slot_seq[0].store(0, Ordering::Relaxed);
    hdr.slot_seq[1].store(0, Ordering::Relaxed);
    // zero pad + pixel area left as-is (memfd is zero-filled by kernel)
}

/// Publish one BGRA8 frame into the inactive slot (host writer).
///
/// # Safety
/// `ptr` must be the host's writable mapping; `pixels` length must be `stride*height`.
pub unsafe fn publish_frame(ptr: *mut u8, pixels: &[u8]) -> Result<(), &'static str> {
    let hdr = &mut *(ptr as *mut CanvasPlaneHeader);
    let w = hdr.width as usize;
    let h = hdr.height as usize;
    let stride = hdr.stride_bytes as usize;
    let need = stride.saturating_mul(h);
    if pixels.len() < need || w == 0 || h == 0 {
        return Err("E_CANVAS_SIZE");
    }
    let active = hdr.active_slot.load(Ordering::Acquire) as usize & 1;
    let dest_slot = 1 - active;
    let seq = &hdr.slot_seq[dest_slot];
    let prev = seq.load(Ordering::Relaxed);
    seq.store(prev | 1, Ordering::Release); // odd = writing
    std::sync::atomic::fence(Ordering::Release);

    let base = ptr.add(CANVAS_HEADER_BYTES + dest_slot * need);
    std::ptr::copy_nonoverlapping(pixels.as_ptr(), base, need);

    std::sync::atomic::fence(Ordering::Release);
    seq.store(prev.wrapping_add(2) & !1, Ordering::Release); // next even
    hdr.active_slot.store(dest_slot as u64, Ordering::Release);
    Ok(())
}

/// Copy the stable active slot into `out` (jail reader). Returns `(width, height)` on success.
///
/// # Safety
/// `ptr` must be a readable mapping of the canvas plane.
pub unsafe fn try_copy_frame(ptr: *const u8, out: &mut Vec<u8>) -> Option<(u32, u32)> {
    let hdr = &*(ptr as *const CanvasPlaneHeader);
    let w = hdr.width;
    let h = hdr.height;
    let stride = hdr.stride_bytes as usize;
    if w == 0 || h == 0 {
        return None;
    }
    let need = stride.saturating_mul(h as usize);
    for _ in 0..4 {
        let slot = hdr.active_slot.load(Ordering::Acquire) as usize & 1;
        let seq1 = hdr.slot_seq[slot].load(Ordering::Acquire);
        if seq1 & 1 != 0 {
            continue;
        }
        std::sync::atomic::fence(Ordering::Acquire);
        // Grow without zero-fill — copy overwrites every byte on success.
        if out.capacity() < need {
            out.reserve(need - out.capacity());
        }
        // SAFETY: capacity >= need; contents overwritten by copy_nonoverlapping.
        unsafe {
            out.set_len(need);
        }
        let base = ptr.add(CANVAS_HEADER_BYTES + slot * need);
        // Volatile-ish copy via ptr::copy — writer uses release fences; we re-check seq.
        std::ptr::copy_nonoverlapping(base, out.as_mut_ptr(), need);
        std::sync::atomic::fence(Ordering::Acquire);
        let seq2 = hdr.slot_seq[slot].load(Ordering::Acquire);
        if seq1 == seq2 && seq2 & 1 == 0 {
            return Some((w, h));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn publish_and_copy_roundtrip() {
        let plane = create_canvas_plane("lotus_canvas_test", 2, 2).expect("plane");
        let mut px = vec![0u8; 2 * 2 * 4];
        px[0] = 10;
        px[1] = 20;
        px[2] = 30;
        px[3] = 255;
        unsafe {
            publish_frame(plane.rw_ptr, &px).expect("publish");
        }
        let mut out = Vec::new();
        let (w, h) = unsafe { try_copy_frame(plane.rw_ptr, &mut out).expect("copy") };
        assert_eq!((w, h), (2, 2));
        assert_eq!(&out[..4], &px[..4]);
        let hdr = unsafe { &*(plane.rw_ptr as *const CanvasPlaneHeader) };
        assert_eq!(hdr.slot_seq[0].load(Ordering::Relaxed) & 1, 0);
    }
}
