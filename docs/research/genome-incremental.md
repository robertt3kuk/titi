# Taimyr: event-driven incremental indexing for the Code Genome

Design note, 2026-10-08. **No code.** Every `titi` claim carries a `file:line`; every crate-health claim
says how it was checked and is marked unverified where this machine cannot check it. **All line numbers
are of commit `a77d12f`, not of the working tree**, which moved while this was written: a sibling has
`crates/titi-genome` dirty — `content.rs`, `shared.rs`, `examples/refresh.rs`, its tests and `lib.rs` —
adding a FNV-1a fingerprint, a `SharedGenome` publish point and `Genome::apply_changes(&self, paths)`:
the shape of this note's phase 0 and phase 1's entry point. None of that is committed and no phase is
approved, so the design below describes it anyway; read the overlap as agreement, not as novelty.
Symbols are the stable pointer.

Constraint that shapes everything: `titi-genome` has **no tokio** — its manifest lists `regex`,
`tree-sitter*`, `thiserror`, `serde_json` only (`crates/titi-genome/Cargo.toml:11-34`) and it exposes a
plain synchronous API (`lib.rs:136`, `:145`, `:244`, `:249`, `lsp.rs:12`); the engine is the async side
(`crates/titi-engine/Cargo.toml:25`). Taimyr must not push tokio down into the genome crate.

## 1. What the spec asks vs what exists

| requirement | today (quoted) | gap |
|---|---|---|
| filesystem watcher (create/modify/delete/rename) | nothing. `grep -rniE 'notify\|watcher\|debounce' crates/*/src` returns only chat notification toggles (`titi-cli/src/chat.rs:87`, `notify_completion`) | no event source outside the harness |
| harness events | `pub const TOUCHING_TOOLS: &[&str] = &["read", "write", "edit"];` (`tool_loop.rs:105`) records `touched` **before** the call runs (`tool_loop.rs:160-164`) | the engine knows the path; nothing tells the genome |
| smart debouncing | none; one synchronous drain per turn (`runtime.rs:149-155`) | a burst is N gates, not one analysis |
| incremental parsing | **exists in part**: `refresh` re-parses only files whose `size`/`mtime` moved (`lib.rs:150-157`), in parallel over cores (`lib.rs:261-292`) | no re-parse avoidance for the file-set-dependent half; `mtime` granularity |
| dependency invalidation | every refresh re-ranks the whole graph (`lib.rs:170`) and re-resolves every file's mentions (`lib.rs:188-242`) | no invalidation; nothing is skipped on evidence |
| priority scheduling | `TouchedSet` (`tool_loop.rs:70-99`) exists, and `project_with` boosts those files ×3 (`project.rs:16`, `:27`) | the ordering only reorders the *projection*; parse order is scan order |
| background indexing | `genome_map` refreshes inside the turn and the turn awaits it (`runtime.rs:140-161`); `run_off_thread` moves it off the async executor but not off the turn (`runtime.rs:37-43`) | the agent pays the walk before every prompt |
| atomic graph updates | `pub fn refresh(&mut self, …)` mutates in place (`lib.rs:145`); consistency comes from holding `type GenomeIndex = Arc<tokio::sync::Mutex<Option<Genome>>>` (`runtime.rs:117`) for the whole build (`runtime.rs:150`) | no snapshot, no publish: a reader either waits or sees nothing |
| content hashing | `pub size: u64, pub mtime: SystemTime` (`lib.rs:87-88`), compared at `lib.rs:156` | a same-size edit inside one `mtime` tick is invisible |
| per-change-kind update strategy | `FileRecord { exports, imports, used_symbols, … }` (`lib.rs:72-88`) — enough to classify, after the fact | nothing compares old and new before deciding work |
| staleness signal | `RefreshStats { parsed, removed, total }` (`lib.rs:115-121`) is discarded: `index.refresh(&root).ok()?` (`runtime.rs:152`) | no generation, no way to say "behind" |
| git checkout/merge/worktree resync | `difftrack::capture` already runs `git diff HEAD` per turn (`difftrack.rs:61`); nothing feeds paths back | no resync trigger |
| public API → connected repositories | no multi-root concept: `pub genome_root: Option<PathBuf>` (`runtime.rs:450`), read as a single path at `:1392`, `:1549`, `:1583` | needs a product concept, see §7 |
| LSP `didOpen`/`didChange` | `"One index at start"` (`lsp.rs:11`); every frame without an `id` is dropped (`lsp.rs:32`); queries read **disk** (`query.rs:177`) | an editor buffer is never seen |

