//! Kokoro voice inference.
//!
//! Kokoro-82M is one ONNX graph and a separate style archive. Where a Piper voice is a
//! model per speaker, this engine is one model with a table of voice styles, and a
//! sentence is phonemes plus the row of that table matching its length.
//!
//! Phonemization is [`arioso_g2p`], the same permissive path Piper uses: CMUdict, then a
//! regular suffix, then letter-to-sound rules. Kokoro was trained on espeak-ng's IPA, so
//! this is the same family of symbols without the GPL dependency; the `gpl-espeak`
//! feature stays the only way to link espeak, and this crate does not need it.

use std::path::Path;

use arioso_core::{Entry, Piece, TtsEngine, UserDictionary};
use arioso_core::{chunk_text, clauses};
use arioso_g2p::CmuDict;
use ort::session::Session;
use ort::value::Tensor;
use thiserror::Error;

pub mod vocab;
pub mod voices;

pub use vocab::Vocab;
pub use voices::{Style, VoiceFile};

/// Sample rate of the audio this model produces.
pub const SAMPLE_RATE: u32 = 24_000;

/// Longest run of tokens one graph call takes.
///
/// The style table has one row per length up to this one, and the graph was exported
/// with it; a longer run is split at a word boundary rather than truncated.
pub const MAX_TOKENS: usize = 510;

/// Longest piece of text handed to the model, in characters.
pub const SYNTHESIS_CHUNK: usize = 300;

/// The token the model expects at both ends of an utterance.
///
/// `0` is not a symbol: the table starts at 1, and `0` is the frame the graph reads as
/// silence. A run fed without one has no boundary frame, so where speech starts and stops
/// is left for the graph to infer from the first and last phoneme.
const PAD: i64 = 0;

/// Why a voice could not be loaded or used.
#[derive(Debug, Error)]
pub enum KokoroError {
    /// A file could not be read.
    #[error("reading {path}: {source}")]
    Io {
        /// File that failed.
        path: std::path::PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// A document is not the shape this engine reads.
    #[error("config: {0}")]
    Config(String),
    /// The style archive is not readable.
    #[error("voices: {0}")]
    Voices(String),
    /// A voice the archive does not hold was asked for.
    #[error("voice {name:?} is not in this archive; the voices here are {}", available.join(", "))]
    UnknownVoice {
        /// The name asked for.
        name: String,
        /// The names the archive holds.
        available: Vec<String>,
    },
    /// ONNX Runtime refused a step.
    #[error("onnx: {0}")]
    Ort(#[from] ort::Error),
    /// The model produced no output tensor.
    #[error("model returned no audio")]
    NoOutput,
    /// The caller stopped accepting audio.
    #[error("{0}")]
    Consumer(String),
    /// Characters in the supplied phonemes that this voice's vocabulary lacks.
    ///
    /// Dropped silently, the caller hears a different word and has no way to know.
    #[error("this voice cannot say {0:?}: its vocabulary has no token for it")]
    UnknownSymbols(String),
}

/// How a render is done.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Speaking rate, as a multiplier of the model's own rate.
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
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RenderReport {
    /// Words absent from the built-in dictionary and pronounced by rule.
    pub guessed: Vec<String>,
    /// Words nothing could pronounce.
    pub missing: Vec<String>,
    /// Dictionary entries no form of which could be spoken.
    pub unusable: Vec<String>,
    /// Phoneme characters the model's table does not have.
    pub dropped: Vec<char>,
}

/// A loaded Kokoro model with one voice selected.
pub struct KokoroVoice {
    session: Session,
    vocab: Vocab,
    voices: VoiceFile,
    style: Style,
    voice: String,
    /// Whether the graph wants the speed as an integer. The reference export takes a
    /// float; a quantised re-export has taken an int, and the dtype is read rather than
    /// assumed so either loads.
    speed_is_integer: bool,
}

impl std::fmt::Debug for KokoroVoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KokoroVoice")
            .field("voice", &self.voice)
            .field("sample_rate", &SAMPLE_RATE)
            .field("symbols", &self.vocab.len())
            .field("voices", &self.voices.names().len())
            .field("style", &(self.style.rows, self.style.width))
            .finish_non_exhaustive()
    }
}

/// Threads used when the caller does not say.
///
/// Every core, which is what this crate did before it took the number.
fn default_threads() -> u32 {
    std::thread::available_parallelism().map_or(1, |count| u32::try_from(count.get()).unwrap_or(1))
}

