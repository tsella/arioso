//! Piper voice inference.
//!
//! A Piper voice is a VITS model in ONNX plus a JSON config holding its phoneme
//! table and synthesis scales.

use std::path::Path;

use arioso_core::{Entry, Piece, UserDictionary, normalize_with};
use arioso_core::{chunk_text, sentences};
use arioso_g2p::{CmuDict, PhonemeMap, arpabet};
use ort::session::Session;
use ort::value::Tensor;
use thiserror::Error;

pub mod wav;

/// Why a voice could not be loaded or used.
#[derive(Debug, Error)]
pub enum PiperError {
    /// A file could not be read.
    #[error("reading {path}: {source}")]
    Io {
        /// File that failed.
        path: std::path::PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// The config is not valid JSON or lacks a required field.
    #[error("parsing voice config: {0}")]
    Config(String),
    /// The phoneme table is unusable.
    #[error("phoneme table: {0}")]
    Phonemes(#[from] arioso_g2p::G2pError),
    /// ONNX Runtime refused a step.
    #[error("onnx: {0}")]
    Ort(#[from] ort::Error),
    /// The model produced no output tensor.
    #[error("model returned no audio")]
    NoOutput,
    /// The caller stopped accepting audio.
    #[error("{0}")]
    Consumer(String),
}

/// Longest piece of text handed to the model, in characters.
pub const SYNTHESIS_CHUNK: usize = 300;

/// How a render is done.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Speaking rate, as a multiplier of the voice's own length scale.
    pub speed: f32,
    /// How text is expanded before it is phonemized.
    pub normalize: arioso_core::NormalizeOptions,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            speed: 1.0,
            normalize: arioso_core::NormalizeOptions::default(),
        }
    }
}

/// What a render could not say.
#[derive(Debug, Default, Clone)]
pub struct RenderReport {
    /// Words absent from the built-in dictionary and pronounced by rule.
    pub guessed: Vec<String>,
    /// Words nothing could pronounce.
    pub missing: Vec<String>,
    /// Dictionary entries no form of which the voice can render.
    pub unusable: Vec<String>,
}

/// Whether a form is written in ARPAbet rather than as a respelling.
///
/// Every token must be a phoneme the table knows, in any case. A capitalised
/// respelling such as "TOM SELLER" fails this test and is looked up as words.
fn is_arpabet(form: &str) -> bool {
    let mut seen = false;
    for token in form.split_whitespace() {
        if !arioso_g2p::arpabet::is_phoneme(token) {
            return false;
        }
        seen = true;
    }
    seen
}

/// A loaded Piper voice.
pub struct PiperVoice {
    session: Session,
    map: PhonemeMap,
    sample_rate: u32,
    scales: [f32; 3],
    /// The voice's own length scale, before a speed multiplier.
    length_scale: f32,
}

impl std::fmt::Debug for PiperVoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PiperVoice")
            .field("sample_rate", &self.sample_rate)
            .field("scales", &self.scales)
            .field("symbols", &self.map.len())
            .finish_non_exhaustive()
    }
}

impl PiperVoice {
    /// Load a voice from its ONNX model and its JSON config.
    ///
    /// # Errors
    ///
    /// Returns [`PiperError`] when either file cannot be read or parsed, the
    /// phoneme table is unusable, or ONNX Runtime rejects the model.
    pub fn load(model: &Path, config: &Path) -> Result<Self, PiperError> {
        let text = std::fs::read_to_string(config).map_err(|source| PiperError::Io {
            path: config.to_path_buf(),
            source,
        })?;
        let document: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| PiperError::Config(e.to_string()))?;

