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

## Check what you touched, then the basics
- The changed feature, including resize (narrow ~60 cols and wide) and
  Ctrl+C mid-turn.
- Composer input, Enter, `/help`, `/model`, Ctrl+D quit leaves the terminal
  clean (cursor visible, no raw mode).
- Kitty/Ghostty image paths if rendering changed; other terminals keep text.

## Record
Update the matching row in `docs/QA_STATUS.md`: status (`pass`/`fail`/
`partial`), date, terminal (e.g. `ghostty 80x24`), one-line note. A changed
surface or widget without its own row (e.g. the composer) gets a new row. Commit it with the change or as `docs(qa): …`.
