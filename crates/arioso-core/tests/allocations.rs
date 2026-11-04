//! What the text path costs in heap traffic, asserted rather than described.
//!
//! Each bound is the measured count with no slack beyond what the test needs to be
//! stable. A change that adds an allocation to a hot path fails here, and the failure
//! says how many it now costs.

use arioso_core::alloc_count::{Counting, measure_warm};
use arioso_core::{NormalizeOptions, chunk_text, normalize_with};

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// A sentence with a full stop and no abbreviation, which is nearly every sentence.
const PLAIN: &str = "The quick brown fox jumps over the lazy dog.";

/// A sentence that really does hold an abbreviation.
const ABBREVIATED: &str = "Dr. Smith arrived at 5 p.m.";

#[test]
fn a_plain_sentence_is_normalized_without_a_copy_of_itself() {
    let options = NormalizeOptions::default();
    let (text, stats) = measure_warm(|| normalize_with(PLAIN, options));
    eprintln!("plain normalize: {stats:?}");
    assert!(text.contains("dog"));
    assert!(
        stats.allocations <= 2,
        "a plain sentence costs {} allocations: {stats:?}",
        stats.allocations
    );
}

#[test]
fn an_abbreviation_is_expanded() {
    let options = NormalizeOptions::default();
    let (text, stats) = measure_warm(|| normalize_with(ABBREVIATED, options));
    eprintln!("abbreviated normalize: {stats:?}");
    assert!(text.to_lowercase().contains("doctor"), "{text}");
    assert!(
        stats.allocations <= 17,
        "an abbreviated sentence costs {} allocations: {stats:?}",
        stats.allocations
    );
}

#[test]
fn a_long_document_is_chunked_without_quadratic_scanning() {
    let document = PLAIN.repeat(200);
    let (chunks, stats) = measure_warm(|| chunk_text(&document, 200));
    eprintln!("chunk 200 sentences: {stats:?} chunks={}", chunks.len());
    assert!(chunks.len() > 1);
    assert!(
        stats.allocations <= 5,
        "chunking costs {} allocations: {stats:?}",
        stats.allocations
    );
}
