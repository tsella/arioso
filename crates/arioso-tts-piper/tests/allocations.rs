//! What writing a WAV costs in heap traffic, asserted rather than described.
//!
//! `write_wav_pcm` writes a PCM document the caller already converted — the CLI's `say`,
//! and anything else that accumulates 16-bit samples. It used to build the whole file in
//! memory first: a 44 byte header in front of a second copy of the audio, handed to
//! `std::fs::write`. The bound below is a fixed few hundred bytes, so any buffer that
//! grows with the payload fails here.

use std::path::Path;

use arioso_core::alloc_count::{Counting, measure};
use arioso_tts_piper::wav::write_wav_pcm;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One second of 22,050 Hz mono audio, as 16-bit PCM.
const SAMPLES: usize = 22_050;
const BYTES: usize = SAMPLES * 2;

fn pcm() -> Vec<u8> {
    // A tone rather than silence, so a copy that drops or reorders bytes is visible.
    (0..SAMPLES)
        .flat_map(|i| (((i % 200) as i16) * 160).to_le_bytes())
        .collect()
}

fn write(path: &Path, pcm: &[u8]) -> arioso_core::alloc_count::Stats {
    let (result, stats) = measure(|| write_wav_pcm(path, pcm, 22_050));
    result.expect("the file is written");
    stats
}

#[test]
fn writing_a_wav_does_not_copy_the_audio() {
    let pcm = pcm();
    assert_eq!(pcm.len(), BYTES);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");

    let stats = write(&path, &pcm);

    // The file is exactly the header and the audio, byte for byte: writing the payload
    // straight to the handle must not change what lands on disk.
    let written = std::fs::read(&path).expect("the file is there");
    assert_eq!(written.len(), 44 + BYTES);
    assert_eq!(&written[44..], &pcm[..], "the payload is unchanged");

    // A path conversion is all that is left. The old writer asked for 44 + 44,100 bytes.
    assert!(
        stats.bytes < 1_024,
        "writing {} bytes of audio allocated {} bytes",
        BYTES,
        stats.bytes
    );
}
