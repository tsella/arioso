//! Offline speaker diarization.
//!
//! One recording, several voices: the model segments the audio where speech changes, takes a
//! speaker embedding from each segment, and clusters the embeddings. The answer is a list of
//! turns, each with a speaker number that is only meaningful within this recording — speaker 1
//! in one file is not speaker 1 in the next.
//!
//! It reads the whole file before it answers, because the clustering needs every embedding at
//! once. There is no streaming form of this, and a socket that promised one would be lying.

use std::path::{Path, PathBuf};

use sherpa_onnx::{
    FastClusteringConfig, OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
    OfflineSpeakerSegmentationModelConfig, OfflineSpeakerSegmentationPyannoteModelConfig,
    SpeakerEmbeddingExtractorConfig,
};

/// The rate both models read.
pub const RATE: u32 = 16_000;

/// One stretch of audio and the voice in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    /// Start, in seconds from the beginning of the recording.
    pub start: f32,
    /// End, in seconds.
    pub end: f32,
    /// Which voice, numbered from zero within this recording.
    pub speaker: u32,
    /// How sure the clustering is, in `0.0..=1.0`. Zero when confidence is not computed.
    pub confidence: f32,
}

/// What went wrong building a diarizer.
#[derive(Debug, thiserror::Error)]
pub enum DiarizeError {
    /// A model file is not where the manifest said it would be.
    #[error("the diarization model is not at {}", .0.display())]
    Missing(PathBuf),
    /// The runtime refused the models.
    #[error("the diarization models could not be loaded")]
    Load,
    /// The audio is at a rate the models do not read.
    #[error("the models read {RATE} Hz audio, and this is {found} Hz")]
    Rate {
        /// The rate the caller supplied.
        found: u32,
    },
}

/// A loaded pair of models, ready to label a recording.
pub struct Diarizer {
    inner: OfflineSpeakerDiarization,
    rate: u32,
}

impl std::fmt::Debug for Diarizer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Diarizer")
            .field("rate", &self.rate)
            .finish_non_exhaustive()
    }
}

impl Diarizer {
    /// Load the segmentation and embedding models.
    ///
    /// `speakers` fixes the number of voices when the caller knows it; `None` lets the
    /// clustering decide from `threshold`, where a higher number means fewer clusters.
    pub fn load(
        segmentation: &Path,
        embedding: &Path,
        speakers: Option<u32>,
        threshold: f32,
    ) -> Result<Self, DiarizeError> {
        for model in [segmentation, embedding] {
            if !model.is_file() {
                return Err(DiarizeError::Missing(model.to_path_buf()));
            }
        }
        let config = OfflineSpeakerDiarizationConfig {
            segmentation: OfflineSpeakerSegmentationModelConfig {
                pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                    model: Some(segmentation.to_string_lossy().into_owned()),
                    window_shift_ratio: 0.1,
                },
                ..OfflineSpeakerSegmentationModelConfig::default()
            },
            embedding: SpeakerEmbeddingExtractorConfig {
                model: Some(embedding.to_string_lossy().into_owned()),
                ..SpeakerEmbeddingExtractorConfig::default()
            },
            clustering: FastClusteringConfig {
                num_clusters: speakers.map_or(-1, |count| count as i32),
                threshold,
                compute_confidence: true,
            },
            // The defaults the upstream examples use: a turn shorter than this is a noise
            // blip, and a gap shorter than this is the same speaker pausing.
            min_duration_on: 0.3,
            min_duration_off: 0.5,
        };
        let inner = OfflineSpeakerDiarization::create(&config).ok_or(DiarizeError::Load)?;
        let rate = u32::try_from(inner.sample_rate()).unwrap_or(RATE);
        Ok(Self { inner, rate })
    }

    /// The rate the models read, as the segmentation model reports it.
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Label one recording.
    ///
    /// `rate` is the rate of `samples`. A mismatch is refused by name rather than resampled
    /// silently: a wrong rate shifts every timestamp by the ratio, and the answer would look
    /// reasonable while being wrong.
    pub fn run(&self, samples: &[f32], rate: u32) -> Result<Vec<Turn>, DiarizeError> {
        if rate != self.rate {
            return Err(DiarizeError::Rate { found: rate });
        }
        let result = self.inner.process(samples).ok_or(DiarizeError::Load)?;
        let mut turns: Vec<Turn> = result
            .sort_by_start_time()
            .into_iter()
            .map(|segment| Turn {
                start: segment.start,
                end: segment.end,
                speaker: u32::try_from(segment.speaker).unwrap_or(0),
                confidence: segment.confidence,
            })
            .collect();
        renumber(&mut turns);
        Ok(turns)
    }
}

