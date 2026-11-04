# arioso: Product Requirements Document

Version 0.6 · 3 November 2026 · Owner: Tom Sella

---

## 1. Summary

arioso is a Rust library and standalone server that gives any application local
text to speech and speech to text, on macOS, Linux, Windows, iOS, and Android,
with no cloud dependency.

One binary, one config file, one HTTP and WebSocket API. Models download on first
run, and the caller can show download progress and readiness. Everything else,
including which engines are on, which voice speaks, where the audio goes, and how
the VAD behaves, is set through the API or the config file.

## 2. Problem

Local speech today is a pile of half-integrated pieces. Piper runs in Python,
Kokoro needs its own G2P, recognition needs a C++ build per platform, and VAD is a
separate model. Every app that wants speech ends up writing the same glue: download a model,
check a hash, load an ONNX session, resample audio, detect silence, chunk text,
stream partial results, handle failures.

There is no single artifact that an application can depend on to get all of this
on every platform it ships to.

## 3. Users

| User | What they need |
|---|---|
| App developer embedding the library | One dependency, model download handled, progress events, no network at runtime |
| Service developer using the server | Stable HTTP and WS API, OpenAPI spec, predictable latency, no cloud keys |
| Desktop tool author | Local playback with device selection, low latency, small binary |
| Mobile developer | Static library, small footprint, works offline after first download |
| Self-hoster | Single binary, config file, Docker image, logs they can read |

## 4. Goals

1. One Rust library that runs on all five platforms, with C ABI for embedding and
   uniffi bindings in a later release.
2. A standalone binary that exposes the library over HTTP and WebSocket with an
   OpenAPI 3.1 spec and Swagger UI.
3. Model lifecycle handled end to end: resolve, download, verify, load, report
   progress, unload.
4. Streaming in both directions: audio out for TTS, partial transcripts in for STT.
5. Everything configurable at runtime through the API, with a JSON file as the
   starting point.
6. Plain, structured logs through `tracing`.

## 5. Non-goals for v1

- Model training or fine-tuning.
- Voice cloning.
- Speaker diarization.
- Cloud providers (OpenAI, ElevenLabs, Azure). The design leaves room for a
  `CloudTts` engine, but v1 ships local only.
- Multi-tenant auth, quotas, or billing.
- GUI. The sample app is a config file and a CLI.

## 6. Decisions already made

| Topic | Decision |
|---|---|
| TTS engines | Pluggable trait. Piper and Kokoro ship in v1 |
| STT engine | Pluggable trait. sherpa-onnx anchors v1: batch Whisper or SenseVoice, streaming Zipformer transducer |
| Bindings | Rust crate plus C ABI in v1. uniffi Swift and Kotlin in v1.1 |
| Audio output | Both: PCM or WAV over the API, plus optional local playback with device selection |
| API exposure | Binds 127.0.0.1 by default, no auth. Bearer token is opt-in for LAN exposure |
| Phonemizer licensing | Permissive default (CMUdict plus letter-to-sound rules). espeak-ng behind an opt-in `gpl-espeak` Cargo feature, desktop only |
| Logging | `tracing` plus `tracing-subscriber` |
| Windows dual capture | Mic plus system loopback, separate per-source transcripts by default |

## 7. Architecture

### 7.1 Workspace layout

```
arioso/
  crates/
    arioso-core/        traits, types, errors, session lifecycle, readiness
    arioso-config/      config model, precedence, validation, JSON schema
    arioso-models/      manifest, downloader, cache, hash verification, progress
    arioso-audio/       PCM types, resampling, WAV/Opus encode, cpal playback
    arioso-vad/         VAD trait, Silero backend, energy backend
    arioso-capture-win/ WASAPI mic plus loopback capture, drift-compensated mixer
    arioso-g2p/         permissive English G2P, optional espeak bridge
    arioso-tts-piper/   Piper engine (ort)
    arioso-tts-kokoro/  Kokoro engine (ort)
    arioso-stt-sherpa/  sherpa-onnx engine, batch and streaming models
    arioso-server/      axum HTTP + WS, utoipa OpenAPI, Swagger UI
    arioso-ffi/         C ABI, cbindgen header
    arioso-cli/         `arioso` binary
  apps/
    sample/              JSON config + README showing library use
```

`arioso-cli` is a thin wrapper: parse flags, load config, start `arioso-server`,
or run one-shot commands (`arioso say`, `arioso transcribe file.wav`).

### 7.2 Core traits

```rust
pub trait TtsEngine: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> TtsCaps;
    async fn load(&mut self, model: &ModelHandle) -> Result<()>;
    async fn unload(&mut self) -> Result<()>;
    async fn voices(&self) -> Result<Vec<Voice>>;
    async fn synthesize(&self, req: SynthRequest) -> Result<AudioStream>;
}

pub trait SttEngine: Send + Sync {
    fn id(&self) -> &'static str;
    async fn load(&mut self, model: &ModelHandle) -> Result<()>;
    async fn transcribe(&self, audio: AudioSource, opts: SttOptions) -> Result<Transcript>;
    fn open_stream(&self, opts: SttOptions) -> Result<Box<dyn SttSession>>;
}

pub trait SttSession: Send {
    fn push(&mut self, pcm: &[u8]) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
    fn next_event(&mut self) -> Option<SttEvent>;
}

pub trait Vad: Send + Sync {
    fn push(&mut self, frame: &[f32]) -> VadState;
    fn reset(&mut self);
}
```

`AudioStream` is `Stream<Item = Result<AudioChunk>>`. Each chunk carries PCM, its
sample rate, and the text span it covers, so a caller can do word highlighting
without a second request.

### 7.3 Audio I/O backends

Playback uses `cpal`, which covers macOS, Linux, Windows, iOS, and Android with one
API. Capture differs per platform, so it lives behind the capture trait:

- Windows: the `wasapi` crate (MIT, 0.26) for both the microphone and loopback. It
  exposes `AUDCLNT_STREAMFLAGS_LOOPBACK` directly, and `cpal` does not offer
  loopback on Windows.
- macOS, Linux, iOS, Android: `cpal` input only in v1.

### 7.4 Pipeline

TTS: text → normalize → G2P → phoneme IDs → engine inference → PCM chunks →
resample → API sink or device sink.

STT: PCM frames → resampler to 16 kHz mono → VAD → segmenter → engine → partial
and final events → API sink.

Both pipelines are push based and cancellable. Dropping the stream stops inference
at the next chunk boundary.

## 8. Functional requirements

### F1. Model management

The library ships no model weights. A manifest describes every supported model.

```json
{
  "id": "piper-en_US-lessac-medium",
  "kind": "tts",
  "engine": "piper",
  "version": "1.0.0",
  "license": "Blizzard-2013",
  "files": [
    {
      "name": "en_US-lessac-medium.onnx",
      "url": "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx",
      "sha256": "<pinned>",
      "size": 63201294
    }
  ],
  "meta": { "sample_rate": 22050, "num_speakers": 1, "languages": ["en-US"] }
}
```

A streaming STT model looks different, because sherpa-onnx models are directories:

```json
{
  "id": "sherpa-zipformer-en",
  "kind": "stt",
  "engine": "sherpa",
  "mode": "streaming",
  "version": "2023-11-22",
  "license": "Apache-2.0",
  "files": [
    { "name": "encoder-epoch-99-avg-1.onnx", "sha256": "<pinned>", "size": 0 },
    { "name": "decoder-epoch-99-avg-1.onnx", "sha256": "<pinned>", "size": 0 },
    { "name": "joiner-epoch-99-avg-1.onnx", "sha256": "<pinned>", "size": 0 },
    { "name": "tokens.txt", "sha256": "<pinned>", "size": 0 }
  ],
  "meta": { "sample_rate": 16000, "languages": ["en"], "chunk_ms": 320 }
}
```

