#!/usr/bin/env python3
"""Automated PTY smoke test for the titi chat screen.

CI has no human at a terminal and no provider key, so this drives the real
binary through a pseudo-terminal instead of a person: wait for the ready
frame, open the slash listing with `/`, run `/help`, quit with Ctrl+D, then
check the terminal was handed back clean (cursor shown, alternate screen
left). Nothing here submits a prompt, so no model turn and no key is needed.

Every wait is bounded; a binary that starts and then hangs is killed and
reported as a failure, never as a pass. See `.agents/skills/tui-smoke` for
the manual checks this automates the basics of.

Usage:
    scripts/tui-smoke.py --bin target/debug/titi

Exit codes: 0 pass, 1 smoke failure, 2 bad usage.
"""

from __future__ import annotations

import argparse
import errno
import fcntl
import os
import pty
import re
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

# The terminal state the screen must set on the way in and undo on the way out.
ALT_ENTER = b"\x1b[?1049h"
ALT_LEAVE = b"\x1b[?1049l"
CURSOR_HIDE = b"\x1b[?25l"
CURSOR_SHOW = b"\x1b[?25h"

# OSC (BEL- or ST-terminated), CSI, then the plain two-byte escapes.
ANSI = re.compile(
    rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)"
    rb"|\x1b\[[0-?]*[ -/]*[@-~]"
    rb"|\x1b[@-Z\\-_]"
)

# Anything that could let the run reach a live provider. A smoke is offline.
SECRET_ENV = re.compile(r"(?:^|_)(?:KEY|TOKEN|SECRET|PASSWORD|CREDENTIALS)$")


class SmokeFailure(Exception):
    """A check failed, or the binary stopped answering."""


def escape(data: bytes) -> str:
    """Printable form of an escape sequence, e.g. `\\x1b[?1049l`."""
    return "".join(chr(b) if 32 <= b < 127 else f"\\x{b:02x}" for b in data)


def strip_ansi(raw: bytes) -> str:
    """Screen text with the escape sequences removed.

    Rows are glued together: a full-screen redraw positions the cursor
    instead of writing newlines. Matching stays inside one rendered row.
    """
    return ANSI.sub(b"", raw).decode("utf-8", "replace")


class PtySession:
    """The binary running on a pty, with its output drained in the background.

    The reader thread matters: a pty buffer is small (about 1 KiB on macOS)
    and one ratatui frame is much larger, so a child whose output nobody
    reads blocks in write() and looks exactly like a hang.
    """

    def __init__(self, argv, env, cwd, rows, cols):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        try:
            self.proc = subprocess.Popen(
                argv,
                stdin=slave,
                stdout=slave,
                stderr=slave,
                env=env,
                cwd=cwd,
                close_fds=True,
                start_new_session=True,
            )
        finally:
            os.close(slave)
        self._buf = bytearray()
        self._lock = threading.Lock()
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self) -> None:
        while True:
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                return  # EIO on Linux once the child side is gone.
            if not chunk:
                return
            with self._lock:
                self._buf.extend(chunk)

    def raw(self) -> bytes:
        with self._lock:
            return bytes(self._buf)

    def text(self) -> str:
        return strip_ansi(self.raw())

    def send(self, data: bytes) -> None:
        os.write(self.master, data)

    def expect(self, needles, timeout: float, what: str, deadline: float) -> float:
        """Wait until every needle has shown up on screen. Returns seconds waited."""
        started = time.monotonic()
        limit = min(started + timeout, deadline)
        bound = "step timeout" if limit < deadline else "--timeout ceiling for the run"
        while True:
            text = self.text()
            missing = [needle for needle in needles if needle not in text]
            if not missing:
                return time.monotonic() - started
            code = self.proc.poll()
            if code is not None:
                raise SmokeFailure(
                    f"{what}: the binary exited with status {code} before "
                    f"showing {missing!r}"
                )
            if time.monotonic() >= limit:
                waited = time.monotonic() - started
                raise SmokeFailure(
                    f"{what}: timed out after {waited:.1f}s waiting for "
                    f"{missing!r} — the binary is still running, hit the {bound}"
                )
            time.sleep(0.05)

    def wait_exit(self, timeout: float):
        try:
            return self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            return None

    def kill(self) -> None:
        """Stop the whole process group; SIGTERM first, SIGKILL if it sulks."""
        if self.proc.poll() is None:
            for sig in (signal.SIGTERM, signal.SIGKILL):
                try:
                    os.killpg(os.getpgid(self.proc.pid), sig)
                except (ProcessLookupError, PermissionError, OSError):
                    break
                try:
                    self.proc.wait(3)
                    break
                except subprocess.TimeoutExpired:
                    continue
        try:
            os.close(self.master)
        except OSError as error:
            if error.errno != errno.EBADF:
                raise


def child_env(agent_dir: str, home: str) -> dict:
    """Environment with a scratch agent dir and no credentials at all."""
    env = {k: v for k, v in os.environ.items() if not SECRET_ENV.search(k)}
    env["TERM"] = "xterm-256color"
    env["HOME"] = home
    env["TITI_AGENT_DIR"] = agent_dir
    env["TITI_NO_GENOME"] = "1"  # No repo scan: the screen is what is under test.
    env.pop("TITI_PROFILE", None)
    env.pop("TITI_CONFIG_FILES", None)
    return env


