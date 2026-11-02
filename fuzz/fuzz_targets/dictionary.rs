#![no_main]

use libfuzzer_sys::fuzz_target;

// The user dictionary is a file the operator writes by hand, so it is wrong more often
// than it is hostile. Either way it must not panic.
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = arioso_core::UserDictionary::parse(text);
    }
});
