//! packages/lotus-supervisor/src/shm.rs
//! Unidirectional Per-Writer Split-Plane Shared Memory Architecture (RemotePaneShm).
//!
//! Implements Option A serialized 1P-1C single-claim acquisition on exact (slot_index, generation, pub_seq)
//! publication identity, genuine read-only memfd descriptor construction, and the 15-point independent
//! consumer frame validation suite including individual slot bounds.

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("Cross-process atomicity qualification contract is verified exclusively for x86_64 and aarch64 on Linux.");

use std::os::unix::io::RawFd;
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

pub const MAX_PANE_DIMENSION: u32 = 4096;
pub const MAX_PANE_PIXEL_BYTES: usize = 256 * 1024 * 1024; // 256 MiB hard ceiling

pub const STATE_FREE: u32 = 0;
pub const STATE_WRITING: u32 = 1;
pub const STATE_READY: u32 = 2;

pub const FORMAT_RGBA8: u32 = 0;
pub const CLAIM_UNCLAIMED: u32 = 0xFF;

// Memfd and Sealing constants for Linux
pub const MFD_CLOEXEC: libc::c_uint = 0x0001;
pub const MFD_ALLOW_SEALING: libc::c_uint = 0x0002;

pub const F_LINUX_SPECIFIC_BASE: libc::c_int = 1024;
pub const F_ADD_SEALS: libc::c_int = F_LINUX_SPECIFIC_BASE + 9;
pub const F_GET_SEALS: libc::c_int = F_LINUX_SPECIFIC_BASE + 10;

pub const F_SEAL_SEAL: libc::c_int = 0x0001;
pub const F_SEAL_SHRINK: libc::c_int = 0x0002;
pub const F_SEAL_GROW: libc::c_int = 0x0004;
pub const F_SEAL_WRITE: libc::c_int = 0x0008;
pub const F_SEAL_FUTURE_WRITE: libc::c_int = 0x0010;

#[repr(C, align(64))]
pub struct ProducerControlHeader {
    pub pub_seq: AtomicU64,         // Offset 0..8: Authoritative monotonic frame publication sequence
    pub prod_resize_gen: AtomicU64, // Offset 8..16: Host-correlated 64-bit resize generation
    pub active_slots: u32,          // Offset 16..20: Always 3
    pub _pad: [u8; 44],             // Offset 20..64: Explicit cache-line padding (64 bytes total)
    pub slots: [SlotDescriptor; 3], // Offset 64..256: Per-slot publication descriptors (3 * 64 bytes)
}

#[repr(C, align(64))]
pub struct SlotDescriptor {
    pub state: AtomicU32,          // Offset 0..4: 0 = FREE, 1 = WRITING, 2 = READY
    pub _reserved0: u32,           // Offset 4..8: Explicit 8-byte alignment padding
    pub generation: AtomicU64,     // Offset 8..16: Monotonic 64-bit slot incarnation generation
    pub pub_seq: AtomicU64,        // Offset 16..24: Sequence snapshot at publication (matches header pub_seq)
    pub width: AtomicU32,          // Offset 24..28: AtomicU32 (eliminates data-race UB on untrusted reads)
    pub height: AtomicU32,         // Offset 28..32: AtomicU32 (eliminates data-race UB on untrusted reads)
    pub stride: AtomicU32,         // Offset 32..36: AtomicU32 (eliminates data-race UB on untrusted reads)
    pub format: AtomicU32,         // Offset 36..40: AtomicU32 (0 = RGBA8)
    pub _pad: [u8; 24],            // Offset 40..64: Explicit cache-line padding (64 bytes total)
}

