//! End to end, against the real model.
//!
//! Ignored by default: the graph is 325 MB and the styles 28 MB, and a unit test that
//! downloads is a unit test that fails on a train. Run it with the files in place:
//!
//! ```text
//! ARIOSO_MODELS_DIR=<dir> cargo test -p arioso-tts-kokoro -- --ignored
//! ```

use std::path::{Path, PathBuf};

use arioso_core::UserDictionary;
use arioso_g2p::CmuDict;
use arioso_tts_kokoro::{KokoroVoice, RenderOptions, SAMPLE_RATE};

/// The directory the model files were placed in, or the reason there is none.
fn models_dir() -> PathBuf {
    let dir = std::env::var_os("ARIOSO_MODELS_DIR")
        .map(PathBuf::from)
        .expect("set ARIOSO_MODELS_DIR to the directory holding the Kokoro files");
    assert!(
        dir.is_dir(),
        "ARIOSO_MODELS_DIR is {} and it is not a directory",
        dir.display()
    );
    dir
}

/// The symbol table: the model's own config, under either name the catalog uses.
fn config_in(dir: &Path) -> PathBuf {
    for name in ["kokoro-config.json", "config.json"] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "no kokoro config in {}; expected kokoro-config.json",
        dir.display()
    );
}

#[test]
#[ignore = "needs kokoro-v1.0.onnx, voices-v1.0.bin and the config in ARIOSO_MODELS_DIR"]
fn a_sentence_renders_at_a_plausible_length() {
    let dir = models_dir();
    let mut voice = KokoroVoice::load(
        &dir.join("kokoro-v1.0.onnx"),
        &dir.join("voices-v1.0.bin"),
        &config_in(&dir),
        "af_heart",
    )
    .expect("loading the model");

    assert!(
        voice.voices().iter().any(|name| name == "af_heart"),
        "the archive names the voices it holds: {:?}",
        voice.voices()
    );

    // The shipped dictionary if this directory has one, so the sentence is spoken from
    // entries rather than spelled out; otherwise the rules do it.
    let dictionary = std::fs::read_to_string(dir.join("cmudict.dict"))
        .map(|text| CmuDict::parse(&text))
        .unwrap_or_else(|_| CmuDict::parse("hello HH AH0 L OW1\nworld W ER1 L D\n"));

    let mut samples = Vec::new();
    let report = voice
        .render(
            "Hello world, this is arioso.",
            &dictionary,
            &UserDictionary::shipped(),
            RenderOptions::default(),
            |audio, rate| {
                assert_eq!(rate, SAMPLE_RATE);
                samples.extend_from_slice(audio);
                Ok(())
            },
        )
        .expect("rendering");

    let seconds = samples.len() as f32 / SAMPLE_RATE as f32;
    println!(
        "rendered {:.2} s at {SAMPLE_RATE} Hz, {} samples, report {report:?}",
        seconds,
        samples.len()
    );
    // A five word sentence is a second or two of speech. Anything outside this is a
    // graph that ran and produced something other than the sentence.
    assert!(
        (0.5..8.0).contains(&seconds),
        "a five word sentence took {seconds:.2} s"
    );
    assert!(
        samples.iter().any(|sample| sample.abs() > 0.01),
        "the audio is silent"
    );
}

#[test]
#[ignore = "needs voices-v1.0.bin in ARIOSO_MODELS_DIR"]
fn the_archive_names_the_voices_it_holds() {
    let dir = models_dir();
    let names =
        KokoroVoice::voice_names(&dir.join("voices-v1.0.bin")).expect("reading the archive");
    println!("{} voices: {names:?}", names.len());
    assert!(names.len() > 20, "a v1.0 archive holds dozens of voices");
    assert!(names.iter().any(|name| name == "af_heart"));
    assert!(names.iter().any(|name| name == "am_michael"));
}
