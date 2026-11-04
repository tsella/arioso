//! What reading one Kokoro style costs in heap traffic, asserted rather than described.
//!
//! `voices-v1.0.bin` holds one table per voice: 511 rows of 256 floats, 522,240 bytes.
//! The read used to hold two copies of that table — the entry's raw bytes, then the
//! decoded floats — so the peak was twice the size of the style being loaded. The bound
//! below is one table and nothing else, so a return of the second copy fails here.

use std::io::Write;
use std::path::{Path, PathBuf};

use arioso_core::alloc_count::{Counting, measure};
use arioso_tts_kokoro::VoiceFile;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The shape the model ships: one row per token length, 256 style dimensions.
const ROWS: usize = 511;
const WIDTH: usize = 256;
/// Bytes of the decoded table, which is the least a read can hold.
const TABLE_BYTES: usize = ROWS * WIDTH * 4;

/// A stored zip holding one `.npy` array, the way `numpy.savez` writes it.
fn archive(name: &str, rows: usize, width: usize) -> Vec<u8> {
    let header =
        format!("{{'descr': '<f4', 'fortran_order': False, 'shape': ({rows}, 1, {width}), }}");
    let mut npy = vec![0x93, b'N', b'U', b'M', b'P', b'Y', 1, 0];
    npy.extend_from_slice(&(header.len() as u16).to_le_bytes());
    npy.extend_from_slice(header.as_bytes());
    npy.resize(npy.len() + rows * width * 4, 0);
    let file_name = format!("{name}.npy");
    let mut out = Vec::new();
    out.extend_from_slice(b"PK\x03\x04");
    out.extend_from_slice(&20u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(npy.len() as u32).to_le_bytes());
    out.extend_from_slice(&(npy.len() as u32).to_le_bytes());
    out.extend_from_slice(&(file_name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(file_name.as_bytes());
    out.extend_from_slice(&npy);
    out
}

fn written(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("voices.bin");
    let mut file = std::fs::File::create(&path).expect("creating the archive");
    file.write_all(bytes).expect("writing the archive");
    (dir, path)
}

#[test]
fn reading_one_style_holds_one_table() {
    let (_dir, path) = written(&archive("af_heart", ROWS, WIDTH));
    let voices = VoiceFile::open(Path::new(&path)).unwrap();
    let (style, stats) = measure(|| voices.style("af_heart").unwrap());
    eprintln!("one style: {stats:?} table={TABLE_BYTES}");
    assert_eq!((style.rows, style.width), (ROWS, WIDTH));
    assert_eq!(style.data.len(), ROWS * WIDTH);
    assert_eq!(
        stats.allocations, 1,
        "the table is one allocation: {stats:?}"
    );
    assert!(
        stats.bytes <= TABLE_BYTES,
        "the read held {} bytes for a {TABLE_BYTES} byte table: {stats:?}",
        stats.bytes
    );
}
