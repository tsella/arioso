#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::Path;

// The config document arrives from a disk, a patch body, or a half-written download. The
// parser must return, not panic: a panic in a library other people embed is a denial of
// service, and an unbounded recursive descent is one too.
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = arioso_config::Config::from_json(text, Path::new("fuzz.json"));
    }
});
