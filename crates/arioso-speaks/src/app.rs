//! The app: what is selected, what is being rendered, and what the worker is doing.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use arioso_config::Config;
use arioso_models::Manifest;
use arioso_server::AppState;

use crate::audio::Player;
use crate::catalog::{self, Voice};
use crate::save::{self, Format};

/// The line the text area starts with.
///
/// It is the project's own sentence, so the first thing the app does when it opens is say
/// what arioso is.
pub const DEFAULT_TEXT: &str = "Arioso gives your apps a voice and ears—not a cloud dependency: local text-to-speech and speech-to-text in Rust, no API keys, no permission to speak required.";

/// How long typing has to stop before the phonemes are recomputed.
const PHONEME_DELAY: Duration = Duration::from_millis(350);

/// Which pane the keyboard is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The voice list.
    Voices,
    /// The text area.
    Text,
    /// The phoneme area.
    Phonemes,
    /// The action buttons.
    Actions,
}

impl Focus {
    /// The next pane, in the order the eye reads them.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Voices => Self::Text,
            Self::Text => Self::Phonemes,
            Self::Phonemes => Self::Actions,
            Self::Actions => Self::Voices,
        }
    }
}

/// The buttons, in the order they appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Render and play.
    Say,
    /// Render if needed, then write a file.
    Save,
    /// Regenerate the phonemes from the text.
    Reset,
}

/// What the worker was asked to do.
enum Job {
    Phonemes {
        state: Arc<AppState>,
        text: String,
    },
    Render {
        state: Arc<AppState>,
        phonemes: Option<String>,
        text: String,
        speed: f32,
        sid: i64,
        then_save: Option<Format>,
    },
    Download {
        state: Arc<AppState>,
        id: String,
    },
}

/// What the worker has to say about it.
pub enum Done {
    /// The phoneme line for one text.
    Phonemes {
        text: String,
        line: Result<String, String>,
    },
    /// Rendered audio, and the file written from it when one was asked for.
    Rendered(Result<Rendered, String>),
    /// A download's bytes so far.
    Progress { received: u64, total: u64 },
    /// A download finished or failed.
    Downloaded {
        id: String,
        result: Result<(), String>,
    },
}

/// Audio, and where it went.
pub struct Rendered {
    /// Mono samples at `rate`.
    pub samples: Vec<f32>,
    /// Samples per second.
    pub rate: u32,
    /// The file written from them, when a save was asked for.
    pub saved: Option<PathBuf>,
}

/// A text field with a cursor, which is the whole editor.
pub struct TextBuffer {
    chars: Vec<char>,
    /// The index the next character goes before, in `0..=chars.len()`.
    pub cursor: usize,
}

impl TextBuffer {
    /// A buffer holding `text`, with the cursor at the end.
    #[must_use]
    pub fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    /// The text as it stands.
    #[must_use]
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// Whether there is nothing to say.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.chars.iter().all(|character| character.is_whitespace())
    }

    /// Insert one character at the cursor.
    pub fn insert(&mut self, character: char) {
        self.chars.insert(self.cursor, character);
        self.cursor += 1;
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    /// Delete the character at the cursor.
    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    /// Move the cursor one place towards the start.
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the cursor one place towards the end.
    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    /// Move the cursor to the start.
    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end.
    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }
}

