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

Code: `crates/titi-genome/src/{lib,scan,graph,project,refs,symbols}.rs`,
`crates/titi-genome/src/lang/{mod,support}.rs` and one module per language,
example `examples/map.rs`. Обзор темы — `docs/research/README.md`.

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

One language is one module under `crates/titi-genome/src/lang/` plus one row
in `LANGS`. The row is the only place that says how a language is recognised:
its extensions, its grammar when one is linked, and its parser. So
`Language::from_path`, the extensions the scanner indexes, the grammar
`ast_edit` needs and the dispatch in `lang::parse` all read the same table —
adding a language is one module and one row, not four lists to keep in step.
`refs.rs` holds the identifier work every language shares.

Every `Language` has a **level**, which is what a user sees when they ask:

- **`Full`** — exports come from a real syntax tree. This is every language
  the table recognises, and by construction: a row's `grammar` is `Some(..)`
  exactly when the row is `Full`, which the table's own test asserts. Some
  rows use more than one grammar — `.tsx`/`.js`/`.jsx`/`.mjs`/`.cjs` parse with
  the TSX grammar and `.ts`/`.mts`/`.cts` with the TypeScript one, and a `.h`
  the C grammar cannot read is retried with the C++ grammar — and each row's
  `note` says what its parser reads.
- **`Heuristic`** — patterns over the text, which is exactly what a row with
  `grammar: None` means. **No row is in this state today.** It is the honest
  word for a language added without a grammar: such a row declares
  `grammar: None` and must claim this level instead of `Full`, and the table's
  test fails when a row becomes non-`Full`, so the step is deliberate rather
  than a silent downgrade of what the index claims.
- **`Unsupported`** — a path indexed for reads that contributes no symbols.
  Its live instance is `Language::Unsupported`, the catch-all for a path no row
  claims (`a.txt`): never indexed, and it contributes nothing if it were. The
  variant is the word for a language the index reads but cannot parse, of
  which "unknown extension" is the current instance.

The level is surfaced where a user asks for it, never in the findings.
`Genome::capabilities()` returns one entry per language — its level, its
extensions and its note — and it is a property of the build rather than of
the workspace, so it answers without indexing anything. `titi genome
capabilities` prints it as a table (`rust  Full  .rs  exported items and
use/mod paths come from a syntax tree`, then `13 languages: 13 full,
0 heuristic`) and `titi genome lsp` reports the
same roster in `initialize`'s `experimental.titiGenome.languages`. `titi
genome check` reports findings only, so a tree with nothing wrong still
answers `genome: clean`, and the per-file LSP `diagnostic` reply never
carries a capability: a constant hint in every file is noise, and a vetter
that also narrated its own reach could no longer say "clean".

- **Imports** (file edges): Rust (`use`, `mod`, read from the syntax tree and
  resolved against the module its declaration sits in — `crate::`, `super::`,
  `self::`, a glob, and `mod x;` beside a `mod.rs` all land on a file; a bare
  path is an external crate, and a path that resolves to the importing file
  is a self-edge and is dropped), TS/JS (`from '…'` / `import '…'`), Python
  (`from .x import` / `from x.y import`), Go (`import` blocks and lines,
  resolved by package path suffix), Java (`import`, resolved by suffix), C#
  (`using`, by suffix), Kotlin (`import`, by suffix), PHP (`use`, by suffix),
  Ruby (`require` / `require_relative`), C/C++ (`#include "…"` relative to the
  file), Swift (`import`, resolved to `<Module>/<Module>.swift` when the
  workspace holds it).
- **An import that resolves to nothing is only a diagnostic when it is this
  workspace's business.** A specifier that names something outside the
  workspace — `java.util.List`, `using System;`, `require 'json'`, Go's `fmt`,
  `from sqlalchemy.orm import x` — is not a missing file, and reporting it
  would put a warning on every file that uses a library. Each module decides
  this as honestly as the language lets it: the own root of the file's
  `package`/`namespace` declaration for Java, C#, Kotlin and PHP (a specifier
  under another root is another world), a dot in the first path segment for
  Go (`fmt` is std, `example.com/x` is a module), the workspace-holds-that-
  directory probe for Python, exact-for-relative and
  module-path-for-bare for TS/JS, `require_relative` versus `require` for
  Ruby, and the `<Module>/<Module>.swift` convention for Swift. Rust is exact:
  only `crate::`, `self::` and `super::` name a file. A quoted `#include` that
  names no file *is* reported — it is a real error — while `<system.h>` is not
  an include this crate looks at.

