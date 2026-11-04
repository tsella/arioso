//! Voice activity detection, and the segmentation that follows from it.
//!
//! Two things live here and they are separate on purpose: a [`Gate`] answers whether one
//! frame holds speech, and the [`Segmenter`] turns a stream of those answers into
//! utterances. The energy gate needs no model, so the crate stays free of dependencies and
//! compiles anywhere; a neural gate is built elsewhere, on the runtime that already runs
//! the speech models, and reaches this crate only through the trait.
//!
//! The segmenter keeps a little audio before the first speech frame, because the onset
//! of a word is quiet and cutting at the threshold clips it.

/// Level above which a frame counts as speech, as a root mean square of samples in
/// the range -1.0 to 1.0.
pub const DEFAULT_THRESHOLD: f32 = 0.01;

/// How the stream is cut into utterances.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// Root mean square above which a frame is speech.
    pub threshold: f32,
    /// Frame length in milliseconds.
    pub frame_ms: u32,
    /// Silence that ends an utterance.
    pub min_silence_ms: u32,
    /// Shortest utterance worth transcribing.
    pub min_speech_ms: u32,
    /// Audio kept from before the first speech frame.
    pub preroll_ms: u32,
    /// Longest utterance before it is cut, whether or not the speaker paused.
    ///
    /// Without this, a stream that never goes quiet grows without limit.
    pub max_utterance_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            frame_ms: 20,
            min_silence_ms: 500,
            min_speech_ms: 200,
            preroll_ms: 200,
            max_utterance_ms: 30_000,
        }
    }
}

/// What decides whether a frame holds speech.
///
/// The segmenter cuts on this answer and knows nothing about how it was reached: a level
/// against a threshold needs no model, a neural detector needs one, and both produce the
/// same boundaries.
pub trait Gate: Send {
    /// Whether this frame holds speech.
    fn speech(&mut self, frame: &[f32]) -> bool;
}

/// A frame is speech when its level clears a threshold.
///
/// The gate that needs nothing installed, and the one a stream uses until someone asks for
/// another.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Energy {
    /// Root mean square above which a frame counts as speech.
    pub threshold: f32,
}

impl Gate for Energy {
    fn speech(&mut self, frame: &[f32]) -> bool {
        rms(frame) > self.threshold
    }
}

/// What the segmenter found in the audio it was given.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// Speech started at this many samples into the stream.
    SpeechStart {
        /// Sample index where the utterance began, including preroll.
        at: u64,
    },
    /// A finished utterance, ready to transcribe.
    Utterance {
        /// The audio, mono.
        samples: Vec<f32>,
        /// Sample index where it began.
        start: u64,
        /// Sample index one past the last sample the segmenter read for this utterance,
        /// which is where it closed.
        ///
        /// `samples` is trimmed to the speech plus a short tail, so it is shorter than
        /// `end - start`: the silence that ended the utterance is behind this index, and a
        /// caller that feeds the utterance to a streaming model reads the stream up to
        /// here. Without it the caller has to guess the boundary, and a guess that runs to
        /// the end of the chunk it is holding feeds the next utterance's words to this one.
        end: u64,
    },
}

/// Cuts a stream of samples into utterances.
pub struct Segmenter {
    config: Config,
    rate: u32,
    /// What decides whether a frame is speech.
    gate: Box<dyn Gate>,
    /// Samples per frame, at least one.
    frame_len: usize,
    /// Audio not yet divided into frames.
    pending: Vec<f32>,
    /// Frames of preroll, newest last.
    preroll: Vec<f32>,
    /// The utterance being collected.
    speech: Vec<f32>,
    /// Consecutive silent frames since the last speech frame.
    silence_frames: u32,
    /// Frames of the open utterance that were speech.
    speech_frames: u32,
    /// Length of the open utterance at the last speech frame.
    last_speech_len: usize,
    /// Whether an utterance is open.
    speaking: bool,
    /// Samples consumed so far, for the reported offsets.
    consumed: u64,
    /// Sample index where the open utterance began.
    started_at: u64,
}

impl Segmenter {
    /// Build a segmenter for a stream at `rate`, cutting on the energy gate.
    #[must_use]
    pub fn new(rate: u32, config: Config) -> Self {
        let gate = Energy {
            threshold: config.threshold,
        };
        Self::with_gate(rate, config, Box::new(gate))
    }

