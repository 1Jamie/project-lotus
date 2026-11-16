//! Locked-vsync frame marshalling.
//!
//! A frame may be held across spare vblanks. It may be replaced only by a
//! strictly newer commit. An older buffer is never scanned out again, including
//! when the client is far below the refresh rate and the next image is late.

use std::collections::VecDeque;

/// Ordered commits waiting to be scanned out.
///
/// `pop_next` yields the oldest commit newer than anything already accepted.
/// `None` means hold the frame already on screen. Pushing an older seq is
/// ignored so a late buffer cannot roll the image backwards.
#[derive(Debug)]
pub struct VsyncFrameLatch<T> {
    accepted_seq: u64,
    pending: VecDeque<(u64, T)>,
}

impl<T> Default for VsyncFrameLatch<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> VsyncFrameLatch<T> {
    pub fn new() -> Self {
        Self {
            accepted_seq: 0,
            pending: VecDeque::new(),
        }
    }

    pub fn accepted_seq(&self) -> u64 {
        self.accepted_seq
    }

    /// Commits queued behind the frame already handed to the presenter.
    pub fn waiting(&self) -> usize {
        self.pending.len()
    }

    /// SoftFifo may enqueue every commit; without a cap a fast client (or a
    /// stalled presenter) retains unbounded SHM/DMA frames in anonymous RAM.
    const MAX_PENDING: usize = 3;

    /// Queue `frame` if `seq` is strictly newer than the accepted frame and not
    /// already pending. Returns false when the frame would roll backward.
    /// When the queue is full, the oldest pending frame is dropped (skip-ahead).
    pub fn push(&mut self, seq: u64, frame: T) -> bool {
        if seq == 0 || seq <= self.accepted_seq {
            return false;
        }
        if self.pending.iter().any(|(existing, _)| *existing == seq) {
            return false;
        }
        while self.pending.len() >= Self::MAX_PENDING {
            let _ = self.pending.pop_front();
        }
        let idx = self
            .pending
            .iter()
            .position(|(existing, _)| *existing > seq)
            .unwrap_or(self.pending.len());
        self.pending.insert(idx, (seq, frame));
        true
    }

    /// Oldest pending frame, in commit order. `None` means hold.
    pub fn pop_next(&mut self) -> Option<(u64, T)> {
        let (seq, frame) = self.pending.pop_front()?;
        if seq <= self.accepted_seq {
            return self.pop_next();
        }
        self.accepted_seq = seq;
        Some((seq, frame))
    }
}

/// wl_buffers (or any token) kept alive until a newer frame has been presented.
///
/// Releasing the scanned-out buffer when the next commit arrives lets the
/// client rewrite that memory while a spare vblank still samples it. The
/// following present then shows the queued frame behind that rewrite.
///
/// Once that newer frame is latched, only it stays held. The buffer it
/// replaced is returned in the same present, so a client with two buffers
/// can render the frame after next into the one that just left the screen.
/// A hold does not release the buffer still being scanned out.
#[derive(Debug)]
pub struct VsyncBufferHolds<T> {
    displayed_seq: u64,
    next_seq: u64,
    holds: VecDeque<(u64, T)>,
}

impl<T> Default for VsyncBufferHolds<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> VsyncBufferHolds<T> {
    pub fn new() -> Self {
        Self {
            displayed_seq: 0,
            next_seq: 0,
            holds: VecDeque::new(),
        }
    }

    pub fn displayed_seq(&self) -> u64 {
        self.displayed_seq
    }

    pub fn len(&self) -> usize {
        self.holds.len()
    }

    pub fn latest(&self) -> Option<&T> {
        self.holds.back().map(|(_, frame)| frame)
    }

    pub fn latest_seq(&self) -> u64 {
        self.holds
            .back()
            .map(|(seq, _)| *seq)
            .unwrap_or(self.displayed_seq)
    }

    /// Cap in-flight holds. SoftFifo pins the scanned-out buffer plus at most
    /// a couple queued; without a bound, missed `release_before` calls retain
    /// every wl_buffer forever.
    const MAX_HOLDS: usize = 4;

    pub fn push_new(&mut self, frame: T) -> u64 {
        self.next_seq = self.next_seq.saturating_add(1);
        let seq = self.next_seq;
        self.holds.push_back((seq, frame));
        while self.holds.len() > Self::MAX_HOLDS {
            let _ = self.holds.pop_front();
        }
        seq
    }

