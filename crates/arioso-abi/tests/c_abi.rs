//! The exported surface, driven the way C drives it.
//!
//! These tests call the `extern "C"` functions through the `rlib`, so they exercise the same
//! code the shared and static libraries export: the panic guard, the option-struct size
//! rule, the error string, and the release functions.
//!
//! The tests that need a voice or a speech model read `ARIOSO_TEST_MODELS_DIR` and skip when
//! it is unset, the way `crates/arioso-server/tests/ws_stream.rs` does. They are also
//! `#[ignore]`d, so a plain `cargo test` does not claim to have tested synthesis; run them
//! with `cargo test -p arioso-abi -- --ignored` and the variable set.

use std::ffi::{CStr, CString, c_void};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

use arioso_stt_sherpa::resample;
use arioso_tts_piper::wav;

use arioso::{
    ARIOSO_ABI_VERSION, ARIOSO_CANCELLED, ARIOSO_ERR_CONFIG, ARIOSO_ERR_ENGINE,
    ARIOSO_ERR_INVALID_ARGUMENT, ARIOSO_ERR_NULL_ARGUMENT, ARIOSO_ERR_PANIC,
    ARIOSO_ERR_STRUCT_SIZE, ARIOSO_OK, AriosoBuffer, AriosoEngine, AriosoSttEvent,
    AriosoSttOptions, AriosoSttSession, AriosoTtsOptions, arioso_abi_version, arioso_diarize,
    arioso_engine_free, arioso_engine_new, arioso_free, arioso_last_error, arioso_model_download,
    arioso_models_json, arioso_phonemes, arioso_stt_close, arioso_stt_finish, arioso_stt_open,
    arioso_stt_push, arioso_tts_phonemes, arioso_tts_stream, arioso_tts_synthesize,
};

/// The sentence the last call left on this thread.
fn last_error() -> String {
    // Safety: the pointer is never null and points at a NUL-terminated string that lives
    // until the next call on this thread, which is after this copy.
    let text = unsafe { CStr::from_ptr(arioso_last_error()) };
    text.to_string_lossy().into_owned()
}

/// A models directory with the two files an engine reads and no model files in it.
fn models_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(
        dir.path().join("manifest.json"),
        r#"{"schema_version": 1, "models": []}"#,
    )
    .expect("a manifest");
    std::fs::write(dir.path().join("cmudict.dict"), "").expect("a dictionary");
    dir
}

/// The config a caller would pass, pointing at `dir`.
///
/// Built with `serde_json` rather than `format!` so a path with a backslash in it is escaped
/// rather than producing a document that will not parse.
fn config_json(dir: &Path) -> CString {
    document(serde_json::json!({"models": {"dir": dir}}))
}

/// A config with a voice, and a streaming model when the test needs one.
fn voice_config(dir: &Path, voice: &str, stream_model: Option<&str>) -> CString {
    let mut doc = serde_json::json!({"models": {"dir": dir}, "tts": {"voice": voice}});
    if let Some(model) = stream_model {
        doc["stt"] = serde_json::json!({"stream_model": model});
    }
    document(doc)
}

/// A JSON document as the NUL-terminated string the C surface takes.
fn document(doc: serde_json::Value) -> CString {
    CString::new(doc.to_string()).expect("a config with no NUL in it")
}

/// An engine over `dir`, which must have a manifest and a dictionary.
fn engine(dir: &Path) -> *mut AriosoEngine {
    let json = config_json(dir);
    let mut engine: *mut AriosoEngine = ptr::null_mut();
    // Safety: `json` outlives the call and `engine` is writable for one pointer.
    let code = unsafe { arioso_engine_new(json.as_ptr(), &raw mut engine) };
    assert_eq!(code, ARIOSO_OK, "creating the engine: {}", last_error());
    assert!(!engine.is_null());
    engine
}

#[test]
fn the_version_is_one() {
    assert_eq!(arioso_abi_version(), 1);
    assert_eq!(ARIOSO_ABI_VERSION, arioso_abi_version());
}

#[test]
fn an_engine_is_created_from_a_config_and_freed() {
    let dir = models_dir();
    let engine = engine(dir.path());
    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };

    // The error string is empty after a call that worked, so a caller reading it is not
    // reading the previous failure.
    assert_eq!(last_error(), "");

    // Freeing null is a no-op rather than a crash: a caller's cleanup path can be careless.
    // Safety: null is the documented no-op.
    unsafe { arioso_engine_free(ptr::null_mut()) };
}

