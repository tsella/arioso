# IMPLEMENTATION.md

You are building something and you want it to speak, or to listen. This file shows the
shortest path for each way of doing that, and the choices that keep it working.

## Pick the face that fits

| you have | use | why |
| --- | --- | --- |
| a Rust program | the library crates, in process | no port, no process, no protocol |
| a program in another language, or several | `arioso serve` and the HTTP API | one server, any client, a documented contract |
| a desktop app that ships to users | sidecar mode | the app owns the process lifetime and reads one line |

All three run offline on the same engines. A model on disk is the only requirement.

## Speak a line from a Rust program

```rust
use arioso_core::UserDictionary;
use arioso_g2p::CmuDict;
use arioso_tts_piper::{wav, PiperVoice, RenderOptions};

let mut voice = PiperVoice::load(&model_path, &config_path)?;
let dict = CmuDict::parse(&std::fs::read_to_string(&dict_path)?);
let user = UserDictionary::shipped();

let mut pcm: Vec<f32> = Vec::new();
voice.render(
    "Your order shipped at 7:45 pm.",
    &dict,
    &user,
    RenderOptions::default(),
    |samples, _rate| {
        pcm.extend_from_slice(samples);
        Ok(())
    },
)?;
wav::write_wav(&out_path, &pcm, voice.sample_rate())?;
```

The sink is called once per sentence. Play each call as it arrives and the first word is
heard before the last sentence is synthesized, which is the difference between a voice and
a pause.

Keep the loaded voice. `PiperVoice::load` reads tens of megabytes; do it once at start,
not per request. `RenderOptions.speed` is a multiplier, and `RenderOptions.normalize`
controls how numbers, times and money are spoken.

## Transcribe a recording

```rust
use arioso_stt_sherpa::Engine;

let (samples, rate) = arioso_tts_piper::wav::read(&clip)?;
let engine = Engine::load(&model_dir, Some("en"), 4)?;
let transcript = engine.transcribe(&samples, rate)?;
println!("{}", transcript.text);
```

The engine takes a directory, because a model is several files. Load it once: the engine
holds the weights, and a second load doubles the memory for no gain. A language hint raises
accuracy on short clips; leave it out and the model decides.

`Engine::load` reads a Whisper directory. A transducer (Parakeet, Zipformer) needs a joiner
as well, so it is loaded by name:

```rust
use arioso_stt_sherpa::Engine;

let engine = Engine::load_transducer(
    &model_dir, &encoder, &decoder, &joiner, &tokens, "nemo_transducer", 4,
)?;
```

A caller that holds the catalog entry does not have to decide: `Engine::from_manifest_files`
takes the entry's file list and its `meta.model_type`, and picks the right family. That is
what the server and the CLI do.

`transcribe` takes any sample rate and cuts long audio into 29 second windows itself, so one
call handles a 27 minute recording. The windows end in pauses where the audio has them.

## Give a program in another language a voice

```bash
arioso serve --config app.json                       # 127.0.0.1:10101

curl -sS -X POST http://127.0.0.1:10101/v1/tts \
  -H 'content-type: application/json' \
  -d '{"text":"The meeting moved to 3:30 pm."}' --output line.wav

curl -sS -X POST http://127.0.0.1:10101/v1/stt \
  -H 'content-type: audio/wav' --data-binary @clip.wav
```

Wait for `GET /readyz` before the first request, not only `/healthz`: readiness is the one
that says the voice is on disk and loadable. `POST /v1/models/{id}/warm` loads it early, so
the first user does not pay for it.

Treat the error codes as configuration advice, not as failures to retry:

- `404 unknown_model`: the id is not in the catalog. `GET /v1/models` lists the real ids.
- `409 model_not_installed`: download it, or point `models.dir` at a directory that has it.
- `409 download_in_progress`: wait, or read the catalog for progress.
- `503 too_many_sessions`: the streaming cap is reached. Raise `server.limits.max_connections`.

## Embed it in a desktop app

Set `server.sidecar` in the config the app ships, then:

```text
spawn:  arioso --config app.json serve
read:   START 41022          ← one line on stdout, nothing else
use:    http://127.0.0.1:41022
quit:   POST /v1/stop        ← graceful, drains in-flight work
        POST /v1/abort       ← immediate, drops what is in flight
```

Read exactly one line and use the port in it. The configured port may be taken; the child
then takes one the system picks and reports that one. A caller that assumes 10101 works on
your machine and fails on a user's.

On quit, prefer `POST /v1/stop` and wait for the process to exit. Use `/v1/abort` when the
window closes and the user is gone. If the process outlives the app, `arioso stop` reads
the PID file beside the config and ends it.

## Stream instead of waiting

Text to speech: upgrade `/v1/tts/ws`, send one JSON text message, then read binary PCM
frames, one per sentence.

```json
{"text": "Line one. Line two.", "speed": 1.0}
```

Speech to text: upgrade `/v1/stt/ws`, send a start frame, then 16 kHz mono `s16le` frames.

```text
client → {"type":"start","sample_rate":16000}
server → {"type":"ready","session":"6ae847c402bc","sources":["mic"]}
client → <binary PCM>
server → {"type":"partial","text":"The quick brown fox jump.","t":1.54}
server → {"type":"final","text":"The quick brown fox jumps.","start":0.84,"end":3.56}
client → {"type":"cancel"}
```

