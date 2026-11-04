//! The C ABI for arioso.
//!
//! One crate, three library shapes: `cdylib` for a caller that loads the library,
//! `staticlib` for a caller that links it into an app bundle, and `rlib` so the tests in
//! this crate can call the exported functions the way C does.
//!
//! The surface is the one PRD F10.7 names and nothing else: an engine built from a config
//! document, synthesis to a buffer or to a callback, streaming transcription, and one way to
//! release what the library allocated. Downloads and mobile smoke tests are deliberately
//! absent (F10.8).
//!
//! Three rules hold at every entry point, and each has a test:
//!
//! - No panic crosses the boundary. A panic becomes [`ARIOSO_ERR_PANIC`] with its message in
//!   `arioso_last_error`, because unwinding into a C frame is undefined behaviour.
//! - Nothing this library allocates is freed by the caller's allocator: `arioso_free` is the
//!   only release, and it recovers the length from a header in front of the payload.
//! - A null handle, or a null output pointer, is a code rather than a crash.
//!
//! The header at `include/arioso.h` is generated from this file. Regenerate it with
//! `abi/generate-header.sh`; CI fails when the committed header and the code disagree.

// Every `unsafe` block here is an obligation the C caller took on, so each one carries a
// comment naming it. Cargo cannot mix inherited workspace lints with one extra entry, so the
// lint is turned on here, where the unsafe code is.
#![warn(clippy::undocumented_unsafe_blocks)]

use std::ffi::{CStr, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, Mutex};

use arioso_config::Config;
use arioso_models::Manifest;
use arioso_server::{AppState, SidProblem};
use arioso_stt_sherpa::StreamSession;

mod error;
mod memory;

use error::{fail, guard};

/// The version of this surface. It changes when a function is added, removed, or given a
/// meaning a caller could not have known.
pub const ARIOSO_ABI_VERSION: u32 = 1;

/// Success.
pub const ARIOSO_OK: i32 = 0;

/// The caller's callback returned nonzero, so the operation stopped. This is not an error:
/// the caller asked for it, and `arioso_last_error` stays empty.
pub const ARIOSO_CANCELLED: i32 = 1;

/// A required pointer was null. Nothing was read, nothing was allocated.
pub const ARIOSO_ERR_NULL_ARGUMENT: i32 = -1;

/// An argument was present but unusable: text that is not UTF-8 or is empty, a speed outside
/// the range the engine accepts, or a sample rate the streaming model cannot take. The
/// sentence names the argument and what it should have been.
pub const ARIOSO_ERR_INVALID_ARGUMENT: i32 = -2;

/// The config document was refused: malformed JSON, or a value a rule rejects. The sentence
/// names the field, because the caller is the one who can fix it.
pub const ARIOSO_ERR_CONFIG: i32 = -3;

/// The engine could not do the work: a manifest or dictionary that is missing, a voice that
/// failed to load, a text with nothing the voice can pronounce.
pub const ARIOSO_ERR_ENGINE: i32 = -4;

/// A `struct_size` this library cannot honour. Either it is larger than the struct this
/// build knows — pass the size of the struct your header declares — or it is too small to
/// hold the size field itself.
pub const ARIOSO_ERR_STRUCT_SIZE: i32 = -5;

/// A panic was caught at the boundary. The sentence is the panic's own message. The engine
/// is still usable: the panic is reported, not propagated.
pub const ARIOSO_ERR_PANIC: i32 = -6;

/// The rate the streaming transcription models in the catalog take.
const STREAM_RATE: u32 = 16_000;

// ---------------------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------------------

/// An engine: one config, one voice slot, one transcription slot.
///
/// Opaque to C. Create it with `arioso_engine_new` and release it with `arioso_engine_free`;
/// no other function frees it. A handle may be used from several threads: the engine
/// serializes what it must internally, and a call that is running is not interrupted by
/// another thread's call.
pub struct AriosoEngine {
    /// The state the HTTP server uses, so a C caller and a server share one implementation
    /// of synthesis, transcription, and the dictionary.
    state: Arc<AppState>,
}

impl std::fmt::Debug for AriosoEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AriosoEngine")
    }
}

/// One streaming transcription utterance.
///
/// Opaque to C. Create it with `arioso_stt_open`, feed it with `arioso_stt_push`, close it
/// with `arioso_stt_finish` or `arioso_stt_close`. A session hears one utterance: after
/// `arioso_stt_finish` the handle is spent and must be closed.
pub struct AriosoSttSession {
    /// `None` once the utterance has been finished.
    inner: Mutex<SessionState>,
}

/// What one session holds.
struct SessionState {
    /// The model's state, until the utterance ends.
    session: Option<StreamSession>,
    /// The audio of the current push, kept between calls so a streaming client that sends
    /// ten pushes a second does not allocate ten times a second.
    scratch: Vec<f32>,
    /// The last partial the model reported. The tail decode at the end can come back empty
    /// for an utterance that stopped mid-word; the partial is what the model had then.
    last: String,
}

impl std::fmt::Debug for AriosoSttSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AriosoSttSession")
    }
}

// ---------------------------------------------------------------------------------------
// Structs that cross the boundary
// ---------------------------------------------------------------------------------------

/// A buffer this library allocated.
///
/// `data` is `len` bytes and belongs to the library until the caller passes it to
/// `arioso_free`; freeing it with `free(3)` or `delete` is undefined behaviour. The bytes are
/// a complete WAV file: 44-byte header, mono, 16-bit little-endian PCM.
#[repr(C)]
#[derive(Debug)]
pub struct AriosoBuffer {
    /// WAV bytes, owned by the library. Release with `arioso_free(buffer.data)`.
    pub data: *mut u8,
    /// Length of `data` in bytes.
    pub len: usize,
}

