# Agent tracing — Laminar-shaped spans for a titi session

Status: **Phase A is implemented** (core `feb9e3c`/`1f6d718`, CLI and panel
`9501184`, engine `a6e4955`); Phase B step 1 (thinking metrics and search) is in
as well, and Phase C is not started. §6/§7 below are the shapes the code uses:
`crates/titi-core/src/trace.rs` (span model, writer, reader, tree, retention,
thinking cap), `crates/titi-cli/src/trace_cmd.rs` (`titi trace [session]
[--turn N]`, `--search`, `--all`), the `/trace` panel in `pickers.rs`, and
`crates/titi-engine/src/spans.rs` (the sink the engine writes through). §10
records the answers the owner gave to the questions this plan raised. Owner ask,
2026-10-09: *"We should be able to stack-trace the agents as Laminar does — at
least minimally — and analyze the thinking part too if needed."*

Method: primary sources read over the network on 2026-10-09 (the OTel GenAI
semantic conventions repo and `lmnr-ai/lmnr`), plus the titi tree at `27c06e5`.
`web_search` was not used (it loops in this environment); every web claim below
is quoted from a fetched file, and every titi claim carries `file:line`.
Network reading worked — the two repos were cloned by `curl`/`git tree` and the
relevant files are cited inline.

---

## 1. What Laminar's trace model is

Laminar is an OpenTelemetry-native observability platform for agents. A **trace**
is one agent run; a trace is a tree of **spans** (LLM calls, tool calls, nested
agents, plain functions); a **session** groups the traces of one conversation.

### 1.1 The span row

The frontend's span type is the ground truth of what a viewer needs
(`lmnr/frontend/lib/traces/types.ts:27-63`):

```
spanId, parentSpanId?, traceId, spanType, name,
startTime, endTime, attributes, status, input, output,
events, path, model,
inputTokens, outputTokens, totalTokens,
cacheReadInputTokens?, reasoningTokens?
```

- **`parentSpanId`** is the nesting link; `path` is the human breadcrumb
  (`["poemWriter","chat"]`), and `path`/`idsPath` can also be reconstructed from
  the `lmnr.span.path` / `lmnr.span.ids_path` attributes when a producer cannot
  set a real parent (`lmnr/app-server/src/traces/spans.rs:459-524` — `path()`,
  `raw_path()`, `ids_path()`).
- **`spanType`** is the kind. Frontend enum
  (`frontend/lib/traces/types.ts:15-25`): `DEFAULT, LLM, EXECUTOR, EVALUATOR,
  EVALUATION, TOOL, HUMAN_EVALUATOR, EVENT, CACHED`. The Rust side adds a
  `PIPELINE` value and parses the string form
  (`app-server/src/db/spans.rs:22-66`).
- Token/cost/`reasoningTokens` are **per-span columns** and only LLM spans
  count: `spans.cache_read_input_tokens` / `cache_creation_input_tokens` /
  `reasoning_tokens` were added in CH migration `61`, fed from `SpanUsage`
  (`reasoning = int_attr("gen_ai.usage.reasoning_tokens")`), and the whole
  aggregation is gated on `is_llm_span()`
  (`lmnr/docs/internal/clickhouse-traces.md`, *Per-span token detail columns*
  and *Trace token/cost aggregation*; `lmnr/docs/internal/frontend-trace-view.md`,
  *Trace-view Span Attributes*).

### 1.2 Span kind is inferred from the OTel GenAI conventions

`AppServerSpan::span_type` (`lmnr/app-server/src/traces/spans.rs:435-470`)
resolves the kind in this order:

1. `lmnr.span.type` attribute if set;
2. **`gen_ai.operation.name`**: `chat`/`text_completion`/`embeddings`/
   `generate_content` → `LLM`, `execute_tool` → `Tool`, **`invoke_agent` →
   `Default`** — with the comment *"agent runs are containers whose children
   carry the LLM/tool content"*;
3. `gen_ai.tool.call.*` present → `Tool`;
4. `gen_ai.system`/`gen_ai.request.model`/`gen_ai.response.model` present →
   `LLM`;
5. otherwise `Default`.

So Laminar's own answer to "what is an agent span" is: a container with no
model of its own, whose LLM and tool children carry the content and the usage.

### 1.3 Where thinking lives

Laminar does not have a first-class "thinking" entity. Reasoning is **content
inside the LLM span's messages**, plus a token count:

- the Responses-API parser maps `reasoning` items to the `assistant` role
  (`frontend-trace-view.md`, *Span-view Message Parsing*);
