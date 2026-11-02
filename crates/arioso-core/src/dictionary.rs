//! The user dictionary: one written term, several spoken forms.
//!
//! Forward, the term is what the caller writes and the spoken forms are what the
//! voice should say. Reverse, any spoken form maps back to the term, so a
//! transcript comes back spelled the way the user spells it.

use serde::Deserialize;
use thiserror::Error;

/// The only schema version this build reads.
pub const SCHEMA_VERSION: u32 = 1;
/// Longest phrase an entry may match, in words.
const MAX_PHRASE_WORDS: usize = 8;

/// Why a dictionary file could not be used.
#[derive(Debug, Error)]
pub enum DictError {
    /// The file is not valid JSON, or has a field this version does not know.
    #[error("parsing dictionary.json: {0}")]
    Json(#[from] serde_json::Error),
    /// The file declares a version this build cannot read.
    #[error("unsupported dictionary version {found}; this build reads version {SCHEMA_VERSION}")]
    Version {
        /// Version found in the file.
        found: u32,
    },
    /// The file could not be read.
    #[error("reading {path}: {source}")]
    Io {
        /// File that failed.
        path: std::path::PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// One entry is invalid.
    #[error("entry {index}: {message}")]
    Entry {
        /// Position in the entries array.
        index: usize,
        /// What is wrong with it.
        message: String,
    },
}

/// One entry as written in the file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// What the caller writes.
    pub term: String,
    /// What the voice should say, tried in order.
    #[serde(default)]
    pub spoken: Vec<String>,
    /// ARPAbet forms, which take precedence over `spoken`.
    #[serde(default)]
    pub phonemes: Vec<String>,
    /// Match the term exactly, including case.
    #[serde(default)]
    pub case_sensitive: bool,
}

impl Entry {
    /// Every form the entry can be spoken as, most precise first.
    #[must_use]
    pub fn forms(&self) -> Vec<&str> {
        let mut forms: Vec<&str> = self.phonemes.iter().map(String::as_str).collect();
        forms.extend(self.spoken.iter().map(String::as_str));
        forms
    }
}

/// A piece of input: either text to process normally, or an entry to speak.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    /// Text with no entry matched.
    Text(&'a str),
    /// An entry that matched here.
    Entry(&'a Entry),
}

/// A compiled pattern: the words of a form, and the entry it belongs to.
#[derive(Debug)]
struct Pattern {
    words: Vec<String>,
    case_sensitive: bool,
    entry: usize,
}

/// The parsed dictionary, with both directions compiled.
#[derive(Debug, Default)]
pub struct UserDictionary {
    entries: Vec<Entry>,
    forward: Vec<Pattern>,
    reverse: Vec<Pattern>,
}

impl UserDictionary {
    /// An empty dictionary, for callers with no file.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse and validate a dictionary file.
    ///
    /// # Errors
    ///
    /// Returns [`DictError`] when the file is unreadable, the version is not
    /// supported, or an entry is invalid.
    pub fn parse(json: &str) -> Result<Self, DictError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            version: u32,
            entries: Vec<Entry>,
        }

        let file: File = serde_json::from_str(json)?;
        if file.version != SCHEMA_VERSION {
            return Err(DictError::Version {
                found: file.version,
            });
        }

        let mut seen: Vec<String> = Vec::new();
        for (index, entry) in file.entries.iter().enumerate() {
            let term = entry.term.trim();
            if term.is_empty() {
                return Err(DictError::Entry {
                    index,
                    message: "term is empty".into(),
                });
            }
            if entry.spoken.is_empty() && entry.phonemes.is_empty() {
                return Err(DictError::Entry {
                    index,
                    message: "needs at least one spoken form or phoneme".into(),
                });
            }
            for form in entry.forms() {
                if form.trim().is_empty() {
                    return Err(DictError::Entry {
                        index,
                        message: "a form is empty".into(),
                    });
                }
                if word_count(form) > MAX_PHRASE_WORDS {
                    return Err(DictError::Entry {
                        index,
                        message: format!("a form is longer than {MAX_PHRASE_WORDS} words"),
                    });
                }
            }

            let key = if entry.case_sensitive {
                term.to_owned()
            } else {
                term.to_lowercase()
            };
            if seen.contains(&key) {
                return Err(DictError::Entry {
                    index,
                    message: format!("duplicate term {term:?}"),
                });
            }
            seen.push(key);
        }

        let mut dictionary = Self {
            entries: file.entries,
            forward: Vec::new(),
            reverse: Vec::new(),
        };

        for (index, entry) in dictionary.entries.iter().enumerate() {
            dictionary.forward.push(Pattern {
                words: words(&entry.term),
                case_sensitive: entry.case_sensitive,
                entry: index,
            });
            for form in entry.forms() {
                dictionary.reverse.push(Pattern {
                    words: words(form),
                    case_sensitive: false,
                    entry: index,
                });
            }
        }
        // Longest first, so "new york" wins over "york".
        dictionary
            .forward
            .sort_by_key(|pattern| std::cmp::Reverse(pattern.words.len()));
        dictionary
            .reverse
            .sort_by_key(|pattern| std::cmp::Reverse(pattern.words.len()));

        Ok(dictionary)
    }

