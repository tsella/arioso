# Test plan: thirty minutes of speech, spoken by one instance and transcribed by another

Date: 2 November 2026. Status: written before the run. Results are appended at the end and
the plan is not edited to fit them.

## The question

A short clip transcribes well. Nobody has measured what happens over a long one. Does
thirty minutes of continuous synthesized speech survive the round trip with usable
accuracy, or does the error rate climb, the audio get truncated, or a limit refuse it?

A second question sits under it: what breaks first on a long run. The candidate failures
are a duration cap, a memory or time cost that makes the run impractical, numbers spoken
as words but written as digits, and proper nouns.

## Corpus

*The Time Machine*, H. G. Wells, Project Gutenberg eBook 35. Public domain in the United
States. The run uses the novel from the first paragraph of chapter I, cut at a paragraph
boundary: 98 paragraphs, 4,675 words, 26,696 characters, with the book's curly quotes
kept. Front matter, the table of contents and the licence text are excluded, because a
table of contents is not prose and would flatter the result.

The file is kept at the run directory as `source.txt`. It is not committed: 26 KB of a
novel belongs in a test fixture only if a test reads it, and this is a measurement.

## Voice and engine

| side | choice | why |
| --- | --- | --- |
| text to speech | `en_US-joe-medium` | CC0, so the sample is redistributable, 22.05 kHz, measured at 0.50 real time factor on this machine |
| speech to text | `base.en` int8, 4 threads | the accuracy tier the project ships as its default, so the result describes the default install |

The high tier (`en_US-ljspeech-high`) sounds better and costs twice the wall clock; the
README already records that it is not twice the voice. A second run with it is worth doing
only if this one lands near the pass threshold.

## Setup: two instances

Both are real servers over HTTP, not library calls, because the question is about the
product a user runs.

| instance | port | config | change from default |
| --- | --- | --- | --- |
| A, text to speech | 10201 | voice `joe`, loopback, no auth | none |
| B, speech to text | 10202 | `base.en`, 4 threads, loopback, no auth | `stt.max_audio_s` raised from 600 to 3600 |

That second change is the first finding, recorded before the run: the default refuses any
audio longer than ten minutes, so a thirty-minute file cannot be transcribed over HTTP
without raising it. A user who records a meeting will meet that limit.

## Procedure

1. Start A and B with their configs. Confirm `GET /readyz` is 200 on both.
2. `POST /v1/tts` on A with the whole text as one JSON body. Save the body as `speech.wav`.
   Record wall clock and the audio duration read from the WAV header.
3. Check the WAV: sample rate, channels, bit depth, duration.
4. `POST /v1/stt` on B with `speech.wav` as the body. Record wall clock and the transcript.
5. Compare the transcript with the source.

## Metrics

- **Word error rate.** Tokens are lowercased and stripped of punctuation on both sides,
  then aligned by Levenshtein distance over words. WER is `(substitutions + insertions +
  deletions) / reference words`.
- **Number-insensitive WER.** The same alignment after mapping digit strings and number
  words to one form, so "1873" and "eighteen seventy three" count as equal. This separates
  a formatting difference from a listening error.
- **Truncation check.** The last twenty reference words must appear at the end of the
  transcript. A missing tail is a truncation, not an error rate.
- **Cost.** Wall clock for each side, and the ratio to audio duration.
- **Error classes.** The twenty most frequent substitution pairs, read by hand.

## Pass criteria

Written before the run:

1. The transcript ends with the source's last sentence, within twenty words of it.
2. Word error rate at or below 5 percent. Synthetic speech from a single voice is the easy
   case; above 5 percent something in the pipeline is wrong, not merely imperfect.
3. Number-insensitive WER no worse than plain WER, and the gap between them explained.
4. Neither side takes longer than the audio itself. A pipeline slower than real time cannot
   serve a recording of a meeting.

A run that fails one criterion is a finding, not a failure of the test. The point is to
learn where the limit is.

## What this does not measure

- Microphone audio. A room, a fan and a second speaker are harder than a clean file.
- Two speakers, interruptions, or overlapping speech.
- Languages other than English.
- Streaming. Both calls here are one request with a long body.

## Results

Run on 2 November 2026, one machine, four cores, no GPU.

### Setup as it ran

| item | value |
| --- | --- |
| text | 5,787 words, 32,705 characters, *The Time Machine*, chapter I onward |
| voice | `en_US-joe-medium`, 22.05 kHz, default speed |
| transcriber | `base.en` int8, 4 threads |
| instance A | `127.0.0.1:10201`, text to speech |
| instance B | `127.0.0.1:10202`, speech to text, `stt.max_audio_s` 3600 |
| audio produced | 1,828.0 seconds, 30 minutes 28 seconds, 80.6 MB |

### What the run cost

