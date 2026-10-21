#!/usr/bin/env python3
"""Pilot of the task-level eval: replay real commits against the static map.

For each repo: take recent non-merge commits that modify 1-8 indexed source files,
check out the PARENT tree, build the map (examples/map, limit 24 and 64), and ask:
  - recall of the files the commit modified inside the static map (top 24 / top 64)
  - the same with files whose path tokens appear in the commit message added (a stand-in for a
    prompt-seeded personalization / a `find` tool)
  - co-change: given ONE of the modified files, how many of the others are among its top-5
    historical partners (decayed counts over the 150 commits before the parent)
Output: JSON lines + a summary table.
"""
import json, os, re, subprocess, sys, math, tempfile, collections

MAP = os.environ.get("MAP", "/Users/homie/proj/titi-wt/atlas-design/target/release/examples/map")
CORP = "/tmp/titi-qa/genome-corpus"
EXT = {".rs", ".go", ".py", ".ts", ".tsx", ".js", ".mjs", ".java", ".kt", ".c", ".h", ".cpp", ".cc", ".hpp",
       ".cs", ".swift", ".php", ".rb"}
AUX = re.compile(r"(^|/)(tests?|spec|specs|__tests__|examples?|docs?|testdata|fixtures?|benches|benchmarks?)(/|$)|_test\.|\.test\.|\.spec\.|Tests?\.")


def git(repo, *a, check=True):
    p = subprocess.run(["git", "-C", repo, *a], capture_output=True, text=True)
    if check and p.returncode:
        raise RuntimeError(p.stderr[:200])
    return p.stdout


def src(p):
    return os.path.splitext(p)[1] in EXT


def words(msg):
    out = set()
    for w in re.findall(r"[A-Za-z_][A-Za-z0-9_]{2,}", msg):
        out.add(w.lower())
        for part in re.findall(r"[A-Z]?[a-z]+|[A-Z]+(?![a-z])|\d+", w):
            if len(part) >= 3:
                out.add(part.lower())
    return out - {"the", "and", "for", "fix", "add", "use", "with", "from", "that", "this", "when", "not",
                   "update", "remove", "make", "test", "tests", "docs", "doc", "src", "lib", "file", "files"}


def path_hits(files, ws):
    hits = []
    for f in files:
        toks = set(re.findall(r"[a-z]+", re.sub(r"([a-z])([A-Z])", r"\1 \2", f).lower()))
        stem = os.path.splitext(os.path.basename(f))[0].lower()
        score = len(toks & ws) + (2 if stem in ws else 0)
        if score:
            hits.append((score, f))
    hits.sort(key=lambda x: (-x[0], x[1]))
    return [f for _, f in hits]


def cochange(repo, parent, depth=150):
    log = git(repo, "log", f"-{depth}", "--no-merges", "--name-only", "--format=@@%H", parent)
    weight = collections.defaultdict(float)
    pair = collections.defaultdict(lambda: collections.defaultdict(float))
    commits = [c.strip().split("\n")[1:] for c in log.split("@@")[1:]]
    for age, files in enumerate(commits):
        files = [f for f in files if f and src(f)]
        if not 2 <= len(files) <= 12:
            continue
        w = 0.5 ** (age / 50)
        for a in files:
            for b in files:
                if a != b:
                    pair[a][b] += w / math.sqrt(len(files))
    return pair


def run_map(repo, n):
    p = subprocess.run([MAP, repo, str(n)], capture_output=True, text=True)
    paths = []
    for line in p.stdout.splitlines():
        m = re.match(r"^(\S.*?) \(→\d+\)", line)
        if m:
            paths.append(m[1])
    return paths, len(p.stdout)