/// How to synthesize.
///
/// `struct_size` must be `sizeof(AriosoTtsOptions)` from the header you compiled against.
/// A size larger than this library knows is refused with `ARIOSO_ERR_STRUCT_SIZE`; a smaller
/// one is read only up to the bytes you declared, and the fields past them keep the defaults
/// described here. Fields are only ever appended.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AriosoTtsOptions {
    /// `sizeof(AriosoTtsOptions)`, as your header declares it.
    pub struct_size: usize,
    /// Speaking rate. `0.0` means the config's `tts.speed`; anything else must be within
    /// 0.5 to 2.0, the same range the HTTP route accepts.
    pub speed: f32,
    /// Speaker index. `0` is the only speaker most voices have; a multi-speaker Piper voice
    /// takes an index the voice reports.
    pub speaker: i64,
}

/// The `struct_size` values an options struct accepts.
///
/// A caller may declare a prefix of the struct, so that a header compiled against an older
/// release keeps working: the fields past the declared bytes keep their defaults. Only sizes
/// that end exactly at the end of a field are valid, because a size that stops inside one
/// would splice half an `f32` or half an `i64` onto a default value.
trait AbiOptions: Copy + Default {
    /// The sizes this library accepts, in bytes.
    const VALID_SIZES: &'static [usize];
}

impl AbiOptions for AriosoTtsOptions {
    // The size field alone, then a struct that ends after `speed` (twelve bytes when the
    // caller packed it, sixteen when the compiler padded it), then the whole struct.
    const VALID_SIZES: &'static [usize] = &[
        std::mem::size_of::<usize>(),
        std::mem::offset_of!(Self, speed) + std::mem::size_of::<f32>(),
        std::mem::offset_of!(Self, speaker),
        std::mem::size_of::<Self>(),
    ];
}

impl AbiOptions for AriosoSttOptions {
    // The size field alone, then a struct that ends after `sample_rate`, then the whole
    // struct, which pads that field out to the alignment of the size field.
    const VALID_SIZES: &'static [usize] = &[
        std::mem::size_of::<usize>(),
        std::mem::offset_of!(Self, sample_rate) + std::mem::size_of::<u32>(),
        std::mem::size_of::<Self>(),
    ];
}

/// How to transcribe.
///
/// `struct_size` follows the same rule as `AriosoTtsOptions`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AriosoSttOptions {
    /// `sizeof(AriosoSttOptions)`, as your header declares it.
    pub struct_size: usize,
    /// Sample rate of the PCM you will push. The streaming models in the catalog take
    /// 16000 Hz; any other rate is refused rather than silently resampled. `0` means 16000.
    pub sample_rate: u32,
}

/// Called with download progress.
///
/// `received` and `total` are bytes, and `total` is the size of the whole model rather than
/// of the file being fetched. Return `0` to keep going, or any nonzero value to stop: the
/// fetch is dropped at its next step, the partial files stay on disk, and a later call
/// resumes them. The callback runs on the thread that called `arioso_model_download`.
pub type AriosoProgressCallback =
    Option<unsafe extern "C-unwind" fn(received: u64, total: u64, user: *mut c_void) -> i32>;

/// One thing the recognizer has to say.
///
/// Filled by the library for the duration of the callback: `text` points into memory the
/// library owns and is valid only until the callback returns. Copy it if you want to keep it.
#[repr(C)]
#[derive(Debug)]
pub struct AriosoSttEvent {
    /// `sizeof(AriosoSttEvent)` for the struct the library filled. Do not set it.
    pub struct_size: usize,
    /// The utterance's text, NUL-terminated UTF-8, valid for the call only.
    pub text: *const c_char,
    /// `1` when the utterance ended and the text is final. v1 delivers no other event.
    pub is_final: i32,
}

/// Called with audio as it is synthesized.
///
/// `pcm` is `samples` 16-bit little-endian mono samples at `sample_rate`, valid for the call
/// only. Return `0` to keep going, or any nonzero value to cancel: synthesis stops at the
/// next chunk boundary and the call answers `ARIOSO_CANCELLED`.
///
/// The callback runs on the thread that called `arioso_tts_stream`, inside that call.
pub type AriosoAudioCallback = Option<
    unsafe extern "C-unwind" fn(
        pcm: *const i16,
        samples: usize,
        sample_rate: u32,
        user: *mut c_void,
    ) -> i32,
>;

/// Called with the result of a transcription.
///
/// Return `0` to accept it, or any nonzero value to cancel: the call answers
/// `ARIOSO_CANCELLED` and the session is closed.
///
/// The callback runs on the thread that called `arioso_stt_finish`, inside that call.
pub type AriosoEventCallback =
    Option<unsafe extern "C-unwind" fn(event: *const AriosoSttEvent, user: *mut c_void) -> i32>;

// ---------------------------------------------------------------------------------------
// Version and errors
// ---------------------------------------------------------------------------------------

/// The ABI version this library implements: `1` for the functions in this header.
///
/// Does not block.
#[unsafe(no_mangle)]
pub extern "C" fn arioso_abi_version() -> u32 {
    // A constant read cannot panic; the guard is here because every entry point has one.
    catch_unwind(|| ARIOSO_ABI_VERSION).unwrap_or(0)
}

/// The last failure on this thread, as a NUL-terminated UTF-8 sentence.
///
/// Never null, and never freed by the caller: the string lives in thread-local storage and
/// is valid until the next call on the same thread. It is empty after a call that returned
/// `ARIOSO_OK` or `ARIOSO_CANCELLED`.
///
/// Does not block.
#[unsafe(no_mangle)]
pub extern "C" fn arioso_last_error() -> *const c_char {
    catch_unwind(error::pointer).unwrap_or(error::EMPTY.as_ptr().cast())
}

