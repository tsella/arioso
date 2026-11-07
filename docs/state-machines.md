# State machines

The states the server, the engines, the sockets and the terminal front end actually move
between, the invariants that hold in each, and the transitions that are reachable but wrong.

Two levels of claim appear here and they are marked:

- **tested** — a test in this repository exercises the transition.
- **read** — the transition was traced through the code and no test pins it. It is a claim
  about the code as it stands, not about behaviour under load.

A machine listed as *read* is not weaker evidence for the table being right; it is weaker
evidence for it staying right. The last section says which test holds which row.

## A. Server process

`AppState::handle` is `Some` exactly while an accept loop exists. An embedded router has no
handle, and that is the whole reason the guard exists.

| state | meaning | entry |
| --- | --- | --- |
| `Serving` | handle set, connections accepted | `serve()` installed the handle |
| `Stopping` | handle set, no new connections, in-flight finishing | `POST /v1/stop` |
| `Ended` | process gone | grace expired, `/v1/abort`, or a signal |

| from | event | to | guard | status |
| --- | --- | --- | --- | --- |
| `Serving` | `POST /v1/stop` | `Stopping` | handle is `Some`, else 409 `no_accept_loop` | tested |
| `Serving` | `POST /v1/abort` | `Ended` | same guard; answers, then exits after 50 ms | read |
| `Stopping` | `POST /v1/stop` | `Stopping` | none — idempotent, 202 again | read |
| `Stopping` | `POST /v1/abort` | `Ended` | none | read |
| embedded | either route | unchanged | handle is `None` → 409 | tested |

Invariants:

- A process that answers `/v1/stop` with 202 is going to end. Grace expiry is not optional.
- `/v1/abort` never answers from a process that has already exited: the exit is in a spawned
  task that sleeps first, so the response is flushed before the socket closes.

Reachable oddity, accepted: a second `/v1/stop` is 202 with the same grace. Refusing it would
make a retry look like a failure. The state does not change, so there is nothing to guard.

## B. A model's single-writer slot

One slot per catalog id, because two writers into one set of `.part` files corrupt the set.
The slot says who holds it, not merely that it is held — that distinction is defect SM-1.

| state | meaning | `download_progress` |
| --- | --- | --- |
| `Free` | no entry in the map | `None` |
| `Downloading{downloaded,total}` | a fetch is running or waiting for a slot | `Some(...)` |
| `Deleting` | a delete is unlinking the files | `None` |

| from | event | to | guard | status |
| --- | --- | --- | --- | --- |
| `Free` | `POST /v1/models/{id}/download` | `Downloading{0,total}` | claimed under one lock | tested |
| `Downloading` | same | unchanged | 202 with the running progress | tested |
| `Deleting` | same | unchanged | 409 `delete_in_progress` | tested |
| `Free` | `DELETE /v1/models/{id}` | `Deleting` | claimed under one lock | tested |
| `Downloading` | `DELETE` | unchanged | 409 `download_in_progress` | tested |
| `Deleting` | `DELETE` | unchanged | 409 | tested |
| `Downloading` | fetch ends, either way | `Free` | `finish_download` | tested |
| `Deleting` | unlink ends, either way | `Free` | `DeleteReservation` drop | tested |

Invariants:

- The check and the claim are one lock acquisition. Two requests that both read `Free` would
  both spawn a writer.
- A `Deleting` slot reports no progress. A poller that saw `0/total` there would wait for
  bytes no task is fetching.
- The reservation is released by a drop guard, so the error paths release it too. A
  reservation left behind wedges the id until the process restarts.
- `note_download` writes only to a `Downloading` slot or to an absent one. It must not
  resurrect a `Deleting` slot: the delete owns that entry.

Files on disk are a second, independent state: `NotInstalled` → `.part` files → verified
rename → `Installed`. The slot does not track it; `is_installed` reads the disk. A process
killed mid-fetch leaves `.part` files and a `Free` slot, and the next fetch resumes.

## C. The voice slot

One loaded TTS engine per process.

| state | meaning |
| --- | --- |
| `Empty` | nothing loaded; the next request loads what the config names |
| `Piper(voice)` | one graph, one voice; the voice *is* the model |
| `Kokoro(voice)` | one graph, a style table of 54; the style is a row |
| `Supertonic(voice)` | four graphs, a style file chosen at load |

