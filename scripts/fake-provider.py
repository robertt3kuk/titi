#!/usr/bin/env python3
"""A scripted OpenAI-compatible provider for driving titi without a key.

Serves `POST /v1/chat/completions` as SSE and `GET /v1/models`, and answers
by matching a word in the last message, so a smoke run can reach every
phase of a turn — streaming, reasoning, each tool, each failure — through
the real binary and the real HTTP stack:

  "sleepy shell"  a bash call that runs for 3 s (the live tool phase)
  "slow"          60 chunks, 0.15 s apart (time to steer or press Ctrl+C)
  "run bash"      text, then a bash call: echo titi-smoke
  "big output"    a bash call that prints 20,000 lines (the output cap)
  "edit readme"   an edit call on README.md (`# smoke ws` -> `# smoke workspace`)
  "read readme"   a read call on README.md
  "write file"    a write call creating smoke.txt
  "think"         reasoning deltas, then markdown
  "wide text"     CJK and emoji
  "fail401" / "fail429" / "fail500"   that HTTP status with an error body
  a tool result   "tool said: <first line of the result>"
  anything else   "Hello from the fake server. You said: <text>"

Every request body is appended to `requests.jsonl` in the working
directory, so a run can check what the model was actually sent.

Point an agent dir at it (no key is involved):

  providers:
    - id: fake
      api: openai-completions
      base_url: http://127.0.0.1:18999/v1
      credential_required: false
  models:
    - id: fake/scripted
      provider: fake
      wire_model: fake
      context_window: 32000

Usage: scripts/fake-provider.py [port]   (default 18999, loopback only)
"""

from __future__ import annotations

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = "requests.jsonl"


def chunk(delta: dict, finish: str | None = None) -> dict:
    return {
        "id": "chatcmpl-fake",
        "object": "chat.completion.chunk",
        "model": "fake",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }


def call(call_id: str, name: str, args: dict) -> list[dict]:
    """One tool call, its arguments in a single fragment."""
    return [
        chunk({"tool_calls": [{
            "index": 0, "id": call_id, "type": "function",
            "function": {"name": name, "arguments": json.dumps(args)},
        }]}),
        chunk({}, "tool_calls"),
    ]


def words(text: str, pause: float = 0.02) -> list[tuple[dict, float]]:
    return [(chunk({"content": word + " "}), pause) for word in text.split(" ")]


def script(last: dict) -> list[tuple[dict, float]]:
    """The chunks to stream for this request, each with the pause after it."""
    content = last.get("content") or ""
    if isinstance(content, list):
        content = " ".join(p.get("text", "") for p in content if isinstance(p, dict))
    if last.get("role") == "tool":
        first = content.strip().splitlines()[0] if content.strip() else "(empty)"
        return words(f"tool said: {first}", 0) + [(chunk({}, "stop"), 0)]
    if "sleepy shell" in content:
        return [(c, 0) for c in call("call_sleepy", "bash", {"command": "sleep 3; echo slept"})]
    if "slow" in content:
        return [(chunk({"content": f"tick{i} "}), 0.15) for i in range(60)] + [(chunk({}, "stop"), 0)]
    if "run bash" in content:
        return [(chunk({"content": "Running it. "}), 0)] + [
            (c, 0) for c in call("call_bash", "bash", {"command": "echo titi-smoke"})
        ]
    if "big output" in content:
        return [(c, 0) for c in call("call_big", "bash", {"command": "seq 1 20000"})]
    if "edit readme" in content:
        args = {"path": "README.md", "old_string": "# smoke ws", "new_string": "# smoke workspace"}
        return [(c, 0) for c in call("call_edit", "edit", args)]
    if "read readme" in content:
        return [(c, 0) for c in call("call_read", "read", {"path": "README.md"})]
    if "write file" in content:
        args = {"path": "smoke.txt", "content": "written by the smoke\n"}
        return [(c, 0) for c in call("call_write", "write", args)]
    if "think" in content:
        thinking = [(chunk({"reasoning_content": w + " "}), 0.05) for w in "let me consider this".split()]
        return thinking + [(chunk({"content": "**Thought** done. `code` here."}), 0), (chunk({}, "stop"), 0)]
    if "wide text" in content:
        parts = ["日本語のテキスト ", "and emoji 🎉🚀 ", "mixed 中文 text ", "done."]
        return [(chunk({"content": p}), 0) for p in parts] + [(chunk({}, "stop"), 0)]
    said = content.strip()[:60]
    return words(f"Hello from the fake server. You said: {said}") + [(chunk({}, "stop"), 0)]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply_json(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def write_chunk(self, data: bytes) -> None:
        self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data))
        self.wfile.flush()

    def do_GET(self):
        self.reply_json(200, {"object": "list", "data": [{"id": "fake", "object": "model"}]})

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        request = json.loads(self.rfile.read(length) or b"{}")
        with open(LOG, "a", encoding="utf-8") as log:
            log.write(json.dumps({"path": self.path, "body": request}) + "\n")
        messages = request.get("messages", [])
        last = messages[-1] if messages else {}
        text = str(last.get("content") or "")
        for code in (401, 429, 500):
            if f"fail{code}" in text:
                self.reply_json(code, {"error": {"message": f"scripted failure {code}", "code": code}})
                return

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        try:
            for event, pause in script(last):
                self.write_chunk(b"data: " + json.dumps(event).encode() + b"\n\n")
                if pause:
                    time.sleep(pause)
            usage = {"id": "u", "object": "chat.completion.chunk", "model": "fake", "choices": [],
                     "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110}}
            self.write_chunk(b"data: " + json.dumps(usage).encode() + b"\n\n")
            self.write_chunk(b"data: [DONE]\n\n")
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18999
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
