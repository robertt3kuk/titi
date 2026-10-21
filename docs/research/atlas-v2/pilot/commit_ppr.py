#!/usr/bin/env python3
"""Pilot 2: does a prompt-seeded personalized PageRank (+ co-change) beat the static map on real commits?

Graph dump comes from /tmp/titi-qa/genome-audit/proto (the public Genome API: files, imports, uses, ranks).
Variants (all rank files; metric = recall of the commit's modified files in the top-k):
  static   : genome rank x role factors (what the map shows today)
  lex      : files whose path tokens / export names match the commit message, ranked by match score
  ppr      : personalized PageRank seeded by the lex matches (weights 1.0 exports, path hits), edges both ways
  ppr+cc   : ppr plus git co-change edges (decayed) from the 150 commits before the parent
  mix      : static-normalised + ppr+cc (what a map would show: seeded rank first, static fills the rest)
"""
import json, os, re, subprocess, sys, math, tempfile, collections
sys.path.insert(0, os.path.dirname(__file__))
PROTO = "/tmp/titi-qa/genome-audit/proto/target/release/proto"
CORP = "/tmp/titi-qa/genome-corpus"
EXT = {".rs", ".go", ".py", ".ts", ".tsx", ".js", ".mjs", ".java", ".kt", ".c", ".h", ".cpp", ".cc", ".hpp",
       ".cs", ".swift", ".php", ".rb"}
AUX = re.compile(r"(^|/)(tests?|spec|specs|__tests__|examples?|docs?|testdata|fixtures?|benches|benchmarks?)(/|$)|_test\.|\.test\.|\.spec\.|Tests?\.")
ENTRY = re.compile(r"(^|/)(main|__main__|Program)\.[a-z]+$|(^|/)lib\.rs$|^index\.[a-z]+$|^[^/]+/index\.[a-z]+$")
STOP = {"the", "and", "for", "fix", "add", "use", "with", "from", "that", "this", "when", "not", "update", "remove",
        "make", "test", "tests", "docs", "doc", "src", "lib", "file", "files", "new", "into", "also", "now", "was",
        "are", "can", "all", "any", "does", "has", "have", "bug", "issue", "error", "support", "allow", "change"}


def git(repo, *a, check=True):
    p = subprocess.run(["git", "-C", repo, *a], capture_output=True, text=True)
    if check and p.returncode:
        raise RuntimeError(p.stderr[:200])
    return p.stdout


def src(p):
    return os.path.splitext(p)[1] in EXT


def split_ident(w):
    return [x.lower() for x in re.findall(r"[A-Z]+(?![a-z])|[A-Z]?[a-z]+|\d+", w) if len(x) >= 3]


def message_terms(msg):
    idents = set(w for w in re.findall(r"[A-Za-z_][A-Za-z0-9_]{3,}", msg))
    words = set()
    for w in idents:
        words.add(w.lower())
        words.update(split_ident(w))
    return idents, words - STOP


def seeds_for(msg, files, exports=True):
    """-> {path: weight}. Export-name matches (identifier-looking words) weigh most, path-token matches less."""
    idents, words = message_terms(msg)
    lower_idents = {w.lower() for w in idents if re.search(r"[A-Z_]|\d", w) or len(w) >= 7}
    seeds = collections.defaultdict(float)
    for path, rec in files.items():
        toks = set(split_ident(re.sub(r"[/._-]", " ", path).replace(" ", "")) ) | set(re.findall(r"[a-z]{3,}", path.lower()))
        stem = os.path.splitext(os.path.basename(path))[0].lower()
        s = 0.0
        if stem in words:
            s += 2.0
        s += 0.5 * len((toks & words) - {stem})
        for e in (rec["exports"] if exports else []):
            el = e.lower()
            if el in lower_idents:
                s += 3.0
            elif any(p in words for p in split_ident(e) if len(p) >= 5):
                s += 0.3
        if s:
            seeds[path] = s
    return seeds


def edges_of(files, cochg=None, rev=0.5):
    """directed weighted edges file -> file it depends on, plus reverse at half weight (a change flows both ways)."""
    E = collections.defaultdict(lambda: collections.defaultdict(float))
    for p, r in files.items():
        ev = collections.defaultdict(float)
        for i in r["imports"]:
            if i in files:
                ev[i] += 1.0
        for name, definer, basis in r["uses"]:
            if definer in files:
                ev[definer] += 1.0 if basis == "Scope" else 0.25
        for q, w in ev.items():
            if q != p:
                E[p][q] += math.sqrt(w)
                E[q][p] += rev * math.sqrt(w)
    if cochg:
        for a, bs in cochg.items():
            if a in files:
                for b, w in bs.items():
                    if b in files:
                        E[a][b] += 1.5 * min(w, 2.0)
    return E


def ppr(nodes, E, seeds, d=0.85, iters=20):
    tot = sum(seeds.values()) or 1.0
    tele = {n: seeds.get(n, 0.0) / tot for n in nodes}
    s = dict(tele)
    out = {n: sum(E[n].values()) for n in nodes}
    for _ in range(iters):
        nxt = {n: (1 - d) * tele[n] for n in nodes}
        for a in nodes:
            if out[a] <= 0 or s[a] == 0:
                for n in seeds:  # dangling mass returns to the seeds
                    nxt[n] += d * s[a] * tele[n]
                continue
            for b, w in E[a].items():
                nxt[b] += d * s[a] * w / out[a]
        s = nxt
    return s


