#!/usr/bin/env python3
"""Reproducible performance benchmarks for titi (see docs/research/perf/README.md).

    scripts/bench.py [--quick] [--out results.json] [--skip-build] [--only a,b]

Builds release, runs every suite, prints a markdown table and writes the
numbers as JSON to --out. No third-party Python packages; the driver is the
standard library's pty/os only. Suites (--only takes a comma list):

  startup    `titi --help` wall time and the real binary on a 120x40 PTY: time to
             the first byte and to the first painted frame, CPU and peak RSS
             of a short session (rusage from wait4, the same number
             `/usr/bin/time -l` prints).
  genome     index build, prompt-map render, no-op refresh, one edited file.
  tools      glob / grep / read through the titi-tools API.
  render     markdown render of a long mixed session, per message / per frame.
  streaming  one turn against the scripted provider: first token, total, CPU.
  prompt     request body per turn from the provider's request log: system
             prompt, tool schemas, genome, diff and history, in bytes and
             estimated tokens (bytes/4, titi's own estimator).

The scripted provider (scripts/fake-provider.py) must listen on 127.0.0.1:18999;
if nothing does, one is started for the run and stopped afterwards. Its
requests.jsonl is only read: `--requests` names it (default: the file next to
the provider that was found, else a fresh one).

Timings are wall-clock and noisy on a loaded machine; every row carries n,
min, median and p95, and the machine's load average is recorded.
"""

import argparse
import glob
import json
import os
import platform
import pty
import fcntl
import queue
import re
import select
import shutil
import signal
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import uuid

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PORT = 18999
DEFAULT_REQUESTS = "/tmp/titi-qa/run/requests.jsonl"
CONFIG = """providers:
  - id: fake
    api: openai-completions
    base_url: http://127.0.0.1:{port}/v1
    credential_required: false
models:
  - id: fake/scripted
    provider: fake
    wire_model: fake
    context_window: 32000
modelRoles:
  default: fake/scripted
"""


def log(message):
    print(message, file=sys.stderr, flush=True)


def summarize(samples):
    ordered = sorted(samples)
    p95 = ordered[min(len(ordered) - 1, round((len(ordered) - 1) * 0.95))]
    return {
        "n": len(ordered),
        "min": round(ordered[0], 3),
        "median": round(statistics.median(ordered), 3),
        "p95": round(p95, 3),
    }


def cargo_bin():
    found = shutil.which("cargo")
    if found:
        return found
    pattern = os.path.expanduser("~/.rustup/toolchains/*/bin/cargo")
    candidates = sorted(glob.glob(pattern))
    stable = [c for c in candidates if "stable" in c]
    if stable or candidates:
        return (stable or candidates)[0]
    sys.exit("cargo not found on PATH or under ~/.rustup/toolchains")


def target_dir():
    return os.environ.get("CARGO_TARGET_DIR") or os.path.join(ROOT, "target")


def build(cargo):
    env = dict(os.environ, PATH=os.path.dirname(cargo) + os.pathsep + os.environ["PATH"])
    for argv in (
        ["build", "--release", "--locked", "-p", "titi-cli"],
        ["build", "--release", "--locked", "--examples", "-p", "titi-genome", "-p", "titi-tools", "-p", "titi-tui"],
    ):
        log("cargo " + " ".join(argv))
        subprocess.run([cargo, *argv], cwd=ROOT, env=env, check=True)


def machine():
    def out(*argv):
        try:
            return subprocess.run(argv, capture_output=True, text=True, check=True).stdout.strip()
        except (OSError, subprocess.CalledProcessError):
            return ""

    info = {
        "cpu": out("sysctl", "-n", "machdep.cpu.brand_string") or platform.processor(),
        "cores": os.cpu_count(),
        "memory_gb": None,
        "os": platform.platform(),
        "load_avg": [round(x, 2) for x in os.getloadavg()],
        "git": out("git", "-C", ROOT, "rev-parse", "--short", "HEAD"),
    }
    mem = out("sysctl", "-n", "hw.memsize")
    if mem.isdigit():
        info["memory_gb"] = round(int(mem) / 2**30, 1)
    return info


