//! Streaming speech to text over WebSocket.
//!
//! The client sends a start frame, then raw audio, and receives a final transcript per
//! utterance. Partials are best effort: they run only when the engine is idle, and they
//! never hold up the audio coming in.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arioso_vad::{Config as VadConfig, Event, Segmenter};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use crate::{AppState, SttFailure};

/// The only rate the streaming path accepts. The engine wants 16 kHz, and resampling a
/// live stream frame by frame belongs in the capture layer, not here.
pub(crate) const STREAM_RATE: u32 = 16_000;

/// Shortest audio worth transcribing, in samples.
const MIN_FINAL: usize = STREAM_RATE as usize / 4;

/// Shortest audio worth a partial. Below this the engine guesses at noise, and the
/// guess is worse than saying nothing.
const MIN_PARTIAL: usize = STREAM_RATE as usize * 3 / 5;

/// Frames queued for the writer before a slow client is dropped.
const QUEUE: usize = 32;

/// Request sent as the first text message.
///
/// Unknown fields stay ignored, so a newer client that sends a field this server does not know
/// still opens a session. The `type` field is the exception, because a frame that says it is
/// something other than a start is not a client from the future: it is a client speaking a
/// different protocol, and answering `ready` to it hides the mistake until no audio arrives.
#[derive(Debug, Deserialize)]
pub(crate) struct StartRequest {
    /// The message type, `start`. Optional, because the first version of this frame had no
    /// type; when present it must be `start`.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Sample rate of the audio that follows.
    #[serde(default = "default_rate")]
    pub sample_rate: u32,
    // A `language` field is accepted and ignored: the engine loads once, with the language
    // from the config, so a per-session language would need a second engine.
}

fn default_rate() -> u32 {
    STREAM_RATE
}

/// The segmenter's settings, from the ones the operator configured.
///
/// Two names differ between the document and the crate: `padding_ms` is audio kept from
/// before the detected start, which the segmenter calls a preroll, and `max_segment_ms` is
/// the hard cap on one utterance. Mapping them by position would be a silent bug.
fn vad_config(vad: &arioso_config::Vad) -> VadConfig {
    VadConfig {
        threshold: vad.threshold,
        frame_ms: vad.frame_ms,
        min_silence_ms: vad.min_silence_ms,
        min_speech_ms: vad.min_speech_ms,
        preroll_ms: vad.padding_ms,
        max_utterance_ms: vad.max_segment_ms,
    }
}

/// Upgrade the connection and transcribe.
pub async fn handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    if !state.config().stt.enabled {
        return crate::engine_disabled("speech to text");
    }
    let Some(permit) = state.open_session() else {
        return crate::too_many_sessions();
    };
    ws.on_upgrade(move |socket| run(socket, state, permit))
}

/// Convert little-endian 16-bit PCM to samples.
fn pcm16(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / f32::from(i16::MAX))
        .collect()
}

/// A session id that sorts by time, for a client to correlate its logs.
fn session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}{:04x}", now.as_secs(), now.subsec_millis())
}

/// Seconds, rounded to a hundredth, for the offsets in an event.
fn seconds(samples: usize) -> f64 {
    (samples as f64 / f64::from(STREAM_RATE) * 100.0).round() / 100.0
}

async fn send(out: &mpsc::Sender<Message>, frame: serde_json::Value) {
    let _ = out.send(Message::Text(frame.to_string().into())).await;
}

