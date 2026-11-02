# Examples

Every block below is a real run on Linux, x86-64, four cores, no GPU, with the
`en_US-lessac-medium` voice. The output is copied from the terminal, not retyped.
Paths such as `/tmp/arioso-say.Uk3iWZ` are the model directory used during the capture.

## 1. Speak a sentence

```console
$ arioso say "hello world. this is arioso speaking locally." --out hello.wav
wrote hello.wav (2.11 s at 22050 Hz)
1 word(s) not in the dictionary and skipped: arioso
```

The WAV is 22050 Hz, 16-bit, mono. "arioso" is not in the CMU dictionary, so it is
skipped and named rather than guessed at. Section 5 fixes that.

The length moves by a few hundredths between runs: the model samples noise, so a second
run of the same sentence gave 2.02 s rather than 2.11 s. Treat these durations as close,
not exact.

## 2. Change the rate

The same sentence at three rates. `tts.speed` multiplies the voice's own length scale.

```console
$ ARIOSO_TTS_SPEED=0.5 arioso say "hello world. this is arioso speaking locally." --out slow.wav
wrote slow.wav (3.89 s at 22050 Hz)

$ arioso say "hello world. this is arioso speaking locally." --out normal.wav
wrote normal.wav (2.21 s at 22050 Hz)

$ ARIOSO_TTS_SPEED=2.0 arioso say "hello world. this is arioso speaking locally." --out fast.wav
wrote fast.wav (1.36 s at 22050 Hz)
```

A value outside 0.5 to 2.0, or one that is not a number, fails before any synthesis:

```console
$ ARIOSO_TTS_SPEED=fast arioso say "hi" --out x.wav
Error: loading config from /home/you/.local/share/arioso/config.json

Caused by:
    invalid config at ARIOSO_TTS_SPEED: invalid float literal
```

## 3. See what the voice will say

`normalize` prints the expansion and writes no audio. Use it when a number or an
abbreviation sounds wrong.

```console
$ arioso normalize 'It costs $3.99 on the 3rd at 14:30, about 12.5% more. Dr. Smith paid $1,234 in 2026.'
It costs three dollars and ninety nine cents on the third at two thirty in the afternoon, about twelve point five percent more. Doctor Smith paid one thousand two hundred thirty four dollars in twenty twenty six.
```

Four digits with no separator are read as a year; `1,234` keeps its thousands separator
and stays a quantity.

## 4. Times and the part of the day

`tts.time_of_day` is on by default. The hour as written decides the period, and a written
`am` or `pm` wins over that.

```console
$ arioso normalize 'the train leaves at 19:45, the meeting is 7:45 pm'
the train leaves at seven forty five in the evening, the meeting is seven forty five in the evening
```

Turn it off and the hour stands alone:

```console
$ ARIOSO_TTS_TIME_OF_DAY=0 arioso normalize 'the train leaves at 19:45, the meeting is 7:45 pm'
the train leaves at seven forty five, the meeting is seven forty five
```

With the option off, `19:45` and `07:45` are indistinguishable. With it on, a bare `7:45`
is morning, `12:30` is afternoon, and `19:45` is evening.

## 5. Teach it a name

Write `dictionary.json` beside the config file, or point `ARIOSO_TTS_DICTIONARY` at it.

```json
{
  "version": 1,
  "entries": [
    { "term": "arioso", "spoken": ["ah ree oh so"] },
    { "term": "Tom Sella", "spoken": ["tom seller"] },
    { "term": "Sella", "phonemes": ["S EH1 L AA0"] }
  ]
}
```

```console
$ ARIOSO_TTS_DICTIONARY=/tmp/examples-dict.json arioso doctor
config      ~/.local/share/arioso/config.json
  valid
models dir  /tmp/arioso-say.Uk3iWZ (writable)
manifest    1 models, 1 installed
dictionary  3 entries from /tmp/examples-dict.json
            every entry can be said by en_US-lessac-medium
ok
```

With no `ARIOSO_TTS_DICTIONARY` and no `tts.dictionary`, the same file is read from
`dictionary.json` beside the config file.

Now the sentence from section 1 says every word:

```console
$ ARIOSO_TTS_DICTIONARY=/tmp/examples-dict.json arioso say "arioso is Tom Sella, and it costs \$3.99." --out names.wav
wrote names.wav (4.27 s at 22050 Hz)
```

No warning. The term `Tom Sella` wins over the shorter `Sella` entry, because the longest
phrase is tried first.

