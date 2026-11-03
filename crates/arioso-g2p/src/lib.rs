//! Grapheme to phoneme conversion for arioso.

pub mod arpabet;
pub mod cmudict;
pub mod inflect;
pub mod lts;
pub mod reverse;

use std::collections::HashMap;

use thiserror::Error;

pub use cmudict::CmuDict;

/// Why a phoneme table could not be built.
#[derive(Debug, Error)]
pub enum G2pError {
    /// The voice config is not valid JSON.
    #[error("parsing voice config: {0}")]
    Json(#[from] serde_json::Error),
    /// The document has no `phoneme_id_map`.
    #[error("voice config has no phoneme_id_map")]
    MissingTable,
    /// A control symbol Piper requires is absent.
    #[error("voice config has no control symbol {0:?} in phoneme_id_map")]
    MissingControl(char),
    /// An entry is not a list of integers.
    #[error("phoneme_id_map entry {symbol:?} is not a list of ids")]
    BadEntry {
        /// The offending key.
        symbol: String,
    },
}

/// IPA symbol to model input id, plus the control symbols Piper expects.
///
/// One symbol can map to several ids, which is how some voices encode
/// affricates and diphthongs.
#[derive(Debug, Clone)]
pub struct PhonemeMap {
    ids: HashMap<String, Vec<u32>>,
    pad: u32,
    bos: u32,
    eos: u32,
}

impl PhonemeMap {
    /// Build the table from a Piper voice config document.
    ///
    /// # Errors
    ///
    /// Returns [`G2pError`] when the JSON is malformed, the table is missing,
    /// a control symbol is absent, or an entry is not a list of integers.
    pub fn from_voice_config(json: &str) -> Result<Self, G2pError> {
        let document: serde_json::Value = serde_json::from_str(json)?;
        let table = document
            .get("phoneme_id_map")
            .and_then(serde_json::Value::as_object)
            .ok_or(G2pError::MissingTable)?;

        let mut ids = HashMap::with_capacity(table.len());
        for (symbol, value) in table {
            let list = value.as_array().ok_or_else(|| G2pError::BadEntry {
                symbol: symbol.clone(),
            })?;
            let mut entry = Vec::with_capacity(list.len());
            for item in list {
                let id = item.as_u64().ok_or_else(|| G2pError::BadEntry {
                    symbol: symbol.clone(),
                })?;
                entry.push(u32::try_from(id).map_err(|_| G2pError::BadEntry {
                    symbol: symbol.clone(),
                })?);
            }
            ids.insert(symbol.clone(), entry);
        }

        let control = |symbol: char| -> Result<u32, G2pError> {
            let key = symbol.to_string();
            ids.get(&key)
                .and_then(|entry| entry.first().copied())
                .ok_or(G2pError::MissingControl(symbol))
        };
        let pad = control('_')?;
        let bos = control('^')?;
        let eos = control('$')?;

        Ok(Self { ids, pad, bos, eos })
    }

    /// Ids for one phoneme, or `None` when the voice does not know it.
    #[must_use]
    pub fn ids_of(&self, phoneme: &str) -> Option<&[u32]> {
        self.ids.get(phoneme).map(Vec::as_slice)
    }

    /// Id of the inter-phoneme pad symbol.
    #[must_use]
    pub fn pad(&self) -> u32 {
        self.pad
    }

    /// Id of the word separator, when the voice maps one.
    ///
    /// espeak emits it between words; a caller that joins words without it
    /// produces one unbroken phoneme run.
    #[must_use]
    pub fn space(&self) -> Option<u32> {
        self.ids.get(" ").and_then(|entry| entry.first().copied())
    }

    /// Id of the beginning-of-sequence symbol.
    #[must_use]
    pub fn bos(&self) -> u32 {
        self.bos
    }

    /// Id of the end-of-sequence symbol.
    #[must_use]
    pub fn eos(&self) -> u32 {
        self.eos
    }

    /// Number of symbols the voice knows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the voice knows no symbols at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Model input ids for a phoneme sequence.
    ///
    /// Piper's convention, as its `phonemes_to_ids` writes it: a begin symbol,
    /// a pad, then each known phoneme followed by one pad, then an end symbol.
    /// Unknown phonemes are dropped rather than padded, so a symbol the voice
    /// lacks cannot shift the alignment.
    #[must_use]
    pub fn to_ids(&self, phonemes: &[&str]) -> Vec<u32> {
        let mut ids = Vec::with_capacity(phonemes.len() * 2 + 3);
        ids.push(self.bos);
        ids.push(self.pad);
        for phoneme in phonemes {
            if let Some(entry) = self.ids.get(*phoneme) {
                ids.extend_from_slice(entry);
                ids.push(self.pad);
            }
        }
        ids.push(self.eos);
        ids
    }

    /// The same sequence, in the type the voice's tensor wants.
    ///
    /// The render path needs `i64`, and converting the `Vec<u32>` afterwards built the
    /// sequence twice for every chunk it spoke.
    #[must_use]
    pub fn to_ids_i64(&self, phonemes: &[&str]) -> Vec<i64> {
        let mut ids = Vec::with_capacity(phonemes.len() * 2 + 3);
        ids.push(i64::from(self.bos));
        ids.push(i64::from(self.pad));
        for phoneme in phonemes {
            if let Some(entry) = self.ids.get(*phoneme) {
                ids.extend(entry.iter().map(|&id| i64::from(id)));
                ids.push(i64::from(self.pad));
            }
        }
        ids.push(i64::from(self.eos));
        ids
    }
}
