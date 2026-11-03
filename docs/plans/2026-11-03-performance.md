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
| P1.3 | ONNX at optimization level 3 with bounded intra-op threads; the tensor sequence is built as `i64` directly | `03e53b1` |
| P1.4 | `arioso say` converts each chunk to PCM as it arrives | `f31f67e` |
| P1.2 | Reverse pass rejects impossible windows by length before the edit distance, reuses its buffers, and stops a DP row that cannot come back under the allowance | `073df25` |
| P2.1 | Normalization skips the abbreviation pass without a full stop and the `Vec<char>` without a digit or `$` | `e2be260` |
| P3.1 | `[profile.release]` with thin LTO, one codegen unit, stripped symbols, `panic = "abort"` | `bd289b4` |

## Measured on this build

Release binary, `x86_64-linux`, this machine, three runs each:

- `arioso doctor` with a user dictionary present, which parses all 126,052 words of
  `cmudict.dict`: **0.07 s** wall, **57.7 MB** peak RSS.
- `arioso say` on 160 sentences (316.44 s of audio, 13.9 MB WAV): **37.9 s** wall
  (real time factor 0.12), **521 MB** peak RSS.
- Release binary size with the new profile: **42.3 MB**.

**No before-number was taken in this pass.** The changes are argued from the code —
allocations per line, buffers held at once, passes over the same data — not from a
comparison, and the numbers above are a baseline for the next pass rather than a delta.
A performance claim without a measurement of both sides is the same error as a figure
without its scope.

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
