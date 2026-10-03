#!/usr/bin/env python3
"""Drive the titi binary on a PTY and print what a person would see.

`scripts/tui-smoke.py` checks the basics with no model; this drives a whole
scenario — typed text, keys, resizes, waits — through a terminal emulator
(pyte, `pip install pyte`), and prints each requested screen as rows, so a
check of the chat reads as frames rather than escape bytes. Pair it with
`scripts/fake-provider.py` for real turns without a key (skill `tui-smoke`).

Usage: scripts/pty-drive.py SCENARIO.json

SCENARIO: {"bin": "target/debug/titi", "cwd": "...", "args": [...],
           "env": {"PATH": "...", "TITI_AGENT_DIR": "...", ...},
           "cols": 80, "rows": 24,
           "steps": [["send", "text"], ["key", "enter"], ["raw", "\u001bm"],
                     ["wait", "text", secs], ["waitgone", "text", secs],
                     ["sleep", secs], ["shot", "label"], ["resize", cols, rows]]}

Keys: enter esc tab up down left right backspace ctrl-c ctrl-d ctrl-x ctrl-l.
The environment is exactly `env`: nothing leaks in from the caller's, so no
provider key can reach the run by accident. Exits 1 on a failed wait (after
printing the screen it failed on), 0 otherwise; the last lines say how the
process ended and whether the terminal was restored. The raw bytes are kept
next to the scenario as `<scenario>.raw`.
"""

import fcntl
import json
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

import pyte

KEYS = {
    "enter": "\r", "esc": "\x1b", "tab": "\t", "up": "\x1b[A", "down": "\x1b[B",
    "right": "\x1b[C", "left": "\x1b[D", "ctrl-c": "\x03", "ctrl-d": "\x04",
    "ctrl-x": "\x18", "backspace": "\x7f", "ctrl-l": "\x0c",
}


def main():
    script = json.load(open(sys.argv[1]))
    cols, rows = script.get("cols", 80), script.get("rows", 24)
    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)
    raw = bytearray()
    # Resolved here, before the child changes directory.
    binary = os.path.abspath(script["bin"])
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(script["cwd"])
        os.execve(binary, [binary] + script.get("args", []), script["env"])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def pump(timeout):
        end = time.time() + timeout
        while True:
            left = end - time.time()
            if left <= 0:
                return
            r, _, _ = select.select([fd], [], [], min(left, 0.05))
            if r:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return
                if not data:
                    return
                raw.extend(data)
                stream.feed(data)

    def text():
        return "\n".join(screen.display)

    def shot(label):
        print(f"===== {label} ({screen.columns}x{screen.lines})")
        for i, line in enumerate(screen.display):
            print(f"{i:02d}|{line.rstrip()}")
        sys.stdout.flush()

    ok = True
    pump(0.5)
    for step in script["steps"]:
        kind = step[0]
        if kind == "send":
            for ch in step[1]:
                os.write(fd, ch.encode())
                pump(0.01)
        elif kind == "key":
            os.write(fd, KEYS[step[1]].encode())
            pump(0.05)
        elif kind == "raw":
            os.write(fd, step[1].encode())
            pump(0.05)
        elif kind == "sleep":
            pump(step[1])
        elif kind == "wait":
            end = time.time() + step[2]
            while step[1] not in text() and time.time() < end:
                pump(0.1)
            if step[1] not in text():
                print(f"!!!!! wait failed: {step[1]!r}")
                shot("at failure")
                ok = False
                break
        elif kind == "waitgone":
            end = time.time() + step[2]
            while step[1] in text() and time.time() < end:
                pump(0.1)
            if step[1] in text():
                print(f"!!!!! still present: {step[1]!r}")
                shot("at failure")
                ok = False
                break
        elif kind == "shot":
            shot(step[1])
        elif kind == "resize":
            screen.resize(step[2], step[1])
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", step[2], step[1], 0, 0))
            os.kill(pid, signal.SIGWINCH)
            pump(0.5)
    pump(0.3)
    try:
        wpid, status = os.waitpid(pid, os.WNOHANG)
    except ChildProcessError:
        wpid, status = pid, 0
    if wpid == 0:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        print("process: still running, killed")
    else:
        print(f"process: exited status={os.waitstatus_to_exitcode(status)}")
    tail = bytes(raw[-200:])
    print("restore:", "?25h" in tail.decode("latin1"), "?1049l" in tail.decode("latin1"))
    open(sys.argv[1] + ".raw", "wb").write(bytes(raw))
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
