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

**Two models in one directory produce a pair that loads and then fails.** Adding whisper
base beside whisper tiny left both sets of files flat in the models directory. The loader
searched for an encoder and a decoder separately, so it took tiny's encoder and base's
decoder; the graph then failed with a cache-dimension error that reads like a corrupt
model. Rule: when a directory can hold more than one version of a thing, select the files
by a shared name or by the manifest, and refuse a set that does not agree.

**A fallback path that returns early is invisible in testing.** The partial-transcript
path used `try_lock` and returned `None` when the engine was not yet loaded, so the first
session after a restart produced no partials at all and later sessions worked. It looked
like a timing quirk. Rule: when a second code path exists for speed (try instead of wait),
it must still reach the same lazy initialization as the slow path, or its failure is a
silent absence rather than an error.

**A ratio computed in a summary is not a measurement.** The first draft of the streaming
docs said the final landed 280 ms after the endpoint. The real arithmetic was 340 ms: the
speech ends at 3.56 s of audio, the detector confirms 500 ms later at 4.06 s, and the
final arrives at 4.42 s of wall clock. Rule: subtract the two numbers you actually have,
in writing, before a latency figure reaches a doc.

**A token stream without separators fails quietly.** Words inside one piece of text were
concatenated with no space symbol, so the model read a sentence as one long word. It still
sounded like speech, so listening did not expose it; reading the symbol list did. Rule:
when a model consumes a token stream, assert the separators in a test, because a missing
one does not raise anything.

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

## The server surface

**A JSON `Value` is not a faithful echo of an `f32`.** Round-tripping a config through
`serde_json::to_value` turns `1.4` into `1.399999976158142`, because the value stores an
`f64` and the `f32` formatter never runs. A test comparing the two fails on a change that
is correct. Serialise to text first and parse that when a number has to come back the way
the file writes it.

**A test can pin the wrong name and still pass.** The active-voice marker compared the
config's bare `en_US-lessac-medium` to a catalog id `piper-en_US-lessac-medium`, so it
marked nothing and every voice looked inactive. The fixture agreed with the bug because it
asserted against the same bare name. Assert against what the code resolves to, not against
the input it was given.

**A document generated from handlers describes routes the server may not register.** With
`stt.enabled: false` the route is absent and the document still promised `/v1/stt`, which
sends a client to a 404. Cut the document to what the router actually built.

**`to_value` on a struct with an `f32` and a test on the same value.** Two symptoms, one
cause: the assertion is right and the serialisation path is wrong.

## Shielding a readme

**A badge is a claim.** Four is enough: ci, license, platforms, and what the product
uniquely does. Platforms names only what the CI matrix runs, because a badge claiming iOS
and Android promises what the repo cannot yet back. "No cloud" would be false while the
downloader reaches the network; "inference local" is true. Check the badge URL returns 200
before committing it, or the readme ships a broken image.

**An example written from a real run finds what a test suite missed.** `?language=en_GB`
returned nothing while the catalog said `en-GB`: the voice ids use an underscore and the
manifest a hyphen, so a prefix match on the raw string failed for every real client
query. The unit tests passed because they asked for `en`. Write the example output by
running the command, then read the answer.

## A build that caches by directory existence

**`is_dir()` is not `has files`.** The sherpa-onnx build script extracts its native
libraries into `target/sherpa-onnx-prebuilt/<stem>/lib` and returns early when that
directory exists, never checking whether anything is inside. `rust-cache` prunes large
directories, so a restored cache left the directory present and empty: the script skipped
its download, printed a link search path, and the linker reported "could not find native
static library". Deleting the build output and the fingerprint changed nothing, because
those decide whether the script runs, not what it finds when it does.

Two failed attempts before the right one. The first removed only `target/debug/build`,
which broke the platforms that had been passing, because it forced a re-run that found the
same empty directory. The second added the fingerprint, with the same result. The rule:
when a third-party build script fails to link, read the script before changing the cache.
The answer was four lines into its download function.

**A PID file without a lock is a number, not a fact.** The first version wrote the child's
PID and nothing else. Two instances on one config overwrote each other's file, so `stop`
killed whichever wrote last, and a file left by a crash named a PID the system had already
given to something else. The lock is what carries the meaning: a file that can still be
locked has no owner. `flock -n` failing is the proof that a process is alive; a file naming
PID 1 was removed as stale without a signal leaving the machine.

**A new config field is a breaking change unless the struct says otherwise.** Adding
`shutdown_grace_ms` to `Limits` made every config written by the previous version fail to
parse with `missing field`. `#[serde(deny_unknown_fields, default)]` gives the two
behaviours that matter together: a field a later version added is filled from the default,
and a field the operator misspelled is still refused.

**Bound the socket before you print the port.** `axum-server` refuses a blocking
`TcpListener`, so the bind has to set non-blocking first. And the port line has to come
after the server object exists, not after the bind: a bound socket is not a server that can
answer, and the first version announced a port and then panicked.