| step | wall clock | audio | real time factor |
| --- | --- | --- | --- |
| synthesis, ten requests | 163.4 s | 1,828.0 s | 0.089 |
| transcription, one request | 422.0 s | 1,828.0 s | 0.231 |

Neither side is close to real time. A thirty-minute recording becomes text in about seven
minutes on this machine.

### Accuracy

| measure | first run | after the window fix |
| --- | --- | --- |
| word error rate | 8.96% | 8.79% |
| numbers folded | 8.79% | 8.60% |
| substitutions | 440 | 437 |
| insertions | 39 | 45 |
| deletions | 45 | 32 |

Against 5,849 reference words. The most frequent substitutions are spelling, not listening:

| count | reference | transcript |
| --- | --- | --- |
| 41 | traveller | traveler |
| 35 | and | in |
| 12 | the | that |
| 12 | in | and |
| 8 | travelling | traveling |
| 8 | travelled | traveled |
| 3 | colour | color |

Fifty-two of the 437 substitutions are British against American spelling of four words.

That framing was wrong, and the number with it. A spelling variant is not a listening
error: Whisper heard `traveller` correctly and spelled it in its own variety of English.
Scored with variants folded, the run reads:

| scoring | word error rate |
| --- | --- |
| plain string comparison | 8.79% |
| digits and number words folded | 8.62% |
| the above, plus British/American spelling folded | 7.49% |

Sixty-four of the substitution pairs are pure spelling. The fold removes 76 substitutions
in total, because folding a word changes the alignment around it, not only that pair.

A dictionary can also settle the spelling instead of merely scoring around it. An entry
whose spoken form is the word itself makes the reverse pass prefer that spelling:

```json
{"term": "traveller", "spoken": ["traveller"]}
{"term": "grey", "spoken": ["grey"]}
```

Measured on the first thirty-second chunk of this run, the same audio that transcribed
`time-traveler` and `gray eyes` transcribed `time-traveller` and `grey eyes` once that file
was configured as `tts.dictionary`. The entry must carry `spoken`, not `phonemes`: the
reverse pass pronounces each spoken form and compares sounds, so a `phonemes` entry gives
it ARPAbet symbols to pronounce as words and it matches nothing.

### Verdict against the criteria written before the run

1. **Ends with the source's last sentence.** Pass. Reference tail: "the white snow flashed
   across the world and vanished and was followed by the bright brief green of spring".
   Transcript tail: the same, with "in" for "and". Nothing was truncated.
2. **Word error rate at or below 5 percent.** Fail, at 8.79 percent. The criterion was
   written on the assumption that clean synthetic speech is the easy case. It is easy for a
   large model; `base.en` is the smallest tier the project ships, and 8 to 9 percent is what
   it does on read prose. The gap is the model, not the pipeline: the tail matches, the
   audio is fully covered, and the errors are the small words a base model confuses.
3. **Number-insensitive rate no worse, and the gap explained.** Pass. The gap is 0.19
   points: this text has few numbers, so folding them changes little.
4. **Neither side slower than real time.** Pass, by a wide margin.

### What the run found

Four defects, three of them silent.

1. **The synthesis route refused the text at 4,000 characters**, with `422` and a clear
   message. Not a defect in itself, but the limit was a constant with no way to raise it.
   Now `server.limits.max_text_chars`, default 4000.
2. **The audio body limit was hardcoded to 600 seconds.** Raising `stt.max_audio_s` to 3600
   changed the duration check and left the body check at 57.6 MB, so an 80 MB file was
   refused with `413` by a config that said it accepted an hour. Two limits disagreed and
   the settable one lost. The body limit now follows `stt.max_audio_s`.
3. **The engine silently kept only the first 30 seconds.** Handed 1,828 seconds, sherpa
   decoded the first half minute, logged a warning to stderr, discarded the rest, and
   returned `200` with a transcript and `"seconds": 1827.96`. A caller had no way to know.
   The wrapper now cuts the audio into windows and joins the text.
4. **A window of exactly 30 seconds landed on the wrong side of the engine's own test**,
   which accepts "less than 30 seconds". The window is now 29 seconds, and the engine's
   warning count went from dozens per run to zero.

### Where the errors come from

514 errors, classified by comparing the pronunciation of each pair in CMUdict. A pair with
the same phonemes is not a mishearing: the model heard the sound and wrote another word
that sounds the same.

| cause | count | share | example |
| --- | --- | --- | --- |
| genuine mishearing | 164 | 31.9% | `trammels` → `traimals`, `controvert` → `contour` |
| function word for function word | 103 | 20.0% | `and` → `in`, `the` → `that` |
| near homophone | 78 | 15.2% | `flashed` → `flushed`, `caressed` → `crest` |
| spelling variant | 64 | 12.5% | `traveller` → `traveler` |
| invented a word | 45 | 8.8% | an `and` that was never said |
| dropped a word | 32 | 6.2% | a `the` that was |
| homophone | 26 | 5.1% | `passed` → `past`, `our` → `hour` |
| rare word misheard | 2 | 0.4% | `recondite` → `recognized` |