    /// Read a file from disk.
    ///
    /// # Errors
    ///
    /// Returns [`DictError::Io`] when the file cannot be read, or the parse
    /// error when it cannot be used.
    pub fn load(path: &std::path::Path) -> Result<Self, DictError> {
        let text = std::fs::read_to_string(path).map_err(|source| DictError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Whether the dictionary has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every entry, for reporting.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Split `text` into pieces, replacing each matched term with its entry.
    #[must_use]
    pub fn pieces<'a>(&'a self, text: &'a str) -> Vec<Piece<'a>> {
        self.split(text, &self.forward)
    }

    /// Rewrite every spoken form back to its term.
    ///
    /// Apply this to finals, never to a provisional partial: a partial may end
    /// mid-phrase, and a half-matched form would be rewritten wrongly.
    #[must_use]
    pub fn correct(&self, transcript: &str) -> String {
        let mut out = String::with_capacity(transcript.len());
        for piece in self.split(transcript, &self.reverse) {
            match piece {
                Piece::Text(text) => out.push_str(text),
                Piece::Entry(entry) => out.push_str(&entry.term),
            }
        }
        out
    }

    fn split<'a>(&'a self, text: &'a str, patterns: &[Pattern]) -> Vec<Piece<'a>> {
        let tokens = tokens(text);
        let mut pieces = Vec::new();
        let mut cursor = 0;
        let mut index = 0;

        while index < tokens.len() {
            let matched = patterns.iter().find(|pattern| {
                let end = index + pattern.words.len();
                end <= tokens.len()
                    && pattern
                        .words
                        .iter()
                        .zip(&tokens[index..end])
                        .all(|(word, token)| {
                            if pattern.case_sensitive {
                                token.text == word
                            } else {
                                token.text.eq_ignore_ascii_case(word)
                            }
                        })
                    // A phrase may not straddle punctuation: "new, york" is not
                    // "New York". Punctuation after the match is fine.
                    && (1..pattern.words.len()).all(|offset| {
                        text[tokens[index + offset - 1].end..tokens[index + offset].start]
                            .chars()
                            .all(char::is_whitespace)
                    })
            });

            match matched {
                Some(pattern) => {
                    let start = tokens[index].start;
                    let end = tokens[index + pattern.words.len() - 1].end;
                    if start > cursor {
                        pieces.push(Piece::Text(&text[cursor..start]));
                    }
                    pieces.push(Piece::Entry(&self.entries[pattern.entry]));
                    cursor = end;
                    index += pattern.words.len();
                }
                None => index += 1,
            }
        }

        if cursor < text.len() {
            pieces.push(Piece::Text(&text[cursor..]));
        }
        pieces
    }
}

/// A word and where it sits in the input.
#[derive(Debug)]
struct Token<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

/// Runs of letters, digits, and apostrophes. Everything else separates.
fn tokens(text: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        let is_word = ch.is_alphanumeric() || ch == '\'';
        match (is_word, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                tokens.push(Token {
                    text: &text[begin..index],
                    start: begin,
                    end: index,
                });
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        tokens.push(Token {
            text: &text[begin..],
            start: begin,
            end: text.len(),
        });
    }
    tokens
}

/// The words of a form, for pattern matching.
fn words(form: &str) -> Vec<String> {
    tokens(form)
        .into_iter()
        .map(|token| token.text.to_owned())
        .collect()
}

fn word_count(form: &str) -> usize {
    tokens(form).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC: &str = r#"{
        "version": 1,
        "entries": [
            { "term": "arioso", "spoken": ["ah ree oh so", "ar ee oh so"] },
            { "term": "Tom Sella", "spoken": ["tom seller"] },
            { "term": "Sella", "spoken": ["seller"] }
        ]
    }"#;

    #[test]
    fn parses_a_minimal_file() {
        let dictionary = UserDictionary::parse(BASIC).unwrap();
        assert_eq!(dictionary.len(), 3);
        assert!(!dictionary.is_empty());
    }

    #[test]
    fn rejects_an_unsupported_version() {
        let json = r#"{"version": 2, "entries": []}"#;
        assert!(matches!(
            UserDictionary::parse(json),
            Err(DictError::Version { found: 2 })
        ));
    }

    #[test]
    fn rejects_a_duplicate_term() {
        let json = r#"{"version": 1, "entries": [
            {"term": "arioso", "spoken": ["ah ree oh so"]},
            {"term": "Arioso", "spoken": ["ah ree oh so"]}
        ]}"#;
        let error = UserDictionary::parse(json).unwrap_err();
        assert!(error.to_string().contains("duplicate term"), "{error}");
    }

    #[test]
    fn rejects_an_entry_with_nothing_to_say() {
        let json = r#"{"version": 1, "entries": [{"term": "arioso"}]}"#;
        let error = UserDictionary::parse(json).unwrap_err();
        assert!(error.to_string().contains("at least one"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_field() {
        let json = r#"{"version": 1, "entries": [{"term": "a", "spokn": ["x"]}]}"#;
        assert!(matches!(
            UserDictionary::parse(json),
            Err(DictError::Json(_))
        ));
    }

    #[test]
    fn phonemes_come_before_spoken_forms() {
        let json = r#"{"version": 1, "entries": [
            {"term": "arioso", "spoken": ["ah ree oh so"], "phonemes": ["AA R IY OW S OW"]}
        ]}"#;
        let dictionary = UserDictionary::parse(json).unwrap();
        assert_eq!(dictionary.entries()[0].forms()[0], "AA R IY OW S OW");
    }

    #[test]
    fn the_longest_phrase_wins() {
        let dictionary = UserDictionary::parse(BASIC).unwrap();
        let pieces = dictionary.pieces("I met Tom Sella today");
        let matched: Vec<&str> = pieces
            .iter()
            .filter_map(|piece| match piece {
                Piece::Entry(entry) => Some(entry.term.as_str()),
                Piece::Text(_) => None,
            })
            .collect();
        assert_eq!(matched, vec!["Tom Sella"]);
    }

    #[test]
    fn matching_is_case_insensitive_by_default() {
        let dictionary = UserDictionary::parse(BASIC).unwrap();
        let pieces = dictionary.pieces("ARIOSO speaks");
        assert!(matches!(pieces[0], Piece::Entry(_)));
    }

    #[test]
    fn a_case_sensitive_entry_only_matches_its_own_case() {
        let json = r#"{"version": 1, "entries": [
            {"term": "Apple", "spoken": ["apple corp"], "case_sensitive": true}
        ]}"#;
        let dictionary = UserDictionary::parse(json).unwrap();
        assert!(matches!(
            dictionary.pieces("Apple makes phones")[0],
            Piece::Entry(_)
        ));
        assert!(matches!(dictionary.pieces("apple pie")[0], Piece::Text(_)));
    }

    #[test]
    fn a_term_does_not_match_inside_a_longer_word() {
        let json = r#"{"version": 1, "entries": [{"term": "cat", "spoken": ["kat"]}]}"#;
        let dictionary = UserDictionary::parse(json).unwrap();
        let pieces = dictionary.pieces("category");
        assert_eq!(pieces, vec![Piece::Text("category")]);
    }

    #[test]
    fn pieces_keep_the_text_around_a_match() {
        let json = r#"{"version": 1, "entries": [{"term": "arioso", "spoken": ["ah ree oh so"]}]}"#;
        let dictionary = UserDictionary::parse(json).unwrap();
        let pieces = dictionary.pieces("say arioso now");
        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces[0], Piece::Text("say "));
        assert_eq!(pieces[2], Piece::Text(" now"));
    }

    #[test]
    fn correct_rewrites_a_spoken_form_back_to_its_term() {
        let dictionary = UserDictionary::parse(BASIC).unwrap();
        assert_eq!(
            dictionary.correct("i met tom seller yesterday"),
            "i met Tom Sella yesterday"
        );
        assert_eq!(dictionary.correct("ah ree oh so speaks"), "arioso speaks");
    }

    #[test]
    fn correct_prefers_the_longest_form() {
        let dictionary = UserDictionary::parse(BASIC).unwrap();
        assert_eq!(dictionary.correct("tom seller"), "Tom Sella");
        assert_eq!(dictionary.correct("seller"), "Sella");
    }

    #[test]
    fn correct_keeps_punctuation() {
        let dictionary = UserDictionary::parse(BASIC).unwrap();
        assert_eq!(
            dictionary.correct("hello, tom seller. bye."),
            "hello, Tom Sella. bye."
        );
    }

    #[test]
    fn text_without_a_match_is_returned_whole() {
        let dictionary = UserDictionary::parse(BASIC).unwrap();
        assert_eq!(dictionary.correct("nothing here"), "nothing here");
        assert_eq!(
            dictionary.pieces("nothing here"),
            vec![Piece::Text("nothing here")]
        );
    }
}
