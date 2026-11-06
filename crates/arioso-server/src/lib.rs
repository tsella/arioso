//! HTTP surface for arioso.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use arioso_config::{AUDIO_FORMATS, Config};
use arioso_core::UserDictionary;
use arioso_g2p::CmuDict;
use arioso_g2p::reverse::ReverseIndex;
use arioso_models::{Kind, Manifest, ModelSpec, models_dir};
use arioso_tts_kokoro::{KokoroError, KokoroVoice};
use arioso_tts_piper::{PiperError, PiperVoice, wav};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use thiserror::Error;

mod ws;
mod ws_stt;

pub use ws::handler as ws_handler;
pub use ws_stt::handler as stt_ws_handler;

/// Why the server could not start.
#[derive(Debug, Error)]
pub enum ServerError {
    /// The pronunciation dictionary is missing or unreadable.
    #[error("reading the dictionary at {path}: {source}")]
    Dictionary {
        /// File that failed.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// The configured voice is not in the manifest.
    #[error("no voice matches tts.voice {0:?} on the engine in tts.engine")]
    NoVoice(String),
    /// The configured speech to text model is not in the manifest.
    #[error("no speech to text model matches stt.batch_model {0:?}")]
    NoSttModel(String),
    /// The configured voice activity detector is not in the manifest.
    #[error("no detector matches vad.model {0:?}")]
    NoVadModel(String),
    /// The configured streaming speech to text model cannot be served.
    ///
    /// The detail is the same sentence PATCH answers with, so a config refused at startup
    /// and one refused at runtime say the same thing.
    #[error("{0}")]
    StreamModel(String),
    /// The user dictionary could not be used.
    #[error("user dictionary: {0}")]
    UserDictionary(#[from] arioso_core::DictError),
    /// A bearer token was configured but the environment does not hold one.
    #[error("server.auth.mode is bearer but the environment variable {variable} is empty or unset")]
    Auth {
        /// Environment variable that should hold the token.
        variable: String,
    },
    /// The certificate or key could not be read, or they do not match.
    #[error("reading the certificate {cert} and the key {key}: {source}")]
    Tls {
        /// Certificate path, as configured.
        cert: String,
        /// Key path, as configured.
        key: String,
        /// Underlying error from the TLS loader.
        #[source]
        source: std::io::Error,
    },
}

/// Split an IPA line into the symbols the engines take.
///
/// Free rather than a method because it needs no state, and shared rather than copied
/// because the C ABI answers a bad character before it looks at a voice: an argument error
/// reported as an engine failure sends a caller looking in the wrong place.
///
/// # Errors
///
/// The first character that is not a phoneme, in a sentence that names it.
pub fn parse_phonemes(phonemes: &str) -> Result<Vec<&'static str>, String> {
    arioso_g2p::arpabet::symbols_from_ipa(phonemes)
        .map_err(|character| format!("{character:?} is not a phoneme"))
}

/// A speaker a render cannot use, and why.
///
/// The two cases answer with different codes on the C ABI: a voice that would not load is an
/// engine failure, and a speaker the voice does not have is the caller's mistake.
#[derive(Debug, Error)]
pub enum SidProblem {
    /// The voice could not be loaded, so nothing can say what speakers it has.
    #[error("{0}")]
    Unavailable(String),
    /// The voice loaded and does not have this speaker.
    #[error("{0}")]
    NoSuchSpeaker(String),
}

/// Shared server state.
#[derive(Debug)]
pub struct AppState {
    /// Mutable so PATCH /v1/config can change it without a restart. Held behind an `Arc`
    /// so a reader takes a reference instead of copying fourteen strings: `config()` runs
    /// several times per request, including in the rate limiter on every `/healthz`.
    config: std::sync::RwLock<Arc<Config>>,
    manifest: Manifest,
    models_dir: PathBuf,
    dictionary: CmuDict,
    user: Mutex<UserState>,
    voice: Mutex<Option<Voice>>,
    /// How many times the loaded voice has been dropped, and how many times a patch tried
    /// to swap the style in place. Test-only: the fact is invisible without a 325 MB graph
    /// in the test binary, and nothing in production reads a counter.
    #[cfg(test)]
    voice_drops: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    style_swaps: std::sync::atomic::AtomicUsize,
    /// Whisper, loaded on first use. One model, so one slot.
    stt: Mutex<Option<arioso_stt_sherpa::Engine>>,
    /// The streaming model the socket uses, when one is configured. A separate slot from
    /// the batch one, because the two models are chosen by different settings and the
    /// socket must not displace the model the file route is using.
    stt_stream: Mutex<Option<arioso_stt_sherpa::Engine>>,
    /// The diarization models, loaded on first use.
    ///
    /// They are 34 MiB together and the load is a second, so the slot keeps them. It holds
    /// nothing between calls but the models: there is no per-recording state to clear,
    /// because the clustering finishes with the file.
    diarizer: Mutex<Option<arioso_stt_sherpa::diarize::Diarizer>>,
    /// Where the config came from, so a patch can be written back. Absent when the
    /// server was started with defaults and no file.
    config_file: Option<std::path::PathBuf>,
    /// Per-client request budgets, from `server.limits`.
    limiter: RateLimiter,
    /// The bearer token, when one was configured and found in the environment.
    token: Option<Arc<str>>,
    /// Permits for long-lived streaming sessions.
    ///
    /// A socket that stays open for the length of a document is the resource that
    /// accumulates: ordinary requests end, streams do not. `server.limits.max_connections`
    /// bounds how many may be open at once, and a caller that finds none free is told so
    /// rather than queued behind an unbounded wait.
    sessions: Arc<tokio::sync::Semaphore>,
    /// The accept loop's handle, once the server is running.
    ///
    /// Set by whoever starts the server, because the server library does not own the
    /// listener. Absent in tests, where there is no accept loop to stop.
    handle: std::sync::OnceLock<axum_server::Handle<std::net::SocketAddr>>,
    /// Downloads in flight, keyed by model id: bytes fetched and bytes expected.
    ///
    /// A download outlives the request that started it, so the progress has to live
    /// somewhere other than the handler. It is a map rather than a channel because the
    /// catalog has to answer "what is happening" without a subscriber.
    downloads: std::sync::Mutex<std::collections::BTreeMap<String, (u64, u64)>>,
    /// Caps how many models are fetched at once. A catalog entry is hundreds of megabytes,
    /// and an unbounded fan-out saturates the link and the disk for everything else.
    download_slots: Arc<tokio::sync::Semaphore>,
    /// Model state changes, fanned out to every `GET /v1/models/events` subscriber.
    ///
    /// A broadcast rather than a queue per subscriber: a download happens once and any
    /// number of clients may be watching it. Capacity 256 is a few seconds of progress on
    /// a fast link, and a subscriber that falls further behind is told it lagged instead of
    /// being handed a silently incomplete sequence.
    events: tokio::sync::broadcast::Sender<ModelEvent>,
}

/// The user dictionary on disk, and what it last looked like.
#[derive(Debug)]
struct UserState {
    path: PathBuf,
    modified: Option<std::time::SystemTime>,
    /// Shared, so a render and a transcription can each hold one without holding the lock.
    ///
    /// A reload replaces the `Arc` and leaves the readers that already took one reading the
    /// copy they started with, which is what a dictionary that changed mid-sentence should
    /// do: one request sees one dictionary.
    dictionary: Arc<UserDictionary>,
    /// The dictionary's spoken forms, pronounced and ordered once. Rebuilding this per
    /// transcript re-pronounced every form of every entry on every final.
    reverse: Arc<ReverseIndex>,
    /// Parse error from the last reload attempt. The last good copy stays in use.
    error: Option<String>,
}

/// The user dictionary, taken at one moment.
///
/// Held as two `Arc`s rather than a guard: the lock is taken to look for a reload and released
/// immediately, so a synthesis that runs for seconds and a transcription that runs for seconds
/// no longer wait for each other over a dictionary neither of them writes.
#[derive(Clone)]
struct UserSnapshot {
    dictionary: Arc<UserDictionary>,
    reverse: Arc<ReverseIndex>,
    error: Option<String>,
}

/// The catalog entry `stt.stream_model` names, when it names one that streams.
///
/// Empty is the documented "use `stt.batch_model`" value, so it resolves to nothing. A
/// non-empty value that names no catalog entry, or one whose family does not stream, is
/// not silently ignored: [`stream_model_problem`] reports it to startup and to PATCH.
fn stream_spec_in<'a>(config: &Config, manifest: &'a Manifest) -> Option<&'a ModelSpec> {
    let id = config.stt.stream_model.trim();
    if id.is_empty() {
        return None;
    }
    let spec = manifest
        .models
        .iter()
        .find(|model| model.kind == Kind::Stt && model.engine == "sherpa" && model.id == id)?;
    arioso_stt_sherpa::is_streaming(AppState::model_type(spec)).then_some(spec)
}

/// Why `stt.stream_model` cannot be served, when it names anything at all.
///
/// A config that leaves the field empty keeps the batch path over `stt.batch_model`, which
/// is what every config written before streaming existed means. A non-empty value must name
/// a catalog entry whose `meta.model_type` streams; a typo or a batch model would otherwise
/// fall back to the batch loop with no error anywhere.
fn stream_model_problem(config: &Config, manifest: &Manifest) -> Option<String> {
    let id = config.stt.stream_model.trim();
    if id.is_empty() {
        return None;
    }
    match manifest
        .models
        .iter()
        .find(|model| model.kind == Kind::Stt && model.engine == "sherpa" && model.id == id)
    {
        Some(spec) if arioso_stt_sherpa::is_streaming(AppState::model_type(spec)) => None,
        Some(_) => Some(format!(
            "stt.stream_model {id} is not a streaming model; its meta.model_type is not {}",
            arioso_stt_sherpa::STREAMING_MODEL_TYPE
        )),
        None => Some(format!(
            "stt.stream_model {id} is not in the catalog; GET /v1/models lists what is"
        )),
    }
}

impl AppState {
    /// Build state, loading the pronunciation dictionary.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Dictionary`] when the dictionary is unreadable.
    pub fn new(
        config: Config,
        manifest: Manifest,
        config_file: Option<&std::path::Path>,
    ) -> Result<Self, ServerError> {
        let models_dir = models_dir(&config);
        let sessions = Arc::new(tokio::sync::Semaphore::new(
            config.server.limits.max_connections,
        ));
        // Read before the config moves into the state, and capped at one so a zero can
        // never make the semaphore refuse every download forever.
        let download_slots = Arc::new(tokio::sync::Semaphore::new(
            config.models.max_concurrent_downloads.max(1) as usize,
        ));
        let limiter = RateLimiter::new(
            config.server.limits.requests_per_second,
            config.server.limits.burst,
        );
        // Nobody has to be listening: `send` to a channel with no receivers is an error
        // that is ignored, because a download nobody is watching is the ordinary case.
        let (events, _) = tokio::sync::broadcast::channel(256);
        // The token lives in the environment, never in the document. Asking for a bearer
        // token and finding none is a startup failure: a server that thinks it is
        // protected and is not is worse than one that refuses to start.
        let token = if config.server.auth.mode == "bearer" {
            let value =
                std::env::var(&config.server.auth.token_env).map_err(|_| ServerError::Auth {
                    variable: config.server.auth.token_env.clone(),
                })?;
            if value.trim().is_empty() {
                return Err(ServerError::Auth {
                    variable: config.server.auth.token_env.clone(),
                });
            }
            Some(value)
        } else {
            None
        };
        let config_file = config_file.map(std::path::Path::to_path_buf);
        let path = models_dir.join("cmudict.dict");
        let text = std::fs::read_to_string(&path)
            .map_err(|source| ServerError::Dictionary { path, source })?;
        let path = config.dictionary_path(config_file.as_deref());
        let modified = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok();
        let (dictionary, error) = match UserDictionary::load(&path) {
            Ok(dictionary) => (dictionary, None),
            // No file is not an empty dictionary: the shipped one carries the names
            // a voice cannot otherwise say.
            Err(arioso_core::DictError::Io { .. }) => (UserDictionary::shipped(), None),
            Err(error) => {
                return Err(ServerError::UserDictionary(error));
            }
        };
        if let Some(detail) = stream_model_problem(&config, &manifest) {
            return Err(ServerError::StreamModel(detail));
        }
        let cmu = CmuDict::parse(&text);
        let reverse = ReverseIndex::build(&dictionary, &cmu);
        let state = Self {
            dictionary: cmu,
            user: Mutex::new(UserState {
                path,
                modified,
                dictionary: Arc::new(dictionary),
                reverse: Arc::new(reverse),
                error,
            }),
            voice: Mutex::new(None),
            #[cfg(test)]
            voice_drops: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            style_swaps: std::sync::atomic::AtomicUsize::new(0),
            stt: Mutex::new(None),
            stt_stream: Mutex::new(None),
            diarizer: Mutex::new(None),
            config: std::sync::RwLock::new(Arc::new(config)),
            config_file,
            limiter,
            token: token.map(Arc::from),
            sessions,
            handle: std::sync::OnceLock::new(),
            downloads: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            download_slots,
            events,
            manifest,
            models_dir,
        };
        // A manifest with entries is a catalog to hold the config against, so a name that is
        // not in it is refused now rather than at the first request. An empty manifest is a
        // models directory with nothing downloaded yet: a voice it does not list is not
        // wrong there, only absent, and the download route is how it arrives.
        // Only a catalog that lists a model of that kind can answer the question. A
        // manifest holding speech models and no voice names is a catalog with nothing to
        // check against, not a config that is wrong.
        if state.config().tts.enabled
            && state
                .manifest
                .models
                .iter()
                .any(|model| model.kind == Kind::Tts)
        {
            state.voice_spec()?;
        }
        if state.config().stt.enabled
            && state
                .manifest
                .models
                .iter()
                .any(|model| model.kind == Kind::Stt)
        {
            state.stt_spec()?;
        }
        // A detector named by a config that streams speech has to be a detector the catalog
        // knows. Only a catalog that lists one can answer the question, for the same reason
        // the voice check above says so: an empty directory is nothing downloaded yet, not a
        // wrong name.
        if state.config().stt.enabled
            && state.config().vad.engine == "silero"
            && state
                .manifest
                .models
                .iter()
                .any(|model| model.kind == Kind::Vad)
        {
            let id = &state.config().vad.model;
            if !state
                .manifest
                .models
                .iter()
                .any(|model| model.kind == Kind::Vad && model.id == *id)
            {
                return Err(ServerError::NoVadModel(id.clone()));
            }
        }
        Ok(state)
    }

    /// The config as it stands, cloned.
    ///
    /// Cloned rather than borrowed so a caller never holds the lock across an await.
    /// A poisoned lock still yields the value: the config is plain data, so a panic in
    /// another task cannot leave it inconsistent.
    /// Claim the download of `id`, or report the one already in flight.
    ///
    /// The check and the insert happen under one lock. Taken separately, two requests that
    /// both saw "not running" would both spawn a task, and the two tasks would write the
    /// same `.part` file at the same time.
    fn try_start_download(&self, id: &str, total: u64) -> Option<(u64, u64)> {
        match self.downloads.lock() {
            Ok(mut map) => {
                if let Some(running) = map.get(id) {
                    return Some(*running);
                }
                map.insert(id.to_owned(), (0, total));
                None
            }
            // A poisoned lock means another task panicked holding it. Reporting the
            // download as already running refuses this request instead of risking a second
            // writer, which is the safe direction to fail in.
            Err(_) => Some((0, total)),
        }
    }

    /// Start fetching a model, or report the one already in flight.
    ///
    /// Returns `Some(progress)` when a download was already running, and `None` when this
    /// call started one. The task waits for a slot, fetches through the configured mirror,
    /// and clears the catalog entry whether it worked or not.
    pub(crate) fn begin_download(self: &Arc<Self>, spec: &ModelSpec) -> Option<(u64, u64)> {
        let id = spec.id.clone();
        let total = spec.total_size();
        if let Some(running) = self.try_start_download(&id, total) {
            return Some(running);
        }
        // Announced before the task is spawned: a subscriber that connects while the
        // download waits for a slot still learns it is running.
        self.publish(ModelEvent {
            id: id.clone(),
            state: "downloading".to_owned(),
            downloaded: Some(0),
            total,
            error: None,
        });
        let state = Arc::clone(self);
        let dir = self.models_dir.clone();
        let spec = spec.clone();
        tokio::spawn(async move {
            // Wait for a slot before touching the network. The catalog shows this model as
            // downloading while it waits, which is true: the request was accepted.
            let slot = Arc::clone(&state.download_slots).acquire_owned().await;
            let downloader = arioso_models::Downloader::with_options(
                state.config().models.mirror.clone(),
                state.config().models.ca_bundle.as_deref(),
            );
            let result = match downloader {
                Ok(downloader) => {
                    let reporting = Arc::clone(&state);
                    let watched = id.clone();
                    let mut progress = move |p: arioso_models::Progress| {
                        reporting.note_download(&watched, p.downloaded, p.total);
                    };
                    downloader.ensure_model(&spec, &dir, &mut progress).await
                }
                Err(error) => Err(error),
            };
            // Taken before the match moves the result: the stream reports how the download
            // ended, and "it failed" without the reason is not an answer.
            let failure = result.as_ref().err().map(ToString::to_string);
            match result {
                Ok(paths) => tracing::info!(
                    model = %id,
                    files = paths.len(),
                    bytes = total,
                    "model downloaded"
                ),
                // The catalog stops showing the download, so the failure has to be visible
                // somewhere: a caller that asked for a model and got silence has nothing.
                Err(error) => tracing::error!(model = %id, %error, "model download failed"),
            }
            drop(slot);
            state.finish_download(&id, failure.as_deref());
        });
        None
    }