#[test]
fn diarization_refuses_what_it_cannot_read() {
    let dir = models_dir();
    let json = config_json(dir.path());
    let mut engine: *mut AriosoEngine = ptr::null_mut();
    // Safety: `json` outlives the call and `engine` is writable for one pointer.
    let code = unsafe { arioso_engine_new(json.as_ptr(), &raw mut engine) };
    assert_eq!(code, ARIOSO_OK, "{}", last_error());

    let samples = vec![0.0_f32; 16_000];
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };

    // A null sample pointer is a code, not a crash.
    // Safety: the null pointer is the case under test.
    let code = unsafe { arioso_diarize(engine, ptr::null(), 16_000, 16_000, &raw mut buffer) };
    assert_eq!(code, ARIOSO_ERR_NULL_ARGUMENT);
    assert!(buffer.data.is_null());

    // No samples is refused by name rather than answered with an empty list.
    // Safety: `samples` is valid for its length.
    let code = unsafe { arioso_diarize(engine, samples.as_ptr(), 0, 16_000, &raw mut buffer) };
    assert_eq!(code, ARIOSO_ERR_INVALID_ARGUMENT);
    assert!(last_error().contains("length"), "{}", last_error());

    // The models read 16 kHz, and a different rate is named rather than resampled.
    // Safety: as above.
    let code = unsafe {
        arioso_diarize(
            engine,
            samples.as_ptr(),
            samples.len(),
            44_100,
            &raw mut buffer,
        )
    };
    assert_eq!(code, ARIOSO_ERR_INVALID_ARGUMENT);
    assert!(last_error().contains("16000"), "{}", last_error());

    // The catalog here is empty, so the models are not installed: the sentence names the
    // entry to fetch rather than the algorithm.
    // Safety: as above.
    let code = unsafe {
        arioso_diarize(
            engine,
            samples.as_ptr(),
            samples.len(),
            16_000,
            &raw mut buffer,
        )
    };
    assert_eq!(code, ARIOSO_ERR_ENGINE);
    assert!(buffer.data.is_null());
    assert!(last_error().contains("diarization-en"), "{}", last_error());

    // Safety: a live handle, freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
