# omp → titi: what is missing, ranked

> **Inventory as of 2026-10-08.** The wave list below is a record of what was open when it was written, not a live ledger: entries marked **ported** or **partly ported** have had work land since — the marking is per-entry status against the current tree, not a rewrite of the entries themselves, which keep the numbering and the omp analysis they were written with. Changes since are recorded in `docs/research/STATE.md`. Entries left unmarked are untouched since the inventory.

omp source: `~/.bun/install/global/node_modules/@oh-my-pi/` (`pi-coding-agent/src` = product, `pi-tui/src` = the TUI it renders with). Its user-visible surface is its settings registry — every key is a `register({ id: "…" })` — plus its slash commands and its TUI components. Ranked by user-visible value ÷ cost. Gate: M6 (MCP/skills/hooks runtime) and M9 (GPUI) are out; nothing here needs a network service, a paid API, or vendored omp code.

## Already ported (do not re-list)
Welcome lockup · todo tool · read ranges · real globs · regex grep · bash bounds · real token usage · OSC 8 links · model picker · progress row · panels + scrollbar · theme choice · `/hotkeys` key table · network tools (`fetch`/`web_search` name the call in their approval, refuse metadata hosts, and judge **every** redirect hop and **every resolved address** — `e58ad4c`, then the per-hop re-check, then the guarded resolver). Pieces still missing from these are folded into the entries below (todo reminders, read line numbers, read previews, grep context lines, startup changelog, per-turn usage row, auto-background bash).

## 1. Turn footer: token usage + turn time + cache-miss marker — **ported** (wave: usage + turn time, `9430689`; cache-miss marker in the footer row, `crates/titi-tui/src/status.rs:111-134`; the three mute switches `display.turnFooter.*`, `crates/titi-config/src/settings.rs:120` + `crates/titi-tui/src/status.rs:127`)
- omp: `display.showTokenUsage` (`modes/settings.ts:646`), `display.showTurnTime` (`:658`), `display.cacheMissMarker` (`:670`); drawn on the assistant message's usage row in `pi-tui/src/chat/chat-transcript-builder.ts:430,535` (`cacheMissMarker`, `turnElapsedMs`), preferences in `pi-tui/src/chat/display-preferences.ts:6-18`.
- **status: ported.** The footer exists: `TurnFooter` in `crates/titi-tui/src/status.rs:96+` (a `~`-free dim row — `1.4s · 3.4k prompt (2.9k cached) · 250 out · cache miss · $0.004`) is built and pushed at turn end in `crates/titi-cli/src/chat.rs:3314`, wired by `9430689`. The three toggles land too: `display.turnFooter.time`, `display.turnFooter.tokens` and `display.turnFooter.cacheMiss`, read with `settings::switch_off` in `chat.rs:144` and unset = on. `TurnFooterSwitches` travels with the row, so a muted part is never built and a row with nothing left is `None` — the screen pushes no line rather than an empty one. The money rides with `tokens`: a price is what those tokens cost, and `$0.004` alone is a figure without its subject. One deliberate departure: omp defaults its three switches to *off*, titi leaves them on, because the row has been the screen's behaviour since `9430689` and a cosmetic key must not silently delete it.
- titi: ported — `TurnUsage` incl. `cached_tokens` (`titi-engine`) is printed per turn, and `/usage` still totals the session whatever the footer says.
- value: every user, after the first answer — the only place titi's token accounting and its cache behaviour (`docs/research/prompt-cache.md`) become visible.
- cost: small. `crates/titi-tui/src/status.rs` (a new usage-row formatter; it already owns "turn timers, badges") + one call where the turn ends in `crates/titi-cli/src/chat.rs`.
- beauty: one dim row under the answer — `1.4s · 3.4k prompt (2.9k cached) · 250 out`; cache miss adds a `——— cache miss ———` divider. No new chrome, theme `muted`/`error` colours.

