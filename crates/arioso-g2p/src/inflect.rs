//! Regular English inflections.
//!
//! CMUdict is a list of words, not a grammar. It holds `avenger` and not `avengers`,
//! `walk` and not `walked`, so a word that differs from an entry only by a regular suffix
//! fell through to the letter-to-sound rules, which read it as a name nobody has heard.
//! The suffix is applied to the entry instead, with the voicing chosen by the last phoneme
//! of the base, which is what a speaker of English does without thinking about it.
//!
//! Only regular forms are derived. An irregular one (`children`, `went`, `knives`) is in
//! the dictionary already, because CMUdict was built from a pronunciation lexicon and not
//! from a rule set. When this module is wrong, it is wrong in a word the dictionary does
//! not hold, which is the case it exists to improve.

use crate::cmudict::CmuDict;

/// A regular suffix, and the sound it adds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// `-s`, `-es`, `-ies`: plural nouns and third person singular verbs.
    Plural,
    /// `-ed`, `-ied`: past tense and past participle.
    Past,
    /// `-ing`: present participle.
    Ing,
    /// `-er`: comparative, and the agent noun that is not one.
    Comparative,
    /// `-est`: superlative.
    Superlative,
    /// `-ly`: adverb.
    Adverb,
}

/// Suffixes to try, most specific first.
///
/// The order matters: `houses` must be read as `house` and not as `hous`, and `boxes` as
/// `box` and not as `boxe`, so the longer spelling is tried before the shorter one.
const SUFFIXES: [(&str, Kind); 10] = [
    ("ies", Kind::Plural),
    ("ied", Kind::Past),
    ("ves", Kind::Plural),
    ("ing", Kind::Ing),
    ("est", Kind::Superlative),
    ("ed", Kind::Past),
    ("es", Kind::Plural),
    ("er", Kind::Comparative),
    ("ly", Kind::Adverb),
    ("s", Kind::Plural),
];

/// Most stems one suffix can be tried against.
const MAX_BASES: usize = 5;

/// The pronunciation of `word` built from a dictionary entry plus a regular suffix.
///
/// Returns `None` when the word has no suffix, when the stem is not in the dictionary, or
/// when the suffix does not apply to the stem that was found. The caller then falls back to
/// the letter-to-sound rules.
#[must_use]
pub fn derive(word: &str, dictionary: &CmuDict) -> Option<String> {
    // The candidate stems are written into this one buffer and tried as slices. They used
    // to be a `Vec<String>` of `format!` results, allocated for every suffix attempt of
    // every word the dictionary does not hold.
    let mut scratch = String::new();
    for (suffix, kind) in SUFFIXES {
        if word.len() <= suffix.len() || !word.ends_with(suffix) {
            continue;
        }
        let stem = &word[..word.len() - suffix.len()];
        let (ranges, count) = bases(stem, suffix, &mut scratch);
        for range in &ranges[..count] {
            let Some(pronunciations) = dictionary.lookup(&scratch[range.clone()]) else {
                continue;
            };
            let Some(first) = pronunciations.first() else {
                continue;
            };
            let mut out = String::with_capacity(first.len() + 8);
            // `knives` is not `knife` plus a sound: the `f` turns into a `v`, and the
            // plural is then the voiced one. English spells that with `ves`.
            if suffix == "ves" {
                push_voiced_f(first, &mut out);
            } else {
                out.push_str(first);
            }
            let added = added_sound(kind, &out);
            if added.is_empty() {
                continue;
            }
            out.push(' ');
            out.push_str(added);
            return Some(out);
        }
    }
    None
}

