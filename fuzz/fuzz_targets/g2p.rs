#![no_main]

use libfuzzer_sys::fuzz_target;

// The letter-to-sound rules are the fallback for every word the dictionary does not hold,
// and they are reached with arbitrary text: a user dictionary can name anything.
fuzz_target!(|data: &[u8]| {
    if let Ok(word) = std::str::from_utf8(data) {
        let _ = arioso_g2p::lts::pronounce(word);
    }
});
