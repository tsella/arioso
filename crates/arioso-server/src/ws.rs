//! Streaming synthesis over WebSocket.

use std::sync::Arc;

use arioso_tts_piper::wav;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;

use crate::AppState;

/// Chunks queued between synthesis and the socket.
///
/// Small on purpose: it is what makes the first sentence leave before the last
/// one is synthesized.
const QUEUE: usize = 2;

/// Request sent as the first text message.
#[derive(Debug, Deserialize)]
pub(crate) struct StreamRequest {
    /// Text to speak.
    pub text: String,
    /// Speaking rate multiplier, overriding the config.
    pub speed: Option<f32>,
    /// Speaker number on a multi-speaker voice, overriding the config.
    pub sid: Option<i64>,
}

/// Upgrade the connection and stream.
pub async fn handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    if !state.config().tts.enabled {
        return crate::engine_disabled("text to speech");
    }
    let Some(permit) = state.open_session() else {
        return crate::too_many_sessions();
    };
    ws.on_upgrade(move |socket| run(socket, state, permit))
}

/// Check a request and resolve the speed.
fn prepare(
    request: &StreamRequest,
    default_speed: f32,
    max_text_chars: usize,
) -> Result<(String, f32), String> {
    let text = request.text.trim();
    if text.is_empty() {
        return Err("text must not be empty".into());
    }
    if text.chars().count() > max_text_chars {
        return Err(format!("text is longer than {max_text_chars} characters"));
    }
    let speed = request.speed.unwrap_or(default_speed);
    if !(0.5..=2.0).contains(&speed) {
        return Err(format!("speed {speed} is outside 0.5 to 2.0"));
    }
    Ok((text.to_owned(), speed))
}

/// Whether a control frame asks to stop the stream.
///
/// The whole document is a control message, so anything that is not JSON, or is JSON
/// without `type: cancel`, is not one.
fn is_cancel(text: &str) -> bool {
    #[derive(Deserialize)]
    struct Control {
        #[serde(rename = "type")]
        kind: String,
    }
    serde_json::from_str::<Control>(text).is_ok_and(|control| control.kind == "cancel")
}

/// One chunk as the payload of a binary frame, and how many samples it holds.
///
/// The conversion runs on the thread that synthesized the audio, inside the blocking
/// render task, so the channel carries what the socket sends and the event loop does no
/// per-chunk work beyond the frame. Cloning the floats across the channel and converting
/// them on a Tokio worker cost a copy of the chunk in `f32` for every sentence.
fn chunk_payload(samples: &[f32]) -> (Vec<u8>, usize) {
    (wav::pcm_s16le(samples), samples.len())
}

/// The first message, or `None` when the client sends nothing in time or the
/// connection ends first.
async fn first_message(socket: &mut WebSocket, limit: std::time::Duration) -> Option<Message> {
    match tokio::time::timeout(limit, socket.recv()).await {
        Ok(Some(Ok(message))) => Some(message),
        _ => None,
    }
}