    /// Fetch a model and wait for it, reporting progress while it runs.
    ///
    /// The routes hand the work to a task and report through `/v1/models`, because an HTTP
    /// request must not wait for a 300 MB fetch. A caller with no event loop — the C ABI, a
    /// terminal app — needs the fetch itself to come back and needs to hear about it as it
    /// goes.
    ///
    /// `progress` returns `false` to stop. The fetch is dropped at its next step and the
    /// `.part` files stay, so a later attempt resumes instead of starting again.
    ///
    /// # Errors
    ///
    /// An id the catalog does not hold, a fetch already in flight for it, or the
    /// downloader's own failure.
    pub fn download_blocking(
        &self,
        id: &str,
        mut progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<PathBuf, String> {
        let Some(spec) = self.manifest.find(id).cloned() else {
            return Err(format!("no model with id {id} in the catalog"));
        };
        let dir = self.models_dir.clone();
        if spec.is_installed(&dir) {
            return Ok(dir);
        }
        let total = spec.total_size();
        if self.try_start_download(id, total).is_some() {
            return Err(format!("{id} is already downloading"));
        }
        let started = std::time::Instant::now();
        let result = self.fetch_and_wait(&spec, &dir, total, &mut progress);
        self.finish_download(id, result.as_ref().err().map(String::as_str));
        match result {
            Ok(()) => {
                tracing::info!(
                    model = %id,
                    bytes = total,
                    ms = started.elapsed().as_millis(),
                    "model downloaded"
                );
                Ok(dir)
            }
            Err(error) => {
                tracing::error!(model = %id, %error, "model download failed");
                Err(error)
            }
        }
    }

    /// The fetch itself: one task, and a loop that reports what it has done.
    ///
    /// A blocking caller has no runtime of its own, so this builds one. The task is aborted
    /// when the callback says stop, which leaves the resumable `.part` files on disk.
    fn fetch_and_wait(
        &self,
        spec: &arioso_models::ModelSpec,
        dir: &Path,
        total: u64,
        progress: &mut impl FnMut(u64, u64) -> bool,
    ) -> Result<(), String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("could not start a runtime for the download: {error}"))?;
        let downloader = arioso_models::Downloader::with_options(
            self.config().models.mirror.clone(),
            self.config().models.ca_bundle.as_deref(),
        )
        .map_err(|error| error.to_string())?;
        let id = spec.id.clone();
        let spec = spec.clone();
        let dir = dir.to_path_buf();
        let seen = Arc::new(std::sync::Mutex::new((0u64, total)));
        let reported = Arc::clone(&seen);
        runtime.block_on(async {
            let task = tokio::spawn(async move {
                let mut report = move |p: arioso_models::Progress| {
                    if let Ok(mut slot) = reported.lock() {
                        *slot = (p.downloaded, p.total);
                    }
                };
                downloader
                    .ensure_model(&spec, &dir, &mut report)
                    .await
                    .map(|_paths| ())
                    .map_err(|error| error.to_string())
            });
            let abort = task.abort_handle();
            tokio::pin!(task);
            loop {
                tokio::select! {
                    finished = &mut task => {
                        return finished
                            .map_err(|error| format!("the download task failed: {error}"))?;
                    }
                    () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                        let (downloaded, total) = *seen
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        self.note_download(&id, downloaded, total);
                        // The callback's answer is "stop". Reading it the other way round
                        // ended every download at its first tick with "cancelled", which is
                        // exactly what a caller that asked to continue was told.
                        if progress(downloaded, total) {
                            abort.abort();
                            return Err("the download was cancelled".to_owned());
                        }
                    }
                }
            }
        })
    }

    /// Start the downloads the config asks for at start-up.
    ///
    /// A server that is told which models it needs should not wait for a caller to ask for
    /// them. An id that is not in the catalog, or that is already on disk, is skipped.
    pub fn spawn_auto_downloads(self: &Arc<Self>) {
        let wanted = self.config().models.auto_download.clone();
        for id in wanted {
            let Some(spec) = self.manifest.find(&id).cloned() else {
                tracing::warn!(model = %id, "auto_download names a model that is not in the catalog");
                continue;
            };
            if self.is_installed(&spec) {
                continue;
            }
            tracing::info!(model = %id, "auto_download started");
            self.begin_download(&spec);
        }
    }

    /// Record a download's progress, replacing what was there.
    fn note_download(&self, id: &str, downloaded: u64, total: u64) {
        if let Ok(mut map) = self.downloads.lock() {
            map.insert(id.to_owned(), (downloaded, total));
        }
        self.publish(ModelEvent {
            id: id.to_owned(),
            state: "downloading".to_owned(),
            downloaded: Some(downloaded),
            total,
            error: None,
        });
    }

    /// Forget a download that ended, whether it worked or not, and say how it ended.
    ///
    /// The last word on a download is whether its files are now on disk, not whether the
    /// task returned `Ok`: a fetch that failed after writing every file is installed.
    fn finish_download(&self, id: &str, error: Option<&str>) {
        if let Ok(mut map) = self.downloads.lock() {
            map.remove(id);
        }
        let Some(spec) = self.manifest.find(id) else {
            return;
        };
        let installed = self.is_installed(spec);
        self.publish(ModelEvent {
            id: id.to_owned(),
            state: if installed {
                "installed"
            } else {
                "not_installed"
            }
            .to_owned(),
            downloaded: installed.then(|| spec.total_size()),
            total: spec.total_size(),
            error: error.map(str::to_owned),
        });
    }

    /// Hand one event to every subscriber, if there are any.
    ///
    /// No receiver is not a failure: it is the ordinary case of a download nobody watches.
    fn publish(&self, event: ModelEvent) {
        let _ = self.events.send(event);
    }

    /// What `GET /v1/models` and `GET /v1/models/events` both say about one model.
    ///
    /// One function for both, so the polled view and the streamed one cannot disagree
    /// about what a model's state is called.
    fn model_event(&self, spec: &ModelSpec) -> ModelEvent {
        let installed = self.is_installed(spec);
        let progress = self.download_progress(&spec.id);
        let state = model_state(installed, progress.is_some());
        ModelEvent {
            id: spec.id.clone(),
            state: state.to_owned(),
            downloaded: match (installed, progress) {
                (true, _) => Some(spec.total_size()),
                (false, Some((downloaded, _))) => Some(downloaded),
                (false, None) => None,
            },
            total: progress.map_or_else(|| spec.total_size(), |(_, total)| total),
            error: None,
        }
    }

    /// Every model's current state, as the first event of a stream.
    fn model_snapshot(&self) -> Vec<ModelEvent> {
        self.manifest
            .models
            .iter()
            .map(|spec| self.model_event(spec))
            .collect()
    }

    /// What a download has reported so far, if one is running.
    fn download_progress(&self, id: &str) -> Option<(u64, u64)> {
        self.downloads.lock().ok()?.get(id).copied()
    }

    /// The accept loop's handle, when this process is serving.
    pub(crate) fn handle(&self) -> Option<&axum_server::Handle<std::net::SocketAddr>> {
        self.handle.get()
    }

    /// Give the state the accept loop's handle, so a route can stop the server.
    pub fn attach_handle(&self, handle: axum_server::Handle<std::net::SocketAddr>) {
        let _ = self.handle.set(handle);
    }

    /// A permit for one streaming session, or `None` when every permit is in use.
    pub(crate) fn open_session(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        Arc::clone(&self.sessions).try_acquire_owned().ok()
    }

    /// The bearer token this process requires, when one was configured.
    pub(crate) fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// The address a request is charged to.
    ///
    /// `X-Forwarded-For` is only believed when `server.trust_forwarded_for` is on, because
    /// a header any client can set is not an identity. Without a peer address the request
    /// is charged to the unspecified address: a shared bucket, so a client that somehow
    /// arrives without an address is still limited rather than exempt.
    fn client_address(
        &self,
        headers: &axum::http::HeaderMap,
        extensions: &axum::http::Extensions,
    ) -> std::net::IpAddr {
        let unknown = std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
        if self.config().server.trust_forwarded_for {
            if let Some(forwarded) = headers
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(',').next())
                .and_then(|value| value.trim().parse().ok())
            {
                return forwarded;
            }
        }
        extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map_or(unknown, |info| info.0.ip())
    }

    /// The config, as a shared handle.
    ///
    /// Cloning an `Arc` is one atomic increment. Cloning the `Config` was a deep copy of
    /// every string and path in it, several times per request.
    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .map(|guard| Arc::clone(&guard))
            .unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner()))
    }

    /// Drop the loaded voice, so the next request loads the one the config now names.
    fn drop_voice(&self) {
        #[cfg(test)]
        self.voice_drops
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut guard) = self.voice.lock() {
            *guard = None;
        }
    }

    /// Apply a patch's change to the loaded engine.
    ///
    /// One Kokoro graph serves every voice in the archive: the 325 MB model, the symbol
    /// table and the session are identical for all 54, and only the 522 KB style table
    /// differs. Dropping the slot for a voice change therefore re-reads and re-compiles
    /// the whole graph to arrive at a different row of a table already in memory, so a
    /// Kokoro style is swapped where it is. A Piper voice *is* a model, so a new name
    /// there is a new graph and the slot is dropped as before.
    ///
    /// Nothing here decides whether the name is speakable: `validate` has already refused
    /// one the archive does not hold, before any of the patch was applied.
    fn retarget_voice(&self, change: VoiceChange) {
        match change {
            VoiceChange::None => {}
            VoiceChange::Reload => self.drop_voice(),
            VoiceChange::Style(name) => match self.swap_style(&name) {
                // The engine in the slot took the new style, or the slot was empty and
                // the next request loads what the config names.
                Swapped::InPlace | Swapped::Empty => {}
                // A Piper voice is one model per voice, so a new name is a new graph; and
                // an archive that no longer holds the name must not be spoken under it.
                Swapped::OtherEngine | Swapped::Refused => self.drop_voice(),
            },
        }
    }

    /// Swap the style table on the loaded Kokoro voice.
    ///
    /// The lock is released before the caller acts on the answer.
    fn swap_style(&self, name: &str) -> Swapped {
        #[cfg(test)]
        self.style_swaps
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut guard = match self.voice.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.as_mut() {
            Some(Voice::Kokoro(voice)) => match voice.select_voice(name) {
                Ok(()) => Swapped::InPlace,
                Err(error) => {
                    // The archive is the authority on which styles exist and the patch was
                    // checked against it, so this is the archive changing under a loaded
                    // engine. Speaking the old style under the new name is the one outcome
                    // not allowed: the caller drops the slot, and the next request refuses
                    // by naming what the archive holds.
                    tracing::warn!(%error, "the loaded voice could not be switched");
                    Swapped::Refused
                }
            },
            Some(Voice::Piper(_)) => Swapped::OtherEngine,
            None => Swapped::Empty,
        }
    }

    /// Drop the loaded batch transcription model.
    fn drop_batch_stt(&self) {
        if let Ok(mut guard) = self.stt.lock() {
            *guard = None;
        }
    }

    /// Drop the loaded streaming model.
    fn drop_stream_stt(&self) {
        if let Ok(mut guard) = self.stt_stream.lock() {
            *guard = None;
        }
    }

    /// Drop both loaded transcription models.
    fn drop_stt(&self) {
        self.drop_batch_stt();
        self.drop_stream_stt();
    }

    /// Whether a speech model is in memory, in either slot.
    ///
    /// The batch route loads the batch slot and the socket loads the streaming one, so a
    /// readiness answer that read only the batch slot called a warm socket cold.
    fn stt_loaded(&self) -> bool {
        let batch = self
            .stt
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false);
        let stream = self
            .stt_stream
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false);
        batch || stream
    }

    /// Replace the config.
    fn set_config(&self, config: Config) {
        let next = Arc::new(config);
        match self.config.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// The catalog entry the config asks for.
    ///
    /// A Piper voice is one entry per voice, so the voice id resolves it. Kokoro is one
    /// entry holding every voice style, so the entry is chosen by engine and
    /// `tts.voice` names a row inside its archive.
    fn voice_spec(&self) -> Result<&ModelSpec, ServerError> {
        let config = self.config();
        let voice = config.voice_name().to_string();
        let found = if config.tts.engine == "kokoro" {
            self.manifest
                .models
                .iter()
                .find(|model| model.engine == "kokoro" && model.kind == Kind::Tts)
        } else {
            self.manifest.find_voice(&voice)
        };
        found.ok_or(ServerError::NoVoice(voice))
    }

    /// The voice readiness reports, and whether it can be spoken.
    ///
    /// A Piper voice is a catalog id, so presence is its files on disk. A Kokoro voice is
    /// a style inside the archive, so presence is the archive indexing that name: the
    /// model can be installed and the configured style absent, and a server in that state
    /// cannot speak. The name reported is the one that was checked.
    fn ready_voice(&self) -> Option<(String, bool)> {
        let config = self.config();
        if !config.tts.enabled {
            return None;
        }
        let Ok(spec) = self.voice_spec() else {
            // A voice the catalog does not hold is not installed, and naming it is what
            // tells the caller which value to change.
            return Some((config.voice_name().to_string(), false));
        };
        if config.tts.engine == "kokoro" {
            let named = config.voice_name();
            let present = self.is_installed(spec)
                && archive_names(&self.models_dir, spec)
                    .is_some_and(|names| names.contains(&named.to_string()));
            return Some((named.to_string(), present));
        }
        Some((spec.id.clone(), self.is_installed(spec)))
    }

    /// The style a requested voice means, when it names the model instead of a style.
    ///
    /// A catalog lists files, and a front end that echoes a row back sends the id it was
    /// given. For an engine whose voices live inside one file, that id is not a voice: it is
    /// the file. The engine's own default style is what such a caller meant.
    fn style_for(requested: &str, model_id: &str, engine: &str) -> Option<&'static str> {
        if requested != model_id {
            return None;
        }
        arioso_config::default_voice(engine)
    }

    /// Load the configured engine into the one voice slot.
    ///
    /// One engine per process: the slot holds whichever `tts.engine` names, and a
    /// caller that changes it reloads rather than running both.
    fn load_voice(&self) -> Result<Voice, TtsFailure> {
        let spec = self
            .voice_spec()
            .map_err(|error| TtsFailure::Client(error.to_string()))?;
        let config = self.config();
        if config.tts.engine == "kokoro" {
            let model = self.file_of(spec, ".onnx").map_err(TtsFailure::Engine)?;
            let voices = self.file_of(spec, ".bin").map_err(TtsFailure::Engine)?;
            let symbols = self
                .file_of(spec, "config.json")
                .map_err(TtsFailure::Engine)?;
            let mut voice = config.voice_name().to_string();
            // A front end that lists the catalog and sends back what it found sends the model
            // id: `kokoro-82m` names a file, and no style in the archive answers to it. The
            // model's own default style is what that caller meant, and the log says so rather
            // than making the substitution in silence.
            if let Some(style) = Self::style_for(&voice, &spec.id, &config.tts.engine) {
                tracing::warn!(
                    requested = %voice,
                    using = style,
                    "a model id is not a style; using the engine default"
                );
                voice = style.to_owned();
            }
            let loaded =
                KokoroVoice::load(&model, &voices, &symbols, &voice).map_err(
                    |error| match error {
                        // The archive does not hold the configured style. The model is fine;
                        // the config names something that is not there.
                        KokoroError::UnknownVoice { .. } => TtsFailure::Client(error.to_string()),
                        other => TtsFailure::Engine(other.to_string()),
                    },
                )?;
            Ok(Voice::Kokoro(loaded))
        } else {
            let onnx = self.file_of(spec, ".onnx").map_err(TtsFailure::Engine)?;
            let symbols = self
                .file_of(spec, ".onnx.json")
                .map_err(TtsFailure::Engine)?;
            let loaded = PiperVoice::load(&onnx, &symbols)
                .map_err(|error| TtsFailure::Engine(error.to_string()))?;
            Ok(Voice::Piper(loaded))
        }
    }

    /// Every engine this binary can serve, with what it can do.
    ///
    /// A disabled engine is absent rather than reported as unavailable: its routes do
    /// not exist either, so a client that reads this list sees exactly what it can call.
    fn engines(&self) -> Vec<EngineView> {
        let mut engines = Vec::new();
        let config = self.config();
        if config.tts.enabled {
            let loaded = self
                .voice
                .lock()
                .map(|guard| guard.as_ref().map(Voice::engine))
                .unwrap_or(None);
            for engine in TTS_ENGINES {
                engines.push(EngineView {
                    id: engine.to_string(),
                    kind: "tts".to_string(),
                    streaming: true,
                    languages: self.catalog_languages(engine),
                    models: self.catalog_count(engine),
                    loaded: loaded == Some(engine),
                });
            }
        }
        if config.stt.enabled {
            engines.push(EngineView {
                id: "sherpa-whisper".to_string(),
                kind: "stt".to_string(),
                streaming: true,
                languages: self.catalog_languages("sherpa"),
                models: self.catalog_count("sherpa"),
                loaded: self.stt_loaded(),
            });
        }
        engines
    }

    /// Languages the catalog has a model for, on one engine.
    fn catalog_languages(&self, engine: &str) -> Vec<String> {
        let mut languages: Vec<String> = self
            .manifest
            .models
            .iter()
            .filter(|model| model.engine == engine)
            // meta is free-form manifest data, so the language list is read as JSON.
            .flat_map(|model| {
                model
                    .meta
                    .get("languages")
                    .and_then(|value| value.as_array())
                    .map(|list| {
                        list.iter()
                            .filter_map(|entry| entry.as_str().map(str::to_owned))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .collect();
        languages.sort();
        languages.dedup();
        languages
    }

    /// How many catalog models belong to one engine.
    fn catalog_count(&self, engine: &str) -> usize {
        self.manifest
            .models
            .iter()
            .filter(|model| model.engine == engine)
            .count()
    }

    fn stt_spec(&self) -> Result<&ModelSpec, ServerError> {
        let config = self.config();
        let model = &config.stt.batch_model;
        self.manifest
            .models
            .iter()
            .filter(|m| m.kind == Kind::Stt && m.engine == "sherpa")
            .find(|m| &m.id == model)
            .ok_or_else(|| ServerError::NoSttModel(model.clone()))
    }

    /// The `meta.model_type` of a catalog entry.
    fn model_type(spec: &ModelSpec) -> Option<&str> {
        spec.meta
            .get("model_type")
            .and_then(serde_json::Value::as_str)
    }

    /// The streaming model the config names, when the catalog knows it and it streams.
    ///
    /// `stt.stream_model` is the model the socket is documented to use. A config that
    /// leaves it empty gets `None`, and the socket keeps the batch path over
    /// `stt.batch_model` exactly as it always did. A non-empty value is checked by
    /// [`stream_model_problem`] at startup and on PATCH, so a name that does not resolve
    /// to a streaming entry is refused rather than silently falling back.
    pub(crate) fn stream_spec(&self) -> Option<&ModelSpec> {
        stream_spec_in(&self.config(), &self.manifest)
    }

    /// The file of a model, found by its manifest id.
    ///
    /// A detector is fetched and checked like any other model, so the path comes from the
    /// catalog rather than from a config field that could name a file the downloader never
    /// wrote.
    pub(crate) fn model_file(&self, id: &str, suffix: &str) -> Result<PathBuf, String> {
        let spec = self
            .manifest
            .models
            .iter()
            .find(|model| model.id == id)
            .ok_or_else(|| format!("{id} is not in this catalog"))?;
        self.file_of(spec, suffix)
    }

    /// Whether a model is on disk, by manifest id.
    pub(crate) fn model_installed(&self, id: &str) -> bool {
        self.manifest
            .models
            .iter()
            .find(|model| model.id == id)
            .is_some_and(|spec| self.is_installed(spec))
    }

    /// Whether the socket reads partials from the model's own state.
    ///
    /// The family comes from the catalog, not from the request: one model is loaded per
    /// slot, so the choice cannot change mid-session.
    pub(crate) fn stt_streams(&self) -> bool {
        self.stream_spec().is_some()
    }

    /// Start the streaming model's state for one utterance.
    ///
    /// The session owns a share of the recognizer, so the lock is held only long enough to
    /// create it: an utterance that runs for ten seconds must not keep every other caller
    /// out of the engine slot.
    pub(crate) fn stream_session(&self) -> Result<arioso_stt_sherpa::StreamSession, SttFailure> {
        let mut guard = self
            .stt_stream
            .lock()
            .map_err(|_| SttFailure::Engine("the streaming model lock is poisoned".to_string()))?;
        if guard.is_none() {
            let spec = self.stream_spec().ok_or_else(|| {
                SttFailure::Engine("no streaming model is configured".to_string())
            })?;
            if !self.is_installed(spec) {
                return Err(SttFailure::NotInstalled(spec.id.clone()));
            }
            *guard = Some(self.build_engine(spec)?);
        }
        guard
            .as_ref()
            .and_then(arioso_stt_sherpa::Engine::streaming)
            .map(arioso_stt_sherpa::StreamingEngine::session)
            .ok_or_else(|| SttFailure::Engine("the streaming model did not load".to_string()))
    }

    /// Transcribe WAV bytes, loading the engine on first use.
    fn transcribe(&self, bytes: &[u8]) -> Result<(String, f64), SttFailure> {
        let (samples, rate) = wav::read_bytes(bytes)
            .map_err(|error| SttFailure::Audio(format!("reading the audio: {error}")))?;
        let seconds = if rate == 0 {
            0.0
        } else {
            samples.len() as f64 / f64::from(rate)
        };
        let max_audio_s = self.config().stt.max_audio_s;
        if seconds > f64::from(max_audio_s) {
            return Err(SttFailure::Audio(format!(
                "the audio is {seconds:.0} s, longer than stt.max_audio_s of {max_audio_s} s"
            )));
        }

        let mut guard = self
            .stt
            .lock()
            .map_err(|_| SttFailure::Engine("the speech to text lock is poisoned".to_string()))?;
        let engine = self.ensure_engine(&mut guard)?;
        let text = self.hear(engine, &samples, rate)?;
        Ok((text, seconds))
    }

    /// Label the voices in one recording.
    ///
    /// The whole file is read before any answer is given, because the clustering needs
    /// every embedding at once. A long recording therefore costs its length in seconds
    /// before the first turn comes back.
    pub fn diarize(
        &self,
        bytes: &[u8],
    ) -> Result<(Vec<arioso_stt_sherpa::diarize::Turn>, f64), DiarizeFailure> {
        let (samples, rate) = wav::read_bytes(bytes)
            .map_err(|error| DiarizeFailure::Audio(format!("reading the audio: {error}")))?;
        if samples.is_empty() {
            return Err(DiarizeFailure::Audio("the audio is empty".to_string()));
        }
        self.diarize_samples(&samples, rate)
    }

    /// Label the voices in samples already in hand, for callers that decode their own audio.
    ///
    /// `rate` is the rate of `samples`, and it must be the rate the models read: a mismatch
    /// is refused rather than resampled, because resampling silently scales every timestamp.
    pub fn diarize_samples(
        &self,
        samples: &[f32],
        rate: u32,
    ) -> Result<(Vec<arioso_stt_sherpa::diarize::Turn>, f64), DiarizeFailure> {
        if samples.is_empty() {
            return Err(DiarizeFailure::Audio("the audio is empty".to_string()));
        }
        let seconds = samples.len() as f64 / f64::from(rate).max(1.0);
        if seconds > f64::from(self.config().stt.max_audio_s) {
            return Err(DiarizeFailure::Audio(format!(
                "the audio is {seconds:.0} s, longer than stt.max_audio_s of {} s",
                self.config().stt.max_audio_s
            )));
        }

        let mut guard = self
            .diarizer
            .lock()
            .map_err(|_| DiarizeFailure::Engine("the diarization lock is poisoned".to_string()))?;
        if guard.is_none() {
            *guard = Some(self.build_diarizer()?);
        }
        let diarizer = guard
            .as_ref()
            .ok_or_else(|| DiarizeFailure::Engine("the models failed to load".to_string()))?;
        let turns = diarizer
            .run(samples, rate)
            .map_err(|error| DiarizeFailure::Audio(error.to_string()))?;
        Ok((turns, seconds))
    }

    /// The turns as one JSON document.
    ///
    /// The route, the command line and the C ABI all read this shape, so it is built in one
    /// place: a front end that parses a second copy of a document is a front end that breaks
    /// when only one copy changes.
    pub fn diarize_json(&self, samples: &[f32], rate: u32) -> Result<String, DiarizeFailure> {
        let (turns, seconds) = self.diarize_samples(samples, rate)?;
        serde_json::to_string(&DiarizeResponse::from_turns(turns, seconds))
            .map_err(|error| DiarizeFailure::Engine(error.to_string()))
    }

    /// Load the models the config names, into a fresh diarizer.
    fn build_diarizer(&self) -> Result<arioso_stt_sherpa::diarize::Diarizer, DiarizeFailure> {
        let config = self.config();
        let id = config.diarize.model.trim().to_string();
        let spec = self
            .manifest
            .models
            .iter()
            .find(|model| model.id == id)
            .ok_or_else(|| DiarizeFailure::Audio(format!("{id} is not in this catalog")))?;
        if !self.is_installed(spec) {
            return Err(DiarizeFailure::NotInstalled(id));
        }
        // Two files, so each is named rather than matched by suffix: both are `.onnx`, and
        // a suffix lookup would hand the segmentation model in twice.
        let file = |suffix: &str| {
            spec.files
                .iter()
                .find(|file| file.name.contains(suffix))
                .map(|file| self.models_dir.join(&file.name))
                .ok_or_else(|| DiarizeFailure::Engine(format!("{id} has no {suffix} file")))
        };
        arioso_stt_sherpa::diarize::Diarizer::load(
            &file("segmentation")?,
            &file("embedding")?,
            config.diarize.speakers,
            config.diarize.threshold,
        )
        .map_err(|error| DiarizeFailure::Engine(error.to_string()))
    }

    /// Transcribe samples already in hand, for the streaming path.
    pub(crate) fn transcribe_samples(
        &self,
        samples: &[f32],
        rate: u32,
    ) -> Result<String, SttFailure> {
        if samples.is_empty() {
            return Err(SttFailure::Audio("the audio is empty".to_string()));
        }
        let mut guard = self
            .stt
            .lock()
            .map_err(|_| SttFailure::Engine("the speech to text lock is poisoned".to_string()))?;
        let engine = self.ensure_engine(&mut guard)?;
        self.hear(engine, samples, rate)
    }

    /// Load the transcription model without transcribing anything.
    ///
    /// The same slot the routes use, so warming is not a second engine that the first
    /// real request would then have to replace.
    pub(crate) fn warm_stt(&self) -> Result<(), String> {
        let mut guard = self
            .stt
            .lock()
            .map_err(|_| "the speech to text lock is poisoned".to_string())?;
        self.ensure_engine(&mut guard)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Load the streaming model without opening a session.
    ///
    /// The socket's slot, not the batch one: warming `stt.stream_model` must not leave the
    /// first socket to pay for the load, and must not displace the model the file route
    /// uses.
    pub(crate) fn warm_stream(&self) -> Result<(), String> {
        let mut guard = self
            .stt_stream
            .lock()
            .map_err(|_| "the streaming model lock is poisoned".to_string())?;
        if guard.is_none() {
            let spec = self
                .stream_spec()
                .ok_or_else(|| "no streaming model is configured".to_string())?;
            if !self.is_installed(spec) {
                return Err(SttFailure::NotInstalled(spec.id.clone()).to_string());
            }
            *guard = Some(self.build_engine(spec).map_err(|error| error.to_string())?);
        }
        Ok(())
    }

    /// The engine, loaded on first use.
    fn ensure_engine<'a>(
        &'a self,
        slot: &'a mut Option<arioso_stt_sherpa::Engine>,
    ) -> Result<&'a arioso_stt_sherpa::Engine, SttFailure> {
        if slot.is_none() {
            let spec = self
                .stt_spec()
                .map_err(|e| SttFailure::Engine(e.to_string()))?;
            if !self.is_installed(spec) {
                return Err(SttFailure::NotInstalled(spec.id.clone()));
            }
            *slot = Some(self.build_engine(spec)?);
        }
        slot.as_ref()
            .ok_or_else(|| SttFailure::Engine("the engine failed to load".to_string()))
    }

    /// Load the model a catalog entry names, into a fresh engine.
    ///
    /// The manifest names the files and the family, so a directory holding two models still
    /// loads the one the config asks for.
    fn build_engine(&self, spec: &ModelSpec) -> Result<arioso_stt_sherpa::Engine, SttFailure> {
        let config = self.config();
        let configured = &config.stt.language;
        let language = if configured == "auto" {
            None
        } else {
            Some(configured.as_str())
        };
        let threads =
            std::thread::available_parallelism().map_or(4, |count| (count.get() / 2).clamp(1, 8));
        let paths: Vec<std::path::PathBuf> = spec
            .files
            .iter()
            .map(|f| self.models_dir.join(&f.name))
            .collect();
        let model_type = Self::model_type(spec);
        arioso_stt_sherpa::Engine::from_manifest_files(
            &self.models_dir,
            &paths,
            model_type,
            language,
            threads,
        )
        .map_err(|e| SttFailure::Engine(format!("loading {}: {e}", spec.id)))
    }

    /// Transcribe samples and run the dictionary over the result.
    fn hear(
        &self,
        engine: &arioso_stt_sherpa::Engine,
        samples: &[f32],
        rate: u32,
    ) -> Result<String, SttFailure> {
        let transcript = engine
            .transcribe(samples, rate)
            .map_err(|e| SttFailure::Engine(e.to_string()))?;
        Ok(self.correct(&transcript.text))
    }

    /// Spell the words the way the user taught them.
    ///
    /// The dictionary runs backwards as well, compared by sound rather than by letters,
    /// because the engine writes "Tonseller" for "tom seller". Finals get this; a
    /// provisional partial does not, because it is replaced a moment later and correcting
    /// it would only make the line flicker.
    pub(crate) fn correct(&self, text: &str) -> String {
        let user = self.user_snapshot();
        user.reverse.apply(&self.dictionary, text)
    }

    /// Transcribe for a partial hypothesis, or give up if the engine is busy.
    ///
    /// A partial is worth less than a final, so it never waits: a missed partial costs
    /// one interval, a queued one delays the final.
    pub(crate) fn try_transcribe_samples(&self, samples: &[f32], rate: u32) -> Option<String> {
        if samples.is_empty() {
            return None;
        }
        let mut guard = self.stt.try_lock().ok()?;
        let engine = self.ensure_engine(&mut guard).ok()?;
        self.hear(engine, samples, rate).ok()
    }

    /// The user dictionary, reloaded if the file changed since the last look.
    ///
    /// Answers a snapshot and releases the lock before returning. A guard held for the length
    /// of a render was the reason a transcription waited behind one, and the reverse: both
    /// read this dictionary, and neither writes it.
    fn user_snapshot(&self) -> UserSnapshot {
        let state = self.lock_user();
        UserSnapshot {
            dictionary: Arc::clone(&state.dictionary),
            reverse: Arc::clone(&state.reverse),
            error: state.error.clone(),
        }
    }

    /// The dictionary state, with the reload applied if the file changed.
    fn lock_user(&self) -> std::sync::MutexGuard<'_, UserState> {
        let mut state = self
            .user
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let modified = std::fs::metadata(&state.path)
            .and_then(|meta| meta.modified())
            .ok();
        if modified == state.modified {
            return state;
        }
        state.modified = modified;

        if modified.is_none() {
            // The file was deleted, so the shipped dictionary is what applies again.
            state.dictionary = Arc::new(UserDictionary::shipped());
            state.reverse = Arc::new(ReverseIndex::build(&state.dictionary, &self.dictionary));
            state.error = None;
            return state;
        }

        match UserDictionary::load(&state.path) {
            Ok(dictionary) => {
                tracing::info!(entries = dictionary.len(), "reloaded the user dictionary");
                state.dictionary = Arc::new(dictionary);
                state.reverse = Arc::new(ReverseIndex::build(&state.dictionary, &self.dictionary));
                state.error = None;
            }
            Err(error) => {
                // Keep the last good copy: a typo must not break speech.
                tracing::warn!(%error, "user dictionary did not reload");
                state.error = Some(error.to_string());
            }
        }
        state
    }

    /// Whether every file of `spec` is present at the recorded size.
    fn is_installed(&self, spec: &ModelSpec) -> bool {
        spec.is_installed(&self.models_dir)
    }

    fn file_of(&self, spec: &ModelSpec, suffix: &str) -> Result<PathBuf, String> {
        spec.files
            .iter()
            .find(|f| f.name.ends_with(suffix))
            .map(|f| self.models_dir.join(&f.name))
            .ok_or_else(|| format!("voice {} has no {suffix} file", spec.id))
    }

    /// Synthesize text, loading the voice on first use.
    ///
    /// The document is collected as 16-bit PCM, converted chunk by chunk as it arrives.
    /// Holding it as `f32` and converting at the end kept four bytes a sample alive for
    /// the length of the render and then allocated the WAV beside it.
    fn render(&self, text: &str, speed: f32, sid: i64) -> Result<(Vec<u8>, u32), String> {
        let mut rate = 0;
        let mut pcm: Vec<u8> = Vec::new();
        self.render_stream(text, speed, sid, |chunk, chunk_rate| {
            rate = chunk_rate;
            wav::write_pcm_s16le(chunk, &mut pcm);
            Ok(())
        })?;
        if pcm.is_empty() {
            return Err("nothing to say: none of the words are in the dictionary".into());
        }
        Ok((pcm, rate))
    }

    /// Load the configured voice without rendering anything.
    ///
    /// The first synthesis pays for the model load; a caller that knows it is about to
    /// ask should be able to pay it earlier. Idempotent: a loaded voice is left alone.
    pub(crate) fn warm_voice(&self) -> Result<(), TtsFailure> {
        let mut guard = self
            .voice
            .lock()
            .map_err(|_| TtsFailure::Engine("the voice lock is poisoned".to_string()))?;
        if guard.is_some() {
            return Ok(());
        }
        *guard = Some(self.load_voice()?);
        Ok(())
    }

    /// Check a speaker number against the configured voice.
    ///
    /// A speaker the voice does not have is the caller's mistake, not an engine failure,
    /// so it is checked before the render and answered with a code a client can switch on.
    ///
    /// Public because the C ABI answers a bad speaker with a distinct code, and it must ask
    /// the same question the HTTP and WebSocket routes ask rather than a copy of it.
    pub fn check_sid(&self, sid: i64) -> Result<(), SidProblem> {
        self.warm_voice()
            .map_err(|error| SidProblem::Unavailable(error.to_string()))?;
        let guard = self
            .voice
            .lock()
            .map_err(|_| SidProblem::Unavailable("the voice lock is poisoned".to_string()))?;
        match guard.as_ref() {
            Some(Voice::Piper(voice)) => voice
                .check_speaker(sid)
                .map_err(|error| SidProblem::NoSuchSpeaker(error.to_string())),
            // A Kokoro voice style is one speaker, and the style is the whole choice.
            Some(Voice::Kokoro(_)) if sid == 0 => Ok(()),
            Some(Voice::Kokoro(_)) => Err(SidProblem::NoSuchSpeaker(format!(
                "this voice has a single speaker, so speaker {sid} does not exist"
            ))),
            None => Err(SidProblem::Unavailable("voice failed to load".to_string())),
        }
    }

    /// Synthesize text one chunk at a time, handing each to `sink`.
    ///
    /// The sink runs while later chunks are still being synthesized, so a
    /// caller can send audio before the whole text is done.
    pub(crate) fn render_stream(
        &self,
        text: &str,
        speed: f32,
        sid: i64,
        mut sink: impl FnMut(&[f32], u32) -> Result<(), String>,
    ) -> Result<(), String> {
        // Every path to a render passes here, so the check lives here rather than at each
        // caller. A voice that does not have speaker `sid` refuses it instead of speaking
        // with the one it does have, which a caller could not tell apart from success.
        self.check_sid(sid).map_err(|error| error.to_string())?;
        let mut guard = self
            .voice
            .lock()
            .map_err(|_| "the voice lock is poisoned".to_string())?;
        if guard.is_none() {
            *guard = Some(self.load_voice().map_err(|error| error.to_string())?);
        }
        let voice = guard.as_mut().ok_or("voice failed to load")?;
        // Taken before the render and dropped with it: the snapshot holds `Arc`s, so the
        // dictionary lock is free for a transcription the whole time this speaks.
        let user = self.user_snapshot();
        let normalize = arioso_core::NormalizeOptions {
            time_of_day: self.config().tts.time_of_day,
        };
        // Both engines report the same three lists; the match ends here so the log
        // below is written once rather than twice.
        let (guessed, missing, unusable) = match voice {
            Voice::Piper(voice) => {
                let report = voice
                    .render(
                        text,
                        &self.dictionary,
                        &user.dictionary,
                        arioso_tts_piper::RenderOptions {
                            speed,
                            normalize,
                            sid,
                        },
                        |audio, rate| sink(audio, rate).map_err(PiperError::Consumer),
                    )
                    .map_err(|error| error.to_string())?;
                (report.guessed, report.missing, report.unusable)
            }
            Voice::Kokoro(voice) => {
                let report = voice
                    .render(
                        text,
                        &self.dictionary,
                        &user.dictionary,
                        arioso_tts_kokoro::RenderOptions { speed, normalize },
                        |audio, rate| sink(audio, rate).map_err(KokoroError::Consumer),
                    )
                    .map_err(|error| error.to_string())?;
                (report.guessed, report.missing, report.unusable)
            }
        };
        // The words themselves are text, and F9.9 says text is not logged unless the
        // operator turned `log.log_text` on. The counts are not text, so they always go.
        let log_text = self.config().log.log_text;
        if !guessed.is_empty() {
            if log_text {
                tracing::debug!(guessed = ?guessed, "words spoken by spelling rules");
            } else {
                tracing::debug!(count = guessed.len(), "words spoken by spelling rules");
            }
        }
        if !missing.is_empty() {
            if log_text {
                tracing::debug!(missing = ?missing, "words nothing could pronounce");
            } else {
                tracing::debug!(count = missing.len(), "words nothing could pronounce");
            }
        }
        if !unusable.is_empty() {
            tracing::warn!(entries = ?unusable, "user dictionary entries the voice cannot say");
        }
        Ok(())
    }

    /// Synthesize text to WAV bytes, for the C ABI.
    ///
    /// The route builds its own body because the config chooses between WAV and raw PCM. A
    /// C caller reads one shape, so this always answers a WAV, whatever `tts.format` says.
    ///
    /// # Errors
    ///
    /// As the internal render: the voice failed to load, or the text has nothing the voice
    /// can say.
    pub fn synthesize_wav(&self, text: &str, speed: f32, sid: i64) -> Result<Vec<u8>, String> {
        let (pcm, rate) = self.render(text, speed, sid)?;
        Ok(wav::to_bytes_from_pcm(&pcm, rate))
    }

    /// Synthesize one chunk at a time, for the C ABI's audio callback.
    ///
    /// A `sink` that returns an error stops the render at the next chunk boundary: both
    /// engines propagate the error out of their own loop, so a cancelled stream stops
    /// synthesizing instead of finishing into a buffer nobody reads.
    ///
    /// # Errors
    ///
    /// The sink's own message, or the engine's.
    pub fn synthesize_stream(
        &self,
        text: &str,
        speed: f32,
        sid: i64,
        sink: impl FnMut(&[f32], u32) -> Result<(), String>,
    ) -> Result<(), String> {
        self.render_stream(text, speed, sid, sink)
    }

    /// The phonemes a text will be spoken as, as the loaded voice builds them.
    ///
    /// The voice is asked rather than the text-to-phoneme rules, because each engine drops
    /// or joins symbols in its own way: a display built from the rules could show a line the
    /// voice would never say.
    ///
    /// # Errors
    ///
    /// A voice that would not load, or a text with nothing in it.
    pub fn phonemes_for(&self, text: &str) -> Result<String, String> {
        if text.trim().is_empty() {
            return Err("the text is empty".to_string());
        }
        self.warm_voice().map_err(|error| error.to_string())?;
        let guard = self
            .voice
            .lock()
            .map_err(|_| "the voice lock is poisoned".to_string())?;
        let voice = guard.as_ref().ok_or("voice failed to load")?;
        let user = self.user_snapshot();
        let normalize = arioso_core::NormalizeOptions {
            time_of_day: self.config().tts.time_of_day,
        };
        // Each engine reports in its own type, and the caller wants the line, so the
        // reports are dropped where they are made rather than unified.
        let symbols = match voice {
            Voice::Piper(voice) => {
                voice
                    .phonemes_for(
                        text,
                        &self.dictionary,
                        &user.dictionary,
                        arioso_tts_piper::RenderOptions {
                            speed: 1.0,
                            normalize,
                            sid: 0,
                        },
                    )
                    .0
            }
            Voice::Kokoro(voice) => {
                voice
                    .phonemes_for(
                        text,
                        &self.dictionary,
                        &user.dictionary,
                        arioso_tts_kokoro::RenderOptions {
                            speed: 1.0,
                            normalize,
                        },
                    )
                    .0
            }
        };
        Ok(symbols.concat())
    }

    /// Synthesize phonemes a caller supplied, one chunk at a time.
    ///
    /// The symbols are taken as written. A symbol the voice cannot say is refused by name:
    /// a caller that asked for a sound and heard a different word has no way to notice.
    ///
    /// # Errors
    ///
    /// A character that is not a phoneme, a symbol this voice has no id for, or the
    /// engine's own failure.
    pub fn render_phonemes_stream(
        &self,
        phonemes: &str,
        speed: f32,
        sid: i64,
        mut sink: impl FnMut(&[f32], u32) -> Result<(), String>,
    ) -> Result<(), String> {
        let symbols = parse_phonemes(phonemes)?;
        self.check_sid(sid).map_err(|error| error.to_string())?;
        let mut guard = self
            .voice
            .lock()
            .map_err(|_| "the voice lock is poisoned".to_string())?;
        if guard.is_none() {
            *guard = Some(self.load_voice().map_err(|error| error.to_string())?);
        }
        let voice = guard.as_mut().ok_or("voice failed to load")?;
        match voice {
            Voice::Piper(voice) => {
                voice.set_speed(speed);
                voice
                    .render_symbols(&symbols, sid, |audio, rate| {
                        sink(audio, rate).map_err(arioso_tts_piper::PiperError::Consumer)
                    })
                    .map_err(|error| error.to_string())
            }
            Voice::Kokoro(voice) => voice
                .render_symbols(&symbols, speed, |audio, rate| {
                    sink(audio, rate).map_err(arioso_tts_kokoro::KokoroError::Consumer)
                })
                .map_err(|error| error.to_string()),
        }
    }

    /// Synthesize phonemes to WAV bytes, for callers that want one buffer.
    ///
    /// # Errors
    ///
    /// As [`AppState::render_phonemes_stream`].
    pub fn synthesize_phonemes_wav(
        &self,
        phonemes: &str,
        speed: f32,
        sid: i64,
    ) -> Result<Vec<u8>, String> {
        let mut samples: Vec<f32> = Vec::new();
        let mut rate = 0u32;
        self.render_phonemes_stream(phonemes, speed, sid, |audio, at| {
            rate = at;
            samples.extend_from_slice(audio);
            Ok(())
        })?;
        Ok(wav::to_bytes_from_pcm(&wav::pcm_s16le(&samples), rate))
    }

    /// The catalog as a client reads it: what each model is, what it costs in bytes, what
    /// its licence allows, and whether it is on disk.
    ///
    /// One implementation for the route, the C ABI and any other front end: three copies of
    /// this loop would disagree about what `installed` means within a release.
    #[must_use]
    pub fn models_view(&self) -> Vec<ModelView> {
        self.manifest
            .models
            .iter()
            .map(|m| {
                let installed = self.is_installed(m);
                // A caller that started a download polls this. Reporting only `installed`
                // made it re-POST to find out whether anything was happening.
                let progress = self.download_progress(&m.id);
                let label = model_state(installed, progress.is_some());
                ModelView {
                    id: m.id.clone(),
                    kind: format!("{:?}", m.kind).to_lowercase(),
                    engine: m.engine.clone(),
                    license: m.license.clone(),
                    installed,
                    state: label.to_owned(),
                    downloaded: progress.map(|(downloaded, _)| downloaded),
                    bytes: m.total_size(),
                    commercial_use: m.commercial_use,
                    // A Kokoro model's styles are voices in its archive, not speakers in the
                    // manifest: `num_speakers` says one, and the fifty-four styles are only
                    // readable once the archive is on disk. A caller that shows a voice list
                    // needs them, and a caller that passes the model id as a style is told
                    // the archive has no such voice.
                    speakers: if m.engine == "kokoro" && installed {
                        archive_names(&self.models_dir, m).unwrap_or_else(|| speaker_names(&m.meta))
                    } else {
                        speaker_names(&m.meta)
                    },
                    languages: meta_strings(&m.meta, "languages"),
                }
            })
            .collect()
    }

    /// The catalog as one JSON document, for callers that speak JSON and nothing else.
    #[must_use]
    pub fn models_json(&self) -> String {
        serde_json::to_string(&self.models_view()).unwrap_or_else(|_| "[]".to_owned())
    }

    /// Open a streaming transcription session, for the C ABI.
    ///
    /// The model loads on the first call, so the first session in a process waits for a
    /// model load. The session owns a share of the recognizer; the lock is not held while
    /// audio is fed.
    ///
    /// # Errors
    ///
    /// A sentence naming what is wrong: no streaming model configured, the model not
    /// installed, or a load that failed.
    pub fn open_stream_session(&self) -> Result<arioso_stt_sherpa::StreamSession, String> {
        self.stream_session().map_err(|error| error.to_string())
    }
}

/// The one speech engine this process has loaded.
///
/// One slot, because one engine loads per process: `tts.engine` chooses it, and a change
/// reloads the slot rather than running two graphs. The enum is what lets the routes be
/// written once.
#[derive(Debug)]
enum Voice {
    /// A Piper voice: one model per voice.
    Piper(PiperVoice),
    /// Kokoro: one model and a voice style.
    Kokoro(KokoroVoice),
}

impl Voice {
    /// Which engine this is, for the engine list.
    fn engine(&self) -> &'static str {
        match self {
            Self::Piper(_) => "piper",
            Self::Kokoro(_) => "kokoro",
        }
    }
}

