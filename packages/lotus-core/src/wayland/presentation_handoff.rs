//! Two-stage wp_presentation_feedback queue (staged on commit → latched on pump → presented).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use smithay::wayland::presentation::PresentationFeedbackCallback;

use crate::pacing::PacingPolicy;

pub struct PresentationHandoff {
    /// Per commit, oldest first. Mailbox keeps only the newest batch.
    staged: Mutex<HashMap<u32, VecDeque<Vec<PresentationFeedbackCallback>>>>,
    latched: Mutex<Vec<PresentationFeedbackCallback>>,
    pub vrr_passthrough: AtomicBool,
}

impl Default for PresentationHandoff {
    fn default() -> Self {
        Self {
            staged: Mutex::new(HashMap::new()),
            latched: Mutex::new(Vec::new()),
            vrr_passthrough: AtomicBool::new(false),
        }
    }
}

impl PresentationHandoff {
    pub fn set_policy(&self, policy: PacingPolicy) {
        self.vrr_passthrough.store(
            policy == PacingPolicy::VrrPassthrough,
            Ordering::SeqCst,
        );
    }

    fn fifo(&self) -> bool {
        !self.vrr_passthrough.load(Ordering::Acquire)
    }

    pub fn stage(&self, surface_id: u32, new: Vec<PresentationFeedbackCallback>) {
        if new.is_empty() {
            return;
        }
        let fifo = self.fifo();
        let mut staged = self.staged.lock().unwrap_or_else(|e| e.into_inner());
        let queue = staged.entry(surface_id).or_default();
        if !fifo {
            for batch in queue.drain(..) {
                for fb in batch {
                    fb.discarded();
                }
            }
        }
        queue.push_back(new);
    }

    /// Present the oldest staged batch (locked vsync) or every staged batch (VRR).
    pub fn latch_surface(&self, surface_id: u32) {
        let fifo = self.fifo();
        let mut staged = self.staged.lock().unwrap_or_else(|e| e.into_inner());
        let mut latched = self.latched.lock().unwrap_or_else(|e| e.into_inner());
        let empty = {
            let Some(queue) = staged.get_mut(&surface_id) else {
                return;
            };
            if fifo {
                if let Some(batch) = queue.pop_front() {
                    latched.extend(batch);
                }
            } else {
                for batch in queue.drain(..) {
                    latched.extend(batch);
                }
            }
            queue.is_empty()
        };
        if empty {
            staged.remove(&surface_id);
        }
    }

    /// Drop the newest staged batch. Used when a locked-vsync commit does not
    /// become its own frame (damage-only reuse of the buffer already held).
    pub fn discard_newest(&self, surface_id: u32) {
        let mut staged = self.staged.lock().unwrap_or_else(|e| e.into_inner());
        let empty = {
            let Some(queue) = staged.get_mut(&surface_id) else {
                return;
            };
            if let Some(batch) = queue.pop_back() {
                for fb in batch {
                    fb.discarded();
                }
            }
            queue.is_empty()
        };
        if empty {
            staged.remove(&surface_id);
        }
    }

    /// Drop the oldest staged batch. Used when that commit was not scanned out.
    pub fn discard_oldest(&self, surface_id: u32) {
        let mut staged = self.staged.lock().unwrap_or_else(|e| e.into_inner());
        let empty = {
            let Some(queue) = staged.get_mut(&surface_id) else {
                return;
            };
            if let Some(batch) = queue.pop_front() {
                for fb in batch {
                    fb.discarded();
                }
            }
            queue.is_empty()
        };
        if empty {
            staged.remove(&surface_id);
        }
    }

    pub fn take_latched(&self) -> Vec<PresentationFeedbackCallback> {
        std::mem::take(&mut *self.latched.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

pub type SharedPresentationHandoff = Arc<PresentationHandoff>;
