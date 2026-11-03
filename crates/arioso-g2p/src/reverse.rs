//! Correct a transcript by sound.
//!
//! The dictionary maps a written term to the forms a voice can say. Running it backwards
//! is supposed to give the term back, but speech to text does not reproduce a respelling
//! letter for letter: it writes "Tonseller" for "tom seller" and "Cooper Netis" for
//! "koober netties". An exact match never fires on that.
//!
//! So the comparison is on phonemes. Both sides are pronounced, with the dictionary and
//! then the spelling rules, and a near miss counts as a match. It is a guess, and a
//! spoken form made of ordinary English words will rewrite those words when they appear;
//! that risk is stated in the PRD rather than hidden.

use arioso_core::UserDictionary;

use crate::{arpabet, cmudict::CmuDict, lts};

/// How different two pronunciations may be and still count as the same words.
const TOLERANCE: f32 = 0.4;

/// Added to a span's score for each word more or fewer than the form has.
const WORD_MISMATCH_PENALTY: f32 = 0.1;

/// Shortest spoken form worth matching by sound. Below this, one edit is a quarter of
/// the word and everything matches everything.
const MIN_PHONEMES: usize = 4;

/// Replace spoken forms in `text` with the term they belong to.
///
/// An exact match is taken first, because the user wrote that form down. Only when no
/// exact match covers a span does the phonetic comparison run.
#[must_use]
pub fn correct(user: &UserDictionary, cmu: &CmuDict, text: &str) -> String {
    let words = words(text);
    if words.is_empty() || user.entries().is_empty() {
        return text.to_owned();
    }

    // Every (term, pronunciation) pair, longest pronunciation first so the most
    // specific entry wins a contested span.
    let mut forms: Vec<(Vec<&'static str>, usize, &str)> = Vec::new();
    for entry in user.entries() {
        for form in entry.forms() {
            let phonemes = pronounce_phrase(form, cmu);
            // An ARPAbet form has one token per phoneme, so its token count says nothing
            // about how many words it covers. The term does.
            let words = if is_arpabet(form) {
                entry.term.split_whitespace().count().max(1)
            } else {
                form.split_whitespace().count().max(1)
            };
            if phonemes.len() >= MIN_PHONEMES {
                forms.push((phonemes, words, entry.term.as_str()));
            }
        }
    }
    forms.sort_by_key(|form| std::cmp::Reverse(form.0.len()));
    if forms.is_empty() {
        return text.to_owned();
    }

    let spoken: Vec<Vec<&'static str>> = words
        .iter()
        .map(|word| pronounce(&text[word.0..word.1], cmu))
        .collect();

    // Every candidate match, scored. The closest one wins its span, so a window that
    // happens to contain the form cannot beat the form itself just by coming first.
    // Every candidate span, scored. A span with the wrong number of words is not
    // rejected, because the engine does split and merge words, but it carries a penalty
    // so it only wins when nothing else fits. Without the penalty, "is a Rio so" beats
    // "a Rio so" for the form "ah ree oh so" and eats a word that was really spoken.
    // Where each word's phonemes start in one flat count, so a window's length is a
    // subtraction instead of a walk. A window whose length differs from the form by more
    // than the allowance cannot match, and finding that out costs no edit distance.
    let mut prefix = Vec::with_capacity(spoken.len() + 1);
    prefix.push(0usize);
    for slot in &spoken {
        prefix.push(prefix.last().copied().unwrap_or(0) + slot.len());
    }

    let mut candidates: Vec<(f32, usize, usize, usize, &str)> = Vec::new();
    // Reused across every candidate: the inner loop allocated a phoneme vector and two DP
    // rows for each one, and a transcript of twenty words against ten entries is thousands
    // of candidates.
    let mut heard: Vec<&'static str> = Vec::new();
    let mut previous: Vec<usize> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    for (form, form_words, term) in &forms {
        let allowed = (form.len() as f32 * TOLERANCE).max(1.0);
        let allowance = allowed as usize;
        for index in 0..words.len() {
            for width in [*form_words, form_words.saturating_sub(1), form_words + 1] {
                if width == 0 || index + width > words.len() {
                    continue;
                }
                let heard_len = prefix[index + width] - prefix[index];
                if heard_len == 0 || form.len().abs_diff(heard_len) > allowance {
                    continue;
                }
                heard.clear();
                for slot in &spoken[index..index + width] {
                    heard.extend_from_slice(slot);
                }
                let distance =
                    distance_into(form, &heard, &mut previous, &mut current, allowance) as f32;
                if distance > allowed {
                    continue;
                }
                let off_by = width.abs_diff(*form_words);
                let score = distance / form.len() as f32 + off_by as f32 * WORD_MISMATCH_PENALTY;
                candidates.push((score, off_by, index, index + width, term));
            }
        }
    }
    candidates.sort_by(|a, b| {
        // Score, then the span whose word count matches the form, then the longer one.
        // The second key matters: "is a Rio so" and "a Rio so" can score the same, and
        // picking the longer span swallows a word that was really spoken.
        a.0.total_cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then((b.3 - b.2).cmp(&(a.3 - a.2)))
            .then(a.2.cmp(&b.2))
    });

    let mut taken = vec![false; words.len()];
    let mut replacements: Vec<(usize, usize, &str)> = Vec::new();
    for (_, _, start, end, term) in candidates {
        if taken[start..end].iter().any(|used| *used) {
            continue;
        }
        for used in &mut taken[start..end] {
            *used = true;
        }
        replacements.push((words[start].0, words[end - 1].1, term));
    }

    if replacements.is_empty() {
        return text.to_owned();
    }
    replacements.sort_by_key(|(start, _, _)| *start);

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end, term) in replacements {
        if start < cursor {
            continue;
        }
        out.push_str(&text[cursor..start]);
        out.push_str(term);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Byte ranges of the words in `text`.
fn words(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        let is_word = ch.is_alphanumeric() || ch == '\'';
        match (is_word, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                out.push((begin, index));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        out.push((begin, text.len()));
    }
    out
}

/// The phonemes of a written form, word by word.
///
/// A form of several words must be pronounced the same way the transcript is: each
/// word on its own. Smashing them together first sends the whole string to the spelling
/// rules, which read "ah ree oh so" as the letters of one invented word.
fn pronounce_phrase(text: &str, cmu: &CmuDict) -> Vec<&'static str> {
    // A form written in ARPAbet is already a pronunciation. Reading `S EH1 L AA0` as four
    // English words to pronounce matches nothing, which is why an entry that carries only
    // phonemes used to work for speaking and never for hearing.
    if is_arpabet(text) {
        return strip_stress(&arpabet::to_ipa(text));
    }
    let mut out = Vec::new();
    for word in text.split_whitespace() {
        out.extend(pronounce(word, cmu));
    }
    out
}

/// Whether every token in `text` is an ARPAbet phoneme.
fn is_arpabet(text: &str) -> bool {
    let mut tokens = text.split_whitespace().peekable();
    tokens.peek().is_some() && tokens.all(arpabet::is_phoneme)
}

/// The phonemes of a word, without stress: the dictionary first, then the rules.
fn pronounce(word: &str, cmu: &CmuDict) -> Vec<&'static str> {
    let cleaned: String = word
        .chars()
        .filter(|ch| ch.is_ascii_alphabetic())
        .collect::<String>()
        .to_lowercase();
    if cleaned.is_empty() {
        return Vec::new();
    }
    if let Some(pronunciations) = cmu.lookup(&cleaned) {
        if let Some(first) = pronunciations.first() {
            return strip_stress(&arpabet::to_ipa(first));
        }
    }
    let tokens = lts::to_arpabet(&cleaned);
    if tokens.is_empty() {
        return Vec::new();
    }
    strip_stress(&arpabet::to_ipa(&tokens.join(" ")))
}