- `reasoningTokens` is summed onto the trace row from
  `gen_ai.usage.reasoning_tokens`;
- the transcript view renders the messages, so thinking shows as an assistant
  block.

### 1.4 The minimal useful viewer

From `lmnr/docs/internal/frontend-trace-view.md`: a **tree/timeline** of spans
(left) plus a **span panel** (right) that shows `input`, `output`, `attributes`,
and `events` for the selected span, with a **transcript** mode that renders the
messages. A trace view does **not** fetch the full attribute blob for the tree;
it extracts a fixed key set (`TRACE_VIEW_ATTRIBUTE_KEYS`) so tens-of-kB LLM
attributes do not ride every row. The essentials for a minimal clone are
therefore: id/parent, kind, name, start/end, status, tokens/cost/model, and the
folded input/output content.

### 1.5 The OTel GenAI conventions Laminar follows

The conventions moved out of the main semconv repo; the current text is
`open-telemetry/semantic-conventions-genai` (fetched 2026-10-09). The parts a
titi span model must copy to be exportable:

| Concept | Convention | Source |
| --- | --- | --- |
| Client inference span | name `{gen_ai.operation.name} {gen_ai.request.model}`, kind `CLIENT` | `docs/gen-ai/client-inference.md:58` |
| Tool span | name `execute_tool {gen_ai.tool.name}`, kind `INTERNAL` | `docs/gen-ai/gen-ai-spans.md:739-780` |
| Agent spans | `create_agent {gen_ai.agent.name}` / `invoke_agent {gen_ai.agent.name}`, kind `CLIENT` | `docs/gen-ai/gen-ai-agent-spans.md:36-190`, `:189-300` |
| Retries | one span covers *"the duration of the logical operation with all retries"* — retries are **not** child spans | `docs/gen-ai/gen-ai-spans.md:26-34` |
| Model/request | `gen_ai.request.model`, `gen_ai.request.temperature`, `gen_ai.request.max_tokens`, `gen_ai.request.reasoning.level` | `client-inference.md:74-91` |
| Usage | `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_write.input_tokens`, `gen_ai.usage.reasoning.output_tokens` | `client-inference.md:96-109` |
| Finish | `gen_ai.response.finish_reasons` (array, one per generation) | `client-inference.md:92` |
| Tool payload | `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result`, `error.type` | `gen-ai-spans.md:739-810` |
| Agent identity | `gen_ai.agent.id`, `gen_ai.agent.name`, `gen_ai.conversation.id` | `gen-ai-agent-spans.md:36-190` |
| Content | `gen_ai.system_instructions`, `gen_ai.input.messages`, `gen_ai.output.messages` — **Opt-In**, *"instrumentations SHOULD NOT capture them by default"* | `gen-ai-spans.md:905-945` |

Two consequences for titi's design, both taken from that text:

- **Retries belong inside the LLM span, not beside it.** A transient retry does
  not get its own span; the span's duration and `error.type` cover the logical
  call. (An *attempt count* can still be a span attribute — see §8.)
- **Content is opt-in.** Prompts, outputs and thinking are sensitive; the
  conventions make them an explicit choice. This matches titi's own trajectory
  posture.

---

## 2. What titi records and emits today

### 2.1 `titi-core` trajectory — the closest existing thing

`crates/titi-core/src/trajectory.rs` writes an append-only JSONL per session at
`<agent_dir>/trajectories/<session_id>.jsonl` (line 1-4, 111-114), one
`TrajectoryEvent { ts: u64 ms, seq: u64, kind }` per line (52-84). `EventKind`
(66-93) is exactly:

```
user_message, assistant_message,
tool_call { id, name, args },
tool_result { id, duration_ms, ok },
turn_end, compaction { folded, strategy }, gepa_review
```

Properties already worth reusing: monotonic gap-free `seq` (115-116, 173-175),
wall-clock `ts` (`crate::session::entry::now_ms`), buffered writes flushed on
`turn_end` and drop (176-193, 282-286), a torn-tail repair on every open
(`truncate_torn_tail`, 126-147, shared with the session store at
`session/store.rs:546-575`), lenient parse of only the final broken line, and
**mode 0600 on create and re-tightened on open** (126-147) — stricter than the
session files, which stay 0644 (`session/store.rs:670-680`).

What it is **not**: there is no thinking, no tokens, no cost, no model name, no
finish reason, no retry, no agent/model span, and no nesting — only a flat `seq`
and a tool call/result pair joined by `id` (subset confirmed by the engine sweep,
see §2.3).

Locked into it today:

