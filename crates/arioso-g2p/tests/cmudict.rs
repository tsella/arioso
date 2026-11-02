//! The pronunciation dictionary that gives words their ARPAbet phonemes.

use arioso_g2p::CmuDict;

const SAMPLE: &str = r#";;; # CMUdict -- Major Version: 0.07
;;; A comment line
AARON  EH1 R AH0 N
AARON(2)  AA1 R AH0 N
ABALONE  AE2 B AH0 L OW1 N IY0
ABBREVIATE  AH0 B R IY1 V IY0 EY2 T
BROKEN
ZZTOP  Z IY1 Z IY1 T AA1 P
"#;

#[test]
fn looks_up_a_pronunciation() {
    let dict = CmuDict::parse(SAMPLE);
    assert_eq!(
        dict.lookup("abalone").expect("known word")[0].as_ref(),
        "AE2 B AH0 L OW1 N IY0"
    );
}

#[test]
fn keeps_alternate_pronunciations_in_file_order() {
    let dict = CmuDict::parse(SAMPLE);
    let all = dict.lookup("aaron").expect("known word");
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].as_ref(), "EH1 R AH0 N");
    assert_eq!(all[1].as_ref(), "AA1 R AH0 N");
}

#[test]
fn lookup_ignores_case() {
    let dict = CmuDict::parse(SAMPLE);
    assert!(dict.lookup("ABALONE").is_some());
    assert!(dict.lookup("Abalone").is_some());
}

#[test]
fn comments_and_blank_lines_are_not_words() {
    let dict = CmuDict::parse(SAMPLE);
    assert_eq!(dict.len(), 4);
    assert_eq!(dict.skipped_lines(), 1, "only BROKEN is malformed");
}

#[test]
fn unknown_word_is_none() {
    let dict = CmuDict::parse(SAMPLE);
    assert!(dict.lookup("xyzzy").is_none());
}

#[test]
fn empty_input_is_an_empty_dict() {
    let dict = CmuDict::parse("");
    assert!(dict.is_empty());
    assert_eq!(dict.len(), 0);
}
