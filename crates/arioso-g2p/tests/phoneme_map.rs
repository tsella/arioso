//! The phoneme table that turns IPA characters into model input ids.

use arioso_g2p::PhonemeMap;

const MAP: &str = r#"{
  "audio": {"sample_rate": 22050},
  "phoneme_id_map": {
    "_": [0], "^": [1], "$": [2], " ": [3],
    "h": [20], "ə": [59], "l": [55], "ˈ": [120], "oʊ": [71],
    "t": [84], "ʃ": [86]
  }
}"#;

#[test]
fn reads_ids_from_a_voice_config() {
    let map = PhonemeMap::from_voice_config(MAP).expect("valid config");
    assert_eq!(map.ids_of("h"), Some(&[20u32][..]));
    assert_eq!(map.ids_of("ə"), Some(&[59u32][..]));
    assert_eq!(map.ids_of("x"), None);
}

#[test]
fn control_symbols_are_exposed() {
    let map = PhonemeMap::from_voice_config(MAP).expect("valid config");
    assert_eq!(map.pad(), 0);
    assert_eq!(map.bos(), 1);
    assert_eq!(map.eos(), 2);
    assert_eq!(map.len(), 11);
}

#[test]
fn a_missing_control_symbol_is_an_error() {
    let err = PhonemeMap::from_voice_config(r#"{"phoneme_id_map":{"h":[20]}}"#).unwrap_err();
    assert!(err.to_string().contains('_'), "{err}");
}

#[test]
fn malformed_json_is_an_error() {
    assert!(PhonemeMap::from_voice_config("{").is_err());
}

#[test]
fn ids_interleave_the_pad_and_are_bracketed() {
    let map = PhonemeMap::from_voice_config(MAP).expect("valid config");
    let ids = map.to_ids(&["h", "ə", "l", "oʊ"]);
    // bos, pad, then each phoneme followed by pad, then eos
    assert_eq!(ids, vec![1, 0, 20, 0, 59, 0, 55, 0, 71, 0, 2]);
}

#[test]
fn unknown_phonemes_are_skipped_not_padded() {
    let map = PhonemeMap::from_voice_config(MAP).expect("valid config");
    let ids = map.to_ids(&["h", "x", "l"]);
    assert_eq!(ids, vec![1, 0, 20, 0, 55, 0, 2]);
}

#[test]
fn a_phoneme_with_several_ids_keeps_them_in_order() {
    let map =
        PhonemeMap::from_voice_config(r#"{"phoneme_id_map":{"_":[0],"^":[1],"$":[2],"aɪ":[7,8]}}"#)
            .expect("valid config");
    assert_eq!(map.to_ids(&["aɪ"]), vec![1, 0, 7, 8, 0, 2]);
}