- the engine writes exactly **five** kinds — `UserMessage` (`runtime.rs:2422`),
  `Compaction` (`2468`), `TurnEnd` (`2582`), `ToolCall` (`tool_loop.rs:207`),
  `ToolResult` (`tool_loop.rs:359`). `AssistantMessage` and `GepaReview` are
  declared but have no production writer (only tests and the recap display);
- the single `ToolCall` site masks arguments first (`mask_args` at `tool_loop.rs:578`,
  called from the single `ToolCall` recording site at `tool_loop.rs:202-211`) because `titi-core` sits **below**
  `titi-memory` and cannot call the redactor itself (comment at
  `trajectory.rs:171-176`; crate deps: `titi-memory/Cargo.toml` depends on
  `titi-tools`, `titi-core/Cargo.toml` depends on neither);
- an existing reader/renderer: `titi_cli::recap::build` opens the recorder and
  joins calls to results (`crates/titi-cli/src/recap.rs:16-35, 91-146`), wired
  to `/recap` (`chat.rs:2257, 2727-2744`);
- `recap.rs:206-214` matches `EventKind` **exhaustively** — adding a variant to
  `EventKind` would break it (and the round-trip expectations).

### 2.2 `titi-core` session store

`Entry { id, parent_id, role, content, ts, tool_calls }`
(`session/entry.rs:27-37`); roles `User/Assistant/System/Tool` (`10-21`). Tool
calls live on the assistant entry's `tool_calls: Vec<ToolCallRef>`; tool results
are a `Role::Tool` entry whose content is the **already-masked** output, with no
call id, no `ok` and no duration (`session_log.rs:42-69`). Thinking is
**absent** — no field, no variant, zero `thinking|reasoning` matches in
`titi-core` source. Layout is flat: `<agent_dir>/sessions/<id>.jsonl`,
`<id>.leaf`, `<id>.checkpoints.jsonl`, plus a shared SQLite/FTS5 catalog at
`<agent_dir>/state.db` (`session/store.rs:48-53, 444-454`; `session/index.rs:37-59`).

### 2.3 `EngineEvent` — what the UI sees

`crates/titi-engine/src/protocol.rs:196-341`, `#[non_exhaustive]`, pinned by
`crates/titi-engine/tests/protocol.rs`. Emission sites and their limits:

| Event | Site | Carries | Missing for a span |
| --- | --- | --- | --- |
| `TurnStarted` | `runtime.rs:2414-2419` | `turn_id`, model *resolver key* | emitted once **per model attempt**, so it is not a per-turn start; no wire model, no timestamp |
| `StreamDelta` | `runtime.rs:2817-2820` | text | — |
| `ThinkingDelta` | `runtime.rs:2822-2825` | text | **dropped**: no buffer, not in `answer`, not in history (see §3) |
| `ToolStarted` | `tool_loop.rs:189-197` | `call_id`, `name`, masked `detail` | `detail` is a human line, not the args; no timestamp |
| `ToolApprovalNeeded` | `tool_loop.rs:639-645` | ids only | — |
| `ToolFinished` | `tool_loop.rs:365-372` | output (masked+capped), `is_error` | **no duration** |
| `TurnUsage` | built `runtime.rs:2709-2722`, sent `2847`/`2727` | tokens, cached, `cost_micro_usd` | **aggregate over all rounds and models**, never per round; no timestamp |
| `ContextUsage` | `runtime.rs:2482-2488` | estimate + window | once per round — a usable round marker |
| `TurnFinished` | `runtime.rs:2848-2850` | `reason` | final round only |
| `Failed` | `runtime.rs:1780, 2387, 2528, 2602, 2893` | `reason`, `message` | no per-span error |
| `Cancelled` | `runtime.rs:1284` | `turn_id` | — |
| `ModelSwitched` | `runtime.rs:1317-1321` (manual), `2375-2379` (fallback) | from/to | fallback only; a transient retry emits **nothing** |
| `AgentStarted` | `agents.rs:301-305` | `agent_id`, `name`, `parent_id`, `kind` | no timestamp, no model |
| `AgentProgress`/`AgentActivity` | `agents.rs:70-92, 418-420`; `tool_agent.rs:200-204` | text | — |
| `AgentFinished` | `agents.rs:286-368` | `summary`, `success` | no duration, no tokens/cost |

**No `EngineEvent` carries a wall-clock timestamp or a duration.** The only
engine `Instant`s are the tool call's (`tool_loop.rs:228` start, `361` elapsed)
and the retry backoff (`runtime.rs:2635-2640`). Per **LLM-call** latency does not
exist anywhere today.

