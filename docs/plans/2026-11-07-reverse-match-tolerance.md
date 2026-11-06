# Plan: the reverse match tolerance as a setting

The reverse map corrects a transcript by sound: every spoken form of every dictionary entry is
pronounced, the transcript is pronounced, and a near miss counts as the term. How near is
`TOLERANCE`, a `const` in `crates/arioso-g2p/src/reverse.rs`. The ask is to make it a setting.

## What the number does today

`allowed = (form.phonemes.len() as f32 * TOLERANCE).max(1.0)` — the fraction of a form's
phonemes that may be edit distance away, with a floor of one edit. Two neighbours share the
decision and stay private:

| constant | value | what it decides |
| --- | --- | --- |
| `TOLERANCE` | 0.4 | how far a heard span may be from a form |
| `WORD_MISMATCH_PENALTY` | 0.1 | how much a span of the wrong word count is penalised |
| `MIN_PHONEMES` | 4 | below this a form is not indexed at all |

The measured evidence behind 0.4 is already in `PRD.md` (F15.9): the exact matcher fired on
none of three round trips, and the phonetic one fires on all three. That table is the reason
0.4 is the default, and it is what a setting must not silently contradict.

## What is not there

`crates/arioso-g2p/src/reverse.rs` is 322 lines and has **no tests**. Nothing in the
repository asserts that a transcript is corrected: the correction tests in
`arioso-core/src/dictionary.rs` exercise the older exact map, which is a different
implementation, and the server builds a `ReverseIndex` at line 312 and applies it at line 1405
with nothing checking the answer between them. The three round trips in PRD F15.9 are the only
record of the behaviour, and they were run by hand.

So step 1 of this plan is not only `build_with`. It is the first direct test of the map, and a
setting that changes a number nothing tests is a number nobody can defend.

## What the setting is

`stt.reverse_tolerance: f32`, default `0.4`, range `0.0..=1.0`.

It belongs under `stt`, not beside `tts.dictionary`. The file is shared by both directions;
the tolerance is a property of reading it backwards, and only transcription reads it
backwards. The alternative — a `dictionary` block holding path and tolerance together — moves
`tts.dictionary` and is a schema migration for one number.

## Decisions this plan has to make, and the answers

**Zero means zero.** Today a tolerance of `0.0` still allows one edit, because of the
`.max(1.0)` floor: a four-phoneme form would match anything within 25 %. That is not an off
switch, and a setting called tolerance that cannot be turned off is a trap. The floor applies
from the first non-zero value instead, so `0.0` is an exact comparison and a user who wants
the old letter-for-letter behaviour can have it.

**The tolerance lives in the index, not in the call.** `ReverseIndex` pronounces every form
once; the tolerance decides which candidates exist, so it is part of what is built. `apply`
keeps its signature. A config change rebuilds the index, which is the same path a dictionary
file change already takes.

**Two knobs stay private.** `MIN_PHONEMES` and `WORD_MISMATCH_PENALTY` interact with the
tolerance in ways nobody can predict from a number, and three exposed knobs is a config whose
interactions are unowned. If the measurement below shows one of them needs to move, it moves
as a constant with the table to justify it, not as a field.

**Validation refuses, it does not clamp.** A value outside `0.0..=1.0` is an error naming the
field. A value above `0.6` is accepted with a warning: at 0.6 a four-phoneme form matches a
difference of two phonemes, which is half the word, and that is worth saying once in the log
rather than refusing outright — a user matching badly-transcribed speech may want it.

## Where it has to reach

