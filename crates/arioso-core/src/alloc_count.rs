//! Counting the heap traffic of a hot path, in tests.
//!
//! A performance claim is a number or it is a guess. This module supplies a global
//! allocator that counts allocations and the bytes requested, and a [`measure`] helper
//! that reports what one closure cost. A test that says a sentence is phonemized in
//! eighty allocations fails when the count rises, so the number is the regression test
//! rather than a note in a document.
//!
//! Always compiled, because a test that is not run is not a measurement. The binary
//! declares the allocator once:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOCATOR: arioso_core::alloc_count::Counting = arioso_core::alloc_count::Counting;
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// A pass-through allocator that counts what the calling thread asks for.
#[derive(Debug, Default)]
pub struct Counting;

thread_local! {
    /// What this thread has allocated.
    ///
    /// The counters are per thread because the tests in one binary run on several threads.
    /// A single global count charged each measurement for whatever the tests beside it
    /// allocated, so a bound depended on the runner's scheduling and the suite passed only
    /// with `--test-threads=1`. `const` init matters here: the first access allocates
    /// nothing, so the allocator cannot call itself.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

/// Count one allocation of `bytes` on this thread.
///
/// `try_with` rather than `with`: a thread in teardown has lost its slot, and an allocator
/// that panicked there would abort the process.
fn count(bytes: usize) {
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    let _ = BYTES.try_with(|count| count.set(count.get() + bytes));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count(size);
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
}

/// What one measured closure cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Calls into the allocator, counting a reallocation as one.
    pub allocations: usize,
    /// Bytes requested. A reallocation counts its new size, not its growth.
    pub bytes: usize,
}

impl Stats {
    /// The counters as they stand for this thread, without measuring anything.
    #[must_use]
    pub fn now() -> Self {
        Self {
            allocations: ALLOCATIONS.with(Cell::get),
            bytes: BYTES.with(Cell::get),
        }
    }

    /// The difference between two readings.
    #[must_use]
    pub fn since(self, earlier: Self) -> Self {
        Self {
            allocations: self.allocations.saturating_sub(earlier.allocations),
            bytes: self.bytes.saturating_sub(earlier.bytes),
        }
    }
}

/// Run `f` and report what it allocated on this thread.
///
/// The value is returned so a test can keep asserting on the output while measuring it.
/// Work the closure hands to another thread is not counted: the number is what this call
/// asked the allocator for.
pub fn measure<T>(f: impl FnOnce() -> T) -> (T, Stats) {
    let before = Stats::now();
    let value = f();
    let after = Stats::now();
    (value, after.since(before))
}

/// Run `f` once to warm any lazy state, then measure the second run.
///
/// The first call of a path often fills a cache that later calls read, and measuring the
/// cold call would report the cache being built rather than the path being walked.
pub fn measure_warm<T>(mut f: impl FnMut() -> T) -> (T, Stats) {
    let _ = measure(&mut f);
    measure(f)
}
