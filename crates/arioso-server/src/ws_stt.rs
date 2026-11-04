//! Streaming speech to text over WebSocket.
//!
//! The client sends a start frame, then raw audio, and receives a final transcript per
//! utterance. How a partial is made depends on the model the config names: a streaming
//! model is asked what it currently believes, and a batch model is run again over the
//! buffer so far. The frames are the same either way, so a client does not care which.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arioso_stt_sherpa::StreamSession;
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

/// The refusal that has to happen before the socket upgrades, if any.
///
/// A streaming model that is not on disk is refused here rather than after the upgrade, so
/// the caller gets the same 409 the batch route answers instead of a socket that opens and
/// then says why. A batch model is not checked: its error arrives as a frame, once the
/// engine is asked for it, and that behaviour does not change.
///
/// It is a plain function of the state so it can be tested: a `WebSocketUpgrade` cannot be
/// built outside a live connection, and the decision is the part that matters.
pub(crate) fn preflight(state: &AppState) -> Option<Response> {
    if !state.config().stt.enabled {
        return Some(crate::engine_disabled("speech to text"));
    }
    if let Some(spec) = state.stream_spec() {
        if !state.is_installed(spec) {
            return Some(crate::model_not_installed(&spec.id));
        }
    }
    None
}

/// Upgrade the connection and transcribe.
pub async fn handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    if let Some(refusal) = preflight(&state) {
        return refusal;
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
            // An utterance the engine heard no words in is not a line. The segmenter already
            // gates on speech frames, but a cough or a door can clear the energy threshold and
            // produce nothing, and a final with empty text would put a blank line in the file.
            if text.trim().is_empty() {
                return true;
            }
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

/// The model's own state for the utterance the segmenter has open.
///
/// One of these exists only while an utterance is being heard. Dropping it throws the
/// hypothesis away, which is what an utterance boundary means.
struct Live {
    /// Taken out for the duration of a blocking call and put straight back.
    session: Option<StreamSession>,
    /// Where the utterance began in the stream, so a final can be matched to it.
    start: usize,
    /// Samples handed to the model for this utterance, which is what `t` counts.
    fed: usize,
    /// What the model currently believes.
    text: String,
}

impl Live {
    fn new(session: StreamSession, start: usize) -> Self {
        Self {
            session: Some(session),
            start,
            fed: 0,
            text: String::new(),
        }
    }

    /// Feed audio and keep the newest hypothesis.
    ///
    /// Decoding is CPU work, so it runs off the runtime's thread: a chunk that takes
    /// twenty milliseconds to decode must not stall every other socket in the process.
    async fn feed(&mut self, samples: Vec<f32>) -> Result<Option<String>, SttFailure> {
        self.fed += samples.len();
        let Some(mut held) = self.session.take() else {
            return Err(SttFailure::Engine(
                "the utterance was already closed".to_string(),
            ));
        };
        let outcome = tokio::task::spawn_blocking(move || {
            let changed = held.feed(&samples);
            (held, changed)
        })
        .await;
        match outcome {
            Ok((held, changed)) => {
                self.session = Some(held);
                if let Some(text) = changed {
                    self.text.clone_from(&text);
                    return Ok(Some(text));
                }
                Ok(None)
            }
            Err(join) => {
                tracing::error!(%join, "the streaming decoder failed");
                Err(SttFailure::Engine(format!(
                    "the streaming decoder failed: {join}"
                )))
            }
        }
    }

    /// Close the utterance and return its text.
    async fn finish(self) -> Result<String, SttFailure> {
        let Some(session) = self.session else {
            return Ok(String::new());
        };
        tokio::task::spawn_blocking(move || session.finish())
            .await
            .map_err(|join| {
                tracing::error!(%join, "the streaming decoder failed");
                SttFailure::Engine(format!("the streaming decoder failed: {join}"))
            })
    }
}

/// When a partial may be sent.
///
/// The model decodes several times per chunk, and a client drawing every change shows
/// text that rewrites itself faster than a person reads. A partial goes out when the text
/// changed and the interval has passed since the last one, which is the same cadence the
/// batch path keeps with its timer.
struct Gate {
    enabled: bool,
    interval: Duration,
    last_at: Instant,
    last_text: String,
}

impl Gate {
    fn new(enabled: bool, interval: Duration) -> Self {
        Self {
            enabled,
            interval,
            last_at: Instant::now(),
            last_text: String::new(),
        }
    }

    /// The text to send now, if this is a new hypothesis and the interval has passed.
    fn allow(&mut self, text: &str) -> Option<String> {
        if !self.enabled || text.is_empty() || text == self.last_text {
            return None;
        }
        if self.last_at.elapsed() < self.interval {
            return None;
        }
        self.last_at = Instant::now();
        self.last_text = text.to_owned();
        Some(text.to_owned())
    }

    /// Forget the last partial, because the utterance it belonged to is final.
    fn reset(&mut self) {
        self.last_text.clear();
    }
}

/// Send the error frame that ends a session.
async fn end_with_error(out: &mpsc::Sender<Message>, failure: &SttFailure) -> bool {
    let (code, detail) = stt_error(failure);
    send(
        out,
        json!({"type": "error", "code": code, "detail": detail}),
    )
    .await;
    false
}

/// Feed one chunk of audio through the streaming path.
///
/// Returns false when the session has to end, which means an error frame has been sent.
async fn stream_chunk(
    state: &Arc<AppState>,
    out: &mpsc::Sender<Message>,
    segmenter: &mut Segmenter,
    live: &mut Option<Live>,
    gate: &mut Gate,
    samples: &[f32],
) -> bool {
    // The utterance the segmenter already has open hears this chunk. A chunk that ends
    // the utterance also feeds the silence that ended it, which is what the model needs
    // to be sure the word is over.
    if let Some(open) = live.as_mut() {
        match open.feed(samples.to_vec()).await {
            Ok(Some(text)) => {
                if open.fed >= MIN_PARTIAL {
                    if let Some(text) = gate.allow(&text) {
                        send(
                            out,
                            json!({
                                "type": "partial",
                                "text": text,
                                "t": seconds(open.fed),
                                "provisional": true,
                            }),
                        )
                        .await;
                    }
                }
            }
            Ok(None) => {}
            Err(failure) => return end_with_error(out, &failure).await,
        }
    }

    // Whatever the segmenter closed is final.
    for event in segmenter.push(samples) {
        let Event::Utterance {
            samples: audio,
            start,
        } = event
        else {
            continue;
        };
        if !finish_utterance(state, out, live, &audio, start as usize, gate).await {
            return false;
        }
    }

    // The utterance the segmenter now has open starts a stream of its own, fed with
    // everything it holds so far: the preroll is part of what the model should hear.
    if live.is_none() {
        if let Some((buffer, start)) = segmenter.open() {
            let session = match state.stream_session() {
                Ok(session) => session,
                Err(failure) => return end_with_error(out, &failure).await,
            };
            let mut open = Live::new(session, start as usize);
            let buffer = buffer.to_vec();
            if let Err(failure) = open.feed(buffer).await {
                return end_with_error(out, &failure).await;
            }
            *live = Some(open);
        }
    }
    true
}

/// Finish one utterance and send its final frame.
///
/// Returns false when the session has to end. An utterance the model heard no words in
/// sends nothing, for the reason the batch path gives: a cough that cleared the energy
/// threshold would otherwise put a blank line in the transcript.
async fn finish_utterance(
    state: &Arc<AppState>,
    out: &mpsc::Sender<Message>,
    live: &mut Option<Live>,
    audio: &[f32],
    start: usize,
    gate: &mut Gate,
) -> bool {
    if audio.len() < MIN_FINAL {
        live.take();
        return true;
    }
    // The stream the segmenter was filling belongs to this utterance. When it does not,
    // or when the utterance was too short to have opened one, the audio is decoded in one
    // go: the words are the same, only the partials are lost.
    let text = match live.take() {
        Some(open) if open.start == start => match open.finish().await {
            Ok(text) => text,
            Err(failure) => return end_with_error(out, &failure).await,
        },
        _ => {
            let session = match state.stream_session() {
                Ok(session) => session,
                Err(failure) => return end_with_error(out, &failure).await,
            };
            let mut open = Live::new(session, start);
            if let Err(failure) = open.feed(audio.to_vec()).await {
                return end_with_error(out, &failure).await;
            }
            match open.finish().await {
                Ok(text) => text,
                Err(failure) => return end_with_error(out, &failure).await,
            }
        }
    };
    gate.reset();
    let text = state.correct(&text);
    if text.trim().is_empty() {
        return true;
    }
    send(
        out,
        json!({
            "type": "final",
            "text": text,
            "start": seconds(start),
            "end": seconds(start + audio.len()),
        }),
    )
    .await;
    true
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
    // Which loop this session runs. The model decides, not the client: one model is
    // loaded per process, so the family cannot change mid-session.
    let streaming = state.stt_streams();
    let mut gate = Gate::new(partials, Duration::from_millis(u64::from(interval_ms)));
    let mut live: Option<Live> = None;
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
                        if streaming {
                            if !stream_chunk(
                                &state,
                                &out,
                                &mut segmenter,
                                &mut live,
                                &mut gate,
                                &samples,
                            )
                            .await
                            {
                                break;
                            }
                            continue;
                        }
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
                                if streaming {
                                    if let Some(Event::Utterance { samples, start }) =
                                        segmenter.flush()
                                    {
                                        if !finish_utterance(
                                            &state,
                                            &out,
                                            &mut live,
                                            &samples,
                                            start as usize,
                                            &mut gate,
                                        )
                                        .await
                                        {
                                            break;
                                        }
                                    }
                                } else if let Some(Event::Utterance { samples, start }) =
                                    segmenter.flush()
                                {
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
                if streaming {
                    // A streaming model has already decoded everything it has; the tick
                    // only sends the newest hypothesis when the interval held it back
                    // during the chunk that produced it.
                    if let Some(open) = live.as_ref() {
                        if open.fed >= MIN_PARTIAL {
                            let text = open.text.clone();
                            if let Some(text) = gate.allow(&text) {
                                send(
                                    &out,
                                    json!({
                                        "type": "partial",
                                        "text": text,
                                        "t": seconds(open.fed),
                                        "provisional": true,
                                    }),
                                )
                                .await;
                            }
                        }
                    }
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
    fn a_partial_is_sent_once_per_interval_and_only_when_it_changed() {
        let mut gate = Gate::new(true, Duration::ZERO);
        assert_eq!(gate.allow("hello"), Some("hello".to_string()));
        assert_eq!(gate.allow("hello"), None, "the same text is not sent twice");
        assert_eq!(gate.allow("hello wor"), Some("hello wor".to_string()));
        // A new utterance may use the words the last one ended on.
        gate.reset();
        assert_eq!(gate.allow("hello wor"), Some("hello wor".to_string()));
    }

    #[test]
    fn a_partial_waits_for_the_interval_or_for_partials_to_be_on() {
        let mut gate = Gate::new(true, Duration::from_secs(3_600));
        assert_eq!(gate.allow("hello"), None, "the interval has not passed");
        let mut gate = Gate::new(false, Duration::ZERO);
        assert_eq!(gate.allow("hello"), None, "partials are off");
    }

    #[test]
    fn offsets_are_rounded_to_hundredths() {
        assert_eq!(seconds(0), 0.0);
        assert_eq!(seconds(16_000), 1.0);
        assert_eq!(seconds(24_000), 1.5);
    }
}
