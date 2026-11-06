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

/// Longest unbroken speech the detector buffers before it forces a segment to end.
///
/// The segmenter cuts an utterance at `max_segment_ms`, whose default is 30 s, so this is the
/// same boundary seen from the detector's side. At this length sherpa raises its own
/// threshold and closes the segment, which is how it honours the cap.
const MAX_SPEECH_S: f32 = 30.0;

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
                // After this much unbroken speech sherpa raises its own threshold to 0.9 and
                // ends the segment, which is how it forces a cut. The segmenter's
                // `max_segment_ms` is the cap that matters, and 30 s is its default, so the
                // two agree instead of one cutting where the other does not.
                max_speech_duration: MAX_SPEECH_S,
            },
            sample_rate: RATE as i32,
            num_threads: 1,
            provider: None,
            debug: false,
            ..VadModelConfig::default()
        };
        // The buffer has to hold the open segment, and sherpa logs an overflow and grows it
        // when it does not: one second of audio is 512 samples short of the first second of
        // speech, so a small buffer complains for the whole utterance. A second more than the
        // longest segment covers the push that crosses the cap.
        let detector = VoiceActivityDetector::create(&config, MAX_SPEECH_S + 1.0)
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
        // The segmenter cuts at exactly the model's window, so the common path needs no
        // buffer: a copy into `pending` and a copy back out, per frame, at thirty-one frames
        // a second, for audio that is already the right shape.
        if self.pending.is_empty() && frame.len() == FRAME_SAMPLES {
            self.last = classify(&mut self.detector, frame);
            return self.last;
        }
        self.pending.extend_from_slice(frame);
        while self.pending.len() >= FRAME_SAMPLES {
            self.last = classify(&mut self.detector, &self.pending[..FRAME_SAMPLES]);
            // `drain` keeps the allocation. `split_off` returned the tail as a new `Vec` and
            // assigning it dropped the buffer, so the next frame allocated again: thirty-one
            // allocations a second, for a window that never changes size.
            self.pending.drain(..FRAME_SAMPLES);
        }
        // A frame shorter than the model's window is answered with the state the last whole
        // window produced: the segmenter cuts per frame, and a partial window cannot be
        // classified without inventing the samples it is missing.
        self.last
    }
}

