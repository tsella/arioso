# Lessons

Dated entries. Each one records a mistake that cost time, what showed it, and
the rule that prevents the repeat. New entries go at the top of their section.

## Testing and verification

**"Byte-identical" is a claim about bytes, not values.** Removing the string-and-tree round
trip from `PATCH /v1/config` left every field and value alone, and the body still parsed to
the same document — but the keys came out in the struct's declaration order instead of
alphabetical, because the tree it used to pass through is a `BTreeMap`. Comparing the two
bodies as parsed values cannot see that. Compare the text, and when it differs, say which
difference is deliberate.

**A gate reads every result, not the ones you expect to pass.** The four checks ran as one
chain writing `FMT=`, `CLIPPY=`, `TEST=` and `DOC=` markers. Three were read. `FMT=1` sat in
the log while six files were committed unformatted, and a later `cargo fmt --all` produced a
54-line diff across five commits' worth of work. Read all four markers, and treat a marker
that is not `0` as a failure whatever the other three say.

**A test that proves a refusal does not need the real file.** Proving that warming the wrong
speech model is refused wanted a model on disk, and the model in the catalog is 631 MiB.
Writing it to a temp directory turned a 0.06 s suite into a minute and proved nothing that an
empty file at a declared size of zero does not. Zero the sizes in the test's own copy of the
manifest, write empty files, and keep one helper for every install-state test.

**A test that passes with the fix removed proves nothing.** The delete reservation test
asserts that a failed delete releases the model id. It passed before the reservation
existed, because with no reservation there is nothing to leak. The test pins the release,
not the fix, and saying so in the commit is the honest version: the atomicity is one lock
acquisition and the observable end is the only part a test can reach. Before claiming a test
covers a change, remove the change and watch it fail; when that is impossible, write down
what the test does pin.

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

## A performance claim needs both sides

A pass that removes allocations can say what it removed, and that is a code fact: two
buffers where one will do, a copy per frame, a lowercase per lookup. It cannot say it made
the program faster without a number from before the change. This pass took the after
numbers and not the before ones, so the plan records a baseline and an argument, never a
speedup. Take the measurement before the first edit, or write down that the claim is
structural.

## A bound you invent is a performance change

Halving the ONNX thread count was written as politeness to the rest of the server, and it
cost a quarter of the synthesis time on the machine the project targets. Any limit added for
a reason other than a measurement is a performance change that has to be measured like one:
run the end-to-end number with the bound and without it, on the smallest machine the project
supports. A design argument that sounds careful is not evidence.

## A catalog of 178 is a different object from a catalog of 5

Adding every Piper voice changed three things that a five-voice catalog hid.

A voice name can contain a letter outside ASCII (`pt_PT-tugão-medium`). Every URL built from it
has to be percent-encoded per path segment, and the identifier has to survive a path parameter,
a JSON body and a file name. Test the one with the accent, not a sample of five.

A voice's licence is in its `MODEL_CARD`, as a markdown bullet (`* License: CC0`), not as YAML.
Most cards carry none, and the honest value for those is `unknown` with `commercial_use: false`,
not a guess from the dataset. Most English voices also name a parent model that is research only,
so the parent is recorded beside the licence: it has to be read before anything ships.

A list of 178 rows is only usable with a filter, and a piped list is normal, so a closed pipe is
not an error. Both were invisible at five.

## An accented letter is spoken, not dropped

The spelling rules read ASCII. A Portuguese or French word is not in an English dictionary, so it
reached the rules with `é` in it, the rules produced nothing, and the word was reported missing
and skipped. A missing word is a hole in the sentence; a folded letter is one wrong vowel that a
dictionary entry can correct. Fold the letter to its base form and say the word.

## A voice can be several voices, and the number is an input

Piper voices are single-speaker unless the model says otherwise. `en_GB-semaine-medium` has four
speakers from the SEMAINE set (prudence, spike, obadiah, poppy), and the ONNX graph takes a `sid`
input for them. Without it the run fails; with it every number is a different voice.

Two facts, not one. The count is in the voice config as `num_speakers`, and the input is in the
graph. Read both: a model can be trained on several speakers and exported without the input, and
that one answers every number with the same voice, which is worse than failing.

Refuse a number the voice does not have. A 422 on the HTTP routes and an error frame on the
socket, because a caller that asked for a different voice has no way to notice it got the same
one. A single-speaker voice accepts 0 and refuses the rest by name.

The test that proves it needs a real multi-speaker model, so the range logic lives in a free
function and is tested without one; the model itself was checked by synthesizing the same
sentence at four numbers and comparing the audio.

## A dictionary is a word list, not a grammar