**Turn loop shape** (`runtime.rs:2369-2618`):
`for model in models` (fallback ladder) → `loop { ContextUsage; for attempt in
0..=max_transient_retries { stream_attempt } ; execute_tools }`. One
`stream_attempt` = one logical model call = the Laminar "LLM span".

### 2.4 Retries and fallback

`runtime.rs:2491` `for attempt in 0..=config.max_transient_retries`;
`is_retryable` at `2571`; a retry only happens when `!visible_output &&
error.is_retryable()`; backoff `runtime.rs:2624-2640` (2 retries default, 500 ms
doubling to 8 s, slept in 50 ms slices so cancel is honored). **Retries are
silent**: no event, and `TurnStarted` carries no attempt index. This is exactly
the OTel rule "one span covers the logical operation with all retries" — the
data to satisfy it (attempt count, error.type, total duration) is in the loop's
scope and simply not recorded.

### 2.5 Thinking today

`StreamEvent::ThinkingDelta` (`titi-providers/src/stream.rs:122`) is emitted by
the engine (`runtime.rs:2822-2825`) and dropped. On the CLI side it lands in
`Chat.thinking`, a plain in-memory `String`, only while the answer is still
empty (`chat.rs:622, 1469-1473`), rendered as the tail **100 chars**
(`chat.rs:4031-4044`), and cleared on the first answer delta and at turn end
(`chat.rs:1461, 1457, 3978, 4046-4057`). It is never written to the session file
or the trajectory. `TokenUsage` (`stream.rs:82-100`) has
`prompt/completion/cached` — **no `reasoning_tokens`** — so a reasoning token
count is not parsed from any provider today even where the wire carries it.

### 2.6 Subagents

Two disjoint paths:

1. **Supervisor** (`agents.rs`) — used by the `agent` tool and `SpawnAgent`.
   `spawn` mints `agent-{n}` from one supervisor counter and hardcodes
   `parent_id = Some("Main")` (`agents.rs:172-182`); the spawning agent's real
   id is never threaded through. `launch` emits `AgentStarted`, then
   `AgentStatusChanged`/`AgentFinished` on completion (`agents.rs:286-368`).
   `AgentRequest` is `{ id, name, task, kind, parent_id }` — no model, no tools
   (`agents.rs:18-24`); model/tools/approval live privately on the runner
   (`tool_agent.rs:40-51`). No timestamps; `AgentRecord` has no children index
   and no usage (`agents.rs:117-125`).
2. **Detached** — goal, council, orchestrator and review build an `AgentRequest`
   directly and call the runner with `AgentContext::detached()`
   (`orchestrator.rs:116-123`, `council.rs:142-149`, `goal.rs:857-864`,
   `review.rs:137-146`). These emit **no agent events at all** and have
   `parent_id = None`.

The subagent's own tool calls never reach anyone: `ToolAgentRunner` calls
`execute_tools` on a channel whose receiver is **dropped** (`tool_agent.rs:141-142`)
and a fake `TurnId(0)` (`tool_agent.rs:210-211`); the only trace is an
`AgentActivity` string. Neither runner reads `StreamEvent::Usage` or computes
cost (`agents.rs:417-425`, `tool_agent.rs:173-183`), so per-agent tokens and
cost are entirely unknown. Nesting is bounded structurally — the subagent's
registry is built **without** the `agent` tool (`runtime.rs:1184-1194`), so no
grandchildren.

### 2.7 Headless

`headless.rs:255-260` serializes each `EngineEvent` with
`serde_json::to_string(&event)` — **one JSON line per event, verbatim, with no
wrapper and no added timestamp**; only the variant's own fields appear
(pinned by `titi-engine/tests/protocol.rs:233-246`). So a trace-span stream
would either be new `EngineEvent` variants (serialized for free) or a separate
file the surface reads.

---

## 3. Thinking analysis — where it could go

Ranked by value / cost. The raw material (persisting the text) is a prerequisite
for all of them; per the ask it belongs to Phase A because the Phase A viewer
folds thinking under its LLM span. The *analysis* is Phase B.

| # | Option | Value | Cost | Notes |
| --- | --- | --- | --- | --- |
| 1 | **Persist thinking per LLM span** (masked, capped) | high — currently thrown away | S/M — new sink field + a buffer in `stream_attempt` | prerequisite for 2–5 |
| 2 | **Per-round thinking metrics** — chars ≈ tokens, time from first `ThinkingDelta` to first `StreamDelta`, provider `reasoning.output_tokens` | high | S — derived at capture, no extra storage | enables "which round thought most", thinking/answer ratio |
| 3 | **Fold thinking in the trace view** under its LLM span | high (human) | S — viewer only | part of Phase A viewer |
| 4 | **Search thinking across a session** (`titi trace --search`, `/trace search`) | medium | M — FTS5 already exists (`session/index.rs:37-59`); add a spans index or grep the trace file | cheap if the trace file is JSONL |
| 5 | **Model-run summary of a trace** (`titi trace --analyze`): feed thinking + outcomes to a model, print a critique | medium | L — tokens per analysis, a new engine consult path | gate on the owner; the ask's "if needed" |