/// Run one window through the model.
///
/// A free function rather than a method, so the window can borrow `pending` while the detector
/// is borrowed mutably: two fields of one struct, which the compiler allows when they are
/// named separately and not when a `&mut self` covers both.
fn classify(detector: &mut VoiceActivityDetector, window: &[f32]) -> bool {
    detector.accept_waveform(window);
    let answer = detector.detected();
    // Sherpa queues the audio of every finished segment, and this gate never reads it: the
    // boundary it was kept for is already decided here. Left in place the queue holds each
    // utterance for the life of the stream, so a long call grows by everything said on it.
    detector.clear();
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The model and a recording, from `ARIOSO_TEST_SILERO`.
    ///
    /// The model is 643 KiB and not in the repository, so the test that uses it is ignored by
    /// default: it `expect`s the fixture instead of returning, and it is run deliberately with
    /// `ARIOSO_TEST_SILERO=... cargo test -p arioso-stt-sherpa -- --ignored`. A tone will not
    /// do as the recording: the model is trained on voices and answers correctly that a sine
    /// is not one.
    fn fixture() -> (PathBuf, PathBuf) {
        let dir = PathBuf::from(
            std::env::var_os("ARIOSO_TEST_SILERO")
                .expect("set ARIOSO_TEST_SILERO to a directory holding silero_vad.onnx and a WAV"),
        );
        let model = dir.join("silero_vad.onnx");
        let wav = std::fs::read_dir(&dir)
            .expect("the fixture directory is readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "wav"))
            .expect("a 16 kHz mono WAV beside the model");
        assert!(model.is_file(), "no model at {}", model.display());
        (model, wav)
    }

    /// Samples from a 16 kHz mono 16-bit WAV, read from its `data` chunk.
    fn read_wav(path: &Path) -> Vec<f32> {
        let bytes = std::fs::read(path).expect("the wav is readable");
        let start = bytes
            .windows(4)
            .position(|window| window == b"data")
            .expect("a data chunk")
            + 8;
        bytes[start..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32_768.0)
            .collect()
    }

    #[test]
    #[ignore = "needs ARIOSO_TEST_SILERO: a real model and a recording"]
    fn a_frame_the_right_size_never_touches_the_buffer() {
        // The segmenter hands the gate one window at a time, so the buffer would be a copy of
        // audio that is already the right shape. It must stay empty and unallocated: a `Vec`
        // that grows to hold it, thirty-one times a second, is the cost this path avoids.
        let (model, wav) = fixture();
        let samples = read_wav(&wav);
        let mut gate = Silero::load(&model, 0.5).expect("the model loads");
        let mut spoken = 0;
        // `as_chunks` rather than `chunks_exact`: the length is a constant, and the newer
        // clippy the gate runs says so.
        for frame in samples.as_chunks::<FRAME_SAMPLES>().0.iter().take(200) {
            if gate.speech(frame) {
                spoken += 1;
            }
        }
        // The struct is built with one window's capacity, and that is the whole point: the
        // fast path must leave it exactly as it was found, not grow it.
        assert!(
            gate.pending.is_empty(),
            "the buffer held audio after exact frames"
        );
        assert_eq!(
            gate.pending.capacity(),
            FRAME_SAMPLES,
            "the buffer grew for frames that needed no buffering"
        );
        assert!(spoken > 0, "the recording has speech in its first frames");
    }

    #[test]
    #[ignore = "needs ARIOSO_TEST_SILERO: a real model and a recording"]
    fn a_buffer_keeps_its_capacity_across_odd_frames() {
        // The buffered path still runs for frames that are not the model's size, and it must
        // not reallocate every time. `split_off` did: it returned the tail as a new `Vec` and
        // the assignment dropped the buffer, so the next frame allocated again.
        let (model, wav) = fixture();
        let samples = read_wav(&wav);
        let mut gate = Silero::load(&model, 0.5).expect("the model loads");
        let mut capacity = 0;
        for frame in samples
            .as_chunks::<{ FRAME_SAMPLES + 7 }>()
            .0
            .iter()
            .take(50)
        {
            let _ = gate.speech(frame);
            capacity = capacity.max(gate.pending.capacity());
        }
        assert!(
            capacity >= FRAME_SAMPLES,
            "the buffer allocated: capacity {capacity}"
        );
        assert!(
            capacity <= FRAME_SAMPLES * 4,
            "the buffer grew without bound: capacity {capacity}"
        );
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
    #[ignore = "needs silero_vad.onnx and a 16 kHz WAV in ARIOSO_TEST_SILERO"]
    fn speech_and_silence_are_told_apart() {
        let (model, wav) = fixture();
        let mut gate = Silero::load(&model, 0.5).expect("loading the model");
        let speech = read_wav(&wav);

        let frames = speech.len() / FRAME_SAMPLES;
        let heard = speech
            .as_chunks::<FRAME_SAMPLES>()
            .0
            .iter()
            .filter(|frame| gate.speech(&frame[..]))
            .count();
        assert!(
            heard > frames / 4,
            "{heard} of {frames} frames of speech were called speech"
        );

        // The detector keeps its own silence hold: `min_silence_duration` is 0.1 s and the
        // model's state decays for a few frames after loud speech, so the answer stays true
        // for about 0.25 s. That is well under the segmenter's `min_silence_ms`, and the
        // property that matters is that the gate lets go; the test says how long it took
        // instead of demanding zero.
        let quiet = vec![0.0_f32; FRAME_SAMPLES];
        let answers: Vec<bool> = (0..80).map(|_| gate.speech(&quiet)).collect();
        let hold = answers.iter().take_while(|answer| **answer).count();
        assert!(
            hold <= 10,
            "speech held for {hold} silent frames, {} ms",
            hold * FRAME_MS as usize
        );
        assert!(
            answers[40..].iter().all(|answer| !answer),
            "silence was still called speech after 1.28 s: {answers:?}"
        );
    }
}
