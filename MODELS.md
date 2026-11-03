# Models

This file is the evidence behind the catalog. Every model here was measured on this
project's corpora, on one machine, with the code in this repository. Where a model is not
shipped, the table says so and says why.

## How the numbers were produced

Two corpora, both English, both about thirty minutes of speech:

| corpus | source | words | audio | character |
| --- | --- | --- | --- | --- |
| novel | Project Gutenberg #35, *The Time Machine*, chapter I onward | 5,787 | 30.5 min | 1895 literary prose |
| podcast | Hacker Public Radio episode 4001, CC BY-SA | 5,761 | 27.4 min | 2023 conversation, several speakers, disfluencies kept |

Speech to text was measured by synthesizing the corpus with `en_US-joe-medium`, transcribing
the audio, and scoring the transcript against the source text three ways: plain, with digits
folded, and with British and American spellings folded. Only the plain number appears here;
the folded numbers are in `docs/plans/2026-11-02-long-transcription.md`.

Text to speech was measured the other way round: the same corpus through the voice, then
through one fixed recognizer, so the word error rate carries the recognizer's own error plus
whatever the voice added. That measures intelligibility. It does not measure naturalness,
and nothing in this file does.

Hardware for every row: four CPU cores, no GPU, int8 weights, four threads. Times are wall
clock, `RTF` is decode time divided by audio length.

Audio is cut the same way for every engine: 29 second windows, the cut at the middle of the
quietest 20 millisecond frame in the last three seconds, never before the halfway mark. That
is the rule in `crates/arioso-stt-sherpa`.

## Speech to text

| id | languages | license | size | novel | podcast | RTF | shipped |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `sherpa-parakeet-tdt-0.6b-v2-en` | English | CC-BY-4.0 | 631 MiB | **4.62%** | **5.88%** | 0.24 | yes, default |
| `sherpa-whisper-base-en` | English | MIT | 153 MiB | 8.79% | 6.99% | 0.23 | yes |
| `sherpa-whisper-tiny-en` | English | MIT | 99 MiB | not scored | not scored | — | yes |
| whisper `small.en` | English | MIT | 358 MiB | 9.20% | 10.93% | 0.73 | no |
| parakeet v3 | 25 European languages | CC-BY-4.0 | 641 MiB | not run | 6.95% | 0.27 | no |
| zipformer-en-2023-06-26 | English | Apache-2.0 | 68 MiB | 6.84% | 14.16% | 0.11 | no |

Languages of parakeet v3, from the model card: English, Spanish, French, German, Bulgarian,
Croatian, Czech, Danish, Dutch, Estonian, Finnish, Greek, Hungarian, Italian, Latvian,
Lithuanian, Maltese, Polish, Portuguese, Romanian, Slovak, Slovenian, Swedish, Russian,
Ukrainian.

What the table says:

- Parakeet v2 is the best model on both corpora and runs at whisper-base speed. Its errors
  are misheard words, not omissions: 248 substitutions and 9 deletions on the novel, where
  `small.en` had 325 and 188.
- Whisper base is a third of the download and about two points worse on contemporary speech,
  four on the novel. It stays in the catalog for small installs.
- `small.en` is worse than `base.en` on both corpora at three times the cost. A bigger model
  is not automatically an upgrade, and this one is not.
- Parakeet v3 costs about a point on English against v2. Choose it for the 25 languages, not
  for accuracy on English.
- Zipformer is twice as fast and a ninth of the size, and it falls apart on conversation:
  49 of its podcast errors are the word `yeah` heard as `yet`. It fits clean speech where
  latency dominates, not a meeting recorder.

Two caveats that belong beside the numbers. The podcast reference transcript is itself
machine-generated, so its rows compare engines rather than measure any engine absolutely. And
one voice, one language, no microphone audio: a real recording with room noise will move
every number.

## Text to speech