/// Stems to try for one suffix, as byte ranges into `scratch`, most likely first.
///
/// Returns the ranges and how many of them are used. The morphology is the same as it was
/// written one `format!` per candidate; only the storage changed.
fn bases(
    stem: &str,
    suffix: &str,
    scratch: &mut String,
) -> ([std::ops::Range<usize>; MAX_BASES], usize) {
    scratch.clear();
    let mut ranges: [std::ops::Range<usize>; MAX_BASES] = std::array::from_fn(|_| 0..0);
    let mut count = 0;
    match suffix {
        // `cities` is `city`, `tried` is `try`.
        "ies" | "ied" => push_candidate(scratch, &mut ranges, &mut count, &[stem, "y"]),
        // `knives` is `knife`, `wolves` is `wolf`.
        "ves" => {
            push_candidate(scratch, &mut ranges, &mut count, &[stem, "fe"]);
            push_candidate(scratch, &mut ranges, &mut count, &[stem, "f"]);
        }
        // `boxes` is `box`, `houses` is `house`.
        "es" => {
            push_candidate(scratch, &mut ranges, &mut count, &[stem]);
            push_candidate(scratch, &mut ranges, &mut count, &[stem, "e"]);
        }
        "s" => push_candidate(scratch, &mut ranges, &mut count, &[stem]),
        // A dropped `e` (`making` is `make`), a doubled consonant (`running` is `run`), and
        // a `y` written as `i` (`happily` is `happy`).
        _ => {
            push_candidate(scratch, &mut ranges, &mut count, &[stem, "e"]);
            push_candidate(scratch, &mut ranges, &mut count, &[stem]);
            if let Some(single) = undoubled(stem) {
                push_candidate(scratch, &mut ranges, &mut count, &[single]);
            }
        }
    }
    if stem.len() > 1 && stem.ends_with('i') {
        push_candidate(
            scratch,
            &mut ranges,
            &mut count,
            &[&stem[..stem.len() - 1], "y"],
        );
    }
    (ranges, count)
}

/// Record one candidate stem, made of `parts` joined, as a range into `scratch`.
fn push_candidate(
    scratch: &mut String,
    ranges: &mut [std::ops::Range<usize>; MAX_BASES],
    count: &mut usize,
    parts: &[&str],
) {
    let start = scratch.len();
    for part in parts {
        scratch.push_str(part);
    }
    ranges[*count] = start..scratch.len();
    *count += 1;
}

/// `stopp` is `stop`; anything else is unchanged.
fn undoubled(stem: &str) -> Option<&str> {
    let mut characters = stem.chars().rev();
    let last = characters.next()?;
    if characters.next()? != last || !last.is_ascii_alphabetic() {
        return None;
    }
    Some(&stem[..stem.len() - 1])
}

/// The phonemes the suffix adds, chosen by the last phoneme of the base.
fn added_sound(kind: Kind, base: &str) -> &'static str {
    let last = last_phoneme(base);
    match kind {
        Kind::Plural => {
            // `boxes` takes a full vowel between the two sibilants. It is written `IH0`
            // because espeak, which phonemized the audio these voices were trained on,
            // produces /ɪ/ there; CMUdict writes `AH0` in the entries it does hold, and
            // those entries keep their own spelling.
            if is_sibilant(last) {
                "IH0 Z"
            } else if is_voiceless(last) {
                "S"
            } else {
                "Z"
            }
        }
        Kind::Past => {
            if matches!(last, "T" | "D") {
                "IH0 D"
            } else if is_voiceless(last) {
                "T"
            } else {
                "D"
            }
        }
        Kind::Ing => "IH0 NG",
        Kind::Comparative => "ER0",
        Kind::Superlative => "IH0 ST",
        Kind::Adverb => "L IY0",
    }
}

/// `N AY1 F` becomes `N AY1 V`: the `f` of `knife` is voiced in `knives`.
fn push_voiced_f(pronunciation: &str, out: &mut String) {
    let (head, last) = match pronunciation.rfind(' ') {
        Some(space) => (&pronunciation[..=space], &pronunciation[space + 1..]),
        None => ("", pronunciation),
    };
    out.push_str(head);
    if last.trim_end_matches(|c: char| c.is_ascii_digit()) == "F" {
        out.push('V');
        out.push_str(&last[1..]);
    } else {
        out.push_str(last);
    }
}

/// The last phoneme of a pronunciation, without its stress digit.
fn last_phoneme(pronunciation: &str) -> &str {
    let token = pronunciation.rsplit(' ').next().unwrap_or(pronunciation);
    token.trim_end_matches(|character: char| character.is_ascii_digit())
}

/// `S`, `Z`, `SH`, `CH`, `ZH` and `JH`: a plural after these needs a vowel to be heard.
fn is_sibilant(phoneme: &str) -> bool {
    matches!(phoneme, "S" | "Z" | "SH" | "CH" | "ZH" | "JH")
}