Requirements:

- F1.1 Downloads resume after interruption. Partial files use a `.part` suffix and
  are verified before an atomic rename.
- F1.2 Every file is checked against a pinned SHA-256. A mismatch deletes the file
  and reports `IntegrityFailure`.
- F1.3 Progress is observable: bytes downloaded, total bytes, percent, and an ETA.
  Progress is available both as an API event stream and as a callback in the
  library and C ABI.
- F1.4 Cache directory is platform correct: `~/Library/Application Support/arioso`,
  `~/.local/share/arioso`, `%LOCALAPPDATA%\arioso`, and the app sandbox on mobile.
  Configurable.
- F1.5 A mirror base URL can replace the default host. Useful behind a proxy.
- F1.6 Models can be pre-seeded by dropping files into the cache. A manifest
  checksum file is written on first successful download.
- F1.7 `auto_download` in config lists model IDs to fetch at startup. Empty means
  the server starts with no models and reports `not_installed`.
- F1.8 Disk space is checked before download. Failure is reported before any bytes
  move.
- F1.9 Concurrent downloads are serialized per model and capped globally (default 2).
- F1.10 The manifest itself is signed. One detached signature over the manifest,
  verified against a public key compiled into the binary. Per file SHA-256 stops
  corruption; the signature stops a hostile mirror.
- F1.11 Model upgrades do not interrupt service. A new version downloads alongside
  the old one, both appear in `GET /v1/models`, and the switch happens on the next
  load or restart. The old version is deleted only after a successful load of the
  new one.
- F1.12 `models.pinned` freezes a model ID to a version. Without a pin, the newest
  verified version wins.
- F1.13 `POST /v1/models/{id}/warm` loads a model without a request, so the first
  real call does not pay load latency.

### F2. Readiness

Each model moves through `not_installed → downloading → verifying → loading →
ready`, or `failed { reason }`.

- F2.1 `GET /readyz` answers 200 only when every enabled engine has its model on disk,
  and 503 with `voice_missing` or `stt_missing` otherwise. An engine that is switched off
  does not count against readiness. The status code is the answer: an orchestrator reads
  the code, not the body.
- F2.2 `GET /v1/models` reports state and progress for every known model: `state`
  (`installed`, `downloading`, `not_installed`), `downloaded`, `bytes`, and
  `commercial_use`. A caller polling after a download does not re-POST to find out whether
  anything is happening.
- F2.3 The library exposes the same state as a `watch` channel, so an app can draw
  a progress bar and enable its UI at the right moment.
- F2.4 A failed model does not take down the server. The affected engine reports
  `unavailable` with a reason, the rest keeps working.
- F2.5 `GET /v1/models/events` is a server-sent event stream of the state `GET /v1/models`
  reports, so a client draws a progress bar without polling. It opens with a `snapshot`
  event carrying `{id, state, downloaded, total, error}` for every model, then sends a
  `state` event at each transition and a `progress` event for each byte count. An idle
  stream carries a comment line every 15 seconds, which keeps a proxy from closing it and
  is not an event. A subscriber more than 256 events behind is told it lagged, with the
  count it missed, and handed a fresh snapshot: a stream that skipped events silently
  would leave a client wrong and unaware of it. Auth and the rate limiter apply as they do
  to every other route.

### F3. Text to speech

- F3.1 Engines: `piper` and `kokoro` in v1. Selection is per request and by default
  from config.
- F3.2 Piper: 178 voices across 53 languages, all of them in the catalog. Sample rate follows
  the quality tier: 22.05 kHz for 124 medium and most high voices, 16 kHz for the low and x_low
  ones, and one outlier at 44.1 kHz (`fr_FR-tom-medium`). The rate is read from the voice's own
  config, never assumed from the tier. Voice IDs are the upstream names
  (`en_US-lessac-medium`), one of which is not ASCII (`pt_PT-tugão-medium`), so ids are matched
  and encoded as UTF-8 end to end.
- F3.3 Kokoro: 54 voices, 24 kHz, English plus seven other languages. Voice IDs are
  the upstream names (`af_heart`, `am_michael`).
- F3.4 Controls: voice, speed (0.5 to 2.0), speaker (`sid`), and format (`wav`, `pcm`). `pcm` is raw
  16-bit samples with the rate in the content type (`audio/pcm;rate=22050`). `mp3` and
  `opus` are not implemented: the config refuses them rather than answering with WAV bytes
  labelled as what was asked for.
- F3.5 Long text is split at sentence boundaries and synthesized incrementally, so
  first audio arrives before the last sentence is computed.
- F3.6 Streaming: chunked HTTP for progressive download, and a WebSocket that
  accepts text fragments and returns audio frames plus a timing event.
- F3.7 Output is deterministic for a fixed voice, speed, and engine version.
- F3.8 Unsupported voice or language returns 422 with the list of valid values.
- F3.10 A multi-speaker voice takes a speaker number: `sid` on the request, `tts.sid` in the
  config, `--sid` on `arioso say`. The count comes from the voice config (`num_speakers`) and
  the input from the graph, because a voice can be trained on several speakers and exported
  without the input. A number the voice does not have is a 422 `invalid_value` on the HTTP
  routes and an `error` frame on the WebSocket, never a 500 and never a silent fallback: a
  caller that asked for a voice and got a different one has no way to notice. A single-speaker
  voice accepts only 0 and refuses the rest by name.
- F3.9 Text passes through normalization and the user dictionary before phonemization.
  See F14.

- F3.12 `arioso models list` filters by language (a code or a family), by kind, and by whether
  the files are on disk, because 178 rows is a catalog only with a filter.

- F3.11 Twenty-six voices are multi-speaker, and each carries its speaker names in the catalog.
  Eighty-two voices state a licence that allows commercial use; seventy-three state none, which
  the catalog records as `unknown` with `commercial_use: false` rather than guessing. The card's
  parent model is kept in `meta.finetuned_from`, because most English voices are finetuned from
  a research-only corpus and that has to be read before anything ships.

### F4. Speech to text

The engine is sherpa-onnx (Apache-2.0). It gives one dependency for both paths,
because it runs batch models and natively streaming models behind the same C API.

- F4.1 Two model families, selected per model, not per request:
  - Batch: Whisper and transducer models (Parakeet, and Zipformer run offline) for
    uploaded files. The family comes from the catalog entry, in `meta.model_type`;
    an entry without it is read as Whisper.
  - Streaming: Zipformer transducer models, which emit partials as audio arrives.
    This is the model family the WebSocket path uses.
  The default batch model is Parakeet v2, because it measured best on both corpora at
  whisper-base speed. The measurements, the languages each model covers, and the
  licenses are in MODELS.md.
- F4.2 Batch endpoint accepts `audio/wav`, `audio/mpeg`, `audio/ogg`, and raw PCM.
  Files are transcoded to 16 kHz mono in process.
- F4.3 Response includes the full text, segment list with start and end times, and
  detected language.
- F4.4 Streaming endpoint accepts PCM frames over WebSocket. Partials arrive at a
  configurable interval (default 300 ms) and finals land on VAD endpoints.
- F4.5 Partials are provisional. Every partial carries `"provisional": true` and
  replaces the previous one. Clients must not append partials, they must replace.
  A stability window (default 200 ms) delays emission so text does not rewrite
  itself on every frame.
- F4.6 If the configured model cannot stream, the streaming endpoint falls back to
  utterance segmentation: one final per VAD segment, no partials. The response
  says which mode ran.
