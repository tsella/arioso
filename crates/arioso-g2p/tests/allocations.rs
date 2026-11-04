//! What phonemization costs in heap traffic, asserted rather than described.
//!
//! The bounds are measured counts. A change that adds an allocation per phoneme, per
//! word or per dictionary entry fails here and says what it now costs.

use arioso_core::NormalizeOptions;
use arioso_core::UserDictionary;

use arioso_core::alloc_count::{Counting, measure, measure_warm};
use arioso_g2p::cmudict::CmuDict;
use arioso_g2p::reverse::ReverseIndex;
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
        stats.allocations <= 7,
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
        stats.allocations <= 7,
        "one spelled word costs {} allocations: {stats:?}",
        stats.allocations
    );
}

#[test]
fn a_derived_inflection_costs_a_bounded_number_of_allocations() {
    // `avengers` is not in this dictionary; `avenger` is, so the word is derived rather
    // than guessed. The suffix machinery runs for every word the dictionary does not hold.
    let parsed = CmuDict::parse("avenger AH0 V EH1 N JH ER0\n");
    let options = NormalizeOptions::default();
    let (phonemes, stats) = measure_warm(|| phonemes_for_text("avengers", &parsed, options, true));
    eprintln!("derived word: {stats:?} symbols={}", phonemes.symbols.len());
    assert!(phonemes.guessed.is_empty(), "{phonemes:?}");
    assert!(
        stats.allocations <= 8,
        "a derived word costs {} allocations: {stats:?}",
        stats.allocations
    );
}

/// A dictionary with the words a short transcript needs.
const CORRECTION_DICT: &str = "\
TOM  T AA1 M
SELLER  S EH1 L ER0
MET  M EH1 T
I  AY1
YESTERDAY  Y EH1 S T ER0 D EY2
";

const CORRECTION_USER: &str = r#"{"version":1,"entries":[
    {"term":"Tom Sella","spoken":["tom seller"]},
    {"term":"Sella","spoken":["sell a"]}
]}"#;

#[test]
fn a_correction_reuses_a_compiled_dictionary() {
    // The forms depend only on the dictionary, so they are pronounced and ordered once.
    // `correct` re-did both for every utterance, which is what this bound is about.
    let parsed = CmuDict::parse(CORRECTION_DICT);
    let user = UserDictionary::parse(CORRECTION_USER).unwrap();
    let index = ReverseIndex::build(&user, &parsed);
    assert_eq!(index.len(), 2, "both forms are long enough to match");
    let text = "I met tom seller yesterday";
    let (out, stats) = measure_warm(|| index.apply(&parsed, text));
    eprintln!("correct: {stats:?} out={out}");
    assert_eq!(out, "I met Tom Sella yesterday");
    assert!(
        stats.allocations <= 15,
        "a correction costs {} allocations: {stats:?}",
        stats.allocations
    );
}