/// Release memory this library allocated.
///
/// The only argument it accepts is a pointer this library returned, which today means
/// `AriosoBuffer.data` from `arioso_tts_synthesize`. Passing null is a no-op; passing
/// anything else, or the same pointer twice, is undefined behaviour.
///
/// Does not block.
///
/// # Safety
///
/// `pointer` must be null, or a pointer this library returned as `AriosoBuffer.data` that has
/// not been released yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_free(pointer: *mut c_void) {
    let _ = guard(|| {
        if pointer.is_null() {
            return Ok(());
        }
        // Safety: the caller guarantees this is a pointer `copy_out` allocated, which is the
        // only kind that carries the length header `release` reads.
        unsafe { memory::release(pointer.cast::<u8>()) };
        Ok(())
    });
}

// ---------------------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------------------

/// Create an engine from a JSON config document.
///
/// The document is the one the server and the CLI read. Two files must be in the config's
/// models directory: `manifest.json`, the model catalog, and `cmudict.dict`, the
/// pronunciation dictionary. The voice and the speech model load on first use, so a caller
/// that only transcribes never pays for a voice.
///
/// On success `*out` holds a handle to release with `arioso_engine_free`. On any failure
/// `*out` is null.
///
/// Blocks: reads two files from the models directory. Milliseconds, not seconds.
///
/// # Safety
///
/// `config_json` must be a NUL-terminated string valid for the call, and `out` must be
/// writable for one `AriosoEngine *`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_engine_new(
    config_json: *const c_char,
    out: *mut *mut AriosoEngine,
) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(fail(
                ARIOSO_ERR_NULL_ARGUMENT,
                "arioso_engine_new: out is null",
            ));
        }
        // Safety: `out` is non-null and the caller guarantees it is writable. It is nulled
        // first so a refusal never leaves a pointer the caller would try to free.
        unsafe { out.write(std::ptr::null_mut()) };

        // Safety: the caller guarantees a NUL-terminated string valid for this call.
        let text = unsafe { c_str(config_json, "arioso_engine_new: config_json") }?;
        let config = Config::from_json(text, Path::new("config.json"))
            .map_err(|source| fail(ARIOSO_ERR_CONFIG, source.to_string()))?;
        let manifest_path = arioso_models::models_dir(&config).join("manifest.json");
        let manifest = Manifest::load(&manifest_path).map_err(|source| {
            fail(
                ARIOSO_ERR_ENGINE,
                format!(
                    "loading the model manifest at {}: {source}",
                    manifest_path.display()
                ),
            )
        })?;
        let state = AppState::new(config, manifest, None)
            .map_err(|source| fail(ARIOSO_ERR_ENGINE, source.to_string()))?;
        let engine = Box::into_raw(Box::new(AriosoEngine {
            state: Arc::new(state),
        }));
        // Safety: as above, `out` is writable for one pointer.
        unsafe { out.write(engine) };
        Ok(())
    })
}

/// Release an engine and everything it loaded.
///
/// Passing null is a no-op. A handle is released once; using it afterwards is undefined
/// behaviour. Blocks: no. Dropping the engine frees the voice and the model but performs no
/// I/O.
///
/// # Safety
///
/// `engine` must be null, or a handle from `arioso_engine_new` that has not been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_engine_free(engine: *mut AriosoEngine) {
    let _ = guard(|| {
        if engine.is_null() {
            return Ok(());
        }
        // Safety: the caller guarantees the handle came from `arioso_engine_new` and is
        // freed once, so this is the only owner.
        drop(unsafe { Box::from_raw(engine) });
        Ok(())
    });
}

// ---------------------------------------------------------------------------------------
// Text to speech
// ---------------------------------------------------------------------------------------

/// Synthesize text into a WAV file in memory.
///
/// `*out` receives the bytes, owned by the library until `arioso_free(buffer.data)`. On any
/// failure `*out` is `{ NULL, 0 }`.
///
/// The first call loads the voice and blocks for it; a Piper voice is tens of megabytes and a
/// Kokoro graph is a few hundred. Every call then blocks for the whole synthesis: there is no
/// callback to cancel, so this is the shape to use for a short string, not for a document.
///
/// # Safety
///
/// `engine` must be a live handle from `arioso_engine_new`, `text` a NUL-terminated string
/// valid for the call, `options` null or a valid `AriosoTtsOptions`, and `out` writable for
/// one `AriosoBuffer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_tts_synthesize(
    engine: *mut AriosoEngine,
    text: *const c_char,
    options: *const AriosoTtsOptions,
    out: *mut AriosoBuffer,
) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(fail(
                ARIOSO_ERR_NULL_ARGUMENT,
                "arioso_tts_synthesize: out is null",
            ));
        }
        // Safety: `out` is non-null and writable by the caller's contract; an empty buffer
        // first means a refusal hands back nothing to free.
        unsafe {
            out.write(AriosoBuffer {
                data: std::ptr::null_mut(),
                len: 0,
            });
        }
        // Safety: the caller guarantees a live handle.
        let state = unsafe { state_of(engine, "arioso_tts_synthesize") }?;
        // Safety: the caller guarantees a NUL-terminated string valid for the call.
        let text = unsafe { c_str(text, "arioso_tts_synthesize: text") }?;
        check_text(state, text)?;
        // Safety: null means the defaults; otherwise the caller guarantees `struct_size`
        // readable bytes starting with the size field.
        let options = unsafe {
            read_options(
                options,
                AriosoTtsOptions::default(),
                "arioso_tts_synthesize: options",
            )
        }?;
        let speed = speed_of(state, options.speed)?;
        let sid = tts_sid(state, options.speaker)?;

        let wav = state
            .synthesize_wav(text, speed, sid)
            .map_err(|detail| fail(ARIOSO_ERR_ENGINE, detail))?;
        // Safety: the bytes are released by `arioso_free` and by nothing else.
        let (data, len) = unsafe { memory::copy_out(&wav) };
        if data.is_null() {
            return Err(fail(ARIOSO_ERR_ENGINE, "the audio could not be allocated"));
        }
        // Safety: `out` is writable for one buffer, as checked above.
        unsafe {
            out.write(AriosoBuffer { data, len });
        }
        Ok(())
    })
}