/// An error returned to the client, in the RFC 9457 shape.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    detail: String,
    /// Stable machine-readable code, as the PRD's error model asks for.
    code: Option<&'static str>,
}

impl ApiError {
    /// Name the failure with the code a client switches on.
    fn coded(status: StatusCode, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
            code: Some(code),
        }
    }
}

/// The API document, derived from the handlers so a new route cannot be missing from it.
///
/// The two WebSockets have no request body to derive from, so they are described in the
/// document's own text.
#[derive(Debug, utoipa::OpenApi)]
#[openapi(
    info(
        title = "arioso",
        version = env!("CARGO_PKG_VERSION"),
        description = "Local text to speech and speech to text.\n\n\
            Two routes are WebSockets and cannot be described by a schema: \n\
            `GET /v1/tts/ws` takes text fragments and returns audio frames; \n\
            `GET /v1/stt/ws` takes a start frame then 16 kHz mono `s16le` PCM and returns \
            partial and final transcripts. Both accept `{\"type\":\"cancel\"}` and answer \
            `{\"type\":\"cancelled\"}`."
    ),
    paths(
        healthz,
        readyz,
        info,
        list_engines,
        list_voices,
        get_config,
        patch_config,
        start_download,
        delete_model,
        warm_model,
        stop_server,
        abort_server,
        list_models,
        model_events,
        synthesize,
        transcribe_audio,
        diarize_audio
    ),
    components(schemas(
        TtsRequest,
        ModelView,
        ModelEvent,
        ModelListView,
        SttResponse,
        DiarizeResponse,
        DiarizeTurn,
        ReadyView,
        DictionaryView,
        Problem
    ))
)]
pub struct ApiDoc;

/// The error body, in the RFC 9457 shape with the PRD's stable `code`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct Problem {
    /// Always `about:blank` for now.
    #[serde(rename = "type")]
    pub kind: String,
    /// Short name of the status.
    pub title: String,
    /// HTTP status, repeated in the body.
    pub status: u16,
    /// Machine-readable code, when the failure has one.
    pub code: Option<String>,
    /// What went wrong, in words meant for a person.
    pub detail: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "type": "about:blank",
            "title": self.status.canonical_reason().unwrap_or("error"),
            "status": self.status.as_u16(),
            "code": self.code,
            "detail": self.detail,
        });
        (self.status, Json(body)).into_response()
    }
}

/// Why a transcription did not happen.
#[derive(Debug, thiserror::Error)]
enum SttFailure {
    /// The bytes were not audio we can read.
    #[error("{0}")]
    Audio(String),
    /// The engine could not load or run.
    #[error("{0}")]
    Engine(String),
    /// The model the config asks for is not downloaded.
    #[error("{0} is not installed")]
    NotInstalled(String),
}

/// Why the diarization models could not be loaded or run.
///
/// The three are answered differently because they are different mistakes: bytes that are
/// not audio, a model that is not downloaded, and a model that is there and will not run.
#[derive(Debug, thiserror::Error)]
pub enum DiarizeFailure {
    /// The bytes were not audio we can read, or at a rate the models do not read.
    #[error("{0}")]
    Audio(String),
    /// The models could not load or run.
    #[error("{0}")]
    Engine(String),
    /// The catalog entry the config names is not downloaded.
    #[error("{0} is not installed")]
    NotInstalled(String),
}

/// Why the voice slot could not be filled.
///
/// The two are answered differently because they are different mistakes: a config naming
/// a voice the engine cannot say is the caller's, and answers 422 so the caller can fix
/// it; an engine that failed to load is the server's, and answers 500. Answered as one,
/// a client is told to retry a request that can never succeed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TtsFailure {
    /// The config names a voice, or a voice name, the engine cannot use.
    #[error("{0}")]
    Client(String),
    /// The engine could not load or run.
    #[error("{0}")]
    Engine(String),
}

/// Request body for `POST /v1/tts`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct TtsRequest {
    /// Text to speak.
    pub text: String,
    /// Speaking rate multiplier, overriding the config.
    pub speed: Option<f32>,
    /// Speaker number on a multi-speaker voice, overriding the config.
    pub sid: Option<i64>,
}

/// One model as reported by `GET /v1/models`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ModelView {
    /// Model id.
    pub id: String,
    /// What the model is for.
    pub kind: String,
    /// Engine that loads it.
    pub engine: String,
    /// SPDX license of the weights.
    pub license: String,
    /// Whether every file is present at the right size.
    pub installed: bool,
    /// `installed`, `downloading`, or `not_installed`.
    pub state: String,
    /// Bytes fetched so far, while a download is running.
    pub downloaded: Option<u64>,
    /// Total size in bytes.
    pub bytes: u64,
    /// Whether the model's license allows commercial use.
    pub commercial_use: bool,
    /// Speaker names, in the order the voice numbers them.
    ///
    /// Empty for a single-speaker model: a list of one would suggest a choice that is not
    /// there.
    pub speakers: Vec<String>,
    /// Languages the model speaks, as the manifest records them.
    pub languages: Vec<String>,
}

/// One model's state, as the catalog reports it and the event stream carries it.
///
/// The same state a [`ModelView`] carries, without the catalog description: a client that
/// has read `GET /v1/models` once parses one shape for the polled answer and the streamed
/// one. `total` is what `ModelView` calls `bytes`.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ModelEvent {
    /// Model id.
    pub id: String,
    /// `installed`, `downloading`, or `not_installed`.
    pub state: String,
    /// Bytes fetched so far. Absent when no download has reported anything.
    pub downloaded: Option<u64>,
    /// Total size in bytes.
    pub total: u64,
    /// Why a download failed, when one did.
    pub error: Option<String>,
}

/// The label for a model's state, from what the disk and the download map say.
fn model_state(installed: bool, downloading: bool) -> &'static str {
    match (installed, downloading) {
        (true, _) => "installed",
        (false, true) => "downloading",
        (false, false) => "not_installed",
    }
}

/// Read a certificate and key, ready to serve TLS.
///
/// The pair is read once here and again whenever the files change, because certbot renews
/// on a timer and a renewal that needs a restart is a renewal that takes the service down
/// the one time nobody is watching.
pub async fn tls_config(
    cert: &std::path::Path,
    key: &std::path::Path,
) -> Result<axum_server::tls_rustls::RustlsConfig, ServerError> {
    // axum-server takes a provider-agnostic rustls build, so the process has to choose
    // one before the first handshake or rustls panics looking for a default. ring rather
    // than aws-lc-rs: it needs no C toolchain beyond what the other native dependencies
    // already require, and Windows CI is already the fragile one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
        .await
        .map_err(|error| ServerError::Tls {
            cert: cert.display().to_string(),
            key: key.display().to_string(),
            source: error,
        })
}

/// Re-read the certificate pair when either file changes.
///
/// certbot renews on a timer. A renewal that needs a restart is a renewal that takes the
/// service down the one time nobody is watching, so the pair is polled instead. The poll
/// is a modification time check every thirty seconds: cheap enough to run forever, and
/// far more predictable than an inotify watcher across the platforms this ships on.
pub fn watch_certificates(
    config: axum_server::tls_rustls::RustlsConfig,
    cert: std::path::PathBuf,
    key: std::path::PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut seen = pair_stamp(&cert, &key);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let now = pair_stamp(&cert, &key);
            if now == seen || now.is_none() {
                continue;
            }
            match config.reload_from_pem_file(&cert, &key).await {
                Ok(()) => {
                    seen = now;
                    tracing::info!(
                        cert = %cert.display(),
                        "reloaded the certificate"
                    );
                }
                Err(error) => {
                    // A half-written file during a renewal is normal; the next poll reads
                    // it again. The old certificate keeps serving in the meantime.
                    tracing::warn!(
                        cert = %cert.display(),
                        %error,
                        "could not reload the certificate, keeping the one in use"
                    );
                }
            }
        }
    })
}

/// The modification times of both files, or `None` when either is unreadable.
fn pair_stamp(
    cert: &std::path::Path,
    key: &std::path::Path,
) -> Option<(std::time::SystemTime, std::time::SystemTime)> {
    let read = |path: &std::path::Path| std::fs::metadata(path).ok()?.modified().ok();
    Some((read(cert)?, read(key)?))
}

/// One client's budget: how many requests it may still make, and when that was counted.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: std::time::Instant,
}

/// Per-client request budgets.
///
/// A token bucket, not a fixed window: a window lets a client send its whole allowance at
/// the end of one window and again at the start of the next, which is twice the rate at
/// exactly the moment the server is least able to absorb it.
#[derive(Debug)]
pub struct RateLimiter {
    per_second: f64,
    burst: f64,
    buckets: Mutex<Buckets>,
}

/// The bucket table, and when it was last swept.
///
/// The sweep time lives beside the table so one lock covers both: two locks would let a
/// request see a swept table and a stale timestamp, and sweep again for nothing.
#[derive(Debug)]
struct Buckets {
    map: std::collections::HashMap<std::net::IpAddr, Bucket>,
    last_sweep: std::time::Instant,
}

/// The table never holds more than this, however many addresses a burst brings.
///
/// A public server sees an address once and never again; without a bound the table is a
/// leak an attacker controls. 4,096 buckets is a few hundred kilobytes and far more than
/// the number of clients one host serves at once.
const MAX_BUCKETS: usize = 4_096;

impl RateLimiter {
    /// Build a limiter from the configured rate and burst.
    #[must_use]
    pub fn new(per_second: u32, burst: u32) -> Self {
        Self {
            per_second: f64::from(per_second),
            burst: f64::from(burst),
            buckets: Mutex::new(Buckets {
                // Sized for a table that fills. The map is capped at `MAX_BUCKETS` and a
                // real deployment sees far more than one client, so an empty map would
                // rehash its way up through the first few hundred addresses.
                map: std::collections::HashMap::with_capacity(64),
                last_sweep: std::time::Instant::now(),
            }),
        }
    }

    /// How long an empty bucket takes to refill.
    ///
    /// A bucket idle for this long is indistinguishable from a new one, so it is what the
    /// sweep drops and how often the sweep is worth running. A rate of zero never refills,
    /// so nothing is ever idle enough to drop.
    fn refill_interval(&self) -> std::time::Duration {
        if self.per_second > 0.0 {
            std::time::Duration::from_secs_f64(self.burst / self.per_second)
        } else {
            std::time::Duration::MAX
        }
    }

    /// Take one token for this client, returning how long to wait when none is left.
    ///
    /// `None` means allowed. `Some(duration)` is how long until the next token, which is
    /// what the `Retry-After` header reports.
    pub fn take(&self, client: std::net::IpAddr) -> Option<std::time::Duration> {
        let now = std::time::Instant::now();
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let refill = self.refill_interval();
        // A table that grows with every address ever seen is a leak, so buckets idle long
        // enough to be full again are dropped. At most one pass per refill interval: during
        // a burst no bucket is idle enough to drop, so a pass per request removed nothing
        // and repeated the full scan.
        if now.duration_since(buckets.last_sweep) >= refill {
            buckets.map.retain(|_, bucket| {
                now.duration_since(bucket.last).as_secs_f64() * self.per_second < self.burst
            });
            buckets.last_sweep = now;
        }
        // The sweep is not enough on its own: a burst of distinct addresses is all active,
        // so nothing is idle and the map would grow without bound. At the cap, the least
        // recently used entry makes room for a new client.
        if buckets.map.len() >= MAX_BUCKETS && !buckets.map.contains_key(&client) {
            if let Some(oldest) = buckets
                .map
                .iter()
                .min_by_key(|(_, bucket)| bucket.last)
                .map(|(address, _)| *address)
            {
                buckets.map.remove(&oldest);
            }
        }
        let bucket = buckets.map.entry(client).or_insert(Bucket {
            tokens: self.burst,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.per_second).min(self.burst);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            None
        } else {
            let wait = (1.0 - bucket.tokens) / self.per_second;
            Some(std::time::Duration::from_secs_f64(wait.max(0.001)))
        }
    }
}

/// Refuse a client that has spent its budget.
async fn rate_limit(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let client = state.client_address(request.headers(), request.extensions());
    if let Some(wait) = state.limiter.take(client) {
        let seconds = wait.as_secs().max(1);
        tracing::debug!(%client, retry_after = seconds, "refused by the rate limit");
        let mut response = ApiError::coded(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many requests from this address",
        )
        .into_response();
        if let Ok(value) = axum::http::HeaderValue::from_str(&seconds.to_string()) {
            response.headers_mut().insert("retry-after", value);
        }
        return response;
    }
    next.run(request).await
}

/// Compare two byte strings without returning early on the first difference.
///
/// A token check that returns as soon as a byte differs tells the caller how much of the
/// guess was right, which turns one secret into a series of cheap questions.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Require the bearer token when one is configured.
///
/// `/healthz` is the only route that answers without a token: a liveness probe has to work
/// before the operator has read the config, and it reveals nothing but the fact that the
/// process is alive. Everything else, including the config document and the API docs,
/// needs the token.
async fn require_auth(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(expected) = state.token() else {
        return next.run(request).await;
    };
    if request.uri().path() == "/healthz" {
        return next.run(request).await;
    }
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        return next.run(request).await;
    }
    tracing::debug!(
        path = %request.uri().path(),
        presented = !presented.is_empty(),
        "refused a request without the token"
    );
    let mut response = ApiError::coded(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "this server needs a bearer token in the Authorization header",
    )
    .into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str("Bearer realm=\"arioso\"") {
        response
            .headers_mut()
            .insert(axum::http::header::WWW_AUTHENTICATE, value);
    }
    response
}

