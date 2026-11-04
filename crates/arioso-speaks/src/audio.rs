//! Playback, and the envelope the waveform is drawn from.
//!
//! The samples come from the engine, so nothing here decodes a file. `rodio` is built with
//! its decoders off and is handed raw samples; a machine with no output device answers
//! [`Player::available`] false and the app offers saving only.

use std::time::Duration;

use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player as Sink, buffer::SamplesBuffer};

/// How much audio one envelope bucket covers.
const BUCKET: f32 = 0.01;

/// The most recent audio, and where the listener is inside it.
pub struct Player {
    device: Option<MixerDeviceSink>,
    sink: Option<Sink>,
    /// Peak per [`BUCKET`] of the last thing played.
    envelope: Vec<f32>,
    /// Seconds of audio the envelope covers.
    duration: f32,
}

impl Player {
    /// Open the default output device, if there is one.
    ///
    /// A device that cannot be opened is not an error to report at startup: the app still
    /// speaks through saving, and a headless machine is a normal place to run a terminal app.
    #[must_use]
    pub fn new() -> Self {
        let device = DeviceSinkBuilder::open_default_sink().ok();
        let sink = device
            .as_ref()
            .map(|device| Sink::connect_new(device.mixer()));
        Self {
            device,
            sink,
            envelope: Vec::new(),
            duration: 0.0,
        }
    }

    /// Whether this machine can play audio at all.
    #[must_use]
    pub fn available(&self) -> bool {
        self.device.is_some() && self.sink.is_some()
    }

    /// Play `samples` from the start, stopping whatever was playing.
    ///
    /// # Errors
    ///
    /// A machine with no output device.
    pub fn play(&mut self, samples: &[f32], rate: u32) -> Result<(), String> {
        let sink = self
            .sink
            .as_ref()
            .ok_or("this machine has no audio output device")?;
        if rate == 0 || samples.is_empty() {
            return Err("there is nothing to play".to_owned());
        }
        self.envelope = envelope(samples, rate);
        self.duration = samples.len() as f32 / rate as f32;
        sink.stop();
        // One channel, and the rate the engine produced: the buffer is fed raw, so nothing
        // here resamples or decodes.
        let Some(channels) = std::num::NonZero::new(1u16) else {
            return Err("a channel count of zero is not playable".to_owned());
        };
        let Some(sample_rate) = std::num::NonZero::new(rate) else {
            return Err("a sample rate of zero is not playable".to_owned());
        };
        sink.append(SamplesBuffer::new(channels, sample_rate, samples.to_vec()));
        sink.play();
        Ok(())
    }

    /// Stop playing, if anything is.
    pub fn stop(&mut self) {
        if let Some(sink) = self.sink.as_ref() {
            sink.stop();
        }
    }

    /// Whether audio is still coming out.
    #[must_use]
    pub fn playing(&self) -> bool {
        self.sink
            .as_ref()
            .is_some_and(|sink| !sink.empty() && self.duration > 0.0)
    }

    /// How far into the audio the listener is, in seconds.
    #[must_use]
    pub fn position(&self) -> f32 {
        self.sink
            .as_ref()
            .map(|sink| {
                sink.get_pos()
                    .min(Duration::from_secs_f32(self.duration.max(0.0)))
            })
            .map_or(0.0, |at| at.as_secs_f32())
    }

    /// The envelope, resampled to `width` columns, around the current position.
    ///
    /// While audio plays this is a three-second window that scrolls with it; when it stops
    /// it is the whole utterance, still, so the wordmark shows what was said rather than
    /// collapsing to a flat line the moment it ends.
    #[must_use]
    pub fn levels(&self, width: usize) -> Vec<f32> {
        if self.envelope.is_empty() || width == 0 {
            return vec![0.0; width];
        }
        let playing = self.playing();
        let (start, span) = if playing {
            let window = 3.0;
            let at = self.position() - window / 2.0;
            (at.max(0.0), window)
        } else {
            (0.0, self.duration.max(0.001))
        };
        (0..width)
            .map(|col| {
                let at = start + span * col as f32 / width as f32;
                let index = (at / BUCKET) as usize;
                let end = ((at + span / width as f32) / BUCKET) as usize;
                self.envelope[index.min(self.envelope.len() - 1)..=end.min(self.envelope.len() - 1)]
                    .iter()
                    .copied()
                    .fold(0.0f32, f32::max)
            })
            .collect()
    }
}

/// Peak per bucket, scaled so the loudest bucket reaches one.
///
/// Scaled rather than absolute: a quiet utterance drawn at its true level is a flat line,
/// and the waveform is there to show shape.
fn envelope(samples: &[f32], rate: u32) -> Vec<f32> {
    let per_bucket = ((rate as f32 * BUCKET) as usize).max(1);
    let mut peaks: Vec<f32> = samples
        .chunks(per_bucket)
        .map(|chunk| {
            chunk
                .iter()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
        })
        .collect();
    let loudest = peaks.iter().copied().fold(0.0f32, f32::max);
    if loudest > 0.0 {
        for peak in &mut peaks {
            *peak = (*peak / loudest).min(1.0);
        }
    }
    peaks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_envelope_is_scaled_to_the_loudest_bucket() {
        // Two buckets: a quiet one and a loud one, at 100 buckets a second.
        let mut samples = vec![0.1f32; 100];
        samples.extend(vec![0.5f32; 100]);
        let peaks = envelope(&samples, 10_000);
        assert_eq!(peaks.len(), 2);
        assert!((peaks[0] - 0.2).abs() < 0.001, "{peaks:?}");
        assert!((peaks[1] - 1.0).abs() < 0.001, "{peaks:?}");
    }

    #[test]
    fn silence_has_no_levels_to_draw() {
        let peaks = envelope(&[0.0; 500], 10_000);
        assert!(peaks.iter().all(|peak| *peak == 0.0));
    }

    #[test]
    fn levels_fill_the_width_even_when_the_audio_is_short() {
        let player = Player {
            device: None,
            sink: None,
            envelope: vec![0.5, 1.0],
            duration: 0.02,
        };
        assert_eq!(player.levels(6).len(), 6);
    }
}