## 2. The pipeline and who owns the state

```
   writer sources                 the indexer thread (one, std::thread)           readers
 ┌──────────────────┐          ┌──────────────────────────────────────┐      ┌────────────────────┐
 │ watcher thread   │          │ loop:                                 │      │ engine turn        │
 │ (phase 3, notify)│  tx      │  1. drain rx until quiet (DEBOUNCE)   │      │  snapshot()        │
 ├──────────────────┤ ───────► │  2. hash/confirm each candidate       │      │ LSP request        │
 │ harness events   │ Request  │  3. parse the dirty set (priority     │      │ genome check       │
 │ (phase 2, engine)│  channel │     queue first, urgent preempts)     │      │                    │
 ├──────────────────┤          │  4. invalidate: tuple compare → edges │      └─────────┬──────────┘
 │ Resync: git HEAD │          │  5. rank (only if edges/nodes moved)  │                │
 └──────────────────┘          │  6. Arc::make_mut + publish + bump gen│ ◄──────────────┘
                               └──────────────────────────────────────┘   RwLock<Arc<Genome>> (read = clone)
```

**State ownership.** The indexer thread owns the only mutable `Genome`. Readers never take the writer's
lock. New module `crates/titi-genome/src/live.rs` exposes:

```rust
pub struct GenomeHandle {                       // Clone, Send + Sync, all fields Arc
    published: Arc<RwLock<Arc<Genome>>>,        // std::sync::RwLock — read = Arc clone, ~ns
    tx: std::sync::mpsc::Sender<Request>,       // eager, unbounded; the worker drains
    generation: Arc<AtomicU64>,                 // bumped on every publish
    pending: Arc<AtomicUsize>,                  // files in the current batch, 0 when idle
}
pub enum Request { Changed(Vec<String>), Removed(Vec<String>), Priority(Vec<String>),
                   Resync, Quiesce(std::sync::mpsc::Sender<u64>), Stop }
```

`snapshot() -> (Arc<Genome>, u64)` and `quiesce(Duration) -> bool` are the whole read API;
`spawn(root, opts) -> GenomeHandle` starts the thread. **`std::sync::mpsc`, not tokio**: the crate stays
tokio-free, and the worker is a real thread because the work is CPU-bound and long-lived —
`spawn_blocking` is for a task that *ends*, and a `tokio::task` would park a runtime worker on a parse.

**Publish.** `published.write()` is held for one `Arc::make_mut` (`Genome` derives `Clone`,
`lib.rs:124-125`): if a reader still holds the previous `Arc` it clones first, otherwise the writer
mutates in place — one `HashMap<String, FileRecord>` of this repo's 145 files, microseconds. An
in-flight reader keeps its own `Arc`, therefore a **complete old graph**; the new one appears at the
swap. Atomicity without a lock held across the build, replacing `Arc<tokio::sync::Mutex<Option<Genome>>>`
(`runtime.rs:117`) whose lock today spans the whole refresh (`runtime.rs:150`). `ArcSwap` would be
marginally faster and needs a dependency with an `unsafe` core — not worth it here.

**Debounce** is not a timer task: the worker blocks on `rx.recv()`, then keeps `recv_timeout(QUIET)`
until `QUIET` (default 120 ms) passes with nothing arriving — a burst of 40 saves is one batch because
the channel is drained, not because 40 timers expired. `Priority(_)` never resets that window.