impl KokoroVoice {
    /// Load the model, the style archive and the symbol table, and select a voice.
    ///
    /// # Errors
    ///
    /// Returns [`KokoroError`] when a file cannot be read, the config has no usable
    /// vocab, the archive does not hold `voice`, or ONNX Runtime rejects the graph.
    pub fn load(
        model: &Path,
        voices: &Path,
        config: &Path,
        voice: &str,
    ) -> Result<Self, KokoroError> {
        Self::load_with_threads(model, voices, config, voice, default_threads())
    }

    /// Load with an explicit intra-op thread count.
    ///
    /// # Errors
    ///
    /// As [`KokoroVoice::load`].
    pub fn load_with_threads(
        model: &Path,
        voices: &Path,
        config: &Path,
        voice: &str,
        threads: u32,
    ) -> Result<Self, KokoroError> {
        let text = std::fs::read_to_string(config).map_err(|source| KokoroError::Io {
            path: config.to_path_buf(),
            source,
        })?;
        let vocab = Vocab::from_config(&text)?;
        let archive = VoiceFile::open(voices)?;
        let style = archive.style(voice)?;

        let threads = usize::try_from(threads).unwrap_or(1).max(1);
        let session = Session::builder()?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)?
            .with_intra_threads(threads)?
            .commit_from_file(model)?;

        // The graph's names are checked here rather than at the first sentence: a model
        // that is not Kokoro should fail at load, where the message can name what it
        // found instead of a runtime error in the middle of a request.
        for required in ["tokens", "style", "speed"] {
            if !session.inputs.iter().any(|input| input.name == required) {
                let found = session
                    .inputs
                    .iter()
                    .map(|input| input.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(KokoroError::Config(format!(
                    "graph has no {required} input; its inputs are {found}"
                )));
            }
        }
        let speed_is_integer = session
            .inputs
            .iter()
            .find(|input| input.name == "speed")
            .is_some_and(|input| format!("{:?}", input.input_type).contains("Int"));

        Ok(Self {
            session,
            vocab,
            voices: archive,
            style,
            voice: voice.to_string(),
            speed_is_integer,
        })
    }

    /// The voices the archive holds, without loading a graph.
    ///
    /// # Errors
    ///
    /// Returns [`KokoroError`] when the archive cannot be read.
    pub fn voice_names(voices: &Path) -> Result<Vec<String>, KokoroError> {
        Ok(VoiceFile::open(voices)?.names())
    }

    /// Sample rate of the audio this model produces.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    /// The voice currently selected.
    #[must_use]
    pub fn voice(&self) -> &str {
        &self.voice
    }

    /// The voices this archive holds.
    #[must_use]
    pub fn voices(&self) -> Vec<String> {
        self.voices.names()
    }

    /// Select another voice from the same archive.
    ///
    /// # Errors
    ///
    /// Returns [`KokoroError::UnknownVoice`] naming the ones that exist.
    pub fn select_voice(&mut self, name: &str) -> Result<(), KokoroError> {
        self.style = self.voices.style(name)?;
        self.voice = name.to_string();
        Ok(())
    }

    /// Turn phoneme symbols into model tokens.
    #[must_use]
    pub fn tokenize(&self, symbols: &[&str]) -> (Vec<i64>, Vec<char>) {
        self.vocab.tokenize(symbols)
    }

    /// Synthesize one run of tokens into mono samples.
    ///
    /// The style row is the one for this token count: the model was trained with a style
    /// per utterance length, and the last row covers anything longer. The count is the
    /// run's own, before the pad frames, because the archive is indexed by what was said
    /// rather than by the length of the tensor.
    ///
    /// # Errors
    ///
    /// Returns [`KokoroError`] when ONNX Runtime fails or returns no tensor.
    /// The symbols a text will be spoken as, without synthesizing any of it.
    ///
    /// The same symbol pass [`KokoroVoice::render`] runs, stopping before the model: a
    /// display that built its own symbols could show a line the voice would never say.
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
                let symbols = self.symbols(chunk, dictionary, user, options, &mut report);
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

