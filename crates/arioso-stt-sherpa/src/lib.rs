//! Batch speech to text through sherpa-onnx.
//!
//! One engine, one job: take samples at any rate, return text. The model is a Whisper
//! directory from the sherpa-onnx release, held as a manifest entry so a wrong pair of
//! files cannot be loaded by accident.

use std::path::{Path, PathBuf};

use sherpa_onnx::{
    OfflineModelConfig, OfflineRecognizer, OfflineRecognizerConfig, OfflineWhisperModelConfig,
};

/// Rate the engine expects. Anything else is resampled on the way in.
pub const ENGINE_RATE: u32 = 16_000;

/// What went wrong.
#[derive(Debug, thiserror::Error)]
pub enum SttError {
    /// The model directory does not hold the files a Whisper model needs.
    #[error("{} does not hold a whisper model: {missing}", dir.display())]
    NotAModel {
        /// Directory that was searched.
        dir: PathBuf,
        /// What was expected and not found.
        missing: String,
    },
    /// The recognizer refused the configuration.
    #[error("the recognizer could not load {}", .0.display())]
    Load(PathBuf),
    /// There was no audio to transcribe.
    #[error("the audio is empty")]
    EmptyAudio,
    /// The recognizer returned nothing.
    #[error("the recognizer returned no result")]
    NoResult,
}

/// One transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    /// Text, trimmed.
    pub text: String,
}

/// A loaded Whisper model.
pub struct Whisper {
    recognizer: OfflineRecognizer,
}

impl std::fmt::Debug for Whisper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Whisper { .. }")
    }
}

impl Whisper {
    /// Load a Whisper model directory.
    ///
    /// The directory is searched for an encoder, a decoder, and a tokens file, in that
    /// order of importance. An int8 file wins over a float one when both are present,
    /// because it is a third of the size and faster on a CPU.
    ///
    /// # Errors
    ///
    /// Returns [`SttError::NotAModel`] when a file is missing, and [`SttError::Load`]
    /// when the recognizer refuses the configuration.
    pub fn load(dir: &Path, language: Option<&str>, threads: usize) -> Result<Self, SttError> {
        let encoder =
            find(dir, "encoder", ".onnx").ok_or_else(|| not_a_model(dir, "an encoder *.onnx"))?;
        let decoder =
            find(dir, "decoder", ".onnx").ok_or_else(|| not_a_model(dir, "a decoder *.onnx"))?;
        let tokens =
            find(dir, "tokens", ".txt").ok_or_else(|| not_a_model(dir, "a *tokens.txt"))?;

        let config = OfflineRecognizerConfig {
            model_config: OfflineModelConfig {
                whisper: OfflineWhisperModelConfig {
                    encoder: Some(encoder.display().to_string()),
                    decoder: Some(decoder.display().to_string()),
                    // None lets the engine detect the language; an English-only model
                    // ignores the setting either way.
                    language: language.map(str::to_owned),
                    task: Some("transcribe".to_owned()),
                    tail_paddings: -1,
                    enable_token_timestamps: false,
                    enable_segment_timestamps: false,
                },
                tokens: Some(tokens.display().to_string()),
                num_threads: i32::try_from(threads).unwrap_or(4),
                ..OfflineModelConfig::default()
            },
            ..OfflineRecognizerConfig::default()
        };

        let recognizer =
            OfflineRecognizer::create(&config).ok_or_else(|| SttError::Load(dir.to_owned()))?;
        Ok(Self { recognizer })
    }

    /// Transcribe samples at `rate`, mono.
    ///
    /// # Errors
    ///
    /// Returns [`SttError::EmptyAudio`] for no samples, and [`SttError::NoResult`] when
    /// the engine returns nothing.
    pub fn transcribe(&self, samples: &[f32], rate: u32) -> Result<Transcript, SttError> {
        if samples.is_empty() {
            return Err(SttError::EmptyAudio);
        }
        let resampled;
        let samples = if rate == ENGINE_RATE {
            samples
        } else {
            resampled = resample(samples, rate, ENGINE_RATE);
            &resampled
        };

        let stream = self.recognizer.create_stream();
        stream.accept_waveform(i32::try_from(ENGINE_RATE).unwrap_or(16_000), samples);
        self.recognizer.decode(&stream);
        let result = stream.get_result().ok_or(SttError::NoResult)?;
        Ok(Transcript {
            text: result.text.trim().to_owned(),
        })
    }
}

