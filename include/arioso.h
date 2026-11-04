/*
 * arioso: local speech, in process.
 *
 * This header is the C surface of the arioso engine. It synthesizes text with a Piper or
 * Kokoro voice and transcribes audio with a sherpa-onnx model, both inside the caller's
 * process: no server, no socket, no child process.
 *
 * Using it
 * --------
 *
 *   AriosoEngine *engine = NULL;
 *   int32_t rc = arioso_engine_new(config_json, &engine);
 *   if (rc != ARIOSO_OK) {
 *       fprintf(stderr, "arioso: %s\n", arioso_last_error());
 *       return 1;
 *   }
 *
 *   AriosoBuffer wav = {0};
 *   rc = arioso_tts_synthesize(engine, "Hello from arioso.", NULL, &wav);
 *   if (rc == ARIOSO_OK) {
 *       fwrite(wav.data, 1, wav.len, stdout);   // a complete WAV file
 *       arioso_free(wav.data);
 *   }
 *
 *   arioso_engine_free(engine);
 *
 * Return codes
 * ------------
 *
 * Every function that can fail returns int32_t. Zero is success. A positive code is not an
 * error: ARIOSO_CANCELLED (1) means a callback of yours asked to stop, and nothing was
 * wrong. Negative codes are errors, and each one has a sentence in arioso_last_error():
 *
 *   ARIOSO_OK                    0   success
 *   ARIOSO_CANCELLED             1   your callback returned nonzero; the call stopped
 *   ARIOSO_ERR_NULL_ARGUMENT    -1   a required pointer was null
 *   ARIOSO_ERR_INVALID_ARGUMENT -2   an argument was unusable; the sentence says which
 *   ARIOSO_ERR_CONFIG           -3   the config was refused; the sentence names the field
 *   ARIOSO_ERR_ENGINE           -4   the engine could not load, or could not do the work
 *   ARIOSO_ERR_STRUCT_SIZE      -5   a struct_size this library does not understand
 *   ARIOSO_ERR_PANIC            -6   a panic was caught at the boundary, not propagated
 *
 * The sentence
 * ------------
 *
 * arioso_last_error() answers a NUL-terminated UTF-8 string for the calling thread. It is
 * never null, it is empty after a successful or cancelled call, and it is valid only until
 * the next call on that thread. You do not free it, and you do not keep it: copy the bytes
 * if you want them later.
 *
 * Ownership
 * ---------
 *
 * Nothing this library allocates is freed with free(3), delete, or any allocator of yours.
 * Three releases exist, and each names what it releases:
 *
 *   arioso_free(buffer.data)    the bytes of an AriosoBuffer, and nothing else
 *   arioso_engine_free(engine)  the handle from arioso_engine_new
 *   arioso_stt_close(session)   the handle from arioso_stt_open, finished or not
 *
 * Pointers the library hands to a callback (audio samples, event text) belong to the
 * library and are valid only for the duration of that callback. Passing null to a release is
 * a no-op. Passing a pointer twice, or a pointer that did not come from this library, is
 * undefined behaviour.
 *
 * Blocking
 * --------
 *
 * No call is asynchronous, and none takes a completion callback except where the signature
 * says so. Which ones block, and for how long:
 *
 *   arioso_abi_version       no
 *   arioso_last_error        no
 *   arioso_engine_new        reads two files from the models directory; milliseconds
 *   arioso_engine_free       no; frees memory, does no I/O
 *   arioso_tts_synthesize    the whole synthesis, seconds for a long string; the first call
 *                            in a process also loads the voice (tens to hundreds of MB)
 *   arioso_tts_stream        the whole synthesis, on the calling thread, which is the point:
 *                            your callback gets each chunk as it is made
 *   arioso_stt_open          the model load, once per process
 *   arioso_stt_push          the decode of the audio pushed, milliseconds
 *   arioso_stt_finish        the decode of the tail of the utterance
 *   arioso_stt_close         no
 *   arioso_free              no
 *
 * Threads
 * -------
 *
 * An engine handle and a session handle may be used from any thread, and a call in progress
 * is not interrupted by another thread's call. The error string is per thread. Two threads
 * synthesizing through one engine take turns: the voice is one loaded model, not a pool.
 *
 * struct_size
 * -----------
 *
 * Every options struct starts with struct_size, which you set to sizeof() of the struct your
 * header declares. A struct larger than this library knows is refused with
 * ARIOSO_ERR_STRUCT_SIZE instead of being read past its end. Fields are only ever appended,
 * so a shorter, older struct keeps the documented defaults for the fields it does not have.
 * A version mismatch is a link error, not a runtime one: arioso_abi_version() is 1 for the
 * functions in this header.
 *
 * The obligation
 * --------------
 *
 * Each function below ends with a Safety paragraph. C cannot enforce it, so it is written
 * down: that paragraph is what you must guarantee before you call. The three that matter
 * most are a live handle, a NUL-terminated string, and an output pointer with room for the
 * struct the signature names.
 */

