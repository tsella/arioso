# Arioso

Local text to speech and speech to text as a Rust library and a standalone server,
on macOS, Linux, Windows, iOS, and Android, with no cloud dependency.

Named for the musical term: a melodious, singing style of delivery.

Status: M1. Piper speaks on Linux, through the CLI and over HTTP. Speech to text is
not wired up yet.

## Layout

| Crate | Purpose |
|---|---|
| `arioso-config` | Config model, defaults, file loading, environment overrides, validation |
| `arioso-models` | Model manifest, cache layout, resumable downloader, integrity and signature checks |
| `arioso-g2p` | Phoneme id map, CMUdict, ARPAbet to IPA |
| `arioso-core` | Sentence splitting and size-bounded chunking |
| `arioso-tts-piper` | Piper VITS inference through ONNX Runtime; the one text to audio path |
| `arioso-server` | axum router: `/healthz`, `/readyz`, `/v1/models`, `POST /v1/tts`, `GET /v1/tts/ws` |
| `arioso-cli` | The `arioso` binary |

The product requirements are in [PRD.md](PRD.md). The execution plan is in
[docs/plans/](docs/plans/). Mistakes that cost time, and the rules that prevent
repeats, are in [docs/lessons.md](docs/lessons.md).

## Build

The Piper engine links against ONNX Runtime. Fetch the prebuilt 1.22.0 runtime once
into the workspace, where `.cargo/config.toml` and the linker both find it:

```bash
mkdir -p vendor
curl -sSL -o /tmp/ort.tgz \
  https://github.com/microsoft/onnxruntime/releases/download/v1.22.0/onnxruntime-linux-x64-1.22.0.tgz
tar xzf /tmp/ort.tgz -C vendor
mv vendor/onnxruntime-linux-x64-1.22.0 vendor/onnxruntime
```

`vendor/` is gitignored. The build sets `ORT_LIB_LOCATION` from it, and an rpath
records the path relative to the binary, so neither `cargo build` nor
`./target/debug/arioso` needs an exported variable.

`ort` is pinned to 2.0.0-rc.10: 2.0.0-rc.15 requires rustc 1.92, and this toolchain is
1.91. The crate's own binary downloader is off, because it pulls OpenSSL in; pointing
`ORT_LIB_LOCATION` at the runtime avoids that.

```bash
cargo build
cargo test
```

## Use

```bash
arioso models list
arioso models download piper-en_US-lessac-medium
arioso doctor
arioso say "hello world" --out hello.wav
arioso serve        # http://127.0.0.1:10101
```

The HTTP surface:

```bash
curl -s localhost:10101/readyz
curl -s -X POST localhost:10101/v1/tts -H 'content-type: application/json' \
  -d '{"text": "hello world"}' -o out.wav
```

`GET /v1/tts/ws` streams instead: send `{"text": ...}` as the first text message,
then read a start frame with the sample rate, one binary PCM frame per sentence,
and an end frame with counts.