/// Synthesize text, handing each chunk of audio to `on_audio` as it is produced.
///
/// The callback runs inside this call, on the calling thread, once per synthesized chunk. A
/// nonzero return cancels: synthesis stops within one chunk and this function answers
/// `ARIOSO_CANCELLED` with no error sentence.
///
/// Blocks for the whole synthesis, which is the point: a caller that wants audio early gets
/// it early instead of at the end.
///
/// # Safety
///
/// `engine` must be a live handle, `text` a NUL-terminated string valid for the call,
/// `options` null or valid, and `on_audio` non-null. `user` is handed back to the callback
/// untouched and must stay valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_tts_stream(
    engine: *mut AriosoEngine,
    text: *const c_char,
    options: *const AriosoTtsOptions,
    on_audio: AriosoAudioCallback,
    user: *mut c_void,
) -> i32 {
    guard(|| {
        // Safety: the caller guarantees a live handle.
        let state = unsafe { state_of(engine, "arioso_tts_stream") }?;
        // Safety: the caller guarantees a NUL-terminated string valid for the call.
        let text = unsafe { c_str(text, "arioso_tts_stream: text") }?;
        check_text(state, text)?;
        let Some(callback) = on_audio else {
            return Err(fail(
                ARIOSO_ERR_NULL_ARGUMENT,
                "arioso_tts_stream: on_audio is null",
            ));
        };
        // Safety: null means the defaults; otherwise `struct_size` readable bytes.
        let options = unsafe {
            read_options(
                options,
                AriosoTtsOptions::default(),
                "arioso_tts_stream: options",
            )
        }?;
        let speed = speed_of(state, options.speed)?;
        let sid = tts_sid(state, options.speaker)?;

        // The code the callback stopped on, kept outside the sink: the engines report a sink
        // failure as a string, which cannot say whether the caller cancelled or the engine
        // broke.
        let mut stopped: Option<i32> = None;
        // One buffer for the whole stream, filled straight from the `f32` chunk: encoding to
        // little-endian bytes and decoding them back would allocate twice per chunk to hand
        // the callback the same values it started with.
        let mut samples: Vec<i16> = Vec::new();
        let rendered = state.synthesize_stream(text, speed, sid, |chunk, rate| {
            samples.clear();
            samples.extend(
                chunk
                    .iter()
                    .map(|&sample| (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16),
            );
            match invoke_audio(callback, &samples, rate, user) {
                Ok(()) => Ok(()),
                Err(code) => {
                    stopped = Some(code);
                    Err(code.to_string())
                }
            }
        });
        if let Some(code) = stopped {
            return Err(code);
        }
        rendered.map_err(|detail| fail(ARIOSO_ERR_ENGINE, detail))
    })
}

// ---------------------------------------------------------------------------------------
// Catalog and phonemes
// ---------------------------------------------------------------------------------------

/// The catalog as one JSON document.
///
/// An array of objects: `id`, `kind`, `engine`, `license`, `commercial_use`, `installed`,
/// `state`, `bytes`, `speakers`, `languages`. `bytes` is the whole model, and `state` is
/// `installed`, `downloading`, or `not_installed`.
///
/// `*out` receives the bytes, owned by the library until `arioso_free(buffer.data)`, and the
/// document is not NUL-terminated: `len` is its length. On any failure `*out` is `{ NULL, 0 }`.
///
/// # Safety
///
/// `engine` must be a live handle from `arioso_engine_new` and `out` writable for one
/// `AriosoBuffer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_models_json(
    engine: *mut AriosoEngine,
    out: *mut AriosoBuffer,
) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(fail(
                ARIOSO_ERR_NULL_ARGUMENT,
                "arioso_models_json: out is null",
            ));
        }
        // Safety: `out` is writable by the caller's contract; an empty buffer first means a
        // refusal hands back nothing to free.
        unsafe {
            out.write(AriosoBuffer {
                data: std::ptr::null_mut(),
                len: 0,
            });
        }
        // Safety: the caller guarantees a live handle.
        let state = unsafe { state_of(engine, "arioso_models_json") }?;
        let document = state.models_json();
        // Safety: the bytes are released by `arioso_free` and by nothing else.
        let (data, len) = unsafe { memory::copy_out(document.as_bytes()) };
        if data.is_null() {
            return Err(fail(
                ARIOSO_ERR_ENGINE,
                "the catalog could not be allocated",
            ));
        }
        // Safety: `out` is writable for one buffer, as checked above.
        unsafe {
            out.write(AriosoBuffer { data, len });
        }
        Ok(())
    })
}

