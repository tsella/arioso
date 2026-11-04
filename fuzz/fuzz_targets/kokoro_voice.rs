#![no_main]

use std::io::Write as _;

use libfuzzer_sys::fuzz_target;

// The Kokoro style archive is a ZIP of `.npy` tensors, parsed from a file rather than from a
// slice, so the input is written to a temporary file and read back the way a voice is read.
// A malformed archive must be refused, not indexed past its end.
fuzz_target!(|data: &[u8]| {
    let path = std::env::temp_dir().join(format!(
        "arioso-fuzz-voices-{}-{}.bin",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let Ok(mut file) = std::fs::File::create(&path) else {
        return;
    };
    if file.write_all(data).is_err() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    drop(file);
    let _ = arioso_tts_kokoro::VoiceFile::open(&path);
    let _ = std::fs::remove_file(&path);
});
