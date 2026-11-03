#!/usr/bin/env python3
"""arioso benchmark harness.

Measures the cost of the shapes a caller actually pays for, so that numbers from two
machines can be compared: synthesis, transcription, server start and throughput, a
streaming session, and two micro measurements.

Run `python3 bench/run.py --list` for the test names, and read `bench/README.md` for what
each one means and what the harness deliberately does not measure.

Standard library only, on purpose: a reporter should not need a build step, a package, or
a second toolchain to contribute a number.
"""

from __future__ import annotations

import argparse
import base64
import csv
import json
import os
import platform
import re
import select
import shutil
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time
import wave

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_AUDIO = os.path.join(HERE, "audio", "bench-60s.wav")
DEFAULT_CSV = os.path.join(HERE, "results.csv")

# One fixed passage, so every reporter synthesizes the same words. Public domain, from
# the opening of The Time Machine (1895), and the same text the audio clip was made from.
SAY_TEXT = (
    "The Time Traveller (for so it will be convenient to speak of him) was expounding a "
    "recondite matter to us. His grey eyes shone and twinkled, and his usually pale face "
    "was flushed and animated. The fire burnt brightly, and the soft radiance of the "
    "incandescent lights in the lilies of silver caught the bubbles that flashed and "
    "passed in our glasses. Our chairs, being his patents, embraced and caressed us "
    "rather than submitted to be sat upon, and there was that luxurious after-dinner "
    "atmosphere when thought runs gracefully free of the trammels of precision."
)



# The ids a run with no --voice and no --model will load. Only used to warn before a test
# fails inside the engine; the binary decides what it actually loads.
DEFAULT_VOICE_ID = "piper-en_US-joe-medium"
DEFAULT_STT_ID = "sherpa-parakeet-tdt-0.6b-v2-en"

COLUMNS = [
    "date",
    "arioso_version",
    "reporter",
    "os",
    "cpu",
    "cores",
    "memory_gb",
    "gpu",
    "binary",
    "voice",
    "stt_model",
    "repeats",
    "say_wall_s",
    "say_audio_s",
    "say_rtf",
    "say_rss_mb",
    "stt_wall_s",
    "stt_audio_s",
    "stt_rtf",
    "stt_rss_mb",
    "startup_ms",
    "healthz_rps",
    "stream_first_ms",
    "stream_total_s",
    "stream_rtf",
    "doctor_ms",
]

TESTS = {
    "say": "Synthesis: wall clock, audio seconds, real time factor, peak memory",
    "stt": "Transcription of the shipped clip: wall clock, real time factor, peak memory",
    "server": "Server: start to the first 200, and requests per second on /healthz",
    "stream": "Streaming: a 60 s session over /v1/stt/ws, first partial and total",
    "micro": "Micro: what the binary loads before it answers (doctor)",
}

# Runs one command in a child of its own, so the peak memory is that command's and not the
# largest any earlier child reached. The kernel keeps the high water mark; nothing in
# Python can read it after the fact.
WRAPPER = """
import resource, subprocess, sys, time
marker = sys.argv[1]
t0 = time.perf_counter()
p = subprocess.run(sys.argv[2:], capture_output=True)
wall = time.perf_counter() - t0
try:
    rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
except Exception:
    rss = 0
with open(marker, "w") as handle:
    handle.write("%f %d %d" % (wall, rss, p.returncode))
sys.stdout.buffer.write(p.stdout)
sys.stderr.buffer.write(p.stderr)
"""


class BenchError(Exception):
    """A measurement could not be taken. The message says which and why."""


def measure(argv, timeout=1800):
    """Run a command and return (wall seconds, stdout, stderr, peak MB, exit code)."""
    with tempfile.NamedTemporaryFile(delete=False) as handle:
        marker = handle.name
    try:
        done = subprocess.run(
            [sys.executable, "-c", WRAPPER, marker] + list(argv),
            capture_output=True,
            timeout=timeout,
        )
        with open(marker) as handle:
            wall, rss, code = handle.read().split()
        peak = int(rss)
        # macOS reports bytes, Linux kilobytes.
        peak_mb = peak / (1024 * 1024) if sys.platform == "darwin" else peak / 1024
        return (
            float(wall),
            done.stdout.decode("utf-8", "replace"),
            done.stderr.decode("utf-8", "replace"),
            round(peak_mb, 1),
            int(code),
        )
    finally:
        try:
            os.unlink(marker)
        except OSError:
            pass


