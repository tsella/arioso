# TTS engine comparison: piper against BlueTTS

## Question

Is BlueTTS a better voice than the piper voice this project ships, and what does it cost?

## Method

The same 5,761 word podcast transcript through both engines, then the same recognizer on
both outputs: Parakeet v2 with arioso's cut rule. A round trip measures intelligibility,
which is the property a caller notices first, and it holds the recognizer constant so the
two numbers compare. It does not measure naturalness, prosody, or whether a person enjoys
listening, and nothing here did.

| | piper | BlueTTS |
| --- | --- | --- |
| version | `en_US-joe-medium` through arioso 0.1.2 | BlueTTS 2.5 ONNX, voice `adam.json` |
| model on disk | 63 MiB | 269 MiB |
| sample rate | 22,050 Hz | 44,100 Hz |
| runtime | Rust crate in process | Python 3.12, uv, onnxruntime |
| license | MIT voice, MIT engine | MIT engine, voice metadata cites LibriTTS |

Both ran on four cores, no GPU.

## Results

| | piper | BlueTTS |
| --- | --- | --- |
| round trip WER | 5.88% | **4.88%** |
| substitutions | 228 | 162 |
| insertions | 31 | 21 |
| deletions | 81 | 99 |
| audio produced | 1,645 s | 1,509 s |
| words per minute | 210 | 229 |
| synthesis wall clock | 163 s | 906 s |
| real time factor | **0.089** | 0.600 |
| peak memory | 290 MiB | 1.5 GiB |

BlueTTS is more intelligible by about a point, with a third fewer substitutions, and it
speaks 9 percent faster. It costs 6.7 times the synthesis time and five times the memory.

For a project that synthesizes a sentence per request, both are fast enough. For a project
that renders long text, 0.089 against 0.600 is the difference between 11 times realtime and
1.7 times realtime, and it changes how many voices one machine can serve.

The vendor calls BlueTTS the fastest open source TTS. On this hardware it is the slower of
the two, so that claim is about small models, GPU backends, or both, and it should not be
repeated as a device measurement.

## The mistake worth recording

The first full run scored 36.44 percent with 1,409 deletions, and the transcript was
gibberish: `This is Hacker Code 40 from Monday to 3000 today's Shield`.

The cause was the chunker, not the model. It split the text on `". "`, which drops the full
stops, so BlueTTS received unpunctuated run-on text and spoke it at 305 words per minute.
Punctuation is the pacing signal. With the sentences intact the same engine produced 229
words per minute and 4.88 percent.

The diagnosis that worked: the same 600 characters, synthesized alone, came back clean. A
defect that depends on how text is assembled, and not on the text itself, points at the
assembly.

## What this does not settle

One voice, one corpus, one language. `adam.json` is a LibriTTS-derived American voice; a
different voice would score differently. Nothing here tested Hebrew, Spanish, Italian, or
German, which BlueTTS also covers and piper does not.

Arioso is Rust and BlueTTS is Python, so adopting it means a sidecar process rather than a
library call. That is the same shape as the sidecar the project already supports, but it
adds a Python runtime to the deployment.

## Recommendation

Piper stays the default. It is 6.7 times cheaper to run, needs a fifth of the memory, and
loses one point of intelligibility.

BlueTTS is worth a second engine slot when a caller asks for a better voice, and the
comparison above is the evidence to show them: better, and what it costs.
