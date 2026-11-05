//! Streaming speech to text over WebSocket.
//!
//! The client sends a start frame, then raw audio, and receives a final transcript per
//! utterance. How a partial is made depends on the model the config names: a streaming
//! model is asked what it currently believes, and a batch model is run again over the
//! buffer so far. The frames are the same either way, so a client does not care which.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arioso_stt_sherpa::{StreamSession, silero};
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

/// The segmenter this config asks for.
///
/// `energy` needs nothing. `silero` loads its model from the catalog; the config's frame
/// length was held against the model's window when the config was validated, so the only
/// thing left to fail here is the file itself.
fn segmenter_for(state: &AppState) -> Result<Segmenter, String> {
    let config = state.config();
    let vad = &config.vad;
    let settings = vad_config(vad);
    if vad.engine != "silero" {
        return Ok(Segmenter::new(STREAM_RATE, settings));
    }
    let model = state.model_file(&vad.model, ".onnx")?;
    let gate = silero::Silero::load(&model, vad.probability).map_err(|error| error.to_string())?;
    Ok(Segmenter::with_gate(STREAM_RATE, settings, Box::new(gate)))
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
    // A detector named in the config is a model like any other: the socket says which file
    // is missing before it upgrades, rather than opening and answering every frame with the
    // energy detector the caller did not ask for.
    let vad = &state.config().vad;
    if vad.engine == "silero" && !state.model_installed(&vad.model) {
        return Some(crate::model_not_installed(&vad.model));
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
        // `i16::MIN` divided by `i16::MAX` lands at -1.00003, outside the range the model
        // takes; the clamp keeps the samples in it.
        .map(|pair| {
            (f32::from(i16::from_le_bytes([pair[0], pair[1]])) / f32::from(i16::MAX)).max(-1.0)
        })
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

/// One thing a chunk asks of the streaming path.
#[derive(Debug)]
enum Step<'a> {
    /// Audio an utterance's session has not heard yet.
    Hear {
        /// Where the utterance began in the stream.
        start: u64,
        /// The samples to feed.
        audio: &'a [f32],
    },
    /// The utterance is over: hear its last audio, then send its final.
    Close {
        /// Where the utterance began in the stream.
        start: u64,
        /// What the segmenter's copy of the utterance holds and the session has not heard.
        audio: &'a [f32],
        /// The silence that ended the utterance, from where this chunk meets that copy.
        tail: &'a [f32],
        /// Everything the segmenter kept for it, which is what the final frame covers and
        /// what is decoded when no session was ever opened for it.
        utterance: &'a [f32],
    },
}

/// What one chunk means for the sessions, in the order it happens.
///
/// The segmenter is the only thing that knows where an utterance ends, so this is built
/// from what it reported and not from the extent of the chunk. A session hears the
/// utterance it belongs to and nothing else, and it hears it from the segmenter's own copy:
/// that copy begins at the utterance's start, so the count a session has already heard
/// lines up with it however the chunk boundaries fell. The chunk is only read where it
/// continues the copy, which is the silence that ended the utterance and not the utterance.
///
/// Slicing the chunk from the utterance's offset instead lost samples in two places. An
/// utterance that began in an earlier chunk, with its preroll in audio that arrived before
/// the utterance existed, has a start before `at`, so the offset clamped to the chunk's
/// first sample and the preroll was never heard. And a session fed up to the last whole
/// frame of the previous chunk is short by the samples the segmenter was still holding for
/// it, which are in the segmenter and not in this chunk.
///
/// `at` is the stream index of `chunk[0]` and `session` is the open session's utterance
/// start with how many samples it has heard.
fn plan_chunk<'a>(
    chunk: &'a [f32],
    at: u64,
    events: &'a [Event],
    open: Option<(&'a [f32], u64)>,
    session: Option<(u64, usize)>,
) -> Vec<Step<'a>> {
    let index = |position: u64| {
        usize::try_from(position.saturating_sub(at))
            .unwrap_or(usize::MAX)
            .min(chunk.len())
    };
    let fed = |start: u64| match session {
        Some((heard_from, heard)) if heard_from == start => heard,
        _ => 0,
    };
    let mut steps = Vec::new();
    for event in events {
        let Event::Utterance {
            samples: utterance,
            start,
            end,
        } = event
        else {
            continue;
        };
        let heard = fed(*start);
        // A session that has heard nothing is opened by the final instead, which decodes
        // the segmenter's copy whole. Handing it this chunk would start the session at the
        // chunk's first sample, and the beginning of an utterance that started in an
        // earlier chunk is not there.
        let (audio, tail) = if heard == 0 {
            (&[][..], &[][..])
        } else {
            // The copy starts at the utterance's start, so the rest of it is what the
            // session has not heard, whichever chunk the two of them fell in.
            let audio = utterance
                .get(heard.min(utterance.len())..)
                .unwrap_or_default();
            // Past the copy is the silence that ended the utterance, which the segmenter
            // trims and the stream still has. It is in this chunk only from where the two
            // meet: reading the chunk from before that point would repeat a sample, and
            // from the chunk's start would skip one.
            let meet = (*start + utterance.len() as u64).max(at);
            let from = index(meet);
            let to = index(*end).max(from);
            (audio, &chunk[from..to])
        };
        steps.push(Step::Close {
            start: *start,
            audio,
            tail,
            utterance,
        });
    }
    if let Some((buffer, start)) = open {
        let from = fed(start).min(buffer.len());
        steps.push(Step::Hear {
            start,
            audio: &buffer[from..],
        });
    }
    steps
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
    // The segmenter hears the chunk first, because it is what knows where an utterance
    // ends. Feeding the open session the whole chunk first made that session hear the
    // silence that ended its utterance and the words that began the next one, so its final
    // covered the wrong audio and those words were then fed to the next session and
    // decoded a second time.
    let at = segmenter.heard();
    let events = segmenter.push(samples);
    let open = segmenter.open();
    let steps = plan_chunk(
        samples,
        at,
        &events,
        open,
        live.as_ref().map(|open| (open.start as u64, open.fed)),
    );

    for step in steps {
        match step {
            Step::Hear { start, audio } => {
                if !feed_utterance(state, out, live, start, audio, gate).await {
                    return false;
                }
            }
            Step::Close {
                start,
                audio,
                tail,
                utterance,
            } => {
                // The session hears the last of its own audio, the silence that ended the
                // utterance included, before it is asked for the final: that silence is
                // what tells the model the word is over. Both slices are empty when the
                // session has heard nothing, and the final then decodes the segmenter's
                // copy whole, preroll included.
                if !feed_utterance(state, out, live, start, audio, gate).await {
                    return false;
                }
                if !feed_utterance(state, out, live, start, tail, gate).await {
                    return false;
                }
                if !finish_utterance(state, out, live, utterance, start as usize, gate).await {
                    return false;
                }
            }
        }
    }
    true
}