def median(values):
    """The middle of a list of numbers, or None when there is nothing to take."""
    numbers = [v for v in values if v is not None]
    return round(statistics.median(numbers), 4) if numbers else None


def audio_seconds(path):
    """The length of a WAV file, in seconds, from its header."""
    with wave.open(path) as handle:
        return handle.getnframes() / handle.getframerate()


def wrote_seconds(stdout):
    """Pull the audio length out of the line `say` prints."""
    match = re.search(r"\((\d+(?:\.\d+)?) s at (\d+) Hz\)", stdout)
    if not match:
        raise BenchError("`say` printed no audio length: " + stdout.strip()[:200])
    return float(match.group(1))


def free_port():
    """A port that is free right now. Racy in principle, fine for one process."""
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def hardware():
    """What the machine is, read from the OS rather than asked of the reporter."""
    system = platform.system()
    cpu = platform.processor() or "unknown"
    memory_gb = None
    if system == "Linux":
        try:
            with open("/proc/cpuinfo") as handle:
                for line in handle:
                    if line.startswith("model name"):
                        cpu = line.split(":", 1)[1].strip()
                        break
            with open("/proc/meminfo") as handle:
                for line in handle:
                    if line.startswith("MemTotal"):
                        memory_gb = round(int(line.split()[1]) / (1024 * 1024))
                        break
        except OSError:
            pass
    elif system == "Darwin":
        cpu = subprocess.run(
            ["sysctl", "-n", "machdep.cpu.brand_string"],
            capture_output=True,
            text=True,
        ).stdout.strip() or cpu
        out = subprocess.run(
            ["sysctl", "-n", "hw.memsize"], capture_output=True, text=True
        ).stdout.strip()
        if out.isdigit():
            memory_gb = round(int(out) / (1024**3))
    gpu = "none"
    if shutil.which("nvidia-smi"):
        out = subprocess.run(
            ["nvidia-smi", "-L"], capture_output=True, text=True
        ).stdout.strip()
        if out:
            gpu = out.splitlines()[0].replace("GPU 0: ", "")
    return {
        "os": f"{system} {platform.release()}",
        "cpu": cpu,
        "cores": os.cpu_count(),
        "memory_gb": memory_gb if memory_gb is not None else "",
        "gpu": gpu,
    }


def reporter_name(given):
    """Who is reporting: the flag, then the CI actor, then the git identity."""
    if given:
        return given
    for key in ("GITHUB_ACTOR", "USER", "USERNAME"):
        if os.environ.get(key):
            return os.environ[key]
    for key in ("github.user", "user.name"):
        out = subprocess.run(
            ["git", "config", "--get", key], capture_output=True, text=True
        ).stdout.strip()
        if out:
            return out
    return ""