        let raw_rate = document
            .pointer("/audio/sample_rate")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| PiperError::Config("missing audio.sample_rate".into()))?;
        let sample_rate = u32::try_from(raw_rate)
            .ok()
            .filter(|rate| (8_000..=48_000).contains(rate))
            .ok_or_else(|| {
                PiperError::Config(format!("audio.sample_rate {raw_rate} is out of range"))
            })?;
        let scale = |key: &str, fallback: f32| -> f32 {
            document
                .pointer(&format!("/inference/{key}"))
                .and_then(serde_json::Value::as_f64)
                .map_or(fallback, |v| v as f32)
        };
        let length_scale = scale("length_scale", 1.0);
        let scales = [
            scale("noise_scale", 0.667),
            length_scale,
            scale("noise_w", 0.8),
        ];

        let map = PhonemeMap::from_voice_config(&text)?;
        let session = Session::builder()?.commit_from_file(model)?;

        Ok(Self {
            session,
            map,
            sample_rate,
            scales,
            length_scale,
        })
    }

    /// Sample rate of the audio this voice produces.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Number of symbols the voice knows.
    #[must_use]
    pub fn symbols(&self) -> usize {
        self.map.len()
    }

    /// Speak `text`, handing each piece of audio to `sink` as it is ready.
    ///
    /// The text is split into sentences, then capped by [`SYNTHESIS_CHUNK`].
    /// Words missing from `dictionary` are skipped and returned, so the caller
    /// can report them. One implementation serves both the CLI and the server;
    /// splitting it would let their output drift apart.
    ///
    /// # Errors
    ///
    /// Returns [`PiperError`] when synthesis fails or `sink` refuses audio.
    pub fn render(
        &mut self,
        text: &str,
        dictionary: &CmuDict,
        user: &UserDictionary,
        options: RenderOptions,
        mut sink: impl FnMut(&[f32], u32) -> Result<(), PiperError>,
    ) -> Result<RenderReport, PiperError> {
        self.set_speed(options.speed);
        let rate = self.sample_rate();
        let mut report = RenderReport::default();

        for sentence in sentences(text) {
            for chunk in chunk_text(sentence, SYNTHESIS_CHUNK) {
                let mut groups: Vec<Vec<&'static str>> = Vec::new();
                for piece in user.pieces(chunk) {
                    match piece {
                        Piece::Entry(entry) => {
                            match self.entry_symbols(entry, dictionary, options.normalize) {
                                Some(symbols) => groups.push(symbols),
                                None => {
                                    // The voice cannot say any form of this entry, so
                                    // say the term itself and report it.
                                    report.unusable.push(entry.term.clone());
                                    groups.push(self.words(
                                        &entry.term,
                                        dictionary,
                                        &mut report,
                                        options.normalize,
                                    ));
                                }
                            }
                        }
                        Piece::Text(segment) => {
                            groups.push(self.words(
                                segment,
                                dictionary,
                                &mut report,
                                options.normalize,
                            ));
                        }
                    }
                }

                let mut symbols: Vec<&str> = Vec::new();
                for group in groups.into_iter().filter(|group| !group.is_empty()) {
                    if !symbols.is_empty() {
                        symbols.push(" ");
                    }
                    symbols.extend(group);
                }
                if symbols.is_empty() {
                    continue;
                }
                let audio = self.synthesize(&symbols)?;
                sink(&audio, rate)?;
            }
        }
        Ok(report)
    }

    /// Symbols for a run of text, after normalization.
    fn words(
        &self,
        text: &str,
        dictionary: &CmuDict,
        report: &mut RenderReport,
        options: arioso_core::NormalizeOptions,
    ) -> Vec<&'static str> {
        text_symbols(text, dictionary, report, options, true)
    }

    /// Symbols for a dictionary entry, trying its forms in order.
    ///
    /// Returns `None` when the voice cannot render any of them.
    fn entry_symbols(
        &self,
        entry: &Entry,
        dictionary: &CmuDict,
        options: arioso_core::NormalizeOptions,
    ) -> Option<Vec<&'static str>> {
        entry
            .forms()
            .into_iter()
            .find_map(|form| form_symbols(form, &self.map, dictionary, options, true))
    }

    /// Speaking rate multiplier: 2.0 is twice as fast, 0.5 half as fast.
    ///
    /// Piper slows speech by lengthening the acoustic time axis, so the scale
    /// is divided by the speed.
    pub fn set_speed(&mut self, speed: f32) {
        self.scales[1] = self.length_scale / speed;
    }

    /// The phoneme table, for callers that build sequences themselves.
    #[must_use]
    pub fn map(&self) -> &PhonemeMap {
        &self.map
    }

    /// Synthesize one chunk of phoneme symbols into mono audio samples.
    ///
    /// # Errors
    ///
    /// Returns [`PiperError`] when ONNX Runtime fails or returns no tensor.
    pub fn synthesize(&mut self, symbols: &[&str]) -> Result<Vec<f32>, PiperError> {
        let ids = self.map.to_ids(symbols);
        let length = ids.len() as i64;
        let input: Vec<i64> = ids.iter().map(|&id| i64::from(id)).collect();

        let outputs = self.session.run(ort::inputs![
            "input" => Tensor::from_array(([1i64, length], input))?,
            "input_lengths" => Tensor::from_array(([1i64], vec![length]))?,
            "scales" => Tensor::from_array(([3i64], self.scales.to_vec()))?,
        ])?;

        let output = outputs.get("output").ok_or(PiperError::NoOutput)?;
        let (_shape, samples) = output.try_extract_tensor::<f32>()?;
        Ok(samples.to_vec())
    }
}

