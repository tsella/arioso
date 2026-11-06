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

/// Points in the window table.
///
/// The window is a smooth curve over `-1..=1`; 2048 points with linear interpolation between
/// them is far finer than the ear and turns two transcendental calls per tap into two table
/// reads and a multiply.
const WINDOW_STEPS: usize = 2048;

/// The Kaiser window, tabulated once.
///
/// It depends only on the constants above, so it is built on first use and shared by every
/// conversion. Without it the inner loop ran the Bessel series fifteen times per tap per
/// output sample: about one and a half million series a second of audio.
fn window_table() -> &'static [f32] {
    static TABLE: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        (0..=WINDOW_STEPS)
            .map(|step| {
                let u = (step as f32 / WINDOW_STEPS as f32) * 2.0 - 1.0;
                window_exact(u)
            })
            .collect()
    })
}

/// The window at `u`, read from the table.
fn window(u: f32) -> f32 {
    let table = window_table();
    let at = ((u + 1.0) * 0.5 * WINDOW_STEPS as f32).clamp(0.0, WINDOW_STEPS as f32);
    let low = at.floor() as usize;
    let high = (low + 1).min(WINDOW_STEPS);
    let fraction = at - low as f32;
    table[low] * (1.0 - fraction) + table[high] * fraction
}

/// Taps per phase, counting both sides of the interpolation point.
const TAPS_PER_PHASE: usize = (TAPS as usize) * 2;

/// Phases in the polyphase table.
///
/// The tap weights depend on where the output sample falls between two input samples, and on
/// nothing else. Quantising that fraction to one part in 1024 moves the interpolation point by
/// under a thousandth of a sample, which is a twenty-microsecond error at 48 kHz: below the
/// resolution of the buffer, let alone the ear.
const PHASES: usize = 1024;

/// Every tap weight for every phase, with the sum each phase needs for normalisation.
///
/// Without it the inner loop evaluated a sine and the Bessel window for each of thirty-two
/// taps of each of forty-eight thousand output samples a second: a second and a half of audio
/// took four hundred milliseconds.
struct Kernel {
    weights: Vec<f32>,
    sums: Vec<f32>,
}

impl Kernel {
    /// Build the table for a conversion ratio.
    fn build(cutoff: f64) -> Self {
        let mut weights = Vec::with_capacity(PHASES * TAPS_PER_PHASE);
        let mut sums = Vec::with_capacity(PHASES);
        for phase in 0..PHASES {
            let fraction = phase as f64 / PHASES as f64;
            let mut sum = 0.0_f32;
            for tap in -TAPS + 1..=TAPS {
                let offset = fraction - f64::from(tap);
                let weight = if offset.abs() > f64::from(TAPS) {
                    0.0
                } else {
                    (sinc(offset * cutoff) * f64::from(window((offset / f64::from(TAPS)) as f32)))
                        as f32
                };
                sum += weight;
                weights.push(weight);
            }
            sums.push(sum);
        }
        Self { weights, sums }
    }

    /// The weights for one phase.
    fn phase(&self, phase: usize) -> &[f32] {
        let start = (phase % PHASES) * TAPS_PER_PHASE;
        &self.weights[start..start + TAPS_PER_PHASE]
    }
}

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
    let kernel = Kernel::build(cutoff);

    let mut out = Vec::with_capacity(length);
    for index in 0..length {
        let at = index as f64 * ratio;
        let base = at.floor() as i64;
        let fraction = at - base as f64;
        let phase = ((fraction * PHASES as f64).round() as usize) % PHASES;
        let weights = kernel.phase(phase);
        let mut sum = 0.0f32;
        for (slot, tap) in (-TAPS + 1..=TAPS).enumerate() {
            let weight = weights[slot];
            if weight == 0.0 {
                continue;
            }
            // Clamp at the edges rather than pad with zeros: a zero pad fades the first and
            // last few milliseconds of every utterance, which is a click on a short one.
            let sample = samples[(base + i64::from(tap)).clamp(0, last as i64) as usize];
            sum += sample * weight;
        }
        let total = kernel.sums[phase];
        // Normalised, so a window whose weights do not sum to one does not change the level.
        out.push(if total.abs() < f32::EPSILON {
            0.0
        } else {
            sum / total
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

/// The Kaiser window over `-1..=1`, evaluated.
fn window_exact(u: f32) -> f32 {
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
    #[ignore = "a measurement, not a check"]
    fn how_long_ten_seconds_take() {
        let input = sine(440.0, 22_050, 10.0);
        for _ in 0..3 {
            let start = std::time::Instant::now();
            let out = resample(&input, 22_050, 48_000);
            let elapsed = start.elapsed();
            eprintln!(
                "22.05 kHz to 48 kHz, 10 s: {:.1} ms for {} samples",
                elapsed.as_secs_f64() * 1000.0,
                out.len()
            );
        }
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