class Bench:
    """One run of the harness: the binary, the config, and the measurements."""

    def __init__(self, args):
        self.args = args
        self.binary = os.path.abspath(args.binary)
        self.audio = os.path.abspath(args.audio)
        self.dir = tempfile.mkdtemp(prefix="arioso-bench.")
        self.config = os.path.join(self.dir, "config.json")
        self.server = None
        self.port = None
        self.values = {}

    def check(self):
        """Fail before measuring, not in the middle of a run."""
        if not os.path.isfile(self.binary):
            raise BenchError(
                f"no binary at {self.binary}. Build one with `cargo build --release`"
            )
        if not os.access(self.binary, os.X_OK):
            raise BenchError(f"{self.binary} is not executable")
        if not os.path.isfile(self.audio):
            raise BenchError(f"no audio at {self.audio}")

    def version(self):
        """The version the binary reports, which is what the row is about."""
        out = subprocess.run(
            [self.binary, "--version"], capture_output=True, text=True
        ).stdout.strip()
        return out.replace("arioso ", "") or "unknown"

    def write_config(self, port=None):
        """A config with the models directory and the voice the run will use."""
        document = {
            "models": {"dir": os.path.abspath(self.args.models)},
            "server": {
                "bind": f"127.0.0.1:{port or 10101}",
                "docs": False,
                # The default is 50 requests a second per address with a burst of 100, and
                # every client here is 127.0.0.1: eight of them share one bucket, so the
                # throughput run would measure the limiter and call it the server. The row
                # is about the server.
                "limits": {"requests_per_second": 100_000, "burst": 100_000},
            },
            "log": {"level": "error"},
            "stt": {"partials": True},
        }
        if self.args.voice:
            document["tts"] = {"voice": self.args.voice}
        if self.args.model:
            document.setdefault("stt", {})["batch_model"] = self.args.model
        with open(self.config, "w") as handle:
            json.dump(document, handle, indent=2)

    def installed(self):
        """The catalog ids this models directory reports as installed, and their kind."""
        out = subprocess.run(
            [self.binary, "--config", self.config, "models", "list"],
            capture_output=True,
            text=True,
        ).stdout
        found = {}
        for line in out.splitlines()[1:]:
            fields = line.split()
            if len(fields) >= 3 and fields[-1] == "installed":
                found[fields[0]] = fields[1]
        return found

    def check_models(self):
        """Warn when the shipped default is not on disk, and refuse an explicit miss.

        A model that was never downloaded fails inside sherpa with three lines naming a file
        in the models directory, which a reporter reads as a broken build. The default is
        only a warning: the config may name another model, and refusing a run that would
        have worked is worse than a line on stderr.
        """
        found = self.installed()
        if self.args.model and self.args.model not in found:
            raise BenchError(
                f"{self.args.model} is not installed. Run "
                f"`{self.binary} models download {self.args.model}`"
            )
        if not self.args.model and DEFAULT_STT_ID not in found:
            print(
                f"  warning: {DEFAULT_STT_ID} is not installed in this directory; the "
                f"speech tests will fail unless the config names another model "
                f"(`{os.path.basename(self.binary)} models download {DEFAULT_STT_ID}`)",
                file=sys.stderr,
            )
        if not self.args.voice and DEFAULT_VOICE_ID not in found:
            print(
                f"  warning: {DEFAULT_VOICE_ID} is not installed in this directory; "
                "synthesis will fail unless the config names another voice",
                file=sys.stderr,
            )

    def say(self):
        """Synthesis of one fixed passage, repeated."""
        walls, audio, rss = [], [], []
        for _ in range(self.args.repeats):
            out = os.path.join(self.dir, "say.wav")
            wall, stdout, stderr, peak, code = measure(
                [self.binary, "--config", self.config, "say", "--out", out, SAY_TEXT]
            )
            if code != 0:
                raise BenchError(f"`say` exited {code}: {stderr.strip()[:300]}")
            walls.append(wall)
            audio.append(wrote_seconds(stdout))
            rss.append(peak)
        wall, seconds, peak = median(walls), median(audio), median(rss)
        self.values.update(
            {
                "say_wall_s": wall,
                "say_audio_s": seconds,
                "say_rtf": round(wall / seconds, 4) if seconds else None,
                "say_rss_mb": peak,
            }
        )

    def stt(self):
        """Transcription of the shipped clip, repeated."""
        walls, rss, seconds = [], [], []
        for _ in range(self.args.repeats):
            wall, stdout, stderr, peak, code = measure(
                [self.binary, "--config", self.config, "transcribe", "--json", self.audio]
            )
            if code != 0:
                raise BenchError(f"`transcribe` exited {code}: {stderr.strip()[:300]}")
            try:
                body = json.loads(stdout)
            except json.JSONDecodeError:
                raise BenchError("`transcribe --json` printed no JSON: " + stdout[:200])
            walls.append(wall)
            seconds.append(float(body["seconds"]))
            rss.append(peak)
        wall, audio, peak = median(walls), median(seconds), median(rss)
        self.values.update(
            {
                "stt_wall_s": wall,
                "stt_audio_s": audio,
                "stt_rtf": round(wall / audio, 4) if audio else None,
                "stt_rss_mb": peak,
            }
        )

    def start_server(self):
        """Start a server on a free port and wait for the first 200 on /healthz."""
        self.port = free_port()
        self.write_config(self.port)
        log = open(os.path.join(self.dir, "server.log"), "wb")
        started = time.perf_counter()
        self.server = subprocess.Popen(
            [self.binary, "--config", self.config, "serve"],
            stdout=log,
            stderr=log,
        )
        deadline = started + 60
        while time.perf_counter() < deadline:
            if self.server.poll() is not None:
                with open(os.path.join(self.dir, "server.log"), "rb") as handle:
                    tail = handle.read().decode("utf-8", "replace")[-400:]
                raise BenchError(f"the server exited before answering: {tail.strip()}")
            try:
                status, _ = http_get(self.port, "/healthz")
                if status == 200:
                    return round((time.perf_counter() - started) * 1000, 1)
            except OSError:
                pass
            time.sleep(0.01)
        raise BenchError("the server did not answer /healthz within 60 s")

    def server_test(self):
        """Server start, then throughput on the cheapest route, repeated."""
        startups, rates = [], []
        for _ in range(self.args.repeats):
            startup = self.start_server()
            try:
                counts = []
                lock = threading.Lock()

                def worker():
                    # Keep-alive, because the number wanted is the cost of a request and
                    # not the cost of a TCP handshake: a fresh connection per request
                    # measured 600 rps where the same server answers several thousand.
                    client = Client(self.port)
                    done = 0
                    try:
                        for _ in range(self.args.requests):
                            if client.get("/healthz")[0] == 200:
                                done += 1
                    finally:
                        client.close()
                    with lock:
                        counts.append(done)

                started = time.perf_counter()
                threads = [
                    threading.Thread(target=worker) for _ in range(self.args.clients)
                ]
                for thread in threads:
                    thread.start()
                for thread in threads:
                    thread.join()
                elapsed = time.perf_counter() - started
                total = sum(counts)
                if total == 0:
                    raise BenchError("no request on /healthz was answered 200")
                startups.append(startup)
                rates.append(total / elapsed)
            finally:
                self.stop_server()
        if not rates:
            raise BenchError("no throughput run finished")
        self.values.update(
            {
                "startup_ms": median(startups),
                "healthz_rps": round(statistics.median(rates)),
            }
        )

    def stop_server(self):
        """Stop the server the harness started."""
        if self.server and self.server.poll() is None:
            self.server.terminate()
            try:
                self.server.wait(15)
            except subprocess.TimeoutExpired:
                self.server.kill()
                self.server.wait(5)

    def stream_test(self):
        """Two sessions: how soon a partial arrives, and how long the whole clip takes.

        They are separate because they need opposite clients. A partial is a response to
        audio arriving at the rate a microphone produces it; the total is what the machine
        costs, which only shows when the audio is sent as fast as the socket takes it. One
        session cannot be both, and blasting the clip starves every partial.
        """
        with wave.open(self.audio) as handle:
            pcm = handle.readframes(handle.getnframes())
        rate = 16000
        chunk = 640  # 20 ms of 16 kHz mono s16le

        # Responsiveness: four seconds of audio at real time.
        if self.server is None or self.server.poll() is not None:
            self.start_server()
        first = None
        try:
            session = self._open_session()
            try:
                started = time.perf_counter()
                paced = 200 * chunk  # 4 s
                for offset in range(0, min(paced, len(pcm)), chunk):
                    session.send_binary(pcm[offset : offset + chunk])
                    for frame in session.drain():
                        if first is None and frame.get("type") == "partial":
                            first = (time.perf_counter() - started) * 1000
                    if first is not None:
                        break
                    time.sleep(0.02)
                session.send(json.dumps({"type": "stop"}))
                while True:
                    frame = session.next_json()
                    if frame is None or frame.get("type") == "done":
                        break
            finally:
                session.close()
        finally:
            self.stop_server()

        # Throughput: the whole clip as fast as the socket takes it.
        totals = []
        for _ in range(self.args.repeats):
            if self.server is None or self.server.poll() is not None:
                self.start_server()
            try:
                session = self._open_session()
                try:
                    started = time.perf_counter()
                    for offset in range(0, len(pcm), chunk):
                        session.send_binary(pcm[offset : offset + chunk])
                    session.send(json.dumps({"type": "stop"}))
                    while True:
                        frame = session.next_json()
                        if frame is None:
                            raise BenchError("the session ended without a done frame")
                        if frame.get("type") == "done":
                            break
                        if frame.get("type") == "error":
                            raise BenchError(
                                "the session answered an error: "
                                + json.dumps(frame)[:300]
                            )
                    totals.append(time.perf_counter() - started)
                finally:
                    session.close()
            finally:
                self.stop_server()

        seconds = audio_seconds(self.audio)
        if not totals:
            raise BenchError("no streaming session finished")
        total = median(totals)
        self.values.update(
            {
                "stream_first_ms": round(first, 1) if first else None,
                "stream_total_s": round(total, 3),
                "stream_rtf": round(total / seconds, 4),
            }
        )

    def _open_session(self):
        """A streaming session that has answered `ready`."""
        session = Ws(self.port, "/v1/stt/ws")
        session.send(json.dumps({"type": "start", "sample_rate": 16000}))
        if session.wait_for("ready") is None:
            session.close()
            raise BenchError("the session sent no ready frame")
        return session

    def micro_test(self):
        """What the binary loads before it can answer: config, manifest, voice config.

        Normalization, VAD framing and the reverse dictionary are all microseconds of work
        behind a seventy millisecond process start, so a harness that spawns a process
        cannot see them. They are measured from inside the crates and recorded in
        BENCHMARK.md, not here.
        """
        walls = []
        for _ in range(self.args.repeats):
            wall, _, stderr, _, code = measure(
                [self.binary, "--config", self.config, "doctor"]
            )
            if code != 0:
                raise BenchError(f"`doctor` exited {code}: {stderr.strip()[:300]}")
            walls.append(wall * 1000)
        self.values.update({"doctor_ms": median(walls)})

    def run(self, names):
        """Run the chosen tests in the order given."""
        self.check()
        self.write_config()
        self.check_models()
        steps = {
            "say": self.say,
            "stt": self.stt,
            "server": self.server_test,
            "stream": self.stream_test,
            "micro": self.micro_test,
        }
        for name in names:
            print(f"  {name} ...", end="", flush=True)
            started = time.perf_counter()
            steps[name]()
            print(f" {time.perf_counter() - started:.1f} s")

    def row(self):
        """One CSV row: the machine, the run, and every measurement taken."""
        machine = hardware()
        row = {
            "date": self.args.date,
            "arioso_version": self.version(),
            "reporter": reporter_name(self.args.reporter),
            "binary": "release" if os.sep + "release" + os.sep in self.binary else "debug",
            "voice": self.args.voice or "default",
            "stt_model": self.args.model or "default",
            "repeats": self.args.repeats,
        }
        row.update(machine)
        # A detection that reads the wrong line, which a VM or a container does, is
        # correctable without editing the row by hand.
        for key, given in (
            ("cpu", self.args.cpu),
            ("memory_gb", self.args.memory_gb),
            ("gpu", self.args.gpu),
        ):
            if given:
                row[key] = given
        row.update(self.values)
        return {key: ("" if row.get(key) is None else row.get(key)) for key in COLUMNS}

    def cleanup(self):
        """Remove the scratch directory the run used."""
        shutil.rmtree(self.dir, ignore_errors=True)