CMUdict holds `avenger` and not `avengers`. The lookup missed, the letter-to-sound rules took
over, and they read the plural as `AE1 V EH0 NG ER0 S`: stress on the first syllable, a hard
`g`, and a name nobody has heard. 103,684 of its 126,052 entries have no plain plural form, so
this was the normal path for most plurals, not an edge case.

A regular suffix belongs on the entry. Try the stems in the order most likely to be the base
(`making` is `make` before it is `mak`, `running` is `run` before it is `runn`), then choose the
sound the base's last phoneme decides: /s/ after a voiceless sound, /z/ after a voiced one,
/ɪz/ after a sibilant, /t/ or /d/ or /ɪd/ for the past. `knives` also needs the `f` voiced to
`v` before the plural is chosen.

The rule generalises: when a fallback fires for a word that differs from a known one by a
regular transformation, the fallback is doing work the language already did, and the result is
worse than the known entry. Look for the entry behind the word before inventing a pronunciation.

## No speech is not a failure

`POST /v1/stt` answered `503 engine_unavailable` for a silent recording. The recognizer returns
no result when it hears no words, and the wrapper mapped "no result" to an engine error, so the
one outcome that is completely normal in a recorder read as a broken server.

The engine's silence is a result: return an empty transcript and let the caller decide. Keep the
error for audio that is empty or cannot be decoded, which is a different fact. On the streaming
path, an utterance with no words produces no frame at all, because a `final` with empty text
puts a blank line in the transcript.

The tell was a user saying "no speech is not an error". Ask what the failure means to the caller
before choosing a status code: a 5xx says the server is broken, and silence is the server
working.

## A frame where every field has a default accepts anything

The streaming route's start frame parsed into a struct whose only field had a default, and
unknown fields are ignored, so `{"type":"nonsense"}` was a valid start: the server answered
`ready` and waited for audio from a client that was speaking a different protocol. The bug
surfaced only because a downstream contract test asserted an error and got `ready`.

Read the type of a protocol message, and refuse a value that names a message this server does
not implement. Keep unknown *fields* ignored, so a newer client's extra field still works; the
two rules look alike and are not.

## A throughput number measured through a rate limiter measures the limiter

The published figure was 3,500 requests a second on `/healthz`. The load script counted
`len(body)` for every response and never read the status, so the 429s the rate limiter
returned were counted as successes: 400 requests came back as 101 answered and 299 refused.
The server's real figure with the limiter raised is about 29,000 a second, and the honest
default-configuration figure is the limiter's 50.

Two rules. A load client reads the status code and counts only what it asked for. And a
throughput run states the policy it ran under: a limit that applies per address is shared by
every client on loopback, so eight local clients do not get eight times the allowance.

## A default that moves leaves the prose behind

The speech model's default moved from Whisper base to Parakeet v2, and the README went on
saying Whisper base for two releases. Nothing tests prose, so nothing failed. When a
default changes, grep the documentation for the old value in the same commit: the model id,
the voice id, the port, the test count, the route list. A number in a README is a claim
about the binary, and it rots the moment the binary moves.

## A probe that sets a clock sets it last

To prove the log rotates, a file was written and its modification time moved back two days
with `touch -d '2 days ago'`. The write came first in one attempt and last in another, and
the version with the write last reset the timestamp the probe had just set: the rotation did
not happen and the feature looked broken. When a probe manipulates a timestamp, the
timestamp is the last thing it touches, and the probe reads it back before starting the
program under test.

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

**A check at one entrance is not a check.** `tts.format` was refused when the config was
patched and accepted when the config was loaded, so a `config.json` naming `mp3` started
cleanly and answered WAV under the label the caller asked for. The route had been fixed and
the file had not. Every setting that is validated at runtime has a second entrance — the
file, the environment, the CLI — and the refusal has to live where all of them pass: one
constant, one `validate()`.

**A guard on the path that works is half a guard.** The `stop` branch discarded the final
flush's failure and answered `done`, while the binary path sent a coded error; the delete
released its reservation only on success, so a failed unlink would have wedged the id until
restart. A guard belongs on every exit of the operation, not on the one that was being
tested.

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

## A guard that skips what it cannot parse is not a guard

The token rule read `server.bind.parse::<SocketAddr>()` and only checked the address when
the parse succeeded. A config naming a host — `myhost.lan:10101` — fell past the guard
entirely and loaded with no token, and the resolver bound it to whatever that name meant.
`localhost` fell into the same hole from the other side: it is loopback, and the sidecar
rule refused it.

Resolve the bind once, before the rules run, and let every rule ask the resolved address.
One resolution, one answer, and nothing gets to skip a check by being unreadable.