/// Add the response headers that cost nothing and close the cheap classes of browser bug.
/// Give every request an id, and say what it was.
///
/// A client that reports a bad answer needs something to quote. An id the client sent is
/// kept when it is a plain token, so a caller can follow its own request through the log;
/// anything else is replaced, because a header is not a place to put a newline.
async fn request_id(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let sent = request
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
        })
        .map(str::to_owned);
    let id = sent.unwrap_or_else(|| {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        format!("{:08x}{:04x}", seq as u32, (nanos & 0xffff) as u16)
    });

    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let span = tracing::info_span!("request", request_id = %id, %method, %path);
    let mut response = {
        use tracing::Instrument;
        next.run(request).instrument(span).await
    };
    if let Ok(value) = axum::http::HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

async fn security_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    // A browser that sniffs a JSON body as HTML turns a reflected string into a script.
    headers.insert(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        axum::http::HeaderValue::from_static("nosniff"),
    );
    // A URL from this server should not carry the address of a private one.
    headers.insert(
        axum::http::header::REFERRER_POLICY,
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        axum::http::header::X_FRAME_OPTIONS,
        axum::http::HeaderValue::from_static("DENY"),
    );
    response
}

/// Refuse a streaming session when every permit is in use.
///
/// A queue would be kinder and worse: the caller would wait an unbounded time behind
/// sessions that end when a document ends. A refusal with a code lets the client decide.
/// Refuse a route whose engine is switched off, as a 404 like an absent route.
///
/// The route exists, so a client that guessed it gets the same answer as one that guessed
/// wrong: the engine is not on, and there is nothing there.
pub(crate) fn engine_disabled(what: &str) -> axum::response::Response {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "engine_disabled",
        format!("{what} is switched off; PATCH /v1/config turns it on"),
    )
    .into_response()
}

pub(crate) fn too_many_sessions() -> axum::response::Response {
    ApiError::coded(
        StatusCode::SERVICE_UNAVAILABLE,
        "too_many_sessions",
        "every streaming session this server allows is in use; try again shortly",
    )
    .into_response()
}

/// The 409 a caller gets when the configured speech model is not on disk.
///
/// The batch route and the socket answer with the same code and the same sentence, so a
/// client that reads one can read the other.
pub(crate) fn model_not_installed(id: &str) -> axum::response::Response {
    ApiError::coded(
        StatusCode::CONFLICT,
        "model_not_installed",
        format!("{id} is not installed. Run `arioso models download {id}`."),
    )
    .into_response()
}

/// Build the router.
///
/// Every route is registered whatever the config says, and each engine handler checks
/// `enabled` when it runs. Registering at startup made `PATCH /v1/config` a one-way switch:
/// a server started with an engine off could never turn it on, and one started with it on
/// kept serving after it was turned off.
pub fn router(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/models", get(list_models))
        .route("/v1/models/events", get(model_events))
        .route("/v1/models/{id}/download", post(start_download))
        .route("/v1/models/{id}", axum::routing::delete(delete_model))
        .route("/v1/models/{id}/warm", post(warm_model))
        .route("/v1/info", get(info))
        .route("/v1/engines", get(list_engines))
        .route("/v1/voices", get(list_voices))
        .route("/v1/config", get(get_config).patch(patch_config))
        .route("/v1/stop", post(stop_server))
        .route("/v1/abort", post(abort_server));

    // The Swagger UI serves the document at the URL it is given, so this registers
    // /openapi.json as well as /docs. An explicit route beside it is a collision.
    //
    // A document that names every route is a map, and a map is the first thing a scanner
    // wants. `server.docs: false` leaves it out entirely, so the paths answer 404 rather
    // than 401 and the surface is not described to anyone.
    if state.config().server.docs {
        // Built per request, so a patch that turns an engine off takes its paths out of
        // the document at the same moment it starts answering 404.
        router = router.route("/openapi.json", get(openapi_json));
        // The UI keeps its own copy, under a path that cannot collide with the route above.
        router = router.merge(
            utoipa_swagger_ui::SwaggerUi::new("/docs").url("/docs/openapi.json", document(&state)),
        );
    }

    router = router
        .route("/v1/tts", post(synthesize))
        .route("/v1/tts/ws", get(ws_handler));
    // The body is audio, so the default 2 MiB limit is too small. The limit follows
    // stt.max_audio_s: a config that allows an hour of audio and then refuses the body
    // at ten minutes is two limits that disagree, and the one the user can set is the
    // one that does not decide.
    let limit = max_audio_bytes(state.config().stt.max_audio_s);
    router = router.route(
        "/v1/stt",
        post(transcribe_audio).layer(axum::extract::DefaultBodyLimit::max(limit)),
    );
    router = router.route("/v1/stt/ws", get(stt_ws_handler));
    router = router.route(
        "/v1/diarize",
        post(diarize_audio).layer(axum::extract::DefaultBodyLimit::max(limit)),
    );

    // The limiter wraps every route, including the ones a client can afford to hammer:
    // /healthz is the cheapest way to make the server do work, which makes it the first
    // thing a flood uses.
    router
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .layer(axum::middleware::from_fn(security_headers))
        .layer(axum::middleware::from_fn(request_id))
        .with_state(state)
}

/// Largest audio body accepted, for audio that may last `max_audio_s` seconds.
///
/// 16-bit mono at 48 kHz, the widest form the transcriber accepts. Saturating, because a
/// config may name a duration no allocation can hold and a refused request is better than
/// an overflow.
fn max_audio_bytes(max_audio_s: u32) -> usize {
    48_000usize
        .saturating_mul(2)
        .saturating_mul(max_audio_s as usize)
}

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "system",
    summary = "Liveness",
    responses((status = 200, description = "The process is alive", body = String))
)]
async fn healthz() -> &'static str {
    "ok"
}

/// The user dictionary as reported by readiness.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DictionaryView {
    /// Entries in the loaded dictionary.
    pub entries: usize,
    /// Why the last reload failed, if it did.
    pub error: Option<String>,
}

/// Response body for `GET /readyz`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ReadyView {
    /// `ready` when every enabled engine can serve, otherwise what is missing.
    pub status: String,
    /// The user dictionary.
    pub dictionary: DictionaryView,
    /// Voice the server will load, when text to speech is enabled. For Kokoro this is
    /// the style name, which is the value that was checked against the archive.
    pub voice: Option<String>,
    /// Whether that voice can be spoken: its model is on disk, and for Kokoro the
    /// archive holds the style.
    pub voice_installed: bool,
    /// Whether it is loaded right now.
    pub voice_loaded: bool,
    /// Speech model the server will load, when speech to text is enabled.
    pub stt: Option<String>,
    /// Whether that model is on disk.
    pub stt_installed: bool,
    /// Whether it is loaded right now.
    pub stt_loaded: bool,
    /// Voice activity detection model the streaming path will load, when speech to text is
    /// on and `vad.engine` is `silero`. Absent when the energy detector runs, which needs no
    /// model.
    pub vad: Option<String>,
    /// Whether that model is on disk.
    pub vad_installed: bool,
    /// How many catalog models are installed.
    pub models_installed: usize,
}

/// Response body for `GET /v1/info`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct InfoView {
    /// Version of the crate that built this binary.
    pub version: String,
    /// Target triple the binary was built for.
    pub target: String,
    /// Engines compiled into this binary, by id.
    pub engines: Vec<String>,
    /// How the speech-to-text socket reads the configured model: `streaming` when the
    /// model keeps its own state and a partial is read from it, `batch` when a partial is
    /// the batch engine run again over the buffer so far. Absent when speech to text is
    /// switched off, because there is no path to report.
    pub stt_path: Option<String>,
}

/// One engine as reported by `GET /v1/engines`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct EngineView {
    /// Engine id, as a client names it in config.
    pub id: String,
    /// `tts` or `stt`.
    pub kind: String,
    /// Whether it can produce results before the input ends.
    pub streaming: bool,
    /// Languages the catalog has a model for.
    pub languages: Vec<String>,
    /// How many models in the catalog belong to this engine.
    pub models: usize,
    /// Whether a model of this engine is loaded right now.
    pub loaded: bool,
}

/// Response body for `GET /v1/engines`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct EngineListView {
    /// Every engine this binary can serve.
    pub engines: Vec<EngineView>,
}

/// One voice as reported by `GET /v1/voices`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct VoiceView {
    /// Id to put in `tts.voice`.
    pub id: String,
    /// Engine that speaks it.
    pub engine: String,
    /// SPDX id of the voice's licence, which differs per voice.
    pub license: String,
    /// Whether the licence allows commercial use.
    pub commercial_use: bool,
    /// Rate the voice speaks at, from the manifest.
    pub sample_rate: Option<u64>,
    /// Quality tier the voice was trained at, where the manifest records one.
    pub quality: Option<String>,
    /// Languages the voice speaks.
    pub languages: Vec<String>,
    /// How many speakers the voice can produce.
    pub num_speakers: u64,
    /// Speaker names and their numbers, where the voice names them. This is what a
    /// picker shows instead of asking the user for an integer.
    pub speakers: Option<serde_json::Value>,
    /// Whether its files are on disk.
    pub installed: bool,
    /// Whether this is the voice the server would load now.
    pub active: bool,
}

/// Response body for `GET /v1/voices`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct VoiceListView {
    /// Every voice the catalog holds, after filtering.
    pub voices: Vec<VoiceView>,
}

/// Query for `GET /v1/voices`.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct VoicesQuery {
    /// Keep only voices that speak this language, matched as a prefix so `en` finds
    /// `en_US`.
    pub language: Option<String>,
}

/// Response body for `GET /v1/models`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ModelListView {
    /// Every model in the catalog.
    pub models: Vec<ModelView>,
}

/// The voice names a Kokoro model's archive holds, when the archive is on disk.
///
/// The catalog entry holds every style, so a style is present only if the archive indexes
/// it. Absent means the archive is not there to ask, which is a model that is not
/// installed rather than a voice that is missing.
fn archive_names(models_dir: &Path, spec: &ModelSpec) -> Option<Vec<String>> {
    spec.files
        .iter()
        .find(|file| file.name.ends_with(".bin"))
        .map(|file| models_dir.join(&file.name))
        .filter(|path| path.is_file())
        .and_then(|path| KokoroVoice::voice_names(&path).ok())
}

#[utoipa::path(
    get,
    path = "/readyz",
    tag = "system",
    summary = "Readiness",
    description = "Ready means every enabled engine can serve, not that some model is. A \
        missing model is 503, and so is a configured voice the engine cannot speak: for \
        Kokoro the named style has to be in the archive.",
    responses(
        (status = 200, body = ReadyView),
        (status = 503, description = "An enabled engine's model is not installed", body = ReadyView)
    )
)]
async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    let config = state.config();
    let installed = state
        .manifest
        .models
        .iter()
        .filter(|model| state.is_installed(model))
        .count();
    // Readiness is about the voice this server will actually load, and only about the
    // engines that are on. A server with speech to text off is not unready because no
    // speech model is installed.
    let (voice, voice_installed) = match state.ready_voice() {
        Some((name, installed)) => (Some(name), installed),
        // Speech to text is off, so no voice is reported and nothing is missing.
        None => (None, true),
    };
    let voice_loaded = state
        .voice
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false);
    let stt = config.stt.enabled.then(|| config.stt.batch_model.clone());
    let stt_installed = !config.stt.enabled
        || (state
            .manifest
            .find(&config.stt.batch_model)
            .is_some_and(|spec| state.is_installed(spec))
            // A configured streaming model is what the socket will load, so a server
            // whose socket cannot start is not ready even when the file route could.
            && state
                .stream_spec()
                .is_none_or(|spec| state.is_installed(spec)));
    let stt_loaded = state.stt_loaded();
    // The detector is only in the streaming path, so it is only part of readiness when
    // speech to text is on and the config asks for a model.
    let vad =
        (config.stt.enabled && config.vad.engine == "silero").then(|| config.vad.model.clone());
    let vad_installed = vad.as_ref().is_none_or(|id| state.model_installed(id));

    let status = if !voice_installed {
        "voice_missing"
    } else if !stt_installed {
        "stt_missing"
    } else if !vad_installed {
        "vad_missing"
    } else {
        "ready"
    };
    let user = state.user_snapshot();
    let dictionary = DictionaryView {
        entries: user.dictionary.len(),
        error: user.error.clone(),
    };
    drop(user);

    // The status code is the part an orchestrator reads. Answering 200 with
    // `"status": "voice_missing"` tells a health check the process is fine when it cannot
    // serve a single request.
    let code = if status == "ready" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(ReadyView {
            status: status.to_owned(),
            dictionary,
            voice,
            voice_installed,
            voice_loaded,
            stt,
            stt_installed,
            stt_loaded,
            vad,
            vad_installed,
            models_installed: installed,
        }),
    )
        .into_response()
}

#[utoipa::path(
    get,
    path = "/v1/info",
    tag = "system",
    summary = "Version, target, and the engines compiled in",
    responses((status = 200, body = InfoView))
)]
async fn info(State(state): State<Arc<AppState>>) -> Json<InfoView> {
    // A caller that plans a session reads this instead of guessing from the model id:
    // the two paths differ in when the first partial can arrive.
    let stt_path = state.config().stt.enabled.then(|| {
        if state.stt_streams() {
            "streaming"
        } else {
            "batch"
        }
        .to_string()
    });
    Json(InfoView {
        version: env!("CARGO_PKG_VERSION").to_string(),
        target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        engines: state.engines().into_iter().map(|e| e.id).collect(),
        stt_path,
    })
}

#[utoipa::path(
    get,
    path = "/v1/engines",
    tag = "engines",
    summary = "What each engine can do",
    description = "Built from the engines this binary can serve, not from the catalog, \
        so a model file on disk does not invent an engine.",
    responses((status = 200, body = EngineListView))
)]
async fn list_engines(State(state): State<Arc<AppState>>) -> Json<EngineListView> {
    Json(EngineListView {
        engines: state.engines(),
    })
}

/// A partial config update. Only the fields present in the body change.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatch {
    /// Write the result to the config file, so it survives a restart.
    #[serde(default)]
    pub persist: bool,
    /// Text to speech settings.
    pub tts: Option<TtsPatch>,
    /// Speech to text settings.
    pub stt: Option<SttPatch>,
    /// Listener settings.
    pub server: Option<ServerPatch>,
    /// Voice activity detection settings.
    pub vad: Option<VadPatch>,
}

/// Text to speech fields a patch may change.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TtsPatch {
    /// Whether the speech routes are served.
    pub enabled: Option<bool>,
    /// Engine id: `piper` or `kokoro`, both built into this binary.
    pub engine: Option<String>,
    /// Voice id, or the bare voice name.
    pub voice: Option<String>,
    /// Speaking rate, 0.5 to 2.0.
    pub speed: Option<f32>,
    /// Speaker number on a multi-speaker voice.
    pub sid: Option<i64>,
    /// `wav` or `pcm`.
    pub format: Option<String>,
    /// Whether times are read with a part of day.
    pub time_of_day: Option<bool>,
}

/// Speech to text fields a patch may change.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SttPatch {
    /// Whether the transcription routes are served.
    pub enabled: Option<bool>,
    /// Catalog id of the batch model.
    pub batch_model: Option<String>,
    /// Catalog id of the model the streaming socket uses. Empty means the socket runs
    /// the batch model instead.
    pub stream_model: Option<String>,
    /// `auto`, or a language code.
    pub language: Option<String>,
    /// Whether the stream sends partial transcripts.
    pub partials: Option<bool>,
}

/// Voice activity detection fields a patch may change.
///
/// The segmenter is built per connection, so a patch applies to the next stream that opens.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct VadPatch {
    /// Which detector runs. Only `energy` is built in.
    pub engine: Option<String>,
    /// Level above which a frame counts as speech, as a root mean square.
    pub threshold: Option<f32>,
    /// Shortest run of speech that counts, in milliseconds.
    pub min_speech_ms: Option<u32>,
    /// Silence that ends an utterance, in milliseconds.
    pub min_silence_ms: Option<u32>,
    /// Audio kept before the detected start, in milliseconds.
    pub padding_ms: Option<u32>,
    /// Hard cap on one segment, in milliseconds.
    pub max_segment_ms: Option<u32>,
    /// Analysis frame size, in milliseconds.
    pub frame_ms: Option<u32>,
}

/// Listener fields a patch may change.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerPatch {
    /// How long a WebSocket client may stay silent, in milliseconds.
    pub first_message_timeout_ms: Option<u64>,
}

/// What a patch did to the loaded speech engine.
///
/// The distinction is what makes a voice change cheap or expensive: a Piper voice is a
/// model, so a new name is a new graph; a Kokoro voice is a row in an archive the loaded
/// graph already holds, so a new name is a table swap.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum VoiceChange {
    /// The engine, its files and its voice are as they were.
    #[default]
    None,
    /// The name changed but not the engine. Whether the engine can take it in place is
    /// the engine's answer, decided where the loaded voice is known.
    Style(String),
    /// A different engine, so a different graph has to be loaded.
    Reload,
}

/// What a loaded engine did with a style change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Swapped {
    /// The loaded Kokoro engine swapped its style table; nothing was reloaded.
    InPlace,
    /// The slot held nothing, so there is nothing to drop and nothing to reload early.
    Empty,
    /// The slot holds a Piper voice, which is one model per voice.
    OtherEngine,
    /// The archive does not hold the name, so the loaded style is no longer the one asked
    /// for and must not keep answering.
    Refused,
}

/// What a patch touched, so the engines it invalidated can be dropped.
#[derive(Debug, Default)]
struct Touched {
    voice: VoiceChange,
    stt: bool,
    /// The voice a patch of the engine had to change, as (was, is). Reported in the
    /// response, because the caller asked for one change and got two.
    voice_defaulted: Option<(String, String)>,
}

impl ConfigPatch {
    /// Apply the fields present, and report what changed.
    fn apply(self, config: &mut Config) -> Touched {
        let mut touched = Touched::default();
        if let Some(tts) = self.tts {
            if let Some(enabled) = tts.enabled {
                config.tts.enabled = enabled;
            }
            // A voice belongs to one engine and the two share no names, so a patch that
            // changes the engine without naming a voice would leave the config asking
            // for something the new engine cannot say. The engine's own default stands
            // in, and the response says so.
            let changed_engine = tts
                .engine
                .as_ref()
                .filter(|engine| **engine != config.tts.engine)
                .cloned();
            let named_voice = tts.voice.is_some();
            // The name in force before the patch, so a change is measured against what
            // was actually being spoken rather than against the stored field.
            let was_voice = config.voice_name().to_string();
            if let Some(engine) = tts.engine {
                config.tts.engine = engine;
            }
            if let Some(voice) = tts.voice {
                // An empty string is how a caller clears a named voice and hands the
                // choice back to the engine.
                config.tts.voice = (!voice.is_empty()).then_some(voice);
            }
            // A new engine cannot say the old engine's voice, and the caller did not name
            // one, so the field is cleared rather than filled: the engine's own default
            // applies, and the config does not end up naming a voice nobody chose.
            if let (Some(_), false) = (&changed_engine, named_voice) {
                let was = config.tts.voice.take();
                if let Some(was) = was {
                    touched.voice_defaulted = Some((was, config.voice_name().to_string()));
                }
            }
            // What the slot has to do about it. A patch that names the engine and voice it
            // already has is not a change at all, and dropping the loaded graph for it
            // would cost a reload for nothing. A new engine is always a new graph; within
            // one engine, only Kokoro can keep the graph, because its voices are rows in
            // one archive rather than a model each.
            touched.voice = if changed_engine.is_some() {
                VoiceChange::Reload
            } else if config.voice_name() == was_voice {
                VoiceChange::None
            } else if config.tts.engine == "kokoro" {
                VoiceChange::Style(config.voice_name().to_string())
            } else {
                VoiceChange::Reload
            };
            if let Some(speed) = tts.speed {
                config.tts.speed = speed;
            }
            if let Some(sid) = tts.sid {
                config.tts.sid = sid;
            }
            if let Some(format) = tts.format {
                config.tts.format = format;
            }
            if let Some(time_of_day) = tts.time_of_day {
                config.tts.time_of_day = time_of_day;
            }
        }
        if let Some(stt) = self.stt {
            if let Some(enabled) = stt.enabled {
                config.stt.enabled = enabled;
            }
            if let Some(model) = stt.batch_model {
                config.stt.batch_model = model;
                touched.stt = true;
            }
            if let Some(model) = stt.stream_model {
                config.stt.stream_model = model;
                touched.stt = true;
            }
            if let Some(language) = stt.language {
                config.stt.language = language;
                touched.stt = true;
            }
            if let Some(partials) = stt.partials {
                config.stt.partials = partials;
            }
        }
        if let Some(vad) = self.vad {
            if let Some(engine) = vad.engine {
                config.vad.engine = engine;
            }
            if let Some(threshold) = vad.threshold {
                config.vad.threshold = threshold;
            }
            if let Some(min_speech_ms) = vad.min_speech_ms {
                config.vad.min_speech_ms = min_speech_ms;
            }
            if let Some(min_silence_ms) = vad.min_silence_ms {
                config.vad.min_silence_ms = min_silence_ms;
            }
            if let Some(padding_ms) = vad.padding_ms {
                config.vad.padding_ms = padding_ms;
            }
            if let Some(max_segment_ms) = vad.max_segment_ms {
                config.vad.max_segment_ms = max_segment_ms;
            }
            if let Some(frame_ms) = vad.frame_ms {
                config.vad.frame_ms = frame_ms;
            }
        }
        if let Some(server) = self.server {
            if let Some(timeout) = server.first_message_timeout_ms {
                config.server.first_message_timeout_ms = timeout;
            }
        }
        touched
    }
}

/// Engines this binary can actually serve, and what they are called.
const TTS_ENGINES: [&str; 2] = ["piper", "kokoro"];
const STT_ENGINES: [&str; 1] = ["sherpa"];

#[utoipa::path(
    get,
    path = "/v1/config",
    tag = "config",
    summary = "The config as it stands",
    description = "No field here is secret: the bearer token is read from the environment, \
        never from this document.",
    responses((status = 200, body = serde_json::Value))
)]
async fn get_config(State(state): State<Arc<AppState>>) -> Response {
    // The bytes the client asked for, written once. Going through a `Value` serialised the
    // config, parsed the text into a tree, and serialised the tree back into a body.
    match serde_json::to_vec(&*state.config()) {
        Ok(body) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(error) => {
            tracing::error!(%error, "the config could not be serialised");
            ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the config could not be written; the server log has the detail",
            )
            .into_response()
        }
    }
}

/// A patch response: the config document, with a `notes` array beside it when a change
/// needs saying.
///
/// The config is flattened rather than nested, so every field a client already reads stays
/// where it was, and the response is serialised straight from the struct — one pass, in the
/// declaration order `GET /v1/config` sends. The document used to be serialised to text,
/// parsed back into a `Value`, given its note, and serialised again by Axum: three passes
/// and a tree for one body.
///
/// The floats matter: `to_value` turns an f32 into an f64, so a speed of 1.4 would come
/// back as 1.399999976158142. A struct serialises through the f32 formatter and keeps 1.4.
#[derive(Serialize)]
struct ConfigResponse<'a> {
    #[serde(flatten)]
    config: &'a Config,
    /// What was replaced without being asked for. Absent when nothing was.
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<Vec<String>>,
}

/// Stop the server gracefully.
///
/// Stops accepting, lets in-flight requests finish, and ends the process when they do or
/// when the grace period expires. A caller that wants the work kept uses this one.
#[utoipa::path(
    post,
    path = "/v1/stop",
    tag = "lifecycle",
    summary = "Stop gracefully",
    description = "Stops accepting new connections, lets in-flight requests finish, and \
        ends the process when they do or when `server.limits.shutdown_grace_ms` expires. \
        A streaming session is ended, because it has no natural end.",
    responses(
        (status = 202, description = "The stop has begun"),
        (status = 409, description = "This process is not serving", body = Problem)
    )
)]
async fn stop_server(State(state): State<Arc<AppState>>) -> Response {
    let grace = state.config().server.limits.shutdown_grace_ms;
    let Some(handle) = state.handle() else {
        return ApiError::coded(
            StatusCode::CONFLICT,
            "no_accept_loop",
            "this process is not serving, so there is nothing to stop",
        )
        .into_response();
    };
    tracing::info!(grace_ms = grace, "stopping on request");
    handle.graceful_shutdown(Some(std::time::Duration::from_millis(grace)));
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "stopping": true, "grace_ms": grace })),
    )
        .into_response()
}

/// Stop now.
///
/// Answers first, so the caller is not left holding a connection that is about to vanish,
/// then ends the process. Work in flight is dropped on purpose: a caller that wanted it
/// kept would have asked to stop.
#[utoipa::path(
    post,
    path = "/v1/abort",
    tag = "lifecycle",
    summary = "Stop immediately",
    description = "Answers, flushes, and ends the process. In-flight work is dropped. Use \
        this when a stuck request must not be waited for.",
    responses((status = 202, description = "The process is ending"))
)]
async fn abort_server(State(state): State<Arc<AppState>>) -> Response {
    if state.handle().is_none() {
        // The same guard as stop, and for a sharper reason: an embedded router has no
        // accept loop to end, so exiting here would kill the process that hosts it.
        return ApiError::coded(
            StatusCode::CONFLICT,
            "no_accept_loop",
            "this process is not serving, so there is nothing to abort",
        )
        .into_response();
    }
    tracing::warn!("aborting on request");
    // The response has to leave the socket before the socket closes, so the exit happens
    // in a task that runs after this one returns.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = std::io::stdout().flush();
        std::process::exit(0);
    });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "aborting": true })),
    )
        .into_response()
}

