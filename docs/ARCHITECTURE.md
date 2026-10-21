# ARCHITECTURE — atlas, tools, and agent state

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
| titi-mcp | MCP client over stdio: servers, their tools as `mcp__<server>__<tool>` |
| titi-atlas | Repo walk, PageRank, projection into the system message |
| titi-core | JSONL sessions, search, trajectory |
| titi-memory | Memory index: full text and local embeddings |
| titi-soul | Identity slot, validated before it enters the prompt |
| titi-config | Layered settings |
| titi-secrets | `.env` and `auth.db` |

A surface (TUI or headless) never calls a model. It sends `EngineCommand`
and renders `EngineEvent`; everything else sits under `titi-engine`.

## Atlas: a ranked map instead of blind search

Code: `crates/titi-atlas/src/{lib,scan,graph,project,refs,scope,recency,symbols}.rs`,
`crates/titi-atlas/src/lang/{mod,support}.rs` and one module per language,
example `examples/map.rs`. Обзор темы — `docs/research/README.md`.

### What gets built

`Atlas` holds `files: HashMap<String, FileRecord>`, `ranks`, `dependents`,
`symbols: HashMap<String, SymbolRecord>` and `used_by`.

- `FileRecord { path, language, exports, imports, reexports, inherits,
  external_uses, uses, raw_refs, size, mtime, … }`. `uses` is the resolved
  half of `raw_refs`: each entry names the one file it is a use of and why
  (`Basis::Scope` or `Basis::Unique`).
- `FileRecord::symbols: Vec<SymbolSite>` (`symbol.rs`) is the per-symbol
  contract: `SymbolSite { name, kind: SymbolKind, span: (start_line,
  end_line), signature: Option<String> /* one line, ≤ 120 chars, built with
  one_line_signature */, exported: bool }`, one entry per export in export
  order (plus private items a module chooses to describe). A language module
  fills `ParsedFile::symbols`; one that has not yet leaves it empty and the
  index derives one entry per export from `export_sites` (kind `Other`, a
  one-line span, no signature).
- `SymbolRecord { files, users }`: who defines a name and, summed over the
  definers, how many distinct files use it. `used_by` is the per-definer
  view the prompt map prints: definer path → `(export, distinct files whose
  uses resolve to this file)`.
- **Two kinds of edges, one graph.** A file imports another file, or a use in
  it resolves to a definition in another. Both feed the same ranking and the
  same dependents count `(→N)`, so "what depends on this" is answered at
  symbol level too.
