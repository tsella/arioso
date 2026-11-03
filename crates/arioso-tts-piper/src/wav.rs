//! Writing and reading WAV files.
//!
//! The writer emits mono 16-bit PCM. The reader accepts 16-bit PCM and 32-bit float,
//! mono or stereo, because those are what a recording or another tool hands over.

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

/// Read a WAV file as mono samples and its rate.
///
/// Stereo is mixed down by averaging, so a stereo recording does not double its length.
///
/// # Errors
///
/// Returns [`std::io::ErrorKind::InvalidData`] for a file that is not a WAV, or whose
/// samples are neither 16-bit PCM nor 32-bit float.
pub fn read(path: &Path) -> std::io::Result<(Vec<f32>, u32)> {
    read_bytes(&std::fs::read(path)?)
}

/// The same, for audio that arrived in a request body rather than a file.
///
/// # Errors
///
/// Returns [`std::io::ErrorKind::InvalidData`] for bytes that are not a WAV, or whose
/// samples are neither 16-bit PCM nor 32-bit float.
pub fn read_bytes(bytes: &[u8]) -> std::io::Result<(Vec<f32>, u32)> {
    let invalid =
        |detail: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, detail.to_owned());

    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(invalid("not a RIFF/WAVE file"));
    }

    let mut format = 0u16;
    let mut channels = 0u16;
    let mut rate = 0u32;
    let mut bits = 0u16;
    let mut data: Option<&[u8]> = None;

    let mut at: usize = 12;
    while at
        .checked_add(8)
        .is_some_and(|end| end <= bytes.len())
    {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
            as usize;
        // A declared size is attacker controlled: it can be larger than the file, and on a
        // 32-bit target it can overflow the pointer-sized arithmetic that follows. Both are
        // refused rather than allowed to wrap or to pass as an empty chunk, because a
        // truncated data chunk that reads as zero samples is a silent wrong answer.
        let start = at.checked_add(8).ok_or_else(|| invalid("chunk offset overflows"))?;
        let end = start
            .checked_add(size)
            .ok_or_else(|| invalid("chunk size overflows"))?;
        if end > bytes.len() {
            return Err(invalid("chunk runs past the end of the file"));
        }
        let body = &bytes[start..end];
        match id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err(invalid("short fmt chunk"));
                }
                format = u16::from_le_bytes([body[0], body[1]]);
                channels = u16::from_le_bytes([body[2], body[3]]);
                rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                bits = u16::from_le_bytes([body[14], body[15]]);
            }
            b"data" => data = Some(body),
            _ => {}
        }
        // Chunks are word aligned: an odd size carries one pad byte.
        at = end
            .checked_add(size & 1)
            .ok_or_else(|| invalid("chunk offset overflows"))?;
    }

    let data = data.ok_or_else(|| invalid("no data chunk"))?;
    if channels == 0 {
        return Err(invalid("zero channels"));
    }

    let samples: Vec<f32> = match (format, bits) {
        (1, 16) => data
            .chunks_exact(2)
            .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / f32::from(i16::MAX))
            .collect(),
        (3, 32) => data
            .chunks_exact(4)
            .map(|word| f32::from_le_bytes([word[0], word[1], word[2], word[3]]))
            .collect(),
        _ => {
            return Err(invalid(&format!(
                "unsupported format {format} with {bits} bits"
            )));
        }
    };

    if channels == 1 {
        return Ok((samples, rate));
    }
    let mixed = samples
        .chunks_exact(channels as usize)
        .map(|frame| frame.iter().sum::<f32>() / f32::from(channels))
        .collect();
    Ok((mixed, rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_file_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("round.wav");
        let samples = [0.0, 0.5, -0.5, 1.0];
        write_wav(&path, &samples, 22_050).unwrap();

        let (read, rate) = read(&path).unwrap();
        assert_eq!(rate, 22_050);
        assert_eq!(read.len(), samples.len());
        for (a, b) in read.iter().zip(samples.iter()) {
            assert!((a - b).abs() < 0.001, "{a} against {b}");
        }
    }

    /// A WAV with an arbitrary channel count, built by hand.
    fn stereo_wav(samples: &[f32], rate: u32) -> Vec<u8> {
        let pcm = pcm_s16le(samples);
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 4).to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        out.extend_from_slice(&pcm);
        out
    }

    #[test]
    fn stereo_is_mixed_down() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stereo.wav");
        // Two frames of two channels: (1.0, 0.0) then (0.0, 1.0).
        std::fs::write(&path, stereo_wav(&[1.0, 0.0, 0.0, 1.0], 16_000)).unwrap();

        let (read, rate) = read(&path).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(read.len(), 2, "one sample per frame");
        assert!((read[0] - 0.5).abs() < 0.001, "{:?}", read[0]);
        assert!((read[1] - 0.5).abs() < 0.001, "{:?}", read[1]);
    }

    #[test]
    fn a_file_that_is_not_a_wav_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"hello").unwrap();
        let error = read(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

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

    /// A RIFF header with one chunk of the given id and declared size, and `body` bytes.
    fn chunked(id: &[u8; 4], declared: u32, body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::from(*b"RIFF");
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(id);
        bytes.extend_from_slice(&declared.to_le_bytes());
        bytes.extend_from_slice(body);
        bytes
    }

    fn fmt_body() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_le_bytes()); // PCM
        body.extend_from_slice(&1u16.to_le_bytes()); // mono
        body.extend_from_slice(&16_000u32.to_le_bytes());
        body.extend_from_slice(&32_000u32.to_le_bytes());
        body.extend_from_slice(&2u16.to_le_bytes());
        body.extend_from_slice(&16u16.to_le_bytes());
        body
    }

    #[test]
    fn a_chunk_larger_than_the_file_is_refused() {
        // The declared size is the whole of `u32`, which on a 32-bit target would wrap the
        // offset arithmetic and on any target runs past the buffer.
        let mut bytes = chunked(b"fmt ", u32::MAX, &fmt_body());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let error = read_bytes(&bytes).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{error}");
        assert!(error.to_string().contains("past the end"), "{error}");
    }

    #[test]
    fn a_truncated_data_chunk_is_refused_rather_than_read_as_silence() {
        // The header promises 400 bytes; the file carries 4. Reading it as an empty body
        // would decode to no samples and report success.
        let mut bytes = Vec::from(*b"RIFF");
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&fmt_body());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&400u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        let error = read_bytes(&bytes).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{error}");
    }

    #[test]
    fn an_odd_chunk_size_still_finds_the_next_chunk() {
        // A 5 byte chunk carries one pad byte, and the data chunk after it must be read.
        let mut bytes = Vec::from(*b"RIFF");
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&fmt_body());
        bytes.extend_from_slice(b"LIST");
        bytes.extend_from_slice(&5u32.to_le_bytes());
        bytes.extend_from_slice(b"hello");
        bytes.push(0); // the pad byte
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let (samples, rate) = read_bytes(&bytes).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(samples.len(), 2);
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