    /// Synthesize phonemes a caller supplied, one run at a time.
    ///
    /// [`KokoroVoice::render`] builds these symbols from text; this takes them as they are,
    /// so a phoneme a person edited is the phoneme that is spoken.
    ///
    /// # Errors
    ///
    /// Characters this vocabulary has no token for are refused by name rather than dropped,
    /// and the engine's own failure comes back unchanged.
    pub fn render_symbols(
        &mut self,
        symbols: &[&str],
        speed: f32,
        mut sink: impl FnMut(&[f32], u32) -> Result<(), KokoroError>,
    ) -> Result<(), KokoroError> {
        let (tokens, dropped) = self.tokenize(symbols);
        if !dropped.is_empty() {
            let named: String = dropped.iter().collect();
            return Err(KokoroError::UnknownSymbols(named));
        }
        if tokens.is_empty() {
            return Ok(());
        }
        let rate = self.sample_rate();
        let space = self.vocab.id(' ').map(i64::from);
        for run in split_tokens(&tokens, space) {
            let audio = self.synthesize(&run, speed)?;
            sink(&audio, rate)?;
        }
        Ok(())
    }

    pub fn synthesize(&mut self, tokens: &[i64], speed: f32) -> Result<Vec<f32>, KokoroError> {
        let style = self.style.row(tokens.len()).to_vec();
        let width = self.style.width as i64;
        let padded = padded(tokens);
        let length = padded.len() as i64;

        let speed_value: ort::session::SessionInputValue<'_> = if self.speed_is_integer {
            Tensor::from_array(([1i64], vec![i64::from(speed.round() as i32)]))?.into()
        } else {
            Tensor::from_array(([1i64], vec![speed]))?.into()
        };
        let feeds: Vec<(&str, ort::session::SessionInputValue<'_>)> = vec![
            (
                "tokens",
                Tensor::from_array(([1i64, length], padded))?.into(),
            ),
            ("style", Tensor::from_array(([1i64, width], style))?.into()),
            ("speed", speed_value),
        ];

        let outputs = self.session.run(feeds)?;
        let output = outputs.get("audio").ok_or(KokoroError::NoOutput)?;
        let (_shape, samples) = output.try_extract_tensor::<f32>()?;
        Ok(samples.to_vec())
    }

    /// Speak `text`, handing each piece of audio to `sink` as it is ready.
    ///
    /// The text is split into clauses, then capped by [`SYNTHESIS_CHUNK`]; the tokens
    /// of a chunk are split again at a word boundary so no run is longer than
    /// [`MAX_TOKENS`]. Words missing from `dictionary` are reported, not skipped.
    ///
    /// # Errors
    ///
    /// Returns [`KokoroError`] when synthesis fails or `sink` refuses audio.
    pub fn render(
        &mut self,
        text: &str,
        dictionary: &CmuDict,
        user: &UserDictionary,
        options: RenderOptions,
        mut sink: impl FnMut(&[f32], u32) -> Result<(), KokoroError>,
    ) -> Result<RenderReport, KokoroError> {
        let rate = self.sample_rate();
        let mut report = RenderReport::default();
        let space = self.vocab.id(' ').map(i64::from);

        // Clauses, not sentences: the first audio leaves after the first comma rather than
        // after the first full stop, which on a long sentence is the whole latency. Each
        // clause is still capped by SYNTHESIS_CHUNK, so nothing becomes unbounded.
        for clause in clauses(text) {
            for chunk in chunk_text(clause, SYNTHESIS_CHUNK) {
                let symbols = self.symbols(chunk, dictionary, user, options, &mut report);
                if symbols.is_empty() {
                    continue;
                }
                let (tokens, dropped) = self.tokenize(&symbols);
                for character in dropped {
                    if !report.dropped.contains(&character) {
                        report.dropped.push(character);
                    }
                }
                if tokens.is_empty() {
                    continue;
                }
                for run in split_tokens(&tokens, space) {
                    let audio = self.synthesize(&run, options.speed)?;
                    sink(&audio, rate)?;
                }
            }
        }
        Ok(report)
    }

    /// Symbols for one chunk, including the closing stop.
    fn symbols(
        &self,
        text: &str,
        dictionary: &CmuDict,
        user: &UserDictionary,
        options: RenderOptions,
        report: &mut RenderReport,
    ) -> Vec<&'static str> {
        let mut groups: Vec<Vec<&'static str>> = Vec::new();
        for piece in user.pieces(text) {
            match piece {
                Piece::Entry(entry) => {
                    if let Some(symbols) = self.entry_symbols(entry, dictionary, options) {
                        groups.push(symbols);
                    } else {
                        // No form of the entry can be said, so say the term itself and
                        // report it: a caller that hears the wrong word can ask why.
                        report.unusable.push(entry.term.clone());
                        let phonemes = arioso_g2p::phonemes_for_text(
                            &entry.term,
                            dictionary,
                            options.normalize,
                            true,
                        );
                        collect(&phonemes, report);
                        groups.push(phonemes.symbols);
                    }
                }
                Piece::Text(segment) => {
                    let phonemes =
                        arioso_g2p::phonemes_for_text(segment, dictionary, options.normalize, true);
                    collect(&phonemes, report);
                    groups.push(phonemes.symbols);
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
        if let Some(stop) = closing_stop(text, &self.vocab).filter(|_| !symbols.is_empty()) {
            // One stop per utterance: the model was trained on punctuated phonemes, and a
            // run that ends without one stops in the middle of a breath. A run that
            // already ends in a mark keeps its own, because a period on `Hello?` makes it
            // a statement.
            symbols.push(stop);
        }
        symbols
    }

    /// Symbols for a dictionary entry, trying its forms in order.
    ///
    /// A form is used only when the model can say every character of it. Taking the first
    /// form that phonemizes to anything would choose a respelling this table cannot say
    /// and then have its unknown characters dropped, which is a different word from the
    /// one the next form would have given. `arioso doctor` asks the same question of the
    /// same forms, and the two answers have to agree.
    fn entry_symbols(
        &self,
        entry: &Entry,
        dictionary: &CmuDict,
        options: RenderOptions,
    ) -> Option<Vec<&'static str>> {
        first_sayable(&entry.forms(), dictionary, options.normalize, &self.vocab)
    }
}

