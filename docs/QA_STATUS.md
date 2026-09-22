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
| Composer `/` picker: skills mid-sentence and at line start | pass | 2026-09-23 (d4dcc1c) | PTY (omp hub) and `script` 60×20 | PTY check, not visual. `please run /smo` opens the picker mid-sentence with `·skill` rows only; Tab completes to `please run /smoke-note ` with the sentence intact; `/smo` at line start lists the same skills after the commands; the transcript keeps `/smoke-note` verbatim. A flagged body showed `/smoke-bad not expanded: SKILL.md reads like a prompt injection` and the turn still ran (then failed on the missing key, as expected). At 60 cols the rows truncate without wrapping. Ctrl+D exits 0 and emits `ESC[?25h ESC[?1049l`, so the terminal is left clean. |
| Local photos (kitty/ghostty) | unverified | — | — | |
| Resize without drift | unverified | — | — | |
| `--prompt` one-shot | unverified | — | — | |
| `--headless` JSONL ready frame | unverified | — | — | |
| `/goal` listing, usage error, start and stop | partial | 2026-09-23 (b2041ce) | tmux 110×30 | `/go` lists `/goal`; bare `/goal` → usage; `/goal <text>` starts and stops cleanly with 0 rounds when no key is set. A live coder/reviewer run needs a provider key — not done. |

Status values: `pass`, `fail` (link the issue or commit), `partial`,
`unverified`.
