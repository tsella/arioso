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
    // One buffer for the word being looked up, and one for its symbols, both reused for
    // every word in the sentence: this used to build two strings per word before it knew
    // whether the word was even in the dictionary.
    let mut cleaned = String::new();
    let mut spoken: Vec<&'static str> = Vec::new();
    for word in
        expanded.split(|c: char| c.is_whitespace() || matches!(c, '-' | '\u{2013}' | '\u{2014}'))
    {
        cleaned.clear();
        let mut lower_needed = false;
        for ch in word.chars() {
            if !(ch.is_alphanumeric() || ch == '\'') {
                continue;
            }
            lower_needed |= !is_lowercase_as_is(ch);
            cleaned.push(ch);
        }
        if lower_needed {
            // `str::to_lowercase` maps a capital sigma at the end of a word to the final
            // form, which a character-by-character pass cannot see. A word already in
            // lower case, which is nearly all of them, pays nothing.
            cleaned = cleaned.to_lowercase();
        }
        if cleaned.is_empty() {
            continue;
        }
        spoken.clear();
        let mut from_rules = false;
        if let Some(pronunciations) = dictionary.lookup(&cleaned) {
            arpabet::extend_ipa(&pronunciations[0], &mut spoken);
        } else if let Some(derived) = crate::inflect::derive(&cleaned, dictionary) {
            // A regular suffix on an entry: the dictionary holds `avenger` and not
            // `avengers`, `walk` and not `walked`. This is not a guess, so it is not
            // reported as one.
            arpabet::extend_ipa(&derived, &mut spoken);
        } else {
            // Not in the dictionary and not a regular form, so spell it out. A guessed
            // word is heard and named; a skipped one leaves a hole.
            from_rules = true;
            if allow_rules {
                crate::lts::extend_ipa(&cleaned, &mut spoken);
            }
        }
        if from_rules {
            // The word is only copied once it is known to be reported, which is the rare
            // case; the lookup itself reads the buffer.
            if spoken.is_empty() {
                phonemes.missing.push(cleaned.clone());
            } else {
                phonemes.guessed.push(cleaned.clone());
            }
        }
        if spoken.is_empty() {
            continue;
        }
        if !phonemes.symbols.is_empty() {
            // Words need the space symbol between them: both engines were trained with
            // word boundaries, and without them the sequence reads as one long word.
            phonemes.symbols.push(" ");
        }
        phonemes.symbols.extend_from_slice(&spoken);
    }
    phonemes
}

/// Whether `ch` lowercases to itself, without building the lowercase form.
fn is_lowercase_as_is(ch: char) -> bool {
    let mut lower = ch.to_lowercase();
    lower.next() == Some(ch) && lower.next().is_none()
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
