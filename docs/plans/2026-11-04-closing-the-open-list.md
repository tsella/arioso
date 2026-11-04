# Closing the open list

> **For Hermes:** implement one task per subagent, each in its own git worktree and branch.
> The controller merges in the order given at the end of this file.

**Goal:** finish the seven items the PRD and the plans still list as open, and leave nothing
in `docs/plans/` that reads as unfinished.

**Approach:** seven independent tasks, each on its own branch, merged one at a time with the
full gate re-run after every merge. Three of them touch `arioso-server/src/lib.rs`, so the
branches conflict by construction and the controller resolves them; nothing else overlaps.

**Not in this list, and why:** the C ABI (F10) and packaging (M6) are out of scope by the
user's decision of 2026-11-04. The Windows PID ownership path stays untested because no
runner exercises a Windows service stop. The Silero VAD backend stays refused rather than
silently ignored. Per-session language stays unimplemented because one engine loads per
process.

---

## Rules every task inherits

1. Work in the worktree named in the task. Never edit, commit, or push on `main`, and never
   push a branch: the controller merges.
2. Before every commit run all four, in this order, as separate commands, and read their exit
   codes rather than grepping the output for a count:
   `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
   `env -u ARIOSO_MODELS_DIR -u ARIOSO_TTS_DICTIONARY -u ARIOSO_TTS_VOICE cargo test --workspace`,
   `cargo doc --no-deps`.
3. Set `CARGO_TARGET_DIR=/home/tsella/projects/arioso-wt/target` so the artefacts are shared
   between worktrees. Cargo takes a lock, so a queued build waits instead of corrupting.
   Expect the first build in a wave to be slow and the rest to be warm.
4. One concern per commit, conventional commit subject, message written to
   `/tmp/arioso-<task>.txt` and passed with `git commit -F`. Read `git log --oneline -1` after
   every commit.
5. Any change to an observable contract updates `PRD.md` in the same commit: a new route in
   the route table, a new `F` requirement, a default that moved.
6. Never write the operator's private environment into this repository: no other project's
   name, no ports those projects use, no home paths, no hostnames, no health data. State the
   reason for a decision without naming the machine.
7. A test that needs a model and returns early when it is absent is decoration. Mark it
   `#[ignore = "needs a real model in ARIOSO_MODELS_DIR"]`, `expect()` the model, and say in
   the commit that the proof ran with `--ignored`.
8. If the work turns out to need something this plan did not foresee, stop and report what
   blocked it. A wrong implementation is worse than an honest blocker.

---

## T1 — Server-sent events for model downloads

**Objective:** a client can watch every download without polling `GET /v1/models`.

**Files:**
- Modify: `crates/arioso-server/src/lib.rs` (state, router, handler)
- Modify: `PRD.md` (route table and a new `F` requirement)
- Modify: `AGENTS.md` (route table), `IMPLEMENTATION.md` (a snippet)

**Design:**
- `AppState` gains `events: tokio::sync::broadcast::Sender<ModelEvent>`.
- `ModelEvent { id, state, downloaded, total, error }` — the same fields `GET /v1/models`
  already reports, so a client parses one shape for both.
- The downloader's progress callback publishes; `broadcast` capacity 256, and a subscriber
  that falls behind gets `RecvError::Lagged` and must re-read the snapshot rather than
  silently missing events.
- `GET /v1/models/events` answers `text/event-stream`: first a `snapshot` event carrying every
  model's current state, then `progress` and `state` events, then a comment line every 15 s so
  a proxy does not close an idle stream.
- Auth applies as for every other route, and the rate limiter applies: a stream is one request.
- Register the route unconditionally, like every other route.

**Steps:**
1. Failing test: subscribe, start a download of a model whose files are empty placeholders,
   assert the first event is `snapshot`, that a `state` event reports `downloading`, and that a
   later event reports `installed`.
2. Failing test: two subscribers both receive the same events.
3. Failing test: a subscriber that drops does not break the download, and the next subscriber
   still gets a snapshot.
4. Implement until green.
5. Update the documents and the route table, and the OpenAPI document if the route is
   documented (`utoipa` or a hand-written entry — follow what the other routes do).

**Verification:** `curl -N http://127.0.0.1:<port>/v1/models/events` against a running server
prints a snapshot then progress as a small model downloads; paste the real output into the
commit body. Leave no server running.

---

## T2 — The manifest signature is enforced where it is loaded

**Objective:** a manifest whose signature does not verify is refused before any bytes are
downloaded.