/// Symbols for a run of text, after normalization.
fn text_symbols(
    text: &str,
    dictionary: &CmuDict,
    report: &mut RenderReport,
    options: arioso_core::NormalizeOptions,
    allow_rules: bool,
) -> Vec<&'static str> {
    let expanded = normalize_with(text, options);
    let mut symbols = Vec::new();
    for word in
        expanded.split(|c: char| c.is_whitespace() || matches!(c, '-' | '\u{2013}' | '\u{2014}'))
    {
        let cleaned: String = word
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '\'')
            .collect::<String>()
            .to_lowercase();
        if cleaned.is_empty() {
            continue;
        }
        let spoken: Vec<&'static str> = match dictionary.lookup(&cleaned) {
            Some(pronunciations) => arpabet::to_ipa(&pronunciations[0]),
            None => {
                // Not in the dictionary, so spell it out. A guessed word is heard
                // and named; a skipped one leaves a hole.
                let guessed = if allow_rules {
                    arioso_g2p::lts::pronounce(&cleaned)
                } else {
                    Vec::new()
                };
                if guessed.is_empty() {
                    report.missing.push(cleaned);
                } else {
                    report.guessed.push(cleaned);
                }
                guessed
            }
        };
        if spoken.is_empty() {
            continue;
        }
        if !symbols.is_empty() {
            // Words need the space symbol between them: the voice was trained with
            // word boundaries, and without them the sequence reads as one long word.
            symbols.push(" ");
        }
        symbols.extend(spoken);
    }
    symbols
}

/// Symbols for one dictionary form, if this voice can render all of them.
///
/// `arioso doctor` uses this to report entries the configured voice cannot say.
#[must_use]
pub fn form_symbols(
    form: &str,
    map: &PhonemeMap,
    dictionary: &CmuDict,
    options: arioso_core::NormalizeOptions,
    allow_rules: bool,
) -> Option<Vec<&'static str>> {
    let symbols = if is_arpabet(form) {
        arpabet::to_ipa(form)
    } else {
        text_symbols(
            form,
            dictionary,
            &mut RenderReport::default(),
            options,
            allow_rules,
        )
    };
    if symbols.is_empty() || symbols.iter().any(|symbol| map.ids_of(symbol).is_none()) {
        return None;
    }
    Some(symbols)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny dictionary, so the symbol path can be checked without a model.
    const TWO_WORDS: &str = "hello HH AH0 L OW1\nworld W ER1 L D\n";

    #[test]
    fn words_are_separated_by_the_space_symbol() {
        let dictionary = CmuDict::parse(TWO_WORDS);
        let mut report = RenderReport::default();
        let symbols = text_symbols(
            "hello world",
            &dictionary,
            &mut report,
            arioso_core::NormalizeOptions::default(),
            true,
        );
        assert_eq!(
            symbols.iter().filter(|symbol| **symbol == " ").count(),
            1,
            "one boundary between two words: {symbols:?}"
        );
        assert!(report.missing.is_empty(), "{report:?}");
    }

    #[test]
    fn a_word_nothing_can_say_leaves_no_empty_boundary() {
        let dictionary = CmuDict::parse(TWO_WORDS);
        let mut report = RenderReport::default();
        let symbols = text_symbols(
            "hello 123 world",
            &dictionary,
            &mut report,
            arioso_core::NormalizeOptions::default(),
            false,
        );
        assert!(
            !symbols
                .windows(2)
                .any(|pair| pair[0] == " " && pair[1] == " "),
            "no doubled space: {symbols:?}"
        );
        assert_eq!(symbols.last().copied(), Some("d"), "no trailing space");
    }

    #[test]
    fn a_word_outside_the_dictionary_is_guessed_when_rules_are_allowed() {
        let dictionary = CmuDict::parse(TWO_WORDS);
        let mut report = RenderReport::default();
        let symbols = text_symbols(
            "zorbansky",
            &dictionary,
            &mut report,
            arioso_core::NormalizeOptions::default(),
            true,
        );
        assert!(!symbols.is_empty());
        assert_eq!(report.guessed, vec!["zorbansky".to_string()]);
        assert!(report.missing.is_empty());
    }
}
