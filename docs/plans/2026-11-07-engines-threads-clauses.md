# Plan: four engines, two engines at a time, and clause-sized audio

The request, in the order the parts depend on each other: two more text-to-speech engines
(Supertonic, BlueTTS), a MODELS.md that compares all four, speech to text that does not wait
behind speech synthesis, a thread count per engine with a defensible default, and audio chunked
by clause instead of by sentence.

## What is true today

- `render_stream` holds two locks for the whole synthesis: the voice slot, and **the user
  dictionary**. `transcribe` needs the user dictionary too, for the phonetic correction. So a
  transcription submitted during a long render waits for it, and a render submitted during a
  transcription waits for that. This is the defect behind "tts and stt should not block each
  other", and it is one lock, not a design.
- Both engines take a `threads` argument, but nothing in the config sets it: the server computes
  half the cores, clamped 1..8, and that is the only answer available.
- Both engines split text with `sentences()`, then cap each sentence at `SYNTHESIS_CHUNK`
  (300 characters). The first audio therefore leaves only after a whole sentence has been read,
  which on a long sentence is the whole latency.
- Two engines exist: Piper (VITS, phonemes from CMUdict plus letter-to-sound) and Kokoro
  (one archive, 54 styles, IPA from the same pipeline).

## Phase 0 — nothing waits for the other engine

The user dictionary is reloaded when its file changes and is read by both routes. Change the
stored values to `Arc` and hand out a snapshot:

```rust
struct UserState { path: PathBuf, modified: Option<SystemTime>, dictionary: Arc<UserDictionary>,
                   reverse: Arc<ReverseIndex>, error: Option<String> }

fn user_snapshot(&self) -> UserSnapshot  // locks, reloads if the file changed, clones two Arcs
```

`render_stream` and `correct` take the snapshot before they start and drop the lock in
milliseconds. The voice lock stays held for the render, which is correct: one graph, one voice,
and a second render would only queue behind it inside ONNX anyway.

Acceptance: a test starts a render of a long document on one thread, then calls
`state.user_snapshot()` from another with a one-second budget. It passes today only if the
render is short, which is why the test uses a document long enough that it cannot be.

## Phase 1 — threads per engine, with a default that has a reason

Config gains `tts.threads` and `stt.threads`, `0` meaning "decide":

- **TTS default: every core, up to 8.** The first draft said 4 and the measurement disagreed:
  a 16.1 s Piper render on four cores took 4.10 s at one thread, 3.12 s at two, 2.90 s at
  three and 2.87 s at four. The curve flattens at three but never turns, so the ceiling is
  about a large host rather than about this one. Above 8 the graph spends more time at the
  barrier than in the kernels, and the process may be transcribing at the same time.
- **STT default: half the cores, clamped 1..8.** A transducer decodes one long sequence, so it
  benefits from threads in a way synthesis does not. This is what the server already does; it
  becomes the documented default rather than an accident in a helper.
- Neither default may take every core: a process that hands all cores to one engine has already
  made the other wait, which is the thing Phase 0 is about.

Validation: `threads <= 64`, and a value above the core count is accepted with a warning rather
than refused — a container's reported core count is not the host's.

## Phase 2 — clauses, not sentences

`arioso_core::chunk::clauses(text)` cuts after `,` `;` `:` and a dash, and after the sentence
terminators the existing splitter already honours. A clause shorter than a floor is merged with
the next one, so a list of three-word clauses does not become three synthesis calls with a pause
between each:

```rust
pub fn clauses(text: &str) -> Vec<&str>   // slices, concatenation returns the input exactly
```

Both engines iterate `clauses(text)` and keep the `chunk_text(clause, SYNTHESIS_CHUNK)` cap, so
nothing that exists becomes unbounded. The streamed unit becomes a clause: the first audio
leaves after the first comma instead of after the first full stop.

The risk is audible: a cut at a comma is a pause the voice did not intend. The floor
(`CLAUSE_FLOOR = 60` characters) exists for that reason, and the plan's own test is a
before/after listen on the same paragraph, recorded in the plan doc.

## Phase 3 — Supertonic

Supertone's model is ONNX and runs on CPU, which is the shape this project already drives. A new
crate `arioso-tts-supertonic` with the same surface as the other two:

```rust
pub struct Voice;                  // loaded from the catalog's files
pub fn load(files: &VoiceFiles, threads: u32) -> Result<Self, SupertonicError>;
pub fn render(&mut self, text: &str, dictionary: &CmuDict, user: &UserDictionary,
              options: RenderOptions, sink: impl FnMut(&[f32], u32) -> Result<(), _>) -> Result<RenderReport, _>;
```

What the research has to settle before any of it is written: the exact file set and sizes, the
input and output tensor names and shapes, how text becomes ids (its own tokenizer, or phonemes
through the existing pipeline), and how a voice or style is chosen. If the pipeline needs a
Python step, the crate does not ship and the reason is recorded — a stub that loads a graph and
produces noise is worse than an absent engine.

## Phase 4 — BlueTTS

Same surface, same rule. BlueTTS is flow matching with an external classifier and a per-voice
style file, exported to ONNX by its own repository. The question the research answers is whether
the sampling loop is expressible in Rust over `ort` or whether it is a Python driver around the
graph. If it is the second, BlueTTS goes on the roadmap with the measurement that decided it,
not into the catalog as a voice that cannot speak.

## Phase 5 — MODELS.md compares four

One table: engine, voice, licence, download, real time factor, memory, quality where a
measurement exists. A row with no measurement says so; a row that was never run is not a row.

## Review of this plan, before any of it was written

Three things the review changed:

1. **The thread default was a guess.** "4 is right for synthesis" came from the shape of the
   work, not from a stopwatch. Measured, it is 2 on this machine for 91 % of the gain and the
   curve is flat after 3. The plan now carries the numbers and the default follows them.
2. **Phase 0 was one lock, not a redesign.** The draft talked about separating the engines'
   executors; reading the code showed both routes already run on `spawn_blocking` and hold
   separate slots. The only shared mutable state was the user dictionary, held for the length
   of a render. A trait or a worker pool would have been a large change for a defect with a
   four-line cause.
3. **Clause splitting needed a floor.** Without it, a sentence of short phrases becomes several
   synthesis calls and the voice pauses where the text did not. `CLAUSE_FLOOR` is in the plan
   because the first version of the splitter did not have it.

What the review could not settle: whether Supertonic and BlueTTS are expressible in Rust over
`ort` at all. That is Phase 3 and 4, and it depends on their graphs, which are being read now.

## Order and gates

0 → 1 → 2 land first, because they change code that exists and can be tested on this machine.
3 and 4 land only as far as the graphs allow, each behind the same gate: fmt, clippy, tests,
docs. Every phase gets its own commit; the config change and the PRD change go with the code
that makes them true.