    /// Build a segmenter that cuts on `gate`.
    ///
    /// The frame length stays the segmenter's: a gate that needs a particular one says so by
    /// refusing the frames it is given, and the caller that chose the gate is the caller that
    /// knows what it needs.
    #[must_use]
    pub fn with_gate(rate: u32, config: Config, gate: Box<dyn Gate>) -> Self {
        let frame_len = ((u64::from(rate) * u64::from(config.frame_ms)) / 1000).max(1) as usize;
        Self {
            config,
            rate,
            gate,
            frame_len,
            pending: Vec::new(),
            preroll: Vec::new(),
            speech: Vec::new(),
            silence_frames: 0,
            speech_frames: 0,
            last_speech_len: 0,
            speaking: false,
            consumed: 0,
            started_at: 0,
        }
    }

    /// The rate this segmenter was built for.
    #[must_use]
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Add audio and collect whatever it completes.
    ///
    /// Whole frames are read straight out of `samples`, so a chunk of audio costs one pass
    /// and no allocations. Copying each frame out of a growing buffer and shifting the tail
    /// down moved the same bytes once per frame: a one second chunk moved half a megabyte
    /// to deliver fifty frames of twenty milliseconds.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Event> {
        let mut events = Vec::new();
        let mut rest = samples;

        // Finish the frame left over from the last call, if there is one. Only this path
        // copies, and only up to one frame.
        if !self.pending.is_empty() {
            let needed = self.frame_len - self.pending.len();
            let take = needed.min(rest.len());
            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.pending.len() < self.frame_len {
                return events;
            }
            let mut frame = std::mem::take(&mut self.pending);
            self.frame(&frame, &mut events);
            // Keep the allocation: the next push that splits a frame reuses it.
            frame.clear();
            self.pending = frame;
        }

