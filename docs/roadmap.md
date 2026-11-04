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

## Platforms

- **Piper and sherpa-onnx on iOS and Android.** Both engines link a prebuilt native library,
  and no build of either for those targets is wired here. `mobile.yml` compiles the crates
  that need no such library, so the boundary is known and the work is a per-target build of
  ONNX Runtime and sherpa-onnx, then a link step.

- **A smoke test on a simulator.** CI builds the mobile targets and runs nothing. A run needs
  a machine with the toolchains installed, which this project does not have, so the build is
  the whole of the evidence today.

## Speech

- **Silero voice activity detection.** The PRD names it the default. What ships is the energy
  detector, which cuts an utterance on a loudness threshold and is fooled by steady noise. The
  engine id `vad.engine: "silero"` is refused at validation rather than accepted and ignored,
  so a config cannot ask for it and be told nothing.

## Release

- **A release signing key.** The manifest loader verifies a detached Ed25519 signature when
  `RELEASE_PUBKEY_HEX` is compiled in, and the keygen and sign commands exist and are tested.
  No key is compiled in, so every build today loads the manifest as `Unsigned` and says so.
  Minting the key is an operator step on the release machine; the private half must never
  enter the repository.

