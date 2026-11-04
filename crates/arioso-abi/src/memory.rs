//! The one allocation shape that crosses the boundary.
//!
//! `arioso_free(void *)` is handed a pointer and no length, so the length has to travel
//! with the bytes. Every buffer this library returns is a block laid out as
//!
//! ```text
//! [ usize length ][ payload bytes... ]
//!                    ^ the pointer the caller sees
//! ```
//!
//! which makes the free a constant-time header read instead of a map of live pointers. The
//! allocation and the release both happen inside this library, so the caller's allocator is
//! never asked to free memory it did not allocate — the reason `arioso_free` exists at all.

use std::alloc::{Layout, alloc, dealloc};

/// Room for the length in front of the payload.
const HEADER: usize = std::mem::size_of::<usize>();

/// Alignment of the block, which is also the alignment of the pointer handed out.
const ALIGN: usize = std::mem::align_of::<usize>();

/// The layout for a payload of `len` bytes, or `None` when the size cannot be expressed.
fn layout(len: usize) -> Option<Layout> {
    Layout::from_size_align(HEADER.checked_add(len)?, ALIGN).ok()
}

/// Copy `bytes` into a block `release` can free without being told the length.
///
/// Answers `(null, 0)` when the allocation fails, which the caller reports as a code rather
/// than dereferencing.
///
/// # Safety
///
/// The returned pointer must be released with [`release`] and never with `free(3)`.
pub(crate) unsafe fn copy_out(bytes: &[u8]) -> (*mut u8, usize) {
    let len = bytes.len();
    let Some(layout) = layout(len) else {
        return (std::ptr::null_mut(), 0);
    };
    // Safety: the layout has a non-zero size, so `alloc` is defined for it.
    let base = unsafe { alloc(layout) };
    if base.is_null() {
        return (std::ptr::null_mut(), 0);
    }
    // Safety: `base` was allocated with this layout, whose first `HEADER` bytes hold the
    // length and are aligned for a `usize` by construction.
    unsafe { base.cast::<usize>().write(len) };
    if len > 0 {
        // Safety: the destination has `len` bytes past the header and cannot overlap a
        // slice the caller owns.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(HEADER), len) };
    }
    // Safety: the payload starts one header past a live allocation.
    (unsafe { base.add(HEADER) }, len)
}

/// Release a block from [`copy_out`].
///
/// # Safety
///
/// `pointer` must be a pointer [`copy_out`] returned and must not have been released
/// already. A pointer from anywhere else is undefined behaviour, which the header states.
pub(crate) unsafe fn release(pointer: *mut u8) {
    // Safety: the caller guarantees this pointer came from `copy_out`, so the header is
    // readable and holds the payload length.
    let base = unsafe { pointer.sub(HEADER) };
    // Safety: written by `copy_out` for this block.
    let len = unsafe { base.cast::<usize>().read() };
    let Some(layout) = layout(len) else {
        return;
    };
    // Safety: the block was allocated with exactly this layout.
    unsafe { dealloc(base, layout) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_reads_back_and_releases() {
        let bytes = b"RIFF....WAVE".to_vec();
        // Safety: the block is released below, once, with `release`.
        let (pointer, len) = unsafe { copy_out(&bytes) };
        assert!(!pointer.is_null());
        assert_eq!(len, bytes.len());
        // Safety: `len` bytes at `pointer` are the copy of `bytes`.
        let read = unsafe { std::slice::from_raw_parts(pointer, len) };
        assert_eq!(read, bytes.as_slice());
        // Safety: the pointer came from `copy_out` and is not used afterwards.
        unsafe { release(pointer) };
    }

    #[test]
    fn an_empty_block_is_still_a_real_allocation() {
        // Safety: released below, once.
        let (pointer, len) = unsafe { copy_out(&[]) };
        assert!(!pointer.is_null(), "an empty buffer is still owned");
        assert_eq!(len, 0);
        // Safety: from `copy_out`, released once.
        unsafe { release(pointer) };
    }
}