Three of those groups are the same event at different distances: spelling variant,
homophone and near homophone total **168 errors, 32.7 percent**, where the sound was heard
and a different word was chosen. A larger model reduces them; a dictionary removes the ones
you care about.

The genuine mishearings concentrate in the vocabulary: `trammels`, `controvert`,
`spasmodic`, `lameness`, `nil`, `unreal`. This is an 1895 novel, and a model trained on
modern prose meets those words rarely. That is a property of the corpus, not of the
pipeline, and it is the reason a modern-text run would score better: a claim this run did
not test.

The 77 inserted and dropped words are mostly real, not alignment noise: only 7 of the 45
insertions sit next to a deletion, which is what a moved word looks like. The rest are the
decoder adding a connective or dropping a small word.

Chunk boundaries are not a significant contributor, measured: nine boundaries, the regions
around them hold 4 percent of the reference words and 5 percent of the errors. My earlier
guess that errors cluster at window edges was wrong for the text-to-speech chunk edges. The
engine's internal 29-second windows cannot be located from the transcript alone, so their
share is unmeasured rather than absent.

### What this did not measure

Microphone audio, several speakers, overlapping speech, languages other than English, and
streaming. Each is a different run, and the plan's original scope statement stands.

## Second run: contemporary speech, and a larger model

The first run used a novel from 1895 and the smallest model tier. Both are the hardest case
and the weakest engine, so the 8.79 percent describes a corner. This run changes both.

### Corpus

*Hacker Public Radio*, episode 4001, "HPR Community News for November 2023", published
under CC BY-SA. The first 5,761 words of the episode's transcript, 30,668 characters, read
as written including the disfluencies: `um`, `uh`, `yeah`, and several speakers
interrupting each other. This is how people actually talk, which is the point.

One correction worth recording: the first attempt at contemporary text was Wikipedia. Its
plaintext extracts carry LaTeX (`{\displaystyle ...}`) and citation markers, and the voice
read that markup aloud, so the reference and the audio disagreed by construction. The run
scored 18.53 percent and the number was meaningless. A corpus has to be checked for
markup before it is spoken, not after.

The episode's own transcript is the reference, and it is itself machine-generated. Its
errors are baked into the ground truth here, which caps how low a real error rate can score
and is a reason to read the absolute numbers as a comparison between models rather than as
a statement about the engine alone.

### Models

`small.en` is not in the catalog; the catalog offers tiny and base. The three files
(358 MiB) were fetched directly and added to a local manifest, with the SHA-256 of each file
computed from the download rather than copied from a page.

### Results

Same audio, same voice, same windowing, two models:

| model | WER | substitutions | insertions | deletions | wall clock | real time factor |
| --- | --- | --- | --- | --- | --- | --- |
| `base.en` | **6.99%** | 295 | 39 | 70 | 428 s | 0.26 |
| `small.en` | 10.93% | 250 | 32 | 350 | 1,238 s | 0.75 |

The larger model is worse by this measure, and the reason is visible in the columns: it
makes 45 fewer substitutions and 280 more deletions. It is not mishearing more words; it is
leaving more out. Its output is 5,464 tokens against the reference's 5,782 and base's
5,751.

What it drops is small words, not just fillers: `a` 12 times, `the` 10, `i` 9, `you` 9,
`and` 8. Only 41 of its 350 deletions are disfluencies. Word error rate counts a dropped
`the` the same as a wrong one, so the score follows the omissions.

Per window the loss is small and even, which is why this is not a windowing artifact:
three separate 29-second slices gave 110/110, 104/98 and 100/96 words for base and small.
Uniformly a few words fewer, not a collapse at one boundary.

Cost: 2.9 times the wall clock and 2.2 times the memory for a worse score on this corpus.

### What the two runs say together

| corpus | model | WER |
| --- | --- | --- |
| 1895 novel | `base.en` | 8.79% |
| podcast, 2023 | `base.en` | 6.99% |
| podcast, 2023 | `small.en` | 10.93% |
| 1895 novel | `small.en` | not run |

Contemporary speech scores better than the novel with the same engine, which is what the
vocabulary analysis predicted. The larger model does not help on this audio, and a reader
who wants fewer wrong words rather than fewer missing words might still prefer it: 250
substitutions against 295.

The missing cell is the honest gap. Without the novel through `small.en`, the interaction
between corpus and model size is measured on one side only, and the claim "small.en is
worse" is scoped to podcast speech rather than to the engine.

A model that omits content is also a correctness risk beyond the score: a caller that
transcribes a meeting and trusts the text cannot tell which sentences were skipped. If the
larger tier is ever offered, the catalog should say what this run found rather than imply
that bigger is better.