**Priority** is a two-queue drain — `urgent: VecDeque<String>` fed only by `Priority`, `ordinary:
BTreeSet<String>` accumulated by the debounce. The parse loop takes urgent first and re-checks it
between files, so a `write` landing mid-batch is parsed before the batch ends. The engine's input is
already there: `touched` is recorded in the tool loop (`tool_loop.rs:163`) and `working_tree_diff.files`
(`difftrack.rs:25`) is inserted into it before the turn builds context (`runtime.rs:1985-1994`).

**Bridge to the async engine.** `GenomeIndex` becomes `GenomeHandle` and `genome_map`
(`runtime.rs:140-161`) stops calling `refresh`: it sends `Priority(touched)`, awaits `quiesce(40 ms)`,
and renders from `snapshot()` — the turn's blocking work drops from walk+parse+rank to a clone.

## 3. Update strategy: what is recomputed, and what is provably not

The graph consumes exactly one tuple per file: `T(f) = (exports(f), imports(f), used_symbols(f))`.
Edges come from `imports`/`used_symbols` through `symbols` (`graph.rs:18-31`); `symbols` comes from
`exports` (`lib.rs:190-198`) and `used_symbols` (`lib.rs:214-228`). `rank` is pure in `(files, symbols)`
— fixed `DAMPING`/`ITERATIONS` (`graph.rs:5-6`), no RNG, no clock. So **if no file's tuple changed,
`ranks` and `dependents` are bit-identical and `rank` is skipped** (`lib.rs:170` moves behind a dirty
check). Everything below rests on that.

| change kind | recomputed | provably skipped, and the evidence |
|---|---|---|
| function body | parse that file (`lib.rs:294-308`); `size`/`hash`/`mtime`/`export_sites`; `rank` **only if** `T` moved; the projection (because `mtime` feeds `[RECENT]`, `project.rs:49-55`) | rank and edge rebuild when `T(f)` compares equal — the tuple is the whole graph input; 19 of 20 graph passes (`graph.rs:46`) |
| signature (same names, new line/character) | `export_sites` (LSP only), `hash`, projection | the whole graph: name-only resolution never reads a site (`MAX_DEFINERS`, `lib.rs:90-96`); no re-parse of importers of that symbol |
| interface (export added/removed) | `symbols` rebuild (definers), re-resolution of only the files whose *raw* mentions include an affected name, then `rank` | re-parsing: the source text is unchanged, and `used_symbols` resolution is set membership over stored mentions (`lib.rs:218-223`) — **requires** keeping raw mentions (below) |
| imports changed | that file's parse, its outgoing edges (`graph.rs:21-22`), `rank` | other files' edges: an importer's tuple does not contain its importer's imports |
| file created | its parse; files whose `unresolved_imports` now resolve; the definer path if its `exports` introduce a name | every other parse; the full walk (`scan.rs:42`) |
| file deleted | drop the node, drop inbound edges (from an `imports` reverse index), definer path if it was a definer, `rank` | any parse at all, and the outbound edges of files that never imported it |
| file renamed | as create + delete, unless `hash` matches an existing record — then the tuple is **carried over** and the parse is skipped too | the parse, on hash evidence (`lib.rs:87-88` has no hash today; phase 0 adds it) |
| `git checkout`/`merge`/worktree switch | full resync: `scan::list_files` (`scan.rs:42`) + hash compare; parse only what the hash says moved; `rank` | the parses of the unchanged majority — the resync is a *walk*, not a re-analysis |
| public API → connected repos | nothing | out of scope, §7 |

**Two things phase 1 needs that do not exist yet.**

1. Raw mentions are **discarded**: `record.used_symbols = used` (`lib.rs:231`) overwrites the parser's
   mention list with the *resolved* subset. Without the raw list there is no way to know which files to
   re-resolve when the definer set moves, so an export change would force a full re-resolution pass. Keep
   both (`used_symbols` resolved, `raw_refs` as parsed, bounded by `MAX_REFS = 512`, `refs.rs:19`) plus an
   inverted `name → files` map. Cost is bounded per file and measured in the same units as `refs.rs:320`.
