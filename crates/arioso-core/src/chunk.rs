//! Splitting text so synthesis can stream sentence by sentence.

/// Cut `text` into chunks of at most `max_chars` characters.
///
/// Chunks are slices of the input, so nothing is copied and nothing is lost:
/// concatenating them returns `text` exactly. A cut lands after sentence
/// punctuation when one is in reach, otherwise at a word boundary, and only
/// a word longer than the limit is cut mid-word. A limit of zero means no limit.
#[must_use]
pub fn chunk_text(text: &str, max_chars: usize) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let limit = if max_chars == 0 {
        usize::MAX
    } else {
        max_chars
    };
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let take = best_cut(&text[start..], limit);
        debug_assert!(take > 0, "a chunk must advance");
        chunks.push(&text[start..start + take]);
        start += take;
    }
    chunks
}

/// Words after which a period does not end the sentence.
const ABBREVIATIONS: [&str; 22] = [
    "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "st", "mt", "vs", "etc", "no", "fig", "approx",
    "inc", "ltd", "co", "dept", "univ", "vol", "pp", "cf",
];

/// Split `text` into sentences, keeping the punctuation.
///
/// A sentence ends after `!`, `?`, or a newline, and after `.` or `…` only when
/// the next character is not a digit and the preceding word is neither a known
/// abbreviation nor a single letter. That keeps `3.99`, `p.m.`, `Dr.`, and
/// `U.S.A.` in one piece, and still splits text typed without capitals.
///
/// Closing quotes and brackets stay with their sentence, surrounding whitespace
/// is trimmed, and empty pieces are dropped. This is the granularity streaming
/// synthesis uses: the first sentence can be spoken before the second is read.
#[must_use]
pub fn sentences(text: &str) -> Vec<&str> {
    const CLOSERS: [char; 7] = ['"', '\'', ')', ']', '}', '\u{2019}', '\u{201d}'];
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();

    while let Some((index, ch)) = chars.next() {
        let ends =
            matches!(ch, '.' | '!' | '?' | '\u{2026}' | '\n') && closes_sentence(text, index, ch);
        if !ends {
            continue;
        }
        let mut cut = index + ch.len_utf8();
        while let Some(&(next_index, next)) = chars.peek() {
            if CLOSERS.contains(&next) || matches!(next, '.' | '!' | '?') {
                cut = next_index + next.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        let piece = text[start..cut].trim();
        if !piece.is_empty() {
            pieces.push(piece);
        }
        start = cut;
    }

    let rest = text[start..].trim();
    if !rest.is_empty() {
        pieces.push(rest);
    }
    pieces
}

/// Shortest clause worth its own synthesis call.
///
/// Every cut is a separate call and a separate pause. A comma inside a short phrase is not a
/// place a voice would breathe, so pieces below this are joined to the next one.
pub const CLAUSE_FLOOR: usize = 60;

/// Split `text` into clauses: the unit streaming synthesis speaks.
///
/// A clause ends at a comma, semicolon, colon or dash, and at the sentence terminators
/// [`sentences`] honours. The pieces are then merged until each is at least
/// [`CLAUSE_FLOOR`] characters, so a sentence made of short phrases is not three calls with a
/// pause between each.
///
/// The result is slices of the input, trimmed, and empty pieces are dropped: concatenating
/// them with spaces returns the text.
#[must_use]
pub fn clauses(text: &str) -> Vec<&str> {
    // Spans rather than slices, so the merge below can join two neighbours without pointer
    // arithmetic: a slice of the same string is `&text[start..end]`, and nothing else is.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        if !ends_clause(text, index, ch) {
            continue;
        }
        let cut = index + ch.len_utf8();
        if let Some(span) = trim_span(text, start, cut) {
            spans.push(span);
        }
        start = cut;
    }
    if let Some(span) = trim_span(text, start, text.len()) {
        spans.push(span);
    }

    let mut merged: Vec<&str> = Vec::with_capacity(spans.len());
    let mut current: Option<(usize, usize)> = None;
    for (from, to) in spans {
        let (from, to) = match current {
            Some((first, _)) => (first, to),
            None => (from, to),
        };
        if text[from..to].chars().count() >= CLAUSE_FLOOR {
            merged.push(&text[from..to]);
            current = None;
        } else {
            current = Some((from, to));
        }
    }
    if let Some((from, to)) = current {
        merged.push(&text[from..to]);
    }
    merged
}