def http_get(port, path):
    """One GET over a fresh connection, returning the status and the body."""
    with socket.create_connection(("127.0.0.1", port), timeout=10) as sock:
        sock.sendall(
            f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
            "Connection: close\r\n\r\n".encode()
        )
        chunks = []
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                break
            chunks.append(chunk)
    head, _, body = b"".join(chunks).partition(b"\r\n\r\n")
    line = head.split(b"\r\n")[0].decode("latin-1")
    return int(line.split()[1]), body


class Client:
    """One keep-alive HTTP connection, enough for a throughput run."""

    def __init__(self, port):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=30)
        self.port = port
        self.buffer = b""

    def get(self, path):
        """Send one request and read its response, returning (status, body)."""
        self.sock.sendall(
            f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{self.port}\r\n\r\n".encode()
        )
        while b"\r\n\r\n" not in self.buffer:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise OSError("the connection closed mid-response")
            self.buffer += chunk
        head, _, rest = self.buffer.partition(b"\r\n\r\n")
        self.buffer = rest
        lines = head.split(b"\r\n")
        status = int(lines[0].split()[1])
        length = 0
        for line in lines[1:]:
            if line.lower().startswith(b"content-length:"):
                length = int(line.split(b":", 1)[1])
        while len(self.buffer) < length:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise OSError("the connection closed mid-body")
            self.buffer += chunk
        body, self.buffer = self.buffer[:length], self.buffer[length:]
        return status, body

    def close(self):
        """Close the connection."""
        try:
            self.sock.close()
        except OSError:
            pass


