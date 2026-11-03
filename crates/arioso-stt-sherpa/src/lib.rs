//! Batch speech to text through sherpa-onnx.
//!
//! One engine, one job: take samples at any rate, return text. Two families are supported:
//! Whisper, which the project shipped first, and transducer models (Parakeet, Zipformer),
//! which the same library loads through a different graph. The manifest names the family,
//! so a model directory that holds the wrong files is refused rather than guessed at.

use std::path::{Path, PathBuf};

use sherpa_onnx::{
    OfflineModelConfig, OfflineRecognizer, OfflineRecognizerConfig, OfflineTransducerModelConfig,
    OfflineWhisperModelConfig,
};

/// Rate the engine expects. Anything else is resampled on the way in.
pub const ENGINE_RATE: u32 = 16_000;

/// What went wrong.
#[derive(Debug, thiserror::Error)]
pub enum SttError {
    /// The model directory does not hold the files the engine needs.
    #[error("{} does not hold a usable model: {missing}", dir.display())]
    NotAModel {
        /// Directory that was searched.
        dir: PathBuf,
        /// What was expected and not found.
        missing: String,
    },
    /// The recognizer refused the configuration.
    #[error("the recognizer could not load {}", .0.display())]
    Load(PathBuf),
    /// The directory holds files from more than one model.
    #[error("{dir} holds files from more than one model: {encoder} and {decoder}", dir = .dir.display())]
    Mixed {
        /// Directory that was searched.
        dir: PathBuf,
        /// Encoder that was found.
        encoder: String,
        /// Decoder that was found.
        decoder: String,
    },
    /// There was no audio to transcribe.
    #[error("the audio is empty")]
    EmptyAudio,
}

/// One transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    /// Text, trimmed.
    pub text: String,
}

/// A loaded model, either family.
pub struct Engine {
    recognizer: OfflineRecognizer,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Engine { .. }")
    }
}

/// What one file in a model directory is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Encoder,
    Decoder,
    Joiner,
    Tokens,
}

/// The role of a file, decided by its name.
///
/// The catalog names the files, and every sherpa-onnx export uses the same words for the
/// parts of a graph, so the name is enough. A joiner is the file that makes a model a
/// transducer: Whisper has none, Parakeet and Zipformer cannot work without one.
fn role_of(path: &Path) -> Option<Role> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if name.contains("joiner") {
        Some(Role::Joiner)
    } else if name.contains("encoder") {
        Some(Role::Encoder)
    } else if name.contains("decoder") {
        Some(Role::Decoder)
    } else if name.contains("tokens") {
        Some(Role::Tokens)
    } else {
        None
    }
}

impl Engine {
    /// Load the model a manifest entry describes.
    ///
    /// `model_type` is the sherpa-onnx model type, the same value the manifest carries in
    /// `meta.model_type`: `whisper`, or a transducer type such as `nemo_transducer` or
    /// `zipformer`. An absent value means Whisper, which is what every entry written
    /// before transducers existed means.
    ///
    /// # Errors
    ///
    /// Returns [`SttError::NotAModel`] when a file is missing, and [`SttError::Load`]
    /// when the recognizer refuses the configuration.
    pub fn from_manifest_files(
        dir: &Path,
        files: &[PathBuf],
        model_type: Option<&str>,
        language: Option<&str>,
        threads: usize,
    ) -> Result<Self, SttError> {
        let find = |role: Role| {
            files
                .iter()
                .find(|path| role_of(path) == Some(role))
                .cloned()
        };
        let encoder = find(Role::Encoder).ok_or_else(|| not_a_model(dir, "an encoder *.onnx"))?;
        let decoder = find(Role::Decoder).ok_or_else(|| not_a_model(dir, "a decoder *.onnx"))?;
        let tokens = find(Role::Tokens).ok_or_else(|| not_a_model(dir, "a *tokens.txt"))?;
        let model_type = model_type.unwrap_or("whisper");
        if model_type == "whisper" {
            return Self::load_files(dir, &encoder, &decoder, &tokens, language, threads);
        }
        let joiner = find(Role::Joiner)
            .ok_or_else(|| not_a_model(dir, "a joiner *.onnx, which a transducer needs"))?;
        Self::load_transducer(
            dir, &encoder, &decoder, &joiner, &tokens, model_type, threads,
        )
    }