**Files:**
- Modify: `crates/arioso-models/src/manifest.rs`
- Modify: `crates/arioso-cli/src/main.rs` (`doctor` reports the signature state)
- Modify: `PRD.md`

**Design:**
- `RELEASE_PUBKEY_HEX` stays empty. That is a fact about the release process, not a bug.
- New `Manifest::load_checked(path, signature_path, pubkey_hex) -> Result<Manifest, ManifestError>`
  with three outcomes: no key compiled in, load and report `unsigned`; key present and no
  signature file, refuse; key present and the signature does not verify, refuse with
  `BadSignature` naming the file.
- `Manifest::load` keeps its current signature and calls the checked path with the compiled
  key and the sibling `<path>.sig`, so every existing caller gains the check without changing.
- The downloader verifies the manifest it fetched before replacing the cached copy, and a
  failure leaves the previous copy in place.

**Steps:**
1. Failing test: generate a keypair, sign a manifest, `load_checked` accepts it.
2. Failing test: flip one byte of the document, `load_checked` refuses with `BadSignature`.
3. Failing test: a key is supplied and no `.sig` exists, refusal names the missing file.
4. Failing test: no key supplied, the manifest loads and the outcome says `unsigned`.
5. Implement, then wire `doctor` to print which of the three states applies.

**Verification:** the four tests, plus `arioso doctor` printing the signature state for a
models directory that has a manifest and no signature.

---

## T3 — `arioso doctor` reports the certificate

**Objective:** an expiring certificate is visible before it breaks a client.

**Files:**
- Modify: `crates/arioso-cli/src/main.rs`
- Modify: `crates/arioso-cli/Cargo.toml` (one parser dependency)
- Modify: `PRD.md` (requirement 4.1.5)

**Design:**
- Dependency: `x509-parser` or `x509-cert`. No OpenSSL, no C toolchain: the CI matrix has no
  openssl headers on Windows.
- When `server.tls.cert` is set, `doctor` prints the subject common name, the not-before and
  not-after dates, and the days remaining.
- Expired is a problem (non-zero exit). Fewer than 30 days is a warning that does not change
  the exit code, unless `--strict` is passed.
- A file that cannot be read, or is not a certificate, is a problem with the path named.
- Absolute dates, not a relative "in 12 days" only: an operator reading a log in six months
  needs the date.

**Steps:**
1. Add `rcgen` as a dev-dependency and generate three certificates in the test: valid for a
   year, valid for ten days, already expired.
2. Failing tests for the three verdicts, and for a file that is not a certificate.
3. Implement the parse and the three verdicts.
4. Print the block in `doctor` where the server settings are already reported.

**Verification:** `arioso doctor` against a config naming a generated certificate prints the
dates and the verdict; paste it.

---

## T4 — Fuzzing runs on a schedule

**Objective:** the four fuzz targets run every night, and no pull request can let them rot.

**Files:**
- Create: `.github/workflows/fuzz.yml`
- Modify: `.github/workflows/ci.yml` (a build-only step, so a target that stops compiling
  fails the pull request)
- Create: `fuzz/corpus/<target>/` seed files where none exist
- Modify: `docs/plans/2026-11-03-residual-fixes.md` (mark the item closed)

**Design:**
- `on: schedule: cron '17 3 * * *'` and `workflow_dispatch`, never on push: a fuzz run is
  minutes of CPU and a pull request does not need it.
- Matrix over the four targets: `config_json`, `dictionary`, `manifest`, `wav_header`.
- Each job: `cargo install cargo-fuzz`, `rustup toolchain install nightly`, then
  `cargo +nightly fuzz run <target> -- -max_total_time=120 -rss_limit_mb=4096`.
- On failure, upload `fuzz/artifacts/<target>/` as an artefact and fail the job. A crash that
  is not kept is a crash that happens again.
- The pull request step is `cargo +nightly fuzz build` only, no run.

**Verification:** dispatch the workflow on the branch with `gh workflow run fuzz.yml --ref
<branch>`, wait for the run, and report the conclusion of all four jobs. A workflow that has
never executed is a guess.

**Status:** done. `.github/workflows/fuzz.yml` runs the four targets nightly at 03:17 UTC and
on dispatch; `ci.yml` builds them on every pull request. The YAML parses, `actionlint` passes,
and `cargo +nightly fuzz build` compiles all four targets locally. The dispatch itself is the
controller's, because a workflow can only be dispatched from a branch that exists on the
remote.

---

## T5 — Kokoro as a second speech engine

**Objective:** `tts.engine = "kokoro"` synthesizes speech instead of being refused.