#ifndef ARIOSO_ABI_H
#define ARIOSO_ABI_H

#pragma once

/*
 * Generated by cbindgen from crates/arioso-abi. Do not edit this file by hand.
 * Regenerate with abi/generate-header.sh; CI fails when it disagrees with the crate.
 */

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

// The version of this surface. It changes when a function is added, removed, or given a
// meaning a caller could not have known.
#define ARIOSO_ABI_VERSION 1

// Success.
#define ARIOSO_OK 0

// The caller's callback returned nonzero, so the operation stopped. This is not an error:
// the caller asked for it, and `arioso_last_error` stays empty.
#define ARIOSO_CANCELLED 1

// A required pointer was null. Nothing was read, nothing was allocated.
#define ARIOSO_ERR_NULL_ARGUMENT -1

// An argument was present but unusable: text that is not UTF-8 or is empty, a speed outside
// the range the engine accepts, or a sample rate the streaming model cannot take. The
// sentence names the argument and what it should have been.
#define ARIOSO_ERR_INVALID_ARGUMENT -2

// The config document was refused: malformed JSON, or a value a rule rejects. The sentence
// names the field, because the caller is the one who can fix it.
#define ARIOSO_ERR_CONFIG -3

// The engine could not do the work: a manifest or dictionary that is missing, a voice that
// failed to load, a text with nothing the voice can pronounce.
#define ARIOSO_ERR_ENGINE -4

// A `struct_size` this library cannot honour. Either it is larger than the struct this
// build knows — pass the size of the struct your header declares — or it is too small to
// hold the size field itself.
#define ARIOSO_ERR_STRUCT_SIZE -5

// A panic was caught at the boundary. The sentence is the panic's own message. The engine
// is still usable: the panic is reported, not propagated.
#define ARIOSO_ERR_PANIC -6

// An engine: one config, one voice slot, one transcription slot.
//
// Opaque to C. Create it with `arioso_engine_new` and release it with `arioso_engine_free`;
// no other function frees it. A handle may be used from several threads: the engine
// serializes what it must internally, and a call that is running is not interrupted by
// another thread's call.
typedef struct AriosoEngine AriosoEngine;

// One streaming transcription utterance.
//
// Opaque to C. Create it with `arioso_stt_open`, feed it with `arioso_stt_push`, close it
// with `arioso_stt_finish` or `arioso_stt_close`. A session hears one utterance: after
// `arioso_stt_finish` the handle is spent and must be closed.
typedef struct AriosoSttSession AriosoSttSession;

// How to synthesize.
//
// `struct_size` must be `sizeof(AriosoTtsOptions)` from the header you compiled against.
// A size larger than this library knows is refused with `ARIOSO_ERR_STRUCT_SIZE`; a smaller
// one is read only up to the bytes you declared, and the fields past them keep the defaults
// described here. Fields are only ever appended.
typedef struct AriosoTtsOptions {
    // `sizeof(AriosoTtsOptions)`, as your header declares it.
    size_t struct_size;
    // Speaking rate. `0.0` means the config's `tts.speed`; anything else must be within
    // 0.5 to 2.0, the same range the HTTP route accepts.
    float speed;
    // Speaker index. `0` is the only speaker most voices have; a multi-speaker Piper voice
    // takes an index the voice reports.
    int64_t speaker;
} AriosoTtsOptions;