class Ws:
    """A WebSocket client, small enough to live in one file and standard enough to work.

    Only what the streaming route needs: a masked client frame out, unmasked server frames
    in, text and binary both accepted.
    """

    def __init__(self, port, path):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=120)
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: 127.0.0.1:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(request.encode())
        self.buffer = b""
        while b"\r\n\r\n" not in self.buffer:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise BenchError("the websocket handshake was closed")
            self.buffer += chunk
        head, _, self.buffer = self.buffer.partition(b"\r\n\r\n")
        if b" 101 " not in head.split(b"\r\n")[0]:
            raise BenchError(
                "the websocket handshake was refused: " + head[:120].decode("latin-1")
            )

    def _send_frame(self, opcode, payload):
        """Send one masked frame, which is what a client must do."""
        mask = os.urandom(4)
        header = bytearray([0x80 | opcode])
        length = len(payload)
        if length < 126:
            header.append(0x80 | length)
        elif length < 65536:
            header.append(0x80 | 126)
            header += struct.pack(">H", length)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", length)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self.sock.sendall(bytes(header) + mask + masked)

    def send(self, text):
        """Send one text frame."""
        self._send_frame(0x1, text.encode())

    def send_binary(self, data):
        """Send one binary frame."""
        self._send_frame(0x2, data)

    def _parse_buffered(self):
        """One frame out of the buffer, or None when it does not hold a whole one."""
        if len(self.buffer) < 2:
            return None
        opcode = self.buffer[0] & 0x0F
        length = self.buffer[1] & 0x7F
        offset = 2
        if length == 126:
            if len(self.buffer) < 4:
                return None
            length = struct.unpack(">H", self.buffer[2:4])[0]
            offset = 4
        elif length == 127:
            if len(self.buffer) < 10:
                return None
            length = struct.unpack(">Q", self.buffer[2:10])[0]
            offset = 10
        if len(self.buffer) < offset + length:
            return None
        payload = self.buffer[offset : offset + length]
        self.buffer = self.buffer[offset + length :]
        return opcode, payload

    def _fill(self):
        """Read more bytes into the buffer, and say whether any arrived."""
        chunk = self.sock.recv(65536)
        if not chunk:
            return False
        self.buffer += chunk
        return True

    def next_frame(self):
        """The next frame as (opcode, payload), or None when the socket closed."""
        while True:
            frame = self._parse_buffered()
            if frame is not None:
                opcode, payload = frame
                if opcode == 0x8:
                    return None
                if opcode in (0x9, 0xA):
                    continue
                return opcode, payload
            if not self._fill():
                return None

    def drain(self):
        """Read whatever has already arrived, without waiting for more."""
        frames = []
        while select.select([self.sock], [], [], 0)[0]:
            if not self._fill():
                return frames
            while True:
                frame = self._parse_buffered()
                if frame is None:
                    break
                opcode, payload = frame
                if opcode == 0x1:
                    try:
                        frames.append(json.loads(payload))
                    except json.JSONDecodeError:
                        raise BenchError("the session sent a frame that is not JSON")
        return frames

    def next_json(self):
        """The next text frame as a dict, skipping binary frames."""
        while True:
            frame = self.next_frame()
            if frame is None:
                return None
            opcode, payload = frame
            if opcode == 0x1:
                try:
                    return json.loads(payload)
                except json.JSONDecodeError:
                    raise BenchError("the session sent a frame that is not JSON")

    def wait_for(self, kind):
        """Read frames until one has this type, and return it."""
        while True:
            frame = self.next_json()
            if frame is None:
                return None
            if frame.get("type") == kind:
                return frame

    def close(self):
        """Close the socket."""
        try:
            self.sock.close()
        except OSError:
            pass


