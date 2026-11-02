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

/// Split `text` into sentences, keeping the punctuation.
///
/// A sentence ends after `.`, `!`, `?`, or `…`, together with any closing
/// quotes or brackets that follow, or at a newline. Surrounding whitespace is
/// trimmed, and empty pieces are dropped. This is the granularity streaming
/// synthesis uses: the first sentence can be spoken before the second is read.
#[must_use]
pub fn sentences(text: &str) -> Vec<&str> {
    const CLOSERS: [char; 7] = ['"', '\'', ')', ']', '}', '\u{2019}', '\u{201d}'];
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();

    while let Some((index, ch)) = chars.next() {
        let ends = matches!(ch, '.' | '!' | '?' | '\u{2026}' | '\n');
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
    fn pieces_cover_the_text_without_the_trimmed_spaces() {
        assert_eq!(sentences("  Alpha.   Beta.  ").join(" "), "Alpha. Beta.");
    }
}