## 6. Catch an entry the voice cannot say

Add a nonsense respelling and `doctor` names it instead of letting it pass:

```console
$ arioso doctor
dictionary  3 entries from /tmp/dictionary2.json
            1 entr(y/ies) this voice cannot say: xyzzy
Error: 1 problem(s) found
```

At speech time the entry falls back to the term itself, so the sentence still plays. The
warning is the signal, not a crash.

## 7. Serve it

```console
$ arioso serve
listening on http://127.0.0.1:10101
```

Health and readiness. Readiness names the voice, whether it is installed, and the state of
the dictionary:

```console
$ curl -s -w '\nHTTP %{http_code}\n' localhost:10101/healthz
ok
HTTP 200

$ curl -s localhost:10101/readyz | jq -c .
{"dictionary":{"entries":0,"error":null},"models_installed":1,"status":"ready","voice":"piper-en_US-lessac-medium","voice_installed":true,"voice_loaded":false}

$ curl -s localhost:10101/v1/models | jq -c .
{"models":[{"bytes":63206179,"engine":"piper","id":"piper-en_US-lessac-medium","installed":true,"kind":"tts","license":"Blizzard-2013"}]}
```

One request, one WAV:

```console
$ curl -s -o out.wav -w 'HTTP %{http_code} %{content_type} %{size_download} bytes\n' \
    -X POST localhost:10101/v1/tts -H 'content-type: application/json' -d '{"text":"hello world"}'
HTTP 200 audio/wav 39980 bytes
```

Bad input is a problem detail, not a stack trace:

```console
$ curl -s -w '\nHTTP %{http_code}\n' -X POST localhost:10101/v1/tts \
    -H 'content-type: application/json' -d '{"text":"  "}'
{"detail":"text must not be empty","status":422,"title":"Unprocessable Entity","type":"about:blank"}
HTTP 422

$ curl -s -o /dev/null -w 'HTTP %{http_code}\n' localhost:10101/nope
HTTP 404
```

## 8. Stream it

Send the request as the first text message. Read a `start` frame, one binary frame per
sentence, and an `end` frame. This client is 20 lines and needs only `websockets`:

```python
import asyncio, json
import websockets

async def main():
    async with websockets.connect("ws://127.0.0.1:10101/v1/tts/ws") as ws:
        await ws.send(json.dumps({"text": "hello world. this is arioso streaming."}))
        total = 0
        async for frame in ws:
            if isinstance(frame, bytes):
                total += len(frame)
                print(f"audio  {len(frame)} bytes")
            else:
                message = json.loads(frame)
                print(f"{message['type']:<5}  {frame}")
                if message["type"] == "end":
                    print(f"total  {total} bytes of pcm_s16le")
                    break

asyncio.run(main())
```

```console
$ python3 ws_demo.py
start  {"format":"pcm_s16le","sample_rate":22050,"type":"start"}
audio  36864 bytes
audio  47616 bytes
end    {"chunks":2,"samples":42240,"type":"end"}
total  84480 bytes of pcm_s16le
```

Two sentences, two frames. The first frame is sent while the second is still being
synthesized, which is the reason for the bounded queue between them.

A client that connects and sends nothing is dropped after `server.first_message_timeout_ms`,
ten seconds by default, with `{"type":"error","detail":"no request within 10000 ms"}`.

## 9. A config file of your own

```json
{
  "schema_version": 1,
  "server": {
    "bind": "127.0.0.1:10101",
    "first_message_timeout_ms": 10000
  },
  "tts": {
    "enabled": true,
    "engine": "piper",
    "voice": "en_US-lessac-medium",
    "speed": 1.0,
    "format": "wav",
    "max_chars": 4000,
    "time_of_day": true,
    "dictionary": null
  }
}
```

Check it before you use it:

```console
$ arioso --config /tmp/example-config.json doctor
config      /tmp/example-config.json
  valid
models dir  /tmp/arioso-say.Uk3iWZ (writable)
manifest    1 models, 1 installed
dictionary  2 entries from /tmp/dictionary.json
            every entry can be said by en_US-lessac-medium
ok
```

`dictionary` is `null`, so the dictionary is looked for beside the config file, which is
why it found `/tmp/dictionary.json`. Set the field to an absolute path to keep the file
somewhere else.

A broken field names its own path:

```console
$ arioso --config broken.json doctor
Error: loading config from broken.json

Caused by:
    invalid config at server.bind: invalid socket address syntax
```

