# AGENTS.md

If you want to use this library or binary rather than change it, read
[IMPLEMENTATION.md](IMPLEMENTATION.md) first. This file is for working on the repository.

Guidance for an agent working in this repository. Read this file before you change code.

## What arioso is

arioso is a Rust workspace that gives other programs local speech: text to speech, and
speech to text. It has two faces. It is a library other applications link, and it is a
binary that serves an HTTP API on the local machine.

It runs offline. No hosted service is called at run time. A downloaded model is the only
network traffic, and only when someone asks for it.

What it is not: a cloud service, a client for a hosted TTS vendor, or a GUI application.
Text to speech uses Piper voices. Speech to text uses sherpa-onnx with Whisper models.
Both engines run in process.

## Quickstart

```bash
cargo build --release
./target/release/arioso models list                 # what the catalog offers
./target/release/arioso models download piper-<voice>
./target/release/arioso say "Hello from arioso" --out hello.wav
./target/release/arioso transcribe hello.wav
./target/release/arioso serve                       # HTTP on 127.0.0.1:10101
```

A build needs a native ONNX Runtime. Put release 1.22.0 at `vendor/onnxruntime`. CI does
the same. Without it, `arioso-tts-piper` fails to link.

## Crates

| crate | owns |
| --- | --- |
| `arioso-config` | the config file, its schema, every validation rule |
| `arioso-models` | the model catalog, the manifest, resumable downloads |
| `arioso-core` | text normalization, sentence splitting, one written form to several spoken forms |
| `arioso-g2p` | grapheme to phoneme: the dictionary, then the letter-to-sound rules |
| `arioso-tts-piper` | the Piper engine, synthesis, WAV output |
| `arioso-stt-sherpa` | the sherpa-onnx engine, transcription |
| `arioso-vad` | voice activity detection for streaming |
| `arioso-server` | the HTTP and WebSocket routes, auth, TLS, limits |
| `arioso-cli` | the binary: commands, sidecar, PID files, logging setup |

Dependency direction is downward: `arioso-cli` and `arioso-server` at the top,
`arioso-core` and `arioso-config` near the bottom. A new engine crate depends on nothing
above it.

## The gate

Every change passes this before it is committed. Check the process exit code, not a grep.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
cargo doc --no-deps
```

Integration tests need a models directory. Pass it, and clear the inherited variables,
because a stale one silently changes what the test reads:

```bash
env -u ARIOSO_MODELS_DIR -u ARIOSO_TTS_DICTIONARY -u ARIOSO_TTS_VOICE \
    ARIOSO_TEST_MODELS_DIR=<models dir> cargo test --workspace
```

## Config

The file is JSON. `arioso config schema` prints its schema. `config.json.example` is the
annotated default. The default install path is the platform config directory; `--config`
names a file.

Fields that matter most:

- `server.bind`: an IP address or `localhost`. Default `127.0.0.1:10101`.
- `server.sidecar`: `true` makes `serve` detach and print `START {port}` on stdout.
- `server.auth`: `none` on loopback, `bearer` beyond it. A non-loopback bind without a
  token is refused at load.
- `server.docs`: serves `/openapi.json` and `/docs` when true.
- `log.level`: trace, debug, info, warn, error. Default `error`.
- `tts.voice`, `tts.speed`, `tts.dictionary`, `tts.time_of_day`: what the voice says.
- `models.dir`: where model files live.

Environment variables override the file: `ARIOSO_MODELS_DIR`, `ARIOSO_TTS_VOICE`,
`ARIOSO_TTS_SPEED`, `ARIOSO_TTS_TIME_OF_DAY`, `ARIOSO_TTS_DICTIONARY`,
`ARIOSO_SERVER_BIND`.

## HTTP API

Loopback by default. `GET /healthz` needs no token; everything else follows
`server.auth`.

| method | path | purpose |
| --- | --- | --- |
| GET | `/healthz`, `/readyz` | liveness, readiness |
| GET | `/v1/info`, `/v1/engines`, `/v1/voices` | what this server has |
| GET, PATCH | `/v1/config` | read and change settings |
| GET | `/v1/models` | catalog with install state |
| GET | `/v1/models/events` | the same state as it changes, as server-sent events |
| POST | `/v1/models/{id}/download` | fetch a model, 202, in a task |
| POST | `/v1/models/{id}/warm` | load it before the first request |
| DELETE | `/v1/models/{id}` | remove its files |
| POST | `/v1/tts` | text in, audio out |
| POST | `/v1/stt` | audio in, text out |
| WS | `/v1/tts/ws`, `/v1/stt/ws` | streaming |
| POST | `/v1/stop`, `/v1/abort` | graceful stop, immediate stop |
| GET | `/openapi.json`, `/docs` | the document, when `server.docs` |

## Recipes

**Add a route.** Write the handler in `crates/arioso-server/src/lib.rs` with a
`#[utoipa::path]` block, add it to `routes()`, and add it to the `paths(...)` list in
`ApiDoc`. Add a test in the same module using the `router(state)` and `call()` helpers.
An error replies with the problem document (`ApiError::coded`), never a bare string.

**Add a config field.** Add it to the struct, give the struct `#[serde(default)]` so an
older config still loads, add a validation rule with a message that names the JSON path,
extend `config.json.example`, and add a test for the default and for a bad value.

