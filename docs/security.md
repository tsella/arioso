# Security notes

What this server exposes, what was tested, and what is still open. Written after a
deliberate pass for remote code execution, so the negative result is recorded with the
method that produced it rather than as a claim.

## What an attacker can reach

Every route takes input from the network:

| Route | Input | Reaches |
| --- | --- | --- |
| `POST /v1/tts` | text, query | phonemizer, ONNX voice |
| `POST /v1/stt` | up to 32 MiB of audio | WAV parser, Whisper |
| `GET /v1/tts/ws`, `GET /v1/stt/ws` | WebSocket frames | same engines, streaming |
| `PATCH /v1/config` | JSON document | config file write, engine reload |
| `GET /v1/*` | query parameters | manifest lookup |
| `/docs`, `/openapi.json` | none | the API document |

## Remote code execution: not found, and why

The search covered the four places an RCE normally lives in a service like this:

1. **Memory-unsafe parsing.** The server is Rust with no `unsafe` in this workspace's own
   crates. The parsers that take attacker bytes — WAV, JSON config, JSON dictionary, JSON
   manifest — were fuzzed for 17.7 million executions with no crash (section below).
2. **Archive extraction.** A tar or zip written to a path from the archive is the classic
   arbitrary-file-write. This workspace extracts nothing at runtime: the only extraction is
   in a third-party build script, which runs at build time, on the builder's own machine,
   against a fixed URL.
3. **Command execution.** No `std::process::Command`, no shell, no `eval`. The phonemizer
   is a linked library, not a subprocess.
4. **Deserialization gadgets.** `serde_json` into plain structs, no trait objects, no
   polymorphic tags, no dynamic code loading.

The honest limit of that result: the STT and TTS engines are C++ through FFI (sherpa-onnx,
onnxruntime). Memory bugs there are reachable in principle from crafted audio or text. The
fuzzing below covers the parsers this project wrote; it does not cover the model runtimes,
which would need a fuzz target linked against a real model and a longer budget. That is the
largest remaining unknown and it is stated as unknown, not as cleared.

## What was found and fixed

**Authentication was configured and never checked.** `server.auth` parsed, validated, and
did nothing: every route answered anyone who could reach the port. Fixed. When
`server.auth.mode` is `bearer`, the token is read from the environment named by
`server.auth.token_env`, every route except `/healthz` requires it, and a missing token in
the environment is a startup failure rather than a server that believes it is protected.

**A public bind was one typo away.** The default is loopback, but changing `server.bind` to
`0.0.0.0` silently exposed everything, because nothing checked. Fixed: validation refuses a
non-loopback bind unless a token is configured, and the error names the exact fix.

**Comparison was not the only issue.** The token check uses a constant-time comparison, so
a caller cannot learn the token one byte at a time from response timing.

## What is still open

These are known and not fixed. They are listed so nobody assumes they were.

- **Engine error detail reaches the client.** `SttFailure::Engine(detail)` is returned in
  the problem document. That detail comes from the runtime and may contain paths or library
  messages. It should be logged at `error` and replaced with a stable sentence.
- **Config write errors name the path.** `writing {path:?}: {error}` is useful in a log and
  unnecessary in a response.
- **No security headers.** `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`
  and a restrictive CSP for the Swagger UI are all absent.
- **The API document is served whenever the server runs.** Behind a token now, but a
  deployment may want `/docs` off entirely.
- **No connection cap.** `server.limits.max_connections` parses and does nothing; the accept
  loop belongs to `axum-server`. The header timeout and the rate limiter are the controls
  that exist.
- **Model files are trusted.** A manifest `name` containing `..` would join outside the
  models directory. The manifest ships with the binary, so this needs local write access to
  exploit, but the join should be checked.

## Fuzzing

Four targets, built with `cargo-fuzz` on nightly, seeded with a real document each:

| Target | Runs in 71 s | Crashes |
| --- | --- | --- |
| `config_json` | 3,257,821 | 0 |
| `dictionary` | 3,913,343 | 0 |
| `manifest` | 1,817,876 | 0 |
| `wav_header` | 8,711,967 | 0 |

Reproduce:

```console
$ cargo +nightly fuzz run wav_header -- -max_total_time=70
```

A crash writes an input to `fuzz/artifacts/<target>/`. All four artifact directories are
empty, which is the result recorded above.

Coverage-guided fuzzing finds panics and memory errors, not logic. A fuzzer cannot tell you
that a token is never checked; that was found by reading the code, and it is the more
serious of the two findings.

## Deployment checklist

1. Keep `server.bind` on `127.0.0.1` unless you mean to expose it.
2. If you expose it, set `server.auth.mode` to `bearer` and export the token. The server
   refuses to start otherwise.
3. Put TLS in front: either `server.tls` with a real certificate, or a reverse proxy.
4. Turn on `server.trust_forwarded_for` only behind a proxy that sets the header.
5. Expect the limits to be per process: a reverse proxy is what absorbs a flood.

## Verified against a running server

Bearer mode, with the token exported:

```console
no token  -> 401        www-authenticate: Bearer realm="arioso"
wrong     -> 401
right     -> 200
healthz   -> 200        (exempt: a probe must answer before the config is read)
openapi   -> 401        (the API document is behind the token too)
```

A public bind without a token refuses to start:

```console
$ arioso --config public.json serve
Error: invalid config at server.auth.mode: server.bind 0.0.0.0:10195 is not a loopback
address, so this server would answer anyone who can reach it. Set server.auth.mode to
"bearer" and put the token in ARIOSO_TOKEN. Bind 127.0.0.1 to keep it on this machine.
```