- **A use resolves to a definition only with evidence** (`scope.rs`). The
  graph used to key edges by bare name, so any file exporting `sync` or `row`
  collected every file that wrote the word. Now a use of `N` in `F` links to
  definer `D` when `D` is in `F`'s scope: reached through `F`'s resolved
  imports; behind a re-export of one (`pub use`, `pub mod x;`, a TypeScript
  barrel's `export … from`, a Python `__init__.py`); the `lib.rs` of a
  workspace crate `F` names (`use titi_providers::X`; the crate is found by
  its manifest's `[lib] name`/`[package] name`, else its directory, and `F`
  also gets an edge to that `lib.rs`); an ancestor's imports
  through `use super::*`; or, for Go, Java, Kotlin and C#, the same package
  directory. Python, TypeScript, JavaScript, PHP, Ruby, Swift and C reach only
  through imports and includes. Two definers in scope, or several and none in
  scope, resolve to nothing. A name exactly one file of the same language
  defines may still link without evidence, as a `Unique` guess, if it is at
  least 5 characters and not in the common-word list (`scope::COMMON`: `sync`,
  `model`, `row`, `table`, …). A common word links only to a file `F` imports
  itself, and a path ending in one (`tokio::sync`) is not collected at all.
  `use std::…::Name` and a `std::…::Name` path are the standard library's, never
  a project's.
- **Ranking is weighted PageRank** (`graph.rs`): `DAMPING = 0.85`,
  `ITERATIONS = 20`, start at `1.0 / n` per node. An edge weighs the square
  root of the evidence behind it — one unit per import and per scoped use, a
  quarter per guess — so a file that leans on four names of another is more
  tied to it than one that mentions a single name, and a file spreads its rank
  over its edges in proportion to their weights. A file's edges weigh at least
  1 together (`MIN_OUTBOUND`): a file whose only edge is a guess passes half
  its rank to it, not all of it.
- **A path has a role** (`role.rs`). Tests (`test`/`tests`/`spec`, `*_test.go`,
  `FooTests.cs`, `*.spec.ts`, `conftest.py`), examples, docs, fixtures,
  benchmarks, vendored and generated files (`*.generated.*`, `*_pb2.py`,
  `*.min.js`) are *auxiliary*: they start with a quarter of the rank
  (`AUXILIARY_SHARE`), their edges weigh 0.7 (`AUXILIARY_EDGE`), and the map
  scores them at a quarter (`AUXILIARY_FACTOR`) unless the session touched
  them. Entry points (`main.*`, `Program.cs`, `lib.rs`, a shallow `index.*`,
  a package's `__init__.py`) start with four times the rank and are scored at
  twice. `(→N)` still counts every dependent, tests included.
- **`(→N)` counts distinct files with an edge to this one**, and `+Name (n)`
  counts distinct files whose uses of `Name` resolve to *this* file. Two files
  defining `Shared` are counted separately.
- **`MAX_DEFINERS = 1` is no longer an edge rule.** It still decides what
  `titi atlas check` calls an ambiguous symbol and whether goto-definition
  has one answer.
- **References come from use sites.** `collect_refs` takes calls (`name(`),
  path-qualified names (`::name`), and capitalised identifiers (types and
  constructors). Bare lowercase identifiers are skipped on purpose: a local
  `path` is not a dependency on the file exporting `path`. Keywords and
  ubiquitous std names are filtered; `MAX_REFS = 512` per file.
- **The walk is cheap and honours ignore files.** `scan.rs` reads
  `.gitignore`, `.reference-productignore` and `.atlasignore` (`!`, `dir/`,
  `/anchor`, `*`, `**`, `?`); `.atlasignore` is the atlas's own file and, unlike
  the other two, is also read in nested directories (rules relative to the
  file's directory). titi's own `.atlasignore` hides `evals/tasks/`. It skips dot-directories, `node_modules`, `dist`, `build`,
  `target`, `coverage`, `vendor`, `__pycache__`, `.git`, files over
  `MAX_FILE_BYTES = 1_000_000`, non-source files, and names with control
  characters (they would break the line-based projection).
- **Refresh is incremental.** `refresh()` re-parses only files whose `size`
  or `mtime` changed, drops deleted ones, and recomputes ranks every time
  (cheap next to parsing).

### Languages

One language is one module under `crates/titi-atlas/src/lang/` plus one row
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
`Atlas::capabilities()` returns one entry per language — its level, its
extensions and its note — and it is a property of the build rather than of
the workspace, so it answers without indexing anything. `titi atlas
capabilities` prints it as a table (`rust  Full  .rs  exported items and
use/mod paths come from a syntax tree`, then `13 languages: 13 full,
0 heuristic`) and `titi atlas lsp` reports the
same roster in `initialize`'s `experimental.titiAtlas.languages`. `titi
atlas check` reports findings only, so a tree with nothing wrong still
answers `atlas: clean`, and the per-file LSP `diagnostic` reply never
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
- **A second look** (`reresolve.rs`) runs once per parse over what a module
  left unresolved. TypeScript/JavaScript: `./a.js` is `./a.ts`/`.tsx`, and a
  relative specifier may name a directory's `index.*`; an `export … from` it
  settles is a re-export. Go: with a root `go.mod`, a path below the module
  is the package directory (the file named like it, else the first non-test
  file), and a path outside the module is a dependency and drops out.

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
(`titi_memory::index::MemoryIndex`), and the atlas (`atlas_system()`). The
result goes first in the turn as `ChatMessage { role: Role::System, … }`.
Before each turn the index is refreshed and rendered with
`atlas.project_with(limit, &touched)`:

```
<atlas>
src/lib.rs (→12)
  +Atlas (9)
  +render (4)
src/runtime.rs (→3) [RECENT]
  +EngineRuntime (7)
</atlas>
```

- `(→N)` — files that depend on this one (file and symbol edges).
- `[RECENT]` — worked on lately. Inside a git repository: uncommitted
  changes (worktree, index or untracked) or a commit whose committer date is
  within 48 hours touched the file, so a fresh clone or checkout of old
  history marks nothing. Read once per index build by `recency.rs` (three git
  calls, 1.5 s budget, run beside the parse) and extended with every file the
  session re-parses. Outside git, or when `git status` does not answer in
  time: mtime within 48 hours.
- `+Name (users)` — up to 4 exports per file, sorted by user count, and an
  export nobody uses is omitted: the symbol-level blast radius next to the
  file-level `(→N)`.
- Files the session read or edited get `TOUCHED_BOOST = 3.0`, so the map
  follows the work. An unknown touched path does not change the order
  (tested).
- Paths containing `<` or `>` are dropped, so a file name cannot forge the
  closing tag — prompt-injection hygiene.
- File limit: `EngineConfig::atlas_limit`, default 24, overridden by the
  `atlas.limit` setting (1..=64, the project `.titi/config.yml` winning);
  `TITI_NO_ATLAS=1` drops the map.
- **The lens** (`lens.rs`, `<atlas-lens>`) is the part of the repository *this*
  prompt is about, built per turn from seeds and a personalised PageRank, no
  model involved. Seeds: files the prompt names (`src/a.rs`, `a.rs:42`, a
  stack frame; 5), identifiers it spells like an exported symbol (3 exact,
  1.5 ignoring case, a plain lowercase word only 0.4 and only when 6+
  characters and not a common word), prompt words that are a file stem (2) or
  a path component (0.5), files the session touched (2, decaying by 0.85 per
  step back) and files the working tree changed (2). The walk follows each
  dependency edge both ways, the reverse at half weight, 20 passes at
  damping 0.85, and a seed's own weight is added back so the file the task
  names is listed first. The block lists up to 12 files within 400 estimated
  tokens (`estimate_tokens`, one token per 3.4 characters), each as `path
  (→N) reason +Symbol…` where the reason says why (`named in the prompt`,
  `defines X`, `touched this session`, `uses a.rs`, `used by a.rs`), and ends
  with `unmapped: …` for identifier-looking prompt words no symbol carries. A
  prompt with no seed has no lens. It rides on the newest user message after
  `<atlas>`, is left out when the newest earlier lens lists the same files,
  and is replaced by a one-line stub in the pass that masks old tool results
  at compaction, so the cached prefix is never edited between turns.
- The turn also carries a `<diff>` block — `git diff HEAD`, bounded to 8
  files, 120 lines and 6000 bytes — right after `<atlas>`, and its paths join
  the touched set the map reads. Secret-named files and hunks with
  secret-shaped assignments are dropped whole, and duck mode gets neither
  block (`crates/titi-engine/src/difftrack.rs`).

**Why it matters:** on the first turn the model already sees the core files
ranked, how risky each is to touch, which symbols have real reach, and
where work is happening — for the cost of one text block.

### Commands

```bash
cargo run -p titi-atlas --example map -- <path> <N>   # defaults: . and 24
titi-map <path> <N>                                    # prebuilt, skill atlas-map
titi atlas check                                      # index diagnostics, exit 1 on any
titi atlas dump [--json] [--limit n]                  # the whole index in map order: role, rank, dependents, exports, edges
titi atlas lens --task <text> [--touched a,b] [--json]   # the lens a prompt would get (the files the task is about)
titi atlas capabilities                               # level and extensions per language
titi atlas lsp                                        # stdio LSP: documentSymbol, definition, references, diagnostic
```

stderr prints `"{files} files, {edges} edges"`, stdout the projection.
`TITI_NO_ATLAS=1` disables the map: `crates/titi-cli/src/engine.rs` sets
`atlas_root` to the cwd only when the variable is unset, and
`atlas_root = None` means no atlas at all. `titi atlas check` prints
`path:line: code: message` per diagnostic and exits 1 on any Warning or
Error (`atlas: clean`, exit 0, when there is nothing wrong — an
`ambiguous-symbol` line is `Info` and does not change the exit code). The
line is the one the problem is on (the import's, the first syntax error's),
`Atlas::check()` returns every finding, and the command prints the first 200
and ends with `… and N more (T total: code n, …)` rather than a silent cap.
`titi atlas capabilities` prints the per-language roster described above
and needs no workspace at all. `titi atlas lsp` serves those symbols over
LSP stdio framing.

## Tools and approval

Code: `crates/titi-tools/src/{lib,fs,cache}.rs`; the approval gate is
`crates/titi-engine/src/tool_loop.rs`, subagent policy
`crates/titi-engine/src/tool_agent.rs`.

`workspace_tools(root)` builds exactly six tools: `read`, `write`, `edit`,
`glob`, `grep`, `bash`. The CLI registers them via
`workspace_tools_with_cache(&workspace, read_cache)` plus `MemoryTool` from
`titi-memory`, in a `ToolRegistry` of `ToolHandler` / `ToolDefinition`.

### MCP servers

Code: `crates/titi-mcp/src/{client,manager,tool}.rs` (client, supervisor,
registry handlers), `crates/titi-config/src/mcp.rs` (files, hash),
`crates/titi-engine/src/mcp.rs` (the engine's side),
`crates/titi-cli/src/{mcp_setup,mcp_panel,hooks_trust}.rs`.

The surface reads `<agent_dir>/mcp.json` (runs unconditionally) and
`.titi/mcp.json` / `.mcp.json` (only with the user's recorded consent, hashed
like hooks) and sends the approved list as `EngineCommand::SetMcp`; the
engine reads no MCP file. Each server is a child process (own process group,
allow-listed environment plus its `env`, stderr in `<agent_dir>/mcp/<server>.log`)
spoken to over newline-delimited JSON-RPC, protocol 2025-11-25. Servers start
in the background; a prompt waits up to 10 s for servers still starting. The
engine swaps a server's tools into its registry when the manager reports a
change (a running turn keeps the registry it started with), and reports
`EngineEvent::McpStatus`. Every `mcp__` tool is exec tier unless the user's own
`mcp.<server>.approval` lowers it for the user's own server.

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

One settings key changes a tool bound: `bash.autoBackground.thresholdMs`
(integer milliseconds, 1 to 3 600 000, default 60 000) is how long a `bash`
call holds a turn before the runtime hands it to the background. It is read
through the effective view into `EngineConfig::background_after`; a value of 0
or past an hour refuses startup by name, and `TITI_BASH_BACKGROUND_MS` overrides
the key when set.

Two more keys bound the tool loop: `agent.maxToolRounds` (1 to 500, default 100;
`EngineConfig::max_tool_rounds`) and `agent.subagentMaxToolRounds` (1 to 500,
default 40; `agent_rounds`). At the main turn's limit nothing is failed: one
last request goes out with tool calls forbidden (`WireRequest::forbid_tool_calls`,
tools still declared so the cached prefix and Anthropic's tool-history rule hold)
and a note asking for what is done and what is left; its answer streams and the
turn ends with a notice. `tool_budget::LoopWatch` appends a one-line nudge, once
per turn, to a tool result when the same call has come back with the same result
three times, or four rounds in a row only re-read unchanged files.

**Why it matters:** `git status` after a session shows nothing new, keys
cannot end up in a commit, profiles are isolated from each other, and a
clone carries no one's chat history or credentials.

## Sources

- `crates/titi-atlas/src/{lib,scan,graph,project,refs,scope,recency,symbols}.rs`,
  `crates/titi-atlas/src/lang/*.rs`, `examples/map.rs`
- `crates/titi-tools/src/{lib,fs,cache}.rs`
- `crates/titi-engine/src/{runtime,tool_loop,tool_agent,protocol}.rs`
- `crates/titi-config/src/{lib,settings}.rs`, `crates/titi-cli/src/engine.rs`
- `README.md` (Atlas, tool jail, subagents, state storage)
