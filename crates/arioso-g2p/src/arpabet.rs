//! ARPAbet (CMUdict) to the IPA symbols a Piper voice expects.
//!
//! The voice maps single characters only, so diphthongs and affricates expand:
//! `EY` is `e` then `ɪ`, `CH` is `t` then `ʃ`. Stress is its own symbol, placed
//! before the vowel it marks.
//!
//! Two choices are judgement calls, kept here so golden audio can settle them:
//! `ER` is `ɚ` when unstressed and `ɜ` plus `ɹ` when stressed, and vowel length
//! is not written, because CMUdict carries stress rather than duration.

/// Symbols for one ARPAbet token such as `AH0`.
#[must_use]
pub fn phoneme_to_ipa(token: &str) -> Vec<&'static str> {
    let (base, stress) = split_stress(token);
    let mut symbols_out = Vec::with_capacity(3);
    if let Some(digit) = stress {
        if let Some(mark) = stress_mark(digit) {
            symbols_out.push(mark);
        }
    }
    symbols_out.extend_from_slice(symbols(base, stress));
    symbols_out
}

/// Symbols for a whole pronunciation such as `HH AH0 L OW1`.
#[must_use]
pub fn to_ipa(pronunciation: &str) -> Vec<&'static str> {
    pronunciation
        .split_whitespace()
        .flat_map(phoneme_to_ipa)
        .collect()
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
        "ER" => &["ɜ", "ɹ"],
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
