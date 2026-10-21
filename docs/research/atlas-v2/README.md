# atlas v2 — a live, task-conditioned map of the repository

Design note, 2026-10-21, updated after the `genome` → `atlas` rename (master `134041c`). Code is referred to by
its current names (`titi-atlas`, `Atlas`, `AtlasHandle`, `SharedAtlas`, `atlas.limit`); every `file:line` is
of `134041c`. Numbers marked **pilot** were measured for this note on the corpus clones
(`/tmp/titi-qa/genome-corpus`, deepened to 150+ commits); the scripts are in `pilot/` and reproduce them.
Anything not measured is marked **[INFERENCE]**.

### Owner decisions (2026-10-21)

1. **No model call on the hot path.** Index-only by default; a model-assisted mode may exist later only as
   an explicit opt-in.
2. **`atlas.limit` is replaced by token budgets in one clean cutover**: `atlas.budget` (tokens), which the
   allocator splits between the skeleton and the lens. No fallback to the old key; the CHANGELOG says so.
3. **The `atlas` tool is on by default from step 6** (read tier); the step-9 A/B may revise that.
4. **The background git-history probe is accepted**: off the hot path, bounded (commit cap and time cap),
   cached per `HEAD`, never blocking the first frame or a turn.
5. **A persisted index, if any, lives in the agent directory**, never in the repository.

## 0. The thesis

Today the genome answers *"what is important in this repository?"* — one static ranking, rendered as the
same 24 files whatever the user asks. The valuable question is different: **for this task, where do things
live, what matters, and what would a change touch?** Three measured facts say that is where the value is:

1. The static map finds the files a real commit changed **52 % of the time in the top 24 and 29 % in the top 10**
   (pilot: 80 commits, 8 repos, `commit_eval.py`/`commit_ppr.py`). A map seeded by the prompt, with the same graph
   plus git co-change, finds **86 % in the top 24 and 71 % in the top 10** — more than twice as good with
   fewer than half the files shown. Same graph, same cost class (a PageRank over ≤ 20 k nodes is single-digit ms).
2. Static quality has plateaued: the audit and corpus round took the static map from p@10 0.30 → 0.41 and
   recall@24 0.70 → 0.87 on a hand-written truth list (`docs/research/atlas-eval/README.md`), and the pilot shows
   what that truth list hides: on large repos (okio, ky, gson, ripgrep) the static map holds 1–43 % of the
   files a task actually touches.
3. The published localisation literature agrees on the shape of the answer: a graph over code plus a few
   **query tools** beats both lexical/embedding retrieval and an upfront map alone (§1 evidence).

atlas v2 pushes five things to their maximum and measures them with one replayable task-level eval:

| axis | one-line target |
|---|---|
| A1 relevance | per-prompt personalised PageRank (seeds: prompt, trace, touched, diff), delivered as a small *lens* that never invalidates the prompt cache |
| A2 precision | binding-level definition→use edges, re-export chains, decayed git co-change edges, test↔source pairing |
| A3 line content | signatures for the top symbols, role tags, named dependents, all under a hard token budget the allocator honours |
| A4 query | one `atlas` tool (find / outline / uses / impact / related) over the same index the prompt and the LSP use |
| A5 freshness | stat-poll tier with no new dependency, post-`bash` resync, importer re-resolution, one path normaliser |
| A6 measurement | replay real commits: recall@k of the changed files via map and via tool, tokens, latency; baseline vs v2 |

## 1. Evidence

**Aider** (repo map blog, `aider/repomap.py`): files are nodes, edges connect referencer → definer, the map is
the highest-ranked *definitions* that fit `--map-tokens` (1 k default), found by binary-searching the budget
with a sampled token estimator. The personalisation is the part titi lacks: nodes for files in the chat, files
and identifiers mentioned in the message get personalisation mass (`100 / n_files` each); an edge's weight is
multiplied ×10 for a mentioned identifier, ×10 for a snake/camel identifier of ≥ 8 chars, ×0.1 for `_private`
and for names defined in more than 5 files, ×50 when the referencer is a chat file; reference counts enter as
`sqrt`; the rank of each file is then split over its out-edges so that the output is ranked **(file, identifier)
pairs**, not files. (`aider.chat/2023/10/22/repomap.html`; `repomap.py` `get_ranked_tags`.)

**LocAgent** (arXiv 2503.09089): a heterogeneous graph (directory/file/class/function; contain/import/invoke/
inherit) and **three** tools — `SearchEntity` (name → BM25 fallback, three detail levels), `TraverseGraph`
(typed multi-hop BFS, output as an indented tree) and `RetrieveEntity` — reach file Acc@5 **94 %** on
SWE-bench-Lite against 61.7 % for BM25, 80–85 % for the best embedding models, 74–79 % for Agentless'
procedural localisation and 69–90 % for SWE-agent/OpenHands/Moatless. Their ablation point that matters here:
tools designed for the model (compact, tree-shaped) beat tools designed for humans, and multi-hop reach is where
agents without a graph lose (accuracy falls with hop distance for every method).

**Agentless** (arXiv 2407.01489): a fixed three-step pipeline — repo-tree → file, file skeleton → class/function,
then edit locations — with no agent loop, 32 % on SWE-bench-Lite at a fraction of the cost: a *skeleton*
(signatures, not bodies) is enough for the model to choose where to look. This is the case for signatures in
the map line (A3).

**RepoGraph** (arXiv 2410.14684): a plug-in line/def-level graph whose ego-network around the lines a task
mentions improves several SWE-bench pipelines — the same idea as the lens: a *k-hop neighbourhood of the
mentioned code*, not a global ranking (A1).