// A buffer this library allocated.
//
// `data` is `len` bytes and belongs to the library until the caller passes it to
// `arioso_free`; freeing it with `free(3)` or `delete` is undefined behaviour. The bytes are
// a complete WAV file: 44-byte header, mono, 16-bit little-endian PCM.
typedef struct AriosoBuffer {
    // WAV bytes, owned by the library. Release with `arioso_free(buffer.data)`.
    uint8_t *data;
    // Length of `data` in bytes.
    size_t len;
} AriosoBuffer;

// Called with audio as it is synthesized.
//
// `pcm` is `samples` 16-bit little-endian mono samples at `sample_rate`, valid for the call
// only. Return `0` to keep going, or any nonzero value to cancel: synthesis stops at the
// next chunk boundary and the call answers `ARIOSO_CANCELLED`.
//
// The callback runs on the thread that called `arioso_tts_stream`, inside that call.
typedef int32_t (*AriosoAudioCallback)(const int16_t *pcm,
                                       size_t samples,
                                       uint32_t sample_rate,
                                       void *user);

// Called with download progress.
//
// `received` and `total` are bytes, and `total` is the size of the whole model rather than
// of the file being fetched. Return `0` to keep going, or any nonzero value to stop: the
// fetch is dropped at its next step, the partial files stay on disk, and a later call
// resumes them. The callback runs on the thread that called `arioso_model_download`.
typedef int32_t (*AriosoProgressCallback)(uint64_t received, uint64_t total, void *user);

// How to transcribe.
//
// `struct_size` follows the same rule as `AriosoTtsOptions`.
typedef struct AriosoSttOptions {
    // `sizeof(AriosoSttOptions)`, as your header declares it.
    size_t struct_size;
    // Sample rate of the PCM you will push. The streaming models in the catalog take
    // 16000 Hz; any other rate is refused rather than silently resampled. `0` means 16000.
    uint32_t sample_rate;
} AriosoSttOptions;

// One thing the recognizer has to say.
//
// Filled by the library for the duration of the callback: `text` points into memory the
// library owns and is valid only until the callback returns. Copy it if you want to keep it.
typedef struct AriosoSttEvent {
    // `sizeof(AriosoSttEvent)` for the struct the library filled. Do not set it.
    size_t struct_size;
    // The utterance's text, NUL-terminated UTF-8, valid for the call only.
    const char *text;
    // `1` when the utterance ended and the text is final. v1 delivers no other event.
    int32_t is_final;
} AriosoSttEvent;

// Called with the result of a transcription.
//
// Return `0` to accept it, or any nonzero value to cancel: the call answers
// `ARIOSO_CANCELLED` and the session is closed.
//
// The callback runs on the thread that called `arioso_stt_finish`, inside that call.
typedef int32_t (*AriosoEventCallback)(const struct AriosoSttEvent *event, void *user);

// The ABI version this library implements: `1` for the functions in this header.
//
// Does not block.
uint32_t arioso_abi_version(void);

// The last failure on this thread, as a NUL-terminated UTF-8 sentence.
//
// Never null, and never freed by the caller: the string lives in thread-local storage and
// is valid until the next call on the same thread. It is empty after a call that returned
// `ARIOSO_OK` or `ARIOSO_CANCELLED`.
//
// Does not block.
const char *arioso_last_error(void);

// Release memory this library allocated.
//
// The only argument it accepts is a pointer this library returned, which today means
// `AriosoBuffer.data` from `arioso_tts_synthesize`. Passing null is a no-op; passing
// anything else, or the same pointer twice, is undefined behaviour.
//
// Does not block.
//
// # Safety
//
// `pointer` must be null, or a pointer this library returned as `AriosoBuffer.data` that has
// not been released yet.
void arioso_free(void *pointer);

// Create an engine from a JSON config document.
//
// The document is the one the server and the CLI read. Two files must be in the config's
// models directory: `manifest.json`, the model catalog, and `cmudict.dict`, the
// pronunciation dictionary. The voice and the speech model load on first use, so a caller
// that only transcribes never pays for a voice.
//
// On success `*out` holds a handle to release with `arioso_engine_free`. On any failure
// `*out` is null.
//
// Blocks: reads two files from the models directory. Milliseconds, not seconds.
//
// # Safety
//
// `config_json` must be a NUL-terminated string valid for the call, and `out` must be
// writable for one `AriosoEngine *`.
int32_t arioso_engine_new(const char *config_json, struct AriosoEngine **out);