#[utoipa::path(
    patch,
    path = "/v1/config",
    tag = "config",
    summary = "Change the config without a restart",
    description = "Only the fields present in the body change. An unknown field is refused, \
        so a typo does not look like a successful change. Validation runs before anything \
        is applied, so a refused patch changes nothing. A patch that changes `tts.engine` \
        without naming a voice gets that engine's default voice, and the response carries \
        a `notes` array saying which. A Kokoro voice change swaps the style on the loaded \
        graph rather than reloading it: one graph serves all 54 voices.",
    request_body = ConfigPatch,
    responses(
        (status = 200, body = serde_json::Value),
        (status = 422, description = "The patch is valid JSON but not a usable value", body = Problem),
        (status = 409, description = "persist was asked for but there is no config file", body = Problem)
    )
)]
async fn patch_config(
    State(state): State<Arc<AppState>>,
    Json(patch): Json<ConfigPatch>,
) -> Result<Response, ApiError> {
    let persist = patch.persist;
    // One deep copy per patch: patches are rare, reads are not.
    let mut next = (*state.config()).clone();
    let touched = patch.apply(&mut next);
    validate(&next, &state)?;

    if persist {
        let Some(path) = state.config_file.as_deref() else {
            return Err(ApiError::coded(
                StatusCode::CONFLICT,
                "no_config_file",
                "the server started without a config file, so there is nothing to persist to",
            ));
        };
        next.save(path).map_err(|error| {
            ApiError::coded(StatusCode::INTERNAL_SERVER_ERROR, "config_write_failed", {
                tracing::error!(path = %path.display(), %error, "could not write the config");
                "the config file could not be written; the server log has the detail".to_owned()
            })
        })?;
    }

    // Said, not silent: the caller named an engine and lost the voice it was using. The note
    // says what was taken away and what speaks now, and it sits beside the config document
    // rather than inside it, so every field a client already reads stays where it was. It
    // does not claim the field was written: the field is cleared, and the engine's own
    // default answers for it.
    let notes = touched.voice_defaulted.as_ref().map(|(was, is)| {
        vec![format!(
            "tts.engine is {}, which cannot say {was}, so tts.voice is now unset and {} speaks with its default voice {is}",
            next.tts.engine, next.tts.engine
        )]
    });
    let body = match serde_json::to_vec(&ConfigResponse {
        config: &next,
        notes,
    }) {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(%error, "the config could not be serialised");
            return Err(ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the config could not be written; the server log has the detail",
            ));
        }
    };
    state.set_config(next);
    state.retarget_voice(touched.voice);
    if touched.stt {
        state.drop_stt();
    }
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

/// The API document for this server, as it stands.
async fn openapi_json(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(document(&state)).unwrap_or_default())
}

/// The API document, cut down to the routes this server actually serves.
///
/// The document is built from the handlers, so it names every route the crate can serve.
/// A disabled engine has no route, and a document promising one would send a client to a
/// 404.
fn document(state: &AppState) -> utoipa::openapi::OpenApi {
    let config = state.config();
    let mut document = <ApiDoc as utoipa::OpenApi>::openapi();
    if !config.tts.enabled {
        document.paths.paths.remove("/v1/tts");
    }
    if !config.stt.enabled {
        document.paths.paths.remove("/v1/stt");
    }
    document
}

/// Refuse a config this binary cannot serve, before any of it is applied.
fn validate(config: &Config, state: &AppState) -> Result<(), ApiError> {
    if !TTS_ENGINES.contains(&config.tts.engine.as_str()) {
        return Err(unprocessable(format!(
            "tts.engine {} is not built into this binary; the engines here are {}",
            config.tts.engine,
            TTS_ENGINES.join(", ")
        )));
    }
    if !STT_ENGINES.contains(&config.stt.engine.as_str()) {
        return Err(unprocessable(format!(
            "stt.engine {} is not built into this binary; the engines here are {}",
            config.stt.engine,
            STT_ENGINES.join(", ")
        )));
    }
    if !(0.5..=2.0).contains(&config.tts.speed) {
        return Err(unprocessable(format!(
            "tts.speed {} is outside 0.5 to 2.0",
            config.tts.speed
        )));
    }
    if !(0..=4095).contains(&config.tts.sid) {
        return Err(unprocessable(format!(
            "tts.sid {} is outside 0 to 4095",
            config.tts.sid
        )));
    }
    if !AUDIO_FORMATS.contains(&config.tts.format.as_str()) {
        return Err(unprocessable(format!(
            "tts.format {} is not one of {}",
            config.tts.format,
            AUDIO_FORMATS.join(", ")
        )));
    }
    if config.server.first_message_timeout_ms < 100 {
        return Err(unprocessable(
            "server.first_message_timeout_ms must be at least 100".to_string(),
        ));
    }
    // A Piper voice is a catalog id, and the manifest decides whether one is known. A Kokoro
    // voice is a style inside one archive, so it is checked against the archive when it is on
    // disk; before that there is nothing to check against, and a wrong name fails at load
    // naming the ones that exist.
    let voice_known = if config.tts.engine == "kokoro" {
        let archive = state
            .manifest
            .models
            .iter()
            .find(|model| model.engine == "kokoro" && model.kind == arioso_models::Kind::Tts)
            .and_then(|model| archive_names(&state.models_dir, model));
        archive.is_none_or(|names| names.contains(&config.voice_name().to_string()))
    } else {
        state.manifest.find_voice(config.voice_name()).is_some()
    };
    if !voice_known {
        return Err(unprocessable(format!(
            "tts.voice {} is not one of the voices the {} engine has; GET /v1/voices lists what is",
            config.voice_name(),
            config.tts.engine
        )));
    }
    let model_known =
        state.manifest.models.iter().any(|model| {
            model.kind == arioso_models::Kind::Stt && model.id == config.stt.batch_model
        });
    if !model_known {
        return Err(unprocessable(format!(
            "stt.batch_model {} is not in the catalog; GET /v1/models lists what is",
            config.stt.batch_model
        )));
    }
    // Empty is the documented fallback to the batch model; anything else must resolve to
    // a streaming entry, or the socket would quietly run the batch loop under a name the
    // operator thought selected streaming.
    if let Some(detail) = stream_model_problem(config, &state.manifest) {
        return Err(unprocessable(detail));
    }
    // The detector is a catalog entry like the speech model, and the socket resolves it by
    // id. A name the catalog does not hold would open a session and then refuse every frame
    // with `model_not_installed`, which sends an operator to download a model that does not
    // exist.
    if config.vad.engine == "silero"
        && !state
            .manifest
            .models
            .iter()
            .any(|model| model.kind == arioso_models::Kind::Vad && model.id == config.vad.model)
    {
        return Err(unprocessable(format!(
            "vad.model {} is not a detector in the catalog; GET /v1/models lists what is",
            config.vad.model
        )));
    }
    Ok(())
}

/// A refused value, with the field named.
fn unprocessable(detail: String) -> ApiError {
    ApiError::coded(StatusCode::UNPROCESSABLE_ENTITY, "invalid_value", detail)
}

#[utoipa::path(
    get,
    path = "/v1/voices",
    tag = "voices",
    summary = "The voices the catalog holds",
    description = "Marked with the active one, so a client can render a picker \
        without a second call.",
    params(VoicesQuery),
    responses((status = 200, body = VoiceListView))
)]
async fn list_voices(
    State(state): State<Arc<AppState>>,
    Query(query): Query<VoicesQuery>,
) -> Json<VoiceListView> {
    // Resolve the way the loader does: the config may name the voice by its bare name,
    // so comparing the raw string to a catalog id marks nothing active.
    let active = state.voice_spec().ok().map(|spec| spec.id.clone());
    let config = state.config();
    let active_engine = config.tts.engine.clone();
    let active_voice = config.voice_name().to_string();
    // Voice ids write en_GB and the catalog writes en-GB, so the separator must not
    // decide whether a client finds its language.
    let normalise = |language: &str| language.to_lowercase().replace('_', "-");
    let wanted = query
        .language
        .as_deref()
        .map(normalise)
        .filter(|language| !language.is_empty());

    let mut voices = Vec::new();
    for model in state
        .manifest
        .models
        .iter()
        .filter(|model| model.kind == arioso_models::Kind::Tts)
    {
        let languages = meta_strings(&model.meta, "languages");
        if let Some(wanted) = &wanted {
            let speaks = languages
                .iter()
                .any(|language| normalise(language).starts_with(wanted.as_str()));
            if !speaks {
                continue;
            }
        }
        let installed = state.is_installed(model);
        // A Kokoro entry is one model holding dozens of voice styles, so the styles are
        // the rows a picker can choose. Until the archive is on disk there is nothing to
        // name, and the catalog row stands in for it rather than a list of guesses.
        let styles = if model.engine == "kokoro" && installed {
            archive_names(&state.models_dir, model).unwrap_or_default()
        } else {
            Vec::new()
        };
        let sample_rate = model
            .meta
            .get("sample_rate")
            .and_then(serde_json::Value::as_u64);
        let quality = model
            .meta
            .get("quality")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        if styles.is_empty() {
            voices.push(VoiceView {
                id: model.id.clone(),
                engine: model.engine.clone(),
                license: model.license.clone(),
                commercial_use: model.commercial_use,
                sample_rate,
                quality,
                languages,
                num_speakers: model
                    .meta
                    .get("num_speakers")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1),
                speakers: model.meta.get("speakers").cloned(),
                installed,
                active: active_engine == model.engine && active.as_ref() == Some(&model.id),
            });
            continue;
        }
        for name in styles {
            voices.push(VoiceView {
                id: name.clone(),
                engine: model.engine.clone(),
                license: model.license.clone(),
                commercial_use: model.commercial_use,
                sample_rate,
                quality: None,
                languages: languages.clone(),
                num_speakers: 1,
                speakers: None,
                installed,
                active: active_engine == "kokoro" && active_voice == *name,
            });
        }
    }
    Json(VoiceListView { voices })
}

/// Start fetching a model, or report the one already in flight.
///
/// The request returns before the bytes arrive: a model is tens of megabytes and a
/// caller that waits for it holds a connection open for the length of a download. The
/// catalog reports the progress.
#[utoipa::path(
    post,
    path = "/v1/models/{id}/download",
    tag = "models",
    summary = "Start or resume a download",
    params(("id" = String, Path, description = "Model id")),
    responses(
        (status = 202, description = "The download is running", body = serde_json::Value),
        (status = 404, description = "No such model", body = Problem)
    )
)]
async fn start_download(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(spec) = state.manifest.find(&id).cloned() else {
        return ApiError::coded(
            StatusCode::NOT_FOUND,
            "unknown_model",
            format!("no model with id {id} in the catalog"),
        )
        .into_response();
    };
    let total = spec.total_size();
    let (downloaded, already) = match state.begin_download(&spec) {
        Some(running) => (running.0, true),
        None => (0, false),
    };
    if already {
        tracing::debug!(model = %id, "download already in flight");
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "id": id, "state": "downloading", "downloaded": downloaded, "total": total
        })),
    )
        .into_response()
}

/// Remove a model's files from the models directory.
///
/// Only the files the manifest names, joined onto the configured directory: a delete that
/// walked a directory would remove whatever else had been put there.
#[utoipa::path(
    delete,
    path = "/v1/models/{id}",
    tag = "models",
    summary = "Remove a model from disk",
    params(("id" = String, Path, description = "Model id")),
    responses(
        (status = 200, description = "What was removed", body = serde_json::Value),
        (status = 404, description = "No such model", body = Problem),
        (status = 409, description = "The model is being downloaded", body = Problem)
    )
)]
async fn delete_model(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(spec) = state.manifest.find(&id).cloned() else {
        return ApiError::coded(
            StatusCode::NOT_FOUND,
            "unknown_model",
            format!("no model with id {id} in the catalog"),
        )
        .into_response();
    };
    // Checking that nothing is downloading and blocking a new download has to be one
    // step. Apart, a request that lands between the check and the unlink starts writing
    // the `.part` file this is removing, and the delete reports a model gone while a
    // download keeps writing into it.
    if state.try_start_download(&id, 0).is_some() {
        return ApiError::coded(
            StatusCode::CONFLICT,
            "download_in_progress",
            "the model is being downloaded; removing it now would leave a half-written set",
        )
        .into_response();
    }
    // Gives the slot back however this ends, including the early return that reports a
    // file could not be removed. A reservation left behind wedges the id until restart.
    let _reserved = DeleteReservation {
        state: Arc::clone(&state),
        id: id.clone(),
    };

    let mut removed = Vec::new();
    for file in &spec.files {
        // The partial file too: an interrupted download leaves one behind, and a delete
        // that leaves half a model on disk has not deleted the model.
        for name in [file.name.clone(), format!("{}.part", file.name)] {
            let path = state.models_dir.join(&name);
            match std::fs::remove_file(&path) {
                Ok(()) => removed.push(name),
                // Absent is the state the caller asked for, so it is not an error.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::error!(%error, model = %id, "could not remove {}", path.display());
                    return ApiError::coded(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "remove_failed",
                        "a file could not be removed; the server log has the detail",
                    )
                    .into_response();
                }
            }
        }
    }
    // A model that is gone from disk must not stay in memory. Left loaded, `/readyz` says
    // the voice is installed while it is not, and the next request speaks from a model the
    // caller believes they deleted.
    let mut dropped = Vec::new();
    if spec.kind == Kind::Tts && state.voice_spec().is_ok_and(|active| active.id == spec.id) {
        state.drop_voice();
        dropped.push("voice");
    }
    if spec.kind == Kind::Stt {
        // The batch route and the socket hold different slots, so only the slots that
        // name the deleted model are dropped. Reporting the slot without unloading it was
        // how the active streaming model kept its files mapped after a delete.
        let config = state.config();
        let is_batch = config.stt.batch_model == spec.id;
        let is_stream = state
            .stream_spec()
            .is_some_and(|active| active.id == spec.id);
        drop(config);
        if is_batch {
            state.drop_batch_stt();
            dropped.push("stt");
        }
        if is_stream {
            state.drop_stream_stt();
            dropped.push("stt_stream");
        }
    }
    tracing::info!(model = %id, files = removed.len(), dropped = ?dropped, "model removed");
    (
        StatusCode::OK,
        Json(serde_json::json!({ "id": id, "removed": removed, "dropped": dropped })),
    )
        .into_response()
}

/// Holds a model's download slot for the length of a delete.
///
/// The slot is what a download claims before it writes, so holding it is how a delete says
/// "not now" without a second lock and without a window between the check and the unlink.
struct DeleteReservation {
    state: Arc<AppState>,
    id: String,
}

impl Drop for DeleteReservation {
    fn drop(&mut self) {
        self.state.finish_download(&self.id, None);
    }
}

/// Load a model so the next request does not pay for the load.
///
/// A voice can only be warmed when it is the configured one: there is one voice slot, and
/// loading a different model into it would change what the next request speaks. Speech to
/// text has two slots, the batch model and the streaming model, and either configured one
/// can be warmed. A model that is not installed is refused rather than fetched, because
/// warming is not downloading.
#[utoipa::path(
    post,
    path = "/v1/models/{id}/warm",
    tag = "models",
    summary = "Load a model without a request",
    params(("id" = String, Path, description = "Model id")),
    responses(
        (status = 200, description = "The model is loaded", body = serde_json::Value),
        (status = 404, description = "No such model", body = Problem),
        (status = 409, description = "Not installed, or not a configured model", body = Problem),
        (status = 422, description = "The configured voice is one the engine cannot say", body = Problem)
    )
)]
async fn warm_model(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(spec) = state.manifest.find(&id).cloned() else {
        return ApiError::coded(
            StatusCode::NOT_FOUND,
            "unknown_model",
            format!("no model with id {id} in the catalog"),
        )
        .into_response();
    };
    if !state.is_installed(&spec) {
        return ApiError::coded(
            StatusCode::CONFLICT,
            "model_not_installed",
            format!("{id} is not on disk; POST /v1/models/{id}/download first"),
        )
        .into_response();
    }
    if spec.kind == Kind::Tts {
        let active = state.voice_spec().ok().map(|active| active.id.clone());
        if active.as_deref() != Some(spec.id.as_str()) {
            return ApiError::coded(
                StatusCode::CONFLICT,
                "not_active_voice",
                "only the configured voice can be warmed; there is one voice slot",
            )
            .into_response();
        }
        let worker = Arc::clone(&state);
        // Loading a voice reads tens of megabytes from disk and builds an ONNX session.
        // On the request thread that stalls every other connection the runtime is serving.
        return match tokio::task::spawn_blocking(move || worker.warm_voice()).await {
            Ok(Ok(())) => {
                tracing::info!(model = %id, "model warmed");
                (
                    StatusCode::OK,
                    Json(serde_json::json!({ "id": id, "loaded": true })),
                )
                    .into_response()
            }
            Ok(Err(failure)) => match failure {
                // A config naming a voice the engine cannot say is the caller's mistake:
                // it will fail the same way however many times it is retried.
                TtsFailure::Client(detail) => {
                    tracing::warn!(model = %id, %detail, "the configured voice cannot be spoken");
                    ApiError::coded(StatusCode::UNPROCESSABLE_ENTITY, "invalid_value", detail)
                        .into_response()
                }
                TtsFailure::Engine(detail) => {
                    tracing::error!(model = %id, %detail, "could not warm the voice");
                    ApiError::coded(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "load_failed",
                        "the voice could not be loaded; the server log has the detail",
                    )
                    .into_response()
                }
            },
            Err(join) => {
                tracing::error!(model = %id, %join, "the warm task failed");
                ApiError::coded(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "load_failed",
                    "the voice could not be loaded; the server log has the detail",
                )
                .into_response()
            }
        };
    }

    // Speech to text has two slots: the batch model the file route reads and the
    // streaming model the socket reads. Warming an id that names neither would load
    // something else and report success for the wrong name.
    let config = state.config();
    let is_batch = config.stt.batch_model == spec.id;
    let is_stream = state
        .stream_spec()
        .is_some_and(|active| active.id == spec.id);
    drop(config);
    if !is_batch && !is_stream {
        return ApiError::coded(
            StatusCode::CONFLICT,
            "not_active_model",
            "only the configured batch or streaming model can be warmed",
        )
        .into_response();
    }
    let worker = Arc::clone(&state);
    // Both slots when one id is configured for both routes: they are separate engines, and
    // a warm that loaded only one would leave the other paying for its first request.
    let warmed = tokio::task::spawn_blocking(move || {
        if is_batch {
            worker.warm_stt()?;
        }
        if is_stream {
            worker.warm_stream()?;
        }
        Ok::<(), String>(())
    })
    .await;
    match warmed {
        Ok(Ok(())) => {
            tracing::info!(model = %id, "model warmed");
            (
                StatusCode::OK,
                Json(serde_json::json!({ "id": id, "loaded": true })),
            )
                .into_response()
        }
        Ok(Err(detail)) => {
            tracing::error!(model = %id, %detail, "could not warm the speech engine");
            ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "load_failed",
                "the engine could not be loaded; the server log has the detail",
            )
            .into_response()
        }
        Err(join) => {
            tracing::error!(model = %id, %join, "the warm task failed");
            ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "load_failed",
                "the engine could not be loaded; the server log has the detail",
            )
            .into_response()
        }
    }
}

/// Speaker names from a manifest's metadata, in the order the voice numbers them.
///
/// The manifest records them as a name-to-index object, so the indices decide the order: a
/// picker that showed them alphabetically would disagree with `sid`.
fn speaker_names(meta: &serde_json::Value) -> Vec<String> {
    let Some(map) = meta.get("speakers").and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };
    let mut pairs: Vec<(i64, String)> = map
        .iter()
        .map(|(name, index)| (index.as_i64().unwrap_or(0), name.clone()))
        .collect();
    pairs.sort_by_key(|(index, _)| *index);
    pairs.into_iter().map(|(_, name)| name).collect()
}

/// Strings under one key of a manifest's free-form metadata.
fn meta_strings(meta: &serde_json::Value, key: &str) -> Vec<String> {
    meta.get(key)
        .and_then(|value| value.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[utoipa::path(
    get,
    path = "/v1/models",
    tag = "models",
    summary = "The catalog with what is installed",
    responses((status = 200, body = ModelListView))
)]
async fn list_models(State(state): State<Arc<AppState>>) -> Json<ModelListView> {
    Json(ModelListView {
        models: state.models_view(),
    })
}

/// How long an idle stream waits before sending a comment line.
///
/// A proxy or load balancer closes a connection that has been silent for a while. A
/// comment is not an event, so a client that only reads `event:`/`data:` ignores it.
const KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(15);

/// One `event:`/`data:` frame, in the shape the browser `EventSource` expects.
fn event_frame(name: &str, data: &str) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

/// Every model's current state, as the first frame of a stream.
fn snapshot_frame(state: &AppState) -> String {
    // A `ModelEvent` is plain data, so this cannot fail; an empty array would still be a
    // well-formed frame if it somehow did.
    let data = serde_json::to_string(&state.model_snapshot()).unwrap_or_else(|_| "[]".to_owned());
    event_frame("snapshot", &data)
}

/// State carried between the frames of one stream.
struct EventCursor {
    state: Arc<AppState>,
    receiver: tokio::sync::broadcast::Receiver<ModelEvent>,
    /// The state each model was last reported in. A repeat is progress; a change is a
    /// state event, so a client can act on transitions without diffing byte counts.
    last: std::collections::BTreeMap<String, String>,
    ticker: tokio::time::Interval,
    /// Whether the opening snapshot is still owed.
    snapshot_pending: bool,
}

/// The stream behind `GET /v1/models/events`.
///
/// One frame at a time, and each frame is produced only when there is something to say:
/// the task sleeps in `select!` rather than polling, so an idle stream costs nothing.
fn model_event_stream(
    state: Arc<AppState>,
    receiver: tokio::sync::broadcast::Receiver<ModelEvent>,
    ticker: tokio::time::Interval,
) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, std::convert::Infallible>> {
    let cursor = EventCursor {
        state,
        receiver,
        last: std::collections::BTreeMap::new(),
        ticker,
        snapshot_pending: true,
    };
    futures_util::stream::unfold(cursor, |mut cursor| async move {
        if cursor.snapshot_pending {
            cursor.snapshot_pending = false;
            let frame = snapshot_frame(&cursor.state);
            return Some((Ok(axum::body::Bytes::from(frame)), cursor));
        }
        let EventCursor {
            state,
            receiver,
            last,
            ticker,
            ..
        } = &mut cursor;
        let frame = tokio::select! {
            received = receiver.recv() => match received {
                Ok(event) => {
                    let name = match last.insert(event.id.clone(), event.state.clone()) {
                        Some(previous) if previous == event.state => "progress",
                        _ => "state",
                    };
                    let data = serde_json::to_string(&event).unwrap_or_default();
                    event_frame(name, &data)
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    // The events it missed are gone, so its view of every model is
                    // suspect, not just the ones that changed. Say so, then restate the
                    // whole catalog: silence here would be a client that is wrong and
                    // does not know it.
                    last.clear();
                    format!(
                        "event: lagged\ndata: {{\"missed\":{missed}}}\n\n{}",
                        snapshot_frame(state)
                    )
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            },
            _ = ticker.tick() => ": keep-alive\n\n".to_owned(),
        };
        Some((Ok(axum::body::Bytes::from(frame)), cursor))
    })
}

/// Watch every model's state without polling the catalog.
///
/// The stream opens with a `snapshot` of the whole catalog, then carries `state` events
/// when a model changes and `progress` events while a download runs. It is one request
/// against the rate limiter however long it stays open, and it holds no streaming session:
/// the bytes it carries are a few dozen per event, not audio.
#[utoipa::path(
    get,
    path = "/v1/models/events",
    tag = "models",
    summary = "Model state and download progress as server-sent events",
    responses((
        status = 200,
        description = "One `snapshot` event, then `progress` and `state` events, with a comment line when idle",
        content_type = "text/event-stream",
        body = String
    ))
)]
async fn model_events(State(state): State<Arc<AppState>>) -> Response {
    // Subscribed before the snapshot is taken: an event published in between is then
    // delivered rather than lost, which at worst repeats what the snapshot already said.
    let receiver = state.events.subscribe();
    let mut ticker = tokio::time::interval(KEEPALIVE);
    // Delay, not burst: a stream that was quiet for a minute should not answer with four
    // comment lines at once.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // An interval's first tick is immediate, and nothing has been idle yet.
    ticker.tick().await;
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        axum::body::Body::from_stream(model_event_stream(Arc::clone(&state), receiver, ticker)),
    )
        .into_response()
}

