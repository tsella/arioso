//! Streaming speech to text through sherpa-onnx.
//!
//! A streaming model keeps the decoder state for the audio it has already heard, so a
//! partial is what it currently believes rather than the batch engine run again over a
//! growing buffer. The binding this crate links against, sherpa-onnx 2.0.1, exposes
//! exactly the calls the loop needs (see its `online_asr.rs`):
//!
//! - `OnlineRecognizer::create(&OnlineRecognizerConfig) -> Option<OnlineRecognizer>`
//! - `OnlineRecognizer::create_stream(&self) -> OnlineStream`
//! - `OnlineStream::accept_waveform(rate: i32, samples: &[f32])`
//! - `OnlineRecognizer::is_ready(&self, &OnlineStream) -> bool`
//! - `OnlineRecognizer::decode(&self, &OnlineStream)`
//! - `OnlineRecognizer::get_result(&self, &OnlineStream) -> Option<RecognizerResult>`,
//!   whose `text` field is the hypothesis so far
//! - `OnlineStream::input_finished()`
//!
//! One recognizer is shared; each utterance gets its own [`StreamSession`], so the model
//! is loaded once per process and a finished utterance leaves no state behind for the
//! next one.

use std::path::Path;
use std::sync::Arc;

use sherpa_onnx::{
    OnlineModelConfig, OnlineRecognizer, OnlineRecognizerConfig, OnlineStream,
    OnlineTransducerModelConfig,
};

use crate::{ENGINE_RATE, SttError, not_a_model};

/// Most decode steps one call will run before it gives up.
///
/// `is_ready` is documented to turn false once the audio in hand is decoded, and the loop
/// ends there. A malformed model that never says so would otherwise hold a worker thread
/// forever, so the loop is bounded. The bound is far above what a chunk of audio needs:
/// one step consumes one feature frame, and a step is a tenth of a second of audio.
const MAX_STEPS: usize = 4096;

/// A loaded streaming model.
pub struct StreamingEngine {
    recognizer: Arc<OnlineRecognizer>,
}

impl std::fmt::Debug for StreamingEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamingEngine { .. }")
    }
}

impl StreamingEngine {
    /// Load a streaming transducer: encoder, decoder, joiner, tokens.
    ///
    /// `model_type` is not passed on to the engine. The four file roles are enough for
    /// sherpa-onnx to identify a transducer, and the catalog's name for the family
    /// (`zipformer_streaming`) is not one the engine's own `model_type` field accepts.
    ///
    /// # Errors
    ///
    /// Returns [`SttError::NotAModel`] when a file is missing, and [`SttError::Load`]
    /// when the recognizer refuses the configuration.
    pub fn from_files(
        dir: &Path,
        encoder: &Path,
        decoder: &Path,
        joiner: &Path,
        tokens: &Path,
        threads: usize,
    ) -> Result<Self, SttError> {
        let present = |path: &Path, what: &str| {
            if path.is_file() {
                Ok(path.display().to_string())
            } else {
                Err(not_a_model(dir, what))
            }
        };
        let config = OnlineRecognizerConfig {
            model_config: OnlineModelConfig {
                transducer: OnlineTransducerModelConfig {
                    encoder: Some(present(encoder, "a streaming encoder *.onnx")?),
                    decoder: Some(present(decoder, "a streaming decoder *.onnx")?),
                    joiner: Some(present(joiner, "a streaming joiner *.onnx")?),
                },
                tokens: Some(present(tokens, "a *tokens.txt")?),
                num_threads: i32::try_from(threads).unwrap_or(4),
                ..OnlineModelConfig::default()
            },
            // Greedy search is the cheapest decoder and the one whose partials settle
            // soonest, which is what a live socket shows a person.
            decoding_method: Some("greedy_search".to_owned()),
            // Endpoints are the segmenter's job here: it owns the offsets the final
            // frame carries, so a second opinion from the model would only disagree.
            enable_endpoint: false,
            ..OnlineRecognizerConfig::default()
        };
        let recognizer =
            OnlineRecognizer::create(&config).ok_or_else(|| SttError::Load(dir.to_owned()))?;
        Ok(Self {
            recognizer: Arc::new(recognizer),
        })
    }

    /// Start a stream for one utterance.
    #[must_use]
    pub fn session(&self) -> StreamSession {
        StreamSession {
            stream: self.recognizer.create_stream(),
            recognizer: Arc::clone(&self.recognizer),
            text: String::new(),
        }
    }
}

/// The model's state for one utterance.
///
/// Dropping it throws the hypothesis away, which is what the end of an utterance means:
/// the next one starts from silence, not from the words before it.
pub struct StreamSession {
    recognizer: Arc<OnlineRecognizer>,
    stream: OnlineStream,
    /// The text last handed to the caller, so a repeat is not sent twice.
    text: String,
}

impl std::fmt::Debug for StreamSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamSession { .. }")
    }
}

impl StreamSession {
    /// Feed audio and return the new hypothesis, when the model changed its mind.
    ///
    /// Returns `None` while the text is unchanged, and while it is empty: a stream that
    /// has not heard a word yet has nothing to say, and an empty partial would clear the
    /// line the client is showing.
    pub fn feed(&mut self, samples: &[f32]) -> Option<String> {
        if samples.is_empty() {
            return None;
        }
        self.stream
            .accept_waveform(i32::try_from(ENGINE_RATE).unwrap_or(16_000), samples);
        self.decode_ready();
        let text = self.hypothesis();
        if text.is_empty() || text == self.text {
            return None;
        }
        self.text.clone_from(&text);
        Some(text)
    }

    /// Close the utterance and return its text.
    ///
    /// The tail is decoded after `input_finished`, so the last chunk is not lost: without
    /// it the model still holds a few frames of context it has not turned into words.
    #[must_use]
    pub fn finish(mut self) -> String {
        self.stream.input_finished();
        self.decode_ready();
        let text = self.hypothesis();
        if !text.is_empty() {
            self.text = text.clone();
        }
        text
    }

    /// The hypothesis so far, trimmed.
    fn hypothesis(&self) -> String {
        self.recognizer
            .get_result(&self.stream)
            .map(|result| result.text.trim().to_owned())
            .unwrap_or_default()
    }

    /// Decode until the recognizer has nothing left it can use.
    fn decode_ready(&self) {
        for _ in 0..MAX_STEPS {
            if !self.recognizer.is_ready(&self.stream) {
                return;
            }
            self.recognizer.decode(&self.stream);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_named_before_the_engine_is_asked() {
        let dir = tempfile::tempdir().unwrap();
        let encoder = dir.path().join("encoder.onnx");
        let decoder = dir.path().join("decoder.onnx");
        let joiner = dir.path().join("joiner.onnx");
        let tokens = dir.path().join("tokens.txt");
        for path in [&encoder, &decoder, &tokens] {
            std::fs::write(path, b"").unwrap();
        }
        // The joiner is absent, so the refusal names it instead of handing four paths to
        // the engine and reporting whatever it says about a file it cannot open.
        let error =
            StreamingEngine::from_files(dir.path(), &encoder, &decoder, &joiner, &tokens, 1)
                .unwrap_err();
        assert!(matches!(error, SttError::NotAModel { .. }), "{error}");
        assert!(error.to_string().contains("joiner"), "{error}");
    }
}