// Release an engine and everything it loaded.
//
// Passing null is a no-op. A handle is released once; using it afterwards is undefined
// behaviour. Blocks: no. Dropping the engine frees the voice and the model but performs no
// I/O.
//
// # Safety
//
// `engine` must be null, or a handle from `arioso_engine_new` that has not been freed.
void arioso_engine_free(struct AriosoEngine *engine);

// Synthesize text into a WAV file in memory.
//
// `*out` receives the bytes, owned by the library until `arioso_free(buffer.data)`. On any
// failure `*out` is `{ NULL, 0 }`.
//
// The first call loads the voice and blocks for it; a Piper voice is tens of megabytes and a
// Kokoro graph is a few hundred. Every call then blocks for the whole synthesis: there is no
// callback to cancel, so this is the shape to use for a short string, not for a document.
//
// # Safety
//
// `engine` must be a live handle from `arioso_engine_new`, `text` a NUL-terminated string
// valid for the call, `options` null or a valid `AriosoTtsOptions`, and `out` writable for
// one `AriosoBuffer`.
int32_t arioso_tts_synthesize(struct AriosoEngine *engine,
                              const char *text,
                              const struct AriosoTtsOptions *options,
                              struct AriosoBuffer *out);

// Synthesize text, handing each chunk of audio to `on_audio` as it is produced.
//
// The callback runs inside this call, on the calling thread, once per synthesized chunk. A
// nonzero return cancels: synthesis stops within one chunk and this function answers
// `ARIOSO_CANCELLED` with no error sentence.
//
// Blocks for the whole synthesis, which is the point: a caller that wants audio early gets
// it early instead of at the end.
//
// # Safety
//
// `engine` must be a live handle, `text` a NUL-terminated string valid for the call,
// `options` null or valid, and `on_audio` non-null. `user` is handed back to the callback
// untouched and must stay valid for the call.
int32_t arioso_tts_stream(struct AriosoEngine *engine,
                          const char *text,
                          const struct AriosoTtsOptions *options,
                          AriosoAudioCallback on_audio,
                          void *user);

// The catalog as one JSON document.
//
// An array of objects: `id`, `kind`, `engine`, `license`, `commercial_use`, `installed`,
// `state`, `bytes`, `speakers`, `languages`. `bytes` is the whole model, and `state` is
// `installed`, `downloading`, or `not_installed`.
//
// `*out` receives the bytes, owned by the library until `arioso_free(buffer.data)`, and the
// document is not NUL-terminated: `len` is its length. On any failure `*out` is `{ NULL, 0 }`.
//
// # Safety
//
// `engine` must be a live handle from `arioso_engine_new` and `out` writable for one
// `AriosoBuffer`.
int32_t arioso_models_json(struct AriosoEngine *engine, struct AriosoBuffer *out);

// Fetch one model and wait for it, reporting progress to `on_progress`.
//
// A model already on disk answers `ARIOSO_OK` without touching the network. `on_progress`
// may be null, which reports nothing.
//
// Blocks for the whole download, which is minutes on a slow link. A callback that returns
// nonzero stops the fetch and this answers `ARIOSO_CANCELLED` with no error sentence.
//
// # Safety
//
// `engine` must be a live handle, `id` a NUL-terminated string valid for the call, and
// `user` handed back to the callback untouched, so it must stay valid for the call.
int32_t arioso_model_download(struct AriosoEngine *engine,
                              const char *id,
                              AriosoProgressCallback on_progress,
                              void *user);

// The phonemes a text will be spoken as, as the loaded voice builds them.
//
// The answer is IPA in UTF-8 and is not NUL-terminated: `len` is its length. It is what the
// same text would be synthesized from, so a caller can show it and then edit it.
//
// The first call loads the voice and blocks for it.
//
// # Safety
//
// `engine` must be a live handle, `text` a NUL-terminated string valid for the call, and
// `out` writable for one `AriosoBuffer`.
int32_t arioso_phonemes(struct AriosoEngine *engine,
                        const char *text,
                        struct AriosoBuffer *out);

