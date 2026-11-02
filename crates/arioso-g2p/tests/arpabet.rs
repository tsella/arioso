//! ARPAbet to the IPA symbols the voice expects.

use arioso_g2p::arpabet::{is_phoneme, phoneme_to_ipa, to_ipa};

/// Every symbol `en_US-lessac-medium` maps, taken from its phoneme_id_map.
/// Single characters only: diphthongs and affricates are sequences.
const VOICE_INVENTORY: &str = "_^$ !'(),-.:;?abcdefhijklmnopqrstuvwxyzæçðøħŋœǀǁǂǃɐɑɒɓɔɕɖɗɘəɚɛɜɞɟɠɡɢɣɤɥɦɧɨɪɫɬɭɮɯɰɱɲɳɴɵɶɸɹɺɻɽɾʀʁʂʃʄʈʉʊʋʌʍʎʏʐʑʒʔʕʘʙʛʜʝʟʡʢʲˈˌːˑ˞βθχᵻⱱ0123456789̧̪̯̩̃ʰˤε↓#\"↑̺̻";

const ALL_ARPABET: [&str; 39] = [
    "AA", "AE", "AH", "AO", "AW", "AY", "B", "CH", "D", "DH", "EH", "ER", "EY", "F", "G", "HH",
    "IH", "IY", "JH", "K", "L", "M", "N", "NG", "OW", "OY", "P", "R", "S", "SH", "T", "TH", "UH",
    "UW", "V", "W", "Y", "Z", "ZH",
];

#[test]
fn hello_maps_to_its_symbols() {
    assert_eq!(to_ipa("HH AH0 L OW1"), vec!["h", "ə", "l", "ˈ", "o", "ʊ"]);
}

#[test]
fn secondary_stress_uses_the_weaker_mark() {
    let ipa = to_ipa("AE2 B AH0 L OW1 N IY0");
    assert_eq!(ipa[..2], ["ˌ", "æ"][..]);
    assert_eq!(ipa[ipa.len() - 1], "i");
}

/// Reference output from espeak-ng through Piper's own bridge, for en-us.
/// Only words where CMUdict and espeak agree on the phonemes themselves.
const REFERENCE: &[(&str, &[&str])] = &[
    ("HH AH0 L OW1", &["h", "ə", "l", "ˈ", "o", "ʊ"]),
    ("W ER1 L D", &["w", "ˈ", "ɜ", "ː", "l", "d"]),
    ("S IY1", &["s", "ˈ", "i", "ː"]),
    ("T UW1", &["t", "ˈ", "u", "ː"]),
    ("L AO1", &["l", "ˈ", "ɔ", "ː"]),
    ("K AA1 R", &["k", "ˈ", "ɑ", "ː", "ɹ"]),
    ("D EY1", &["d", "ˈ", "e", "ɪ"]),
    ("K AH1 P", &["k", "ˈ", "ʌ", "p"]),
    ("B UH1 K", &["b", "ˈ", "ʊ", "k"]),
    ("F AA1 DH ER0", &["f", "ˈ", "ɑ", "ː", "ð", "ɚ"]),
    ("N EY1 SH AH0 N", &["n", "ˈ", "e", "ɪ", "ʃ", "ə", "n"]),
    ("B ER1 D", &["b", "ˈ", "ɜ", "ː", "d"]),
];

#[test]
fn matches_the_espeak_reference() {
    for (arpabet, expected) in REFERENCE {
        assert_eq!(to_ipa(arpabet), expected.to_vec(), "{arpabet}");
    }
}

#[test]
fn unknown_phonemes_are_dropped() {
    assert_eq!(to_ipa("HH XX L"), vec!["h", "l"]);
}

#[test]
fn every_arpabet_phoneme_maps_to_something() {
    for phoneme in ALL_ARPABET {
        assert!(
            !phoneme_to_ipa(phoneme).is_empty(),
            "{phoneme} produced nothing"
        );
    }
}

#[test]
fn every_emitted_symbol_exists_in_the_voice() {
    for phoneme in ALL_ARPABET {
        for stress in ["", "0", "1", "2"] {
            let token = format!("{phoneme}{stress}");
            for symbol in phoneme_to_ipa(&token) {
                assert_eq!(symbol.chars().count(), 1, "{token} emitted {symbol:?}");
                assert!(
                    VOICE_INVENTORY.contains(symbol),
                    "{token} emitted {symbol:?}, absent from the voice"
                );
            }
        }
    }
}

#[test]
fn a_phoneme_is_recognised_in_any_case() {
    assert!(is_phoneme("AH0"));
    assert!(is_phoneme("ah0"));
    assert!(is_phoneme("AA"));
    assert!(is_phoneme("ch"));
    assert!(!is_phoneme("TOM"));
    assert!(!is_phoneme("hello"));
    assert!(!is_phoneme(""));
}

#[test]
fn a_lowercase_pronunciation_gives_the_same_symbols() {
    assert_eq!(to_ipa("ah0 l ow1"), to_ipa("AH0 L OW1"));
    assert!(to_ipa("ah0 l ow1").contains(&"ə"));
}