        let mut chunks = rest.chunks_exact(self.frame_len);
        for chunk in &mut chunks {
            self.frame(chunk, &mut events);
        }
        let remainder = chunks.remainder();
        if !remainder.is_empty() {
            self.pending.extend_from_slice(remainder);
        }
        events
    }

    /// The utterance being collected, with its start, if one is open.
    ///
    /// A caller that wants a partial transcript reads this between pushes.
    #[must_use]
    pub fn open(&self) -> Option<(&[f32], u64)> {
        if self.speech.is_empty() {
            None
        } else {
            Some((&self.speech, self.started_at))
        }
    }

    /// Samples handed to this segmenter, including the ones not yet a whole frame.
    ///
    /// A caller that slices the stream it pushes needs the index of the next sample, which
    /// is this value: `consumed` alone is short by whatever the last push left over.
    #[must_use]
    pub fn heard(&self) -> u64 {
        self.consumed + self.pending.len() as u64
    }

    /// Close the open utterance, if any, at the end of the stream.
    pub fn flush(&mut self) -> Option<Event> {
        if !self.speaking || self.speech.is_empty() {
            return None;
        }
        let samples = std::mem::take(&mut self.speech);
        self.speaking = false;
        self.silence_frames = 0;
        self.preroll.clear();
        let speech_ms = self.speech_frames * self.config.frame_ms;
        self.speech_frames = 0;
        let start = self.started_at;
        if speech_ms < self.config.min_speech_ms {
            return None;
        }
        Some(Event::Utterance {
            samples,
            start,
            end: self.heard(),
        })
    }

    fn frame(&mut self, frame: &[f32], events: &mut Vec<Event>) {
        let speech = self.gate.speech(frame);
        let frame_len = frame.len();

        if !self.speaking {
            self.consumed += frame_len as u64;
            if !speech {
                self.remember(frame);
                return;
            }
            // The frame that starts the utterance is not preroll: it is the utterance's
            // first frame, and remembering it here as well put a copy of it at the front of
            // every utterance, so a model heard it twice. The count is added before the
            // start is taken from it, because the utterance begins with this frame.
            self.speaking = true;
            self.speech = std::mem::take(&mut self.preroll);
            self.speech.extend_from_slice(frame);
            self.started_at = self.consumed.saturating_sub(self.speech.len() as u64);
            self.silence_frames = 0;
            self.speech_frames = 1;
            self.last_speech_len = self.speech.len();
            events.push(Event::SpeechStart {
                at: self.started_at,
            });
            return;
        }

        self.speech.extend_from_slice(frame);
        self.silence_frames = if speech { 0 } else { self.silence_frames + 1 };
        if speech {
            self.speech_frames += 1;
            self.last_speech_len = self.speech.len();
        }
        self.consumed += frame_len as u64;

        let silent_ms = self.silence_frames * self.config.frame_ms;
        let too_long = self.speech_ms() >= self.config.max_utterance_ms;
        if silent_ms < self.config.min_silence_ms && !too_long {
            return;
        }
        let mut samples = std::mem::take(&mut self.speech);
        self.speaking = false;
        self.silence_frames = 0;
        let speech_ms = self.speech_frames * self.config.frame_ms;
        self.speech_frames = 0;
        if speech_ms < self.config.min_speech_ms {
            return;
        }
        // Keep a short tail after the last speech frame and drop the rest: the silence
        // that ended the utterance is not part of it.
        let tail = (u64::from(self.rate) * u64::from(self.config.frame_ms) * 5 / 1000) as usize;
        samples.truncate((self.last_speech_len + tail).min(samples.len()));
        events.push(Event::Utterance {
            samples,
            start: self.started_at,
            end: self.consumed,
        });
    }

    /// How much of the open utterance is speech.
    fn speech_ms(&self) -> u32 {
        self.speech_frames * self.config.frame_ms
    }

    /// Keep the last `preroll_ms` of audio, for the start of the next utterance.
    fn remember(&mut self, frame: &[f32]) {
        self.preroll.extend_from_slice(frame);
        let keep = ((u64::from(self.rate) * u64::from(self.config.preroll_ms)) / 1000) as usize;
        if keep == 0 {
            self.preroll.clear();
        } else if self.preroll.len() > keep {
            // One move instead of a drain per frame. The window is bounded by
            // `preroll_ms`, so this is a copy of at most that much audio, and it only runs
            // while the buffer is over its size: a ring buffer would save the copy and
            // cost the wholesale handover to `speech` that starts an utterance.
            let excess = self.preroll.len() - keep;
            self.preroll.copy_within(excess.., 0);
            self.preroll.truncate(keep);
        }
    }
}

impl std::fmt::Debug for Segmenter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segmenter")
            .field("config", &self.config)
            .field("rate", &self.rate)
            .field("frame_len", &self.frame_len)
            .field("speaking", &self.speaking)
            .finish_non_exhaustive()
    }
}

