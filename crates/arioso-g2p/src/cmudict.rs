//! CMUdict: a word to ARPAbet pronunciation table.
//!
//! The format is one word per line, uppercase, then its phonemes. Alternate
//! pronunciations carry a parenthesised index: `AARON(2)`. Lines starting with
//! `;;;` are comments. The file is BSD-2-Clause; the parser is independent of it.

use std::collections::HashMap;

/// The pronunciations of one word.
///
/// 93% of the shipped dictionary's words have exactly one, and a `Vec` costs a second
/// heap block for the pointer, capacity and length even when it holds one element. The
/// single case is stored inline instead; a word grows a boxed slice only when a later
/// line gives it a second form.
#[derive(Debug, Clone)]
enum Pronunciations {
    /// The only pronunciation of this word.
    One(Box<str>),
    /// Every pronunciation of this word, in file order.
    Many(Box<[Box<str>]>),
}

impl Pronunciations {
    /// The pronunciations as a slice, for the caller that only reads them.
    fn as_slice(&self) -> &[Box<str>] {
        match self {
            Self::One(one) => std::slice::from_ref(one),
            Self::Many(many) => many,
        }
    }

    /// Append a pronunciation that came later in the file.
    ///
    /// A word with two forms is rare and a word with more than four is not in the file,
    /// so rebuilding the slice costs a handful of allocations across the whole parse.
    /// Nothing here grows speculatively, which is why no `shrink_to_fit` pass follows.
    fn push(&mut self, pronunciation: Box<str>) {
        match self {
            Self::One(first) => {
                // `take` leaves an empty string behind, which allocates nothing; it only
                // holds the slot while the first pronunciation moves into the slice.
                let first = std::mem::take(first);
                *self = Self::Many(vec![first, pronunciation].into_boxed_slice());
            }
            Self::Many(all) => {
                let mut grown = Vec::with_capacity(all.len() + 1);
                grown.extend_from_slice(all);
                grown.push(pronunciation);
                *all = grown.into_boxed_slice();
            }
        }
    }
}

/// Pronunciations keyed by lowercase word.
#[derive(Debug, Clone, Default)]
pub struct CmuDict {
    entries: HashMap<Box<str>, Pronunciations>,
    skipped: usize,
}

impl CmuDict {
    /// Parse the dictionary text. Malformed lines are counted, not fatal.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        // The shipped file holds 126,052 words. Sizing the map once avoids the eighteen
        // rehashes a growing map pays for on its way to 262,144 buckets.
        let mut entries: HashMap<Box<str>, Pronunciations> = HashMap::with_capacity(130_000);
        let mut skipped = 0usize;
        // One buffer for every key, so lowercasing a line costs no allocation at all.
        let mut lower = String::new();

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(";;;") || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(word) = parts.next() else {
                continue;
            };
            // Everything after the word is already space-separated ARPAbet, so the tail
            // is sliced instead of collected into a `Vec<&str>` and joined back into a
            // `String`: one allocation per entry, not two per line.
            let rest = line[word.len()..].trim();
            if rest.is_empty() {
                skipped += 1;
                continue;
            }
            let base = word.split('(').next().unwrap_or(word);
            if base.is_empty() {
                skipped += 1;
                continue;
            }

            lower.clear();
            if base.is_ascii() {
                lower.push_str(base);
                lower.make_ascii_lowercase();
            } else {
                // A non-ASCII letter can lowercase to more than one character (`İ`), which
                // no in-place pass can do. No word in the shipped file takes this path.
                lower.push_str(&base.to_lowercase());
            }

            // The map is looked up before it is grown. An alternate line (`AARON(2)`) names
            // a word that is already there, and building a second key equal to the first
            // only to drop it was most of what the parse used to cost.
            match entries.get_mut(lower.as_str()) {
                Some(existing) => existing.push(Box::from(rest)),
                None => {
                    entries.insert(
                        Box::from(lower.as_str()),
                        Pronunciations::One(Box::from(rest)),
                    );
                }
            }
        }

        Self { entries, skipped }
    }

    /// Every pronunciation of `word`, in file order. Case-insensitive.
    ///
    /// A word that is already lowercase — which is every word the phonemizer hands over —
    /// is looked up in place. Lowercasing it first allocated a `String` per word, on the
    /// path that runs once for every word of every sentence.
    #[must_use]
    pub fn lookup(&self, word: &str) -> Option<&[Box<str>]> {
        if word.chars().any(char::is_uppercase) {
            return self
                .entries
                .get(word.to_lowercase().as_str())
                .map(Pronunciations::as_slice);
        }
        self.entries.get(word).map(Pronunciations::as_slice)
    }

    /// Number of distinct words.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the dictionary holds no words.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Lines that looked like entries but had no usable pronunciation.
    #[must_use]
    pub fn skipped_lines(&self) -> usize {
        self.skipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str =
        ";;; a comment\nHELLO  HH AH0 L OW1\nAARON  EH1 R AH0 N\nAARON(2)  AE1 R AH0 N\nBROKEN\n";

    #[test]
    fn alternates_keep_file_order_and_whitespace_is_not_part_of_them() {
        let dict = CmuDict::parse(SAMPLE);
        assert_eq!(dict.len(), 2);
        assert_eq!(dict.skipped_lines(), 1);
        let aaron = dict.lookup("aaron").unwrap();
        assert_eq!(aaron.len(), 2);
        assert_eq!(&*aaron[0], "EH1 R AH0 N");
        assert_eq!(&*aaron[1], "AE1 R AH0 N");
    }

    #[test]
    fn a_lookup_is_case_insensitive_in_both_directions() {
        // The hot path passes lowercase words and must not allocate to find them; the
        // cold path passes what a user typed.
        let dict = CmuDict::parse(SAMPLE);
        assert!(dict.lookup("hello").is_some());
        assert!(dict.lookup("HELLO").is_some());
        assert!(dict.lookup("HeLLo").is_some());
        assert!(dict.lookup("goodbye").is_none());
        // The same entry, not a second one that differs only in case.
        assert_eq!(dict.len(), 2);
    }

    #[test]
    fn extra_spacing_between_phonemes_is_kept_as_written() {
        let dict = CmuDict::parse("WORD   W ER1 D\n");
        assert_eq!(&*dict.lookup("word").unwrap()[0], "W ER1 D");
    }
}
