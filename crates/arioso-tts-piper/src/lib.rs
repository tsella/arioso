//! Piper voice inference.
//!
//! A Piper voice is a VITS model in ONNX plus a JSON config holding its phoneme
//! table and synthesis scales.

use std::path::Path;

use arioso_core::{Entry, Piece, UserDictionary};
use arioso_core::{chunk_text, clauses};
use arioso_g2p::{CmuDict, PhonemeMap};
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
    /// A speaker was asked for on a voice that has only one.
    #[error("this voice has a single speaker, so speaker {0} does not exist")]
    NoSpeakers(i64),
    /// A phoneme the voice's table has no id for.
    ///
    /// Dropped silently, the caller hears a different word and has no way to know.
    #[error("this voice cannot say {0:?}: it has no id for that symbol")]
    UnknownSymbol(String),
    /// A speaker number outside the voice's range.
    #[error("speaker {sid} is out of range: this voice has {count}")]
    SpeakerRange {
        /// The speaker asked for.
        sid: i64,
        /// How many the voice has.
        count: usize,
    },
}

/// Longest piece of text handed to the model, in characters.
pub const SYNTHESIS_CHUNK: usize = 300;

/// Threads used when the caller does not say.
///
/// Every core, which is what this crate did before it took the number: one voice speaks one
/// request at a time, so the graph is the only work in flight.
fn default_threads() -> u32 {
    std::thread::available_parallelism().map_or(1, |count| u32::try_from(count.get()).unwrap_or(1))
}

/// How a render is done.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Speaking rate, as a multiplier of the voice's own length scale.
    pub speed: f32,
    /// How text is expanded before it is phonemized.
    pub normalize: arioso_core::NormalizeOptions,
    /// Which speaker to use on a multi-speaker voice. Ignored, and must be 0, on a
    /// single-speaker one.
    pub sid: i64,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            speed: 1.0,
            normalize: arioso_core::NormalizeOptions::default(),
            sid: 0,
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

/// Check a speaker number against how many a voice has.
///
/// # Errors
///
/// Returns [`PiperError::NoSpeakers`] for a number on a single-speaker voice, and
/// [`PiperError::SpeakerRange`] for one outside the range.
pub fn check_speaker(sid: i64, num_speakers: usize) -> Result<(), PiperError> {
    if sid < 0 {
        return Err(PiperError::SpeakerRange {
            sid,
            count: num_speakers,
        });
    }
    if num_speakers <= 1 {
        // A voice with one speaker has nothing to choose, so any other number is a
        // request the voice cannot answer rather than a number out of range.
        return if sid == 0 {
            Ok(())
        } else {
            Err(PiperError::NoSpeakers(sid))
        };
    }
    match usize::try_from(sid) {
        Ok(number) if number < num_speakers => Ok(()),
        _ => Err(PiperError::SpeakerRange {
            sid,
            count: num_speakers,
        }),
    }
}

/// A loaded Piper voice.
pub struct PiperVoice {
    session: Session,
    map: PhonemeMap,
    sample_rate: u32,
    scales: [f32; 3],
    /// The voice's own length scale, before a speed multiplier.
    length_scale: f32,
    /// How many speakers the voice was trained on.
    num_speakers: usize,
    /// Whether the graph takes a speaker id. A multi-speaker voice without it produces
    /// the same voice for every number, so the two facts are kept apart.
    has_sid: bool,
    /// The speaker [`TtsEngine::render`] uses. The trait has no speaker parameter,
    /// because a voice style is the whole of Kokoro's choice; a caller that wants
    /// another speaker on this voice sets it here.
    speaker: i64,
}