/// Root mean square of a frame.
fn rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f32 = frame.iter().map(|sample| sample * sample).sum();
    (sum / frame.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    fn tone(ms: u32) -> Vec<f32> {
        let count = (RATE * ms / 1000) as usize;
        (0..count)
            .map(|i| 0.3 * (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / RATE as f32).sin())
            .collect()
    }

    fn silence(ms: u32) -> Vec<f32> {
        vec![0.0; (RATE * ms / 1000) as usize]
    }

    /// A gate that answers from a marker in the audio rather than its level.
    ///
    /// A frame whose first sample is below -0.9 is speech, whatever its energy. Silence is
    /// never speech here, which the energy gate would have called speech if it were loud.
    struct Marker;

    impl Gate for Marker {
        fn speech(&mut self, frame: &[f32]) -> bool {
            frame.first().is_some_and(|first| *first < -0.9)
        }
    }

    #[test]
    fn the_segmenter_cuts_on_the_gate_it_was_given() {
        // The rule used to live inside `frame`, so a different detector could not be used
        // without editing the segmenter. A gate that ignores the level proves the segmenter
        // now follows the answer it is given.
        let config = Config {
            min_silence_ms: 100,
            min_speech_ms: 0,
            preroll_ms: 0,
            ..Config::default()
        };
        let mut segmenter = Segmenter::with_gate(RATE, config, Box::new(Marker));

        // Loud, and not speech to this gate: the energy detector would have opened here.
        let mut loud = vec![0.9_f32; 320];
        loud[0] = 0.5;
        let events = segmenter.push(&loud);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::SpeechStart { .. })),
            "the level opened an utterance the gate refused"
        );

        // Quiet, and speech to this gate: the energy detector would have ignored it.
        let mut quiet = vec![0.0_f32; 320];
        quiet[0] = -1.0;
        let events = segmenter.push(&quiet);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::SpeechStart { .. })),
            "the gate opened an utterance and the segmenter ignored it: {events:?}"
        );
    }

    #[test]
    fn frames_split_across_pushes_are_the_same_as_one_push() {
        // The framing changed from copying frames out of a buffer to reading them from the
        // caller's slice, so a frame that straddles two pushes is the case to pin.
        let rate = 16_000u32;
        let config = Config::default();
        let mut audio = vec![0.0f32; rate as usize * 3];
        for sample in &mut audio[rate as usize / 2..rate as usize] {
            *sample = 0.5;
        }
        for sample in &mut audio[rate as usize * 2..rate as usize * 5 / 2] {
            *sample = 0.5;
        }

        let shape = |events: Vec<Event>| -> Vec<(u8, usize)> {
            events
                .into_iter()
                .map(|event| match event {
                    Event::SpeechStart { .. } => (0, 0),
                    Event::Utterance { samples, .. } => (1, samples.len()),
                })
                .collect()
        };

        let mut whole = Segmenter::new(rate, config);
        let one = shape(whole.push(&audio));
        let mut split = Segmenter::new(rate, config);
        let mut many = Vec::new();
        for chunk in audio.chunks(123) {
            many.extend(split.push(chunk));
        }
        assert_eq!(shape(many), one);
        assert!(!one.is_empty(), "the fixture has to produce speech");
    }

    #[test]
    fn a_burst_between_silences_is_one_utterance() {
        let mut segmenter = Segmenter::new(RATE, Config::default());
        let mut audio = silence(500);
        audio.extend(tone(800));
        audio.extend(silence(1000));

        let events = segmenter.push(&audio);
        let utterances: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::Utterance { samples, .. } => Some(samples.len()),
                Event::SpeechStart { .. } => None,
            })
            .collect();
        assert_eq!(utterances.len(), 1, "one utterance: {events:?}");
        // 800 ms of speech, 200 ms of preroll, and a 100 ms tail: the trailing
        // silence that ended the utterance is trimmed off.
        let ms = utterances[0] * 1000 / RATE as usize;
        assert!(
            (1000..=1200).contains(&ms),
            "about the burst length, got {ms} ms"
        );
    }

    #[test]
    fn silence_produces_nothing() {
        let mut segmenter = Segmenter::new(RATE, Config::default());
        let events = segmenter.push(&silence(2000));
        assert!(events.is_empty(), "{events:?}");
        assert!(segmenter.flush().is_none());
    }

    #[test]
    fn audio_arriving_in_small_pieces_is_handled() {
        let mut segmenter = Segmenter::new(RATE, Config::default());
        let mut audio = tone(600);
        audio.extend(silence(900));
        let mut events = Vec::new();
        for chunk in audio.chunks(157) {
            events.extend(segmenter.push(chunk));
        }
        let count = events
            .iter()
            .filter(|event| matches!(event, Event::Utterance { .. }))
            .count();
        assert_eq!(count, 1, "chunk boundaries must not split an utterance");
    }

    #[test]
    fn a_stream_that_never_pauses_is_closed_by_flush() {
        let mut segmenter = Segmenter::new(RATE, Config::default());
        let events = segmenter.push(&tone(1000));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Utterance { .. })),
            "no pause, no utterance yet"
        );
        let Some(Event::Utterance { samples, .. }) = segmenter.flush() else {
            panic!("flush must close the open utterance");
        };
        assert!(samples.len() >= RATE as usize * 9 / 10);
    }

    #[test]
    fn a_click_too_short_to_be_speech_is_dropped() {
        let mut segmenter = Segmenter::new(RATE, Config::default());
        let mut audio = silence(300);
        audio.extend(tone(40));
        audio.extend(silence(1000));
        let events = segmenter.push(&audio);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::Utterance { .. })),
            "40 ms is under min_speech_ms: {events:?}"
        );
    }

    #[test]
    fn the_utterance_starts_before_the_threshold_is_crossed() {
        let mut segmenter = Segmenter::new(RATE, Config::default());
        let mut audio = silence(400);
        audio.extend(tone(600));
        audio.extend(silence(900));
        let events = segmenter.push(&audio);
        let start = events
            .iter()
            .find_map(|event| match event {
                Event::SpeechStart { at } => Some(*at),
                Event::Utterance { .. } => None,
            })
            .expect("speech must start");
        // Preroll is 200 ms, so the start is before the 400 ms of leading silence ends.
        assert!(start <= 200 * u64::from(RATE) / 1000, "start at {start}");
    }

    #[test]
    fn an_utterance_that_never_pauses_is_cut_at_the_limit() {
        let config = Config {
            max_utterance_ms: 1000,
            ..Config::default()
        };
        let mut segmenter = Segmenter::new(RATE, config);
        let events = segmenter.push(&tone(3000));
        let cut: Vec<usize> = events
            .iter()
            .filter_map(|event| match event {
                Event::Utterance { samples, .. } => Some(samples.len()),
                Event::SpeechStart { .. } => None,
            })
            .collect();
        assert!(!cut.is_empty(), "a long utterance must be cut");
        let ms = cut[0] * 1000 / RATE as usize;
        assert!(
            (900..=1200).contains(&ms),
            "cut near the limit, got {ms} ms"
        );
    }

    #[test]
    fn the_event_says_where_the_utterance_ended() {
        // The socket slices the chunk it just pushed by this index. If it were the end of
        // the push instead, the speech that followed the silence would be handed to the
        // utterance that closed.
        let mut segmenter = Segmenter::new(RATE, Config::default());
        let mut audio = silence(500);
        audio.extend(tone(600));
        audio.extend(silence(900));
        let events = segmenter.push(&audio);
        let Some(Event::Utterance {
            samples,
            start,
            end,
        }) = events
            .iter()
            .find(|event| matches!(event, Event::Utterance { .. }))
        else {
            panic!("one utterance: {events:?}");
        };
        // 500 ms of leading silence, 600 ms of speech, and the 500 ms of silence that
        // ended it: the close is there, and the 400 ms of silence after it is not.
        assert_eq!(*end, 1600 * u64::from(RATE) / 1000, "end at {end}");
        assert!(*end <= segmenter.heard());
        assert_eq!(segmenter.heard(), audio.len() as u64);
        // The audio is trimmed to the speech and a short tail, so it is shorter.
        assert!(samples.len() as u64 <= *end - *start);
    }

    #[test]
    fn the_frame_that_starts_an_utterance_is_not_preroll() {
        // Remembering that frame as preroll and then appending it again put a copy of it at
        // the front of every utterance, so a model heard one frame of it twice.
        let mut segmenter = Segmenter::new(RATE, Config::default());
        let audio = tone(100);
        let events = segmenter.push(&audio);
        let Some((buffer, start)) = segmenter.open() else {
            panic!("speech must open an utterance: {events:?}");
        };
        assert_eq!(buffer.len(), audio.len());
        assert_eq!(start, 0);

        // Leading silence is preroll, and the utterance still begins with it.
        let mut segmenter = Segmenter::new(RATE, Config::default());
        let mut audio = silence(400);
        audio.extend(tone(100));
        segmenter.push(&audio);
        let Some((buffer, start)) = segmenter.open() else {
            panic!("speech must open an utterance");
        };
        // 200 ms of preroll and 100 ms of speech, with no frame counted twice.
        assert_eq!(buffer.len(), RATE as usize * 300 / 1000);
        assert_eq!(start, 200 * u64::from(RATE) / 1000);
    }

    #[test]
    fn two_bursts_are_two_utterances() {
        let mut segmenter = Segmenter::new(RATE, Config::default());
        let mut audio = silence(300);
        audio.extend(tone(500));
        audio.extend(silence(900));
        audio.extend(tone(500));
        audio.extend(silence(900));
        let events = segmenter.push(&audio);
        let count = events
            .iter()
            .filter(|event| matches!(event, Event::Utterance { .. }))
            .count();
        assert_eq!(count, 2, "{events:?}");
    }
}
