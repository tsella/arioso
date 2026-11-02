//! Configuration model for arioso.
//!
//! Precedence, lowest to highest: compiled defaults, the JSON file, environment
//! variables with the `ARIOSO_` prefix.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Schema version written by this build.
pub const SCHEMA_VERSION: u32 = 1;

/// Everything arioso reads at startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Config {
    /// Config document version. Older documents are migrated, newer ones refused.
    pub schema_version: u32,
    /// HTTP and WebSocket listener.
    pub server: Server,
    /// Text to speech.
    pub tts: Tts,
    /// Speech to text.
    pub stt: Stt,
    /// Voice activity detection.
    pub vad: Vad,
    /// Audio routing and playback.
    pub audio: Audio,
    /// Model cache and download behaviour.
    pub models: Models,
    /// Logging.
    pub log: Log,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            server: Server::default(),
            tts: Tts::default(),
            stt: Stt::default(),
            vad: Vad::default(),
            audio: Audio::default(),
            models: Models::default(),
            log: Log::default(),
        }
    }
}

/// HTTP and WebSocket listener settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Server {
    /// Address to bind. Loopback by default.
    pub bind: String,
    /// Bearer token policy for exposure beyond loopback.
    pub auth: Auth,
    /// How long a WebSocket client may stay silent before it is dropped.
    ///
    /// Without this, a client that connects and sends nothing holds a task for
    /// as long as it keeps the connection open.
    pub first_message_timeout_ms: u64,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            // 10101 is IANA-registered to a defunct 2002 service and unused in
            // practice, so a stray client does not find the port by accident.
            bind: "127.0.0.1:10101".into(),
            auth: Auth::default(),
            first_message_timeout_ms: 10_000,
        }
    }
}

/// Bearer token policy. The token itself is read from the environment only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Auth {
    /// `none` or `bearer`.
    pub mode: String,
    /// Environment variable holding the token.
    pub token_env: String,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            mode: "none".into(),
            token_env: "ARIOSO_TOKEN".into(),
        }
    }
}

/// Text to speech settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Tts {
    /// Whether the TTS routes and engines are active.
    pub enabled: bool,
    /// Engine id: `piper` or `kokoro`.
    pub engine: String,
    /// Voice id understood by the engine.
    pub voice: String,
    /// Speaking rate multiplier.
    pub speed: f32,
    /// Output format: `wav`, `pcm`, `mp3`, or `opus`.
    pub format: String,
    /// Longest accepted input, in characters.
    pub max_chars: u32,
    /// Path to the user dictionary. Defaults to `dictionary.json` beside the
    /// config file.
    #[serde(default)]
    pub dictionary: Option<PathBuf>,
    /// Say the part of the day after a clock time: "seven forty five in the
    /// evening". A written `am` or `pm` decides it; otherwise the hour does.
    #[serde(default = "default_true")]
    pub time_of_day: bool,
}

/// A default that is on.
fn default_true() -> bool {
    true
}

impl Default for Tts {
    fn default() -> Self {
        Self {
            enabled: true,
            engine: "piper".into(),
            voice: "en_US-lessac-medium".into(),
            speed: 1.0,
            format: "wav".into(),
            max_chars: 4000,
            dictionary: None,
            time_of_day: true,
        }
    }
}

/// Speech to text settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Stt {
    /// Whether the STT routes and engines are active.
    pub enabled: bool,
    /// Engine id. `sherpa` is the only engine in v1.
    pub engine: String,
    /// Model used for uploaded files.
    pub batch_model: String,
    /// Model used for the streaming WebSocket.
    pub stream_model: String,
    /// Language code, or `auto`.
    pub language: String,
    /// Emit partial hypotheses while audio arrives.
    pub partials: bool,
    /// Milliseconds between partials.
    pub partial_interval_ms: u32,
    /// Milliseconds a partial must stay stable before it is emitted.
    pub stability_window_ms: u32,
    /// Ask the engine for word timestamps.
    pub word_timestamps: bool,
    /// Longest accepted audio, in seconds.
    pub max_audio_s: u32,
    /// Capture sources. `mic` only until the Windows dual-capture work lands.
    pub capture: Capture,
}