2. Import resolution is not invertible today: the `resolve_*` helpers take the known-file set and return
   `Option<String>` after `files.contains(…)` (`lang/support.rs:80-100`, `rust.rs:162`, `python.rs:146`,
   `swift.rs:139`), and the only entry point is a full parse (`lang/mod.rs:406`). Phase 1 makes each
   helper return its *candidate* paths and filters outside, so "does specifier `s` of `f` resolve to the
   new path `p`" is a pure answer over stored `unresolved_imports` — no parse, no guess.

**Content hashing** (phase 0) rides on bytes already in memory: `parse_one` does
`fs::read_to_string` (`lib.rs:295`), so the hash costs no I/O. Size+mtime stays as the cheap pre-filter;
the hash catches a same-size write inside one `mtime` tick, which today is silently lost (`lib.rs:156`).
Hash choice is open — §8, question 5.

## 4. The dependency decision: `notify` or harness events only

**Checked, and how.** Not in the tree: `grep -c '^name = "notify"' Cargo.lock` → 0, no `Cargo.toml`
mentions it, no crate has it transitively. Not available locally either:
`ls ~/.cargo/registry/src/index.crates.io-*/ | grep -i notify` → nothing (the cache holds 694 extracted
crates, `filetime` and `walkdir` among them, so its absence is real and not a missing cache); no network
here, and `cargo` was not run for this note. **Version, license, last release, maintainer, features and
RustSec status of `notify` are therefore UNVERIFIED** — the recommendation below is not a clean bill.
Project-wide context, verified: RustSec is *already* unchecked for the whole tree — `docs/BRAIN.md:35`
("**UNVERIFIED** — `cargo audit` is not installed, so *no* advisory was checked"),
`docs/audits/2026-10-08-facts.md:168`, `docs/audits/2026-10-08-general.md:335-336`. Taste signal,
verified: the workspace forbids `unsafe` in its own code (`Cargo.toml:38-39`,
`unsafe_code = "forbid"`), and that lint does not reach dependencies, so a watcher puts OS FFI
(FSEvents/kqueue/inotify) behind a boundary this project cannot lint. Worth naming; not a blocker.

**Recommendation: adopt `notify` at phase 3, nothing earlier, under the `dependency-update` skill's
procedure** — MIT/Apache licence, maintenance and last-release check, `cargo audit`, `default-features =
false` plus the single platform backend actually needed (macOS here), declared in
`crates/titi-genome/Cargo.toml` rather than `[workspace.dependencies]` until a second crate wants it.
Phases 0, 1, 2, 4 and 5 need **no** new dependency and carry most of the value; the watcher is the one
requirement whose absence a user can see, because everything else learns about a file only when the
harness touched it.

**Alternative — harness events only (phase 2, no watcher).** Cost: edits made *outside* the agent are
invisible — `git checkout`, a formatter, another agent in another process, an editor, and the whole LSP
surface, a separate process (`genome_cmd.rs:63-67`) that receives no harness events and answers from a
start-of-process index forever. Savings: no new dependency, no FFI; 0/1/2/4/5 still deliver incremental
parsing, invalidation, priority, background indexing and atomic publish.

**What the watcher must ignore — the scanner's own rules, not a second list.** The filter must be
exactly `scan.rs:5-14` (`PRUNE_DIRS`: `target/`, `node_modules`, `.git`, `dist`, `build`, `coverage`,
`vendor`, `__pycache__`), the dot-directory prune (`scan.rs:148-150`), the gitignore-style rules from
`.gitignore` and `.reference-productignore` (`scan.rs:44-45`, matched by `is_ignored`, `scan.rs:157`),
the size ceiling (`MAX_FILE_BYTES = 1_000_000`, `scan.rs:16`) and the language table
(`lang::is_source_file`, `lang/mod.rs:402`) — all private today, `pub(crate)` after this. Two cases are
*not* drops: events inside `.git/` (HEAD, refs, index.lock — constant churn) become one
`Request::Resync`, and a written `.gitignore` is a `Resync`, because the ignore rules are part of the
file set.