## 2. Transcript markdown tables (GFM) — **ported** (`ae1f35c feat(tui): render GFM tables in the transcript`)
- omp: `pi-tui/src/components/markdown.ts:1281` (`TableToken`), `:1159` (table cells as containers), `:3750-3794` (column widths, borders, theme glyphs via `theme.symbols.table`).
- titi: absent — `crates/titi-tui/src/markdown.rs:1-30` documents every construct it renders (headings … paragraphs) and tables are not among them; `grep -rn "table\|Table" crates/titi-tui/src/markdown.rs` → none; repo-wide `grep -rln "TableAlign\|pipe_table" crates/` → none.
- value: every user, constantly — models emit tables and titi currently shows raw `|`-rows.
- cost: medium. `crates/titi-tui/src/markdown.rs` only (+ golden rows in `crates/titi-tui/tests/markdown_golden.rs`); wrapping/width helpers and the theme already exist.
- beauty: box-drawn table in `MdCodeBlockBorder`/`MdHeading` colours, columns padded to width, right-align honoured; wraps or falls back to code-block form when too wide.

## 3. Terminal title run state (spinner / your-turn / waiting) — **ported** (`1a2ecc3 feat(cli): put the run state in the terminal title`)
- omp: `tui.titleState` (`modes/settings.ts:495`) and `tui.titleSpinner` (`:508`, braille/pulse/dots/line); written with `setTitle` → `\x1b]0;…\x07` in `pi-tui/src/terminal.ts:2548-2550`.
- titi: absent — `grep -rn 'SetTitle\|\x1b]0;' crates/ --include=*.rs` → none; the tab title never changes.
- value: every user who tabs away — the tab shows work state without looking back at the pane.
- cost: small. New `crates/titi-cli/src/title.rs` + one write on the existing 50 ms tick in `crates/titi-cli/src/chat.rs`; spinner frames can come from `titi-tui/src/theme` (`symbols::spinner_frames`).
- beauty: `<⠋> titi · <session>` while working, `> …` when it is your turn, `! …` when blocked on approval; no screen chrome, pure OSC.

## 4. Completion / error / ask notifications — **ported** (`f8ea043 feat(cli): notify when a turn finishes, fails or asks`)
- omp: `completion.notify` (`modes/settings.ts:1172`), `error.notify` (`:1185`), `ask.notify` (`:1217`); delivery in `pi-tui/src/desktop-notify.ts:1-20` (OSC 777/BEL, `notify-send` fallback) with the protocol chosen from `terminal-capabilities.ts`.
- titi: absent — `grep -rni "osc 777\|\x07\|bell" crates/ --include=*.rs` → none (only "mislabelled"/"relabelled" text matches).
- value: every user on a long turn; the single biggest "come back to the terminal" affordance.
- cost: small. `crates/titi-tui/src/caps.rs` (capability flag) + one emit in `crates/titi-cli/src/chat.rs`; no new crate.
- beauty: nothing on screen — the terminal raises a toast/rings once when the turn ends or fails, never on streaming deltas.

## 5. Native terminal progress (OSC 9;4) — **ported** (`e7cd8ae feat(cli): raise the terminal's own progress for a running turn`)
- omp: `terminal.showProgress` (`modes/settings.ts:391`); sequences at `pi-tui/src/terminal.ts:38-39,602`.
- titi: absent — `grep -rni "9;4" crates/` → none.
- value: every user — the tab/window shows an indeterminate bar for the whole turn.
- cost: small. `crates/titi-tui/src/caps.rs` + one pair of writes around a turn in `crates/titi-cli/src/chat.rs`.
- beauty: no rows change; the tab spinner runs while the agent works and clears on yield.