**Empryo** (public docs, ideas only): the *Genome* is a ranked graph with `(→N)`, one-line symbol summaries,
a git **co-change** signal and live updates; the *Marionette* is a pre-pass that attaches a per-request map
(`loci` = `file:lines · symbol · role`, `strands` = callers / files that change together / what an edit could
affect, `unmapped`) *under* the prompt, never rewriting it, with a `none` mode that is index lookup only,
about 0.2 s and zero tokens. Two lessons: the per-request map is a distinct artefact from the repo map, and
a model-free mode is the default-worthy one. Its rule "the Marionette only locates, it never suggests a fix"
is right for a cheap model and is kept here by construction: atlas is index-only, no model call.

**titi's own practices note** (`docs/research/agent-practices/README.md` #16, #17): hybrid is right — a
small upfront map plus just-in-time retrieval; "first expose a read-only `symbols`/`refs` tool from the genome
the engine already holds"; budget tied to repo size; A/B on/off in the eval. This note is that plan with numbers.

**Cache constraints** (`docs/research/prompt-cache.md`, perf README prompt table): the map moved out of the
system prompt into the newest user message so one re-ranked file stopped costing every token behind it;
"stale" copies of the map in older messages were 22 KB over a 9-request session even after dedupe. Any v2
design must keep the prefix byte-stable and keep per-turn additions small and non-repeating.

### Pilot measurements (this note)

Method (`pilot/commit_ppr.py`): per repo take the 10 most recent non-merge commits that modify 1–8 indexed
source files and have a message ≥ 20 chars; check out the **parent** tree; index it with the public API; the
task is the commit message; truth is the modified files that exist at the parent. Co-change uses only the 150
commits before the parent. 80 tasks over cobra, express, requests, ripgrep, ky, gson, okio, sinatra.

| ranking of the files | recall@10 | recall@24 |
|---|---|---|
| static (today: graph rank × role factors) | 0.29 | 0.52 |
| lexical only (path tokens + export-name matches of the message) | 0.57 | 0.64 |
| personalised PageRank, seeds = lexical matches, edges both ways | 0.65 | 0.82 |
| + git co-change edges (decay 0.5^(age/50 commits)) | 0.71 | 0.86 |
| mix (seeded + 5 % static) | 0.71 | 0.86 |

Per repo, static → mix (recall@10 / @24): cobra 0.65/1.00 → 0.80/1.00; express 0.54/0.54 → 0.54/0.84;
requests 0.71/0.87 → 0.97/0.97; ripgrep **0.03/0.42 → 0.92/0.92**; ky 0.15/0.23 → 0.68/0.85; gson
0.17/0.33 → 0.47/0.57; okio **0.01/0.06 → 0.56/0.75**; sinatra 0.07/0.70 → 0.77/1.00.

Ablations (subset where path seeds exist, n = 66, so compare only within the block): forward-only edges (a
file spreads rank only to what it depends on) 0.51 / 0.81 versus both directions 0.65 / 0.83 — **dependents
matter**, a change flows to its users; without co-change 0.62 / 0.82 versus with 0.65 / 0.83; seeding with
export-name matches added nothing over path tokens on commit *messages* (0.65 / 0.83 either way), which is
plausible: messages name concepts, a user's prompt or a stack trace names symbols. The pilot therefore
under-states A1's seeds (S2, S3 below) and says nothing about them.

Edge coverage (`pilot/edge_cover.py`, 145 files modified in 2+-file commits): 61 % have an import/use edge
to another file of the same commit, 58 % a co-change edge from earlier history, **77 % have at least one, 43 %
both**. The two edge kinds are complementary, not redundant (A2).

Caveats, honestly: truth is "files changed", not "files needed"; commit messages are shorter than prompts;
small repos (cobra 36 files) are trivially covered at k = 24; the pilot's seeds and PageRank are Python
re-implementations over the public graph (`pilot/graph_dump.rs`), not the Rust code; messages sometimes name
the file. The eval in §A6 closes these.

## 2. Principles

1. **Index-only, no model call** in the hot path. Latency budget for building a lens: p95 ≤ 10 ms on 5 k
   files, ≤ 25 ms on 20 k; map tokens ≤ a configured budget; both measured in the eval.
2. **One query layer, many front ends**: the prompt skeleton, the lens, the `atlas` tool and the LSP are thin
   renderers over `Atlas::{find, outline, uses, impact, related}` on an immutable snapshot (`Arc<Atlas>`, the
   publish point that exists today: `shared.rs:36`, `live.rs:122`).
3. **Append-only prompt**: never edit an earlier message; per-turn additions are small and not repeated.
4. **Evidence tags**: every relation shown to the model names how it is known (`import`, `use`, `co-change`,
   `test`), so nothing in a lens is a guess it cannot weigh.
5. **No new crate** unless owner-approved; everything below is `std` + the crates the workspace already has.
6. **Sanitise at the render boundary** (paths with `<`/`>` are dropped today, `project.rs:35`; signatures and
   lens strings get the same treatment plus control-character stripping and length caps — repo text is
   attacker-controlled input to the prompt).

## A1 — Task-conditioned relevance

### Current state
* One static rank per file: weighted PageRank, `DAMPING 0.85`, 20 iterations, uniform start with role shares
  (`graph.rs:7-8, 46-150`); personalisation exists only as a *render-time multiplier* — `TOUCHED_BOOST = 3.0`
  on files the session read/edited (`project.rs:12, 45-53`), which is far too weak for the leaves one actually
  edits (their static rank is the minimum), and not seeded by anything in the prompt.
