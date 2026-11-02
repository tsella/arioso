# Arioso

Local text to speech and speech to text as a Rust library and a standalone server,
on macOS, Linux, Windows, iOS, and Android, with no cloud dependency.

Named for the musical term: a melodious, singing style of delivery.

Status: M0 (skeleton). Model management works; engines are not wired up yet.

## Layout

| Crate | Purpose |
|---|---|
| `arioso-config` | Config model, defaults, file loading, environment overrides, validation |
| `arioso-models` | Model manifest, cache layout, resumable downloader, integrity and signature checks |
| `arioso-cli` | The `arioso` binary |

The product requirements are in [PRD.md](PRD.md). The execution plan is in
[docs/plans/](docs/plans/).

## Build

The Piper engine links against ONNX Runtime. Fetch the prebuilt 1.22.0 runtime once
and point the build at it:

```bash
mkdir -p ~/.local/lib && cd ~/.local/lib
curl -sSL -o ort.tgz \
  https://github.com/microsoft/onnxruntime/releases/download/v1.22.0/onnxruntime-linux-x64-1.22.0.tgz
tar xzf ort.tgz
export ORT_LIB_LOCATION=$HOME/.local/lib/onnxruntime-linux-x64-1.22.0
export LD_LIBRARY_PATH=$ORT_LIB_LOCATION/lib
```

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
```
