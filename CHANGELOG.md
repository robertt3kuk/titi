# Changelog

All notable user-facing changes to titi, newest first.

## Unreleased — 2026-10-08

### Added

- `--continue` / `-c` reopens the newest session in the agent directory instead of starting blank; with nothing to resume, it starts fresh and says so. `session.autoResume`, set in a config layer, does the same at every launch and is off unless set.
- A bare `exit`, `quit`, `q` (or `/exit`, `/quit`) in the composer leaves the chat instead of being sent to the model — straight away before the first turn, and on a second Enter once the session has one.
- A second `Escape` on an empty composer opens the rewind cut, the same as `/rewind`.
- A `bash` command that outlives its background threshold — `TITI_BASH_BACKGROUND_MS`, in milliseconds, 60 000 by default — is handed over as a background job instead of holding the turn; `/jobs cancel` stops it. The threshold is also a setting, `bash.autoBackground.thresholdMs` (1 to 3 600 000; 0 or anything larger is refused at startup), which the environment variable overrides when set.
- A `todo` checklist tool: the agent writes a multi-step list, moves one item at a time through statuses, and a `todo N/M · name` chip tracks it in the transcript.
- A splash screen at startup: a `TITI` wordmark, a tagline, facts, chords and one of twenty `Tip: …` hints, picked by the session id.
- `read` takes `offset` and `limit`; `glob` matches real glob patterns (`**/`, `[a-z]`, `{rs,toml}`); `grep` takes a regular expression with `ignore_case` and a `glob` file filter, and a `path` may name one file.
- `titi genome` manages the prompt map (`on`, `off`, `limit <n>`), `titi genome check` reports diagnostics per line, and `titi genome lsp` serves a stdio LSP; `/genome` repeats the same verbs in the chat.
- Each turn carries a `<diff>` block: the working tree's changes next to the genome map, so the model sees its own edits.
- `titi --login <provider>` signs in with OAuth in the browser and stores the subscription with its refresh token; a device-code flow is offered where the provider supports it, and bare `/login` opens a picker.
- Screen switches read from the same settings layers: `statusLine.preset` and `statusLine.contextLine`, `notify.completion` / `notify.error` / `notify.ask` (desktop notifications, unset = on), `terminal.progress` (OSC 9;4, unset = on), `composer.tokenRate` (generation rate on the working row, unset = on), `genome.enabled` (unset = on) and `genome.limit` (1–64, default 24).

### Changed

- `bash` output reaches the model as the text a terminal would show: colour codes and window titles removed, `\r` progress bars reduced to their last state — on a pty and on a pipe alike, through timeouts and interrupts too.
- A `bash` run is bounded on both paths: a deadline (`timeout_secs`, 300 by default), a whole-process-group stop on Ctrl+C, and a 64 KiB cap per stream kept (first and last on a pipe). Zero nowhere means "no deadline"; a foreground dev server, watcher or `tail -f` is refused before it runs.
- `read` answers at most 30,000 characters and 2,000 lines without a `limit`; a longer file arrives paged as `[lines 1-N of M; read on from offset N+1]`, not cut in the middle. A directory read lists it; a binary is named as one.
- `edit`, when `old_string` is not there as written, matches loosely by lines — ignoring surrounding whitespace and typographic quotes — and when it fits exactly one run, edits it with the file's own indentation and says the match was loose.
- A `grep` path outside the workspace or non-existent is an error, not a silent search everywhere; a hit on a very long line shows about 300 characters around the match.
- `/usage` counts what the provider reported for prompt, completion and cached tokens instead of an estimate; `/budget` no longer says "estimated".
- Cancelled or failed turns still report their token usage, so `/usage` and the budget do not lose a paid round.
- Model switches in headless JSONL now answer `ModelSwitched`, so a client can switch a model before a turn.

### Fixed

- The provider catalog can no longer be redeclared by a project `.titi/config.yml`, so a cloned repository cannot redirect `openai.base_url` and take the first request with its own key; the `settings` tool also no longer exposes `providers` or key-bearing leaves.
- A turn before its first visible token that fails walks the rest of the model list in order and says so (`model fallback: a → b`); a `401` and an unknown model are not retried.
- Network tools (`fetch`, `web_search`, `settings`) say in their approval prompt what they will do, and addresses named `localhost` or the metadata host are refused.
- Provider errors are quoted as the provider wrote them (`rejected (HTTP 401): …`) instead of `upstream status N`.
- A `edit` on a CRLF or BOM-carrying file matches from the LF text and keeps the file's line endings and byte-order mark.
- Transient provider retries wait between rounds (500 ms doubling to 8 s) instead of hammering, always stoppable.
- A turn can no longer replay its reasoning when it retries, and a panic leaves its message visible instead of taking the screen down with itself.