/// Renumber the voices from zero, in the order they first speak.
///
/// The clustering numbers its clusters as it builds them, so a two-voice recording comes back
/// as speakers 0 and 4: the ids are an artefact of the algorithm and depend on how many
/// candidate clusters it opened and merged. A person reading a transcript expects the first
/// voice to be 0, and an id that looks like a miscount reads as a bug.
fn renumber(turns: &mut [Turn]) {
    let mut order: Vec<u32> = Vec::new();
    for turn in turns.iter() {
        if !order.contains(&turn.speaker) {
            order.push(turn.speaker);
        }
    }
    for turn in turns.iter_mut() {
        if let Some(index) = order.iter().position(|speaker| *speaker == turn.speaker) {
            turn.speaker = index as u32;
        }
    }
}

/// The number of distinct voices in `turns`.
#[must_use]
pub fn speakers(turns: &[Turn]) -> usize {
    let mut seen: Vec<u32> = turns.iter().map(|turn| turn.speaker).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The models are 34 MiB and not in the repository, so this runs when they are on disk.
    ///
    /// `ARIOSO_TEST_DIARIZATION` names a directory holding `seg.onnx`, `emb.onnx` and a
    /// recording with two voices in it called `two.wav`.
    fn fixture() -> Option<(PathBuf, PathBuf, PathBuf)> {
        let dir = PathBuf::from(std::env::var_os("ARIOSO_TEST_DIARIZATION")?);
        let (segmentation, embedding, wav) = (
            dir.join("seg.onnx"),
            dir.join("emb.onnx"),
            dir.join("two.wav"),
        );
        (segmentation.is_file() && embedding.is_file() && wav.is_file()).then_some((
            segmentation,
            embedding,
            wav,
        ))
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
        let error = Diarizer::load(
            Path::new("/nonexistent/seg.onnx"),
            Path::new("/nonexistent/emb.onnx"),
            None,
            0.5,
        )
        .expect_err("a model that is not there was loaded");
        assert!(matches!(error, DiarizeError::Missing(_)), "{error}");
    }

    #[test]
    fn the_voices_are_numbered_from_zero_in_the_order_they_speak() {
        // The clustering hands back sparse ids — a two-voice file came back as 0 and 4 —
        // so the numbering is redone here rather than shown to a reader as it arrives.
        let mut turns = vec![
            Turn {
                start: 0.0,
                end: 1.0,
                speaker: 4,
                confidence: 0.0,
            },
            Turn {
                start: 1.0,
                end: 2.0,
                speaker: 0,
                confidence: 0.0,
            },
            Turn {
                start: 2.0,
                end: 3.0,
                speaker: 4,
                confidence: 0.0,
            },
        ];
        renumber(&mut turns);
        assert_eq!(
            turns.iter().map(|turn| turn.speaker).collect::<Vec<_>>(),
            vec![0, 1, 0]
        );
    }

    #[test]
    fn a_rate_the_models_do_not_read_is_refused() {
        let Some((segmentation, embedding, _)) = fixture() else {
            return;
        };
        let diarizer = Diarizer::load(&segmentation, &embedding, None, 0.5).expect("loading");
        let error = diarizer
            .run(&[0.0; 16_000], 8_000)
            .expect_err("eight kilohertz was accepted");
        assert!(
            matches!(error, DiarizeError::Rate { found: 8_000 }),
            "{error}"
        );
    }

    #[test]
    fn a_recording_of_two_voices_is_labelled_with_two_speakers() {
        let Some((segmentation, embedding, wav)) = fixture() else {
            return;
        };
        let diarizer = Diarizer::load(&segmentation, &embedding, None, 0.5).expect("loading");
        let samples = read_wav(&wav);
        let turns = diarizer.run(&samples, RATE).expect("diarizing");

        assert!(!turns.is_empty(), "no turns were found");
        assert_eq!(speakers(&turns), 2, "turns: {turns:?}");
        // The turns are sorted, in range, and do not run past the recording.
        let duration = samples.len() as f32 / RATE as f32;
        let mut previous = 0.0_f32;
        for turn in &turns {
            assert!(
                turn.start >= previous - 0.001,
                "{turn:?} starts before the last one ended"
            );
            assert!(turn.end > turn.start, "{turn:?} has no length");
            assert!(
                turn.end <= duration + 0.001,
                "{turn:?} runs past the recording"
            );
            previous = turn.end;
        }
    }
}