# ---------------------------------------------------------------- workspace

def make_workspace(base, template_agent):
    """A git repo holding this tree's HEAD (a real, ranked codebase), a
    long.txt, and an agent dir whose config points at the scripted provider."""
    ws = os.path.join(base, "ws")
    os.makedirs(ws)
    archive = subprocess.run(["git", "-C", ROOT, "archive", "HEAD"], capture_output=True, check=True).stdout
    subprocess.run(["tar", "-x", "-C", ws], input=archive, check=True)
    with open(os.path.join(ws, "long.txt"), "w") as handle:
        handle.write("".join(f"{i}\n" for i in range(1, 5001)))
    git = ["git", "-C", ws, "-c", "user.name=bench", "-c", "user.email=bench@example.invalid"]
    subprocess.run(["git", "-C", ws, "init", "-q"], check=True)
    subprocess.run([*git, "add", "-A"], check=True, capture_output=True)
    subprocess.run([*git, "commit", "-qm", "bench"], check=True, capture_output=True)
    agent = os.path.join(base, "agent")
    if template_agent and os.path.isdir(template_agent):
        shutil.copytree(template_agent, agent, ignore=shutil.ignore_patterns("sessions", "traces", "trajectories"))
    else:
        os.makedirs(agent)
    with open(os.path.join(agent, "config.yml"), "w") as handle:
        handle.write(CONFIG.format(port=PORT))
    return ws, agent


def titi_env(base, agent, genome=True):
    env = {"PATH": "/usr/bin:/bin", "TITI_AGENT_DIR": agent, "TERM": "xterm-256color", "HOME": base}
    if not genome:
        env["TITI_NO_GENOME"] = "1"
    return env


# ---------------------------------------------------------------- provider

def provider_up():
    try:
        with socket.create_connection(("127.0.0.1", PORT), timeout=0.5):
            return True
    except OSError:
        return False


class Provider:
    """Uses the provider already on the port, or starts one for the run."""

    def __init__(self, requests_path):
        self.proc = None
        self.requests = requests_path
        if not provider_up():
            self.dir = tempfile.mkdtemp(prefix="titi-bench-provider-")
            script = os.path.join(ROOT, "scripts", "fake-provider.py")
            self.proc = subprocess.Popen(
                [sys.executable, script, str(PORT)], cwd=self.dir, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
            )
            self.requests = os.path.join(self.dir, "requests.jsonl")
            for _ in range(50):
                if provider_up():
                    break
                time.sleep(0.1)
            else:
                sys.exit("could not start scripts/fake-provider.py")

    def size(self):
        try:
            return os.path.getsize(self.requests)
        except OSError:
            return 0

    def close(self):
        if self.proc:
            self.proc.terminate()
            self.proc.wait()
            shutil.rmtree(self.dir, ignore_errors=True)


# ---------------------------------------------------------------- measuring

def rusage_row(ru):
    # ru_maxrss is bytes on macOS and KiB on Linux.
    rss = ru.ru_maxrss * (1 if sys.platform == "darwin" else 1024)
    return {"cpu_ms": (ru.ru_utime + ru.ru_stime) * 1e3, "rss_mb": rss / 2**20}


def run_json_lines(argv, cwd):
    out = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, check=True).stdout
    return [json.loads(line) for line in out.splitlines() if line.startswith("{")]


