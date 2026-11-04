# Silero voice activity detection

## What exists

`arioso-vad` holds one segmenter and one rule: a frame is speech when its root mean square
clears `vad.threshold`. The segmenter owns that rule inside `frame()`, so the boundaries and
the decision are the same code. `vad.engine: "silero"` is refused at validation, because a
config that loads and runs the energy detector would read as working.

## The shape

Silero is a small ONNX model that classifies one 32 ms frame at a time, and sherpa-onnx
already carries it: `VoiceActivityDetector` with `SileroVadModelConfig`, on the runtime this
project links for speech to text. PRD F5.1 asks for exactly that: the sherpa runtime, not a
second ONNX session.

So the decision becomes an input to the segmenter instead of a line inside it.

- `arioso-vad` gains `trait Gate { fn speech(&mut self, frame: &[f32]) -> bool }`, and
  `Energy` implementing it with the rule that is there today. `Segmenter::new` keeps the
  energy gate, so nothing that exists changes; `Segmenter::with_gate` takes another.
- `arioso-stt-sherpa` gains `Silero`, which wraps `VoiceActivityDetector` and answers
  `detected()` after `accept_waveform`. It implements `arioso_vad::Gate`, so the segmenter
  does not learn about sherpa.
- The manifest gains `silero-vad`, kind `vad`: `silero_vad.onnx`, 643,854 bytes, MIT, from
  the sherpa-onnx release, hash verified like every other model.
- `vad.model` names the manifest entry, defaulting to `silero-vad`, so the file is fetched by
  the same downloader as a voice.
- The server builds the gate from `vad.engine`: `energy` needs nothing, `silero` needs the
  model on disk and answers `409 model_not_installed` at the socket when it is not.

## Constraints the model imposes

Silero reads 512 samples at 16 kHz, which is `frame_ms: 32`. A config that asks for another
frame length with `silero` is refused rather than fed frames the model cannot use.

## What does not change

`energy` stays the default. F5.5 promises the library works with no model installed, and a
default that needs a download and a native runtime is not a default for a library.

## Diarization, which is not in this plan

The engine has no diarization and this plan does not add it. The transcriber gets two labels
from two channels — microphone and system audio — with no model at all, which is what its v1
is built on. Separating unknown speakers from one stream is a different problem and needs a
segmentation model, an embedding extractor and clustering; sherpa-onnx ships that
(`sherpa-onnx-offline-speaker-diarization`), so it is a matter of two more manifest entries
and a clustering pass over the segments this VAD produces. It is not scheduled.