/// A duration or offset in seconds, rounded to a hundredth.
///
/// Every other number on this protocol is rounded this way, and a raw `f64` in `duration`
/// was the one that was not: `0.123456789` next to `0.12` in the same document.
fn hundredths(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// The code and the safe sentence for a transcription failure.
///
/// The engine's own message can name a path or a library symbol, so it goes to the log and
/// the client gets a stable sentence. The batch route makes the same split, and the two
/// must not describe one failure differently.
fn stt_error(failure: &SttFailure) -> (&'static str, String) {
    match failure {
        SttFailure::Audio(detail) => ("bad_audio", detail.clone()),
        SttFailure::Engine(detail) => {
            tracing::error!(%detail, "the speech engine failed");
            (
                "engine_unavailable",
                "the speech engine failed; the server log has the detail".to_string(),
            )
        }
        SttFailure::NotInstalled(id) => (
            "model_not_installed",
            format!("{id} is not installed. Run `arioso models download {id}`."),
        ),
    }
}

/// Transcribe one utterance and send its final frame.
///
/// Returns false when the session has to end, which is what an error frame means here: the
/// stream stops on the frame that says why. The final flush used to discard the failure
/// and answer `done`, so a client that stopped early on a broken engine was told the
/// session finished cleanly and lost its only utterance without a word.
async fn send_utterance(
    state: &Arc<AppState>,
    out: &mpsc::Sender<Message>,
    samples: Vec<f32>,
    start: usize,
) -> bool {
    let length = samples.len();
    let worker = Arc::clone(state);
    let result =
        tokio::task::spawn_blocking(move || worker.transcribe_samples(&samples, STREAM_RATE)).await;
    match result {
        Ok(Ok(text)) => {
            send(
                out,
                json!({
                    "type": "final",
                    "text": text,
                    "start": seconds(start),
                    "end": seconds(start + length),
                }),
            )
            .await;
            true
        }
        Ok(Err(failure)) => {
            let (code, detail) = stt_error(&failure);
            send(
                out,
                json!({"type": "error", "code": code, "detail": detail}),
            )
            .await;
            false
        }
        Err(join) => {
            tracing::error!(%join, "the transcription task failed");
            send(
                out,
                json!({
                    "type": "error",
                    "code": "engine_unavailable",
                    "detail": "the speech engine failed; the server log has the detail",
                }),
            )
            .await;
            false
        }
    }
}

async fn run(socket: WebSocket, state: Arc<AppState>, permit: tokio::sync::OwnedSemaphorePermit) {
    // Held for the life of the socket and dropped with it, so the permit returns even if
    // the client disappears mid-frame.
    let _permit = permit;
    let (mut sink, mut stream) = socket.split();
    let (out, mut queued) = mpsc::channel::<Message>(QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });

    let limit = Duration::from_millis(state.config().server.first_message_timeout_ms);
    let first = match tokio::time::timeout(limit, stream.next()).await {
        Ok(Some(Ok(message))) => message,
        _ => {
            send(
                &out,
                json!({"type": "error", "detail": format!("no start frame within {} ms", limit.as_millis())}),
            )
            .await;
            drop(out);
            let _ = writer.await;
            return;
        }
    };
    let Message::Text(payload) = first else {
        send(
            &out,
            json!({"type": "error", "detail": "expected a JSON start frame"}),
        )
        .await;
        drop(out);
        let _ = writer.await;
        return;
    };
    let start: StartRequest = match serde_json::from_str(payload.as_str()) {
        Ok(start) => start,
        Err(error) => {
            send(
                &out,
                json!({"type": "error", "detail": format!("bad start frame: {error}")}),
            )
            .await;
            drop(out);
            let _ = writer.await;
            return;
        }
    };
    if let Some(kind) = start.kind.as_deref() {
        if kind != "start" {
            send(
                &out,
                json!({
                    "type": "error",
                    "detail": format!("expected a start frame, got type {kind:?}"),
                }),
            )
            .await;
            drop(out);
            let _ = writer.await;
            return;
        }
    }
    if start.sample_rate != STREAM_RATE {
        send(
            &out,
            json!({
                "type": "error",
                "code": "unsupported_format",
                "detail": format!("send {STREAM_RATE} Hz audio, not {} Hz", start.sample_rate),
            }),
        )
        .await;
        drop(out);
        let _ = writer.await;
        return;
    }

    send(
        &out,
        json!({"type": "ready", "session": session_id(), "sources": ["mic"]}),
    )
    .await;

    let mut segmenter = Segmenter::new(STREAM_RATE, vad_config(&state.config().vad));
    let interval_ms = state.config().stt.partial_interval_ms.max(50);
    let mut tick = tokio::time::interval(Duration::from_millis(u64::from(interval_ms)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let partials = state.config().stt.partials;
    // A socket that opens and then says nothing holds a session permit and an engine slot
    // for as long as the process runs. F11.3 puts a clock on it.
    let idle = Duration::from_millis(state.config().server.limits.idle_timeout_ms);
    let mut last_heard = Instant::now();
    let began = Instant::now();
    // Bumped when an utterance is finalised, so a partial still running for it is
    // dropped instead of arriving after the final.
    let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Shared with the partial tasks so the same hypothesis is not sent twice.
    let last_partial = Arc::new(tokio::sync::Mutex::new(String::new()));

    loop {
        tokio::select! {
            incoming = stream.next() => {
                let Some(Ok(message)) = incoming else { break };
                last_heard = Instant::now();
                match message {
                    Message::Binary(bytes) => {
                        let samples = pcm16(&bytes);
                        let mut failed = false;
                        for event in segmenter.push(&samples) {
                            let Event::Utterance { samples, start } = event else { continue };
                            if samples.len() < MIN_FINAL {
                                continue;
                            }
                            last_partial.lock().await.clear();
                            generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            if !send_utterance(&state, &out, samples, start as usize).await {
                                failed = true;
                                break;
                            }
                        }
                        if failed {
                            break;
                        }
                    }
                    Message::Text(text) => {
                        let Ok(command) = serde_json::from_str::<serde_json::Value>(text.as_str()) else {
                            continue;
                        };
                        match command.get("type").and_then(|kind| kind.as_str()) {
                            Some("cancel") => {
                                send(&out, json!({"type": "cancelled"})).await;
                                break;
                            }
                            Some("stop") => {
                                if let Some(Event::Utterance { samples, start }) = segmenter.flush() {
                                    if samples.len() >= MIN_FINAL
                                        && !send_utterance(&state, &out, samples, start as usize)
                                            .await
                                    {
                                        // The error frame ended the session; a `done`
                                        // after it would say the work finished.
                                        break;
                                    }
                                }
                                send(
                                    &out,
                                    json!({
                                        "type": "done",
                                        "duration": hundredths(began.elapsed().as_secs_f64()),
                                    }),
                                )
                                .await;
                                break;
                            }
                            _ => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            _ = tick.tick() => {
                // The clock runs whether or not partials are on, because the permit it
                // protects is held either way.
                if last_heard.elapsed() > idle {
                    send(
                        &out,
                        json!({
                            "type": "error",
                            "code": "idle_timeout",
                            "detail": format!(
                                "no audio or control message for {} s",
                                idle.as_secs()
                            ),
                        }),
                    )
                    .await;
                    break;
                }
                if !partials {
                    continue;
                }
                let Some((buffer, _)) = segmenter.open() else { continue };
                if buffer.len() < MIN_PARTIAL {
                    continue;
                }
                let copy = buffer.to_vec();
                let at = seconds(copy.len());
                let worker = Arc::clone(&state);
                let out = out.clone();
                let last_partial = Arc::clone(&last_partial);
                let seen = generation.load(std::sync::atomic::Ordering::SeqCst);
                let generation = Arc::clone(&generation);
                // Not awaited: a partial that takes its time must not stop the audio.
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        worker.try_transcribe_samples(&copy, STREAM_RATE)
                    })
                    .await;
                    let Ok(Some(text)) = result else { return };
                    if text.is_empty() {
                        return;
                    }
                    if generation.load(std::sync::atomic::Ordering::SeqCst) != seen {
                        // The utterance this partial belongs to is already final.
                        return;
                    }
                    {
                        let mut last = last_partial.lock().await;
                        if *last == text {
                            return;
                        }
                        last.clone_from(&text);
                    }
                    // Provisional, per F4.5: a client that appends partials to a
                    // transcript has to know that this text replaces the last one.
                    send(
                        &out,
                        json!({"type": "partial", "text": text, "t": at, "provisional": true}),
                    )
                    .await;
                });
            }
        }
    }

    drop(out);
    // Partial tasks hold clones of the sender, so the writer only finishes when they
    // do. Give them a moment and then stop waiting: a stuck partial must not keep a
    // closed session alive.
    if tokio::time::timeout(Duration::from_secs(2), writer)
        .await
        .is_err()
    {
        tracing::debug!("a partial was still running when the session closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_final_that_cannot_be_transcribed_answers_with_a_code() {
        // The final flush discarded the failure and answered `done`, so a client that
        // stopped early on a broken engine lost its only utterance without a word.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmudict.dict"), "hello HH AH0 L OW1\n").unwrap();
        let mut config = arioso_config::Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        let manifest =
            arioso_models::Manifest::parse(include_bytes!("../../../models/manifest.json"))
                .unwrap();
        let state = Arc::new(AppState::new(config, manifest, None).unwrap());

        let (tx, mut rx) = mpsc::channel(4);
        let ended = send_utterance(&state, &tx, vec![0.0; STREAM_RATE as usize], 0).await;
        assert!(!ended, "the session must end on the error");

        let Some(Message::Text(text)) = rx.recv().await else {
            panic!("no frame arrived");
        };
        let frame: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["code"], "model_not_installed");
        let detail = frame["detail"].as_str().unwrap();
        assert!(detail.contains("models download"), "{detail}");
        assert!(
            !detail.contains(dir.path().to_str().unwrap()),
            "the detail names the filesystem: {detail}"
        );
    }

    #[test]
    fn an_engine_failure_keeps_its_detail_in_the_log() {
        let (code, detail) = stt_error(&SttFailure::Engine(
            "onnxruntime: /opt/lib/libonnxruntime.so is missing".to_string(),
        ));
        assert_eq!(code, "engine_unavailable");
        assert!(!detail.contains("onnxruntime"), "{detail}");
        assert!(!detail.contains('/'), "{detail}");
    }

    #[test]
    fn a_duration_is_rounded_to_a_hundredth() {
        // Every other number on this protocol is rounded this way.
        assert_eq!(hundredths(0.123_456_789), 0.12);
        assert_eq!(hundredths(1.855), 1.86);
    }

    #[test]
    fn the_segmenter_takes_its_settings_from_the_config() {
        let vad = arioso_config::Vad {
            engine: "energy".into(),
            threshold: 0.02,
            min_speech_ms: 111,
            min_silence_ms: 222,
            padding_ms: 333,
            max_segment_ms: 444,
            frame_ms: 20,
        };
        let config = vad_config(&vad);
        assert!((config.threshold - 0.02).abs() < f32::EPSILON);
        assert_eq!(config.min_speech_ms, 111);
        assert_eq!(config.min_silence_ms, 222);
        // `padding_ms` is the preroll, not a tail.
        assert_eq!(config.preroll_ms, 333);
        assert_eq!(config.max_utterance_ms, 444);
        assert_eq!(config.frame_ms, 20);
    }

    #[test]
    fn pcm16_is_scaled_to_the_unit_range() {
        let bytes = [0x00, 0x00, 0xff, 0x7f, 0x01, 0x80];
        let samples = pcm16(&bytes);
        assert_eq!(samples.len(), 3);
        assert!(samples[0].abs() < 1e-6);
        assert!((samples[1] - 1.0).abs() < 0.001);
        assert!((samples[2] + 1.0).abs() < 0.001);
    }

    #[test]
    fn an_odd_trailing_byte_is_ignored() {
        let samples = pcm16(&[0x00, 0x00, 0x42]);
        assert_eq!(samples.len(), 1);
    }

    #[test]
    fn session_ids_differ_between_calls() {
        let first = session_id();
        std::thread::sleep(Duration::from_millis(2));
        assert_ne!(first, session_id());
    }

    #[test]
    fn the_start_frame_defaults_to_sixteen_kilohertz() {
        let start: StartRequest = serde_json::from_str(r#"{"type":"start"}"#).unwrap();
        assert_eq!(start.sample_rate, STREAM_RATE);
    }

    #[test]
    fn an_unknown_field_in_the_start_frame_is_ignored() {
        let start: StartRequest =
            serde_json::from_str(r#"{"type":"start","language":"en","sources":["mic"]}"#).unwrap();
        assert_eq!(start.sample_rate, STREAM_RATE);
    }

    #[test]
    fn offsets_are_rounded_to_hundredths() {
        assert_eq!(seconds(0), 0.0);
        assert_eq!(seconds(16_000), 1.0);
        assert_eq!(seconds(24_000), 1.5);
    }
}