/// Everything the app knows.
pub struct App {
    /// The config as it is on disk, plus what this app changed in it.
    pub config: Config,
    /// Where the config came from, so the state can re-read dictionaries beside it.
    pub config_path: PathBuf,
    /// The catalog, kept so the state can be rebuilt when the voice changes.
    pub manifest: Manifest,
    /// The engine and voice currently loaded, with everything that owns.
    pub state: Arc<AppState>,
    /// Every voice of the selected engine.
    pub voices: Vec<Voice>,
    /// The positions in `voices` the filter keeps.
    pub visible: Vec<usize>,
    /// What has been typed into the filter.
    pub filter: String,
    /// The row the list has highlighted, as a position in `visible`.
    pub selected: usize,
    /// The engine tab that is showing.
    pub engine: String,
    /// Which speaker of a multi-speaker voice to use.
    pub sid: i64,
    /// The text to say.
    pub text: TextBuffer,
    /// The phonemes that will be spoken.
    pub phonemes: String,
    /// Whether the phoneme line was edited by hand.
    pub phonemes_edited: bool,
    /// Whether a phoneme pass is in flight.
    pub phonemes_pending: bool,
    /// Which pane has the keyboard.
    pub focus: Focus,
    /// Which button is highlighted.
    pub action: usize,
    /// Playback and the envelope.
    pub player: Player,
    /// What the status line says.
    pub status: String,
    /// What went wrong, until the next successful thing.
    pub error: Option<String>,
    /// The download in flight: its id, bytes so far, and total.
    pub download: Option<(String, u64, u64)>,
    /// Whether the save format is being chosen.
    pub save_prompt: bool,
    /// The last audio rendered, kept so a save does not render it again.
    pub audio: Option<Rendered>,
    /// Set when the app should close.
    pub quit: bool,
    jobs: Sender<Job>,
    done: Receiver<Done>,
    phonemes_due: Option<Instant>,
}

impl App {
    /// Build the app from a config file.
    ///
    /// # Errors
    ///
    /// The manifest beside the models cannot be read, or the state cannot be built from the
    /// config — a dictionary that will not load, for instance.
    pub fn new(config: Config, config_path: PathBuf, manifest: Manifest) -> Result<Self, String> {
        let engine = config.tts.engine.clone();
        let state = Arc::new(
            AppState::new(config.clone(), manifest.clone(), Some(&config_path))
                .map_err(|error| error.to_string())?,
        );
        let voices = catalog::voices(&state.models_view(), &engine);
        let visible = catalog::matching(&voices, "");
        // The list opens on the voice the config names, because that is the one this app
        // would speak with if the keyboard were never touched.
        let selected = voices
            .iter()
            .position(|voice| catalog::is_named(&voice.id, config.voice_name()))
            .and_then(|index| visible.iter().position(|row| *row == index))
            .unwrap_or(0);
        let (jobs, job_rx) = channel();
        let (job_done, done) = channel();
        // One worker for every job: a render and a download are both long, and a second
        // thread would let two of them fight over the same engine lock.
        thread::spawn(move || worker(job_rx, job_done));
        let mut app = Self {
            config,
            config_path,
            manifest,
            state,
            voices,
            visible,
            filter: String::new(),
            selected,
            engine,
            sid: 0,
            text: TextBuffer::new(DEFAULT_TEXT),
            phonemes: String::new(),
            phonemes_edited: false,
            phonemes_pending: false,
            focus: Focus::Text,
            action: 0,
            player: Player::new(),
            status: String::new(),
            error: None,
            download: None,
            save_prompt: false,
            audio: None,
            quit: false,
            jobs,
            done,
            phonemes_due: Some(Instant::now()),
        };
        if !app.player.available() {
            app.status = "no audio output device: saving still works".to_owned();
        }
        Ok(app)
    }

    /// The voice the list has highlighted.
    #[must_use]
    pub fn current_voice(&self) -> Option<&Voice> {
        self.visible
            .get(self.selected)
            .and_then(|index| self.voices.get(*index))
    }