impl std::fmt::Debug for PiperVoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PiperVoice")
            .field("sample_rate", &self.sample_rate)
            .field("scales", &self.scales)
            .field("symbols", &self.map.len())
            .field("num_speakers", &self.num_speakers)
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
        Self::load_with_threads(model, config, default_threads())
    }

    /// Load with an explicit intra-op thread count.
    ///
    /// The count is a property of the machine and of what else is running in the process, so
    /// the caller decides it rather than the crate reading the core count behind its back.
    ///
    /// # Errors
    ///
    /// As [`PiperVoice::load`].
    pub fn load_with_threads(
        model: &Path,
        config: &Path,
        threads: u32,
    ) -> Result<Self, PiperError> {
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
        // Level 3 fuses the graph; the default is Level 1, which leaves the fused kernels
        // unused. The thread count is set rather than left to the runtime, and it is every
        // core: one voice synthesizes one request at a time, so the graph is the only work
        // in flight. Halving it cost a fifth to a quarter of the synthesis time on a four
        // core machine.
        let threads = usize::try_from(threads).unwrap_or(1).max(1);
        let session = Session::builder()?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)?
            .with_intra_threads(threads)?
            .commit_from_file(model)?;

        // A multi-speaker voice takes a `sid` input and produces one voice per number.
        // The count comes from the config and the input from the graph, because a voice
        // can be trained on several speakers and exported without the input, and that
        // one would otherwise answer every number with the same voice.
        let num_speakers = document
            .pointer("/num_speakers")
            .and_then(serde_json::Value::as_u64)
            .and_then(|count| usize::try_from(count).ok())
            .filter(|count| *count > 0)
            .unwrap_or(1);
        let has_sid = session.inputs.iter().any(|input| input.name == "sid");

        Ok(Self {
            session,
            map,
            sample_rate,
            scales,
            length_scale,
            num_speakers,
            has_sid,
            speaker: 0,
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

    /// How many speakers the voice was trained on.
    #[must_use]
    pub fn num_speakers(&self) -> usize {
        self.num_speakers
    }

    /// Whether the graph takes a speaker id at all.
    #[must_use]
    pub fn has_speaker_input(&self) -> bool {
        self.has_sid
    }

    /// Check a speaker number against this voice.
    ///
    /// # Errors
    ///
    /// Returns [`PiperError::NoSpeakers`] when a speaker was asked for on a voice that
    /// has one, and [`PiperError::SpeakerRange`] when the number is out of range. Both
    /// are errors rather than silent fallbacks: a caller that asked for a voice and got
    /// a different one has no way to notice.
    pub fn check_speaker(&self, sid: i64) -> Result<(), PiperError> {
        check_speaker(sid, self.num_speakers)
    }

    /// Speak `text`, handing each piece of audio to `sink` as it is ready.
    ///
    /// The text is split into clauses, then capped by [`SYNTHESIS_CHUNK`].
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
        self.check_speaker(options.sid)?;
        let rate = self.sample_rate();
        let mut report = RenderReport::default();

        // Clauses, not sentences: the first audio leaves after the first comma rather than
        // after the first full stop, which on a long sentence is the whole latency. Each
        // clause is still capped by SYNTHESIS_CHUNK, so nothing becomes unbounded.
        for clause in clauses(text) {
            for chunk in chunk_text(clause, SYNTHESIS_CHUNK) {
                let symbols = self.chunk_symbols(chunk, dictionary, user, options, &mut report);
                if symbols.is_empty() {
                    continue;
                }
                let audio = self.synthesize(&symbols, options.sid)?;
                sink(&audio, rate)?;
            }
        }
        Ok(report)
    }

    /// The symbols a text will be spoken as, without synthesizing any of it.
    ///
    /// The same loop [`PiperVoice::render`] runs, stopping before the model: a display that
    /// built its own symbols could show a line the voice would never say.
    #[must_use]
    pub fn phonemes_for(
        &self,
        text: &str,
        dictionary: &CmuDict,
        user: &UserDictionary,
        options: RenderOptions,
    ) -> (Vec<&'static str>, RenderReport) {
        let mut report = RenderReport::default();
        let mut out: Vec<&'static str> = Vec::new();
        // Clauses, not sentences: the first audio leaves after the first comma rather than
        // after the first full stop, which on a long sentence is the whole latency. Each
        // clause is still capped by SYNTHESIS_CHUNK, so nothing becomes unbounded.
        for clause in clauses(text) {
            for chunk in chunk_text(clause, SYNTHESIS_CHUNK) {
                let symbols = self.chunk_symbols(chunk, dictionary, user, options, &mut report);
                if symbols.is_empty() {
                    continue;
                }
                if !out.is_empty() {
                    out.push(" ");
                }
                out.extend(symbols);
            }
        }
        (out, report)
    }

    /// Symbols for one chunk of text, with the user dictionary applied.
    fn chunk_symbols(
        &self,
        chunk: &str,
        dictionary: &CmuDict,
        user: &UserDictionary,
        options: RenderOptions,
        report: &mut RenderReport,
    ) -> Vec<&'static str> {
        let mut groups: Vec<Vec<&'static str>> = Vec::new();
        for piece in user.pieces(chunk) {
            match piece {
                Piece::Entry(entry) => {
                    match self.entry_symbols(entry, dictionary, options.normalize) {
                        Some(symbols) => groups.push(symbols),
                        None => {
                            // The voice cannot say any form of this entry, so say the term
                            // itself and report it.
                            report.unusable.push(entry.term.clone());
                            groups.push(self.words(
                                &entry.term,
                                dictionary,
                                report,
                                options.normalize,
                            ));
                        }
                    }
                }
                Piece::Text(segment) => {
                    groups.push(self.words(segment, dictionary, report, options.normalize));
                }
            }
        }

        let mut symbols: Vec<&'static str> = Vec::new();
        for group in groups.into_iter().filter(|group| !group.is_empty()) {
            if !symbols.is_empty() {
                symbols.push(" ");
            }
            symbols.extend(group);
        }
        symbols
    }

    /// Synthesize phonemes a caller supplied, one chunk at a time.
    ///
    /// [`PiperVoice::render`] builds these symbols from text; this takes them as they are,
    /// so a phoneme a person edited is the phoneme that is spoken.
    ///
    /// # Errors
    ///
    /// A symbol this voice has no id for is refused by name rather than dropped, and the
    /// engine's own failure comes back unchanged.
    pub fn render_symbols(
        &mut self,
        symbols: &[&str],
        sid: i64,
        mut sink: impl FnMut(&[f32], u32) -> Result<(), PiperError>,
    ) -> Result<(), PiperError> {
        self.check_speaker(sid)?;
        if let Some(missing) = symbols.iter().find(|symbol| !self.map.contains(symbol)) {
            return Err(PiperError::UnknownSymbol((*missing).to_owned()));
        }
        let rate = self.sample_rate();
        for chunk in symbols.chunks(SYNTHESIS_CHUNK) {
            let audio = self.synthesize(chunk, sid)?;
            sink(&audio, rate)?;
        }
        Ok(())
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
    /// `sid` selects the speaker on a multi-speaker voice. It is sent only when the graph
    /// declares the input, because a model that does not have it rejects the run.
    ///
    /// # Errors
    ///
    /// Returns [`PiperError`] when ONNX Runtime fails or returns no tensor.
    pub fn synthesize(&mut self, symbols: &[&str], sid: i64) -> Result<Vec<f32>, PiperError> {
        let input = self.map.to_ids_i64(symbols);
        self.synthesize_ids(&input, sid)
    }

    /// Synthesize a run of phoneme ids, the form the graph takes.
    ///
    /// [`arioso_core::TtsEngine::render`] lands here; [`PiperVoice::synthesize`] converts symbols and
    /// then calls it, so there is one place that builds the feeds.
    ///
    /// # Errors
    ///
    /// Returns [`PiperError`] when ONNX Runtime fails or returns no tensor.
    pub fn synthesize_ids(&mut self, input: &[i64], sid: i64) -> Result<Vec<f32>, PiperError> {
        let length = input.len() as i64;

        let mut feeds: Vec<(&str, ort::session::SessionInputValue<'_>)> = vec![
            (
                "input",
                Tensor::from_array(([1i64, length], input.to_vec()))?.into(),
            ),
            (
                "input_lengths",
                Tensor::from_array(([1i64], vec![length]))?.into(),
            ),
            (
                "scales",
                Tensor::from_array(([3i64], self.scales.to_vec()))?.into(),
            ),
        ];
        if self.has_sid {
            feeds.push(("sid", Tensor::from_array(([1i64], vec![sid]))?.into()));
        }

        let outputs = self.session.run(feeds)?;

        let output = outputs.get("output").ok_or(PiperError::NoOutput)?;
        let (_shape, samples) = output.try_extract_tensor::<f32>()?;
        Ok(samples.to_vec())
    }

    /// Choose the speaker [`arioso_core::TtsEngine::render`] will use.
    ///
    /// Not validated here: [`PiperVoice::check_speaker`] is the check, and the trait path
    /// is for callers that already hold a voice they chose.
    pub fn set_speaker(&mut self, sid: i64) {
        self.speaker = sid;
    }
}

