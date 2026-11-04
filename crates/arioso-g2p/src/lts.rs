//! Letter to sound, for words the dictionary does not hold.
//!
//! These are spelling rules, not a phonemizer. English spelling is irregular enough
//! that a rule set gets a name approximately right and sometimes wrong. That is the
//! trade: a guessed word is heard, and one dictionary entry corrects it.
//!
//! Every rule produces ARPAbet, which then goes through the same table a CMUdict
//! entry uses. One mapping path means the voice inventory check cannot differ
//! between a known word and a guessed one.

use crate::arpabet;

/// Vowels, for counting nuclei and placing stress.
const VOWELS: [&str; 15] = [
    "AA", "AE", "AH", "AO", "AW", "AY", "EH", "ER", "EY", "IH", "IY", "OW", "OY", "UH", "UW",
];

/// Two-letter patterns, longest-first order does not matter: the table is checked
/// before single letters and after three- and four-letter patterns.
const PAIRS: [(&str, &[&str]); 35] = [
    ("ch", &["CH"]),
    ("sh", &["SH"]),
    ("th", &["TH"]),
    ("ph", &["F"]),
    ("wh", &["W"]),
    ("ck", &["K"]),
    ("ng", &["NG"]),
    ("qu", &["K", "W"]),
    ("kn", &["N"]),
    ("wr", &["R"]),
    ("gn", &["N"]),
    ("ps", &["S"]),
    ("mb", &["M"]),
    ("ee", &["IY"]),
    ("ea", &["IY"]),
    ("oo", &["UW"]),
    ("ou", &["AW"]),
    ("ow", &["OW"]),
    ("ai", &["EY"]),
    ("ay", &["EY"]),
    ("au", &["AO"]),
    ("aw", &["AO"]),
    ("oi", &["OY"]),
    ("oy", &["OY"]),
    ("oa", &["OW"]),
    ("ie", &["IY"]),
    ("ei", &["EY"]),
    ("eu", &["UW"]),
    ("ue", &["UW"]),
    ("ui", &["UW"]),
    ("er", &["ER"]),
    ("ir", &["ER"]),
    ("ur", &["ER"]),
    ("ar", &["AA", "R"]),
    ("or", &["AO", "R"]),
];

/// Three-letter patterns.
const TRIPLES: [(&str, &[&str]); 8] = [
    ("tch", &["CH"]),
    ("dge", &["JH"]),
    ("igh", &["AY"]),
    ("ear", &["IH", "R"]),
    ("eer", &["IH", "R"]),
    ("air", &["EH", "R"]),
    ("oor", &["AO", "R"]),
    ("our", &["AW", "ER"]),
];

/// Four-letter patterns.
const QUADS: [(&str, &[&str]); 5] = [
    ("tion", &["SH", "AH", "N"]),
    ("sion", &["ZH", "AH", "N"]),
    ("ture", &["CH", "ER"]),
    ("ough", &["AO"]),
    ("eigh", &["EY"]),
];

/// Pronounce a word by spelling rule, as the voice's IPA symbols.
///
/// Returns an empty vector when the word holds no letters to pronounce.
#[must_use]
pub fn pronounce(word: &str) -> Vec<&'static str> {
    let tokens = to_arpabet(word);
    if tokens.is_empty() {
        return Vec::new();
    }
    arpabet::to_ipa(&tokens.join(" "))
}

/// Fold a Latin letter carrying a diacritic to the letter these rules can read.
///
/// The rules are English, so `é` has no rule of its own. Dropping the letter left a hole in
/// the sentence, which is worse than saying `e`: a wrong vowel is heard and can be corrected
/// with a dictionary entry, a missing word cannot.
fn fold_latin(ch: char) -> Option<&'static str> {
    Some(match ch {
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        'þ' => "th",
        'ð' | 'đ' => "d",
        'ø' => "o",
        'ł' => "l",
        'ı' => "i",
        // Latin-1 Supplement and Latin Extended-A, which is the range the voices in the
        // catalog write in.
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'ç' | 'ć' | 'č' | 'ĉ' | 'ċ' => "c",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
        'ğ' | 'ģ' => "g",
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ĭ' | 'į' => "i",
        'ñ' | 'ń' | 'ņ' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ō' | 'ŏ' | 'ő' => "o",
        'ŕ' | 'ř' => "r",
        'ś' | 'š' | 'ş' | 'ș' => "s",
        'ť' | 'ţ' | 'ț' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
        'ý' | 'ÿ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        _ => return None,
    })
}

