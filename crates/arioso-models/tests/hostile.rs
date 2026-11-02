//! A manifest with one thing wrong with it must be refused, not panic.

use arioso_models::Manifest;

fn mutations(valid: &str) -> Vec<String> {
    let bytes = valid.as_bytes();
    let mut out = Vec::new();
    for cut in (0..bytes.len()).step_by(5) {
        out.push(String::from_utf8_lossy(&bytes[..cut]).into_owned());
    }
    for at in (0..bytes.len()).step_by(9) {
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
