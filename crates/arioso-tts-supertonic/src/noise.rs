//! A deterministic source of normally distributed numbers.
//!
//! The model starts from noise and integrates towards speech, so the noise is the one random
//! step in a render. It is seeded rather than drawn from the operating system: F3.7 says the
//! same text at the same speed and engine version gives the same audio, and an engine that
//! answers differently on every call cannot be tested, cached, or compared against itself.
//!
//! SplitMix64 is the generator and Box–Muller the transform. Neither is cryptographic and
//! neither needs to be: this is noise for a model, not a key.

/// The first value of the stream.
#[must_use]
pub(crate) fn normal(seed: u64, count: usize) -> Vec<f32> {
    let mut state = seed;
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        let (a, b) = (uniform(&mut state), uniform(&mut state));
        // Box–Muller needs a value in (0, 1]; the generator can return exactly 0.
        let a = if a <= 0.0 { f64::EPSILON } else { a };
        let radius = (-2.0 * a.ln()).sqrt();
        let angle = std::f64::consts::TAU * b;
        out.push((radius * angle.cos()) as f32);
        if out.len() < count {
            out.push((radius * angle.sin()) as f32);
        }
    }
    out
}

/// One number in `(0, 1)`.
fn uniform(state: &mut u64) -> f64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // The top 53 bits, which is what an f64 mantissa holds, so every value is distinct.
    ((z >> 11) as f64) / ((1u64 << 53) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_noise() {
        assert_eq!(normal(7, 16), normal(7, 16));
    }

    #[test]
    fn a_different_seed_gives_different_noise() {
        assert_ne!(normal(7, 16), normal(8, 16));
    }

    #[test]
    fn the_noise_is_centred_and_has_the_variance_a_normal_has() {
        // Not a statistical proof, a tripwire: a generator stuck at zero, or one that returns
        // values outside the distribution, fails here.
        let values = normal(1, 100_000);
        let mean = values.iter().map(|v| f64::from(*v)).sum::<f64>() / values.len() as f64;
        let variance = values
            .iter()
            .map(|v| (f64::from(*v) - mean).powi(2))
            .sum::<f64>()
            / values.len() as f64;
        assert!(mean.abs() < 0.02, "mean {mean}");
        assert!((variance - 1.0).abs() < 0.05, "variance {variance}");
    }

    #[test]
    fn every_value_is_finite() {
        assert!(normal(3, 10_000).iter().all(|v| v.is_finite()));
    }
}