fn a_bad_config_is_a_code_and_a_sentence() {
    let mut engine: *mut AriosoEngine = ptr::null_mut();

    let json = CString::new(r#"{"server": {"bind": "not an address"}}"#).unwrap();
    // Safety: as above; the out pointer is nulled on refusal.
    let code = unsafe { arioso_engine_new(json.as_ptr(), &raw mut engine) };
    assert_eq!(code, ARIOSO_ERR_CONFIG);
    assert!(engine.is_null(), "a refusal must not hand back a handle");
    let sentence = last_error();
    assert!(sentence.contains("server.bind"), "got {sentence:?}");

    // Malformed JSON is the same code with a different sentence: the caller can tell the two
    // apart by reading it, which is the point of keeping the message.
    let json = CString::new("{not json").unwrap();
    // Safety: as above.
    let code = unsafe { arioso_engine_new(json.as_ptr(), &raw mut engine) };
    assert_eq!(code, ARIOSO_ERR_CONFIG);
    let sentence = last_error();
    assert!(sentence.contains("parsing"), "got {sentence:?}");
}

#[test]
fn a_config_whose_models_directory_has_no_manifest_is_an_engine_error() {
    let dir = tempfile::tempdir().unwrap();
    let json = config_json(dir.path());
    let mut engine: *mut AriosoEngine = ptr::null_mut();
    // Safety: `json` outlives the call, `engine` is writable for one pointer.
    let code = unsafe { arioso_engine_new(json.as_ptr(), &raw mut engine) };
    assert_eq!(code, ARIOSO_ERR_ENGINE);
    let sentence = last_error();
    assert!(sentence.contains("manifest"), "got {sentence:?}");
}

#[test]
fn a_null_argument_is_a_code_rather_than_a_crash() {
    let dir = models_dir();
    let json = config_json(dir.path());
    let mut engine: *mut AriosoEngine = ptr::null_mut();

    // Safety: a null string is the case under test.
    let code = unsafe { arioso_engine_new(ptr::null(), &raw mut engine) };
    assert_eq!(code, ARIOSO_ERR_NULL_ARGUMENT);
    assert!(last_error().contains("config_json"), "{}", last_error());

    // Safety: a null out pointer is the case under test.
    let code = unsafe { arioso_engine_new(json.as_ptr(), ptr::null_mut()) };
    assert_eq!(code, ARIOSO_ERR_NULL_ARGUMENT);

    // Every call that takes an engine refuses a null one, and none of them reads it.
    let text = CString::new("hello").unwrap();
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };
    // Safety: a null engine is the case under test.
    let code = unsafe {
        arioso_tts_synthesize(ptr::null_mut(), text.as_ptr(), ptr::null(), &raw mut buffer)
    };
    assert_eq!(code, ARIOSO_ERR_NULL_ARGUMENT);
    assert!(buffer.data.is_null());

    // Safety: a null engine is the case under test.
    let code = unsafe {
        arioso_tts_stream(
            ptr::null_mut(),
            text.as_ptr(),
            ptr::null(),
            None,
            ptr::null_mut(),
        )
    };
    assert_eq!(code, ARIOSO_ERR_NULL_ARGUMENT);

    // A session call with no session.
    let mut session: *mut AriosoSttSession = ptr::null_mut();
    // Safety: a null out pointer is the case under test.
    let code = unsafe { arioso_stt_open(ptr::null_mut(), ptr::null(), &raw mut session) };
    assert_eq!(code, ARIOSO_ERR_NULL_ARGUMENT);
    // Safety: a null session is the case under test.
    let code = unsafe { arioso_stt_push(ptr::null_mut(), ptr::null(), 0, 16_000) };
    assert_eq!(code, ARIOSO_ERR_NULL_ARGUMENT);
    assert_eq!(last_error(), "arioso_stt_push: session is null");
    // Safety: a null session is the case under test.
    let code = unsafe { arioso_stt_finish(ptr::null_mut(), None, ptr::null_mut()) };
    assert_eq!(code, ARIOSO_ERR_NULL_ARGUMENT);
    assert_eq!(last_error(), "arioso_stt_finish: session is null");

    // Diarization refuses what it cannot read, before it looks for a model.
    let samples = vec![0.0_f32; 16_000];
    // Safety: a null out pointer is the case under test.
    let code = unsafe {
        arioso_diarize(
            ptr::null_mut(),
            samples.as_ptr(),
            samples.len(),
            16_000,
            ptr::null_mut(),
        )
    };
    assert_eq!(code, ARIOSO_ERR_NULL_ARGUMENT);
    assert_eq!(last_error(), "arioso_diarize: out is null");
    // Safety: a null engine is the case under test.
    let code = unsafe {
        arioso_diarize(
            ptr::null_mut(),
            samples.as_ptr(),
            samples.len(),
            16_000,
            &raw mut buffer,
        )
    };
    assert_eq!(code, ARIOSO_ERR_NULL_ARGUMENT);
    assert!(
        buffer.data.is_null(),
        "a refusal must not hand back a buffer"
    );

    // The releases are quiet no-ops on null, so a cleanup path can be careless.
    // Safety: null is the documented no-op for each of these.
    unsafe {
        arioso_stt_close(ptr::null_mut());
        arioso_free(ptr::null_mut());
    }
    assert_eq!(last_error(), "");
}

#[test]
fn a_struct_size_larger_than_the_library_knows_is_refused() {
    let dir = models_dir();
    let engine = engine(dir.path());
    let text = CString::new("hello").unwrap();
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };

    let options = AriosoTtsOptions {
        // A caller built against a newer header than this library.
        struct_size: std::mem::size_of::<AriosoTtsOptions>() + 8,
        speed: 1.0,
        speaker: 0,
    };
    // Safety: `options` is a valid struct and `buffer` is writable for one buffer.
    let code = unsafe {
        arioso_tts_synthesize(engine, text.as_ptr(), &raw const options, &raw mut buffer)
    };
    assert_eq!(code, ARIOSO_ERR_STRUCT_SIZE);
    assert!(buffer.data.is_null(), "nothing is allocated by a refusal");
    assert!(last_error().contains("struct_size"), "{}", last_error());

    // A size too small to hold the size field itself is the same refusal.
    let mut tiny = [0usize; 2];
    tiny[0] = 4;
    // Safety: `tiny` is aligned for a `usize`, which is all the library reads before it
    // refuses.
    let code = unsafe {
        arioso_tts_synthesize(engine, text.as_ptr(), tiny.as_ptr().cast(), &raw mut buffer)
    };
    assert_eq!(code, ARIOSO_ERR_STRUCT_SIZE);

    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
