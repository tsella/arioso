# Performance and Memory Pass

The plan asked for thirteen changes across five areas: startup allocations, audio buffers,
hot loops, per-request overhead, and the release build. This file records what was done,
what was measured, and what was deliberately left.

Every task was one commit, and the whole tree was gated on `cargo fmt --all -- --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test --workspace` with the `ARIOSO_*`
variables unset, and `cargo doc --no-deps`, with all four exit codes read.

## Done

| Task | Change | Commit |
|---|---|---|
| P0.1 | `CmuDict` sized once, the line tail sliced instead of split and rejoined, entries shrunk, `lookup` no longer lowercases a lowercase word | `715a00c` |
| P0.2 | `write_pcm_s16le` appends into the caller's buffer; `read_bytes` decodes and mixes channels in one pass | `9e045e8` |
| P0.3 | STT windows cut at the source rate and converted one window at a time into one buffer; `quietest` squares each half frame once | `bc83664` |
| P0.4 | VAD frames read straight out of the caller's slice; only a frame that straddles two pushes is copied | `6257c67` |
| P0.5 | A download that starts at zero is hashed as it arrives; writes are buffered at 256 KiB | `3e5ee78` |
| P1.1 | `config()` hands out an `Arc<Config>`; `token()` borrows | `6d879ab` |
| P3.2 | `GET /v1/config` writes the response once instead of through a `Value` tree | `8b569fd` |
| P1.3 | ONNX at optimization level 3, the tensor sequence built as `i64` directly; the thread bound was measured and reverted | `03e53b1`, `fix` |
| P1.4 | `arioso say` converts each chunk to PCM as it arrives | `f31f67e` |
| P1.2 | Reverse pass rejects impossible windows by length before the edit distance, reuses its buffers, and stops a DP row that cannot come back under the allowance | `073df25` |
| P2.1 | Normalization skips the abbreviation pass without a full stop and the `Vec<char>` without a digit or `$` | `e2be260` |
| P3.1 | `[profile.release]` with thin LTO, one codegen unit, stripped symbols, `panic = "abort"` | `bd289b4` |

## Measured, both builds

The first pass shipped without a before-number, which made every claim in it structural. The
second pass built 0.1.4 from the tag and this branch side by side and ran the same harness
against both: three runs each, the middle value quoted, on the four core machine the project
targets. Every row is in `BENCHMARK.md`.

| change | before | after | ratio |
| --- | --- | --- | --- |
| dictionary parse | 126.2 ms | 86.8 ms | 1.45x |
| dictionary lookup, 11 words | 0.61 us | 0.43 us | 1.42x |
| dictionary memory | 43.5 MB | 36.5 MB | 1.19x |
| WAV write, 60 s | 6.55 ms | 4.42 ms | 1.48x |
| WAV read, 10 min stereo | 248 ms | 178 ms | 1.40x |
| WAV read memory | 454 MB | 227 MB | 2.00x |
| transcription, 10 min | 109.3 s | 81.2 s | 1.35x |
| VAD framing, 60 s | 3.86 ms | 1.06 ms | 3.64x |
| reverse pass, 20 words | 1.29 ms | 0.06 ms | 23.4x |
| normalization, 200 sentences | 0.15 ms | 0.12 ms | 1.24x |
| binary size | 54.9 MB | 42.3 MB | 1.30x |
| start to first healthy response | 105 ms | 85 ms | 1.24x |

Three changes measured flat and are recorded as flat: the `Arc<Config>` (the route costs
285 us, the copy was a few of them), the `i64` phoneme sequence (one allocation per chunk of
eleven phonemes), and synthesis time itself (30.0 s against 29.0 s, inside a plus or minus
three percent spread).

One change was reverted after it was measured. Bounding the ONNX intra-op threads to half
the cores cost between a fifth and a quarter of the synthesis time on four cores: 36.5 s and
36.8 s on two runs against 28.7 s to 30.2 s on three runs of the older build, for the same
316 s of audio. The reasoning behind the bound, leaving room for the server's other
work, was wrong for this design: one voice synthesizes one request at a time, so the graph
is the only work in flight. The count is every core now.

## A note on the audio

The optimized graph does not produce the same bytes as the unoptimized one: the same text
gave 13.98 MB and 13.95 MB of WAV on the two builds, and repeated runs of one build vary by
as much. That is duration prediction moving a frame or two per sentence under float
reordering, not a quality change, and this pass did not score the audio against a reference.
A round trip through one recognizer would be the way to check it, and it has not been run.

## Left undone, on purpose

- **P2.2, the first-word index in `UserDictionary`.** The plan is right that matching is
  linear in the number of patterns. The shipped dictionary has three entries, so the
  change buys nothing measurable today and adds an index that has to be kept in step with
  the pattern list. It belongs with a dictionary large enough to feel it.
- **P2.3, the per-word `String` in `text_symbols`.** A reusable buffer is a real saving per
  word, but the same loop now calls a `lookup` that no longer allocates, which was the
  larger of the two costs. Recorded rather than done.
- **P2.1's `write_cardinal`.** The two fast paths are in; rewriting the cardinal writer to
  push static words into a `&mut String` touches every number path in the normalizer for a
  saving that only shows on text dense with numbers. The `Vec<char>` fast path already
  removes the cost for text with no numbers at all.
- **P2.4, the rate limiter.** The plan's own fix is a pre-sized map; the lock is held for
  four arithmetic operations. Sharding it is a change to the shape of the limiter with no
  measured cost behind it.

## What the plan claimed that the code did not need

- `wav::read_bytes` already refused a chunk past the buffer; the double buffer was real and
  is gone.
- `CmuDict::parse` did not allocate a `Vec` per line for the map itself, but it did for the
  phoneme tokens and the joined string: two per line, not one.
- The `preroll` drain is a `copy_within` now rather than a ring buffer. The window is
  bounded by `preroll_ms`, and the buffer is handed over wholesale when an utterance
  starts, which a ring buffer would make more expensive than the copy it saves.

## A third pass: the phonemizer

The bounds added with the counting allocator named two costs in `arioso-g2p` that nobody
had measured: a two-word sentence and a spelled-out word. Both sit in the path every
synthesis request walks, once per word. The counts below are from
`cargo test -p arioso-g2p --test allocations -- --test-threads=1`, which is the only way to
run them: the lock in `alloc_count` keeps two measurements apart, not the rest of the
process (see `docs/lessons.md`).

| Finding | Change | Before | After |
| --- | --- | --- | --- |
| PERF-H1 | `arpabet` uppercases a token into a four-byte stack buffer and reads a `&'static [&'static str]` from the table; `extend_ipa` appends into the caller's vector | `phonemes_for_text("hello world")` 28 allocations, 1,492 bytes; `lts::pronounce("zorbansky")` 49, 1,678 | 12, 1,093; 31, 1,231 |