Streaming pays off when the text is long or the audio is live. For one short line, the
plain routes are simpler and just as fast.

## Working with long text and long audio

Four limits stand between you and an hour of speech. Three are yours to set.

| limit | default | bounds |
| --- | --- | --- |
| `server.limits.max_text_chars` | 4000 characters | one synthesis request, about 2.5 minutes of speech |
| `stt.max_audio_s` | 600 seconds | audio one transcription request accepts |
| request body | follows `stt.max_audio_s`, 16-bit mono at 48 kHz | the bytes on the wire |
| decode window | 29 seconds | internal to the engine, not settable |

The text limit exists because one request renders its whole text into memory before the
reply is written. 4000 characters is a compromise, not a law: raise it and each request
holds more memory for longer, which is the trade a single call makes. The streaming route
makes no such trade, because it sends a frame per sentence.

**Speak thirty minutes.** Send the text in pieces and join the audio. Cut at paragraph
boundaries, then at sentence ends, and never in the middle of a word:

```python
def chunks(text, limit=3800):          # under the route's 4000, room for the JSON
    out, current = [], ""
    for para in text.split("\n\n"):
        candidate = f"{current}\n\n{para}".strip() if current else para
        if len(candidate) <= limit:
            current = candidate
            continue
        out.append(current)
        current = para
    if current:
        out.append(current)
    return out
```

Every reply is a complete WAV at the voice's sample rate, so the pieces concatenate: read
each one, append its frames, write one header at the end. A small silence between pieces
reads better than a hard cut.

Measured on this machine, 5,787 words: ten requests of at most 3,800 characters produced
1,828 seconds of audio in 163 seconds of wall clock, a real time factor of 0.089. The
model loads once and stays loaded, so per-request cost is small.

**Transcribe thirty minutes.** Raise `stt.max_audio_s`, and the body limit follows it. Send
the whole file:

```console
$ curl -sS -X POST http://127.0.0.1:10194/v1/stt \
    -H 'content-type: audio/wav' --data-binary @meeting.wav
{"text":"…","seconds":1827.96}
```

The server cuts the audio into 29-second windows, decodes each, and joins the text. The
window is one second short of the engine's own limit on purpose: the engine accepts "less
than 30 seconds" and keeps only the first thirty of anything longer, without an error.
Cutting at 29 s makes that impossible.

Measured on the same file: 1,828 seconds of audio transcribed in 429 seconds, a real time
factor of 0.235, word error rate 8.96 percent against the source text with the default
`base.en` model. Most of that error is spelling (`traveller` for `traveler`, `colour` for
`color`) and small words at window edges, which is what the base tier does; a larger model
trades memory and time for accuracy.

## Make text say what you mean

```console
$ arioso normalize 'Order #4471 ships 3/14 for $19.99 at 19:45'
Order #four thousand four hundred seventy one ships three/fourteen for nineteen dollars
and ninety nine cents at seven forty five in the evening
```

`#` and `3/14` stay as written, because the normalizer does not guess whether a slash is a
date or a fraction. Everything else is spoken.

Names are the common failure. The dictionary is JSON, and an entry gives either spoken
words or phonemes:

```json
{"term": "arioso", "spoken": ["ah ree oh so"]}
{"term": "Sella", "phonemes": ["S EH1 L AA0"]}
```

A term may carry several spoken forms, tried in order. Point `tts.dictionary` at your own
file to add names without editing the shipped one.

Dictionary entries win over the letter-to-sound rules, and the rules win over a guess. If a
word is pronounced wrong, add it to the dictionary rather than editing the text.

The same file runs backwards on transcripts. An entry whose spoken form is the word itself
settles the spelling the transcriber chooses, which is how you get one variety of English
out of a model trained on both:

```json
{"term": "traveller", "spoken": ["traveller"]}
{"term": "grey", "spoken": ["grey"]}
```

Measured: without that file the same audio transcribed `time-traveler` and `gray eyes`; with
it, `time-traveller` and `grey eyes`. An entry with `spoken` works in both directions. An
entry with only `phonemes` works too: the reverse pass recognises ARPAbet and compares the
sounds directly, so a term the recognizer spells its own way still comes back.

## Test that the voice says the right thing

```bash
arioso say "Flight BA 249 boards at 7:45 pm" --out check.wav
arioso transcribe check.wav
```

Transcribe what you synthesized and assert the words. That test catches a normalization
change, a dictionary edit and a voice swap in one line, and it is cheap enough to run on
every commit.

## Choices that keep it working

- Pin the model id in your config and fetch it once. Models are tens to hundreds of
  megabytes; do not fetch at startup in front of a user.
- Never commit model files. Point `models.dir` at a cache directory.
- Keep the bind on loopback. A non-loopback bind needs a token, and the server refuses to
  start without one. Use sidecar mode rather than exposing a port.
- Load each engine once per process. Memory follows the model, not the request rate.
- Read the sample rate from the voice or the WAV header. Voices differ, and a wrong rate
  makes a correct transcript into noise.
- Text to speech is MIT. The `gpl-espeak` feature pulls in a GPL fallback phonemizer; leave
  it off unless your distribution can carry GPL.

## Where to look next

`README.md` for the whole surface. `EXAMPLES.md` for worked calls with real output.
`PRD.md` for what the product promises. `docs/security.md` for the exposure decisions.
`AGENTS.md` if you are changing this repository rather than using it.
