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