    /// The buttons, in the order they appear, with what they would do right now.
    #[must_use]
    pub fn actions(&self) -> Vec<(Action, String, bool)> {
        let voice = self.current_voice();
        let mut actions = Vec::new();
        let can_play = self.player.available();
        match voice {
            Some(voice) if !voice.installed => actions.push((
                Action::Say,
                format!(
                    "Download {} — {} · {}",
                    voice.id,
                    catalog::size(voice.bytes),
                    voice.licence_label()
                ),
                true,
            )),
            Some(voice) => actions.push((
                Action::Say,
                format!("Say it · {}", voice.id),
                can_play && !self.text.is_blank(),
            )),
            None => actions.push((Action::Say, "Say it".to_owned(), false)),
        }
        actions.push((
            Action::Save,
            "Save it".to_owned(),
            !self.text.is_blank() || !self.phonemes.is_empty(),
        ));
        actions.push((
            Action::Reset,
            "Reset phonemes".to_owned(),
            self.phonemes_edited,
        ));
        actions
    }

    /// Switch engine tabs.
    pub fn set_engine(&mut self, engine: &str) {
        if engine == self.engine {
            return;
        }
        self.engine = engine.to_owned();
        self.config.tts.engine = engine.to_owned();
        // A voice belongs to one engine, so the name it had means nothing here.
        self.config.tts.voice = None;
        self.rebuild();
    }

    /// Choose the highlighted voice.
    pub fn select_voice(&mut self) {
        let Some(voice) = self.current_voice().cloned() else {
            return;
        };
        if !voice.installed {
            self.start_download(&voice.id);
            return;
        }
        self.config.tts.engine = voice.engine.clone();
        self.config.tts.voice = Some(voice.id.clone());
        self.rebuild();
        self.status = format!("{} selected · {}", voice.id, voice.licence_label());
        self.error = None;
    }

    /// The speakers of the highlighted voice, as the manifest names them.
    #[must_use]
    pub fn speakers(&self) -> &[String] {
        self.current_voice()
            .map(|voice| voice.speakers.as_slice())
            .unwrap_or(&[])
    }

    /// Move to the next speaker of a multi-speaker voice.
    ///
    /// A Kokoro style is one speaker, so there is nothing to move through and the number
    /// stays where it is rather than wrapping to a speaker that does not exist.
    pub fn next_speaker(&mut self) {
        let count = self.speakers().len();
        if count > 1 {
            self.sid = (self.sid + 1) % count as i64;
        }
    }

    /// Move to the previous speaker.
    pub fn previous_speaker(&mut self) {
        let count = self.speakers().len();
        if count > 1 {
            self.sid = (self.sid + count as i64 - 1) % count as i64;
        }
    }

    /// Rebuild the state after the config changed.
    fn rebuild(&mut self) {
        match AppState::new(
            self.config.clone(),
            self.manifest.clone(),
            Some(&self.config_path),
        ) {
            Ok(state) => {
                self.state = Arc::new(state);
                self.voices = catalog::voices(&self.state.models_view(), &self.engine);
                self.visible = catalog::matching(&self.voices, &self.filter);
                self.selected = self.selected.min(self.visible.len().saturating_sub(1));
                self.audio = None;
                self.phonemes.clear();
                self.phonemes_edited = false;
                self.phonemes_due = Some(Instant::now());
            }
            Err(error) => {
                self.error = Some(error.to_string());
            }
        }
    }

    /// Ask for the phoneme line for the current text.
    fn ask_phonemes(&mut self) {
        if self.phonemes_pending {
            self.phonemes_due = Some(Instant::now() + PHONEME_DELAY);
            return;
        }
        self.phonemes_pending = true;
        let _ = self.jobs.send(Job::Phonemes {
            state: Arc::clone(&self.state),
            text: self.text.text(),
        });
    }

    /// Say the current text, or download the highlighted voice if it is not on disk.
    pub fn say(&mut self) {
        if let Some(voice) = self.current_voice()
            && !voice.installed
        {
            let id = voice.id.clone();
            self.start_download(&id);
            return;
        }
        if self.download.is_some() {
            self.error = Some("a download is already running".to_owned());
            return;
        }
        self.render(None);
    }

