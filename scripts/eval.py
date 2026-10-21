#!/usr/bin/env python3
"""Run titi on small tasks and grade the result: an eval harness.

For every task x run it copies the task's `repo/` into a fresh temp dir, makes
it a git repo, runs `titi --prompt "<task.md>" --approval yolo` there with a
fresh copy of an agent dir, then runs the task's `check.sh` in that dir. Exit
0 from `check.sh` means solved.

  scripts/eval.py --bin target/debug/titi --tasks evals/tasks \
      [--only NAME]... [--runs N] [--agent-dir DIR] [--out report.json] \
      [--timeout S] [--smoke] [--keep]

Task layout (`evals/tasks/<name>/`): `task.md` (the prompt), `repo/` (the
starting project), `check.sh` (offline; runs the project's own test),
optional `hidden/` (the grading tests: copied over the working copy after the
run, overwriting same-named files, so editing or deleting a test cannot pass a
task; the report row says whether the run touched a test file as
`tests_touched`), optional `meta.json` {"timeout_s": int, "tags": [..], "requires": [tool..]}.
A task whose `requires` tool is not on PATH is reported as skipped, not as
unsolved. Names starting with `_` (pipeline probes) run only with `--smoke`.

The numbers come from titi's stdout JSONL (`EngineEvent` in
crates/titi-engine/src/protocol.rs): `TurnUsage` (prompt / completion / cached
tokens, `cost_micro_usd`), `ToolStarted`, `ToolFinished` (`is_error`), `Failed`.
Python 3 standard library only.
"""

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time

# What a run must not inherit from the agent dir it was copied from: sessions
# (`--prompt` resumes the newest one), traces, and state that belongs to a
# person's own use. Keys (`auth.db`) and `config.yml` stay.
AGENT_DIR_SKIP = ("sessions", "traces", "trajectories", "state.db*", "memory.db*")
DEFAULT_TIMEOUT_S = 600
CHECK_TIMEOUT_S = 300


def load_tasks(root, only, smoke):
    tasks = []
    for name in sorted(os.listdir(root)):
        path = os.path.abspath(os.path.join(root, name))
        if not os.path.isfile(os.path.join(path, "task.md")):
            continue
        if only:
            if name not in only:
                continue
        elif name.startswith("_") and not smoke:
            continue
        meta = {}
        meta_path = os.path.join(path, "meta.json")
        if os.path.isfile(meta_path):
            with open(meta_path, encoding="utf-8") as handle:
                meta = json.load(handle)
        with open(os.path.join(path, "task.md"), encoding="utf-8") as handle:
            prompt = handle.read().strip()
        tasks.append({"name": name, "dir": path, "prompt": prompt, "meta": meta})
    missing = sorted(set(only or []) - {task["name"] for task in tasks})
    if missing:
        sys.exit("eval: no such task: " + ", ".join(missing))
    return tasks


def git(cwd, *args):
    # Identity is passed per command so no global config or env is touched.
    subprocess.run(
        ["git", "-c", "user.name=eval", "-c", "user.email=eval@example.invalid",
         "-c", "commit.gpgsign=false", *args],
        cwd=cwd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )


def run_titi(binary, prompt, cwd, agent_dir, timeout_s):
    env = dict(os.environ)
    env["TITI_AGENT_DIR"] = agent_dir
    proc = subprocess.Popen(
        [binary, "--prompt", prompt, "--approval", "yolo"],
        cwd=cwd, env=env, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, errors="replace", start_new_session=True,
    )
    timed_out = False
    try:
        out, err = proc.communicate(timeout=timeout_s)
    except subprocess.TimeoutExpired:
        timed_out = True
        os.killpg(proc.pid, signal.SIGKILL)
        out, err = proc.communicate()
    return proc.returncode, out or "", err or "", timed_out


def parse_events(stdout):
    stats = {
        "prompt_tokens": 0, "completion_tokens": 0, "cached_tokens": 0,
        "cost_micro_usd": 0, "unpriced_turns": 0, "turns": 0,
        "tool_calls": 0, "tool_errors": 0, "failed": None,
    }
    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if not isinstance(event, dict) or len(event) != 1:
            continue
        (kind, body), = event.items()
        if not isinstance(body, dict):
            continue
        if kind == "TurnUsage":
            stats["turns"] += 1
            stats["prompt_tokens"] += body.get("prompt_tokens", 0)
            stats["completion_tokens"] += body.get("completion_tokens", 0)
            stats["cached_tokens"] += body.get("cached_tokens", 0)
            cost = body.get("cost_micro_usd")
            if cost is None:
                stats["unpriced_turns"] += 1
            else:
                stats["cost_micro_usd"] += cost
        elif kind == "ToolStarted":
            stats["tool_calls"] += 1
        elif kind == "ToolFinished":
            if body.get("is_error"):
                stats["tool_errors"] += 1
        elif kind == "Failed":
            stats["failed"] = body.get("message")
    # Unpriced is not free: a run with any unpriced turn has no cost figure.
    stats["cost_usd"] = (
        None if stats["unpriced_turns"] or not stats["turns"]
        else stats["cost_micro_usd"] / 1_000_000
    )
    del stats["cost_micro_usd"]
    return stats