| id | languages | license | size | round trip | RTF | memory |
| --- | --- | --- | --- | --- | --- | --- |
| `piper-en_US-joe-medium` | en-US | CC0-1.0 voice, GPL-3.0 engine | 60 MiB | 5.88% | **0.089** | 290 MiB |
| `piper-en_US-lessac-medium`, the shipped default | en-US | Blizzard-2013 voice, research only | 60 MiB | not scored | 0.109 | 368 MB |
| BlueTTS 2.5, voice `adam` | he, en, es, it, de | MIT engine, LibriTTS voice | 269 MiB | **4.88%** | 0.600 | 1.5 GiB |

The piper row is the project default and was measured through this repository's own server.
The BlueTTS row was measured outside it, from the upstream Python package, because that
engine is Python and this project is Rust. Full write-up, including a chunker bug of mine
that scored 36 percent before it was fixed, is in
`docs/plans/2026-11-02-tts-engine-comparison.md`.

BlueTTS is the more intelligible voice by about a point and costs 6.7 times the synthesis
time and five times the memory. The vendor calls it the fastest open source TTS; on four CPU
cores it is the slower of the two.

## Speed, with process start included

A single CLI invocation on the same four core machine, so the time includes reading the
config, loading the model and writing the result. That is what a caller of `arioso say` or
`arioso transcribe` waits for. The `RTF` column above is steady state through the server and
is the one to use for a long session; this one is what a one-shot run costs.

| voice | audio | wall | RTF | peak RSS |
| --- | --- | --- | --- | --- |
| `piper-en_US-kathleen-low` | 59.73 s | 7.0 s | 0.117 | 345 MB |
| `piper-en_US-joe-medium` | 78.82 s | 8.5 s | 0.108 | 497 MB |
| `piper-en_US-lessac-medium` | 75.33 s | 8.2 s | 0.109 | 368 MB |
| `piper-en_GB-alba-medium` | 81.87 s | 9.9 s | 0.120 | 335 MB |
| `piper-en_US-ljspeech-high` | 88.43 s | 53.9 s | **0.609** | 406 MB |

| recognizer | audio | wall | RTF | peak RSS |
| --- | --- | --- | --- | --- |
| `sherpa-whisper-tiny-en` | 600 s | 78.1 s | 0.130 | 548 MB |
| `sherpa-whisper-base-en` | 600 s | 134.6 s | 0.224 | 822 MB |

The four medium and low voices cost the same, within a fifth of each other. `ljspeech-high`
is the exception: five times the synthesis time for the same sentence, which is what the
high quality model costs and the reason it is not the default. The recognizer's memory is
mostly activations, not weights: base is 153 MiB on disk and 822 MB resident on a ten minute
file.

Parakeet v2 was not re-measured in this pass. Its row above stands as measured on the
corpora, and it is the default because it wins on accuracy at whisper-base speed.

## Recommended configuration

Speech to text defaults to Parakeet v2:

```json
{
  "stt": {
    "enabled": true,
    "engine": "sherpa",
    "batch_model": "sherpa-parakeet-tdt-0.6b-v2-en",
    "language": "auto"
  }
}
```

That is the shipped default. It costs a 631 MiB download. A caller who wants the small one
sets `stt.batch_model` to `sherpa-whisper-base-en` and downloads 153 MiB instead.

Text to speech ships `piper-en_US-lessac-medium`, and that is a problem worth naming: it is
Blizzard-2013, research only, so the default voice cannot be used in a shipped product.
`piper-en_US-joe-medium` is the voice to pick instead. It is the cheapest that is still
intelligible, its license allows commercial use, and the accuracy gap to BlueTTS is one
point against a sevenfold cost. BlueTTS is the right second engine for a caller who wants a
better voice and will pay for it in a Python sidecar.

Changing the default is a config change and a decision, not a measurement, so it has not
been made here.

Nothing in the catalog is a model whose numbers were taken from a model card. If a model
here has no row, it has not been measured, and the table says so rather than implying a
result.

## Re-running a row

The corpora, the cut rule, the harness, and the scoring scripts are described in
`docs/plans/2026-11-02-long-transcription.md`. A new model needs three things: a catalog
entry with the family in `meta.model_type`, the four files a transducer needs or the three a
Whisper model needs, and a run of the same two corpora. Publish the numbers with the corpus
name attached, because contemporary speech and literary prose do not score the same.
