//! HTTP surface for arioso.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use arioso_config::Config;
use arioso_core::UserDictionary;
use arioso_g2p::CmuDict;
use arioso_models::{Kind, Manifest, ModelSpec, models_dir};
use arioso_tts_piper::{PiperError, PiperVoice, wav};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod ws;
mod ws_stt;

pub use ws::handler as ws_handler;
pub use ws_stt::handler as stt_ws_handler;

/// Longest text one synthesis request accepts.
pub(crate) const MAX_TEXT_CHARS: usize = 4_000;

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
    #[error("no piper voice matches tts.voice {0:?}")]
    NoVoice(String),
    /// The configured speech to text model is not in the manifest.
    #[error("no speech to text model matches stt.batch_model {0:?}")]
    NoSttModel(String),
    /// The user dictionary could not be used.
    #[error("user dictionary: {0}")]
    UserDictionary(#[from] arioso_core::DictError),
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

/// Shared server state.
#[derive(Debug)]
pub struct AppState {
    /// Mutable so PATCH /v1/config can change it without a restart. Read through
    /// `config()`, which clones, so no lock is held across an await.
    config: std::sync::RwLock<Config>,
    manifest: Manifest,
    models_dir: PathBuf,
    dictionary: CmuDict,
    user: Mutex<UserState>,
    voice: Mutex<Option<PiperVoice>>,
    /// Whisper, loaded on first use. One model, so one slot.
    stt: Mutex<Option<arioso_stt_sherpa::Whisper>>,
    /// Where the config came from, so a patch can be written back. Absent when the
    /// server was started with defaults and no file.
    config_file: Option<std::path::PathBuf>,
}

/// The user dictionary on disk, and what it last looked like.
#[derive(Debug)]
struct UserState {
    path: PathBuf,
    modified: Option<std::time::SystemTime>,
    dictionary: UserDictionary,
    /// Parse error from the last reload attempt. The last good copy stays in use.
    error: Option<String>,
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
        Ok(Self {
            dictionary: CmuDict::parse(&text),
            user: Mutex::new(UserState {
                path,
                modified,
                dictionary,
                error,
            }),
            voice: Mutex::new(None),
            stt: Mutex::new(None),
            config: std::sync::RwLock::new(config),
            config_file,
            manifest,
            models_dir,
        })
    }

    /// The config as it stands, cloned.
    ///
    /// Cloned rather than borrowed so a caller never holds the lock across an await.
    /// A poisoned lock still yields the value: the config is plain data, so a panic in
    /// another task cannot leave it inconsistent.
    pub(crate) fn config(&self) -> Config {
        self.config
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// Drop the loaded voice, so the next request loads the one the config now names.
    fn drop_voice(&self) {
        if let Ok(mut guard) = self.voice.lock() {
            *guard = None;
        }
    }

    /// Drop the loaded transcription model.
    fn drop_stt(&self) {
        if let Ok(mut guard) = self.stt.lock() {
            *guard = None;
        }
    }

    /// Replace the config.
    fn set_config(&self, config: Config) {
        match self.config.write() {
            Ok(mut guard) => *guard = config,
            Err(poisoned) => *poisoned.into_inner() = config,
        }
    }

    /// The voice the config asks for.
    fn voice_spec(&self) -> Result<&ModelSpec, ServerError> {
        let voice = self.config().tts.voice;
        self.manifest
            .models
            .iter()
            .filter(|m| m.kind == Kind::Tts && m.engine == "piper")
            .find(|m| m.id == voice || m.id.ends_with(&voice))
            .ok_or(ServerError::NoVoice(voice))
    }

    /// The speech to text model the config asks for.
    /// Every engine this binary can serve, with what it can do.
    ///
    /// A disabled engine is absent rather than reported as unavailable: its routes do
    /// not exist either, so a client that reads this list sees exactly what it can call.
    fn engines(&self) -> Vec<EngineView> {
        let mut engines = Vec::new();
        let config = self.config();
        if config.tts.enabled {
            engines.push(EngineView {
                id: "piper".to_string(),
                kind: "tts".to_string(),
                streaming: true,
                languages: self.catalog_languages("piper"),
                models: self.catalog_count("piper"),
                loaded: self
                    .voice
                    .lock()
                    .map(|guard| guard.is_some())
                    .unwrap_or(false),
            });
        }
        if config.stt.enabled {
            engines.push(EngineView {
                id: "sherpa-whisper".to_string(),
                kind: "stt".to_string(),
                streaming: true,
                languages: self.catalog_languages("sherpa"),
                models: self.catalog_count("sherpa"),
                loaded: self
                    .stt
                    .lock()
                    .map(|guard| guard.is_some())
                    .unwrap_or(false),
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
        let model = self.config().stt.batch_model;
        self.manifest
            .models
            .iter()
            .filter(|m| m.kind == Kind::Stt && m.engine == "sherpa")
            .find(|m| m.id == model)
            .ok_or(ServerError::NoSttModel(model))
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

    /// The engine, loaded on first use.
    fn ensure_engine<'a>(
        &'a self,
        slot: &'a mut Option<arioso_stt_sherpa::Whisper>,
    ) -> Result<&'a arioso_stt_sherpa::Whisper, SttFailure> {
        if slot.is_none() {
            let spec = self
                .stt_spec()
                .map_err(|e| SttFailure::Engine(e.to_string()))?;
            if !self.is_installed(spec) {
                return Err(SttFailure::NotInstalled(spec.id.clone()));
            }
            let configured = self.config().stt.language;
            let language = if configured == "auto" {
                None
            } else {
                Some(configured.as_str())
            };
            let threads = std::thread::available_parallelism()
                .map_or(4, |count| (count.get() / 2).clamp(1, 8));
            // The manifest names the files, so a directory holding two models still
            // loads the one the config asks for.
            let file = |needle: &str| {
                spec.files
                    .iter()
                    .find(|f| f.name.contains(needle))
                    .map(|f| self.models_dir.join(&f.name))
            };
            let engine = match (file("encoder"), file("decoder"), file("tokens")) {
                (Some(encoder), Some(decoder), Some(tokens)) => {
                    arioso_stt_sherpa::Whisper::load_files(
                        &self.models_dir,
                        &encoder,
                        &decoder,
                        &tokens,
                        language,
                        threads,
                    )
                }
                _ => arioso_stt_sherpa::Whisper::load(&self.models_dir, language, threads),
            };
            *slot =
                Some(engine.map_err(|e| SttFailure::Engine(format!("loading {}: {e}", spec.id)))?);
        }
        slot.as_ref()
            .ok_or_else(|| SttFailure::Engine("the engine failed to load".to_string()))
    }

    /// Transcribe samples and run the dictionary over the result.
    fn hear(
        &self,
        engine: &arioso_stt_sherpa::Whisper,
        samples: &[f32],
        rate: u32,
    ) -> Result<String, SttFailure> {
        let transcript = engine
            .transcribe(samples, rate)
            .map_err(|e| SttFailure::Engine(e.to_string()))?;
        // The dictionary runs backwards as well, compared by sound rather than by
        // letters, because the engine writes "Tonseller" for "tom seller".
        let user = self.user_dictionary();
        Ok(arioso_g2p::reverse::correct(
            &user.dictionary,
            &self.dictionary,
            &transcript.text,
        ))
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
    fn user_dictionary(&self) -> std::sync::MutexGuard<'_, UserState> {
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
            state.dictionary = UserDictionary::shipped();
            state.error = None;
            return state;
        }

        match UserDictionary::load(&state.path) {
            Ok(dictionary) => {
                tracing::info!(entries = dictionary.len(), "reloaded the user dictionary");
                state.dictionary = dictionary;
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
        spec.files.iter().all(|file| {
            std::fs::metadata(self.models_dir.join(&file.name))
                .map(|meta| meta.len() == file.size)
                .unwrap_or(false)
        })
    }

    fn file_of(&self, spec: &ModelSpec, suffix: &str) -> Result<PathBuf, String> {
        spec.files
            .iter()
            .find(|f| f.name.ends_with(suffix))
            .map(|f| self.models_dir.join(&f.name))
            .ok_or_else(|| format!("voice {} has no {suffix} file", spec.id))
    }

    /// Synthesize text, loading the voice on first use.
    fn render(&self, text: &str, speed: f32) -> Result<(Vec<f32>, u32), String> {
        let mut rate = 0;
        let mut samples = Vec::new();
        self.render_stream(text, speed, |chunk, chunk_rate| {
            rate = chunk_rate;
            samples.extend_from_slice(chunk);
            Ok(())
        })?;
        if samples.is_empty() {
            return Err("nothing to say: none of the words are in the dictionary".into());
        }
        Ok((samples, rate))
    }

    /// Synthesize text one chunk at a time, handing each to `sink`.
    ///
    /// The sink runs while later chunks are still being synthesized, so a
    /// caller can send audio before the whole text is done.
    pub(crate) fn render_stream(
        &self,
        text: &str,
        speed: f32,
        mut sink: impl FnMut(&[f32], u32) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut guard = self
            .voice
            .lock()
            .map_err(|_| "the voice lock is poisoned".to_string())?;
        if guard.is_none() {
            let spec = self.voice_spec().map_err(|e| e.to_string())?;
            let onnx = self.file_of(spec, ".onnx")?;
            let config = self.file_of(spec, ".onnx.json")?;
            *guard = Some(PiperVoice::load(&onnx, &config).map_err(|e| e.to_string())?);
        }
        let voice = guard.as_mut().ok_or("voice failed to load")?;
        let user = self.user_dictionary();
        let options = arioso_tts_piper::RenderOptions {
            speed,
            normalize: arioso_core::NormalizeOptions {
                time_of_day: self.config().tts.time_of_day,
            },
        };
        let report = voice
            .render(
                text,
                &self.dictionary,
                &user.dictionary,
                options,
                |audio, rate| sink(audio, rate).map_err(PiperError::Consumer),
            )
            .map_err(|error| error.to_string())?;
        if !report.guessed.is_empty() {
            tracing::debug!(guessed = ?report.guessed, "words spoken by spelling rules");
        }
        if !report.missing.is_empty() {
            tracing::debug!(missing = ?report.missing, "words nothing could pronounce");
        }
        if !report.unusable.is_empty() {
            tracing::warn!(entries = ?report.unusable, "user dictionary entries the voice cannot say");
        }
        Ok(())
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
    fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
            code: None,
        }
    }

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
        list_models,
        synthesize,
        transcribe_audio
    ),
    components(schemas(
        TtsRequest,
        ModelView,
        ModelListView,
        SttResponse,
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

/// Request body for `POST /v1/tts`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct TtsRequest {
    /// Text to speak.
    pub text: String,
    /// Speaking rate multiplier, overriding the config.
    pub speed: Option<f32>,
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
    /// Total size in bytes.
    pub bytes: u64,
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

/// Build the router.
///
/// A disabled engine does not register its routes, so `stt.enabled: false` answers 404
/// on the speech to text path rather than pretending the feature exists.
pub fn router(state: Arc<AppState>) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/models", get(list_models))
        .route("/v1/info", get(info))
        .route("/v1/engines", get(list_engines))
        .route("/v1/voices", get(list_voices))
        .route("/v1/config", get(get_config).patch(patch_config))
        // The Swagger UI serves the document at the URL it is given, so this registers
        // /openapi.json as well as /docs. An explicit route beside it is a collision.
        .merge(utoipa_swagger_ui::SwaggerUi::new("/docs").url("/openapi.json", document(&state)));

    if state.config().tts.enabled {
        router = router
            .route("/v1/tts", post(synthesize))
            .route("/v1/tts/ws", get(ws_handler));
    }
    if state.config().stt.enabled {
        // The body is audio, so the default 2 MiB limit is too small.
        let limit = MAX_AUDIO_BYTES;
        router = router.route(
            "/v1/stt",
            post(transcribe_audio).layer(axum::extract::DefaultBodyLimit::max(limit)),
        );
        router = router.route("/v1/stt/ws", get(stt_ws_handler));
    }

    router.with_state(state)
}

/// Largest audio body accepted: 16-bit mono at 48 kHz for the longest allowed audio.
const MAX_AUDIO_BYTES: usize = 48_000 * 2 * 600;

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
    /// `ready` when the configured voice is installed.
    pub status: String,
    /// The user dictionary.
    pub dictionary: DictionaryView,
    /// Voice the server will load.
    pub voice: Option<String>,
    /// Whether that voice is on disk.
    pub voice_installed: bool,
    /// Whether it is loaded right now.
    pub voice_loaded: bool,
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

#[utoipa::path(
    get,
    path = "/readyz",
    tag = "system",
    summary = "Readiness",
    description = "Ready means the configured voice is installed, not that some model is.",
    responses((status = 200, body = ReadyView))
)]
async fn readyz(State(state): State<Arc<AppState>>) -> Json<ReadyView> {
    let installed = state
        .manifest
        .models
        .iter()
        .filter(|model| state.is_installed(model))
        .count();
    // Readiness is about the voice this server will actually load, not about
    // any model being present.
    let voice = state.voice_spec().ok().map(|spec| spec.id.clone());
    let voice_installed = state
        .voice_spec()
        .is_ok_and(|spec| state.is_installed(spec));
    let voice_loaded = state
        .voice
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false);
    let user = state.user_dictionary();
    let dictionary = DictionaryView {
        entries: user.dictionary.len(),
        error: user.error.clone(),
    };
    drop(user);
    Json(ReadyView {
        status: if voice_installed {
            "ready"
        } else {
            "voice_missing"
        }
        .to_string(),
        dictionary,
        voice,
        voice_installed,
        voice_loaded,
        models_installed: installed,
    })
}

#[utoipa::path(
    get,
    path = "/v1/info",
    tag = "system",
    summary = "Version, target, and the engines compiled in",
    responses((status = 200, body = InfoView))
)]
async fn info(State(state): State<Arc<AppState>>) -> Json<InfoView> {
    Json(InfoView {
        version: env!("CARGO_PKG_VERSION").to_string(),
        target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        engines: state.engines().into_iter().map(|e| e.id).collect(),
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
}

/// Text to speech fields a patch may change.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TtsPatch {
    /// Whether the speech routes are served.
    pub enabled: Option<bool>,
    /// Engine id. Only `piper` is built into this binary.
    pub engine: Option<String>,
    /// Voice id, or the bare voice name.
    pub voice: Option<String>,
    /// Speaking rate, 0.5 to 2.0.
    pub speed: Option<f32>,
    /// `wav`, `pcm`, `mp3`, or `opus`.
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
    /// `auto`, or a language code.
    pub language: Option<String>,
    /// Whether the stream sends partial transcripts.
    pub partials: Option<bool>,
}

/// Listener fields a patch may change.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerPatch {
    /// How long a WebSocket client may stay silent, in milliseconds.
    pub first_message_timeout_ms: Option<u64>,
}

