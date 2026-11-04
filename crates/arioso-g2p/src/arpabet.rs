//! ARPAbet (CMUdict) to the IPA symbols a Piper voice expects.
//!
//! The voice maps single characters only, so diphthongs and affricates expand:
//! `EY` is `e` then `ɪ`, `CH` is `t` then `ʃ`. Stress is its own symbol, placed
//! before the vowel it marks.
//!
//! The table was checked against espeak-ng through Piper's own bridge, which is
//! the phonemizer these voices were trained with. Its habits are copied where
//! they were observed: stressed `ER` is `ɜ` with no following `ɹ`, unstressed
//! `ER` is `ɚ`, and stressed tense vowels take a length mark.
//!
//! One difference is known and accepted: espeak sometimes gives unstressed `AH`
//! as `ɐ` rather than `ə`, depending on where the word sits. CMUdict does not
//! carry that information, so this table always writes `ə`.

/// Longest token this table knows: two letters and a stress digit, as in `ZH0`.
///
/// A token longer than this is not a phoneme, so it is refused without building a
/// `String` to look it up. The hot path allocates nothing per phoneme.
const MAX_TOKEN: usize = 4;

/// Whether a token is a phoneme this table knows, with or without a stress
/// digit. Case does not matter: `ah0` and `AH0` are the same token.
#[must_use]
pub fn is_phoneme(token: &str) -> bool {
    let mut buffer = [0u8; MAX_TOKEN];
    let Some(upper) = upper_into(token, &mut buffer) else {
        return false;
    };
    let (base, stress) = split_stress(upper);
    !symbols(base, stress).is_empty()
}

/// Symbols for one ARPAbet token such as `AH0`.
#[must_use]
pub fn phoneme_to_ipa(token: &str) -> Vec<&'static str> {
    let mut out = Vec::with_capacity(3);
    extend_phoneme(token, true, &mut out);
    out
}

/// Symbols for a whole pronunciation such as `HH AH0 L OW1`.
#[must_use]
pub fn to_ipa(pronunciation: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    extend_ipa(pronunciation, &mut out);
    out
}

/// Append the symbols for one ARPAbet token to `out`.
pub fn extend_phoneme(token: &str, with_mark: bool, out: &mut Vec<&'static str>) {
    let mut buffer = [0u8; MAX_TOKEN];
    let Some(upper) = upper_into(token, &mut buffer) else {
        return;
    };
    let (base, stress) = split_stress(upper);
    extend_symbols(base, stress, with_mark, out);
}

/// Append the symbols for a whole pronunciation such as `HH AH0 L OW1` to `out`.
///
/// The caller keeps the buffer, so a sentence is phonemized into one vector rather than
/// a fresh one per phoneme, per word and per utterance.
pub fn extend_ipa(pronunciation: &str, out: &mut Vec<&'static str>) {
    for token in pronunciation.split_whitespace() {
        extend_phoneme(token, true, out);
    }
}

/// Append the symbols for a pronunciation, leaving the stress marks out.
///
/// The reverse comparison in [`crate::reverse`] is about sounds, not emphasis, and it used
/// to build every symbol and then filter the marks back out.
pub fn extend_ipa_unstressed(pronunciation: &str, out: &mut Vec<&'static str>) {
    for token in pronunciation.split_whitespace() {
        extend_phoneme(token, false, out);
    }
}

/// Append the symbols for one base and its stress digit.
///
/// The letter-to-sound rules hold a base and a digit rather than a written token, so they
/// hand the two pieces here instead of formatting a token and parsing it back.
pub fn extend_symbols(
    base: &str,
    stress: Option<u8>,
    with_mark: bool,
    out: &mut Vec<&'static str>,
) {
    if with_mark {
        if let Some(mark) = stress.and_then(stress_mark) {
            out.push(mark);
        }
    }
    out.extend_from_slice(symbols(base, stress));
    if is_long(base, stress) {
        out.push("ː");
    }
}

/// Uppercase `token` into `buffer` and return it as a string slice.
///
/// `None` for a token the table cannot hold: longer than [`MAX_TOKEN`], or not ASCII, in
/// which case no phoneme matches it anyway.
fn upper_into<'a>(token: &str, buffer: &'a mut [u8; MAX_TOKEN]) -> Option<&'a str> {
    let bytes = token.as_bytes();
    if bytes.len() > MAX_TOKEN {
        return None;
    }
    for (slot, byte) in buffer.iter_mut().zip(bytes) {
        if !byte.is_ascii() {
            return None;
        }
        *slot = byte.to_ascii_uppercase();
    }
    std::str::from_utf8(&buffer[..bytes.len()]).ok()
}

/// espeak lengthens the tense vowels when they carry stress, and leaves the lax
/// ones alone: `see` is `s ˈ i ː`, `happy` is `h ˈ æ p i`.
fn is_long(base: &str, stress: Option<u8>) -> bool {
    matches!(stress, Some(1) | Some(2)) && matches!(base, "AA" | "AO" | "IY" | "UW" | "ER")
}

fn split_stress(token: &str) -> (&str, Option<u8>) {
    match token.as_bytes().last() {
        Some(digit @ b'0'..=b'2') => (&token[..token.len() - 1], Some(digit - b'0')),
        _ => (token, None),
    }
}

fn stress_mark(digit: u8) -> Option<&'static str> {
    match digit {
        1 => Some("ˈ"),
        2 => Some("ˌ"),
        _ => None,
    }
}

fn symbols(base: &str, stress: Option<u8>) -> &'static [&'static str] {
    match base {
        "AA" => &["ɑ"],
        "AE" => &["æ"],
        "AH" if stress == Some(0) => &["ə"],
        "AH" => &["ʌ"],
        "AO" => &["ɔ"],
        "AW" => &["a", "ʊ"],
        "AY" => &["a", "ɪ"],
        "B" => &["b"],
        "CH" => &["t", "ʃ"],
        "D" => &["d"],
        "DH" => &["ð"],
        "EH" => &["ɛ"],
        "ER" if stress == Some(0) => &["ɚ"],
        "ER" => &["ɜ"],
        "EY" => &["e", "ɪ"],
        "F" => &["f"],
        "G" => &["ɡ"],
        "HH" => &["h"],
        "IH" => &["ɪ"],
        "IY" => &["i"],
        "JH" => &["d", "ʒ"],
        "K" => &["k"],
        "L" => &["l"],
        "M" => &["m"],
        "N" => &["n"],
        "NG" => &["ŋ"],
        "OW" => &["o", "ʊ"],
        "OY" => &["ɔ", "ɪ"],
        "P" => &["p"],
        "R" => &["ɹ"],
        "S" => &["s"],
        "SH" => &["ʃ"],
        "T" => &["t"],
        "TH" => &["θ"],
        "UH" => &["ʊ"],
        "UW" => &["u"],
        "V" => &["v"],
        "W" => &["w"],
        "Y" => &["j"],
        "Z" => &["z"],
        "ZH" => &["ʒ"],
        _ => &[],
    }
}
