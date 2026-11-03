# Residual Review Fixes

The deep review of 2026-11-02 closed fourteen findings. A second audit over that work
(`a87c618..5302ae8`) found seven more, six of them created or exposed by the fixes
themselves. This file records what each one was, how it was verified, and the commit that
carries it.

Every task is one commit, gated on `cargo fmt --all -- --check`, `cargo clippy --all-targets
-- -D warnings`, `cargo test --workspace`, and `cargo doc --no-deps`, with all four exit
codes read.

## Tasks

| # | Finding | Fix | Commit |
|---|---|---|---|
| 1 | `arioso doctor` exited with ENOENT when a dictionary existed and the configured voice was listed but not downloaded | The voice config path is returned only when the file is on disk | `457fbab` |
| 2 | `tts.format` was checked when patched and not when loaded, so `mp3` started cleanly and answered WAV | `AUDIO_FORMATS` in `arioso-config`, checked by `validate()` and the patch handler | `411a686` |
| 3 | `log.file` grew without bound; the PRD asked for daily rotation and `log.retention_days` did not exist | `RotatingLog` writer, `<file>.<YYYY-MM-DD>` siblings, `log.retention_days` default 7 | `58b41cd` |
| 4 | A failed final flush on `{"type":"stop"}` was discarded and answered `done`; `duration` was a raw `f64` | One `send_utterance` for both paths, coded error frames, `duration` to a hundredth | `abf8698` |
| 5 | `DELETE /v1/models/{id}` checked for a download and then unlinked, so a download could start in the gap | The delete claims the id through `try_start_download` and releases it with a guard | `70fc237` |
| 6 | `tokio::select!` without `biased;` could answer `cancel` behind megabytes of queued audio | `biased;` with the socket branch first | `bcadb79` |
| 7 | `2026,100` was read as 2,026,100 because a comma never checked the digits before it | A comma groups only after one to three digits, and only three when one already came | `98af746` |

## Verification

| Claim | Evidence |
|---|---|
| `doctor` survives a fresh install with a dictionary | Live run on a temp directory with a manifest, a dictionary and no models: `voice en_US-lessac-medium is not installed, so entries were not checked`, then `ok`, exit 0. Test `a_voice_that_is_listed_but_not_downloaded_has_no_config`, which fails with the filter removed. |
| A format the binary cannot produce is refused at load | `arioso doctor` with `"format":"mp3"`: `invalid config at tts.format: expected one of wav, pcm, got "mp3"; no other audio encoder is built into this binary`. Test fails with the check disabled. |
| The log rotates and prunes | A log file with a two-day-old modification time rotated to `arioso.log.2026-11-01` on the first line the server wrote; the configured path held only today's record. Tests cover the rollover, the window, and that `arioso.log.json` is not pruned. |
| A failed final ends the session with a code | Test `a_final_that_cannot_be_transcribed_answers_with_a_code`: `model_not_installed`, no path in the detail, and `send_utterance` returns false. |
| A failed delete gives the model back | Test `a_delete_that_fails_still_gives_the_model_back`: a directory where a file belongs makes the unlink fail, and the id must come back unreserved. |
| Cancel is answered before the next chunk | Live probe: cancel at 2.157 s, `cancelled` at 2.483 s, two binary frames and none after the cancel. A race cannot be proven gone by one run; the bias removes the draw. |
| A year is not a number's head | `2026,100` → `twenty twenty six,one hundred`; `1,234,567` unchanged. |

## Deliberate departures from the audit

- **The delete race is pinned at its observable end, not in the middle.** A test cannot
  open the window it is meant to close. The atomicity is one lock acquisition in
  `try_start_download`, and the test proves the reservation is released on the error path,
  which is the failure mode that wedges an id until restart.
- **`is_downloading` was deleted rather than kept for the test.** It had one caller and the
  test can read `download_progress`; a method kept alive only by a test is dead code with a
  reason attached.
- **The `1.5,000` case needed no code.** The whole-part loop stops at the dot, so a comma
  after a decimal was never consumed. It is in the test because the audit expected a fix
  and the next reader will ask why there is none.
- **The rotation day boundary is UTC**, which is what a log without a timezone should be.
  The active file keeps the configured name; crates that rename the active file break every
  operator who tails it.

## Still open

Carried from the first review, unchanged by this pass: `GET /v1/models/events` (SSE
progress), the plain-socket connection cap, TLS and certificate reporting in `doctor`, the
fuzz targets, the Windows PID ownership path, Kokoro, the Silero backend, Zipformer
streaming, per-session language, manifest signing, and the C ABI the transcriber needs.
