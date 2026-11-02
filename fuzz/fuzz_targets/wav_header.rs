#![no_main]

use libfuzzer_sys::fuzz_target;

// Audio arrives from the network in /v1/stt. This parser reads chunk lengths and offsets
// out of attacker-controlled bytes, which is the classic place for an overflow.
fuzz_target!(|data: &[u8]| {
    let _ = arioso_tts_piper::wav::read_bytes(data);
});
