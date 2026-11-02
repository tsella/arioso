//! CMUdict: a word to ARPAbet pronunciation table.
//!
//! The format is one word per line, uppercase, then its phonemes. Alternate
//! pronunciations carry a parenthesised index: `AARON(2)`. Lines starting with
//! `;;;` are comments. The file is BSD-2-Clause; the parser is independent of it.

use std::collections::HashMap;

/// Pronunciations keyed by lowercase word.
#[derive(Debug, Clone, Default)]
pub struct CmuDict {
    entries: HashMap<String, Vec<Box<str>>>,
    skipped: usize,
}

impl CmuDict {
    /// Parse the dictionary text. Malformed lines are counted, not fatal.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut entries: HashMap<String, Vec<Box<str>>> = HashMap::new();
        let mut skipped = 0usize;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(";;;") || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(word) = parts.next() else {
                continue;
            };
            let phonemes: Vec<&str> = parts.collect();
            if phonemes.is_empty() {
                skipped += 1;
                continue;
            }
            let base = word.split('(').next().unwrap_or(word);
            if base.is_empty() {
                skipped += 1;
                continue;
            }
            entries
                .entry(base.to_lowercase())
                .or_default()
                .push(phonemes.join(" ").into_boxed_str());
        }

        Self { entries, skipped }
    }

    /// Every pronunciation of `word`, in file order. Case-insensitive.
    #[must_use]
    pub fn lookup(&self, word: &str) -> Option<&[Box<str>]> {
        self.entries.get(&word.to_lowercase()).map(Vec::as_slice)
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