* The engine rebuilds the map each turn from the published snapshot: `atlas_map` asks the worker for
  `Priority(touched)` + `Resync`, waits 40 ms (`runtime.rs:128, 147-186`), renders `project_with_pending`,
  attaches it to the **newest user message** (`prompt_with_context`, `runtime.rs:212-234`) only when it differs
  byte-for-byte from the newest copy in history (`previous_copy_matches`, `runtime.rs:245-251`).
* `touched` records the path *as the model spelled it* (`tool_loop.rs:235`), so an absolute or `./` path never
  matches an index key (audit finding 7); diff paths join it but are repo-root-relative in a monorepo subdirectory.

### Target design — the *lens*
`Atlas::lens(&Seeds, Budget) -> Lens`, computed per turn on the immutable snapshot.

**Seeds** (a sparse vector over files, plus optional symbols; weights are starting points to be tuned in the eval):

| seed | source | weight |
|---|---|---|
| S1 paths | `@file` mentions, any token that is or ends with an indexed path | 5 per file; a directory mention seeds its children at 1 |
| S2 identifiers | prompt tokens that equal an exported name (case-sensitive 3, case-insensitive/split-token ≥ 5 chars 1), backticked spans ×2 | → the *defining* file |
| S3 traces | `path:line[:col]` in compiler output / stack traces (Rust, Python `File "x", line N`, JS `at f (x:1:2)`, Go, Java `File.java:N`), failing test names (`test_foo`, `FooTest`, `--- FAIL: TestX`) | 4 for the file, and for the enclosing symbol (needs item spans, A3); a test name seeds its test file *and* its paired source (A2) |
| S4 session | files read (1) / edited (3), exponential decay over tool calls (half-life ≈ 6) | |
| S5 git | uncommitted + staged files (2); files changed on this branch since the merge-base (1.5); the existing RECENT commits as a *continuous* weight `0.5^(age/24h)` instead of a 48 h cliff (`recency.rs:24`) | |
| S6 lexical | path-token and basename matches of prompt words (pilot: this alone is 0.57 @ 10) | 0.5 per token, 2 for the stem |

**Propagation.** Personalised PageRank on the same weighted edges as today **plus the reverse edge at 0.5**
(pilot: forward-only loses 0.14 recall@10) **plus co-change edges (A2)**, damping 0.85 (restart 0.15), 20
iterations, dangling mass returned to the seeds. Weights keep the existing evidence model (`graph.rs:11-32`:
import and scoped use 1.0, guess 0.25, `sqrt` of summed evidence, auxiliary-source edges ×0.7). The final score
is `ppr + ε · static` with ε ≈ 0.05 (pilot "mix"), so a prompt with no signal degrades to today's map, and a
prompt with signal is dominated by its neighbourhood. Following Aider, rank is then split over the out-edges
so the lens ranks **(file, symbol)** pairs: the symbols shown under a file are the ones carrying seeded flow, not
the ones with the highest global user count.

