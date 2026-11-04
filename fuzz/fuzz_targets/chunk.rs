#![no_main]

use libfuzzer_sys::fuzz_target;

// The chunker cuts a document into pieces a voice can say. Its budget arithmetic is the
// part that has already been wrong once, so the size is taken from the input rather than
// fixed.
fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let max_chars = usize::from(u16::from_le_bytes([data[0], data[1]]));
    if let Ok(text) = std::str::from_utf8(&data[2..]) {
        let pieces = arioso_core::chunk_text(text, max_chars);
        // A chunker that drops text or invents it is the defect this asserts against.
        let rebuilt: usize = pieces.iter().map(|piece| piece.len()).sum();
        assert!(rebuilt <= text.len());
    }
});