| surface | change |
| --- | --- |
| `arioso-g2p` | `ReverseIndex::build_with(user, cmu, tolerance)`; `build` keeps the default; `tolerance()` for tests and logs |
| `arioso-config` | `stt.reverse_tolerance`, default 0.4, validation, `config.json.example` |
| `arioso-server` | `UserState` records the tolerance the index was built with; `user_snapshot` rebuilds when the file **or the tolerance** changed; `/v1/config` GET and PATCH carry it; OpenAPI field |
| `arioso-cli` | `arioso transcribe --tolerance`, a per-run override of the config; there is no `correct` subcommand today and this plan does not add one |
| `arioso-abi` | unchanged. The C surface has no correction call to widen, and a per-call tolerance is a v2 addition, not a new argument on a function that does not exist |
| `PRD.md` | F14.7a: the tolerance is a setting, zero is exact, the default is 0.4 and why |
| `arioso-transcriber` | out of scope here; the sidecar config gains the field and the app's settings screen is a later item |

## The measurement that defends the default

A setting needs a number, and the number needs a table. Fixture: the three round-trip pairs
already in F15.9 (`tom seller` → `Tonseller`, `koober netties` → `Cooper Netis`, `sell a` →
`cell a`), the near misses the module's own doc comment claims (`ah ree oh so` → `a Rio so`,
`is a Rio so`), and a clean paragraph of ordinary English containing no dictionary term. Run every pair at
`0.0, 0.2, 0.3, 0.4, 0.5, 0.6, 0.8` and report, per setting:

- corrections made, and how many of them are the expected term;
- rewrites on the clean paragraph, which are false by construction.

The default stays `0.4` unless the table shows a better point, and the table ships in the PRD
next to the one it already has. A tolerance table without false positives is a table that
measured only what it wanted to find.

## Tests, each failing before the change

- `a_tolerance_of_zero_matches_only_exactly` — a one-phoneme miss is left alone at 0.0 and
  corrected at the default.
- `a_higher_tolerance_corrects_a_word_a_lower_one_leaves` — monotone on one pair, so the
  knob is shown to do what its name says.
- `the_index_remembers_the_tolerance_it_was_built_with` — two indexes, same input, different
  answers.
- `a_tolerance_outside_zero_to_one_is_refused` — and the message names the field.
- `a_clean_paragraph_is_unchanged_at_the_default` — the false-positive guard, which is the
  one that fails first when the number moves.
- A server test: patching `stt.reverse_tolerance` changes the next correction without a
  restart, through the same reload path the dictionary file uses.

## Order

1. `build_with`, exact-zero semantics, and the first tests this map has ever had: one form, one
   near miss, one exact miss, one clean paragraph. The crate decides everything and needs no
   models, so this step is cheap and it is the one that makes the rest safe.
2. Config field, validation, example file, PRD entry.
3. Server wiring: tolerance in `UserState`, rebuild on change, route tests.
4. CLI flag.
5. The measurement table, and the PRD paragraph that cites it.

Each step is a commit with the gate: fmt, clippy, tests, docs.

## What happened

Executed as written. `stt.reverse_tolerance` is a setting, zero is exact, the index is built
with it and rebuilt when it changes, `arioso transcribe --tolerance` overrides it for one run,
and `reverse.rs` has eight tests where it had none. The measurement is F14.7b in `PRD.md`:

| tolerance | corrected | clean paragraph |
| --- | --- | --- |
| 0 | 1 of 6 | unchanged |
| 0.2 | 5 of 6 | unchanged |
| 0.3 | 6 of 6 | unchanged |
| **0.4** | **6 of 6** | **unchanged** |
| 0.5 | 6 of 6 | `nothing Sell A is on the list` |
| 0.8 | 6 of 6 | five rewrites |

The default did not move. 0.4 is the last setting that corrects every fixture without
rewriting the clean paragraph, and 0.5 is where the first false positive appears — one step
below the shipped number rather than several, which is tighter than the plan assumed and worth
knowing before anyone argues for raising it.

## What this does not do

- No per-entry tolerance. One number for the dictionary, because the reason to raise it is the
  recogniser, not the term.
- No tolerance on the forward direction. Synthesis already refuses a voice that cannot say a
  form; there is nothing to approximate.
- No change to the default. 0.4 is measured, and moving it would change every transcript of
  every user who never asked for the setting.
