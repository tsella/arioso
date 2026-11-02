# Lessons

Dated entries. Each one records a mistake that cost time, what showed it, and
the rule that prevents the repeat. New entries go at the top of their section.

## Testing and verification

**A test that has never run is not a test.** The `arioso-tts-piper` tests used
`tempfile` without the dev-dependency. Only `cargo build` had been run on that
crate, so the workspace suite broke two commits later. Rule: run `cargo test -p
<crate>` for every crate touched, not just a build.

**Test in the environment the user will have.** `arioso say` worked without
`LD_LIBRARY_PATH` only because the variable was still exported in the shell from
an earlier command. `arioso serve` then died with a missing shared object. Rule:
re-check with `env -u VAR` before believing a run.

**Quote test strings with single quotes.** A sample text containing `$3.99`
inside double quotes let bash expand `$3` to nothing, so the program received
`.99` and reported a phantom unknown word. The bug was in the test command, not
the code.

**A heuristic needs a case that breaks its assumption.** The sentence splitter
skipped a split when the next character was lowercase. That is correct for
`p.m.` and wrong for a message typed entirely in lowercase, where it produced
one giant sentence and no streaming at all. Rule: run the splitter over the
awkward inputs (numbers, abbreviations, all-lowercase) before trusting it.

**Instrument before believing.** A five-line `cargo run --example probe` printing
`sentences(text)` settled in seconds what reading the code had left ambiguous.
Delete the probe when the question is answered.

**A stale binary gives a stale measurement.** Three speed runs came out within
2 % of each other, which looked like the speed setting did nothing. The CLI binary
had not been rebuilt after the config change, so the new environment override was
not in it. Rule: build the binary in the same command as the measurement, and
treat "this setting has no effect" as a claim to verify, not to report.

**A summary line is not the result.** Two server tests were failing and the
commit went in anyway, because the check summed a parsed field of `cargo test`
output instead of reading its exit code. The field was wrong, so the number
looked fine. Rule: gate a commit on the exit code, and never on a number you
computed from the output.

**Two writes in one clock tick look like one.** The dictionary reload compares
modification times, so a test that rewrites the file microseconds after the first
write sometimes sees no change and sometimes does. The test now moves the
modification time forward by hand. Rule: when production keys on a clock, the
test sets the clock; it does not race it. The production limit is real and
accepted: two saves inside one tick are indistinguishable.

**A module that is not declared compiles to nothing.** Twice, a new file with
tests was written but never added to `lib.rs`, so `cargo test` reported the old
count and passed. Rule: after adding a module, check that the test count went up
by the number of tests written.

**A large text slice can delete code that is not in the slice's name.** Cutting
from one function to the end of the file to move a helper removed `set_speed`,
`map`, and `synthesize` along with it. The build caught it, but only after the
edit. Rule: read `git diff --stat` after a structural edit, and prefer small
patches over one wide cut.

## Build and environment

**Pin the dependency to what the toolchain supports.** `ort` 2.0.0-rc.15 needs
rustc 1.92 and the host has 1.91; rc.10 works. Its default features also pull
OpenSSL through the binary downloader, so the runtime is fetched by hand and
located by `ORT_LIB_LOCATION`.

**An rpath needs one entry per binary depth.** `target/debug/arioso` and the test
binaries under `target/debug/deps` are one directory apart, so a single
`$ORIGIN` relative path leaves the tests unable to load the runtime (exit 127).
Two entries cover both.

**A stale crate index looks like a version conflict.** `cargo build` reported
"failed to select a version" for `tokio-tungstenite ^0.29.0` with candidates
ending at 0.28. `cargo info tokio-tungstenite` refreshed the index and the build
resolved. Rule: refresh before downgrading anything.

**Run the formatter before the commit, not after.** Several commits landed
unformatted and the failure surfaced later, in a background job whose output
arrived out of order. Rule: `cargo fmt --all && cargo clippy --all-targets -- -D
warnings && cargo test` before `git commit`, in that order.

## Structure

**Two copies of a pipeline drift.** The CLI and the server each had their own
word splitting, dictionary lookup, and synthesis loop. The server split
sentences, the CLI did not, so the same sentence produced different audio
depending on the entry point. Rule: one implementation, callers pass a sink.

**A name that describes the wrong thing misleads the next reader.** A counter
called `frames` counted samples. Renamed when found.

## Tooling

**`pgrep -f <pattern>` matches your own shell.** It reports the wrapper whose
command line contains the pattern, so a stopped server looked alive. Check the
port or `ps -p <pid>` instead.

**Backgrounding with `&` is refused by the terminal tool.** Use the background
option and poll, or the call is rejected outright.