def check_terminal_restored(raw: bytes) -> list:
    """The escape-sequence evidence that the terminal was left usable."""
    if ALT_ENTER not in raw:
        raise SmokeFailure(
            f"the screen never entered the alternate screen ({escape(ALT_ENTER)}): "
            "the UI did not start"
        )
    if ALT_LEAVE not in raw:
        raise SmokeFailure(
            f"the screen never left the alternate screen ({escape(ALT_LEAVE)})"
        )
    if raw.rindex(ALT_LEAVE) < raw.rindex(ALT_ENTER):
        raise SmokeFailure(
            f"{escape(ALT_ENTER)} came after the last {escape(ALT_LEAVE)}: "
            "the alternate screen was left open"
        )
    if CURSOR_SHOW not in raw:
        raise SmokeFailure(f"the cursor was never restored ({escape(CURSOR_SHOW)})")
    if CURSOR_HIDE in raw and raw.rindex(CURSOR_HIDE) > raw.rindex(CURSOR_SHOW):
        raise SmokeFailure(
            f"the last {escape(CURSOR_HIDE)} came after {escape(CURSOR_SHOW)}: "
            "the cursor was left hidden"
        )
    return [
        (escape(ALT_ENTER), raw.index(ALT_ENTER)),
        (escape(CURSOR_SHOW), raw.rindex(CURSOR_SHOW)),
        (escape(ALT_LEAVE), raw.rindex(ALT_LEAVE)),
    ]


def report_failure(reason: str, session, tail_bytes: int) -> None:
    print(f"tui-smoke: FAIL {reason}", file=sys.stderr)
    if session is None:
        return
    raw = session.raw()
    text = session.text()
    print(f"--- screen text, last {tail_bytes} chars ---", file=sys.stderr)
    print(text[-tail_bytes:] or "(nothing was printed)", file=sys.stderr)
    print("--- raw output, last 300 bytes ---", file=sys.stderr)
    print(escape(raw[-300:]) or "(nothing was printed)", file=sys.stderr)


def smoke(args) -> None:
    binary = os.path.abspath(args.bin)
    if not os.path.isfile(binary) or not os.access(binary, os.X_OK):
        raise SystemExit(
            f"tui-smoke: {args.bin} is not an executable file; "
            "build it first with `cargo build -p titi-cli --locked`"
        )

    session = None
    started = time.monotonic()
    deadline = started + args.timeout
    with tempfile.TemporaryDirectory(prefix="titi-smoke-") as scratch:
        agent_dir = os.path.join(scratch, "agent")
        workspace = os.path.join(scratch, "workspace")
        os.makedirs(agent_dir)
        os.makedirs(workspace)
        try:
            session = PtySession(
                [binary],
                child_env(agent_dir, scratch),
                workspace,
                args.rows,
                args.cols,
            )

            # 1. The ready frame: masthead title and the idle state.
            waited = session.expect(
                ["titi", "ready"], args.ready_timeout, "ready frame", deadline
            )
            print(f"tui-smoke: ready frame after {waited:.1f}s")

            # 2. A bare `/` lists the commands.
            session.send(b"/")
            waited = session.expect(
                ["/help", "/checkpoint", "/model"],
                args.step_timeout,
                "slash listing",
                deadline,
            )
            print(f"tui-smoke: `/` listed the commands after {waited:.1f}s")

            # 3. `/help` prints the catalog into the transcript.
            session.send(b"help\r")
            waited = session.expect(
                ["list these commands", "switch model"],
                args.step_timeout,
                "/help output",
                deadline,
            )
            print(f"tui-smoke: /help printed the catalog after {waited:.1f}s")

            # 4. Ctrl+D on an empty composer quits.
            session.send(b"\x04")
            code = session.wait_exit(min(args.exit_timeout, max(deadline - time.monotonic(), 0.1)))
            if code is None:
                raise SmokeFailure(
                    f"the binary did not exit within {args.exit_timeout:.0f}s of Ctrl+D"
                )
            if code != 0:
                raise SmokeFailure(f"the binary exited with status {code}, expected 0")
            print(f"tui-smoke: Ctrl+D quit with status {code}")

            # 5. The terminal has to be usable afterwards.
            evidence = check_terminal_restored(session.raw())
            trail = ", ".join(f"{sequence} @{at}" for sequence, at in evidence)
            print(f"tui-smoke: terminal restored ({trail})")
        except SmokeFailure as failure:
            report_failure(str(failure), session, args.tail)
            raise SystemExit(1)
        finally:
            if session is not None:
                session.kill()

    print(f"tui-smoke: PASS in {time.monotonic() - started:.1f}s")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--bin",
        default=os.environ.get("TITI_BIN", "target/debug/titi"),
        help="titi binary to drive (default: $TITI_BIN or target/debug/titi)",
    )
    parser.add_argument("--cols", type=int, default=100, help="pty width (default: 100)")
    parser.add_argument("--rows", type=int, default=30, help="pty height (default: 30)")
    parser.add_argument(
        "--ready-timeout",
        type=float,
        default=45.0,
        help="seconds to wait for the first frame (default: 45)",
    )
    parser.add_argument(
        "--step-timeout",
        type=float,
        default=15.0,
        help="seconds to wait for each key to land (default: 15)",
    )
    parser.add_argument(
        "--exit-timeout",
        type=float,
        default=15.0,
        help="seconds to wait for the quit after Ctrl+D (default: 15)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=120.0,
        help="hard ceiling for the whole run in seconds (default: 120)",
    )
    parser.add_argument(
        "--tail",
        type=int,
        default=1200,
        help="characters of screen text to print on failure (default: 1200)",
    )
    smoke(parser.parse_args())


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        raise SystemExit(130)
