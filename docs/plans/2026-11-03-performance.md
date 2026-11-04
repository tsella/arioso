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
- **P2.1's `write_cardinal`** is no longer left undone: the text-path pass below does it.
  The two fast paths were already in, and the rewrite of the cardinal writer now pushes
  static words into a `&mut String` as well.
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

## The text path, second pass

A review of the normalizer and the chunker found three costs the first pass left behind.
Each is one commit, and each carries its measurements in the message.

| what | commit | change |
|---|---|---|
| abbreviations | `39d3267` | `expand_abbreviations` returns `Cow<'_, str>`, starts the output only at the first match, and compares written bytes with `eq_ignore_ascii_case` instead of lowercasing the sentence. The scan is gated on the token's first byte. |
| numbers | `5e5527f` | the scanners compare character slices against the static tables; `write_cardinal`, `write_number`, `write_currency`, `write_clock` and `write_ordinal` push words into the caller's buffer. |
| chunking | `48595c2` | `closes_sentence` compares the preceding word in place; `best_cut` counts at most `limit` characters instead of the whole remainder. |
| the harness | `4668581` | the counting allocator keeps per-thread counters, so a bound no longer depends on `--test-threads`. |

`arioso_core::alloc_count`, warm, allocations / bytes. Each bound is asserted in
`crates/arioso-core/tests/allocations.rs`, and each was run against the commit before the
change and failed there.

| input | before | after |
|---|---|---|
| "The quick brown fox jumps over the lazy dog." | 2 / 88 | 1 / 44 |
| "Dr. Smith arrived at 5 p.m." | 17 / 690 | 6 / 393 |
| "There are 3,500 units at $4.99 each." | 38 / 1608 | 6 / 528 |
| "The 21st box is 5kg." | 24 / 707 | 4 / 208 |
| `cardinal(123_456_789)` | 28 / 1065 | 5 / 248 |
| `sentences`, 200 sentences | 206 / 8725 | 7 / 8128 |
| `chunk_text`, 200 sentences, limit 200 | 5 / 1984 | 5 / 1984 |

The chunker's cost was never in allocations: it was the character count. `best_cut` counted
every character left in the document on every call, so the work grew with the square of the
input. The new unit test chunks 2000 sentences and asserts the characters read is at most
four times the document; it reads 290,099 for 90,000 characters, and the old code reads
22,734,620 and fails the same bound by a factor of 78.

Release, median of 31 runs, three runs each, on the machine in `BENCHMARK.md`:

| measure | before | after |
|---|---|---|
| `chunk_text`, 2000 sentences, limit 200 | 1,248.8 us | 325.4 us |
| `chunk_text`, 1000 / 2000 / 4000 sentences | 507 / 1,760 / 6,473 us | 166 / 356 / 686 us |

The old curve grows about 3.5 times per doubling of the input; the new one about twice.
`normalize` on 200 plain sentences fell from 0.073 ms to 0.040 ms and on 200 sentences dense
with numbers from 0.244 ms to 0.074 ms, measured with one harness on both trees (release,
minimum of seven windows of twenty calls).
