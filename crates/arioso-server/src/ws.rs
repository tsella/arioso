//! Streaming synthesis over WebSocket.

use std::sync::Arc;

use arioso_tts_piper::wav;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;

use crate::{AppState, MAX_TEXT_CHARS};

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
}

/// Upgrade the connection and stream.
pub async fn handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| run(socket, state))
}

/// Check a request and resolve the speed.
fn prepare(request: &StreamRequest, default_speed: f32) -> Result<(String, f32), String> {
    let text = request.text.trim();
    if text.is_empty() {
        return Err("text must not be empty".into());
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(format!("text is longer than {MAX_TEXT_CHARS} characters"));
    }
    let speed = request.speed.unwrap_or(default_speed);
    if !(0.5..=2.0).contains(&speed) {
        return Err(format!("speed {speed} is outside 0.5 to 2.0"));
    }
    Ok((text.to_owned(), speed))
}

/// The first message, or `None` when the client sends nothing in time or the
/// connection ends first.
async fn first_message(socket: &mut WebSocket, limit: std::time::Duration) -> Option<Message> {
    match tokio::time::timeout(limit, socket.recv()).await {
        Ok(Some(Ok(message))) => Some(message),
        _ => None,
    }
}

async fn run(mut socket: WebSocket, state: Arc<AppState>) {
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

    let (text, speed) = match prepare(&request, state.config().tts.speed) {
        Ok(pair) => pair,
        Err(detail) => {
            let frame = json!({"type": "error", "detail": detail});
            let _ = socket.send(Message::Text(frame.to_string().into())).await;
            return;
        }
    };

    let (sender, mut receiver) = tokio::sync::mpsc::channel::<(Vec<f32>, u32)>(QUEUE);
    let worker = Arc::clone(&state);
    let joining = tokio::task::spawn_blocking(move || {
        worker.render_stream(&text, speed, |samples, rate| {
            sender
                .blocking_send((samples.to_vec(), rate))
                .map_err(|_| "the client went away".to_string())
        })
    });

    let mut chunks = 0usize;
    let mut total_samples = 0usize;
    while let Some((samples, rate)) = receiver.recv().await {
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
        total_samples += samples.len();
        if socket
            .send(Message::Binary(wav::pcm_s16le(&samples).into()))
            .await
            .is_err()
        {
            break;
        }
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

    fn request(text: &str, speed: Option<f32>) -> StreamRequest {
        StreamRequest {
            text: text.to_owned(),
            speed,
        }
    }

    #[test]
    fn empty_text_is_refused() {
        assert!(prepare(&request("   ", None), 1.0).is_err());
    }

    #[test]
    fn long_text_is_refused() {
        let text = "a".repeat(MAX_TEXT_CHARS + 1);
        let error = prepare(&request(&text, None), 1.0).unwrap_err();
        assert!(error.contains("longer than"));
    }

    #[test]
    fn speed_defaults_to_the_config() {
        let (_, speed) = prepare(&request("hello", None), 0.8).unwrap();
        assert!((speed - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn speed_outside_the_range_is_refused() {
        assert!(prepare(&request("hello", Some(3.0)), 1.0).is_err());
    }

    #[test]
    fn text_is_trimmed() {
        let (text, _) = prepare(&request("  hello  ", None), 1.0).unwrap();
        assert_eq!(text, "hello");
    }
}