def bench_help(binary, env, runs):
    wall, cpu, rss = [], [], []
    for _ in range(runs + 1):
        start = time.perf_counter()
        proc = subprocess.Popen([binary, "--help"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        _, _, ru = os.wait4(proc.pid, 0)
        wall.append((time.perf_counter() - start) * 1e3)
        row = rusage_row(ru)
        cpu.append(row["cpu_ms"])
        rss.append(row["rss_mb"])
    # The first run pays for the page cache; it is dropped, not hidden.
    return {"wall_ms": summarize(wall[1:]), "cpu_ms": summarize(cpu[1:]), "rss_mb": summarize(rss[1:])}


def pty_session(binary, ws, env, settle=2.0, marker=b"titi"):
    """Start the TUI on a 120x40 PTY. Returns first byte ms, first frame ms
    (the banner's text arrives), the bytes written while idle for `settle`
    seconds, and the process's CPU and peak RSS once Ctrl+D ended it."""
    rows, cols = 40, 120
    start = time.perf_counter()
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(ws)
        os.execve(binary, [binary], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    seen = bytearray()
    first_byte = first_frame = None
    frame_bytes = 0
    deadline = start + 20
    while time.perf_counter() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.02)
        if not ready:
            if first_frame is not None and time.perf_counter() - start > first_frame / 1e3 + settle:
                break
            continue
        try:
            data = os.read(fd, 65536)
        except OSError:
            break
        if not data:
            break
        now = (time.perf_counter() - start) * 1e3
        if first_byte is None:
            first_byte = now
        seen.extend(data)
        if first_frame is None and marker in seen:
            first_frame = now
        if first_frame is not None:
            frame_bytes += len(data)
    os.write(fd, b"\x04")
    end = time.perf_counter() + 5
    result = None
    while time.perf_counter() < end:
        try:
            got = os.wait4(pid, os.WNOHANG)
        except ChildProcessError:
            break
        if got[0]:
            result = got[2]
            break
        try:
            select.select([fd], [], [], 0.05)
            os.read(fd, 65536)
        except OSError:
            pass
    if result is None:
        os.kill(pid, signal.SIGKILL)
        result = os.wait4(pid, 0)[2]
    os.close(fd)
    row = rusage_row(result)
    return {"first_byte_ms": first_byte, "first_frame_ms": first_frame, "idle_bytes": frame_bytes, **row}


def bench_startup(binary, ws, env, runs):
    out = {"help": bench_help(binary, env, runs)}
    for label, genome in (("tui", True), ("tui_no_genome", False)):
        env2 = dict(env)
        if not genome:
            env2["TITI_NO_GENOME"] = "1"
        samples = [pty_session(binary, ws, env2) for _ in range(runs)]
        out[label] = {
            key: summarize([s[key] for s in samples])
            for key in ("first_byte_ms", "first_frame_ms", "idle_bytes", "cpu_ms", "rss_mb")
            if all(s[key] is not None for s in samples)
        }
    return out


class Headless:
    """`titi --headless` on pipes; events arrive on a reader thread."""

    def __init__(self, binary, ws, env, approval="yolo"):
        self.proc = subprocess.Popen(
            [binary, "--headless", "--approval", approval],
            cwd=ws,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        self.start = time.perf_counter()
        self.events = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for raw in self.proc.stdout:
            self.events.put((time.perf_counter(), raw))
        self.events.put((time.perf_counter(), None))

    def next_event(self, timeout=60):
        at, raw = self.events.get(timeout=timeout)
        if raw is None:
            raise RuntimeError("titi --headless exited early")
        return at, json.loads(raw)

    def ready(self):
        at, event = self.next_event()
        if not event.get("ready"):
            raise RuntimeError(f"unexpected first frame: {event}")
        return (at - self.start) * 1e3

    def turn(self, text):
        """Submit and read to the end of the turn. Returns first-delta and
        total ms, and the number of stream deltas."""
        frame = json.dumps({"v": 1, "command": {"SubmitPrompt": {"text": text}}})
        sent = time.perf_counter()
        self.proc.stdin.write((frame + "\n").encode())
        self.proc.stdin.flush()
        first = None
        deltas = 0
        while True:
            at, event = self.next_event()
            if "StreamDelta" in event:
                deltas += 1
                if first is None:
                    first = (at - sent) * 1e3
            if "TurnFinished" in event or "Failed" in event or "Cancelled" in event:
                if "Failed" in event:
                    raise RuntimeError(f"turn failed: {event}")
                return {"first_ms": first, "total_ms": (at - sent) * 1e3, "deltas": deltas}

    def close(self):
        self.proc.stdin.close()
        try:
            _, _, ru = os.wait4(self.proc.pid, 0)
        except ChildProcessError:
            return {"cpu_ms": 0.0, "rss_mb": 0.0}
        return rusage_row(ru)


def bench_streaming_mode(binary, ws, env, runs):
    # Baseline: start, ready, close: what the process costs before any turn.
    base_cpu, ready = [], []
    for _ in range(runs):
        session = Headless(binary, ws, env)
        ready.append(session.ready())
        base_cpu.append(session.close()["cpu_ms"])
    out = {"ready_ms": summarize(ready), "idle_cpu_ms": summarize(base_cpu)}
    for label, prompt, expected in (("plain", "hello", None), ("slow", "slow", 60 * 0.15 * 1e3)):
        first, total, cpu, rss = [], [], [], []
        for _ in range(runs if label == "plain" else max(1, runs // 2)):
            session = Headless(binary, ws, env)
            session.ready()
            turn = session.turn(f"{prompt} {uuid.uuid4().hex[:8]}")
            row = session.close()
            first.append(turn["first_ms"])
            total.append(turn["total_ms"])
            cpu.append(row["cpu_ms"])
            rss.append(row["rss_mb"])
        # CPU above is the whole process; the turn's share is what the
        # idle baseline does not account for.
        floor = statistics.median(base_cpu)
        out[label] = {
            "first_token_ms": summarize(first),
            "total_ms": summarize(total),
            "cpu_ms": summarize(cpu),
            "turn_cpu_ms": summarize([max(0.0, c - floor) for c in cpu]),
            "rss_mb": summarize(rss),
        }
        if expected:
            out[label]["scripted_ms"] = expected
    return out


def bench_streaming(binary, ws, env, runs):
    """Both with the genome index (the default) and without it: with it, the
    process's CPU is dominated by the one-off index, so the turn's own CPU is
    only readable in the second."""
    off = dict(env, TITI_NO_GENOME="1")
    return {
        "genome": bench_streaming_mode(binary, ws, env, runs),
        "no_genome": bench_streaming_mode(binary, ws, off, runs),
    }


# ---------------------------------------------------------------- prompt

# "!edit" is the user changing a tracked file between turns, so the working-tree
# diff block appears; every other entry is a prompt for the scripted provider.
SCENARIO = [
    "hello",
    "read readme",
    "!edit",
    "hello again",
    "read long",
    "hello a third time",
    "hello a fourth time",
    "hello a fifth time",
]
BLOCKS = {
    "genome": re.compile(r"<genome\b.*?</genome>", re.S),
    "diff": re.compile(r"<diff>.*?</diff>", re.S),
    "recall": re.compile(r"<recall\b.*?</recall>", re.S),
}


def wire_len(value):
    return len(json.dumps(value, separators=(",", ":"), ensure_ascii=False).encode())


def content_text(message):
    content = message.get("content")
    if isinstance(content, list):
        return " ".join(part.get("text", "") for part in content if isinstance(part, dict))
    return content or ""


def breakdown(body):
    """Bytes of one request body by what they are. `genome`/`diff`/`recall` split
    into the newest user message's copy (this turn's) and copies riding in
    older messages (history)."""
    messages = body.get("messages", [])
    last_user = max((i for i, m in enumerate(messages) if m.get("role") == "user"), default=-1)
    parts = {
        "system": 0,
        "tools": wire_len(body.get("tools", [])),
        "genome_fresh": 0,
        "genome_stale": 0,
        "diff_fresh": 0,
        "diff_stale": 0,
        "recall_fresh": 0,
        "recall_stale": 0,
        "history": 0,
    }
    for i, message in enumerate(messages):
        text = content_text(message)
        if message.get("role") == "system":
            parts["system"] += len(text.encode())
            continue
        rest = len(text.encode())
        if message.get("role") == "user":
            for name, pattern in BLOCKS.items():
                for block in pattern.findall(text):
                    size = len(block.encode())
                    rest -= size
                    parts[f"{name}_{'fresh' if i == last_user else 'stale'}"] += size
        parts["history"] += rest
        # tool_calls ride beside content on assistant messages.
        extra = {k: v for k, v in message.items() if k not in ("role", "content")}
        parts["history"] += wire_len(extra) if extra else 0
    total = wire_len(body)
    parts["total"] = total
    parts["framing"] = total - sum(v for k, v in parts.items() if k != "total")
    return parts


def bench_prompt(binary, ws, env, provider):
    tag = "BNCH" + uuid.uuid4().hex[:8]
    offset = provider.size()
    session = Headless(binary, ws, env)
    session.ready()
    for prompt in SCENARIO:
        if prompt == "!edit":
            with open(os.path.join(ws, "CHANGELOG.md"), "a") as handle:
                handle.write("".join(f"- bench edit line {i}\n" for i in range(40)))
            continue
        session.turn(f"{prompt} {tag}")
    session.close()
    time.sleep(0.3)
    requests = []
    with open(provider.requests, "rb") as handle:
        handle.seek(offset)
        for raw in handle:
            try:
                row = json.loads(raw)
            except ValueError:
                continue
            if tag in json.dumps(row.get("body", {})):
                requests.append(row["body"])
    if not requests:
        sys.exit("no requests for this run found in " + provider.requests)
    rows = [breakdown(body) for body in requests]
    keys = [k for k in rows[0]]
    cumulative = {k: sum(r[k] for r in rows) for k in keys}
    return {
        "requests": len(rows),
        "per_request": rows,
        "sum_bytes": cumulative,
        "est_tokens_sum": cumulative["total"] // 4,
        "first_request": rows[0],
        "last_request": rows[-1],
    }


# ---------------------------------------------------------------- examples

def bench_example(name, extra):
    binary = os.path.join(target_dir(), "release", "examples", name)
    return run_json_lines([binary, *extra], ROOT)


# ---------------------------------------------------------------- report

def flatten_rows(rows):
    return {row["name"]: row for row in rows}


def table(headers, body):
    lines = ["| " + " | ".join(headers) + " |", "|" + "|".join("---" for _ in headers) + "|"]
    lines += ["| " + " | ".join(str(c) for c in row) + " |" for row in body]
    return "\n".join(lines)


def f(value, digits=1):
    return f"{value:.{digits}f}" if isinstance(value, (int, float)) else str(value)


def example_table(rows):
    body = []
    for row in rows:
        if row["name"].endswith(".files"):
            body.append([row["name"], 1, "", f(row["median_ms"], 0), "", "files"])
            continue
        body.append(
            [
                row["name"],
                row["n"],
                f(row["min_ms"], 2),
                f(row["median_ms"], 2),
                f(row["p95_ms"], 2),
                row.get("bytes", ""),
            ]
        )
    return table(["case", "n", "min ms", "median ms", "p95 ms", "bytes"], body)


def report(results):
    out = ["## Machine", "", "```", json.dumps(results["machine"], indent=1), "```", ""]
    if "startup" in results:
        s = results["startup"]
        body = [["titi --help wall ms", *[f(s["help"]["wall_ms"][k]) for k in ("n", "min", "median", "p95")]]]
        body.append(["titi --help peak RSS MB", *[f(s["help"]["rss_mb"][k]) for k in ("n", "min", "median", "p95")]])
        for label in ("tui", "tui_no_genome"):
            for key, name in (
                ("first_byte_ms", "first byte ms"),
                ("first_frame_ms", "first frame ms"),
                ("idle_bytes", "bytes after first frame (2 s)"),
                ("cpu_ms", "session CPU ms"),
                ("rss_mb", "peak RSS MB"),
            ):
                if key in s[label]:
                    body.append([f"{label} {name}", *[f(s[label][key][k]) for k in ("n", "min", "median", "p95")]])
        out += ["## Startup", "", table(["case", "n", "min", "median", "p95"], body), ""]
    for suite, title in (("genome", "Genome"), ("tools", "Tools"), ("render", "Render")):
        if suite in results:
            out += [f"## {title}", "", example_table(results[suite]), ""]
    if "streaming" in results:
        body = []
        for mode in ("genome", "no_genome"):
            s = results["streaming"][mode]
            body.append([f"{mode} ready ms", *[f(s["ready_ms"][k]) for k in ("n", "min", "median", "p95")]])
            body.append([f"{mode} idle process CPU ms", *[f(s["idle_cpu_ms"][k]) for k in ("n", "min", "median", "p95")]])
            for label in ("plain", "slow"):
                for key, name in (
                    ("first_token_ms", "first token ms"),
                    ("total_ms", "turn total ms"),
                    ("turn_cpu_ms", "turn CPU ms (process minus idle)"),
                    ("rss_mb", "peak RSS MB"),
                ):
                    body.append(
                        [f"{mode} {label} {name}", *[f(s[label][key][k]) for k in ("n", "min", "median", "p95")]]
                    )
        out += ["## Streaming", "", table(["case", "n", "min", "median", "p95"], body), ""]
    if "prompt" in results:
        p = results["prompt"]
        cols = [
            "system",
            "tools",
            "genome_fresh",
            "genome_stale",
            "diff_fresh",
            "diff_stale",
            "recall_fresh",
            "recall_stale",
            "history",
            "framing",
            "total",
        ]
        body = []
        for i, row in enumerate(p["per_request"]):
            body.append([i + 1, *[row[c] for c in cols], row["total"] // 4])
        body.append(["**sum**", *[p["sum_bytes"][c] for c in cols], p["est_tokens_sum"]])
        out += [
            "## Prompt (bytes per request; estimated tokens = bytes/4)",
            "",
            table(["req", *cols, "est tokens"], body),
            "",
        ]
    return "\n".join(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--quick", action="store_true", help="fewer runs (whole suite under 5 minutes)")
    parser.add_argument("--out", help="write the results as JSON here")
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--bin", help="titi binary to measure (default: target/release/titi)")
    parser.add_argument("--only", help="comma list of suites to run")
    parser.add_argument("--requests", default=DEFAULT_REQUESTS, help="provider request log (read only)")
    parser.add_argument("--agent-template", default="/tmp/titi-qa/run/agent", help="agent dir to copy SOUL.md etc. from")
    args = parser.parse_args()
    suites = args.only.split(",") if args.only else ["startup", "genome", "tools", "render", "streaming", "prompt"]
    runs = 3 if args.quick else 7

    cargo = cargo_bin()
    if not args.skip_build:
        build(cargo)
    binary = os.path.abspath(args.bin) if args.bin else os.path.join(target_dir(), "release", "titi")
    quick = ["--quick"] if args.quick else []

    results = {"machine": machine(), "quick": args.quick}
    base = tempfile.mkdtemp(prefix="titi-bench-")
    provider = None
    try:
        ws, agent = make_workspace(base, args.agent_template)
        env = titi_env(base, agent)
        if "genome" in suites:
            log("genome")
            results["genome"] = bench_example("bench_genome", [*quick, ws])
        if "tools" in suites:
            log("tools")
            results["tools"] = bench_example("bench_tools", [*quick, ws])
        if "render" in suites:
            log("render")
            results["render"] = bench_example("bench_render", quick)
        if "startup" in suites:
            log("startup")
            results["startup"] = bench_startup(binary, ws, env, runs)
        if {"streaming", "prompt"} & set(suites):
            provider = Provider(args.requests)
        if "streaming" in suites:
            log("streaming")
            results["streaming"] = bench_streaming(binary, ws, env, runs)
        if "prompt" in suites:
            log("prompt")
            results["prompt"] = bench_prompt(binary, ws, env, provider)
    finally:
        if provider:
            provider.close()
        shutil.rmtree(base, ignore_errors=True)

    print(report(results))
    if args.out:
        with open(args.out, "w") as handle:
            json.dump(results, handle, indent=1)
        log(f"wrote {args.out}")


if __name__ == "__main__":
    main()