/// The span with surrounding whitespace removed, or nothing when it is all whitespace.
fn trim_span(text: &str, mut from: usize, mut to: usize) -> Option<(usize, usize)> {
    while from < to {
        let ch = text[from..to].chars().next()?;
        if !ch.is_whitespace() {
            break;
        }
        from += ch.len_utf8();
    }
    while to > from {
        let ch = text[from..to].chars().next_back()?;
        if !ch.is_whitespace() {
            break;
        }
        to -= ch.len_utf8();
    }
    (from < to).then_some((from, to))
}

/// Whether the character at `index` ends a clause.
fn ends_clause(text: &str, index: usize, ch: char) -> bool {
    match ch {
        ',' | ';' | ':' | '\u{2014}' | '\u{2013}' => {
            // `1,000` and `12:30` are numbers, not pauses: a cut needs a space after it, or
            // the end of the text.
            match text[index + ch.len_utf8()..].chars().next() {
                None => true,
                Some(next) => next.is_whitespace() || is_closing(next),
            }
        }
        '.' | '!' | '?' | '\u{2026}' | '\n' => closes_sentence(text, index, ch),
        _ => false,
    }
}

/// A character that may follow a terminator and still leave it at the end of a clause.
fn is_closing(ch: char) -> bool {
    matches!(ch, '"' | '\'' | ')' | ']' | '}' | '\u{2019}' | '\u{201d}')
}

/// Whether the terminator at `index` ends the sentence.
fn closes_sentence(text: &str, index: usize, ch: char) -> bool {
    if matches!(ch, '!' | '?' | '\n') {
        return true;
    }
    match text[index + ch.len_utf8()..]
        .chars()
        .find(|c| !c.is_whitespace())
    {
        None => true,
        // 3.99, not 3. and 99.
        Some(next) if next.is_numeric() => false,
        // p.m., e.g., U.S.A. all end in a single letter.
        Some(_) => {
            let word = preceding_word(&text[..index]);
            word.chars().count() > 1
                && !ABBREVIATIONS
                    .iter()
                    .any(|short| short.eq_ignore_ascii_case(word))
        }
    }
}

/// The run of letters just before `text`'s end, in the case it was written.
fn preceding_word(text: &str) -> &str {
    let start = text
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_alphanumeric())
        .map_or(0, |(index, ch)| index + ch.len_utf8());
    &text[start..]
}

/// Byte length of the longest acceptable prefix of `s`.
fn best_cut(s: &str, limit: usize) -> usize {
    best_cut_counted(s, limit, &mut 0)
}

/// The same, adding the number of characters read to `examined`.
///
/// The first version counted every character left in the document on every call, to learn
/// what the loop below learns within `limit` characters, which made chunking quadratic in
/// the length of the input. The count here stops at the limit, and the test that asserts a
/// bound on it is the reason the counter exists.
fn best_cut_counted(s: &str, limit: usize, examined: &mut usize) -> usize {
    // The byte index of the character after the limit, or `s.len()` when there is none.
    let mut boundary = s.len();
    for (seen, (index, _)) in s.char_indices().enumerate() {
        *examined += 1;
        if seen == limit {
            boundary = index;
            break;
        }
    }
    if boundary == s.len() {
        return s.len();
    }

    let mut sentence_end = None;
    let mut space_end = None;
    for (index, ch) in s.char_indices() {
        if index >= boundary {
            break;
        }
        *examined += 1;
        let after = index + ch.len_utf8();
        if matches!(ch, '.' | '!' | '?' | '…') {
            sentence_end = Some(after);
        }
        if ch.is_whitespace() {
            space_end = Some(after);
        }
    }

    if let Some(mut cut) = sentence_end {
        // Keep the trailing space with the sentence it follows, so the next
        // chunk does not begin with one.
        let mut used = s[..cut].chars().count();
        *examined += used;
        while used < limit {
            match s[cut..].chars().next() {
                Some(ch) if ch.is_whitespace() => {
                    *examined += 1;
                    cut += ch.len_utf8();
                    used += 1;
                }
                _ => break,
            }
        }
        return cut;
    }

    if let Some(cut) = space_end {
        return cut;
    }

    boundary
}