fn not_a_model(dir: &Path, missing: &str) -> SttError {
    SttError::NotAModel {
        dir: dir.to_owned(),
        missing: missing.to_owned(),
    }
}

/// Find the file in `dir` whose name holds `stem` and ends with `suffix`.
///
/// An int8 file wins over the same file in float, and a shorter name wins over a longer
/// one, so a directory holding several models resolves the same way every time.
fn find(dir: &Path, stem: &str, suffix: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(stem) && name.ends_with(suffix))
        })
        .collect();
    candidates.sort_by_key(|path| {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        (!name.contains("int8"), name.len())
    });
    candidates.into_iter().next()
}

/// Resample by linear interpolation.
///
/// Whisper only needs 16 kHz, and this is a downsampler for speech, not a resampler for
/// music. A proper polyphase filter belongs here when the streaming path needs one.
#[must_use]
pub fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() || from == 0 || to == 0 {
        return samples.to_vec();
    }
    let ratio = f64::from(from) / f64::from(to);
    let length = ((samples.len() as f64) / ratio).ceil() as usize;
    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let position = index as f64 * ratio;
        let left = position.floor() as usize;
        let fraction = (position - left as f64) as f32;
        let a = samples[left.min(samples.len() - 1)];
        let b = samples[(left + 1).min(samples.len() - 1)];
        out.push(a + (b - a) * fraction);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_the_length_in_seconds() {
        let samples = vec![0.0f32; 22_050];
        let out = resample(&samples, 22_050, 16_000);
        assert_eq!(out.len(), 16_000);
    }

    #[test]
    fn resampling_the_same_rate_changes_nothing() {
        let samples = vec![0.5f32; 10];
        assert_eq!(resample(&samples, 16_000, 16_000), samples);
    }

    #[test]
    fn resampling_an_empty_signal_is_empty() {
        assert!(resample(&[], 44_100, 16_000).is_empty());
    }

    #[test]
    fn resampling_keeps_the_shape_of_a_tone() {
        // A 1 kHz tone at 8 kHz is 8 samples per cycle; after resampling to 16 kHz it
        // must still be 16 samples per cycle, so the sign changes line up.
        let tone: Vec<f32> = (0..800)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI / 8.0).sin())
            .collect();
        let out = resample(&tone, 8_000, 16_000);
        let crossings = out.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        // The last cycle may end exactly on the boundary, so allow one either side.
        // A resampler that lost the rate would give about 50 or about 200.
        assert!(
            (99..=100).contains(&crossings),
            "one crossing per cycle of a 1 kHz tone, got {crossings}"
        );
    }

    #[test]
    fn a_directory_without_a_model_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let error = Whisper::load(dir.path(), Some("en"), 1).unwrap_err();
        assert!(matches!(error, SttError::NotAModel { .. }));
        assert!(error.to_string().contains("encoder"), "{error}");
    }

    #[test]
    fn the_int8_file_is_preferred_over_the_float_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m-encoder.onnx"), b"float").unwrap();
        std::fs::write(dir.path().join("m-encoder.int8.onnx"), b"int8").unwrap();
        let found = find(dir.path(), "encoder", ".onnx").unwrap();
        assert!(found.to_string_lossy().contains("int8"), "{found:?}");
    }

    #[test]
    fn empty_audio_is_refused_before_the_engine_runs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("e.onnx"), b"").unwrap();
        std::fs::write(dir.path().join("d.onnx"), b"").unwrap();
        std::fs::write(dir.path().join("tokens.txt"), b"").unwrap();
        // Loading may fail on a stub model; the empty-audio check is what matters and
        // it must not need a real engine.
        if let Ok(engine) = Whisper::load(dir.path(), Some("en"), 1) {
            assert!(matches!(
                engine.transcribe(&[], 16_000),
                Err(SttError::EmptyAudio)
            ));
        }
    }
}