impl Default for Stt {
    fn default() -> Self {
        Self {
            enabled: true,
            engine: "sherpa".into(),
            // Base, not tiny: measured on the dictionary round trip, tiny wrote
            // "Tonseller built REO so" where base wrote "Tom Sella built a Rio so",
            // and the phonetic correction then swallowed the word "built". The extra
            // 57 MiB buys a transcript that is actually right.
            batch_model: "sherpa-whisper-base-en".into(),
            stream_model: "sherpa-zipformer-en".into(),
            language: "auto".into(),
            partials: true,
            partial_interval_ms: 300,
            stability_window_ms: 200,
            word_timestamps: false,
            max_audio_s: 600,
            capture: Capture::default(),
        }
    }
}

/// Audio capture settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Capture {
    /// Sources to record. `mic`, `loopback`, `process`.
    pub sources: Vec<String>,
    /// `separate` keeps one stream per source, `mixed` sums them.
    pub mode: String,
    /// Render device id for loopback, or null for the default device.
    pub loopback_device: Option<String>,
}

impl Default for Capture {
    fn default() -> Self {
        Self {
            sources: vec!["mic".into()],
            mode: "separate".into(),
            loopback_device: None,
        }
    }
}

/// Voice activity detection settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Vad {
    /// `silero` or `energy`.
    pub engine: String,
    /// Speech probability threshold, 0.0 to 1.0.
    pub threshold: f32,
    /// Shortest run of speech that counts, in milliseconds.
    pub min_speech_ms: u32,
    /// Silence that ends an utterance, in milliseconds.
    pub min_silence_ms: u32,
    /// Audio kept before the detected start, in milliseconds.
    pub padding_ms: u32,
    /// Hard cap on one segment, in milliseconds.
    pub max_segment_ms: u32,
    /// Analysis frame size, in milliseconds.
    pub frame_ms: u32,
}

impl Default for Vad {
    fn default() -> Self {
        Self {
            engine: "silero".into(),
            threshold: 0.5,
            min_speech_ms: 250,
            min_silence_ms: 700,
            padding_ms: 200,
            max_segment_ms: 30_000,
            frame_ms: 32,
        }
    }
}

/// Where the audio goes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Audio {
    /// `api`, `device`, or `both`.
    pub output: String,
    /// Playback device id, or null for the default device.
    pub device: Option<String>,
    /// Playback queue bound, in milliseconds.
    pub queue_ms: u32,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            output: "api".into(),
            device: None,
            queue_ms: 2000,
        }
    }
}

/// Model cache and download behaviour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Models {
    /// Cache directory. Null means the platform data directory.
    pub dir: Option<PathBuf>,
    /// Model ids fetched at startup.
    pub auto_download: Vec<String>,
    /// Base URL replacing the default host, for mirrors.
    pub mirror: Option<String>,
    /// Download concurrency cap.
    pub max_concurrent_downloads: u32,
}

impl Default for Models {
    fn default() -> Self {
        Self {
            dir: None,
            auto_download: Vec::new(),
            mirror: None,
            max_concurrent_downloads: 2,
        }
    }
}

/// Logging settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Log {
    /// `trace`, `debug`, `info`, `warn`, or `error`.
    pub level: String,
    /// `pretty` or `json`.
    pub format: String,
    /// Optional log file. Rotated daily when set.
    pub file: Option<PathBuf>,
    /// Log synthesized and transcribed text. Off by default, because it is private.
    pub log_text: bool,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "pretty".into(),
            file: None,
            log_text: false,
        }
    }
}

