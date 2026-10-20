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

### 5. `grep` read and matched every file on one thread (fixed)

Evidence: 92–95 ms over 5,000 small files whatever the pattern (a miss
included) and 17–20 ms over this repository; `glob` over the same trees is
4.6 and 2.0 ms, so the walk is not where the time goes: it is
`read_to_string` and a regex call per line, one file after another.

Fix (`a470151`): the walk only lists files, in the order the answer lists them
(name order per directory, credential and glob filters applied). Threads claim
files from a shared counter and the results are folded in listing order, so
the lines, the 500-line cap and the "more matches" count are those of the
sequential walk. A file formats no more lines than the folded prefix still has
room for, which keeps a dense pattern from formatting what the cap discards.
Trees under 16 files stay on the calling thread. A test keeps the old
sequential walk as an oracle and compares byte for byte on a tree that crosses
the cap mid-file and holds every kind of skipped file.

Result: 1.8× on 5,000 files (94 → 52 ms) and 2.2× on this repository
(18 → 8 ms), well short of the core count. The serial part (directory listing,
sort, `stat`) and the host's load are in it; the rest was not profiled.

### 6. A refresh that found nothing still copied the graph (fixed)

Evidence: through `SharedGenome` (the engine's path) a no-op refresh cost 6.5 ms
here and 23 ms at 5,000 files, against 3.4 and 10.4 ms for the same walk on an
in-place `Genome`: the difference is the copy of the whole graph that is made
before anyone knows whether the walk changes it.

Fix (`4a78144`): list the tree first and ask the published snapshot whether the
listing is current (same root, same path set, no open buffer, every file past
the metadata gate). If it is, no copy and no publish: the generation does not
move and readers keep the same `Arc`. Anything else takes the old path. Result:
6.5 → 3.4 ms and 23 → 11 ms, equal to the in-place numbers; what remains is the
walk.

### 7. The first walk competed with the thread that draws (fixed)

Evidence: with the index on, the first frame was still ~95–120 ms against
~34 ms without it (fix 1 moved the index off the caller, not off the cores).
A sweep of the number of parse threads, 30 launches per row on this 10-core
host at load 8–14, first frame in ms:

| parse threads | all (10) | 9 | 8 | 7 | 5 | 4 | no index |
|---|---|---|---|---|---|---|---|
| median first frame | 111 | 78 | 51 | 41 | 35 | 34 | 34 |

(Two sweeps: the column for 8 comes from the first, where "all" measured 97.)
The cost to the index (`bench_genome --quick`, `index_cold`, median of 3,
repo / 5k, ms): 238 / 400 on every core, 223 / 341 on 8, 224 / 369 on 6,
281 / 426 on 4.

Fix (`546f8ef`): the first walk of a deferred handle parses on two fewer
threads than cores from eight cores up, one fewer below that. Later walks and
the synchronous `spawn` use every core. One free core, as first proposed, only
recovered a third of the gap; two free cores is where the first frame is within
~20 ms of having no index and the index is not slower. The sweep is one
machine under load: on an idle one the gap is smaller and the constant in
`first_walk_workers_for` is the place to retune.

### 8. Tool schemas carried repeated wording (trimmed, 11%)

Fix (`e3a3623`): wording that repeated itself or another field went: a tool
description restating its parameter (`ask` `free_text`, `todo` `op`), the same
example in a description and its parameter (`glob`, `grep`, `read`), a second
sentence paraphrasing the first (`agent`, `project`, `settings`). Every
parameter, enum, default, limit and "when to use it" sentence stayed. Tests pin
which parameters each tool has and which carry a description, and a byte
ceiling. The result is 12,239 → 10,881 bytes per request (−11%, ~340 tokens) —
not the 20% asked for. What is left in the big five (`agent`, `todo`, `ask`,
`project`, `grep`: 4.3 KB) is guidance on when and how to call them, and
cutting it is a behaviour change to be judged with the model in the loop, not a
wording fix.

### Found, not fixed

* **Tool schemas are still 10.9 KB — 46% of the first request, ~2.7k tokens,
  sent on every request.** See the proposal below.
* **The system prompt is 9.4 KB in a repo like this one** (`AGENTS.md` and the
  skill list ride in it). Constant per session, so cheap for the cache, but it
  is a fixed cost of every request.
* **The genome costs ~48 MB of resident memory** on this repo (59 MB with it,
  11.6 MB without). Fix 6 removed the per-turn copy but not the graph itself.
* **`grep` over 5,000 files is still ~52 ms**, ten times what listing the same
  files costs (`glob`, 4.6 ms). The gap to the core count is not explained and
  was not profiled. A no-op refresh is now only its walk (3.4 ms here, 11 ms at
  5,000 files).

### Proposal: load the rarely used tools on demand (not implemented)

This is a product decision, so it is measured here and not built. After fix 8
the schemas of five tools that most turns never call are 2,225 bytes, 20.4% of
the 10,881 sent per request:

| tool | bytes |
|---|---|
| `settings` | 632 |
| `memory` | 616 |
| `hashline_edit` | 446 |
| `git_commit` | 349 |
| `diagnose` | 182 |
| **together** | **2,225** |

Offering them behind one small tool that lists the unloaded ones and loads a
schema when the model asks (a catalogue of five names and one line each, ~300
bytes) would send ~8.96 KB instead of 10.88 KB: −1.9 KB per request net (−17.7%,
~480 tokens), −27% against the 12.2 KB baseline with fix 8 included. Over the
9-request session that is ~17 KB of the 490 KB sent. Two things to weigh
first: a schema added mid-session changes the tools block, which in the usual
provider layout comes before the conversation in the cache prefix [inference,
not measured here], so the turn that loads one would pay for the
prefix again; and the model has to know to ask, which is a behaviour to test
with a real model rather than the scripted provider. A cheaper first step,
needing no meta tool: `web_search` (215 bytes) is offered even when no search
provider is configured, where every call answers "no provider configured";
leaving it out of that registry costs nothing.


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

### Wave 2 — grep, no-op refresh, tool schemas, first walk

Baseline `a7b1352` (master when this wave started) against after `546f8ef`.
Both binaries were run back to back, alternating, two rounds; latencies are
the minimum of each round's runs unless a row says median, so each cell shows
the two rounds. Three rows are new in `bench.py` (`7a28fe0`):
`refresh_shared_noop`, `grep_miss`, `grep_dense`. Host load average 8–16.

#### `grep` (`tools` suite, min of 15 runs, ms)

| case | repo, baseline | repo, after | 5k files, baseline | 5k files, after |
|---|---|---|---|---|
| rare pattern | 18.2 / 18.2 | 8.6 / 8.7 | 92.4 / 94.1 | 52.8 / 51.9 |
| `pub (fn\|struct)` in `*.rs` | 14.8 / 15.0 | 7.2 / 6.2 | 50.3 / 51.0 | 29.1 / 29.1 |
| miss (no hit anywhere) | 17.3 / 17.4 | 8.2 / 8.2 | 93.5 / 95.3 | 52.4 / 53.6 |
| dense (hits far past the cap) | 20.4 / 20.7 | 9.5 / 8.3 | 94.5 / 95.8 | 52.5 / 51.7 |
| ignore case | 19.7 / 20.0 | 9.0 / 8.1 | 94.9 / 97.3 | 52.7 / 50.0 |

`glob` (2.0 / 4.6 ms) and `read` (0.15 ms) did not move.

#### Genome refresh through the publish point (min of 7 runs, ms)

| case | baseline | after |
|---|---|---|
| repo `refresh_shared_noop` | 6.7 / 6.3 | 3.4 / 3.4 |
| 5k `refresh_shared_noop` | 22.8 / 23.0 | 11.4 / 10.5 |
| repo / 5k in-place `refresh_noop` (no copy to begin with) | 3.4 / 10.4–10.7 | 3.4 / 10.4–11.0 |
| repo / 5k `refresh_one_edit` | 4.9 / 47–51 | 4.8 / 48 |

The shared no-op now costs what the in-place one does: the walk.

#### Startup (TUI on a 120x40 PTY, ms)

| case | baseline | after |
|---|---|---|
| first frame, genome on, median (`bench.py`, 7 launches) | 119.7 / 124.6 | 101.0 / 94.8 |
| first frame, genome on, median (40 alternating launches) | 117 / 112 | 69 / 84 |
| first frame, genome off, median | 33.7 / 32.2 | 35.5 / 36.5 |
| first byte, genome on, median | 15.0 / 15.0 | 15.3 / 18.5 |
| session CPU, genome on, median | 1226 / 1154 | 1134 / 1119 |
| headless first token (`streaming`, min of 7) | 241 / 225 | 227 / 211 |

The index itself did not get slower (the last row waits for it).

#### Tool schemas (`prompt` suite, bytes)

| | baseline | after |
|---|---|---|
| `tools` per request (18 tools) | 12,239 | 10,881 |
| `tools`, summed over the 9-request session | 110,151 | 97,929 |
| request 1, total / est. tokens | 24,878 / 6,219 | 23,520 / 5,880 |
| request 9, total / est. tokens | 70,051 / 17,512 | 68,693 / 17,173 |
| whole session, total / est. tokens | 502,395 / 125,598 | 490,173 / 122,543 |

Per tool (bytes of the schema as sent): `agent` 2,010 → 1,552, `todo` 1,397 →
1,194, `ask` 1,091 → 888, `project` 968 → 884, `grep` 846 → 732, `read` 697 →
624, `glob` 687 → 618, `bash` 614 → 561, `edit` 536 → 479, `settings` 676 →
632, `git` 480 → 462. `memory`, `hashline_edit`, `git_commit`, `write`,
`web_search`, `fetch`, `diagnose` are unchanged (`memory` lives in
`titi-memory`). The baseline is larger than the first table's 11,270 because
the `project` tool (968 B) landed in between.

## Limits

* One machine, shared and busy; read ratios, not absolute milliseconds.
* The scripted provider answers instantly, so streaming numbers are titi's
  overhead, not a model's latency.
* `prompt` measures bytes on the wire and an estimate of tokens; it does not
  see the provider's cache, so a byte saved is not necessarily a billed token
  saved — but every byte counts against the context window.
* The transcript renderer in `titi-cli` is only measured through the real TUI
  (`resume`), not in isolation.
