use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use glow::HasContext;
use log::{trace, warn};

use lotus_supervisor::shm::{
    calculate_shm_geometry, claim_and_verify_slot, release_slot_claim, validate_frame_metadata,
    ConsumerAckHeader, ProducerControlHeader, CLAIM_UNCLAIMED, STATE_READY,
};

pub struct RemotePaneConsumer {
    pub ro_fd: RawFd,
    pub ro_ptr: *const u8,
    pub mapped_size: usize,
    pub ack_rw_fd: RawFd,
    pub ack_ptr: *mut ConsumerAckHeader,
    pub active_resize_gen: u64,
    pub last_rendered_seq: u64,
    /// Last-good pixels composited (stretch interim source).
    pub texture: Option<glow::Texture>,
    pub tex_width: u32,
    pub tex_height: u32,
    /// Upload target while a Resize is in flight — not shown until promote.
    staging_texture: Option<glow::Texture>,
    staging_width: u32,
    staging_height: u32,
    /// Frames uploaded to staging since the last resize-gen bump. The first is
    /// usually a cleared FB; the 2nd+ is treated as painted and becomes the new
    /// stretch source (step-through interim).
    staging_frames_since_resize: u32,
    pub max_width: u32,
    pub max_height: u32,
    local_ack_fallback: Option<Box<ConsumerAckHeader>>,
}

// Raw pointers across threads need unsafe impl Send
unsafe impl Send for RemotePaneConsumer {}

impl RemotePaneConsumer {
    pub fn new(
        ro_fd: RawFd,
        mapped_size: usize,
        ack_rw_fd: RawFd,
        max_width: u32,
        max_height: u32,
    ) -> Result<Self, &'static str> {
        if ro_fd < 0 {
            return Err("E_SHM_INVALID_FD");
        }
        if mapped_size == 0 {
            return Err("E_SHM_INVALID_SIZE");
        }

