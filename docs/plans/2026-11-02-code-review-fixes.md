# Code review fixes

A review of the whole workspace found fourteen defects: configured controls that are parsed
and then ignored, state races, and two contracts that the code does not keep. This plan
verifies each one against the tree before touching it, then fixes them in severity order.

## Verification

Every finding was checked against the code at `v0.1.3`, not taken on trust. Two were
narrowed; the rest stand as written.

| id | finding | verdict | evidence |
| --- | --- | --- | --- |
| C1 | WAV chunk size overflow and silent truncation | confirmed, narrowed | `wav.rs` advances `at += 8 + size + (size & 1)` with no check. On a 64-bit target `usize` holds the sum, so the wrap needs a 32-bit target; the silent truncation (`unwrap_or(&[])`) is reachable everywhere: a truncated `data` chunk decodes as zero samples with no error |
| C2 | warm warms the configured STT model whatever the id says | confirmed | `warm_model` checks `not_active_voice` for `Kind::Tts` and has no equivalent check before `state.warm_stt()`, which loads `config.stt.batch_model` |
| C3 | two downloads of one model can race | confirmed | `download_progress` and `note_download` take the lock separately, so two requests both see `None` and both spawn a task on the same `.part` |
| C4 | `/v1/tts/ws` ignores `cancel` | confirmed | `ws::run` awaits `receiver.recv()` only; the socket is read once, before streaming. `PRD.md` F8 and the `ApiDoc` text both promise `{"type":"cancelled"}` |
| H1 | `config.vad` ignored | confirmed | `ws_stt.rs` builds `Segmenter::new(STREAM_RATE, VadConfig::default())`; the config's thresholds never reach it. Defaults also disagree: config `0.5` (probability), VAD `0.01` (RMS) |
| H2 | delete leaves the model loaded, `.part` behind | confirmed | `delete_model` removes the manifest's files and drops nothing from `self.voice` or `self.stt` |
| H3 | `models.mirror`, `max_concurrent_downloads`, `auto_download` unused | confirmed | no reference outside `arioso-config` |
| H4 | `/readyz` always 200, ignores STT | confirmed | the handler returns `Json<ReadyView>`, which cannot carry a status code, and only the voice is examined |
| H5 | `/v1/models` hides download progress | confirmed | `ModelView` has `installed` and `bytes` only |
| H6 | `tts.max_chars` is dead | confirmed | defined in `Tts`, read nowhere; `server.limits.max_text_chars` is what the routes enforce |
| H7 | SHA-256 of a 652 MB file on a Tokio worker | confirmed | `hash_file(&part_path)` is called directly inside async `ensure_file` |
| M1 | `/v1/tts` returns uncoded errors | confirmed | five `ApiError::new` calls in `synthesize`, where every other route uses `coded` |
| M2 | reverse pass mangles ARPAbet entries | confirmed | `pronounce_phrase` splits on whitespace and pronounces each token as a word, so `S EH1 L AA0` is read as four English words |
| M3 | `/v1/stt/ws` omits `provisional` and `done` | confirmed | partials carry `text` and `t` only; `stop` flushes and breaks without a `done` frame |
| M4 | `PATCH /v1/config` cannot toggle an engine | confirmed | routes are registered at startup from `tts.enabled`; `synthesize` never re-checks it, so disabling mid-flight leaves the route live |
| M5 | `log.log_text` never read, no request id | confirmed | the field is defined and unused; words are logged at `debug` unconditionally |
| L1 | voice and STT resolution duplicated | confirmed | `arioso-cli` re-implements install checks and spec lookup that `arioso-server` also has |
| L2 | `arioso doctor` misses a catalog-id voice | confirmed | it matches `contains(voice)` on file names, so `piper-en_US-lessac-medium` matches nothing |
| L3 | comma eaten after a number | confirmed, with output | `arioso normalize 'In 2026, hello'` gives `In two thousand twenty six hello`: the comma is consumed and the year changes reading from `twenty twenty six` to `two thousand twenty six` |

The normalizer also confirms the ordinal boundary: `1stand` gives `firstand`.

## Order

Severity first, and inside a phase the changes that a later task depends on.

1. **Phase 1, safety and contracts.** C1 (parser), C2 (warm), C3 + H7 (download), C4 (cancel).
2. **Phase 2, configuration that lies.** H1 (VAD), H2 (delete), H3 (mirror and concurrency),
   H4 + H5 (readiness and catalog), H6 (one limit).
3. **Phase 3, protocol and observability.** M1 (errors and format), M2 (ARPAbet), M3 (STT
   events), M4 (runtime toggles), M5 (log text and request id).
4. **Phase 4, polish.** L3 (normalizer), L1 + L2 (dedup).

Each task is one commit that passes the whole gate: `cargo fmt --all -- --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`,
`cargo doc --no-deps`.

## Tasks

### Phase 1

- [ ] 1.1 C1: checked arithmetic in the WAV chunk walk; a chunk that runs past the buffer is
      `InvalidData`; tests for `u32::MAX` and for a truncated `data` chunk.
- [ ] 1.2 C2: refuse to warm an STT model that is not `stt.batch_model` (`409
      not_active_model`); run both warm paths on `spawn_blocking`.
- [ ] 1.3 C3 + H7: one lock for check-and-insert; a semaphore from
      `models.max_concurrent_downloads`; `hash_file` on `spawn_blocking`.
- [ ] 1.4 C4: `select!` on the socket and the render channel; `cancel` stops the render at the
      next chunk, answers `{"type":"cancelled"}`, and frees the session permit.

### Phase 2

- [ ] 2.1 H1: build the VAD config from `config.vad`, map its fields, and give the energy
      backend a default threshold that works out of the box.
- [ ] 2.2 H2: delete removes `.part` files and drops a loaded voice or STT model.
- [ ] 2.3 H3: mirror rewriting, a download semaphore, and `auto_download` at start.
- [ ] 2.4 H4 + H5: `/readyz` reports STT and answers 503 when an enabled engine is missing;
      `/v1/models` reports download state and progress.
- [ ] 2.5 H6: one limit. `tts.max_chars` is removed and an old config that carries it still
      loads.

### Phase 3

- [ ] 3.1 M1: coded errors in `synthesize`, `409 model_not_installed` instead of a 422 with a
      path, and `pcm` honoured when `tts.format` asks for it.
- [ ] 3.2 M2: an ARPAbet form is converted to unstressed IPA instead of being pronounced as
      words, with a round-trip test.
- [ ] 3.3 M3: `provisional: true` on partials, a `done` frame on `stop`, and an idle timeout.
- [ ] 3.4 M4: routes always registered; handlers check `enabled` and answer 404; the document
      is built per request.
- [ ] 3.5 M5: words logged only with `log.log_text`; `x-request-id` on every response.

### Phase 4

- [ ] 4.1 L3: a comma is a thousands separator only when exactly three digits follow it; an
      ordinal suffix must end at a word boundary.
- [ ] 4.2 L1 + L2: `ModelSpec::is_installed` and `Manifest::find_voice` in `arioso-models`,
      used by the CLI and the server; `doctor` resolves the voice through the manifest.

## What is not in scope

- `tts.format` for `mp3` and `opus`. The config allows them and nothing encodes them. 3.1
  narrows the allowed set to what exists rather than pretending.
- A model slot per id. Warming and loading stay single-slot; 1.2 makes the refusal honest
  instead of widening the design.
- Silero VAD. 2.1 wires the config into the energy segmenter, which is the only backend.