- F4.7 Optional word timestamps. Off by default because they cost CPU. Zipformer
  models give them cheaply, Whisper does not.
- F4.8 Language can be forced or left as `auto`. Streaming transducer models are
  language specific; the registry records which languages each model covers.
- F4.9 Translation to English is available for Whisper models and off by default.
- F4.10 On Windows the input can be two sources at once, mic plus what the machine
  is playing. See F13. Every other platform captures the microphone only in v1.
- F4.11 Finals pass through the user dictionary in reverse, so a term the user taught
  the system comes back spelled their way. See F14.7.

### F5. Voice activity detection

- F5.1 Backends: `energy` (RMS threshold, no model) is the only one built into this
  binary and is the default. `silero` is the intended default for v1, reusing the
  sherpa-onnx runtime rather than loading a second ONNX session; until that path exists
  `vad.engine: "silero"` is refused at validation, because a config that loads and does
  nothing reads as working.
- F5.2 Configurable: `threshold`, `min_speech_ms`, `min_silence_ms`, `padding_ms`,
  `max_segment_ms`, and `frame_ms`.
- F5.3 VAD gates the streaming STT pipeline. Audio before speech confirmation is
  held in a ring buffer and prepended, so the first phoneme is not clipped.
- F5.4 VAD is exposed as its own event stream for callers that want to drive their
  own segmentation.
- F5.5 `energy` VAD exists so the library works with zero models installed.

### F6. Audio output

- F6.1 API returns audio. That is the default and always available.
- F6.2 Local playback through `cpal`, with device enumeration
  (`GET /v1/audio/devices`), default device selection, and explicit device choice
  by ID.
- F6.3 Playback resamples to the device's native rate and channel count.
- F6.4 Output mode per request or in config: `api`, `device`, or `both`.
- F6.5 Playback queue depth is bounded. When full, the request either blocks or
  fails with `429`, per config.
- F6.6 Mobile playback uses the platform's audio session: AVAudioSession on iOS,
  AAudio or OpenSL on Android, with the correct category so it mixes or ducks as
  the app asks.

### F7. Configuration

- F7.1 Sources, in increasing precedence: compiled defaults, JSON file
  (`--config` or the platform default path), environment variables with the
  `ARIOSO_` prefix, runtime API changes.
- F7.2 Runtime changes apply immediately where possible. Engine and model changes
  trigger an unload and reload, and the endpoint returns when the new state is
  `ready` or `failed`.
- F7.3 `PATCH /v1/config` takes a partial document plus `persist: bool`. With
  `persist: true`, the merged document is written back to the config file.
- F7.4 Every field is validated on load and on update. Errors name the JSON path.
- F7.5 `GET /v1/config` returns the effective config with secrets redacted.
- F7.6 The full config is a published JSON Schema, versioned with the release.
- F7.7 Config carries `schema_version`. On load, older documents are migrated in
  memory and reported as migrated in the logs. Writing back persists the current
  version.
- F7.8 PATCH semantics are explicit: scalars replace, objects merge, arrays
  replace. There is no array merging, because merging voice lists or model lists
  produces states nobody asked for.
- F7.9 The bearer token is read from the environment only. `GET /v1/config` never
  echoes it, and a config file containing one is rejected with a clear error.

```json
{
  "schema_version": 1,
  "server": {
    "bind": "127.0.0.1:10101",
    "auth": { "mode": "none", "token_env": "ARIOSO_TOKEN" },
    "openapi": true,
    "cors_origins": [],
    "limits": { "max_text_chars": 4000, "idle_timeout_ms": 30000 }
  },
  "tts": {
    "enabled": true,
    "engine": "piper",
    "voice": "en_US-joe-medium",
    "speed": 1.0,
    "sid": 0,
    "format": "wav"
  },
  "stt": {
    "enabled": true,
    "engine": "sherpa",
    "batch_model": "sherpa-parakeet-tdt-0.6b-v2-en",
    "stream_model": "sherpa-zipformer-en",
    "language": "auto",
    "partials": true,
    "partial_interval_ms": 300,
    "stability_window_ms": 200,
    "word_timestamps": false,
    "max_audio_s": 600,
    "capture": {
      "sources": ["mic"],
      "mode": "separate",
      "loopback_device": null,
      "process_name": null,
      "mic_gain_db": 0.0,
      "loopback_gain_db": 0.0,
      "echo_guard": "gate"
    }
  },
  "vad": {
    "engine": "energy",
    "threshold": 0.01,
    "min_speech_ms": 250,
    "min_silence_ms": 700,
    "padding_ms": 200,
    "max_segment_ms": 30000,
    "frame_ms": 32
  },
  "audio": {
    "output": "api",
    "device": null,
    "queue_ms": 2000
  },
  "models": {
    "dir": null,
    "auto_download": ["piper-en_US-lessac-medium", "sherpa-whisper-base"],
    "mirror": null,
    "max_concurrent_downloads": 2
  },
  "log": {
    "level": "info",
    "format": "pretty",
    "file": null,
    "log_text": false
  }
}
```

### F8. API

HTTP, JSON, versioned under `/v1`.

| Method | Path | Purpose |
|---|---|---|
| GET | `/healthz` | Process alive |
| GET | `/readyz` | All enabled engines ready |
| GET | `/v1/info` | Version, build target, available engines |
| GET | `/v1/config` | Effective config, secrets redacted |
| PATCH | `/v1/config` | Partial update, optional persist |
| GET | `/v1/models` | Catalog with state and progress |
| POST | `/v1/models/{id}/download` | Start or resume download |
| DELETE | `/v1/models/{id}` | Remove from disk |
| GET | `/v1/models/events` | SSE stream of model state and download progress |
| POST | `/v1/models/{id}/warm` | Load a model without waiting for a request |
| POST | `/v1/stop` | Stop gracefully: drain in-flight work, then exit |
| POST | `/v1/abort` | Stop now: flush, drop in-flight work, exit |
| GET | `/v1/engines` | Capabilities per engine: streaming, languages, voice count, loaded |
| GET | `/v1/voices` | Voices for the active engine, filterable by language |
| POST | `/v1/tts` | Synthesize, returns audio bytes |
| POST | `/v1/tts/stream` | Chunked audio response |
| WS | `/v1/tts/ws` | Text fragments in, audio frames out |
| POST | `/v1/stt` | Transcribe an uploaded file |
| WS | `/v1/stt/ws` | PCM in, partial and final events out |
| GET | `/v1/audio/devices` | Enumerate playback devices |
| POST | `/v1/audio/play` | Synthesize and play on a device |
| GET | `/openapi.json` | OpenAPI 3.1 document |
| GET | `/docs` | Swagger UI |
| GET | `/metrics` | Prometheus metrics, off by default |

Error model: RFC 9457 problem details, with a stable `code` field:
`model_not_installed`, `model_integrity_failed`, `engine_unavailable`,
`engine_busy`, `cancelled`, `unsupported_voice`, `unsupported_format`, `bad_audio`,
`queue_full`, `too_large`, `session_limit`, `invalid_value`, `unknown_model`,
`engine_disabled`, `idle_timeout`.

Every route answers with a coded problem document. There is no constructor that writes a
problem without a code, because one route that used it gave a client `"code": null` for the
four cases it most wanted to switch on: empty text, text over the limit, a speed out of
range, and a voice that is not installed.

**No speech is a result, not an error.** `POST /v1/stt` answers `200` with `"text": ""` and
the duration when the audio holds no words: a silent file, a muted microphone, a room with
nobody in it. It answered `503 engine_unavailable` before, because the recognizer returns no
result for silence and the wrapper read that as a failure, which sends an operator to inspect
a model that is working. `422 bad_audio` stays for a body that is empty or cannot be decoded,
which is a different thing: nothing was sent, or what was sent was not audio. The streaming
route makes the same distinction, and never sends a `final` with empty text.