/// Why a config could not be loaded or used.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file exists but could not be read.
    #[error("reading {path}: {source}")]
    Io {
        /// File that failed.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// The file is not valid JSON or does not match the schema.
    #[error("parsing {path}: {source}")]
    Parse {
        /// File that failed.
        path: PathBuf,
        /// Underlying error.
        source: serde_json::Error,
    },
    /// The document is newer or older than this build understands.
    #[error("unsupported schema_version {found}, this build supports {supported}")]
    UnsupportedVersion {
        /// Version found in the document.
        found: u32,
        /// Version this build writes.
        supported: u32,
    },
    /// A value is present but out of range.
    #[error("invalid config at {path}: {message}")]
    Invalid {
        /// Dotted path of the offending value.
        path: String,
        /// What is wrong with it.
        message: String,
    },
}

impl Config {
    /// Load a config file, falling back to defaults when the file is absent.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file exists but cannot be read, parsed,
    /// or validated.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_json(&text, path)
    }

    /// Parse a config document. `origin` is used for error messages only.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] on malformed JSON, an unknown schema version, or
    /// a value that fails validation.
    pub fn from_json(text: &str, origin: &Path) -> Result<Self, ConfigError> {
        let cfg: Self = serde_json::from_str(text).map_err(|source| ConfigError::Parse {
            path: origin.to_path_buf(),
            source,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load a file and then apply `ARIOSO_*` environment overrides.
    ///
    /// # Errors
    ///
    /// As [`Config::load`], plus [`ConfigError::Invalid`] for a malformed
    /// environment value.
    /// Write the document to `path`, atomically.
    ///
    /// The text goes to a sibling file first and is renamed into place, so a crash
    /// mid-write cannot leave a half-written config behind: a reader sees either the old
    /// document or the new one.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] when the directory or file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let temporary = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        std::fs::write(&temporary, format!("{text}\n")).map_err(|source| ConfigError::Io {
            path: temporary.clone(),
            source,
        })?;
        std::fs::rename(&temporary, path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn load_with_env(path: &Path) -> Result<Self, ConfigError> {
        let mut cfg = Self::load(path)?;
        cfg.apply_env(|key| std::env::var(key).ok())?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Apply environment overrides through `get`, so tests can inject values.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] when a value cannot be parsed.
    pub fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) -> Result<(), ConfigError> {
        if let Some(v) = get("ARIOSO_SERVER_BIND") {
            self.server.bind = v;
        }
        if let Some(v) = get("ARIOSO_AUTH_MODE") {
            self.server.auth.mode = v;
        }
        if let Some(v) = get("ARIOSO_TTS_ENABLED") {
            self.tts.enabled = parse_bool(&v, "ARIOSO_TTS_ENABLED")?;
        }
        if let Some(v) = get("ARIOSO_TTS_ENGINE") {
            self.tts.engine = v;
        }
        if let Some(v) = get("ARIOSO_TTS_SPEED") {
            self.tts.speed = v.parse::<f32>().map_err(|e| ConfigError::Invalid {
                path: "ARIOSO_TTS_SPEED".into(),
                message: e.to_string(),
            })?;
        }
        if let Some(v) = get("ARIOSO_TTS_DICTIONARY") {
            self.tts.dictionary = Some(PathBuf::from(v));
        }
        if let Some(v) = get("ARIOSO_TTS_TIME_OF_DAY") {
            self.tts.time_of_day = parse_bool(&v, "ARIOSO_TTS_TIME_OF_DAY")?;
        }
        if let Some(v) = get("ARIOSO_TTS_VOICE") {
            self.tts.voice = v;
        }
        if let Some(v) = get("ARIOSO_STT_ENABLED") {
            self.stt.enabled = parse_bool(&v, "ARIOSO_STT_ENABLED")?;
        }
        if let Some(v) = get("ARIOSO_STT_ENGINE") {
            self.stt.engine = v;
        }
        if let Some(v) = get("ARIOSO_MODELS_DIR") {
            self.models.dir = Some(PathBuf::from(v));
        }
        if let Some(v) = get("ARIOSO_LOG_LEVEL") {
            self.log.level = v;
        }
        if let Some(v) = get("ARIOSO_LOG_FORMAT") {
            self.log.format = v;
        }
        Ok(())
    }

    /// Check every value that has a valid range or a closed set of choices.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnsupportedVersion`] or [`ConfigError::Invalid`].
    /// Where the user dictionary lives: the configured path when set, else
    /// `dictionary.json` beside the config file, else beside the models.
    #[must_use]
    pub fn dictionary_path(&self, config_file: Option<&Path>) -> PathBuf {
        if let Some(path) = &self.tts.dictionary {
            return path.clone();
        }
        if let Some(directory) = config_file.and_then(Path::parent) {
            return directory.join("dictionary.json");
        }
        PathBuf::from("dictionary.json")
    }

    /// Validate every field, naming the offending path.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedVersion {
                found: self.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        self.server
            .bind
            .parse::<SocketAddr>()
            .map_err(|e| ConfigError::Invalid {
                path: "server.bind".into(),
                message: e.to_string(),
            })?;
        if !(100..=600_000).contains(&self.server.first_message_timeout_ms) {
            return Err(ConfigError::Invalid {
                path: "server.first_message_timeout_ms".into(),
                message: format!(
                    "{} is outside 100 to 600000",
                    self.server.first_message_timeout_ms
                ),
            });
        }
        match self.server.auth.mode.as_str() {
            "none" | "bearer" => {}
            other => {
                return Err(ConfigError::Invalid {
                    path: "server.auth.mode".into(),
                    message: format!("expected none or bearer, got {other}"),
                });
            }
        }
        if !(0.5..=2.0).contains(&self.tts.speed) {
            return Err(ConfigError::Invalid {
                path: "tts.speed".into(),
                message: format!("expected 0.5 to 2.0, got {}", self.tts.speed),
            });
        }
        if !(0.0..=1.0).contains(&self.vad.threshold) {
            return Err(ConfigError::Invalid {
                path: "vad.threshold".into(),
                message: format!("expected 0.0 to 1.0, got {}", self.vad.threshold),
            });
        }
        if self.vad.frame_ms == 0 {
            return Err(ConfigError::Invalid {
                path: "vad.frame_ms".into(),
                message: "must be greater than zero".into(),
            });
        }
        if self.vad.max_segment_ms < self.vad.min_speech_ms {
            return Err(ConfigError::Invalid {
                path: "vad.max_segment_ms".into(),
                message: "must be at least vad.min_speech_ms".into(),
            });
        }
        if self.models.max_concurrent_downloads == 0 {
            return Err(ConfigError::Invalid {
                path: "models.max_concurrent_downloads".into(),
                message: "must be at least 1".into(),
            });
        }
        match self.log.level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" => {}
            other => {
                return Err(ConfigError::Invalid {
                    path: "log.level".into(),
                    message: format!("expected trace, debug, info, warn, or error, got {other}"),
                });
            }
        }
        match self.log.format.as_str() {
            "pretty" | "json" => {}
            other => {
                return Err(ConfigError::Invalid {
                    path: "log.format".into(),
                    message: format!("expected pretty or json, got {other}"),
                });
            }
        }
        Ok(())
    }

    /// The config as a JSON Schema document.
    #[must_use]
    pub fn json_schema() -> schemars::Schema {
        schemars::schema_for!(Config)
    }
}

