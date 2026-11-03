# bench

A harness that measures arioso on one machine, and one CSV that collects the results so
that two machines can be compared.

The harness is Python 3 and the standard library only. It drives the built `arioso` binary
from the outside, so it measures what a caller pays, and a reporter needs no build step, no
package, and no second toolchain to contribute a number.

## For agents: the short version

```bash
cargo build --release
python3 bench/run.py --models ~/.local/share/arioso/models --reporter <github-user>
```

That runs the whole set and prints a table. To put the run in `bench/results.csv`, add
`--write`. To run one test, name it:

```bash
python3 bench/run.py say   --models ~/.local/share/arioso/models --reporter <github-user>
python3 bench/run.py stt   --models ~/.local/share/arioso/models --reporter <github-user>
python3 bench/run.py micro --models ~/.local/share/arioso/models --reporter <github-user>
```

`python3 bench/run.py --list` names the five tests. `python3 bench/run.py --help` lists
every flag. A run with no test name runs all five, in the order `say`, `stt`, `server`,
`stream`, `micro`.

## What it measures

| Test | What it does | What lands in the row |
| --- | --- | --- |
| `say` | Synthesizes one fixed 560-character passage three times | `say_wall_s`, `say_audio_s`, `say_rtf`, `say_rss_mb` |
| `stt` | Transcribes the 60 s clip in `bench/audio/` three times | `stt_wall_s`, `stt_audio_s`, `stt_rtf`, `stt_rss_mb` |
| `server` | Starts a server on a free port, waits for the first `200`, then 8 keep-alive clients × 200 requests on `/healthz` | `startup_ms`, `healthz_rps` |
| `stream` | One paced session for the first partial, then the whole clip as fast as the socket takes it | `stream_first_ms`, `stream_total_s`, `stream_rtf` |
| `micro` | `doctor` (config, manifest, voice config), then `normalize` against the process floor | `doctor_ms`, `normalize_us` |

Every timing is the median of `--repeats` runs, default 3. Peak memory is the kernel's high
water mark for that one process, read in a child of its own so it is not the largest any
earlier command reached.

## What it deliberately does not measure

- **Accuracy.** Word error rate needs a corpus and a reference, and it does not vary with
  hardware. `MODELS.md` carries those numbers.
- **The segmenter and the reverse dictionary on their own.** Both need a library call, and
  a Python driver has no way in. They are inside the `stream` and `stt` numbers, not beside
  them. `BENCHMARK.md` has the isolated figures, measured from inside the crates.
- **Download speed.** That is the network, not the machine.
- **Quality of the output.** A fast wrong answer scores well here. `stt_rtf` says nothing
  about whether the words were right.

`normalize_us` is a difference of two medians, so it carries the noise of both. On a quiet
machine it lands near 200 µs; treat anything under about 100 µs as "below the noise floor",
not as a fast machine.

`healthz_rps` is driven by 8 Python threads, and the client becomes the limit on a fast
server: on one four-core machine the same binary measured between 1,580 and 1,940 rps across
runs. Compare rows from this harness with each other, not with `wrk` or `ab`.

## Requirements

- A release build. `cargo build --release` produces `target/release/arioso`.
- The default voice and the default speech model, installed:

  ```bash
  ./target/release/arioso models download piper-en_US-joe-medium
  ./target/release/arioso models download sherpa-parakeet-tdt-0.6b-v2-en
  ```

  Pass `--voice` and `--model` to measure a different pair, and put the same values in the
  row: the `voice` and `stt_model` columns exist so that two rows with different models are
  not compared as if they were the same run.
- Python 3.9 or newer, no packages.
- `--models` or `ARIOSO_MODELS_DIR` pointing at the models directory. The harness refuses
  to start without it, because a run against an empty directory measures an error message.

## The results CSV

`bench/results.csv` is the shared file. One row per run: the machine, the date, who ran it,
and every measurement taken. A test that was not run leaves its cells empty, so a partial
run is still one row.

```
date,arioso_version,reporter,os,cpu,cores,memory_gb,gpu,binary,voice,stt_model,repeats,say_wall_s,...
```

- Append rows. Never edit or reorder a row that is already there, and never reformat the
  file: a pull request that changes more than one line cannot be reviewed as a measurement.
- A new run on a machine that already has a row is a new row with a new date. That is how a
  machine's own history and a version's effect stay readable.
- `binary` is `release` or `debug`. A debug row is welcome as a data point and is not
  comparable with a release row.

## Submitting a result as a pull request

1. Fork `github.com/tsella/arioso` and make a branch named `bench/<github-user>-<date>`,
   for example `bench/tsella-2026-11-03`.
2. Run the whole set on a machine that is otherwise idle:

   ```bash
   cargo build --release
   python3 bench/run.py --models ~/.local/share/arioso/models \
       --reporter <github-user> --write
   ```

3. Check the diff. It must be one added line in `bench/results.csv` and nothing else. If
   the file was rewritten or a column moved, throw the branch away and start again from
   `main`: a CSV that gained a column is a format change, and that belongs in its own pull
   request with a note in this file.
4. Open the pull request. Title:

   ```
   bench: <cpu>, <cores> cores, <os>
   ```

   Body:

   ```
   Machine: <cpu>, <cores> cores, <memory> GB, gpu <gpu>
   OS: <os>
   arioso: <version>, release build, median of 3
   Models: default voice, default speech model
   Nothing else was running during the run.
   ```

5. A maintainer checks three things: the row is the only change, the hardware columns match
   the machine named in the title, and the numbers are the shape the machine should produce.
   A row that is faster than every other row of its class is read twice, not merged first.

An agent submitting on someone's behalf does the same thing, with two rules of its own: it
never edits another row to make the table line up, and it never runs the harness on a
machine it was not asked about, because the hardware columns would then describe the wrong
box.

## Rules for a fair number

- Build release. A debug binary is several times slower and its row says so.
- Run on a machine that is otherwise idle. A build running beside the harness shows up in
  `say_rtf` and in `healthz_rps` and nowhere else.
- Do not tune the machine during a run: no governor change, no closing the laptop lid, no
  second benchmark in another terminal.
- Take the median of at least three runs. `--repeats 3` is the default and the minimum.
- Report the machine as the harness detects it. Pass `--cpu`, `--memory-gb` or `--gpu` only
  to correct a detection, not to describe what you wish the machine were.

## The audio clip

`bench/audio/bench-60s.wav` is 60.0 s, 16 kHz, mono, 16-bit PCM, 1.9 MB. Every reporter
transcribes the same file, so `stt_rtf` compares machines and not recordings. 16 kHz is the
rate the streaming route accepts.

The clip is synthetic and carries no third-party rights: a CC0 voice reading the opening of
*The Time Machine* (1895), public domain. `bench/audio/bench-60s.txt` is the text it was
made from. To regenerate it, with the voice already installed:

```bash
./target/release/arioso say --out /tmp/bench-full.wav "$(cat bench/audio/bench-60s.txt)"
ffmpeg -y -i /tmp/bench-full.wav -t 60 -ac 1 -ar 16000 -sample_fmt s16 \
    bench/audio/bench-60s.wav
```

The file that ships was cut from the first 60 seconds of that synthesis. A different build
produces different bytes, because the model samples noise; the length and the rate are what
the harness depends on, and it reads both from the header.
