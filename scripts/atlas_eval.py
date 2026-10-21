#!/usr/bin/env python3
"""Atlas verification harness: map, check, cost, LSP probes, incremental probe.

    python3 scripts/atlas_eval.py [--corpus DIR] [--only NAME[,NAME]] [--out report.json]

For each repo under the corpus dir (default /tmp/titi-qa/genome-corpus) it:
  1. runs the `map` example with N=24 and parses the projection;
  2. runs `titi atlas check` from the repo root (exit code, diagnostics by code);
  3. measures cold index+render wall time and peak RSS (`/usr/bin/time -l`);
  4. drives `titi atlas lsp` over stdio (documentSymbol on the top file;
     definition + references for the top export of the top 3 files; definition
     from a reference site back to the exporting file);
  5. appends an exported function to the top file, measures a cold re-index of
     the modified disk file and the LSP didOpen/didChange path, then reverts;
  6. scores the map against hand-written ground truth (TRUTH below) and flags
     suspicious lines.

`--mode commits` replays real commits instead (task = commit message, index built at the parent,
truth = modified indexed files; recall@10/@24 and map tokens from the real `titi atlas dump`).

Build first: cargo build --release -p titi-cli -p titi-atlas --examples --locked
Stdlib only. Raw results go to --out (default <corpus>/report.json).
"""
import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")) / "release"
MAP_BIN = TARGET / "examples" / "map"
TITI_BIN = TARGET / "titi"
N = 24

# name -> (language, snippet appended to the top file for the incremental probe,
#          marker that must appear in a documentSymbol name afterwards)
SNIPPET = {
    "rust": "\npub fn titi_probe_added() {}\n",
    "go": "\nfunc TitiProbeAdded() {}\n",
    "python": "\n\ndef titi_probe_added():\n    pass\n",
    "typescript": "\nexport function titiProbeAdded() {}\n",
    "javascript": "\nexport function titiProbeAdded() {}\n",
    "java": "\nclass TitiProbeAdded {}\n",
    "kotlin": "\nfun titiProbeAdded() {}\n",
    "c": "\nint titi_probe_added(void) { return 0; }\n",
    "cpp": "\nint titi_probe_added(void) { return 0; }\n",
    "csharp": "\npublic class TitiProbeAdded {}\n",
    "swift": "\npublic func titiProbeAdded() {}\n",
    "php": "\nfunction titi_probe_added() {}\n",
    "ruby": "\ndef titi_probe_added; end\n",
}
EXT_LANG = {
    ".rs": "rust", ".go": "go", ".py": "python", ".ts": "typescript", ".tsx": "typescript",
    ".mts": "typescript", ".js": "javascript", ".mjs": "javascript", ".cjs": "javascript",
    ".jsx": "javascript", ".java": "java", ".kt": "kotlin", ".kts": "kotlin", ".c": "c",
    ".h": "c", ".cpp": "cpp", ".cc": "cpp", ".hpp": "cpp", ".cs": "csharp",
    ".swift": "swift", ".php": "php", ".rb": "ruby",
}