**Files:**
- Create: `crates/arioso-tts-kokoro/` (`Cargo.toml`, `src/lib.rs`)
- Modify: `Cargo.toml` (workspace member and dependency)
- Modify: `crates/arioso-core/src/` (the `TtsEngine` trait)
- Modify: `crates/arioso-config/src/lib.rs` (`tts.engine` accepts both, validation names both)
- Modify: `crates/arioso-server/src/lib.rs` (the voice slot holds either engine)
- Modify: `crates/arioso-cli/src/main.rs` (`say`, `doctor`)
- Modify: `models/manifest.json` (the Kokoro entry), `MODELS.md`, `PRD.md`

**Design:**
- `TtsEngine` in `arioso-core` is the minimum a second implementation needs:
  `fn sample_rate(&self) -> u32`, `fn render(&self, symbols: &[i64], speed: f32) -> Result<Vec<f32>, EngineError>`.
  Do not invent a wider trait than the two engines need.
- Kokoro takes a phoneme symbol sequence and a voice style vector; the `voices-v1.0.bin` file
  holds one style per voice. Voice selection maps `tts.voice` onto a style index, and an
  unknown name is a client error naming the ones that exist.
- `ort` is already a workspace dependency, so the session code follows the Piper crate.
- The manifest entry carries the real `sha256`, `size`, `sample_rate`, and the licence from
  the model card. **If you cannot download the model and hash it, stop and report that**: a
  made-up hash is worse than an absent entry.
- One engine loads per process, chosen by `tts.engine`. Switching engines needs a restart, and
  the refusal says so.

**Steps:**
1. The trait, with the Piper crate implementing it and every existing call site unchanged.
2. The config accepts both names and validation names the one that was refused.
3. The Kokoro crate: load, tokenize through the existing g2p path, render, return samples.
4. The server's single voice slot becomes an enum over the two engines.
5. An `#[ignore]`d end-to-end test that renders a sentence and asserts a plausible duration.
6. Manifest entry from the measured file, `MODELS.md` row with languages and licence.

**Verification:** `ARIOSO_MODELS_DIR=<dir> cargo test -p arioso-tts-kokoro -- --ignored`
renders audio, and `arioso say "hello" --engine kokoro` writes a WAV whose duration you quote.
If the model cannot be fetched in this environment, report exactly that and leave the manifest
entry out.

**Closed:** `tts.engine = "kokoro"` speaks. `arioso-tts-kokoro` loads the graph, the
`voices-v1.0.bin` archive and the model's `config.json` symbol table; the archive is a
numpy `.npz`, indexed and read one style at a time. `TtsEngine` in `arioso-core` is the
two-method contract, Piper implements it, and the server's voice slot is an enum over the
two engines chosen by `tts.engine`. `/v1/voices` lists the 54 styles of an installed
archive and `PATCH /v1/config` that changes the engine drops the slot, so the change takes
effect on the next request instead of needing a restart. Phonemization is `arioso-g2p`:
no espeak, no GPL dependency, `gpl-espeak` untouched. The catalog entry was written from
the downloaded bytes — `sha256 7d5df8ec…a6c5` for the 325,532,387 byte graph,
`bca610b8…bf7d` for the 28,214,398 byte archive and `5abb01e2…c17f` for the 2,351 byte
config. A real `arioso say --engine kokoro` wrote 6.80 s of audio at 24 kHz (326,444
bytes), and the ignored test renders the same sentence. RTF was measured on a shared
machine and is an upper bound, as `MODELS.md` says.

---

## T6 — Streaming Zipformer on the speech-to-text socket

**Objective:** a streaming model produces partials from its own state instead of re-running
the batch engine over the growing buffer.

**Files:**
- Create: `crates/arioso-stt-sherpa/src/streaming.rs`
- Modify: `crates/arioso-stt-sherpa/src/lib.rs` (family detection already exists; add the
  streaming path)
- Modify: `crates/arioso-server/src/lib.rs` (the WebSocket handler chooses the path)
- Modify: `models/manifest.json` (one streaming entry), `PRD.md`, `docs/plans/`

**Design:**
- `sherpa-onnx 2.0.1` exposes `OnlineRecognizer`, `OnlineRecognizerConfig` and `OnlineStream`;
  `is_ready`, `decode` and `get_result` are the three calls the loop needs.
- The wire protocol does not change: `partial` and `final` frames, the same fields, the same
  rounding. The transcriber must not need a release.
- A batch-only model keeps the current path, and `/v1/info` says which path an engine uses so
  a caller is not guessing.
- A streaming model that is not installed is `409 model_not_installed`, as with the batch path.