/// The first form the table can say in full, in the order the entry lists them.
fn first_sayable(
    forms: &[&str],
    dictionary: &CmuDict,
    normalize: arioso_core::NormalizeOptions,
    vocab: &Vocab,
) -> Option<Vec<&'static str>> {
    forms.iter().find_map(|form| {
        let symbols = arioso_g2p::phonemes_for_form(form, dictionary, normalize, true);
        sayable(&symbols, vocab).then_some(symbols)
    })
}

/// Whether every character of a run of symbols is in the model's table.
fn sayable(symbols: &[&str], vocab: &Vocab) -> bool {
    !symbols.is_empty()
        && symbols
            .iter()
            .flat_map(|symbol| symbol.chars())
            .all(|character| vocab.id(character).is_some())
}

fn collect(phonemes: &arioso_g2p::Phonemes, report: &mut RenderReport) {
    report.guessed.extend(phonemes.guessed.iter().cloned());
    report.missing.extend(phonemes.missing.iter().cloned());
}

/// The stop to end a run with, when the table has one.
///
/// Phonemization drops punctuation, so `Hello?` and `Hello.` reach the graph as the same
/// symbols. The mark is taken from the text before that happens: the table's own `?` and
/// `!` are used when the sentence ended with one, because the graph was trained with them
/// and a period in their place turns a question into a statement. Anything else, and a
/// sentence with no mark at all, gets a period.
fn closing_stop(text: &str, vocab: &Vocab) -> Option<&'static str> {
    let mark = match text.trim_end().chars().next_back() {
        Some('!') => "!",
        Some('?') => "?",
        _ => ".",
    };
    if vocab.id(mark.chars().next()?).is_some() {
        return Some(mark);
    }
    // A table without the mark the sentence ended with still gets a stop rather than
    // none: a run with no stop at all is what this is here to avoid.
    vocab.id('.').is_some().then_some(".")
}

/// A run with the pad frame at each end, which is the shape the graph was exported for.
fn padded(tokens: &[i64]) -> Vec<i64> {
    let mut run = Vec::with_capacity(tokens.len() + 2);
    run.push(PAD);
    run.extend_from_slice(tokens);
    run.push(PAD);
    run
}

/// Split a run of tokens at a word boundary, so no call is longer than the model's
/// table. A run with no boundary in reach is cut at the limit.
fn split_tokens(tokens: &[i64], space: Option<i64>) -> Vec<Vec<i64>> {
    let mut runs = Vec::new();
    let mut rest = tokens;
    while rest.len() > MAX_TOKENS {
        let boundary = space
            .and_then(|space| rest[..MAX_TOKENS].iter().rposition(|token| *token == space))
            .filter(|position| *position > 0)
            .unwrap_or(MAX_TOKENS);
        runs.push(rest[..boundary].to_vec());
        rest = &rest[boundary..];
        // The space belongs to neither run.
        if let Some(space) = space {
            if rest.first() == Some(&space) {
                rest = &rest[1..];
            }
        }
    }
    if !rest.is_empty() {
        runs.push(rest.to_vec());
    }
    runs
}

