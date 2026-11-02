#![no_main]

use libfuzzer_sys::fuzz_target;

// The manifest names model files and their sizes and hashes. It ships with the binary, but
// it is parsed from bytes, so a corrupted copy must fail rather than panic.
fuzz_target!(|data: &[u8]| {
    let _ = arioso_models::Manifest::parse(data);
});
