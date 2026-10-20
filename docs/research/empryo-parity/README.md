# Empryo → titi: ideas worth re-implementing, ranked

> Source: Empryo's **public** documentation only — https://empryo.com/docs
> (every page also serves markdown at `<path>.md`), `llms.txt` /
> `llms-full.txt`, and the README of https://github.com/proxysoul/Empryo.
> Nothing was downloaded, installed, run or vendored (AGENTS.md: Empryo is a
> closed, untrusted binary). Every entry below is an idea re-described in our
> own words; the implementation is titi's own Rust.

Inventory taken 2026-10-21 against `238e911`. omp-sourced gaps live in
[`../omp-parity/GAP.md`](../omp-parity/GAP.md); an idea both products share is
listed once, there.

Status words: **wave-1** (being built in the 2026-10-21 wave), **open**,
**gated** (PLAN milestone not started), **have** (titi already does it).

## UI/UX

| # | Idea | What the user sees | titi today | Status |
|---|---|---|---|---|
| 1 | Speaker rows | `◆ you · 14:02` / `● titi · model · 14:02` above each message | `you │` / `titi │` gutter, no time, no model | wave-1 |
| 2 | Action rail | one `N actions ▾` block per round; a row per call: verb, target, result summary, right-aligned duration | one chip per call (`▸ name`, `✓ preview`), no verb or duration | wave-1 |
| 3 | Same-kind folding | `Read ×5` with the newest targets listed | none | wave-1 |
| 4 | Steering under the turn | `↯ <note>` attached to the running turn | a new `you` block | wave-1 |
| 5 | `@file` mentions | fuzzy file picker on `@`, file attached on send | none | wave-1 |
| 6 | Shell-style history | ↑/↓ on the first/last line recall prompts in place | Ctrl+R panel only | wave-1 |
| 7 | Fuzzy slash list | `/ctx` finds `/context` | prefix filter | wave-1 |
| 8 | Draft stash | Alt+S / Alt+P, auto-stash a long cleared draft | draft lost on clear | wave-1 |
| 9 | Status segments | context gauge coloured by fill, `✎ +N ~M` agent edits, session `$` | gauge one colour, no edits or cost | wave-1 |
| 10 | Command palette | Ctrl+K fuzzy over every command, panel and setting; value pickers for arguments | `/` list only; Ctrl+K is delete-to-end | open |
| 11 | Mode cycle | Shift+Tab / Ctrl+D cycle modes; read-only postures (architect, challenge) | `/plan` `/duck` `/done`, no cycle key | open |
| 12 | Turn footer actions | `26s · $0.19` plus copy / fork / review | time + tokens + cost, no actions | open |
| 13 | Checkpoint rail | gutter marks per checkpoint; Ctrl+B/Ctrl+F step through, send forks from there | `/rewind`, double-Esc, no step-forward | open |
| 14 | Tabs | several conversations in one window sharing map and memory, cross-tab file claims | one conversation per window | open |
| 15 | Error log | Alt+R panel of every error this session | inline red rows | open |
| 16 | Per-model usage | `/usage` per model and period, cache hit rate | session totals | open |

## Capability

| # | Idea | Why it matters | titi today | Status |
|---|---|---|---|---|
| 17 | `project` tool | lint / typecheck / test / build resolved from the repo, one structured result, no guessing `npm test` vs `npx jest` | the model guesses a `bash` command | wave-1 |
| 18 | Diagnostics diff after edit | the tool result says which errors an edit introduced or fixed | none | open |
| 19 | Independent review verdict | clean-context reviewer of the session's edits, PASS/FAIL/PARTIAL, one key to apply | `/goal` has a reviewer; no standalone `/review` | open |
| 20 | Pre-commit gate | an agent `git commit` runs lint + typecheck first | none | open |
| 21 | Symbol-addressed edits | edit a function by name, all-or-nothing batches | text/hashline edits | open |
| 22 | Compound refactors | rename/move symbol through the LSP in one call | none | gated (needs LSP client) |

## Efficiency

| # | Idea | Why it matters | titi today | Status |
|---|---|---|---|---|
| 23 | Measured prompt budget | bytes per turn by part (system, tools, map, diff, history) | `/context` estimates | wave-1 (bench) |
| 24 | Size-budgeted tool results | every result capped so a single grep is not re-billed for the rest of the session | 40k-char cap per result | open |
| 25 | Cache-break diagnostics | a notice naming what changed in the prefix and what it cost | none | open |
| 26 | On-demand tool loading | rare tools behind a search tool, decided per session so the cache holds | every tool every turn | open |
| 27 | Co-change ranking | the map ranks files that change together | import graph + touched set | open |

## Not taken

- Embedded Neovim, phone remote, voice, computer use: outside a terminal
  agent's core; titi's CUA theme is PLAN M3 and separate from this list.
- `/orchestrate` workflow graphs and routines beyond `/loop`: titi has `/graph`
  and `/council`; a general graph runtime waits for M7.