Audio contract, stated once and enforced everywhere:

| Field | Value |
|---|---|
| Encoding | `s16le` little endian, or `f32le` on request |
| Channels | mono |
| TTS rates | Piper 16000 or 22050 depending on voice tier, Kokoro 24000 |
| STT input rate | resampled to 16000 before the engine |
| Device playback | resampled to the device's native rate and channel count |

A client that sends a mismatched rate gets `415 unsupported_format` with the
expected rate in the problem detail, not silent pitch shifting.

Streaming mechanisms, and which one is canonical: the two WebSockets are the
contract. `/v1/models/events` is a convenience SSE feed for progress bars, and
`/v1/tts/stream` is a convenience HTTP response for one-shot audio. Anything that
needs cancellation, partial results, or bidirectional control uses the WebSocket.

Cancellation: both WebSockets accept `{"type":"cancel"}`. The server stops at the
next chunk boundary and replies `{"type":"cancelled"}`. Dropping the socket has
the same effect, and the engine releases its permit either way.

Engine switches take effect at once. Every route is registered whatever the config says, and
the handler for a disabled engine answers `404 engine_disabled`: registering at startup made
`PATCH /v1/config` one-way, so an engine switched off at start could never be turned on and
one switched on kept serving after it was switched off. The API document is built per request
for the same reason, so a path leaves it at the moment it starts refusing.

WebSocket STT protocol:

```text
client → {"type":"start","language":"auto","sample_rate":16000}
client → {"type":"start","language":"auto","sample_rate":16000,
          "sources":["mic","loopback"],"mode":"separate"}   # Windows dual capture
server → {"type":"ready","session":"01J...","sources":["mic","loopback"]}
client → <binary PCM frames, 16 kHz mono s16le>
server → {"type":"partial","text":"hello wor","t":1.20,"provisional":true}
server → {"type":"final","text":"hello world","start":0.0,"end":1.85}
client → {"type":"stop"}
server → {"type":"done","duration":1.85}
```

The flush on `stop` is a transcription like any other: if it fails, the session ends on
`{"type":"error","code":…,"detail":…}` and no `done` follows, because `done` says the work
finished. Every number in these frames is rounded to a hundredth, `duration` included.

A first frame whose `type` is present and is not `start` is refused with an error frame. It
used to be accepted, because every field had a default and unknown fields are ignored, so a
client speaking a different protocol got `ready` and then waited for audio it was never going
to send. Fields this server does not know stay ignored on purpose, so the documented
`sources` field above can arrive from a newer client without breaking an older server.

WebSocket TTS protocol:

```text
client → {"type":"synthesize","text":"Hello.","voice":"af_heart","format":"pcm"}
server → {"type":"start","sample_rate":24000,"channels":1,"encoding":"s16le"}
server → <binary audio frames>
server → {"type":"timing","word":"Hello","start":0.0,"end":0.42}
server → {"type":"end","duration":0.55}
```

Binary frames carry audio. JSON frames carry control and metadata. A client that
cannot read binary frames can request `encoding: "base64"` and get audio inside
JSON, at roughly 33 percent size cost.

### F9. Logging

- F9.1 `tracing` throughout, with one span per request (`http.request`,
  `tts.synthesize`, `stt.transcribe`, `model.download`) and fields for engine,
  model, voice, duration, and bytes.
- F9.2 `RUST_LOG` and `log.level` both work. `log.format` is `pretty` or `json`.
- F9.3 Optional file output with daily rotation. `log.file` stays the active file and
  keeps its name, so an operator who tails it is not left watching a file that stopped
  growing. Yesterday's records move to `<file>.<YYYY-MM-DD>`, and
  `log.retention_days` (default 7) removes rotated files older than that window. The day
  boundary is UTC. A rotation that cannot rename keeps writing to the open file and says
  so on stderr: losing records because the filesystem refused a rename is worse than a
  file that rotates a day late.
- F9.4 `log.log_text` defaults to false. Transcripts and synthesized text are not
  logged. Turn it on deliberately when debugging.
- F9.5 Download and load events are logged at info with byte counts, so a stuck
  first run is diagnosable from logs alone.
- F9.6 Every request gets a request ID, returned in the `x-request-id` header and
  carried on every span. A client-reported bug is one grep away from its traces.
- F9.7 The default level is `error`. A server that is working says nothing; a server
  that is not says why. Anything a healthy run would print on every request is not
  information, it is noise that trains the operator to ignore the log.
- F9.8 The level policy, applied across every crate:

  | Level | Belongs here | Examples |
  | --- | --- | --- |
  | `error` | The process cannot do its job for this request, and the operator must know | a model file fails its hash, a config reload writes an unusable file, a port cannot be bound |
  | `warn` | The request was served, but something is wrong that will bite later | the dictionary did not parse and the last good copy was kept, a download resumed from a truncated file, a request was refused by a limit, a certificate expires within 14 days |
  | `info` | A state change an operator would want in a timeline, and nothing per-request | listening on an address, model downloaded with byte count, engine loaded, config changed by a patch, dictionary reloaded with its entry count |
  | `debug` | One request or one internal step, useful when reproducing a report | request finished with status, engine, duration and bytes; a sentence split; a phoneme guess; a VAD segment boundary; a rate limit bucket refilled |
  | `trace` | Per-frame or per-chunk detail that only matters inside one algorithm | every audio frame, every queue push, every partial re-run |

  The test for `info`: would an operator reading the log tomorrow care that this happened
  at 03:14? If the answer is "only if it broke", it is `debug`. The test for `warn`: is
  there an action the operator could take? If not, it is `info` or `debug`.
- F9.9 Text is never logged at any level unless `log.log_text` is on, including at
  `trace`. A recording is the most private thing this server touches.

### F10. C ABI

- F10.1 Header generated with `cbindgen`. Opaque handles, no Rust types in the
  interface.
- F10.2 Functions: init and shutdown, config load and patch, model download with a
  progress callback, TTS create and synthesize with an audio callback, STT create
  and push audio with an event callback, last error string.
- F10.3 Every callback can return a cancel code. The engine stops within one chunk.
- F10.4 Thread safe. Handles are `Send` and `Sync` behind the scenes; the header
  documents which calls may block.
- F10.5 Built and smoke tested for iOS (arm64, simulator) and Android
  (arm64-v8a, x86_64) in CI.
- F10.6 The header exports `arioso_abi_version()` and every options struct starts
  with a `struct_size` field, so a newer library accepts an older caller and the
  reverse fails with a clear error instead of reading past the end.

### F11. Concurrency, backpressure, and limits

- F11.1 Each engine holds a semaphore. Default permits: TTS 2, STT 1 per streaming
  session model. Configurable per engine.
- F11.2 Requests beyond the permits wait in a bounded queue, default depth 32.
  A full queue returns `429 engine_busy` with `Retry-After`.
- F11.3 WebSocket sessions are capped (default 8) and idle sessions close after
  `server.limits.idle_timeout_ms` without audio or control messages, default 30000. The
  clock resets on every audio frame and every control frame, and applies to the whole
  session, not only to the first message. Exceeding the cap returns `session_limit`
  before the upgrade completes.
- F11.4 Cancellation is honored within one chunk. A cancelled request frees its
  permit before the response is written.
- F11.5 The server reports current queue depth and permit saturation on
  `/metrics`, so capacity problems are visible before users feel them.

### F12. Mobile audio session

