//! The voice against the real graphs.
//!
//! The four graphs are 398 MB and not in the repository, so this runs when they are on disk.
//! `ARIOSO_TEST_SUPERTONIC` names a directory holding `onnx/` and `voice_styles/` as the
//! catalog entry lays them out.

use std::path::PathBuf;

use arioso_core::UserDictionary;
use arioso_g2p::CmuDict;
use arioso_tts_supertonic::{Files, RenderOptions, Voice};

/// The four graphs, the two tables and one style, or nothing when the models are absent.
fn fixture() -> Option<(PathBuf, PathBuf)> {
    let dir = PathBuf::from(std::env::var_os("ARIOSO_TEST_SUPERTONIC")?);
    let onnx = dir.join("onnx");
    let ready = onnx.join("text_encoder.onnx").is_file()
        && onnx.join("vector_estimator.onnx").is_file()
        && dir.join("voice_styles/M1.json").is_file();
    ready.then_some((onnx, dir.join("voice_styles/M1.json")))
}

#[test]
fn a_sentence_becomes_audio_of_the_length_the_predictor_promised() {
    let Some((onnx, style)) = fixture() else {
        return;
    };
    let files = Files {
        text_encoder: &onnx.join("text_encoder.onnx"),
        duration_predictor: &onnx.join("duration_predictor.onnx"),
        vector_estimator: &onnx.join("vector_estimator.onnx"),
        vocoder: &onnx.join("vocoder.onnx"),
        config: &onnx.join("tts.json"),
        indexer: &onnx.join("unicode_indexer.json"),
        style: &style,
    };
    let mut voice = Voice::load_with_threads(&files, "en", 4).expect("the models load");
    assert_eq!(voice.sample_rate(), 44_100);

    let mut audio = Vec::new();
    let report = voice
        .render(
            "The quick brown fox jumps over the lazy dog.",
            &CmuDict::parse(""),
            &UserDictionary::shipped(),
            RenderOptions { speed: 1.0 },
            |chunk, rate| {
                assert_eq!(rate, 44_100);
                audio.extend_from_slice(chunk);
                Ok(())
            },
        )
        .expect("the voice speaks");

    // Both lists are empty by design: this engine has no dictionary to miss a word from.
    assert!(report.guessed.is_empty() && report.missing.is_empty());

    // About three seconds of speech for that sentence, and not silence. A model that loaded
    // and produced nothing would pass a length check alone, which is why the energy is here.
    let seconds = audio.len() as f32 / 44_100.0;
    assert!(
        (1.5..6.0).contains(&seconds),
        "the sentence took {seconds} s"
    );
    let peak = audio
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    assert!(peak > 0.05, "the audio is silent: peak {peak}");
    assert!(peak <= 1.5, "the audio clips badly: peak {peak}");
}

#[test]
fn the_same_text_twice_gives_the_same_audio() {
    // F3.7: the noise is seeded, so a render is reproducible. Without this, nothing about the
    // engine can be compared against itself.
    let Some((onnx, style)) = fixture() else {
        return;
    };
    let files = Files {
        text_encoder: &onnx.join("text_encoder.onnx"),
        duration_predictor: &onnx.join("duration_predictor.onnx"),
        vector_estimator: &onnx.join("vector_estimator.onnx"),
        vocoder: &onnx.join("vocoder.onnx"),
        config: &onnx.join("tts.json"),
        indexer: &onnx.join("unicode_indexer.json"),
        style: &style,
    };
    let mut voice = Voice::load_with_threads(&files, "en", 4).expect("the models load");
    let render = |voice: &mut Voice| {
        let mut audio = Vec::new();
        voice
            .render(
                "Deterministic, and short.",
                &CmuDict::parse(""),
                &UserDictionary::shipped(),
                RenderOptions { speed: 1.0 },
                |chunk, _| {
                    audio.extend_from_slice(chunk);
                    Ok(())
                },
            )
            .expect("the voice speaks");
        audio
    };
    let first = render(&mut voice);
    let second = render(&mut voice);
    assert_eq!(first.len(), second.len());
    assert_eq!(first, second, "two renders of one text differed");
}
