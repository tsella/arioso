//! Piper speaks, Whisper reads it back.
//!
//! It needs both models, so it runs only when `ARIOSO_TEST_MODELS_DIR` points at a
//! directory holding the voice, the dictionary, and the whisper files. CI skips it.

use std::path::PathBuf;

use arioso_g2p::CmuDict;
use arioso_stt_sherpa::Engine;
use arioso_tts_piper::{PiperVoice, RenderOptions};

#[test]
fn a_sentence_survives_the_round_trip() {
    let Some(dir) = std::env::var("ARIOSO_TEST_MODELS_DIR")
        .ok()
        .map(PathBuf::from)
    else {
        eprintln!("skipped: ARIOSO_TEST_MODELS_DIR is not set");
        return;
    };
    let voice_config = dir.join("en_US-lessac-medium.onnx.json");
    if !voice_config.exists() || !dir.join("tiny.en-encoder.int8.onnx").exists() {
        eprintln!("skipped: a model is missing from {}", dir.display());
        return;
    }

    let dictionary = CmuDict::parse(&std::fs::read_to_string(dir.join("cmudict.dict")).unwrap());
    let mut voice = PiperVoice::load(&dir.join("en_US-lessac-medium.onnx"), &voice_config).unwrap();
    let mut samples = Vec::new();
    voice
        .render(
            "the quick brown fox jumps over the lazy dog",
            &dictionary,
            &arioso_core::UserDictionary::empty(),
            RenderOptions::default(),
            |audio, _| {
                samples.extend_from_slice(audio);
                Ok(())
            },
        )
        .unwrap();
    assert!(!samples.is_empty(), "the voice said nothing");

    // The directory may hold more than one model, so name the files from the manifest
    // the way the server does.
    let manifest =
        arioso_models::Manifest::parse(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    let spec = manifest
        .models
        .iter()
        .find(|spec| {
            spec.kind == arioso_models::Kind::Stt && dir.join(&spec.files[0].name).exists()
        })
        .expect("a speech to text model in the directory");
    let file = |needle: &str| {
        spec.files
            .iter()
            .find(|f| f.name.contains(needle))
            .map(|f| dir.join(&f.name))
            .unwrap()
    };
    let engine = Engine::load_files(
        &dir,
        &file("encoder"),
        &file("decoder"),
        &file("tokens"),
        Some("en"),
        2,
    )
    .unwrap();
    let transcript = engine.transcribe(&samples, voice.sample_rate()).unwrap();
    let heard = transcript.text.to_lowercase();
    let heard = heard.trim_end_matches('.');

    assert_eq!(
        heard, "the quick brown fox jumps over the lazy dog",
        "heard: {heard}"
    );
}