These are requirements, not implementation notes, because they change the API.

- F12.1 iOS: the app declares its audio session category (playback, play and
  record, or ambient) through config. The library applies it on start and restores
  the previous category on stop.
- F12.2 iOS: background audio and background recording are supported when the app
  declares the corresponding mode. A streaming session that loses foreground stays
  alive if background audio is enabled, and closes cleanly if it is not.
- F12.3 iOS and Android: microphone permission state is queryable and every audio
  entry point fails with `permission_denied` rather than silence.
- F12.4 Android: streaming capture runs in a foreground service when the app asks
  for it, with the notification the platform requires.
- F12.5 Route changes are events. Headphones unplugged, Bluetooth connected, or a
  call interrupting playback are surfaced to the caller, and playback pauses
  instead of switching to the speaker at full volume.

### F13. Windows dual-source capture

The case this serves: a call where one side is in the room and the other side is in
the speakers. Transcribing only the mic loses half the conversation.

- F13.1 Windows only in v1. Other platforms reject a second source with
  `unsupported_platform` rather than silently capturing one.
- F13.2 Sources: `mic`, `loopback`, and `process`. `loopback` captures the default
  render endpoint through WASAPI loopback, which needs no driver and works in
  shared mode. `process` captures one application and its children through
  `ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`,
  which needs Windows 10 build 19041 or newer.
- F13.3 Two modes. `separate` runs one transcription stream per source and tags
  every event with `source`, default. `mixed` sums the sources into one stream
  before the engine, for callers that want a single transcript and accept the
  accuracy cost of two overlapping speakers.
- F13.4 Transcript events gain a `source` field with `mic`, `loopback`, `process`,
  or `mixed`. Segment timestamps stay relative to session start, so two streams can
  be interleaved on one timeline.
- F13.5 The two endpoints have independent clocks. Each source is resampled to
  16 kHz with its own drift estimate, and the mixer compensates by inserting or
  dropping whole frames. Buffers never grow without bound, and a 30 minute call
  does not accumulate seconds of skew.
- F13.6 Echo. A microphone in a room with speakers hears the remote side twice. The
  supported configuration is headphones, and the docs say so. `echo_guard` offers
  `off`, `gate` (suppress mic frames while loopback energy is above threshold),
  and `aec` (WebRTC echo cancellation, behind a Cargo feature because it adds a
  C++ dependency).
- F13.7 Per-source gain: `mic_gain_db` and `loopback_gain_db`, both 0 by default.
- F13.8 Device choice: `loopback_device` takes a device ID, default is the default
  render device. `GET /v1/audio/devices` lists render endpoints with
  `kind: "render"`, so a caller can pick the headset instead of the speakers.
- F13.9 Failure modes are explicit. No render device is `no_device`. A device held
  in exclusive mode is `loopback_unavailable`. DRM-protected audio is captured as
  silence by Windows itself, and the session reports a `silent_source` warning
  after 10 seconds rather than pretending it is working.
- F13.10 WebSocket `start` accepts
  `{"sources":["mic","loopback"],"mode":"separate"}`. The reply names the sources
  that actually opened, because a caller needs to know when one failed.
- F13.11 CPU cost is two engine sessions, not one. The semaphore in F11 accounts
  for it, and `separate` mode takes two permits.

### F14. Text normalization and the user dictionary

What a caller writes is rarely what should be spoken. Digits, currency, dates, and
proper nouns need a deterministic transform before the phonemizer sees them, and the
user needs a way to correct the result without waiting for a release.

Order of operations, both directions:

- Text to speech: sentence split, then user dictionary, then normalization, then
  CMUdict, then a regular inflection of a CMUdict entry, then letter-to-sound, then
  phonemes.
- The inflection step exists because the dictionary is a word list and not a grammar: it
  holds `avenger` and not `avengers`, `walk` and not `walked`. Without the step a plural
  falls to the letter rules, which read `avengers` as `AV-en-ger-s`. A regular suffix
  (`-s`, `-es`, `-ies`, `-ed`, `-ied`, `-ves`, `-ing`, `-er`, `-est`, `-ly`) is applied to
  the entry instead, with the voicing the base's last phoneme decides: `-s` is /s/ after a
  voiceless sound, /z/ after a voiced one, and /ɪz/ after a sibilant. A derived form is not
  a guess and is not reported as one.
- Speech to text: transcript, then user dictionary in reverse, then the corrected text.
  The reverse comparison uses the same inflection step, so a transcript of `avengers`
  compares against the sounds of `avenger` plus a /z/.

- F14.1 Normalization covers cardinals, currency, ordinals, times, dates, years,
  percentages, units, and a short list of abbreviations. `$3.99` becomes
  `three dollars ninety nine cents`; `2026` becomes `twenty twenty six`; `3rd`
  becomes `third`. A comma groups thousands only when three digits follow it and the
  group before it is one to three digits: `1,234,567` is a number, `2026,100` is a year
  followed by a clause, and `1.5,000` is a decimal followed by one.
- F14.1a A unit written after a number is spoken: `5kg` is `five kilograms`, `190cm` is
  `one hundred ninety centimeters`, `1.5kg` is `one point five kilograms`, `20°C` is
  `twenty degrees Celsius`, `60mph` is `sixty miles per hour`. The unit is plural unless
  the number is exactly one. One space before the unit is accepted when the token cannot
  be a word (`190 cm`, `5 kg`, `5 min`), and refused when it can (`5 in the morning` is
  not five inches, `2 m of rope` is not two metres), because the two are the same shape.
- F14.1b A magnitude suffix after a currency amount is spoken: `$1m` is `one million
  dollars`, `$5k` is `five thousand dollars`, `$2.5b` is `two point five billion dollars`.
  The suffix is only read after `$`, so `4K` stays a display resolution.
- F14.2 Normalization is language-specific. English ships in v1; every other language
  passes through unchanged until its rules exist.
- F14.3 Every expansion is inspectable without listening: `arioso normalize "<text>"`
  prints the expanded text, and the server exposes the same result.
- F14.4 The user dictionary is a `dictionary.json` file beside `config.json`, with the
  path overridable in config and by environment variable.
- F14.5 One written `term` maps to a list of `spoken` forms, tried in order. The first
  form the active voice can render wins. If none can, the term falls back to
  normalization and CMUdict, and `arioso doctor` reports the entry as unusable for
  that voice.
- F14.6 An entry may carry `phonemes` in ARPAbet, which takes precedence over `spoken`.
  ARPAbet is voice-independent and is mapped to the voice's symbol set at render time,
  so one entry serves every voice.
- F14.7 Speech to text uses the same file in reverse: every `spoken` form and every
  `phonemes` entry maps back to `term`, so a transcript containing `ah ree oh so`
  becomes `arioso`. Correction applies to finals, not to provisional partials.
- F14.8 Matching covers single words and multi-word phrases, longest match first,
  case-insensitive by default, with `case_sensitive` per entry. Matching is on word
  boundaries, so `cat` does not match inside `category`.
- F14.9 Precedence is user dictionary, then normalization, then built-in dictionary,
  then letter-to-sound. An entry that shadows a normalization rule is allowed and
  logged at debug level.
- F14.10 The file reloads when it changes, checked at request start, so a correction
  needs no restart. A malformed file keeps the last good copy and reports the parse
  error in `/readyz` as a warning; it never takes the server down.
- F14.11 The file carries a schema version. Unknown fields are rejected, so a typo
  fails loudly instead of being silently ignored.
- F14.12 Validation: duplicate terms rejected, empty `spoken` forms rejected, phrases
  capped in length, and every error names the offending entry.

