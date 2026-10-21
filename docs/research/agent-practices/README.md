# Coding-agent harness best practices (Oct 2026), ranked, with titi gap analysis

Method and limits:
- I read the cited primary sources and the titi code with `read`/`grep`. I ran no builds or tests, and I wrote no files. Empryo was read only through its public docs and `llms.txt`.
- Where I infer something, it is marked **[INFERENCE]**.
- Status is have / partial / missing, with `file:line` against the current tree.
- `docs/research/prompt-cache.md` is stale. Its 3.1–3.3 blockers are fixed in code:
  - `cache_control` is on the tools, the system block and the newest message (`wire.rs:386,472,604-618,928-931`).
  - The system prompt is static (`runtime.rs:1943`).
  - Tools are sorted (`titi-tools/src/lib.rs:201`).
  - The moving blocks ride on the newest user message (`runtime.rs:224`).
- I did not read Terminal-Bench leaderboard rows or SWE-bench Pro rows, so I make no claims about top-harness rankings.

## Summary: the ten changes worth most

1. The default system prompt has almost no operating guidance (#1).
   - Status 2026-10-21: have — versioned operating manual ahead of SOUL.md (`8002c57`, pointed at the project rules in `a89c020`).
2. A turn dies after 8 tool rounds, and a subagent after 6 (#2).
   - Status 2026-10-21: have — `agent.maxToolRounds` default 100 and `agent.subagentMaxToolRounds` default 40 (`adb9d27`), wrap-up at the cap and a repeat nudge (`95140d2`), subagent wrap-up (`98a9b1b`). No token or dollar budget yet.
3. Compaction never uses a model and keeps only the first line of ≤8 user prompts (#3).
   - Status 2026-10-21: partial — the session's model writes the summary (`71aea04`), compaction triggers on the provider's prompt-token count (`db0d792`), the digest lists files to re-read (`5fc6c11`) and sits behind the system prompt (`d42f9f7`). The summary prompt is not tuned on real traces.
4. Nothing checks an edit automatically (#4).
   - Status 2026-10-21: have — `verify.command` at the end of a turn that edited files (`851c2d1`, `26e5e05`, `/verify`), a syntax line on every edit (`57a22a6`), a trust hash for a repository's command (`db2a803`). No type or LSP diagnostics.
5. Reasoning effort is never sent on the wire (#5).
   - Status 2026-10-21: have — effort goes out on every wire family (`7ade005`), is a per-session setting (`5ed6389`) with `/effort` (`6179a75`).
6. Memory recall ignores the prompt (#6).
   - Status 2026-10-21: have — recalled notes are ranked by the prompt, not by use alone (`28dab1d`).
7. The bash/write/git tool descriptions are one-liners (#7).
   - Status 2026-10-21: have for the three named — `bash`, `write` and `git` say what they do and refuse (`20b6c7b`); the other tools keep their short descriptions.
8. No context editing for stale tool results (#8).
   - Status 2026-10-21: partial — old tool results are elided to one-line stubs before a fold (`2ea13de`); nothing clears them between compactions.
9. Every subagent and helper runs on the main model (#9).
   - Status 2026-10-21: have — a role router over `modelRoles` (`27d59cb`), subagents, `/review` and the `/goal` reviewer on their roles (`b59a5ac`), advisor, session namer and compaction summary routed (`f67c76a`), `/router` view (`622c413`), map entries with model and effort (`1a298ce`).
10. No eval harness: titi has no SWE-bench, Terminal-Bench or Aider run, so none of the above can be measured (#10).
   - Status 2026-10-21: partial — `scripts/eval.py` with five starter tasks and hidden grading tests (`4d42a61`, `934baf2`, `7670dad`, [evals README](../evals/README.md)). Not yet run against a real model, so there is no baseline.

---

## A. Highest impact (task success and cost)

### 1. Operating guidance in the system prompt
- **Practice:** The prompt carries a short, right-altitude workflow: explore, then edit, then verify. It also says to persist to the end, batch parallel reads, and follow repo conventions.
- **Evidence:**
  - Anthropic's SWE-bench agent was "a prompt, a bash tool and an edit tool", and tool-description tuning produced the SOTA ([swe-bench-sonnet](https://www.anthropic.com/engineering/swe-bench-sonnet)).
  - OpenAI ships a full starter prompt with autonomy/persistence, exploration, tool-use and plan-closure sections ([Codex prompting guide](https://developers.openai.com/cookbook/examples/gpt-5/codex_prompting_guide.md)).
  - [Context engineering](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents) says "right altitude, minimal but not short".
- **titi:** missing. `DEFAULT_IDENTITY` is about 100 words of persona (`titi-soul/src/soul.rs:23-39`). `system_prompt()` adds `AGENTS.md`, the skill list and a plan/duck brief (`runtime.rs:1943-1968`). I found no verify, persist or parallel-read guidance.
- **Do:**
  - Add a versioned built-in "operating manual" block of about 600–900 tokens ahead of `SOUL.md`. It stays static, so it is cache-safe.
  - Base it on the Codex starter prompt, trimmed.
  - Do not add upfront-plan or preamble instructions. OpenAI warns those make GPT models stop early.

### 2. Tool-round cap
- **Practice:** Run until done, with step and cost budgets and a graceful stop.
- **Evidence:** mini-swe-agent runs a linear loop to submission and scores >74% on SWE-bench Verified with bash only ([docs](https://mini-swe-agent.com/latest/)). Anthropic's agent samples "until the model decides it is finished or exceeds 200k context". Anthropic's harness post found agents fail by stopping early or declaring victory early, not by running too long.
- **titi:** partial, and harmful. `max_tool_rounds: 8` (`runtime.rs:737`) is never overridden outside tests, and exceeding it emits `Failed "tool round cap reached"` and ends the turn (`runtime.rs:2775-2785`). Subagents get 6 (`tool_agent.rs:39`).
- **titi, 2026-10-21:** a subagent at its cap (`agent.subagentMaxToolRounds`, default 40; `maxTurns` in a definition lowers it) now gets the same wrap-up request as the main turn and returns its summary marked `(stopped at the tool budget)` instead of an error.
- **Do:**
  - Raise the main default to about 100 and expose it as a setting.
  - At the cap, ask the model for a wrap-up message and keep the work, rather than failing the turn.
  - Add a repeated-call and no-progress detector. Codex's prompt says to stop if it is re-reading or re-editing without progress.
  - Add a token or dollar budget. The meter already exists (`registry.rs` cost).

### 3. Compaction quality
- **Practice:** Summarise history with a model. Preserve decisions, open bugs, files touched and commands run; tune the prompt on real traces, maximising recall first. Keep the most recent files.
- **Evidence:**
  - [Anthropic](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents) describes compaction as the first lever.
  - Codex ships first-class compaction ([guide](https://developers.openai.com/cookbook/examples/gpt-5/codex_prompting_guide.md)).
  - Empryo compacts with no model by default and offers an optional cheap `compact` model slot ([docs](https://empryo.com/docs/context/compaction.md)).
- **titi:** partial.
  - Trigger at 80% of the window using a len/4 estimate (`titi-core/src/compaction.rs:118-124,237`), called at `runtime.rs:2666`, with forced retry on a provider overflow error (`runtime.rs:2836`).
  - The production summarizer is model-free. `Remote`/`Soft` always return "needs a provider" (`compaction.rs:255-275`).
  - The digest lists the first line of at most 8 user prompts plus counts (`compaction.rs:282-306`).
  - It drops assistant conclusions, edited files, failing commands and decisions.
  - The digest is inserted as a `Role::System` message (`titi-engine/src/compaction.rs:70-77`).
- **Do:**
  - Implement the model-backed `Remote` strategy with a structured prompt: goal, decisions, files and symbols touched, commands and outcomes, unresolved errors, next step.
  - Route it to a cheap model (see #9) and keep the structural digest as the fallback.
  - Preserve the last N read file paths as a "re-read these" list.
  - Use the provider's token count, not len/4, as the trigger (usage is already parsed).
  - For OpenAI GPT-5.x, evaluate `/responses/compact`. Note it rewrites the prefix and misses the cache from the first changed token ([OpenAI caching](https://developers.openai.com/api/docs/guides/prompt-caching.md)).

### 4. Verification loop on edit
- **Practice:** Give the agent a check it can run, and make "done" depend on it. Layers: in-prompt check, goal evaluator, deterministic Stop hook, independent verifier.
- **Evidence:**
  - Claude Code best practices list the four layers ([source](https://code.claude.com/docs/en/best-practices.md)).
  - Anthropic's harness post saw agents mark features complete without e2e testing.
  - SWE-agent's ACI (lint-on-edit and other interface choices) gave +10.7 pp over a baseline agent on SWE-bench Lite ([paper](https://arxiv.org/html/2405.15793v3)). I did not isolate the per-feature numbers.
- **titi:** partial.
  - The `project` tool resolves test/lint/typecheck commands (`titi-tools/src/project.rs:784`).
  - `/goal` has command gates plus a fresh-context reviewer (`goal.rs:261,306,755`).
  - `/review` exists (`change_review.rs`).
  - In an ordinary turn nothing runs a check, and the prompt never says to (#1).
  - ast_edit's reparse guard (`BrokenResult`, `ast_edit.rs:253-263`) is not reachable from any tool.
  - Update (branch `wt/verify`): `write`/`edit`/`hashline_edit` now append a `syntax:` line when an edit changed the file's tree-sitter error count (`titi_genome::syntax_check`), and `verify.command` runs when a turn that edited files ends, feeding a failure back once. The reparse guard is still not a tool.
- **Do:**
  - Add a post-edit cheap syntax/diagnostic check to `edit`/`write` results, reported as "introduced N errors" (empryo-parity #18, still open).
  - Add an optional `verify.command` setting that runs when the model ends its turn, and feeds failures back once (a Stop-hook equivalent).
  - Add a prompt line: "run the project's checks before you say done".

### 5. Reasoning effort / thinking control
- **Practice:** Set effort per role and per task. Default to medium or high. Keep the setting constant within a conversation.
- **Evidence:**
  - Anthropic's adaptive thinking: `output_config.effort` is the primary lever. Changing effort mid-conversation invalidates the cache unless you use the per-message beta. Sonnet 5.5 does not reliably obey prompt-based "think less" ([doc](https://platform.claude.com/docs/en/build-with-claude/thinking-steering-and-cost.md)).
  - OpenAI recommends medium for interactive coding and high/xhigh for hard tasks; GPT-6 has `configuration_update` to change effort without breaking the cache ([Codex guide](https://developers.openai.com/cookbook/examples/gpt-5/codex_prompting_guide.md), [caching](https://developers.openai.com/api/docs/guides/prompt-caching.md)).
- **titi:** missing. `Effort`, `ThinkingFormat` and `resolve_compat` exist (`titi-providers/src/compat.rs:9-146`), but `resolve_compat`, `RequestOpts` and `clamp_effort` have no caller outside `compat.rs` and `lib.rs`. No request body carries `thinking`, `output_config`, `reasoning` or `thinkingConfig`. Council members carry an effort label (`council.rs:131`); I found no path that sends it.
- **Do:**
  - Wire `RequestOpts.effort` into the three wire builders (Anthropic `output_config.effort` plus adaptive thinking, OpenAI `reasoning.effort`, Gemini `thinkingConfig`).
  - Add a `/effort` command and a per-session default.
  - Per-role defaults: review/plan high, explore/compact low.
  - Never change it mid-session without telling the user the cache will miss.

### 6. Memory recall
- **Practice:** Retrieval should be conditioned on the task. Structured notes and a memory directory persist across sessions.
- **Evidence:**
  - Anthropic's memory tool plus context editing gave +39% on an internal agentic-search eval; editing alone gave +29%, with 84% fewer tokens in a 100-turn run ([announcement](https://www.anthropic.com/news/context-management)).
  - The harness post uses a progress file plus git log to bridge sessions ([source](https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents)).
- **titi:** partial.
  - FTS plus local embeddings, top 5 (`titi-memory/src/index.rs:27,214`).
  - The turn calls `index.recall("", &touched)` with an empty query (`runtime.rs:119-136`). Ranking uses only touched files and use count, never the user's prompt.
  - A `memory` tool writes notes (`titi-memory/src/tool.rs:60`).
- **Do:**
  - Pass the submitted prompt as the recall query, and recall again when the touched set shifts.
  - Keep the block on the user message (cache-safe, already so).
  - Add a prompt nudge: write a note on decisions and gotchas at task end.
  - Optionally add a per-project `NOTES.md`/progress file convention for long tasks.

### 7. Tool descriptions
- **Practice:** Tool specs are prompts. Describe like onboarding a new hire; say what the output looks like, what limits exist and what to do instead; make errors actionable.
- **Evidence:** [Writing tools for agents](https://www.anthropic.com/engineering/writing-tools-for-agents) says small description refinements gave dramatic gains. The Anthropic bash tool description is multi-line (no internet, background long jobs, avoid huge output). Anthropic's multi-agent post cut task time 40% by rewriting one description.
- **titi:** partial.
  - `read`, `glob`, `grep`, `edit`, `todo` and `ask` are good.
  - `bash` is "Run a shell command in the workspace" (`fs.rs:1140`), `write` is "Write a UTF-8 file in the workspace" (`fs.rs:479`), `git` is "Show git status or diff…" (`git.rs:83`), and `fetch` / `web_search` are one line (`web.rs:257,361`).
- **Do:**
  - Rewrite `bash`: timeout, auto-background threshold, no TTY, prefer dedicated tools, output cap and how to narrow.
  - Rewrite `write`: prefer `edit` for existing files.
  - Add an eval for tool-error rate per tool (the spans already record tool calls).

### 8. Stale tool-result clearing (context editing)
- **Practice:** Clear old tool results once their use is over; keep the call record.
- **Evidence:**
  - Anthropic calls it "one of the safest, lightest-touch forms of compaction", with the numbers in #6.
  - [The Complexity Trap](https://arxiv.org/abs/2508.21433): simple observation masking halves cost versus a raw agent and matches or slightly exceeds LLM summarisation on SWE-bench Verified; a hybrid saves a further 7–11%.
  - **Counter-evidence:** Empryo ported NVIDIA's pruning and measured +30% cost, because every mid-history edit re-bills the suffix. Only removals within about 7 blocks of the tail never broke the cache ([blog](https://empryo.com/blog/sol-pi-pruning-on-real-bugs.md)). That is a vendor self-benchmark on 3 bugs and one model, so weak evidence.
  - **Reconcile [INFERENCE]:** mask once, at compaction time, in a single rewrite, never per turn.
- **titi:** missing. Results are capped at 40k chars at arrival (`tool_loop.rs:601-620`), and old results are never altered.
- **Do:**
  - When #3 fires, first replace tool-result bodies older than the kept tail with one-line stubs (path, tool, size, "re-run to see").
  - Only then call the summariser.
  - Do not clear on every turn.

### 9. Model routing by role
- **Practice:** The best model for editing and planning; a cheap model for search, summaries, naming and reviews. Same-model helpers can reuse the cache.
- **Evidence:**
  - Empryo's task router has slots for explore, compact, review, advisor and others ([docs](https://empryo.com/docs/recipes/task-router.md)). It notes a cheaper helper loses the shared cache, so measure.
  - Claude Code: Sonnet for most work; `model: haiku` for simple subagents ([costs](https://code.claude.com/docs/en/costs.md)).
  - Anthropic's multi-agent system used Opus lead plus Sonnet subagents (+90.2% over single-agent Opus on its research eval).
  - Haiku 5.5 lists at $0.10/$0.50 per MTok and Sonnet 5.5 at $2/$10 ([pricing](https://platform.claude.com/docs/en/build-with-claude/prompt-caching.md)).
- **titi:** partial. One `agent_model` for all subagents (`titi-cli/src/engine.rs:922`); aliases via `modelRoles` (STATE.md); `judgment.rs` uses a small model for closed questions; advisor and council take their own models. There is no per-role slot for explore, compact or namer, and no effort per role.
- **titi, 2026-10-21:** a subagent definition (`.claude/agents/*.md`, `docs/research/extensibility-marketplace/design-2026-10.md` L5) can set `model` (a `modelRoles` alias or a catalog id) and `effort` per named specialist, so a cheap model for a reviewer or a scout is a file, not a code change; the model is chosen only from the user's own catalog. `taskRouter.*` is still not built.
- **titi, 2026-10-21 (built):** the slots live in `modelRoles` rather than a new `taskRouter` block, so `/switch @role` and the picker keep working: `subagent`, `explore` (a subagent whose tools can only read, falls back to `subagent`), `review` (`/review` and the `/goal` reviewer), `advisor`, `compact`, `namer` (then `smol`). An entry is a model id or `{model, effort}`; model and effort resolve along the chain separately; the effort goes through `resolve_reasoning` for the model it lands on. Unset is byte-for-byte today's request (pinned by tests); an unusable model falls back to the chat model with one notice per slot. `/router` prints the table. `crates/titi-engine/src/router.rs`. Not built: `judgment` and the `/council` seats are still on their own roles, and the A/B in #10 that should come before recommending a cheaper default.
- **Do:** Add a `taskRouter.{explore,compact,review,namer}` settings layer defaulting to the chat model. Ship an A/B (#10) before recommending a cheaper default.

### 10. Evals: measure titi's own harness
- **Practice:** Start with about 20 real tasks, grade the end state, and report variance, tokens and cost together with pass rate.
- **Evidence:**
  - [Anthropic](https://www.anthropic.com/engineering/multi-agent-research-system): start small; judge end-state for state-mutating agents.
  - [Infrastructure noise](https://www.anthropic.com/engineering/infrastructure-noise): resource config moved Terminal-Bench 2.0 by 6 pp; differences under 3 pp deserve skepticism; ~6% of tasks failed from pod errors.
  - [Scaffold Effect](https://arxiv.org/abs/2607.22585): harness choice changes tokens per solved task up to 40× while paired pass-rate gaps stay 0–8 pp, so always report cost.
  - Two blogs claim 22–29 pp swings on SWE-bench Pro from harness alone; I did not read them in full and treat them as unverified ([particula](https://particula.tech/blog/agent-scaffolding-beats-model-upgrades-swe-bench), [topclanker](https://topclanker.com/blog/2026-08-10-swe-bench-pro-harness-variance-code-agents)).
  - Terminal-Bench 2.0 is archived as saturated at Vals AI ([Vals](https://www.vals.ai/benchmarks/terminal-bench-2)).
- **titi:** missing. `scripts/bench.py` measures perf and prompt bytes ([perf README](docs/research/perf/README.md)). A grep for terminal-bench, swe-bench and polyglot over docs, scripts and crates found nothing. Trajectories and spans are captured (`titi-core/src/trajectory.rs`, `trace.rs`), and a headless JSONL surface exists (`titi-cli/src/headless.rs`).
- **Do:**
  - Build `titi eval`: N repo+task+test triples, N=3 runs each, reporting pass rate, tokens, cache hit, cost and tool-error rate, with a fixed container for resource limits.
  - Use it to gate every item below.
  - Add an adapter for a public benchmark (Terminal-Bench or SWE-bench-style) later.

---

## B. Context engineering and caching

### 11. Stable, append-only prefix
- **Practice:** Keep the prefix byte-stable, use deterministic serialization, and put volatile content last.
- **Evidence:** Manus calls KV-cache hit rate "the single most important metric", with a 10× cached versus uncached price gap and 100:1 input to output ([Manus](https://manus.im/blog/Context-Engineering-for-AI-Agents-Lessons-from-Building-Manus)). OpenAI says keep the prefix stable and move dynamic content later, because summarisation or truncation resets reuse ([OpenAI](https://developers.openai.com/api/docs/guides/prompt-caching.md)).
- **titi:** have. Static system prompt (`runtime.rs:1943`), volatile blocks on the newest user message with dedup against the previous copy (`runtime.rs:224-262`), one-element block arrays so message shape never changes (`wire.rs:390-393`), sorted tools (`lib.rs:201`).
- **Do:** keep the regression tests (`titi-engine/tests/loop.rs` has the byte-compare). Add the same test for the OpenAI and Gemini bodies.

### 12. Anthropic breakpoints and TTL
- **Practice:** Breakpoints on tools, system and the newest message. Auto caching is available via a top-level `cache_control`. TTL is 5 min by default; 1 h costs 2× on write. Reads are 0.1× (0.05× on Sonnet 5.5 / Opus 5.5).
- **Evidence:** [Prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching.md) and its pricing table. The 20-block look-back and 4-breakpoint limits are quoted in titi's own `prompt-cache.md`; I did not re-verify them in the current doc.
- **titi:** partial. Three breakpoints (`wire.rs:472,618,931`), but `ephemeral()` takes no TTL (`wire.rs:386`).
- **Do:** Add a `cache.ttl` setting (`5m`/`1h`). Default 1 h for interactive sessions where the user thinks for more than 5 min between turns; the write is 2× once and reads stay cheap. Re-check after #10.

### 13. OpenAI caching
- **Practice:** On GPT-5.6+, choose implicit or explicit mode and `ttl` (30 m at 1.25× write, 12 h at 2.5×); reads are 0.1× (0.05× on GPT-6.1 Sol). Before GPT-5.6, send a stable `prompt_cache_key` and stay under about 15 requests/min per key. Caching is automatic from 1,024 tokens.
- **Evidence:** [OpenAI](https://developers.openai.com/api/docs/guides/prompt-caching.md).
- **titi:** partial. Cached tokens are parsed (`openai.rs:113,127`). No `prompt_cache_key`, no `prompt_cache_options` (grep: no hits).
- **Do:** send `prompt_cache_key = session_id` for pre-5.6 models. For 5.6+ use explicit mode with a breakpoint after the static system and tools block and one on the newest message.

### 14. Gemini caching
- **Practice:** Implicit caching is on by default for Gemini 2.5+. Minimum prefix is 4,096 tokens on 3.x models and 2,048 on 2.5. Put large, common content first and send similar prefixes close together.
- **Evidence:** [Gemini caching](https://ai.google.dev/gemini-api/docs/caching.md). That page documents only implicit caching; an older page lists explicit `cachedContent` too, and I did not verify it.
- **titi:** partial. Cached tokens parsed (`gemini.rs:65`); nothing else is needed for implicit caching.
- **Do:** Ensure the static prefix exceeds 4,096 tokens once the operating manual (#1) lands. Skip explicit caching until the eval shows a gain.

### 15. Cache-miss diagnostics
- **Practice:** Report hit rate, misses with a likely cause, and expected rebuilds.
- **Evidence:** Claude Code prints "N requests · 91% from cache · 2 misses (likely cause: tool definitions changed) · 1 expected rebuild" ([costs](https://code.claude.com/docs/en/costs.md)). OpenAI offers a Prompt Cache Diagnostics tool.
- **titi:** partial. Per-turn cached tokens and a footer "cache miss" marker exist (omp-parity #1); `/usage` shows hit rate per model. No cause.
- **Do:** Hash the tools, system block and each history message per request, and on a miss name the first changed segment (empryo-parity #25).

### 16. Just-in-time retrieval plus a small upfront map
- **Practice:** Hybrid. Load only `AGENTS.md`-style rules up front, plus glob/grep for the rest; add a ranked map when it pays.
- **Evidence:**
  - [Anthropic](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents) describes Claude Code's hybrid.
  - Aider found a PageRank'd tree-sitter map lets the model "figure out which files it needs" ([Aider](https://aider.chat/2023/10/22/repomap.html), 1k-token default).
  - Cursor: semantic search added +12.5% accuracy, +2.6% retention on repos ≥1000 files ([Cursor](https://cursor.com/blog/semsearch)).
- **titi:** have, with the unproven part being value versus cost. About 1k tokens of genome (24 files by default, `runtime.rs:740`) rides on the user message, sent only when changed. 13 languages from syntax trees.
- **Do:** A/B the map on and off in #10. Keep the budget tied to repo size.

### 17. Semantic search tool
- **Evidence:** Cursor, #16. A May 2026 study finds grep beats vector retrieval in several harnesses, but it measures conversation memory (LongMemEval), not code ([arXiv 2605.15184](https://arxiv.org/abs/2605.15184)).
- **titi:** missing. `titi-memory/src/embed.rs` has local embeddings used for memory only.
- **Do:** **[INFERENCE]** reuse the embedder to index symbol-level chunks as an optional `find` tool. Gate on the eval, and expect the value only on large repos.

### 18. Tool-result budgets
- **Practice:** Cap each result, with pagination and an actionable truncation message. Claude Code caps responses at 25k tokens.
- **Evidence:** [Writing tools](https://www.anthropic.com/engineering/writing-tools-for-agents).
- **titi:** have. 40k-char head/tail cap with a "narrow it" note (`tool_loop.rs:601-620`); `read` 30k chars / 2,000 lines (`fs.rs:295-301`); grep 500 matches × 300 chars (`fs.rs:919,1435`); glob 1,000; pty 64 KiB (`pty.rs:34`).
- **Do:** Add a per-turn aggregate budget, so many 30k results cannot stack. Empryo's data suggests capping at arrival beats pruning later.

### 19. Keep failures in context; avoid pattern lock-in
- **Evidence:** Manus: leave failed actions and stack traces visible; add variation so the model does not mimic a rut ([Manus](https://manus.im/blog/Context-Engineering-for-AI-Agents-Lessons-from-Building-Manus)).
- **titi:** have for errors (they return as tool results). No variation work; skip unless the eval shows loops.

### 20. Recitation via todo
- **Evidence:** Manus todo.md recitation pushes the plan into recent attention; Codex requires plan closure before finishing.
- **titi:** have. `todo` "every answer shows the list" (`todo.rs:141`).
- **Do:** add the plan-closure rule to #1.

### 21. AGENTS.md hierarchy
- **Practice:** Inject root→leaf, one message per file, with a size cap; keep it short and prune it.
- **Evidence:** [Codex guide](https://developers.openai.com/cookbook/examples/gpt-5/codex_prompting_guide.md) (file enumeration, merge order); Claude Code: "would removing this line cause mistakes?" ([best practices](https://code.claude.com/docs/en/best-practices.md)).
- **titi:** partial. Walks from `cwd` up to the repo root plus the agent dir and home, 64 KiB per file, in the static system block (`project_context.rs:19,22,110`). The system block never changes mid-session, so a rule file in a subdirectory the agent first visits mid-session is not loaded **[INFERENCE]**.
- **Do:** On the first `read`/`edit` under a subdirectory with its own `AGENTS.md`, append the file's text to that tool result once. This keeps the prefix stable.

### 22. Skills with progressive disclosure
- **Evidence:** Three levels (name+description, `SKILL.md`, linked files); now an open standard ([Anthropic](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills)).
- **titi:** partial. Name and description list capped at 32 entries × 1 KiB in the system block (`skills.rs:19-22,82`), body by `/name` (32 KiB cap) or via `read`. Only `.titi/skills` and `<agent_dir>/skills` are scanned, not `.agents/skills` where this repo's own skills live.
- **Do:** scan `.agents/skills` as well; support third-level files. The skills *runtime* is gated by PLAN M6 (AGENTS.md).

---

## C. Tool design and edit formats
- **Status, 2026-10-21:** partial — skills v2 reads `.titi`, `.agents` and `.claude` roots with real YAML, and the model loads a body with the `skill` tool (`c48e704`, `cd41bff`).

### 23. Fewer, better tools
- **Evidence:**
  - Anthropic: "bloated tool sets" and overlap are the common failure; consolidate (`search_logs`, not `read_logs`).
  - mini-swe-agent: bash alone reaches >74%. SWE-agent's own authors call a lot of the 2024 interface work "not needed at all" for strong models.
- **titi:** have (6 core tools). It also has `git`, `git_commit`, `diagnose`, `project`, `todo`, `ask`, `settings`, `memory`, `fetch`, `web_search`, `agent` and a hashline `edit` variant, about 18 in all. Specs (names, descriptions and schemas) are estimated by `context_parts` (`runtime.rs:2046-2062`).
- **Do:** measure the tool-spec token cost and per-tool usage from spans. Prune or fold anything under about 2% of calls (`diagnose`, `git`, `git_commit` can be `bash`). Do not grow the list without an eval.

### 24. Edit formats
- **Evidence:**
  - Aider found SEARCH/REPLACE and udiff work well, and that putting code in JSON function calls was worse ([unified diffs](https://aider.chat/2023/12/21/unified-diffs.html)). That result is from 2023 on GPT-4 Turbo and is dated.
  - Line-number formats were "terrible". "High-level diff" prompting cut edit errors by 30–50%.
  - Format compliance on the polyglot run ranged from 71.6% (Qwen2.5-Coder-32B) to 100% (Sonnet 3.5), as the 2024 table shows ([polyglot](https://aider.chat/2024/12/21/polyglot.html)).
  - Anthropic's best result was exact `str_replace`, unique match, absolute paths ([source](https://www.anthropic.com/engineering/swe-bench-sonnet)).
  - OpenAI trains on its own `apply_patch` and says to use it exactly ([guide](https://developers.openai.com/cookbook/examples/gpt-5/codex_prompting_guide.md)).
- **titi:** have for str_replace. Exact match with a whitespace-tolerant fallback that never picks between two candidates (`loose.rs:1-11`), plus hash-anchored line edits (`hashline.rs:1-8`, registered at `fs.rs:1508`). `write` replaces whole files. No `apply_patch`.
- **Do:** Offer an `apply_patch`-compatible tool when the model family is OpenAI/Codex. Track edit-failure retries per model in the eval; use that, not the format's age, to pick per-model defaults.

### 25. AST/symbol edits and code intelligence
- **Evidence:** Empryo sells symbol edits ([docs](https://empryo.com/docs/tools/ast-edit.md)); Claude Code says LSP plugins replace grep+read chains and report type errors after edits ([costs](https://code.claude.com/docs/en/costs.md)). I found no independent measurement of either.
- **titi:** partial. `ast_edit` is "a library capability" (`titi-genome/src/ast_edit.rs:10-14`) for Rust, TS/JS and Python. `titi genome lsp` is an LSP *server* for symbols; there is no LSP *client* (diagnostics, definition, rename); compound refactors are gated (empryo-parity #22).
- **Do:** First expose a read-only `symbols`/`refs` tool from the genome the engine already holds (cheap, no LSP). Then an LSP client for post-edit diagnostics (#4). Make `ast_edit` a tool only after the eval shows edit failures worth fixing.

### 26. Parallel tool calls
- **Evidence:** Anthropic's research system cut time up to 90% with parallel subagents and parallel tool calls; Codex says "maximise parallelism" for reads.
- **titi:** have. Read-tier calls are grouped (`tool_loop.rs:489-531`, `READ_GROUP_LIMIT`), writes serial with per-file claims (`claims.rs`).
- **Do:** state it in the operating manual (#1) so models batch.

### 27. Deferred tool loading and code-mode orchestration
- **Evidence:**
  - Anthropic's Tool Search Tool cut context ~85% and raised MCP-eval accuracy (Opus 4 49→74%, Opus 4.5 79.5→88.1%); it does not break the cache because deferred tools are not in the initial prompt ([advanced tool use](https://www.anthropic.com/engineering/advanced-tool-use)).
  - Programmatic tool calling: −37% tokens.
  - Code execution with MCP: 150k→2k tokens, with caveats about sandboxing ([source](https://www.anthropic.com/engineering/code-execution-with-mcp)).
  - Manus: avoid adding or removing tools mid-session.
- **titi:** missing, and moot until MCP exists (gated, #36).
- **Do:** When MCP lands, defer all MCP tools by default behind one `tool_search` core tool; keep the built-ins always loaded.

### 28. Search-tool ergonomics
- **titi:** have. Own regex grep with parallel workers (`fs.rs:1341`), path:line:text output, sorted glob, `@file` mentions (empryo-parity #5).
- **Do:** add `-C` context lines and a files-only mode if missing (grep context is noted as landed, `GREP_LINE_CHARS`). Otherwise leave alone.

### 29. Structured, concise responses
- **Evidence:** Anthropic's `response_format: concise|detailed` cut a Slack result from 206 to 72 tokens; names beat opaque IDs.
- **titi:** partial. The `detail` field keeps diffs out of the model's tool message (`titi-tools/src/lib.rs:105-115`).
- **Do:** audit outputs of `project`, `git`, `diagnose` for IDs or boilerplate the model doesn't use.

---

## D. Planning and review

### 30. Plan mode
- **Evidence:** Claude Code: explore → plan → implement → commit; skip the plan when the diff fits one sentence. Codex: skip the plan tool for the easiest ~25%, no single-step plans.
- **titi:** have. Plan mode restricts to the read tier (`runtime.rs:76-117,1982`), duck mode, and the `todo` tool.
- **Do:** add the "skip for trivial tasks" rule to #1. Optionally let the plan be edited externally (Claude Code `Ctrl+G`).

### 31. Independent reviewer
- **Evidence:** Claude Code uses a fresh model to try to refute results; Anthropic GA'd Code Review on Oct 15, 2026 (title only; I did not read it).
- **titi:** have. `/review` and `/goal` use a fresh-context reviewer that never sees the conversation (`review.rs:1-6`, `change_review.rs:1-10`); council and advisor exist.
- **Do:** give the reviewer the diff plus the check output, not just file paths; route to a different model family if possible (#9).

### 32. Long-horizon harness
- **Evidence:** One feature at a time, a JSON feature list marked passes=false, a progress file, git commits, an `init.sh`, a start-of-session smoke test ([harness post](https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents)).
- **titi:** partial. `/goal` loops with gates (`goal.rs`), orchestrator graph with resume-from-failed-node (`orchestrator.rs`), checkpoints exist. No feature-list or progress-file convention.
- **Do:** let `/goal` write and read `.titi/goal/progress.md` and a JSON checklist, so a fresh context can resume after compaction.

### 33. Mid-flight steering
- **titi:** have. Queue drained at step boundaries (`steering.rs`). Matches Empryo's steering feature.

---

## E. Subagents and multi-agent

### 34. When subagents help
- **Evidence:** Multi-agent beat single-agent by 90.2% on breadth-first research but used ~15× the tokens; "most coding tasks involve fewer truly parallelizable tasks" ([Anthropic](https://www.anthropic.com/engineering/multi-agent-research-system)). Context isolation: subagents explore at length and return 1–2k tokens.
- **titi:** have. The `agent` tool says to use it for context-flooding work, not one-call answers (`agent_tool.rs:129-141`). Subagents are read-only by default.
- **Do:** keep the default. Count subagent tokens separately in `/usage`.

### 35. Delegation briefs and effort scaling
- **Evidence:** Each subagent needs an objective, output format, tool guidance and boundaries; embed rules like "simple fact: 1 agent, 3–10 calls". Early agents spawned 50 subagents for simple queries.
- **titi:** partial. The `task` parameter says "name the files, the goal and what to report back"; batch limit 4; no scaling rules or return-size cap.
- **Do:** add a short "delegation" paragraph to #1 with those rules, and cap subagent reports (about 1–2k tokens), spilling larger output to a file path (Anthropic's "output to a filesystem" pattern).

### 36. Write contention
- **titi:** have. Per-file exclusive claims (`claims.rs`), findings log (`findings.rs`).

---

## F. Permissions, sandbox, hooks, MCP

### 37. OS-level sandbox
- **Evidence:** Anthropic's Seatbelt/bubblewrap sandbox with filesystem and network isolation cut permission prompts 84%; both isolations are needed; open source ([source](https://www.anthropic.com/engineering/claude-code-sandboxing)). Cursor published an agent-sandboxing post (Feb 2026), which I did not read. mini-swe-agent supports bubblewrap.
- **titi:** missing (grep for sandbox/seatbelt/landlock/bubblewrap: no hits). `bash` runs `sh -c` with `current_dir(root)` and no path jail (ARCHITECTURE.md). Secret-path policy, `.git` write refusal (`fs.rs:69-93`) and output redaction exist.
- **Do:** wrap `bash` (and the pty path) in `sandbox-exec` on macOS and bubblewrap on Linux. Allow writes to the workspace plus tmp, and network through an allowlist. Then default `bash` to auto-approve inside the sandbox. Run the dependency checklist (`dependency-update`) before adopting Anthropic's `sandbox-runtime`.

### 38. Permission rules and an auto-mode
- **Evidence:** Claude Code: allowlists via `/permissions`; a classifier "auto mode" that blocks only risky actions (default on from v2.1.283) ([best practices](https://code.claude.com/docs/en/best-practices.md)). Approval fatigue is called out.
- **titi:** partial. Four tiers and three modes (`titi-tools/src/lib.rs:50-100`); an unknown tool is treated as exec. No per-command or per-path rules and no remembered approvals.
- **Do:** add `permissions.allow: ["bash(cargo test*)", "edit(src/**)"]` evaluated in `execute_tools` before the approval prompt. A small-model classifier is optional and comes after the sandbox.

### 39. Hooks
- **Evidence:** Deterministic gates versus advisory prompts; PreToolUse/Stop; Empryo reads Claude Code hooks too ([docs](https://empryo.com/docs/tools/hooks.md)).
- **titi:** missing, gated by PLAN M6.
- **Do:** when M6 opens, ship three events first: PreToolUse (block), PostToolUse (add context), Stop (block until check passes). That gives #4's Stop-hook for free.
- **Status, 2026-10-21:** have — Claude-compatible command hooks around a turn (`509d9e5`), consent panel for a repository's hooks (`1484908`), allow-listed hook environment (`7b7ca44`).

### 40. MCP
- **titi:** missing, gated by PLAN M6. Design with #27 deferral and `readOnlyHint`-style annotations mapped to approval tiers.
- **Status, 2026-10-21:** have for tools — stdio client and manager (`2dc8e69`, `e61952a`), `/mcp` and the consent panel (`bc5a197`), Streamable HTTP (`331a3cb`), one-read status and registry (`3a557aa`). No OAuth, no resources or prompts, no sampling.

### 41. Prompt-injection hygiene
- **titi:** have. The soul scan, the genome path sanitiser (paths containing `<`/`>` dropped) and secret redaction cover memory, genome and tool output. Project rules and skills pass an injection scanner (STATE.md:164).
- **Do:** also tag `fetch`/`web_search` output as untrusted data in the tool result.

---

## G. Reliability, surfaces, telemetry

### 42. Provider retry and fallback
- **Evidence:** Anthropic: let the agent know when a tool fails; resume rather than restart; checkpoints.
- **titi:** have. Stream retry (tested for thinking replay, `loop.rs:903+`), fallback transport, forced compaction on overflow, session resume.

### 43. Thinking block replay
- **Evidence:** Pass thinking blocks back unmodified during tool use ([Anthropic](https://platform.claude.com/docs/en/build-with-claude/thinking-steering-and-cost.md)).
- **titi:** have. Signed blocks replay to their own family only (`wire.rs:423-440`, test `wire.rs:1691`).

### 44. Tracing and observability
- **Evidence:** Anthropic: full production tracing was what made agent failures diagnosable.
- **titi:** have. Spans with tokens, cache, cost and thinking size (`titi-core/src/trace.rs`, `spans.rs`), `/trace`, OTel-style names, trajectory capture.
- **Do:** add tool-error and retry counts per tool to the eval report (#10).

### 45. Cost visibility
- **titi:** have. Per-turn cost from the catalog price (`registry.rs:67-76`), footer and `/usage`.

### 46. Context awareness in the UI
- **titi:** have. `/context` breaks the window into parts (`runtime.rs:2046-2075`).

### 47. Dependable background execution
- **titi:** have. Auto-background after 60 s, foreground-server interceptor (`intercept.rs`).

---

## H. Negative findings: things the evidence says not to do

48. **Don't prune or rewrite mid-history per turn.** Empryo's measurements, plus OpenAI's note that compaction and truncation reset cache reuse. The one safe case is single-shot masking at compaction (#8).
49. **Don't wrap code in JSON edit payloads or use line-number edit formats** (Aider, 2023, dated but consistent). Titi's hashline anchors hash the text, not the number.
50. **Don't tell Anthropic models to "think less" in the prompt.** Use effort (#5).
51. **Don't make multi-agent the default for coding.** Anthropic says coding is less parallelizable and costs ~15×.
52. **Don't add pre-task plan or status-update prompts for Codex-family models.** They can stop the rollout early.
53. **Don't change tools, effort or model mid-session without accepting a cache miss.** Manus, OpenAI and Anthropic agree.
54. **Don't trust small leaderboard gaps.** Under 3 pp is within infrastructure noise (Anthropic).
55. **Don't oversize `AGENTS.md`.** Claude Code: bloated rule files cause the model to ignore real instructions. Titi's 64 KiB per-file cap (`project_context.rs:19`) is far above Claude Code's guidance. Add a soft warning at about 8k tokens.

---

## Suggested order of work
1. #2, #1, #7 (round cap, operating manual, tool descriptions). Low cost, high expected effect.
2. #10 (eval harness). It validates everything after it.
3. #5, #6, #4 (effort wiring, query-conditioned recall, post-edit checks).
4. #3 with #8 (model-backed compaction plus single-shot result masking).
5. #12, #13, #15 (cache TTL, OpenAI keys, diagnostics).
6. #9, #37, #38 (routing slots, sandbox, permission rules).
7. #39, #40, #27 once PLAN M6 opens.

## Sources
- Anthropic engineering: [context engineering](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents), [writing tools](https://www.anthropic.com/engineering/writing-tools-for-agents), [multi-agent research](https://www.anthropic.com/engineering/multi-agent-research-system), [advanced tool use](https://www.anthropic.com/engineering/advanced-tool-use), [code execution with MCP](https://www.anthropic.com/engineering/code-execution-with-mcp), [long-running harnesses](https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents), [sandboxing](https://www.anthropic.com/engineering/claude-code-sandboxing), [infrastructure noise](https://www.anthropic.com/engineering/infrastructure-noise), [SWE-bench agent](https://www.anthropic.com/engineering/swe-bench-sonnet), [Agent Skills](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills), [context management](https://www.anthropic.com/news/context-management).
- Claude docs: [prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching.md), [thinking and effort](https://platform.claude.com/docs/en/build-with-claude/thinking-steering-and-cost.md), [Claude Code best practices](https://code.claude.com/docs/en/best-practices.md), [Claude Code costs](https://code.claude.com/docs/en/costs.md).
- OpenAI: [prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching.md), [Codex prompting guide](https://developers.openai.com/cookbook/examples/gpt-5/codex_prompting_guide.md). Google: [Gemini caching](https://ai.google.dev/gemini-api/docs/caching.md), [Gemini CLI subagents](https://geminicli.com/docs/core/subagents).
- Aider: [repo map](https://aider.chat/2023/10/22/repomap.html), [unified diffs](https://aider.chat/2023/12/21/unified-diffs.html), [edit formats](https://aider.chat/docs/more/edit-formats.html), [polyglot](https://aider.chat/2024/12/21/polyglot.html).
- Papers: [SWE-agent](https://arxiv.org/html/2405.15793v3), [Complexity Trap](https://arxiv.org/abs/2508.21433), [Scaffold Effect](https://arxiv.org/abs/2607.22585), [Is Grep All You Need](https://arxiv.org/abs/2605.15184).
- Others: [mini-swe-agent](https://mini-swe-agent.com/latest/), [Manus](https://manus.im/blog/Context-Engineering-for-AI-Agents-Lessons-from-Building-Manus), [Cursor semantic search](https://cursor.com/blog/semsearch), Empryo ([llms.txt](https://empryo.com/llms.txt), [compaction](https://empryo.com/docs/context/compaction.md), [task router](https://empryo.com/docs/recipes/task-router.md), [pruning test](https://empryo.com/blog/sol-pi-pruning-on-real-bugs.md)).
- Not read: omp's own source (the shared `omp-parity/GAP.md` was read instead), the Terminal-Bench and SWE-bench Pro leaderboard rows, and the OpenHands condenser source. The OpenHands result I rely on is the Complexity Trap paper's abstract.
