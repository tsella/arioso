//! Splitting text so synthesis can stream sentence by sentence.

use arioso_core::{chunk_text, clauses};

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
fn clauses_end_at_commas_and_semicolons() {
    // A clause is what a voice can say in one breath, so the cut is where the text already
    // has a pause: after a comma, a semicolon, a colon, or a full stop.
    let text = "The first part of the sentence runs on for a while, and then it continues \
                after the comma; a semicolon follows, and finally it stops.";
    let pieces = clauses(text);
    // The first piece is short on its own, so it is joined to the clause after it: two cuts,
    // not three.
    assert_eq!(pieces.len(), 2, "{pieces:?}");
    assert!(
        pieces[0].contains("runs on for a while, and then"),
        "{pieces:?}"
    );
    assert!(pieces[1].starts_with("a semicolon follows"), "{pieces:?}");
}

#[test]
fn a_number_is_not_a_clause_boundary() {
    // `1,000` and `12:30` are values, not pauses. Cutting inside them is how a voice says
    // "one" and then "thousand" as two sentences.
    let pieces = clauses("There were 1,000 people at 12:30 in the afternoon.");
    assert_eq!(
        pieces,
        vec!["There were 1,000 people at 12:30 in the afternoon."]
    );
}

#[test]
fn short_clauses_are_joined_to_the_next() {
    // Every cut is a synthesis call and a pause, so a list of two-word phrases must not
    // become a list of two-word utterances.
    let pieces =
        clauses("Yes, no, maybe, and that is the end of a longer clause that stands alone.");
    assert_eq!(pieces.len(), 1, "{pieces:?}");
    assert!(pieces[0].chars().count() > 60);
}

#[test]
fn clauses_cover_the_text_they_came_from() {
    let text = "One clause here, a second one there; and a third that is long enough to \
                stand on its own at the end of the paragraph.";
    let pieces = clauses(text);
    let rejoined: String = pieces.join(" ");
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(squash(&rejoined), squash(text));
}

#[test]
fn question_and_exclamation_end_sentences_too() {
    let chunks = chunk_text("Really? Yes! Fine.", 10);
    assert_eq!(chunks.concat(), "Really? Yes! Fine.");
    assert!(chunks.iter().all(|c| chars(c) <= 10));
}