fn the_catalog_comes_back_as_json() {
    let dir = models_dir();
    let engine = engine(dir.path());
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };
    // Safety: `engine` is live and `buffer` is writable for one buffer.
    let code = unsafe { arioso_models_json(engine, &raw mut buffer) };
    assert_eq!(code, ARIOSO_OK, "{}", last_error());
    assert!(!buffer.data.is_null());
    // Safety: the library wrote `len` bytes at `data`, and it is not NUL-terminated.
    let bytes = unsafe { std::slice::from_raw_parts(buffer.data.cast::<u8>(), buffer.len) };
    let parsed: serde_json::Value = serde_json::from_slice(bytes).expect("a JSON document");
    // The test catalog is empty, so the shape is an empty array rather than an object.
    assert!(parsed.is_array(), "{parsed}");
    // Safety: the pointer came from this library and is released once.
    unsafe { arioso_free(buffer.data.cast()) };
    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
fn phonemes_for_empty_text_are_refused() {
    let dir = models_dir();
    let engine = engine(dir.path());
    let text = CString::new("   ").unwrap();
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };
    // Safety: `engine` is live, `text` outlives the call, `buffer` is writable.
    let code = unsafe { arioso_phonemes(engine, text.as_ptr(), &raw mut buffer) };
    assert_eq!(code, ARIOSO_ERR_INVALID_ARGUMENT, "{}", last_error());
    assert!(
        buffer.data.is_null(),
        "a refusal hands back nothing to free"
    );
    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
fn a_character_that_is_not_a_phoneme_is_named() {
    let dir = models_dir();
    let engine = engine(dir.path());
    // `x` is a letter, not one of the symbols this engine's table emits.
    let phonemes = CString::new("hɛlx").unwrap();
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };
    // Safety: `engine` is live, the text outlives the call, `buffer` is writable.
    let code =
        unsafe { arioso_tts_phonemes(engine, phonemes.as_ptr(), ptr::null(), &raw mut buffer) };
    assert_eq!(code, ARIOSO_ERR_INVALID_ARGUMENT, "{}", last_error());
    assert!(last_error().contains('x'), "{}", last_error());
    assert!(buffer.data.is_null());
    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
