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
#[must_use]
pub fn cell(level: f32, row: usize, col: usize) -> &'static str {
    let scaled = level.clamp(0.0, 1.0) * (RAMP.len() - 1) as f32;
    let whole = scaled.floor() as usize;
    let fraction = scaled - whole as f32;
    let threshold = (f32::from(BAYER[row % 4][col % 4]) + 0.5) / 16.0;
    let index = (whole + usize::from(fraction > threshold)).min(RAMP.len() - 1);
    RAMP[index]
}

/// A mirrored waveform, `rows` tall and one column per level.
///
/// The centre line is the quiet one, and the envelope grows outwards from it, so a syllable
/// reads as a shape rather than a number.
#[must_use]
pub fn waveform(levels: &[f32], rows: usize) -> Vec<Vec<&'static str>> {
    let rows = rows.max(1);
    let middle = (rows as f32 - 1.0) / 2.0;
    let mut grid = vec![vec![" "; levels.len()]; rows];
    for (col, level) in levels.iter().enumerate() {
        // Half the height is the most a symmetric shape can use.
        let amplitude = level.clamp(0.0, 1.0) * middle;
        for (row, line) in grid.iter_mut().enumerate() {
            let distance = (row as f32 - middle).abs();
            // Full density inside the envelope, nothing outside it, and the matrix softens
            // the boundary by one cell. Silence is no ink at all rather than a half-full
            // centre row, which is what the raw arithmetic would give.
            let inside = if amplitude <= f32::EPSILON {
                0.0
            } else {
                (amplitude - distance + 0.5).clamp(0.0, 1.0)
            };
            line[col] = cell(inside, row, col);
        }
    }
    grid
}

/// A left-to-right fill, for a progress bar of `width` columns.
#[must_use]
pub fn bar(fraction: f32, width: usize) -> Vec<&'static str> {
    let width = width.max(1);
    let filled = fraction.clamp(0.0, 1.0) * width as f32;
    (0..width)
        .map(|col| {
            let level = (filled - col as f32).clamp(0.0, 1.0);
            cell(level, 1, col)
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
        let quiet = waveform(&[0.0, 0.0], 3);
        assert!(quiet.iter().all(|row| row.iter().all(|cell| *cell == " ")));
        let loud = waveform(&[1.0, 1.0], 3);
        // The centre row of a full envelope is inked everywhere.
        assert_eq!(loud[1], vec!["█", "█"]);
    }

    #[test]
    fn a_progress_bar_fills_from_the_left() {
        let bar = bar(0.5, 4);
        assert_eq!(bar, vec!["█", "█", " ", " "]);
    }
}
