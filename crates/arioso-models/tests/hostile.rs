//! A manifest with one thing wrong with it must be refused, not panic.

use arioso_models::Manifest;

/// How many mutations to try. The manifest is a quarter of a megabyte, so a cut every five
/// bytes was fifty thousand parses of a growing document: the sample is spread over the whole
/// file instead, which still reaches the last voice and every kind of damage.
const CASES: usize = 300;

fn mutations(valid: &str) -> Vec<String> {
    let bytes = valid.as_bytes();
    let step = (bytes.len() / CASES).max(1);
    let mut out = Vec::new();
    for cut in (0..bytes.len()).step_by(step) {
        out.push(String::from_utf8_lossy(&bytes[..cut]).into_owned());
    }
    for at in (0..bytes.len()).step_by(step) {
        let mut copy = bytes.to_vec();
        copy[at] = b'\0';
        out.push(String::from_utf8_lossy(&copy).into_owned());
    }
    out.push("[".repeat(50_000));
    out.push("{\"a\":".repeat(50_000));
    out
}

#[test]
fn no_mutation_of_the_shipped_manifest_panics() {
    let valid = include_str!("../../../models/manifest.json");
    let cases = mutations(valid);
    let mut rejected = 0;
    for case in &cases {
        if Manifest::parse(case.as_bytes()).is_err() {
            rejected += 1;
        }
    }
    assert!(
        rejected > 50,
        "only {rejected} of {} mutations were refused",
        cases.len()
    );
}
