# Evals: measuring titi's own harness

Practice #10 in [agent-practices](../agent-practices/README.md): without a
run that grades real tasks, none of the other practices can be measured.
`scripts/eval.py` is that run: small tasks, a check per task, and numbers from
titi's own event stream. Python 3 standard library only.

## What it does

For each task × run it

1. copies `evals/tasks/<name>/repo/` into a fresh temp dir, `git init` and one
   commit (so `git diff` shows what the agent changed);
2. copies the agent dir into a fresh temp dir, leaving out `sessions`,
   `traces`, `trajectories`, `state.db*` and `memory.db*` (`--prompt` resumes
   the newest session, which would leak one run into the next). `config.yml`
   and `auth.db` stay, so the key comes along;
3. runs `titi --prompt "<task.md>" --approval yolo` there with
   `TITI_AGENT_DIR` pointed at the copy, and kills the whole process group at
   the timeout;
4. copies the task's `hidden/` (the grading tests) over the working copy,
   overwriting same-named files, then runs `check.sh` there. A model that edits
   or deletes a test cannot pass that way; the report row says whether it tried
   (`tests_touched`, from `git status` against the start commit, taken before
   `hidden/` goes back).
   Exit 0 from `check.sh` is *solved*. It is
   graded even after a timeout, because the work may already be done.

The environment is inherited except `TITI_AGENT_DIR`, so `PATH` finds `cargo`
and `node`, and a provider key in the environment reaches titi.

## Running against a real model

Put the key in an agent dir once, never in the repo:

```bash
mkdir -p ~/.titi/eval-agent
TITI_AGENT_DIR=~/.titi/eval-agent target/release/titi --set-key <provider> <key>
# a config.yml in that dir picks the model (modelRoles.default)
scripts/eval.py --bin target/release/titi --tasks evals/tasks \
    --agent-dir ~/.titi/eval-agent --runs 3 --out /tmp/eval-report.json
```

Flags: `--only NAME` (repeatable), `--runs N`, `--timeout S` (overrides every
`meta.json`; default 300 s per task there, 600 s without), `--keep` (leave each
run's workdir, `stdout.jsonl` and `stderr.txt` in the temp dir and print the
path in the report), `--smoke` (also run tasks whose name starts with `_`).

A task whose `meta.json` `requires` tool is not on `PATH` is listed as
*skipped*, not counted as unsolved.

## Adding a task

`evals/tasks/<name>/` holds `task.md` (the prompt, word for word),
`repo/` (a tiny self-contained project; it may hold a copy of the tests so the
model can run them), `hidden/` (the tests that grade, same paths as in `repo/`),
`check.sh` (runs only the hidden tests, offline; it runs from outside the repo, so the agent cannot edit it) and
optional `meta.json` `{"timeout_s": 300, "tags": [...], "requires": [...]}`.
The check must fail on the untouched `repo/`; verify that before committing.

`_smoke` is a pipeline probe, not a task: its prompt is `edit readme`, which
only the scripted provider (`scripts/fake-provider.py`) answers with an edit
that satisfies its check. Against that provider it must come out solved and
the other tasks unsolved — that is the proof the grading is not vacuous.

## Conditions: an A/B in one command

`--conditions atlas-off,atlas-map,atlas-full` runs every task × run once per
condition, each with its own environment, and adds a per-condition table
(pass rate, mean tokens, mean cost, mean wall time, mean tool calls, how many
`atlas` tool calls the model made and in how many runs, counted from
`ToolStarted` names):

```bash
scripts/eval.py --bin target/release/titi --tasks evals/tasks \
    --agent-dir ~/.titi/eval-agent --runs 3 \
    --conditions atlas-off,atlas-map,atlas-full --out /tmp/eval-ab.json
```

| condition | environment | what the model sees |
| --- | --- | --- |
| `atlas-off` | `TITI_NO_ATLAS=1` | no `<atlas>` map, no lens |
| `atlas-full` | (default) | map, per-prompt lens and, once it exists, the atlas tool |
| `atlas-map` | **missing knob** | the map only |

`atlas-map` is not run today: titi has no setting or environment variable
that turns off the lens (and, later, the atlas tool) while keeping the map,
so the harness reports it as "not run" instead of silently running it as
`atlas-full`. When such a knob exists, add it without touching the script:
`--condition-env atlas-map=TITI_ATLAS_NO_LENS=1` (name and value are whatever
the knob is; repeat the flag for several variables, and any other name defines
a new condition). Atlas variables inherited from the shell are cleared for a
named condition. Without `--conditions` the run is as before, one unnamed
condition, and the report has no conditions table.

Smoke (proves the plumbing with the scripted provider, no key): point an agent
dir at `scripts/fake-provider.py` as in its docstring, start it, and run
`scripts/eval.py --bin target/release/titi --tasks evals/tasks --only _smoke
--smoke --runs 2 --agent-dir <dir> --conditions atlas-off,atlas-map,atlas-full`.
`_smoke` is solved in both runnable conditions, `atlas-map` is listed as not
run, and the provider's `requests.jsonl` shows `<atlas>` in the `atlas-full`
requests only.

## What the numbers mean

From titi's stdout JSONL (`EngineEvent`, `crates/titi-engine/src/protocol.rs`):

| Column | Source |
| --- | --- |
| solved | `check.sh` exit 0 |
| wall s | harness clock around the titi process |
| prompt / completion / cached | sum of `TurnUsage` `prompt_tokens`, `completion_tokens`, `cached_tokens` (cached is part of prompt) |
| cost $ | sum of `TurnUsage.cost_micro_usd` / 1e6. `-` means at least one turn had no price (*unpriced*, not free) — local models and unpriced ids always show `-` |
| tools | count of `ToolStarted` |
| tool errors | `ToolFinished` with `is_error` |
| tool-error rate | tool errors / tool calls over the group |
| tests touched | the run added, changed or deleted a test file (or a `hidden/` path) |
| timeouts | runs killed at the time limit |

The JSON report also has `turns`, `failed` (the `Failed` event's message),
`exit_code`, and for unsolved runs the tail of `check.sh` output and of titi's
stderr. Mean cost averages only the priced runs (`priced_runs` says how many).

## Variance

One run of one task says almost nothing: a model that solves a task 60% of the
time passes a single run 60% of the time. Use `--runs 3` at least, and compare
two configurations (a prompt change, a model, a tool) on the same task set
with the same N. With five tasks × 3 runs a difference of one or two solved
runs is noise; look at token and cost means and the tool-error rate as well,
which move less. Treat a change as real only when it repeats across a second
batch. Never compare a report from one model with one from another unless the
cost column is priced for both.

## Limits

Five tiny tasks measure regressions in the harness (loop, tools, prompt), not
general capability, and a hosted model's answer is not deterministic. No
container isolates a run: the agent has your user's file access under
`--approval yolo`, so run real-model evals on a machine you accept that on.