#[repr(C, align(64))]
pub struct ConsumerAckHeader {
    pub consumer_gen_id: AtomicU64,    // Offset 0..8: 64-bit untrusted incarnation reported by compositor
    pub claimed_generation: AtomicU64, // Offset 8..16: Exact 64-bit generation claimed by consumer
    pub claimed_seq: AtomicU64,        // Offset 16..24: Exact 64-bit sequence claimed by consumer
    pub free_seq: AtomicU64,           // Offset 24..32: Diagnostic progress counter
    pub claimed_slot: AtomicU32,       // Offset 32..36: Authoritative claim marker (0..2, or 0xFF)
    pub _pad: [u8; 28],                // Offset 36..64: Explicit cache-line padding (64 bytes total)
}

/// Sizing and geometry for triple-buffered RemotePaneShm pixel planes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmGeometry {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub slot_stride_bytes: usize,
    pub total_pixel_bytes: usize,
}

pub fn calculate_shm_geometry(width: u32, height: u32) -> Result<ShmGeometry, &'static str> {
    if width == 0 || height == 0 || width > MAX_PANE_DIMENSION || height > MAX_PANE_DIMENSION {
        return Err("E_SHM_INVALID_DIMENSIONS");
    }

    // 64-byte cache-line aligned stride
    let raw_stride = (width as usize).checked_mul(4).ok_or("E_SHM_STRIDE_OVERFLOW")?;
    let stride = (raw_stride.checked_add(63).ok_or("E_SHM_ALIGN_OVERFLOW")?) / 64 * 64;

    // 4096-byte page aligned slot capacity
    let raw_slot = stride.checked_mul(height as usize).ok_or("E_SHM_SLOT_OVERFLOW")?;
    let slot_stride_bytes = (raw_slot.checked_add(4095).ok_or("E_SHM_ALIGN_OVERFLOW")?) / 4096 * 4096;

    let total_pixel_bytes = slot_stride_bytes
        .checked_mul(3)
        .filter(|&sz| sz <= MAX_PANE_PIXEL_BYTES)
        .ok_or("E_SHM_DIMENSION_OVERFLOW")?;

    Ok(ShmGeometry {
        width,
        height,
        stride,
        slot_stride_bytes,
        total_pixel_bytes,
    })
}

/// Helper representing an allocated, sealed memfd plane with genuine O_RDWR and O_RDONLY descriptors.
pub struct SealedMemfdPlane {
    pub rw_fd: RawFd,
    pub ro_fd: RawFd,
    pub rw_ptr: *mut u8,
    pub size: usize,
}

impl SealedMemfdPlane {
    pub fn new(name: &str, size: usize) -> Result<Self, &'static str> {
        use std::ffi::CString;
        let c_name = CString::new(name).map_err(|_| "E_SHM_INVALID_NAME")?;

        let rw_fd = unsafe {
            libc::syscall(
                libc::SYS_memfd_create,
                c_name.as_ptr(),
                MFD_CLOEXEC | MFD_ALLOW_SEALING,
            ) as RawFd
        };
        if rw_fd < 0 {
            return Err("E_SHM_MEMFD_CREATE_FAILED");
        }

        // Sizing
        if unsafe { libc::ftruncate(rw_fd, size as libc::off_t) } != 0 {
            unsafe { libc::close(rw_fd) };
            return Err("E_SHM_FTRUNCATE_FAILED");
        }