**Add a model.** Add an entry to the manifest with id, engine, kind, language, files and
sizes. The catalog and the downloader read the manifest; nothing else lists files. A speech
model also carries `meta.model_type`, which decides the graph: `whisper`, a transducer such as
`nemo_transducer` or `zipformer`, or `zipformer_streaming` for an export built for online
decoding. A streaming entry is loaded once and decoded as audio arrives, and `stt.stream_model`
names it; leave that empty and the socket stays on `stt.batch_model`. Hash the files you
actually downloaded, not the sizes in the model card.

**Change how text is spoken.** Normalization lives in `arioso-core`, phonemes in
`arioso-g2p`. Dictionary entries win over rules. One written form may map to several
spoken forms, tried in order. Whatever you change, add a test with the exact sentence and
the exact expected phonemes.

**Add an engine.** New crate implementing the same trait as `arioso-tts-piper`, wire it
into `arioso-server` behind a config value, keep the license permissive or gate it behind
a feature. Add the model entries.

## Submitting a pull request

The remote is named `upstream` (github.com/tsella/arioso), and `main` is the branch.
`origin` does not exist here; a command that says `origin` is a copy from another
repository.

```bash
git switch -c fix/pid-file-lock        # a name that says what the change does
# … change, test, commit …
cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings
cargo test --workspace
git push upstream HEAD:refs/heads/fix/pid-file-lock
gh pr create --repo tsella/arioso --base main \
  --head fix/pid-file-lock \
  --title "fix(cli): lock the PID file before emptying it" \
  --body-file /tmp/pr.md
```

Before you push, rebase onto the current `main` and make the history readable: one commit
per logical change, each of which builds. A reviewer reads commits, not only the final
tree.

The body answers four questions, in this order:

1. **What changed.** The behaviour a user sees, in one paragraph.
2. **Why.** The defect or the need. A test that failed, an output that was wrong, an issue.
3. **How it was verified.** The commands you ran and what they printed. Not "tests pass";
   the command and its result, including the live call when the change is a route.
4. **What is not covered.** The gap you know about, stated plainly. A reviewer who finds it
   themselves trusts the rest less.

Keep one behaviour per pull request. A PR that fixes a bug and renames a module cannot be
reviewed as either.

Never rewrite published history, force-push `main`, or amend a commit that others may have
pulled, without asking first. The gate is a local promise; CI repeats it on three operating
systems, and a red CI run is answered by reading the failing job, not by re-running it.

The same rules about this machine's internals apply to a PR: no host names, no ports of
other services, no home paths, no personal data in the title, the body, or the diff.

## Rules that are not negotiable here

1. **Prove it with a run.** A test that has not run, or a route you have not called, is
   not done. Rebuild the binary before a live check: `cargo build -p arioso-cli` after a
   server change, or you will test the old router.
2. **Never invent a number.** No placeholder measurements, no made-up benchmarks, no
   plausible latency. If a value is not measured, write that it is not measured.
3. **Keep the machine's internals out of the repository.** No host names, other project
   names, ports, home paths, or personal data in code, comments, docs, commits, or
   history. State the reason without the private fact.
4. **Docs match the code in the same commit.** `PRD.md` holds requirements,
   `docs/plans/` holds tasks with ticks, `EXAMPLES.md` holds measured transcripts,
   `MODELS.md` holds the measurements behind the model defaults,
   `docs/lessons.md` holds what a change taught, `docs/security.md` holds the threat
   notes. A behaviour change that is not in the docs is not finished.
5. **Conventional commits, one logical change each.** `feat`, `fix`, `docs`, `chore`,
   `test`, `refactor`, `perf`. The message says why, not only what.
6. **Warnings are errors.** The workspace denies warnings; there is no `#[allow]` without
   a written reason.
7. **stdout belongs to the caller in sidecar mode.** `START {port}` is the only line on
   stdout. Logs go to stderr at all levels.

## Pitfalls that have already cost time

- **A new config field is a breaking change** unless the struct carries
  `#[serde(default)]`. `Limits` gained one and every earlier config stopped loading.
- **A guard that skips what it cannot parse is not a guard.** Write
  `let addr = bind_address(bind)?`, not `if let Ok(addr) = bind.parse()`.
- **`localhost` is `127.0.0.1`** and is resolved before any rule runs.
- **A PID file without a lock is a number, not a fact.** The file is held under
  `flock`; a file that can still be locked has no owner and is removed, not signalled.
- **Truncate the PID file after the lock,** never at open, or a racing process empties
  the winner's file.
- **A `&mut dyn FnMut` cannot cross a thread boundary.** A spawned task needs
  `F: FnMut(..) + Send`.
- **An id moved into a spawned task needs a clone** for the response that follows.
- **Print the port only after the socket is bound and the PID file is claimed.**
- **The sherpa build script caches by directory existence.** If linking fails with a
  missing static library, delete `target/sherpa-onnx-prebuilt` and the crate's build
  fingerprint.
- **Test models directories leak between runs.** Clear `ARIOSO_*` before the suite.

## Where the truth lives

`PRD.md` for what the product promises and in what order. `docs/plans/` for the task
list, ticked as work lands. `examples.md` for how the API behaves in a real call, with
transcripts. `docs/lessons.md` for the traps. `docs/security.md` for the exposure
decisions and the fuzz results. `openapi.json` when `server.docs` is on.

If this file and the code disagree, the code is right and this file is a bug: fix it in
the same commit.
