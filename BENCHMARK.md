# Benchmarks

What arioso costs on this machine, measured with the code in this repository. Every number
here was produced by a command in this file's method section, on the release build, three
runs each, and the middle value is the one quoted.

## The machine

Intel N150, four cores, no GPU, 16 GB of memory, Linux 6.8, `x86_64`. Release build with
thin LTO, one codegen unit, symbols stripped. This is the small server the project is aimed
at, not a workstation, so the numbers are pessimistic on purpose.

## Method

- **Synthesis.** `arioso say` on 160 sentences of fixed text: 316.44 s of audio through
  `en_US-joe-medium`. Wall clock and peak resident memory from `/usr/bin/time -v`.
- **Transcription.** `arioso transcribe` on ten minutes of 22050 Hz speech through
  `sherpa-whisper-tiny-en`. Ten minutes is long enough to cross several 29 second windows
  and to need resampling, which is where the cost is.
- **Server.** Time from process start to the first `200` on `/healthz`, then requests per
  second with eight concurrent clients on a keep-alive connection each, one route at a time.
- **Per path.** A timing harness that calls one function in a loop and reports the median of
  three runs. Memory for a single call is a separate process measured with
  `/usr/bin/time -v`, because a peak is not something a loop can report.

`RTF` is processing time divided by audio length. Below 1.0 is faster than real time.

## End to end

| what | audio | wall | RTF | peak RSS |
| --- | --- | --- | --- | --- |
| synthesis, `en_US-joe-medium` | 316.44 s | 29.9 s | 0.095 | 522 MB |
| transcription, `sherpa-whisper-tiny-en` | 600.00 s | 81.2 s | 0.135 | 548 MB |

## Server

| measure | value |
| --- | --- |
| start to first healthy response | 85 ms |
| `GET /healthz` | 3,528 per second |
| `GET /v1/config` | 3,330 per second |
| `GET /v1/models` | 3,172 per second |
| release binary | 42.3 MB |

Eight clients on four cores: a request is about 285 µs, and almost all of it is the HTTP
stack. No route in this set touches a model.

## Per path

| path | measure | cost |
| --- | --- | --- |
| `CmuDict::parse`, 126,052 words | one parse | 86.8 ms, 36.5 MB peak |
| `CmuDict::lookup` | 11 words | 0.43 µs |
| `wav::to_bytes` | 60 s at 22050 Hz | 4.42 ms |
| `wav::read_bytes` | 10 min stereo at 48 kHz | 178 ms, 227 MB peak |
| `Segmenter::push` | 60 s at 16 kHz, one second at a time | 1.06 ms |
| `normalize` | 200 sentences, no digits | 0.12 ms |
| `normalize` | 200 sentences, dense with numbers | 0.38 ms |
| `reverse::correct` | 20 word transcript, 10 entries | 0.06 ms |
| `PhonemeMap::to_ids_i64` | one chunk of 11 phonemes | 0.25 µs |

The two memory figures are the ones worth knowing before sizing a machine: a ten minute
stereo file costs 227 MB to read, and the dictionary costs 36.5 MB to hold.

## What this pass changed, and what it bought

Each row is one change, measured on both builds with the same harness. The right hand
column is the ratio of the older build's time to this one.

| change | before | after | ratio |
| --- | --- | --- | --- |
| dictionary parse, map sized once, no per-line join | 126.2 ms | 86.8 ms | 1.45× |
| dictionary lookup, no lowercase of a lowercase word | 0.61 µs | 0.43 µs | 1.42× |
| dictionary memory, entries shrunk | 43.5 MB | 36.5 MB | 1.19× |
| WAV write, one buffer instead of two | 6.55 ms | 4.42 ms | 1.48× |
| WAV read, channels mixed while decoding | 248 ms | 178 ms | 1.40× |
| WAV read memory | 454 MB | 227 MB | 2.00× |
| transcription, windows cut before resampling | 109.3 s | 81.2 s | 1.35× |
| VAD framing, frames read from the caller's slice | 3.86 ms | 1.06 ms | 3.64× |
| reverse pass, length reject and reused buffers | 1.29 ms | 0.06 ms | 23.4× |
| normalization, fast paths for text with nothing to expand | 0.15 ms | 0.12 ms | 1.24× |
| binary size, release profile | 54.9 MB | 42.3 MB | 1.30× |
| start to first healthy response | 105 ms | 85 ms | 1.24× |

## What did not move

Three changes were measured and showed nothing, which is worth recording as much as the
ones that did.

- **The config clone.** Handing out an `Arc<Config>` instead of a copy is correct, and at
  3,500 requests per second on `/healthz` the difference is inside the noise: the route
  costs 285 µs and the copy was a few of them.
- **The phoneme sequence.** Building the tensor's `i64` sequence directly instead of
  converting a `u32` one saves one allocation per chunk, and 11 phonemes is too small a
  chunk to show it: 0.26 µs against 0.25 µs.
- **Synthesis time.** The graph now runs at optimization level 3, and the end-to-end time is
  29.9 s against 29.0 s on the older build: inside the run-to-run spread, which is ±3% on
  this machine. The level is kept for the fused kernels, not for a measured win.

One change was reverted after it was measured. Bounding the ONNX intra-op threads to half
the cores cost between a fifth and a quarter of the synthesis time on four cores: 36.5 s and
36.8 s on two runs against 28.7 s to 30.2 s on three runs of the older build, for the same
316 s of audio. The reasoning behind the bound, leaving room for the server's other work,
was wrong for this design: one voice synthesizes one request at a time, so the graph is the
only work in flight. The count is every core now.

## Limits

- **One machine, one build, one corpus per engine.** Nothing here says what a machine with
  more cores, or a different model, will do.
- **Wall clock on a quiet machine.** The measurements were taken with nothing else running;
  a busy machine moves every number.
- **Peak RSS includes the model.** 522 MB for synthesis is mostly the voice's weights and
  the ONNX runtime, not the audio.
- **Transcripts differ between builds and between runs.** The ten minute transcription
  produced 1,877 and 1,875 words on the two builds, agreeing on 97.6% of them. Whisper's
  decode is not deterministic across thread scheduling, so that difference is variance, not
  a measured accuracy change. Accuracy is in `MODELS.md`, which scores against a reference;
  this file does not.
- **No GPU numbers.** There is no GPU here to measure.