**Cost.** A CSR (compressed sparse row) copy of the edge list is built once per publish, not per query.
20 iterations × E edges with E ≈ 5 N: N = 20 k → ~2 M multiply-adds ≈ 2–4 ms **[INFERENCE, to be measured in
the eval's latency column]**; the pilot's unoptimised Python did 80 tasks including indexing in 48 s total.

**Delivery and cache.** Two artefacts, both on the newest user message, neither ever edited afterwards:
1. **Skeleton** `<atlas>`: the repo structure, static + role-aware, as today but token-budgeted (A3). Sent with
   the first user message of a session and after a compaction; thereafter only as a **delta** (`<atlas-delta>`:
   files that entered/left the skeleton, `(→N)` changes ≥ 20 %) when membership drifts (Jaccard of the shown set
   < 0.85). The skeleton is rendered *grouped by directory with sorted order*, not rank order, so a rank wobble
   that does not change membership changes no byte — today's exact-bytes dedupe (`runtime.rs:245`) then holds
   far more often.
2. **Lens** `<atlas-lens>`: ≤ 400 tokens, present only when seeds exist; lists ≤ 12 files with `why` tags
   (`seed`, `uses X`, `used by Y`, `co-change`, `test of Z`), per-file top symbols with signatures. It is
   omitted when its file set equals the previous lens's. Older lenses are masked to a one-line stub by the same
   pass that masks old tool results at compaction (`docs/research/compaction-context`), so a 30-turn session pays
   ≈ 400 tokens once per *change of focus*, not 400 × turns. Today's per-change cost is ≈ 600 tokens
   (24-file map, 2.4 KB measured on this repo); v2 averages less while carrying 2× the recall.

### Expected effect
recall@10 0.29 → ~0.7 on the pilot (seeds S1–S6 are a superset of what the pilot used; richer prompts and
traces should add), tokens per changed turn ≤ today's, no prefix invalidation.

### Risks
* Over-seeding from chatty prompts (every word hits a file): cap the number of seeds (top 16 by weight), require
  ≥ 5 chars for case-insensitive matches, and add the *`unmapped`* tag — a `terms: no match for …` line — so the
  model knows when the lens is guessing (Marionette's `unmapped`).
* The lens anchors the model on wrong files: tags say *why*; weights are tuned on the eval, which reports
  precision@k (share of lens files that were *not* in the changed set) alongside recall.
* Prompt-injection through identifiers: sanitiser at render (principle 6).

### Proof
`atlas::lens` unit tests on fixtures (seeded neighbourhood outranks the global top; no seeds ⇒ identical to
static; a leaf touched file appears); an engine test with the scripted provider that the lens rides the newest
message, is absent when unchanged, and that `requests.jsonl` shows byte-identical history prefixes across turns;
the A6 eval's `lens` condition against `static`.

## A2 — Precision of the graph

### Current state
* Resolution is by scope evidence (`scope.rs`): imports → re-exports → crate facade (`lib.rs`, manifest name)
  → package directory; `Unique` guesses (≥ 5 chars, not in `COMMON`, `scope.rs:40-47`) never from source into
  test code (this round's fix). Edge weights `graph.rs:11-32`.
* **Uses are collected narrowly**: calls (`name(`), `::name` paths and capitalised identifiers; bare lowercase
  identifiers are skipped on purpose; `MAX_REFS = 512` per file (`refs.rs:21`). Audit evidence (scratch ts repo):
  `import { formatDate } from "."; [App, formatDate]` — used as a value, never called — is not a use, so
  `+formatDate` reads 2 where 3 files use it.
* Re-exports are a file list (`FileRecord::reexports`), enough to attribute a use behind a barrel to the
  definer; a re-export line is no longer a user (this round). Rust `mod x; pub use x::Y;` landed with the
  lang round (`0ff49fc`).
* No history signal: `recency.rs` reads git only for a 48 h RECENT flag.
* Tests are linked only through their imports and by name guesses; no source ↔ test pairing.

### Target design
**2a. Binding-level uses.** The import statement is *evidence of use*: a named import binds a local name
to a definer; every mention of that local name in the file is a use of that definition, whatever its case or
whether it is called. Per-file `Binding { local, definer, kind }` is stored; `raw_refs` stays for the unbound
rest. This removes the "bare lowercase skipped" blind spot exactly where it is safe — bound names — and keeps
it where it protects (`path`, `name` locals).

**2b. Spans and sites.** `ExportSite` (`lib.rs:55`, today name/line/char) gains `end_line`, `kind`
(fn/struct/enum/trait/type/const/method/class) and `signature` (first line(s) of the declaration, trimmed,
no initialisers or bodies, ≤ 100 chars). `FileRecord` gains the first 3 use *sites* (line) per `(file, definer
name)` pair, bounded, so `uses` can name call sites (A4) and the LSP's `references` can return real positions
(audit finding 11) from the same data. Memory: +3 `u32` per use ≈ +10 % of the record.

**2c. Co-change edges.** One `git log --no-merges --name-only -n 500 --format=%H` per index build, run beside
the parse like `recency::Probe` (`recency.rs:29`, 1.5 s budget, degrade to none on timeout or shallow clone).
Commits touching 2–12 indexed source files contribute `w = 0.5^(age/50) / sqrt(n)` to every pair; keep the
top 8 partners per file; ignore bulk commits (> 12 files), merges, and pairs where both files are generated or
auxiliary. At rank time the edge weight is `1.5 · min(w, 2)`. Evidence: pilot +0.03–0.09 recall@10 on top of the
structural graph, and 58 % of the files in multi-file commits are reachable by co-change alone.
Decay protects against old reorganisations; the corpus' depth-1 clones have no history, so the eval deepens
them (done for this note) and CI fixtures build synthetic history.

**2d. Test ↔ source pairing.** By convention first, then evidence: `foo.rs ↔ tests/foo.rs`, `foo.py ↔
test_foo.py`, `Foo.java ↔ FooTest.java`, `foo.ts ↔ foo.test.ts`, `x/mod.rs ↔ x/tests.rs`, and any test file
whose imports resolve to exactly one non-test file. A pair becomes (i) a reverse rank edge source→test at 0.5,
(ii) a `test of` / `tested by` tag in lens and `impact`, (iii) the target of failing-test-name seeds (A1 S3).
The existing `role.rs` already classifies test paths; pairing adds the relation.

**2e. Collisions.** Keep the evidence rule; add *proximity* as a tie-breaker when several definers are in scope
(same crate/package directory wins) and an explicit `ambiguous` count shown as `(?N)` in `uses` output instead
of silently dropping — open corpus items sinatra `Base`, cobra `Name` stay hard and are listed, not hidden.

### Expected effect
Better `(→N)`/`+X (n)` fidelity (audit: counts are right where resolution is, wrong where uses are skipped);
the co-change contribution above; test files become findable from a source seed (the pilot cannot measure this:
its truth is mostly one language's production files — see A6 for the test-recall column).

### Cost
Binding + span extraction is one extra tree walk per file (the parse dominates: 28 KB ≈ 8 ms,
`CHANGELOG` syntax note); co-change probe ≤ 1.5 s in the background and cached by `HEAD` sha; memory ≈ +15 %.

### Risks
Co-change from a squash-merge workflow degenerates to "same PR"; bounded by the > 12-files rule and decay.
Bindings misattribute when a local shadows an imported name — rare, and scope-limited by only binding the
file's top-level imports.

### Proof
Fixture tests per language for bindings (value-use of an import counts; shadowing local does not); a synthetic
git history test (three commits touching A+B ⇒ B is A's top partner, an old bulk commit is ignored); the
**compiler-oracle** test: in a Rust fixture workspace rename a `pub fn`, run `cargo check`, and require
`impact(symbol)` ⊇ the files with errors (precision/recall reported) — cargo is already in CI; and the
corpus eval's edge-coverage report (`pilot/edge_cover.py` productised).

## A3 — What a map line carries, under a strict token budget

### Current state
A file line is `path (→N) [RECENT]`, then up to 4 `  +Export (users)` lines (`project.rs:8, 62-93`); 24 files
≈ 2.4 KB ≈ 600 tokens (this repo, measured); the cap is a *file count* (`atlas.limit` 1..64, default 24,
`runtime.rs:732`). Paths are ≈ 44 % of the bytes (1037 of 2376); grouping files by directory would save
≈ 13 % by itself (pilot arithmetic on titi's map). No signature, no role tag besides RECENT, no names for the
dependents; export lines are ranked by user count, so a symbol that is central by flow but little-used is hidden.

### Target design
**Budgeted allocator.** The unit of budget is **tokens**, not files: one setting, `atlas.budget`
(default scaled with repo size: 600 tokens for < 200 files, 900 for < 2 k, 1200 beyond), which the allocator splits
between the skeleton and the lens (lens ≤ 40 %, and the skeleton takes what the lens leaves).
Tokens are estimated as `ceil(chars / 3.4)` for code-ish text, calibrated against the provider's reported
usage in the eval (`ContextUsage` events exist); no tokenizer dependency. A greedy allocator picks items by
marginal rank-per-token — a file header costs its path tokens (a directory header is amortised across its
files), a symbol line costs its signature tokens — and stops at the budget (Aider binary-searches a prefix of
a ranked list; greedy-by-value is strictly better when item costs differ by 3×). The `atlas.limit` setting is
replaced by `atlas.budget` in one cut-over (no alias; config docs and the `titi atlas` verb updated together).

**Line grammar** (illustrative; `§` = 1 token ≈ 3.4 chars):
```
<atlas gen=412 files=351 shown=19>
crates/titi-engine/src/
  runtime.rs  →56 R   struct EngineRuntime · fn atlas_map(root, limit, genome, touched, quiesce) -> Option<SmolStr>
  tool_loop.rs →31    fn run_tool_round(…) -> RoundOutcome   used by: runtime, subagent +9
crates/titi-atlas/src/
  graph.rs →1  [core]  fn rank(files, layout) -> (ranks, dependents)
  lib.rs →55 [entry]   struct Atlas · fn index(root) · fn refresh(&mut self, root)
</atlas>
```
* **Signatures** for the top symbols of a file: one line, from the stored `signature`, truncated at the first
  `{`/`=`/`:`-body boundary and at 100 chars; types of parameters kept, default values dropped; doc comments off
  by default (`atlas.docs: first-line` as an option). Aider shows signatures for the same reason, Agentless'
  skeleton step is built on them, and the model can answer "what does `atlas_map` take?" without a `read`.
* **Role tags** from `role.rs` and a small config detector: `[entry]`, `[test]`, `[cfg]` (Cargo.toml,
  package.json, `*.config.*`, CI files — currently not indexed at all, so they could only ever be in the
  skeleton as a one-line list of manifests), `[gen]`. `R` = recent (today's `[RECENT]`).
* **Blast radius**: `→N` stays (distinct dependent files). A lens file additionally names its **top 3
  dependents** (`used by: runtime, subagent +9`), the rest on demand (`atlas impact`). The skeleton names none
  (budget).
* **Header** carries `gen` (index generation) and `pending` (existing contract, `project.rs:57-61`) so the model
  and tool results can be compared.
* Ordering inside the skeleton: directory-sorted (stability, A1 cache rule); inside the lens: score order.

### Expected effect
Same or fewer tokens for ≥ 2× the information per file; the exact saving is an A6 column
(`tokens/recall@k`). Signature lines cost ≈ 12–18 tokens (titi sample: 71 exported items, median declaration
24 chars, `fn` lines ≈ 70) — about 3 per file at the 1000-token budget, so shown files drop from 24 to ≈ 14–18,
which the lens's task-specific list more than recovers.

### Risks
Signatures expose more of the code to the provider than names did: no initialisers/bodies/strings, secret-path
exclusion as in `<diff>` (`difftrack.rs`), and the same sanitiser. A poor token estimate overruns the budget:
calibration test against recorded `usage` fixtures.

### Proof
Golden render tests (budget respected for 5 estimators ± 15 %, no line over 160 chars, sanitiser strips
`<`, control characters and `</atlas>` look-alikes); a calibration test comparing the estimator with the scripted
provider's reported prompt tokens; A6 `tokens` and `tokens/recall` columns.

## A4 — On-demand querying: the `atlas` tool

### Current state
Six tools only (`workspace_tools`, `titi-tools/src/fs.rs:1530`); the index is invisible to the model except as
the prompt map (the practices note's own first "Do"). The LSP (`lsp.rs:23`) builds its **own** second index at
process start and reads source from disk or an open buffer; `references` returns line 1 per file (audit #11).

### Target design
**One tool, `atlas`, read tier** (no approval), one schema with `op` — a single tool keeps the always-sent
tool spec to ≈ 250 tokens (every tool spec is part of the cache-critical prefix, ~11 KB today for all tools);
LocAgent's three-tool design is the model for the *shape*: search, traverse, retrieve, tree-formatted output.

| op | args | returns (≤ 40 lines default, `cursor` to page) |
|---|---|---|
| `find` | `query`, `kind?` (symbol/file/any), `limit?` | `path:line  signature  [role] (→N)` for symbols and files matching by exact → case-insensitive → identifier-token → BM25-ish over names/paths, **ranked by PPR seeded by the query's top hits** (same code as the lens) |
| `outline` | `path` | every symbol of the file with `line-end_line`, kind, signature, visibility; imports summarised; role; tests paired |
| `uses` | `symbol` \| `path:line` | definition site, then users grouped by file with call-site lines (A2b), by evidence (`scope`/`unique`/`co-change`), ambiguity count |
| `impact` | `symbol` \| `path` \| `diff` | what a change touches: direct users, users through re-exports, paired tests, co-change partners with strength, transitive count at depth 2; `diff` = the current git diff's files/symbols ⇒ affected tests to run |
| `related` | `query` \| `path` | the lens for an arbitrary query or file (what to open next) |

Every result starts `atlas gen=N` and `stale=K` (K > 0 when the worker has a backlog), and ends with
`more: cursor=…` when truncated. Output is indented trees (LocAgent's finding that tree layout beats edge lists
for graph reasoning) with the evidence tag per edge. Errors are actionable (`no symbol named X; closest: …`).

**Shared index with the LSP.** The query layer is a library surface (`Atlas::find/outline/uses/impact/related`,
all `&self` on an immutable snapshot, no I/O). The engine's tool and `titi atlas lsp` are protocol adapters over
it — identical semantics in both, and the LSP's `references`/`definition` get use sites for free. The LSP stays a
separate stdio process (that is the protocol), so it keeps building its own index; it adopts the same
incremental updates (`didSave` ⇒ `apply_changes`, watched-file poll tier, A5) and, optionally later, a
content-hash-keyed on-disk index cache (`.titi/atlas/`) so both processes start warm — `atlas-incremental.md` §7
named this the next ticket; cold index is 0.1–0.9 s on the corpus, so it is an optimisation, not a prerequisite.

**Prompt interplay.** With the tool, the skeleton can shrink on big repos (structure only) and the model asks
for the rest; the operating manual gets one sentence (“before grepping for a symbol, `atlas find`; before
changing an exported symbol, `atlas impact`”). Whether models call it is an *empirical* question — hence A6's
scripted-oracle and real-model conditions.

### Expected effect
LocAgent-class gains need the model to use graph tools; in titi the cheap part is guaranteed (the lens) and the
tool adds multi-hop reach (`impact`, `uses`) the prompt cannot carry. Measured later: reads-before-first-edit and
tokens-to-first-correct-edit in the end-to-end A/B.

### Cost / risks
+250 tokens of tool spec; results are bounded; no network, no write. Risks: tool under-use (measured), result
verbosity (budgeted, paged), schema drift (one pinned JSON fixture test like `tests/protocol.rs`).

### Proof
Query-layer unit tests on fixtures; a golden test per `op` for output shape and budget; a tool-registry test
(`atlas` present, read tier, in duck mode absent like the map); an LSP test asserting `references` returns call
sites from the same data as `uses`; A6 scripted-query recall.

## A5 — Freshness

### Current state
* Agent writes fold in immediately: `apply_changes` on the written path after the tool returns
  (`tool_loop.rs:605-611`) — but with the model's raw spelling (abs / `./` ⇒ duplicate key until the next
  walk; audit #7).
* Every turn asks the worker for `Resync` (a full tree walk): no-op 3.5 ms on this repo, 11–12 ms on 5 k files
  (perf README); debounce 120 ms, turn waits 40 ms and otherwise names `pending` (`runtime.rs:128`, `live.rs:88`).
  Verified in the audit: external edit/delete/rename between turns appear the next turn; the live map equals a
  cold index modulo the touched boost; `pending` never stuck in any scenario.
* **No watcher**; `notify` was owner-gated (`atlas-incremental.md` §4). A `bash` side effect (`rm`, `mv`, a
  formatter) is seen only at the next turn's walk (audit scenario 2: map after `delete card` unchanged within the
  turn). Importers of a deleted file keep a stale `imports` entry (ranker ignores it; the diagnostic is stale —
  STATE.md open item).

### Target design (no new dependency)
1. **One path normaliser** at the single choke point: `Atlas::key(path)` strips the root (trying the
   canonicalised root for `/tmp` vs `/private/tmp`), `./`, `..` segments and rejects outside-root paths. Used by
   `apply_changes`, touched seeds, LSP URIs, tool arguments. (Fixes audit #7 and the LSP symlink-root item.)
2. **Post-`bash` resync**: after any `bash` tool call returns (and after a hook), enqueue a targeted
   `Resync` — one walk, 3–12 ms — so the next atlas read in the same turn is current. Today's per-turn
   `Resync` remains the correctness backstop.
3. **Stat-poll tier** (replaces the watcher decision): a worker-owned ticker, adaptive — 250 ms while a turn
   is running, 2 s while the session is idle and the TUI focused, off after 5 idle minutes. Three cheap
   checks per tick, no dependency, no FFI, no `unsafe`, portable:
   * **directories**: stat every directory that contains an indexed file; a directory's mtime moves on
     create/delete/rename inside it, so a moved one is listed and diffed alone — O(dirs), ≈ 400 dirs on a 5 k-file
     repo (< 1 ms), ≈ 4 k on 20 k files (≈ 5 ms) **[INFERENCE, measured in A6's latency column]**;
   * **hot set**: re-stat the touched, diff and lens files (≤ 64) every tick, because a *content* edit does not
     move its directory's mtime;
   * **everything else**: a full file-list stat every 10th tick (one `stat` per file, the cost of today's
     Resync walk), and `.git/HEAD` + `.git/index` every tick — a moved one is a branch switch/checkout ⇒ one
     full `Resync`.

   Racy mtimes are already handled (`RACY_MTIME`, `lib.rs:216`). Cost ≈ 0.1–1 ms per tick at this repo's size,
   nothing when off. What it gives up against `notify`: sub-second reaction while a human edits in another
   window during an *idle* session — nobody reads the map then, and the next prompt's `Resync` catches up
   anyway. **Recommendation: do not adopt `notify`** unless the A6 staleness metric shows it is needed.
4. **Deletions and renames**: store *every* import specifier with its candidates and its resolution
   (`ImportSpec { spec, candidates, resolved }`, today only unresolved ones keep candidates, `lib.rs:121-127`);
   when a path disappears, importers whose `resolved` equals it are re-resolved from the stored candidates with
   no re-parse (the machinery exists for *arrivals*, `resolve_new_imports`, `lib.rs:868`), restoring the
   `unresolved-import` diagnostic and removing the dead edge. A disappearing path + an appearing path with the
   same content hash (`FileRecord::hash`) is a **rename**: the record is carried over (no parse), ranks and
   co-change history are re-keyed (`git log --follow`-style via `-M` on the probe), importers stay unresolved
   until edited — which is the truth, and what `check` should say.
5. **Staleness is explicit**: `gen` and `pending` in every skeleton/lens/tool header; the eval measures
   *staleness at read* (fraction of reads whose snapshot predates the file's current content).

### Expected effect
Mid-turn and post-`bash` reads current; branch switches and formatters seen within a tick; deleted-file
diagnostics correct; zero duplicate keys. Staleness-at-read target: 0 within a turn, ≤ 2 s idle.

### Risks
Poll cost on huge trees (bounded by tick adaptation and the 20 k-file measurement), `git` index lock races (we
only `stat`), clock skew (existing racy-window logic), NFS directory mtimes (fall back to full `Resync` on
tick 10).

### Proof
Scripted-provider e2e scenarios (the audit's `scen1-3` formalised): bash `rm`/`mv` then an `atlas` read in the
same turn sees it; external write mid-turn visible within 2 ticks; `git checkout` triggers one full Resync;
abs-path / `./` edit leaves no duplicate key; a deletion test asserting the importer's unresolved diagnostic
returns without a re-parse (parse counter); a rename test (hash match ⇒ `parsed == 0`).

## A6 — Measurement: a task-level eval on real commits

### Why the current eval is not enough
`scripts/atlas_eval.py` scores the static map against hand-picked "central files": useful as a smoke test
(it found 14 real problems), but the truth list is a judgement, p@10 has 10 slots of noise, and it cannot see
whether a map helps *a task*.

### Harness: `scripts/atlas_eval.py` (productises `pilot/`)
* **Tasks.** For each corpus repo, up to 30 non-merge commits (stratified over the history, depth ≥ 300 —
  clones are deepened, `git fetch --deepen`), with 1–8 modified indexed source files and a message ≥ 20 chars.
  **Task text** = commit subject+body, plus the text of an issue/PR referenced as `#123` / `fixes #123` when
  `gh api` can fetch it (cached under `/tmp`; skipped offline). Also a *synthetic prompt* variant: the diff's
  symbols as the user would phrase them is **not** used — no leakage from the answer.
* **Truth.** Modified files that exist at the parent, split into production / test; symbol truth =
  enclosing symbols of the hunks (needs A2b spans). Added files are reported separately (cannot be in the map).
* **Index.** Built on the **parent tree** only; co-change uses only history before the parent; session seeds
  (touched/diff) are empty for the cold-start condition and set to the first changed file for the "continued
  work" condition (pilot `co5|1` generalised).
* **Conditions.** (A) static map @ budget (today); (B) skeleton + lens; (C) `atlas find`/`related` driven by a
  scripted oracle query (message terms — no model) for the tool's recall ceiling; (D) B ∪ C; (E) lexical-only and
  `grep`-only baselines (what the model has without atlas).
* **Metrics.** recall@{5,10,24} and MRR of the first truth file; recall of production vs test truth; symbol
  recall; **map tokens** (estimator + provider-reported tokens in the end-to-end mode); tokens per recalled
  file; lens/tool **latency** p50/p95 and index build time/RSS; **blast recall**: share of truth files reachable
  from the first truth file by `impact` at depth ≤ 2 (pilot: 77 % by edges ∪ co-change); **precision@k** of
  the lens; **staleness at read**; plus the existing p@10/recall@24 on the hand-picked lists so nothing there
  regresses (the guard the lang round used).
* **Controls for leakage and noise.** Report separately tasks whose message names a truth path (lexical
  leak); bootstrap confidence intervals over tasks; fixed seeds; a run records `atlas` sha, corpus SHAs.
* **End-to-end (later, in `titi eval`).** The same tasks with a real model through the scripted harness: tokens
  to first correct edit, `read`/`grep` calls before the first edit, atlas-tool calls, pass rate on repos with
  tests — the A/B the practices note calls for (#10, #16), reporting cost with pass rate.

### Baseline (pilot, static map today) and targets
| | baseline | v2 target (pilot already shows) |
|---|---|---|
| recall@10 | 0.29 | ≥ 0.70 |
| recall@24 | 0.52 | ≥ 0.85 |
| map tokens per changed turn | ≈ 450 (24 files, corpus mean; 600 on titi) | ≤ 400 lens + amortised skeleton |
| large repos (okio, ky, ripgrep) recall@10 | 0.01 / 0.15 / 0.03 | ≥ 0.55 / 0.65 / 0.9 (pilot mix: 0.56 / 0.68 / 0.92) |
| lens latency p95 (5 k files) | n/a | ≤ 10 ms |

### Proof that the harness is right
It runs in CI on a **fixture repo with synthetic history** (script builds ~40 commits with known changed sets and
asserts deterministic recall for each condition — including a deliberate "no seeds ⇒ equals static" case); the
corpus run is a nightly/manual job writing `docs/research/atlas-eval/README.md` like `atlas-eval` does.

## 3. Cross-cutting design

* **Data model.** `Atlas` snapshot = files (records with `signature`, spans, bindings, use sites, import specs),
  CSR edge arrays (structural + reverse + co-change), inverted indexes (name → symbols, path token → files),
  `gen`. Everything the five front ends need is a pure function of it. Publication stays `Arc` swap
  (`SharedAtlas`→`SharedAtlas`).
* **Config.** `atlas.enabled`, `atlas.budget` (replaces `atlas.limit`, no fallback), `atlas.tool`
  (on by default), `atlas.coChange` (on if git history present), `atlas.poll` (adaptive/off). One cut-over,
  no aliases.
* **Duck mode** stays repo-blind: no skeleton, no lens, no tool (`atlas_root = None`, `runtime.rs:159`).
* **Subagents** get the same snapshot through the existing `with_atlas` handle (now `with_atlas`); a subagent
  inherits the parent's *lens seeds* (its task text is seeds too).
* **Failure modes** degrade to today's behaviour: no seeds ⇒ static skeleton; no git ⇒ no co-change/RECENT by
  mtime; index error ⇒ no atlas (the prompt goes through unchanged).

## 4. Build plan — smallest high-impact first

Each step lands alone, with a behavioural test that fails first, a CHANGELOG line and the eval numbers in the
commit body.

| # | step | why now | size | proves |
|---|---|---|---|---|
| 0 | **Path normaliser** (`Atlas::key`) used by `apply_changes`, touched seeds, LSP; fixes duplicate keys and the weak touched boost's matching | every later step keys on it; audit #7 | S | abs/`./` edit test; no duplicate key |
| 1 | **`atlas dump` + `scripts/atlas_eval.py`** (pilot productised; `titi atlas graph --json`), fixture repo CI test, baseline table committed | numbers before code; pilot already shows the gap | M | harness self-test; baseline in docs |
| 2 | **Seeds + personalised PageRank (`Atlas::lens`)** with both-way edges and the static fallback; engine attaches `<atlas-lens>` to the newest user message; masking of old lenses at compaction | the biggest measured effect: recall@10 0.29 → ~0.7 | M | lens unit tests; engine prefix-stability test; A6 `lens` vs `static` |
| 3 | **Co-change edges** (git probe beside recency, decay, caps, shallow fallback) | +0.03–0.09 recall@10; a signal no static analysis has | S–M | synthetic-history test; A6 ablation |
| 4 | **Token-budgeted skeleton**: directory-grouped sorted render, role tags, named dependents in lens, `atlas.budget` replacing `atlas.limit`, skeleton delta policy | tokens/cache; makes 2 affordable | M | golden render + calibration tests; A6 tokens columns |
| 5 | **Spans, signatures, use sites** in `ExportSite`/`FileRecord` (all languages, `lang/*`) and signatures in map lines | needed by 6 and by symbol-level truth | M–L | per-language fixture tests |
| 6 | **`atlas` tool** (`find`/`outline`/`uses`/`impact`/`related`) on the query layer; LSP `references` and `didSave` on the same layer; manual sentence | multi-hop reach the prompt cannot carry | L | per-op golden tests; LSP test; A6 conditions C/D |
| 7 | **Binding-level uses + test↔source pairing** | precision of `uses`/`impact`, test discovery, trace seeds | M | fixture tests; compiler-oracle test |
| 8 | **Freshness**: post-`bash` resync, poll tier, import specs with re-resolution on deletion, rename carry | tool reads in the same turn must be current | M | e2e scenarios; parse-counter tests |
| 9 | **End-to-end A/B in `titi eval`** (real model, tokens-to-first-correct-edit, reads before edit) and the decision on `notify`/persisted index from the staleness and cold-start numbers | the only test of "does it help the agent" | L | published A/B |

Steps 2–4 are the core of the concept and are independently shippable behind `atlas.lens` / budgets; step 6
is the second big lever; 5/7/8 are precision and freshness work that the earlier steps expose the need for in
the eval rather than assume.

## 5. Questions, as decided (2026-10-21)

1. *Model call on the hot path?* **No.** Index-only; an assisted mode only as an explicit later opt-in.
2. *`atlas.limit`?* **Replaced** by `atlas.budget` (tokens) in one cutover, no fallback key, CHANGELOG says so.
3. *Tool default?* **On from step 6**, read tier; the step-9 A/B may revise it.
4. *Git history probe?* **Fine**: background, capped by commits and time, cached per `HEAD`, never blocks the first
   frame or a turn.
5. *Persisted index?* **Agent directory**, never the repository.

## Appendix — pilot files and how to reproduce

`pilot/commit_eval.py` (static map and lexical/co-change on replayed commits; needs
`cargo build --release -p titi-atlas --examples`), `pilot/commit_ppr.py` (personalised PageRank variants; needs
the graph dumper `pilot/graph_dump.rs` built as a scratch crate that depends on `titi-atlas` — the repo's
public API only), `pilot/edge_cover.py` (import/use vs co-change coverage). Corpus clones deepened with
`git fetch --deepen 150`. Paths inside the scripts point at this machine's `/tmp` layout and the pre-rename
crate name (`titi-atlas`); the rename sweep should update them with the rest.

Sources: Aider repo-map blog and `repomap.py` (aider.chat/2023/10/22/repomap.html,
github.com/Aider-AI/aider); LocAgent, arXiv 2503.09089 (Table 4: file Acc@5 94.2 % vs BM25 61.7 %, CodeRankEmbed
84.7 %, Agentless 74–79 %, SWE-agent 69–90 %); Agentless, arXiv 2407.01489; RepoGraph, arXiv 2410.14684;
Empryo docs `concepts/genome` and `concepts/marionette` (empryo.com, public, ideas only); titi
`docs/research/{agent-practices,prompt-cache,atlas-incremental,perf,atlas-eval}`.