/// Pronounce a word by spelling rule, as ARPAbet tokens with one primary stress.
#[must_use]
pub fn to_arpabet(word: &str) -> Vec<String> {
    let text: String = word
        .chars()
        .flat_map(|ch| {
            let lower = ch.to_lowercase().next().unwrap_or(ch);
            match fold_latin(lower) {
                Some(folded) => folded.chars().collect::<Vec<char>>(),
                None => vec![lower],
            }
        })
        .filter(char::is_ascii_alphabetic)
        .collect();
    if text.is_empty() {
        return Vec::new();
    }

    let mut tokens: Vec<&'static str> = Vec::new();
    let mut index = 0;
    while index < text.len() {
        index += segment(&text, index, &mut tokens);
    }

    let mut tokens: Vec<String> = tokens.into_iter().map(str::to_owned).collect();
    stress(&mut tokens);
    tokens
}

/// Mark the first vowel nucleus primary and the rest secondary.
///
/// Crude on purpose: English stress depends on a word's history, which the spelling
/// does not carry. A guessed stress still reads better than a skipped word.
fn stress(tokens: &mut [String]) {
    let mut first = true;
    for token in tokens.iter_mut() {
        if VOWELS.contains(&token.as_str()) {
            token.push(if first { '1' } else { '0' });
            first = false;
        }
    }
}

/// Apply the longest rule that fits at `index`, appending its phonemes.
///
/// Returns how many letters the rule consumed. Always at least one.
fn segment(text: &str, index: usize, out: &mut Vec<&'static str>) -> usize {
    let rest = &text[index..];
    let at_start = index == 0;
    let ends_at = |len: usize| index + len == text.len();
    let before = text[..index].chars().next_back();

    // A final silent e lengthens the vowel before it across one consonant:
    // name, time, hope, cube.
    if rest.len() == 3 && rest.as_bytes()[2] == b'e' {
        let vowel = rest.as_bytes()[0];
        let consonant = rest.as_bytes()[1];
        let long = match vowel {
            b'a' => Some("EY"),
            b'e' => Some("IY"),
            b'i' => Some("AY"),
            b'o' => Some("OW"),
            b'u' => Some("UW"),
            _ => None,
        };
        if let Some(long) = long {
            if !is_vowel(char::from(consonant)) {
                out.push(long);
                // The consonant is followed by that final `e`, which is a front vowel, so a
                // `c` or `g` here is soft: face, ice, page, huge. `single` has no context
                // and would say the hard form.
                out.extend_from_slice(match consonant {
                    b'c' => &["S"],
                    b'g' => &["JH"],
                    other => single(other),
                });
                return 3;
            }
        }
    }

    if let Some((pattern, phonemes)) = QUADS.iter().find(|(pattern, _)| rest.starts_with(pattern)) {
        out.extend_from_slice(phonemes);
        return pattern.len();
    }

    if let Some((pattern, phonemes)) = TRIPLES
        .iter()
        .find(|(pattern, _)| rest.starts_with(pattern))
    {
        out.extend_from_slice(phonemes);
        return pattern.len();
    }

    if let Some((pattern, phonemes)) = PAIRS.iter().find(|(pattern, _)| rest.starts_with(pattern)) {
        let needs_start = matches!(*pattern, "kn" | "wr" | "gn" | "ps");
        let needs_end = *pattern == "mb";
        // "or" before another r is two sounds, not one: sorry, carry, hurry.
        let before_r = pattern.ends_with('r') && rest[2..].starts_with('r');
        if (!needs_start || at_start) && (!needs_end || ends_at(2)) && !before_r {
            out.extend_from_slice(phonemes);
            return 2;
        }
    }

    // A doubled consonant is one sound: happy, ladder, summer.
    if rest.len() >= 2 && rest.as_bytes()[0] == rest.as_bytes()[1] {
        let doubled = rest.as_bytes()[0];
        if !is_vowel(char::from(doubled)) && doubled.is_ascii_alphabetic() {
            out.extend_from_slice(single(doubled));
            return 2;
        }
    }

    // "gh" is silent after a vowel and a hard g at the start of a word.
    if rest.starts_with("gh") {
        if before.is_some_and(is_vowel) {
            return 2;
        }
        out.push("G");
        return 2;
    }

    let letter = rest.as_bytes()[0];
    let following = rest[1..].chars().next();
    let next_is_front = following.is_some_and(|ch| matches!(ch, 'e' | 'i' | 'y'));

    let phonemes: &'static [&'static str] = match letter {
        b'c' if next_is_front => &["S"],
        b'g' if next_is_front => &["JH"],
        b's' if before.is_some_and(is_vowel) && following.is_some_and(is_vowel) => &["Z"],
        b's' if ends_at(1) && before.is_some_and(is_vowel) => &["Z"],
        b'x' if at_start => &["Z"],
        b'x' => &["K", "S"],
        b'y' if at_start => &["Y"],
        // A final y after a consonant and no other vowel is a long i: sky, fly, try.
        b'y' if ends_at(1) && index > 0 && !text[..index].chars().any(is_vowel) => &["AY"],
        b'y' if ends_at(1) => &["IY"],
        b'y' => &["IH"],
        // A final e after at least two letters is silent: name, time, hope.
        b'e' if ends_at(1) && index > 1 => &[],
        other => single(other),
    };
    out.extend_from_slice(phonemes);
    1
}