/// Fetch one model and wait for it, reporting progress to `on_progress`.
///
/// A model already on disk answers `ARIOSO_OK` without touching the network. `on_progress`
/// may be null, which reports nothing.
///
/// Blocks for the whole download, which is minutes on a slow link. A callback that returns
/// nonzero stops the fetch and this answers `ARIOSO_CANCELLED` with no error sentence.
///
/// # Safety
///
/// `engine` must be a live handle, `id` a NUL-terminated string valid for the call, and
/// `user` handed back to the callback untouched, so it must stay valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_model_download(
    engine: *mut AriosoEngine,
    id: *const c_char,
    on_progress: AriosoProgressCallback,
    user: *mut c_void,
) -> i32 {
    guard(|| {
        // Safety: the caller guarantees a live handle.
        let state = unsafe { state_of(engine, "arioso_model_download") }?;
        // Safety: the caller guarantees a NUL-terminated string valid for the call.
        let id = unsafe { c_str(id, "arioso_model_download: id") }?;
        let mut cancelled = false;
        let result = state.download_blocking(id, |received, total| {
            let Some(callback) = on_progress else {
                // No callback is no reason to stop.
                return false;
            };
            // Safety: the callback is the caller's, and `user` is theirs to keep valid.
            let answer = unsafe { callback(received, total, user) };
            cancelled = answer != 0;
            // The C contract is "return 0 to continue, nonzero to stop", which is the same
            // answer this closure must give.
            cancelled
        });
        if cancelled {
            return Err(ARIOSO_CANCELLED);
        }
        result
            .map(|_| ())
            .map_err(|detail| fail(ARIOSO_ERR_ENGINE, detail))
    })
}

/// The phonemes a text will be spoken as, as the loaded voice builds them.
///
/// The answer is IPA in UTF-8 and is not NUL-terminated: `len` is its length. It is what the
/// same text would be synthesized from, so a caller can show it and then edit it.
///
/// The first call loads the voice and blocks for it.
///
/// # Safety
///
/// `engine` must be a live handle, `text` a NUL-terminated string valid for the call, and
/// `out` writable for one `AriosoBuffer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_phonemes(
    engine: *mut AriosoEngine,
    text: *const c_char,
    out: *mut AriosoBuffer,
) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(fail(
                ARIOSO_ERR_NULL_ARGUMENT,
                "arioso_phonemes: out is null",
            ));
        }
        // Safety: `out` is writable by the caller's contract.
        unsafe {
            out.write(AriosoBuffer {
                data: std::ptr::null_mut(),
                len: 0,
            });
        }
        // Safety: the caller guarantees a live handle.
        let state = unsafe { state_of(engine, "arioso_phonemes") }?;
        // Safety: the caller guarantees a NUL-terminated string valid for the call.
        let text = unsafe { c_str(text, "arioso_phonemes: text") }?;
        let line = state
            .phonemes_for(text)
            .map_err(|detail| fail(ARIOSO_ERR_INVALID_ARGUMENT, detail))?;
        // Safety: the bytes are released by `arioso_free` and by nothing else.
        let (data, len) = unsafe { memory::copy_out(line.as_bytes()) };
        if data.is_null() {
            return Err(fail(
                ARIOSO_ERR_ENGINE,
                "the phonemes could not be allocated",
            ));
        }
        // Safety: `out` is writable for one buffer, as checked above.
        unsafe {
            out.write(AriosoBuffer { data, len });
        }
        Ok(())
    })
}

/// Synthesize phonemes a caller supplied into a WAV file in memory.
///
/// The phonemes are spoken as written. A character that is not a phoneme, or a symbol this
/// voice has no id for, is refused with the symbol named: a caller that asked for a sound and
/// heard a different word has no way to notice.
///
/// # Safety
///
/// `engine` must be a live handle, `phonemes` a NUL-terminated string valid for the call,
/// `options` null or a valid `AriosoTtsOptions`, and `out` writable for one `AriosoBuffer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_tts_phonemes(
    engine: *mut AriosoEngine,
    phonemes: *const c_char,
    options: *const AriosoTtsOptions,
    out: *mut AriosoBuffer,
) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(fail(
                ARIOSO_ERR_NULL_ARGUMENT,
                "arioso_tts_phonemes: out is null",
            ));
        }
        // Safety: `out` is writable by the caller's contract.
        unsafe {
            out.write(AriosoBuffer {
                data: std::ptr::null_mut(),
                len: 0,
            });
        }
        // Safety: the caller guarantees a live handle.
        let state = unsafe { state_of(engine, "arioso_tts_phonemes") }?;
        // Safety: the caller guarantees a NUL-terminated string valid for the call.
        let phonemes = unsafe { c_str(phonemes, "arioso_tts_phonemes: phonemes") }?;
        // The characters are checked before the voice is touched: a caller who mistyped a
        // phoneme should not be told about a voice they never asked about.
        arioso_server::parse_phonemes(phonemes)
            .map_err(|detail| fail(ARIOSO_ERR_INVALID_ARGUMENT, detail))?;
        // Safety: null means the defaults; otherwise `struct_size` readable bytes.
        let options = unsafe {
            read_options(
                options,
                AriosoTtsOptions::default(),
                "arioso_tts_phonemes: options",
            )
        }?;
        let speed = speed_of(state, options.speed)?;
        let sid = tts_sid(state, options.speaker)?;
        let wav = state
            .synthesize_phonemes_wav(phonemes, speed, sid)
            .map_err(|detail| fail(ARIOSO_ERR_INVALID_ARGUMENT, detail))?;
        // Safety: the bytes are released by `arioso_free` and by nothing else.
        let (data, len) = unsafe { memory::copy_out(&wav) };
        if data.is_null() {
            return Err(fail(ARIOSO_ERR_ENGINE, "the audio could not be allocated"));
        }
        // Safety: `out` is writable for one buffer, as checked above.
        unsafe {
            out.write(AriosoBuffer { data, len });
        }
        Ok(())
    })
}