fn downloading_an_id_the_catalog_lacks_is_refused() {
    let dir = models_dir();
    let engine = engine(dir.path());
    let id = CString::new("no-such-model").unwrap();
    // Safety: `engine` is live, `id` outlives the call, and no callback is passed.
    let code = unsafe { arioso_model_download(engine, id.as_ptr(), None, ptr::null_mut()) };
    assert_eq!(code, ARIOSO_ERR_ENGINE, "{}", last_error());
    assert!(last_error().contains("no-such-model"), "{}", last_error());
    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
fn a_speaker_out_of_range_is_an_invalid_argument() {
    let dir = models_dir();
    let engine = engine(dir.path());
    let text = CString::new("hello").unwrap();
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };
    let options = AriosoTtsOptions {
        struct_size: std::mem::size_of::<AriosoTtsOptions>(),
        speed: 1.0,
        // No voice has four thousand speakers, and the range the routes accept is the range
        // this surface accepts.
        speaker: 4096,
    };
    // Safety: `options` is a valid struct and `buffer` is writable for one buffer.
    let code = unsafe {
        arioso_tts_synthesize(engine, text.as_ptr(), &raw const options, &raw mut buffer)
    };
    assert_eq!(code, ARIOSO_ERR_INVALID_ARGUMENT, "{}", last_error());
    assert!(last_error().contains("4096"), "{}", last_error());
    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
fn text_longer_than_the_configured_limit_is_an_invalid_argument() {
    let dir = models_dir();
    let json = document(serde_json::json!({
        "models": {"dir": dir.path()},
        "server": {"limits": {"max_text_chars": 8}}
    }));
    let mut engine: *mut AriosoEngine = ptr::null_mut();
    // Safety: `json` outlives the call and `engine` is writable for one pointer.
    let code = unsafe { arioso_engine_new(json.as_ptr(), &raw mut engine) };
    assert_eq!(code, ARIOSO_OK, "{}", last_error());

    let text = CString::new("this is longer than eight characters").unwrap();
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };
    // Safety: `engine` is live, the text outlives the call, `buffer` is writable.
    let code =
        unsafe { arioso_tts_synthesize(engine, text.as_ptr(), ptr::null(), &raw mut buffer) };
    assert_eq!(code, ARIOSO_ERR_INVALID_ARGUMENT, "{}", last_error());
    assert!(last_error().contains("8"), "{}", last_error());
    assert!(buffer.data.is_null(), "nothing is allocated by a refusal");
    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
fn a_struct_size_that_stops_inside_a_field_is_refused() {
    let dir = models_dir();
    let engine = engine(dir.path());
    let text = CString::new("hello").unwrap();
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };
    // Ten bytes ends two bytes into `speed`: copying it would splice half a float onto the
    // default and render audio nobody asked for.
    let mut bytes = [0u8; 24];
    bytes[..8].copy_from_slice(&10usize.to_le_bytes());
    // Safety: the buffer is 24 bytes and the library reads the size field, then refuses.
    let code = unsafe {
        arioso_tts_synthesize(
            engine,
            text.as_ptr(),
            bytes.as_ptr().cast(),
            &raw mut buffer,
        )
    };
    assert_eq!(code, ARIOSO_ERR_STRUCT_SIZE, "{}", last_error());
    assert!(last_error().contains("boundary"), "{}", last_error());
    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
fn options_at_an_unaligned_address_are_read() {
    let dir = models_dir();
    let engine = engine(dir.path());
    let text = CString::new("hello").unwrap();
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };
    // A packed C struct can sit at any address. The size field is a `usize`, and reading it
    // with a plain load would be undefined behaviour here.
    let mut bytes = [0u8; 32];
    bytes[1..9].copy_from_slice(&24usize.to_le_bytes());
    bytes[9..13].copy_from_slice(&1.0f32.to_le_bytes());
    bytes[17..25].copy_from_slice(&0i64.to_le_bytes());
    // Safety: the buffer is 32 bytes, the struct starts at offset 1 and spans 24 of them.
    let code = unsafe {
        arioso_tts_synthesize(
            engine,
            text.as_ptr(),
            bytes.as_ptr().add(1).cast(),
            &raw mut buffer,
        )
    };
    // The engine has no voice, so the render fails; what matters is that the options were
    // read rather than refused for their size.
    assert_ne!(code, ARIOSO_ERR_STRUCT_SIZE, "{}", last_error());
    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
fn an_older_shorter_options_struct_is_read_only_up_to_its_size() {
    let dir = models_dir();
    let engine = engine(dir.path());
    let text = CString::new("hello").unwrap();
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };

    // A caller with an older header: `struct_size` and `speed`, nothing after them. The
    // library must read those twelve bytes and keep the default speaker, not read past the
    // end of what the caller has.
    let mut short = [0usize; 2];
    short[0] = 12;
    // Safety: the second half of an aligned buffer, written as the float the field holds.
    unsafe {
        short
            .as_mut_ptr()
            .cast::<u8>()
            .add(8)
            .cast::<f32>()
            .write(1.0)
    };
    // Safety: `short` is 16 bytes and the library reads at most the 12 declared.
    let code = unsafe {
        arioso_tts_synthesize(
            engine,
            text.as_ptr(),
            short.as_ptr().cast(),
            &raw mut buffer,
        )
    };
    // The engine has no voice in this directory, so the call fails after the options were
    // accepted; a struct-size refusal is what would have come first.
    assert_eq!(code, ARIOSO_ERR_ENGINE, "{}", last_error());

    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
fn empty_text_is_refused_before_the_engine_is_asked() {
    let dir = models_dir();
    let engine = engine(dir.path());
    let text = CString::new("   ").unwrap();
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };
    // Safety: `buffer` is writable for one buffer.
    let code =
        unsafe { arioso_tts_synthesize(engine, text.as_ptr(), ptr::null(), &raw mut buffer) };
    assert_eq!(code, ARIOSO_ERR_INVALID_ARGUMENT);
    assert_eq!(last_error(), "the text is empty");

    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
fn a_config_without_a_streaming_model_refuses_to_open_a_session() {
    let dir = models_dir();
    let engine = engine(dir.path());
    let mut session: *mut AriosoSttSession = ptr::null_mut();
    // Safety: `session` is writable for one pointer; a refusal nulls it.
    let code = unsafe { arioso_stt_open(engine, ptr::null(), &raw mut session) };
    assert_eq!(code, ARIOSO_ERR_ENGINE);
    assert!(session.is_null());
    let sentence = last_error();
    assert!(
        sentence.contains("streaming model"),
        "the refusal names what is missing: {sentence:?}"
    );

    // A rate the streaming models cannot take is refused before the model is touched.
    let options = AriosoSttOptions {
        struct_size: std::mem::size_of::<AriosoSttOptions>(),
        sample_rate: 44_100,
    };
    // Safety: `options` is a valid struct.
    let code = unsafe { arioso_stt_open(engine, &raw const options, &raw mut session) };
    assert_eq!(code, ARIOSO_ERR_INVALID_ARGUMENT);
    assert_eq!(last_error(), "send 16000 Hz audio, not 44100 Hz");

    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

// ---------------------------------------------------------------------------------------
// Tests that need a voice or a model
// ---------------------------------------------------------------------------------------

/// The models directory a voice test reads, or `None` when the suite was not given one.
fn voice_dir() -> Option<String> {
    match std::env::var("ARIOSO_TEST_MODELS_DIR") {
        Ok(dir) => Some(dir),
        Err(_) => {
            eprintln!("skipped: ARIOSO_TEST_MODELS_DIR is not set");
            None
        }
    }
}

/// An engine over the voice directory, with the caller's environment cleared.
///
/// A stale `ARIOSO_TTS_VOICE` would silently change which voice loads, so the config is
/// built from the directory and nothing else.
fn voice_engine(dir: &str) -> *mut AriosoEngine {
    let json = voice_config(Path::new(dir), "en_US-joe-medium", None);
    let mut engine: *mut AriosoEngine = ptr::null_mut();
    // Safety: `json` outlives the call and `engine` is writable for one pointer.
    let code = unsafe { arioso_engine_new(json.as_ptr(), &raw mut engine) };
    assert_eq!(code, ARIOSO_OK, "creating the engine: {}", last_error());
    engine
}

/// Count one chunk and keep going.
///
/// # Safety
///
/// `user` is the `AtomicUsize` the test handed over, alive for the call.
unsafe extern "C-unwind" fn count_chunks(
    _pcm: *const i16,
    samples: usize,
    _sample_rate: u32,
    user: *mut c_void,
) -> i32 {
    assert!(samples > 0, "a chunk with no samples in it");
    // Safety: the test passes a live counter and keeps it alive across the call.
    unsafe { &*user.cast::<AtomicUsize>() }.fetch_add(1, Ordering::SeqCst);
    0
}

/// Count one chunk, then cancel.
///
/// # Safety
///
/// As above.
unsafe extern "C-unwind" fn cancel_after_one(
    _pcm: *const i16,
    _samples: usize,
    _sample_rate: u32,
    user: *mut c_void,
) -> i32 {
    // Safety: as above.
    unsafe { &*user.cast::<AtomicUsize>() }.fetch_add(1, Ordering::SeqCst);
    1
}

/// Panic, which the library must catch before it reaches the engine's own frames.
///
/// # Safety
///
/// As above.
unsafe extern "C-unwind" fn explode(
    _pcm: *const i16,
    _samples: usize,
    _sample_rate: u32,
    _user: *mut c_void,
) -> i32 {
    panic!("the audio callback exploded");
}

#[test]
#[ignore = "needs a voice: set ARIOSO_TEST_MODELS_DIR"]
fn synthesis_writes_a_wav_and_the_free_path_releases_it() {
    let Some(dir) = voice_dir() else {
        return;
    };
    let engine = voice_engine(&dir);
    let text = CString::new("hello world. this is arioso.").unwrap();
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };
    // Safety: `buffer` is writable for one buffer and `text` outlives the call.
    let code =
        unsafe { arioso_tts_synthesize(engine, text.as_ptr(), ptr::null(), &raw mut buffer) };
    assert_eq!(code, ARIOSO_OK, "{}", last_error());
    assert!(!buffer.data.is_null());
    assert!(buffer.len > 44, "only {} bytes", buffer.len);

    // Safety: `buffer.len` bytes at `buffer.data` are the WAV the library wrote.
    let wav = unsafe { std::slice::from_raw_parts(buffer.data, buffer.len) };
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");

    // The only release: the caller's allocator is never asked to free this.
    // Safety: the pointer came from `arioso_tts_synthesize` and is freed once.
    unsafe { arioso_free(buffer.data.cast()) };

    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
    assert_eq!(last_error(), "");
}

#[test]
#[ignore = "needs a voice: set ARIOSO_TEST_MODELS_DIR"]
fn a_stream_hands_over_every_chunk() {
    let Some(dir) = voice_dir() else {
        return;
    };
    let engine = voice_engine(&dir);
    let text = CString::new("hello world. this is arioso.").unwrap();
    // A local counter, not a static: the callback travels through `user` so two tests
    // running in parallel do not count into each other's total.
    let chunks = AtomicUsize::new(0);
    let user = (&raw const chunks).cast_mut().cast::<c_void>();
    // Safety: the callback is a real function with the declared signature, and `user` is a
    // live counter the callback counts into.
    let code =
        unsafe { arioso_tts_stream(engine, text.as_ptr(), ptr::null(), Some(count_chunks), user) };
    assert_eq!(code, ARIOSO_OK, "{}", last_error());
    assert_eq!(chunks.load(Ordering::SeqCst), 2, "one chunk per sentence");

    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
#[ignore = "needs a voice: set ARIOSO_TEST_MODELS_DIR"]
fn a_callback_that_returns_nonzero_cancels_within_one_chunk() {
    let Some(dir) = voice_dir() else {
        return;
    };
    let engine = voice_engine(&dir);
    let text = CString::new("hello world. this is arioso.").unwrap();
    // A local counter, not a static: the callback travels through `user` so two tests
    // running in parallel do not count into each other's total.
    let chunks = AtomicUsize::new(0);
    let user = (&raw const chunks).cast_mut().cast::<c_void>();
    // Safety: as above; the callback is `cancel_after_one`.
    let code = unsafe {
        arioso_tts_stream(
            engine,
            text.as_ptr(),
            ptr::null(),
            Some(cancel_after_one),
            user,
        )
    };
    assert_eq!(code, ARIOSO_CANCELLED);
    assert_eq!(
        chunks.load(Ordering::SeqCst),
        1,
        "the engine stopped at the first chunk instead of finishing the text"
    );
    // A cancellation the caller asked for is not an error, so there is no sentence.
    assert_eq!(last_error(), "");

    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
#[ignore = "needs a voice: set ARIOSO_TEST_MODELS_DIR"]
fn a_panic_in_the_callback_is_a_code_and_a_sentence() {
    let Some(dir) = voice_dir() else {
        return;
    };
    let engine = voice_engine(&dir);
    let text = CString::new("hello world. this is arioso.").unwrap();
    // Safety: as above; the callback is `explode`, which panics on the first chunk.
    let code = unsafe {
        arioso_tts_stream(
            engine,
            text.as_ptr(),
            ptr::null(),
            Some(explode),
            ptr::null_mut(),
        )
    };
    assert_eq!(code, ARIOSO_ERR_PANIC);
    let sentence = last_error();
    assert!(
        sentence.contains("the audio callback exploded"),
        "the panic's own message is what the caller reads: {sentence:?}"
    );

    // Safety: the handle came from `arioso_engine_new` and is freed once.
    unsafe { arioso_engine_free(engine) };
}

#[test]
#[ignore = "needs a voice and a streaming model: set ARIOSO_TEST_MODELS_DIR"]
fn a_session_transcribes_speech_the_voice_spoke() {
    let Some(dir) = voice_dir() else {
        return;
    };
    // One engine with both directions: a voice and the streaming model.
    let json = voice_config(
        Path::new(&dir),
        "en_US-joe-medium",
        Some("sherpa-zipformer-streaming-en"),
    );
    let mut engine: *mut AriosoEngine = ptr::null_mut();
    // Safety: `json` outlives the call; `engine` is writable for one pointer.
    let code = unsafe { arioso_engine_new(json.as_ptr(), &raw mut engine) };
    if code != ARIOSO_OK {
        eprintln!(
            "skipped: no voice and streaming model in the directory: {}",
            last_error()
        );
        return;
    }

    // Speak, through the C surface.
    let spoken = CString::new("hello world, this is arioso speaking.").unwrap();
    let mut buffer = AriosoBuffer {
        data: ptr::null_mut(),
        len: 0,
    };
    // Safety: `buffer` is writable for one buffer and `spoken` outlives the call.
    let code =
        unsafe { arioso_tts_synthesize(engine, spoken.as_ptr(), ptr::null(), &raw mut buffer) };
    assert_eq!(code, ARIOSO_OK, "{}", last_error());
    // Safety: `buffer.len` bytes at `buffer.data` are the WAV the library wrote.
    let spoken_wav = unsafe { std::slice::from_raw_parts(buffer.data, buffer.len) }.to_vec();
    // Safety: the pointer came from `arioso_tts_synthesize` and is freed once.
    unsafe { arioso_free(buffer.data.cast()) };

    let (samples, rate) = wav::read_bytes(&spoken_wav).unwrap();
    let mut audio = resample(&samples, rate, 16_000);
    // A second of silence after the words: the model decodes what it has when the audio
    // stops, and an utterance that ends on a phoneme has nothing after it to flush with.
    audio.extend(std::iter::repeat_n(0.0, 16_000));

    // Hear it back, through the C surface.
    let mut session: *mut AriosoSttSession = ptr::null_mut();
    // Safety: `session` is writable for one pointer.
    let code = unsafe { arioso_stt_open(engine, ptr::null(), &raw mut session) };
    if code != ARIOSO_OK {
        eprintln!("skipped: no streaming model installed: {}", last_error());
        // Safety: the handle came from `arioso_engine_new` and is freed once.
        unsafe { arioso_engine_free(engine) };
        return;
    }
    for chunk in audio.chunks(1_600) {
        let pcm: Vec<i16> = chunk
            .iter()
            .map(|sample| (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16)
            .collect();
        // Safety: `pcm` is `pcm.len()` readable values.
        let code = unsafe { arioso_stt_push(session, pcm.as_ptr(), pcm.len(), 16_000) };
        assert_eq!(code, ARIOSO_OK, "{}", last_error());
    }

    // A null callback is refused and the session survives it: nothing was taken.
    // Safety: `session` is live and `on_event` is null, which is refused, not dereferenced.
    let code = unsafe { arioso_stt_finish(session, None, ptr::null_mut()) };
    assert_eq!(code, ARIOSO_ERR_NULL_ARGUMENT);

    let mut heard: Option<String> = None;
    // Safety: the session is live, the callback is a real function, and `user` points at
    // `heard`, which outlives the call.
    let code =
        unsafe { arioso_stt_finish(session, Some(keep_text), (&raw mut heard).cast::<c_void>()) };
    assert_eq!(code, ARIOSO_OK, "{}", last_error());
    let heard = heard.expect("the callback ran");
    assert!(
        !heard.trim().is_empty(),
        "the model heard nothing in {:.2} s of speech",
        audio.len() as f32 / 16_000.0
    );
    eprintln!("heard: {heard:?}");

    // Safety: the session came from `arioso_stt_open` and is closed once.
    unsafe {
        arioso_stt_close(session);
        arioso_engine_free(engine);
    }
}

/// Copy the event's text out, which is the only thing a caller may do with it.
///
/// # Safety
///
/// `event` is the library's, valid for this call; `user` is the `Option<String>` the test
/// keeps alive across the call.
unsafe extern "C-unwind" fn keep_text(event: *const AriosoSttEvent, user: *mut c_void) -> i32 {
    // Safety: the library passes a live event for the duration of this call.
    let event = unsafe { &*event };
    assert_eq!(event.is_final, 1);
    assert!(!event.text.is_null());
    // Safety: the text is a NUL-terminated string the library owns for this call.
    let text = unsafe { CStr::from_ptr(event.text) }
        .to_string_lossy()
        .into_owned();
    // Safety: the test passes a live `Option<String>` and reads it after the call.
    unsafe { *user.cast::<Option<String>>() = Some(text) };
    0
}