/// The phonemes for one letter, with no context.
fn single(letter: u8) -> &'static [&'static str] {
    match letter {
        b'a' => &["AE"],
        b'b' => &["B"],
        b'c' => &["K"],
        b'd' => &["D"],
        b'e' => &["EH"],
        b'f' => &["F"],
        b'g' => &["G"],
        b'h' => &["HH"],
        b'i' => &["IH"],
        b'j' => &["JH"],
        b'k' => &["K"],
        b'l' => &["L"],
        b'm' => &["M"],
        b'n' => &["N"],
        b'o' => &["AA"],
        b'p' => &["P"],
        b'r' => &["R"],
        b's' => &["S"],
        b't' => &["T"],
        b'u' => &["AH"],
        b'v' => &["V"],
        b'w' => &["W"],
        b'x' => &["K", "S"],
        b'y' => &["IH"],
        b'z' => &["Z"],
        _ => &[],
    }
}

fn is_vowel(letter: char) -> bool {
    matches!(letter, 'a' | 'e' | 'i' | 'o' | 'u')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn say(word: &str) -> String {
        to_arpabet(word).join(" ")
    }

    #[test]
    fn an_accented_letter_is_spoken_as_its_base_letter() {
        // A Portuguese or French word is not in an English dictionary, and the rules have no
        // rule for `é`. Folding it says the word with one wrong vowel; dropping it says
        // nothing at all, which is the worse failure.
        assert_eq!(say("café"), say("cafe"));
        assert_eq!(say("São"), say("sao"));
        assert_eq!(say("Straße"), say("strasse"));
        assert_eq!(say("niño"), say("nino"));
        assert!(!pronounce("é").is_empty());
        // A letter with no base form is still not a word.
        assert!(pronounce("漢").is_empty());
    }

    #[test]
    fn a_word_with_no_letters_is_silent() {
        assert!(pronounce("").is_empty());
        assert!(pronounce("123").is_empty());
        assert!(pronounce("   ").is_empty());
    }

    #[test]
    fn simple_words_get_a_plausible_reading() {
        assert_eq!(say("cat"), "K AE1 T");
        assert_eq!(say("dog"), "D AA1 G");
        assert_eq!(say("sun"), "S AH1 N");
    }

    #[test]
    fn a_final_silent_e_lengthens_the_vowel() {
        assert_eq!(say("name"), "N EY1 M");
        assert_eq!(say("time"), "T AY1 M");
        assert_eq!(say("hope"), "HH OW1 P");
        assert_eq!(say("cube"), "K UW1 B");
    }

    #[test]
    fn a_final_silent_e_keeps_a_soft_c_and_g() {
        // These words are not in a dictionary, so they are spoken by the rules. The silent
        // e lengthened the vowel and then the consonant was read with no context, which
        // said a hard `K` or `G`: `face` came out as `F EY1 K`.
        assert_eq!(say("face"), "F EY1 S");
        assert_eq!(say("ice"), "AY1 S");
        assert_eq!(say("age"), "EY1 JH");
        assert_eq!(say("huge"), "HH UW1 JH");

        // The hard form is gone, not merely present alongside a soft one.
        for word in ["face", "ice", "age", "huge"] {
            let spoken = to_arpabet(word);
            assert!(
                !spoken.iter().any(|token| token == "K" || token == "G"),
                "{word} was spoken as {}",
                spoken.join(" ")
            );
        }
    }

    #[test]
    fn digraphs_are_one_sound() {
        assert_eq!(say("shop"), "SH AA1 P");
        assert_eq!(say("chin"), "CH IH1 N");
        assert_eq!(say("phone"), "F OW1 N");
        assert_eq!(say("queen"), "K W IY1 N");
        assert_eq!(say("sing"), "S IH1 NG");
        assert_eq!(say("back"), "B AE1 K");
    }

    #[test]
    fn a_vowel_pair_is_one_nucleus() {
        assert_eq!(say("see"), "S IY1");
        assert_eq!(say("moon"), "M UW1 N");
        assert_eq!(say("rain"), "R EY1 N");
        assert_eq!(say("play"), "P L EY1");
        assert_eq!(say("coin"), "K OY1 N");
        assert_eq!(say("boat"), "B OW1 T");
    }

    #[test]
    fn a_doubled_r_does_not_get_a_r_coloured_vowel() {
        assert_eq!(say("sorry"), "S AA1 R IY0");
        assert_eq!(say("carry"), "K AE1 R IY0");
        assert_eq!(say("hurry"), "HH AH1 R IY0");
    }

    #[test]
    fn r_coloured_vowels() {
        assert_eq!(say("her"), "HH ER1");
        assert_eq!(say("bird"), "B ER1 D");
        assert_eq!(say("car"), "K AA1 R");
        assert_eq!(say("for"), "F AO1 R");
    }

    #[test]
    fn c_and_g_soften_before_a_front_vowel() {
        assert_eq!(say("city"), "S IH1 T IY0");
        assert_eq!(say("gem"), "JH EH1 M");
        assert_eq!(say("cat"), "K AE1 T");
        assert_eq!(say("gum"), "G AH1 M");
    }

    #[test]
    fn an_initial_silent_letter_is_dropped() {
        assert_eq!(say("knight"), "N AY1 T");
        assert_eq!(say("write"), "R AY1 T");
        assert_eq!(say("lamb"), "L AE1 M");
    }

    #[test]
    fn a_final_y_is_a_long_e() {
        assert_eq!(say("happy"), "HH AE1 P IY0");
        assert_eq!(say("sky"), "S K AY1");
    }

    #[test]
    fn stress_lands_on_the_first_nucleus() {
        assert!(say("zorbansky").contains("AO1"));
        let tokens = to_arpabet("zorbansky");
        assert_eq!(
            tokens.iter().filter(|token| token.ends_with('1')).count(),
            1,
            "one primary stress: {tokens:?}"
        );
    }

    #[test]
    fn a_made_up_name_produces_symbols() {
        assert!(!pronounce("zorbansky").is_empty());
        assert!(!pronounce("Kowalczyk").is_empty());
        assert!(!pronounce("Xylophone").is_empty());
    }
}
