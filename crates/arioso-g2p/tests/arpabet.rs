//! ARPAbet to the IPA symbols the voice expects.

use arioso_g2p::arpabet::{phoneme_to_ipa, to_ipa};

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

#[test]
fn unknown_phonemes_are_dropped() {
    assert_eq!(to_ipa("HH XX L"), vec!["h", "l"]);
}

#[test]
fn every_arpabet_phoneme_maps_to_something() {
    for phoneme in ALL_ARPABET {
        assert!(!phoneme_to_ipa(phoneme).is_empty(), "{phoneme} produced nothing");
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