/// Synthesize phonemes, handing each chunk of audio to `on_audio` as it is produced.
///
/// The same contract as [`arioso_tts_stream`], from phonemes instead of text.
///
/// # Safety
///
/// `engine` must be a live handle, `phonemes` a NUL-terminated string valid for the call,
/// `options` null or valid, and `on_audio` non-null. `user` is handed back to the callback
/// untouched and must stay valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_tts_phonemes_stream(
    engine: *mut AriosoEngine,
    phonemes: *const c_char,
    options: *const AriosoTtsOptions,
    on_audio: AriosoAudioCallback,
    user: *mut c_void,
) -> i32 {
    guard(|| {
        // Safety: the caller guarantees a live handle.
        let state = unsafe { state_of(engine, "arioso_tts_phonemes_stream") }?;
        // Safety: the caller guarantees a NUL-terminated string valid for the call.
        let phonemes = unsafe { c_str(phonemes, "arioso_tts_phonemes_stream: phonemes") }?;
        arioso_server::parse_phonemes(phonemes)
            .map_err(|detail| fail(ARIOSO_ERR_INVALID_ARGUMENT, detail))?;
        let Some(callback) = on_audio else {
            return Err(fail(
                ARIOSO_ERR_NULL_ARGUMENT,
                "arioso_tts_phonemes_stream: on_audio is null",
            ));
        };
        // Safety: null means the defaults; otherwise `struct_size` readable bytes.
        let options = unsafe {
            read_options(
                options,
                AriosoTtsOptions::default(),
                "arioso_tts_phonemes_stream: options",
            )
        }?;
        let speed = speed_of(state, options.speed)?;
        let sid = tts_sid(state, options.speaker)?;

        let mut stopped: Option<i32> = None;
        let mut samples: Vec<i16> = Vec::new();
        let rendered = state.render_phonemes_stream(phonemes, speed, sid, |chunk, rate| {
            samples.clear();
            samples.extend(
                chunk
                    .iter()
                    .map(|&sample| (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16),
            );
            match invoke_audio(callback, &samples, rate, user) {
                Ok(()) => Ok(()),
                Err(code) => {
                    stopped = Some(code);
                    Err(code.to_string())
                }
            }
        });
        if let Some(code) = stopped {
            return Err(code);
        }
        rendered.map_err(|detail| fail(ARIOSO_ERR_INVALID_ARGUMENT, detail))
    })
}

// ---------------------------------------------------------------------------------------
// Speech to text
// ---------------------------------------------------------------------------------------

/// Open a streaming transcription session.
///
/// The streaming model named by `stt.stream_model` must be configured and installed; a config
/// without one refuses here rather than returning a session that cannot transcribe. The first
/// open in a process blocks for the model load.
///
/// On success `*out` holds a session to release with `arioso_stt_finish` or
/// `arioso_stt_close`. On any failure `*out` is null.
///
/// # Safety
///
/// `engine` must be a live handle, `options` null or a valid `AriosoSttOptions`, and `out`
/// writable for one `AriosoSttSession *`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_stt_open(
    engine: *mut AriosoEngine,
    options: *const AriosoSttOptions,
    out: *mut *mut AriosoSttSession,
) -> i32 {
    guard(|| {
        if out.is_null() {
            return Err(fail(
                ARIOSO_ERR_NULL_ARGUMENT,
                "arioso_stt_open: out is null",
            ));
        }
        // Safety: non-null and writable by the caller's contract; nulled so a refusal hands
        // back nothing to close.
        unsafe { out.write(std::ptr::null_mut()) };
        // Safety: the caller guarantees a live handle.
        let state = unsafe { state_of(engine, "arioso_stt_open") }?;
        // Safety: null means the defaults; otherwise `struct_size` readable bytes.
        let options = unsafe {
            read_options(
                options,
                AriosoSttOptions::default(),
                "arioso_stt_open: options",
            )
        }?;
        check_rate(options.sample_rate)?;

        let session = state
            .open_stream_session()
            .map_err(|detail| fail(ARIOSO_ERR_ENGINE, detail))?;
        let handle = Box::into_raw(Box::new(AriosoSttSession {
            inner: Mutex::new(SessionState {
                session: Some(session),
                scratch: Vec::new(),
                last: String::new(),
            }),
        }));
        // Safety: as above, `out` is writable for one pointer.
        unsafe { out.write(handle) };
        Ok(())
    })
}

/// Feed 16-bit PCM to a session.
///
/// `pcm` is `samples` mono samples at `sample_rate`, which must be 16000: the streaming
/// models in the catalog take that rate, and a different one is refused rather than
/// resampled behind the caller's back. Pushing nothing is not an error.
///
/// Partials the model forms between pushes are not reported; v1 delivers one event, at
/// `arioso_stt_finish`.
///
/// Blocks while the model decodes the audio, which is milliseconds for a chunk of a few
/// hundred milliseconds.
///
/// # Safety
///
/// `session` must be a live handle from `arioso_stt_open`, and `pcm` must point at `samples`
/// readable `int16_t` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_stt_push(
    session: *mut AriosoSttSession,
    pcm: *const i16,
    samples: usize,
    sample_rate: u32,
) -> i32 {
    guard(|| {
        if session.is_null() {
            return Err(fail(
                ARIOSO_ERR_NULL_ARGUMENT,
                "arioso_stt_push: session is null",
            ));
        }
        check_rate(sample_rate)?;
        if samples == 0 {
            return Ok(());
        }
        if pcm.is_null() {
            return Err(fail(
                ARIOSO_ERR_NULL_ARGUMENT,
                "arioso_stt_push: pcm is null with samples > 0",
            ));
        }
        // Safety: the caller guarantees a live handle, and the lock is released before this
        // call returns.
        let mut guard = unsafe { &*session }.inner.lock().map_err(|_| {
            fail(
                ARIOSO_ERR_ENGINE,
                "arioso_stt_push: the session lock is poisoned",
            )
        })?;
        let SessionState {
            session,
            scratch,
            last,
        } = &mut *guard;
        let Some(model) = session.as_mut() else {
            return Err(fail(
                ARIOSO_ERR_INVALID_ARGUMENT,
                "arioso_stt_push: the session has already been finished",
            ));
        };
        scratch.clear();
        // Safety: the caller guarantees `samples` readable values.
        let audio = unsafe { std::slice::from_raw_parts(pcm, samples) };
        // `i16::MIN` is one step further from zero than `i16::MAX`, so dividing by the
        // maximum lands at -1.00003; the clamp keeps the range the model was trained on.
        scratch.extend(
            audio
                .iter()
                .map(|&sample| (f32::from(sample) / f32::from(i16::MAX)).max(-1.0)),
        );
        if let Some(partial) = model.feed(scratch) {
            *last = partial;
        }
        Ok(())
    })
}

