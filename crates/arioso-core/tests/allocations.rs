//! What the text path costs in heap traffic, asserted rather than described.
//!
//! Each bound is the measured count with no slack beyond what the test needs to be
//! stable. A change that adds an allocation to a hot path fails here, and the failure
//! says how many it now costs.

use arioso_core::alloc_count::{Counting, measure_warm};
use arioso_core::{NormalizeOptions, chunk_text, normalize_with, sentences};

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
        stats.allocations <= 1,
        "a plain sentence costs {} allocations: {stats:?}",
        stats.allocations
    );
    assert!(
        stats.bytes <= 44,
        "a plain sentence requests {} bytes: {stats:?}",
        stats.bytes
    );
}

#[test]
fn an_abbreviation_is_expanded() {
    let options = NormalizeOptions::default();
    let (text, stats) = measure_warm(|| normalize_with(ABBREVIATED, options));
    eprintln!("abbreviated normalize: {stats:?}");
    assert!(text.to_lowercase().contains("doctor"), "{text}");
    assert!(
        stats.allocations <= 6,
        "an abbreviated sentence costs {} allocations: {stats:?}",
        stats.allocations
    );
    assert!(
        stats.bytes <= 393,
        "an abbreviated sentence requests {} bytes: {stats:?}",
        stats.bytes
    );
}

#[test]
fn a_sentence_of_numbers_is_spoken_without_a_string_per_candidate() {
    // A thousands separator, a currency amount, and a unit: every scan that used to build
    // a temporary `String` to compare one to four characters against a table.
    const NUMBERED: &str = "There are 3,500 units at $4.99 each.";
    let options = NormalizeOptions::default();
    let (text, stats) = measure_warm(|| normalize_with(NUMBERED, options));
    eprintln!("numbered normalize: {stats:?}");
    assert_eq!(
        text,
        "There are three thousand five hundred units at four dollars and ninety nine cents each."
    );
    assert!(
        stats.allocations <= 6,
        "a numbered sentence costs {} allocations: {stats:?}",
        stats.allocations
    );
    assert!(
        stats.bytes <= 528,
        "a numbered sentence requests {} bytes: {stats:?}",
        stats.bytes
    );
}

#[test]
fn an_ordinal_and_an_attached_unit_are_spoken_without_temporary_strings() {
    const MEASURED: &str = "The 21st box is 5kg.";
    let options = NormalizeOptions::default();
    let (text, stats) = measure_warm(|| normalize_with(MEASURED, options));
    eprintln!("ordinal and unit normalize: {stats:?}");
    assert_eq!(text, "The twenty first box is five kilograms.");
    assert!(
        stats.allocations <= 4,
        "an ordinal and a unit cost {} allocations: {stats:?}",
        stats.allocations
    );
    assert!(
        stats.bytes <= 208,
        "an ordinal and a unit request {} bytes: {stats:?}",
        stats.bytes
    );
}

#[test]
fn sentence_splitting_does_not_lowercase_the_word_before_every_period() {
    // Two hundred periods, none of them after an abbreviation. The old `closes_sentence`
    // allocated a lowercase copy of the preceding word at each one.
    let document = PLAIN.repeat(200);
    let (pieces, stats) = measure_warm(|| sentences(&document));
    eprintln!("sentences 200: {stats:?} pieces={}", pieces.len());
    assert_eq!(pieces.len(), 200);
    assert!(
        stats.allocations <= 7,
        "splitting costs {} allocations: {stats:?}",
        stats.allocations
    );
    assert!(
        stats.bytes <= 8_128,
        "splitting requests {} bytes: {stats:?}",
        stats.bytes
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
    assert!(
        stats.bytes <= 1_984,
        "chunking requests {} bytes: {stats:?}",
        stats.bytes
    );
}