def static_scores(files):
    sc = {}
    for p, r in files.items():
        v = r["rank"] or 0.0
        if AUX.search(p):
            v *= 0.25
        elif ENTRY.search(p):
            v *= 2.0
        sc[p] = v
    return sc


def cochange(repo, parent, depth=150):
    log = git(repo, "log", f"-{depth}", "--no-merges", "--name-only", "--format=@@%H", parent)
    pair = collections.defaultdict(lambda: collections.defaultdict(float))
    for age, chunk in enumerate(log.split("@@")[1:]):
        files = [f for f in chunk.strip().split("\n")[1:] if f and src(f)]
        if not 2 <= len(files) <= 12:
            continue
        w = 0.5 ** (age / 50) / math.sqrt(len(files))
        for a in files:
            for b in files:
                if a != b:
                    pair[a][b] += w
    return pair


def topk(scores, k, only=None):
    items = [(v, p) for p, v in scores.items() if v > 0 and (only is None or p in only)]
    items.sort(key=lambda x: (-x[0], x[1]))
    return [p for _, p in items[:k]]


def norm(s):
    m = max(s.values()) if s else 1.0
    return {k: v / m for k, v in s.items()} if m else s


def main():
    repos = sys.argv[1].split(",")
    per = int(sys.argv[2]) if len(sys.argv) > 2 else 12
    K = (10, 24)
    res = collections.defaultdict(lambda: collections.defaultdict(list))
    detail = []
    for name in repos:
        repo = os.path.join(CORP, name)
        head = git(repo, "rev-parse", "HEAD").strip()
        revs = git(repo, "rev-list", "--no-merges", "-n", "400", "HEAD").split()
        tmp = tempfile.mkdtemp(prefix="cp-")
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
            if not 1 <= len(mod) <= 8 or len(msg) < 20 or len(diff) > 12:
                continue
            try:
                git(tmp, "checkout", "-q", "--detach", parent)
            except RuntimeError:
                continue
            out = subprocess.run([PROTO, tmp], capture_output=True, text=True).stdout
            files = json.loads(out)
            truth = set(mod) & set(files)
            if not truth:
                continue
            seeds = seeds_for(msg, files)
            E0 = edges_of(files)
            cc = cochange(repo, parent)
            E1 = edges_of(files, cc)
            nodes = list(files)
            st = static_scores(files)
            variants = {"static": st, "lex": dict(seeds)}
            seeds_p = seeds_for(msg, files, exports=False)
            if seeds_p:
                variants["ppr:paths-only"] = ppr(nodes, E1, seeds_p)
                variants["ppr:fwd-only"] = ppr(nodes, edges_of(files, cc, rev=0.0), seeds) if seeds else dict(st)
                variants["ppr:no-cc"] = ppr(nodes, E0, seeds) if seeds else dict(st)
            if seeds:
                p0 = ppr(nodes, E0, seeds)
                p1 = ppr(nodes, E1, seeds)
                variants["ppr"] = p0
                variants["ppr+cc"] = p1
                mixed = {n: p1.get(n, 0.0) + 0.05 * norm(st).get(n, 0.0) * max(p1.values()) for n in nodes}
                variants["mix"] = mixed
            else:
                for v in ("ppr", "ppr+cc", "mix"):
                    variants[v] = dict(st)
            for v, sc in variants.items():
                for k in K:
                    got = set(topk(sc, k))
                    res[v][k].append(len(truth & got) / len(truth))
                    if v in ("static", "mix"):
                        res[v + "@" + name][k].append(len(truth & got) / len(truth))
            res["_n"][0].append(len(files))
            detail.append(dict(repo=name, rev=rev[:8], n=len(truth), files=len(files), seeds=len(seeds),
                               msg=msg.split("\n")[0][:60]))
            done += 1
        git(repo, "worktree", "remove", "--force", tmp, check=False)
    print(f"{'variant':8} " + " ".join(f"r@{k:<3}" for k in K))
    for v in ("static", "lex", "ppr", "ppr+cc", "mix", "ppr:paths-only", "ppr:fwd-only", "ppr:no-cc"):
        if v not in res: continue
        print(f"{v:8} " + " ".join(f"{sum(res[v][k]) / len(res[v][k]):5.2f}" for k in K), f"(n={len(res[v][24])})")
    for name in repos:
        a=res["static@"+name]; b=res["mix@"+name]
        if a[24]: print(f"{name:10} static r@10/24 {sum(a[10])/len(a[10]):.2f}/{sum(a[24])/len(a[24]):.2f}  mix {sum(b[10])/len(b[10]):.2f}/{sum(b[24])/len(b[24]):.2f}  n={len(a[24])}")
    json.dump(detail, open("/tmp/titi-qa/genome-audit/commit_ppr.json", "w"))


main()