def main():
    repos = sys.argv[1].split(",") if len(sys.argv) > 1 else ["cobra", "express", "requests", "ripgrep", "ky", "gson"]
    per = int(sys.argv[2]) if len(sys.argv) > 2 else 12
    rows = []
    for name in repos:
        repo = os.path.join(CORP, name)
        head = git(repo, "rev-parse", "HEAD").strip()
        revs = git(repo, "rev-list", "--no-merges", "-n", "400", "HEAD").split()
        tmp = tempfile.mkdtemp(prefix="ce-")
        git(repo, "worktree", "add", "--detach", tmp, head)
        done = 0
        for rev in revs:
            if done >= per:
                break
            parents = git(repo, "rev-list", "--parents", "-n", "1", rev).split()[1:]
            if len(parents) != 1:
                continue
            parent = parents[0]
            msg = git(repo, "log", "-1", "--format=%s%n%b", rev).strip()
            diff = git(repo, "diff", "--name-status", "-M", parent, rev).splitlines()
            mod = [l.split("\t")[-1] for l in diff if l.startswith("M") and src(l.split("\t")[-1])]
            other = [l for l in diff if not l.startswith("M")]
            allc = [l.split("\t")[-1] for l in diff]
            if not 1 <= len(mod) <= 8 or len(msg) < 20 or len(allc) > 12:
                continue
            try:
                git(tmp, "checkout", "-q", "--detach", parent)
            except RuntimeError:
                continue
            m24, b24 = run_map(tmp, 24)
            m64, b64 = run_map(tmp, 64)
            tracked = [f for f in git(tmp, "ls-files").splitlines() if src(f)]
            ws = words(msg)
            hits = path_hits(tracked, ws)
            truth = set(mod)
            prod = {f for f in truth if not AUX.search(f)}
            r24 = len(truth & set(m24)) / len(truth)
            r64 = len(truth & set(m64)) / len(truth)
            seeded = set(m24) | set(hits[:8])
            rs = len(truth & seeded) / len(truth)
            only_lex = len(truth & set(hits[:8])) / len(truth)
            # co-change: pretend the first modified file is known
            pair = cochange(repo, parent)
            seed = sorted(truth)[0]
            rest = truth - {seed}
            co = None
            if rest:
                top = [b for b, _ in sorted(pair.get(seed, {}).items(), key=lambda x: -x[1])[:5]]
                co = len(rest & set(top)) / len(rest)
            rows.append(dict(repo=name, rev=rev[:8], n=len(truth), r24=r24, r64=r64, lex8=only_lex,
                             seeded24=rs, co5=co, tok24=b24 / 4, tok64=b64 / 4, msg=msg.split("\n")[0][:70]))
            done += 1
        git(repo, "worktree", "remove", "--force", tmp, check=False)
    with open("/tmp/titi-qa/genome-audit/commit_eval.jsonl", "w") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    by = collections.defaultdict(list)
    for r in rows:
        by[r["repo"]].append(r)
    print(f"{'repo':10} {'tasks':>5} {'map24':>6} {'map64':>6} {'lex8':>6} {'map24+lex8':>10} {'cochg5|1':>9} {'tok24':>6} {'tok64':>6}")
    tot = collections.defaultdict(list)
    for name, rs in by.items():
        def avg(k):
            v = [x[k] for x in rs if x[k] is not None]
            return sum(v) / len(v) if v else float('nan')
        print(f"{name:10} {len(rs):5} {avg('r24'):6.2f} {avg('r64'):6.2f} {avg('lex8'):6.2f} {avg('seeded24'):10.2f} {avg('co5'):9.2f} {avg('tok24'):6.0f} {avg('tok64'):6.0f}")
        for k in ("r24", "r64", "lex8", "seeded24", "co5", "tok24", "tok64"):
            tot[k] += [x[k] for x in rs if x[k] is not None]
    print("ALL        %5d %6.2f %6.2f %6.2f %10.2f %9.2f %6.0f %6.0f" % (
        len(rows), *(sum(tot[k]) / len(tot[k]) for k in ("r24", "r64", "lex8", "seeded24", "co5", "tok24", "tok64"))))


main()
