//! What phonemization costs in heap traffic, asserted rather than described.
//!
//! The bounds are measured counts. A change that adds an allocation per phoneme, per
//! word or per dictionary entry fails here and says what it now costs.

use arioso_core::NormalizeOptions;
use arioso_core::alloc_count::{Counting, measure, measure_warm};
use arioso_g2p::cmudict::CmuDict;
use arioso_g2p::{lts, phonemes_for_text};

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// A dictionary big enough to show the per-entry cost without a multi-megabyte file.
fn dictionary(entries: usize) -> String {
    let mut text = String::with_capacity(entries * 24);
    for index in 0..entries {
        text.push_str(&format!("word{index} W ER1 D\n"));
    }
    text.push_str("hello HH AH0 L OW1\n");
    text.push_str("world W ER1 L D\n");
    text.push_str("zorbansky Z AO0 R B AE1 N S K IY0\n");
    text
}

#[test]
fn parsing_costs_a_bounded_amount_per_entry() {
    // The shipped file holds 126,052 words, and the map is sized for that up front, so a
    // smaller corpus would pay the same fixed cost and report a larger per-entry figure.
    let text = dictionary(126_052);
    let (parsed, stats) = measure(|| CmuDict::parse(&text));
    eprintln!("parse 126k entries: {stats:?}");
    assert!(parsed.lookup("hello").is_some());
    let per_entry = stats.allocations / 126_052;
    assert!(
        per_entry <= 4,
        "parsing costs {per_entry} allocations per entry: {stats:?}"
    );
}

#[test]
fn a_two_word_sentence_is_phonemized_in_a_bounded_number_of_allocations() {
    let parsed = CmuDict::parse(&dictionary(2));
    let options = NormalizeOptions::default();
    let (phonemes, stats) =
        measure_warm(|| phonemes_for_text("hello world", &parsed, options, true));
    eprintln!("two words: {stats:?} symbols={}", phonemes.symbols.len());
    assert!(!phonemes.symbols.is_empty());
    assert!(
        stats.allocations <= 28,
        "two words cost {} allocations: {stats:?}",
        stats.allocations
    );
}

#[test]
fn a_spelled_out_word_costs_a_bounded_number_of_allocations() {
    let (symbols, stats) = measure_warm(|| lts::pronounce("zorbansky"));
    eprintln!("lts one word: {stats:?} symbols={}", symbols.len());
    assert!(!symbols.is_empty());
    assert!(
        stats.allocations <= 49,
        "one spelled word costs {} allocations: {stats:?}",
        stats.allocations
    );
}