// Synthesize phonemes a caller supplied into a WAV file in memory.
//
// The phonemes are spoken as written. A character that is not a phoneme, or a symbol this
// voice has no id for, is refused with the symbol named: a caller that asked for a sound and
// heard a different word has no way to notice.
//
// # Safety
//
// `engine` must be a live handle, `phonemes` a NUL-terminated string valid for the call,
// `options` null or a valid `AriosoTtsOptions`, and `out` writable for one `AriosoBuffer`.
int32_t arioso_tts_phonemes(struct AriosoEngine *engine,
                            const char *phonemes,
                            const struct AriosoTtsOptions *options,
                            struct AriosoBuffer *out);

// Synthesize phonemes, handing each chunk of audio to `on_audio` as it is produced.
//
// The same contract as [`arioso_tts_stream`], from phonemes instead of text.
//
// # Safety
//
// `engine` must be a live handle, `phonemes` a NUL-terminated string valid for the call,
// `options` null or valid, and `on_audio` non-null. `user` is handed back to the callback
// untouched and must stay valid for the call.
int32_t arioso_tts_phonemes_stream(struct AriosoEngine *engine,
                                   const char *phonemes,
                                   const struct AriosoTtsOptions *options,
                                   AriosoAudioCallback on_audio,
                                   void *user);

// Open a streaming transcription session.
//
// The streaming model named by `stt.stream_model` must be configured and installed; a config
// without one refuses here rather than returning a session that cannot transcribe. The first
// open in a process blocks for the model load.
//
// On success `*out` holds a session to release with `arioso_stt_finish` or
// `arioso_stt_close`. On any failure `*out` is null.
//
// # Safety
//
// `engine` must be a live handle, `options` null or a valid `AriosoSttOptions`, and `out`
// writable for one `AriosoSttSession *`.
int32_t arioso_stt_open(struct AriosoEngine *engine,
                        const struct AriosoSttOptions *options,
                        struct AriosoSttSession **out);

// Feed 16-bit PCM to a session.
//
// `pcm` is `samples` mono samples at `sample_rate`, which must be 16000: the streaming
// models in the catalog take that rate, and a different one is refused rather than
// resampled behind the caller's back. Pushing nothing is not an error.
//
// Partials the model forms between pushes are not reported; v1 delivers one event, at
// `arioso_stt_finish`.
//
// Blocks while the model decodes the audio, which is milliseconds for a chunk of a few
// hundred milliseconds.
//
// # Safety
//
// `session` must be a live handle from `arioso_stt_open`, and `pcm` must point at `samples`
// readable `int16_t` values.
int32_t arioso_stt_push(struct AriosoSttSession *session,
                        const int16_t *pcm,
                        size_t samples,
                        uint32_t sample_rate);

// End the utterance and report its text.
//
// The callback runs once, with `is_final` set, and its `text` is valid only for the call.
// Returning nonzero cancels: this function answers `ARIOSO_CANCELLED` and the utterance is
// thrown away. Either way the session is spent and must be released with
// `arioso_stt_close`.
//
// Blocks while the model decodes the tail of the utterance.
//
// # Safety
//
// `session` must be a live handle, `on_event` non-null, and `user` handed back to the
// callback untouched and valid for the call.
int32_t arioso_stt_finish(struct AriosoSttSession *session,
                          AriosoEventCallback on_event,
                          void *user);

// Release a session.
//
// An unfinished utterance is thrown away: the next one starts from silence, not from the
// words before it. Passing null is a no-op, and closing a session that was already finished
// is the ordinary path, so both are quiet.
//
// Does not block.
//
// # Safety
//
// `session` must be null, or a handle from `arioso_stt_open` that has not been closed.
void arioso_stt_close(struct AriosoSttSession *session);

#endif  /* ARIOSO_ABI_H */

/*
 * Layout of the buffers this library returns.
 *
 * AriosoBuffer.data is a complete WAV file: 44-byte header, mono, 16-bit little-endian PCM,
 * at the voice's own rate (22050 Hz for a Piper voice, 24000 Hz for Kokoro). Write it to a
 * file, or read the header to find the rate and the sample count.
 *
 * Audio handed to AriosoAudioCallback is the same samples without the header: `samples`
 * int16_t values, mono, at `sample_rate`. The next chunk follows immediately, so a player
 * that queues them in order gets continuous speech.
 */