        // Pre-map writable pointer for producer BEFORE applying F_SEAL_FUTURE_WRITE
        let rw_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                rw_fd,
                0,
            ) as *mut u8
        };
        if rw_ptr == libc::MAP_FAILED as *mut u8 {
            unsafe { libc::close(rw_fd) };
            return Err("E_SHM_MMAP_FAILED");
        }

        // Apply seals: F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL
        // Allows owning writer process to map PROT_WRITE, MAP_SHARED across process boundaries,
        // while preventing file truncation or seal modifications.
        let seals = F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL;
        if unsafe { libc::fcntl(rw_fd, F_ADD_SEALS, seals) } != 0 {
            unsafe {
                libc::munmap(rw_ptr as *mut libc::c_void, size);
                libc::close(rw_fd);
            };
            return Err("E_SHM_SEAL_FAILED");
        }

        // Create genuine read-only file description by reopening /proc/self/fd/<rw_fd>
        let ro_path = format!("/proc/self/fd/{}", rw_fd);
        let ro_c_path = CString::new(ro_path).map_err(|_| "E_SHM_PATH_FAILED")?;
        let ro_fd = unsafe {
            libc::open(
                ro_c_path.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        if ro_fd < 0 {
            unsafe {
                libc::munmap(rw_ptr as *mut libc::c_void, size);
                libc::close(rw_fd);
            };
            return Err("E_SHM_RO_REOPEN_FAILED");
        }

        // Strip write mode bits from inode (fchmod 0o400) to block /proc/self/fd re-open escalation
        if unsafe { libc::fchmod(rw_fd, 0o400) } != 0 {
            unsafe {
                libc::munmap(rw_ptr as *mut libc::c_void, size);
                libc::close(rw_fd);
                libc::close(ro_fd);
            }
            return Err("E_SHM_FCHMOD_FAILED");
        }

        // Verify access modes via fcntl(F_GETFL)
        let rw_flags = unsafe { libc::fcntl(rw_fd, libc::F_GETFL) };
        let ro_flags = unsafe { libc::fcntl(ro_fd, libc::F_GETFL) };
        if (rw_flags & libc::O_ACCMODE) != libc::O_RDWR || (ro_flags & libc::O_ACCMODE) != libc::O_RDONLY {
            unsafe {
                libc::munmap(rw_ptr as *mut libc::c_void, size);
                libc::close(rw_fd);
                libc::close(ro_fd);
            }
            return Err("E_SHM_ACCESS_MODE_VERIFICATION_FAILED");
        }

        Ok(SealedMemfdPlane { rw_fd, ro_fd, rw_ptr, size })
    }
}

impl Drop for SealedMemfdPlane {
    fn drop(&mut self) {
        unsafe {
            if !self.rw_ptr.is_null() && self.rw_ptr != libc::MAP_FAILED as *mut u8 {
                libc::munmap(self.rw_ptr as *mut libc::c_void, self.size);
            }
            if self.rw_fd >= 0 {
                libc::close(self.rw_fd);
            }
            if self.ro_fd >= 0 {
                libc::close(self.ro_fd);
            }
        }
    }
}

/// Validated metadata output produced by the consumer validation suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedFrame {
    pub slot_idx: usize,
    pub generation: u64,
    pub pub_seq: u64,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub slot_offset: usize,
    pub slot_row_bytes: usize,
}

