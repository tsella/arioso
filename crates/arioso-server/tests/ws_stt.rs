//! The parts of the streaming contract that need no model.
//!
//! A test that cannot set up its fixture should fail loudly, so `unwrap` is the
//! right tool here.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use arioso_config::Config;
use arioso_models::Manifest;
use arioso_server::{AppState, router};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

const MANIFEST: &str = include_str!("../../../models/manifest.json");

async fn serve() -> String {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cmudict.dict"), "HELLO  HH AH0 L OW1\n").unwrap();
    let mut config = Config::default();
    config.models.dir = Some(dir.path().to_path_buf());
    let manifest = Manifest::parse(MANIFEST.as_bytes()).unwrap();
    let state = Arc::new(AppState::new(config, manifest, None).unwrap());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    std::mem::forget(dir);
    format!("ws://{address}/v1/stt/ws")
}

#[tokio::test]
async fn a_start_frame_is_answered_with_ready() {
    let url = serve().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    socket
        .send(Message::Text(
            r#"{"type":"start","sample_rate":16000}"#.into(),
        ))
        .await
        .unwrap();

    let first = socket.next().await.unwrap().unwrap();
    let Message::Text(frame) = first else {
        panic!("expected a text frame, got {first:?}");
    };
    assert!(frame.contains("\"type\":\"ready\""), "got {frame}");
    assert!(frame.contains("\"session\""), "got {frame}");
}

#[tokio::test]
async fn the_wrong_sample_rate_is_named() {
    let url = serve().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    socket
        .send(Message::Text(
            r#"{"type":"start","sample_rate":44100}"#.into(),
        ))
        .await
        .unwrap();

    let first = socket.next().await.unwrap().unwrap();
    let Message::Text(frame) = first else {
        panic!("expected a text frame, got {first:?}");
    };
    assert!(frame.contains("unsupported_format"), "got {frame}");
    assert!(frame.contains("16000"), "got {frame}");
}

#[tokio::test]
async fn cancel_is_answered_before_the_socket_closes() {
    let url = serve().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    socket
        .send(Message::Text(r#"{"type":"start"}"#.into()))
        .await
        .unwrap();
    let _ready = socket.next().await.unwrap().unwrap();

    socket
        .send(Message::Text(r#"{"type":"cancel"}"#.into()))
        .await
        .unwrap();
    let reply = socket.next().await.unwrap().unwrap();
    let Message::Text(frame) = reply else {
        panic!("expected a text frame, got {reply:?}");
    };
    assert!(frame.contains("cancelled"), "got {frame}");
}