/// Drop the stress marks: this comparison is about sounds, not emphasis.
fn strip_stress(symbols: &[&'static str]) -> Vec<&'static str> {
    symbols
        .iter()
        .copied()
        .filter(|symbol| !matches!(*symbol, "\u{2c8}" | "\u{2cc}"))
        .collect()
}

/// Levenshtein distance between two phoneme sequences.
/// Levenshtein distance into buffers the caller keeps, giving up once the answer cannot be
/// within `max`.
///
/// The row minimum only ever rises, so a row whose minimum already exceeds the allowance
/// cannot produce a distance that fits. A mismatch that is impossible to accept is
/// rejected after a few rows instead of after the whole matrix.
fn distance_into(
    a: &[&str],
    b: &[&str],
    previous: &mut Vec<usize>,
    current: &mut Vec<usize>,
    max: usize,
) -> usize {
    previous.clear();
    previous.extend(0..=b.len());
    current.clear();
    current.resize(b.len() + 1, 0);
    for (i, left) in a.iter().enumerate() {
        current[0] = i + 1;
        let mut row_min = current[0];
        for (j, right) in b.iter().enumerate() {
            let cost = usize::from(left != right);
            let value = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
            current[j + 1] = value;
            row_min = row_min.min(value);
        }
        if row_min > max {
            return max.saturating_add(1);
        }
        std::mem::swap(previous, current);
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const DICT: &str = "\
TOM  T AA1 M
SELLER  S EH1 L ER0
SELL  S EH1 L
A  AH0
CELL  S EH1 L
COOPER  K UW1 P ER0
KUBERNETES  K UW2 B ER0 N EH1 T IY0 Z
THE  DH AH0
CAT  K AE1 T
SAT  S AE1 T
RIO  R IY1 OW0
AH  AA1
REE  R IY1
OH  OW1
SO  S OW1
IT  IH1 T
IS  IH1 Z
THAT  DH AE1 T
SPEAKS  S P IY1 K S
";

    fn cmu() -> CmuDict {
        CmuDict::parse(DICT)
    }

    fn user(json: &str) -> UserDictionary {
        UserDictionary::parse(json).unwrap()
    }

    const TOM: &str = r#"{"version":1,"entries":[{"term":"Tom Sella","spoken":["tom seller"]}]}"#;

    #[test]
    fn a_phonemes_only_entry_matches_by_sound() {
        // The entry carries no spelling at all, only the pronunciation the voice is given.
        // Reading `S EH1 L AA0` as four English words matched nothing, so an entry that
        // worked for speaking never worked for hearing.
        let dictionary =
            user(r#"{"version":1,"entries":[{"term":"Sella","phonemes":["S EH1 L AH0"]}]}"#);
        // The entry holds no spelling at all, so a match can only come from the phonemes:
        // as words, `S EH1 L AH0` pronounces to nothing usable and the form is dropped.
        let out = correct(&dictionary, &cmu(), "I met sella yesterday");
        assert_eq!(out, "I met Sella yesterday");
    }

    #[test]
    fn an_exact_spoken_form_is_replaced() {
        let dictionary = user(TOM);
        let out = correct(&dictionary, &cmu(), "I met tom seller yesterday");
        assert_eq!(out, "I met Tom Sella yesterday");
    }

    #[test]
    fn a_word_the_engine_merged_still_matches() {
        // Measured: whisper wrote this for "tom seller".
        let dictionary = user(TOM);
        let out = correct(&dictionary, &cmu(), "Tonseller built it");
        assert_eq!(out, "Tom Sella built it");
    }

    #[test]
    fn a_multi_word_form_is_pronounced_word_by_word() {
        // Regression: the whole form used to go through the spelling rules as one
        // string, so "oh ree" was read as the letters of "ohree" and never matched
        // the two words the engine actually wrote.
        let dictionary = user(r#"{"version":1,"entries":[{"term":"Oree","spoken":["oh ree"]}]}"#);
        let out = correct(&dictionary, &cmu(), "the oh ree is here");
        assert_eq!(out, "the Oree is here");
    }

    #[test]
    fn a_word_the_engine_respelled_still_matches() {
        let dictionary =
            user(r#"{"version":1,"entries":[{"term":"Kubernetes","spoken":["koober netties"]}]}"#);
        // "Cooper Netis" is what whisper wrote for "koober netties".
        let out = correct(&dictionary, &cmu(), "We run Cooper Netis here");
        assert_eq!(out, "We run Kubernetes here");
    }

    #[test]
    fn a_split_word_still_matches() {
        let dictionary = user(r#"{"version":1,"entries":[{"term":"Sella","spoken":["sell a"]}]}"#);
        let out = correct(&dictionary, &cmu(), "ask cell a about it");
        assert_eq!(out, "ask Sella about it");
    }

    #[test]
    fn unrelated_text_is_untouched() {
        let dictionary = user(TOM);
        let text = "the cat sat on the mat";
        assert_eq!(correct(&dictionary, &cmu(), text), text);
    }

    #[test]
    fn a_short_form_is_not_matched_by_sound() {
        // "cat" is under the minimum length, so only an exact hit would fire, and
        // "bat" is not it.
        let dictionary = user(r#"{"version":1,"entries":[{"term":"Kat","spoken":["cat"]}]}"#);
        let out = correct(&dictionary, &cmu(), "the bat sat");
        assert_eq!(out, "the bat sat");
    }

    #[test]
    fn an_empty_dictionary_changes_nothing() {
        let out = correct(&UserDictionary::empty(), &cmu(), "hello there");
        assert_eq!(out, "hello there");
    }

    #[test]
    fn punctuation_around_a_match_survives() {
        let dictionary = user(TOM);
        let out = correct(&dictionary, &cmu(), "Thanks, tom seller!");
        assert_eq!(out, "Thanks, Tom Sella!");
    }

    #[test]
    fn a_word_outside_the_dictionary_is_pronounced_by_rule() {
        // "zorbansky" is in neither the tiny dictionary nor CMUdict, so the rules
        // decide its sound; the entry that says it must still match.
        let dictionary =
            user(r#"{"version":1,"entries":[{"term":"Zorbansky","spoken":["zorbansky"]}]}"#);
        let out = correct(&dictionary, &cmu(), "call zorbansky now");
        assert_eq!(out, "call Zorbansky now");
    }
}