        let ro_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mapped_size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                ro_fd,
                0,
            ) as *const u8
        };

        if ro_ptr == libc::MAP_FAILED as *const u8 {
            return Err("E_SHM_MMAP_FAILED");
        }

        let (ack_ptr, local_ack_fallback) = if ack_rw_fd >= 0 {
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    std::mem::size_of::<ConsumerAckHeader>(),
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    ack_rw_fd,
                    0,
                ) as *mut ConsumerAckHeader
            };
            if ptr == libc::MAP_FAILED as *mut ConsumerAckHeader {
                unsafe { libc::munmap(ro_ptr as *mut libc::c_void, mapped_size); }
                return Err("E_SHM_ACK_MMAP_FAILED");
            }
            unsafe {
                (*ptr).consumer_gen_id.store(1, Ordering::Relaxed);
                (*ptr).claimed_generation.store(0, Ordering::Relaxed);
                (*ptr).claimed_seq.store(0, Ordering::Relaxed);
                (*ptr).free_seq.store(0, Ordering::Relaxed);
                (*ptr).claimed_slot.store(CLAIM_UNCLAIMED, Ordering::Relaxed);
            }
            (ptr, None)
        } else {
            let mut boxed = Box::new(ConsumerAckHeader {
                consumer_gen_id: AtomicU64::new(1),
                claimed_generation: AtomicU64::new(0),
                claimed_seq: AtomicU64::new(0),
                free_seq: AtomicU64::new(0),
                claimed_slot: AtomicU32::new(CLAIM_UNCLAIMED),
                _pad: [0; 28],
            });
            let ptr = boxed.as_mut() as *mut ConsumerAckHeader;
            (ptr, Some(boxed))
        };

        Ok(Self {
            ro_fd,
            ro_ptr,
            mapped_size,
            ack_rw_fd,
            ack_ptr,
            active_resize_gen: 1,
            last_rendered_seq: 0,
            texture: None,
            tex_width: max_width,
            tex_height: max_height,
            staging_texture: None,
            staging_width: 0,
            staging_height: 0,
            staging_frames_since_resize: 0,
            max_width,
            max_height,
            local_ack_fallback,
        })
    }

    pub fn update_active_resize_gen(&mut self, gen: u64) {
        self.active_resize_gen = gen;
        self.staging_frames_since_resize = 0;
    }

    pub fn staging_frames_since_resize(&self) -> u32 {
        self.staging_frames_since_resize
    }

    pub fn ack_header(&self) -> &ConsumerAckHeader {
        unsafe { &*self.ack_ptr }
    }

    /// True when the producer has published a frame newer than `last_rendered_seq`.
    pub fn has_unread_frame(&self) -> bool {
        let prod_hdr = unsafe { &*(self.ro_ptr as *const ProducerControlHeader) };
        prod_hdr.pub_seq.load(Ordering::Acquire) > self.last_rendered_seq
    }

    /// Size of the most recently uploaded frame (staging while holding display, else display).
    pub fn latest_upload_size(&self) -> (u32, u32) {
        if self.staging_width > 0 && self.staging_height > 0 {
            (self.staging_width, self.staging_height)
        } else {
            (self.tex_width, self.tex_height)
        }
    }

    /// Checks the producer control header, claims the latest published frame,
    /// verifies the 15-point untrusted producer validation protocol, and uploads
    /// the verified pixel buffer into an OpenGL texture.
    ///
    /// When `preserve_display` is true (resize in flight), pixels go to a staging
    /// texture so composite can keep stretching the last-good display frame.
    /// Call [`promote_staging_to_display`] on nuclear flip.
    ///
    /// Returns Ok(true) if a new frame was consumed and uploaded, Ok(false) if no new frame.
    pub fn consume_latest_frame(
        &mut self,
        gl: &glow::Context,
        preserve_display: bool,
    ) -> Result<bool, &'static str> {
        let prod_hdr = unsafe { &*(self.ro_ptr as *const ProducerControlHeader) };
        let latest_pub_seq = prod_hdr.pub_seq.load(Ordering::Acquire);

        // Non-blocking early exit: no newer frame available
        if latest_pub_seq <= self.last_rendered_seq {
            return Ok(false);
        }

        let consumer_ack = unsafe { &*self.ack_ptr };

        // Find the slot with the highest published sequence that is in STATE_READY.
        // Ignore slots whose seq is ahead of the producer header — after a respawn
        // CreateSurface resets pub_seq to 0 but may leave prior-life READY slots in
        // the ring; consuming those re-paints the crash-stale frame under chrome.
        let mut target_slot: Option<usize> = None;
        let mut best_seq = self.last_rendered_seq;

        for i in 0..3 {
            let slot = &prod_hdr.slots[i];
            if slot.state.load(Ordering::Acquire) == STATE_READY {
                let seq = slot.pub_seq.load(Ordering::Acquire);
                if seq > latest_pub_seq {
                    continue;
                }
                if seq > best_seq {
                    best_seq = seq;
                    target_slot = Some(i);
                }
            }
        }

        let slot_idx = match target_slot {
            Some(idx) => idx,
            None => return Ok(false),
        };

        // 1. Claim slot under single-claim protocol
        let (_claimed_gen, claimed_seq) = claim_and_verify_slot(
            slot_idx,
            prod_hdr,
            consumer_ack,
            self.active_resize_gen,
        )?;

        // 2. Execute 15-Point Independent Untrusted Producer Validation Protocol
        let geom = match calculate_shm_geometry(self.max_width.max(1), self.max_height.max(1)) {
            Ok(g) => g,
            Err(e) => {
                release_slot_claim(consumer_ack, claimed_seq);
                return Err(e);
            }
        };

        let validated = match validate_frame_metadata(
            slot_idx,
            &prod_hdr.slots[slot_idx],
            prod_hdr,
            consumer_ack,
            self.active_resize_gen,
            self.max_width.max(1),
            self.max_height.max(1),
            geom.slot_stride_bytes,
            geom.total_pixel_bytes,
        ) {
            Ok(v) => v,
            Err(err) => {
                warn!("SHM Consumer: 15-point frame validation failed: {}", err);
                release_slot_claim(consumer_ack, claimed_seq);
                return Err(err);
            }
        };

        // 3. Upload verified frame to OpenGL texture.
        //
        // NEVER set GL_UNPACK_ROW_LENGTH here. LotusPaneContext / Servo / WebRender share
        // this window GL context; a non-zero unpack row length makes Webrender's next
        // texture upload compute src_size with our SHM stride and assert:
        //   src_size <= len * size_of::<T>()  (webrender gl.rs).
        // Instead, tightly pack RGBA rows when SHM stride has padding.
        //
        // While preserve_display: upload to staging so composite keeps last-good.
        // Steady state: upload straight to display (and drop stale staging).
        let hold = preserve_display && self.texture.is_some();
        unsafe {
            let (tex_slot, is_new, prev_w, prev_h) = if hold {
                let is_new = self.staging_texture.is_none();
                let tex = match self.staging_texture {
                    Some(t) => t,
                    None => {
                        let new_tex = gl.create_texture().map_err(|_| "E_GL_CREATE_TEXTURE")?;
                        self.staging_texture = Some(new_tex);
                        new_tex
                    }
                };
                (tex, is_new, self.staging_width, self.staging_height)
            } else {
                let is_new = self.texture.is_none();
                let tex = match self.texture {
                    Some(t) => t,
                    None => {
                        let new_tex = gl.create_texture().map_err(|_| "E_GL_CREATE_TEXTURE")?;
                        self.texture = Some(new_tex);
                        new_tex
                    }
                };
                (tex, is_new, self.tex_width, self.tex_height)
            };

            gl.bind_texture(glow::TEXTURE_2D, Some(tex_slot));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

            let pixel_ptr = self.ro_ptr.add(256 + validated.slot_offset);
            let tight_stride = (validated.width as usize).saturating_mul(4);
            let height = validated.height as usize;
            if tight_stride == 0 || height == 0 || validated.stride < tight_stride {
                release_slot_claim(consumer_ack, claimed_seq);
                gl.bind_texture(glow::TEXTURE_2D, None);
                return Err("E_SHM_BAD_STRIDE");
            }

            // Ensure default unpack state before upload (in case a prior caller leaked).
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);

            let packed: Vec<u8>;
            let upload_slice: &[u8] = if validated.stride == tight_stride {
                std::slice::from_raw_parts(pixel_ptr, tight_stride.saturating_mul(height))
            } else {
                packed = {
                    let mut buf = Vec::with_capacity(tight_stride.saturating_mul(height));
                    for y in 0..height {
                        let row = std::slice::from_raw_parts(
                            pixel_ptr.add(y.saturating_mul(validated.stride)),
                            tight_stride,
                        );
                        buf.extend_from_slice(row);
                    }
                    buf
                };
                packed.as_slice()
            };

            // Reallocate texture surface if dimensions changed or if this is the first upload.
            if is_new || prev_w != validated.width || prev_h != validated.height {
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    validated.width as i32,
                    validated.height as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(upload_slice)),
                );
            } else {
                gl.tex_sub_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    0,
                    0,
                    validated.width as i32,
                    validated.height as i32,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(upload_slice)),
                );
            }
            gl.bind_texture(glow::TEXTURE_2D, None);

            if hold {
                self.staging_width = validated.width;
                self.staging_height = validated.height;
                self.staging_frames_since_resize =
                    self.staging_frames_since_resize.saturating_add(1);
            } else {
                self.tex_width = validated.width;
                self.tex_height = validated.height;
                // Steady-state path: drop any leftover staging from a prior resize.
                if let Some(st) = self.staging_texture.take() {
                    gl.delete_texture(st);
                }
                self.staging_width = 0;
                self.staging_height = 0;
                self.staging_frames_since_resize = 0;
            }
        }

        // 4. Release claim and advance last_rendered_seq
        release_slot_claim(consumer_ack, validated.pub_seq);
        self.last_rendered_seq = validated.pub_seq;

        // Step-through: once we have a painted frame at the new size (2nd+ after
        // resize), promote it to display immediately so the next stretch interim
        // uses fresh pixels — without ever showing the cleared first buffer.
        if hold && self.staging_frames_since_resize >= 2 {
            self.promote_staging_to_display(gl);
        }

        trace!(
            "SHM Consumer: Consumed frame seq {} ({}x{}) from slot {} (preserve_display={}, staging_frames={})",
            validated.pub_seq, validated.width, validated.height, slot_idx, hold,
            self.staging_frames_since_resize
        );

        Ok(true)
    }

    /// Swap staging into display so the new size becomes the stretch source.
    pub fn promote_staging_to_display(&mut self, gl: &glow::Context) {
        let Some(staging) = self.staging_texture.take() else {
            return;
        };
        unsafe {
            if let Some(old) = self.texture.take() {
                gl.delete_texture(old);
            }
        }
        self.texture = Some(staging);
        self.tex_width = self.staging_width;
        self.tex_height = self.staging_height;
        self.staging_width = 0;
        self.staging_height = 0;
        // Keep staging_frames_since_resize so callers know we had painted content.
    }

    /// Promote only when staging has a post-clear painted frame; otherwise drop it.
    pub fn promote_painted_or_discard_staging(&mut self, gl: &glow::Context) {
        if self.staging_frames_since_resize >= 2 {
            self.promote_staging_to_display(gl);
        } else {
            self.discard_staging(gl);
        }
    }

    /// Drop staging without touching display — avoids flashing a cleared FB.
    pub fn discard_staging(&mut self, gl: &glow::Context) {
        if let Some(st) = self.staging_texture.take() {
            unsafe {
                gl.delete_texture(st);
            }
        }
        self.staging_width = 0;
        self.staging_height = 0;
    }

    pub fn get_texture(&self) -> Option<glow::Texture> {
        self.texture
    }

    /// Drop the cached GL texture and align `last_rendered_seq` to the current
    /// producer header so a crash-stale upload cannot be redrawn under cover chrome.
    pub fn invalidate_cached_frame(&mut self, gl: &glow::Context) {
        unsafe {
            if let Some(tex) = self.texture.take() {
                gl.delete_texture(tex);
            }
            if let Some(tex) = self.staging_texture.take() {
                gl.delete_texture(tex);
            }
        }
        self.tex_width = 0;
        self.tex_height = 0;
        self.staging_width = 0;
        self.staging_height = 0;
        self.staging_frames_since_resize = 0;
        let prod_hdr = unsafe { &*(self.ro_ptr as *const ProducerControlHeader) };
        self.last_rendered_seq = prod_hdr.pub_seq.load(Ordering::Acquire);
    }
}

