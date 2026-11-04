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
//!
//! The dictionary side depends only on the dictionary, so it is compiled once into a
//! [`ReverseIndex`] and reused for every transcript. [`correct`] is the one-shot form.

use arioso_core::UserDictionary;

use crate::{arpabet, cmudict::CmuDict, inflect, lts};

/// How different two pronunciations may be and still count as the same words.
const TOLERANCE: f32 = 0.4;

/// Added to a span's score for each word more or fewer than the form has.
const WORD_MISMATCH_PENALTY: f32 = 0.1;

/// Shortest spoken form worth matching by sound. Below this, one edit is a quarter of
/// the word and everything matches everything.
const MIN_PHONEMES: usize = 4;

/// One spoken form, pronounced and ready to compare.
#[derive(Debug, Clone)]
struct Form {
    /// The sounds of the form, with the stress marks dropped.
    phonemes: Vec<&'static str>,
    /// How many words of a transcript the form covers.
    words: usize,
    /// The term this form belongs to.
    term: String,
}

/// Every spoken form of a user dictionary, pronounced once and ordered once.
///
/// [`correct`] used to re-pronounce every form and re-sort them on every utterance, which
/// is work that depends only on the dictionary. A server builds this once and calls
/// [`ReverseIndex::apply`] per transcript.
#[derive(Debug, Default, Clone)]
pub struct ReverseIndex {
    /// Longest pronunciation first, so the most specific entry wins a contested span.
    forms: Vec<Form>,
}

impl ReverseIndex {
    /// Pronounce and order every form in `user`.
    #[must_use]
    pub fn build(user: &UserDictionary, cmu: &CmuDict) -> Self {
        let mut forms: Vec<Form> = Vec::new();
        let mut cleaned = String::new();
        for entry in user.entries() {
            for form in entry.forms() {
                let mut phonemes = Vec::new();
                pronounce_form(form, cmu, &mut cleaned, &mut phonemes);
                // An ARPAbet form has one token per phoneme, so its token count says
                // nothing about how many words it covers. The term does.
                let words = if is_arpabet(form) {
                    entry.term.split_whitespace().count().max(1)
                } else {
                    form.split_whitespace().count().max(1)
                };
                if phonemes.len() >= MIN_PHONEMES {
                    forms.push(Form {
                        phonemes,
                        words,
                        term: entry.term.clone(),
                    });
                }
            }
        }
        forms.sort_by_key(|form| std::cmp::Reverse(form.phonemes.len()));
        Self { forms }
    }