/// What a patch touched, so the engines it invalidated can be dropped.
#[derive(Debug, Default)]
struct Touched {
    voice: bool,
    stt: bool,
}

impl ConfigPatch {
    /// Apply the fields present, and report what changed.
    fn apply(self, config: &mut Config) -> Touched {
        let mut touched = Touched::default();
        if let Some(tts) = self.tts {
            if let Some(enabled) = tts.enabled {
                config.tts.enabled = enabled;
            }
            if let Some(engine) = tts.engine {
                config.tts.engine = engine;
                touched.voice = true;
            }
            if let Some(voice) = tts.voice {
                config.tts.voice = voice;
                touched.voice = true;
            }
            if let Some(speed) = tts.speed {
                config.tts.speed = speed;
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
            if let Some(language) = stt.language {
                config.stt.language = language;
                touched.stt = true;
            }
            if let Some(partials) = stt.partials {
                config.stt.partials = partials;
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
const TTS_ENGINES: [&str; 1] = ["piper"];
const STT_ENGINES: [&str; 1] = ["sherpa"];
const AUDIO_FORMATS: [&str; 4] = ["wav", "pcm", "mp3", "opus"];

#[utoipa::path(
    get,
    path = "/v1/config",
    tag = "config",
    summary = "The config as it stands",
    description = "No field here is secret: the bearer token is read from the environment, \
        never from this document.",
    responses((status = 200, body = serde_json::Value))
)]
async fn get_config(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(config_json(&state.config()))
}

/// The config as JSON, with floats written the way the file writes them.
///
/// `to_value` turns an f32 into an f64, so a speed of 1.4 comes back as
/// 1.399999976158142. Serialising to text first uses the f32 formatter and keeps 1.4.
fn config_json(config: &Config) -> serde_json::Value {
    serde_json::to_string(config)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[utoipa::path(
    patch,
    path = "/v1/config",
    tag = "config",
    summary = "Change the config without a restart",
    description = "Only the fields present in the body change. An unknown field is refused, \
        so a typo does not look like a successful change. Validation runs before anything \
        is applied, so a refused patch changes nothing.",
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
) -> Result<Json<serde_json::Value>, ApiError> {
    let persist = patch.persist;
    let mut next = state.config();
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
            ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "config_write_failed",
                format!("writing {path:?}: {error}"),
            )
        })?;
    }

    state.set_config(next.clone());
    if touched.voice {
        state.drop_voice();
    }
    if touched.stt {
        state.drop_stt();
    }
    Ok(Json(config_json(&next)))
}

/// The API document, cut down to the routes this server actually registered.
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
    let voice_known = state.manifest.models.iter().any(|model| {
        model.kind == arioso_models::Kind::Tts
            && (model.id == config.tts.voice || model.id.ends_with(&config.tts.voice))
    });
    if !voice_known {
        return Err(unprocessable(format!(
            "tts.voice {} is not in the catalog; GET /v1/voices lists what is",
            config.tts.voice
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
    // Voice ids write en_GB and the catalog writes en-GB, so the separator must not
    // decide whether a client finds its language.
    let normalise = |language: &str| language.to_lowercase().replace('_', "-");
    let wanted = query
        .language
        .as_deref()
        .map(normalise)
        .filter(|language| !language.is_empty());

    let voices = state
        .manifest
        .models
        .iter()
        .filter(|model| model.kind == arioso_models::Kind::Tts)
        .filter_map(|model| {
            let languages = meta_strings(&model.meta, "languages");
            if let Some(wanted) = &wanted {
                let speaks = languages
                    .iter()
                    .any(|language| normalise(language).starts_with(wanted.as_str()));
                if !speaks {
                    return None;
                }
            }
            Some(VoiceView {
                id: model.id.clone(),
                engine: model.engine.clone(),
                license: model.license.clone(),
                commercial_use: model.commercial_use,
                sample_rate: model
                    .meta
                    .get("sample_rate")
                    .and_then(serde_json::Value::as_u64),
                quality: model
                    .meta
                    .get("quality")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                languages,
                installed: state.is_installed(model),
                active: active.as_ref() == Some(&model.id),
            })
        })
        .collect();
    Json(VoiceListView { voices })
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
    let models: Vec<ModelView> = state
        .manifest
        .models
        .iter()
        .map(|m| ModelView {
            id: m.id.clone(),
            kind: format!("{:?}", m.kind).to_lowercase(),
            engine: m.engine.clone(),
            license: m.license.clone(),
            installed: state.is_installed(m),
            bytes: m.total_size(),
        })
        .collect();
    Json(ModelListView { models })
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
    let text = request.text.trim();
    if text.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "text must not be empty",
        ));
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("text is longer than {MAX_TEXT_CHARS} characters"),
        ));
    }
    let speed = request.speed.unwrap_or(state.config().tts.speed);
    if !(0.5..=2.0).contains(&speed) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("speed {speed} is outside 0.5 to 2.0"),
        ));
    }

    let text = text.to_owned();
    let worker = Arc::clone(&state);
    let (samples, rate) = tokio::task::spawn_blocking(move || worker.render(&text, speed))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e))?;

    Ok((
        [(header::CONTENT_TYPE, "audio/wav")],
        wav::to_bytes(&samples, rate),
    )
        .into_response())
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
            SttFailure::Engine(detail) => ApiError::coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "engine_unavailable",
                detail,
            ),
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
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const MANIFEST: &str = include_str!("../../../models/manifest.json");
    const DICT: &str = "HELLO  HH AH0 L OW1\nWORLD  W ER1 L D\n";

    fn state() -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        (
            Arc::new(AppState::new(config, manifest, None).unwrap()),
            dir,
        )
    }

    async fn call(app: Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, body)
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
    async fn an_engine_that_is_not_built_is_refused_with_the_alternative() {
        let (state, _dir) = state();
        let (status, body) = patch(state, r#"{"tts":{"engine":"kokoro"}}"#).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let detail = body["detail"].as_str().unwrap();
        assert!(
            detail.contains("piper"),
            "the refusal names what works: {detail}"
        );
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
        let first = &json["models"][0];
        assert_eq!(first["id"], "piper-en_US-lessac-medium");
        assert_eq!(first["kind"], "tts");
        assert_eq!(first["installed"], false);
    }

    #[tokio::test]
    async fn readiness_reports_a_missing_voice() {
        let (state, _dir) = state();
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
        assert_eq!(json["status"], "voice_missing");
        assert_eq!(json["voice"], "piper-en_US-lessac-medium");
        assert_eq!(json["voice_installed"], false);
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
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // No file of the user's own, so the shipped dictionary is what applies.
        assert_eq!(json["dictionary"]["entries"], 3);
        assert!(json["dictionary"]["error"].is_null());
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
    async fn a_missing_voice_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), DICT).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.tts.voice = "no-such-voice".into();
        let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());

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
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(String::from_utf8_lossy(&body).contains("no piper voice"));
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
    async fn unknown_route_is_not_found() {
        let (state, _dir) = state();
        let (status, _) = call(
            router(state),
            Request::builder().uri("/nope").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