# Ground truth: files a maintainer would call central. `source` says where the
# list comes from. Paths are repo-relative.
TRUTH = {
    "ripgrep": {
        "source": "GUIDE/README crate list + the grep-* crate architecture: the binary entry, "
                  "searcher, matcher, regex matcher, printer, ignore walker, globset",
        "files": [
            "crates/core/main.rs", "crates/searcher/src/searcher/mod.rs",
            "crates/matcher/src/lib.rs", "crates/regex/src/matcher.rs",
            "crates/printer/src/lib.rs", "crates/ignore/src/walk.rs",
            "crates/globset/src/lib.rs", "crates/core/flags/parse.rs",
        ],
    },
    "cobra": {
        "source": "README (Command, Args, completions are the user-facing surface)",
        "files": ["command.go", "cobra.go", "args.go", "completions.go", "flag_groups.go",
                  "shell_completions.go"],
    },
    "requests": {
        "source": "docs/api + README: the public API is api/sessions/models/adapters",
        "files": ["src/requests/api.py", "src/requests/sessions.py", "src/requests/models.py",
                  "src/requests/adapters.py", "src/requests/exceptions.py",
                  "src/requests/__init__.py", "src/requests/auth.py"],
    },
    "ky": {
        "source": "readme: the `ky` default export, its options and errors (source/index.ts, core/Ky.ts)",
        "files": ["source/index.ts", "source/core/Ky.ts", "source/types/options.ts",
                  "source/errors/HTTPError.ts", "source/errors/TimeoutError.ts"],
    },
    "express": {
        "source": "package.json main + Readme: index.js -> lib/express.js, application, request, response, router",
        "files": ["index.js", "lib/express.js", "lib/application.js", "lib/request.js",
                  "lib/response.js", "lib/view.js", "lib/utils.js"],
    },
    "gson": {
        "source": "UserGuide.md: Gson, GsonBuilder, TypeAdapter, JsonElement, reader/writer",
        "files": ["gson/src/main/java/com/google/gson/Gson.java",
                  "gson/src/main/java/com/google/gson/GsonBuilder.java",
                  "gson/src/main/java/com/google/gson/TypeAdapter.java",
                  "gson/src/main/java/com/google/gson/JsonElement.java",
                  "gson/src/main/java/com/google/gson/stream/JsonReader.java",
                  "gson/src/main/java/com/google/gson/stream/JsonWriter.java",
                  "gson/src/main/java/com/google/gson/TypeAdapterFactory.java"],
    },
    "okio": {
        "source": "README: ByteString, Buffer, Source/Sink, FileSystem, BufferedSource/Sink",
        "files": ["okio/src/commonMain/kotlin/okio/ByteString.kt",
                  "okio/src/commonMain/kotlin/okio/Buffer.kt",
                  "okio/src/commonMain/kotlin/okio/Source.kt",
                  "okio/src/commonMain/kotlin/okio/Sink.kt",
                  "okio/src/commonMain/kotlin/okio/FileSystem.kt",
                  "okio/src/commonMain/kotlin/okio/BufferedSource.kt",
                  "okio/src/commonMain/kotlin/okio/BufferedSink.kt",
                  "okio/src/commonMain/kotlin/okio/Path.kt"],
    },
    "kilo": {
        "source": "single-file editor; the only source file",
        "files": ["kilo.c"],
    },
    "hiredis": {
        "source": "README: hiredis.h/c (sync API), async, read (reply parser), net, sds",
        "files": ["hiredis.c", "hiredis.h", "async.c", "async.h", "read.c", "read.h", "net.c",
                  "sds.h", "sds.c"],
    },
    "json": {
        "source": "README: single header nlohmann/json.hpp (single_include) is the library; "
                  "include/nlohmann/json.hpp is the multi-header entry",
        "files": ["single_include/nlohmann/json.hpp", "include/nlohmann/json.hpp",
                  "include/nlohmann/json_fwd.hpp", "include/nlohmann/detail/json_pointer.hpp",
                  "include/nlohmann/detail/input/parser.hpp",
                  "include/nlohmann/detail/output/serializer.hpp",
                  "include/nlohmann/detail/value_t.hpp"],
    },
    "AutoMapper": {
        "source": "docs + README: MapperConfiguration, Mapper, IMapper, Profile, TypeMap",
        "files": ["src/AutoMapper/Configuration/MapperConfiguration.cs",
                  "src/AutoMapper/Mapper.cs", "src/AutoMapper/Configuration/Profile.cs",
                  "src/AutoMapper/TypeMap.cs",
                  "src/AutoMapper/Configuration/MapperConfigurationExpression.cs",
                  "src/AutoMapper/Configuration/IMappingExpression.cs"],
    },
    "Alamofire": {
        "source": "README: Session, Request, DataRequest, Response serialization, Interceptor",
        "files": ["Source/Core/Session.swift", "Source/Core/Request.swift",
                  "Source/Core/DataRequest.swift", "Source/Core/AFError.swift",
                  "Source/Features/RequestInterceptor.swift",
                  "Source/Features/ResponseSerialization.swift",
                  "Source/Alamofire.swift", "Source/Core/Protected.swift"],
    },
    "guzzle": {
        "source": "README/docs: Client, HandlerStack, Middleware, Psr7 messages, Exception",
        "files": ["src/Client.php", "src/ClientInterface.php", "src/HandlerStack.php",
                  "src/Middleware.php", "src/RequestOptions.php", "src/Utils.php",
                  "src/Handler/CurlFactory.php", "src/Pool.php"],
    },
    "sinatra": {
        "source": "README: lib/sinatra/base.rb is the framework; main.rb is the classic DSL",
        "files": ["lib/sinatra/base.rb", "lib/sinatra/main.rb", "lib/sinatra.rb",
                  "lib/sinatra/show_exceptions.rb", "lib/sinatra/indifferent_hash.rb"],
    },
    "titi": {
        "source": "docs/ARCHITECTURE.md crate table: CLI main, engine turn loop, atlas lib, "
                  "providers, tools, core sessions",
        "files": ["crates/titi-cli/src/main.rs", "crates/titi-engine/src/lib.rs",
                  "crates/titi-atlas/src/lib.rs", "crates/titi-atlas/src/graph.rs",
                  "crates/titi-tools/src/lib.rs", "crates/titi-providers/src/lib.rs",
                  "crates/titi-core/src/lib.rs", "crates/titi-config/src/lib.rs"],
    },
}