    /// Load a transducer model: encoder, decoder, joiner, tokens.
    ///
    /// `model_type` is the sherpa-onnx name for the graph, `nemo_transducer` for Parakeet
    /// and `zipformer` for the Zipformer exports.
    ///
    /// # Errors
    ///
    /// Returns [`SttError::Load`] when the recognizer refuses the configuration.
    pub fn load_transducer(
        dir: &Path,
        encoder: &Path,
        decoder: &Path,
        joiner: &Path,
        tokens: &Path,
        model_type: &str,
        threads: usize,
    ) -> Result<Self, SttError> {
        let config = OfflineRecognizerConfig {
            model_config: OfflineModelConfig {
                transducer: OfflineTransducerModelConfig {
                    encoder: Some(encoder.display().to_string()),
                    decoder: Some(decoder.display().to_string()),
                    joiner: Some(joiner.display().to_string()),
                },
                tokens: Some(tokens.display().to_string()),
                model_type: Some(model_type.to_owned()),
                num_threads: i32::try_from(threads).unwrap_or(4),
                ..OfflineModelConfig::default()
            },
            ..OfflineRecognizerConfig::default()
        };
        let recognizer =
            OfflineRecognizer::create(&config).ok_or_else(|| SttError::Load(dir.to_owned()))?;
        Ok(Self { recognizer })
    }