## 5. The consistency contract

A context request never silently reads a graph that is behind. Which of the spec's two allowed outcomes
a caller gets depends on who waits; `pending` is what makes staleness visible.

| caller | outcome | what it does | user-visible signal |
|---|---|---|---|
| per-turn prompt (`runtime.rs:1583-1593`) | **priority-finish, then report** | `Priority(touched)`, `quiesce(40 ms)`, render from `snapshot()` | none when caught up; otherwise the map's header names the backlog: `<genome pending="3">` instead of `<genome>` (`project.rs:41`). The model sees it in the same block it reads, so it can re-read a file instead of trusting the map |
| LSP (`lsp.rs:37-62`) | **priority-finish, bounded by a deadline** | the server is already synchronous request/response on its own thread; `quiesce(200 ms)` for the paths the request touches | none on success; on deadline the reply is answered from the published graph and the non-standard field `"stale": N` rides beside the result (a client that ignores it loses one field, never wrong semantics) |
| `titi genome check` (`genome_cmd.rs:95-96`) and `/genome check` (`chat.rs:5622`, `local_genome_note`) | **fresh** | a one-shot process indexes and exits; there is no graph to be behind | nothing — staleness is not a concept for a process that ends |

Why bounded priority-finish on the turn path rather than unbounded: the turn already pays the full
refresh today (`runtime.rs:152`), and removing that is the ticket's point. 40 ms buys the batch that is
almost certainly being built right now (the tool call that wrote the file returned a moment ago) and
bounds the worst case; anything still pending is *named*, not hidden. Why unbounded-ish for the LSP: the
client is already blocked on the answer, and an answer about the file under the cursor sourced from a
graph that ignores the last keystroke is the exact failure the spec forbids. `pending` counts **files**,
not changes, so it is comparable across batches and cannot be inflated by a debounce window.

## 6. Phases

Each ships alone; each acceptance test fails without it. New integration tests extend
`crates/titi-genome/tests/index.rs`, which already has `refresh_reparses_only_changed_files:243` and
`a_recently_modified_file_is_marked_recent_not_new:405`.

| phase | what lands | acceptance test that fails without it | dependency decision |
|---|---|---|---|
| 0 | content hash in `FileRecord`; `DirtySet`; `Arc<Genome>` + swap publish + `generation`; `GenomeHandle` skeleton | rewrite a file with the *same size* and the same `mtime` tick → `parsed == 0` after the batch, and no `rank`; a reader holding `snapshot()` across a batch observes a complete graph (`files.len()` is one of the two valid states, never in between) | no |
| 1 | tuple compare → edge invalidation; raw mentions + `name → files`; candidate-returning `resolve_*`; priority queues | body-only edit with an identical `T(f)` → `ranks`/`dependents` byte-identical and the rank pass never entered; adding an export used elsewhere re-resolves only that name's users; creating a file that resolves another's `unresolved_imports` fixes that importer's `imports` without re-parsing it | no |
| 2 | harness events: post-`invoke` hook on `WRITING_TOOLS` (`tool_loop.rs:108`) plus `difftrack` paths (`difftrack.rs:25`) → `Request::Changed/Priority` | a `write` tool call through a scripted provider makes the export visible to the next prompt's map with **no** `scan::list_files` call (counter) | no |
| 3 | watcher thread + debounce drain + `.git` → `Resync` translation | an `fs::write` from the test (never through a tool) lands in the snapshot inside the debounce window; delete and rename are seen; a write under `target/` and one inside `.git/` produce no parse request | **yes — `notify`** |
| 4 | background worker wired into the engine; `pending` attribute; `quiesce` | with the batch held open by a test hook, the turn's map says `pending="1"` and the turn's own latency excludes parse time; `quiesce` returns as soon as the batch lands | no |
| 5 | LSP `didOpen`/`didChange`/`didClose`: buffer text as the source of truth for open documents; single-file `Request::Changed`; `documentSymbol` answers from the buffer | `didOpen` a buffer whose exports differ from disk → `documentSymbol` reports the buffer's exports; `didChange` re-indexes only that file; `didClose` falls back to disk (`query.rs:177`) | no |