fn parse_bool(value: &str, key: &str) -> Result<bool, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(ConfigError::Invalid {
            path: key.into(),
            message: format!("expected a boolean, got {other}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> PathBuf {
        PathBuf::from("<test>")
    }

    #[test]
    fn the_example_config_is_the_defaults() {
        // The example is what a new user copies, so a default that moves without it
        // leaves a file that claims to be complete and is not.
        let text = include_str!("../../../config.json.example");
        let parsed: Config = serde_json::from_str(text).expect("the example parses");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn default_bind_is_loopback_on_10101() {
        assert_eq!(Config::default().server.bind, "127.0.0.1:10101");
    }

    #[test]
    fn defaults_validate() {
        Config::default().validate().expect("defaults are valid");
    }

    #[test]
    fn empty_document_is_defaults() {
        let cfg = Config::from_json("{}", &origin()).expect("empty object is valid");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compatibility() {
        let cfg =
            Config::from_json(r#"{"tts":{"future_knob":true}}"#, &origin()).expect("tolerated");
        assert_eq!(cfg.tts.engine, "piper");
    }

    #[test]
    fn schema_version_is_enforced() {
        let err = Config::from_json(r#"{"schema_version":99}"#, &origin()).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::UnsupportedVersion { found: 99, .. }
        ));
    }

    #[test]
    fn speed_out_of_range_names_the_path() {
        let err = Config::from_json(r#"{"tts":{"speed":3.0}}"#, &origin()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("tts.speed"), "{msg}");
    }

    #[test]
    fn the_speed_can_come_from_the_environment() {
        let mut cfg = Config::default();
        cfg.apply_env(|k| (k == "ARIOSO_TTS_SPEED").then(|| "1.75".to_string()))
            .unwrap();
        assert!((cfg.tts.speed - 1.75).abs() < f32::EPSILON);

        let mut cfg = Config::default();
        let err = cfg
            .apply_env(|k| (k == "ARIOSO_TTS_SPEED").then(|| "fast".to_string()))
            .unwrap_err();
        assert!(err.to_string().contains("ARIOSO_TTS_SPEED"));
    }

    #[test]
    fn a_silent_client_timeout_is_validated() {
        let err = Config::from_json(r#"{"server":{"first_message_timeout_ms":5}}"#, &origin())
            .unwrap_err();
        assert!(err.to_string().contains("server.first_message_timeout_ms"));
        assert_eq!(Config::default().server.first_message_timeout_ms, 10_000);
    }

    #[test]
    fn bad_bind_names_the_path() {
        let err =
            Config::from_json(r#"{"server":{"bind":"not-an-address"}}"#, &origin()).unwrap_err();
        assert!(err.to_string().contains("server.bind"));
    }

    #[test]
    fn env_overrides_win_over_file() {
        let mut cfg = Config::from_json(r#"{"tts":{"engine":"kokoro"}}"#, &origin()).unwrap();
        cfg.apply_env(|k| match k {
            "ARIOSO_TTS_ENGINE" => Some("piper".into()),
            "ARIOSO_TTS_ENABLED" => Some("false".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(cfg.tts.engine, "piper");
        assert!(!cfg.tts.enabled);
    }

    #[test]
    fn the_part_of_the_day_is_on_unless_turned_off() {
        assert!(Config::default().tts.time_of_day);
        let file = Config::from_json(r#"{"tts":{}}"#, &origin()).unwrap();
        assert!(file.tts.time_of_day, "absent means on");

        let mut cfg = Config::from_json(r#"{"tts":{"time_of_day":false}}"#, &origin()).unwrap();
        assert!(!cfg.tts.time_of_day);

        cfg.apply_env(|k| (k == "ARIOSO_TTS_TIME_OF_DAY").then(|| "true".to_string()))
            .unwrap();
        assert!(cfg.tts.time_of_day, "the environment wins");
    }

    #[test]
    fn bad_env_boolean_names_the_variable() {
        let mut cfg = Config::default();
        let err = cfg
            .apply_env(|k| (k == "ARIOSO_STT_ENABLED").then(|| "maybe".to_string()))
            .unwrap_err();
        assert!(err.to_string().contains("ARIOSO_STT_ENABLED"));
    }

    #[test]
    fn schema_is_generated() {
        let schema = serde_json::to_value(Config::json_schema()).unwrap();
        assert_eq!(schema["title"], "Config");
        assert!(schema["properties"]["server"].is_object());
    }

    #[test]
    fn missing_file_is_defaults() {
        let cfg = Config::load(Path::new("/nonexistent/arioso.json")).unwrap();
        assert_eq!(cfg, Config::default());
    }
}
