# The C ABI and manifest signing

Two features that the rest of the product already depends on and does not have. The C ABI
is what a sandboxed Mac App Store build needs, because a sandboxed app may not spawn a child
that listens on a socket (F25.3). Signing is what makes a mirror hostile instead of merely
wrong: per-file SHA-256 stops corruption, and only a signature stops a swapped manifest.

Nothing here changes an existing route or an existing behaviour. Both are additions, and
both are proven by a test that fails without them.

## Part one: the manifest signature

### What exists

`arioso_models::manifest` already carries the whole verification path:

- `RELEASE_PUBKEY_HEX`, a compile-time constant, empty in this repository today.
- `signature_path(manifest)` pairs `manifest.json` with `manifest.json.sig`.
- `verify_manifest(bytes, signature_hex, pubkey_hex)` checks a detached Ed25519 signature
  with `verify_strict`.
- `Manifest::load_checked` reports `SignatureState::Verified` or `Unsigned`, and returns
  `SignatureMissing` or `BadSignature` when a compiled key exists and the document does not
  prove itself.
- `arioso doctor` already prints the state it established.

What is missing is the other half: there is no way to make a signature. A release cannot be
signed, so the compiled key is empty, so nothing is ever verified.

### What to add

1. `arioso models keygen [--out PATH]` mints an Ed25519 keypair. It writes the private key to
   `PATH` with mode `0600`, refuses to overwrite an existing file, and prints the public key
   as lowercase hex on stdout, which is the exact string that goes into `RELEASE_PUBKEY_HEX`.
   The private key file is 32 raw bytes. It is never written anywhere else, never logged, and
   never printed.
2. `arioso models sign <manifest> --key PATH` writes `<manifest>.sig` as lowercase hex over
   the manifest's bytes exactly as they sit on disk. It prints the path it wrote and the
   public key the signature belongs to, so an operator can check that it matches the compiled
   one before shipping.
3. A test that signs a manifest with a generated key, verifies it, then tampers with one byte
   and is refused. A test that a signature from a different key is refused. A test that a
   missing `.sig` is refused when a key is compiled in, and reported as `Unsigned` when none
   is.
4. Documentation: how to mint a key, where the private half lives, and what rotation costs.

### Rules the code has to keep

- **The private key is not in the repository, not in the tree, and not in a test fixture that
  ships.** Tests generate a keypair in a temporary directory for the length of one test.
- **Fail closed.** A build with a compiled key refuses a manifest whose sibling signature is
  absent or does not verify, naming the signature file (F1.10). A build with no key reports
  `Unsigned` and says so in `doctor` and at startup, because silence there is how an unsigned
  release ships by accident.
- **The signature covers the bytes on disk, not a re-serialization.** Re-encoding the JSON
  before signing would let a whitespace change break verification for no security gain.
- **Rotation is a documented cost, not a silent one.** A new key invalidates every signature
  made with the old one. The old public key stays compiled into releases that already exist;
  a build carries exactly one key.

### The decision left to the operator

This plan does not mint a release key. Generating one here would put the private half on a
development machine and make this repository's releases depend on a key nobody chose. The
mechanism ships complete, the constant stays empty, and the first real release is the moment
to run `arioso models keygen`, store the file offline, and paste the public key in.

## Part two: the C ABI

### Why

The store build links the engine in-process. Without a C surface the app cannot reach the
engine at all, and F25.3 makes the C ABI the prerequisite for that build rather than a
convenience.

### Shape

A new crate `crates/arioso-abi`, crate types `cdylib`, `staticlib` and `rlib`, library name
`arioso`. The `rlib` is what makes the surface testable from Rust; the other two are what a C
caller links against. The header is generated with `cbindgen` into `include/arioso.h` and a CI
step regenerates it and fails on a diff, so the committed header cannot drift from the code.