    /// Render the current text, optionally saving the result.
    pub fn render(&mut self, then_save: Option<Format>) {
        if self.text.is_blank() {
            self.error = Some("there is no text to say".to_owned());
            return;
        }
        // The edited line is what is spoken. Rendering the text instead would speak
        // something other than what the screen shows.
        let phonemes = if self.phonemes_edited && !self.phonemes.is_empty() {
            Some(self.phonemes.clone())
        } else {
            None
        };
        self.status = "rendering…".to_owned();
        self.error = None;
        // Two voices at once is never what the key meant.
        self.player.stop();
        let _ = self.jobs.send(Job::Render {
            state: Arc::clone(&self.state),
            phonemes,
            text: self.text.text(),
            speed: self.config.tts.speed,
            sid: self.sid,
            then_save,
        });
    }

    /// Save the last audio, rendering it first if there is none.
    pub fn save(&mut self, format: Format) {
        self.save_prompt = false;
        if let Some(audio) = &self.audio {
            let path = save::suggested_path(&self.text.text(), format);
            match save::save(&path, format, &audio.samples, audio.rate) {
                Ok(()) => {
                    self.status = format!("saved {}", path.display());
                    self.error = None;
                }
                Err(error) => self.error = Some(format!("{error:#}")),
            }
            return;
        }
        self.render(Some(format));
    }

    /// Regenerate the phoneme line from the text.
    pub fn reset_phonemes(&mut self) {
        self.phonemes_edited = false;
        self.phonemes.clear();
        self.phonemes_due = Some(Instant::now());
        self.status = "phonemes regenerated".to_owned();
    }

    /// Fetch a voice that is not on disk.
    pub fn start_download(&mut self, id: &str) {
        if self.download.is_some() {
            self.error = Some("a download is already running".to_owned());
            return;
        }
        let total = self
            .voices
            .iter()
            .find(|voice| voice.id == id)
            .map_or(0, |voice| voice.bytes);
        self.download = Some((id.to_owned(), 0, total));
        self.status = format!("downloading {id}…");
        self.error = None;
        let _ = self.jobs.send(Job::Download {
            state: Arc::clone(&self.state),
            id: id.to_owned(),
        });
    }

    /// Mark the text as changed, so the phonemes are recomputed after a pause.
    pub fn text_changed(&mut self) {
        self.audio = None;
        self.phonemes_due = Some(Instant::now() + PHONEME_DELAY);
    }

