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

```bash
cargo build
cargo test
```

## Use

```bash
arioso models list
arioso models download piper-en_US-lessac-medium
arioso doctor
```
