# QA_STATUS — manual checks in a real terminal

CI covers unit, integration, and golden tests. This file records what a human
or agent verified by hand (skill `tui-smoke`). Update the row you touched;
add a row for a new surface.

Last full manual pass: — (none recorded yet)

| Feature | Status | Last verified | Terminal | Notes |
| --- | --- | --- | --- | --- |
| Chat screen: send, stream, Ctrl+C stop | unverified | — | — | |
| Approval `y`/`n` for write and bash | unverified | — | — | |
| `/model` switch | unverified | — | — | |
| `/checkpoint`, `/rewind`, `/recap`, `/pause` | unverified | — | — | |
| Sign-in from chat, slash command list | partial | 2026-09-23 (d4dcc1c) | PTY (omp hub) | PTY check: `/` at line start lists the commands as before, `/help` dispatches and prints the list. Sign-in not exercised. |
| Composer `/` picker: skills mid-sentence and at line start | pass | 2026-09-23 | PTY (omp hub) and `script` 60×20 | PTY check, not visual. Now against this repo's own `.agents/skills`: `run /tui` lists `▶ /tui-smoke ·skill Manually verify a titi TUI change…`, Tab completes to `run /tui-smoke ` with the sentence intact, and the transcript keeps `run /tui-smoke` verbatim. Mid-sentence `apply /c` offers only skills (ci-and-tests, code-review, commit); an empty prefix mid-sentence lists all ten. Commands still come first at the start of a line and `/help` dispatches. Expansion proven end-to-end: with the provider pointed at a local capture server, the request's user message was `check /tui-smoke now` followed by a `# Skill: tui-smoke` section carrying the file's body (1387 bytes), and the system prompt listed all ten project skills. A flagged body reports `not expanded: SKILL.md reads like a prompt injection` and the turn still runs. At 60 cols the rows truncate without wrapping. Ctrl+D exits 0 and emits `ESC[?25h ESC[?1049l`, so the terminal is left clean. |
| Local photos (kitty/ghostty) | unverified | — | — | |
| Resize without drift | unverified | — | — | |
| `--prompt` one-shot | unverified | — | — | |
| `--headless` JSONL ready frame | unverified | — | — | |
| `/goal` listing, usage error, start and stop | partial | 2026-09-23 (b2041ce) | tmux 110×30 | `/go` lists `/goal`; bare `/goal` → usage; `/goal <text>` starts and stops cleanly with 0 rounds when no key is set. A live coder/reviewer run needs a provider key — not done. |

Status values: `pass`, `fail` (link the issue or commit), `partial`,
`unverified`.