GENERIC = {"new", "get", "set", "run", "init", "main", "default", "test", "from", "into", "len",
           "name", "id", "value", "data", "build", "create", "parse", "open", "close", "read",
           "write", "add", "remove", "update", "list", "all", "of", "to", "is", "has", "next",
           "with", "handle", "process", "execute", "call", "apply", "make", "type", "config",
           "options", "utils", "helper", "index", "setup", "start", "stop", "clone", "equals",
           "hashCode", "toString"}
NOISE_PATH = re.compile(r"(^|/)(tests?|__tests__|spec|fixtures?|testdata|third_?party|vendor|"
                        r"node_modules|generated|gen|examples?|samples?|benchmarks?|benchsuite|"
                        r"docs?|bench|fuzz|site|test-[a-z-]+)(/|$)|_test\.|\.test\.|\.spec\.|"
                        r"Tests?\.|\.min\.|\.generated\.|\.pb\.|_pb2", re.I)


def run_time(cmd, cwd):
    """Run cmd under /usr/bin/time -l; return (proc, wall_ms, rss_mb)."""
    t0 = time.perf_counter()
    p = subprocess.run(["/usr/bin/time", "-l", *cmd], cwd=cwd, capture_output=True, text=True)
    wall = (time.perf_counter() - t0) * 1000
    m = re.search(r"(\d+)\s+maximum resident set size", p.stderr)
    rss = int(m.group(1)) / 1048576 if m else None
    return p, wall, rss


def parse_map(text):
    """-> list of {path, deps, tags, recent, exports:[[name, users]], signatures:{name: sig}} in print order.

    A file line is `path (→N)` followed by any number of ` [TAG]` suffixes (`[RECENT]`, `[entry]`,
    `[test]`, ...); an export line is `  +Name (users)`, optionally followed by two spaces and a
    one-line signature. Name and users are read from the prefix only.
    """
    entries = []
    for line in text.splitlines():
        if line.startswith("<") or not line.strip():
            continue
        m = re.match(r"^(\S.*?) \(→(\d+)\)((?: \[[A-Za-z]+\])*)\s*$", line)
        if m:
            tags = re.findall(r"\[([A-Za-z]+)\]", m[3])
            entries.append({"path": m[1], "deps": int(m[2]), "tags": tags, "recent": "RECENT" in tags,
                            "exports": [], "signatures": {}, "raw": line})
            continue
        m = re.match(r"^\s+\+(.+?) \((\d+)\)(?:  (.*?))?\s*$", line)
        if m and entries:
            entries[-1]["exports"].append([m[1], int(m[2])])
            if m[3]:
                entries[-1]["signatures"][m[1]] = m[3]
            entries[-1].setdefault("raw_exports", []).append(line)
            continue
        if line.startswith(("  ", "\t")) and entries:
            entries[-1].setdefault("other", []).append(line)
        elif entries or not line.startswith("<"):
            entries.append({"path": line.strip(), "deps": None, "tags": [], "recent": "[RECENT]" in line,
                            "exports": [], "signatures": {}, "raw": line, "unparsed": True})
    return entries


def self_test_parse_map():
    old = "<atlas>\nsrc/a.rs (→3) [RECENT]\n  +Engine (4)\n  +run (2)\nsrc/b.rs (→0)\n</atlas>"
    new = ("<atlas>\nsrc/main.rs (→0) [entry]\n  +run (7)  pub fn run(&self) -> Out\n"
           "tests/t.rs (→2) [test] [RECENT]\n  +Name With Space (1)  fn x()  (and parens) \nsrc/c.rs (→5)\n</atlas>")
    a = parse_map(old)
    assert [e["path"] for e in a] == ["src/a.rs", "src/b.rs"], a
    assert a[0]["recent"] and a[0]["tags"] == ["RECENT"] and a[0]["deps"] == 3
    assert a[0]["exports"] == [["Engine", 4], ["run", 2]] and a[0]["signatures"] == {}
    assert not a[1]["recent"] and a[1]["tags"] == [] and a[1]["deps"] == 0
    b = parse_map(new)
    assert [e["path"] for e in b] == ["src/main.rs", "tests/t.rs", "src/c.rs"], b
    assert b[0]["tags"] == ["entry"] and not b[0]["recent"]
    assert b[0]["exports"] == [["run", 7]] and b[0]["signatures"] == {"run": "pub fn run(&self) -> Out"}
    assert b[1]["tags"] == ["test", "RECENT"] and b[1]["recent"]
    assert b[1]["exports"] == [["Name With Space", 1]]
    assert b[1]["signatures"] == {"Name With Space": "fn x()  (and parens)"}
    assert not any(e.get("unparsed") for e in a + b)
    print("parse_map self-test: ok")


def map_text(repo):
    p, wall, rss = run_time([str(MAP_BIN), str(repo), str(N)], repo)
    return p, wall, rss


# ---- LSP client ------------------------------------------------------------