Symbol edges (`used_symbols`) come to every language through `finish()` →
`refs::collect_refs()`, and every language contributes export sites.

`syntax_errors` counts tree-sitter `ERROR` nodes for a parsed language, from a
parse with the pinned grammar's `&raw` token ambiguity neutralised, so
borrowing an ordinary identifier named `raw` is not reported as a syntax error
— a file that genuinely does not parse still is. Heuristic languages with no
grammar report zero: this crate does not invent errors it did not parse.


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
src/runtime.rs:(→3) [RECENT]
  +EngineRuntime (7)
</genome>
```

- `(→N)` — files that depend on this one (file and symbol edges).
- `[RECENT]` — recently modified: mtime within `NEW_WINDOW = 48 * 3600`
  seconds, which is not the same as newly created.
- `+Name (users)` — up to 4 exports per file, sorted by user count, and an
  export nobody uses is omitted: the symbol-level blast radius next to the
  file-level `(→N)`.
- Files the session read or edited get `TOUCHED_BOOST = 3.0`, so the map
  follows the work. An unknown touched path does not change the order
  (tested).
- Paths containing `<` or `>` are dropped, so a file name cannot forge the
  closing tag — prompt-injection hygiene.
- File limit: `EngineConfig::genome_limit`, default 24, overridden by the
  `genome.limit` setting (1..=64, the project `.titi/config.yml` winning);
  `TITI_NO_GENOME=1` drops the map.
- The turn also carries a `<diff>` block — `git diff HEAD`, bounded to 8
  files, 120 lines and 6000 bytes — right after `<genome>`, and its paths join
  the touched set the map reads. Secret-named files and hunks with
  secret-shaped assignments are dropped whole, and duck mode gets neither
  block (`crates/titi-engine/src/difftrack.rs`).

**Why it matters:** on the first turn the model already sees the core files
ranked, how risky each is to touch, which symbols have real reach, and
where work is happening — for the cost of one text block.

### Commands

```bash
cargo run -p titi-genome --example map -- <path> <N>   # defaults: . and 24
titi-map <path> <N>                                    # prebuilt, skill genome-map
titi genome check                                      # index diagnostics, exit 1 on any
titi genome capabilities                               # level and extensions per language
titi genome lsp                                        # stdio LSP: documentSymbol, definition, references, diagnostic
```

stderr prints `"{files} files, {edges} edges"`, stdout the projection.
`TITI_NO_GENOME=1` disables the map: `crates/titi-cli/src/engine.rs` sets
`genome_root` to the cwd only when the variable is unset, and
`genome_root = None` means no genome at all. `titi genome check` prints
`path:line: code: message` per diagnostic and exits 1 on any Warning or
Error (`genome: clean`, exit 0, when there is nothing wrong — an
`ambiguous-symbol` line is `Info` and does not change the exit code).
`titi genome capabilities` prints the per-language roster described above
and needs no workspace at all. `titi genome lsp` serves those symbols over
LSP stdio framing.

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
override settings, but state stays in `agent_dir`. A cloned repo is untrusted
input, so three things are read from the user's layers only (`get_user`):
`privacy.allow`, `privacy.maskIps`, and the provider catalog (`providers`,
`models`), whose entries name the URL a key is sent to.

**Why it matters:** `git status` after a session shows nothing new, keys
cannot end up in a commit, profiles are isolated from each other, and a
clone carries no one's chat history or credentials.

## Sources

- `crates/titi-genome/src/{lib,scan,graph,project,refs,symbols}.rs`,
  `crates/titi-genome/src/lang/*.rs`, `examples/map.rs`
- `crates/titi-tools/src/{lib,fs,cache}.rs`
- `crates/titi-engine/src/{runtime,tool_loop,tool_agent,protocol}.rs`
- `crates/titi-config/src/{lib,settings}.rs`, `crates/titi-cli/src/engine.rs`
- `README.md` (Genome, tool jail, subagents, state storage)