## 6. Large-paste menu — **ported** (marker + fenced block + file)
- omp: `paste.largeMenuThreshold` (`modes/settings.ts:1012`, default 100 lines); the menu re-inserts as code block / XML tags / file via `pi-tui/src/components/editor.ts:755,3226` (`handleLargePaste` / `presentLargePasteMenu`, `modes/controllers/input-controller.ts:2497,2520`).
- titi: **ported** — a paste of `paste.menuThreshold` lines (`crates/titi-config/src/settings.rs:186`, default 100, `0` turns the menu off) stages its marker exactly as before and then opens a two-row panel above the composer (`paste_panel`, `crates/titi-cli/src/pickers.rs`): **attach as a block** keeps the marker and makes what it stands for at send the fenced body; **attach as a file** writes the body to `.titi/pastes/paste-<n>.txt` in the workspace (`session_fs::write_paste`) and leaves that workspace-relative path in the draft, where `read` — whose root is the workspace — walks it in ranges instead of the model paying for the body in every request. Esc, or any key aimed at the composer, keeps the marker verbatim: omp's third row ("paste inline") is this screen's Esc, and a paste is never lost to the menu. A write that fails keeps the marker too, and says why. One deliberate difference: omp's other wrapper is `<attachment>` XML tags, and titi has no tag vocabulary for them — a code fence is titi's own way of saying "this is a block of text", and the transcript already draws one as a box.
- value: anyone pasting logs/stacktraces — chooses how it enters the prompt instead of a `[Paste]` marker.
- cost: small. `crates/titi-cli/src/keys.rs` (the paste path and the two attachments), `crates/titi-cli/src/pickers.rs` (the panel), `crates/titi-cli/src/session_fs.rs` (`write_paste`), `crates/titi-config/src/settings.rs` (the key).
- beauty: a two-row picker above the composer after a ≥100-line paste, titled `pasted 150 lines · esc keeps the marker`; the input then holds a fenced block or `.titi/pastes/paste-1.txt`.

## 7. Emoji autocomplete — **ported** (`faa2de9 feat(tui): expand emoji shortcodes and emoticons in the composer`, `effcf3e feat(cli): expand emoji and open the picker in the live composer`)
- omp: `emojiAutocomplete` (`modes/settings.ts:999`); shortcode + emoticon expansion in `pi-tui/src/prompt/emoji-autocomplete.ts:2,12` against `pi-tui/src/prompt/data/emojis.json` (33 KB, sorted longest-first).
- titi: absent — `grep -rn "emoji" crates/titi-tui/src/` → only `width.rs` (emoji = 2 cells) and a `is_valid_symbol_preset("emoji")` negative test in `theme/schema.rs:515`.
- value: everyone who types `:tada:` or `:-)`; small but noticed daily.
- cost: small. `crates/titi-tui/src/composer.rs` + a new `crates/titi-tui/src/emoji.rs` with its own shortcode table (hand-written, no vendoring).
- beauty: `:smile:` turns into 🙂 inline as you type; the composer shows the glyph, the sent message keeps it.

## 8. LaTeX math in markdown — **partly ported** (`3a49b6a feat(tui): a LaTeX subset rendered to Unicode`)
- omp: always on as part of markdown — `pi-tui/src/components/markdown.ts:21-22,1334,3078,3119` render inline and display math via `pi-tui/src/latex-to-unicode.ts` and `pi-tui/src/latex-block.ts` (1460 lines). No settings key.
- titi: absent — `grep -rni "latex" crates/ --include=*.rs` → none. A model answer with `$O(n\log n)$` reaches the screen verbatim.
- value: every user who asks anything mathematical — answers currently show raw TeX.
- cost: medium (large but mechanical). `crates/titi-tui/src/markdown.rs` + a new `crates/titi-tui/src/latex.rs` (symbol map + a small 2-D box renderer for `\frac`/matrices).
- beauty: inline math becomes Unicode (`$O(n log n)$` → italic `O(n log n)` with real super/subscripts); display math draws a centred block in `MdQuote` colours.