## 10. Words the dictionary does not hold

A name outside CMUdict is pronounced by spelling rule, and the words are named:

```console
$ arioso say "hello world. this is arioso speaking locally. zorbansky wrote it." --out spoken.wav
wrote spoken.wav (4.01 s at 22050 Hz)
2 word(s) not in the dictionary, spoken by spelling rules: arioso, zorbansky
```

Before the rules landed, both words were dropped from the sentence: the same command
reported them as skipped and said nothing in their place. The reading of "zorbansky" is a
guess, and a dictionary entry replaces it with the correct one.

`doctor` makes the same distinction for every entry, so a typo in a respelling does not
hide behind the rules:

```console
$ ARIOSO_TTS_DICTIONARY=/tmp/dict-lts.json arioso doctor
dictionary  4 entries from /tmp/dict-lts.json
            1 entr(y/ies) spoken by spelling rules: xyzzy
            every entry can be said by en_US-lessac-medium
ok
```

## 11. Read a file back

Synthesize a sentence, then transcribe it. This is the whole loop in two commands.

```console
$ arioso say "the quick brown fox jumps over the lazy dog" --out round.wav
wrote round.wav (2.67 s at 22050 Hz)

$ arioso transcribe round.wav
The quick brown fox jumps over the lazy dog.

$ arioso transcribe round.wav --json
{"seconds":2.67,"text":"The quick brown fox jumps over the lazy dog."}
```

The engine takes the rate from the file, so a 22.05 kHz voice recording needs no
conversion. It took 1.58 s of wall clock to transcribe 2.67 s of audio, model load
included, on four cores with no GPU.

The catalog now holds both models:

```console
$ arioso models list
id                                 kind        size  license    state
piper-en_US-lessac-medium          tts     60.3 MiB  Blizzard-2013 installed
sherpa-whisper-tiny-en             stt     98.8 MiB  MIT        installed
```

### The dictionary on the way back

The reverse map compares sounds, not letters, because the engine never reproduces a
respelling exactly. Measured, both directions synthesized by arioso, with a dictionary
entry for the name:

```console
$ arioso say "Tom Sella built arioso for his own machine." --out dict.wav
$ arioso transcribe dict.wav
Tom Sella built arioso for his own machine.
```

The engine heard "Tom Sella built a Rio so" and the entry put "arioso" back. The same
recording through whisper tiny, which is not the default, gives "Tonseller built REO so"
and the matcher then swallows the word "built": that is why the default is base.

## 12. Comparing voices

The same sentence through each voice, with `ARIOSO_TTS_VOICE`:

```console
$ for v in en_US-kathleen-low en_US-joe-medium en_GB-alba-medium en_US-lessac-medium en_US-ljspeech-high; do
    ARIOSO_TTS_VOICE=$v arioso say "The quick brown fox jumps over the lazy dog while the five boxing wizards jump quickly" --out $v.wav
  done
```

| voice | wall clock | audio | real time factor | read back by whisper |
| --- | --- | --- | --- | --- |
| `en_US-kathleen-low` | 2.40 s | 5.38 s | 0.45 | exact |
| `en_US-joe-medium` | 2.47 s | 4.92 s | 0.50 | exact |
| `en_GB-alba-medium` | 2.47 s | 4.83 s | 0.51 | "the quick brand fox ... the laser dork" |
| `en_US-lessac-medium` | 2.53 s | 5.03 s | 0.50 | exact |
| `en_US-ljspeech-high` | 5.77 s | 5.83 s | 0.99 | "the lazy door ... five barksing wizards" |

The last column is a rough intelligibility check, not a quality score: whisper tiny.en is
an American English model, so the Scottish voice scores badly partly because the reader is
wrong for it. The timing columns are the honest part.

The high tier is the surprise: twice the time of medium, a 108.9 MiB file against 60.3,
and nothing here shows it is better.

## 13. Streaming a microphone

`/v1/stt/ws` takes raw PCM and answers as it goes. This client streams a file in real
time; a microphone does the same thing from a capture callback.

```python
import asyncio, json, websockets

async def main():
    audio = open("hello.pcm", "rb").read()      # 16 kHz mono s16le
    async with websockets.connect("ws://127.0.0.1:10101/v1/stt/ws") as socket:
        await socket.send(json.dumps({"type": "start", "sample_rate": 16000}))
        print(await socket.recv())               # ready
        for index in range(0, len(audio), 3200): # 100 ms frames
            await socket.send(audio[index:index + 3200])
            await asyncio.sleep(0.1)
        await socket.send(json.dumps({"type": "cancel"}))

asyncio.run(main())
```