/// End the utterance and report its text.
///
/// The callback runs once, with `is_final` set, and its `text` is valid only for the call.
/// Returning nonzero cancels: this function answers `ARIOSO_CANCELLED` and the utterance is
/// thrown away. Either way the session is spent and must be released with
/// `arioso_stt_close`.
///
/// Blocks while the model decodes the tail of the utterance.
///
/// # Safety
///
/// `session` must be a live handle, `on_event` non-null, and `user` handed back to the
/// callback untouched and valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_stt_finish(
    session: *mut AriosoSttSession,
    on_event: AriosoEventCallback,
    user: *mut c_void,
) -> i32 {
    guard(|| {
        if session.is_null() {
            return Err(fail(
                ARIOSO_ERR_NULL_ARGUMENT,
                "arioso_stt_finish: session is null",
            ));
        }
        let Some(callback) = on_event else {
            return Err(fail(
                ARIOSO_ERR_NULL_ARGUMENT,
                "arioso_stt_finish: on_event is null",
            ));
        };
        // Safety: the caller guarantees a live handle; the lock is released before this call
        // returns.
        let mut held = unsafe { &*session }.inner.lock().map_err(|_| {
            fail(
                ARIOSO_ERR_ENGINE,
                "arioso_stt_finish: the session lock is poisoned",
            )
        })?;
        let Some(model) = held.session.take() else {
            return Err(fail(
                ARIOSO_ERR_INVALID_ARGUMENT,
                "arioso_stt_finish: the session has already been finished",
            ));
        };
        let text = model.finish();
        let text = if text.is_empty() {
            held.last.clone()
        } else {
            text
        };
        drop(held);

        let text = std::ffi::CString::new(text.replace('\0', " ")).unwrap_or_default();
        let event = AriosoSttEvent {
            struct_size: std::mem::size_of::<AriosoSttEvent>(),
            text: text.as_ptr(),
            is_final: 1,
        };
        // Safety: `event` outlives the callback and `user` is the caller's own pointer.
        match catch_unwind(AssertUnwindSafe(|| unsafe {
            callback(&raw const event, user)
        })) {
            Ok(0) => Ok(()),
            Ok(_) => Err(ARIOSO_CANCELLED),
            Err(payload) => Err(fail(
                ARIOSO_ERR_PANIC,
                error::panic_message(payload.as_ref()),
            )),
        }
    })
}

/// Release a session.
///
/// An unfinished utterance is thrown away: the next one starts from silence, not from the
/// words before it. Passing null is a no-op, and closing a session that was already finished
/// is the ordinary path, so both are quiet.
///
/// Does not block.
///
/// # Safety
///
/// `session` must be null, or a handle from `arioso_stt_open` that has not been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arioso_stt_close(session: *mut AriosoSttSession) {
    let _ = guard(|| {
        if session.is_null() {
            return Ok(());
        }
        // Safety: the caller guarantees the handle came from `arioso_stt_open` and is closed
        // once, so this is the only owner.
        drop(unsafe { Box::from_raw(session) });
        Ok(())
    });
}

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

impl Default for AriosoTtsOptions {
    fn default() -> Self {
        Self {
            struct_size: std::mem::size_of::<Self>(),
            speed: 0.0,
            speaker: 0,
        }
    }
}

impl Default for AriosoSttOptions {
    fn default() -> Self {
        Self {
            struct_size: std::mem::size_of::<Self>(),
            sample_rate: STREAM_RATE,
        }
    }
}

/// Borrow a NUL-terminated string as UTF-8.
///
/// # Safety
///
/// `pointer` must be null, or point at a NUL-terminated string valid for the call.
unsafe fn c_str<'a>(pointer: *const c_char, what: &str) -> Result<&'a str, i32> {
    if pointer.is_null() {
        return Err(fail(ARIOSO_ERR_NULL_ARGUMENT, format!("{what} is null")));
    }
    // Safety: the caller's contract is a NUL-terminated string that lives for the call.
    let bytes = unsafe { CStr::from_ptr(pointer) }.to_bytes();
    std::str::from_utf8(bytes)
        .map_err(|_| fail(ARIOSO_ERR_INVALID_ARGUMENT, format!("{what} is not UTF-8")))
}

/// The engine behind a handle.
///
/// # Safety
///
/// `engine` must be null, or a live handle from `arioso_engine_new`.
unsafe fn state_of<'a>(engine: *mut AriosoEngine, what: &str) -> Result<&'a AppState, i32> {
    if engine.is_null() {
        return Err(fail(
            ARIOSO_ERR_NULL_ARGUMENT,
            format!("{what}: engine is null"),
        ));
    }
    // Safety: the caller guarantees a live handle.
    Ok(unsafe { &*engine }.state.as_ref())
}