## 9. Vim modal editing - **ported** (`feat(cli): edit the draft the vim way when asked`)
- omp: `tui.vimMode` (`modes/settings.ts:774`, boolean, default `false`) and `tui.vimModeDisplay` (`:788`, `text`/`icon`/`none`); the state machine is `pi-tui/src/vim.ts` (`VimState`) driven by `pi-tui/src/components/editor.ts`. Enabled, it starts in **Insert**; Esc leaves Insert and steps the caret back one grapheme; a quiet Normal Esc is handed back to the host (`vimConsumesEscape`), which is where omp's own interrupt/clear lives; Normal swallows printable keys; the mode shows as `NORMAL`/`INSERT`/`VISUAL` in the status line and as a block cursor.
- titi: `editor.vim` (`crates/titi-config/src/settings.rs`, unset = **off**, `switch_on`) read before the first frame; the state, the vocabulary and the motions are `crates/titi-cli/src/vim.rs`, the key path is `Chat::vim_key` in `on_key`, and the mode is a chip in the composer's bottom border. Ported: Insert/Normal, Esc both ways, `i a I A`, the motions `h l w b e 0 ^ $` with counts, `x X s`, `d`/`c` with a motion, `dd`, `D`, `cc`, `C`, `S`, `cw`'s quirk, and the named-key normalisation omp's `VIM_NAV_KEYS` does (arrows/Home/End/Delete/Backspace). Every motion and cut goes through the composer's paste-marker rule, so a marker is one unit here too.
- not ported, and why: `j`/`k`, `gg`/`G`, `o`/`O` and linewise counts (the composer is one row - the draft has no lines to move between, so `dd`/`D`/`cc`/`C`/`S` mean the whole draft or its tail); visual modes `v`/`V` and their `y`/`d`/`c`; registers, `p`/`P` and `y`; text objects (`iw`, `a(`, `i"`, `ap`); `u` (the composer keeps no undo stack, so a single-level undo would be a lie about what "undo" means here - left out rather than faked); `^`/`$` are the draft's ends, not a line's, and `w`/`b`/`e` use the draft's own whitespace-delimited word rule (the one `ctrl+w` and `alt+←/→` use) rather than vim's keyword/punctuation classes. The named keys ↑/↓ keep scrolling the transcript: omp maps them onto `j`/`k`, which have no subject in one row.
- value: the vim-using slice of users, every session; the loudest complaint of a terminal-agent switcher.
- cost: paid - one module, one config key, one chip, and the tests the vocabulary needs.

## 10. Status line: presets, custom segments, context gauge — **partly ported** (presets + gauge: `6de3f1d`, `8f2072e`, `/statusline` command `8f20ced`)
- omp: `statusLine.preset` (`modes/settings.ts:152`, default/minimal/compact/full/nerd/ascii/custom), `statusLine.leftSegments`/`rightSegments` (`:277,284`), `separator` (`:174`), `contextLine` (`:196`, the accent line between the segments doubles as a context gauge), `sessionAccent` (`:227`), `transparent` (`:239`); implementation `pi-tui/src/status-line/presets.ts`, `segments.ts`, `context-usage.ts`.
- titi: partly — only the default preset is painted, hard-coded (`crates/titi-tui/src/status_bar.rs:77`, snapshot fields at `:15-40`); no preset/segment setting exists (`grep -rn "preset" crates/titi-tui/src/status_bar.rs` → the doc comment only).
- value: every user, from the first frame — the bar is the biggest piece of always-on chrome.
- cost: medium. `crates/titi-tui/src/status_bar.rs` + `crates/titi-config/src/settings.rs` (new `statusLine.*` keys) + snapshot plumbing in `crates/titi-cli/src/chat.rs`.
- beauty: `/statusline minimal|compact|full`; the box composer's top rule fills with an accent-coloured context gauge with an embedded `72% · 128k` label.

## 11. Generation rate (tok/s) — **ported** (`64bc233 feat(tui): show the generation rate on the working row`)
- omp: `composer.tokenRate` (`modes/settings.ts:138`); `token_rate` segment at `pi-tui/src/status-line/segments.ts:760,1294`.
- titi: absent — `grep -rn "token_rate\|tok/s" crates/` → only a symbol table entry in `theme/symbols_data.rs`.
- value: every user, while waiting — turns "is it stuck?" into a number; free, since deltas are already counted.
- cost: small. `crates/titi-tui/src/status.rs` (a rolling delta estimate) + one field through the existing snapshot in `crates/titi-cli/src/chat.rs`.
- beauty: `42 tok/s` next to the thinking level on the working row, last reading kept between turns.

## 12. Auto-resume the last session — **ported** (`--continue`/`-c` + `session.autoResume`)
- omp: `autoResume` (`modes/settings.ts:30`, off by default) — resumes the most recent session in the cwd.
- titi: `--continue`/`-c` reopens the newest session in the agent directory and `session.autoResume` (`crates/titi-config/src/settings.rs`, unset = off) does the same at every launch; with nothing to resume it starts fresh and says so. omp's is cwd-scoped where titi's is agent-dir-wide — the workspace is not in `SessionMeta` yet, which is a separate gap.
- value: every user who closes the terminal and comes back — today the past turns are only reachable via `/sessions` or `Ctrl+X`.
- cost: small. `crates/titi-cli/src/main.rs` (one flag + one config read) + a call into `titi-core::session::store` (already lists sessions); the restart path in `crates/titi-cli/src/chat.rs` already replays a session.
- beauty: the lockup's fact block shows yesterday's session instead of a fresh one; no new chrome.