A session with 1 s of silence, 2.65 s of speech, then silence again, printed with the
wall clock at each frame:

```text
 0.02s <- {"session":"6ae84a9600bb","sources":["mic"],"type":"ready"}
 2.67s <- {"t":0.64,"text":"The quick brush","type":"partial"}
 3.24s <- {"t":1.84,"text":"The quick brown fox jumps over","type":"partial"}
 3.97s <- {"t":2.44,"text":"The quick brown fox jumps over the lazy dog.","type":"partial"}
 4.72s <- {"end":3.56,"start":0.84,"text":"The quick brown fox jumps over the lazy dog.","type":"final"}
```

Four things to read from those numbers. The first partial is late (2.67 s) because that
session paid the model load. Partials are sparser here than the interval suggests: each
one re-runs the whole buffer, and whisper base takes about 0.6 s, so most ticks find the
engine busy and skip. The speech ends at 3.56 s of audio, the detector confirms the
endpoint 500 ms later at 4.06 s, and the final arrives at 4.72 s of wall clock: about
660 ms after the endpoint, against the PRD target of 700. Whisper tiny is faster and
lands at about 340 ms, which is the trade the model choice makes.

Partials are best effort by design. A partial still running when the utterance ends is
dropped rather than delivered after its own final.

## 14. Models

```console
$ arioso models list
id                                 kind        size  license    state
piper-en_US-lessac-medium          tts     60.3 MiB  Blizzard-2013 installed

$ arioso models download piper-en_US-lessac-medium
checking piper-en_US-lessac-medium (63206179 bytes) in /tmp/arioso-say.Uk3iWZ
   100.0%  60.3 MiB / 60.3 MiB
ready: 2 file(s)
```

That run found the model already present: it hashed both files, matched them against the
manifest, and fetched nothing. On a fresh directory the same command downloads, and an
interrupted download resumes from the partial file. SHA-256 is checked before a file is
used, never after. The
license column is there because the default voice is research only; see the license
section of the README before shipping anything.

## 15. The API surface

Start the server, then read what it can do instead of guessing. This run had
`stt.enabled: false`, which is why one engine answers.

```console
$ arioso serve &
$ curl -sS http://127.0.0.1:10101/v1/info
{"version":"0.1.0","target":"x86_64-linux","engines":["piper"]}
```

```console
$ curl -sS http://127.0.0.1:10101/v1/engines
{"engines":[{"id":"piper","kind":"tts","streaming":true,"languages":["en-GB","en-US"],"models":5,"loaded":false}]}
```

```console
$ curl -sS 'http://127.0.0.1:10101/v1/voices?language=en_GB'
{"voices":[{"id":"piper-en_GB-alba-medium","engine":"piper","license":"CC-BY-4.0","commercial_use":true,"sample_rate":22050,"quality":"medium","languages":["en-GB"],"installed":true,"active":false}]}
```

The filter is a prefix and ignores the separator, so `en` finds both `en_US` and `en_GB`,
and `en_GB` and `en-GB` find the same voice.

```console
$ curl -sS -X PATCH http://127.0.0.1:10101/v1/config \
    -H 'content-type: application/json' -d '{"persist":true,"tts":{"speed":1.6}}'
$ curl -sS http://127.0.0.1:10101/v1/config | python3 -c \
    'import json,sys; print(json.load(sys.stdin)["tts"]["speed"])'
1.6
```

Restart the server and the number is still 1.6, because `persist` wrote the file. Without
it the change holds until the process exits.

```console
$ curl -sS -X PATCH http://127.0.0.1:10101/v1/config \
    -H 'content-type: application/json' -d '{"tts":{"engine":"kokoro"}}'
{"code":"invalid_value","detail":"tts.engine kokoro is not built into this binary; the engines here are piper","status":422,"title":"Unprocessable Entity","type":"about:blank"}
```

`/docs` renders the same surface as a page, and `/openapi.json` is the document for a
client generator.

## 16. Watching it work

The server logs to stderr, and the config decides the level and the shape. The default
level is `error`, so a working server says one line and nothing else:

```console
$ arioso serve
listening on http://127.0.0.1:10101
```

Requests arrive, a dictionary is rewritten, and the output above is still the whole of it.
That is the point: a log that prints on every healthy request is a log nobody reads.

Raise the level when you want the timeline. At `info`, a state change is one line:

```json
{ "log": { "level": "info", "format": "pretty" } }
```

```console
$ arioso serve
listening on http://127.0.0.1:10101
2026-11-02T10:13:37.803882Z  INFO arioso_server: reloaded the user dictionary entries=3
```

That line appears when `dictionary.json` changes under a running server: it re-reads the
file, keeps the last good copy if the new one does not parse, and says how many entries it
now holds.

Set `log.format` to `json` for a log collector:

```json
{ "log": { "level": "info", "format": "json" } }
```

```console
$ arioso serve
listening on http://127.0.0.1:10101
{"timestamp":"2026-11-02T10:13:24.537942Z","level":"INFO","fields":{"message":"reloaded the user dictionary","entries":2},"target":"arioso_server"}
```

`RUST_LOG` wins over `log.level` when it is set, so a debugging session needs no config
edit:

```console
$ RUST_LOG=arioso_server=debug,arioso=debug arioso serve
```

Two keys in `log` are accepted and not yet used: `log.file` (write to a file instead of
stderr) and `log.log_text` (include the sentence being synthesized). They parse, so a
config that sets them is valid, and they change nothing. The PRD records them as
unimplemented rather than the config pretending otherwise.

`config.json.example` in the repository root is a complete document with every default in
it. Copy it to the config path and edit what you need:

```console
$ cp config.json.example ~/.local/share/arioso/config.json
```

## 17. Serving over TLS

Set both files and the server speaks HTTPS; set neither and it speaks HTTP as before. One
without the other is refused at startup, because half a pair cannot serve a connection.

```json
{ "server": { "tls": { "cert": "/path/cert.pem", "key": "/path/key.pem" } } }
```

### A pair for your own machine or LAN

No certificate authority is involved, so clients must be told to trust it. This is the
right tool for a laptop, a test box, or a LAN address, and it is not a substitute for a
public certificate.

```console
$ openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout key.pem -out cert.pem -days 365 \
    -subj "/CN=localhost" \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
$ arioso --config config.json serve
listening on https://127.0.0.1:10197
```

```console
$ curl --cacert cert.pem https://localhost:10197/healthz -w ' [%{http_code} h%{http_version}]'
ok [200 h2]
$ curl http://127.0.0.1:10197/healthz
curl: (7) Failed to connect to 127.0.0.1 port 10197
```

The second request fails because the port speaks TLS only: there is no plain listener
beside it, so a client cannot silently downgrade. Both lines above are from a real run.

### A certificate for a public hostname

The commands below are the standard certbot recipe. They are not run by this repository's
tests, because they need a public hostname and a port 80 that Let's Encrypt can reach.

```console
$ sudo certbot certonly --standalone -d tts.example.com
$ sudo chgrp -R staff /etc/letsencrypt/live /etc/letsencrypt/archive
$ sudo chmod -R g+r /etc/letsencrypt/live /etc/letsencrypt/archive
```

Point the config at the two files certbot maintains:

```json
{ "server": { "tls": {
    "cert": "/etc/letsencrypt/live/tts.example.com/fullchain.pem",
    "key":  "/etc/letsencrypt/live/tts.example.com/privkey.pem"
} } }
```

`fullchain.pem`, not `cert.pem`: a client that does not already hold the intermediate
certificate cannot build a chain from the leaf alone. The group and mode commands matter
because certbot writes as root and the server usually does not run as root.

### Renewal

The pair is re-read when either file's modification time changes, so a renewal takes effect
without a restart. A restart would work too, until the one time it is forgotten and the
certificate expires while the service is up.

Measured, with a running server and two generated pairs:

```console
$ echo | openssl s_client -connect 127.0.0.1:10197 2>/dev/null | openssl x509 -noout -subject
subject=CN = first
$ cp cert2.pem cert.pem; cp key2.pem key.pem
$ sleep 40
$ echo | openssl s_client -connect 127.0.0.1:10197 2>/dev/null | openssl x509 -noout -subject
subject=CN = second
```

```console
INFO arioso_server: reloaded the certificate cert=/tmp/arioso-tls/cert.pem
```

The poll runs every 30 seconds. A file caught half-written during a renewal logs a warning
and keeps serving the certificate already in use, then succeeds on the next poll.

### What TLS does not do

It encrypts the connection. It does not authenticate the caller, limit what they ask for,
or stop a flood. Set `server.auth` for the first, `server.limits` for the second, and put a
reverse proxy in front for the third.