async fn run(
    mut socket: WebSocket,
    state: Arc<AppState>,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    // Held for the life of the socket and dropped with it, so the permit returns even if
    // the client disappears mid-frame.
    let _permit = permit;
    // A client that connects and says nothing must not hold the task open.
    let limit = std::time::Duration::from_millis(state.config().server.first_message_timeout_ms);
    let Some(first) = first_message(&mut socket, limit).await else {
        let frame = json!({
            "type": "error",
            "detail": format!("no request within {} ms", limit.as_millis()),
        });
        let _ = socket.send(Message::Text(frame.to_string().into())).await;
        return;
    };
    let Message::Text(payload) = first else {
        let frame = json!({"type": "error", "detail": "expected a JSON text message"});
        let _ = socket.send(Message::Text(frame.to_string().into())).await;
        return;
    };

    let request: StreamRequest = match serde_json::from_str(payload.as_str()) {
        Ok(request) => request,
        Err(error) => {
            let frame = json!({"type": "error", "detail": format!("bad request: {error}")});
            let _ = socket.send(Message::Text(frame.to_string().into())).await;
            return;
        }
    };

    let config = state.config();
    let sid = request.sid.unwrap_or(config.tts.sid);
    if !(0..=4095).contains(&sid) {
        let frame = json!({"type": "error", "detail": format!("sid {sid} is outside 0 to 4095")});
        let _ = socket.send(Message::Text(frame.to_string().into())).await;
        return;
    }
    if let Err(detail) = state.check_sid(sid) {
        let detail = detail.to_string();
        let frame = json!({"type": "error", "detail": detail});
        let _ = socket.send(Message::Text(frame.to_string().into())).await;
        return;
    }
    let (text, speed) = match prepare(
        &request,
        config.tts.speed,
        config.server.limits.max_text_chars,
    ) {
        Ok(pair) => pair,
        Err(detail) => {
            let frame = json!({"type": "error", "detail": detail});
            let _ = socket.send(Message::Text(frame.to_string().into())).await;
            return;
        }
    };

    let (sender, mut receiver) = tokio::sync::mpsc::channel::<(Vec<u8>, usize, u32)>(QUEUE);
    let worker = Arc::clone(&state);
    let joining = tokio::task::spawn_blocking(move || {
        worker.render_stream(&text, speed, sid, |samples, rate| {
            let (pcm, count) = chunk_payload(samples);
            sender
                .blocking_send((pcm, count, rate))
                .map_err(|_| "the client went away".to_string())
        })
    });

    let mut chunks = 0usize;
    let mut total_samples = 0usize;
    let mut cancelled = false;
    loop {
        let chunk = tokio::select! {
            // A control frame is read before the next chunk, every time. With both ready an
            // unbiased select picks at random, so a `cancel` that arrived while two
            // rendered sentences sat in the queue could be answered only after megabytes
            // the client no longer wants had gone down the socket.
            biased;
            // The socket is read for the whole stream, not only before it starts. A client
            // that cancels halfway through must be heard at the next chunk boundary.
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) if is_cancel(text.as_str()) => {
                        cancelled = true;
                        break;
                    }
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    // Audio and unknown frames from the client are ignored; the protocol
                    // sends control as JSON text.
                    Some(Ok(_)) => continue,
                }
            }
            chunk = receiver.recv() => chunk,
        };
        let Some((pcm, samples, rate)) = chunk else {
            break;
        };
        if chunks == 0 {
            let start = json!({"type": "start", "sample_rate": rate, "format": "pcm_s16le"});
            if socket
                .send(Message::Text(start.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        chunks += 1;
        total_samples += samples;
        if socket.send(Message::Binary(pcm.into())).await.is_err() {
            break;
        }
    }

    if cancelled {
        // Dropping the receiver is what stops the synthesis: the render task's next
        // `blocking_send` fails, it returns, and the voice lock is released.
        drop(receiver);
        let _ = joining.await;
        let frame = json!({
            "type": "cancelled",
            "chunks": chunks,
            "samples": total_samples,
        });
        let _ = socket.send(Message::Text(frame.to_string().into())).await;
        return;
    }

    let detail = match joining.await {
        Ok(Ok(())) if chunks == 0 => {
            Some("nothing to say: none of the words are in the dictionary".to_owned())
        }
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(error) => Some(error.to_string()),
    };

    let frame = match detail {
        Some(detail) => json!({"type": "error", "detail": detail}),
        None => json!({
            "type": "end",
            "chunks": chunks,
            "samples": total_samples,
        }),
    };
    let _ = socket.send(Message::Text(frame.to_string().into())).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use arioso_core::alloc_count::measure;

    #[test]
    fn a_chunk_crosses_the_channel_as_bytes() {
        // One sentence of 22,050 Hz audio. The chunk leaves the render thread as the
        // frame's payload: two bytes a sample, one allocation. The previous path cloned
        // the floats into the channel (four bytes a sample) and converted them on the
        // socket task (two more), which is 132,300 bytes for this chunk.
        let samples = vec![0.25f32; 22_050];
        let ((pcm, count), stats) = measure(|| chunk_payload(&samples));
        assert_eq!(count, samples.len(), "the frame reports every sample");
        assert_eq!(pcm.len(), samples.len() * 2, "16-bit mono");
        assert!(
            stats.bytes <= samples.len() * 2 + 64,
            "a chunk of {} samples allocated {} bytes",
            samples.len(),
            stats.bytes
        );
    }

    #[test]
    fn a_cancel_frame_is_recognised_and_nothing_else_is() {
        assert!(is_cancel("{\"type\":\"cancel\"}"));
        assert!(is_cancel(" { \"type\" : \"cancel\" } "));
        assert!(!is_cancel("{\"type\":\"stop\"}"));
        assert!(!is_cancel("not json"));
        assert!(!is_cancel("{\"text\":\"hello\"}"));
    }

    fn request(text: &str, speed: Option<f32>) -> StreamRequest {
        StreamRequest {
            text: text.to_owned(),
            speed,
            sid: None,
        }
    }

    #[test]
    fn empty_text_is_refused() {
        assert!(prepare(&request("   ", None), 1.0, 4_000).is_err());
    }

    #[test]
    fn long_text_is_refused() {
        let text = "a".repeat(4_001);
        let error = prepare(&request(&text, None), 1.0, 4_000).unwrap_err();
        assert!(error.contains("longer than"));
    }

    #[test]
    fn speed_defaults_to_the_config() {
        let (_, speed) = prepare(&request("hello", None), 0.8, 4_000).unwrap();
        assert!((speed - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn speed_outside_the_range_is_refused() {
        assert!(prepare(&request("hello", Some(3.0)), 1.0, 4_000).is_err());
    }

    #[test]
    fn text_is_trimmed() {
        let (text, _) = prepare(&request("  hello  ", None), 1.0, 4_000).unwrap();
        assert_eq!(text, "hello");
    }
}
