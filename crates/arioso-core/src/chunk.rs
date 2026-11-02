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
            word.chars().count() > 1 && !ABBREVIATIONS.contains(&word.to_lowercase().as_str())
        }
    }
}

/// The run of letters just before `text`'s end, lowercased by the caller.
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
    if s.chars().count() <= limit {
        return s.len();
    }

    let mut sentence_end = None;
    let mut space_end = None;
    for (consumed, (index, ch)) in s.char_indices().enumerate() {
        if consumed == limit {
            break;
        }
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
        while used < limit {
            match s[cut..].chars().next() {
                Some(ch) if ch.is_whitespace() => {
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

    s.char_indices()
        .nth(limit)
        .map_or(s.len(), |(index, _)| index)
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
