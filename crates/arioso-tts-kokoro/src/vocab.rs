//! The model's symbol table: one id per IPA character.
//!
//! Kokoro does not ship a per-voice phoneme map the way Piper does. The table is the
//! `vocab` object in the model's own `config.json`, and it is keyed by single characters,
//! so an affricate written as two characters is two tokens — which is what the model was
//! trained on.

use std::collections::HashMap;

use crate::KokoroError;

/// A character-to-id table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vocab {
    ids: HashMap<char, u32>,
}

impl Vocab {
    /// Read the table from the model's `config.json`.
    ///
    /// # Errors
    ///
    /// Returns [`KokoroError::Config`] when the document is not JSON or has no `vocab`
    /// object of character-to-number entries.
    pub fn from_config(json: &str) -> Result<Self, KokoroError> {
        let document: serde_json::Value = serde_json::from_str(json)
            .map_err(|error| KokoroError::Config(format!("parsing config: {error}")))?;
        let table = document
            .get("vocab")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| KokoroError::Config("config has no vocab object".into()))?;
        let mut ids = HashMap::with_capacity(table.len());
        for (symbol, value) in table {
            let mut chars = symbol.chars();
            let (Some(single), None) = (chars.next(), chars.next()) else {
                return Err(KokoroError::Config(format!(
                    "vocab key {symbol:?} is not a single character"
                )));
            };
            let id = value
                .as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .ok_or_else(|| {
                    KokoroError::Config(format!("vocab id for {symbol:?} is not a number"))
                })?;
            ids.insert(single, id);
        }
        if ids.is_empty() {
            return Err(KokoroError::Config("vocab object is empty".into()));
        }
        Ok(Self { ids })
    }

    /// The id for one character, if the model knows it.
    #[must_use]
    pub fn id(&self, symbol: char) -> Option<u32> {
        self.ids.get(&symbol).copied()
    }

    /// How many characters the table holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the table holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Turn a run of phoneme symbols into model tokens.
    ///
    /// Each character of each symbol is looked up. A character the model does not know
    /// is dropped and returned, so the caller can say what was lost rather than quietly
    /// shortening the sentence.
    #[must_use]
    pub fn tokenize(&self, symbols: &[&str]) -> (Vec<i64>, Vec<char>) {
        let mut tokens = Vec::with_capacity(symbols.len() + 8);
        let mut dropped = Vec::new();
        for symbol in symbols {
            for character in symbol.chars() {
                match self.id(character) {
                    Some(id) => tokens.push(i64::from(id)),
                    None => {
                        if !dropped.contains(&character) {
                            dropped.push(character);
                        }
                    }
                }
            }
        }
        (tokens, dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"{"istftnet":{},"vocab":{"a":43,"b":44," ":16,"ˈ":156,"ʒ":147}}"#;

    #[test]
    fn characters_map_to_ids_and_affricates_are_two_tokens() {
        let vocab = Vocab::from_config(CONFIG).unwrap();
        let (tokens, dropped) = vocab.tokenize(&["b", "ˈ", "a", "dʒ"]);
        // "dʒ" is two characters, and only the second is in this table, so the first
        // is reported rather than silently skipped.
        assert_eq!(tokens, vec![44, 156, 43, 147]);
        assert_eq!(dropped, vec!['d']);
    }

    #[test]
    fn a_config_without_a_vocab_is_refused() {
        assert!(matches!(
            Vocab::from_config(r#"{"istftnet":{}}"#),
            Err(KokoroError::Config(_))
        ));
        assert!(matches!(
            Vocab::from_config("not json"),
            Err(KokoroError::Config(_))
        ));
    }

    #[test]
    fn a_multi_character_key_is_refused() {
        assert!(matches!(
            Vocab::from_config(r#"{"vocab":{"ab":1}}"#),
            Err(KokoroError::Config(_))
        ));
    }
}