impl arioso_core::TtsEngine for PiperVoice {
    fn sample_rate(&self) -> u32 {
        self.sample_rate()
    }

    /// Render a run of phoneme ids at `speed`, using the speaker last set.
    ///
    /// The error is flattened to a string because the trait is shared and Piper's
    /// variants mean nothing to another engine.
    fn render(
        &mut self,
        symbols: &[i64],
        speed: f32,
    ) -> Result<Vec<f32>, arioso_core::EngineError> {
        self.set_speed(speed);
        let sid = self.speaker;
        self.synthesize_ids(symbols, sid)
            .map_err(|error| arioso_core::EngineError::Backend(error.to_string()))
    }
}

/// Symbols for a run of text, after normalization.
///
/// The work is [`arioso_g2p::phonemes_for_text`]; this wrapper only files the guesses
/// and the misses into the report the callers already print.
fn text_symbols(
    text: &str,
    dictionary: &CmuDict,
    report: &mut RenderReport,
    options: arioso_core::NormalizeOptions,
    allow_rules: bool,
) -> Vec<&'static str> {
    let phonemes = arioso_g2p::phonemes_for_text(text, dictionary, options, allow_rules);
    report.guessed.extend(phonemes.guessed);
    report.missing.extend(phonemes.missing);
    phonemes.symbols
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
    let symbols = arioso_g2p::phonemes_for_form(form, dictionary, options, allow_rules);
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
    fn a_speaker_number_is_checked_against_the_voice() {
        // A single-speaker voice answers 0 and refuses anything else, because a caller
        // that asked for a different voice has no way to notice it got the same one.
        assert!(check_speaker(0, 1).is_ok());
        assert!(matches!(
            check_speaker(1, 1),
            Err(PiperError::NoSpeakers(1))
        ));
        // A multi-speaker voice has a range, and the first number outside it is refused.
        for sid in 0..4 {
            assert!(check_speaker(sid, 4).is_ok(), "sid {sid}");
        }
        assert!(matches!(
            check_speaker(4, 4),
            Err(PiperError::SpeakerRange { sid: 4, count: 4 })
        ));
        assert!(matches!(
            check_speaker(-1, 4),
            Err(PiperError::SpeakerRange { sid: -1, count: 4 })
        ));
    }

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
    fn a_plural_of_a_dictionary_word_is_not_a_guess() {
        // The dictionary holds `avenger` and not `avengers`. Reading the plural through the
        // letter rules produced a name nobody has heard; the suffix belongs on the entry.
        let dictionary = CmuDict::parse("avenger AH0 V EH1 N JH ER0\n");
        let mut singular = RenderReport::default();
        let mut plural = RenderReport::default();
        let options = arioso_core::NormalizeOptions::default();
        let one = text_symbols("avenger", &dictionary, &mut singular, options, true);
        let many = text_symbols("avengers", &dictionary, &mut plural, options, true);

        assert!(plural.guessed.is_empty(), "{plural:?}");
        assert!(plural.missing.is_empty(), "{plural:?}");
        assert_eq!(&many[..one.len()], &one[..], "the base sounds the same");
        assert_eq!(&many[one.len()..], &["z"], "and takes the voiced plural");
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
