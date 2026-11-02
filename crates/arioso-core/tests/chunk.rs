//! Splitting text so synthesis can stream sentence by sentence.

use arioso_core::chunk_text;

fn chars(s: &str) -> usize {
    s.chars().count()
}

#[test]
fn short_text_is_one_chunk() {
    assert_eq!(chunk_text("Hello there.", 100), vec!["Hello there."]);
}

#[test]
fn empty_text_has_no_chunks() {
    assert!(chunk_text("", 100).is_empty());
}

#[test]
fn splits_between_sentences() {
    let text = "One. Two. Three.";
    let chunks = chunk_text(text, 10);
    assert_eq!(chunks, vec!["One. Two. ", "Three."]);
}

#[test]
fn no_chunk_exceeds_the_limit_and_nothing_is_lost() {
    let text = "The quick brown fox jumps over the lazy dog. \
                Pack my box with five dozen liquor jugs. \
                How vexingly quick daft zebras jump! \
                Sphinx of black quartz, judge my vow.";
    for limit in [10, 20, 40, 80] {
        let chunks = chunk_text(text, limit);
        assert!(
            chunks.iter().all(|c| chars(c) <= limit),
            "limit {limit}: {:?}",
            chunks.iter().map(|c| chars(c)).collect::<Vec<_>>()
        );
        assert_eq!(chunks.concat(), text, "limit {limit}");
    }
}

#[test]
fn a_sentence_longer_than_the_limit_breaks_at_a_word() {
    let text = "word ".repeat(40);
    let chunks = chunk_text(&text, 30);
    assert!(chunks.iter().all(|c| chars(c) <= 30));
    assert_eq!(chunks.concat(), text);
}

#[test]
fn a_word_longer_than_the_limit_is_cut_on_a_character_boundary() {
    let text = "a".repeat(25);
    let chunks = chunk_text(&text, 10);
    assert_eq!(chunks.len(), 3);
    assert!(chunks.iter().all(|c| chars(c) <= 10));
    assert_eq!(chunks.concat(), text);
}

#[test]
fn multibyte_text_is_never_split_inside_a_character() {
    let text = "שלום עולם. זה מבחן קצר. עוד משפט כאן.";
    for limit in [5, 9, 14] {
        let chunks = chunk_text(text, limit);
        assert!(chunks.iter().all(|c| chars(c) <= limit), "limit {limit}");
        assert_eq!(chunks.concat(), text, "limit {limit}");
    }
}

#[test]
fn a_zero_limit_is_treated_as_no_limit() {
    assert_eq!(chunk_text("Hello. Bye.", 0), vec!["Hello. Bye."]);
}

#[test]
fn question_and_exclamation_end_sentences_too() {
    let chunks = chunk_text("Really? Yes! Fine.", 10);
    assert_eq!(chunks.concat(), "Really? Yes! Fine.");
    assert!(chunks.iter().all(|c| chars(c) <= 10));
}
