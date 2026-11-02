//! Writing mono 16-bit PCM WAV files.

use std::path::Path;

/// Write `samples` (nominally -1.0 to 1.0) as a mono 16-bit PCM WAV file.
///
/// # Errors
///
/// Returns the underlying I/O error.
pub fn write_wav(path: &Path, samples: &[f32], sample_rate: u32) -> std::io::Result<()> {
    std::fs::write(path, to_bytes(samples, sample_rate))
}

/// The same audio as an in-memory WAV, for HTTP responses.
#[must_use]
pub fn to_bytes(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());

    out.extend_from_slice(&pcm_s16le(samples));
    out
}

/// Samples as little-endian 16-bit PCM, without a container.
#[must_use]
pub fn pcm_s16le(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        let scaled = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        out.extend_from_slice(&scaled.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_readable_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.wav");
        write_wav(&path, &[0.0, 0.5, -0.5], 22_050).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 1, "mono");
        assert_eq!(
            u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]),
            22_050
        );
        assert_eq!(bytes.len(), 44 + 6);
    }

    #[test]
    fn clamps_out_of_range_samples() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.wav");
        write_wav(&path, &[2.0, -2.0], 16_000).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(i16::from_le_bytes([bytes[44], bytes[45]]), i16::MAX);
        assert_eq!(i16::from_le_bytes([bytes[46], bytes[47]]), -i16::MAX);
    }

    #[test]
    fn pcm_matches_the_file_payload() {
        let samples = [0.0, 0.5, -0.5];
        let wav = to_bytes(&samples, 16_000);
        assert_eq!(&wav[44..], pcm_s16le(&samples).as_slice());
    }

    #[test]
    fn bytes_match_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.wav");
        let samples = [0.0, 0.25, -0.25];
        write_wav(&path, &samples, 16_000).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), to_bytes(&samples, 16_000));
    }
}