## 13. Bare exit / bare slash commands — **ported** (bare `exit`/`quit`/`q`, and a second Enter once the session has turns)
- omp: `input.bareExitOnEmptySession` (`modes/settings.ts:882`) and `input.bareSlashCommands` (`:895`, Enter twice to confirm once messages exist).
- titi: a bare `exit`/`quit`/`q` (or `/exit`, `/quit`) in the composer leaves the chat — straight away before the first turn, and on a second Enter once the session has turns (`Chat::exit_word`, the same two-press window Ctrl+C uses). `input.bareSlashCommands` (a bare `/` meaning a command) is not ported.
- value: every user in their first minute — typing `exit` (or `q`) to leave is muscle memory.
- cost: tiny (single file). `crates/titi-cli/src/chat.rs` submit path only.
- beauty: `exit` quits (or shows `press Enter again to quit` mid-session) instead of costing an API call.

## 14. Double-Escape action — **ported** (a second Esc on an empty composer opens the rewind; `doubleEscapeAction`'s `tree` and `none` are not offered)
- omp: `doubleEscapeAction` (`modes/settings.ts:867`, rewind / tree / none); handled in `pi-coding-agent/src/modes/controllers/input-controller.ts:582-586`, legacy mapping at `config/settings.ts:2579`.
- titi: a second Esc on an empty composer opens the rewind cut (`Chat::escape`, the same 2 s window Ctrl+C uses), which is exactly what `/rewind` does; Esc on an open list or picker still closes that first. `doubleEscapeAction` is not a setting: `tree` and `none` are not offered, and the chord is always the rewind.
- value: every user who wants to undo the last turn — one chord instead of typing `/rewind`.
- cost: small. `crates/titi-cli/src/chat.rs` only (reuse the existing 2-press timer).
- beauty: an empty composer + Esc Esc opens the rewind picker that `/rewind` already shows.

## 15. Tight layout
- omp: `tui.tight` (`modes/settings.ts:560`) — drops the 1-cell left/right padding.
- titi: absent — `grep -rn "\btight\b" crates/titi-tui/src/caps.rs` → a comment only; the composer border reserves `BOX_PADDING_X` (`crates/titi-tui/src/composer.rs:243-300`).
- value: narrow terminals (≤80 cols) — 2 more usable columns for every row.
- cost: small. `crates/titi-tui/src/composer.rs` + `renderer.rs` padding constant; one new config key.
- beauty: settings-level only; transcripts and the box composer sit flush to the edge.

## 16. Working-row shimmer
- omp: `display.shimmer` (`modes/settings.ts:572`, classic / KITT / disabled); `setShimmerMode` at `pi-tui/src/theme/shimmer.ts:40`.
- titi: absent — `grep -rli shimmer crates/` → none; the busy row is a plain spinner (`crates/titi-tui/src/status.rs`).
- value: every user, for the whole of every turn — the "it is alive" signal.
- cost: small. `crates/titi-tui/src/status.rs` (+ theme colour ramp); driven by the tick that already advances the spinner.
- beauty: a soft highlight sweeps the width of the working text; `disabled` leaves today's static muted row.

## 17. Pinned live agents + subagent preview
- omp: `display.pinnedAgents` (`modes/settings.ts:591`, off/collapsed/full) and `display.subagentLivePreview` (`:610`).
- titi: partly — subagent runs are transcript sections (`crates/titi-tui/src/transcript.rs`) and a hub roster overlay exists (`crates/titi-tui/src/hub.rs`), but nothing is pinned above the editor and live current-tool calls are not shown: `grep -rn "pinned" crates/titi-tui/src/hub.rs` → none.
- value: anyone running parallel subagents — sees what each is doing without `/hub`.
- cost: medium. `crates/titi-tui/src/hub.rs` (a jump-list widget) + `crates/titi-cli/src/chat.rs` (layout slot above the composer) + engine events already exist.
- beauty: 1–3 rows above the composer — `⠋ Explore · grep cache` with a hover highlight; an expander lists them all.

