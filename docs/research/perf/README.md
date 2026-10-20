# Performance: how it is measured, where it stood, what moved

`scripts/bench.py` measures titi end to end with the standard library only.
This document is the method, the baseline, the hotspots with their evidence,
and the numbers after the fixes that shipped with it. Everything below was
produced by that script on the machine below; nothing is estimated except the
token counts, which are bytes/4 — the estimator titi itself uses
(`titi_core::compaction::estimate_tokens`).

## Reproduce

```bash
scripts/bench.py --quick --out /tmp/bench.json     # ~80 s with a warm build
scripts/bench.py --out /tmp/bench.json             # full run, ~2.5 min
scripts/bench.py --only prompt,resume --skip-build # one or two suites
scripts/bench.py --bin ./titi-old --example-dir ./examples-old   # a saved baseline
```

It builds release (`titi-cli` plus the three `bench_*` examples), copies this
repository's `HEAD` into a scratch git repo with a `long.txt`, writes an agent
dir whose `config.yml` points at the scripted provider
(`scripts/fake-provider.py`, port 18999; the script starts one if nothing
listens and only ever *reads* the provider's `requests.jsonl`), and prints a
markdown table. `--out` writes the same numbers as JSON. Every row carries `n`,
min, median and p95, and the machine's load average is recorded with it.

| suite | what it measures | how |
|---|---|---|
| `startup` | `titi --help` wall; the TUI on a 120x40 PTY: first byte, first painted frame (the banner text arrives), bytes written in the next 2 s, CPU and peak RSS of the session, with and without the genome index | `pty.fork` + `os.wait4` (its rusage is what `/usr/bin/time -l` prints) |
| `genome` | cold index, prompt-map render, no-op refresh, one edited file by `refresh` and by `apply_changes`; on this repo and on a synthetic 5,000-file tree (half TypeScript, half Rust, imports between them) | `crates/titi-genome/examples/bench_genome.rs`, public API only |
| `tools` | `glob`, `grep`, `read` (cold and cache-warm) through `ToolHandler::invoke`, on the same two trees | `crates/titi-tools/examples/bench_tools.rs` |
| `render` | markdown renderer on a 2,000-message mixed session (prose, headings, lists, code, tables) at 120 columns: whole session, per message, the rows one 40-row frame needs, and a 300-delta streamed answer re-rendered from scratch each delta | `crates/titi-tui/examples/bench_render.rs` |
| `resume` | a 200-turn session reopened with `--continue` on a PTY: first bytes, screen filled, repaint after ten typed characters and ten PageUps | headless runner builds the session, PTY reopens it |
| `streaming` | `--headless` against the scripted provider, with and without the genome: time to ready, first token, turn total, CPU, RSS; plain text and the 60-chunk `slow` keyword | JSONL on pipes |
| `prompt` | every request body of a 9-request session (hello, a README read, the user edits a tracked file, six more turns), split into system prompt, tool schemas, genome map, diff, history | the provider's request log, filtered by a per-run tag |

What the examples cannot reach: `titi-cli` keeps its transcript renderer
private (`transcript.rs`, owned by another change), so the render example times
the public markdown renderer, which the transcript calls for every assistant
row, and `resume` covers the real screen end to end instead.

## Machine

Apple M1 Pro (`sysctl -n machdep.cpu.brand_string`), 10 cores, 16 GiB,
macOS 27.0.1, rustc 1.98 stable, release profile (`lto = "thin"`,
`strip = "symbols"`). The host was shared with other builds the whole time:
load average ranged from 8 to 28 across runs. Wall-clock rows therefore carry
noise; the A/B pairs quoted per fix were run back to back, alternating, and the
tables use minimums where a row is a latency and medians where it is a
distribution. Commit `2497d97` is the "after"; the baseline is `238e911`.

## Hotspots, ranked by what a user pays

### 1. The first frame waited for the code map's index (fixed)

Evidence: `tui first byte` 246 ms with the genome on against 16 ms with
`TITI_NO_GENOME=1`; first frame 264 ms against 34 ms; the process burns ~1.0 s
of CPU (`tui session CPU`) against 70 ms. `Engine::start_inner` called
`GenomeHandle::spawn`, which parses every source file on the caller's thread
before returning — the surface cannot draw until the engine exists. A sample of
the index shows the time is tree-sitter itself (`ts_parser_parse`,
`ts_lex`), so the work is real; what was wrong was where it ran.

Fix (`b7514c1`): `GenomeHandle::spawn_deferred` hands the first walk to the
worker thread; `wait_ready` blocks for it, and `genome_map` calls it first, so
the first prompt still sees the whole tree and no reader gets an empty graph.

### 2. Context the model already had was sent again on every turn (fixed)

Evidence (`prompt` suite, bytes): the map (2.8 KB) and a dirty tree's diff
(1.5 KB) ride on the newest user message and stay in history, so request *n*
carried *n* copies. By request 9 the baseline sent 16.7 KB of stale maps and
5.8 KB of stale diffs, 24% of the body, and the share grows with every turn.

Fix (`505c86b`): a block is left out of the new message when the newest copy of
its kind in the history is byte-identical. Only the new message changes, so the
provider's cache prefix is untouched.

### 3. The graph re-rank hashed and cloned a path per file per iteration (fixed)

Evidence: a one-file update that changes an export costs 3.7 ms on this repo
and 56 ms on 5,000 files (`apply_changes_one`), against 3.9 and 16 ms for a
no-op refresh of the same trees. `graph::rank` ran 20 PageRank iterations over
`HashMap<String, _>`.

Fix (`fb01b65`): iterate over `Vec`s indexed by file; same visit order, so the
sums are added in the same order (a test keeps the old form as an oracle).

### 4. One thread got the big files (fixed)

Evidence: `index_cold` on this repo is ~250 ms of wall for ~1.0 s of CPU on ten
cores; the files are cut into contiguous path-ordered slices and a 15k-line
`chat.rs` or 3k-line `runtime.rs` pins one thread while the rest idle.

Fix (`87e15b7`): threads claim the next file from a shared counter, in list
order (so `prioritize` still takes the urgent files first).

### Found, not fixed

* **Tool schemas are 11.3 KB — 47% of the first request, ~2.8k tokens, sent on
  every request.** `agent` (2.1 KB), `todo` (1.5 KB), `ask` (1.1 KB) and `grep`
  (0.9 KB) lead; 17 tools in all. They are static and cache well, but they
  fill the context window from the first turn. The descriptions are prose the
  model leans on, so shortening them is a product decision, not a refactor;
  exposing the rarely used ones (`settings`, `memory`, `git_commit`,
  `diagnose`, `hashline_edit`) on demand would save ~2.4 KB without touching a
  word.
* **The system prompt is 9.4 KB in a repo like this one** (`AGENTS.md` and the
  skill list ride in it). Constant per session, so cheap for the cache, but it
  is a fixed cost of every request.
* **The genome costs ~48 MB of resident memory** on this repo (59 MB with it,
  11.6 MB without) and `refresh` copies the whole graph into a private value on
  every update, which is why a no-op refresh is 3.7 ms here and 13 ms at 5,000
  files. Skipping the copy when the walk finds nothing stale would remove it.
* **`grep` over 5,000 small files takes ~100 ms** (a miss included): one thread,
  one `read_to_string` and a per-line regex call per file. Parallel reads with
  the cap applied in path order should cut it by about the core count
  [inference, not tried].
* After fix 1 the first frame with the genome on is still ~95 ms against 34 ms
  without it; the likely cause is the indexer's threads competing with the
  startup thread for cores [inference]. Leaving one core free for the first
  walk is untried.

### Measured and fine

* Markdown rendering: 2,000 mixed messages (505 KB) render in 16 ms total,
  0.01 ms per message median, 0.04 ms for the rows of one frame. A streamed
  answer re-rendered from scratch on each of 300 deltas costs 0.4 ms median and
  0.8 ms p95 (31 KB at the end) — linear in the message, not a hotspot at
  realistic lengths.
* The real TUI over a resumed 200-turn session: 17 ms to first byte, 36 ms to a
  full screen, 1.2 ms to repaint a typed character (minimum; the medians move with the
  harness's 50 ms read quantum) and no growth against a 60-turn session.
* 60 consecutive turns in one headless process: first-token and total time are
  flat (47 ms / 290 ms at turn 1, 47 ms / 293 ms at turn 60). No O(n²) in the
  turn loop, history handling or session writes at this size.
* `read` 0.2 ms, `glob` 2 ms (5 ms at 5,000 files).

## Numbers

Baseline `238e911` against after `2497d97`. Medians unless a row says min.
"Genome on/off" is the default against `TITI_NO_GENOME=1`.

### Startup (TUI on a 120x40 PTY, ms)

| case | baseline | after | change |
|---|---|---|---|
| `titi --help` wall | 4.7 | 5.0 | noise |
| first byte, genome on | 246 | 15 | −94% |
| first frame, genome on | 264 | 94 | −64% |
| first frame, genome off | 34 | 33 | — |
| session CPU, genome on | 1031 | 1127 | the index is the same work, later |
| peak RSS MB, genome on / off | 59.5 / 11.5 | 59.9 / 11.7 | — |
| headless ready (genome on) | 261 | 11 | −96% |

### Genome (ms, minimum of 7; two back-to-back rounds)

| case | baseline | after |
|---|---|---|
| repo `index_cold` | 255 / 248 | 210 / 214 |
| repo `refresh_noop` | 3.87 / 3.83 | 3.52 / 3.79 |
| repo `refresh_one_edit` | 7.75 / 7.52 | 4.80 / 5.07 |
| repo `apply_changes_one` | 3.67 / 3.55 | 1.33 / 1.36 |
| repo `project` (map render, 2.8 KB) | 0.04 | 0.04 |
| 5k `index_cold` | 383 / 399 | 316 / 341 |
| 5k `refresh_noop` | 16.0 / 13.3 | 11.8 / 12.3 |
| 5k `refresh_one_edit` | 72.7 / 72.5 | 51.4 / 52.1 |
| 5k `apply_changes_one` | 56.1 / 57.1 | 37.3 / 37.9 |
| 5k `project` | 0.54 | 0.53 |

### Streaming (scripted provider, ms)

| case | baseline | after |
|---|---|---|
| genome off: first token / turn total (plain) | 32 / 283 | 31 / 282 |
| genome off: first token (slow) | 30 | 29 |
| genome on: first token / turn total (plain) | 48 / 288 | 224 / 464 |
| genome on: ready + first token, from process start | 308 | 235 |

With the genome on, the first turn of a *fresh process* now pays the index the
startup used to pay (`ready` 261 ms → 11 ms, first token 48 → 224 ms): the
bench submits the instant the process is ready. A person types for longer
than that, and `ready` + first token, which is what a script sees, is lower
(308 → 235 ms). Later turns are unchanged. The `genome` rows of "turn CPU" in
`bench.py`'s own table include the index for the same reason.

### Prompt (9-request session, bytes)

| | system | tools | genome fresh | genome stale | diff fresh | diff stale | history | JSON framing | total | est. tokens |
|---|---|---|---|---|---|---|---|---|---|---|
| baseline, summed | 84,249 | 101,430 | 25,083 | 69,675 | 8,718 | 15,983 | 257,344 | 20,665 | 583,147 | 145,786 |
| after, summed | 84,249 | 101,430 | 2,787 | 22,296 | 1,453 | 7,265 | 257,170 | 17,031 | 493,681 | 123,420 |
| baseline, request 9 | 9,361 | 11,270 | 2,787 | 16,722 | 1,453 | 5,812 | 40,952 | 4,285 | 92,642 | 23,160 |
| after, request 9 | 9,361 | 11,270 | 0 | 2,787 | 0 | 1,453 | 40,869 | 3,342 | 69,082 | 17,270 |
| request 1 (both) | 9,361 | 11,270 | 2,787 | 0 | 0 | 0 | 20 | 474 | 23,912 | 5,978 |

−15% of all bytes sent over the session, −25% on the ninth request, and the
stale-copy columns stop growing. "Fresh" is the copy in the newest user message,
"stale" the copies in older ones; "framing" is JSON escaping and keys.

### Tools and render (medians, ms; no code changed, so this is the noise floor)

| case | baseline | after |
|---|---|---|
| repo `glob **/*.rs` / `*.toml` | 2.4 / 2.3 | 2.5 / 2.3 |
| repo `grep` rare / common / ignore-case | 24 / 24 / 30 | 24 / 20 / 28 |
| repo `read` cold / warm | 0.18 / 0.13 | 0.23 / 0.15 |
| 5k `glob **/*.rs` / `*.toml` | 5.1 / 5.2 | 6.5 / 5.4 |
| 5k `grep` rare / common / ignore-case | 150 / 87 / 182 | 182 / 66 / 152 |
| render, 2,000 messages | 16.2 | 16.2 |
| render, one 40-row frame | 0.04 | 0.04 |
| render, per-delta of a growing 31 KB answer | 0.42 | 0.41 |

### Resumed 200-turn session (minimum over 3 runs, ms)

| case | baseline | after |
|---|---|---|
| first byte / screen filled | 17 / 37 | 18 / 36 |
| repaint, one typed character | 1.2 | 1.2 |
| repaint, one PageUp | 1.3 | 1.4 |
| session CPU | 171 | 157 |

## Limits

* One machine, shared and busy; read ratios, not absolute milliseconds.
* The scripted provider answers instantly, so streaming numbers are titi's
  overhead, not a model's latency.
* `prompt` measures bytes on the wire and an estimate of tokens; it does not
  see the provider's cache, so a byte saved is not necessarily a billed token
  saved — but every byte counts against the context window.
* The transcript renderer in `titi-cli` is only measured through the real TUI
  (`resume`), not in isolation.