- F14.13 Times are spoken on a 12-hour clock, and the part of the day follows.
  `tts.time_of_day` is on by default; `ARIOSO_TTS_TIME_OF_DAY` overrides it.
  With the option on:

  | Source hour | Spoken |
  | --- | --- |
  | 00–11 | "seven forty five in the morning" |
  | 12–17 | "twelve thirty in the afternoon" |
  | 18–23 | "seven forty five in the evening" |

  The table applies to the hour as written, so an unmarked `7:45` is morning and
  `19:45` is evening. A written marker wins over the table and is consumed:
  `7:45 pm` speaks as "seven forty five in the evening", `12:30 am` as "twelve
  thirty in the morning". With the option off, the hour is 12-hour with no
  marker, and `19:45` and `07:45` both read as "seven forty five".

  A dictionary entry still cannot override a time, because a phrase may not match
  across the colon. That limit is unchanged by this option.

Assumptions taken without asking, all reversible:

- A pronunciation is written as plain respelling, with optional ARPAbet for precision.
  IPA is not accepted in v1.
- The dictionary is a separate file, not a section of `config.json`.
- The speech to text side is defined and implemented as a pure function now, and wired
  to the engine when M2 lands.
- Matching covers phrases as well as words, longest first, case-insensitive by default.

### F15. Letter to sound for words outside the dictionary

The CMU dictionary holds 126,052 words. Names, brands, and jargon are not in it. Today
those words are skipped and named, which leaves a hole in the sentence.

- F15.1 Order. A word is looked up in the user dictionary, then normalized, then looked up
  in CMUdict, and only then pronounced by rule. The rules never override a known word.
- F15.2 Rules. Grapheme patterns of up to four letters, longest match first, applied left
  to right. A rule may require the start or the end of the word, or a following letter
  class.
- F15.3 Consonants with a following letter that changes them: `c` before `e`, `i`, `y` is
  `s`; `g` before `e`, `i`, `y` is `j`; `s` between vowels or after a voiced sound is `z`.
  Digraphs `ch`, `sh`, `th`, `ph`, `wh`, `ck`, `ng`, `qu` are handled as units.
- F15.4 Vowels. Short by default. A final silent `e` is dropped, and it lengthens the
  vowel before it across one consonant (`name`, `time`, `hope`). Digraphs are handled as
  units (`ee`, `ea`, `oo`, `ou`, `ow`, `ai`, `ay`, `au`, `aw`, `oi`, `oy`, `oa`, `ie`).
  `er`, `ir`, `ur` are `er`; `ar` and `or` keep the vowel and add `r`. A final `y` is a
  long `e`.
- F15.5 Stress. One primary stress, on the first vowel of the word. This is crude and
  accepted: a guessed stress is better than a skipped word, and the dictionary is the
  place to correct it.
- F15.6 Output goes through the same ARPAbet to IPA table as a dictionary entry, so the
  check against the voice's symbol inventory is identical for both.
- F15.7 A guessed word is reported, never silent. The render report carries `guessed`, and
  `arioso say` prints the words it pronounced by rule.
- F15.8 `arioso doctor` counts a dictionary entry as sayable when the rules can pronounce
  it, so an entry is not reported as unusable when it will in fact be spoken.
- F15.9 Quality bar. The rules are a fallback, not a phonemizer. They must not change the
  pronunciation of any word the dictionary knows, and a wrong guess must be fixable with
  one dictionary entry.

Acceptance: `arioso say "zorbansky"` produces audio, and reports the word as pronounced by
rule. Before this section the same command wrote a 44-byte WAV and warned.

**Measured limit, found on the first round trip.** The reverse map in F14.9 matches a
spoken form exactly, and real speech to text does not reproduce it exactly. Two runs, both
synthesized by arioso and read back by whisper tiny.en:

| Spoken form | Whisper wrote | Corrected |
| --- | --- | --- |
| `tom seller` | `Tonseller` | no |
| `koober netties` | `Cooper Netis` | no |
| `sell a` | `cell a` | no |

So the reverse map only fires when the transcript matches the written form letter for
letter, which on this evidence is rare. The fix, now built, compares sounds: both sides
are pronounced with CMUdict and the F15 rules, and a phoneme edit distance within 40 % of
the form's length counts as the same words. The closest match wins its span, and the
longer span wins a tie.

Re-measured after the change, same recordings, with whisper base as the default model:

| Spoken form | Whisper wrote | Corrected to |
| --- | --- | --- |
| `tom seller` | `Tonseller` | `Tom Sella` |
| `koober netties` | `Cooper Netis` | `Kubernetes` |
| `sell a` | `cell a` | `Sella` |
| `ah ree oh so` | `a Rio so` | `arioso` |

The full sentence now round-trips: "Tom Sella built arioso for his own machine." comes
back exactly.

Two limits, measured rather than assumed. A form of several words must be pronounced
word by word; the first version smashed them into one string, sent it to the spelling
rules, and never matched. And the model matters more than the matcher: tiny wrote
"Tonseller built REO so", the matcher then matched three words against a four-word form
and swallowed "built", and base does not have the problem. That is why base is the
default despite being 57 MiB larger.

Known risk, accepted: a spoken form made of ordinary English words rewrites those words
wherever they appear. An entry that says `sell a` will turn "we sell a car" into "we Sella
car". That follows from the user writing that form down, and the dictionary is the place
to correct it.

### F16. Transport security and abuse resistance

A local server is not attacked until someone exposes it. These requirements cover the
case where it is exposed on purpose, and they are honest about what a single process can
and cannot do about a flood.

- F16.1 TLS terminates in the server with rustls when `server.tls.cert` and
  `server.tls.key` are both set. With neither set the server speaks plain HTTP exactly as
  it does today, so an existing config keeps working.
- F16.2 Certificates are file paths, never values in the document. The pair is re-read
  when either file's modification time changes, because certbot renews on a timer and a
  renewal that needs a restart is a renewal that eventually takes the service down.
- F16.3 ALPN offers `h2` and `http/1.1`.
- F16.4 Two documented paths to a certificate: certbot for a public hostname, and a
  locally generated pair for a LAN or a development machine. Both examples are run in CI
  or in `EXAMPLES.md` against a real server before they are published.
- F16.5 The default bind stays loopback. Exposing the port is an explicit edit, and the
  README says what that exposes.
- F16.6 Every route has a body limit. The speech route raises it deliberately, and the
  raise follows `stt.max_audio_s`: a body limit fixed at the default duration would refuse
  audio the config says it accepts, which is two limits disagreeing and the settable one
  losing. The rest stay at the framework default.
- F16.6a The longest text one synthesis request accepts is
  `server.limits.max_text_chars`, default 4000 characters, about two and a half minutes of
  speech. It applies to `POST /v1/tts` and to the first message on `/v1/tts/ws`. One
  request renders its whole text before the reply is written, so the limit bounds the work
  and the memory of a single call; a client with more text sends it in pieces or streams
  it, and `IMPLEMENTATION.md` shows both.
- F16.6b The transcriber decodes windows of at most 29 seconds. The engine accepts "less
  than 30 seconds" and silently keeps only the first thirty of anything longer, so the
  window is cut one second short and the pieces are joined. Before this, an hour of audio
  returned the first half minute with a 200 and no error.
- F16.7 A per-client token bucket caps request rate: `server.limits.requests_per_second`
  and `server.limits.burst`. Over the limit answers `429 rate_limited` with `Retry-After`.
- F16.8 A request-header timeout, and `server.limits.max_connections` applied where it can
  be enforced: concurrent WebSocket sessions, which are the connections that accumulate
  because they end when a document ends. A refused session gets `503 too_many_sessions`
  rather than queueing behind sessions whose length it cannot see. A cap on plain sockets
  would need the accept loop, which the server does not own; the PRD says so rather than
  claiming a control that does not exist.
