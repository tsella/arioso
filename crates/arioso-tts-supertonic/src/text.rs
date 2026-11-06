//! Text preparation for Supertonic.
//!
//! The model reads characters, not phonemes. There is no dictionary and no letter-to-sound
//! step: text is decomposed, cleaned, tagged with its language, and looked up in a table of
//! 65,536 code points. That is the whole front end, and it is why this engine ignores the
//! pronunciation dictionary the other two use — a spoken form written as phonemes means
//! nothing to a model that was trained on letters.

use unicode_normalization::UnicodeNormalization;

/// Languages the model was trained on, plus `na` for text in none of them.
pub const LANGUAGES: [&str; 32] = [
    "en", "ko", "ja", "ar", "bg", "cs", "da", "de", "el", "es", "et", "fi", "fr", "hi", "hr", "hu",
    "id", "it", "lt", "lv", "nl", "pl", "pt", "ro", "ru", "sk", "sl", "sv", "tr", "uk", "vi", "na",
];

/// Whether `lang` is a tag the model knows.
#[must_use]
pub fn is_language(lang: &str) -> bool {
    LANGUAGES.contains(&lang)
}

/// Clean one clause and wrap it in its language tags.
///
/// The order matters and is the model's own: decompose first, because the vocabulary holds
/// decomposed forms and a precomposed character is a code point the table does not have.
#[must_use]
pub fn preprocess(text: &str, lang: &str) -> String {
    let mut text: String = text.nfkd().filter(|ch| !is_emoji(*ch)).collect();

    // One pass over the characters rather than a `replace` per pair: twenty-one passes over a
    // clause is twenty-one allocations, and a character map is one.
    text = text
        .chars()
        .map(|ch| match ch {
            '\u{2013}' | '\u{2011}' | '\u{2014}' => '-',
            '_' | '[' | ']' | '|' | '/' | '#' | '\u{2192}' | '\u{2190}' | '\u{2665}'
            | '\u{2606}' | '\u{2661}' | '\u{a9}' | '\\' => ' ',
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2018}' | '\u{2019}' | '\u{b4}' | '`' => '\'',
            other => other,
        })
        .collect();

    text = text.replace('@', " at ");
    text = text.replace("e.g.,", "for example, ");
    text = text.replace("i.e.,", "that is, ");

    // A space before punctuation is a code point the table has, and it makes the model pause
    // in the middle of a sentence. Removing them is cheaper than teaching it to ignore them.
    let mut spaced = String::with_capacity(text.len());
    let mut previous = '\0';
    for ch in text.chars() {
        if matches!(ch, ',' | '.' | '!' | '?' | ';' | ':' | '\'') && previous == ' ' {
            spaced.pop();
        }
        spaced.push(ch);
        previous = ch;
    }
    text = spaced;

    while text.contains("\"\"") {
        text = text.replace("\"\"", "\"");
    }
    while text.contains("''") {
        text = text.replace("''", "'");
    }
    text = text.split_whitespace().collect::<Vec<_>>().join(" ");

    if !text.is_empty() && !text.ends_with(is_closing) {
        text.push('.');
    }
    format!("<{lang}>{text}</{lang}>")
}

/// Characters that may end a clause without the model needing a full stop.
fn is_closing(ch: char) -> bool {
    matches!(
        ch,
        '.' | '!'
            | '?'
            | ';'
            | ':'
            | ','
            | '\''
            | '"'
            | '\u{201c}'
            | '\u{201d}'
            | '\u{2018}'
            | '\u{2019}'
            | ')'
            | ']'
            | '}'
            | '\u{2026}'
            | '\u{3002}'
            | '\u{300d}'
            | '\u{300f}'
            | '\u{3009}'
            | '\u{300b}'
            | '\u{203a}'
            | '\u{bb}'
    )
}

/// The emoji blocks the model was not trained on.
fn is_emoji(ch: char) -> bool {
    matches!(ch as u32,
        0x1F600..=0x1F64F
        | 0x1F300..=0x1F5FF
        | 0x1F680..=0x1F6FF
        | 0x1F700..=0x1F77F
        | 0x1F780..=0x1F7FF
        | 0x1F800..=0x1F8FF
        | 0x1F900..=0x1F9FF
        | 0x1FA00..=0x1FA6F
        | 0x1FA70..=0x1FAFF
        | 0x2600..=0x26FF
        | 0x2700..=0x27BF
        | 0x1F1E6..=0x1F1FF)
}

/// The code points of `text`, as ids the model's embedding understands.
///
/// A code point the table does not carry becomes `-1`, which the mask hides; the alternative,
/// dropping it, would shift every id after it.
#[must_use]
pub fn index(text: &str, table: &[i32]) -> Vec<i64> {
    text.chars()
        .map(|ch| table.get(ch as usize).map_or(-1, |id| i64::from(*id)))
        .collect()
}

/// A mask that is one where `length` reaches and zero after it, laid out `[1, 1, length]`.
#[must_use]
pub fn mask(length: usize) -> Vec<f32> {
    vec![1.0; length]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clause_is_wrapped_in_its_language() {
        assert_eq!(preprocess("Hello there.", "en"), "<en>Hello there.</en>");
    }

    #[test]
    fn a_clause_without_punctuation_gets_a_full_stop() {
        // The model was trained on sentences. A clause that already ends in punctuation keeps
        // it: a comma is a pause the text asked for.
        assert_eq!(preprocess("Hello there,", "en"), "<en>Hello there,</en>");
        assert_eq!(preprocess("Hello there", "en"), "<en>Hello there.</en>");
    }

    #[test]
    fn an_emoji_is_dropped_and_the_text_survives() {
        assert_eq!(
            preprocess("Great \u{1F600} work.", "en"),
            "<en>Great work.</en>"
        );
    }

    #[test]
    fn a_space_before_punctuation_is_removed() {
        assert_eq!(
            preprocess("Hello , world .", "en"),
            "<en>Hello, world.</en>"
        );
    }

    #[test]
    fn an_unknown_code_point_is_minus_one_rather_than_dropped() {
        // Dropping it would shift every id after it, and the mask would then cover the wrong
        // characters.
        let table = vec![0i32; 100];
        let ids = index("a\u{1F600}b", &table);
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[1], -1, "the emoji is the second character");
    }

    #[test]
    fn the_language_list_is_the_models_own() {
        assert!(is_language("en"));
        assert!(is_language("na"));
        assert!(!is_language("xx"));
    }
}