def is_test_path(path):
    parts = path.split("/")
    if "__pycache__" in parts or path.endswith(".pyc"):
        return False
    name = parts[-1].lower()
    return (
        "tests" in parts[:-1] or "test" in parts[:-1]
        or name.startswith("test") or "_test." in name or ".test." in name
        or name.endswith("_test.go")
    )


def tests_touched(workdir, hidden_files):
    """Whether the run added, changed or deleted a test file since `start`."""
    done = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=all", "-z"],
        cwd=workdir, stdout=subprocess.PIPE, text=True, errors="replace", check=True,
    )
    paths = []
    fields = done.stdout.split("\0")
    index = 0
    while index < len(fields):
        entry = fields[index]
        index += 1
        if len(entry) < 4:
            continue
        paths.append(entry[3:])
        if entry[0] in "RC":  # a rename lists its source as the next field
            paths.append(fields[index])
            index += 1
    return any(is_test_path(path) or path in hidden_files for path in paths)


def hidden_paths(task):
    hidden = os.path.join(task["dir"], "hidden")
    return {
        os.path.relpath(os.path.join(root, name), hidden)
        for root, _, names in os.walk(hidden) for name in names
    }


def restore_hidden(task, workdir):
    hidden = os.path.join(task["dir"], "hidden")
    if os.path.isdir(hidden):
        shutil.copytree(hidden, workdir, dirs_exist_ok=True)


