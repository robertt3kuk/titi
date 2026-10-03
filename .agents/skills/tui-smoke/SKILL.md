---
name: tui-smoke
description: Manually verify a titi TUI change in a real terminal and record it in docs/QA_STATUS.md. Use after changing titi-tui or titi-cli screen code, keys, slash commands, rendering, or when asked to smoke-test the chat.
---

# tui-smoke

TUI changes are the one exception to "nothing compiles locally" (AGENTS.md).

## Run
```bash
cargo run -p titi-cli                       # chat screen
cargo run -p titi-cli -- --prompt "read Cargo.toml and tell me the version"
cargo run -p titi-cli -- --headless --approval write   # expect {"ready":true,"protocol":1}
```
A live model turn needs a key the user set with `titi --set-key <provider>`.
Without one the turn fails in the open — that is still a valid check of the
screen. Never ask for or paste a key.

For an agent without a real terminal, drive it through a PTY (omp:
`hub start` with `pty: true`, then `logs`/`send keys`) and inspect the
escape output; say in the report that it was a PTY check, not a visual one.

## A real turn without a key
Streaming, tools, approvals, errors and steering need a model. A scripted
local one gives real HTTP turns through the real binary, no key involved:

1. `mkdir -p $T/agent $T/ws` in a scratch dir; put the provider block from
   the docstring of `scripts/fake-provider.py` in `$T/agent/config.yml`
   (`credential_required: false`, `base_url` on `127.0.0.1`). It lives in
   the agent dir: the provider catalog is never read from a project file.
2. `(cd $T && python3 scripts/fake-provider.py 18999 &)` — it answers by
   keyword (`run bash`, `slow`, `edit readme`, `fail429`, …, listed in its
   docstring) and logs every request body to `$T/requests.jsonl`.
3. Write a scenario (`pip install pyte` once) and run
   `python3 scripts/pty-drive.py scenario.json`. Set `env` explicitly —
   `TITI_AGENT_DIR=$T/agent`, `TITI_NO_GENOME=1`, `TERM`, `PATH` — so no real
   key or agent dir leaks in. Each `shot` prints the screen as numbered rows.
4. Check the screen and `requests.jsonl` (what the model was really sent).
   `--prompt` and `--headless` need no PTY: pipe them with the same `env`.

Use a fresh agent dir per scenario: the chat resumes the newest session. To
stop the server, kill the `python3` process by name, never with
`pkill -f` on a pattern your own shell command line also contains.

## Check what you touched, then the basics
- The changed feature, including resize (narrow ~60 cols and wide) and
  Ctrl+C mid-turn.
- Composer input, Enter, `/help`, `/model`, Ctrl+D quit leaves the terminal
  clean (cursor visible, no raw mode).
- Kitty/Ghostty image paths if rendering changed; other terminals keep text.

## Record
Update the matching row in `docs/QA_STATUS.md`: status (`pass`/`fail`/
`partial`), date, terminal (e.g. `ghostty 80x24`, or `PTY + scripted
provider`), one-line note. A changed
surface or widget without its own row (e.g. the composer) gets a new row. Commit it with the change or as `docs(qa): …`.