/// Give one utterance's session the audio it has not heard.
///
/// The session is created when the utterance first has audio, and hears nothing but its
/// own. Returns false when the session has to end, which means an error frame was sent.
async fn feed_utterance(
    state: &Arc<AppState>,
    out: &mpsc::Sender<Message>,
    live: &mut Option<Live>,
    start: u64,
    audio: &[f32],
    gate: &mut Gate,
) -> bool {
    let start = start as usize;
    // A session for another utterance is one the segmenter has moved past.
    if live.as_ref().is_some_and(|open| open.start != start) {
        *live = None;
    }
    if live.is_none() {
        if audio.is_empty() {
            // Nothing to hear, so there is nothing to open: the final is decoded from the
            // segmenter's own copy of the utterance.
            return true;
        }
        let session = match state.stream_session() {
            Ok(session) => session,
            Err(failure) => return end_with_error(out, &failure).await,
        };
        *live = Some(Live::new(session, start));
    }
    let Some(open) = live.as_mut() else {
        return true;
    };
    if audio.is_empty() {
        return true;
    }
    match open.feed(audio.to_vec()).await {
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

    // Building the detector reads the model and creates a session, which is blocking work:
    // on the runtime thread it stalls every other connection for the length of the load.
    let worker = Arc::clone(&state);
    let built = tokio::task::spawn_blocking(move || segmenter_for(&worker))
        .await
        .unwrap_or_else(|error| Err(format!("the detector task failed: {error}")));
    let mut segmenter = match built {
        Ok(segmenter) => segmenter,
        Err(detail) => {
            tracing::error!(%detail, "the voice activity detector could not be loaded");
            send(
                &out,
                json!({
                    "type": "error",
                    "code": "engine_unavailable",
                    "detail": "the voice activity detector could not be loaded; the server log has the detail",
                }),
            )
            .await;
            drop(out);
            let _ = writer.await;
            return;
        }
    };
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
                            let Event::Utterance { samples, start, .. } = event else {
                                continue;
                            };
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
                                    if let Some(Event::Utterance { samples, start, .. }) =
                                        segmenter.flush()
                                    {
                                        let start = start as usize;
                                        // The frames the segmenter had not completed are
                                        // the last of the utterance; the session hears
                                        // them before its final, as it hears every chunk.
                                        let heard = live
                                            .as_ref()
                                            .filter(|open| open.start == start)
                                            .map_or(0, |open| open.fed);
                                        let rest = samples
                                            .get(heard.min(samples.len())..)
                                            .unwrap_or_default()
                                            .to_vec();
                                        if !feed_utterance(
                                            &state,
                                            &out,
                                            &mut live,
                                            start as u64,
                                            &rest,
                                            &mut gate,
                                        )
                                        .await
                                        {
                                            break;
                                        }
                                        if !finish_utterance(
                                            &state,
                                            &out,
                                            &mut live,
                                            &samples,
                                            start,
                                            &mut gate,
                                        )
                                        .await
                                        {
                                            break;
                                        }
                                    }
                                } else if let Some(Event::Utterance { samples, start, .. }) =
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
            model: "silero-vad".into(),
            probability: 0.5,
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

    /// A segmenter that cuts quickly, so a test can carry a whole utterance boundary in
    /// one chunk.
    fn test_vad() -> VadConfig {
        VadConfig {
            threshold: 0.01,
            frame_ms: 20,
            min_silence_ms: 300,
            min_speech_ms: 100,
            preroll_ms: 100,
            max_utterance_ms: 30_000,
        }
    }

    /// Audio at a level the segmenter counts as speech. Each utterance gets its own level,
    /// so a sample can be traced to the utterance it came from.
    fn voiced(ms: u32, level: f32) -> Vec<f32> {
        vec![level; (STREAM_RATE * ms / 1000) as usize]
    }

    fn quiet(ms: u32) -> Vec<f32> {
        vec![0.0; (STREAM_RATE * ms / 1000) as usize]
    }

    /// A server whose config names the Silero detector and whose models directory is empty.
    fn silero_state() -> Arc<AppState> {
        let dir = tempfile::tempdir().unwrap();
        // The engine loads its dictionary at startup, so a state needs one on disk.
        std::fs::write(dir.path().join("cmudict.dict"), "HELLO  HH AH0 L OW1\n").unwrap();
        let mut config = arioso_config::Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.vad.engine = "silero".to_owned();
        let manifest = arioso_models::Manifest::parse(
            include_str!("../../../models/manifest.json").as_bytes(),
        )
        .unwrap();
        // The temp directory is dropped at the end of this function, which is the point: the
        // state keeps the path, and the file is not there.
        Arc::new(AppState::new(config, manifest, None).unwrap())
    }

    #[test]
    fn a_detector_that_is_not_on_disk_is_refused_before_the_upgrade() {
        // A socket that opens and then answers every frame with the energy detector is the
        // defect F5.1 exists to prevent: the caller asked for a model and would be told
        // nothing.
        let state = silero_state();
        let refusal = preflight(&state).expect("a missing detector is refused");
        assert_eq!(refusal.status(), axum::http::StatusCode::CONFLICT);
    }

    /// What a session is: the utterance it belongs to, and how much of it has been fed.
    type Session = Option<(u64, usize)>;

    /// Everything each utterance's session heard, by the utterance's start.
    type Heard = std::collections::BTreeMap<u64, Vec<f32>>;

    /// Give one utterance's session the audio it has not heard, as `feed_utterance` does.
    ///
    /// A session for another utterance is one the segmenter has moved past, so it is
    /// dropped rather than kept. Empty audio opens nothing.
    fn feed(session: &mut Session, heard: &mut Heard, start: u64, audio: &[f32]) {
        if audio.is_empty() {
            return;
        }
        if session.is_some_and(|(from, _)| from != start) {
            *session = None;
        }
        let already = match *session {
            Some((from, fed)) if from == start => fed,
            _ => 0,
        };
        heard.entry(start).or_default().extend_from_slice(audio);
        *session = Some((start, already + audio.len()));
    }

    /// The plan for one chunk, applied the way the socket applies it.
    ///
    /// The engine is not available in a unit test, so this does what `stream_chunk`,
    /// `feed_utterance` and `finish_utterance` do with the plan: a session is opened by its
    /// first audio, hears each step in turn, and is gone once its utterance is final. A
    /// final with no session of its own decodes the segmenter's copy whole, which is the
    /// fallback `finish_utterance` takes. Returns the utterances the chunk finalised, with
    /// where the segmenter said each began and the audio it kept for it.
    fn drive(
        segmenter: &mut Segmenter,
        chunk: &[f32],
        session: &mut Session,
        heard: &mut Heard,
    ) -> Vec<(u64, Vec<f32>)> {
        let at = segmenter.heard();
        let events = segmenter.push(chunk);
        let open = segmenter.open();
        let steps = plan_chunk(chunk, at, &events, open, *session);
        let mut finals = Vec::new();
        for step in steps {
            match step {
                Step::Hear { start, audio } => feed(session, heard, start, audio),
                Step::Close {
                    start,
                    audio,
                    tail,
                    utterance,
                } => {
                    feed(session, heard, start, audio);
                    feed(session, heard, start, tail);
                    let own = matches!(*session, Some((from, _)) if from == start);
                    if !own {
                        heard.entry(start).or_default().extend_from_slice(utterance);
                    }
                    *session = None;
                    finals.push((start, utterance.to_vec()));
                }
            }
        }
        finals
    }

    #[test]
    fn a_chunk_that_ends_one_utterance_and_begins_the_next_feeds_each_session_its_own_audio() {
        // One WebSocket message carrying the silence that ends the first utterance and the
        // first words of the second. The session for the first was fed the whole message,
        // so its final covered the next utterance's words, and those words were then fed
        // to the new session and decoded a second time.
        let mut segmenter = Segmenter::new(STREAM_RATE, test_vad());
        let mut heard = Heard::new();
        let mut session: Session = None;

        let first = voiced(400, 0.5);
        let finals = drive(&mut segmenter, &first, &mut session, &mut heard);
        assert!(finals.is_empty(), "nothing closes on the first chunk");
        let Some((start, fed)) = session else {
            panic!("the first utterance has a session");
        };
        assert_eq!(fed, first.len(), "the session heard the whole chunk");

        let mut second = quiet(400);
        second.extend(voiced(400, 0.7));
        let finals = drive(&mut segmenter, &second, &mut session, &mut heard);
        assert_eq!(
            finals.len(),
            1,
            "the chunk closes one utterance: {finals:?}"
        );

        // Everything the closing session heard is its own: its speech, then the 300 ms of
        // silence that ended it, and none of the 0.7 speech that began the next utterance.
        let closing = heard.get(&start).expect("the first utterance was heard");
        assert_eq!(closing.len(), first.len() + quiet(300).len());
        assert!(closing[..first.len()].iter().all(|sample| *sample == 0.5));
        assert!(
            closing[first.len()..].iter().all(|sample| *sample == 0.0),
            "the closing session heard the next utterance's speech"
        );

        // The final covers the utterance the segmenter closed, and stops before the next
        // one begins.
        let (next, _) = session.expect("the second utterance has a session");
        assert_eq!(finals[0].0, start);
        assert!(
            start + finals[0].1.len() as u64 <= next,
            "the final runs into the next utterance"
        );

        // The next utterance's speech was fed once, to its own session, as its preroll
        // followed by the speech.
        let fresh = heard.get(&next).expect("the second utterance was heard");
        assert_eq!(fresh.len(), quiet(100).len() + voiced(400, 0.7).len());
        assert_eq!(
            fresh.iter().filter(|sample| **sample == 0.7).count(),
            voiced(400, 0.7).len(),
            "the next utterance's speech is fed exactly once"
        );
    }

    /// Every sample of the segmenter's copy reached that utterance's session once, in order,
    /// and the session heard nothing but trailing silence after it.
    ///
    /// This is the invariant the plan exists to keep: a session belongs to one utterance, so
    /// it hears that utterance's audio from its first sample to its last, and the only other
    /// audio it may hear is the silence that ended it.
    fn assert_heard_its_own(heard: &Heard, start: u64, utterance: &[f32]) {
        let audio = heard
            .get(&start)
            .unwrap_or_else(|| panic!("no session heard the utterance at {start}"));
        assert!(
            audio.len() >= utterance.len(),
            "the session at {start} heard {} of the utterance's {} samples",
            audio.len(),
            utterance.len()
        );
        assert!(
            audio[..utterance.len()] == *utterance,
            "the session at {start} did not hear the utterance's own audio, in order"
        );
        assert!(
            audio[utterance.len()..].iter().all(|sample| *sample == 0.0),
            "the session at {start} heard audio that is not its own"
        );
    }

    #[test]
    fn an_utterance_that_began_in_an_earlier_chunk_hears_its_preroll() {
        // The preroll of an utterance is audio from before its first speech frame, so it can
        // arrive in a chunk that came before the utterance existed, when no session was open
        // for it. The closing chunk was sliced from the utterance's start, which is before
        // that chunk, so the offset clamped to the chunk's first sample: the session was
        // opened at the utterance's start and fed the chunk from its own beginning, and the
        // preroll was never heard. A clipped onset is a word the model guesses at.
        let mut segmenter = Segmenter::new(STREAM_RATE, test_vad());
        let mut heard = Heard::new();
        let mut session: Session = None;

        let before = quiet(200);
        assert!(drive(&mut segmenter, &before, &mut session, &mut heard).is_empty());
        assert!(
            session.is_none(),
            "no utterance is open, so nothing was fed and no session exists"
        );

        let mut closing = voiced(400, 0.5);
        closing.extend(quiet(400));
        let finals = drive(&mut segmenter, &closing, &mut session, &mut heard);
        assert_eq!(finals.len(), 1, "the chunk closes one utterance");
        let (start, utterance) = &finals[0];

        // The last 100 ms of the earlier chunk is the preroll, and it opens the copy.
        assert_eq!(*start, 100 * u64::from(STREAM_RATE) / 1000);
        assert_eq!(
            utterance.len(),
            quiet(100).len() + voiced(400, 0.5).len() + quiet(100).len(),
            "the copy is the preroll, the speech and a short tail"
        );
        assert!(
            utterance[..quiet(100).len()]
                .iter()
                .all(|sample| *sample == 0.0),
            "the preroll is the front of the copy"
        );

        // The session heard the preroll, and heard it once: the copy, exactly.
        assert_heard_its_own(&heard, *start, utterance);
        let audio = heard.get(start).expect("the utterance was heard");
        assert_eq!(audio.len(), utterance.len(), "the copy was fed twice");
        assert_eq!(
            audio.iter().filter(|sample| **sample == 0.5).count(),
            voiced(400, 0.5).len(),
            "the speech was fed once"
        );
    }

    #[test]
    fn a_frame_split_across_chunks_is_heard_in_full_by_its_own_session() {
        // The segmenter holds the samples of a frame that a chunk boundary cut in half. When
        // an utterance closes in the chunk after one, those samples are in the segmenter and
        // not in the chunk, so a chunk sliced from the utterance's offset clamped to its
        // first sample and dropped them: up to a frame of the utterance's own audio, and the
        // frame that starts a word is where the word is.
        //
        // A cut ends on speech, so its copy is the whole utterance and a session that lost
        // samples is short by them, which makes the loss countable.
        let config = VadConfig {
            max_utterance_ms: 1_000,
            ..test_vad()
        };
        let mut segmenter = Segmenter::new(STREAM_RATE, config);
        let mut stream = quiet(300);
        stream.extend(voiced(2_500, 0.5));
        stream.extend(quiet(500));
        stream.extend(voiced(400, 0.7));
        stream.extend(quiet(500));

        let mut heard = Heard::new();
        let mut session: Session = None;
        let mut finals = Vec::new();
        for chunk in stream.chunks(777) {
            finals.extend(drive(&mut segmenter, chunk, &mut session, &mut heard));
        }

        assert_eq!(
            finals.len(),
            4,
            "the two cuts and the pause make four utterances: {finals:?}"
        );
        assert_eq!(heard.len(), 4, "four sessions heard audio");
        for (start, utterance) in &finals {
            assert_heard_its_own(&heard, *start, utterance);
        }
        // The cuts are the first two, and each ends on speech, so nothing may be missing.
        for (start, utterance) in finals.iter().take(2) {
            let audio = heard.get(start).expect("the utterance was heard");
            assert_eq!(
                audio.len(),
                utterance.len(),
                "the session at {start} lost the samples the segmenter was still holding"
            );
            let speech = utterance.iter().filter(|sample| **sample == 0.5).count();
            assert!(speech > 0, "a cut utterance is mostly speech");
            assert_eq!(
                audio.iter().filter(|sample| **sample == 0.5).count(),
                speech,
                "the speech of the utterance at {start} was fed once"
            );
        }
    }

    #[test]
    fn no_sample_reaches_another_utterance_and_none_is_fed_twice() {
        // A stream cut into chunks that line up with neither frames nor utterances: every
        // sample of speech has to reach its own session once, and no other session at all.
        let mut segmenter = Segmenter::new(STREAM_RATE, test_vad());
        let mut stream = quiet(200);
        stream.extend(voiced(300, 0.5));
        stream.extend(quiet(400));
        stream.extend(voiced(300, 0.7));
        stream.extend(quiet(500));
        stream.extend(voiced(300, 0.9));
        stream.extend(quiet(400));

        let mut heard = Heard::new();
        let mut session: Session = None;
        let mut finals = 0;
        for chunk in stream.chunks(777) {
            finals += drive(&mut segmenter, chunk, &mut session, &mut heard).len();
        }

        assert_eq!(finals, 3, "three utterances were final");
        assert_eq!(heard.len(), 3, "three sessions heard audio");
        for (start, audio) in &heard {
            let levels: Vec<f32> = [0.5, 0.7, 0.9]
                .into_iter()
                .filter(|level| audio.iter().any(|sample| sample == level))
                .collect();
            assert_eq!(
                levels.len(),
                1,
                "the session at {start} heard more than one utterance: {levels:?}"
            );
            assert_eq!(
                audio.iter().filter(|sample| **sample == levels[0]).count(),
                voiced(300, levels[0]).len(),
                "the speech of {levels:?} was fed once, to one session"
            );
        }
    }
}