    /// Do the work that is due: drain the worker, and ask for phonemes when typing stops.
    pub fn tick(&mut self) {
        while let Ok(done) = self.done.try_recv() {
            match done {
                Done::Phonemes { text, line } => {
                    self.phonemes_pending = false;
                    // A line for text that has since changed is history.
                    if text == self.text.text() {
                        match line {
                            Ok(line) => {
                                if !self.phonemes_edited {
                                    self.phonemes = line;
                                }
                            }
                            Err(error) => self.error = Some(error),
                        }
                    } else {
                        self.phonemes_due = Some(Instant::now());
                    }
                }
                Done::Rendered(Ok(rendered)) => {
                    if let Some(path) = &rendered.saved {
                        self.status = format!("saved {}", path.display());
                    } else {
                        self.status = format!(
                            "{} at {} Hz",
                            format_seconds(rendered.samples.len() as f32 / rendered.rate as f32),
                            rendered.rate
                        );
                    }
                    self.error = None;
                    if self.player.available() {
                        match self.player.play(&rendered.samples, rendered.rate) {
                            Ok(()) => {}
                            Err(error) => self.error = Some(error),
                        }
                    }
                    self.audio = Some(rendered);
                }
                Done::Rendered(Err(error)) => {
                    self.error = Some(error);
                    self.status.clear();
                }
                Done::Progress { received, total } => {
                    if let Some(download) = &mut self.download {
                        download.1 = received;
                        download.2 = total;
                    }
                }
                Done::Downloaded { id, result } => {
                    self.download = None;
                    match result {
                        Ok(()) => {
                            self.status = format!("{id} downloaded");
                            self.rebuild();
                            self.select_voice();
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
            }
        }
        if let Some(due) = self.phonemes_due
            && Instant::now() >= due
            && !self.text.is_blank()
        {
            self.phonemes_due = None;
            self.ask_phonemes();
        }
    }

    /// How far along the download is, in `0.0..=1.0`.
    #[must_use]
    pub fn download_fraction(&self) -> Option<f32> {
        let (_, received, total) = self.download.as_ref()?;
        if *total == 0 {
            return Some(0.0);
        }
        Some((*received as f32 / *total as f32).clamp(0.0, 1.0))
    }
}

/// Seconds, as `0.0s`.
fn format_seconds(seconds: f32) -> String {
    format!("{seconds:.1}s")
}

/// The worker: one thread, one job at a time, so two renders never hold the engine at once.
fn worker(jobs: Receiver<Job>, done: Sender<Done>) {
    while let Ok(job) = jobs.recv() {
        match job {
            Job::Phonemes { state, text } => {
                let line = state.phonemes_for(&text);
                let _ = done.send(Done::Phonemes { text, line });
            }
            Job::Render {
                state,
                phonemes,
                text,
                speed,
                sid,
                then_save,
            } => {
                let mut samples: Vec<f32> = Vec::new();
                let mut rate = 0u32;
                let mut sink = |audio: &[f32], at: u32| {
                    rate = at;
                    samples.extend_from_slice(audio);
                    Ok(())
                };
                let rendered = match phonemes {
                    Some(line) => state.render_phonemes_stream(&line, speed, sid, &mut sink),
                    None => state.synthesize_stream(&text, speed, sid, &mut sink),
                };
                let result = match rendered {
                    Ok(()) => {
                        let mut rendered = Rendered {
                            samples,
                            rate,
                            saved: None,
                        };
                        if let Some(format) = then_save {
                            let path = save::suggested_path(&text, format);
                            match save::save(&path, format, &rendered.samples, rendered.rate) {
                                Ok(()) => rendered.saved = Some(path),
                                Err(error) => {
                                    let _ = done.send(Done::Rendered(Err(format!("{error:#}"))));
                                    continue;
                                }
                            }
                        }
                        Ok(rendered)
                    }
                    Err(error) => Err(error),
                };
                let _ = done.send(Done::Rendered(result));
            }
            Job::Download { state, id } => {
                let reporting = id.clone();
                let mut progress = |received: u64, total: u64| {
                    let _ = done.send(Done::Progress { received, total });
                    // A download in a terminal app is not cancelled from the keyboard: the
                    // action row is showing the bar, and the escape key is the quit key.
                    let _ = &reporting;
                    true
                };
                let result = state
                    .download_blocking(&id, &mut progress)
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                let _ = done.send(Done::Downloaded { id, result });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_focus_order_returns_to_the_top() {
        assert_eq!(Focus::Voices.next(), Focus::Text);
        assert_eq!(Focus::Text.next(), Focus::Phonemes);
        assert_eq!(Focus::Phonemes.next(), Focus::Actions);
        assert_eq!(Focus::Actions.next(), Focus::Voices);
    }

    #[test]
    fn a_buffer_edits_at_the_cursor() {
        let mut buffer = TextBuffer::new("hello");
        buffer.left();
        buffer.insert('!');
        assert_eq!(buffer.text(), "hell!o");
        buffer.backspace();
        assert_eq!(buffer.text(), "hello");
        buffer.home();
        buffer.delete();
        assert_eq!(buffer.text(), "ello");
        buffer.end();
        buffer.insert('!');
        assert_eq!(buffer.text(), "ello!");
    }

    #[test]
    fn the_default_line_is_not_blank() {
        assert!(!TextBuffer::new(DEFAULT_TEXT).is_blank());
        assert!(TextBuffer::new("  \n ").is_blank());
    }
}