## 18. Read / preview display preferences
- omp: `readLineNumbers` (`tools/settings.ts:149`), `read.renderMarkdown` (`:180`), `read.toolResultPreview` (`:278`), `read.defaultLimit` (`:161`, default 300).
- titi: partly — ranges and caps are in (`crates/titi-tools/src/fs.rs:262,268`, `READ_MAX_LINES`/`READ_MAX_CHARS`), but read results carry no line numbers by default and are never rendered as markdown: `grep -rn "line_numbers" crates/titi-tui/src/` → only `diff.rs` (diff gutter).
- value: every user reading the agent's reads in the transcript.
- cost: small–medium. `crates/titi-tools/src/fs.rs` (numbering) + `crates/titi-tui/src/markdown.rs` (reuse the new table/render path for `.md`).
- beauty: read blocks show `  12 │` gutters; a `.md` read renders as the same styled transcript markdown.

## 19. Transcript display preferences — **partly ported** (`/details` hides a section; the `Compacted` divider is the fold)
- omp: `display.smoothStreaming` (`modes/settings.ts:622`), `display.collapseCompacted` (`:693`), `display.hideToolActivity` (`:634`).
- titi: `/details` sets the visibility of each transcript section (`Chat::details`, `titi_tui::markdown::SectionMode`), which covers "hide tool activity"; the post-compaction collapse is the `LineKind::Fold` divider the engine's `Compacted` event pushes (`▸ folded 14 turns · 22k tokens`, expanded by `/details folded expanded`), which is `display.collapseCompacted`'s job. Missing: `display.smoothStreaming` — the reveal is per delta today, not paced (`grep -rli smooth crates/` → none), and it is the one item here that would make every streaming frame time-dependent.
- value: every user after the first compaction — a folded divider keeps the live transcript short.
- cost: small. `crates/titi-tui/src/transcript.rs` (a summary divider with an expander) + the compaction event in `crates/titi-cli/src/chat.rs`.
- beauty: one line — `▸ folded 14 turns · 22k tokens` — expandable with the existing `/details` machinery.

## 20. Auto-background bash
- omp: `bash.autoBackground.enabled` (`exec/settings.ts:108`) / `bash.autoBackground.thresholdMs` (`:250`) — a command that outlives the threshold becomes a background job.
- titi: absent — `grep -rn "autoBackground" crates/` → none; long commands instead hit the 300 s deadline and the turn loses the output (`crates/titi-tools/src/pipe.rs`).
- value: every user who runs a dev server or a long test run — the single most common way a turn is ruined today.
- cost: medium–large. `crates/titi-tools/src/pipe.rs` + job plumbing in `crates/titi-engine/src/runtime.rs` (there is a job concept) + a chip in `crates/titi-tui`.
- beauty: the tool chip turns into `⚙ bash · backgrounded (bg_7)` and the output lands as a follow-up message when it finishes.