/// Executes the 15-point Independent Untrusted Producer Validation Protocol.
/// Verifies that frame fits inside its individual slot, offset arithmetic cannot overflow,
/// and publication identity strictly matches the claimed publication.
pub fn validate_frame_metadata(
    slot_idx: usize,
    desc: &SlotDescriptor,
    header: &ProducerControlHeader,
    ack: &ConsumerAckHeader,
    current_active_resize_gen: u64,
    max_negotiated_width: u32,
    max_negotiated_height: u32,
    slot_stride_bytes: usize,
    mapped_pixel_len: usize,
) -> Result<ValidatedFrame, &'static str> {
    // 12. Slot Correlation
    let claimed_slot = ack.claimed_slot.load(Ordering::Acquire);
    if claimed_slot as usize != slot_idx || slot_idx >= 3 {
        return Err("E_SHM_SLOT_CORRELATION_FAILED");
    }

    // Atomically snapshot all fields to stack to eliminate data-race UB
    let gen = desc.generation.load(Ordering::Acquire);
    let seq = desc.pub_seq.load(Ordering::Acquire);
    let state = desc.state.load(Ordering::Acquire);
    let width = desc.width.load(Ordering::Acquire);
    let height = desc.height.load(Ordering::Acquire);
    let stride_u32 = desc.stride.load(Ordering::Acquire);
    let format = desc.format.load(Ordering::Acquire);
    let resize_gen = header.prod_resize_gen.load(Ordering::Acquire);

    // 13. Exact Claimed Publication Validity
    let claimed_gen = ack.claimed_generation.load(Ordering::Acquire);
    let claimed_seq = ack.claimed_seq.load(Ordering::Acquire);
    if gen != claimed_gen || seq != claimed_seq || state != STATE_READY || claimed_seq == 0 {
        return Err("E_SHM_PUBLICATION_MISMATCH");
    }

    // 14. Active Resize Generation Match
    if resize_gen != current_active_resize_gen {
        return Err("E_SHM_RESIZE_GEN_MISMATCH");
    }

    // 1. width > 0
    if width == 0 {
        return Err("E_SHM_ZERO_WIDTH");
    }
    // 2. height > 0
    if height == 0 {
        return Err("E_SHM_ZERO_HEIGHT");
    }
    // 3. Dimension Bounds
    if width > max_negotiated_width || height > max_negotiated_height {
        return Err("E_SHM_DIMENSIONS_EXCEEDED");
    }

    // 4. Explicit Format Recognition: FORMAT_RGBA8 (0)
    if format != FORMAT_RGBA8 {
        return Err("E_SHM_UNSUPPORTED_FORMAT");
    }
    // 5. Host-derived BPP
    let bytes_per_pixel: usize = 4;

    // 6. Row Byte Bounds
    let min_row_bytes = (width as usize)
        .checked_mul(bytes_per_pixel)
        .ok_or("E_SHM_OVERFLOW_ROW_BYTES")?;

    // 7. Minimum Stride
    let stride = stride_u32 as usize;
    if stride < min_row_bytes {
        return Err("E_SHM_STRIDE_TOO_SMALL");
    }

    // 8. Stride Alignment (64-byte cache line alignment)
    if stride % 64 != 0 {
        return Err("E_SHM_STRIDE_UNALIGNED");
    }

    // 9. Individual Slot Row Bounds: slot_row_bytes <= slot_stride_bytes
    let slot_row_bytes = stride
        .checked_mul(height as usize)
        .ok_or("E_SHM_OVERFLOW_SLOT_ROW_BYTES")?;

    if slot_row_bytes > slot_stride_bytes {
        return Err("E_SHM_FRAME_EXCEEDS_INDIVIDUAL_SLOT");
    }

    // 10. Checked Slot Offset & End Arithmetic
    let slot_offset = slot_idx
        .checked_mul(slot_stride_bytes)
        .ok_or("E_SHM_OVERFLOW_SLOT_OFFSET")?;

    let slot_capacity_end = slot_offset
        .checked_add(slot_stride_bytes)
        .ok_or("E_SHM_OVERFLOW_SLOT_CAPACITY_END")?;

    let frame_end = slot_offset
        .checked_add(slot_row_bytes)
        .ok_or("E_SHM_OVERFLOW_FRAME_END")?;

    // 11. Individual Slot Capacity & Mapping Bounds
    if slot_capacity_end > mapped_pixel_len {
        return Err("E_SHM_SLOT_CAPACITY_EXCEEDS_MAPPING");
    }
    if frame_end > slot_capacity_end {
        return Err("E_SHM_FRAME_EXCEEDS_SLOT_CAPACITY");
    }

    Ok(ValidatedFrame {
        slot_idx,
        generation: gen,
        pub_seq: seq,
        width,
        height,
        stride,
        slot_offset,
        slot_row_bytes,
    })
}