**Steps:**
1. Read the binding's `online_asr.rs` first and write down the exact API in the commit body.
2. Failing test for family detection: an entry with `meta.model_type: "zipformer_streaming"`
   selects the streaming path; `nemo_transducer` still selects batch.
3. Implement the stream loop: feed frames, decode while `is_ready`, emit a partial when the
   text changes, emit a final on `stop`.
4. An `#[ignore]`d test with a real streaming model.

**Verification:** a real socket session against an installed streaming model prints partials
then one final; quote the frames. If no streaming model can be installed here, say so and
leave the test `#[ignore]`d with the command that runs it.

**Closed:** the streaming loop lives in `crates/arioso-stt-sherpa/src/streaming.rs` and is
chosen from `meta.model_type`; `stt.stream_model` names the model the socket uses and an
unset one leaves the socket on the batch path. A session against
`sherpa-zipformer-streaming-en` printed twelve partials and one final over the same frames
the client already parses, and `GET /v1/info` reports `stt_path`. The ignored test runs with
`ARIOSO_MODELS_DIR=<dir> cargo test -p arioso-stt-sherpa -- --ignored`.

---

## T7 — The iOS and Android targets are compiled

**Objective:** the crates that have no native dependency are known to compile for both mobile
targets, and the ones that cannot say why.

**Files:**
- Create: `.github/workflows/mobile.yml`
- Modify: `README.md` (the platforms badge names what CI actually builds)
- Modify: `PRD.md` (requirement 5.1.x for the mobile build)

**Design:**
- Two jobs. `ios`: macOS runner, `rustup target add aarch64-apple-ios`, then
  `cargo check -p arioso-config -p arioso-models -p arioso-g2p -p arioso-core -p arioso-vad
  --target aarch64-apple-ios`. `android`: Ubuntu runner, the NDK is preinstalled on the
  runner image, `rustup target add aarch64-linux-android`, the same crate list.
- The engine crates are excluded on purpose: `ort` and `sherpa-onnx` need a native build per
  target and neither is wired here. Write that in the workflow as a comment and in the PRD as
  a requirement that names the gap, so the exclusion reads as a decision.
- No `continue-on-error`. A job that cannot fail is not a check.

**Verification:** dispatch the workflow on the branch and report both conclusions. If a crate
in the list fails to compile, the finding is the deliverable: report which crate and why
rather than removing it from the list.

---

## Order of work and merges

Wave 1, no shared files, dispatch together: T2 (models), T3 (cli), T4 (workflows), T7 (workflows).
T4 and T7 both touch `.github/workflows/`, but different files; the controller merges T4 first.

Wave 2, both touch the server: T1 (routes and state), T6 (stt path).

Wave 3, the largest and the one most likely to conflict: T5 (config, core, server, cli, manifest).

After every merge the controller runs the gate on `main` and pushes only when it is green. A
branch that conflicts is rebased onto the new `main` by the controller, not by the task.

## Open defect: Windows cannot open a speech graph

Surfaced by the first test that asks the runtime to load anything, which arrived with T6.

`cargo test -p arioso-stt-sherpa` on `windows-latest` prints
`The requested API version [28] is not available, only API versions [1, 17] are supported in
this build` and aborts the process inside C++, so the whole test binary dies and no other test
in it runs. The C API version belongs to `onnxruntime`, and the caller is sherpa's prebuilt
library, so the two disagree about which runtime they are talking to.

Two candidate causes, neither verified without a Windows machine:

1. The prebuilt that `sherpa-onnx-sys` downloads on Windows bundles or expects a different
   `onnxruntime.dll` from the one it finds. The CI step that deletes `target/sherpa-onnx-prebuilt`
   forces a fresh download, so a stale cache is not the explanation for a first run.
2. The workspace builds `ort` for the Piper engine into the same `target/debug`, and cargo
   places its `onnxruntime.dll` beside the test binaries. A DLL search that finds that one
   would explain a version the speech library never expected.

Until this is settled, the STT engine is unproven on Windows, not merely untested: the matrix
has never opened a graph there. The one test that does is quarantined on Windows with the
reason in its attribute, and this entry is the record.

Next step: a Windows runner that prints `dumpbin /dependents` for the speech library, the
version of every `onnxruntime.dll` in `target/debug` and `target/debug/deps`, and which one the
loader picked. That is one workflow run, not a guess.

## Done means

1. `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test --workspace` and `cargo doc --no-deps` all exit 0 on `main`.
2. CI green on Ubuntu, macOS and Windows.
3. Every item above appears in `PRD.md` as a requirement, and `docs/plans/` no longer lists it
   as open.
4. The test count in `README.md` matches the gate log.
