//! A lock-free ring of samples between the capture thread and the playback thread.
//!
//! One writer (the capture callback) and one reader (the playback callback): neither ever waits for the
//! other, takes a lock or allocates, so neither audio thread can be held up. The writer only moves
//! `written`, the reader only moves `read`; both are running totals of samples, never wrapped.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Ring {
    samples: UnsafeCell<Box<[f32]>>,
    written: AtomicUsize,
    read: AtomicUsize,
}

// One thread writes and one reads, and they never touch the same sample: the reader stays behind
// `written`, the writer stays within `read + capacity`.
unsafe impl Sync for Ring {}

impl Ring {
    pub fn new(capacity: usize) -> Ring {
        Ring { samples: UnsafeCell::new(vec![0.0; capacity].into_boxed_slice()), written: AtomicUsize::new(0), read: AtomicUsize::new(0) }
    }

    fn capacity(&self) -> usize {
        unsafe { (&*self.samples.get()).len() }
    }

    /// Samples waiting to be read.
    pub fn len(&self) -> usize {
        self.written.load(Ordering::Acquire) - self.read.load(Ordering::Acquire)
    }

    /// The writer: put samples in. Returns how many fitted (the rest are dropped: the reader is behind).
    pub fn push(&self, data: &[f32]) -> usize {
        let written = self.written.load(Ordering::Relaxed);
        let free = self.capacity() - (written - self.read.load(Ordering::Acquire));
        let n = data.len().min(free);
        let samples = unsafe { &mut *self.samples.get() };
        for (i, &s) in data[..n].iter().enumerate() {
            samples[(written + i) % samples.len()] = s;
        }
        self.written.store(written + n, Ordering::Release);
        n
    }

    /// The reader: the sample `offset` places ahead of the read position (it must be below `len`).
    pub fn peek(&self, offset: usize) -> f32 {
        let samples = unsafe { &*self.samples.get() };
        samples[(self.read.load(Ordering::Relaxed) + offset) % samples.len()]
    }

    /// The reader: move past `n` samples.
    pub fn skip(&self, n: usize) {
        self.read.fetch_add(n, Ordering::Release);
    }
}
