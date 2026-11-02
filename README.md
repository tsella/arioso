# arioso

[![ci](https://github.com/tsella/arioso/actions/workflows/ci.yml/badge.svg)](https://github.com/tsella/arioso/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![platforms](https://img.shields.io/badge/platforms-linux%20%7C%20macOS%20%7C%20windows-lightgrey)
![inference](https://img.shields.io/badge/inference-local-brightgreen)

Speech out of text, and text out of speech, on your own machine. No account, no API key,
no audio leaving the box.

Named for the musical term: a melodious, singing style of delivery.

arioso is a Rust library and a single binary. It speaks with Piper voices and listens with
Whisper, streams both ways over WebSocket, and gives you a place to correct the
pronunciation of your own names in both directions.

```console
$ arioso say "hello world. this is arioso speaking locally."
wrote arioso.wav (2.11 s at 22050 Hz)
1 word(s) not in the dictionary, spoken by spelling rules: arioso
```

That line is the point of the dictionary section below: a word the built-in dictionary
does not hold is spoken by spelling rules, and named, so a guess is never silent. The
length moves by a few hundredths between runs, because the model samples noise.

## Why this exists

Cloud text to speech is good and fast, and it sends every sentence to someone else's
server. That is fine for a shopping list and wrong for a support call, a medical note, or
anything under a confidentiality agreement. arioso keeps the sentence on the machine that
read it.

It is not a cloud service in a box. It runs a 63 MiB voice on the CPU, at about a twelfth
of real time on a four-core N150 with no GPU. You trade voice quality and latency for
that.

## Status

Working today:

- Text to speech with Piper voices: WAV over HTTP, PCM frames over WebSocket
- Model download from a manifest, with SHA-256 verification and resumable transfers
- Text normalization: currency, ordinals, years, times, percentages, abbreviations
- A user dictionary for names and jargon, reloaded without a restart
- Spelling rules for words the dictionary does not hold, reported, never silent
- Speech to text: Whisper through sherpa-onnx, batch, from a WAV file
- `arioso doctor`, which says what is wrong before a request fails
- 144 tests

Not built yet:

- Silero voice activity detection, which the PRD names as the default over the energy
  detector that ships
- A streaming Zipformer model, so partials come from a streaming engine instead of
  re-running Whisper on the buffer
- A signed manifest

## Requirements

- Rust 1.91 or newer
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
arioso models download piper-en_US-lessac-medium
arioso say "hello world. this is arioso speaking locally." --out hello.wav
arioso serve        # http://127.0.0.1:10101
```

```console
$ arioso models list
id                                 kind        size  license    state
piper-en_US-lessac-medium          tts     60.3 MiB  Blizzard-2013 installed
```

Then, from another shell:

```bash
curl -s -X POST localhost:10101/v1/tts -H 'content-type: application/json' \
  -d '{"text": "hello world"}' -o out.wav
```

## Commands

| Command | What it does |
| --- | --- |
| `arioso say <text> [--out FILE] [--dict FILE]` | Write a WAV file |
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

Four voices beyond the default, all verified by SHA-256 against the manifest:

| id | tier | rate | licence | commercial |
| --- | --- | --- | --- | --- |
| `piper-en_US-lessac-medium` | medium | 22.05 kHz | Blizzard-2013 | no |
| `piper-en_US-ljspeech-high` | high | 22.05 kHz | public domain | yes |
| `piper-en_US-joe-medium` | medium | 22.05 kHz | CC0 | yes |
| `piper-en_US-kathleen-low` | low | 16 kHz | CC0 | yes |
| `piper-en_GB-alba-medium` | medium | 22.05 kHz | CC BY 4.0 | yes |

Pick one with `ARIOSO_TTS_VOICE` or `tts.voice`. Measured on four cores with no GPU, same
sentence, model load included:

| voice | wall clock | audio | real time factor |
| --- | --- | --- | --- |
| `en_US-kathleen-low` | 2.40 s | 5.38 s | 0.45 |
| `en_US-joe-medium` | 2.47 s | 4.92 s | 0.50 |
| `en_GB-alba-medium` | 2.47 s | 4.83 s | 0.51 |
| `en_US-lessac-medium` | 2.53 s | 5.03 s | 0.50 |
| `en_US-ljspeech-high` | 5.77 s | 5.83 s | 0.99 |

The high tier costs twice the time of medium and is not twice the voice. The low tier is
the fastest and the smallest file, at 16 kHz.

## Speech to text

`arioso transcribe` takes a WAV file, mono, 16-bit PCM or 32-bit float, at any rate. The
audio is resampled to 16 kHz and read by Whisper base, the catalog default at 153 MiB.
Whisper tiny is in the catalog too, at 99 MiB, and it is not as accurate: on the same
recording it wrote "Tonseller built REO so" where base wrote "Tom Sella built a Rio so".

```console
$ arioso transcribe hello.wav
The quick brown fox jumps over the lazy dog.
```

`POST /v1/stt` takes the audio as the body and answers with the text and its length:

```console
$ curl -X POST --data-binary @hello.wav -H 'Content-Type: audio/wav' \
    http://127.0.0.1:10101/v1/stt
{"seconds":2.67,"text":"The quick brown fox jumps over the lazy dog."}
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

Utterances are cut by an energy voice activity detector, so a pause ends one and starts
the next. The final lands about 660 ms after the endpoint on four cores with whisper
base, and about 340 ms with tiny: the detector confirms at 4.06 s of audio, and the final
arrives at 4.72 s of wall clock with base.

The same dictionary that fixes a pronunciation on the way out is applied on the way in,
so a term you taught the voice comes back spelled your way. That path is real but narrow:
it matches a spoken form exactly, and Whisper rarely reproduces it letter for letter. The
PRD records the measured pairs and the planned fix, which is to compare sounds rather than
letters.

## HTTP

| Route | Method | Returns |
| --- | --- | --- |
| `/healthz` | GET | `ok`, once the process is up |
| `/readyz` | GET | The voice, whether it is installed, and the dictionary state |
| `/v1/info` | GET | Version, target, and the engines built into this binary |
| `/v1/engines` | GET | What each engine can do, and whether one is loaded |
| `/v1/models` | GET | Every model in the manifest, with size and license |
| `/v1/voices` | GET | Voices with licence, rate, and which one is active |
| `/v1/config` | GET, PATCH | The config; a patch changes it without a restart |
| `/v1/tts` | POST | `audio/wav` for `{"text": "...", "speed": 1.0}` |
| `/v1/tts/ws` | GET | Upgrade to WebSocket, then one PCM frame per sentence |
| `/v1/stt` | POST | A transcript for a WAV body |
| `/v1/stt/ws` | GET | Upgrade to WebSocket, then partial and final transcripts |
| `/docs` | GET | Swagger UI over the document below |
| `/openapi.json` | GET | The OpenAPI 3.1 document |

The document is generated from the handlers, so a route cannot exist without appearing in
it. A route belonging to a disabled engine is left out, because it is not registered.

Errors use RFC 9457 problem details, with `status`, `title`, and `detail`.

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
refused with the name of the one it can:

```console
$ curl -sS -X PATCH http://127.0.0.1:10101/v1/config \
    -H 'content-type: application/json' -d '{"tts": {"engine": "kokoro"}}'
{"code":"invalid_value","detail":"tts.engine kokoro is not built into this binary;
the engines here are piper","status":422,...}
```

Kokoro is M4. Until then the switch mechanism works and has one destination.

The WebSocket stream sends a `start` frame with the sample rate, one binary frame per
sentence, and an `end` frame with the counts. Audio starts before the last sentence is
synthesized, because the queue between them holds two frames.

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
    "voice": "en_US-lessac-medium",
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
| `ARIOSO_MODELS_DIR` | `models.dir` |
| `ARIOSO_TTS_VOICE` | `tts.voice` |
| `ARIOSO_TTS_SPEED` | `tts.speed`, from 0.5 to 2.0 |
| `ARIOSO_TTS_TIME_OF_DAY` | `tts.time_of_day` |
| `ARIOSO_TTS_DICTIONARY` | `tts.dictionary` |

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

Piper voices carry their own licenses, and they are not all permissive. The default in the
manifest, `en_US-lessac-medium`, is **Blizzard-2013, research only**. Before you ship
anything, pick a voice whose license fits: `ljspeech` is public domain, `libritts` is
CC BY 4.0, and `joe`, `mike`, `kathleen`, and `reza_ibrahim` are CC0. `arioso models list`
prints the license of every model.

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
- One Whisper model per models directory. The loader searches the directory flat, so a
  second model would resolve to whichever name sorts first.
- The manifest is not signed yet, so a download trusts the SHA-256 in the manifest file
  itself.
- The server binds to loopback. Binding it elsewhere is your call, and the bearer token
  policy in `server.auth` is the only thing standing there.

## Layout

| Crate | Purpose |
|---|---|
| `arioso-config` | Config model, defaults, file loading, environment overrides, validation |
| `arioso-models` | Model manifest, cache layout, resumable downloader, integrity checks |
| `arioso-core` | Sentence splitting, chunking, normalization, the user dictionary |
| `arioso-g2p` | Phoneme id map, CMUdict, ARPAbet to IPA |
| `arioso-tts-piper` | Piper VITS inference through ONNX Runtime; the one text to audio path |
| `arioso-server` | axum router: `/healthz`, `/readyz`, `/v1/models`, `POST /v1/tts`, `GET /v1/tts/ws` |
| `arioso-cli` | The `arioso` binary |

More worked examples, with the real output of each, are in [examples.md](examples.md).
Guidance for putting this in your own program, with a use case per shape, is in
[IMPLEMENTATION.md](IMPLEMENTATION.md). The product requirements are in [PRD.md](PRD.md).
The execution plan is in [docs/plans/](docs/plans/). Mistakes that cost time, and the rules
that prevent repeats, are in [docs/lessons.md](docs/lessons.md). If you are changing this
repository, read [AGENTS.md](AGENTS.md).
