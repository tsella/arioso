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

Before the rules landed, the same command wrote a shorter file and dropped both words.
The reading of "zorbansky" is a guess, and a dictionary entry replaces it with the
correct one.

`doctor` makes the same distinction for every entry, so a typo in a respelling does not
hide behind the rules:

```console
$ ARIOSO_TTS_DICTIONARY=/tmp/dict-lts.json arioso doctor
dictionary  4 entries from /tmp/dict-lts.json
            1 entr(y/ies) spoken by spelling rules: xyzzy
            every entry can be said by en_US-lessac-medium
ok
```

## 11. Models

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
