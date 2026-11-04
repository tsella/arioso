//! Counting the heap traffic of a hot path, in tests.
//!
//! A performance claim is a number or it is a guess. This module supplies a global
//! allocator that counts allocations and the bytes requested, and a [`measure`] helper
//! that reports what one closure cost. A test that says a sentence is phonemized in
//! eighty allocations fails when the count rises, so the number is the regression test
//! rather than a note in a document.
//!
//! The count is per thread. Tests in one binary run on several threads, and the harness,
//! the other tests and their printing allocate while one of them is measuring; a
//! process-wide count charges that work to whichever measurement was open, which is a
//! bound that fails on a busy machine and passes on a quiet one.
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

/// A pass-through allocator that counts what each thread asks for.
#[derive(Debug, Default)]
pub struct Counting;

thread_local! {
    /// What this thread has asked for since it started.
    ///
    /// A measurement is one thread's work, and the counter has to say so. The tests in a
    /// binary run on several threads, and the harness, the other tests and their printing
    /// allocate while one of them is measuring: a process-wide counter charged that to
    /// whichever measurement was open, which made a bound of seventeen fail at eighteen
    /// about a quarter of the time. These are `const`-initialised `Cell`s with no
    /// destructor, so reading them from the allocator is a plain thread-local load and
    /// cannot allocate, recurse or fail.
    static THREAD_ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    static THREAD_BYTES: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        THREAD_ALLOCATIONS.with(|count| count.set(count.get() + 1));
        THREAD_BYTES.with(|count| count.set(count.get() + layout.size()));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        THREAD_ALLOCATIONS.with(|count| count.set(count.get() + 1));
        THREAD_BYTES.with(|count| count.set(count.get() + size));
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        THREAD_ALLOCATIONS.with(|count| count.set(count.get() + 1));
        THREAD_BYTES.with(|count| count.set(count.get() + layout.size()));
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
    /// This thread's counters as they stand, without measuring anything.
    #[must_use]
    pub fn now() -> Self {
        Self {
            allocations: THREAD_ALLOCATIONS.with(Cell::get),
            bytes: THREAD_BYTES.with(Cell::get),
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
/// Two tests may measure at once: the counters are per thread, so one cannot charge its
/// allocations to the other.
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