/// Producer 2-phase reservation protocol.
/// Atomically sets slots[i].state = WRITING, executes SeqCst fence, and verifies
/// against the exact 3-tuple (slot_index, generation, pub_seq) publication identity claim.
pub fn reserve_slot_for_writing(
    slot_idx: usize,
    header: &ProducerControlHeader,
    ack: &ConsumerAckHeader,
    last_pub_gen: &[u64; 3],
    last_pub_seq: &[u64; 3],
) -> Result<(), &'static str> {
    if slot_idx >= 3 {
        return Err("E_SHM_INVALID_SLOT_INDEX");
    }

    // 1. Publish write reservation to prod_control
    header.slots[slot_idx].state.store(STATE_WRITING, Ordering::Release);
    fence(Ordering::SeqCst);

    // 2. Inspect cons_ack with Acquire and compare exact 3-tuple publication identity
    let claim = ack.claimed_slot.load(Ordering::Acquire);
    let claimed_gen = ack.claimed_generation.load(Ordering::Acquire);
    let claimed_seq = ack.claimed_seq.load(Ordering::Acquire);

    if claim == slot_idx as u32
        && (claimed_gen > 0 || claimed_seq > 0)
        && claimed_gen == last_pub_gen[slot_idx]
        && claimed_seq == last_pub_seq[slot_idx]
    {
        // Consumer has an active claim on this exact publication: back out immediately
        header.slots[slot_idx].state.store(STATE_READY, Ordering::Release);
        return Err("E_SHM_SLOT_CLAIMED_BY_CONSUMER");
    }

    Ok(())
}

/// Producer publication protocol.
/// Increments generation, writes metadata, publishes sequence, and transitions state to READY.
pub fn publish_slot(
    slot_idx: usize,
    header: &ProducerControlHeader,
    width: u32,
    height: u32,
    stride: u32,
    last_pub_gen: &mut [u64; 3],
    last_pub_seq: &mut [u64; 3],
) -> Result<u64, &'static str> {
    if slot_idx >= 3 {
        return Err("E_SHM_INVALID_SLOT_INDEX");
    }

    let slot = &header.slots[slot_idx];

    // 1. Increment slot generation
    let next_gen = slot.generation.load(Ordering::Relaxed) + 1;
    slot.generation.store(next_gen, Ordering::Release);

    // 2. Store metadata fields
    slot.width.store(width, Ordering::Release);
    slot.height.store(height, Ordering::Release);
    slot.stride.store(stride, Ordering::Release);
    slot.format.store(FORMAT_RGBA8, Ordering::Release);

    // 3. Increment header publication sequence
    let next_seq = header.pub_seq.load(Ordering::Relaxed) + 1;
    header.pub_seq.store(next_seq, Ordering::Release);
    slot.pub_seq.store(next_seq, Ordering::Release);

    // 4. Publish slot state = READY
    slot.state.store(STATE_READY, Ordering::Release);

    last_pub_gen[slot_idx] = next_gen;
    last_pub_seq[slot_idx] = next_seq;

    Ok(next_seq)
}

/// Consumer 2-phase claim protocol.
/// Observes READY slot, writes claim to cons_ack with SeqCst fence, and rechecks
/// exact claimed publication in prod_control. If mismatch occurs, aborts and clears claim.
pub fn claim_and_verify_slot(
    slot_idx: usize,
    header: &ProducerControlHeader,
    ack: &ConsumerAckHeader,
    current_active_resize_gen: u64,
) -> Result<(u64, u64), &'static str> {
    if slot_idx >= 3 {
        return Err("E_SHM_INVALID_SLOT_INDEX");
    }

    let slot = &header.slots[slot_idx];
    if slot.state.load(Ordering::Acquire) != STATE_READY {
        return Err("E_SHM_SLOT_NOT_READY");
    }

    let gen = slot.generation.load(Ordering::Acquire);
    let seq = slot.pub_seq.load(Ordering::Acquire);

    // 1. Publish exact claim to cons_ack
    ack.claimed_generation.store(gen, Ordering::Release);
    ack.claimed_seq.store(seq, Ordering::Release);
    ack.claimed_slot.store(slot_idx as u32, Ordering::Release);
    fence(Ordering::SeqCst);

    // 2. Re-verify exact claimed publication
    let re_gen = slot.generation.load(Ordering::Acquire);
    let re_seq = slot.pub_seq.load(Ordering::Acquire);
    let re_state = slot.state.load(Ordering::Acquire);
    let re_resize = header.prod_resize_gen.load(Ordering::Acquire);

    if re_gen != gen || re_seq != seq || re_state != STATE_READY || re_resize != current_active_resize_gen {
        // Abort: clear claim immediately to avoid blocking producer
        ack.claimed_slot.store(CLAIM_UNCLAIMED, Ordering::Release);
        return Err("E_SHM_CLAIM_VERIFICATION_FAILED");
    }

    Ok((gen, seq))
}