- F16.9 Engine permits and the bounded queue from F11 stay the last line: a full queue
  answers `429 engine_busy`, which is cheaper than queueing work nobody will run.
- F16.10 The client identity used by the limits is the peer address. `X-Forwarded-For` is
  honoured only when `server.trust_forwarded_for` is on, because a header any client can
  set is not an identity.
- F16.11 Limits are per process and per address, not distributed. A volumetric flood is
  the network's problem and is out of scope: what this server promises is to refuse work
  cheaply, not to absorb a flood. The README says so rather than implying otherwise.

### F17. Authentication and exposure

- F17.1 `server.auth.mode = "bearer"` requires `Authorization: Bearer <token>` on every
  route. The token is read from the environment variable named by `server.auth.token_env`,
  never from the document.
- F17.2 A bearer mode with no token in the environment is a startup failure. A server that
  believes it is protected and is not is worse than one that refuses to start.
- F17.3 `/healthz` answers without a token. A liveness probe must work before the operator
  has read the config, and it reveals only that the process is alive.
- F17.4 The comparison is constant time, so a caller cannot learn the token one byte at a
  time from response timing.
- F17.5 A non-loopback `server.bind` without a token is refused at validation, and the
  error names the exact fix. Exposure is a decision, not a typo.
- F17.6 A refusal carries `WWW-Authenticate: Bearer realm="arioso"` and the problem code
  `unauthorized`.

### F18. Response hygiene and surface control

Added after a deliberate pass for remote code execution. Each item below came from a
finding, not from a checklist.

- F18.1 Every response carries `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer` and `X-Frame-Options: DENY`. A browser that sniffs a JSON
  body as HTML turns a reflected string into a script.
- F18.2 A runtime failure's own message is logged at `error` and never returned. The client
  gets a stable sentence and a code. The runtime's words can name a path or a library
  symbol, which is a map of the machine handed to whoever asked.
- F18.3 The same rule for a failed config write: the path goes to the log, not the response.
- F18.4 `server.docs` turns `/docs` and `/openapi.json` off. Unregistered, so they answer
  404 rather than 401: a refusal confirms a route exists, a 404 does not.
- F18.5 A model file name in the manifest is a plain name: no separator, no `..`, not
  absolute. Refused at parse time, so a corrupted manifest cannot become an arbitrary file
  write.

### F19. Sidecar mode

For a caller that wants a server, not a shell. A desktop or mobile application spawns
arioso, reads one line, and talks HTTP.

- F19.1 `server.sidecar: true` makes `arioso serve` detach: the parent spawns a child copy of
  itself, the child binds and serves, the parent exits 0 once the child is ready. No `fork`,
  no `setsid`, so the same code works on macOS, Linux and Windows.
- F19.2 The child writes exactly one line to stdout, `START {port}`, and flushes it.
  Everything else — logs, the listening message, errors — goes to stderr, so a caller can
  read the port from a pipe without parsing anything.
- F19.2a `server.bind` accepts an IP address or `localhost`. `localhost` is the IPv4
  loopback: it is resolved to `127.0.0.1` before the loopback rules run, so the sidecar rule
  and the token rule answer for the socket the process will take. A host name other than
  `localhost` is refused, because those rules cannot be applied to a name without a
  resolver, and a guard that skips what it cannot read is not a guard.
- F19.3 The child binds `server.bind`. When that port is taken it binds port 0 and reports
  the port the operating system gave it, so two instances on one machine never collide.
- F19.4 The child writes `arioso-{port}.pid` beside the config file. The port is in the
  name because one config can run two instances, and a single file would let the second
  overwrite the first. The child holds an exclusive lock on that file for its lifetime, so
  a file that can still be locked has no owner: `stop` removes it instead of signalling a
  number the system may have given to another program. `arioso stop` walks every
  `arioso-*.pid` beside that config, stops the live ones, and removes the rest. A process
  that stops on request removes its own file.
- F19.5 Sidecar mode requires a loopback bind and carries no token. The caller is on the
  same machine, and a secret passed through a pipe on a shared machine protects nothing. A
  non-loopback bind with sidecar mode is refused at validation.
- F19.6 The parent waits for readiness before exiting, so a caller never connects to a port
  that is not listening yet, and a child that fails to start makes the parent exit non-zero
  instead of leaving the caller waiting.

### F20. Lifecycle over the API

A caller that can start a server should be able to end it without a process table.

- F20.1 `POST /v1/stop` stops accepting, lets in-flight requests finish, and ends the
  process when they do or when `server.limits.shutdown_grace_ms` expires (default 5000).
  Answers `202` before it begins. A streaming session is ended, because it has no natural
  end. When the process is not serving, it answers `409 no_accept_loop`.
- F20.2 `POST /v1/abort` answers `202`, flushes, and ends the process. Work in flight is
  dropped on purpose; a caller that wanted it kept would have asked to stop.
- F20.3 Both are ordinary routes: bearer auth applies when it is configured, and neither
  is exempt. `/healthz` stays the only unauthenticated route.
- F20.4 SIGTERM and Ctrl-C take the graceful path. A service manager that stops the
  process and a caller that posts `/v1/stop` get the same behaviour.

## 9. Non-functional requirements

| Area | Target |
|---|---|
| TTS first audio, Piper medium, desktop | under 300 ms after warm load |
| TTS throughput, Piper medium, 4-core mini PC | measured 0.52 s for 6.8 s of audio, RTF 0.08. Keep at or below 0.15 |
| TTS throughput, Kokoro int8, same box | measured RTF 1.04. Document it as realtime on weak CPUs and prefer Piper there |
| STT throughput, batch Whisper base, desktop | RTF at or below 0.5. To be measured in M2 |
| STT streaming, Zipformer transducer, desktop | first partial under 400 ms after speech starts, final under 700 ms after the endpoint. To be measured in M2 |
| Windows dual capture, separate mode | both streams live within 200 ms of session start, drift under 20 ms over 30 minutes, CPU under 2 cores at 4 threads |
| Cold start, models cached | under 2 s to ready |
| Cold start, empty cache | server up and serving `/v1/models` before any download starts |
| Idle memory, Piper only | under 300 MB |
| Idle memory, Piper plus whisper base | under 800 MB |
| Binary size, desktop release | under 25 MB without models |
| Concurrency | 8 parallel TTS requests, 4 parallel STT sessions, no data races |
| Offline | zero network calls once models are installed and `auto_download` is satisfied |
| Platforms | macOS arm64 and x86_64, Linux x86_64 and aarch64, Windows x86_64, iOS arm64 plus simulator, Android arm64-v8a and x86_64 |

## 10. Testing and quality gates

- Golden audio. For each engine, model, and voice, a fixed sentence is synthesized
  and compared on duration, RMS envelope, and a spectral distance against a stored
  reference. Tolerance based, because CPUs and SIMD paths differ; never bit exact.
- G2P regression. A 200 word list with expected IPA output, including names,
  heteronyms, acronyms, and numbers. Any change to the phonemizer shows up as a
  diff.
- Transcript regression. Ten audio clips per streaming model, compared on WER with
  a ceiling per clip, not an average that hides one bad file.
- Property tests on the resampler and the sentence chunker: no sample loss, no
  duplicated audio at boundaries.
- Fuzz the config loader and the model manifest parser.
- Load test: 8 concurrent TTS requests and 4 STT sessions for 10 minutes, asserting
  no queue overflow beyond the configured depth and no memory growth above 5 percent.
