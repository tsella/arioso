# arioso-speaks — a terminal front end, planned before it is built

A single-window terminal app that speaks text with a voice you choose, shows the phonemes
that will be spoken, lets you edit them, and saves the audio. It runs on macOS first and
keeps the other two platforms compiling.

## What exists that this leans on

- `arioso-server`'s `AppState` already owns the whole voice lifecycle: config, manifest,
  the loaded engine behind a lock, `render_stream`, `synthesize_wav`, `check_sid`, and the
  dictionary. A second front end that re-implemented any of that would drift from the
  routes within a week.
- `arioso-models` already has a resumable downloader with a progress callback, SHA-256
  verification and a host allowlist. The catalog already carries `size` per file, `license`
  and `commercial_use`.
- `arioso-g2p` already turns text into `Phonemes { symbols, guessed, missing }`, where
  `symbols` are IPA and include the space symbol between words.
- Both engines are phoneme-driven internally: `PiperVoice::synthesize(&[&str], sid)` and
  `KokoroVoice::synthesize(&[i64], speed)` after a vocabulary lookup.

## The three gaps

1. **No phoneme entry point.** `render` takes text and calls the text-to-phoneme rules
   inside. Nothing accepts phonemes that a person edited.
2. **No catalog answer in one place.** The route handlers each build their own JSON, so a
   second front end would copy them.
3. **No download call that reports progress to a caller.** The downloader has the callback;
   nothing above it exposes one.

Each gap is closed in `arioso-server` first, because three front ends now ask the same
questions: the HTTP routes, the C ABI, and this app.

## What the C ABI gains (v2, additive)

| Function | Answers |
| :--- | :--- |
| `arioso_models_json` | Every model: id, engine, kind, speakers, size, licence, commercial use, whether it is on disk |
| `arioso_model_download` | Downloads one model by id, calling a progress callback with bytes received and total |
| `arioso_phonemes` | The IPA a text will be spoken as |
| `arioso_tts_phonemes` | Synthesizes from phonemes a caller supplies, to WAV |
| `arioso_tts_phonemes_stream` | The same, one chunk at a time |

`arioso_abi_version()` stays `1`: nothing existing changes, and a library built before these
functions exists fails at link, which the header already documents as the intended failure.
`arioso_voices_json` is not added separately — a voice is a model of kind `tts`, and one
list that can be filtered is one list that cannot disagree with itself.

## The app

`crates/arioso-speaks`, one binary. It uses the crates directly; it does not load the C ABI.

- **Playback**: `rodio` with default features off, fed the f32 samples the engine returns.
  No decoder, no child process. A machine with no output device reports it, and the app
  then offers saving only.
- **Layout**, top to bottom:
  - the wordmark, with the rule under it replaced by a dithered waveform of the audio being
    played;
  - a left column of engine tabs and a filterable voice list — name, size, licence, with a
    marker when the licence is unknown or commercial use is not permitted;
  - a right column holding the text area above the phoneme area;
  - one action row: Say it, Save it, Reset phonemes, and the key hints.
- **Dither**: a 4×4 Bayer matrix chooses between five glyph densities per cell from the
  sample envelope. Density stands in for opacity, which is the one thing a terminal cannot
  do. It is the same matrix the progress bar uses, so the two read as one instrument.
- **Palette**: petrol `#1B5E7E`, tan `#8A6432`, plum `#6F4F8B`, moss `#3E6F4F` from the
  transcriber's own palette, mapped to 256 colours, with pastel tints for panels on the
  terminal's background.
- **Download**: when the chosen voice is absent, Say it becomes
  `Download <name> — <size> · <licence>`; pressing it swaps the action row for a full-width
  dithered bar with bytes, rate and the licence text, then speaks nothing and returns.

## Order of work

1. `arioso-server`: catalog JSON in one place, the phoneme entry points, the download call.
2. `arioso-abi`: the five functions, the regenerated header, C smoke checks.
3. `arioso-speaks`: audio, catalog, dither, layout, actions.
4. Gate, README, CI.

## Rules this must keep

- A speaker the voice does not have is refused, never replaced — the app shows the refusal.
- A licence is shown as the manifest states it, including `unknown`; the app never infers a
  licence from the model name.
- Edited phonemes are spoken exactly as written, or refused with the symbol that could not
  be spoken. Silently falling back to the generated line is the defect this app exists to
  avoid.
- The waveform is drawn from the audio that was rendered, not from a plausible curve.