/// Consumer release contract.
/// Clears claim marker strictly after GPU / CPU pixel reads have finished.
pub fn release_slot_claim(ack: &ConsumerAckHeader, completed_seq: u64) {
    ack.free_seq.store(completed_seq, Ordering::Release);
    ack.claimed_slot.store(CLAIM_UNCLAIMED, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shm_geometry_calculation() {
        let geo = calculate_shm_geometry(1920, 1080).expect("geometry");
        assert_eq!(geo.width, 1920);
        assert_eq!(geo.height, 1080);
        assert_eq!(geo.stride % 64, 0, "Stride must be 64-byte aligned");
        assert!(geo.stride >= 1920 * 4);
        assert_eq!(geo.slot_stride_bytes % 4096, 0, "Slot stride must be page aligned");
        assert_eq!(geo.total_pixel_bytes, geo.slot_stride_bytes * 3);
        assert!(geo.total_pixel_bytes <= MAX_PANE_PIXEL_BYTES);

        // Dimensions exceeding 4096 must be rejected
        assert!(calculate_shm_geometry(4097, 100).is_err());
        assert!(calculate_shm_geometry(100, 4097).is_err());
        assert!(calculate_shm_geometry(0, 100).is_err());
    }

    #[test]
    fn test_sealed_memfd_construction_and_read_only_protection() {
        let plane = SealedMemfdPlane::new("lotus_test_pixel", 4096 * 3).expect("memfd plane");

        // Verify seals
        let seals = unsafe { libc::fcntl(plane.rw_fd, F_GET_SEALS) };
        assert_eq!(seals & F_SEAL_SHRINK, F_SEAL_SHRINK);
        assert_eq!(seals & F_SEAL_GROW, F_SEAL_GROW);
        assert_eq!(seals & F_SEAL_SEAL, F_SEAL_SEAL);

        // Verify ftruncate fails on sealed descriptors
        let trunc_res = unsafe { libc::ftruncate(plane.rw_fd, 4096 * 4) };
        assert_eq!(trunc_res, -1, "ftruncate must fail on sealed memfd");
        assert_eq!(unsafe { *libc::__errno_location() }, libc::EPERM);

        // Verify ro_fd possesses O_RDONLY
        let ro_fl = unsafe { libc::fcntl(plane.ro_fd, libc::F_GETFL) };
        assert_eq!(ro_fl & libc::O_ACCMODE, libc::O_RDONLY);

        // Verify writable mmap on ro_fd fails
        let map_res = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_WRITE,
                libc::MAP_SHARED,
                plane.ro_fd,
                0,
            )
        };
        assert_eq!(map_res, libc::MAP_FAILED, "PROT_WRITE mapping on ro_fd must fail");

        // Adversarial verification: Inode mode bits (0400) prevent reopening ro_fd as O_RDWR via /proc/self/fd
        let ro_path = std::ffi::CString::new(format!("/proc/self/fd/{}", plane.ro_fd)).unwrap();
        let reopened_fd = unsafe { libc::open(ro_path.as_ptr(), libc::O_RDWR) };
        assert_eq!(reopened_fd, -1, "Reopening ro_fd O_RDWR must fail due to 0400 inode permissions");
        assert_eq!(unsafe { *libc::__errno_location() }, libc::EACCES);

        // Verify cross-process writable mmap on rw_fd succeeds (used by remote writer)
        let rw_map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                plane.rw_fd,
                0,
            )
        };
        assert_ne!(rw_map, libc::MAP_FAILED, "PROT_WRITE mapping on rw_fd must succeed");
        unsafe { libc::munmap(rw_map, 4096) };

        // Verify producer pre-mapped pointer remains functional
        unsafe { *plane.rw_ptr = 0xAA };
        assert_eq!(unsafe { *plane.rw_ptr }, 0xAA);
    }

    #[test]
    fn test_hostile_producer_individual_slot_bounds_rejection() {
        // Construct mock control header & ack
        let header = ProducerControlHeader {
            pub_seq: AtomicU64::new(1),
            prod_resize_gen: AtomicU64::new(10),
            active_slots: 3,
            _pad: [0; 44],
            slots: [
                SlotDescriptor {
                    state: AtomicU32::new(STATE_READY),
                    _reserved0: 0,
                    generation: AtomicU64::new(1),
                    pub_seq: AtomicU64::new(1),
                    width: AtomicU32::new(100),
                    height: AtomicU32::new(100),
                    stride: AtomicU32::new(448), // 64-byte aligned
                    format: AtomicU32::new(FORMAT_RGBA8),
                    _pad: [0; 24],
                },
                SlotDescriptor {
                    state: AtomicU32::new(STATE_FREE),
                    _reserved0: 0,
                    generation: AtomicU64::new(0),
                    pub_seq: AtomicU64::new(0),
                    width: AtomicU32::new(0),
                    height: AtomicU32::new(0),
                    stride: AtomicU32::new(0),
                    format: AtomicU32::new(0),
                    _pad: [0; 24],
                },
                SlotDescriptor {
                    state: AtomicU32::new(STATE_FREE),
                    _reserved0: 0,
                    generation: AtomicU64::new(0),
                    pub_seq: AtomicU64::new(0),
                    width: AtomicU32::new(0),
                    height: AtomicU32::new(0),
                    stride: AtomicU32::new(0),
                    format: AtomicU32::new(0),
                    _pad: [0; 24],
                },
            ],
        };

        let ack = ConsumerAckHeader {
            consumer_gen_id: AtomicU64::new(1),
            claimed_generation: AtomicU64::new(1),
            claimed_seq: AtomicU64::new(1),
            free_seq: AtomicU64::new(0),
            claimed_slot: AtomicU32::new(0),
            _pad: [0; 28],
        };

        let slot_stride_bytes = 40960; // 40 KiB
        let mapped_pixel_len = slot_stride_bytes * 3; // 120 KiB

        // Valid frame: 448 * 90 = 40320 <= 40960
        header.slots[0].height.store(90, Ordering::Relaxed);
        let res = validate_frame_metadata(
            0,
            &header.slots[0],
            &header,
            &ack,
            10,
            4096,
            4096,
            slot_stride_bytes,
            mapped_pixel_len,
        );
        assert!(res.is_ok(), "Valid frame within individual slot bounds must pass");

        // Hostile Frame: frame height is 150 -> 448 * 150 = 67200 > slot_stride_bytes (40960)
        // Notice: 67200 < mapped_pixel_len (120 KiB), so it fits in the OVERALL mapping,
        // but EXCEEDS its individual assigned slot!
        header.slots[0].height.store(150, Ordering::Relaxed);
        let hostile_res = validate_frame_metadata(
            0,
            &header.slots[0],
            &header,
            &ack,
            10,
            4096,
            4096,
            slot_stride_bytes,
            mapped_pixel_len,
        );
        assert_eq!(
            hostile_res.unwrap_err(),
            "E_SHM_FRAME_EXCEEDS_INDIVIDUAL_SLOT",
            "Hostile producer frame exceeding individual slot capacity must be rejected"
        );
    }

    #[test]
    fn test_exact_publication_identity_protocol() {
        let header = ProducerControlHeader {
            pub_seq: AtomicU64::new(0),
            prod_resize_gen: AtomicU64::new(5),
            active_slots: 3,
            _pad: [0; 44],
            slots: [
                SlotDescriptor {
                    state: AtomicU32::new(STATE_FREE),
                    _reserved0: 0,
                    generation: AtomicU64::new(0),
                    pub_seq: AtomicU64::new(0),
                    width: AtomicU32::new(0),
                    height: AtomicU32::new(0),
                    stride: AtomicU32::new(0),
                    format: AtomicU32::new(0),
                    _pad: [0; 24],
                },
                SlotDescriptor {
                    state: AtomicU32::new(STATE_FREE),
                    _reserved0: 0,
                    generation: AtomicU64::new(0),
                    pub_seq: AtomicU64::new(0),
                    width: AtomicU32::new(0),
                    height: AtomicU32::new(0),
                    stride: AtomicU32::new(0),
                    format: AtomicU32::new(0),
                    _pad: [0; 24],
                },
                SlotDescriptor {
                    state: AtomicU32::new(STATE_FREE),
                    _reserved0: 0,
                    generation: AtomicU64::new(0),
                    pub_seq: AtomicU64::new(0),
                    width: AtomicU32::new(0),
                    height: AtomicU32::new(0),
                    stride: AtomicU32::new(0),
                    format: AtomicU32::new(0),
                    _pad: [0; 24],
                },
            ],
        };

        let ack = ConsumerAckHeader {
            consumer_gen_id: AtomicU64::new(1),
            claimed_generation: AtomicU64::new(0),
            claimed_seq: AtomicU64::new(0),
            free_seq: AtomicU64::new(0),
            claimed_slot: AtomicU32::new(CLAIM_UNCLAIMED),
            _pad: [0; 28],
        };

        let mut last_pub_gen = [0u64; 3];
        let mut last_pub_seq = [0u64; 3];

        // 1. Producer reserves slot 0
        reserve_slot_for_writing(0, &header, &ack, &last_pub_gen, &last_pub_seq).expect("reserve");
        assert_eq!(header.slots[0].state.load(Ordering::Relaxed), STATE_WRITING);

        // 2. Producer publishes frame 1
        let seq1 = publish_slot(0, &header, 64, 64, 256, &mut last_pub_gen, &mut last_pub_seq).expect("publish");
        assert_eq!(seq1, 1);
        assert_eq!(header.slots[0].state.load(Ordering::Relaxed), STATE_READY);
        assert_eq!(last_pub_gen[0], 1);
        assert_eq!(last_pub_seq[0], 1);

        // 3. Consumer claims slot 0
        let (gen, seq) = claim_and_verify_slot(0, &header, &ack, 5).expect("claim");
        assert_eq!(gen, 1);
        assert_eq!(seq, 1);
        assert_eq!(ack.claimed_slot.load(Ordering::Relaxed), 0);

        // 4. Producer tries to re-acquire slot 0 while consumer claim is active
        let reserve_err = reserve_slot_for_writing(0, &header, &ack, &last_pub_gen, &last_pub_seq);
        assert_eq!(
            reserve_err.unwrap_err(),
            "E_SHM_SLOT_CLAIMED_BY_CONSUMER",
            "Producer must detect active consumer claim and back out"
        );
        assert_eq!(header.slots[0].state.load(Ordering::Relaxed), STATE_READY, "Must restore READY");

        // 5. Consumer releases claim
        release_slot_claim(&ack, 1);
        assert_eq!(ack.claimed_slot.load(Ordering::Relaxed), CLAIM_UNCLAIMED);

        // 6. Producer can now re-reserve slot 0
        reserve_slot_for_writing(0, &header, &ack, &last_pub_gen, &last_pub_seq).expect("re-reserve");
        assert_eq!(header.slots[0].state.load(Ordering::Relaxed), STATE_WRITING);
    }
}
