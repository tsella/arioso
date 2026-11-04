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

/// Audio formats this binary can produce.
///
/// `mp3` and `opus` are absent because no encoder for either is linked in. A format the
/// config accepts and the server cannot produce is the same lie as a field nothing reads:
/// the caller asks for one thing and receives another.
pub const AUDIO_FORMATS: [&str; 2] = ["wav", "pcm"];

/// Speech engines this binary can load.
///
/// One engine per process: `tts.engine` chooses it at load, and a change takes a
/// restart. The list is here so the refusal names both, not so a config can use both.
pub const TTS_ENGINES: [&str; 2] = ["piper", "kokoro"];

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
    /// TLS, when the server is exposed beyond loopback.
    #[serde(default)]
    pub tls: Tls,
    /// What a single client may ask for.
    #[serde(default)]
    pub limits: Limits,
    /// Whether `X-Forwarded-For` may name the client.
    ///
    /// Off by default: a header any client can set is not an identity, and trusting it
    /// on a directly exposed server lets one caller spend another's budget.
    #[serde(default)]
    pub trust_forwarded_for: bool,
    /// Whether to serve the API document and the Swagger UI at `/docs`.
    ///
    /// On by default because it is the fastest way to learn the API. A deployment that
    /// does not want to publish its own surface turns it off: a document that names every
    /// route is a map, and a map is the first thing a scanner wants.
    #[serde(default = "yes")]
    pub docs: bool,
    /// Whether `arioso serve` detaches and hands its port to the caller.
    ///
    /// For a caller that wants a server, not a shell: an application spawns arioso, reads
    /// one line from stdout, and talks HTTP. The child prints `START {port}` and nothing
    /// else; logs and errors go to stderr so the port can be read from a pipe without
    /// parsing.
    #[serde(default)]
    pub sidecar: bool,
}

/// The default for a boolean that is on unless the document says otherwise.
fn yes() -> bool {
    true
}

/// Transport security. Absent means plain HTTP, which is right on loopback.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    /// PEM certificate chain. A file path, never a value in this document.
    pub cert: Option<PathBuf>,
    /// PEM private key.
    pub key: Option<PathBuf>,
}

impl Tls {
    /// Whether TLS was asked for at all.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.cert.is_some() || self.key.is_some()
    }
}

/// What one client may ask for, and how long it may take to ask.
///
/// `default` and `deny_unknown_fields` together: a tunable added in a later version must
/// not stop a config file written by an earlier one from loading, and a misspelled field
/// must still be refused rather than silently ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// Requests per second a single client address may make.
    pub requests_per_second: u32,
    /// How many may arrive at once before the rate applies.
    pub burst: u32,
    /// Open sockets the process will hold.
    pub max_connections: usize,
    /// How long a client has to finish sending its request headers.
    ///
    /// Without this, a socket that opens and sends one byte a minute holds a slot.
    pub header_timeout_ms: u64,
    /// Longest text one synthesis request accepts, in characters.
    ///
    /// One request renders the whole text into memory before the reply is written, so this
    /// bounds the work and the memory of a single call. A client with more text sends it in
    /// pieces, or streams it over the WebSocket, which sends a frame per sentence.
    pub max_text_chars: usize,
    /// How long a streaming session may go without audio or a control message.
    ///
    /// A socket that opens and then says nothing holds a session permit and an engine slot.
    /// The clock resets on every audio frame and every control frame.
    pub idle_timeout_ms: u64,
    /// How long a graceful stop waits for in-flight work before ending it.
    ///
    /// A stop that waits forever is not a stop, and one that ends immediately drops
    /// responses that were nearly written. The default is long enough for a synthesis that
    /// has started and short enough that a stuck client cannot hold the process open.
    pub shutdown_grace_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_text_chars: 4_000,
            requests_per_second: 50,
            burst: 100,
            max_connections: 512,
            header_timeout_ms: 10_000,
            idle_timeout_ms: 30_000,
            shutdown_grace_ms: 5_000,
        }
    }
}

