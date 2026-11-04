#![no_main]

use libfuzzer_sys::fuzz_target;

// Normalization rewrites text before any voice sees it: numbers, currency, times, units,
// abbreviations, URLs, code spans. Every branch reads the input, so a panic here is a panic
// on a path every synthesis takes.
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = arioso_core::normalize(text);
    }
});