impl Drop for RemotePaneConsumer {
    fn drop(&mut self) {
        if !self.ro_ptr.is_null() && self.ro_ptr != libc::MAP_FAILED as *const u8 {
            unsafe {
                libc::munmap(self.ro_ptr as *mut libc::c_void, self.mapped_size);
            }
        }
        if self.ro_fd >= 0 {
            unsafe {
                libc::close(self.ro_fd);
            }
        }
        if self.local_ack_fallback.is_none() && !self.ack_ptr.is_null() && self.ack_ptr != libc::MAP_FAILED as *mut ConsumerAckHeader {
            unsafe {
                libc::munmap(self.ack_ptr as *mut libc::c_void, std::mem::size_of::<ConsumerAckHeader>());
            }
        }
        if self.ack_rw_fd >= 0 {
            unsafe {
                libc::close(self.ack_rw_fd);
            }
        }
        // GL textures are owned by the window context; host must invalidate before drop.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lotus_supervisor::shm::{publish_slot, reserve_slot_for_writing, SealedMemfdPlane};

    #[test]
    fn test_remote_pane_consumer_lifecycle_and_validation() {
        let width = 200u32;
        let height = 150u32;
        let geom = calculate_shm_geometry(width, height).expect("shm geometry");
        let total_size = 256 + geom.total_pixel_bytes;

        let plane = SealedMemfdPlane::new("test_shm_consumer", total_size).expect("sealed memfd");

        // Initialize producer control header
        let prod_hdr = unsafe { &mut *(plane.rw_ptr as *mut ProducerControlHeader) };
        prod_hdr.pub_seq.store(0, Ordering::Relaxed);
        prod_hdr.prod_resize_gen.store(1, Ordering::Relaxed);
        prod_hdr.active_slots = 3;

        let consumer = RemotePaneConsumer::new(plane.ro_fd, total_size, -1, width, height)
            .expect("RemotePaneConsumer::new");

        assert_eq!(consumer.last_rendered_seq, 0);
        assert_eq!(consumer.ack_header().claimed_slot.load(Ordering::Relaxed), CLAIM_UNCLAIMED);

        // Produce a frame
        let mut last_pub_gen = [0u64; 3];
        let mut last_pub_seq = [0u64; 3];
        let slot_idx = 0usize;

        reserve_slot_for_writing(slot_idx, prod_hdr, consumer.ack_header(), &last_pub_gen, &last_pub_seq)
            .expect("reserve slot");

        let pub_seq = publish_slot(
            slot_idx,
            prod_hdr,
            width,
            height,
            geom.stride as u32,
            &mut last_pub_gen,
            &mut last_pub_seq,
        )
        .expect("publish slot");

        // Verify consumer claim and 15-point metadata protocol
        let (_claimed_gen, claimed_seq) = claim_and_verify_slot(
            slot_idx,
            prod_hdr,
            consumer.ack_header(),
            consumer.active_resize_gen,
        )
        .expect("claim and verify");

        assert_eq!(claimed_seq, pub_seq);

        let validated = validate_frame_metadata(
            slot_idx,
            &prod_hdr.slots[slot_idx],
            prod_hdr,
            consumer.ack_header(),
            consumer.active_resize_gen,
            width,
            height,
            geom.slot_stride_bytes,
            geom.total_pixel_bytes,
        )
        .expect("validate frame metadata");

        assert_eq!(validated.width, width);
        assert_eq!(validated.height, height);
        assert_eq!(validated.stride, geom.stride);
        assert_eq!(validated.pub_seq, pub_seq);

        release_slot_claim(consumer.ack_header(), pub_seq);
        assert_eq!(consumer.ack_header().claimed_slot.load(Ordering::Acquire), CLAIM_UNCLAIMED);
    }

    #[test]
    fn test_remote_pane_consumer_rejects_corrupted_frame() {
        let width = 100u32;
        let height = 100u32;
        let geom = calculate_shm_geometry(width, height).expect("shm geometry");
        let total_size = 256 + geom.total_pixel_bytes;

        let plane = SealedMemfdPlane::new("test_shm_corrupt", total_size).expect("sealed memfd");
        let prod_hdr = unsafe { &mut *(plane.rw_ptr as *mut ProducerControlHeader) };
        prod_hdr.pub_seq.store(0, Ordering::Relaxed);
        prod_hdr.prod_resize_gen.store(1, Ordering::Relaxed);
        prod_hdr.active_slots = 3;

        let consumer = RemotePaneConsumer::new(plane.ro_fd, total_size, -1, width, height)
            .expect("RemotePaneConsumer::new");

        // Attempting to validate a slot that is not in STATE_READY must fail
        assert_eq!(
            claim_and_verify_slot(0, prod_hdr, consumer.ack_header(), 1),
            Err("E_SHM_SLOT_NOT_READY")
        );
    }
}
