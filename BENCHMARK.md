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
  second with eight concurrent clients on a keep-alive connection each, one route at a time,
  1,000 requests per client, median of three rounds. **The rate limiter is raised to
  100,000 for this measurement.** Left at its default of 50 a second with a burst of 100,
  every client is `127.0.0.1`, so the eight of them share one bucket and the run measures
  the limiter instead of the server: 400 requests come back as 101 answered and 299
  refused.
- **Per path.** A timing harness that calls one function in a loop and reports the median of
  three runs. Memory for a single call is a separate process measured with
  `/usr/bin/time -v`, because a peak is not something a loop can report.

`RTF` is processing time divided by audio length. Below 1.0 is faster than real time.

## End to end

| what | audio | wall | RTF | peak RSS |
| --- | --- | --- | --- | --- |
| synthesis, `en_US-joe-medium` | 316.44 s | 30.0 s | 0.095 | 522 MB |
| transcription, `sherpa-whisper-tiny-en` | 600.00 s | 81.2 s | 0.135 | 548 MB |

## Server

| measure | value |
| --- | --- |
| start to first healthy response | 89 ms |
| `GET /healthz` | 29,100 per second |
| `GET /v1/config` | 27,600 per second |
| `GET /v1/models` | 23,300 per second |
| release binary | 42.3 MB |

Eight clients on four cores: a request costs about 275 µs of client wall time, and almost
all of it is the HTTP stack on both ends. No route in this set touches a model. The three
routes differ by the size of the response, 2 bytes against 1,243 and 1,453.

## Per path

| path | measure | cost |
| --- | --- | --- |
| `CmuDict::parse`, 126,052 words | one parse | 20.5 ms, 22.5 MB peak |
| `CmuDict::lookup` | 11 words | 0.43 µs |
| `wav::to_bytes` | 60 s at 22050 Hz | 4.42 ms |
| `wav::read_bytes` | 10 min stereo at 48 kHz | 178 ms, 227 MB peak |
| `Segmenter::push` | 60 s at 16 kHz, one second at a time | 1.06 ms |
| `normalize` | 200 sentences, no digits | 0.12 ms |
| `normalize` | 200 sentences, dense with numbers | 0.38 ms |
| `reverse::correct` | 20 word transcript, 10 entries | 0.06 ms |
| `PhonemeMap::to_ids_i64` | one chunk of 11 phonemes | 0.25 µs |

The two memory figures are the ones worth knowing before sizing a machine: a ten minute
stereo file costs 227 MB to read, and the dictionary costs 22.5 MB to hold.

The dictionary row is a re-measurement. Its 86.8 ms and 36.5 MB came from the harness of the
earlier performance pass; the parse now stores a one-form word's pronunciation inline, and
the row was measured again on this tree with the loop harness and the separate-process RSS
method below. The two readings are not a before/after pair — that pair, with both sides
measured by one harness, is in `docs/plans/2026-11-03-performance.md`.

The two `normalize` rows are from the build before the text-path pass; that pass removes the
lowercase copy of every sentence and the temporary `String` per number, and changes what
`chunk_text` costs. `docs/plans/2026-11-03-performance.md` records both sides of it,
measured with one harness on the two trees.

## A voice switch

Kokoro is one 325 MB graph and an archive of 54 style tables, 522,368 bytes each. Changing
`tts.voice` used to drop the loaded engine, so the next request re-read and re-compiled the
graph; it now reads the one style table and swaps it on the engine already in memory. Both
builds served the same request on the same machine:

- Five voice switches each, alternating `af_heart` and `af_bella`, one synthesis of the same
  sentence after each. Median quoted. Load average 3.9 to 5.4 on the older build and 4.5 to
  6.1 on the newer one, so the machine was not quiet and the synthesis figures are not a
  controlled comparison; the forced load is, because one build did it and the other did not.
- `POST /v1/models/{id}/warm` loads the configured voice only when the slot is empty, so its
  latency is exactly the work the patch left for the next request.

| what | before | after |
| --- | --- | --- |
| `PATCH /v1/config` (`tts.voice` only) | 16.6 ms | 1.1 ms |
| load forced by the patch, paid by the next request | 804.9 ms | 0.5 ms |
| `voice_loaded` on `/readyz` right after the patch | `false`, 5 of 5 | `true`, 5 of 5 |
| first synthesis after the patch | 1.90 s | 2.02 s |
| `notes` in the PATCH response | absent | absent |

The first switch on the newer build cost 715 ms, against 1.0 to 1.2 ms for the four after it:
that is the 522 KB table being read from disk the first time. The first load on the older
build cost 2.45 s against 0.80 s for the four after it, which is the 325 MB graph being read
from disk with nothing in the page cache. Neither is a steady-state figure.

Reading one style table on this machine, median of 21 reads, three runs: 7.3 ms, 10.1 ms and
12.6 ms for the 522,368-byte entry. That read used to hold two copies of the table it
decoded — the entry's bytes and then the floats — 1,044,608 bytes for a 522,240 byte table;
it now decodes as the bytes arrive, one allocation and one table. Indexing the archive by
walking its 54 local headers, the same method: 0.23 to 0.29 ms. That walk is paid once per
`GET /v1/voices` and once per readiness check, which is why it is kept: reading the central
directory instead would save 0.3 ms and drop the per-entry header check that refuses an entry
whose size disagrees with the file.

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
  29,000 requests per second on `/healthz` the difference is inside the noise: the route
  costs about 275 µs and the copy was a few of them.
- **The phoneme sequence.** Building the tensor's `i64` sequence directly instead of
  converting a `u32` one saves one allocation per chunk, and 11 phonemes is too small a
  chunk to show it: 0.26 µs against 0.25 µs.
- **Synthesis time.** The graph now runs at optimization level 3, and the end-to-end time is
  30.0 s against 29.0 s on the older build: inside the run-to-run spread, which is ±3% on
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
