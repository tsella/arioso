# Roadmap

Work that is wanted and not scheduled. A row here carries the reason it is not done yet, and
where one exists, the measurement that decided that.

## Engines

- **BlueTTS 2.5 as a third engine.** MIT engine, LibriTTS voice `adam`, 269 MiB, Hebrew,
  English, Spanish, Italian and German. Measured outside this repository, from the upstream
  Python package: round trip 4.88% against 5.88% for the shipped Piper voice, RTF 0.600
  against 0.089, memory 1.5 GiB against 290 MiB. It is more intelligible by about a point at
  6.7 times the synthesis time. The engine is Python and this project is Rust, so adopting it
  means a sidecar process and a protocol of its own, not a new branch in the current one.
  Numbers and the full comparison: `MODELS.md`, and
  `docs/plans/2026-11-02-tts-engine-comparison.md`.