def print_table(row):
    """The run as a table a human reads."""
    labels = {
        "say_wall_s": ("synthesis wall clock", "s"),
        "say_audio_s": ("synthesis audio length", "s"),
        "say_rtf": ("synthesis real time factor", ""),
        "say_rss_mb": ("synthesis peak memory", "MB"),
        "stt_wall_s": ("transcription wall clock", "s"),
        "stt_audio_s": ("transcription audio length", "s"),
        "stt_rtf": ("transcription real time factor", ""),
        "stt_rss_mb": ("transcription peak memory", "MB"),
        "startup_ms": ("server start to first 200", "ms"),
        "healthz_rps": ("requests per second on /healthz", ""),
        "stream_first_ms": ("streaming, first partial", "ms"),
        "stream_total_s": ("streaming, whole session", "s"),
        "stream_rtf": ("streaming real time factor", ""),
        "doctor_ms": ("doctor, config, manifest and voice config", "ms"),
    }
    width = max(len(name) for name, _ in labels.values())
    print()
    print(f"arioso {row['arioso_version']}  {row['os']}  {row['cpu']}")
    print(
        f"{row['cores']} cores, {row['memory_gb']} GB, gpu {row['gpu']}, "
        f"{row['binary']} build, median of {row['repeats']}"
    )
    print(f"voice {row['voice']}, speech model {row['stt_model']}")
    print()
    for key, (name, unit) in labels.items():
        value = row.get(key, "")
        if value == "":
            continue
        print(f"  {name.ljust(width)}  {str(value).rjust(10)} {unit}")
    print()


