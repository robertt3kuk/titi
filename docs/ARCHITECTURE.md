# ARCHITECTURE — genome, tools, and agent state

How titi orients itself in an unfamiliar repo, how it acts safely, and where
it keeps its state. Every number and name below is taken from the code; the
source files are listed at the end. If the code changes, the code wins.

## Crates and control flow

| Crate | Purpose |
|---|---|
| titi-cli | The `titi` binary: screen, headless, keys |
| titi-tui | Frame, composer, panels, themes. Knows nothing about models |
| titi-engine | Protocol, turn loop, provider registry, subagents |
| titi-providers | Transport and stream parsing |
| titi-tools | Tools and the read / write / exec tiers |
| titi-genome | Repo walk, PageRank, projection into the system message |
| titi-core | JSONL sessions, search, trajectory |
| titi-memory | Memory index: full text and local embeddings |
| titi-soul | Identity slot, validated before it enters the prompt |
| titi-config | Layered settings |
| titi-secrets | `.env` and `auth.db` |

A surface (TUI or headless) never calls a model. It sends `EngineCommand`
and renders `EngineEvent`; everything else sits under `titi-engine`.

## Genome: a ranked map instead of blind search

Code: `crates/titi-genome/src/{lib,scan,parse,graph,project}.rs`, example
`examples/map.rs`. Spec: `docs/research/reference-product-port/` (E3).

### What gets built

`Genome` holds `files: HashMap<String, FileRecord>`, `ranks`, `dependents`,
and `symbols: HashMap<String, SymbolRecord>`.

- `FileRecord { path, language, exports, imports, used_symbols, size, mtime }`.
- `SymbolRecord { files, users }`: who defines a symbol and how many distinct
  files reference it.
- **Two kinds of edges, one graph.** A file imports another file, or a file
  references a symbol another file defines. Both feed the same ranking and
  the same dependents count `(→N)`, so "what depends on this" is answered at
  symbol level too.
- **Ranking is PageRank** (`graph.rs`): `DAMPING = 0.85`, `ITERATIONS = 20`,
  start at `1.0 / n` per node.
- **Ambiguous names get no edges.** `MAX_DEFINERS = 1`: a name exported by
  more than one file gets neither edges nor a user count. Otherwise `new`,
  `len`, `is_empty` would make every file depend on every other. Resolution
  is by name only, so ambiguity is excluded rather than guessed.
- **References come from use sites.** `collect_refs` takes calls (`name(`),
  path-qualified names (`::name`), and capitalised identifiers (types and
  constructors). Bare lowercase identifiers are skipped on purpose: a local
  `path` is not a dependency on the file exporting `path`. Keywords and
  ubiquitous std names are filtered; `MAX_REFS = 512` per file.
- **The walk is cheap and honours ignore files.** `scan.rs` reads
  `.gitignore` and `.reference-productignore` (`!`, `dir/`, `/anchor`, `*`,
  `**`, `?`). It skips dot-directories, `node_modules`, `dist`, `build`,
  `target`, `coverage`, `vendor`, `__pycache__`, `.git`, files over
  `MAX_FILE_BYTES = 1_000_000`, non-source files, and names with control
  characters (they would break the line-based projection).
- **Refresh is incremental.** `refresh()` re-parses only files whose `size`
  or `mtime` changed, drops deleted ones, and recomputes ranks every time
  (cheap next to parsing).

### Languages

`Language` (`parse.rs`) lists 12 named languages plus `Other`: Rust,
TypeScript, Python, Go, Java, C, Cpp, CSharp, Ruby, Kotlin, Swift, Php. The
`EXTENSIONS` table is the single source of truth for `Language::from_path`:
`rs`; `ts tsx js jsx mjs cjs`; `py`; `go`; `java`; `c h`;
`cpp cc cxx hpp hh hxx`; `cs`; `rb`; `kt kts`; `swift`; `php`.

- **Imports and symbols** (file edges and symbol edges): Rust (`use`, `mod`),
  TS/JS (relative `from '…'` / `import '…'`), Python (relative
  `from .x import`), Go (block and line `import`, resolved by package path
  suffix), Java and C# (shared `parse_path_imports`, resolved by suffix),
  Kotlin, PHP, Ruby (`require` / `require_relative`), C/C++
  (`#include "…"` relative to the file).
- **Symbols only:** Swift — `finish(source, swift_exports(source), Vec::new())`
  parses no imports. That is why the README says "eleven languages": eleven
  produce import edges.

Symbol edges (`used_symbols`) come to every language through `finish()` →
`collect_refs()`; Swift simply lacks the file-edge half. Parsing is regex
heuristics; tree-sitter is not there yet.

### How the map reaches the prompt

`EngineRuntime::system_prompt()` (`titi-engine/src/runtime.rs`) joins three
parts: identity (`titi_soul::SystemPromptBuilder`), recalled memory
(`titi_memory::index::MemoryIndex`), and the genome (`genome_system()`). The
result goes first in the turn as `ChatMessage { role: Role::System, … }`.
Before each turn the index is refreshed and rendered with
`genome.project_with(limit, &touched)`:

```
<genome>
src/lib.rs:(→12)
  +Genome (9)
  +render (4)
src/runtime.rs:(→3) [NEW]
  +EngineRuntime (7)
</genome>
```