/// Phonemes with no voice in them, so a suffix after one is also voiceless.
fn is_voiceless(phoneme: &str) -> bool {
    matches!(phoneme, "P" | "T" | "K" | "F" | "TH" | "S" | "SH" | "CH")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dictionary() -> CmuDict {
        CmuDict::parse(
            "avenger AH0 V EH1 N JH ER0\n\
             city S IH1 T IY0\n\
             box B AA1 K S\n\
             house HH AW1 S\n\
             watch W AA1 CH\n\
             walk W AO1 K\n\
             run R AH1 N\n\
             make M EY1 K\n\
             stop S T AA1 P\n\
             big B IH1 G\n\
             happy HH AE1 P IY0\n\
             quick K W IH1 K\n\
             cry K R AY1\n\
             knife N AY1 F\n\
             pass P AE1 S\n\
             need N IY1 D\n\
             love L AH1 V\n\
             hope HH OW1 P\n",
        )
    }

    #[test]
    fn a_plural_adds_z_after_a_voiced_sound() {
        assert_eq!(
            derive("avengers", &dictionary()).as_deref(),
            Some("AH0 V EH1 N JH ER0 Z")
        );
        assert_eq!(
            derive("cities", &dictionary()).as_deref(),
            Some("S IH1 T IY0 Z")
        );
    }

    #[test]
    fn a_plural_after_a_sibilant_takes_a_vowel() {
        assert_eq!(
            derive("boxes", &dictionary()).as_deref(),
            Some("B AA1 K S IH0 Z")
        );
        assert_eq!(
            derive("watches", &dictionary()).as_deref(),
            Some("W AA1 CH IH0 Z")
        );
        assert_eq!(
            derive("houses", &dictionary()).as_deref(),
            Some("HH AW1 S IH0 Z")
        );
    }

    #[test]
    fn a_plural_after_a_voiceless_sound_is_s() {
        assert_eq!(derive("walks", &dictionary()).as_deref(), Some("W AO1 K S"));
    }

    #[test]
    fn a_past_tense_follows_the_same_voicing_rule() {
        assert_eq!(
            derive("walked", &dictionary()).as_deref(),
            Some("W AO1 K T")
        );
        assert_eq!(
            derive("passed", &dictionary()).as_deref(),
            Some("P AE1 S T")
        );
        assert_eq!(
            derive("needed", &dictionary()).as_deref(),
            Some("N IY1 D IH0 D")
        );
        assert_eq!(derive("loved", &dictionary()).as_deref(), Some("L AH1 V D"));
    }

    #[test]
    fn a_dropped_e_comes_back_before_a_suffix() {
        // `hoped` is `hope` plus a `d`, not `hop` plus `ed`.
        assert_eq!(
            derive("hoped", &dictionary()).as_deref(),
            Some("HH OW1 P T")
        );
        assert_eq!(
            derive("making", &dictionary()).as_deref(),
            Some("M EY1 K IH0 NG")
        );
    }

    #[test]
    fn a_doubled_consonant_is_undoubled() {
        assert_eq!(
            derive("running", &dictionary()).as_deref(),
            Some("R AH1 N IH0 NG")
        );
        assert_eq!(
            derive("stopped", &dictionary()).as_deref(),
            Some("S T AA1 P T")
        );
        assert_eq!(
            derive("bigger", &dictionary()).as_deref(),
            Some("B IH1 G ER0")
        );
    }

    #[test]
    fn an_i_stands_for_a_y() {
        assert_eq!(
            derive("happily", &dictionary()).as_deref(),
            Some("HH AE1 P IY0 L IY0")
        );
        assert_eq!(
            derive("happiest", &dictionary()).as_deref(),
            Some("HH AE1 P IY0 IH0 ST")
        );
        assert_eq!(derive("cried", &dictionary()).as_deref(), Some("K R AY1 D"));
    }

    #[test]
    fn an_irregular_plural_is_left_to_the_dictionary() {
        // `knives` is derived from `knife`, but `children` is not derivable and must not be
        // guessed from `child` by adding a sound.
        assert_eq!(
            derive("knives", &dictionary()).as_deref(),
            Some("N AY1 V Z")
        );
        assert!(derive("children", &dictionary()).is_none());
    }

    #[test]
    fn a_word_with_no_entry_at_all_is_not_invented() {
        assert!(derive("zorbansky", &dictionary()).is_none());
        assert!(derive("the", &dictionary()).is_none());
    }

    #[test]
    fn a_suffix_is_not_applied_twice() {
        // `quickly` is in the dictionary in the real thing; here it is derived once from
        // `quick`, and the result is not fed back through the rules.
        assert_eq!(
            derive("quickly", &dictionary()).as_deref(),
            Some("K W IH1 K L IY0")
        );
    }
}
