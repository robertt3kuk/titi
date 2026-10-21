#!/usr/bin/env python3
"""A scripted OpenAI-compatible provider for driving titi without a key.

Serves `POST /v1/chat/completions` as SSE and `GET /v1/models`, and answers
by matching a word in the last message, so a smoke run can reach every
phase of a turn — streaming, reasoning, each tool, each failure — through
the real binary and the real HTTP stack:

  "[titi-review]" (the marker every /review request carries; matched before the
                  words below) a reviewer reply: `VERDICT: FAIL — missing test` and
                  one finding, `README.md:1 — no test covers the new title`
  "[titi-compact]" (the marker in the system prompt of a compaction summary
                  request; matched before every other word, because the
                  transcript it carries repeats the words of the whole
                  conversation) a structured summary, `## Goal` … `## Next step`
  "sleepy shell"  a bash call that runs for 3 s (the live tool phase)
  "danger shell"  a bash call on `rm -rf ./victim` (what a PreToolUse hook that
                  refuses `rm -rf` is for)
  "forever"       a bash call that never exits (Ctrl+C must stop it)
  "dev server"    a bash call on `npm run dev` (refused before it runs)
  "make todo"     a todo write: three items, the second in progress
  "ask me"        an `ask` call: a question with three options
  "color shell"   a pty bash call printing colour and a `\r` progress bar
  "loose edit"    an edit on README.md whose old_string is off in whitespace
  "slow"          60 chunks, 0.15 s apart (time to steer or press Ctrl+C)
  "spin cap"      (first user message) a `read` of README.md every round with a
                  different `limit`, forever; once the request carries
                  `tool_choice: "none"` it answers "Wrapped up: ..."
  "spin repeat"   the same `read` of README.md every round until a tool result
                  carries titi's `[titi:` nudge, then it answers and stops
  "break syntax"  (first user message; needs `lib.rs` in the workspace holding
                  `fn main() {\n    println!("hi");\n}\n`) an edit that leaves one
                  `}` too many; when the tool result carries titi's `syntax:` line
                  it repairs the file with a second edit, then answers with the
                  `syntax:` line the repair returned
  "verify loop"   (first user message) a write of a.txt, then "Done"; a user
                  message that starts `verification failed:` (the turn-end
                  verifier's feedback) makes it write ok.txt and answer "Fixed";
                  set `verify.command: test -f ok.txt || exit 3` to see the loop
  "run bash"      text, then a bash call: echo titi-smoke
  "load skill"    a `skill` call naming the first skill the system prompt lists
                  (`- name: description` under `# Skills`); `skill: none listed`
                  when it lists none
  "call mcp"      a `mcp__fixture__echo` call {text: "hello from mcp"} (the tool
                  a server named `fixture` offers; see crates/titi-mcp/tests/fixtures)
  "run project"   a `project` call: {action: list} (detects the workspace's
                  ecosystem and prints the commands; runs nothing)
  "delegate reviewer"  an `agent` call naming the definition `reviewer`, with a
                  plain task, so the child's request shows the definition's
                  prompt and tools (needs `.claude/agents/reviewer.md`)
  "delegate capped"  an `agent` call naming the definition `capped`, whose task
                  holds `spin cap`: the child reads README.md every round until
                  its request forbids tool calls (give the definition a small
                  `maxTurns`)
  "delegate nobody"  an `agent` call naming a definition that does not exist
  "pin agents"    two `agent` calls in one round, each with a small task: the
                  tool loop runs them one after another, so the strip shows one
                  live agent at a time
  "pin batch"     one `agent` call carrying `tasks: [...]`: the children run at
                  once, so the strip shows two live agents together
  "pin exec"      a batch of two `agent` calls *and* a `bash` call in one
                  response: the strip is live while the parent turn asks for an
                  approval, which is the crossing a pane has to survive
  "mermaid answer"  a `flowchart TD` fence, closed, so the transcript can draw it
  "chart table"   a GFM table whose one numeric column has a wide spread, which
                  the transcript charts under the table
  "big output"    a bash call that prints 20,000 lines (the output cap)
  "edit readme"   an edit call on README.md (`# smoke ws` -> `# smoke workspace`)
  "read readme"   a read call on README.md
  "atlas probe X" an `atlas` tool call {op: find, query: X}: X is the word right after the
                  keyword, so a prompt that names an identifier from the code makes the
                  model ask the index about it
  "read long"     a read call on long.txt, no range (make it with `seq 1 5000`)
  "read dir"      a read call on the workspace root (a directory)
  "write file"    a write call creating smoke.txt
  "think"         reasoning deltas, then markdown
  "markdown table"  a GFM table: alignment colons, a bold and an inline-code
                  cell, a CJK cell, and a wide cell that must wrap
  "latex math"    inline `$O(\log n)$` plus a display `$$…$$` block
  "latex only"    the same maths and nothing else (needs the chat gate)
  "wide text"     CJK and emoji
  "fail401" / "fail429" / "fail500"   that HTTP status with an error body
  a tool result   "tool said: <first line of the result>"
  anything else   "Hello from the fake server. You said: <text>"

Every answer ends with a usage chunk: 100 prompt tokens, 60 of them cached,
and 10 completion tokens. With `FAKE_CACHE=1` in the environment the usage is
instead a simulated prefix cache (see `simulated_cache`): a request that
repeats the previous one's front reads it back as cached, one whose model,
effort, tools or earlier messages differ loses everything from the first
difference, so a cache-break diagnostic has something to find.

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
import re
import os
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


def call(call_id: str, name: str, args: dict, index: int = 0, finish: bool = True) -> list[dict]:
    """One tool call, its arguments in a single fragment.

    `index` numbers the call within the message: a provider that sends two
    calls with the same index has them merged into one by any reader that
    keys on it, which is what a two-call case must not do.
    """
    return [
        chunk({"tool_calls": [{
            "index": index, "id": call_id, "type": "function",
            "function": {"name": name, "arguments": json.dumps(args)},
        }]}),
        chunk({}, "tool_calls") if finish else chunk({}),
    ]


def words(text: str, pause: float = 0.02) -> list[tuple[dict, float]]:
    return [(chunk({"content": word + " "}), pause) for word in text.split(" ")]


def first_listed_skill(messages: list[dict]) -> str | None:
    """The first `- name: description` line under `# Skills` in a system message."""
    for message in messages:
        if message.get("role") != "system":
            continue
        text = str(message.get("content") or "")
        _, found, rest = text.partition("# Skills\n")
        if not found:
            continue
        for line in rest.splitlines():
            if line.startswith("- "):
                return line[2:].split(":", 1)[0].strip()
    return None


def spin(request: dict) -> list[tuple[dict, float]] | None:
    """The multi-round keywords, which need the whole conversation.

    `script` answers from the last message alone, and a tool result ends a
    chain after one round; these two keep calling tools for as long as the
    first user message says so, until the request forbids tool calls
    (`tool_choice: "none"`, the turn's wrap-up) or, for `spin repeat`, a tool
    result carries titi's `[titi:` nudge.
    """
    messages = request.get("messages", [])
    first = next((m for m in messages if m.get("role") == "user"), {})
    opening = str(first.get("content") or "")
    if "spin cap" not in opening and "spin repeat" not in opening:
        return None
    if request.get("tool_choice") == "none":
        return words("Wrapped up: read README.md over and over, nothing left to do.", 0) + [
            (chunk({}, "stop"), 0)
        ]
    results = [m for m in messages if m.get("role") == "tool"]
    if "spin repeat" in opening:
        if results and "[titi:" in str(results[-1].get("content") or ""):
            return words("Changing approach: I will stop repeating that read.", 0) + [
                (chunk({}, "stop"), 0)
            ]
        args = {"path": "README.md"}
    else:
        # A different range each round, so no call ever repeats.
        args = {"path": "README.md", "limit": len(results) + 1}
    return [(c, 0) for c in call(f"call_spin_{len(results)}", "read", args)]


def chain(request: dict) -> list[tuple[dict, float]] | None:
    """The turn-end keywords: `break syntax` and `verify loop`.

    Like `spin`, they key on the first user message and read the whole
    conversation, because what they do next depends on what titi appended: a
    `syntax:` line on an edit's result, or a `verification failed:` message.
    """
    messages = request.get("messages", [])
    first = next((m for m in messages if m.get("role") == "user"), {})
    opening = str(first.get("content") or "")
    last = messages[-1] if messages else {}
    text = str(last.get("content") or "")
    done = [(chunk({}, "stop"), 0)]
    if "break syntax" in opening:
        if last.get("role") == "user":
            args = {
                "path": "lib.rs",
                "old_string": '    println!("hi");\n}\n',
                "new_string": '    println!("hi");\n}\n}\n',
            }
            return [(c, 0) for c in call("call_break", "edit", args)]
        results = [m for m in messages if m.get("role") == "tool"]
        syntax = next((l for l in text.splitlines() if l.startswith("syntax:")), "")
        if len(results) == 1 and syntax:
            args = {"path": "lib.rs", "old_string": "}\n}\n", "new_string": "}\n"}
            return [(c, 0) for c in call("call_repair", "edit", args)]
        return words(f"Syntax check said: {syntax or 'nothing'}", 0) + done
    if "verify loop" in opening:
        if last.get("role") == "user" and text.startswith("verification failed:"):
            args = {"path": "ok.txt", "content": "fixed\n"}
            return [(c, 0) for c in call("call_verify_fix", "write", args)]
        if last.get("role") == "user":
            args = {"path": "a.txt", "content": "hello\n"}
            return [(c, 0) for c in call("call_verify_a", "write", args)]
        fixed = any(
            m.get("role") == "user" and str(m.get("content") or "").startswith("verification failed:")
            for m in messages
        )
        return words("Fixed: ok.txt written." if fixed else "Done: a.txt written.", 0) + done
    return None

COMPACT_SUMMARY = (
    "## Goal\nFake summary: carry on the scripted conversation.\n"
    "## Decisions made\nnone\n"
    "## Files and symbols touched\nnone\n"
    "## Commands run and outcomes\nnone\n"
    "## Unresolved errors and failing checks\nnone\n"
    "## User preferences and constraints\nnone\n"
    "## Next step\nanswer the next question"
)


def script(last: dict, messages: list[dict] | None = None) -> list[tuple[dict, float]]:
    """The chunks to stream for this request, each with the pause after it."""
    content = last.get("content") or ""
    if isinstance(content, list):
        content = " ".join(p.get("text", "") for p in content if isinstance(p, dict))
    if any(
        m.get("role") == "system" and "[titi-compact]" in str(m.get("content") or "")
        for m in messages or []
    ):
        return words(COMPACT_SUMMARY, 0) + [(chunk({}, "stop"), 0)]
    if last.get("role") == "tool":
        first = content.strip().splitlines()[0] if content.strip() else "(empty)"
        return words(f"tool said: {first}", 0) + [(chunk({}, "stop"), 0)]
    if "[titi-review]" in content:
        answer = "VERDICT: FAIL — missing test\n- README.md:1 — no test covers the new title\n"
        return words(answer, 0) + [(chunk({}, "stop"), 0)]
    if "load skill" in content:
        name = first_listed_skill(messages or [])
        if name is None:
            return words("skill: none listed", 0) + [(chunk({}, "stop"), 0)]
        return [(c, 0) for c in call("call_skill", "skill", {"name": name})]
    if "delegate reviewer" in content:
        args = {"agent": "reviewer", "task": "review the top of the readme and say what you find"}
        return [(c, 0) for c in call("call_delegate", "agent", args)]
    if "delegate capped" in content:
        args = {"agent": "capped", "task": "spin cap: keep reading the readme"}
        return [(c, 0) for c in call("call_delegate_capped", "agent", args)]
    if "delegate nobody" in content:
        args = {"agent": "nobody", "task": "do a thing"}
        return [(c, 0) for c in call("call_delegate_nobody", "agent", args)]
    if "sleepy shell" in content:
        return [(c, 0) for c in call("call_sleepy", "bash", {"command": "sleep 3; echo slept"})]
    if "danger shell" in content:
        return [(c, 0) for c in call("call_danger", "bash", {"command": "rm -rf ./victim"})]
    if "forever" in content:
        return [(c, 0) for c in call("call_forever", "bash", {"command": "echo started; sleep 600"})]
    if "dev server" in content:
        return [(c, 0) for c in call("call_dev", "bash", {"command": "npm run dev"})]
    if "color shell" in content:
        command = r"printf 'fetch 10%%\rfetch 99%%\r\033[Kfetched \033[32mok\033[0m\n'"
        return [(c, 0) for c in call("call_color", "bash", {"command": command, "pty": True})]
    if "loose edit" in content:
        args = {"path": "README.md", "old_string": "#   smoke ws  ", "new_string": "# smoke loose"}
        return [(c, 0) for c in call("call_loose", "edit", args)]
    if "make todo" in content:
        items = [
            {"content": "Read the failing test", "status": "completed"},
            {"content": "Fix the parser", "status": "in_progress"},
            {"content": "Run the suite", "status": "pending"},
        ]
        return [(c, 0) for c in call("call_todo", "todo", {"op": "write", "items": items})]
    if "ask me" in content:
        args = {
            "question": "Which database should I use?",
            "options": ["postgres", "sqlite", "duckdb"],
            "multi": False,
        }
        return [(c, 0) for c in call("call_ask", "ask", args)]
    if "call mcp" in content:
        args = {"text": "hello from mcp"}
        return [(c, 0) for c in call("call_mcp", "mcp__fixture__echo", args)]
    if "slow" in content:
        return [(chunk({"content": f"tick{i} "}), 0.15) for i in range(60)] + [(chunk({}, "stop"), 0)]
    if "run project" in content:
        return [(c, 0) for c in call("call_project", "project", {"action": "list"})]
    if "run bash" in content:
        return [(chunk({"content": "Running it. "}), 0)] + [
            (c, 0) for c in call("call_bash", "bash", {"command": "echo titi-smoke"})
        ]
    if "pin agents" in content:
        return [(chunk({"content": "Spawning two. "}), 0)] + [
            (c, 0)
            for c in call(
                "call_agent_1",
                "agent",
                {"name": "alpha", "task": "slow alpha", "kind": "subagent"},
                index=0,
                # The finish_reason belongs to the message, not to one call:
                # sending it after the first call ends the round there, and the
                # second call never arrives.
                finish=False,
            )
        ] + [
            (c, 0)
            for c in call(
                "call_agent_2",
                "agent",
                {"name": "beta", "task": "slow beta", "kind": "subagent"},
                index=1,
            )
        ]
    if "pin exec" in content:
        return [(chunk({"content": "Batch and a shell. "}), 0)] + [
            (c, 0)
            for c in call(
                "call_pin_exec",
                "agent",
                {
                    "tasks": [
                        {"task": "slow alpha", "name": "alpha"},
                        {"task": "slow beta", "name": "beta"},
                    ]
                },
                # Same reason as the batch above: a finish_reason on the first
                # call ends the round there and the `bash` call never arrives.
                finish=False,
            )
        ] + [
            (c, 0)
            for c in call(
                "call_exec", "bash", {"command": "echo qa-approval"}, index=1
            )
        ]
    if "chart table" in content:
        return [
            (chunk({"content": "The steps, slowest first.\n\n| Step | Time |\n|---|---|\n| build | 120 ms |\n| test | 45 ms |\n| lint | 8 ms |\n| fmt | 2 ms |\n\nBuild is the one to look at.\n"}), 0)
        ]
    if "mermaid answer" in content:
        return [
            (chunk({"content": "Here it is.\n\n```mermaid\nflowchart TD\n  A[Start] --> B{Go?}\n  B -->|yes| C[Done]\n  B -->|no| A\n```\n"}), 0)
        ]
    if "pin batch" in content:
        return [(chunk({"content": "Spawning a batch. "}), 0)] + [
            (c, 0)
            for c in call(
                "call_batch",
                "agent",
                {
                    "tasks": [
                        {"task": "slow alpha", "name": "alpha"},
                        {"task": "slow beta", "name": "beta"},
                    ]
                },
            )
        ]
    if "big output" in content:
        return [(c, 0) for c in call("call_big", "bash", {"command": "seq 1 20000"})]
    if "edit readme" in content:
        args = {"path": "README.md", "old_string": "# smoke ws", "new_string": "# smoke workspace"}
        return [(c, 0) for c in call("call_edit", "edit", args)]
    if "read long" in content:
        return [(c, 0) for c in call("call_long", "read", {"path": "long.txt"})]
    if "read dir" in content:
        return [(c, 0) for c in call("call_dir", "read", {"path": "."})]
    probe = re.search(r"atlas probe\s+(\w+)", content)
    if probe:
        args = {"op": "find", "query": probe.group(1)}
        return [(c, 0) for c in call("call_atlas", "atlas", args)]
    if "read readme" in content:
        return [(c, 0) for c in call("call_read", "read", {"path": "README.md"})]
    if "write file" in content:
        args = {"path": "smoke.txt", "content": "written by the smoke\n"}
        return [(c, 0) for c in call("call_write", "write", args)]
    if "think" in content:
        thinking = [(chunk({"reasoning_content": w + " "}), 0.05) for w in "let me consider this".split()]
        return thinking + [(chunk({"content": "**Thought** done. `code` here."}), 0), (chunk({}, "stop"), 0)]
    if "markdown table" in content:
        table = (
            "Here is the comparison:\n\n"
            "| Option | Latency | Notes |\n"
            "|:-------|:-------:|------:|\n"
            "| alpha | 12ms | `fast` |\n"
            "| **beta** | 340ms | 日本語のテキストです |\n"
            "| gamma | 7ms | one two three four five six seven |\n"
        )
        return words(table, 0.01) + [(chunk({}, "stop"), 0)]
    if "latex math" in content:
    # NOTE: the reply of the "latex math" keyword carries `**bold**` as well as the
    # formulas, because the chat screen only takes the markdown path when an answer
    # carries a markdown marker it knows (`has_markdown` in crates/titi-cli/src/chat.rs
    # does not recognise `$`/`$$` yet). Drop the bold once that gate knows about
    # maths and this keyword still exercises the same renderer.
        answer = (
            "**Binary search** is $O(\\log n)$ and the harmonic sum is:\n\n"
            "$$\\sum_{i=1}^{n} \\frac{i}{i+1}$$\n\n"
            "with $\\alpha \\le \\beta$ as the bound.\n"
        )
        return words(answer, 0.01) + [(chunk({}, "stop"), 0)]
    if "latex only" in content:
        # Maths and nothing else: the case that needs the chat gate
        # (`has_markdown`) to know about `$`, since no other markdown marker is
        # in the answer for it to route on.
        answer = (
            "The bound is $O(\\log n)$ and the sum is:\n\n"
            "$$\\sum_{i=1}^{n} \\frac{i}{i+1}$$\n"
        )
        return words(answer, 0.01) + [(chunk({}, "stop"), 0)]
    if "wide text" in content:
        parts = ["日本語のテキスト ", "and emoji 🎉🚀 ", "mixed 中文 text ", "done."]
        return [(chunk({"content": p}), 0) for p in parts] + [(chunk({}, "stop"), 0)]
    said = content.strip()[:60]
    return words(f"Hello from the fake server. You said: {said}") + [(chunk({}, "stop"), 0)]


_SIM_PREVIOUS: dict[str, str] = {}


def simulated_cache(request: dict) -> tuple[int, int] | None:
    """Prompt and cached tokens as a prefix cache would report them.

    With `FAKE_CACHE=1` the usage chunk is computed from the request instead of
    the fixed 100/60: the prompt is the request's model, reasoning effort,
    tools and messages (in the order a provider reads them) at four characters
    a token, and the cached part is the longest common prefix with the
    previous request of the same conversation — the one that opens with the
    same first message, so a title or review request does not stand in for the
    chat's own.
    """
    if not os.environ.get("FAKE_CACHE"):
        return None
    messages = request.get("messages", [])
    first = messages[0] if messages else {}
    key = f"{first.get('role')}:{str(first.get('content'))[:40]}"
    text = "\0".join([
        str(request.get("model")),
        str(request.get("reasoning_effort")),
        json.dumps(request.get("tools", [])),
        json.dumps(messages),
    ])
    shared = len(os.path.commonprefix([_SIM_PREVIOUS.get(key, ""), text]))
    _SIM_PREVIOUS[key] = text
    return max(1, len(text) // 4), shared // 4


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
            for event, pause in (chain(request) or spin(request) or script(last, messages)):
                self.write_chunk(b"data: " + json.dumps(event).encode() + b"\n\n")
                if pause:
                    time.sleep(pause)
            counts = simulated_cache(request) or (100, 60)
            usage = {"id": "u", "object": "chat.completion.chunk", "model": "fake", "choices": [],
                     "usage": {"prompt_tokens": counts[0], "completion_tokens": 10,
                               "total_tokens": counts[0] + 10,
                               "prompt_tokens_details": {"cached_tokens": counts[1]}}}
            self.write_chunk(b"data: " + json.dumps(usage).encode() + b"\n\n")
            self.write_chunk(b"data: [DONE]\n\n")
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18999
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
