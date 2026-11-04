//! Counting the heap traffic of a hot path, in tests.
//!
//! A performance claim is a number or it is a guess. This module supplies a global
//! allocator that counts allocations and the bytes requested, and a [`measure`] helper
//! that reports what one closure cost. A test that says a sentence is phonemized in
//! eighty allocations fails when the count rises, so the number is the regression test
//! rather than a note in a document.
//!
//! The counters are per thread. A single global counter measured the whole process, and
//! `cargo test` runs the tests in one binary on several threads: a test that built a
//! 126,052-entry dictionary string while another one measured put a hundred thousand
//! allocations into that measurement, which then reported 145 where the call cost 7.
//! Counting the calling thread's own calls makes the number the number the test asked for,
//! whatever else is running beside it.
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
    /// Calls this thread made into the allocator.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    /// Bytes this thread requested.
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

/// Count one call and the bytes it asked for, on the calling thread.
///
/// A thread that is tearing down has no counter left to write to, and nothing is measuring
/// it any more, so a failed lookup is not an error.
fn count(size: usize) {
    let _ = ALLOCATIONS.try_with(|calls| calls.set(calls.get() + 1));
    let _ = BYTES.try_with(|bytes| bytes.set(bytes.get() + size));
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
    /// The calling thread's counters as they stand, without measuring anything.
    #[must_use]
    pub fn now() -> Self {
        Self {
            allocations: ALLOCATIONS.try_with(Cell::get).unwrap_or(0),
            bytes: BYTES.try_with(Cell::get).unwrap_or(0),
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

/// Run `f` and report what it allocated.
///
/// The value is returned so a test can keep asserting on the output while measuring it.
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