```c
uint32_t arioso_abi_version(void);          /* 1 for this surface */
const char *arioso_last_error(void);        /* thread-local; valid until the next call */

int32_t arioso_engine_new(const char *config_json, AriosoEngine **out);
void    arioso_engine_free(AriosoEngine *engine);

int32_t arioso_tts_synthesize(AriosoEngine *engine, const char *text,
                              const AriosoTtsOptions *options, AriosoBuffer *out);
int32_t arioso_tts_stream(AriosoEngine *engine, const char *text,
                          const AriosoTtsOptions *options,
                          AriosoAudioCallback on_audio, void *user);

int32_t arioso_stt_open(AriosoEngine *engine, const AriosoSttOptions *options,
                        AriosoSttSession **out);
int32_t arioso_stt_push(AriosoSttSession *session, const int16_t *pcm,
                        size_t samples, uint32_t sample_rate);
int32_t arioso_stt_finish(AriosoSttSession *session,
                          AriosoEventCallback on_event, void *user);
void    arioso_stt_close(AriosoSttSession *session);

void    arioso_free(void *pointer);
```

### The rules that make it safe to call from C

- **Every struct starts with `struct_size`** (F10.6). A newer library reads an older caller's
  struct by size, and refuses a struct larger than it knows rather than reading past the end.
- **Nothing allocated by the library is freed by the caller's allocator.** `arioso_free` is
  the only way to release a buffer or a string, and the header says so on every pointer.
- **No panic crosses the boundary.** Every entry point is wrapped in `catch_unwind`, and a
  panic becomes `ARIOSO_ERR_PANIC` with the message in `arioso_last_error`. A panic unwinding
  into C is undefined behaviour, and this is the one place where the cost of the guard is
  worth it.
- **Errors are a code plus a sentence.** Negative codes are errors, `0` is success, and
  `arioso_last_error` returns a thread-local string that the caller does not free.
- **A callback that returns nonzero cancels** (F10.3). The engine stops within one chunk and
  returns `ARIOSO_CANCELLED`, which is not an error.
- **Handles are `Send` and `Sync` behind the scenes** (F10.4): the engine is an `Arc` over the
  same state the server uses, and the header documents which calls block.

### What this plan does not build

- **The download callback (F10.2).** Downloads are an operator action and already have an
  event stream over HTTP. A C callback over the downloader is a second progress surface for a
  caller that is, in the store case, an app with no model manager. Deferred, and named here so
  the omission is a decision.
- **The mobile CI jobs (F10.5).** `mobile.yml` already builds the library for iOS and Android;
  adding the C ABI to it is one line once the crate exists, and the smoke test needs a device
  or a simulator, which this machine does not have. The desktop C caller runs in CI; the
  mobile builds stay build-only and that limit is stated rather than implied.

### Proof

- A Rust integration test that drives the exported functions through the `rlib`: create an
  engine from a JSON config, read the error string for a bad config, synthesize through the
  real engine when a voice is present, free everything.
- A C program, `abi/smoke.c`, compiled with `cc` in CI, linked against the static library. It
  covers version, engine creation, a bad config producing a code and a sentence, and the free
  path. Synthesis in C needs a model, so it asserts the refusal is clean rather than pretending
  to speak.
- A test that a panic in a callback is caught and reported instead of unwinding into C.

## Order of work

1. Signing: the tools, the tests, the documentation. Small, self-contained, no new crate.
2. The C ABI: crate, header, Rust tests, C smoke test, CI wiring.
3. PRD and `AGENTS.md` in the same commits as the behaviour they describe.

## Risks

- **A key on a development machine is a key that leaks.** The mechanism never touches a key;
  the operator does, once, on the machine that holds the release.
- **`catch_unwind` is not free.** It costs a little on every call. The alternative is
  undefined behaviour in the caller's process, which is worse than a few nanoseconds.
- **A generated header that nobody reads.** The CI diff makes it correct, not clear; the
  header's comments are written for the C programmer who will use it.
