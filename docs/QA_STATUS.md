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
| Sign-in from chat, slash command list | unverified | — | — | |
| Local photos (kitty/ghostty) | unverified | — | — | |
| Resize without drift | unverified | — | — | |
| `--prompt` one-shot | unverified | — | — | |
| `--headless` JSONL ready frame | unverified | — | — | |
| `/goal` listing, usage error, start and stop | partial | 2026-09-23 (b2041ce) | tmux 110×30 | `/go` lists `/goal`; bare `/goal` → usage; `/goal <text>` starts and stops cleanly with 0 rounds when no key is set. A live coder/reviewer run needs a provider key — not done. |

Status values: `pass`, `fail` (link the issue or commit), `partial`,
`unverified`.
