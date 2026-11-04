//! Ordered dithering: a terminal cell has no opacity, so density stands in for it.
//!
//! A 4×4 Bayer matrix decides, per cell, whether the next step of the ramp is inked. The
//! result is a gradient with a visible grid rather than a banded one, which is the look the
//! waveform and the progress bar share.

/// The 4×4 Bayer matrix, in the order the thresholds are compared.
pub const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// Five densities, from empty to full.
pub const RAMP: [&str; 5] = [" ", "░", "▒", "▓", "█"];

/// The glyph for one cell at `level` in `0.0..=1.0`.
///
/// The level is scaled onto the ramp, and the matrix decides whether the fractional part
/// rounds up for this cell. Two cells at the same level on the same row get the same glyph;
/// the row above gets a different one, which is what makes the edge look halftoned.
/// Whether this level puts a mark in the cell at all.
///
/// Silence draws nothing, so a caller that wants to know whether something is already in a
/// cell asks this rather than comparing the glyph it got.
#[must_use]
pub fn has_ink(level: f32) -> bool {
    level > 0.0
}

/// The glyph for one cell at `level` in `0.0..=1.0`.
///
/// The level is scaled onto the ramp, and the matrix decides whether the fractional part
/// rounds up for this cell. Two cells at the same level on the same row get the same glyph;
/// the row above gets a different one, which is what makes the edge look halftoned.
#[must_use]
pub fn cell(level: f32, row: usize, col: usize) -> &'static str {
    let scaled = level.clamp(0.0, 1.0) * (RAMP.len() - 1) as f32;
    let whole = scaled.floor() as usize;
    let fraction = scaled - whole as f32;
    let threshold = (f32::from(BAYER[row % 4][col % 4]) + 0.5) / 16.0;
    let index = (whole + usize::from(fraction > threshold)).min(RAMP.len() - 1);
    RAMP[index]
}

/// A mirrored waveform, `rows` tall and one column per level, as intensities.
///
/// The centre line is the quiet one, and the envelope grows outwards from it, so a syllable
/// reads as a shape rather than a number. Intensities rather than glyphs, because the caller
/// colours a peak differently from the body: without the numbers it would have to guess the
/// colour back out of the character.
#[must_use]
pub fn waveform_levels(levels: &[f32], rows: usize) -> Vec<Vec<f32>> {
    let rows = rows.max(1);
    let middle = (rows as f32 - 1.0) / 2.0;
    let mut grid = vec![vec![0.0f32; levels.len()]; rows];
    for (col, level) in levels.iter().enumerate() {
        // Half the height is the most a symmetric shape can use.
        let amplitude = level.clamp(0.0, 1.0) * middle;
        for (row, line) in grid.iter_mut().enumerate() {
            let distance = (row as f32 - middle).abs();
            // Full density inside the envelope, nothing outside it, and the matrix softens
            // the boundary by one cell. Silence is no ink at all rather than a half-full
            // centre row, which is what the raw arithmetic would give.
            line[col] = if amplitude <= f32::EPSILON {
                0.0
            } else {
                (amplitude - distance + 0.5).clamp(0.0, 1.0)
            };
        }
    }
    grid
}

/// A left-to-right fill, for a progress bar of `width` columns, as intensities.
///
/// A ripple travels along it: a bar that fills smoothly is a bar nobody looks at.
///
/// A bar that fills smoothly is a bar nobody looks at. The ripple is decoration and it is
/// inside the filled part only, so it never claims progress the download has not made; the
/// head of the bar is drawn at full intensity so the edge stays sharp.
#[must_use]
pub fn bar_levels(fraction: f32, width: usize, phase: usize) -> Vec<f32> {
    let width = width.max(1);
    let filled = fraction.clamp(0.0, 1.0) * width as f32;
    let head = filled.floor() as usize;
    (0..width)
        .map(|col| {
            let base = (filled - col as f32).clamp(0.0, 1.0);
            if base <= 0.0 {
                return 0.0;
            }
            // The travelling part is a fraction of the cell, so the bar keeps its edge.
            let ripple = 0.22 * (col as f32 * 0.7 - phase as f32 * 0.45).sin();
            if col == head {
                1.0
            } else {
                (base + ripple * base).clamp(0.0, 1.0)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ends_of_the_ramp_are_the_ends_of_the_ramp() {
        assert_eq!(cell(0.0, 0, 0), " ");
        assert_eq!(cell(1.0, 0, 0), "█");
    }

    #[test]
    fn a_middle_level_is_a_mix_of_two_glyphs() {
        // A level between two ramp steps is the case the matrix exists for: without it
        // every cell would answer the same glyph and the edge would band. Exactly half is
        // a step, so it is not the interesting one.
        let glyphs: std::collections::BTreeSet<&str> = (0..4)
            .flat_map(|row| (0..4).map(move |col| (row, col)))
            .map(|(row, col)| cell(0.375, row, col))
            .collect();
        assert_eq!(
            glyphs.len(),
            2,
            "three eighths is two densities: {glyphs:?}"
        );
    }

    #[test]
    fn a_waveform_is_empty_at_zero_and_full_at_one() {
        let quiet = waveform_levels(&[0.0, 0.0], 3);
        assert!(
            quiet
                .iter()
                .all(|row| row.iter().all(|level| *level == 0.0))
        );
        let loud = waveform_levels(&[1.0, 1.0], 3);
        // The centre row of a full envelope is inked everywhere.
        assert_eq!(loud[1], vec![1.0, 1.0]);
    }

    #[test]
    fn a_progress_bar_fills_from_the_left() {
        let levels = bar_levels(0.5, 4, 0);
        assert_eq!(levels[0], 1.0);
        assert_eq!(levels[1], 1.0);
        assert_eq!(levels[2], 0.0);
        assert_eq!(levels[3], 0.0);
    }

    #[test]
    fn the_ripple_stays_inside_the_filled_part() {
        // The ripple must never claim progress: an empty bar has no ink at any phase.
        for phase in 0..8 {
            let levels = bar_levels(0.0, 6, phase);
            assert!(levels.iter().all(|level| *level == 0.0), "phase {phase}");
        }
    }
}
