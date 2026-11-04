//! Text to IPA symbols, the path both speech engines phonemize through.
//!
//! Piper and Kokoro want different symbol tables, but the same symbols: what a word
//! sounds like does not depend on which graph says it. The split into words, the
//! dictionary lookup, the regular suffix and the letter-to-sound fallback live here so
//! the two engines cannot drift into saying the same word differently.

use arioso_core::{NormalizeOptions, normalize_with};

use crate::{CmuDict, arpabet};

/// What phonemization produced, and what it could not say.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Phonemes {
    /// The symbols, with the space symbol between words.
    pub symbols: Vec<&'static str>,
    /// Words absent from the dictionary and pronounced by rule.
    pub guessed: Vec<String>,
    /// Words nothing could pronounce.
    pub missing: Vec<String>,
}

/// Whether a form is written in ARPAbet rather than as a respelling.
///
/// Every token must be a phoneme the table knows, in any case. A capitalised
/// respelling such as "TOM SELLER" fails this test and is looked up as words.
#[must_use]
pub fn is_arpabet(form: &str) -> bool {
    let mut seen = false;
    for token in form.split_whitespace() {
        if !arpabet::is_phoneme(token) {
            return false;
        }
        seen = true;
    }
    seen
}

/// Symbols for a run of text, after normalization.
///
/// A word in the dictionary is spoken from its entry; a regular suffix on an entry is
/// derived rather than guessed; anything else goes through the letter-to-sound rules
/// when `allow_rules` is set, and is reported as guessed. A word with no symbols at all
/// is reported as missing, and leaves no empty word boundary behind.
#[must_use]
pub fn phonemes_for_text(
    text: &str,
    dictionary: &CmuDict,
    options: NormalizeOptions,
    allow_rules: bool,
) -> Phonemes {
    let expanded = normalize_with(text, options);
    let mut phonemes = Phonemes::default();
    for word in
        expanded.split(|c: char| c.is_whitespace() || matches!(c, '-' | '\u{2013}' | '\u{2014}'))
    {
        let cleaned: String = word
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '\'')
            .collect::<String>()
            .to_lowercase();
        if cleaned.is_empty() {
            continue;
        }
        let spoken: Vec<&'static str> = if let Some(pronunciations) = dictionary.lookup(&cleaned) {
            arpabet::to_ipa(&pronunciations[0])
        } else if let Some(derived) = crate::inflect::derive(&cleaned, dictionary) {
            // A regular suffix on an entry: the dictionary holds `avenger` and not
            // `avengers`, `walk` and not `walked`. This is not a guess, so it is not
            // reported as one.
            arpabet::to_ipa(&derived)
        } else {
            // Not in the dictionary and not a regular form, so spell it out. A guessed
            // word is heard and named; a skipped one leaves a hole.
            let guessed = if allow_rules {
                crate::lts::pronounce(&cleaned)
            } else {
                Vec::new()
            };
            if guessed.is_empty() {
                phonemes.missing.push(cleaned);
            } else {
                phonemes.guessed.push(cleaned);
            }
            guessed
        };
        if spoken.is_empty() {
            continue;
        }
        if !phonemes.symbols.is_empty() {
            // Words need the space symbol between them: both engines were trained with
            // word boundaries, and without them the sequence reads as one long word.
            phonemes.symbols.push(" ");
        }
        phonemes.symbols.extend(spoken);
    }
    phonemes
}

/// Symbols for one dictionary form, without the reports.
///
/// An ARPAbet form is read directly; anything else goes through
/// [`phonemes_for_text`], because a respelling is just text.
#[must_use]
pub fn phonemes_for_form(
    form: &str,
    dictionary: &CmuDict,
    options: NormalizeOptions,
    allow_rules: bool,
) -> Vec<&'static str> {
    if is_arpabet(form) {
        arpabet::to_ipa(form)
    } else {
        phonemes_for_text(form, dictionary, options, allow_rules).symbols
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_WORDS: &str = "hello HH AH0 L OW1\nworld W ER1 L D\n";

    fn options() -> NormalizeOptions {
        NormalizeOptions::default()
    }

    #[test]
    fn a_dictionary_word_is_spoken_from_its_entry() {
        let dictionary = CmuDict::parse(TWO_WORDS);
        let phonemes = phonemes_for_text("hello", &dictionary, options(), true);
        assert_eq!(phonemes.symbols, vec!["h", "ə", "l", "ˈ", "o", "ʊ"]);
        assert!(phonemes.guessed.is_empty(), "{phonemes:?}");
        assert!(phonemes.missing.is_empty(), "{phonemes:?}");
    }

    #[test]
    fn words_are_separated_by_one_space() {
        let dictionary = CmuDict::parse(TWO_WORDS);
        let phonemes = phonemes_for_text("hello world", &dictionary, options(), true);
        assert_eq!(
            phonemes
                .symbols
                .iter()
                .filter(|symbol| **symbol == " ")
                .count(),
            1,
            "{phonemes:?}"
        );
    }

    #[test]
    fn a_word_nothing_can_say_is_reported_and_leaves_no_boundary() {
        let dictionary = CmuDict::parse(TWO_WORDS);
        let phonemes = phonemes_for_text("hello zorbansky world", &dictionary, options(), false);
        assert_eq!(phonemes.missing, vec!["zorbansky".to_string()]);
        assert!(
            !phonemes
                .symbols
                .windows(2)
                .any(|pair| pair[0] == " " && pair[1] == " "),
            "no doubled space: {phonemes:?}"
        );
        assert_eq!(phonemes.symbols.last().copied(), Some("d"));
    }

    #[test]
    fn a_regular_suffix_is_derived_not_guessed() {
        let dictionary = CmuDict::parse("avenger AH0 V EH1 N JH ER0\n");
        let phonemes = phonemes_for_text("avengers", &dictionary, options(), true);
        assert!(phonemes.guessed.is_empty(), "{phonemes:?}");
        assert_eq!(phonemes.symbols.last().copied(), Some("z"));
    }

    #[test]
    fn an_arpabet_form_is_read_as_phonemes() {
        let dictionary = CmuDict::parse(TWO_WORDS);
        assert!(is_arpabet("HH AH0 L OW1"));
        assert!(!is_arpabet("TOM SELLER"));
        assert_eq!(
            phonemes_for_form("HH AH0 L OW1", &dictionary, options(), true),
            vec!["h", "ə", "l", "ˈ", "o", "ʊ"]
        );
        // A respelling is words, not phonemes.
        assert!(!phonemes_for_form("hello", &dictionary, options(), true).is_empty());
    }
}