def write_csv(path, row):
    """Append one row, writing the header first if the file is new."""
    new = not os.path.exists(path) or os.path.getsize(path) == 0
    with open(path, "a", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=COLUMNS)
        if new:
            writer.writeheader()
        writer.writerow(row)


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Measure arioso on this machine.",
        epilog="Run `--list` for the tests. Read bench/README.md before reporting.",
    )
    parser.add_argument("tests", nargs="*", help="tests to run; all of them by default")
    parser.add_argument("--list", action="store_true", help="name the tests and stop")
    parser.add_argument(
        "--binary",
        default=os.path.join("target", "release", "arioso"),
        help="the binary to measure",
    )
    parser.add_argument(
        "--models",
        default=os.environ.get("ARIOSO_MODELS_DIR", ""),
        help="models directory, or set ARIOSO_MODELS_DIR",
    )
    parser.add_argument("--audio", default=DEFAULT_AUDIO, help="audio for the speech tests")
    parser.add_argument("--voice", default="", help="voice id, e.g. en_US-joe-medium")
    parser.add_argument("--model", default="", help="speech model id")
    parser.add_argument("--repeats", type=int, default=3, help="runs per measurement")
    parser.add_argument("--clients", type=int, default=8, help="concurrent clients")
    parser.add_argument(
        "--requests",
        type=int,
        default=1000,
        help="requests per client; below about 500 the thread start is the measurement",
    )
    parser.add_argument("--reporter", default="", help="your github user name")
    parser.add_argument("--cpu", default="", help="override the detected CPU name")
    parser.add_argument("--memory-gb", default="", help="override the detected memory")
    parser.add_argument("--gpu", default="", help="override the detected GPU, or `none`")
    parser.add_argument(
        "--date", default=time.strftime("%Y-%m-%d", time.gmtime()), help="run date, UTC"
    )
    parser.add_argument("--json", default="", help="write the run as JSON to this path")
    parser.add_argument("--csv", default="", help="append the run to this CSV path")
    parser.add_argument(
        "--write",
        action="store_true",
        help=f"append the run to {os.path.basename(DEFAULT_CSV)}",
    )
    args = parser.parse_args(argv)

    if args.list:
        for name, meaning in TESTS.items():
            print(f"{name.ljust(8)} {meaning}")
        return 0

    names = args.tests or list(TESTS)
    unknown = [name for name in names if name not in TESTS]
    if unknown:
        parser.error("unknown test(s): " + ", ".join(unknown) + "; try --list")
    if args.repeats < 1:
        parser.error("--repeats must be at least 1")
    if not args.models:
        parser.error("--models or ARIOSO_MODELS_DIR is required")
    if not os.path.isdir(args.models):
        parser.error(f"models directory {args.models} does not exist")

    bench = Bench(args)
    try:
        print(f"arioso bench: {', '.join(names)}")
        bench.run(names)
        row = bench.row()
        print_table(row)
        if args.json:
            with open(args.json, "w") as handle:
                json.dump(row, handle, indent=2)
            print(f"wrote {args.json}")
        target = DEFAULT_CSV if args.write else args.csv
        if target:
            if not row["reporter"]:
                raise BenchError(
                    "no reporter: pass --reporter <github-user> so the row has an owner"
                )
            write_csv(target, row)
            print(f"appended to {target}")
        return 0
    except BenchError as error:
        print(f"bench failed: {error}", file=sys.stderr)
        return 1
    finally:
        bench.stop_server()
        bench.cleanup()


if __name__ == "__main__":
    sys.exit(main())