- `(→N)` — files that depend on this one (file and symbol edges).
- `[NEW]` — mtime within `NEW_WINDOW = 48 * 3600` seconds.
- `+Name (users)` — up to 8 exports per file, sorted by user count: the
  symbol-level blast radius next to the file-level `(→N)`.
- Files the session read or edited get `TOUCHED_BOOST = 3.0`, so the map
  follows the work. An unknown touched path does not change the order
  (tested).
- Paths containing `<` or `>` are dropped, so a file name cannot forge the
  closing tag — prompt-injection hygiene.
- File limit: `EngineConfig::genome_limit`, default 24.

**Why it matters:** on the first turn the model already sees the core files
ranked, how risky each is to touch, which symbols have real reach, and
where work is happening — for the cost of one text block.

### Commands

```bash
cargo run -p titi-genome --example map -- <path> <N>   # defaults: . and 24
titi-map <path> <N>                                    # prebuilt, skill genome-map
```

stderr prints `"{files} files, {edges} edges"`, stdout the projection.
`TITI_NO_GENOME=1` disables the map: `crates/titi-cli/src/engine.rs` sets
`genome_root` to the cwd only when the variable is unset, and
`genome_root = None` means no genome at all.

## Tools and approval

Code: `crates/titi-tools/src/{lib,fs,cache}.rs`; the approval gate is
`crates/titi-engine/src/tool_loop.rs`, subagent policy
`crates/titi-engine/src/tool_agent.rs`.

`workspace_tools(root)` builds exactly six tools: `read`, `write`, `edit`,
`glob`, `grep`, `bash`. The CLI registers them via
`workspace_tools_with_cache(&workspace, read_cache)` plus `MemoryTool` from
`titi-memory`, in a `ToolRegistry` of `ToolHandler` / `ToolDefinition`.

### Tiers and modes

| Tool | `ApprovalTier` |
|---|---|
| `read`, `glob`, `grep` | `Read` |
| `write`, `edit` | `Write` |
| `bash` | `Exec` |

An unknown tool is treated as `Exec`: failure errs toward strictness.

`ApprovalMode` (CLI `--approval always-ask|write|yolo`, default `write`):

- `Write` — auto-approves only `Read`; `Write` and `Exec` ask.
- `AlwaysAsk` — asks for everything.
- `Yolo` — approves everything (a deliberate choice for headless, which has
  no approval panel).

`execute_tools` looks up `tools.approval_tier(&call.name)`; if the mode does
not auto-approve it, it emits `EngineEvent::ToolApprovalNeeded { turn_id,
call_id, name, … }` and blocks on a oneshot in `ApprovalWaiters` until
`EngineCommand::ApproveTool { call_id, approved }`. In the TUI that is
`y` / `n`.

### The workspace jail

`jail_path(root, raw)` in `fs.rs` canonicalises both the root and the
candidate (the parent, for a file that does not exist yet) and requires
`resolved.starts_with(root)`, else `"{path} is outside the workspace"`.
`read`, `write`, `edit`, `glob`, and `grep` go through it. `bash` runs
`sh -c <cmd>` with `current_dir(root)` and is not path-jailed — which is
exactly why it is the only `Exec` tool.

### Subagents are read-only by default

`EngineConfig::agent_writes` defaults to `false`. At startup the runtime
then calls `ToolRegistry::retain_tiers(&[ApprovalTier::Read])` on the
subagent registry: no write or exec tool is registered, so no call can wait
for an approval that this surface cannot show. The registry handed over *is*
the policy; escalation is the caller's decision, made once at construction.
Subagents share the read cache with the main turn.

**Why it matters:** reading costs no friction, mutation is always visible,
misconfiguration closes capability instead of opening it, subagents cannot
hang on approvals by construction, and file tools cannot leave the
workspace.

## Agent state lives outside the repo

`titi_config::agent_dir()` resolves:

1. `$TITI_AGENT_DIR` — overrides everything;
2. else `$TITI_PROFILE` (non-empty, not `default`) →
   `~/.titi/profiles/<name>/agent`;
3. else `~/.titi/agent`.

Under `agent_dir`:

- sessions — `sessions/*.jsonl`, indexed in `state.db` (SQLite WAL + FTS5);
- trajectories — `trajectories/<session_id>.jsonl`;
- keys — `auth.db`, read through `LayeredCredentialSource` after the env and
  `.env` layers (`/keys` lists providers, never keys);
- memory and `SOUL.md`.

Settings layer bottom-up: defaults ← `<agent_dir>/config.yml` ←
`<project>/.titi/config.yml` (read-only through this API) ←
`$TITI_CONFIG_FILES` and explicit overlays ← runtime overrides. A project can
override settings, but state stays in `agent_dir`.

**Why it matters:** `git status` after a session shows nothing new, keys
cannot end up in a commit, profiles are isolated from each other, and a
clone carries no one's chat history or credentials.

## Sources

- `crates/titi-genome/src/{lib,scan,parse,graph,project}.rs`, `examples/map.rs`
- `crates/titi-tools/src/{lib,fs,cache}.rs`
- `crates/titi-engine/src/{runtime,tool_loop,tool_agent,protocol}.rs`
- `crates/titi-config/src/{lib,settings}.rs`, `crates/titi-cli/src/engine.rs`
- `README.md` (Genome, tool jail, subagents, state storage)
