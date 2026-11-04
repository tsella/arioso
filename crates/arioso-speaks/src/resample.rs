//! Resampling to the device rate.
//!
//! `rodio` converts every source to the mixer's rate with linear interpolation. A 22.05 kHz
//! voice played on a 48 kHz device is a 2.18× upsample through that filter, which leaves
//! audible roughness on sibilants: the same audio written to a file and played by the
//! system sounds cleaner, because the system resamples properly.
//!
//! So the samples are converted here, before they reach the sink, and the mixer is handed a
//! buffer already at its own rate. The filter is a Kaiser-windowed sinc: the stopband is far
//! enough down that a voice keeps its consonants.

/// Taps on each side of the interpolation point.
///
/// Sixteen is the usual compromise: below about eight the aliasing is audible on speech,
/// above about thirty-two the cost buys nothing a listener can hear.
const TAPS: i32 = 16;

/// Kaiser window shape. Eight is a good stopband for audio.
const BETA: f32 = 8.0;

/// Fraction of the input Nyquist the filter passes. Slightly under one, so the transition
/// band sits below the fold rather than across it.
const CUTOFF: f64 = 0.95;

/// Convert `samples` from `from` to `to` hertz.
///
/// A matching rate is a copy, not a pass through the filter: the common case should cost
/// nothing and must not colour the sound at all.
#[must_use]
pub fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if samples.is_empty() || from == 0 || to == 0 || from == to {
        return samples.to_vec();
    }
    let ratio = f64::from(from) / f64::from(to);
    // Outputs are counted from the input length so the duration is preserved, which matters
    // for the waveform: a buffer that grows by a sample per second drifts against the audio.
    let length = ((samples.len() as f64) * f64::from(to) / f64::from(from)).round() as usize;
    let cutoff = f64::min(1.0, f64::from(to) / f64::from(from)) * CUTOFF;
    let last = samples.len() - 1;

    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let at = index as f64 * ratio;
        let base = at.floor() as i64;
        let mut sum = 0.0f64;
        let mut weights = 0.0f64;
        for tap in -TAPS + 1..=TAPS {
            let offset = at - (base + i64::from(tap)) as f64;
            if offset.abs() > f64::from(TAPS) {
                continue;
            }
            let weight =
                sinc(offset * cutoff) * f64::from(window((offset / f64::from(TAPS)) as f32));
            // Clamp at the edges rather than pad with zeros: a zero pad fades the first and
            // last few milliseconds of every utterance, which is a click on a short one.
            let sample = samples[(base + i64::from(tap)).clamp(0, last as i64) as usize];
            sum += f64::from(sample) * weight;
            weights += weight;
        }
        // Normalised, so a window whose weights do not sum to one does not change the level.
        out.push(if weights.abs() < f64::EPSILON {
            0.0
        } else {
            (sum / weights) as f32
        });
    }
    out
}

/// `sin(πx) / (πx)`, and one at zero.
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 {
        1.0
    } else {
        let angle = std::f64::consts::PI * x;
        angle.sin() / angle
    }
}

/// The Kaiser window over `-1..=1`.
fn window(u: f32) -> f32 {
    let inside = 1.0 - u * u;
    if inside <= 0.0 {
        return 0.0;
    }
    bessel_i0(BETA * inside.sqrt()) / bessel_i0(BETA)
}

/// The zeroth-order modified Bessel function of the first kind, by its series.
///
/// The series converges in a handful of terms for the arguments a window needs, so there is
/// no table and no dependency.
fn bessel_i0(x: f32) -> f32 {
    let quarter = (x / 2.0) * (x / 2.0);
    let mut sum = 1.0f32;
    let mut term = 1.0f32;
    let mut order = 1.0f32;
    while order < 64.0 {
        term *= quarter / (order * order);
        sum += term;
        if term < sum * 1e-7 {
            break;
        }
        order += 1.0;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sine at `hz`, `seconds` long.
    fn sine(hz: f32, rate: u32, seconds: f32) -> Vec<f32> {
        let count = (rate as f32 * seconds) as usize;
        (0..count)
            .map(|index| (std::f32::consts::TAU * hz * index as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    /// Upward zero crossings per second, which is the frequency of a clean sine.
    fn crossings(samples: &[f32], rate: u32) -> f32 {
        let up = samples
            .windows(2)
            .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
            .count();
        up as f32 * rate as f32 / samples.len() as f32
    }

    #[test]
    fn a_matching_rate_is_the_same_samples() {
        let input = sine(440.0, 48_000, 0.05);
        assert_eq!(resample(&input, 48_000, 48_000), input);
    }

    #[test]
    fn the_duration_is_preserved() {
        let input = sine(440.0, 22_050, 0.5);
        let out = resample(&input, 22_050, 48_000);
        let seconds = out.len() as f32 / 48_000.0;
        assert!((seconds - 0.5).abs() < 0.001, "{seconds} seconds");
    }

    #[test]
    fn a_tone_keeps_its_frequency() {
        let input = sine(1_000.0, 22_050, 0.5);
        let out = resample(&input, 22_050, 48_000);
        let measured = crossings(&out, 48_000);
        assert!((measured - 1_000.0).abs() < 20.0, "{measured} Hz, not 1000");
    }

    #[test]
    fn the_level_is_preserved() {
        let input = sine(500.0, 22_050, 0.5);
        let out = resample(&input, 22_050, 48_000);
        // Skip the edges, where the clamped tail is still settling.
        let middle = &out[1_000..out.len() - 1_000];
        let peak = middle
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        assert!((peak - 0.5).abs() < 0.02, "peak {peak}, not 0.5");
    }

    #[test]
    fn a_constant_stays_constant() {
        let input = vec![0.25f32; 2_000];
        let out = resample(&input, 22_050, 48_000);
        let worst = out
            .iter()
            .fold(0.0f32, |worst, sample| worst.max((sample - 0.25).abs()));
        assert!(worst < 0.01, "drifted by {worst}");
    }

    #[test]
    fn silence_stays_silent() {
        let out = resample(&vec![0.0f32; 1_000], 22_050, 48_000);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn a_downward_conversion_also_holds_the_tone() {
        let input = sine(1_000.0, 48_000, 0.5);
        let out = resample(&input, 48_000, 22_050);
        let measured = crossings(&out, 22_050);
        assert!((measured - 1_000.0).abs() < 20.0, "{measured} Hz");
    }
}
