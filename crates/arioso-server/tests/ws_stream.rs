//! End-to-end check of the WebSocket stream.
//!
//! It needs a real voice, so it runs only when `ARIOSO_TEST_MODELS_DIR` points
//! at a directory holding the voice and the dictionary. CI skips it.
//!
//! The test asserts frame order and count. It does not assert wall-clock
//! overlap between synthesis and delivery; that is what the bounded queue
//! guarantees, and a timing assertion would be flaky on a loaded machine.

use std::path::PathBuf;
use std::sync::Arc;

use arioso_config::Config;
use arioso_models::Manifest;
use arioso_server::{AppState, router};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

const MANIFEST: &str = include_str!("../../../models/manifest.json");

#[tokio::test]
async fn streams_one_frame_per_sentence() {
    let Ok(dir) = std::env::var("ARIOSO_TEST_MODELS_DIR") else {
        eprintln!("skipped: ARIOSO_TEST_MODELS_DIR is not set");
        return;
    };

    let mut config = Config::default();
    config.models.dir = Some(PathBuf::from(dir));
    let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
    let state = Arc::new(AppState::new(config, manifest, None).unwrap());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/v1/tts/ws"))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            r#"{"text":"hello world. this is arioso."}"#.into(),
        ))
        .await
        .unwrap();

    let first = socket.next().await.unwrap().unwrap();
    let Message::Text(start) = first else {
        panic!("expected a start frame, got {first:?}");
    };
    assert!(start.contains("\"type\":\"start\""), "got {start}");
    assert!(start.contains("22050"), "got {start}");

    let mut audio_frames = 0;
    let mut samples = 0;
    loop {
        match socket.next().await.unwrap().unwrap() {
            Message::Binary(pcm) => {
                audio_frames += 1;
                samples += pcm.len() / 2;
            }
            Message::Text(end) => {
                assert!(end.contains("\"type\":\"end\""), "got {end}");
                break;
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }

    assert_eq!(audio_frames, 2, "one frame per sentence");
    assert!(samples > 10_000, "only {samples} samples");
}