impl Default for Server {
    fn default() -> Self {
        Self {
            // 10101 is IANA-registered to a defunct 2002 service and unused in
            // practice, so a stray client does not find the port by accident.
            bind: "127.0.0.1:10101".into(),
            auth: Auth::default(),
            first_message_timeout_ms: 10_000,
            tls: Tls::default(),
            limits: Limits::default(),
            trust_forwarded_for: false,
            docs: true,
            sidecar: false,
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
    /// Speaker number on a multi-speaker voice. Voices with one speaker ignore it.
    pub sid: i64,
    /// Output format: `wav` or `pcm`.
    pub format: String,
    /// The old limit. Read so a config written before it moved still loads, then folded
    /// into `server.limits.max_text_chars`, which is the one the routes enforce.
    #[serde(default, skip_serializing, rename = "max_chars")]
    pub legacy_max_chars: Option<u32>,
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
            // CC0, so the default voice can be used in a shipped product. lessac is
            // Blizzard-2013, research only, and was the default until it was noticed that
            // the one voice you cannot ship was the one everybody got.
            voice: "en_US-joe-medium".into(),
            speed: 1.0,
            sid: 0,
            format: "wav".into(),
            legacy_max_chars: None,
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
            // Parakeet measured better than every Whisper tier on both corpora and runs at
            // whisper-base speed, so it is the default. Whisper base stays in the catalog
            // as the small download. See docs/models.md.
            batch_model: "sherpa-parakeet-tdt-0.6b-v2-en".into(),
            // Empty means the socket uses `batch_model`, which is what every config
            // written before streaming existed does. Naming a streaming model here moves
            // the socket onto that model without touching the file route.
            stream_model: String::new(),
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
    /// Which detector runs. Only `energy` is built in; `silero` is refused rather than
    /// accepted and ignored.
    pub engine: String,
    /// Level above which a frame counts as speech, as a root mean square of samples in the
    /// range -1.0 to 1.0. The energy detector reads it that way, not as a probability.
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
            engine: "energy".into(),
            // A probability default of 0.5 on an RMS detector would need near-clipping
            // audio before it called anything speech.
            threshold: 0.01,
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
    /// Rotated log files to keep before the oldest are removed.
    pub retention_days: u32,
    /// Log synthesized and transcribed text. Off by default, because it is private.
    pub log_text: bool,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: "error".into(),
            format: "pretty".into(),
            file: None,
            retention_days: 7,
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

/// The address a bind string means.
///
/// `localhost` is an alias for 127.0.0.1: the loopback the rest of this project means
/// when it says loopback, so a config that names it gets the same answers as one that
/// spells the address out. Resolving here rather than at the socket makes the rules that
/// ask "is this loopback" answer for the address the socket will take, and it picks IPv4
/// so the port a caller is told is the port that answers.
///
/// A name that is not `localhost` is refused. The loopback rules cannot be applied to a
/// name without a resolver, and a guard that skips what it cannot read is not a guard.
pub fn bind_address(bind: &str) -> Result<SocketAddr, ConfigError> {
    if let Ok(address) = bind.parse::<SocketAddr>() {
        return Ok(address);
    }
    let unusable = |message: String| ConfigError::Invalid {
        path: "server.bind".into(),
        message,
    };
    let Some((host, port)) = bind.rsplit_once(':') else {
        return Err(unusable(format!(
            "server.bind {bind} is not an address; write an IP address and a port, for \
             example 127.0.0.1:10101 or localhost:10101"
        )));
    };
    if host.eq_ignore_ascii_case("localhost") {
        return format!("127.0.0.1:{port}").parse().map_err(|_| {
            unusable(format!(
                "server.bind {bind} has a port this machine cannot read; the port must be a \
                 number from 0 to 65535"
            ))
        });
    }
    Err(unusable(format!(
        "server.bind {bind} names a host, and only localhost can be checked without a \
         resolver. Write an IP address, for example 127.0.0.1:{port} or 0.0.0.0:{port}"
    )))
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

    /// Move a limit that used to live under `tts` into the one the routes read.
    ///
    /// `tts.max_chars` was never enforced. A config that sets it gets the value it asked
    /// for, in the field that works, rather than a silent no-op.
    fn fold_legacy_limits(&mut self) {
        if let Some(chars) = self.tts.legacy_max_chars.take() {
            self.server.limits.max_text_chars = chars as usize;
        }
    }

    /// Parse a config document. `origin` is used for error messages only.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] on malformed JSON, an unknown schema version, or
    /// a value that fails validation.
    pub fn from_json(text: &str, origin: &Path) -> Result<Self, ConfigError> {
        let mut cfg: Self = serde_json::from_str(text).map_err(|source| ConfigError::Parse {
            path: origin.to_path_buf(),
            source,
        })?;
        cfg.fold_legacy_limits();
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
        if let Some(v) = get("ARIOSO_TTS_SID") {
            self.tts.sid = v.parse::<i64>().map_err(|e| ConfigError::Invalid {
                path: "ARIOSO_TTS_SID".into(),
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
        // A sidecar exists to serve the process that started it. Reaching beyond this
        // machine is a different product with a different threat model, and it is refused
        // here so nobody ships the one while believing they have the other.
        // Resolved once, so every rule below answers for the address the socket will
        // actually take rather than for the string a person typed.
        let bind = bind_address(&self.server.bind)?;
        if self.server.sidecar && !bind.ip().is_loopback() {
            return Err(ConfigError::Invalid {
                path: "server.sidecar".into(),
                message: format!(
                    "server.sidecar needs a loopback bind, and server.bind is {}. A sidecar \
                     serves the process that started it; bind 127.0.0.1 or localhost, or turn \
                     server.sidecar off and configure a token.",
                    self.server.bind
                ),
            });
        }
        // A server on a loopback address is reachable by this machine only. Any other
        // bind is reachable by whoever can route to it, and without a token that is
        // everyone. Refusing is the difference between a control and a comment in a log.
        if self.server.auth.mode == "none" && !bind.ip().is_loopback() {
            return Err(ConfigError::Invalid {
                path: "server.auth.mode".into(),
                message: format!(
                    "server.bind {} is not a loopback address, so this server would answer \
                     anyone who can reach it. Set server.auth.mode to \"bearer\" and put the \
                     token in {}. Bind 127.0.0.1 or localhost to keep it on this machine.",
                    self.server.bind, self.server.auth.token_env
                ),
            });
        }
        if self.server.tls.cert.is_some() != self.server.tls.key.is_some() {
            return Err(ConfigError::Invalid {
                path: "server.tls".into(),
                message: "cert and key must be set together; one without the other cannot \
                          serve a connection"
                    .into(),
            });
        }
        if self.server.limits.burst == 0 {
            return Err(ConfigError::Invalid {
                path: "server.limits.burst".into(),
                message: "a burst of 0 refuses every request, which is not a limit".into(),
            });
        }
        if self.server.limits.requests_per_second == 0 {
            return Err(ConfigError::Invalid {
                path: "server.limits.requests_per_second".into(),
                message: "a rate of 0 refuses every request, which is not a limit".into(),
            });
        }
        if self.server.limits.max_text_chars == 0 {
            return Err(ConfigError::Invalid {
                path: "server.limits.max_text_chars".into(),
                message: "a limit of 0 refuses every synthesis request".into(),
            });
        }
        if self.server.limits.max_connections == 0 {
            return Err(ConfigError::Invalid {
                path: "server.limits.max_connections".into(),
                message: "a cap of 0 refuses every connection".into(),
            });
        }
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
        if !TTS_ENGINES.contains(&self.tts.engine.as_str()) {
            return Err(ConfigError::Invalid {
                path: "tts.engine".into(),
                message: format!(
                    "expected one of {}, got {:?}; one engine loads per process",
                    TTS_ENGINES.join(", "),
                    self.tts.engine
                ),
            });
        }
        if !(0.5..=2.0).contains(&self.tts.speed) {
            return Err(ConfigError::Invalid {
                path: "tts.speed".into(),
                message: format!("expected 0.5 to 2.0, got {}", self.tts.speed),
            });
        }
        // The routes refuse a speaker number outside this range, so a config that names one
        // has to be refused here too: otherwise it loads, `doctor` calls it valid, and the
        // first request is the thing that fails.
        if !(0..=4095).contains(&self.tts.sid) {
            return Err(ConfigError::Invalid {
                path: "tts.sid".into(),
                message: format!("expected 0 to 4095, got {}", self.tts.sid),
            });
        }
        if self.log.retention_days == 0 {
            return Err(ConfigError::Invalid {
                path: "log.retention_days".into(),
                message: "must be at least 1".into(),
            });
        }
        if !AUDIO_FORMATS.contains(&self.tts.format.as_str()) {
            return Err(ConfigError::Invalid {
                path: "tts.format".into(),
                message: format!(
                    "expected one of {}, got {:?}; no other audio encoder is built into this binary",
                    AUDIO_FORMATS.join(", "),
                    self.tts.format
                ),
            });
        }
        if self.vad.engine != "energy" {
            return Err(ConfigError::Invalid {
                path: "vad.engine".into(),
                message: format!(
                    "expected energy, got {:?}; no other detector is built into this binary",
                    self.vad.engine
                ),
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
        if self.server.limits.idle_timeout_ms < 1_000 {
            return Err(ConfigError::Invalid {
                path: "server.limits.idle_timeout_ms".into(),
                message: "must be at least 1000; a shorter clock drops active sessions".into(),
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

    /// Every key path in a JSON document, so a key the example omits shows up as a
    /// difference.
    fn key_paths(value: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    out.push(path.clone());
                    key_paths(child, &path, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    key_paths(item, prefix, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn the_example_config_is_the_defaults() {
        // The example is what a new user copies, so a default that moves without it
        // leaves a file that claims to be complete and is not.
        let text = include_str!("../../../config.json.example");
        let example: serde_json::Value = serde_json::from_str(text).expect("the example parses");
        let defaults = serde_json::to_value(Config::default()).expect("the defaults serialize");

        // The keys are compared as well as the values, because every section carries
        // `#[serde(default)]`: a key the example is missing is filled from the very
        // defaults it documents, so parsing alone would pass without the key ever being
        // written down.
        let mut example_keys = Vec::new();
        key_paths(&example, "", &mut example_keys);
        example_keys.sort();
        let mut default_keys = Vec::new();
        key_paths(&defaults, "", &mut default_keys);
        default_keys.sort();
        assert_eq!(example_keys, default_keys, "the example names every field");

        // Values are compared through `Config`, which compares the `f32` threshold as the
        // number it is rather than as the decimal JSON writes.
        let parsed: Config = serde_json::from_str(text).expect("the example parses");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn a_speaker_number_outside_the_route_range_is_refused() {
        // The routes answer 422 for a sid outside 0 to 4095, and the config used to load
        // one anyway, so `doctor` called a config valid that no request could use.
        for sid in ["-1", "99999"] {
            let json = format!(r#"{{"tts":{{"sid":{sid}}}}}"#);
            let err = Config::from_json(&json, &origin()).unwrap_err();
            let message = err.to_string();
            assert!(message.contains("tts.sid"), "{sid}: {message}");
        }
        // Both ends of the range are usable.
        for sid in ["0", "4095"] {
            let json = format!(r#"{{"tts":{{"sid":{sid}}}}}"#);
            let config = Config::from_json(&json, &origin()).expect("in range");
            assert_eq!(config.tts.sid, sid.parse::<i64>().unwrap());
        }
    }

    #[test]
    fn the_speaker_number_can_come_from_the_environment() {
        let mut cfg = Config::default();
        cfg.apply_env(|k| (k == "ARIOSO_TTS_SID").then(|| "7".to_string()))
            .unwrap();
        assert_eq!(cfg.tts.sid, 7);

        let mut cfg = Config::default();
        let err = cfg
            .apply_env(|k| (k == "ARIOSO_TTS_SID").then(|| "seven".to_string()))
            .unwrap_err();
        assert!(err.to_string().contains("ARIOSO_TTS_SID"));
    }

    #[test]
    fn an_old_tts_max_chars_still_takes_effect() {
        // The field was never read. A config that set it got nothing, so the value moves
        // into the limit the routes enforce instead of being dropped.
        let text = r#"{"tts":{"max_chars":1234}}"#;
        let config = Config::from_json(text, std::path::Path::new("test.json")).unwrap();
        assert_eq!(config.server.limits.max_text_chars, 1234);
        assert_eq!(
            config.tts.legacy_max_chars, None,
            "the old field is consumed"
        );
    }

    #[test]
    fn a_vad_engine_that_is_not_built_in_is_refused() {
        // The config used to ship `silero`, which no backend implements. Accepting the
        // name and running the energy detector is the defect this refusal replaces.
        let mut config = Config::default();
        config.vad.engine = "silero".into();
        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("vad.engine"), "{error}");
        assert!(error.to_string().contains("energy"), "{error}");
    }

    #[test]
    fn the_default_vad_threshold_suits_an_rms_detector() {
        // 0.5 was a probability default for a detector that reads a level; at 0.5 RMS the
        // segmenter hears nothing short of clipping.
        assert_eq!(Config::default().vad.engine, "energy");
        assert!(Config::default().vad.threshold < 0.05);
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
    fn the_log_keeps_a_week_and_refuses_none() {
        assert_eq!(Config::default().log.retention_days, 7);
        let err = Config::from_json(r#"{"log":{"retention_days":0}}"#, &origin()).unwrap_err();
        assert!(err.to_string().contains("log.retention_days"), "{err}");
        // A config written before the field existed still loads.
        let old = Config::from_json(r#"{"log":{"level":"info"}}"#, &origin()).unwrap();
        assert_eq!(old.log.retention_days, 7);
    }

    #[test]
    fn an_audio_format_that_is_not_built_in_is_refused() {
        // The route wrote WAV for any format it did not recognise, so a config asking for
        // mp3 got WAV bytes labelled as mp3. The document has to refuse what the binary
        // cannot produce, at load, where the operator sees it.
        for format in ["mp3", "opus", "flac"] {
            let json = format!(r#"{{"tts":{{"format":"{format}"}}}}"#);
            let err = Config::from_json(&json, &origin()).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("tts.format"), "{format}: {msg}");
        }
        for format in AUDIO_FORMATS {
            let json = format!(r#"{{"tts":{{"format":"{format}"}}}}"#);
            assert!(Config::from_json(&json, &origin()).is_ok(), "{format}");
        }
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
    fn a_public_bind_without_a_token_is_refused() {
        let mut config = Config::default();
        config.server.bind = "0.0.0.0:10101".into();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("server.auth.mode"), "{error}");
        assert!(error.contains("ARIOSO_TOKEN"), "{error}");

        // The same address with a token is allowed.
        config.server.auth.mode = "bearer".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_certificate_without_a_key_is_refused() {
        let mut config = Config::default();
        config.server.tls.cert = Some(PathBuf::from("cert.pem"));
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("server.tls"), "{error}");
    }

    #[test]
    fn a_limit_that_refuses_everything_is_refused() {
        for (field, change) in [("burst", 0_u32), ("requests_per_second", 0_u32)] {
            let mut config = Config::default();
            match field {
                "burst" => config.server.limits.burst = change,
                _ => config.server.limits.requests_per_second = change,
            }
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(field), "{field} must be named: {error}");
        }
    }

    #[test]
    fn a_sidecar_needs_a_loopback_bind() {
        let mut config = Config::default();
        config.server.sidecar = true;
        assert!(config.validate().is_ok(), "the default bind is loopback");

        config.server.bind = "0.0.0.0:10101".into();
        config.server.auth.mode = "bearer".into();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("server.sidecar"),
            "the refusal should name the option: {error}"
        );
    }

    #[test]
    fn sidecar_is_off_unless_the_document_says_otherwise() {
        assert!(!Config::default().server.sidecar);
        let config = Config::from_json(
            r#"{"schema_version":1,"server":{"sidecar":true}}"#,
            std::path::Path::new("t.json"),
        )
        .unwrap();
        assert!(config.server.sidecar);
    }

    #[test]
    fn a_limit_block_may_name_one_field() {
        // A config written before a tunable existed still loads, and takes the default
        // for what it does not mention.
        let config = Config::from_json(
            r#"{"schema_version":1,"server":{"limits":{"burst":9}}}"#,
            std::path::Path::new("t.json"),
        )
        .unwrap();
        assert_eq!(config.server.limits.burst, 9);
        assert_eq!(
            config.server.limits.shutdown_grace_ms,
            Limits::default().shutdown_grace_ms
        );

        // A misspelled field is still an error, not a silently ignored default.
        let error = Config::from_json(
            r#"{"schema_version":1,"server":{"limits":{"brust":9}}}"#,
            std::path::Path::new("t.json"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("brust"), "{error}");
    }

    #[test]
    fn localhost_is_the_loopback_for_every_rule() {
        let mut config = Config::default();
        config.server.bind = "localhost:10101".into();
        config.server.sidecar = true;
        config
            .validate()
            .expect("localhost is loopback, so a sidecar may bind it");

        let mut config = Config::default();
        config.server.bind = "localhost:10101".into();
        config.server.auth.mode = "none".into();
        config
            .validate()
            .expect("localhost is loopback, so no token is needed");
    }

    #[test]
    fn localhost_resolves_to_the_ipv4_loopback() {
        assert_eq!(
            bind_address("localhost:10101").unwrap().to_string(),
            "127.0.0.1:10101"
        );
        // Host names are compared without regard to case.
        assert_eq!(
            bind_address("LOCALHOST:9").unwrap().to_string(),
            "127.0.0.1:9"
        );
    }

    #[test]
    fn a_bind_the_rules_cannot_read_is_refused_rather_than_skipped() {
        // Before this, a name that did not parse fell past the token guard entirely, so a
        // public hostname with no token loaded and then bound.
        let mut config = Config::default();
        config.server.bind = "myhost.lan:10101".into();
        config.server.auth.mode = "none".into();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("server.bind"), "{err}");
        assert!(err.to_string().contains("localhost"), "{err}");
    }

    #[test]
    fn the_text_limit_has_a_default_and_refuses_zero() {
        assert_eq!(Limits::default().max_text_chars, 4_000);
        let mut config = Config::default();
        config.server.limits.max_text_chars = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_text_chars"), "{err}");
    }

    #[test]
    fn missing_file_is_defaults() {
        let cfg = Config::load(Path::new("/nonexistent/arioso.json")).unwrap();
        assert_eq!(cfg, Config::default());
    }
}
