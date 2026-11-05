# Diarization

Written after the work, which is the honest way to record what the work taught. The decision
it starts from was taken before any code: diarization is a whole-file operation, and nothing
in the API pretends otherwise.

## What it does

One recording, several voices. The segmentation model marks where speech changes, an embedding
is taken from each marked stretch, and the embeddings are clustered. The answer is a list of
turns, each with a start, an end and a speaker number.

## Decisions

**Offline only.** The clustering needs every embedding at once, so a file is read in full
before any turn is returned. A streaming route would have to answer with a speaker number
before the evidence for it exists, and then change it. `POST /v1/diarize` and `arioso diarize`
are the two doors, and both are synchronous.

**On the runtime that is already linked.** `sherpa-onnx` carries `OfflineSpeakerDiarization`,
and the crate is linked for speech recognition. A second ONNX session for two more graphs would
duplicate the runtime for no gain, which is the same argument F5.1 makes for Silero.

**One catalog entry, two files.** `diarization-en` holds the segmentation model and the
embedding model. The alternative — two entries — makes the user download a segmentation model
that cannot do anything alone, and puts two sizes in a list where one thing was asked for.
The loader picks the files by name rather than by suffix, because both are `.onnx` and a
suffix lookup hands the same file in twice.

**Speaker numbers are renumbered from zero.** The clustering numbers its clusters as it builds
them: the two-voice clip came back as speakers 0 and 4, and the four-voice clip used 0, 1, 6
and 11. Those ids are an artefact of how many candidate clusters were opened and merged. A
transcript that says "speaker 4" in a two-voice recording reads as a miscount, so the turns are
renumbered in the order the voices first speak. The numbers remain local to the file, and the
docs say so.

**A rate that is not 16 kHz is refused.** Resampling silently would scale every timestamp by
the ratio: the turns would look plausible and be wrong by 2.75× on a 44.1 kHz file. The error
names both rates.

**The two model licences are stated separately.** pyannote segmentation 3.0 is MIT; the
3D-Speaker CAM++ embedding is Apache 2.0. The catalog entry carries both in `license_text`
rather than one of them standing for the pair.

## Measurements

On the recordings the segmentation model's release ships, from the CLI on this machine:

| clip | audio | turns | speakers | wall | RTF |
| --- | --- | --- | --- | --- | --- |
| `1-two-speakers-en.wav` | 16.0 s | 4 | 2 | 2.4 s | 0.15 |
| `0-four-speakers-zh.wav` | 56.9 s | 12 | 4 | 11.5 s | 0.20 |

Both find the number of voices the recording contains. Neither is scored for boundary
accuracy: no hand-marked turns exist here to compare against, and a figure without one would
be invented. Memory: 156 MB resident for the 16 s clip, 180 MB for the 57 s one.

## What is not done

- **No scoring.** Word-level diarization error rate needs reference turns. The two release
  clips have a known speaker count and nothing finer.
- **No streaming.** See above: it is not a scheduling question, it is what the algorithm is.
- **No resampling.** A 44.1 kHz file is refused. The engine has no resampler; the terminal
  front end grew one for playback, and a second copy in the server is not obviously right.
- **No cross-file identity.** Speaker 1 in one recording is not speaker 1 in another, and
  nothing here tries to link them.