## A model card is not a measurement

Three engines were compared on two corpora before any of them was wired in: whisper base,
whisper small, Parakeet v2 and v3, Zipformer. Parakeet won both, at whisper-base speed. The
larger Whisper lost to the smaller one on both corpora, by dropping words rather than
mishearing them. The fastest model, Zipformer, was the worst on conversation.

The rules that came out of it: measure on the corpus the product will see, because
contemporary speech and 1895 prose do not score the same; feed every engine the same cuts,
because a harness that cuts at fixed marks and a project that cuts at pauses are not
measuring the same thing; and report the plain number beside the folded one, because a
spelling difference is not a listening error.

## A vendor's speed claim is about their hardware

BlueTTS calls itself the fastest open source TTS. On four CPU cores it took 6.7 times the
synthesis time of the piper voice it was compared against, and five times the memory. It was
the more intelligible of the two by a point, which is a real reason to offer it and no
reason to repeat the claim.

## Punctuation is the pacing signal

A text chunker that splits on a period and a space drops the full stops. The engine then
receives unpunctuated run-on text, speaks it at 305 words per minute instead of 229, and the
round trip scored 36 percent instead of 4.88. The tell was that the same 600 characters
synthesized alone came back clean: when a defect depends on how pieces are assembled and not
on the pieces, the assembly is the suspect.

## A default belongs to the implementation that reads it

`vad.threshold` shipped as `0.5`, which is a Silero speech probability. The only detector in
the binary is an RMS energy segmenter where 0.01 is speech, so the config validated cleanly
for months while the field was ignored, and wiring it in with that default would have made the
segmenter deaf to everything short of clipping. The same commit that wires a config field has
to read its default against the code that will now use it. The same review found
`tts.format` accepting `mp3` and `opus` while the route always wrote WAV: a value the config
accepts and the server cannot produce is the same lie as a field nothing reads.

## A boundary rule fixes one case and breaks the next

An ordinal suffix matched anywhere, so `1stand` read as `first` plus `and`. Requiring a word
boundary fixed that and turned `2ndly` from `secondly` into `twondly`, which was the correct
answer before the fix. Check both directions of a rule before committing it: the input that
motivated the change, and the input that was already right. The exception kept here is
deliberate — an adverb ending in `ly` keeps its ordinal — and it is written in the commit
message so the next reader knows it is a decision.

## A process-wide counter measures the process, not the call

`alloc_count::measure` held a lock, so two measurements never overlapped. That was not
enough. The tests in one binary run on several threads, and a test that built a
126,052-entry dictionary string outside its own measurement put 126,000 allocations inside
another test's window: a call that costs 7 allocations reported 145, and the suite passed or
failed depending on which test started first. The counters are per thread now, so a
measurement counts the thread that asked for it. Rule: a bound that can absorb another
test's work is not a bound.

## A voice is not always a model

`PATCH /v1/config` dropped the loaded engine whenever `tts.voice` changed. That is right for
Piper, where each voice is its own graph, and wrong for Kokoro, where 54 voices are rows of
one archive under one graph: the switch re-read and re-compiled 325 MB to read 522 KB, and
paid it on the next request, where it looked like slow synthesis rather than a config change.
A field that names a choice says nothing about what the choice costs; the invalidation rule
belongs to the engine, and a second engine is when the first one's assumption starts costing
time.

## Decode where the bytes arrive

A reader that fills a byte buffer and then parses it holds the input and the output at once,
which is the whole cost when the output is the table the input describes. `VoiceFile::style`
read a 522 KB entry into a `Vec<u8>` and decoded that into a `Vec<f32>`, so loading one style
cost a megabyte. Parsing the header first and decoding as the payload arrives is one
allocation instead of three and half the memory; the refusals stayed the same because the
checks moved before the read rather than into it — a shape that needs more bytes than the
entry holds is refused by comparing the two, not by a short read failing.

## A shortcut keyed on the shape of a line is a claim about the file

The dictionary parse took a fast path for an alternate line: `AARON(2)` names a word the map
already holds, so the rewrite looked the key up with `get_mut` instead of allocating one, and
left every other line to insert. That matches the shipped file, where a word appears once
with indexed alternates, and it was wrong for the same word written twice with no index —
the second line replaced the first pronunciation instead of appending, which is what the
parser had always done. Every unit test stayed green because none of them writes a word
twice; the dump of every lookup on a generated corpus, compared against the old tree, found
it on its duplicate line. A rule inferred from how the data happens to be shaped is not the
format's rule, and the test that pins it has to be written for the rule.