The cheapest honest win is 2+3: counts and timing cost almost nothing and make
the trace view useful immediately; 1 makes 3 possible; 4 and 5 are follow-ons.

---

## 4. Privacy and cost

- **Secrets.** Two existing layers to reuse: `SensitivePolicy` (path/name based,
  `titi-tools/src/sensitive.rs:63`) and the content masker
  (`titi-memory/src/redact.rs:29, 57, 67`, applied at `tool_loop.rs:350-351,
  563-569` for output and `tool_loop.rs:585-601` for args). **Thinking text
  must go through the same `redact` before it is persisted** — a model can echo
  a key it just read. titi-engine already depends on titi-memory
  (`titi-engine/Cargo.toml:19`), so the engine can do it; `titi-core` cannot
  (layering), which is why the trajectory keeps the "caller masks" contract
  (`trajectory.rs:171-176`) — the same split applies to spans.
- **Caps.** Tool output is already capped to 40 000 chars
  (`tool_loop.rs:537 MAX_TOOL_OUTPUT`, head 30 000 + tail 10 000); `read`
  30 000 (`tools/fs.rs:300`), bash 64 KiB (`tools/pty.rs:34`), subagent answer
  30 000 (`agent_tool.rs:51`). Thinking is capped at **`THINKING_CAP_CHARS` =
  8 KiB**, kept as its head and its tail with a `… [N characters left out] …`
  marker — the tool cap's shape at a size a trace can carry per round, counted
  in characters and enforced in `Span::with_thinking`
  (`crates/titi-core/src/trace.rs`), so a caller cannot store an unbounded
  block.
- **Retention.** **Decided: bounded.** `MAX_TRACE_FILES` = 500 and
  `MAX_TRACE_AGE_DAYS` = 30 are constants (no settings key — nothing else reads
  retention yet); `trace::prune` runs once per session start (`main.rs`, before
  the engine starts), and deleting a session deletes its traces and its
  trajectory. That last part also closed the old leak where `delete_session_from`
  removed only the transcript.
- **Location and permissions.** `~/.titi/agent/traces/<session_id>/<turn>.jsonl`
  — a directory per session, a file per turn — never the repo. The trajectory's
  posture: 0600 on create and re-tightened on open. Agent dir via
  `titi_config::agent_dir()` (`titi-config/src/lib.rs:21-31`),
  `TITI_AGENT_DIR`-overridable.
