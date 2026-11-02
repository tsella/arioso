//! Piper voice inference.
//!
//! A Piper voice is a VITS model in ONNX plus a JSON config holding its phoneme
//! table and synthesis scales.

use std::path::Path;

use arioso_g2p::PhonemeMap;
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
}

/// A loaded Piper voice.
pub struct PiperVoice {
    session: Session,
    map: PhonemeMap,
    sample_rate: u32,
    scales: [f32; 3],
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

        let sample_rate = document
            .pointer("/audio/sample_rate")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| PiperError::Config("missing audio.sample_rate".into()))?;
        let scale = |key: &str, fallback: f32| -> f32 {
            document
                .pointer(&format!("/inference/{key}"))
                .and_then(serde_json::Value::as_f64)
                .map_or(fallback, |v| v as f32)
        };
        let scales = [
            scale("noise_scale", 0.667),
            scale("length_scale", 1.0),
            scale("noise_w", 0.8),
        ];

        let map = PhonemeMap::from_voice_config(&text)?;
        let session = Session::builder()?.commit_from_file(model)?;

        Ok(Self {
            session,
            map,
            sample_rate: u32::try_from(sample_rate).unwrap_or(22_050),
            scales,
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