#[utoipa::path(
    post,
    path = "/v1/tts",
    tag = "tts",
    summary = "Synthesize speech",
    request_body = TtsRequest,
    responses(
        (status = 200, description = "WAV audio", content_type = "audio/wav", body = Vec<u8>),
        (status = 400, body = Problem),
        (status = 409, description = "The voice is not installed", body = Problem)
    )
)]
async fn synthesize(
    State(state): State<Arc<AppState>>,
    Json(request): Json<TtsRequest>,
) -> Result<Response, ApiError> {
    if !state.config().tts.enabled {
        return Err(ApiError::coded(
            StatusCode::NOT_FOUND,
            "engine_disabled",
            "text to speech is switched off; PATCH /v1/config turns it on",
        ));
    }
    let text = request.text.trim();
    if text.is_empty() {
        return Err(unprocessable("text must not be empty".to_owned()));
    }
    let max_text_chars = state.config().server.limits.max_text_chars;
    if text.chars().count() > max_text_chars {
        return Err(ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "too_large",
            format!("text is longer than {max_text_chars} characters"),
        ));
    }
    let speed = request.speed.unwrap_or(state.config().tts.speed);
    if !(0.5..=2.0).contains(&speed) {
        return Err(unprocessable(format!(
            "speed {speed} is outside 0.5 to 2.0"
        )));
    }
    let sid = request.sid.unwrap_or(state.config().tts.sid);
    if !(0..=4095).contains(&sid) {
        return Err(ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
            format!("sid {sid} is outside 0 to 4095"),
        ));
    }

    // Checked before the render, so a missing voice is the code a client switches on rather
    // than a 422 carrying an operating system path.
    // Two different failures: a voice the catalog does not know is a bad config value,
    // and a known voice whose files are absent is a model that is not installed.
    let spec = state
        .voice_spec()
        .map_err(|detail| unprocessable(detail.to_string()))?;
    if !state.is_installed(spec) {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "model_not_installed",
            format!(
                "voice {} is not on disk; POST /v1/models/{}/download first",
                spec.id, spec.id
            ),
        ));
    }

    // A voice with one speaker has no other voice to give, and a number out of range is
    // the caller's mistake. Both are 422 with a code, not a 500 from inside the engine.
    if let Err(detail) = state.check_sid(sid) {
        return Err(ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_value",
            detail.to_string(),
        ));
    }

    let text = text.to_owned();
    let worker = Arc::clone(&state);
    let rendered = tokio::task::spawn_blocking(move || worker.render(&text, speed, sid)).await;
    let (pcm, rate) = match rendered {
        Ok(Ok(pair)) => pair,
        Ok(Err(detail)) => {
            // The engine's own message can name a path or a file. It goes to the log, and
            // the client gets a code and a sentence that cannot leak the layout.
            tracing::error!(%detail, "synthesis failed");
            return Err(ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "engine_unavailable",
                "the engine could not synthesize this text; the server log has the detail",
            ));
        }
        Err(join) => {
            tracing::error!(%join, "the render task failed");
            return Err(ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "engine_unavailable",
                "the engine could not synthesize this text; the server log has the detail",
            ));
        }
    };

    // The format the config names, for the two that exist. `pcm` is raw samples, so the
    // caller needs the rate in the content type to make anything of them.
    if state.config().tts.format == "pcm" {
        let content_type = format!("audio/pcm;rate={rate}");
        return Ok(([(header::CONTENT_TYPE, content_type)], pcm).into_response());
    }
    Ok((
        [(header::CONTENT_TYPE, "audio/wav".to_owned())],
        wav::to_bytes_from_pcm(&pcm, rate),
    )
        .into_response())
}

/// One stretch of audio and the voice in it, in a diarization response.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DiarizeTurn {
    /// Start, in seconds from the beginning of the recording.
    pub start: f32,
    /// End, in seconds.
    pub end: f32,
    /// Which voice, numbered from zero. The numbers mean nothing outside this recording.
    pub speaker: u32,
}

/// Seconds, rounded to the millisecond.
///
/// An `f32` widened to `f64` prints sixteen digits of noise after the third decimal, and the
/// answer comes from clustering over windows of half a second: a caller cannot use anything
/// finer, and a document full of digits invites the reader to believe there is.
fn round_ms(seconds: f32) -> f32 {
    (seconds * 1000.0).round() / 1000.0
}

impl DiarizeResponse {
    /// Build the document every front end reads from one list of turns.
    fn from_turns(turns: Vec<arioso_stt_sherpa::diarize::Turn>, seconds: f64) -> Self {
        Self {
            speakers: arioso_stt_sherpa::diarize::speakers(&turns),
            turns: turns
                .into_iter()
                .map(|turn| DiarizeTurn {
                    start: round_ms(turn.start),
                    end: round_ms(turn.end),
                    speaker: turn.speaker,
                })
                .collect(),
            seconds: (seconds * 1000.0).round() / 1000.0,
        }
    }
}

/// Response body for `POST /v1/diarize`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DiarizeResponse {
    /// The turns, in time order.
    pub turns: Vec<DiarizeTurn>,
    /// How many distinct voices were found.
    pub speakers: usize,
    /// Length of the recording, in seconds.
    pub seconds: f64,
}

/// Label the voices in an uploaded WAV file.
///
/// The body is the audio itself. The answer is every stretch of speech with a speaker
/// number: the numbers are assigned per recording and mean nothing across files.
#[utoipa::path(
    post,
    path = "/v1/diarize",
    tag = "diarize",
    summary = "Label the voices in an uploaded file",
    description = "The body is the audio itself, not a JSON envelope. Offline: the whole \
                   file is read before any answer is given.",
    request_body(content = Vec<u8>, content_type = "audio/wav"),
    responses(
        (status = 200, body = DiarizeResponse),
        (status = 415, description = "Not a WAV, or a rate the models do not read", body = Problem),
        (status = 409, description = "The models are not installed", body = Problem)
    )
)]
async fn diarize_audio(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<DiarizeResponse>, ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !matches!(
        content_type.as_str(),
        "audio/wav" | "audio/x-wav" | "audio/wave" | "application/octet-stream" | ""
    ) {
        return Err(ApiError::coded(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_format",
            format!("{content_type} is not audio/wav"),
        ));
    }
    if body.is_empty() {
        return Err(ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_audio",
            "the request body is empty",
        ));
    }

    let worker = Arc::clone(&state);
    let (turns, seconds) = tokio::task::spawn_blocking(move || worker.diarize(&body))
        .await
        .map_err(|e| {
            ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "engine_unavailable",
                e.to_string(),
            )
        })?
        .map_err(|failure| match failure {
            DiarizeFailure::Audio(detail) => {
                ApiError::coded(StatusCode::UNPROCESSABLE_ENTITY, "bad_audio", detail)
            }
            DiarizeFailure::NotInstalled(id) => ApiError::coded(
                StatusCode::CONFLICT,
                "model_not_installed",
                format!("{id} is not installed"),
            ),
            DiarizeFailure::Engine(detail) => {
                tracing::error!(%detail, "the diarization engine failed");
                ApiError::coded(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "engine_unavailable",
                    "the diarization engine failed; the server log has the detail",
                )
            }
        })?;

    Ok(Json(DiarizeResponse::from_turns(turns, seconds)))
}

/// Response body for `POST /v1/stt`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct SttResponse {
    /// What the engine heard, after the user dictionary.
    pub text: String,
    /// Length of the audio, in seconds.
    pub seconds: f64,
}

