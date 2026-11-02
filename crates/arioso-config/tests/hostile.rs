//! A valid config document with one thing wrong with it must be refused, not panic.
//!
//! This is the stable-toolchain half of the fuzzing the plan asks for: deterministic,
//! runs in CI in milliseconds, and covers the shapes a real corruption takes. Coverage
//! guided fuzzing needs nightly and `cargo-fuzz`, and is a separate slice.

use std::path::Path;

use arioso_config::Config;

/// Documents built by breaking a valid one, rather than by inventing bytes.
///
/// A random byte string is rejected by the JSON parser before it reaches this crate, which
/// tests serde and not us. A valid document with one thing wrong with it gets all the way
/// in, which is where a panic would live.
fn mutations(valid: &str) -> Vec<String> {
    let bytes = valid.as_bytes();
    let mut out = Vec::new();
    // Truncation: a file written while the disk filled, or a transfer cut short.
    for cut in (0..bytes.len()).step_by(7) {
        out.push(String::from_utf8_lossy(&bytes[..cut]).into_owned());
    }
    // One byte replaced, which is what a corrupted write or a stray edit looks like.
    for at in (0..bytes.len()).step_by(11) {
        let mut copy = bytes.to_vec();
        copy[at] = b'\0';
        out.push(String::from_utf8_lossy(&copy).into_owned());
    }
    // Nesting deep enough to blow a recursive parser's stack if it has no limit.
    out.push("[".repeat(50_000));
    out.push("{\"a\":".repeat(50_000));
    out
}

#[test]
fn no_mutation_of_the_example_config_panics() {
    let valid = include_str!("../../../config.json.example");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let cases = mutations(valid);
    let mut rejected = 0;

    for case in &cases {
        std::fs::write(&path, case).unwrap();
        // The only requirement is that it returns. A panic here is the defect.
        if Config::load(&path).is_err() {
            rejected += 1;
        }
    }

    assert!(
        rejected > 100,
        "only {rejected} of {} mutations were refused, which suggests the harness is not \\
         reaching the parser",
        cases.len()
    );
}

#[test]
fn an_empty_file_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, "").unwrap();
    let error = Config::load(Path::new(&path)).unwrap_err().to_string();
    assert!(error.contains("config.json"), "{error}");
}