/// Read a caller's options struct, refusing a size this build cannot honour.
///
/// `struct_size` is the first field of every options struct (F10.6), so it is readable
/// whatever else the caller's version holds. Only the bytes the caller declared are copied
/// into `default`, which is what lets an older, shorter struct be read without touching the
/// bytes past its end; the fields past the declared size keep the default.
///
/// # Safety
///
/// `pointer` must be null, or point at `struct_size` readable bytes whose first
/// `size_of::<usize>()` bytes are that size.
unsafe fn read_options<T: AbiOptions>(pointer: *const T, default: T, what: &str) -> Result<T, i32> {
    if pointer.is_null() {
        return Ok(default);
    }
    let mut value = default;
    // Safety: the caller guarantees the size field is readable, and it is the first field.
    // Read unaligned because a C caller may hand over a packed struct: the size field is a
    // `usize`, and nothing in the ABI promises the pointer is aligned for one.
    let declared = unsafe { pointer.cast::<usize>().read_unaligned() };
    let known = std::mem::size_of::<T>();
    if declared > known {
        return Err(fail(
            ARIOSO_ERR_STRUCT_SIZE,
            format!("{what}: struct_size {declared} is larger than this library's {known}"),
        ));
    }
    // A size that stops in the middle of a field would copy part of an `f32` or an `i64`
    // over the default and render audio nobody asked for, so only whole-field prefixes are
    // accepted. `VALID_SIZES` is where each type says which those are.
    if !T::VALID_SIZES.contains(&declared) {
        return Err(fail(
            ARIOSO_ERR_STRUCT_SIZE,
            format!(
                "{what}: struct_size {declared} does not end on a field boundary; this \
                 library reads {:?}",
                T::VALID_SIZES
            ),
        ));
    }
    // Safety: the caller declared `declared` bytes, they fit inside `value`, and the two
    // cannot overlap because one is the caller's struct and the other is a local.
    unsafe {
        std::ptr::copy_nonoverlapping(
            pointer.cast::<u8>(),
            std::ptr::from_mut(&mut value).cast::<u8>(),
            declared,
        );
    }
    Ok(value)
}

/// Refuse text with nothing in it.
///
/// The engine answers "nothing to say" for it anyway, but a caller reading that sentence
/// cannot tell an empty request from a voice that could not pronounce a single word.
fn check_text(state: &AppState, text: &str) -> Result<(), i32> {
    if text.trim().is_empty() {
        return Err(fail(ARIOSO_ERR_INVALID_ARGUMENT, "the text is empty"));
    }
    // The same limit the HTTP route applies, because a C caller can hand over a document
    // and this function buffers all of it: an unbounded one is an allocation the caller
    // did not ask for and cannot see.
    let max_text_chars = state.config().server.limits.max_text_chars;
    if text.chars().count() > max_text_chars {
        return Err(fail(
            ARIOSO_ERR_INVALID_ARGUMENT,
            format!("text is longer than {max_text_chars} characters"),
        ));
    }
    Ok(())
}

/// The speaker to render with, refused rather than replaced.
///
/// A voice that has no speaker `n` must not answer with speaker `0`: the caller would have
/// no way to notice it got a different voice than it asked for (PRD F3.10).
fn tts_sid(state: &AppState, speaker: i64) -> Result<i64, i32> {
    if !(0..=4095).contains(&speaker) {
        return Err(fail(
            ARIOSO_ERR_INVALID_ARGUMENT,
            format!("speaker {speaker} is outside 0 to 4095"),
        ));
    }
    match state.check_sid(speaker) {
        Ok(()) => Ok(speaker),
        // The voice loaded and has no such speaker: the caller named something that does not
        // exist, which is an argument error.
        Err(SidProblem::NoSuchSpeaker(detail)) => Err(fail(ARIOSO_ERR_INVALID_ARGUMENT, detail)),
        // The voice would not load at all, so the speaker is not the problem: the render
        // fails for the same reason and the caller should see that instead.
        Err(SidProblem::Unavailable(detail)) => Err(fail(ARIOSO_ERR_ENGINE, detail)),
    }
}

/// The rate a session accepts, refusing one the model cannot take.
///
/// The same sentence the streaming socket sends, because it is the same rule.
fn check_rate(rate: u32) -> Result<u32, i32> {
    let rate = if rate == 0 { STREAM_RATE } else { rate };
    if rate != STREAM_RATE {
        return Err(fail(
            ARIOSO_ERR_INVALID_ARGUMENT,
            format!("send {STREAM_RATE} Hz audio, not {rate} Hz"),
        ));
    }
    Ok(rate)
}

/// The speed to render at: the caller's, or the config's when the caller passed zero.
fn speed_of(state: &AppState, speed: f32) -> Result<f32, i32> {
    let speed = if speed == 0.0 {
        state.config().tts.speed
    } else {
        speed
    };
    if !(0.5..=2.0).contains(&speed) {
        return Err(fail(
            ARIOSO_ERR_INVALID_ARGUMENT,
            format!("speed {speed} is outside 0.5 to 2.0"),
        ));
    }
    Ok(speed)
}

/// Hand one chunk to the caller's callback, turning its answer into a code.
///
/// The panic guard sits here rather than at the entry point because this is the only place a
/// panic can originate inside the engine: the callback is the caller's code. Catching it here
/// means the unwind never travels through the engine's own frames.
fn invoke_audio(
    callback: unsafe extern "C-unwind" fn(*const i16, usize, u32, *mut c_void) -> i32,
    samples: &[i16],
    rate: u32,
    user: *mut c_void,
) -> Result<(), i32> {
    let pointer = samples.as_ptr();
    let count = samples.len();
    // Safety: `pointer` and `count` describe a slice alive for the call, and `user` is the
    // caller's own pointer, handed back untouched.
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        callback(pointer, count, rate, user)
    })) {
        Ok(0) => Ok(()),
        Ok(_) => Err(ARIOSO_CANCELLED),
        Err(payload) => Err(fail(
            ARIOSO_ERR_PANIC,
            error::panic_message(payload.as_ref()),
        )),
    }
}