| from | event | to | status |
| --- | --- | --- | --- |
| any | `PATCH` changes `tts.engine` | `Empty` (`VoiceChange::Reload`) | tested |
| `Kokoro` | `PATCH` changes `tts.voice` to a style in the archive | `Kokoro` in place | tested |
| `Kokoro` | `PATCH` names a style the archive lost | `Empty` (`Swapped::Refused`) | read |
| `Piper`/`Supertonic` | `PATCH` changes `tts.voice` | `Empty` (`Swapped::OtherEngine`) | tested |
| `Empty` | `PATCH` changes `tts.voice` | `Empty` (`Swapped::Empty`) | tested |
| `Empty` | a TTS request | loaded, or the request fails naming the file | tested |

Invariants:

- The config and the slot name the same engine. Every path that changes `tts.engine` drops the
  slot, and a patch is validated before any of it is applied.
- A style the archive does not hold is never spoken under that name. `Swapped::Refused` drops
  the slot rather than leaving the old style loaded under a new label.
- The slot lock is not held across a render's I/O beyond the render itself: a render holds it
  for its own duration, which is why a patch waits rather than interleaves.

## D. Config patch

`validate(next)` → `set_config(next)` → `retarget_voice(touched.voice)` → `drop_stt()` when
`touched.stt`.

Invariants, all tested:

- A refused patch changes nothing: validation runs against the prospective config, and the
  write happens only after it passes.
- A patch that changes `tts.engine` without naming a voice clears the voice, so the new
  engine's default applies. Leaving the old engine's voice would name something the new
  engine cannot load.
- `stt.threads`, `stt.batch_model`, `stt.stream_model`, `stt.language` and `tts.threads` are
  read when an engine loads, so each sets a `touched` flag and the slot is dropped.
- `stt.reverse_tolerance` does not drop the recogniser; it rebuilds the reverse index on the
  next correction, which is cheaper and is what the measurement in `PRD.md` F14.7b assumed.

## E. Streaming speech to text

| state | meaning | entry |
| --- | --- | --- |
| `AwaitingStart` | socket upgraded, no start frame | connection |
| `Refused` | start frame rejected; socket closes | bad rate, no model, no permit |
| `Ready` | `ready` sent, detector built | start frame accepted |
| `Streaming` | frames accepted, utterances emitted | first binary frame |
| `Ended` | socket closed by either side | close, cancel, idle timeout |

| from | event | to | answer | status |
| --- | --- | --- | --- | --- |
| `AwaitingStart` | start frame, rate ≠ 16 kHz | `Refused` | `error: unsupported_format` | tested |
| `AwaitingStart` | start frame, no detector | `Refused` | `error: engine_unavailable` | read |
| `AwaitingStart` | start frame, accepted | `Ready` | `ready` | tested |
| `Ready` | binary frame | `Streaming` | — | tested |
| `Streaming` | text frame `{"type":"cancel"}` | `Ended` | — | read |
| `Streaming` | no frames for `idle_timeout_ms` | `Ended` | — | read |
| any | client close | `Ended` | — | tested |

Invariants:

- One model per process, so `streaming` is decided by the config, not by the client, and
  cannot change mid-session.
- A partial is never sent for an utterance that has already been finalised; the generation
  counter is bumped when an utterance closes.
- The session holds one decode buffer for its life. A shorter frame replaces the longer one
  instead of leaving its tail.

## F. Voice activity

| state | meaning |
| --- | --- |
| `Quiet` | no utterance in progress; preroll keeps the last frames |
| `Speech` | collecting into the utterance |
| `Trailing` | silence counted since the last speech frame |

| from | event | to | effect | status |
| --- | --- | --- | --- | --- |
| `Quiet` | speech frame | `Speech` | `SpeechStart` with the preroll included | tested |
| `Speech` | speech frame | `Speech` | append | tested |
| `Speech` | silence frame | `Trailing` | count | tested |
| `Trailing` | speech frame | `Speech` | count reset | tested |
| `Trailing` | silence ≥ `min_silence_ms` | `Quiet` | `Utterance` if `speech_ms ≥ min_speech_ms`, else dropped | tested |
| `Speech`/`Trailing` | utterance reaches `max_segment_ms` | `Quiet` | forced `Utterance` | read |

Invariant: a sound shorter than `min_speech_ms` never becomes an utterance, which is why a
tone is not a test for this machine.

