# arioso

[![ci](https://github.com/tsella/arioso/actions/workflows/ci.yml/badge.svg)](https://github.com/tsella/arioso/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![platforms](https://img.shields.io/badge/platforms-linux%20%7C%20macOS%20%7C%20windows%20%7C%20iOS%20arm64%20%7C%20Android%20arm64-lightgrey)
![inference](https://img.shields.io/badge/inference-local-brightgreen)

Speech out of text, and text out of speech, on your own machine. No account, no API key,
no audio leaving the box.

Named for the musical term: a melodious, singing style of delivery.

arioso is a Rust library and a single binary. It speaks with Piper voices and listens with
Parakeet or Whisper through sherpa-onnx, streams both ways over WebSocket, and gives you a
place to correct the pronunciation of your own names in both directions.

```console
$ arioso say "hello world. this is arioso speaking locally."
wrote out.wav (3.16 s at 22050 Hz)
```

A word the built-in dictionary does not hold is spoken by spelling rules and named on a
second line, so a guess is never silent. `arioso` is in the shipped dictionary, which is why
this run has no such line. The length moves by a few hundredths between runs, because the
model samples noise.

## Why this exists

Cloud text to speech is good and fast, and it sends every sentence to someone else's
server. That is fine for a shopping list and wrong for a support call, a medical note, or
anything under a confidentiality agreement. arioso keeps the sentence on the machine that
read it.

It is not a cloud service in a box. It runs a 60 MiB voice on the CPU, at about a tenth of
real time on a four-core N150 with no GPU. You trade voice quality and latency for that.

## Status

Working today:

- Text to speech with Piper voices: WAV over HTTP, PCM frames over WebSocket
- Model download from a manifest, with SHA-256 verification and resumable transfers
- Text normalization: currency, ordinals, years, times, percentages, abbreviations
- A user dictionary for names and jargon, reloaded without a restart
- Spelling rules for words the dictionary does not hold, reported, never silent
- Speech to text: Parakeet or Whisper through sherpa-onnx, batch, from a WAV file
- `arioso doctor`, which says what is wrong before a request fails
- The crates that need no prebuilt native library compile for `aarch64-apple-ios` and
  `aarch64-linux-android` in CI
- A nightly fuzz job over the config parser, the dictionary, the manifest and the WAV reader,
  and a build of every fuzz target on each pull request
- 401 tests

Not built yet:

- The Piper and sherpa-onnx engines on iOS and Android: each links a prebuilt native library
  that needs a build for the target, and no such build is wired here yet
- Silero voice activity detection, which the PRD names as the default over the energy
  detector that ships
- A release signing key: the manifest loader verifies a detached signature when one is
  compiled in, and no key is compiled in yet

## Requirements

- Rust 1.85 or newer, the floor the workspace declares. 1.91 is the only version this
  project builds and tests on
- ONNX Runtime 1.22, unpacked into `vendor/onnxruntime`
- A Piper voice and the CMU dictionary, about 64 MiB together

The Piper engine links against ONNX Runtime. Fetch the prebuilt 1.22.0 runtime once into
the workspace, where `.cargo/config.toml` and the linker both find it:

```bash
mkdir -p vendor
curl -sSL -o /tmp/ort.tgz \
  https://github.com/microsoft/onnxruntime/releases/download/v1.22.0/onnxruntime-linux-x64-1.22.0.tgz
tar xzf /tmp/ort.tgz -C vendor
mv vendor/onnxruntime-linux-x64-1.22.0 vendor/onnxruntime
```

`vendor/` is gitignored. The build sets `ORT_LIB_LOCATION` from it, and two rpaths record
the path relative to the binary, so neither `cargo build` nor `./target/debug/arioso` needs
an exported variable. On macOS and Windows, unpack the runtime and set `ORT_LIB_LOCATION`
to that directory yourself.

`ort` is pinned to 2.0.0-rc.10: 2.0.0-rc.15 requires rustc 1.92, and this toolchain is
1.91. The crate's own binary downloader is off, because it pulls OpenSSL in; pointing
`ORT_LIB_LOCATION` at the runtime avoids that.

```bash
cargo build
cargo test
```

## Quick start

```bash
arioso models list
arioso models download piper-en_US-joe-medium
arioso say "hello world. this is arioso speaking locally." --out hello.wav
arioso serve        # http://127.0.0.1:10101
```

```console
$ arioso models list
id                                 kind        size  license    state
piper-en_US-joe-medium             tts     60.3 MiB  CC0-1.0    installed
```

Then, from another shell:

```bash
curl -s -X POST localhost:10101/v1/tts -H 'content-type: application/json' \
  -d '{"text": "hello world"}' -o out.wav
```

## Commands

| Command | What it does |
| --- | --- |
| `arioso say <text> [--out FILE] [--dict FILE] [--engine ENGINE] [--voice VOICE]` | Write a WAV file |
| `arioso transcribe <file.wav> [--json]` | Read a WAV file back as text |
| `arioso serve` | Run the HTTP and WebSocket server |
| `arioso stop` | Stop a server started in sidecar mode |
| `arioso normalize <text>` | Show what the voice will actually say |
| `arioso doctor` | Check the config, the models, and the dictionary |
| `arioso models list` | Show the manifest and what is installed |
| `arioso models download <id>` | Fetch a model, resuming if it was interrupted |
| `arioso config schema` | Print the JSON Schema for the config file |

`arioso normalize` is the fastest way to find a pronunciation problem, because it prints
the expansion without synthesizing anything:

```console
$ arioso normalize "It costs \$3.99 on the 3rd at 14:30."
It costs three dollars and ninety nine cents on the third at two thirty in the afternoon.
```

## Voices

The catalog holds all 178 Piper voices across 53 languages, each verified by SHA-256 against
the manifest. Filter the list rather than reading it:

```console
$ arioso models list --kind tts --language pt
$ arioso models list --language en --installed
```

Five English voices, as a starting point:

| id | tier | rate | licence | commercial |
| --- | --- | --- | --- | --- |
| `piper-en_US-lessac-medium` | medium | 22.05 kHz | Blizzard-2013 | no |
| `piper-en_US-ljspeech-high` | high | 22.05 kHz | public domain | yes |
| `piper-en_US-joe-medium` | medium, the default | 22.05 kHz | CC0 | yes |
| `piper-en_US-kathleen-low` | low | 16 kHz | CC0 | yes |
| `piper-en_GB-alba-medium` | medium | 22.05 kHz | CC BY 4.0 | yes |

The default voice belongs to the engine, so `tts.voice` may be left out: Piper answers with
`en_US-joe-medium` and Kokoro with `af_heart`. A config that names a voice keeps it, and a
config that names nothing follows whichever engine is loaded.

Twenty-six voices are multi-speaker and take `--sid` or `"sid"` on the request; the names are in
the catalog, so a picker does not have to ask for an integer. `piper-pt_PT-tugão-medium` is the
one id with a letter outside ASCII, and it is tested end to end: download, hash, and speech.

Eighty-two voices state a licence that allows commercial use. Seventy-three state none, which
the catalog records as `unknown` with `commercial_use: false` rather than guessing, and most
English cards name a parent model that is research only, kept in `meta.finetuned_from`. Read both
before shipping a voice in a product.

Pick one with `ARIOSO_TTS_VOICE` or `tts.voice`. Measured cost per voice, with model load
included, is in [MODELS.md](MODELS.md): the four medium and low voices sit within a fifth of
each other, and the high tier costs about five times as much. The low tier is also the
smallest file, at 16 kHz.

## Speech to text

`arioso transcribe` takes a WAV file, mono, 16-bit PCM or 32-bit float, at any rate. The
audio is resampled to 16 kHz and read by the model in `stt.batch_model`, which defaults to
Parakeet v2 at 631 MiB. Whisper base and tiny are in the catalog at 153 and 99 MiB for a
smaller install; tiny is the least accurate of the three, and on the same recording it wrote
"Tonseller built REO so" where base wrote "Tom Sella built a Rio so".

```console
$ arioso transcribe hello.wav
The quick brown fox jumps over the lazy dog.
```

`POST /v1/stt` takes the audio as the body and answers with the text and its length:

```console
$ curl -X POST --data-binary @hello.wav -H 'Content-Type: audio/wav' \
    http://127.0.0.1:10101/v1/stt
{"text":"The quick brown fox jumps over the lazy dog.","seconds":2.67}
```

`GET /v1/stt/ws` streams. The client sends a start frame, then 16 kHz mono `s16le` PCM,
and receives a final per utterance with its offsets. Partials arrive between them, when
the engine is free:

```text
client → {"type":"start","sample_rate":16000}
server → {"type":"ready","session":"6ae847c402bc","sources":["mic"]}
client → <binary PCM frames>
server → {"type":"partial","text":"The quick brown fox jump.","t":1.54}
server → {"type":"final","text":"The quick brown fox jumps over the lazy dog.","start":0.84,"end":3.56}
client → {"type":"cancel"}
server → {"type":"cancelled"}
```

A failure answers `{"type":"error","code":...,"detail":...}` with a sentence safe to show a
user, and ends the session: no `done` follows, because `done` says the work finished.

Utterances are cut by an energy voice activity detector, so a pause ends one and starts
the next. The final lands about 660 ms after the endpoint on four cores with whisper
base, and about 340 ms with tiny: the detector confirms at 4.06 s of audio, and the final
arrives at 4.72 s of wall clock with base.

The same dictionary that fixes a pronunciation on the way out is applied on the way in,
so a term you taught the voice comes back spelled your way. The correction compares sounds
rather than letters: the transcript is phonemized and matched against the pronunciation the
dictionary entry produces, so a recognition that spells the term differently still resolves.

## HTTP

| Route | Method | Returns |
| --- | --- | --- |
| `/healthz` | GET | `ok`, once the process is up |
| `/readyz` | GET | `200 ready`, or `503` with the reason: the voice, the speech model, both installation flags, the dictionary state, and how many models are installed |
| `/v1/info` | GET | Version, target, and the engines built into this binary |
| `/v1/engines` | GET | What each engine can do, and whether one is loaded |
| `/v1/models` | GET | Every model in the manifest, with kind, engine, licence, bytes, whether it is installed, and whether commercial use is allowed |
| `/v1/models/events` | GET | The same state as it changes, as server-sent events: a snapshot, then state and progress, and a `lagged` event that says to re-read the snapshot |
| `/v1/models/{id}/download` | POST | Start a download; `202` with progress, and `202` again with the current byte count if one is already running |
| `/v1/models/{id}/warm` | POST | Load a model before the first request needs it |
| `/v1/models/{id}` | DELETE | Remove a model and its partial file, and unload it |
| `/v1/voices` | GET | Voices with licence, rate, and which one is active |
| `/v1/config` | GET, PATCH | The config; a patch changes it without a restart |
| `/v1/tts` | POST | Audio for `{"text": "...", "speed": 1.0}`, as `wav` or `pcm` per `tts.format` |
| `/v1/tts/ws` | GET | Upgrade to WebSocket, then one PCM frame per sentence |
| `/v1/stt` | POST | A transcript for a WAV body |
| `/v1/stt/ws` | GET | Upgrade to WebSocket, then partial and final transcripts |
| `/v1/stop` | POST | `202`, then drain in-flight work and exit |
| `/v1/abort` | POST | `202`, then exit at once |
| `/docs` | GET | Swagger UI over the document below |
| `/openapi.json` | GET | The OpenAPI 3.1 document |

The document is generated from the route table, so a route cannot exist without appearing
in it. Every route is registered whatever the config says; a route belonging to a disabled
engine answers `404` with code `engine_disabled`, and leaves the document while the engine
is off.

Errors use RFC 9457 problem details: `status`, `title`, `detail`, and a `code` a client can
branch on without reading the sentence.

### Changing the config while it runs

`PATCH /v1/config` changes only the fields you name:

```console
$ curl -sS -X PATCH http://127.0.0.1:10101/v1/config \
    -H 'content-type: application/json' \
    -d '{"persist": true, "tts": {"speed": 1.6}}'
```

`persist` writes the file, so the change survives a restart. An unknown field is refused,
so a typo cannot look like success:

```console
$ curl -sS -X PATCH http://127.0.0.1:10101/v1/config \
    -H 'content-type: application/json' -d '{"tts": {"speeed": 1.2}}'
Failed to deserialize the JSON body into the target type: tts.speeed: unknown field
`speeed`, expected one of `enabled`, `engine`, `voice`, `speed`, `format`, `time_of_day`
at line 1 column 16
```

That one is axum's rejection and answers in plain text with 422. A value the schema
accepts but the server cannot use answers in the problem-details shape instead.

Validation runs on the whole resulting config before anything is applied, so a patch that
names one bad value changes none of its good ones. An engine this binary cannot serve is
refused with the names of the ones it can:

```console
$ curl -sS -X PATCH http://127.0.0.1:10101/v1/config \
    -H 'content-type: application/json' -d '{"tts": {"engine": "espeak"}}'
{"code":"invalid_value","detail":"tts.engine espeak is not built into this binary;
the engines here are piper, kokoro","status":422,...}
```

`piper` and `kokoro` are both built in. One engine loads per process: a patch that changes
`tts.engine` drops the loaded voice, so the next request speaks with the other engine. A
voice belongs to one engine and the two share no names, so a patch that changes the engine
and names no voice gets that engine's default (`af_heart` for Kokoro), and the response
carries a `notes` array saying which voice was set:

```console
$ curl -sS -X PATCH http://127.0.0.1:10101/v1/config \
    -H 'content-type: application/json' -d '{"tts": {"engine": "kokoro"}}' | \
    jq -r '.notes[], .tts.voice'
tts.engine is kokoro, so tts.voice was set to that engine's default af_heart; it was en_US-joe-medium
af_heart
```

The WebSocket stream sends a `start` frame with the sample rate and the sample format
(`pcm_s16le`), one binary frame per sentence, and an `end` frame with the chunk and sample
counts. Audio starts before the last sentence is synthesized, because the queue between
them holds two frames. A `{"type":"cancel"}` frame stops the render and answers `{"type":"cancelled"}`. A
failure answers `{"type":"error","detail":...}` and ends the stream.

## Sidecar mode

A program that wants a server, not a shell, sets `server.sidecar` and reads one line:

```console
$ arioso serve
START 10194
```

The parent process exits; the child serves. `START` is the only thing on stdout — logs and
errors go to stderr — so a caller reads the port from a pipe without parsing anything. When
the configured port is taken the child takes one the system picks and reports that instead,
so two instances never collide. `arioso stop` ends it.

A running server can also be stopped over HTTP: `POST /v1/stop` drains in-flight work, and
`POST /v1/abort` answers and exits at once. Sidecar mode refuses a non-loopback bind,
because it carries no token.

## Configuration

`config.json.example` in the repository root is a complete document with every default in
it, and a test fails if the defaults move without it.

The config file lives at `~/.local/share/arioso/config.json` on Linux,
`~/Library/Application Support/arioso/config.json` on macOS, and
`%LOCALAPPDATA%\arioso\config.json` on Windows. Pass `--config` to use another file.

```json
{
  "schema_version": 1,
  "server": { "bind": "127.0.0.1:10101", "first_message_timeout_ms": 10000 },
  "tts": {
    "enabled": true,
    "engine": "piper",
    "voice": "en_US-joe-medium",
    "speed": 1.0,
    "time_of_day": true,
    "dictionary": null
  }
}
```

Every setting has an environment override, and the environment wins over the file:

| Variable | Setting |
| --- | --- |
| `ARIOSO_SERVER_BIND` | `server.bind` |
| `ARIOSO_AUTH_MODE` | `server.auth.mode` |
| `ARIOSO_MODELS_DIR` | `models.dir` |
| `ARIOSO_TTS_ENABLED` | `tts.enabled` |
| `ARIOSO_TTS_ENGINE` | `tts.engine` |
| `ARIOSO_TTS_VOICE` | `tts.voice` |
| `ARIOSO_TTS_SPEED` | `tts.speed`, from 0.5 to 2.0 |
| `ARIOSO_TTS_TIME_OF_DAY` | `tts.time_of_day` |
| `ARIOSO_TTS_DICTIONARY` | `tts.dictionary` |
| `ARIOSO_STT_ENABLED` | `stt.enabled` |
| `ARIOSO_STT_ENGINE` | `stt.engine` |
| `ARIOSO_LOG_LEVEL` | `log.level` |
| `ARIOSO_LOG_FORMAT` | `log.format` |

## The user dictionary

`dictionary.json` sits beside the config file. One written term maps to one or more spoken
forms, tried in order. The first form the voice can render wins.

The binary ships a small dictionary and uses it when you have no file of your own. It
holds the project's name and the name of the person it was built for, because neither is
in CMUdict:

```json
{
  "version": 1,
  "entries": [
    { "term": "arioso", "spoken": ["ah ree oh so"] },
    { "term": "Tom Sella", "spoken": ["tom seller"] },
    { "term": "Sella", "phonemes": ["S EH1 L AA0"] }
  ]
}
```

Your file replaces it rather than adding to it, so what you wrote is what is heard. Copy
the block above as a starting point and change the names to your own.

```json
{
  "version": 1,
  "entries": [
    { "term": "arioso", "spoken": ["ah ree oh so"] },
    { "term": "Tom Sella", "spoken": ["tom seller"] },
    { "term": "Sella", "phonemes": ["S EH1 L AA0"] }
  ]
}
```

A respelling is plain text. `phonemes` takes ARPAbet, in any case, and wins over a
respelling when the voice can say it. The same file works backwards for speech to text:
any spoken form maps back to the term, so a transcript comes back spelled your way.

The file reloads on change. A file that stops parsing keeps the last good copy and reports
the error in `/readyz`, because a typo should not stop speech.

`arioso doctor` names the entries the configured voice cannot say, so a dictionary does not
fail silently:

```console
$ arioso doctor
dictionary  3 entries from /home/you/.local/share/arioso/dictionary.json
            1 entr(y/ies) this voice cannot say: xyzzy
Error: 1 problem(s) found
```

## Voices and licenses

Piper voices carry their own licenses, and they are not all permissive. The default,
`en_US-joe-medium`, is CC0, so what ships is shippable. `en_US-lessac-medium` is
**Blizzard-2013, research only**: it is in the catalog because it is the voice Piper's own
examples use, and it is the one voice here you cannot ship. The rest are permissive:
`ljspeech` is public domain, `alba` is CC BY 4.0, and `kathleen` is CC0. `arioso models
list` prints the license of every model, and `/v1/models` says whether commercial use is
allowed.

The code is MIT. The CMU dictionary is BSD-2-Clause. Piper's runtime is MIT; the training
code is GPL-3.0, which is why the `gpl-espeak` feature is off by default.

## Limits worth knowing

- Times are spoken on a 12-hour clock. `19:45` and `7:45` differ only because the part of
  the day is added, and a bare `7:45` is read as morning.
- A word outside the CMU dictionary is pronounced by spelling rule and named in the
  warning. The rules are a fallback: "zorbansky" is heard, and it is a guess. One
  dictionary entry replaces the guess with the truth.
- One voice is loaded at a time, so concurrent requests queue. Each voice costs about
  60 MB, which is a memory decision before it is a code one.
- The speech model is resolved through the manifest, so several can sit in one models
  directory and `stt.batch_model` picks between them.
- The manifest is not signed yet, so a download trusts the SHA-256 in the manifest file
  itself.
- The server binds to loopback. Binding it elsewhere is your call: set `server.auth` to a
  token, and `server.tls` to a certificate and key, before you do.

## Layout

| Crate | Purpose |
|---|---|
| `arioso-config` | Config model, defaults, file loading, environment overrides, validation |
| `arioso-models` | Model manifest, cache layout, resumable downloader, integrity checks |
| `arioso-core` | Sentence splitting, chunking, normalization, the user dictionary |
| `arioso-g2p` | Phoneme id map, CMUdict, ARPAbet to IPA |
| `arioso-tts-piper` | Piper VITS inference through ONNX Runtime; the one text to audio path |
| `arioso-stt-sherpa` | Speech to text through sherpa-onnx: Parakeet transducers and Whisper |
| `arioso-vad` | Energy voice activity detection, and the rule that cuts an utterance |
| `arioso-server` | axum router, the engines behind it, the downloader, and the WebSocket paths |
| `arioso-cli` | The `arioso` binary |

More worked examples, with the real output of each, are in [EXAMPLES.md](EXAMPLES.md).
Guidance for putting this in your own program, with a use case per shape, is in
[IMPLEMENTATION.md](IMPLEMENTATION.md). The product requirements are in [PRD.md](PRD.md).
Which models are measured, what they cost, which languages they cover, and the evidence
behind the defaults is in [MODELS.md](MODELS.md). What the code costs in time and memory, on
the smallest machine it targets, is in [BENCHMARK.md](BENCHMARK.md). To measure your own
machine and add it to the shared table, run [bench/run.py](bench/README.md).
The execution plan is in [docs/plans/](docs/plans/). Mistakes that cost time, and the rules
that prevent repeats, are in [docs/lessons.md](docs/lessons.md). If you are changing this
repository, read [AGENTS.md](AGENTS.md).
