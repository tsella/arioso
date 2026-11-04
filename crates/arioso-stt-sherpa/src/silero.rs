//! Silero voice activity detection.
//!
//! The model classifies one short frame at a time, and sherpa-onnx already runs it on the
//! runtime this crate links for speech recognition. So the detector is a [`Gate`]: it answers
//! whether a frame holds speech, and the segmenter in `arioso-vad` decides where utterances
//! begin and end. The boundary rules stay in one place, whichever gate feeds them.

use std::path::{Path, PathBuf};

use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};

use arioso_vad::Gate;

/// The rate the model reads.
pub const RATE: u32 = 16_000;

/// The frame length the model reads, in milliseconds.
pub const FRAME_MS: u32 = 32;

/// Samples in one frame at [`RATE`].
pub const FRAME_SAMPLES: usize = (RATE as usize * FRAME_MS as usize) / 1000;

/// What went wrong building a detector.
#[derive(Debug, thiserror::Error)]
pub enum SileroError {
    /// The model file is not where the manifest said it would be.
    #[error("the Silero model is not at {}", .0.display())]
    Missing(PathBuf),
    /// The runtime refused the model.
    #[error("the Silero model at {} could not be loaded", .0.display())]
    Load(PathBuf),
}

/// A gate that asks Silero whether the frame holds speech.
pub struct Silero {
    detector: VoiceActivityDetector,
    /// Samples handed in that do not yet make a frame.
    pending: Vec<f32>,
    /// The answer for the frame last completed.
    last: bool,
}

impl Silero {
    /// Load the model at `model`.
    ///
    /// `probability` is the confidence above which a frame counts as speech. It is a
    /// different number from the energy gate's threshold, which is a level: the two are not
    /// interchangeable and the config keeps them apart.
    pub fn load(model: &Path, probability: f32) -> Result<Self, SileroError> {
        if !model.is_file() {
            return Err(SileroError::Missing(model.to_path_buf()));
        }
        let config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(model.to_string_lossy().into_owned()),
                threshold: probability,
                // The segmenter owns the boundaries: this detector answers for one frame.
                // These are the shortest runs sherpa will honour, so it does not hold a
                // decision back while the segmenter is already cutting on it.
                min_silence_duration: 0.1,
                min_speech_duration: 0.0,
                window_size: FRAME_SAMPLES as i32,
                max_speech_duration: 30.0,
            },
            sample_rate: RATE as i32,
            num_threads: 1,
            provider: None,
            debug: false,
            ..VadModelConfig::default()
        };
        // A second of buffer: the detector queues segments, and this gate never reads them,
        // so the buffer only has to hold what the model writes between frames.
        let detector = VoiceActivityDetector::create(&config, 1.0)
            .ok_or_else(|| SileroError::Load(model.to_path_buf()))?;
        Ok(Self {
            detector,
            pending: Vec::with_capacity(FRAME_SAMPLES),
            last: false,
        })
    }
}

impl Gate for Silero {
    fn speech(&mut self, frame: &[f32]) -> bool {
        self.pending.extend_from_slice(frame);
        while self.pending.len() >= FRAME_SAMPLES {
            let rest = self.pending.split_off(FRAME_SAMPLES);
            self.detector.accept_waveform(&self.pending);
            self.last = self.detector.detected();
            self.pending = rest;
        }
        // A frame shorter than the model's window is answered with the state the last whole
        // window produced: the segmenter cuts per frame, and a partial window cannot be
        // classified without inventing the samples it is missing.
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The model is 643 KiB and not in the repository, so this runs when it is on disk.
    ///
    /// `ARIOSO_TEST_SILERO` names a directory holding `silero_vad.onnx` and one 16 kHz mono
    /// 16-bit WAV of speech. A tone will not do: the model is trained on voices and answers
    /// correctly that a sine is not one.
    fn fixture() -> Option<(PathBuf, PathBuf)> {
        let dir = PathBuf::from(std::env::var_os("ARIOSO_TEST_SILERO")?);
        let model = dir.join("silero_vad.onnx");
        let wav = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "wav"))?;
        (model.is_file() && wav.is_file()).then_some((model, wav))
    }

    /// Samples from a 16 kHz mono 16-bit WAV, read by its `data` chunk.
    fn read_wav(path: &Path) -> Vec<f32> {
        let bytes = std::fs::read(path).expect("the wav is readable");
        let start = bytes
            .windows(4)
            .position(|window| window == b"data")
            .expect("a data chunk")
            + 8;
        bytes[start..]
            .chunks_exact(2)
            .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / 32_768.0)
            .collect()
    }

    #[test]
    fn a_missing_model_is_named() {
        let error = match Silero::load(Path::new("/nonexistent/silero_vad.onnx"), 0.5) {
            Ok(_) => panic!("a file that does not exist was loaded"),
            Err(error) => error,
        };
        assert!(matches!(error, SileroError::Missing(_)), "{error}");
    }

    #[test]
    fn speech_and_silence_are_told_apart() {
        let Some((model, wav)) = fixture() else {
            // No fixture on this machine: the load path is covered above, and the rest needs
            // the model and a recording.
            return;
        };
        let mut gate = Silero::load(&model, 0.5).expect("loading the model");
        let speech = read_wav(&wav);

        let frames = speech.len() / FRAME_SAMPLES;
        let heard = speech
            .chunks_exact(FRAME_SAMPLES)
            .filter(|frame| gate.speech(frame))
            .count();
        assert!(
            heard > frames / 4,
            "{heard} of {frames} frames of speech were called speech"
        );

        let quiet = vec![0.0_f32; FRAME_SAMPLES];
        let mut heard_silence = false;
        for _ in 0..80 {
            heard_silence |= gate.speech(&quiet);
        }
        assert!(!heard_silence, "silence was called speech");
    }
}