def run_check(task, workdir):
    try:
        done = subprocess.run(
            ["bash", os.path.join(task["dir"], "check.sh")], cwd=workdir,
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True, errors="replace",
            timeout=CHECK_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        return False, "check.sh timed out"
    return done.returncode == 0, done.stdout[-2000:]


def run_one(args, task, index, agent_src):
    scratch = tempfile.mkdtemp(prefix="titi-eval-")
    workdir = os.path.join(scratch, "repo")
    agent_dir = os.path.join(scratch, "agent")
    shutil.copytree(os.path.join(task["dir"], "repo"), workdir)
    git(workdir, "init", "-q")
    git(workdir, "add", "-A")
    git(workdir, "commit", "-q", "-m", "start")
    shutil.copytree(agent_src, agent_dir, symlinks=True,
                    ignore=shutil.ignore_patterns(*AGENT_DIR_SKIP))
    timeout_s = args.timeout or task["meta"].get("timeout_s", DEFAULT_TIMEOUT_S)
    started = time.monotonic()
    code, out, err, timed_out = run_titi(args.bin, task["prompt"], workdir, agent_dir, timeout_s)
    wall = time.monotonic() - started
    # Judged before the grading files go back, so what the model did to them shows.
    touched = tests_touched(workdir, hidden_paths(task))
    restore_hidden(task, workdir)
    solved, check_tail = run_check(task, workdir)
    row = {
        "task": task["name"], "run": index, "solved": solved,
        "wall_s": round(wall, 2), "timed_out": timed_out, "exit_code": code,
        "tests_touched": touched,
        **parse_events(out),
    }
    if not solved:
        row["check_tail"] = check_tail
        row["stderr_tail"] = err[-1000:]
    if args.keep:
        row["workdir"] = workdir
        with open(os.path.join(scratch, "stdout.jsonl"), "w", encoding="utf-8") as handle:
            handle.write(out)
        with open(os.path.join(scratch, "stderr.txt"), "w", encoding="utf-8") as handle:
            handle.write(err)
    else:
        shutil.rmtree(scratch, ignore_errors=True)
    return row


def mean(values):
    return sum(values) / len(values) if values else None


def fmt(value, digits=0, prefix=""):
    return "-" if value is None else f"{prefix}{value:.{digits}f}"


def summarize(rows):
    tokens = [r["prompt_tokens"] + r["completion_tokens"] for r in rows]
    costs = [r["cost_usd"] for r in rows if r["cost_usd"] is not None]
    calls = sum(r["tool_calls"] for r in rows)
    errors = sum(r["tool_errors"] for r in rows)
    return {
        "runs": len(rows),
        "solved": sum(1 for r in rows if r["solved"]),
        "timeouts": sum(1 for r in rows if r["timed_out"]),
        "tests_touched": sum(1 for r in rows if r["tests_touched"]),
        "pass_rate": (sum(1 for r in rows if r["solved"]) / len(rows)) if rows else None,
        "mean_wall_s": mean([r["wall_s"] for r in rows]),
        "mean_tokens": mean(tokens),
        "mean_prompt_tokens": mean([r["prompt_tokens"] for r in rows]),
        "mean_completion_tokens": mean([r["completion_tokens"] for r in rows]),
        "mean_cached_tokens": mean([r["cached_tokens"] for r in rows]),
        "mean_cost_usd": mean(costs),
        "priced_runs": len(costs),
        "tool_calls": calls,
        "tool_errors": errors,
        "tool_error_rate": (errors / calls) if calls else None,
    }


def print_report(tasks, rows, skipped):
    print("## Runs\n")
    for task in tasks:
        task_rows = [r for r in rows if r["task"] == task["name"]]
        if not task_rows:
            continue
        print(f"### {task['name']}\n")
        print("| run | solved | wall s | timeout | tests touched | prompt | completion | cached | cost $ | tools | tool errors |")
        print("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
        for r in task_rows:
            print(
                f"| {r['run']} | {'yes' if r['solved'] else 'no'} | {r['wall_s']:.1f} "
                f"| {'yes' if r['timed_out'] else 'no'} | {'yes' if r['tests_touched'] else 'no'} | {r['prompt_tokens']} "
                f"| {r['completion_tokens']} | {r['cached_tokens']} "
                f"| {fmt(r['cost_usd'], 4)} | {r['tool_calls']} | {r['tool_errors']} |"
            )
        print()
    print("## Totals\n")
    print("| task | solved | pass rate | mean wall s | mean tokens | mean cost $ | tool-error rate | timeouts | tests touched |")
    print("| --- | --- | --- | --- | --- | --- | --- | --- | --- |")
    groups = [(task["name"], [r for r in rows if r["task"] == task["name"]]) for task in tasks]
    groups = [(name, group) for name, group in groups if group]
    for name, group in groups + [("**all**", rows)]:
        if not group:
            continue
        s = summarize(group)
        rate = fmt(None if s["tool_error_rate"] is None else s["tool_error_rate"] * 100, 1)
        print(
            f"| {name} | {s['solved']}/{s['runs']} | {s['pass_rate'] * 100:.0f}% "
            f"| {s['mean_wall_s']:.1f} | {fmt(s['mean_tokens'])} | {fmt(s['mean_cost_usd'], 4)} "
            f"| {rate}{'' if rate == '-' else '%'} | {s['timeouts']} | {s['tests_touched']} |"
        )
    for name, why in skipped:
        print(f"| {name} | skipped: {why} | - | - | - | - | - | - | - |")
    print()


def main():
    parser = argparse.ArgumentParser(description="Run titi on small tasks and grade the result.")
    parser.add_argument("--bin", required=True, help="path to the titi binary")
    parser.add_argument("--tasks", required=True, help="directory of task directories")
    parser.add_argument("--only", action="append", help="run just this task (repeatable)")
    parser.add_argument("--runs", type=int, default=1, help="runs per task (use >= 3 for a real number)")
    parser.add_argument("--agent-dir", help="agent dir to copy per run (default: $TITI_AGENT_DIR, else ~/.titi/agent)")
    parser.add_argument("--out", help="write the JSON report here")
    parser.add_argument("--timeout", type=int, help="seconds per titi run; overrides meta.json")
    parser.add_argument("--smoke", action="store_true", help="also run tasks whose name starts with _")
    parser.add_argument("--keep", action="store_true", help="keep each run's workdir, stdout and stderr")
    args = parser.parse_args()
    if args.runs < 1:
        sys.exit("eval: --runs must be at least 1")
    if not os.access(args.bin, os.X_OK):
        sys.exit(f"eval: not an executable: {args.bin}")
    args.bin = os.path.abspath(args.bin)
    agent_src = args.agent_dir or os.environ.get("TITI_AGENT_DIR") or os.path.expanduser("~/.titi/agent")
    if not os.path.isdir(agent_src):
        sys.exit(f"eval: agent dir not found: {agent_src}")

    tasks = load_tasks(args.tasks, args.only, args.smoke)
    runnable, skipped = [], []
    for task in tasks:
        absent = [tool for tool in task["meta"].get("requires", []) if not shutil.which(tool)]
        if absent:
            skipped.append((task["name"], "missing " + ", ".join(absent)))
        else:
            runnable.append(task)
    rows = []
    for task in runnable:
        for index in range(1, args.runs + 1):
            print(f"eval: {task['name']} run {index}/{args.runs} ...", file=sys.stderr, flush=True)
            row = run_one(args, task, index, agent_src)
            print(f"eval:   {'solved' if row['solved'] else 'unsolved'} in {row['wall_s']}s", file=sys.stderr, flush=True)
            rows.append(row)

    print_report(runnable, rows, skipped)
    if args.out:
        report = {
            "bin": args.bin, "runs_per_task": args.runs,
            "tasks": {t["name"]: summarize([r for r in rows if r["task"] == t["name"]]) for t in runnable},
            "skipped": [{"task": name, "reason": why} for name, why in skipped],
            "totals": summarize(rows) if rows else None,
            "runs": rows,
        }
        with open(args.out, "w", encoding="utf-8") as handle:
            json.dump(report, handle, indent=2)
            handle.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