## 21. `ask` tool (structured question to the user) — **ported** (`d52f1cf` + `ebf8fc6`, held off `master` until the chat answers it)
- omp: `ask.enabled` (`tools/settings.ts:801`), `ask.timeout` (`modes/settings.ts:1198`, auto-select a recommended option), `ask.notify` (`:1217`); tool in `pi-coding-agent/src/tools/ask.ts:101,545`.
- **status: ported, and deliberately not the whole of omp.** The tool (`crates/titi-tools/src/ask.rs`) hands an `AskRequest` to an `AskSink` the session installs (`ToolHandler::set_ask`, the shape `bash`'s background door already had) and parks the turn on the answer; the engine's `SessionAsk` turns one ask into one `EngineEvent::AskRequested` plus a wait that `EngineCommand::AnswerAsk` resolves, and a cancel raises the session's `Interrupt`, which answers `Cancelled`. Read-tier, so a question is never itself an approval prompt; a **subagent cannot ask** (its registry is built without the door and its `ask` says so, where omp aborts the whole turn — forwarding would attribute a question to a worker the user never started); and there is **no timeout**, where omp's auto-selects the recommended option, because a deadline would have to answer a question only the user can. One question per call, not omp's list. Held off `master` until the chat answers an `AskRequested`: the engine half is complete and tested (`tests/ask.rs`, 3 tests, each with a fail-without), the surface half is not written.
- titi (before this): absent — no tool offers the user a choice: `grep -rn "\"ask\"" crates/titi-tools/src/` → none; the only user prompt is the y/n approval.
- value: every user on an ambiguous turn — the agent asks with options instead of guessing or stalling.
- cost: medium. `crates/titi-tools/src/` (new tool) + `crates/titi-engine/src/tool_loop.rs` (a blocking question) + a picker overlay in `crates/titi-tui/src/panels.rs`.
- beauty: a small picker above the composer — `Which DB? ▸ sqlite / postgres / ask me later` — with a countdown when `ask.timeout` is set.

## 22. Agent reactions (emoji badge)
- omp: `tui.reactions` (`modes/settings.ts:469`) — the agent may attach an emoji badge to the user's bubble.
- titi: absent — `grep -rli reaction crates/` → none.
- value: every user, small delight and a fast read of the model's mood/verdict.
- cost: medium. `crates/titi-soul/src/builder.rs` (invite it) + `crates/titi-tui/src/transcript.rs` (badge row) + engine parse of the marker.
- beauty: `👍` on the right edge of your prompt bubble; no layout shift, one theme colour.

## 23. Session tree with filters + branch summaries — **partly ported** (`/tree` view + branch switch; the filter: `feat(cli): filter what the tree shows`)
- omp: `treeFilterMode` (`modes/settings.ts:908`, `default`/`no-tools`/`user-only`/`labeled-only`/`all`, default `default`) as the mode the panel opens in, plus alt+key shortcuts and filter tabs inside it (`pi-tui/src/overlays/tree-selector.ts:15,573`); `branchSummary.enabled` (`session/context-settings.ts:528`); `doubleEscapeAction: "tree"` (`:867`); commands `/branch`, `/tree`, `/move`.
- titi: `/tree` draws one session's stored entries as the tree they are (`SessionStore::open` for every branch, `walk` for the path to the leaf) in the panel above the composer: indented by depth, the path to the leaf marked `•`, the leaf named `✓ current`, the title counting the entries off the path. Enter moves the leaf to the row under the cursor and replays that path to the engine (`crates/titi-cli/src/session_fs.rs:branch_at` → `EngineCommand::RestoreHistory`), so the entry left behind stays in the store: a branch, not a rewind. The filter is in too: `treeFilterMode` names the mode the panel opens in and `alt+f` cycles it, and a hidden entry's children are re-parented onto the nearest entry the filter keeps (`TreeFilter`/`lay_out`, `crates/titi-cli/src/pickers.rs`), so hiding the tool traffic leaves one tree rather than a row of orphans at depth 0.
- not ported, and why: omp's `all` would be titi's `default` (titi's tree hides nothing to begin with, where omp's hides bookkeeping entries) and `labeled-only` has nothing to filter by (a titi entry carries no label); a generated one-line summary per branch (`branchSummary.enabled`) needs a model call, which is the engine's half and not this wave's; `/move` (moving a branch to another parent) has no store operation; `doubleEscapeAction: "tree"` is moot, because the chord is already the rewind (`feat(cli): rewind on a double Escape`).
- value: users exploring multiple approaches in one repo.
- cost: paid for the view, the branch switch and the filter; the summary is the one piece left.
## 24. Mermaid fenced blocks as ASCII diagrams
- omp: `tui.renderMermaid` (`modes/settings.ts:417`); `pi-tui/src/chat/fence-figure.ts:38-68` and `components/markdown.ts:3141`, cache in `pi-tui/src/theme/mermaid-cache.ts:1`.
- titi: absent — `grep -rli mermaid crates/` → none; titi's markdown draws every fence as a box (`crates/titi-tui/src/markdown.rs:281`).
- value: every user reading an architecture answer — a flowchart beats a wall of `graph TD` text.
- cost: large. `crates/titi-tui/src/markdown.rs` + a new box-drawing graph layout module (its own tests).
- beauty: the fence is replaced by an ASCII flowchart in `MdCodeBlock` colours, redrawn only when the block closes.

## 25. Auto-graph under numeric tables
- omp: `tui.autoGraph` (`modes/settings.ts:442`, smart/always/off); `pi-tui/src/charts/chart-plan.ts`, `chart-svg.ts`, `table-data.ts`.
- titi: absent — `grep -rln "chart\|Chart" crates/titi-tui/src/` → none.
- value: users who ask for data ("compare these numbers") — a chart makes the answer readable at a glance.
- cost: large (depends on #2 for the table, needs a plot renderer + a graphics path).
- beauty: a sparkline/bar block in theme colours directly under the table; `off` leaves the table alone.

## 26. Spelling, typo detection, word autocomplete
- omp: `spelling.typoDetection` (`modes/settings.ts:941`), `spelling.autocomplete` (`:954`, n-gram / SmolLM / macOS dictionary), `spelling.autocorrect` (`:986`).
- titi: absent — `grep -rn "autocorrect" crates/` → none; `\btypo\b` hits are the word "typo" in test text, not a checker.
- value: everyone typing long prompts — fewer misspelled instructions.
- cost: large and platform-coupled (dictionary access; SmolLM downloads weights). Skip until the cheap items are gone.

## 27. `eval` tool (Python/JS with a workpool)
- omp: `eval.tools.enabled`, `eval.py`, `eval.js`, `eval.autoProvision`, `eval.workpool.freshAgents`; `pi-coding-agent/src/eval/`.
- titi: absent — `grep -rn "eval\b" crates/ --include=*.rs` → only a `tool.eval` icon in `crates/titi-tui/src/theme/symbols_data.rs:257,504,751` and unrelated text in `crates/titi-memory/src/embed.rs:120`.
- value: users doing data work; the tool that makes the agent usable as a scratchpad.
- cost: large (kernel lifecycle, output capture, isolation) and it drags in the workpool/subagent runtime — leave it behind M6/M7.

## Recommended next three
1. **Transcript markdown tables** — every model answer with a table is broken today and no other worker is in the file; `crates/titi-tui/src/markdown.rs` (+ golden rows); the user sees a real bordered, aligned table instead of `|`-rows.
2. **Turn footer: token usage + turn time + cache-miss marker** — the data is already in `TurnUsage` and the row is the only way titi's cache behaviour becomes visible; `crates/titi-tui/src/status.rs` + one call at turn end in `crates/titi-cli/src/chat.rs`; the user sees `1.4s · 3.4k prompt (2.9k cached) · 250 out` under the first answer.
3. **Terminal title run state** — costs one new small file and one line on the tick that already exists; `crates/titi-cli/src/title.rs` + one call in `crates/titi-cli/src/chat.rs`; the user sees the spinner/`>`/`!` in the terminal tab from the first turn.

Caveat: #2 and #3 share the single `chat.rs` call site (the file AGENTS.md serializes); #1 is fully independent. If all three run at once, land #1 and #2 first and add #3's one line after — or hand #3 to the same worker as #2.

## Not verified / not portable
- Left unranked on purpose (checked, too large or outside the gates): `xd://` tool devices (`tools.xdev`, `tools.xdevDocs`, `tools.xdevInlineDevices` — a whole device-URL tool layer titi has no counterpart to), `snapcompact.*` (a compaction-shaping package; titi's `titi-engine/src/compaction.rs` already decides the same policy by hand), `worktree.*` / `/worktree`, `loop.*` (titi has `/loop`), `hindsight`/`mnemopi` memory backends (titi has `titi-memory`), `share.*` (titi has `titi-core/src/share.rs`), and everything gated by M6/M9 (`skills.*`, `hooks`, `mcp.*`, `extensions`, `plugins`).
- `find.enabled` (`tools/settings.ts:521`) is a judge-model semantic grep; it auto-enables only on omp's native judge model, so it is not portable under the "no paid API" rule.
- omp's image stack is only partly comparable: titi has a kitty graphics budget (`crates/titi-tui/src/image.rs:1-230`) but no `terminal.showImages`, `images.autoResize`, `tui.maxInlineImages` limits or `images.describeForTextModels`, and I did not trace how titi attaches images to a request, so that gap's size is unmeasured.
- omp's statusline/editor keybinding *names* already match titi's (`crates/titi-tui/src/keybindings.rs:32+`), so `tui.editor.*` is not a gap. I ran neither program: every omp claim is a source read, every titi claim a grep plus file read.
