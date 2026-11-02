//! Streaming speech to text over WebSocket.
//!
//! The client sends a start frame, then raw audio, and receives a final transcript per
//! utterance. Partials are best effort: they run only when the engine is idle, and they
//! never hold up the audio coming in.

use std::sync::Arc;
use std::time::Duration;

use arioso_vad::{Config as VadConfig, Event, Segmenter};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use crate::AppState;

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
#[derive(Debug, Deserialize)]
pub(crate) struct StartRequest {
    /// Sample rate of the audio that follows.
    #[serde(default = "default_rate")]
    pub sample_rate: u32,
    // A `language` field is accepted and ignored: the engine loads once, with the
    // language from the config, so a per-session language would need a second engine.
}

fn default_rate() -> u32 {
    STREAM_RATE
}

/// Upgrade the connection and stream transcripts.
pub async fn handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| run(socket, state))
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

async fn run(socket: WebSocket, state: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();
    let (out, mut queued) = mpsc::channel::<Message>(QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });

    let limit = Duration::from_millis(state.config.server.first_message_timeout_ms);
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

    let mut segmenter = Segmenter::new(STREAM_RATE, VadConfig::default());
    let interval_ms = state.config.stt.partial_interval_ms.max(50);
    let mut tick = tokio::time::interval(Duration::from_millis(u64::from(interval_ms)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let partials = state.config.stt.partials;
    // Bumped when an utterance is finalised, so a partial still running for it is
    // dropped instead of arriving after the final.
    let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Shared with the partial tasks so the same hypothesis is not sent twice.
    let last_partial = Arc::new(tokio::sync::Mutex::new(String::new()));

    loop {
        tokio::select! {
            incoming = stream.next() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Binary(bytes) => {
                        let samples = pcm16(&bytes);
                        for event in segmenter.push(&samples) {
                            let Event::Utterance { samples, start } = event else { continue };
                            if samples.len() < MIN_FINAL {
                                continue;
                            }
                            last_partial.lock().await.clear();
                            generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let length = samples.len();
                            let worker = Arc::clone(&state);
                            let result = tokio::task::spawn_blocking(move || {
                                worker.transcribe_samples(&samples, STREAM_RATE)
                            })
                            .await;
                            match result {
                                Ok(Ok(text)) => {
                                    send(&out, json!({
                                        "type": "final",
                                        "text": text,
                                        "start": seconds(start as usize),
                                        "end": seconds(start as usize + length),
                                    })).await;
                                }
                                Ok(Err(failure)) => {
                                    send(&out, json!({
                                        "type": "error",
                                        "detail": failure.to_string(),
                                    })).await;
                                }
                                Err(_) => {
                                    send(&out, json!({
                                        "type": "error",
                                        "detail": "the transcription task failed",
                                    })).await;
                                }
                            }
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
                                    let length = samples.len();
                                    if length >= MIN_FINAL {
                                        let worker = Arc::clone(&state);
                                        let result = tokio::task::spawn_blocking(move || {
                                            worker.transcribe_samples(&samples, STREAM_RATE)
                                        })
                                        .await;
                                        if let Ok(Ok(text)) = result {
                                            send(&out, json!({
                                                "type": "final",
                                                "text": text,
                                                "start": seconds(start as usize),
                                                "end": seconds(start as usize + length),
                                            })).await;
                                        }
                                    }
                                }
                                break;
                            }
                            _ => {}
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            _ = tick.tick(), if partials => {
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
                    send(&out, json!({"type": "partial", "text": text, "t": at})).await;
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
