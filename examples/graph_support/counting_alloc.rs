//! Benchmark-only global allocator that counts live heap bytes (ITEM-021).
//!
//! Install it with `#[global_allocator]` in an example. It forwards every
//! request to [`System`] and counts, process-wide, the live blocks and their
//! requested sizes (`Layout::size`). It also records the peak of the live
//! byte count since [`Counting::reset_peak`].
//!
//! What it does not count:
//!
//! - allocator headers, alignment padding, size-class rounding or memory
//!   cached by the system allocator. The numbers are requested heap bytes,
//!   not RSS or committed memory;
//! - memory that bypasses Rust's global allocator (the OS, C libraries,
//!   memory maps), stacks and static data.
//!
//! `realloc` is not overridden. The trait default allocates the new block,
//! copies, then frees the old one through this allocator, so both blocks
//! count toward the peak while the data is copied, independent of whether
//! the system allocator could have resized in place.
//!
//! The counters are shared by all threads, so a measurement window is exact
//! only while no other thread allocates or frees.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Live requested heap bytes and live blocks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub bytes: usize,
    pub blocks: usize,
}

pub struct Counting {
    bytes: AtomicUsize,
    blocks: AtomicUsize,
    peak: AtomicUsize,
}

impl Counting {
    pub const fn new() -> Self {
        Self {
            bytes: AtomicUsize::new(0),
            blocks: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    pub fn usage(&self) -> Usage {
        Usage {
            bytes: self.bytes.load(Relaxed),
            blocks: self.blocks.load(Relaxed),
        }
    }

    /// The largest live byte count since the last [`Self::reset_peak`].
    pub fn peak_bytes(&self) -> usize {
        self.peak.load(Relaxed)
    }

    /// Restarts the peak at the current live byte count.
    pub fn reset_peak(&self) {
        self.peak.store(self.bytes.load(Relaxed), Relaxed);
    }

    fn added(&self, size: usize) {
        let live = self.bytes.fetch_add(size, Relaxed).wrapping_add(size);
        self.blocks.fetch_add(1, Relaxed);
        self.peak.fetch_max(live, Relaxed);
    }

    fn removed(&self, size: usize) {
        self.bytes.fetch_sub(size, Relaxed);
        self.blocks.fetch_sub(1, Relaxed);
    }
}

// SAFETY: every block is allocated and freed by `System` with the caller's
// unchanged layout, so `Counting` upholds the `GlobalAlloc` contract exactly
// as `System` does. The bookkeeping only updates atomics, which neither
// allocate, unwind nor touch the blocks.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded with the caller's guarantee that `layout` has a
        // non-zero size.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            self.added(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as in `alloc`.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            self.added(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller guarantees that `ptr` is a live block of this
        // allocator allocated with `layout`. Every such block came from
        // `System.alloc` or `System.alloc_zeroed` with that layout, because
        // the default `realloc` allocates through `Self::alloc`.
        unsafe { System.dealloc(ptr, layout) };
        self.removed(layout.size());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Local instances: exact even while other tests use the global allocator.

    #[test]
    fn alloc_and_dealloc_balance() {
        let counting = Counting::new();
        let layout = Layout::from_size_align(24, 8).unwrap();
        // SAFETY: `layout` has a non-zero size; each block is freed once
        // with its own layout.
        unsafe {
            let a = counting.alloc(layout);
            let b = counting.alloc_zeroed(Layout::from_size_align(100, 16).unwrap());
            assert!(!a.is_null() && !b.is_null());
            assert!((0..100).all(|i| *b.add(i) == 0));
            assert_eq!(
                counting.usage(),
                Usage {
                    bytes: 124,
                    blocks: 2
                }
            );
            assert_eq!(counting.peak_bytes(), 124);
            counting.dealloc(a, layout);
            assert_eq!(
                counting.usage(),
                Usage {
                    bytes: 100,
                    blocks: 1
                }
            );
            counting.dealloc(b, Layout::from_size_align(100, 16).unwrap());
        }
        assert_eq!(counting.usage(), Usage::default());
        assert_eq!(counting.peak_bytes(), 124);
    }

    #[test]
    fn realloc_counts_both_blocks_at_the_peak_and_keeps_the_data() {
        let counting = Counting::new();
        let small = Layout::from_size_align(16, 8).unwrap();
        // SAFETY: as above; `realloc` receives the block's current layout
        // and a non-zero new size, and the result is freed with the new
        // size.
        unsafe {
            let ptr = counting.alloc(small);
            ptr.write_bytes(7, 16);
            let grown = counting.realloc(ptr, small, 64);
            assert!(!grown.is_null());
            assert!((0..16).all(|i| *grown.add(i) == 7));
            assert_eq!(
                counting.usage(),
                Usage {
                    bytes: 64,
                    blocks: 1
                }
            );
            assert_eq!(counting.peak_bytes(), 16 + 64);
            counting.reset_peak();
            assert_eq!(counting.peak_bytes(), 64);
            let grown_layout = Layout::from_size_align(64, 8).unwrap();
            let shrunk = counting.realloc(grown, grown_layout, 8);
            assert_eq!(
                counting.usage(),
                Usage {
                    bytes: 8,
                    blocks: 1
                }
            );
            assert_eq!(counting.peak_bytes(), 64 + 8);
            assert!((0..8).all(|i| *shrunk.add(i) == 7));
            counting.dealloc(shrunk, Layout::from_size_align(8, 8).unwrap());
        }
        assert_eq!(counting.usage(), Usage::default());
    }

    #[test]
    fn reset_peak_restarts_at_the_live_bytes() {
        let counting = Counting::new();
        let layout = Layout::from_size_align(32, 8).unwrap();
        // SAFETY: as above.
        unsafe {
            let a = counting.alloc(layout);
            let b = counting.alloc(layout);
            counting.dealloc(b, layout);
            assert_eq!(counting.peak_bytes(), 64);
            counting.reset_peak();
            assert_eq!(counting.peak_bytes(), 32);
            counting.dealloc(a, layout);
            assert_eq!(counting.peak_bytes(), 32);
        }
        assert_eq!(counting.usage(), Usage::default());
    }
}