Only phase 3 needs the dependency decision; each other phase is useful alone — 0 stops lost
same-tick edits and stops blocking readers, 1 stops re-ranking on a comment change, 2 makes the agent's
own writes visible without a walk, 4 takes the parse off the turn's critical path, 5 makes the LSP
correct for open buffers. Phase 3 before 4 also works, but 4 first gives the watcher a worker that
already coalesces and prioritises.

## 7. Deliberately left out

- **Public API → connected repositories.** titi has no notion of connected repositories: verified,
  `pub genome_root: Option<PathBuf>` is a single root (`runtime.rs:450`) read at exactly one path
  (`:1392`, `:1549`, `:1583`), with no repo list, no workspace set and no "public" marker anywhere in
  `titi-genome` or `titi-config`. Worse than missing plumbing, the *rule opposes it*: `MAX_DEFINERS = 1`
  (`lib.rs:96`) makes a name exported by two files ambiguous and gives it no edge at all, deliberately,
  to stop shared method names from making everything depend on everything — across repositories that
  would be the normal case. Doing this honestly needs three product decisions first (what a root set is,
  what "public" means, how a cross-root edge is named), none of them in this ticket. Recommend: no.
- **A persistent on-disk index.** `Genome::index` rebuilds from zero at every process start
  (`lib.rs:136-140`) and the LSP does the same (`lsp.rs:12`). Taimyr makes a *running* index cheap and
  leaves a cold start a full walk+parse+rank. Caching the graph keyed by content hash is the obvious next
  ticket, not this one — but it is why §8's hash question is worth answering now.
- **tree-sitter incremental reparse** (`Parser::parse` with an `old_tree`). "Incremental" here means
  re-parsing only changed files, not only changed *regions*: once invalidation exists the parse is
  bounded by the files the agent touched, and a `Tree` per file costs memory for a second-order gain.
- **Watching the wider world** — a network filesystem, a container mount, a symlinked root. Out of scope;
  the watcher's ignore rules are the scanner's (§4) and nothing more.

## 8. Open questions for the owner

1. **`notify`, yes or no — and only at phase 3?** Recommendation: yes at phase 3, after the
   `dependency-update` checklist (license, maintenance, `cargo audit`, minimal features) is run against
   the registry — nothing on this machine could verify any of it. If declined, ship 0/1/2/4/5.
2. **The staleness signal's shape in the prompt.** `<genome pending="N">` (`project.rs:41`) is
   model-visible, costs a few header bytes and needs no new surface. The alternatives — a TUI note only
   (the model stays blind) or nothing (silent staleness, which the spec forbids) — are worse.
   Recommendation: the attribute.
3. **How long may a turn wait for freshness?** Recommendation: `quiesce(40 ms)` on the prompt path,
   200 ms in the LSP, unbounded never — a config knob, not a constant, so it retunes without a code
   change.
4. **Is keeping raw mentions per file acceptable?** Recommendation: yes — the only way an export change
   avoids a full re-resolution pass, bounded by `MAX_REFS = 512` (`refs.rs:19`).
5. **Hash algorithm — in-process or stable?** The sibling's uncommitted `content.rs` already picks a
   hand-rolled FNV-1a, on the argument that the value never leaves the process and is not a security
   boundary; that is sound, and it costs exactly one thing: a future on-disk cache (§7) could not trust
   it. `sha2` is **already a direct dependency of two workspace crates** (`titi-memory`,
   `titi-providers`, `Cargo.lock:2240`), so it adds no compile unit. Recommendation: FNV-1a now, `sha2`
   only if a persistent index lands.