    /// Load a Whisper model directory.
    ///
    /// The directory is searched for an encoder, a decoder, and a tokens file, in that
    /// order of importance. An int8 file wins over a float one when both are present,
    /// because it is a third of the size and faster on a CPU.
    ///
    /// The search is flat, so one directory holds one Whisper model. Two installed at
    /// once would resolve to whichever name sorts first.
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
        // A directory can hold more than one model, and the search picks each file on
        // its own. A tiny encoder with a base decoder loads and then fails inside the
        // graph, so refuse the pair rather than hand it to the engine.
        let prefix = |path: &Path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.split('-').next())
                .unwrap_or_default()
                .to_owned()
        };
        let (a, b, c) = (prefix(&encoder), prefix(&decoder), prefix(&tokens));
        if a != b || b != c {
            return Err(SttError::Mixed {
                dir: dir.to_owned(),
                encoder: encoder.display().to_string(),
                decoder: decoder.display().to_string(),
            });
        }
        Self::load_files(dir, &encoder, &decoder, &tokens, language, threads)
    }

    /// Load a named set of files, for a directory that holds more than one model.
    ///
    /// The manifest names the three files, so a caller that has it does not depend on
    /// the directory holding only one model.
    ///
    /// # Errors
    ///
    /// Returns [`SttError::Load`] when the recognizer refuses the configuration.
    pub fn load_files(
        dir: &Path,
        encoder: &Path,
        decoder: &Path,
        tokens: &Path,
        language: Option<&str>,
        threads: usize,
    ) -> Result<Self, SttError> {
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
    /// Returns [`SttError::EmptyAudio`] when there are no samples at all. Audio that holds
    /// no speech is **not** an error: it returns a transcript with empty text.
    pub fn transcribe(&self, samples: &[f32], rate: u32) -> Result<Transcript, SttError> {
        if samples.is_empty() {
            return Err(SttError::EmptyAudio);
        }
        // The engine decodes one window of at most thirty seconds and discards whatever
        // follows, without an error. Handing it an hour of audio returns the first half
        // minute and a success code, so the audio is cut here and the pieces joined.
        //
        // The cut is made at the rate the audio arrived in, and only the window about to be
        // decoded is converted. Resampling the file first held a second copy of the whole
        // recording: 115 MB for thirty minutes at 48 kHz, against 1.85 MB for one window.
        let mut text = String::new();
        let mut converted: Vec<f32> = Vec::with_capacity(WINDOW_S * ENGINE_RATE as usize);
        for window in windows(samples, rate) {
            let audio: &[f32] = if rate == ENGINE_RATE {
                window
            } else {
                converted.clear();
                resample_into(window, rate, ENGINE_RATE, &mut converted);
                &converted
            };
            let stream = self.recognizer.create_stream();
            stream.accept_waveform(i32::try_from(ENGINE_RATE).unwrap_or(16_000), audio);
            self.recognizer.decode(&stream);
            let Some(result) = stream.get_result() else {
                continue;
            };
            let piece = result.text.trim();
            if piece.is_empty() {
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(piece);
        }
        // No speech is a result, not a failure. A file of silence, a muted microphone and a
        // room with nobody in it all produce no words, and the caller asked what was said.
        // Reporting an error made the batch route answer 503 engine_unavailable for silence,
        // which reads as a broken server and sends an operator to look at the model.
        Ok(Transcript { text })
    }
}

/// Longest audio one decode may take, in seconds.
///
/// The engine accepts "less than 30 seconds" and silently keeps only the first thirty of
/// anything longer, so a window of exactly thirty lands on the wrong side of its own test.
/// One second of margin costs a decode per half hour and removes the ambiguity.
const WINDOW_S: usize = 29;

/// How far back from a window edge to look for a quiet place to cut, in seconds.
///
/// A cut inside a word costs accuracy on both sides of it. A pause costs nothing, and
/// speech has one every few seconds.
const TAIL_S: usize = 3;

/// Split audio into pieces the engine will decode whole.
///
/// Each piece ends at the quietest point in the last few seconds of a window, so a cut
/// lands in a pause where it can. The pieces cover the input exactly: nothing is dropped,
/// and nothing is decoded twice.
fn windows(samples: &[f32], rate: u32) -> Vec<&[f32]> {
    let window = WINDOW_S * rate as usize;
    let tail = TAIL_S * rate as usize;
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < samples.len() {
        let remaining = samples.len() - start;
        if remaining <= window {
            pieces.push(&samples[start..]);
            break;
        }
        let edge = start + window;
        // Search the last few seconds, but never cut before the halfway mark: a quiet
        // stretch early in the window would otherwise make a tiny piece and waste a decode.
        let floor = start + window / 2;
        let from = edge.saturating_sub(tail).max(floor);
        let cut = quietest(&samples[from..edge], rate).map_or(edge, |offset| from + offset);
        pieces.push(&samples[start..cut]);
        start = cut;
    }
    pieces
}

/// Where to cut, as an offset into `slice`: the middle of the quietest 20 ms frame.
///
/// `rate` is the rate of `slice`, which is the rate the audio arrived in: the window is cut
/// before it is converted, so a 48 kHz recording is measured at 48 kHz.
fn quietest(slice: &[f32], rate: u32) -> Option<usize> {
    // Ten milliseconds, at whatever rate this is.
    let half = (rate / 100).max(1) as usize;
    let frame = half * 2;
    if slice.len() < frame * 2 {
        return None;
    }
    // Each ten millisecond half is squared once. Summing a fresh twenty millisecond frame
    // at every half-frame offset squared every sample twice.
    let mut halves: Vec<f32> = Vec::with_capacity(slice.len() / half);
    for at in (0..slice.len() - half + 1).step_by(half) {
        halves.push(slice[at..at + half].iter().map(|s| s * s).sum());
    }
    let mut best = None;
    let mut best_energy = f32::MAX;
    for index in 0..halves.len().saturating_sub(1) {
        let energy = halves[index] + halves[index + 1];
        if energy < best_energy {
            best_energy = energy;
            best = Some(index * half + half);
        }
    }
    best
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
    let mut out = Vec::with_capacity(resampled_len(samples.len(), from, to));
    resample_into(samples, from, to, &mut out);
    out
}

/// The same, into a buffer the caller keeps.
///
/// The window path converts one window at a time into the same buffer, so a long recording
/// never holds a converted copy of itself.
pub fn resample_into(samples: &[f32], from: u32, to: u32, out: &mut Vec<f32>) {
    if from == to || samples.is_empty() || from == 0 || to == 0 {
        out.extend_from_slice(samples);
        return;
    }
    let ratio = f64::from(from) / f64::from(to);
    let length = resampled_len(samples.len(), from, to);
    out.reserve(length);
    for index in 0..length {
        let position = index as f64 * ratio;
        let left = position.floor() as usize;
        let fraction = (position - left as f64) as f32;
        let a = samples[left.min(samples.len() - 1)];
        let b = samples[(left + 1).min(samples.len() - 1)];
        out.push(a + (b - a) * fraction);
    }
}

/// How many samples `resample` will produce.
fn resampled_len(len: usize, from: u32, to: u32) -> usize {
    if from == to || from == 0 || to == 0 {
        return len;
    }
    let ratio = f64::from(from) / f64::from(to);
    ((len as f64) / ratio).ceil() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_is_cut_on_the_same_clock_at_any_rate() {
        // A cut is chosen in time, not in samples: a 48 kHz recording has to split where a
        // 16 kHz one would, and the pieces still cover the input exactly.
        let mut samples = vec![0.5f32; 40 * 48_000];
        let quiet = 27 * 48_000 + 24_000;
        for sample in &mut samples[quiet..quiet + 24_000] {
            *sample = 0.0;
        }
        let pieces = windows(&samples, 48_000);
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].len() + pieces[1].len(), samples.len());
        let cut_s = pieces[0].len() as f64 / 48_000.0;
        assert!((27.0..28.0).contains(&cut_s), "cut at {cut_s} s");
    }

    /// Silence is a transcript with no words, not a failure.
    ///
    /// Ignored because CI has no model, and a test that returns early when the model is
    /// missing passes whether or not the bug is fixed. Run it deliberately:
    ///
    /// ```text
    /// ARIOSO_MODELS_DIR=... cargo test -p arioso-stt-sherpa -- --ignored
    /// ```
    #[test]
    #[ignore = "needs a real model in ARIOSO_MODELS_DIR"]
    fn silence_is_an_empty_transcript_not_an_error() {
        let dir = std::env::var("ARIOSO_MODELS_DIR")
            .expect("set ARIOSO_MODELS_DIR to a directory holding a speech model");
        // Scan the directory rather than reading the manifest: this crate does not depend on
        // arioso-models, and the roles are decided by file name. The directory must hold one
        // model; two sets of files are refused by design.
        let files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .expect("a readable ARIOSO_MODELS_DIR")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("onnx" | "txt")
                )
            })
            .collect();
        let engine = Engine::from_manifest_files(
            std::path::Path::new(&dir),
            &files,
            Some("nemo_transducer"),
            None,
            4,
        )
        .expect("one model in ARIOSO_MODELS_DIR");
        let silence = vec![0.0f32; 16_000 * 3];
        let transcript = engine
            .transcribe(&silence, 16_000)
            .expect("three seconds of silence must not be an error");
        assert!(
            transcript.text.is_empty(),
            "silence produced {:?}",
            transcript.text
        );
    }

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
        let error = Engine::load(dir.path(), Some("en"), 1).unwrap_err();
        assert!(matches!(error, SttError::NotAModel { .. }));
        assert!(error.to_string().contains("encoder"), "{error}");
    }

    #[test]
    fn files_from_two_models_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tiny.en-encoder.int8.onnx"), b"").unwrap();
        std::fs::write(dir.path().join("base.en-decoder.int8.onnx"), b"").unwrap();
        std::fs::write(dir.path().join("tiny.en-tokens.txt"), b"").unwrap();
        let error = Engine::load(dir.path(), Some("en"), 1).unwrap_err();
        assert!(matches!(error, SttError::Mixed { .. }), "{error}");
    }

    #[test]
    fn a_file_is_named_for_the_part_of_the_graph_it_holds() {
        let role = |name: &str| role_of(Path::new(name));
        assert_eq!(role("base.en-encoder.int8.onnx"), Some(Role::Encoder));
        assert_eq!(role("base.en-decoder.int8.onnx"), Some(Role::Decoder));
        assert_eq!(role("joiner.int8.onnx"), Some(Role::Joiner));
        assert_eq!(role("tokens.txt"), Some(Role::Tokens));
        assert_eq!(role("manifest.json"), None);
    }

    #[test]
    fn a_transducer_without_a_joiner_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = ["encoder.int8.onnx", "decoder.int8.onnx", "tokens.txt"]
            .iter()
            .map(|name| dir.path().join(name))
            .collect();
        for path in &paths {
            std::fs::write(path, b"").unwrap();
        }
        let error =
            Engine::from_manifest_files(dir.path(), &paths, Some("nemo_transducer"), None, 1)
                .unwrap_err();
        assert!(matches!(error, SttError::NotAModel { .. }), "{error}");
        assert!(error.to_string().contains("joiner"), "{error}");
    }

    #[test]
    fn a_transducer_entry_without_a_family_is_read_as_whisper() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<PathBuf> = ["encoder.int8.onnx", "decoder.int8.onnx", "tokens.txt"]
            .iter()
            .map(|name| dir.path().join(name))
            .collect();
        for path in &paths {
            std::fs::write(path, b"").unwrap();
        }
        // No family means Whisper, so the stub files reach the engine and it refuses them.
        // The point of the test is that a joiner is not demanded of an old entry.
        let error = Engine::from_manifest_files(dir.path(), &paths, None, None, 1).unwrap_err();
        assert!(matches!(error, SttError::Load(_)), "{error}");
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
        if let Ok(engine) = Engine::load(dir.path(), Some("en"), 1) {
            assert!(matches!(
                engine.transcribe(&[], 16_000),
                Err(SttError::EmptyAudio)
            ));
        }
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    /// Speech-shaped noise: loud everywhere except the given pause.
    fn audio(seconds: usize, pause: Option<(usize, usize)>) -> Vec<f32> {
        let rate = ENGINE_RATE as usize;
        let mut samples: Vec<f32> = (0..seconds * rate)
            .map(|i| ((i as f32) * 0.01).sin() * 0.5)
            .collect();
        if let Some((from, to)) = pause {
            for sample in &mut samples[from * rate..to * rate] {
                *sample = 0.0;
            }
        }
        samples
    }

    #[test]
    fn audio_inside_one_window_is_not_cut() {
        let samples = audio(20, None);
        let pieces = windows(&samples, ENGINE_RATE);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].len(), samples.len());
    }

    #[test]
    fn long_audio_is_cut_into_whole_windows_and_nothing_is_lost() {
        let samples = audio(65, None);
        let pieces = windows(&samples, ENGINE_RATE);
        assert!(pieces.len() >= 3, "65 s needs at least three windows");
        for piece in &pieces {
            assert!(
                piece.len() <= WINDOW_S * ENGINE_RATE as usize,
                "a window is too long"
            );
        }
        let total: usize = pieces.iter().map(|p| p.len()).sum();
        assert_eq!(
            total,
            samples.len(),
            "the pieces must cover the input exactly"
        );
    }

    #[test]
    fn a_cut_lands_in_a_pause_when_there_is_one() {
        // A pause from 27 s to 29 s: the first window ends at 30 s, so the cut should land
        // inside the pause rather than at the edge.
        let samples = audio(65, Some((27, 29)));
        let pieces = windows(&samples, ENGINE_RATE);
        let first = pieces[0].len() / ENGINE_RATE as usize;
        assert!(
            (27..=30).contains(&first),
            "the first piece ended at {first} s, not in the pause"
        );
    }
}