- Cancellation test: cancel mid-stream on both WebSockets and assert the permit is
  released within 100 ms.
- Dual-capture soak on Windows: a 30 minute synthetic call, one voice on the mic and
  one on a virtual render device, asserting both transcripts arrive and drift stays
  under 20 ms.
- CI matrix: macOS arm64 and x86_64, Linux x86_64 and aarch64, Windows x86_64,
  plus build-only jobs for iOS and Android.

## 11. Licensing and compliance

- Core crates: dual MIT or Apache-2.0.
- `gpl-espeak` feature: off by default. Turning it on links espeak-ng and makes the
  resulting binary GPL-3.0. The README states this plainly, and CI fails a job that
  enables the feature in a release build.
- Model licenses are per model. The manifest carries a `license` field, and
  `GET /v1/models` exposes it. Non-commercial voices (CC BY-NC-SA, Blizzard) are
  marked `commercial_use: false` and the default catalog ships only permissive ones:
  LibriTTS (CC BY 4.0), LJSpeech (public domain), Joe, Mike, Kathleen, Reza
  Ibrahim (CC0), Sam (Apache-2.0), Kokoro voices (Apache-2.0).
- No telemetry. No analytics endpoint. No phone-home.
- Model downloads go only to the configured host or mirror.
- `cargo-deny` in CI blocks a GPL dependency from entering the default feature set,
  and a second job builds with `--features gpl-espeak` to prove the opt-in path
  still compiles.
- `cargo about` generates a NOTICE file from the dependency tree and the model
  catalog, so attribution ships with the binary instead of living in a wiki.
- The model catalog records `commercial_use`, `attribution_required`, and the
  license URL per model. `GET /v1/models` returns them, so a caller can refuse a
  non-commercial voice before downloading 60 MB of it.

## 12. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| Kokoro int8 ONNX has a dtype mismatch in `kokoro-onnx` 0.4.7 (speed fed as int32, model wants float) | Engine fails on first run | Pin the model revision, cast inputs in our own session wrapper, add a golden-audio test |
| sherpa-onnx models are directories of ONNX files plus tokens and config, so a wrong pair loads and produces garbage | Silent bad output | One manifest entry per model directory, pinned revision and hashes, and a CI job that loads every catalog model and transcribes a fixture |
| Zipformer transducers are language specific and trail Whisper large on hard audio | Worse transcripts than a user expects | Ship Whisper for batch, transducers for live, and state the accuracy difference in `/v1/engines` |
| Dual capture drifts: mic and render endpoint run on separate clocks | Transcripts slide out of sync on long calls | Independent resampling per source with frame-level drift compensation, plus a 30 minute soak test in CI |
| Microphone hears the speakers, so the remote side is transcribed twice | Duplicate or garbled text | Headphones documented as supported, `echo_guard` gate by default, AEC behind a feature flag |
| Windows captures DRM-protected audio as silence | Feature looks broken on some calls | `silent_source` warning after 10 seconds, and a doc note naming the cause |
| ONNX Runtime iOS build needs a Mac and adds 10 to 20 MB | Mobile binary bloat, CI complexity | `--minimal_build` with an ops config, ORT-format models, one macOS CI runner |
| Permissive G2P is weaker on names than espeak | Mispronunciations | CMUdict plus rules plus a curated exception list; measure on a fixed 200-word test set; document the `gpl-espeak` escape hatch |
| Mobile CPU cannot hit the desktop RTF targets | Laggy speech on phones | Streaming synthesis, sentence-level pipelining, ship `low` tier Piper voices for mobile |
| Disk usage, 1 to 2 GB for a full model set | Bad first-run experience | Per-engine opt-in downloads, size shown before download, `DELETE /v1/models/{id}` |

## 13. Milestones

| Milestone | Scope | Exit criteria |
|---|---|---|
| M0 Skeleton, 2 weeks | Workspace, config crate with JSON Schema, tracing, model manifest and downloader with progress and signature check, CI on three desktop OSes | Download a model, verify hash, resume after a killed process, reject a bad signature, progress visible in logs |
| M1 Piper TTS, 3 weeks | Piper through `ort`, permissive G2P, sentence chunking, `/v1/tts` and `/v1/tts/ws`, WAV and PCM, local playback, concurrency limits | Ten sentences match the golden reference, first audio under 300 ms, queue returns 429 when saturated |
| M2 STT, 3 weeks | sherpa-onnx engine, batch `/v1/stt`, streaming `/v1/stt/ws` on a Zipformer transducer, Silero VAD, provisional partials, cancel | Live microphone test produces partials during speech and one final per utterance, cancel frees the permit |
| M3 Config and engines, 2 weeks | `/v1/config` PATCH with persist, `/v1/engines`, engine selection, `arioso doctor`, OpenAPI complete with Swagger UI | Config change switches engine without restart and survives restart |
| M4 Kokoro, 2 weeks | Kokoro engine behind the same trait, dtype workaround, voice listing | Same sentence through both engines, golden audio for each |
| M5 FFI and mobile, 3 weeks | C ABI with versioned structs, cbindgen header, iOS and Android builds, audio session requirements | Sample app on a phone synthesizes one sentence and streams one utterance |
| M6 Release, 2 weeks | Docs, sample app, packaging (binaries, Docker, Homebrew), performance pass, v1.0 tag | Every acceptance criterion passes on all five platforms |

## 14. Acceptance criteria

1. Fresh install, no models, `arioso serve --config sample.json` starts in under
   two seconds and serves `/v1/models` with every model `not_installed`.
2. `POST /v1/models/piper-en_US-lessac-medium/download` streams progress events,
   and a killed download resumes without refetching completed bytes.
3. After download, `GET /readyz` returns 200 and `POST /v1/tts` returns playable
   audio.
4. `POST /v1/tts/stream` delivers the first chunk before synthesis of the last
   sentence finishes.
5. A WebSocket STT session on a Zipformer transducer model returns provisional
   partials during speech and one final per utterance, and every partial carries
   `provisional: true`.
6. `PATCH /v1/config` with `{"tts":{"engine":"kokoro"},"persist":true}` switches
   the engine, returns only after it is ready, and the change survives a restart.
7. `tts.enabled: false` returns 404 on TTS routes and leaves STT working.
8. With `audio.output: "device"`, audio plays on the selected device, and
   `/v1/audio/devices` lists it.
9. Disconnecting the network after models are cached changes nothing.
10. `cargo build --target aarch64-apple-ios` and the Android NDK equivalent
    produce a static library that links into a sample app and synthesizes one
    sentence.
11. `{"type":"cancel"}` mid-stream stops synthesis within one chunk and the engine
    permit is free within 100 ms.
12. A manifest whose signature does not verify is rejected before any bytes are
    downloaded.
13. `arioso doctor` on a machine with no models reports what is missing, what is
    installed, and what to run next.
14. On Windows, a session started with `{"sources":["mic","loopback"]}` returns
    two interleaved transcript streams, each tagged with its source, and a 30
    minute run stays within 20 ms of drift.
15. On macOS and Linux, requesting a loopback source fails with
    `unsupported_platform` instead of capturing only the microphone.

## 15. Open questions

1. Which transducer models enter the default catalog? Each language is a separate
   model directory, so this is a catalog decision, not a code decision.
2. If the transducer is monotonic, does the stability window still earn its
   latency, or is provisional labeling alone enough?
3. Is an embedded web UI for model management worth v1.1, or does the API cover it?
4. Do we publish prebuilt binaries and Docker images on day one, or after v1.0
   stabilizes?
5. Does v1 accept an SSML subset for pauses and emphasis, or plain text with speed
   as the only prosody control?