impl TtsEngine for KokoroVoice {
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn render(
        &mut self,
        symbols: &[i64],
        speed: f32,
    ) -> Result<Vec<f32>, arioso_core::EngineError> {
        let mut audio = Vec::new();
        let space = self.vocab.id(' ').map(i64::from);
        for run in split_tokens(symbols, space) {
            let samples = self
                .synthesize(&run, speed)
                .map_err(|error| arioso_core::EngineError::Backend(error.to_string()))?;
            audio.extend_from_slice(&samples);
        }
        if audio.is_empty() {
            return Err(arioso_core::EngineError::NoOutput);
        }
        Ok(audio)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPACE: i64 = 16;

    #[test]
    fn a_long_run_is_split_at_a_word_boundary() {
        let mut tokens = vec![1i64; MAX_TOKENS + 20];
        tokens[MAX_TOKENS - 5] = SPACE;
        let runs = split_tokens(&tokens, Some(SPACE));
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].len(), MAX_TOKENS - 5);
        assert!(!runs[1].contains(&SPACE), "the boundary is in neither run");
        assert_eq!(runs[0].len() + runs[1].len(), tokens.len() - 1);
    }

    #[test]
    fn a_run_is_padded_at_both_ends() {
        assert_eq!(padded(&[4, 5, 6]), vec![0, 4, 5, 6, 0]);
        assert_eq!(padded(&[]), vec![0, 0], "an empty run is still two frames");
    }

    #[test]
    fn a_form_the_table_cannot_say_falls_through_to_the_next() {
        let dictionary = CmuDict::parse("hello HH AH0 L OW1\nworld W ER1 L D\n");
        let options = arioso_core::NormalizeOptions::default();
        // A table holding exactly the characters `world` needs, so the first form is
        // refused for a character it cannot say rather than for an empty result.
        let wanted = arioso_g2p::phonemes_for_form("W ER1 L D", &dictionary, options, true);
        let characters: Vec<char> = wanted
            .iter()
            .flat_map(|symbol| symbol.chars())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let table = characters
            .iter()
            .enumerate()
            .map(|(index, character)| format!("\"{character}\":{}", index + 1))
            .collect::<Vec<_>>()
            .join(",");
        let vocab = Vocab::from_config(&format!("{{\"vocab\":{{{table}}}}}")).unwrap();

        let forms = ["HH AH0 L OW1", "W ER1 L D"];
        assert!(!sayable(
            &arioso_g2p::phonemes_for_form(forms[0], &dictionary, options, true),
            &vocab
        ));
        assert_eq!(
            first_sayable(&forms, &dictionary, options, &vocab),
            Some(wanted),
            "the first form the table can say is the one used"
        );
    }

    #[test]
    fn a_sentence_keeps_the_mark_it_ends_with() {
        let vocab = Vocab::from_config(r#"{"vocab":{"a":1,".":4,"!":5,"?":6," ":16}}"#).unwrap();
        assert_eq!(closing_stop("Hello.", &vocab), Some("."));
        assert_eq!(closing_stop("Hello!", &vocab), Some("!"));
        assert_eq!(
            closing_stop("Hello?", &vocab),
            Some("?"),
            "a question stays a question"
        );
        assert_eq!(
            closing_stop("Hello", &vocab),
            Some("."),
            "a run with no mark gets a period"
        );
        assert_eq!(
            closing_stop("Hello?  ", &vocab),
            Some("?"),
            "trailing space hides nothing"
        );
    }

    #[test]
    fn a_table_without_the_mark_falls_back_to_a_period() {
        // A stripped table may have no `?`. The run still ends in something: no stop at
        // all is the failure the period is there to prevent.
        let partial = Vocab::from_config(r#"{"vocab":{"a":1,".":4}}"#).unwrap();
        assert_eq!(closing_stop("Hello?", &partial), Some("."));
        let bare = Vocab::from_config(r#"{"vocab":{"a":1}}"#).unwrap();
        assert_eq!(closing_stop("Hello.", &bare), None);
    }

    #[test]
    fn a_short_run_is_one_call() {
        let runs = split_tokens(&[1, 2, 3], Some(SPACE));
        assert_eq!(runs, vec![vec![1, 2, 3]]);
    }

    #[test]
    fn a_run_with_no_boundary_is_cut_at_the_limit() {
        let runs = split_tokens(&vec![1i64; MAX_TOKENS + 1], Some(SPACE));
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].len(), MAX_TOKENS);
        assert_eq!(runs[1].len(), 1);
    }
}