- **Default.** The trajectory is already on by default and local-only. Spans can
  follow the same posture (local, redacted, 0600). Thinking **full text** is the
  sensitive part: either default-on (redacted+capped, honest to the ask) or
  opt-in (OTel's own default). Owner decision (§9).

---

## 5. Export — is OTLP / Laminar worth it?

Laminar ingests OTLP (`lmnr/README.md`: *"OpenTelemetry-native"*, a gRPC
exporter, one line of SDK init). A titi → Laminar export would mean one HTTP
`POST /v1/traces` with `Authorization: Bearer <project key>`.

Two ways:

- **Hand-rolled OTLP/HTTP JSON** over the existing `reqwest` (already in
  `titi-providers`) — no new crate. The payload is a plain JSON tree:
  `{"resourceSpans":[{"resource":{"attributes":[…]}, "scopeSpans":[{"scope":{…},
  "spans":[{"traceId","spanId","parentSpanId","name","kind",
  "startTimeUnixNano","endTimeUnixNano","attributes":[…],"status":{…}}]}]}]}`
  with 16-byte hex trace ids and 8-byte hex span ids. Because the titi span
  model would already use `gen_ai.*` attribute names (§8), this is a mechanical
  mapping.
- **`opentelemetry-proto` / `prost`** for the protobuf encoding — a **new
  crate**, which AGENTS.md gates behind the `dependency-update` skill and the
  owner's word.

Verdict: **worth it only if the owner wants remote/team traces, search, evals
and dashboards** — that is Laminar's real value over a local `titi trace`. It is
Phase C, opt-in (a new egress: prompts and thinking would leave the machine),
and the hand-rolled JSON path keeps it dependency-free. Privacy work (§4) must
land before any export.

---

## 6. Design — a titi span model in the Laminar shape

**Storage decision: a new per-session JSONL of spans, not the trajectory file.**

- **Rejected: extend `EventKind`.** `recap.rs:206-214` matches it exhaustively,
  so every new variant ripples into recap and its tests; and spans are a
  different shape (nested ids + start/end) than the flat, GEPA-digest-oriented
  trajectory. Mixing them would complicate both.
- **Rejected: the session store.** Entries are replayed into the model's
  history; spans must never enter it.
- **Chosen: `<agent_dir>/traces/<session_id>/<turn>.jsonl`** — one line = **one
  finished span**, one file per turn inside a directory per session (so pruning
  a session's traces is one `remove_dir_all`). Writing a span when it *ends*
  (not open/close pairs) means no pairing state to corrupt, and a crash tears
  at most the last line — the trajectory's exact machinery applies
  (`truncate_torn_tail`, 0600 on create and on every open, a buffered writer
  that flushes on drop). Ordering comes from `start_ms`/`end_ms` and the parent
  ids, not from file position, so a child finished before its parent still
  nests. The reader tolerates a child whose parent is absent (a partially
  flushed file still renders; a missing parent renders as a root). The flat
  trajectory stays untouched for GEPA and `/recap`.

**Span kinds** (titi → Laminar → OTel `gen_ai.operation.name`):

| titi kind | Laminar `SpanType` | OTel op | Span name |
| --- | --- | --- | --- |
| `Turn` | `DEFAULT` | `invoke_agent` | `invoke_agent titi turn {n}` |
| `Agent` | `DEFAULT` | `invoke_agent` | `invoke_agent {name}` |
| `Llm` | `LLM` | `chat` | `chat {wire_model}` |
| `Tool` | `TOOL` | `execute_tool` | `execute_tool {name}` |
| `Event` | `EVENT` | — | `retry` / `compaction` / `error` |
| `Compaction` | `DEFAULT` | `plan`? | `compact` |

`Turn` = one trace root; `trace_id` = session id, so a titi **session groups
turns** exactly as a Laminar session groups traces.

**Span record** (typed core + an open attribute map so Phase C is mechanical):

```
Span {
  trace_id: String,          // session id
  span_id: String,           // unique within the session
  parent_span_id: Option<String>,
  kind: SpanKind,            // Turn | Llm | Tool | Agent | Event
  name: String,
  start_ms: u64, end_ms: u64,
  status: SpanStatus,        // Ok | Error | Cancelled
  error: Option<String>,
  attributes: BTreeMap<String, Value>,  // gen_ai.* + titi.* (below)
  input_tokens: u64, output_tokens: u64,
  cached_tokens: u64, reasoning_tokens: u64,
  cost_micro_usd: Option<u64>,
  thinking: Option<String>,  // masked, capped at THINKING_CAP_CHARS; Llm only
}
```

**Attributes** (names chosen to export verbatim):

- LLM: `gen_ai.operation.name=chat`, `gen_ai.provider.name`,
  `gen_ai.request.model` (wire model), `gen_ai.usage.input_tokens` /
  `output_tokens` / `cache_read.input_tokens`,
  `gen_ai.usage.reasoning.output_tokens` (once parsed),
  `gen_ai.response.finish_reasons`, plus `titi.attempt` (retries inside the
  span, per §1.5), `titi.cost_micro_usd`, `titi.thinking_ms`,
  `titi.thinking_chars`.
- Tool: `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.tool.call.arguments`
  (masked), `gen_ai.tool.call.result` (masked, capped), `error.type`,
  `titi.duration_ms`.
- Agent/Turn: `gen_ai.agent.id`, `gen_ai.agent.name`, `gen_ai.conversation.id`
  (session id), `titi.phase`.

**Where the data comes from** (all already in scope at the real sites):

- LLM span: wrap `stream_attempt` (`runtime.rs:2772-2865`) — start/end `Instant`,
  `resolved.wire_model` (`runtime.rs:2368`), the round's own usage from
  `meter.charge` (`runtime.rs:2832-2844`), `StopReason` from `Done`, the attempt
  index and retry count from the loop at `2491`, and a `ThinkingDelta` buffer
  (new, §2.5/§3).
- Tool span: `Prepared.started` + `elapsed()` already exist
  (`tool_loop.rs:228, 361`); args and masked result are in the same function.
- Agent span: `agents.rs:286-368` (start/end `Instant` on launch/finish), the
  real parent id threaded through `spawn` (`agents.rs:172-182`), and the
  subagent's tool sink reattached (`tool_agent.rs:141-142`) so its tool spans
  nest under the agent span.
- Turn span: `runtime.rs:2369` (start) → `2849`/`2617` (finish).

**No new protocol fields in Phase A.** The viewer reads the trace file (like
`/recap` reads the trajectory), so `EngineEvent` and
`crates/titi-engine/tests/protocol.rs` are untouched. Live push (an
`EngineEvent::Span`) is a deliberate later option if the panel must update
mid-turn.

---

## 7. Phase A — minimal, no new dependency

Goal: every turn writes a span tree to
`<agent_dir>/traces/<session_id>/<turn>.jsonl`, and both `titi trace [session]`
and `/trace` render it.

| # | Change | Files | Notes |
| --- | --- | --- | --- |
| A1 | `Span`/`SpanKind`/`SpanStatus` types + `TraceWriter` + reader + tree + retention (done, `feb9e3c`/`1f6d718`) | **new** `crates/titi-core/src/trace.rs`; `crates/titi-core/src/lib.rs` (add `pub mod trace;` + re-exports, mirroring `:16`) | reuses the trajectory's torn-tail/0600/flush patterns rather than extracting them (the third repetition would be the trigger) |
| A2 | A `SpanSink` threaded through the engine; the recorder opened beside the trajectory (done, `a6e4955`) | **new** `crates/titi-engine/src/spans.rs`; `crates/titi-engine/src/runtime.rs`, `lib.rs`; `crates/titi-cli/src/engine.rs:1036` | same shape as the trajectory, but a std `Mutex` — never held across an await, so a turn's future stays `Send` |
| A3 | LLM span: start/end, wire model, per-round usage, finish reason, attempt count, thinking (done, `a6e4955`) | `crates/titi-engine/src/runtime.rs` (`stream_attempt`'s `RoundReport`) | the thinking is measured from the *deltas*, not the collector's blocks — only some families send a block, so a delta is the only thing every reasoning model gives |
| A4 | Tool span: `Prepared.started` + masked args + masked, capped result (done, `a6e4955`) | `crates/titi-engine/src/tool_loop.rs` (`finish`) | the parent is passed in — the round's span id — rather than read from a shared stack, because a read group runs concurrently |
| A5 | Turn + Agent spans; the subagent's tool sink reattached (done, `a6e4955`) | `crates/titi-engine/src/runtime.rs`, `crates/titi-engine/src/tool_agent.rs` | the agent hangs from the turn's frame (not from the tool call that spawned it — that needs a per-call context the tool trait does not carry); detached goal/council/orchestrator runs still have no span graph |
| A6 | `titi trace [session]` CLI tree (dur/tokens/cost/error, thinking folded) | **new** `crates/titi-cli/src/trace_cmd.rs`; `crates/titi-cli/src/main.rs` (short-circuit at `:81`, mirroring `genome_cmd`); resolve id via `session_fs.rs:62, 119` | bare = newest session |
| A7 | `/trace` panel: reuse the `/tree` machinery | `crates/titi-cli/src/chat.rs:2245` (match arm), `crates/titi-cli/src/pickers.rs` (`COMMANDS` ~`:129`, `panel_view_for` `:1107`, a `trace_panel` beside `tree_panel` `:1080`, `lay_out` `:969`) | re-render on open; no live push |

**Size: M–L** (A3–A5 are the bulk). **Tests that prove it:**

- titi-core: serde round-trip of a `SpanRecord`; a child written before its
  parent renders nested; torn-tail repair drops only the last line; 0600 on
  create; `seq`/`ts` monotonic across a reopen.
- titi-engine (integration, fake provider, `tests/`): after one turn the trace
  file holds a `Turn` root, an `Llm` child with model + tokens + finish reason,
  a `Tool` child with name/args/result/duration; a forced transient retry
  yields one `Llm` span with `titi.attempt = 2` (not two spans); a subagent test
  asserts the `Agent` span's non-`Main` parent and its nested tool span.
- titi-cli: `titi trace <id>` against a fixture trace file renders the expected
  tree; a `panel_view_for` test for `/trace` (mirroring the `/tree` tests).
- protocol: **unchanged** — assert that no `EngineEvent` changed by running the
  existing `tests/protocol.rs`.

**Risks:** the hot files (`runtime.rs`, `tool_loop.rs`, `protocol.rs`,
`agents.rs`) are under concurrent edits right now (siblings `taimyr-p4`,
`chat-split`) — sequence A after they land; thinking capture adds a buffer per
round (bounded by the cap); a new file means a new retention question (§9).

---

## 8. Phase B — thinking analysis

| # | Change | Files | Size |
| --- | --- | --- | --- |
| B1 | `thinking_chars`, `thinking_ms` (first `ThinkingDelta` → first `StreamDelta`) on the LLM span; parse `reasoning.output_tokens` where the wire carries it (add the field to `TokenUsage`, `titi-providers/src/stream.rs:82-100`) | `crates/titi-engine/src/runtime.rs`; `crates/titi-providers/src/stream.rs`; provider wire files | S |
| B2 | Search thinking across a session: an FTS index over spans, or `titi trace --search <pat>` | `crates/titi-core/src/session/index.rs` (FTS5 already there) or `trace.rs`; `crates/titi-cli/src/trace_cmd.rs` | M |
| B3 | Thinking folded + navigable in the `/trace` panel (mostly Phase A's rendering) | `crates/titi-cli/src/pickers.rs` | S |
| B4 | `titi trace --analyze` / `/trace analyze`: run the trace's thinking + outcomes past a model and print a critique | `crates/titi-cli/src/trace_cmd.rs`; a consult path beside `advisor.rs` | L, gated |

Ranked value/cost puts B1+B3 first (nearly free), B2 next, B4 only if the owner
wants it. **Owner decides** whether "analyze the thinking" means B1–B3 (local
metrics + view + search) or includes B4 (a model run, tokens spent per trace).

---

## 9. Phase C — optional OTLP / Laminar export (gated)

| # | Change | Files | Size |
| --- | --- | --- | --- |
| C1 | Map `SpanRecord` → OTLP/HTTP JSON spans (hex ids, `gen_ai.*` attributes verbatim) and POST over the existing `reqwest` | **new** `crates/titi-core/src/trace_export.rs` or `crates/titi-cli/src/otlp.rs` | M |
| C2 | Config: `observability.otlp.endpoint`, `.headers`, `.enabled`, opt-in; a redaction pass before send | `crates/titi-config/**`, settings guard (`titi-tools/src/settings.rs`) | M |
| C3 | If the owner prefers protobuf: add `opentelemetry-proto`/`prost` via the `dependency-update` skill | `Cargo.toml`, `Cargo.lock` | S/M, **new crate** |

**Risks:** a new egress (prompts + thinking leave the machine) — must be opt-in,
redacted, and documented beside the other egresses in BRAIN's risk list; the
export must not block a turn (fire-and-forget, bounded queue).

---

## 10. Owner decisions (answered 2026-10-09)

1. **Thinking full text**: **opt-in** — `trace.thinking` (unset = off), read
   through `switch_on`; the metrics (chars, ms, reasoning tokens) are recorded
   either way, and the text, when on, is masked and capped.
2. **Retention**: **bounded** — the newest 500 files and nothing older than 30
   days, pruned at session start; deleting a session deletes its traces, and its
   trajectory now goes with it too (`ee4d2c2`).
3. **Trace granularity**: **one trace per turn**, grouped by session — the file
   is named for the session's own turn ordinal (not the engine's per-process
   turn counter, which a resumed session would restart at 1).
4. **`/trace` liveness**: **read the file on open** — no protocol change; the
   panel re-renders from the file each time it opens.
5. **Phase C**: **not now** — no exporter, local traces only. The attributes are
   already the OTel GenAI names, so an exporter stays a mapping.
6. **Phase B scope**: **local metrics, view and search first** (step 1, done);
   the model-run `--analyze` is later and opt-in.

## 11. Sources

- OpenTelemetry GenAI semantic conventions:
  `opentelemetry/semantic-conventions-genai` — `docs/gen-ai/gen-ai-spans.md`,
  `docs/gen-ai/client-inference.md`, `docs/gen-ai/gen-ai-agent-spans.md`,
  `docs/registry/attributes/gen-ai.md` (fetched 2026-10-09).
- Laminar: `lmnr-ai/lmnr` — `README.md`, `docs/internal/clickhouse-traces.md`,
  `docs/internal/frontend-trace-view.md`, `frontend/lib/traces/types.ts`,
  `app-server/src/traces/spans.rs`, `app-server/src/db/spans.rs` (fetched
  2026-10-09).
- titi: `crates/titi-core/src/{trajectory.rs,session/*}`, `crates/titi-engine/
  src/{protocol.rs,runtime.rs,tool_loop.rs,agents.rs,tool_agent.rs,agent_tool.rs}`,
  `crates/titi-cli/src/{chat.rs,pickers.rs,recap.rs,headless.rs,main.rs,engine.rs}`,
  `crates/titi-providers/src/stream.rs` at `27c06e5`.