    /// Whether the dictionary held no form long enough to match by sound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forms.is_empty()
    }

    /// Number of forms that can match.
    #[must_use]
    pub fn len(&self) -> usize {
        self.forms.len()
    }

    /// Replace spoken forms in `text` with the term they belong to.
    ///
    /// An exact match is taken first, because the user wrote that form down. Only when no
    /// exact match covers a span does the phonetic comparison run.
    #[must_use]
    pub fn apply(&self, cmu: &CmuDict, text: &str) -> String {
        let words = words(text);
        if words.is_empty() || self.forms.is_empty() {
            return text.to_owned();
        }

        // Every word's phonemes in one flat buffer, and where each word starts in it, so a
        // window is a slice and its length is a subtraction. A vector per word was one
        // allocation per word of every transcript. Three symbols a word is a typical
        // English word; the buffer grows if it is wrong.
        let mut spoken: Vec<&'static str> = Vec::with_capacity(words.len() * 3);
        let mut starts: Vec<usize> = Vec::with_capacity(words.len() + 1);
        let mut cleaned = String::new();
        for (start, end) in &words {
            starts.push(spoken.len());
            pronounce_into(&text[*start..*end], cmu, &mut cleaned, &mut spoken);
        }
        starts.push(spoken.len());

        // Every candidate span, scored. The closest one wins its span, so a window that
        // happens to contain the form cannot beat the form itself just by coming first. A
        // span with the wrong number of words is not rejected, because the engine does
        // split and merge words, but it carries a penalty so it only wins when nothing else
        // fits. Without the penalty, "is a Rio so" beats "a Rio so" for the form "ah ree oh
        // so" and eats a word that was really spoken.
        //
        // A window whose length differs from the form by more than the allowance cannot
        // match, and finding that out costs no edit distance.
        let mut candidates: Vec<(f32, usize, usize, usize, &str)> = Vec::new();
        // Reused across every candidate: the inner loop allocated a phoneme vector and two DP
        // rows for each one, and a transcript of twenty words against ten entries is thousands
        // of candidates.
        let mut previous: Vec<usize> = Vec::new();
        let mut current: Vec<usize> = Vec::new();
        for form in &self.forms {
            let allowed = (form.phonemes.len() as f32 * TOLERANCE).max(1.0);
            let allowance = allowed as usize;
            for index in 0..words.len() {
                for width in [form.words, form.words.saturating_sub(1), form.words + 1] {
                    if width == 0 || index + width > words.len() {
                        continue;
                    }
                    let heard_len = starts[index + width] - starts[index];
                    if heard_len == 0 || form.phonemes.len().abs_diff(heard_len) > allowance {
                        continue;
                    }
                    let heard = &spoken[starts[index]..starts[index + width]];
                    let distance = distance_into(
                        &form.phonemes,
                        heard,
                        &mut previous,
                        &mut current,
                        allowance,
                    ) as f32;
                    if distance > allowed {
                        continue;
                    }
                    let off_by = width.abs_diff(form.words);
                    let score = distance / form.phonemes.len() as f32
                        + off_by as f32 * WORD_MISMATCH_PENALTY;
                    candidates.push((score, off_by, index, index + width, &form.term));
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
}

/// Replace spoken forms in `text` with the term they belong to.
///
/// A convenience for a caller that corrects one transcript: it builds the index, applies
/// it and drops it. A caller that corrects many should hold a [`ReverseIndex`] instead.
#[must_use]
pub fn correct(user: &UserDictionary, cmu: &CmuDict, text: &str) -> String {
    ReverseIndex::build(user, cmu).apply(cmu, text)
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

/// Append the phonemes of a written form, word by word.
///
/// A form of several words must be pronounced the same way the transcript is: each word on
/// its own. Smashing them together first sends the whole string to the spelling rules, which
/// read "ah ree oh so" as the letters of one invented word.
fn pronounce_form(form: &str, cmu: &CmuDict, cleaned: &mut String, out: &mut Vec<&'static str>) {
    // A form written in ARPAbet is already a pronunciation. Reading `S EH1 L AA0` as four
    // English words to pronounce matches nothing, which is why an entry that carries only
    // phonemes used to work for speaking and never for hearing.
    if is_arpabet(form) {
        arpabet::extend_ipa_unstressed(form, out);
        return;
    }
    for word in form.split_whitespace() {
        pronounce_into(word, cmu, cleaned, out);
    }
}

/// Whether every token in `text` is an ARPAbet phoneme.
fn is_arpabet(text: &str) -> bool {
    let mut tokens = text.split_whitespace().peekable();
    tokens.peek().is_some() && tokens.all(arpabet::is_phoneme)
}

/// Append the phonemes of a word, without stress: the dictionary first, then the rules.
fn pronounce_into(word: &str, cmu: &CmuDict, cleaned: &mut String, out: &mut Vec<&'static str>) {
    // The letters only, lowercased, in the caller's buffer: this used to be a filtered
    // `String` and then a second one for its lowercase, per word.
    cleaned.clear();
    for ch in word.chars() {
        if ch.is_ascii_alphabetic() {
            cleaned.push(ch.to_ascii_lowercase());
        }
    }
    if cleaned.is_empty() {
        return;
    }
    if let Some(pronunciations) = cmu.lookup(cleaned) {
        if let Some(first) = pronunciations.first() {
            arpabet::extend_ipa_unstressed(first, out);
            return;
        }
    }
    // The same fallback the voice uses: a regular suffix on an entry, so a transcript of
    // `avengers` compares against the sounds of `avenger` plus a `z`.
    if let Some(derived) = inflect::derive(cleaned, cmu) {
        arpabet::extend_ipa_unstressed(&derived, out);
        return;
    }
    lts::extend_ipa_unstressed(cleaned, out);
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