/// Transcribe an uploaded WAV file.
///
/// The body is the audio itself, not a JSON envelope: a client that already has a
/// recording should not have to base64 it.
#[utoipa::path(
    post,
    path = "/v1/stt",
    tag = "stt",
    summary = "Transcribe an uploaded file",
    description = "The body is the audio itself, not a JSON envelope.",
    request_body(content = Vec<u8>, content_type = "audio/wav"),
    responses(
        (status = 200, body = SttResponse),
        (status = 415, description = "Not a WAV, or an unsupported rate", body = Problem),
        (status = 409, description = "The model is not installed", body = Problem)
    )
)]
async fn transcribe_audio(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<SttResponse>, ApiError> {
    if !state.config().stt.enabled {
        return Err(ApiError::coded(
            StatusCode::NOT_FOUND,
            "engine_disabled",
            "speech to text is switched off; PATCH /v1/config turns it on",
        ));
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // An absent type is accepted: curl with --data-binary sends none, and the bytes
    // either parse as a WAV or they do not.
    if !matches!(
        content_type.as_str(),
        "audio/wav" | "audio/x-wav" | "audio/wave" | "application/octet-stream" | ""
    ) {
        return Err(ApiError::coded(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_format",
            format!("{content_type} is not audio/wav"),
        ));
    }
    if body.is_empty() {
        return Err(ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_audio",
            "the request body is empty",
        ));
    }

    let worker = Arc::clone(&state);
    let (text, seconds) = tokio::task::spawn_blocking(move || worker.transcribe(&body))
        .await
        .map_err(|e| {
            ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "engine_unavailable",
                e.to_string(),
            )
        })?
        .map_err(|failure| match failure {
            SttFailure::Audio(detail) => {
                ApiError::coded(StatusCode::UNPROCESSABLE_ENTITY, "bad_audio", detail)
            }
            SttFailure::Engine(detail) => {
                // The runtime's message can name a path or a library symbol. It belongs in
                // the log, where the operator can use it, not in a response, where it is a
                // map of the machine handed to whoever asked.
                tracing::error!(%detail, "the speech engine failed");
                ApiError::coded(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "engine_unavailable",
                    "the speech engine failed; the server log has the detail",
                )
            }
            SttFailure::NotInstalled(id) => ApiError::coded(
                StatusCode::CONFLICT,
                "model_not_installed",
                format!("{id} is not installed. Run `arioso models download {id}`."),
            ),
        })?;

    Ok(Json(SttResponse {
        text,
        seconds: (seconds * 100.0).round() / 100.0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arioso_core::alloc_count::measure;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// Count heap traffic in the unit tests, so a memory claim is a bound rather than a note.
    ///
    /// The counters are per thread, so a measurement is the work of the test's own thread
    /// and not of whatever the suite beside it allocated.
    #[global_allocator]
    static ALLOCATOR: arioso_core::alloc_count::Counting = arioso_core::alloc_count::Counting;

    const MANIFEST: &str = include_str!("../../../models/manifest.json");
    const DICT: &str = "HELLO  HH AH0 L OW1\nWORLD  W ER1 L D\n";

    fn state() -> (Arc<AppState>, tempfile::TempDir) {
        state_with(Config::default())
    }

    /// The same server, with a config the test chose.
    fn state_with(mut config: Config) -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        config.models.dir = Some(dir.path().to_path_buf());
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        (
            Arc::new(AppState::new(config, manifest, None).unwrap()),
            dir,
        )
    }

    /// Five sentences, and long enough that the document has to grow to hold them.
    const DOCUMENT: &str = "The quick brown fox jumps over the lazy dog. \
                            Pack my box with five dozen liquor jugs. \
                            How vexingly quick daft zebras jump. \
                            Sphinx of black quartz, judge my vow. \
                            The five boxing wizards jump quickly.";

    #[test]
    fn a_document_is_held_as_pcm_not_floats() {
        // Needs a real voice, like `tests/ws_stream.rs`: the claim is about the buffer the
        // real path builds, and the engine is what produces the chunks it is built from.
        let Ok(dir) = std::env::var("ARIOSO_TEST_MODELS_DIR") else {
            eprintln!("skipped: ARIOSO_TEST_MODELS_DIR is not set");
            return;
        };
        let mut config = Config::default();
        config.models.dir = Some(PathBuf::from(&dir));
        config.tts.voice = Some("en_US-joe-medium".to_owned());
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());
        // The first render loads the graph; the load is not what this measures.
        let _ = state.render(DOCUMENT, 1.0, 0).expect("the voice speaks");

        // The engine alone, with a sink that keeps nothing, so the difference below is the
        // document and not the inference.
        let (_, engine) = measure(|| {
            state
                .render_stream(DOCUMENT, 1.0, 0, |_, _| Ok(()))
                .expect("the voice speaks")
        });
        let ((pcm, _rate), full) = measure(|| state.render(DOCUMENT, 1.0, 0).unwrap());
        let samples = pcm.len() / 2;
        assert!(
            samples > 100_000,
            "the text is too short to grow the document: {samples} samples"
        );

        // Measured at 7.7 bytes a sample; the f32 document this replaced measured 13.5,
        // because the floats are twice the size and every doubling copies them again.
        let collected = full.bytes.saturating_sub(engine.bytes);
        assert!(
            collected <= 9 * samples,
            "collecting {samples} samples allocated {collected} bytes"
        );
        // What is held when the render returns: measured at 3.8 bytes a sample, which is
        // the doubling of a two-byte document. The float document alone held 7.2.
        assert!(
            pcm.capacity() <= 5 * samples,
            "the document holds {} bytes for {samples} samples",
            pcm.capacity()
        );
    }

    async fn call(app: Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
        let (status, body, _) = call_with_headers(app, request).await;
        (status, body)
    }

    /// The same, keeping the headers, for tests that are about a header.
    async fn call_with_headers(
        app: Router,
        request: Request<Body>,
    ) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, body, headers)
    }

    /// Bytes of the toy model the stream tests download.
    const TOY_BYTES: &[u8] = b"arioso toy model, small enough to fetch in a test";

    /// A catalog with one Kokoro entry, for the tests that are about its voice archive.
    const KOKORO_MANIFEST: &str = r#"{"schema_version":1,"models":[
        {"id":"kokoro-82m","kind":"tts","engine":"kokoro","version":"1.0",
         "license":"Apache-2.0","commercial_use":true,
         "files":[
           {"name":"kokoro-v1.0.onnx","url":"https://example.invalid/kokoro.onnx","sha256":"0000000000000000000000000000000000000000000000000000000000000000","size":1},
           {"name":"voices-v1.0.bin","url":"https://example.invalid/voices.bin","sha256":"0000000000000000000000000000000000000000000000000000000000000000","size":1},
           {"name":"kokoro-config.json","url":"https://example.invalid/config.json","sha256":"0000000000000000000000000000000000000000000000000000000000000000","size":1}
         ],
         "meta":{"sample_rate":24000,"languages":["en-US"],"num_speakers":1}}]}"#;

    /// A voice archive holding `rows` by `width` of zeros per name, in the shape
    /// `numpy.savez` writes: a zip whose entries are stored `.npy` arrays.
    fn test_archive(voices: &[(&str, usize, usize)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, rows, width) in voices {
            let header = format!(
                "{{'descr': '<f4', 'fortran_order': False, 'shape': ({rows}, 1, {width}), }}"
            );
            let mut npy = vec![0x93, b'N', b'U', b'M', b'P', b'Y', 1, 0];
            npy.extend_from_slice(&(header.len() as u16).to_le_bytes());
            npy.extend_from_slice(header.as_bytes());
            npy.resize(npy.len() + rows * width * 4, 0);
            let file_name = format!("{name}.npy");
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // stored
            out.extend_from_slice(&[0; 4]); // time and date
            out.extend_from_slice(&0u32.to_le_bytes()); // crc
            out.extend_from_slice(&(npy.len() as u32).to_le_bytes()); // compressed
            out.extend_from_slice(&(npy.len() as u32).to_le_bytes()); // uncompressed
            out.extend_from_slice(&(file_name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(file_name.as_bytes());
            out.extend_from_slice(&npy);
        }
        out
    }

    /// A catalog with one model small enough to fetch inside a test.
    ///
    /// The URL names a host that does not resolve: the tests point the config at a loopback
    /// mirror, so a test that reached the origin would fail loudly instead of passing on
    /// whatever network happened to be available.
    fn toy_manifest() -> Manifest {
        use sha2::Digest as _;
        let digest: String = sha2::Sha256::digest(TOY_BYTES)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let document = serde_json::json!({
            "schema_version": 1,
            "models": [{
                "id": "toy",
                "kind": "tts",
                "engine": "piper",
                "version": "2026.11",
                "license": "MIT",
                "commercial_use": true,
                "files": [{
                    "name": "toy.onnx",
                    "url": "https://example.invalid/toy.onnx",
                    "sha256": digest,
                    "size": TOY_BYTES.len(),
                }],
            }],
        });
        Manifest::parse(document.to_string().as_bytes()).unwrap()
    }

    fn toy_state(mirror: &str) -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.models.mirror = Some(mirror.to_owned());
        config.models.max_concurrent_downloads = 1;
        // The catalog holds one voice, and a config naming another is refused at startup, so
        // this one names the toy.
        config.tts.voice = Some("toy".into());
        (
            Arc::new(AppState::new(config, toy_manifest(), None).unwrap()),
            dir,
        )
    }

    /// A loopback HTTP server that serves [`TOY_BYTES`] at every path, and its base URL.
    ///
    /// A mirror is the supported way to fetch through something other than the origin, so
    /// the download under test is the real one, through the real downloader.
    async fn toy_mirror() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route("/{*path}", get(|| async { TOY_BYTES }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    /// The next frame of a stream, waiting at most ten seconds for one.
    async fn next_frame(body: &mut axum::body::Body) -> String {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), body.frame())
            .await
            .expect("a frame within ten seconds")
            .expect("the stream stays open")
            .expect("a data frame");
        String::from_utf8(frame.into_data().unwrap().to_vec()).unwrap()
    }

    /// The JSON of an `event:`/`data:` frame.
    fn frame_data(frame: &str) -> serde_json::Value {
        let data = frame
            .trim()
            .split("data: ")
            .nth(1)
            .unwrap_or_else(|| panic!("no data line in {frame:?}"));
        serde_json::from_str(data).unwrap()
    }

    /// Open the event stream through the router, as a client would.
    async fn open_events(state: &Arc<AppState>) -> axum::body::Body {
        let response = router(Arc::clone(state))
            .oneshot(
                Request::builder()
                    .uri("/v1/models/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream",
            "the route must answer as an event stream"
        );
        response.into_body()
    }

    #[tokio::test]
    async fn the_event_stream_opens_with_a_snapshot_then_follows_a_download() {
        let mirror = toy_mirror().await;
        let (state, _dir) = toy_state(&mirror);
        let spec = state.manifest.find("toy").cloned().expect("the toy model");

        let mut body = open_events(&state).await;
        let snapshot = next_frame(&mut body).await;
        assert!(snapshot.starts_with("event: snapshot\n"), "{snapshot}");
        let models = frame_data(&snapshot);
        let toy = models
            .as_array()
            .expect("the snapshot is a list")
            .iter()
            .find(|model| model["id"] == "toy")
            .expect("the toy model is in the snapshot");
        assert_eq!(toy["state"], "not_installed", "{snapshot}");

        state.begin_download(&spec);

        let mut frames = Vec::new();
        let last = loop {
            let frame = next_frame(&mut body).await;
            if frame.starts_with(": ") {
                // A keep-alive comment, not an event.
                continue;
            }
            frames.push(frame.clone());
            if frame.contains("\"state\":\"installed\"") {
                break frame;
            }
        };
        assert!(
            last.starts_with("event: state\n"),
            "the end of a download is a state event: {last}"
        );
        assert!(
            frames
                .iter()
                .any(|frame| frame.starts_with("event: state\n")
                    && frame.contains("\"downloading\"")),
            "a state event reports the download starting: {frames:?}"
        );
        assert!(
            frames
                .iter()
                .any(|frame| frame.starts_with("event: progress\n")),
            "bytes are reported as progress events: {frames:?}"
        );
        let installed = frame_data(&last);
        assert_eq!(installed["downloaded"], TOY_BYTES.len() as u64);
        assert!(installed["error"].is_null(), "{installed}");
    }

    #[tokio::test]
    async fn two_subscribers_both_see_the_same_download() {
        let mirror = toy_mirror().await;
        let (state, _dir) = toy_state(&mirror);
        let spec = state.manifest.find("toy").cloned().expect("the toy model");

        let mut first = open_events(&state).await;
        let mut second = open_events(&state).await;
        assert!(
            next_frame(&mut first)
                .await
                .starts_with("event: snapshot\n")
        );
        assert!(
            next_frame(&mut second)
                .await
                .starts_with("event: snapshot\n")
        );

        state.begin_download(&spec);

        for (name, body) in [("first", &mut first), ("second", &mut second)] {
            let mut started = false;
            let mut finished = false;
            while !finished {
                let frame = next_frame(body).await;
                started |= frame.contains("\"downloading\"");
                finished = frame.contains("\"state\":\"installed\"");
            }
            assert!(started, "{name} missed the download starting");
        }
    }

    #[tokio::test]
    async fn a_subscriber_that_leaves_does_not_stop_the_download() {
        let mirror = toy_mirror().await;
        let (state, _dir) = toy_state(&mirror);
        let spec = state.manifest.find("toy").cloned().expect("the toy model");

        // A client that connects and goes away before anything happens.
        let abandoned = open_events(&state).await;
        drop(abandoned);

        state.begin_download(&spec);

        // The download runs to completion with nobody listening.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !state.is_installed(&spec) {
            assert!(
                std::time::Instant::now() < deadline,
                "the download did not finish with no subscriber"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // The next subscriber is told where things stand, without waiting for a change.
        let mut body = open_events(&state).await;
        let snapshot = next_frame(&mut body).await;
        assert!(snapshot.starts_with("event: snapshot\n"), "{snapshot}");
        assert!(snapshot.contains("\"state\":\"installed\""), "{snapshot}");
    }

    #[tokio::test]
    async fn an_idle_stream_sends_a_comment_so_a_proxy_keeps_it_open() {
        // Time is paused: the comment arrives on a fifteen second timer, and a test that
        // waited for it in real time would be a test nobody runs.
        let (state, _dir) = state();
        let mut body = open_events(&state).await;
        assert!(next_frame(&mut body).await.starts_with("event: snapshot\n"));

        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(15)).await;
        let idle = next_frame(&mut body).await;
        assert_eq!(
            idle, ": keep-alive\n\n",
            "an idle stream comments, not events"
        );
    }

    #[tokio::test]
    async fn a_subscriber_that_falls_behind_is_told_and_gets_a_new_snapshot() {
        let (state, _dir) = state();
        let id = state.manifest.models.first().unwrap().id.clone();
        let mut body = open_events(&state).await;
        assert!(next_frame(&mut body).await.starts_with("event: snapshot\n"));

        // More events than the channel holds, with nobody reading them: the subscriber has
        // fallen behind by construction.
        for step in 0..300 {
            state.note_download(&id, step, 1000);
        }

        let frame = next_frame(&mut body).await;
        assert!(frame.contains("event: lagged\n"), "{frame}");
        assert!(
            frame.contains("\"missed\":"),
            "the client is told how far behind it fell: {frame}"
        );
        assert!(
            frame.contains("event: snapshot\n"),
            "and handed a fresh snapshot rather than a gap: {frame}"
        );
    }

    #[tokio::test]
    async fn the_document_names_every_route() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let paths = document["paths"].as_object().expect("a paths object");
        // Every route the router registers, except the two WebSockets and the Swagger
        // UI, which have no schema to derive.
        for route in [
            "/healthz",
            "/readyz",
            "/v1/info",
            "/v1/engines",
            "/v1/voices",
            "/v1/config",
            "/v1/models",
            "/v1/models/events",
            "/v1/tts",
            "/v1/stt",
        ] {
            assert!(
                paths.contains_key(route),
                "{route} is missing from the document"
            );
        }
        assert_eq!(document["openapi"], "3.1.0", "{document}");
        assert_eq!(document["info"]["title"], "arioso");
    }

    #[tokio::test]
    async fn info_names_the_engines_the_binary_serves() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/v1/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        let engines = body["engines"].as_array().unwrap();
        assert!(engines.iter().any(|id| id == "piper"), "{body}");
        assert!(engines.iter().any(|id| id == "sherpa-whisper"), "{body}");
    }

    #[tokio::test]
    async fn engines_report_what_they_can_do() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/v1/engines")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let engines = body["engines"].as_array().unwrap();
        let piper = engines.iter().find(|e| e["id"] == "piper").unwrap();
        assert_eq!(piper["kind"], "tts");
        assert_eq!(piper["streaming"], true);
        assert!(piper["models"].as_u64().unwrap() > 0, "{body}");
        assert!(
            piper["languages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l.as_str().unwrap().starts_with("en"))
        );
    }

    #[tokio::test]
    async fn a_disabled_engine_is_absent_from_the_list() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.stt.enabled = false;
        config.models.dir = Some(dir.path().to_path_buf());
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());
        let (_, body) = call(
            router(state),
            Request::builder()
                .uri("/v1/engines")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let engines = body["engines"].as_array().unwrap();
        assert!(engines.iter().all(|e| e["kind"] != "stt"), "{body}");
    }

    #[tokio::test]
    async fn voices_are_listed_and_the_active_one_is_marked() {
        let (state, _dir) = state();
        // The config names the voice by its bare name; the catalog answers with an id.
        let active = state.voice_spec().unwrap().id.clone();
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/v1/voices")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let voices = body["voices"].as_array().unwrap();
        assert!(voices.len() >= 5, "the catalog holds five voices: {body}");
        let marked: Vec<_> = voices
            .iter()
            .filter(|voice| voice["active"] == true)
            .collect();
        assert_eq!(marked.len(), 1, "exactly one voice is active: {body}");
        assert_eq!(marked[0]["id"], active);
        // A voice carries the licence a client needs to know about before shipping it.
        assert!(voices.iter().all(|voice| voice["license"].is_string()));
    }

    #[tokio::test]
    async fn voices_are_filtered_by_language_prefix() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/v1/voices?language=en")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let voices = body["voices"].as_array().unwrap();
        assert!(!voices.is_empty(), "en must match en_US: {body}");
        assert!(voices.iter().all(|voice| {
            voice["languages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l.as_str().unwrap().starts_with("en"))
        }));
    }

    #[tokio::test]
    async fn a_language_is_found_whichever_separator_it_uses() {
        let (state, _dir) = state();
        for query in ["en_GB", "en-GB", "en_gb"] {
            let (status, body) = call(
                router(state.clone()),
                Request::builder()
                    .uri(format!("/v1/voices?language={query}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let voices = body["voices"].as_array().unwrap();
            assert!(
                !voices.is_empty(),
                "a voice id writes en_GB and the catalog writes en-GB: {query} found nothing"
            );
        }
    }

    #[tokio::test]
    async fn a_language_nobody_speaks_returns_no_voices() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/v1/voices?language=zz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(body["voices"].as_array().unwrap().is_empty(), "{body}");
    }

    async fn patch(state: Arc<AppState>, body: &str) -> (StatusCode, serde_json::Value) {
        let (status, bytes) = call(
            router(state),
            Request::builder()
                .method("PATCH")
                .uri("/v1/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await;
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn a_patch_changes_only_the_fields_it_names() {
        let (state, _dir) = state();
        let before = state.config();
        let (status, body) = patch(state.clone(), r#"{"tts":{"speed":1.4}}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["tts"]["speed"], 1.4);
        // Everything else is untouched.
        assert_eq!(state.config().tts.voice, before.tts.voice);
        assert_eq!(state.config().tts.format, before.tts.format);
    }

    #[tokio::test]
    async fn a_patch_body_is_the_config_document_byte_for_byte() {
        // The response is the config struct serialised once, with the note flattened in
        // beside it. A client that reads a field must see the same document as before,
        // including the float: 1.4 is written by the f32 formatter, not as the f64 it
        // widens to.
        let (state, _dir) = state();
        let (status, body) = call(
            router(state.clone()),
            Request::builder()
                .method("PATCH")
                .uri("/v1/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"tts":{"speed":1.4}}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(body).unwrap();
        assert_eq!(
            text,
            serde_json::to_string(&*state.config()).unwrap(),
            "the body is the document, unchanged"
        );
        assert!(text.contains(r#""speed":1.4"#), "{text}");
    }

    #[tokio::test]
    async fn a_patch_that_replaces_a_voice_says_so_beside_the_document() {
        // The note is the one thing the response adds. It sits after every config field,
        // so nothing a client already reads moves.
        let (state, _dir) = state();
        // A named voice, so the switch to the other engine replaces it and owes a note.
        let (status, _) = patch(state.clone(), r#"{"tts":{"voice":"en_US-lessac-medium"}}"#).await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = call(
            router(state.clone()),
            Request::builder()
                .method("PATCH")
                .uri("/v1/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"tts":{"engine":"kokoro"}}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(body).unwrap();
        let document = serde_json::to_string(&*state.config()).unwrap();
        let fields = document.strip_suffix('}').expect("a document is an object");
        assert!(
            text.starts_with(fields),
            "every config field is where it was: {text}"
        );
        let notes = text
            .strip_prefix(fields)
            .expect("the note follows the fields");
        assert!(notes.starts_with(r#","notes":["#), "{notes}");
        assert!(notes.ends_with("]}"), "{notes}");
    }

    #[tokio::test]
    async fn an_unknown_field_is_refused() {
        let (state, _dir) = state();
        let before = state.config();
        let (status, _) = patch(state.clone(), r#"{"tts":{"speeed":1.4}}"#).await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::BAD_REQUEST,
            "a typo must not look like success, got {status}"
        );
        assert_eq!(state.config(), before, "a refused patch changes nothing");
    }

    #[tokio::test]
    async fn a_value_out_of_range_is_refused_and_nothing_is_applied() {
        let (state, _dir) = state();
        let before = state.config();
        let (status, body) = patch(state.clone(), r#"{"tts":{"speed":5.0,"format":"opus"}}"#).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], "invalid_value");
        assert_eq!(
            state.config().tts.format,
            before.tts.format,
            "the valid half of a refused patch must not be applied"
        );
    }

    #[tokio::test]
    async fn an_engine_that_is_not_built_is_refused_with_the_alternatives() {
        let (state, _dir) = state();
        let (status, body) = patch(state, r#"{"tts":{"engine":"espeak"}}"#).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let detail = body["detail"].as_str().unwrap();
        assert!(
            detail.contains("piper") && detail.contains("kokoro"),
            "the refusal names what works: {detail}"
        );
    }

    #[tokio::test]
    async fn a_patch_that_changes_the_engine_brings_that_engine_a_voice() {
        // The two engines share no voice names, so a patch naming only the engine would
        // leave `tts.voice` asking for something the new engine cannot say. The field is
        // cleared rather than filled: the new engine's own default answers for it, and the
        // config does not end up naming a voice nobody chose. The response says what will
        // be spoken rather than changing it quietly.
        let (state, _dir) = state();

        // A config that names no voice switches engines with nothing to say: the new
        // engine's default was already the answer.
        assert_eq!(state.config().tts.voice, None);
        let (status, body) = patch(state.clone(), r#"{"tts":{"engine":"kokoro"}}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(state.config().voice_name(), "af_heart");
        assert!(
            body["notes"].as_array().is_none_or(Vec::is_empty),
            "nothing was replaced, so nothing is owed: {body}"
        );

        // A config that named a Piper voice and is switched to Kokoro loses that voice, and
        // that is worth a sentence: it says which voice went and which one speaks now.
        let (status, _) = patch(state.clone(), r#"{"tts":{"engine":"piper"}}"#).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = patch(state.clone(), r#"{"tts":{"voice":"en_US-lessac-medium"}}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state.config().tts.voice.as_deref(),
            Some("en_US-lessac-medium")
        );

        let (status, body) = patch(state.clone(), r#"{"tts":{"engine":"kokoro"}}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["tts"]["voice"].is_null(),
            "the stored field is cleared, not filled: {body}"
        );
        assert_eq!(state.config().voice_name(), "af_heart");
        let notes = body["notes"].as_array().expect("the change is said");
        let note = notes[0].as_str().unwrap();
        assert!(note.contains("af_heart"), "{note}");
        assert!(note.contains("en_US-lessac-medium"), "{note}");
        // The field was cleared, not written: the note must not claim a value it did not set.
        assert!(note.contains("unset"), "{note}");
        assert!(
            !note.contains("was set to"),
            "the field is cleared, so nothing was set: {note}"
        );
    }

    #[tokio::test]
    async fn a_patch_that_names_the_voice_keeps_it() {
        // A caller that names both is not second-guessed, and no note is owed: nothing
        // was changed that was not asked for.
        let (state, _dir) = state();
        let (status, body) = patch(
            state.clone(),
            r#"{"tts":{"engine":"kokoro","voice":"af_heart"}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(state.config().voice_name(), "af_heart");
        assert!(body["notes"].is_null(), "{body}");
    }

    /// A Kokoro server over the test catalog, with an archive holding two styles.
    ///
    /// The slot is empty: a test binary has no 325 MB graph to load, so what these tests
    /// observe is which path a patch chose — the in-place swap or the drop. The loaded
    /// case is [`a_voice_only_patch_keeps_the_real_graph_loaded`], which needs the files.
    fn kokoro_voice_state() -> (Arc<AppState>, tempfile::TempDir) {
        let mut config = Config::default();
        config.tts.engine = "kokoro".to_owned();
        config.tts.voice = Some("af_heart".to_owned());
        let (state, dir) = state_with(config);
        std::fs::write(
            dir.path().join("voices-v1.0.bin"),
            test_archive(&[("af_heart", 2, 2), ("af_bella", 2, 2)]),
        )
        .unwrap();
        (state, dir)
    }

    /// Style swaps attempted, and engines dropped, by the patches so far.
    fn swaps_and_drops(state: &AppState) -> (usize, usize) {
        (
            state.style_swaps.load(std::sync::atomic::Ordering::Relaxed),
            state.voice_drops.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    #[tokio::test]
    async fn a_voice_only_patch_swaps_the_style_instead_of_dropping_the_engine() {
        // Every Kokoro voice is the same graph and the same vocabulary: only the style
        // table differs. Dropping the slot for a different style re-reads and re-compiles
        // 325 MB to arrive at a different row of a table already in memory.
        let (state, _dir) = kokoro_voice_state();
        let (status, body) = patch(state.clone(), r#"{"tts":{"voice":"af_bella"}}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(state.config().voice_name(), "af_bella");
        assert_eq!(
            swaps_and_drops(&state),
            (1, 0),
            "the patch must swap the style and keep the graph"
        );
        // The caller asked for a voice and got it; nothing was replaced behind its back.
        assert!(body["notes"].is_null(), "{body}");
    }

    #[tokio::test]
    async fn a_patch_that_names_the_same_voice_keeps_the_engine() {
        let (state, _dir) = kokoro_voice_state();
        let (status, body) = patch(state.clone(), r#"{"tts":{"voice":"af_heart"}}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            swaps_and_drops(&state),
            (0, 0),
            "a patch that changes nothing must not cost a reload"
        );
    }

    #[tokio::test]
    async fn a_changed_engine_still_drops_the_loaded_engine() {
        // One slot, one engine: a patch that changes the engine must not leave the old
        // graph answering for the new one.
        let (state, _dir) = state();
        let (status, body) = patch(state.clone(), r#"{"tts":{"engine":"kokoro"}}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            swaps_and_drops(&state),
            (0, 1),
            "a different engine is a different graph"
        );
    }

    #[tokio::test]
    async fn a_piper_voice_change_drops_the_loaded_voice() {
        // A Piper voice is a model, not a row in a table: a new name is a new graph.
        let (state, _dir) = state();
        let (status, body) =
            patch(state.clone(), r#"{"tts":{"voice":"en_US-lessac-medium"}}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            swaps_and_drops(&state),
            (0, 1),
            "one model per voice means a reload"
        );
    }

    /// The same patch against the real files: the graph is still in memory afterwards.
    ///
    /// Ignored by default, because the graph is 325 MB and the styles 28 MB. Run it with
    /// `ARIOSO_TEST_MODELS_DIR=<dir> cargo test -p arioso-server -- --ignored`.
    #[tokio::test]
    #[ignore = "needs the real kokoro files in ARIOSO_TEST_MODELS_DIR"]
    async fn a_voice_only_patch_keeps_the_real_graph_loaded() {
        let dir = std::env::var_os("ARIOSO_TEST_MODELS_DIR")
            .map(std::path::PathBuf::from)
            .expect("set ARIOSO_TEST_MODELS_DIR to the directory holding the Kokoro files");
        let mut config = Config::default();
        config.tts.engine = "kokoro".to_owned();
        config.tts.voice = Some("af_heart".to_owned());
        config.models.dir = Some(dir);
        let manifest =
            Manifest::parse(include_str!("../../../models/manifest.json").as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());
        state.warm_voice().expect("loading the model");

        let (status, body) = patch(state.clone(), r#"{"tts":{"voice":"af_bella"}}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let guard = state.voice.lock().unwrap();
        match guard.as_ref() {
            // The same graph, a different row of the archive: the session was kept and the
            // style swapped under it.
            Some(Voice::Kokoro(voice)) => assert_eq!(voice.voice(), "af_bella"),
            _ => panic!("a voice-only patch dropped the loaded graph"),
        }
    }

    #[tokio::test]
    async fn switching_the_engine_is_accepted_and_drops_the_loaded_voice() {
        // The slot holds one engine, so a patch that changes it must not leave a Piper
        // voice answering for Kokoro.
        let (state, _dir) = state();
        let (status, body) = patch(
            state.clone(),
            r#"{"tts":{"engine":"kokoro","voice":"af_heart"}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(state.config().tts.engine, "kokoro");
        assert_eq!(state.config().voice_name(), "af_heart");
    }

    #[test]
    fn a_model_id_is_read_as_the_engines_default_style() {
        // A catalog lists files. A front end that sends a row back sends the id it was given,
        // and for Kokoro that id is the file, not a voice.
        assert_eq!(
            AppState::style_for("kokoro-82m", "kokoro-82m", "kokoro"),
            Some("af_heart")
        );
        // A name that is neither the model nor a style is still nobody's business but the
        // archive's: it must reach the engine and be refused by name.
        assert_eq!(
            AppState::style_for("xx_nobody", "kokoro-82m", "kokoro"),
            None
        );
        // And a real style is left alone.
        assert_eq!(
            AppState::style_for("af_bella", "kokoro-82m", "kokoro"),
            None
        );
    }

    #[test]
    fn a_kokoro_models_styles_are_read_from_its_archive() {
        // The manifest says one speaker; the archive holds the styles. A caller that lists
        // voices needs the archive's answer, and without it the only name on offer is the
        // model's, which no style answers to.
        let dir = tempfile::tempdir().unwrap();
        let archive = test_archive(&[("af_heart", 2, 2), ("af_bella", 2, 2)]);
        std::fs::write(dir.path().join("voices-v1.0.bin"), archive).unwrap();
        let manifest = Manifest::parse(KOKORO_MANIFEST.as_bytes()).unwrap();
        let spec = manifest.find("kokoro-82m").expect("the catalog holds it");
        let names = archive_names(dir.path(), spec).expect("the archive is there");
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().any(|name| name == "af_heart"), "{names:?}");
        assert!(names.iter().any(|name| name == "af_bella"), "{names:?}");
    }

    #[tokio::test]
    async fn a_kokoro_voice_the_archive_does_not_hold_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        // An archive holding one style, built the way numpy writes one.
        let archive = test_archive(&[("af_heart", 2, 2)]);
        std::fs::write(dir.path().join("voices-v1.0.bin"), archive).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        // A catalog of one Kokoro model cannot answer for a Piper voice, so the config names
        // the engine the catalog holds.
        config.tts.engine = "kokoro".to_owned();
        config.tts.voice = Some("af_heart".to_owned());
        let manifest = Manifest::parse(KOKORO_MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());

        let (status, body) =
            patch(state, r#"{"tts":{"engine":"kokoro","voice":"xx_nobody"}}"#).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let detail = body["detail"].as_str().unwrap();
        assert!(detail.contains("xx_nobody"), "{detail}");
    }

    #[tokio::test]
    async fn a_voice_outside_the_catalog_is_refused() {
        let (state, _dir) = state();
        let (status, body) = patch(state, r#"{"tts":{"voice":"piper-xx_YY-nobody"}}"#).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }

    #[tokio::test]
    async fn persisting_without_a_config_file_is_a_conflict() {
        let (state, _dir) = state();
        let (status, body) = patch(state, r#"{"persist":true,"tts":{"speed":1.2}}"#).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "no_config_file");
    }

    #[tokio::test]
    async fn a_patch_survives_a_reload_when_it_persists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let path = dir.path().join("config.json");
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.save(&path).unwrap();
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, Some(&path)).unwrap());

        let (status, body) = patch(state, r#"{"persist":true,"tts":{"speed":1.7}}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let reloaded = Config::load(&path).unwrap();
        assert_eq!(reloaded.tts.speed, 1.7, "the file holds the change");
    }

    #[tokio::test]
    async fn the_config_is_readable_and_carries_no_token() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/v1/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["tts"]["engine"], "piper");
        assert!(
            body["server"]["auth"].get("token").is_none(),
            "the token lives in the environment, not the document"
        );
    }

    #[tokio::test]
    async fn the_document_leaves_out_a_disabled_route() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.stt.enabled = false;
        config.models.dir = Some(dir.path().to_path_buf());
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            document["paths"].get("/v1/stt").is_none(),
            "a route that is not registered must not be promised: {document}"
        );
        assert!(document["paths"].get("/v1/tts").is_some(), "{document}");
    }

    #[tokio::test]
    async fn swagger_ui_is_served() {
        let (state, _dir) = state();
        // /docs redirects to /docs/, which is where the UI lives.
        let (status, _) = call(
            router(state.clone()),
            Request::builder().uri("/docs").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/docs/")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("swagger"), "{}", &html[..html.len().min(200)]);
    }

    async fn post_stt(
        state: Arc<AppState>,
        content_type: &str,
        body: Vec<u8>,
    ) -> (StatusCode, serde_json::Value) {
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/stt")
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn speech_to_text_is_not_registered_when_it_is_off() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.stt.enabled = false;
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());

        let (status, _) = post_stt(state, "audio/wav", vec![1, 2, 3]).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_model_that_is_not_downloaded_is_named() {
        let (state, _dir) = state();
        // The test directory holds no whisper files, so the request reaches the
        // installed check and stops there.
        let wav = wav::to_bytes(&[0.0; 160], 16_000);
        let (status, body) = post_stt(state, "audio/wav", wav).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "model_not_installed");
        // Named from the config, so the test does not pin the default model id.
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains(&Config::default().stt.batch_model),
            "{body}"
        );
    }

    async fn post_diarize(
        state: Arc<AppState>,
        content_type: &str,
        body: Vec<u8>,
    ) -> (StatusCode, serde_json::Value) {
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/diarize")
                    .header(header::CONTENT_TYPE, content_type)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn a_diarization_request_for_models_that_are_not_installed_is_refused() {
        // The config names the entry and the directory is empty, so the answer is the same
        // one `/v1/stt` gives for a missing model, with the catalog id a client can fetch.
        let (state, _dir) = state();
        let pcm = vec![0_u8; 32_000];
        let wav = arioso_tts_piper::wav::to_bytes_from_pcm(&pcm, 16_000);
        let (status, body) = post_diarize(state, "audio/wav", wav).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "model_not_installed");
        assert!(
            body["detail"].as_str().unwrap().contains("diarization-en"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_diarization_body_that_is_not_a_wav_is_refused() {
        let (state, _dir) = state();
        let (status, body) = post_diarize(state, "audio/wav", b"not a wav".to_vec()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "bad_audio");
    }

    #[tokio::test]
    async fn a_wrong_content_type_is_refused() {
        let (state, _dir) = state();
        let (status, body) = post_stt(state, "text/plain", b"hello".to_vec()).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(body["code"], "unsupported_format");
        assert!(body["detail"].as_str().unwrap().contains("text/plain"));
    }

    #[tokio::test]
    async fn bytes_that_are_not_audio_are_refused() {
        let (state, _dir) = state();
        let (status, body) = post_stt(state, "audio/wav", b"not a wav at all".to_vec()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "bad_audio");
    }

    #[tokio::test]
    async fn an_empty_body_is_refused() {
        let (state, _dir) = state();
        let (status, body) = post_stt(state, "audio/wav", Vec::new()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "bad_audio");
        assert!(body["detail"].as_str().unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn healthz_is_ok() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"ok");
    }

    #[tokio::test]
    async fn models_are_listed_with_their_state() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let models = json["models"].as_array().expect("models is a list");
        // The catalog is sorted by language, so the first entry is not a fixed voice. The
        // test names the voice it wants instead of trusting the order.
        assert!(models.len() >= 5, "the catalog holds five voices at least");
        assert!(
            models[0]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("piper-"))
        );
        let lessac = models
            .iter()
            .find(|model| model["id"] == "piper-en_US-lessac-medium")
            .expect("lessac is in the catalog");
        assert_eq!(lessac["kind"], "tts");
        assert_eq!(lessac["installed"], false);
    }

    #[tokio::test]
    async fn readiness_reports_a_missing_voice() {
        let (state, _dir) = state();
        // Read from the config rather than naming the default, so changing the default is a
        // one line change and not a failure in a readiness test.
        let expected = format!("piper-{}", state.config().voice_name());
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        // An orchestrator reads the status code, not the body. 200 with `voice_missing`
        // told a health check the process was fine when it could not serve one request.
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "voice_missing");
        assert_eq!(json["voice"], expected);
        assert_eq!(json["voice_installed"], false);
    }

    #[tokio::test]
    async fn the_catalog_reports_a_download_in_flight() {
        let (state, _dir) = state();
        let id = state.manifest.models.first().unwrap().id.clone();
        state.note_download(&id, 40, 100);
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let entry = json["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == id)
            .expect("the model is listed");
        assert_eq!(entry["state"], "downloading");
        assert_eq!(entry["downloaded"], 40);
        assert_eq!(entry["commercial_use"], false, "lessac is not commercial");
    }

    /// A server whose Kokoro model is installed, holding `voices` in its archive.
    ///
    /// The graph and the symbol table are one byte each: nothing here loads a session,
    /// and the catalog's sizes are set to what was written so the model reads as
    /// installed.
    fn kokoro_state(voices: &[(&str, usize, usize)]) -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let archive = test_archive(voices);
        std::fs::write(dir.path().join("voices-v1.0.bin"), &archive).unwrap();
        std::fs::write(dir.path().join("kokoro-v1.0.onnx"), b"x").unwrap();
        // A real symbol table, so a load reaches the archive before the graph: the graph
        // itself is a byte and would fail as an ONNX session.
        std::fs::write(
            dir.path().join("kokoro-config.json"),
            br#"{"vocab":{"a":1,".":4," ":16}}"#,
        )
        .unwrap();
        let mut manifest = Manifest::parse(KOKORO_MANIFEST.as_bytes()).unwrap();
        for model in &mut manifest.models {
            for file in &mut model.files {
                file.size = std::fs::metadata(dir.path().join(&file.name))
                    .unwrap()
                    .len();
            }
        }
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.tts.engine = "kokoro".to_owned();
        config.tts.voice = Some("af_heart".to_owned());
        (
            Arc::new(AppState::new(config, manifest, None).unwrap()),
            dir,
        )
    }

    #[tokio::test]
    async fn readiness_checks_that_the_kokoro_voice_is_in_the_archive() {
        // Installed files are not enough: the style has to be one the archive holds, or
        // the first request fails to load it.
        let (state, _dir) = kokoro_state(&[("af_heart", 2, 2)]);
        let (_, body) = call(
            router(state),
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["voice"], "af_heart",
            "the style that was checked is named"
        );
        assert_eq!(json["voice_installed"], true);
    }

    #[tokio::test]
    async fn readiness_is_not_ready_when_the_kokoro_voice_is_absent() {
        // The same model, installed, holding a different style: nothing can be spoken.
        let (state, _dir) = kokoro_state(&[("am_michael", 2, 2)]);
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "voice_missing");
        assert_eq!(json["voice"], "af_heart");
        assert_eq!(json["voice_installed"], false);
    }

    #[tokio::test]
    async fn warming_a_voice_the_archive_does_not_hold_is_the_callers_error() {
        // Every file is installed, so this is not a missing model, and no retry can make
        // the style appear: the config names something the engine cannot say.
        let (state, _dir) = kokoro_state(&[("am_michael", 2, 2)]);
        let (status, body) = call(
            router(state),
            Request::builder()
                .method("POST")
                .uri("/v1/models/kokoro-82m/warm")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "invalid_value");
        let detail = json["detail"].as_str().unwrap();
        assert!(detail.contains("af_heart"), "{detail}");
    }

    #[tokio::test]
    async fn readiness_names_the_speech_model_when_that_is_what_is_missing() {
        let (state, dir) = state_of_empty_files();
        // The voice is installed, so readiness moves on to the speech model.
        let voice = state.voice_spec().unwrap().clone();
        for file in &voice.files {
            std::fs::write(dir.path().join(&file.name), b"").unwrap();
        }
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "stt_missing");
        assert_eq!(json["stt_installed"], false);
        assert_eq!(json["voice_installed"], true);
    }

    #[tokio::test]
    async fn readiness_names_the_detector_when_that_is_what_is_missing() {
        // The voice and the speech model are installed, and the config asks for Silero
        // whose model is not. The file route would still transcribe, but every streaming
        // session answers 409, so the server cannot do what it was configured to do.
        let (state, dir) = silero_state();
        let (status, body) = call(
            router(state.clone()),
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "vad_missing");
        assert_eq!(json["vad"], "silero-vad");
        assert_eq!(json["vad_installed"], false);
        assert_eq!(json["stt_installed"], true, "the speech model is there");

        // With the model on disk the same server is ready, and says which detector it runs.
        let vad = state.manifest.find("silero-vad").unwrap().clone();
        for file in &vad.files {
            std::fs::write(dir.path().join(&file.name), b"").unwrap();
        }
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ready");
        assert_eq!(json["vad_installed"], true);
    }

    /// A server whose config names Silero, with the voice and the speech model installed and
    /// the detector absent.
    fn silero_state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        // Every catalog file is zero bytes here, so an empty file is an installed model and
        // a missing one is a model that was never downloaded.
        for spec in &mut manifest.models {
            for file in &mut spec.files {
                file.size = 0;
            }
        }
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.vad.engine = "silero".to_owned();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());
        let voice = state.voice_spec().unwrap().clone();
        for file in &voice.files {
            std::fs::write(dir.path().join(&file.name), b"").unwrap();
        }
        let stt = state.stt_spec().unwrap().clone();
        for file in &stt.files {
            std::fs::write(dir.path().join(&file.name), b"").unwrap();
        }
        (state, dir)
    }

    #[tokio::test]
    async fn a_disabled_engine_does_not_make_the_server_unready() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        for spec in &mut manifest.models {
            for file in &mut spec.files {
                file.size = 0;
            }
        }
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.stt.enabled = false;
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());
        let voice = state.voice_spec().unwrap().clone();
        for file in &voice.files {
            std::fs::write(dir.path().join(&file.name), b"").unwrap();
        }
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ready");
        assert!(json["stt"].is_null(), "a disabled engine is not reported");
    }

    /// Write a dictionary and move its modification time forward, so a second
    /// write in the same clock tick is still a change. Two writes microseconds
    /// apart are indistinguishable by time alone, and a test must not race it.
    fn write_dictionary(path: &std::path::Path, text: &str, forward: u64) {
        std::fs::write(path, text).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(forward))
            .unwrap();
    }

    #[tokio::test]
    async fn readiness_reports_the_user_dictionary() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        // The voice is not installed in this state, so readiness is 503 and the dictionary
        // is still reported: a health check that fails must still say what it found.
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // No file of the user's own, so the shipped dictionary is what applies.
        assert_eq!(json["dictionary"]["entries"], 3);
        assert!(json["dictionary"]["error"].is_null());
    }

    #[tokio::test]
    async fn a_snapshot_is_held_without_holding_the_lock() {
        // The defect this replaces: a render held the dictionary guard for the length of the
        // synthesis, so a transcription submitted while it spoke waited for it, and the
        // reverse. A snapshot copies two `Arc`s and lets go, so a second reader is never
        // behind the first.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let path = dir.path().join("dictionary.json");
        std::fs::write(
            &path,
            r#"{"version": 1, "entries": [{"term": "arioso", "spoken": ["ah ree oh so"]}]}"#,
        )
        .unwrap();

        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.tts.dictionary = Some(path.clone());
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());

        let held = state.user_snapshot();
        assert_eq!(held.dictionary.len(), 1);

        // A second reader while the first snapshot is alive, with a budget short enough that
        // a held lock would miss it.
        let reader = Arc::clone(&state);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let snapshot = reader.user_snapshot();
            let _ = sender.send(snapshot.dictionary.len());
        });
        assert_eq!(
            receiver.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(1),
            "a reader waited for another reader"
        );

        // A reload replaces the shared copy; the snapshot taken before it keeps reading what
        // it started with, so one request sees one dictionary.
        write_dictionary(
            &path,
            r#"{"version": 1, "entries": [
                {"term": "arioso", "spoken": ["ah ree oh so"]},
                {"term": "Sella", "spoken": ["seller"]}
            ]}"#,
            2,
        );
        assert_eq!(state.user_snapshot().dictionary.len(), 2);
        assert_eq!(
            held.dictionary.len(),
            1,
            "a held snapshot changed under the reader"
        );
    }

    #[tokio::test]
    async fn a_changed_dictionary_is_picked_up_without_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let path = dir.path().join("dictionary.json");
        std::fs::write(
            &path,
            r#"{"version": 1, "entries": [{"term": "arioso", "spoken": ["ah ree oh so"]}]}"#,
        )
        .unwrap();

        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.tts.dictionary = Some(path.clone());
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());

        let app = router(Arc::clone(&state));
        let (_, body) = call(
            app,
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["dictionary"]["entries"], 1);

        // Add a second entry and ask again.
        write_dictionary(
            &path,
            r#"{"version": 1, "entries": [
                {"term": "arioso", "spoken": ["ah ree oh so"]},
                {"term": "Sella", "spoken": ["seller"]}
            ]}"#,
            2,
        );
        let (_, body) = call(
            router(Arc::clone(&state)),
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["dictionary"]["entries"], 2);
    }

    #[tokio::test]
    async fn a_broken_dictionary_keeps_the_last_good_copy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let path = dir.path().join("dictionary.json");
        std::fs::write(
            &path,
            r#"{"version": 1, "entries": [{"term": "arioso", "spoken": ["ah ree oh so"]}]}"#,
        )
        .unwrap();

        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.tts.dictionary = Some(path.clone());
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());

        write_dictionary(&path, "{ not json", 2);

        let (_, body) = call(
            router(Arc::clone(&state)),
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["dictionary"]["entries"], 1, "the good copy stays");
        assert!(
            json["dictionary"]["error"].as_str().is_some(),
            "the parse error is reported"
        );
    }

    #[tokio::test]
    async fn empty_text_is_rejected() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .method("POST")
                .uri("/v1/tts")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"   "}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], 422);
        assert!(json["detail"].as_str().unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn every_response_carries_a_request_id() {
        let (state, _dir) = state();
        let app = router(state);
        let (_, _, headers) = call_with_headers(
            app.clone(),
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let generated = headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .expect("an id is generated when the client sends none")
            .to_owned();
        assert!(!generated.is_empty());

        // A token the client sends is kept, so it can follow its own request.
        let (_, _, headers) = call_with_headers(
            app.clone(),
            Request::builder()
                .uri("/healthz")
                .header("x-request-id", "abc-123")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(headers.get("x-request-id").unwrap(), "abc-123");

        // A header that could break the response or the log is replaced, not echoed.
        let (_, _, headers) = call_with_headers(
            app,
            Request::builder()
                .uri("/healthz")
                .header("x-request-id", "bad id with spaces")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_ne!(headers.get("x-request-id").unwrap(), "bad id with spaces");
    }

    #[tokio::test]
    async fn a_patch_can_turn_an_engine_off_and_on_again() {
        // Routes are registered whatever the config says, so the switch works both ways.
        // Registering at startup made it one-way in each direction.
        let (state, _dir) = state();
        let app = router(Arc::clone(&state));
        let (status, _) = call(
            app.clone(),
            Request::builder()
                .method("PATCH")
                .uri("/v1/config")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"tts":{"enabled":false}}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            app.clone(),
            Request::builder()
                .method("POST")
                .uri("/v1/tts")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"hello"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "engine_disabled");

        // The document follows the same state.
        let (_, body) = call(
            app.clone(),
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            document["paths"].get("/v1/tts").is_none(),
            "a switched-off route is still described"
        );

        // And back on.
        let (status, _) = call(
            app.clone(),
            Request::builder()
                .method("PATCH")
                .uri("/v1/config")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"tts":{"enabled":true}}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = call(
            app,
            Request::builder()
                .method("POST")
                .uri("/v1/tts")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"hello"}"#))
                .unwrap(),
        )
        .await;
        // The voice is not installed here, so the answer is 409 rather than 404: the
        // route is live again.
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }

    #[tokio::test]
    async fn a_known_voice_with_no_files_is_model_not_installed() {
        // The catalog knows this voice; its files are not on disk. That is a 409 with a
        // code, not a 422 carrying an operating system path.
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .method("POST")
                .uri("/v1/tts")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"text":"hello"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "model_not_installed");
        let detail = json["detail"].as_str().unwrap();
        assert!(
            !detail.contains("/tmp") && !detail.contains("tempdir"),
            "the detail leaked a path: {detail}"
        );
    }

    #[tokio::test]
    async fn a_config_naming_a_voice_the_catalog_lacks_is_refused_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.tts.voice = Some("no-such-voice".into());
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();

        // A catalog that lists voices can answer whether this name is one of them, so the
        // name is refused where it is written rather than at the first request.
        let error = AppState::new(config, manifest, None)
            .expect_err("a voice the catalog does not hold must not start a server");
        let message = error.to_string();
        assert!(message.contains("no-such-voice"), "{message}");
    }

    #[tokio::test]
    async fn a_generated_pair_is_accepted_and_a_bad_key_is_named() {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, generated.cert.pem()).unwrap();
        std::fs::write(&key, generated.signing_key.serialize_pem()).unwrap();

        assert!(tls_config(&cert, &key).await.is_ok());

        std::fs::write(&key, "not a key").unwrap();
        let error = tls_config(&cert, &key).await.unwrap_err().to_string();
        assert!(error.contains("cert.pem"), "{error}");
        assert!(error.contains("key.pem"), "{error}");
    }

    #[tokio::test]
    async fn a_client_that_spends_its_burst_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.server.limits.requests_per_second = 1;
        config.server.limits.burst = 2;
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());

        let peer: std::net::SocketAddr = "203.0.113.7:5000".parse().unwrap();
        let ask = || {
            Request::builder()
                .uri("/healthz")
                .extension(axum::extract::ConnectInfo(peer))
                .body(Body::empty())
                .unwrap()
        };

        // The burst, then one more than the burst.
        assert_eq!(call(router(state.clone()), ask()).await.0, StatusCode::OK);
        assert_eq!(call(router(state.clone()), ask()).await.0, StatusCode::OK);

        let response = router(state.clone()).oneshot(ask()).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .expect("a refusal must say when to come back");
        assert!(retry >= 1, "retry-after was {retry}");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("rate_limited"), "{text}");
    }

    #[tokio::test]
    async fn another_address_has_its_own_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.server.limits.requests_per_second = 1;
        config.server.limits.burst = 1;
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());

        let ask = |ip: &str| {
            let peer: std::net::SocketAddr = format!("{ip}:5000").parse().unwrap();
            Request::builder()
                .uri("/healthz")
                .extension(axum::extract::ConnectInfo(peer))
                .body(Body::empty())
                .unwrap()
        };

        assert_eq!(
            call(router(state.clone()), ask("203.0.113.7")).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(router(state.clone()), ask("203.0.113.7")).await.0,
            StatusCode::TOO_MANY_REQUESTS
        );
        // A different client is unaffected by the first one's spending.
        assert_eq!(
            call(router(state.clone()), ask("198.51.100.9")).await.0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_configured_token_is_required_and_healthz_is_not() {
        // Edition 2024 marks the environment as unsafe to mutate because another thread
        // may be reading it. This test owns the variable's only use, and the name is
        // unique to it.
        unsafe { std::env::set_var("ARIOSO_TEST_TOKEN_AUTH", "s3cret-token") };

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.server.auth.mode = "bearer".into();
        config.server.auth.token_env = "ARIOSO_TEST_TOKEN_AUTH".into();
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());

        let ask = |path: &str, token: Option<&str>| {
            let mut request = Request::builder().uri(path);
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            request.body(Body::empty()).unwrap()
        };

        // No token, a wrong token, and the right one.
        assert_eq!(
            call(router(state.clone()), ask("/v1/info", None)).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(router(state.clone()), ask("/v1/info", Some("wrong")))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(router(state.clone()), ask("/v1/info", Some("s3cret-token")))
                .await
                .0,
            StatusCode::OK
        );
        // A liveness probe answers without the token, and reveals only that it is alive.
        assert_eq!(
            call(router(state.clone()), ask("/healthz", None)).await.0,
            StatusCode::OK
        );
        // The docs are behind the token like everything else.
        assert_eq!(
            call(router(state.clone()), ask("/openapi.json", None))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn the_api_document_can_be_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.server.docs = false;
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());

        let ask = |path: &str| Request::builder().uri(path).body(Body::empty()).unwrap();
        // Not described and not served: 404, not 401, so a scanner learns nothing.
        for path in ["/openapi.json", "/docs", "/docs/"] {
            assert_eq!(
                call(router(state.clone()), ask(path)).await.0,
                StatusCode::NOT_FOUND,
                "{path} was still served"
            );
        }
        // The rest of the surface is unchanged.
        assert_eq!(
            call(router(state.clone()), ask("/v1/info")).await.0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn the_last_session_permit_is_the_last() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.server.limits.max_connections = 2;
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());

        let first = state.open_session();
        let second = state.open_session();
        assert!(first.is_some() && second.is_some());
        // The third finds nothing free rather than queueing behind the other two.
        assert!(state.open_session().is_none());

        // A session that ends returns its permit.
        drop(first);
        assert!(state.open_session().is_some());
    }

    #[tokio::test]
    async fn a_download_of_an_unknown_model_is_a_404() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .method("POST")
                .uri("/v1/models/not-a-model/download")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "unknown_model");
    }

    #[tokio::test]
    async fn deleting_a_model_removes_the_files_the_manifest_names() {
        let (state, dir) = state();
        let spec = state.manifest.models.first().unwrap().clone();
        // One file the manifest names, and one it does not: the delete must not sweep.
        for file in &spec.files {
            std::fs::write(dir.path().join(&file.name), b"weights").unwrap();
        }
        std::fs::write(dir.path().join("unrelated.bin"), b"keep me").unwrap();

        let (status, body) = call(
            router(Arc::clone(&state)),
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/models/{}", spec.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

        for file in &spec.files {
            assert!(
                !dir.path().join(&file.name).exists(),
                "{} survived",
                file.name
            );
        }
        assert!(
            dir.path().join("unrelated.bin").exists(),
            "a file the manifest does not name was removed"
        );
    }

    #[test]
    fn only_one_caller_can_start_a_download_of_one_model() {
        // Check and insert under one lock. Taken apart, two requests both saw "not
        // running" and both spawned a task writing the same .part file.
        let (state, _dir) = state();
        let id = state.manifest.models.first().unwrap().id.clone();
        assert_eq!(
            state.try_start_download(&id, 100),
            None,
            "the first caller owns it"
        );
        let second = state.try_start_download(&id, 100);
        assert_eq!(second, Some((0, 100)), "the second caller must be refused");
        state.note_download(&id, 40, 100);
        assert_eq!(
            state.try_start_download(&id, 100),
            Some((40, 100)),
            "a third caller sees the progress, not a fresh start"
        );
        state.finish_download(&id, None);
        assert_eq!(
            state.try_start_download(&id, 100),
            None,
            "a finished download can be started again"
        );
    }

    /// A state whose catalog says every file is zero bytes, so a test can create the files
    /// without writing hundreds of megabytes to a temporary directory.
    fn state_of_empty_files() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        for spec in &mut manifest.models {
            for file in &mut spec.files {
                file.size = 0;
            }
        }
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        (
            Arc::new(AppState::new(config, manifest, None).unwrap()),
            dir,
        )
    }

    #[tokio::test]
    async fn deleting_a_model_also_removes_its_partial_file() {
        let (state, dir) = state_of_empty_files();
        let spec = state.manifest.models.first().unwrap().clone();
        for file in &spec.files {
            std::fs::write(dir.path().join(&file.name), b"").unwrap();
            std::fs::write(dir.path().join(format!("{}.part", file.name)), b"half").unwrap();
        }
        let (status, body) = call(
            router(Arc::clone(&state)),
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/models/{}", spec.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        for file in &spec.files {
            assert!(
                !dir.path().join(format!("{}.part", file.name)).exists(),
                "{} .part survived",
                file.name
            );
        }
    }

    #[tokio::test]
    async fn a_model_being_downloaded_cannot_be_deleted() {
        let (state, _dir) = state();
        let id = state.manifest.models.first().unwrap().id.clone();
        state.note_download(&id, 10, 100);

        let (status, body) = call(
            router(Arc::clone(&state)),
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/models/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "download_in_progress");
    }

    #[tokio::test]
    async fn a_delete_that_fails_still_gives_the_model_back() {
        // The reservation is held for the length of the unlink. Left behind on the error
        // path, the id stays busy until the process restarts: no download, no second
        // delete, and a catalog entry that says "downloading" forever.
        let (state, dir) = state();
        let spec = state
            .manifest
            .models
            .iter()
            .find(|spec| spec.kind == Kind::Tts)
            .cloned()
            .unwrap();
        // A directory where a file belongs: `remove_file` refuses it, so the handler takes
        // the `remove_failed` path.
        std::fs::create_dir(dir.path().join(&spec.files[0].name)).unwrap();

        let (status, body) = call(
            router(Arc::clone(&state)),
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/models/{}", spec.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "remove_failed");
        assert!(
            state.download_progress(&spec.id).is_none(),
            "the id is still reserved after a failed delete"
        );
    }

    #[tokio::test]
    async fn a_second_download_request_reports_the_first() {
        let (state, _dir) = state();
        let id = state.manifest.models.first().unwrap().id.clone();
        state.note_download(&id, 40, 100);

        let (status, body) = call(
            router(Arc::clone(&state)),
            Request::builder()
                .method("POST")
                .uri(format!("/v1/models/{id}/download"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["downloaded"], 40, "the running download was restarted");
    }

    #[tokio::test]
    async fn warming_a_model_that_is_not_on_disk_is_refused() {
        let (state, _dir) = state();
        let id = state.manifest.models.first().unwrap().id.clone();
        let (status, body) = call(
            router(state),
            Request::builder()
                .method("POST")
                .uri(format!("/v1/models/{id}/warm"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "model_not_installed");
    }

    #[tokio::test]
    async fn warming_a_speech_model_that_is_not_the_configured_one_is_refused() {
        // The speech slot holds `stt.batch_model` and nothing else. Warming another id used
        // to load that one anyway, or fail for a model the caller never asked for, and
        // answer for the id in the path.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        // Every size is zero in this copy of the catalog, so an empty file counts as
        // installed and the test does not write a few hundred megabytes to prove a refusal.
        let mut manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        for spec in &mut manifest.models {
            for file in &mut spec.files {
                file.size = 0;
            }
        }
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());
        let configured = state.config().stt.batch_model.clone();
        let other = state
            .manifest
            .models
            .iter()
            .find(|spec| spec.kind == Kind::Stt && spec.id != configured)
            .expect("the catalog holds more than one speech model")
            .clone();
        for file in &other.files {
            std::fs::write(dir.path().join(&file.name), b"").unwrap();
        }
        let (status, body) = call(
            router(state),
            Request::builder()
                .method("POST")
                .uri(format!("/v1/models/{}/warm", other.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "not_active_model");
    }

    #[test]
    fn a_stream_model_outside_the_catalog_is_refused_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.stt.stream_model = "sherpa-nobody-streaming-en".to_string();
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let error = AppState::new(config, manifest, None)
            .expect_err("a stream model the catalog does not hold must not start");
        let detail = error.to_string();
        assert!(detail.contains("stt.stream_model"), "{detail}");
        assert!(detail.contains("sherpa-nobody-streaming-en"), "{detail}");
    }

    #[test]
    fn a_batch_model_is_refused_as_the_stream_model_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        // The default batch model is a batch family, so naming it here would leave the
        // socket on the batch loop while the config says otherwise.
        config.stt.stream_model = config.stt.batch_model.clone();
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let error = AppState::new(config, manifest, None)
            .expect_err("a batch model cannot be the streaming model");
        let detail = error.to_string();
        assert!(detail.contains("stt.stream_model"), "{detail}");
        assert!(detail.contains("not a streaming model"), "{detail}");
    }

    #[tokio::test]
    async fn a_patch_can_choose_and_clear_the_streaming_model() {
        let (state, _dir) = state();
        let (status, body) = patch(
            state.clone(),
            r#"{"stt":{"stream_model":"sherpa-zipformer-streaming-en"}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            state.config().stt.stream_model,
            "sherpa-zipformer-streaming-en"
        );
        assert!(state.stt_streams());

        let (status, body) = patch(state.clone(), r#"{"stt":{"stream_model":""}}"#).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(state.config().stt.stream_model.is_empty());
        assert!(!state.stt_streams(), "empty means the batch path");
    }

    #[tokio::test]
    async fn a_stream_model_outside_the_catalog_is_refused_and_nothing_is_applied() {
        let (state, _dir) = state();
        let before = state.config();
        let (status, body) =
            patch(state.clone(), r#"{"stt":{"stream_model":"sherpa-nobody"}}"#).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let detail = body["detail"].as_str().unwrap();
        assert!(detail.contains("stt.stream_model"), "{detail}");
        assert_eq!(state.config(), before, "a refused patch changes nothing");
    }

    #[tokio::test]
    async fn a_batch_model_cannot_be_chosen_as_the_streaming_model() {
        let (state, _dir) = state();
        let batch = state.config().stt.batch_model.clone();
        let request = format!(r#"{{"stt":{{"stream_model":"{batch}"}}}}"#);
        let (status, body) = patch(state.clone(), &request).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("not a streaming model"),
            "{body}"
        );
    }

    /// The streaming model has its own slot, so warming it must not be refused as "not the
    /// batch model". The files here are empty, so the load itself fails; what is under test
    /// is that the active-model gate let the id through to its slot.
    #[tokio::test]
    async fn the_configured_streaming_model_can_be_warmed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        // Every size is zero in this copy of the catalog, so an empty file counts as
        // installed and the test does not write a few hundred megabytes to prove a refusal.
        let mut manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        for spec in &mut manifest.models {
            for file in &mut spec.files {
                file.size = 0;
            }
        }
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.stt.stream_model = "sherpa-zipformer-streaming-en".to_string();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());
        let spec = state
            .manifest
            .find("sherpa-zipformer-streaming-en")
            .expect("the catalog holds the streaming model")
            .clone();
        for file in &spec.files {
            std::fs::write(dir.path().join(&file.name), b"").unwrap();
        }
        let (status, body) = call(
            router(state),
            Request::builder()
                .method("POST")
                .uri("/v1/models/sherpa-zipformer-streaming-en/warm")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_ne!(
            status,
            StatusCode::CONFLICT,
            "the streaming slot refused its own model: {body}"
        );
        assert_ne!(body["code"], "not_active_model", "{body}");
    }

    /// Deleting the active streaming model drops its slot. The batch slot holds a different
    /// model, so it must stay untouched.
    #[tokio::test]
    async fn deleting_the_streaming_model_unloads_its_slot() {
        let mut config = Config::default();
        config.stt.stream_model = "sherpa-zipformer-streaming-en".to_string();
        let (state, _dir) = state_with(config);
        let (status, body) = call(
            router(state),
            Request::builder()
                .method("DELETE")
                .uri("/v1/models/sherpa-zipformer-streaming-en")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let dropped = body["dropped"].as_array().unwrap();
        assert!(dropped.iter().any(|slot| slot == "stt_stream"), "{body}");
        assert!(
            !dropped.iter().any(|slot| slot == "stt"),
            "the batch slot holds a different model: {body}"
        );
    }

    /// The first clients do not rehash the table.
    ///
    /// The constructor sizes the map for 64 entries, which costs 6,288 bytes once. Filling
    /// it from empty costs 12,444 bytes in six rehashes to reach the same place, and a
    /// deployment sees more than one client.
    #[test]
    fn the_first_clients_do_not_rehash_the_table() {
        let limiter = RateLimiter::new(50, 100);
        let (_, stats) = measure(|| {
            for index in 1..=64u32 {
                limiter.take(std::net::IpAddr::V4(std::net::Ipv4Addr::from(index)));
            }
        });
        // The table is sized at construction, so filling it costs nothing that grows. macOS
        // spends one small allocation the first time the lock is taken, which is the
        // platform's lock and not the table: the byte bound is what says so, because the
        // growth this test replaced cost 6 allocations and 12,444 bytes.
        assert!(
            stats.allocations <= 1 && stats.bytes <= 1024,
            "64 clients cost {} allocations and {} bytes",
            stats.allocations,
            stats.bytes
        );
        assert_eq!(limiter.buckets.lock().unwrap().map.len(), 64);
    }

    /// The bucket table has a fixed maximum, however many distinct addresses arrive.
    #[test]
    fn the_rate_limiter_table_stops_at_its_cap() {
        let limiter = RateLimiter::new(1, 1);
        let address = |index: u32| std::net::IpAddr::V4(std::net::Ipv4Addr::from(index));
        for index in 1..=(MAX_BUCKETS as u32 + 64) {
            limiter.take(address(index));
        }
        let buckets = limiter.buckets.lock().unwrap();
        assert_eq!(buckets.map.len(), MAX_BUCKETS);
    }

    /// At the cap, a new client evicts the bucket that has gone unused longest.
    #[test]
    fn the_rate_limiter_evicts_the_least_recently_used_client() {
        let limiter = RateLimiter::new(1, 1);
        let address = |index: u32| std::net::IpAddr::V4(std::net::Ipv4Addr::from(index));
        let first = address(1);
        limiter.take(first);
        for index in 2..=(MAX_BUCKETS as u32) {
            limiter.take(address(index));
        }
        // Timestamps from a fast loop can tie, so the oldest bucket is aged explicitly.
        {
            let mut buckets = limiter.buckets.lock().unwrap();
            buckets.map.get_mut(&first).unwrap().last =
                std::time::Instant::now() - std::time::Duration::from_secs(60);
        }
        let newcomer = address(MAX_BUCKETS as u32 + 1);
        limiter.take(newcomer);
        let buckets = limiter.buckets.lock().unwrap();
        assert!(
            !buckets.map.contains_key(&first),
            "the oldest bucket stayed"
        );
        assert!(buckets.map.contains_key(&newcomer));
        assert_eq!(buckets.map.len(), MAX_BUCKETS);
    }

    #[tokio::test]
    async fn warming_an_unknown_model_is_a_404() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .method("POST")
                .uri("/v1/models/nope/warm")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "unknown_model");
    }

    #[test]
    fn the_audio_body_limit_follows_the_configured_duration() {
        // The default, ten minutes, is what the old constant said.
        assert_eq!(max_audio_bytes(600), 48_000 * 2 * 600);
        // A config that allows an hour must accept an hour of 48 kHz mono.
        assert_eq!(max_audio_bytes(3600), 48_000 * 2 * 3600);
        // A duration no allocation can hold must saturate rather than wrap.
        assert!(max_audio_bytes(u32::MAX) >= 48_000 * 2 * 600);
    }

    #[tokio::test]
    async fn unknown_route_is_not_found() {
        let (state, _dir) = state();
        let (status, _) = call(
            router(state),
            Request::builder().uri("/nope").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The catalog's family is what decides the path, so the answer is available before
    /// the model is downloaded.
    #[tokio::test]
    async fn info_names_the_path_the_configured_model_uses() {
        let (state, _dir) = state();
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/v1/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // The default stream model names nothing in the catalog, so the socket keeps the
        // batch path it has always run: a partial is a re-run, not a read.
        assert_eq!(body["stt_path"], "batch");

        let mut config = Config::default();
        config.stt.stream_model = "sherpa-zipformer-streaming-en".to_string();
        let (state, _dir) = state_with(config);
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/v1/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["stt_path"], "streaming");
    }

    /// With speech to text off there is no path to report, rather than a path a caller
    /// could mistake for a working one.
    #[tokio::test]
    async fn info_reports_no_path_when_speech_to_text_is_off() {
        let mut config = Config::default();
        config.stt.enabled = false;
        config.stt.stream_model = "sherpa-zipformer-streaming-en".to_string();
        let (state, _dir) = state_with(config);
        let (status, body) = call(
            router(state),
            Request::builder()
                .uri("/v1/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(body["stt_path"].is_null(), "{body}");
    }

    /// A streaming model that is not on disk is refused before the socket upgrades, with
    /// the same code and sentence the batch route answers.
    #[tokio::test]
    async fn a_streaming_model_that_is_not_installed_is_a_409() {
        let mut config = Config::default();
        config.stt.stream_model = "sherpa-zipformer-streaming-en".to_string();
        let (state, _dir) = state_with(config);
        let refusal =
            ws_stt::preflight(&state).expect("a streaming model with no files is refused");
        assert_eq!(refusal.status(), StatusCode::CONFLICT);
        let body = refusal.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "model_not_installed");
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("sherpa-zipformer-streaming-en"),
            "{body}"
        );
    }

    /// A batch model keeps the path it had: the socket opens, and the missing model is
    /// reported as a frame when the engine is asked for it.
    #[tokio::test]
    async fn a_batch_model_is_not_refused_before_the_upgrade() {
        let (state, _dir) = state();
        assert!(
            ws_stt::preflight(&state).is_none(),
            "the batch path must not gain a pre-upgrade check"
        );
    }
}