class Lsp:
    def __init__(self, root):
        self.root = str(Path(root).resolve())
        self.p = subprocess.Popen([str(TITI_BIN), "atlas", "lsp"], cwd=self.root,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE)
        self.id = 0

    def send(self, obj):
        body = json.dumps(obj).encode()
        self.p.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        self.p.stdin.flush()

    def recv(self):
        length = None
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise EOFError("lsp closed: " + self.p.stderr.read().decode(errors="replace")[:300])
            line = line.strip()
            if not line:
                break
            if line.lower().startswith(b"content-length:"):
                length = int(line.split(b":")[1])
        return json.loads(self.p.stdout.read(length))

    def request(self, method, params):
        self.id += 1
        self.send({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
        while True:
            msg = self.recv()
            if msg.get("id") == self.id:
                return msg.get("result")

    def notify(self, method, params):
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def uri(self, rel):
        return "file://" + os.path.join(self.root, rel)

    def start(self):
        self.request("initialize", {"processId": None, "rootUri": "file://" + self.root,
                                    "capabilities": {}})
        self.notify("initialized", {})

    def doc_symbols(self, rel):
        return self.request("textDocument/documentSymbol",
                            {"textDocument": {"uri": self.uri(rel)}}) or []

    def at(self, method, rel, line0, ch, extra=None):
        params = {"textDocument": {"uri": self.uri(rel)},
                  "position": {"line": line0, "character": ch}}
        if extra:
            params.update(extra)
        return self.request(method, params)

    def close(self):
        try:
            self.request("shutdown", None)
            self.notify("exit", None)
            self.p.stdin.close()
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()


def uri_rel(root, uri):
    return uri[len("file://"):].replace(os.path.realpath(root) + "/", "", 1) if uri else None


def pick_symbol(symbols, name):
    for s in symbols:
        if s["name"] == name:
            return s
    return None


def lsp_probes(repo, entries):
    """Returns dict with a list of probe results."""
    root = str(Path(repo).resolve())
    out = {"probes": [], "error": None}
    if not entries:
        out["error"] = "empty map"
        return out
    lsp = Lsp(root)
    try:
        t0 = time.perf_counter()
        lsp.start()
        out["init_ms"] = (time.perf_counter() - t0) * 1000
        top = entries[0]["path"]
        syms = lsp.doc_symbols(top)
        out["probes"].append({"probe": "documentSymbol", "file": top, "ok": len(syms) > 0,
                              "n": len(syms), "exports_in_map": [e[0] for e in entries[0]["exports"]],
                              "names": [s["name"] for s in syms][:12]})
        # Files with at least one export line are the probe targets: a file the
        # map shows with no export (an implementation .c, a barrel) has nothing to
        # ask about, and counting it would score the map's shape, not the LSP.
        for entry in [e for e in entries if e["exports"]][:3]:
            rel = entry["path"]
            if not entry["exports"]:
                for kind in ("definition", "references", "ref-site definition"):
                    out["probes"].append({"probe": kind, "file": rel, "export": None, "ok": False,
                                          "note": "no export in map"})
                continue
            name = entry["exports"][0][0]
            fsyms = lsp.doc_symbols(rel)
            sym = pick_symbol(fsyms, name)
            if not sym:
                for kind in ("definition", "references", "ref-site definition"):
                    out["probes"].append({"probe": kind, "file": rel, "export": name, "ok": False,
                                          "note": "documentSymbol has no symbol for the map export"})
                continue
            start = sym["location"]["range"]["start"]
            line0, ch = start["line"], start["character"]
            d = lsp.at("textDocument/definition", rel, line0, ch)
            d_rel = uri_rel(root, d["uri"]) if d else None
            out["probes"].append({"probe": "definition", "file": rel, "export": name,
                                  "ok": bool(d), "points_into_exporter": d_rel == rel,
                                  "target": d_rel})
            refs = lsp.at("textDocument/references", rel, line0, ch,
                          {"context": {"includeDeclaration": True}}) or []
            ref_files = sorted({uri_rel(root, r["uri"]) for r in refs})
            out["probes"].append({"probe": "references", "file": rel, "export": name,
                                  "ok": len(refs) > 0, "n": len(refs),
                                  "n_files": len(ref_files),
                                  "map_users": entry["exports"][0][1],
                                  "files": ref_files[:5]})
            # References come back file-level (line 1, col 0), not as use sites, so
            # the use site is found by scanning the referencing file for the name.
            site = None
            for r in refs:
                other_rel = uri_rel(root, r["uri"])
                if other_rel == rel:
                    continue
                try:
                    lines = (Path(root) / other_rel).read_text(errors="replace").splitlines()
                except OSError:
                    continue
                for i, text in enumerate(lines):
                    hit = re.search(r"(?<![\w])" + re.escape(name) + r"(?![\w])", text)
                    if hit:
                        site = (other_rel, i, hit.start())
                        break
                if site:
                    break
            if site:
                d2 = lsp.at("textDocument/definition", site[0], site[1], site[2])
                d2_rel = uri_rel(root, d2["uri"]) if d2 else None
                out["probes"].append({"probe": "ref-site definition", "file": rel, "export": name,
                                      "from": f"{site[0]}:{site[1] + 1}", "ok": bool(d2),
                                      "points_into_exporter": d2_rel == rel, "target": d2_rel})
            else:
                out["probes"].append({"probe": "ref-site definition", "file": rel, "export": name,
                                      "ok": False,
                                      "note": "no referencing file mentions the name textually"})
    except Exception as exc:  # noqa: BLE001
        out["error"] = repr(exc)
    finally:
        lsp.close()
    return out


def incremental(repo, entries):
    res = {}
    if not entries:
        return {"error": "empty map"}
    rel = entries[0]["path"]
    path = Path(repo) / rel
    lang = EXT_LANG.get(path.suffix)
    res["file"] = rel
    if lang not in SNIPPET:
        return {**res, "error": f"no snippet for {path.suffix}"}
    original = path.read_bytes()
    needle = "titiprobeadded"

    def has(syms):
        return any(needle in s["name"].lower().replace("_", "") for s in syms)

    try:
        # (a) didOpen carrying modified text, disk untouched: the live-buffer path.
        lsp = Lsp(repo)
        lsp.start()
        text = original.decode("utf-8", "replace") + SNIPPET[lang]
        t0 = time.perf_counter()
        lsp.notify("textDocument/didOpen", {"textDocument": {
            "uri": lsp.uri(rel), "languageId": lang, "version": 1, "text": text}})
        syms = lsp.doc_symbols(rel)
        res["didOpen_ms"] = (time.perf_counter() - t0) * 1000
        res["didOpen_sees_new_export"] = has(syms)
        t0 = time.perf_counter()
        lsp.notify("textDocument/didChange", {"textDocument": {"uri": lsp.uri(rel), "version": 2},
                                              "contentChanges": [{"text": original.decode("utf-8", "replace")}]})
        syms = lsp.doc_symbols(rel)
        res["didChange_revert_ms"] = (time.perf_counter() - t0) * 1000
        res["didChange_revert_drops_export"] = not has(syms)
        lsp.close()
        # (b) disk edit, cold re-index (no refresh mode in the examples/CLI).
        path.write_bytes(original + SNIPPET[lang].encode())
        lsp = Lsp(repo)
        t0 = time.perf_counter()
        lsp.start()
        res["cold_reindex_ms"] = (time.perf_counter() - t0) * 1000
        res["disk_sees_new_export"] = has(lsp.doc_symbols(rel))
        lsp.close()
        p, wall, _ = map_text(Path(repo))
        m = parse_map(p.stdout)
        ent = next((e for e in m if e["path"] == rel), None)
        res["map_after"] = {"file_in_map": ent is not None,
                            "recent_flag": bool(ent and ent["recent"]),
                            "new_export_listed": bool(ent and any(needle in n.lower().replace("_", "")
                                                                  for n, _ in ent["exports"]))}
    except Exception as exc:  # noqa: BLE001
        res["error"] = repr(exc)
    finally:
        path.write_bytes(original)
    return res


# ---- scoring / flags -------------------------------------------------------

def score(name, entries):
    truth = TRUTH.get(name)
    if not truth:
        return None
    t = set(truth["files"])
    top10 = [e["path"] for e in entries[:10]]
    top24 = [e["path"] for e in entries[:24]]
    hit10 = [p for p in top10 if p in t]
    hit24 = [p for p in top24 if p in t]
    return {"source": truth["source"], "truth": truth["files"],
            "p_at_10": len(hit10) / 10 if top10 else 0,
            "p_at_10_denominator": 10,
            "recall_at_24": len(hit24) / len(t),
            "hits10": hit10, "hits24": hit24,
            "missed": [p for p in truth["files"] if p not in top24]}


def flags(entries, indexed_files, clone_date):
    f = []
    seen = Counter(e["path"] for e in entries)
    for p, c in seen.items():
        if c > 1:
            f.append(f"duplicated path ({c}x): {p}")
    for i, e in enumerate(entries):
        rank = i + 1
        if rank <= 10 and NOISE_PATH.search(e["path"]):
            f.append(f"#{rank} looks like test/fixture/vendored/generated/example/doc: {e['raw']}")
        if rank <= 10 and e["deps"] == 0:
            f.append(f"#{rank} has (→0): {e['raw']}")
        if not e["exports"]:
            f.append(f"#{rank} shows 0 exports: {e['raw']}")
        gen = [n for n, _ in e["exports"] if n in GENERIC]
        if gen:
            f.append(f"#{rank} generic export name(s) {gen}: {e['path']}")
        if e["recent"]:
            f.append(f"#{rank} [RECENT] (last commit {clone_date}): {e['raw']}")
        if e.get("unparsed"):
            f.append(f"#{rank} unparsed map line: {e['raw']!r}")
    return f


# ---- driver ----------------------------------------------------------------

def git(repo, *args):
    r = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True)
    return r.stdout.strip()


def eval_repo(repo):
    repo = Path(repo)
    name = repo.name
    r = {"name": name, "sha": git(repo, "rev-parse", "HEAD"),
         "commit_date": git(repo, "log", "-1", "--format=%cI"),
         "tracked_files": len(git(repo, "ls-files").splitlines())}
    langs = Counter(EXT_LANG[Path(f).suffix] for f in git(repo, "ls-files").splitlines()
                    if Path(f).suffix in EXT_LANG)
    r["languages"] = dict(langs.most_common(3))
    runs = []
    for _ in range(3):
        p, wall, rss = map_text(repo)
        runs.append((p, wall, rss))
    p = runs[0][0]
    r["map_exit"] = p.returncode
    r["map_stderr_first"] = next((l for l in p.stderr.splitlines() if "files," in l), "")
    m = re.search(r"(\d+) files, (\d+) edges", p.stderr)
    r["indexed_files"], r["edges"] = (int(m[1]), int(m[2])) if m else (None, None)
    r["index_ms_min"] = min(x[1] for x in runs)
    r["index_ms_runs"] = [round(x[1], 1) for x in runs]
    r["rss_mb"] = max(x[2] for x in runs if x[2] is not None)
    r["map_text"] = p.stdout
    entries = parse_map(p.stdout)
    r["entries"] = entries
    cp, cwall, _ = run_time([str(TITI_BIN), "atlas", "check"], repo)
    lines = [l for l in cp.stdout.splitlines() if l.strip()]
    codes = Counter()
    for l in lines:
        m = re.match(r"^.+?:\d+: ([\w.-]+):", l)
        if m:
            codes[m[1]] += 1
    r["check"] = {"exit": cp.returncode, "diagnostics": sum(codes.values()),
                  "by_code": dict(codes), "ms": round(cwall, 1),
                  "stdout_first": lines[:3], "stderr": cp.stderr.strip()[:200] if cp.returncode > 1 else ""}
    r["lsp"] = lsp_probes(repo, entries)
    r["incremental"] = incremental(repo, entries)
    r["score"] = score(name, entries)
    for f in TRUTH.get(name, {}).get("files", []):
        if not (repo / f).exists():
            print(f"warning: {name}: ground-truth file missing: {f}", file=sys.stderr)
    r["flags"] = flags(entries, r["indexed_files"], r["commit_date"])
    return r


def render_markdown(results):
    """Corpus table + per-repo top-10 and flags, verbatim from the map."""
    out = ["| repo | lang | SHA | tracked / indexed files | index ms | RSS MB | check exit | diagnostics (capped at 32) | p@10 | recall@24 | LSP ok/total | incremental |",
           "|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for r in results:
        pr = r["lsp"]["probes"]
        ok = sum(1 for x in pr if x["ok"])
        sc = r["score"]
        inc = r["incremental"]
        incs = ("didOpen " + ("sees" if inc.get("didOpen_sees_new_export") else "MISSES")
                + f" ({inc.get('didOpen_ms', 0):.0f} ms); cold reindex "
                + ("sees" if inc.get("disk_sees_new_export") else "MISSES")
                + f" ({inc.get('cold_reindex_ms', 0):.0f} ms)") if "error" not in inc else "error: " + inc["error"]
        lang = "/".join(r["languages"]) or "?"
        codes = ", ".join(f"{k} {v}" for k, v in r["check"]["by_code"].items()) or "none"
        out.append(f"| {r['name']} | {lang} | `{r['sha'][:12]}` | {r['tracked_files']} / {r['indexed_files']} "
                   f"| {r['index_ms_min']:.0f} | {r['rss_mb']:.0f} | {r['check']['exit']} | {codes} "
                   f"| {sc['p_at_10']:.1f} | {sc['recall_at_24']:.2f} ({len(sc['hits24'])}/{len(sc['truth'])}) "
                   f"| {ok}/{len(pr)} | {incs} |")
    for r in results:
        out += ["", f"### {r['name']}", "",
                f"Last commit {r['commit_date']}; {r['indexed_files']} files, {r['edges']} edges. "
                f"Ground truth ({r['score']['source']}): " + ", ".join(f"`{f}`" for f in r['score']['truth']),
                f"Truth hits in top 24: {r['score']['hits24']}; missed: {r['score']['missed']}", "",
                "Top 10 map entries:", "", "```"]
        for e in r["entries"][:10]:
            out.append(e["raw"])
            out += e.get("raw_exports", [])
        out += ["```", "", "Flagged:", ""]
        out += [f"- {f}" for f in r["flags"]] or ["- none"]
        bad = [x for x in r["lsp"]["probes"] if not x["ok"] or x.get("points_into_exporter") is False]
        out += ["", "LSP probes failing or not pointing into the exporting file:", ""]
        out += [f"- `{json.dumps({k: v for k, v in x.items() if k not in ('names', 'files', 'exports_in_map')})}`"
                for x in bad] or ["- none"]
        out += ["", f"Incremental: `{json.dumps(r['incremental'])}`"]
    return "\n".join(out) + "\n"


# ---- task-level eval: replay real commits ------------------------------------

# Conditions rank the files of the index for one task. Each takes
# (worktree, task_text, dump) and returns an ordered list of paths, or None when
# the condition is not available on this machine. `static` is what the prompt
# map shows today: the real `titi atlas dump` order, no task input.
#
# HOOK for the atlas tool (v2 step 3+): register a condition here. The
# `tool` condition below runs the command in $ATLAS_TOOL_CMD as
# `$ATLAS_TOOL_CMD <worktree> <task-file>` and reads a JSON list of paths from
# its stdout; unset, it is skipped and absent from the report.
def condition_static(worktree, task, dump):
    return [f["path"] for f in dump["files"]]


def condition_tool(worktree, task, dump):
    cmd = os.environ.get("ATLAS_TOOL_CMD")
    if not cmd:
        return None
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as fh:
        fh.write(task)
    try:
        p = subprocess.run([*cmd.split(), worktree, fh.name], capture_output=True, text=True, timeout=120)
    finally:
        os.unlink(fh.name)
    if p.returncode:
        raise RuntimeError(f"ATLAS_TOOL_CMD failed: {p.stderr[:200]}")
    return list(json.loads(p.stdout))


CONDITIONS = {"static": condition_static, "tool": condition_tool}
SRC_EXT = {".rs", ".go", ".py", ".ts", ".tsx", ".js", ".mjs", ".cjs", ".jsx", ".java", ".kt", ".kts", ".c",
           ".h", ".cpp", ".cc", ".hpp", ".cs", ".swift", ".php", ".rb"}


def git(repo, *args, check=True):
    p = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True)
    if check and p.returncode:
        raise RuntimeError(p.stderr[:200])
    return p.stdout


def atlas_dump(worktree, limit=N):
    p = subprocess.run([str(TITI_BIN), "atlas", "dump", "--json", "--limit", str(limit)], cwd=worktree,
                       capture_output=True, text=True)
    if p.returncode:
        raise RuntimeError(f"atlas dump failed: {p.stderr[:200]}")
    return json.loads(p.stdout)


def replay_repo(repo, commits, max_files, min_msg, max_changed, scan, conditions):
    """Tasks of one repo: recent non-merge commits, the index built at the PARENT tree."""
    repo = Path(repo)
    head = git(repo, "rev-parse", "HEAD").strip()
    revs = git(repo, "rev-list", "--no-merges", "-n", str(scan), "HEAD").split()
    tmp = tempfile.mkdtemp(prefix="atlas-replay-")
    git(repo, "worktree", "add", "--detach", tmp, head)
    tasks = []
    try:
        for rev in revs:
            if len(tasks) >= commits:
                break
            parents = git(repo, "rev-list", "--parents", "-n", "1", rev).split()[1:]
            if len(parents) != 1:
                continue  # root or merge
            parent = parents[0]
            msg = git(repo, "log", "-1", "--format=%s%n%b", rev).strip()
            status = git(repo, "diff", "--name-status", "-M", parent, rev).splitlines()
            if len(msg) < min_msg or not status or len(status) > max_changed:
                continue  # huge commits are refactors, not tasks
            modified = [l.split("\t")[-1] for l in status if l.startswith("M")
                        and os.path.splitext(l.split("\t")[-1])[1] in SRC_EXT]
            if not 1 <= len(modified) <= max_files:
                continue
            try:
                git(tmp, "checkout", "-q", "--detach", parent)
                dump = atlas_dump(tmp)
            except RuntimeError:
                continue
            indexed = {f["path"] for f in dump["files"]}
            truth = [f for f in modified if f in indexed]
            if not truth:
                continue  # nothing the map could name (ignored or unsupported files)
            names = {os.path.basename(f).rsplit(".", 1)[0].lower() for f in truth}
            task = {"repo": repo.name, "rev": rev[:10], "parent": parent[:10], "msg": msg.split("\n")[0][:80],
                    "truth": truth, "indexed_files": len(indexed), "map_tokens": len(dump["map"]["text"]) / 4,
                    "leaky": any(n in msg.lower() for n in names if len(n) >= 4), "conditions": {}}
            for name in conditions:
                ranked = CONDITIONS[name](tmp, msg, dump)
                if ranked is None:
                    continue
                task["conditions"][name] = {
                    "r10": len(set(truth) & set(ranked[:10])) / len(truth),
                    "r24": len(set(truth) & set(ranked[:24])) / len(truth),
                }
            tasks.append(task)
    finally:
        git(repo, "worktree", "remove", "--force", tmp, check=False)
    return tasks


def render_replay(tasks, conditions):
    by = {}
    for t in tasks:
        by.setdefault(t["repo"], []).append(t)
    names = [c for c in conditions if any(c in t["conditions"] for t in tasks)]

    def mean(rows, f):
        v = [f(r) for r in rows]
        return sum(v) / len(v) if v else float("nan")

    head = "| repo | tasks | files | " + " | ".join(f"{c} r@10 | {c} r@24" for c in names) + " | map tokens | leaky |"
    out = [head, "|" + "---|" * (head.count("|") - 1)]
    for repo, rows in list(by.items()) + [("**ALL**", tasks)]:
        cells = " | ".join(
            f"{mean([r for r in rows if c in r['conditions']], lambda r, c=c, k=k: r['conditions'][c][k]):.2f}"
            for c in names for k in ("r10", "r24"))
        out.append(f"| {repo} | {len(rows)} | {mean(rows, lambda r: r['indexed_files']):.0f} | {cells} "
                   f"| {mean(rows, lambda r: r['map_tokens']):.0f} | {sum(r['leaky'] for r in rows)} |")
    return "\n".join(out) + "\n"


def replay_main(a):
    corpus = Path(a.corpus)
    only = set(filter(None, a.only.split(",")))
    default = ["cobra", "express", "requests", "ripgrep", "ky", "gson", "okio", "sinatra"]
    repos = [corpus / n for n in (sorted(only) if only else default)]
    conditions = [c for c in a.conditions.split(",") if c]
    tasks = []
    for repo in repos:
        print(f"== replay {repo.name}", file=sys.stderr, flush=True)
        tasks += replay_repo(repo, a.commits, a.max_files, a.min_msg, a.max_changed, a.scan, conditions)
    table = render_replay(tasks, conditions)
    out = Path(a.out) if a.out else corpus / "commit_replay.json"
    meta = {"atlas": git(Path(__file__).resolve().parent, "rev-parse", "HEAD", check=False).strip(),
            "repos": {r.name: git(r, "rev-parse", "HEAD").strip() for r in repos},
            "config": {k: getattr(a, k) for k in ("commits", "max_files", "min_msg", "max_changed", "scan")}}
    out.write_text(json.dumps({"meta": meta, "tasks": tasks}, indent=1))
    if a.md:
        Path(a.md).write_text(table)
    print(table)
    print(f"wrote {out}", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", default="/tmp/titi-qa/genome-corpus")
    ap.add_argument("--only", default="")
    ap.add_argument("--out", default=None)
    ap.add_argument("--md", default=None, help="also write the markdown tables here")
    ap.add_argument("--mode", choices=("survey", "commits"), default="survey",
                    help="survey: map/check/LSP over each repo; commits: replay real commits as tasks")
    ap.add_argument("--commits", type=int, default=10, help="commits mode: tasks per repo")
    ap.add_argument("--max-files", type=int, default=8, help="commits mode: modified indexed source files, at most")
    ap.add_argument("--min-msg", type=int, default=20, help="commits mode: message length, at least")
    ap.add_argument("--max-changed", type=int, default=12, help="commits mode: skip commits touching more files")
    ap.add_argument("--scan", type=int, default=400, help="commits mode: recent non-merge commits to look through")
    ap.add_argument("--conditions", default="static,tool", help="commits mode: rankings to score (tool needs ATLAS_TOOL_CMD)")
    ap.add_argument("--self-test", action="store_true", help="check the map parser on old and new map text, then exit")
    a = ap.parse_args()
    if a.self_test:
        return self_test_parse_map()
    if a.mode == "commits":
        if not TITI_BIN.exists():
            sys.exit(f"missing {TITI_BIN}: cargo build --release -p titi-cli --locked")
        return replay_main(a)
    for b in (MAP_BIN, TITI_BIN):
        if not b.exists():
            sys.exit(f"missing {b}: cargo build --release -p titi-cli -p titi-atlas --examples --locked")
    corpus = Path(a.corpus)
    only = set(filter(None, a.only.split(",")))
    repos = sorted(d for d in corpus.iterdir() if (d / ".git").exists() and (not only or d.name in only))
    results = []
    for repo in repos:
        print(f"== {repo.name}", file=sys.stderr, flush=True)
        results.append(eval_repo(repo))
    out = Path(a.out) if a.out else corpus / "report.json"
    out.write_text(json.dumps(results, indent=1))
    if a.md:
        Path(a.md).write_text(render_markdown(results))
    print(f"wrote {out}", file=sys.stderr)


if __name__ == "__main__":
    main()