## G. The terminal front end

| state | meaning |
| --- | --- |
| `Idle` | nothing running |
| `Typing` | a phoneme recompute is due after a pause |
| `Synthesizing` | the worker is rendering |
| `Downloading` | one model download in flight |
| `Saving` | the save prompt is open |

| from | event | to | guard | status |
| --- | --- | --- | --- | --- |
| `Idle` | keystroke | `Typing` | — | read |
| `Typing` | pause elapses | `Idle` with new phonemes | — | read |
| `Idle` | Enter | `Synthesizing` | — | read |
| `Synthesizing` | worker returns | `Idle`, audio replaced | — | read |
| `Idle` | Ctrl+D | `Downloading` | refuses if one is already running | read |
| `Downloading` | progress, done, failed | `Downloading`, `Idle` | — | read |
| `Idle` | `s` | `Saving` | — | read |
| `Saving` | Esc | `Idle` | — | read |

Invariant: one download at a time, and a row's id and its model are both accepted where a
model is expected — a caller echoing a row back should not have to know which it holds.

## H. The C ABI

| state | meaning |
| --- | --- |
| `Uninitialised` | no handle |
| `Live` | handle valid, calls may proceed |
| `Freed` | handle released; every use is refused, not undefined |

Invariants: every entry point is inside `catch_unwind`; a null or freed handle is a typed
refusal; the header is generated from the same declarations CI diffs.

---

## Defects

### SM-1 — a download asked for during a delete was accepted and never ran (fixed)

**Reachable.** `DELETE /v1/models/{id}` claims the slot before unlinking. `POST
/v1/models/{id}/download` that landed in that window was answered `202 downloading` with the
delete's zero progress, and no task ever fetched a byte: the caller polled a progress that
could not move.

**Evidence.** The slot was `(u64, u64)`, so `begin_download` could not tell a download from a
delete; `start_download` answered 202 for any held slot. Two tests now pin the fixed
behaviour (`a_download_asked_for_during_a_delete_is_refused_not_accepted`,
`a_model_being_deleted_is_not_reported_as_downloading`), and `a_second_delete_of_the_same_model_is_refused`
pins the delete side.

**Fix.** `Reservation::{Downloading{downloaded,total}, Deleting}`. A download on `Deleting` is
409 `delete_in_progress`; `download_progress` reports `None` for a delete, so the catalog never
shows a phantom download; `note_download` will not write over a delete's reservation.

### SM-2 — a second `/v1/stop` is accepted (accepted, not a defect)

Documented above. Idempotent by design.

### SM-3 — a panicking fetch task would wedge the id (unreachable today)

`begin_download` claims the slot, then spawns a task that releases it at the end. If the task
unwound, the entry would stay and every retry would be refused with `Downloading` until the
process restarted. The task body has no `unwrap`, `expect` or panic path today, so the state
is not reachable; the safety rests on that discipline rather than on a guard. A drop guard
around the task body is the shape of the fix if the body ever grows a panic.

---

## Which test holds which row

| machine | test |
| --- | --- |
| A | `stop_without_an_accept_loop_is_refused` |
| B | `only_one_caller_can_start_a_download_of_one_model`, `a_download_asked_for_during_a_delete_is_refused_not_accepted`, `a_model_being_deleted_is_not_reported_as_downloading`, `a_second_delete_of_the_same_model_is_refused`, `a_delete_that_fails_still_gives_the_model_back`, `a_model_being_downloaded_cannot_be_deleted` |
| C | `a_voice_only_patch_keeps_the_real_graph_loaded`, `the_third_engine_is_listed_and_patchable`, `a_supertonic_voice_the_catalog_does_not_hold_is_refused` |
| D | `the_engine_settings_are_patchable_too`, `a_tolerance_patch_changes_the_next_correction`, `a_refused_patch_changes_nothing` |
| E | `tests/ws_stream.rs`, `a_reused_buffer_keeps_its_capacity_and_its_length` |
| F | `speech_and_silence_are_told_apart`, `a_click_too_short_to_be_speech_is_dropped`, `a_frame_the_right_size_never_touches_the_buffer` |
| G | none — the app has no test harness that builds an `App`, so the download guard is read |
| H | `abi/smoke.sh`, the header diff in CI |

Rows marked *read* are the work this document leaves behind: each one is a test that does not
exist yet, and the table says so rather than implying coverage it does not have.
