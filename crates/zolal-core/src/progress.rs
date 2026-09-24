//! Progress reporting and cooperative cancellation.
//!
//! The core is **synchronous by design** — no async runtime. Swift calls it off the main thread
//! and marshals updates back to the UI, which keeps the FFI surface simple and avoids shipping
//! a second scheduler inside the app.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Implemented by the caller to observe progress and request cancellation.
///
/// Called at chunk boundaries, so implementations must be cheap and non-blocking.
pub trait Progress: Send + Sync {
    /// Called as work proceeds. `total` is 0 when the size isn't known yet.
    fn update(&self, done: u64, total: u64);

    /// Polled at chunk boundaries; returning `true` aborts with
    /// [`crate::error::ZolalError::Cancelled`].
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// A [`Progress`] that does nothing. Useful in tests.
pub struct NoProgress;

impl Progress for NoProgress {
    fn update(&self, _done: u64, _total: u64) {}
}

/// Thread-safe [`Progress`] that records the latest counts and supports cancellation.
#[derive(Default)]
pub struct AtomicProgress {
    done: AtomicU64,
    total: AtomicU64,
    cancelled: AtomicBool,
}

impl AtomicProgress {
    /// Create a fresh tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Latest `(done, total)`.
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }

    /// Request cancellation; takes effect at the next chunk boundary.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Progress for AtomicProgress {
    fn update(&self, done: u64, total: u64) {
        self.done.store(done, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}