    /// `presented_seq` is now on screen. Returns every hold strictly older than
    /// it. The presented frame stays held. A seq older than the current
    /// displayed seq is ignored so a late ack cannot release the image still
    /// being scanned out or walk the sequence backwards.
    pub fn release_before(&mut self, presented_seq: u64) -> Vec<T> {
        if presented_seq == 0 || presented_seq < self.displayed_seq {
            return Vec::new();
        }
        self.displayed_seq = presented_seq;
        let mut released = Vec::new();
        while self
            .holds
            .front()
            .is_some_and(|(seq, _)| *seq < presented_seq)
        {
            released.push(self.holds.pop_front().unwrap().1);
        }
        released
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presents_in_order_and_holds_when_the_next_frame_is_late() {
        let mut latch = VsyncFrameLatch::new();
        // Cap drops the oldest when the queue is deep — push within the cap.
        for seq in 1..=3 {
            assert!(latch.push(seq, seq));
        }
        let mut shown = Vec::new();
        for tick in 0..20 {
            if tick % 4 == 0 {
                let Some((seq, frame)) = latch.pop_next() else {
                    break;
                };
                assert_eq!(seq, frame);
                assert!(shown.last().copied().unwrap_or(0) < seq);
                shown.push(seq);
            } else if latch.waiting() == 0 {
                assert!(latch.pop_next().is_none());
                assert_eq!(latch.accepted_seq(), *shown.last().unwrap());
            } else {
                assert!(latch.waiting() > 0);
                assert_eq!(latch.accepted_seq(), *shown.last().unwrap());
            }
        }
        assert_eq!(shown, vec![1, 2, 3]);
    }

    #[test]
    fn drops_oldest_when_pending_cap_is_hit() {
        let mut latch = VsyncFrameLatch::new();
        for seq in 1..=5 {
            assert!(latch.push(seq, seq));
        }
        assert_eq!(latch.waiting(), 3);
        assert_eq!(latch.pop_next(), Some((3, 3)));
        assert_eq!(latch.pop_next(), Some((4, 4)));
        assert_eq!(latch.pop_next(), Some((5, 5)));
        assert_eq!(latch.pop_next(), None);
    }

    #[test]
    fn rejects_frames_older_than_the_one_already_accepted() {
        let mut latch = VsyncFrameLatch::new();
        assert!(latch.push(1, "a"));
        assert!(latch.push(3, "c"));
        assert_eq!(latch.pop_next(), Some((1, "a")));
        assert!(latch.push(2, "b"));
        assert!(!latch.push(1, "rollback"));
        assert_eq!(latch.pop_next(), Some((2, "b")));
        assert_eq!(latch.pop_next(), Some((3, "c")));
        assert_eq!(latch.pop_next(), None);
        assert!(!latch.push(3, "again"));
        assert_eq!(latch.pop_next(), None);
    }

    #[test]
    fn low_fps_burst_never_walks_backwards() {
        let mut latch = VsyncFrameLatch::new();
        let mut holds = VsyncBufferHolds::new();
        let mut produced = 0u64;
        let mut last_shown = 0u64;
        for tick in 0..60 {
            if tick % 5 == 0 {
                produced += 1;
                let seq = holds.push_new(produced);
                assert!(latch.push(seq, produced));
            }
            if let Some((seq, frame)) = latch.pop_next() {
                assert!(seq > last_shown);
                assert_eq!(frame, seq);
                let released = holds.release_before(seq);
                assert!(released.iter().all(|old| *old < frame));
                assert!(!released.contains(&frame));
                assert_eq!(holds.displayed_seq(), seq);
                last_shown = seq;
            } else if last_shown > 0 {
                let rolled = holds.release_before(last_shown.saturating_sub(1));
                assert!(rolled.is_empty());
                assert_eq!(holds.displayed_seq(), last_shown);
            }
        }
        assert_eq!(last_shown, produced);
        assert_eq!(holds.len(), 1);
    }

    #[test]
    fn double_buffer_frees_the_front_buffer_when_the_back_is_presented() {
        let mut holds = VsyncBufferHolds::new();
        let a = holds.push_new("A");
        assert_eq!(holds.len(), 1);
        // Spare vblank: the front buffer stays pinned. The other slot is free.
        assert!(holds.release_before(0).is_empty());
        assert_eq!(holds.len(), 1);
        assert_eq!(holds.latest(), Some(&"A"));

        let b = holds.push_new("B");
        assert_eq!(holds.len(), 2);
        // Latch B. A is retired in this present, not held across the next vblank.
        let released = holds.release_before(b);
        assert_eq!(released, vec!["A"]);
        assert_eq!(holds.len(), 1);
        assert_eq!(holds.latest(), Some(&"B"));
        assert_eq!(holds.displayed_seq(), b);
        assert_ne!(a, b);

        // Holding B releases nothing, including B itself.
        assert!(holds.release_before(b.saturating_sub(1)).is_empty());
        assert_eq!(holds.len(), 1);
        assert_eq!(holds.latest(), Some(&"B"));

        // The client renders the following frame into the freed buffer.
        let next = holds.push_new("A2");
        let released = holds.release_before(next);
        assert_eq!(released, vec!["B"]);
        assert_eq!(holds.len(), 1);
        assert_eq!(holds.latest(), Some(&"A2"));
        assert_eq!(holds.displayed_seq(), next);
    }
}
