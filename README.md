# Arioso

Speech out of text, on your own machine. No account, no API key, no audio leaving the box.

Arioso is a Rust library and a single binary. It speaks text with Piper voices, streams
audio over WebSocket as each sentence is ready, and gives you a place to correct the
pronunciation of your own names. Speech to text comes next.

```console
$ arioso say "hello world. this is arioso speaking locally."
wrote arioso.wav (2.11 s at 22050 Hz)
1 word(s) not in the dictionary and skipped: arioso
```

That warning is the point of the dictionary section below. Arioso tells you what it could
not say instead of quietly leaving a hole in the sentence. The length moves by a few
hundredths between runs, because the model samples noise.

## Why this exists

Cloud text to speech is good and fast, and it sends every sentence to someone else's
server. That is fine for a shopping list and wrong for a support call, a medical note, or
anything under a confidentiality agreement. Arioso keeps the sentence on the machine that
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

- Speech to text over HTTP and WebSocket, and streaming partials (M2)
- Silero voice activity detection
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

## Speech to text

`arioso transcribe` takes a WAV file, mono, 16-bit PCM or 32-bit float, at any rate. The
audio is resampled to 16 kHz and read by Whisper.

```console
$ arioso transcribe hello.wav
The quick brown fox jumps over the lazy dog.
```

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
| `/v1/models` | GET | Every model in the manifest, with size and license |
| `/v1/tts` | POST | `audio/wav` for `{"text": "...", "speed": 1.0}` |
| `/v1/tts/ws` | GET | Upgrade to WebSocket, then one PCM frame per sentence |

Errors use RFC 9457 problem details, with `status`, `title`, and `detail`.

The WebSocket stream sends a `start` frame with the sample rate, one binary frame per
sentence, and an `end` frame with the counts. Audio starts before the last sentence is
synthesized, because the queue between them holds two frames.

## Configuration

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

Named for the musical term: a melodious, singing style of delivery.

More worked examples, with the real output of each, are in [examples.md](examples.md). The
product requirements are in [PRD.md](PRD.md). The execution plan is in
[docs/plans/](docs/plans/). Mistakes that cost time, and the rules that prevent repeats,
are in [docs/lessons.md](docs/lessons.md).