#[cfg(test)]
mod chunk_tests {
    use super::best_cut_counted;

    #[test]
    fn chunking_a_long_document_reads_a_bounded_number_of_characters() {
        // Two thousand sentences, cut into hundreds of chunks. The first version counted
        // every character left in the document on every call, so the characters read grew
        // with the square of the input. This bound is linear in the document.
        let document = "The quick brown fox jumps over the lazy dog. ".repeat(2000);
        let limit = 200;
        let mut examined = 0;
        let mut start = 0;
        let mut chunks = 0;
        while start < document.len() {
            let take = best_cut_counted(&document[start..], limit, &mut examined);
            assert!(take > 0, "a chunk must advance");
            assert!(
                document[start..start + take].chars().count() <= limit,
                "chunk {chunks} is over the limit"
            );
            start += take;
            chunks += 1;
        }
        let chars = document.chars().count();
        assert_eq!(start, document.len(), "the chunks must cover the document");
        assert!(chunks > 100, "expected many chunks, got {chunks}");
        eprintln!("{chunks} chunks, {examined} characters read for {chars}");
        assert!(
            examined <= 4 * chars,
            "{chunks} chunks read {examined} characters for a {chars} character document"
        );
    }
}

#[cfg(test)]
mod sentence_tests {
    use super::sentences;

    #[test]
    fn splits_at_terminators() {
        assert_eq!(
            sentences("One. Two! Three?"),
            vec!["One.", "Two!", "Three?"]
        );
    }

    #[test]
    fn keeps_text_without_a_terminator() {
        assert_eq!(sentences("No terminator"), vec!["No terminator"]);
    }

    #[test]
    fn splits_at_newlines() {
        assert_eq!(
            sentences("Line one\nLine two"),
            vec!["Line one", "Line two"]
        );
    }

    #[test]
    fn empty_text_has_no_sentences() {
        assert!(sentences("").is_empty());
        assert!(sentences("   \n  ").is_empty());
    }

    #[test]
    fn closing_quotes_stay_with_their_sentence() {
        assert_eq!(sentences("\"Quoted.\" Next."), vec!["\"Quoted.\"", "Next."]);
    }

    #[test]
    fn runs_of_terminators_stay_together() {
        assert_eq!(sentences("Hello... World."), vec!["Hello...", "World."]);
        assert_eq!(sentences("Wait!? Yes."), vec!["Wait!?", "Yes."]);
    }

    #[test]
    fn decimals_do_not_end_a_sentence() {
        assert_eq!(
            sentences("It cost $3.99. Cheap!"),
            vec!["It cost $3.99.", "Cheap!"]
        );
    }

    #[test]
    fn abbreviations_do_not_end_a_sentence() {
        assert_eq!(
            sentences("Dr. Smith arrived at 3.5 p.m. today."),
            vec!["Dr. Smith arrived at 3.5 p.m. today."]
        );
        assert_eq!(
            sentences("Mr. Jones met Mrs. Lee."),
            vec!["Mr. Jones met Mrs. Lee."]
        );
    }

    #[test]
    fn initials_do_not_end_a_sentence() {
        assert_eq!(
            sentences("The U.S.A. is large. Yes."),
            vec!["The U.S.A. is large.", "Yes."]
        );
    }

    #[test]
    fn a_lowercase_start_continues_the_sentence() {
        assert_eq!(
            sentences("see e.g. this one. Then stop."),
            vec!["see e.g. this one.", "Then stop."]
        );
    }

    #[test]
    fn text_without_capitals_still_splits() {
        assert_eq!(
            sentences("hello world. it costs $3.99 today. dr. smith agrees."),
            vec!["hello world.", "it costs $3.99 today.", "dr. smith agrees."]
        );
    }

    #[test]
    fn pieces_cover_the_text_without_the_trimmed_spaces() {
        assert_eq!(sentences("  Alpha.   Beta.  ").join(" "), "Alpha. Beta.");
    }
}
