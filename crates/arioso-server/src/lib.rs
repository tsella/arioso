//! HTTP surface for Arioso.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use arioso_config::Config;
use arioso_core::UserDictionary;
use arioso_g2p::CmuDict;
use arioso_models::{Kind, Manifest, ModelSpec, models_dir};
use arioso_tts_piper::{PiperError, PiperVoice, wav};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod ws;

pub use ws::handler as ws_handler;

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
    /// The user dictionary could not be used.
    #[error("user dictionary: {0}")]
    UserDictionary(#[from] arioso_core::DictError),
}

/// Shared server state.
#[derive(Debug)]
pub struct AppState {
    config: Config,
    manifest: Manifest,
    models_dir: PathBuf,
    dictionary: CmuDict,
    user: Mutex<UserState>,
    voice: Mutex<Option<PiperVoice>>,
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
        let path = models_dir.join("cmudict.dict");
        let text = std::fs::read_to_string(&path)
            .map_err(|source| ServerError::Dictionary { path, source })?;
        let path = config.dictionary_path(config_file);
        let modified = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok();
        let (dictionary, error) = match UserDictionary::load(&path) {
            Ok(dictionary) => (dictionary, None),
            Err(arioso_core::DictError::Io { .. }) => (UserDictionary::empty(), None),
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
            config,
            manifest,
            models_dir,
        })
    }

    /// The voice the config asks for.
    fn voice_spec(&self) -> Result<&ModelSpec, ServerError> {
        self.manifest
            .models
            .iter()
            .filter(|m| m.kind == Kind::Tts && m.engine == "piper")
            .find(|m| m.id == self.config.tts.voice || m.id.ends_with(&self.config.tts.voice))
            .ok_or_else(|| ServerError::NoVoice(self.config.tts.voice.clone()))
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
            state.dictionary = UserDictionary::empty();
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
            normalize: arioso_core::Options {
                time_of_day: self.config.tts.time_of_day,
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
        if !report.missing.is_empty() {
            tracing::debug!(missing = ?report.missing, "words absent from the dictionary");
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
}

impl ApiError {
    fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "type": "about:blank",
            "title": self.status.canonical_reason().unwrap_or("error"),
            "status": self.status.as_u16(),
            "detail": self.detail,
        });
        (self.status, Json(body)).into_response()
    }
}

/// Request body for `POST /v1/tts`.
#[derive(Debug, Deserialize)]
pub struct TtsRequest {
    /// Text to speak.
    pub text: String,
    /// Speaking rate multiplier, overriding the config.
    pub speed: Option<f32>,
}

/// One model as reported by `GET /v1/models`.
#[derive(Debug, Serialize)]
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

/// Build the router.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/models", get(list_models))
        .route("/v1/tts", post(synthesize))
        .route("/v1/tts/ws", get(ws_handler))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
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
    let dictionary = serde_json::json!({
        "entries": user.dictionary.len(),
        "error": user.error,
    });
    drop(user);
    Json(serde_json::json!({
        "status": if voice_installed { "ready" } else { "voice_missing" },
        "dictionary": dictionary,
        "voice": voice,
        "voice_installed": voice_installed,
        "voice_loaded": voice_loaded,
        "models_installed": installed,
    }))
}

async fn list_models(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
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
    Json(serde_json::json!({ "models": models }))
}

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
    let speed = request.speed.unwrap_or(state.config.tts.speed);
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
    async fn healthz_answers_ok() {
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
        assert_eq!(json["dictionary"]["entries"], 0);
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
        std::fs::write(
            &path,
            r#"{"version": 1, "entries": [
                {"term": "arioso", "spoken": ["ah ree oh so"]},
                {"term": "Sella", "spoken": ["seller"]}
            ]}"#,
        )
        .unwrap();
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

        std::fs::write(&path, "{ not json").unwrap();
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
