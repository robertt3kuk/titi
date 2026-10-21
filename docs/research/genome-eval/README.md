# Genome evaluation over a multi-language corpus

Harness: `scripts/genome_eval.py`. Raw results: `/tmp/titi-qa/genome-corpus/report.json`
(not in the repo). Built from master `f78d934` (`cargo build --release -p titi-cli -p titi-genome --examples --locked`).
Run on 2026-10-21, macOS arm64.

## Method

- **Corpus**: shallow clones (`--depth 1`) under `/tmp/titi-qa/genome-corpus/`, SHAs in the table
  (`git clone --depth 1 https://github.com/<owner>/<repo>`; checkout the SHA to reproduce). One repo per
  language except C (kilo, hiredis). titi itself is a clone of `/Users/homie/proj/titi` at master `f78d934`.
  nlohmann/json is a *header-only* library: the real library is `single_include/nlohmann/json.hpp`
  (1.59 MB, over the 1 MB `MAX_FILE_BYTES` cap so the index skips it) and the multi-header
  `include/nlohmann/`; the repo is 1441 tracked files, most of them tests and docs.
- **Map**: `examples/map <repo> 24`; entries parsed from `path (→N) [RECENT]` and `  +Export (users)` lines.
  The map only prints exports that have at least one resolved user, so "shows 0 exports" means "no used
  export", not "no symbols" (e.g. `requests/api.py` has 8 symbols per documentSymbol and shows none).
- **Cost**: `/usr/bin/time -l` around the map example (cold index + projection), min wall of 3 runs,
  max RSS. There is no standalone "index" timer, so this is index+render; the render is negligible.
- **check**: `titi genome check` run with cwd = repo root. The command truncates its output at
  `CHECK_LIMIT = 32` (`query.rs`), so diagnostic counts are capped at 32 and every line says `:1:`.
- **LSP**: `titi genome lsp` over stdio, Content-Length framed; `initialize`, `initialized`, then probes
  (no didOpen is needed for queries; the server reads from disk). Probes: documentSymbol on
  the top-ranked file; for each of the top 3 files *that show an export line* (rule changed in the fix
  round; the baseline counted the top 3 files even with none), taking the first map export: `definition` at its
  documentSymbol position, `references`, and `definition` from a use site (first textual occurrence of the
  name in the first referencing file; `references` returns only file-level `1:0` locations, so the use
  site has to be found by scanning). "ok" = non-empty result. "Points into exporter" = the definition
  result is the file the map lists the export under. A file with no export in the map gives three
  failed probes.
- **Incremental**: the examples/CLI have no append-and-refresh mode (`examples/refresh` only touches an
  unchanged file). So: (a) LSP `didOpen` with the file text + an appended exported function, `documentSymbol`
  sees it, then `didChange` back to the original drops it; (b) the same function written to disk, a fresh
  LSP is started (= cold re-index) and `documentSymbol` must show it; file is restored afterwards.
  The map after the disk edit marks the file `[RECENT]` and does not list the new function (only the
  top 4 used exports are shown, and the new one has no users), which is expected.
- **Ground truth**: 5-8 central files per repo, chosen from each repo's README/docs (source named per repo
  below; paths verified to exist). p@10 = hits in top 10 / 10; recall@24 = hits in top 24 / |truth|.
  For kilo (single file) p@10 is capped at 0.1. The truth list is a judgement; header-only/C repos
  where the central files are `.c` implementations with no exports are structurally disadvantaged.
- **Flags**: test/example/doc/fixture/vendored path in the top 10, a generic export name, `(→0)` in the
  top 10, a line with 0 exports, duplicated paths, `[RECENT]` (clone's last commit date is printed per
  repo; okio, nlohmann/json and titi had commits within 48 h of the run so `[RECENT]` on files of that
  commit is *correct* for them; every other clone is older than 48 h and shows none).

## Fix round (lang/ only)

Commits on `wt/genome-harness`, one per language, each with a test that failed before:

1. JavaScript CommonJS exports (`exports.x`, `module.exports = …`, aliases) and a root `require('../..')`.
2. C++ test macros, SCREAMING_CASE names, keywords and function-body locals no longer exports.
3. Swift: an `extension` does not export the extended type.
4. Python: `from .x import a`, `__all__`, `import x as y` in `__init__.py`/alias modules publish names.
5. Kotlin: `actual` declarations are not exports (`expect` is the definition).
6. C/C++: a header and its same-stem source import each other.
7. Rust: same-file `mod x; pub use x::Y` and `self::Enum::*`/`self::imp::*`/`super::Token::*` globs.
Also: a test pinning TypeScript barrel exports/re-exports (already worked).

**Open (not done):** TS `./x.js`→`x.ts` and `..` directory resolution (moved to `reresolve.rs`
by the other worker); ambiguous-symbol must skip a definer that re-exports the other (Python facade:
requests 1→15 until it does); ranking/graph items (entry points, tests outranking library code,
`[RECENT]` blanket, check cap of 32 and `:1:` lines); Go/Java/C#/PHP/Ruby module-name export noise
(sinatra `Base`/`Test`); TS barrel `export *` names; C++ real declarations inside derailed parses.

## Before → after

| repo | p@10 before→after | recall@24 before→after | LSP ok before→after (rule changed) | check exit before→after | diagnostics before→after |
|---|---|---|---|---|---|
| Alamofire | 0.3→0.6 | 0.75→1.00 | 10/10→10/10 | 1→1 | {'syntax-error': 6, 'ambiguous-symbol': 26}→{'syntax-error': 6, 'ambiguous-symbol': 18} |
| AutoMapper | 0.3→0.3 | 0.83→0.83 | 9/10→9/10 | 1→1 | {'syntax-error': 2, 'unresolved-import': 30}→{'syntax-error': 2, 'unresolved-import': 30} |
| cobra | 0.6→0.6 | 1.00→1.00 | 10/10→10/10 | 1→1 | {'unresolved-import': 12, 'ambiguous-symbol': 2}→{'unresolved-import': 12, 'ambiguous-symbol': 2} |
| express | 0.2→0.7 | 0.86→1.00 | 0/10→10/10 | 1→0 | {'unresolved-import': 32}→{} |
| gson | 0.6→0.6 | 0.86→0.86 | 8/10→8/10 | 1→1 | {'unresolved-import': 32}→{'unresolved-import': 32} |
| guzzle | 0.2→0.2 | 0.38→0.38 | 10/10→10/10 | 1→1 | {'unresolved-import': 32}→{'unresolved-import': 32} |
| hiredis | 0.4→0.5 | 0.44→1.00 | 10/10→10/10 | 1→1 | {'syntax-error': 12, 'unresolved-import': 3}→{'syntax-error': 12, 'unresolved-import': 3} |
| json | 0.0→0.2 | 0.29→0.29 | 10/10→10/10 | 1→1 | {'syntax-error': 32}→{'syntax-error': 32} |
| kilo | 0.1→0.1 | 1.00→1.00 | 1/4→1/1 | 0→0 | {}→{} |
| ky | 0.1→0.1 | 0.60→0.60 | 7/10→10/10 | 1→1 | {'unresolved-import': 32}→{'unresolved-import': 32} |
| okio | 0.4→0.6 | 0.75→1.00 | 9/10→9/10 | 1→1 | {'syntax-error': 5, 'unresolved-import': 27}→{'syntax-error': 5, 'unresolved-import': 27} |
| requests | 0.3→0.3 | 1.00→1.00 | 6/10→10/10 | 1→1 | {'unresolved-import': 2, 'ambiguous-symbol': 1}→{'unresolved-import': 2, 'ambiguous-symbol': 15} |
| ripgrep | 0.4→0.4 | 0.50→0.50 | 10/10→10/10 | 1→1 | {'unresolved-import': 30, 'ambiguous-symbol': 1}→{'unresolved-import': 20, 'ambiguous-symbol': 1} |
| sinatra | 0.5→0.5 | 1.00→1.00 | 9/10→9/10 | 0→0 | {'ambiguous-symbol': 15}→{'ambiguous-symbol': 15} |
| titi | 0.2→0.2 | 0.25→0.25 | 10/10→10/10 | 1→1 | {'unresolved-import': 1, 'ambiguous-symbol': 8}→{'unresolved-import': 1, 'ambiguous-symbol': 8} |

## Corpus table (after)

| repo | lang | SHA | tracked / indexed files | index ms | RSS MB | check exit | diagnostics (capped at 32) | p@10 | recall@24 | LSP ok/total | incremental |
|---|---|---|---|---|---|---|---|---|---|---|---|
| Alamofire | swift/javascript | `bda9ed57d729` | 571 / 111 | 253 | 34 | 1 | syntax-error 6, ambiguous-symbol 18 | 0.6 | 1.00 (8/8) | 10/10 | didOpen sees (10 ms); cold reindex sees (412 ms) |
| AutoMapper | csharp/python | `3019d86b3e2f` | 591 / 514 | 349 | 29 | 1 | syntax-error 2, unresolved-import 30 | 0.3 | 0.83 (5/6) | 9/10 | didOpen sees (12 ms); cold reindex sees (435 ms) |
| cobra | go | `adbc8813901b` | 66 / 36 | 212 | 13 | 1 | unresolved-import 12, ambiguous-symbol 2 | 0.6 | 1.00 (6/6) | 10/10 | didOpen sees (13 ms); cold reindex sees (193 ms) |
| express | javascript | `0dee2c52c7c8` | 214 / 141 | 168 | 13 | 0 | none | 0.7 | 1.00 (7/7) | 10/10 | didOpen sees (2 ms); cold reindex sees (288 ms) |
| gson | java | `e2292f9c9f2e` | 313 / 264 | 389 | 20 | 1 | unresolved-import 32 | 0.6 | 0.86 (6/7) | 8/10 | didOpen sees (26 ms); cold reindex sees (488 ms) |
| guzzle | php | `b920e5fcaf81` | 184 / 145 | 292 | 60 | 1 | unresolved-import 32 | 0.2 | 0.38 (3/8) | 10/10 | didOpen sees (4 ms); cold reindex sees (397 ms) |
| hiredis | c/cpp | `5bb7f030f663` | 77 / 53 | 223 | 18 | 1 | syntax-error 12, unresolved-import 3 | 0.5 | 1.00 (9/9) | 10/10 | didOpen sees (5 ms); cold reindex sees (276 ms) |
| json | cpp/python/swift | `5e13f834c16d` | 1441 / 604 | 936 | 65 | 1 | syntax-error 32 | 0.2 | 0.29 (2/7) | 10/10 | didOpen sees (185 ms); cold reindex sees (901 ms) |
| kilo | c | `323d93b29bd8` | 6 / 1 | 92 | 6 | 0 | none | 0.1 | 1.00 (1/1) | 1/1 | didOpen sees (8 ms); cold reindex sees (98 ms) |
| ky | typescript | `3541888878c4` | 115 / 98 | 118 | 26 | 1 | unresolved-import 32 | 0.1 | 0.60 (3/5) | 10/10 | didOpen sees (1 ms); cold reindex sees (130 ms) |
| okio | kotlin/java/c | `3f58c6bc968d` | 439 / 357 | 792 | 26 | 1 | syntax-error 5, unresolved-import 27 | 0.6 | 1.00 (8/8) | 9/10 | didOpen sees (15 ms); cold reindex sees (1054 ms) |
| requests | python | `611c6162cbc4` | 130 / 37 | 248 | 12 | 1 | unresolved-import 2, ambiguous-symbol 15 | 0.3 | 1.00 (7/7) | 10/10 | didOpen sees (3 ms); cold reindex sees (290 ms) |
| ripgrep | rust/ruby | `3fce3b5bb023` | 237 / 111 | 181 | 32 | 1 | unresolved-import 20, ambiguous-symbol 1 | 0.4 | 0.50 (4/8) | 10/10 | didOpen sees (7 ms); cold reindex sees (162 ms) |
| sinatra | ruby | `cb22afd7902b` | 292 / 147 | 198 | 17 | 0 | ambiguous-symbol 15 | 0.5 | 1.00 (5/5) | 9/10 | didOpen sees (2 ms); cold reindex sees (174 ms) |
| titi | rust/python/javascript | `f78d93438ad8` | 579 / 351 | 560 | 68 | 1 | unresolved-import 1, ambiguous-symbol 8 | 0.2 | 0.25 (2/8) | 10/10 | didOpen sees (9 ms); cold reindex sees (667 ms) |


## Problems found in the baseline run, ranked (see the fix round for which are addressed)

1. **JS CommonJS exports are invisible** (express, 0/10 LSP probes): `exports.x = ...` / `module.exports`
   yield no symbols; every one of express's 24 map lines has no export lines, `lib/application.js`,
   `lib/request.js`, `lib/response.js`, `lib/express.js` look empty, and documentSymbol is `[]`.
2. **Tests/examples outrank the library in half the corpus**: express #1 `test/support/utils.js`, #3-6, #8-10
   are `examples/`/`test/`; nlohmann/json top 10 is all tests + `tests/abi/.../json_v3_10_5.hpp`;
   AutoMapper has 4 `src/UnitTests/...` files in the top 10 (`AutoMapperSpecBase.cs` is #2); cobra
   `completions_test.go` #6; guzzle `tests/UnvalidatedUri*.php` #9-10; ky `test/helpers/...` #3; sinatra
   `test/test_helper.rb` #3; ripgrep `tests/hay.rs`, `tests/util.rs` in 11-15.
3. **C++ test macros parsed as exported functions**: `SECTION (103)`, `TEST_CASE_TEMPLATE (12)`,
   `CHECK_THROWS_AS (28)`, `NLOHMANN_DEFINE_TYPE_NON_INTRUSIVE`, `noexcept`, `JSON_INTERNAL_CATCH`,
   `JSON_CATCH`, `array` appear as exports with big user counts, so test files get `→111`.
4. **The real nlohmann library is missing**: `single_include/nlohmann/json.hpp` (>1 MB) is skipped without
   a word; `include/nlohmann/json.hpp` is #21; 0 of 10 top entries are library code; 32+ `syntax-error`s
   on the macro-heavy headers.
5. **Swift: framework/stdlib types listed as exports**: `+NSNumber`, `+Array`, `+JSONDecoder`, `+SecCertificate`,
   `+URLComponents` come from `extension X` blocks and read as Alamofire's own types; also
   `+completion (16)` (a parameter label). 4 Alamofire `Tests/` files in 11-20.
6. **Same-named test/helper types pooled into huge user counts**: AutoMapper `+MapFrom (107)`,
   `+Include (42)`, `+Instance (23)`, `+Field (12)` are test-local classes/members whose names collide
   with the library's API names, so `(→108)` on `Bug/InterfaceMultipleInheritance.cs` (#7!).
7. **`unresolved-import` for imports that resolve**: Rust `self::Enum::*` / `self::imp::*` (ripgrep, 30);
   JS `../..` and `../../` directory imports (express, 32); TypeScript `../errors/HTTPError.js`
   -> `.ts` (ky, 32). `genome check` therefore exits 1 on 13 of 15 repos, and the top-line fact the model
   sees (`unresolved`) is wrong for the common ESM/Rust cases. External packages (`github.com/spf13/pflag`,
   `com.google.common.truth`) are also reported as unresolved, indistinguishable from real breakage.
8. **Ranking ignores the entry point / facade**: ripgrep `crates/core/main.rs` (not even indexed as having
   symbols: private `fn main`), `crates/regex/src/matcher.rs`, `crates/printer/src/lib.rs` not in top 24
   (recall 0.50), #1 is `printer/hyperlink/mod.rs`; titi's own `titi-cli/src/main.rs`, `titi-engine/src/lib.rs`
   are missing (recall 0.25); guzzle `HandlerStack`, `Middleware`, `Pool` missing, but `InvalidArgumentException`
   is #1 (recall 0.38); Python `requests/api.py` (the public entry) is #21, `__init__.py` shows no exports.
9. **C: headers outrank implementation, 0-export headers fill the map**: hiredis `.c` files absent from the
   top 10 (recall 0.44), `read.h`, `fmacros.h`, `win32.h`, `sdsalloc.h` rank high with no export lines;
   `examples/example-qt.h` (#8) outranks `net.h`; `adapters/ae.h`/`glib.h`/`ivykis.h` are `(→0)` in the
   map (#22-24). kilo: the only file shows `(→0)` and no exports (all `static`), 3 of 4 LSP probes impossible.
10. **Python re-export/alias modules show nothing**: `requests/compat.py` is #1 `(→13)` with 0 symbols (all
    `from x import y` aliases), `__init__.py` shows 0 exports, `__version__.py` is #5 with none.
11. **`check` output is capped at 32 and has no line numbers**: every diagnostic is `path:1:`; real counts
    are unknown (counts in the table are lower bounds).
12. **Kotlin expect/actual: one export per source set**: okio lists `Buffer`, `SegmentPool`, `Timeout`, `Sink`,
    `ByteString` once per source set (`commonMain` and `jvmMain`), definition from a use site lands on the
    `jvmMain` one; `IOException`/`Closeable` typealiases appear as exports of two files with different counts;
    okio `samples/.../GoldenValue.kt` `+assertEquals (72)` is #7.
13. **Generic / low-signal export names**: cobra `+Name (25)`, `+Type`, `+Set`, `+GetSlice` (test file
    #6); gson `+value`, `+hashCode`, `+set`; guzzle `+create`, `+execute`; requests `+next`, `+setup`;
    ripgrep `+name`, `+index`, `+description`, `+from_iter`; titi `+store`, `+pending`, `+shared`, `+api_key`;
    sinatra `+Base (13)`, `+Test (39)`, `+Rack`, `+Protection`: a module name as export, merged across
    unrelated files (`Base` in `lib/sinatra/base.rb` and `rack-protection/lib/rack/protection/base.rb`).
14. **ky `source/types/ky.ts` #1 with 0 exports and `source/index.ts` (the entry) absent**: re-exports
    (`export * from`, `export { default }`) are not counted as exports; a barrel's value is its edges,
    but the map hides it.
15. **`[RECENT]` on a shallow clone's last commit is correct but blankets the map**: every ranked okio and
    nlohmann/json file is `[RECENT]` (a depth-1 clone's single commit touches everything), so on a fresh
    clone the marker carries no signal (titi master: 15 of 24).

Other facts: `definition` from a use site returns null or a different definer when more than one file defines
the name (`query.rs` returns `None` unless exactly one file defines it: AutoMapper `Mapper`, gson `Gson`/`JsonReader`,
okio `SegmentPool` returned null; sinatra `Base`/`Rack`/`Test` landed on the other definer), and `references`
returns file-level `1:0` locations rather than use sites. Incremental via LSP `didOpen`/`didChange` works
for all 15 repos (1-30 ms); cold re-index was 57-980 ms, RSS 6-62 MB. No duplicated paths, no crash, no `(→0)`
in the top 10 except kilo.

## Per-repo results (verbatim, after the fixes)

### Alamofire

Last commit 2026-09-09T21:24:49-04:00; 111 files, 558 edges. Ground truth (README: Session, Request, DataRequest, Response serialization, Interceptor): `Source/Core/Session.swift`, `Source/Core/Request.swift`, `Source/Core/DataRequest.swift`, `Source/Core/AFError.swift`, `Source/Features/RequestInterceptor.swift`, `Source/Features/ResponseSerialization.swift`, `Source/Alamofire.swift`, `Source/Core/Protected.swift`
Truth hits in top 24: ['Source/Core/AFError.swift', 'Source/Core/Session.swift', 'Source/Core/Request.swift', 'Source/Alamofire.swift', 'Source/Features/ResponseSerialization.swift', 'Source/Features/RequestInterceptor.swift', 'Source/Core/Protected.swift', 'Source/Core/DataRequest.swift']; missed: []

Top 10 map entries:

```
Source/Core/ParameterEncoding.swift (→37)
  +Parameters (33)
  +URLEncoding (5)
  +ParameterEncoding (4)
  +JSONEncoding (2)
Source/Core/AFError.swift (→47)
  +AFError (47)
  +MultipartEncodingFailureReason (2)
  +ResponseValidationFailureReason (2)
  +UnexpectedInputStreamLength (2)
Source/Core/Session.swift (→40)
  +Session (39)
  +Upload (3)
  +RequestConvertible (1)
  +RequestModifier (1)
Source/Core/HTTPMethod.swift (→17)
  +HTTPMethod (17)
Source/Core/Request.swift (→37)
  +Request (37)
  +RequestDelegate (6)
  +ResponseDisposition (5)
  +cURLDescription (1)
Source/Alamofire.swift (→44)
  +AFInfo (2)
Source/Core/URLConvertible+URLRequestConvertible.swift (→14)
  +URLRequestConvertible (14)
  +URLConvertible (3)
Source/Features/ResponseSerialization.swift (→11)
  +DataDecoder (7)
  +DataPreprocessor (6)
  +PassthroughPreprocessor (6)
  +ResponseSerializer (6)
Source/Features/RequestInterceptor.swift (→20)
  +RequestInterceptor (16)
  +RetryResult (9)
  +RequestAdapter (8)
  +RequestRetrier (5)
Source/Core/DataStreamRequest.swift (→29)
  +completion (16)
  +DataStreamRequest (15)
  +DataStreamSerializer (4)
  +DecodableStreamSerializer (4)
```

Flagged:

- none

LSP probes failing or not pointing into the exporting file:

- none

Incremental: `{"file": "Source/Core/ParameterEncoding.swift", "didOpen_ms": 9.905209000000248, "didOpen_sees_new_export": true, "didChange_revert_ms": 9.769958000000134, "didChange_revert_drops_export": true, "cold_reindex_ms": 411.5832499999996, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### AutoMapper

Last commit 2026-10-16T15:08:34-05:00; 514 files, 2300 edges. Ground truth (docs + README: MapperConfiguration, Mapper, IMapper, Profile, TypeMap): `src/AutoMapper/Configuration/MapperConfiguration.cs`, `src/AutoMapper/Mapper.cs`, `src/AutoMapper/Configuration/Profile.cs`, `src/AutoMapper/TypeMap.cs`, `src/AutoMapper/Configuration/MapperConfigurationExpression.cs`, `src/AutoMapper/Configuration/IMappingExpression.cs`
Truth hits in top 24: ['src/AutoMapper/Configuration/MapperConfiguration.cs', 'src/AutoMapper/Mapper.cs', 'src/AutoMapper/TypeMap.cs', 'src/AutoMapper/Configuration/Profile.cs', 'src/AutoMapper/Configuration/IMappingExpression.cs']; missed: ['src/AutoMapper/Configuration/MapperConfigurationExpression.cs']

Top 10 map entries:

```
src/AutoMapper/Configuration/MapperConfiguration.cs (→304)
  +MapperConfiguration (295)
  +IConfigurationProvider (11)
  +LazyValue (1)
src/UnitTests/AutoMapperSpecBase.cs (→190)
  +AutoMapperSpecBase (169)
  +NonValidatingSpecBase (53)
src/AutoMapper/Mapper.cs (→168)
  +Mapper (154)
  +IMapper (21)
  +IInternalRuntimeMapper (1)
  +IMapperBase (1)
src/AutoMapper/MemberMap.cs (→46)
  +MemberMap (40)
  +ValueTransformerConfiguration (10)
  +ValueTransformerConfigurationExtensions (1)
src/AutoMapper/TypeMap.cs (→31)
  +TypeMap (31)
src/UnitTests/MappingInheritance/PreserveReferencesWithInheritance.cs (→35)
  +Instance (23)
  +Field (12)
src/UnitTests/Bug/InterfaceMultipleInheritance.cs (→108)
  +MapFrom (107)
  +MapTo (1)
src/AutoMapper/Internal/InternalApi.cs (→47)
  +IGlobalConfiguration (45)
  +IGlobalConfigurationExpression (4)
  +IProfileExpressionInternal (2)
src/AutoMapper/Internal/TypePair.cs (→48)
  +TypePair (48)
  +MapRequest (3)
src/UnitTests/MappingInheritance/IncludeBaseBug.cs (→42)
  +Include (42)
  +Concrete (1)
```

Flagged:

- none

LSP probes failing or not pointing into the exporting file:

- `{"probe": "ref-site definition", "file": "src/AutoMapper/Mapper.cs", "export": "Mapper", "from": "src/AutoMapper/Configuration/MapperConfiguration.cs:17", "ok": false, "points_into_exporter": false, "target": null}`

Incremental: `{"file": "src/AutoMapper/Configuration/MapperConfiguration.cs", "didOpen_ms": 11.81079199999946, "didOpen_sees_new_export": true, "didChange_revert_ms": 16.16291599999986, "didChange_revert_drops_export": true, "cold_reindex_ms": 434.73012499999976, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### cobra

Last commit 2026-07-10T20:43:07-04:00; 36 files, 112 edges. Ground truth (README (Command, Args, completions are the user-facing surface)): `command.go`, `cobra.go`, `args.go`, `completions.go`, `flag_groups.go`, `shell_completions.go`
Truth hits in top 24: ['command.go', 'completions.go', 'cobra.go', 'args.go', 'flag_groups.go', 'shell_completions.go']; missed: []

Top 10 map entries:

```
command.go (→35)
  +Command (34)
  +Name (25)
  +Commands (11)
  +AddCommand (10)
completions.go (→12)
  +ShellCompDirectiveNoFileComp (8)
  +ShellCompNoDescRequestCmd (8)
  +Completion (6)
  +ShellCompDirectiveFilterDirs (6)
cobra.go (→22)
  +WriteStringAndCheck (7)
  +CheckErr (3)
  +EnableCaseInsensitive (2)
  +EnableCommandSorting (2)
args.go (→7)
  +NoArgs (6)
  +MinimumNArgs (4)
  +ArbitraryArgs (3)
  +OnlyValidArgs (3)
bash_completions.go (→5)
  +BashCompSubdirsInDir (4)
  +BashCompFilenameExt (3)
  +BashCompOneRequiredFlag (3)
  +GenBashCompletion (2)
completions_test.go (→7)
  +Type (4)
  +Set (3)
  +GetSlice (1)
flag_groups.go (→3)
  +MarkFlagsMutuallyExclusive (2)
  +MarkFlagsOneRequired (2)
  +MarkFlagsRequiredTogether (2)
  +ValidateFlagGroups (1)
shell_completions.go (→5)
  +MarkFlagRequired (4)
  +MarkFlagFilename (3)
  +MarkPersistentFlagRequired (3)
  +MarkPersistentFlagFilename (2)
zsh_completions.go (→3)
  +GenZshCompletion (3)
  +GenZshCompletionNoDesc (2)
powershell_completions.go (→2)
  +GenPowerShellCompletion (2)
  +GenPowerShellCompletionWithDesc (1)
```

Flagged:

- #6 looks like test/fixture/vendored/generated/example/doc: completions_test.go (→7)
- #18 shows 0 exports: active_help_test.go (→0)
- #19 shows 0 exports: args_test.go (→0)
- #20 shows 0 exports: bash_completionsV2_test.go (→0)
- #21 shows 0 exports: bash_completions_test.go (→0)
- #22 shows 0 exports: cobra_test.go (→0)
- #23 shows 0 exports: command_notwin.go (→0)
- #24 shows 0 exports: command_test.go (→0)

LSP probes failing or not pointing into the exporting file:

- none

Incremental: `{"file": "command.go", "didOpen_ms": 12.696917000001307, "didOpen_sees_new_export": true, "didChange_revert_ms": 12.606166999999502, "didChange_revert_drops_export": true, "cold_reindex_ms": 193.0464590000014, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### express

Last commit 2026-10-17T11:42:03-05:00; 141 files, 242 edges. Ground truth (package.json main + Readme: index.js -> lib/express.js, application, request, response, router): `index.js`, `lib/express.js`, `lib/application.js`, `lib/request.js`, `lib/response.js`, `lib/view.js`, `lib/utils.js`
Truth hits in top 24: ['lib/express.js', 'index.js', 'lib/utils.js', 'lib/response.js', 'lib/application.js', 'lib/request.js', 'lib/view.js']; missed: []

Top 10 map entries:

```
lib/express.js (→87)
  +request (80)
  +Router (9)
  +application (1)
index.js (→96)
lib/utils.js (→7)
  +compileETag (1)
  +compileQueryParser (1)
  +compileTrust (1)
  +normalizeType (1)
lib/response.js (→2)
  +sendFile (1)
lib/application.js (→1)
lib/request.js (→1)
lib/view.js (→1)
  +View (1)
test/support/utils.js (→10)
  +shouldSkipQuery (3)
examples/mvc/db.js (→3)
examples/content-negotiation/db.js (→2)
```

Flagged:

- #2 shows 0 exports: index.js (→96)
- #5 shows 0 exports: lib/application.js (→1)
- #6 shows 0 exports: lib/request.js (→1)
- #8 looks like test/fixture/vendored/generated/example/doc: test/support/utils.js (→10)
- #9 looks like test/fixture/vendored/generated/example/doc: examples/mvc/db.js (→3)
- #9 shows 0 exports: examples/mvc/db.js (→3)
- #10 looks like test/fixture/vendored/generated/example/doc: examples/content-negotiation/db.js (→2)
- #10 shows 0 exports: examples/content-negotiation/db.js (→2)
- #11 shows 0 exports: examples/ejs/index.js (→1)
- #12 shows 0 exports: examples/vhost/index.js (→1)
- #13 shows 0 exports: test/acceptance/ejs.js (→1)
- #14 shows 0 exports: test/acceptance/vhost.js (→1)
- #15 shows 0 exports: examples/session/redis.js (→2)
- #16 shows 0 exports: test/support/tmpl.js (→2)
- #17 shows 0 exports: examples/mvc/lib/boot.js (→1)
- #18 shows 0 exports: examples/auth/index.js (→1)
- #19 shows 0 exports: examples/content-negotiation/index.js (→1)
- #20 shows 0 exports: examples/cookie-sessions/index.js (→1)
- #21 shows 0 exports: examples/downloads/index.js (→1)
- #22 shows 0 exports: examples/error-pages/index.js (→1)
- #23 shows 0 exports: examples/error/index.js (→1)
- #24 shows 0 exports: examples/hello-world/index.js (→1)

LSP probes failing or not pointing into the exporting file:

- none

Incremental: `{"file": "lib/express.js", "didOpen_ms": 2.0220000000001903, "didOpen_sees_new_export": true, "didChange_revert_ms": 1.1458749999988527, "didChange_revert_drops_export": true, "cold_reindex_ms": 287.5844170000015, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### gson

Last commit 2026-10-11T14:06:38-07:00; 264 files, 1530 edges. Ground truth (UserGuide.md: Gson, GsonBuilder, TypeAdapter, JsonElement, reader/writer): `gson/src/main/java/com/google/gson/Gson.java`, `gson/src/main/java/com/google/gson/GsonBuilder.java`, `gson/src/main/java/com/google/gson/TypeAdapter.java`, `gson/src/main/java/com/google/gson/JsonElement.java`, `gson/src/main/java/com/google/gson/stream/JsonReader.java`, `gson/src/main/java/com/google/gson/stream/JsonWriter.java`, `gson/src/main/java/com/google/gson/TypeAdapterFactory.java`
Truth hits in top 24: ['gson/src/main/java/com/google/gson/Gson.java', 'gson/src/main/java/com/google/gson/GsonBuilder.java', 'gson/src/main/java/com/google/gson/stream/JsonReader.java', 'gson/src/main/java/com/google/gson/stream/JsonWriter.java', 'gson/src/main/java/com/google/gson/JsonElement.java', 'gson/src/main/java/com/google/gson/TypeAdapter.java']; missed: ['gson/src/main/java/com/google/gson/TypeAdapterFactory.java']

Top 10 map entries:

```
gson/src/main/java/com/google/gson/Gson.java (→133)
  +Gson (132)
  +fromJson (3)
  +getDelegateAdapter (3)
  +toJson (3)
gson/src/main/java/com/google/gson/GsonBuilder.java (→100)
  +GsonBuilder (100)
  +registerTypeAdapter (5)
  +setFieldNamingStrategy (3)
  +setVersion (3)
gson/src/main/java/com/google/gson/stream/JsonReader.java (→80)
  +JsonReader (79)
  +isLenient (3)
  +peek (3)
  +getStrictness (2)
gson/src/main/java/com/google/gson/stream/JsonWriter.java (→63)
  +JsonWriter (62)
  +value (3)
  +getSerializeNulls (2)
  +isLenient (2)
gson/src/main/java/com/google/gson/JsonElement.java (→69)
  +JsonElement (69)
  +isJsonNull (1)
gson/src/main/java/com/google/gson/Strictness.java (→19)
  +Strictness (19)
gson/src/main/java/com/google/gson/TypeAdapter.java (→72)
  +TypeAdapter (72)
  +nullSafe (3)
  +NullSafeTypeAdapter (1)
  +fromJsonTree (1)
gson/src/main/java/com/google/gson/reflect/TypeToken.java (→74)
  +TypeToken (74)
  +hashCode (1)
gson/src/main/java/com/google/gson/internal/TroubleshootingGuide.java (→6)
  +TroubleshootingGuide (6)
gson/src/main/java/com/google/gson/internal/NonNullElementWrapperList.java (→2)
  +NonNullElementWrapperList (1)
  +indexOf (1)
```

Flagged:

- #4 generic export name(s) ['value']: gson/src/main/java/com/google/gson/stream/JsonWriter.java
- #8 generic export name(s) ['hashCode']: gson/src/main/java/com/google/gson/reflect/TypeToken.java
- #24 generic export name(s) ['set']: gson/src/main/java/com/google/gson/JsonArray.java

LSP probes failing or not pointing into the exporting file:

- `{"probe": "ref-site definition", "file": "gson/src/main/java/com/google/gson/Gson.java", "export": "Gson", "from": "extras/src/main/java/com/google/gson/extras/examples/rawcollections/RawCollectionsExample.java:18", "ok": false, "points_into_exporter": false, "target": null}`
- `{"probe": "ref-site definition", "file": "gson/src/main/java/com/google/gson/stream/JsonReader.java", "export": "JsonReader", "from": "extras/src/main/java/com/google/gson/interceptors/InterceptorFactory.java:23", "ok": false, "points_into_exporter": false, "target": null}`

Incremental: `{"file": "gson/src/main/java/com/google/gson/Gson.java", "didOpen_ms": 25.802958000001652, "didOpen_sees_new_export": true, "didChange_revert_ms": 29.73862500000024, "didChange_revert_drops_export": true, "cold_reindex_ms": 488.39962499999956, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### guzzle

Last commit 2026-10-18T14:38:43+01:00; 145 files, 738 edges. Ground truth (README/docs: Client, HandlerStack, Middleware, Psr7 messages, Exception): `src/Client.php`, `src/ClientInterface.php`, `src/HandlerStack.php`, `src/Middleware.php`, `src/RequestOptions.php`, `src/Utils.php`, `src/Handler/CurlFactory.php`, `src/Pool.php`
Truth hits in top 24: ['src/Utils.php', 'src/Client.php', 'src/RequestOptions.php']; missed: ['src/ClientInterface.php', 'src/HandlerStack.php', 'src/Middleware.php', 'src/Handler/CurlFactory.php', 'src/Pool.php']

Top 10 map entries:

```
src/Exception/InvalidArgumentException.php (→49)
  +InvalidArgumentException (49)
src/Exception/GuzzleException.php (→14)
  +GuzzleException (14)
src/Utils.php (→46)
  +Utils (46)
  +normalizeHeaderKeys (4)
  +chooseHandler (3)
  +debugResource (3)
src/Client.php (→32)
  +Client (32)
  +send (6)
  +sendAsync (3)
  +getConfig (2)
src/Exception/RequestException.php (→28)
  +RequestException (28)
  +__construct (2)
  +create (1)
src/NonSerializableTrait.php (→24)
  +NonSerializableTrait (24)
src/Exception/ResponseException.php (→31)
  +ResponseException (31)
  +getResponse (14)
  +withResponse (2)
src/HostIdentity.php (→8)
  +HostIdentity (8)
  +canonicalHost (5)
  +numericIpv4ToBinary (3)
  +canonicalCookieDomain (2)
tests/UnvalidatedUriRequest.php (→37)
  +getHeaderLine (24)
  +getUri (20)
  +getBody (18)
  +hasHeader (18)
tests/UnvalidatedUri.php (→19)
  +getHost (12)
  +getScheme (8)
  +withHost (6)
  +getPort (5)
```

Flagged:

- #5 generic export name(s) ['create']: src/Exception/RequestException.php
- #9 looks like test/fixture/vendored/generated/example/doc: tests/UnvalidatedUriRequest.php (→37)
- #10 looks like test/fixture/vendored/generated/example/doc: tests/UnvalidatedUri.php (→19)
- #22 generic export name(s) ['execute']: src/Handler/CurlMultiHandler.php

LSP probes failing or not pointing into the exporting file:

- none

Incremental: `{"file": "src/Exception/InvalidArgumentException.php", "didOpen_ms": 4.167125000002159, "didOpen_sees_new_export": true, "didChange_revert_ms": 3.1368750000027035, "didChange_revert_drops_export": true, "cold_reindex_ms": 397.22433400000057, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### hiredis

Last commit 2026-10-15T15:47:43+02:00; 53 files, 91 edges. Ground truth (README: hiredis.h/c (sync API), async, read (reply parser), net, sds): `hiredis.c`, `hiredis.h`, `async.c`, `async.h`, `read.c`, `read.h`, `net.c`, `sds.h`, `sds.c`
Truth hits in top 24: ['hiredis.h', 'async.h', 'sds.h', 'sds.c', 'async.c', 'read.c', 'read.h', 'hiredis.c', 'net.c']; missed: []

Top 10 map entries:

```
alloc.h (→6)
  +hi_calloc (3)
  +hi_free (3)
  +hi_malloc (2)
  +hi_realloc (2)
alloc.c (→1)
fmacros.h (→8)
hiredis.h (→15)
  +redisBufferRead (2)
  +redisBufferWrite (2)
  +freeReplyObject (1)
  +redisAppendCommand (1)
async.h (→16)
  +redisAsyncHandleRead (12)
  +redisAsyncHandleWrite (12)
  +redisAsyncHandleTimeout (7)
  +redisAsyncDisconnect (2)
sockcompat.h (→4)
  +win32_redisKeepAlive (1)
sockcompat.c (→1)
sds.h (→6)
  +sdsfree (3)
  +sdscatlen (2)
  +sdsempty (2)
  +sdsrange (2)
sds.c (→1)
async.c (→1)
```

Flagged:

- #2 shows 0 exports: alloc.c (→1)
- #3 shows 0 exports: fmacros.h (→8)
- #7 shows 0 exports: sockcompat.c (→1)
- #9 shows 0 exports: sds.c (→1)
- #10 shows 0 exports: async.c (→1)
- #11 shows 0 exports: read.c (→1)
- #12 shows 0 exports: read.h (→2)
- #14 shows 0 exports: sdsalloc.h (→1)
- #15 shows 0 exports: hiredis.c (→1)
- #17 shows 0 exports: examples/example-qt.cpp (→1)
- #18 shows 0 exports: dict.c (→2)
- #19 shows 0 exports: win32.h (→6)
- #20 shows 0 exports: net.c (→1)
- #24 shows 0 exports: dict.h (→1)

LSP probes failing or not pointing into the exporting file:

- `{"probe": "ref-site definition", "file": "alloc.h", "export": "hi_calloc", "from": "alloc.c:70", "ok": true, "points_into_exporter": false, "target": "alloc.c"}`
- `{"probe": "ref-site definition", "file": "hiredis.h", "export": "redisBufferRead", "from": "hiredis.c:983", "ok": true, "points_into_exporter": false, "target": "hiredis.c"}`
- `{"probe": "ref-site definition", "file": "async.h", "export": "redisAsyncHandleRead", "from": "async.c:724", "ok": true, "points_into_exporter": false, "target": "async.c"}`

Incremental: `{"file": "alloc.h", "didOpen_ms": 5.068292000000696, "didOpen_sees_new_export": true, "didChange_revert_ms": 3.145792000001535, "didChange_revert_drops_export": true, "cold_reindex_ms": 276.3461669999998, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### json

Last commit 2026-10-21T12:26:46+02:00; 604 files, 502 edges. Ground truth (README: single header nlohmann/json.hpp (single_include) is the library; include/nlohmann/json.hpp is the multi-header entry): `single_include/nlohmann/json.hpp`, `include/nlohmann/json.hpp`, `include/nlohmann/json_fwd.hpp`, `include/nlohmann/detail/json_pointer.hpp`, `include/nlohmann/detail/input/parser.hpp`, `include/nlohmann/detail/output/serializer.hpp`, `include/nlohmann/detail/value_t.hpp`
Truth hits in top 24: ['include/nlohmann/json.hpp', 'include/nlohmann/detail/output/serializer.hpp']; missed: ['single_include/nlohmann/json.hpp', 'include/nlohmann/json_fwd.hpp', 'include/nlohmann/detail/json_pointer.hpp', 'include/nlohmann/detail/input/parser.hpp', 'include/nlohmann/detail/value_t.hpp']

Top 10 map entries:

```
tests/abi/include/nlohmann/json_v3_10_5.hpp (→207) [RECENT]
  +out_of_range (64)
  +type_error (59)
  +value_t (57)
  +exception (46)
tests/src/unit-regression1.cpp (→98) [RECENT]
  +array (98)
include/nlohmann/json.hpp (→13) [RECENT]
  +format_as (3)
  +json_value (3)
  +start_pos (3)
  +assert_invariant (2)
tests/src/unit-allocator.cpp (→6) [RECENT]
  +construct (6)
include/nlohmann/detail/output/serializer.hpp (→7) [RECENT]
  +dump_escaped_impl (4)
  +dump_escaped (3)
  +put_literal (2)
  +write_u_escape (2)
tests/src/test_utils.hpp (→12) [RECENT]
  +ignore_return_value (11)
  +istreambuf_sentinel (6)
  +read_binary_file (6)
  +next_integer_sample (4)
include/nlohmann/detail/input/binary_reader.hpp (→4) [RECENT]
  +little_endianness (2)
  +get_bon8_string (1)
  +get_cbor_container_size (1)
tests/src/make_test_data_available.hpp (→12) [RECENT]
tests/src/unit-bon8.cpp (→18) [RECENT]
  +concat (18)
tests/src/unit-enum_keyed_maps.cpp (→7) [RECENT]
  +digit (4)
  +plain (3)
```

Flagged:

- #1 looks like test/fixture/vendored/generated/example/doc: tests/abi/include/nlohmann/json_v3_10_5.hpp (→207) [RECENT]
- #1 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/abi/include/nlohmann/json_v3_10_5.hpp (→207) [RECENT]
- #2 looks like test/fixture/vendored/generated/example/doc: tests/src/unit-regression1.cpp (→98) [RECENT]
- #2 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/src/unit-regression1.cpp (→98) [RECENT]
- #3 [RECENT] (last commit 2026-10-21T12:26:46+02:00): include/nlohmann/json.hpp (→13) [RECENT]
- #4 looks like test/fixture/vendored/generated/example/doc: tests/src/unit-allocator.cpp (→6) [RECENT]
- #4 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/src/unit-allocator.cpp (→6) [RECENT]
- #5 [RECENT] (last commit 2026-10-21T12:26:46+02:00): include/nlohmann/detail/output/serializer.hpp (→7) [RECENT]
- #6 looks like test/fixture/vendored/generated/example/doc: tests/src/test_utils.hpp (→12) [RECENT]
- #6 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/src/test_utils.hpp (→12) [RECENT]
- #7 [RECENT] (last commit 2026-10-21T12:26:46+02:00): include/nlohmann/detail/input/binary_reader.hpp (→4) [RECENT]
- #8 looks like test/fixture/vendored/generated/example/doc: tests/src/make_test_data_available.hpp (→12) [RECENT]
- #8 shows 0 exports: tests/src/make_test_data_available.hpp (→12) [RECENT]
- #8 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/src/make_test_data_available.hpp (→12) [RECENT]
- #9 looks like test/fixture/vendored/generated/example/doc: tests/src/unit-bon8.cpp (→18) [RECENT]
- #9 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/src/unit-bon8.cpp (→18) [RECENT]
- #10 looks like test/fixture/vendored/generated/example/doc: tests/src/unit-enum_keyed_maps.cpp (→7) [RECENT]
- #10 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/src/unit-enum_keyed_maps.cpp (→7) [RECENT]
- #11 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/thirdparty/fifo_map/fifo_map.hpp (→4) [RECENT]
- #12 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/abi/diag/diag.hpp (→3) [RECENT]
- #13 shows 0 exports: tests/abi/diag/diag.cpp (→1) [RECENT]
- #13 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/abi/diag/diag.cpp (→1) [RECENT]
- #14 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/cmake_target_include_directories/project/Foo.hpp (→2) [RECENT]
- #15 shows 0 exports: tests/cmake_target_include_directories/project/Foo.cpp (→1) [RECENT]
- #15 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/cmake_target_include_directories/project/Foo.cpp (→1) [RECENT]
- #16 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/src/sax_countdown.hpp (→7) [RECENT]
- #17 [RECENT] (last commit 2026-10-21T12:26:46+02:00): include/nlohmann/detail/input/input_adapters.hpp (→5) [RECENT]
- #18 [RECENT] (last commit 2026-10-21T12:26:46+02:00): tests/src/round_trip_corpus.hpp (→6) [RECENT]
- #19 shows 0 exports: docs/mkdocs/docs/examples/custom_binary_type.cpp (→1) [RECENT]
- #19 [RECENT] (last commit 2026-10-21T12:26:46+02:00): docs/mkdocs/docs/examples/custom_binary_type.cpp (→1) [RECENT]
- #20 shows 0 exports: docs/mkdocs/docs/examples/custom_binary_type.hpp (→1) [RECENT]
- #20 [RECENT] (last commit 2026-10-21T12:26:46+02:00): docs/mkdocs/docs/examples/custom_binary_type.hpp (→1) [RECENT]
- #21 shows 0 exports: docs/mkdocs/docs/examples/custom_object_type.cpp (→1) [RECENT]
- #21 [RECENT] (last commit 2026-10-21T12:26:46+02:00): docs/mkdocs/docs/examples/custom_object_type.cpp (→1) [RECENT]
- #22 shows 0 exports: docs/mkdocs/docs/examples/custom_object_type.hpp (→1) [RECENT]
- #22 [RECENT] (last commit 2026-10-21T12:26:46+02:00): docs/mkdocs/docs/examples/custom_object_type.hpp (→1) [RECENT]
- #23 shows 0 exports: docs/mkdocs/docs/examples/custom_string_type.cpp (→1) [RECENT]
- #23 [RECENT] (last commit 2026-10-21T12:26:46+02:00): docs/mkdocs/docs/examples/custom_string_type.cpp (→1) [RECENT]
- #24 shows 0 exports: docs/mkdocs/docs/examples/custom_string_type.hpp (→1) [RECENT]
- #24 [RECENT] (last commit 2026-10-21T12:26:46+02:00): docs/mkdocs/docs/examples/custom_string_type.hpp (→1) [RECENT]

LSP probes failing or not pointing into the exporting file:

- none

Incremental: `{"file": "tests/abi/include/nlohmann/json_v3_10_5.hpp", "didOpen_ms": 185.46300000000215, "didOpen_sees_new_export": true, "didChange_revert_ms": 222.43945899999673, "didChange_revert_drops_export": true, "cold_reindex_ms": 900.8934999999987, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### kilo

Last commit 2025-01-04T18:31:29+01:00; 1 files, 0 edges. Ground truth (single-file editor; the only source file): `kilo.c`
Truth hits in top 24: ['kilo.c']; missed: []

Top 10 map entries:

```
kilo.c (→0)
```

Flagged:

- #1 has (→0): kilo.c (→0)
- #1 shows 0 exports: kilo.c (→0)

LSP probes failing or not pointing into the exporting file:

- none

Incremental: `{"file": "kilo.c", "didOpen_ms": 7.925583999998764, "didOpen_sees_new_export": true, "didChange_revert_ms": 8.784166999994625, "didChange_revert_drops_export": true, "cold_reindex_ms": 97.76175000000364, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### ky

Last commit 2026-10-09T13:29:22+07:00; 98 files, 121 edges. Ground truth (readme: the `ky` default export, its options and errors (source/index.ts, core/Ky.ts)): `source/index.ts`, `source/core/Ky.ts`, `source/types/options.ts`, `source/errors/HTTPError.ts`, `source/errors/TimeoutError.ts`
Truth hits in top 24: ['source/types/options.ts', 'source/errors/HTTPError.ts', 'source/core/Ky.ts']; missed: ['source/index.ts', 'source/errors/TimeoutError.ts']

Top 10 map entries:

```
source/types/ky.ts (→21)
source/types/options.ts (→8)
  +InternalOptions (4)
  +HttpMethod (2)
  +InitOptions (2)
  +RequestHttpMethod (2)
test/helpers/create-http-test-server.ts (→27)
  +createHttpTestServer (27)
  +ExtendedHttpTestServer (1)
  +HttpServerOptions (1)
source/core/constants.ts (→5)
  +getSupportedResponseTypes (2)
  +retry (2)
  +ForceRetryOptions (1)
  +RetryMarker (1)
source/types/hooks.ts (→2)
  +NormalizedHooks (2)
source/utils/is.ts (→6)
  +isError (3)
  +isNonArrayObject (3)
  +isNonNegativeNumber (3)
  +isObject (2)
source/types/common.ts (→1)
  +LiteralUnion (1)
source/types/retry.ts (→1)
  +MutableRetryOptions (1)
source/utils/delay.ts (→10)
  +delay (10)
source/errors/NonError.ts (→7)
  +NonError (7)
```

Flagged:

- #1 shows 0 exports: source/types/ky.ts (→21)
- #3 looks like test/fixture/vendored/generated/example/doc: test/helpers/create-http-test-server.ts (→27)
- #24 shows 0 exports: source/core/Ky.ts (→0)

LSP probes failing or not pointing into the exporting file:

- none

Incremental: `{"file": "source/types/ky.ts", "didOpen_ms": 0.8345410000032416, "didOpen_sees_new_export": true, "didChange_revert_ms": 0.549124999999151, "didChange_revert_drops_export": true, "cold_reindex_ms": 130.07274999999652, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### okio

Last commit 2026-10-21T03:44:39Z; 357 files, 1470 edges. Ground truth (README: ByteString, Buffer, Source/Sink, FileSystem, BufferedSource/Sink): `okio/src/commonMain/kotlin/okio/ByteString.kt`, `okio/src/commonMain/kotlin/okio/Buffer.kt`, `okio/src/commonMain/kotlin/okio/Source.kt`, `okio/src/commonMain/kotlin/okio/Sink.kt`, `okio/src/commonMain/kotlin/okio/FileSystem.kt`, `okio/src/commonMain/kotlin/okio/BufferedSource.kt`, `okio/src/commonMain/kotlin/okio/BufferedSink.kt`, `okio/src/commonMain/kotlin/okio/Path.kt`
Truth hits in top 24: ['okio/src/commonMain/kotlin/okio/Buffer.kt', 'okio/src/commonMain/kotlin/okio/ByteString.kt', 'okio/src/commonMain/kotlin/okio/Source.kt', 'okio/src/commonMain/kotlin/okio/BufferedSource.kt', 'okio/src/commonMain/kotlin/okio/BufferedSink.kt', 'okio/src/commonMain/kotlin/okio/Sink.kt', 'okio/src/commonMain/kotlin/okio/FileSystem.kt', 'okio/src/commonMain/kotlin/okio/Path.kt']; missed: []

Top 10 map entries:

```
okio/src/commonMain/kotlin/okio/Buffer.kt (→140) [RECENT]
  +Buffer (140)
  +UnsafeCursor (10)
  +expandBuffer (3)
  +snapshot (3)
okio/src/commonMain/kotlin/okio/Segment.kt (→34) [RECENT]
  +Segment (34)
  +SIZE (5)
okio/src/commonMain/kotlin/okio/SegmentPool.kt (→16) [RECENT]
  +SegmentPool (15)
  +MAX_SIZE (5)
  +recycle (2)
okio/src/commonMain/kotlin/okio/CommonPlatform.kt (→128) [RECENT]
  +IOException (99)
  +EOFException (25)
  +FileNotFoundException (24)
  +Closeable (22)
okio/src/commonMain/kotlin/okio/ByteString.kt (→96) [RECENT]
  +ByteString (96)
  +endsWith (3)
  +startsWith (3)
  +utf8 (1)
okio/src/commonMain/kotlin/okio/Source.kt (→78) [RECENT]
  +Source (78)
okio/src/commonMain/kotlin/okio/Timeout.kt (→54) [RECENT]
  +Timeout (54)
  +NONE (2)
okio/src/commonMain/kotlin/okio/BufferedSource.kt (→43) [RECENT]
  +BufferedSource (43)
okio/src/commonMain/kotlin/okio/BufferedSink.kt (→41) [RECENT]
  +BufferedSink (41)
okio/src/commonMain/kotlin/okio/Sink.kt (→11) [RECENT]
  +Sink (11)
```

Flagged:

- #1 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/Buffer.kt (→140) [RECENT]
- #2 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/Segment.kt (→34) [RECENT]
- #3 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/SegmentPool.kt (→16) [RECENT]
- #4 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/CommonPlatform.kt (→128) [RECENT]
- #5 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/ByteString.kt (→96) [RECENT]
- #6 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/Source.kt (→78) [RECENT]
- #7 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/Timeout.kt (→54) [RECENT]
- #8 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/BufferedSource.kt (→43) [RECENT]
- #9 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/BufferedSink.kt (→41) [RECENT]
- #10 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/Sink.kt (→11) [RECENT]
- #11 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/FileSystem.kt (→48) [RECENT]
- #12 [RECENT] (last commit 2026-10-21T03:44:39Z): samples/src/jvmMain/kotlin/okio/samples/GoldenValue.kt (→72) [RECENT]
- #13 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/Options.kt (→16) [RECENT]
- #14 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/TypedOptions.kt (→12) [RECENT]
- #15 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/hashFunctions/kotlin/okio/internal/HashFunction.kt (→13) [RECENT]
- #16 shows 0 exports: okio/src/nonAppleMain/kotlin/okio/ByteString.kt (→14) [RECENT]
- #16 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/nonAppleMain/kotlin/okio/ByteString.kt (→14) [RECENT]
- #17 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/Util.kt (→33) [RECENT]
- #18 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/jvmMain/kotlin/okio/Buffer.kt (→16) [RECENT]
- #19 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/Path.kt (→32) [RECENT]
- #20 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/FileHandle.kt (→26) [RECENT]
- #21 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/jvmTest/kotlin/okio/TestUtil.kt (→21) [RECENT]
- #22 [RECENT] (last commit 2026-10-21T03:44:39Z): okio-testing-support/src/commonMain/kotlin/okio/TestingCommon.kt (→14) [RECENT]
- #23 [RECENT] (last commit 2026-10-21T03:44:39Z): okio-fakefilesystem/src/commonMain/kotlin/okio/fakefilesystem/FakeFileSystem.kt (→9) [RECENT]
- #24 [RECENT] (last commit 2026-10-21T03:44:39Z): okio/src/commonMain/kotlin/okio/FileMetadata.kt (→8) [RECENT]

LSP probes failing or not pointing into the exporting file:

- `{"probe": "ref-site definition", "file": "okio/src/commonMain/kotlin/okio/Buffer.kt", "export": "Buffer", "from": "okio-assetfilesystem/src/androidTest/kotlin/okio/assetfilesystem/AssetFileSystemTest.kt:30", "ok": false, "points_into_exporter": false, "target": null}`

Incremental: `{"file": "okio/src/commonMain/kotlin/okio/Buffer.kt", "didOpen_ms": 14.635540999996977, "didOpen_sees_new_export": true, "didChange_revert_ms": 10.185083999999733, "didChange_revert_drops_export": true, "cold_reindex_ms": 1053.906375000004, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### requests

Last commit 2026-09-21T13:22:33-07:00; 37 files, 120 edges. Ground truth (docs/api + README: the public API is api/sessions/models/adapters): `src/requests/api.py`, `src/requests/sessions.py`, `src/requests/models.py`, `src/requests/adapters.py`, `src/requests/exceptions.py`, `src/requests/__init__.py`, `src/requests/auth.py`
Truth hits in top 24: ['src/requests/models.py', 'src/requests/exceptions.py', 'src/requests/__init__.py', 'src/requests/auth.py', 'src/requests/adapters.py', 'src/requests/sessions.py', 'src/requests/api.py']; missed: []

Top 10 map entries:

```
src/requests/compat.py (→13)
  +Morsel (2)
src/requests/cookies.py (→10)
  +RequestsCookieJar (5)
  +cookiejar_from_dict (4)
  +extract_cookies_to_jar (2)
  +CookieConflictError (1)
src/requests/models.py (→12)
  +Response (9)
  +PreparedRequest (8)
  +Request (8)
  +next (2)
src/requests/__version__.py (→4)
src/requests/exceptions.py (→9)
  +ConnectionError (5)
  +ChunkedEncodingError (4)
  +InvalidHeader (4)
  +InvalidURL (4)
src/requests/_internal_utils.py (→6)
  +to_native_string (5)
  +unicode_is_ascii (2)
  +HEADER_VALIDATORS (1)
src/requests/_types.py (→7)
  +SupportsItems (1)
  +SupportsRead (1)
src/requests/structures.py (→9)
  +CaseInsensitiveDict (8)
  +LookupDict (1)
src/requests/utils.py (→6)
  +get_auth_from_url (4)
  +requote_uri (3)
  +to_key_val_list (3)
  +get_encoding_from_headers (2)
src/requests/__init__.py (→7)
```

Flagged:

- #3 generic export name(s) ['next']: src/requests/models.py
- #4 shows 0 exports: src/requests/__version__.py (→4)
- #10 shows 0 exports: src/requests/__init__.py (→7)
- #15 shows 0 exports: src/requests/status_codes.py (→3)
- #18 shows 0 exports: src/requests/api.py (→1)
- #19 shows 0 exports: src/requests/packages.py (→1)
- #20 shows 0 exports: src/requests/certs.py (→1)
- #23 shows 0 exports: tests/compat.py (→2)

LSP probes failing or not pointing into the exporting file:

- `{"probe": "ref-site definition", "file": "src/requests/models.py", "export": "Response", "from": "src/requests/__init__.py:184", "ok": true, "points_into_exporter": false, "target": "src/requests/__init__.py"}`

Incremental: `{"file": "src/requests/compat.py", "didOpen_ms": 3.216000000001884, "didOpen_sees_new_export": true, "didChange_revert_ms": 2.336791000004723, "didChange_revert_drops_export": true, "cold_reindex_ms": 289.70633299999804, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### ripgrep

Last commit 2026-08-04T10:00:08-04:00; 111 files, 405 edges. Ground truth (GUIDE/README crate list + the grep-* crate architecture: the binary entry, searcher, matcher, regex matcher, printer, ignore walker, globset): `crates/core/main.rs`, `crates/searcher/src/searcher/mod.rs`, `crates/matcher/src/lib.rs`, `crates/regex/src/matcher.rs`, `crates/printer/src/lib.rs`, `crates/ignore/src/walk.rs`, `crates/globset/src/lib.rs`, `crates/core/flags/parse.rs`
Truth hits in top 24: ['crates/globset/src/lib.rs', 'crates/matcher/src/lib.rs', 'crates/searcher/src/searcher/mod.rs', 'crates/ignore/src/walk.rs']; missed: ['crates/core/main.rs', 'crates/regex/src/matcher.rs', 'crates/printer/src/lib.rs', 'crates/core/flags/parse.rs']

Top 10 map entries:

```
crates/printer/src/hyperlink/mod.rs (→16)
  +HyperlinkConfig (5)
  +description (4)
  +Interpolator (3)
  +InterpolatorStatus (3)
crates/globset/src/lib.rs (→11)
  +Candidate (7)
  +GlobSetBuilder (6)
  +GlobSet (5)
  +ErrorKind (2)
crates/matcher/src/lib.rs (→24)
  +Matcher (16)
  +LineTerminator (11)
  +Captures (6)
  +LineMatchKind (4)
crates/printer/src/util.rs (→7)
  +PrinterPath (3)
  +find_iter_at_in_context (3)
  +DecimalFormatter (2)
  +Replacer (2)
crates/searcher/src/searcher/mod.rs (→21)
  +Searcher (14)
  +SearcherBuilder (7)
  +Encoding (4)
  +Config (2)
crates/ignore/src/walk.rs (→13)
  +DirEntry (4)
  +WalkBuilder (4)
  +from_iter (3)
  +WalkState (2)
crates/searcher/src/sink.rs (→17)
  +Bytes (7)
  +SinkMatch (6)
  +SinkContext (5)
  +SinkError (5)
crates/globset/src/glob.rs (→7)
  +Glob (4)
  +GlobBuilder (2)
  +GlobMatcher (1)
  +MatchStrategy (1)
crates/printer/src/hyperlink/aliases.rs (→1)
  +HYPERLINK_PATTERN_ALIASES (1)
tests/hay.rs (→13)
  +SHERLOCK (13)
  +SHERLOCK_CRLF (3)
```

Flagged:

- #10 looks like test/fixture/vendored/generated/example/doc: tests/hay.rs (→13)
- #16 shows 0 exports: crates/matcher/src/interpolate.rs (→1)
- #19 generic export name(s) ['name']: crates/ignore/src/types.rs
- #22 generic export name(s) ['index']: crates/core/flags/hiargs.rs

LSP probes failing or not pointing into the exporting file:

- none

Incremental: `{"file": "crates/printer/src/hyperlink/mod.rs", "didOpen_ms": 6.894707999997252, "didOpen_sees_new_export": true, "didChange_revert_ms": 6.645165999998426, "didChange_revert_drops_export": true, "cold_reindex_ms": 162.21412499999843, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### sinatra

Last commit 2026-07-20T11:59:21+02:00; 147 files, 260 edges. Ground truth (README: lib/sinatra/base.rb is the framework; main.rb is the classic DSL): `lib/sinatra/base.rb`, `lib/sinatra/main.rb`, `lib/sinatra.rb`, `lib/sinatra/show_exceptions.rb`, `lib/sinatra/indifferent_hash.rb`
Truth hits in top 24: ['lib/sinatra/base.rb', 'lib/sinatra/indifferent_hash.rb', 'lib/sinatra.rb', 'lib/sinatra/main.rb', 'lib/sinatra/show_exceptions.rb']; missed: []

Top 10 map entries:

```
rack-protection/lib/rack/protection.rb (→22)
  +Rack (2)
  +Protection (1)
lib/sinatra/base.rb (→44)
  +Base (17)
  +Application (13)
  +Delegator (8)
  +before (8)
test/test_helper.rb (→39)
  +Test (39)
  +Base (13)
  +mock_app (8)
  +Builder (4)
lib/sinatra/middleware/logger.rb (→8)
  +Logger (6)
  +Middleware (3)
lib/sinatra/indifferent_hash.rb (→5)
  +IndifferentHash (5)
  +Sinatra (1)
  +key (1)
lib/sinatra.rb (→7)
lib/sinatra/main.rb (→1)
test/contest.rb (→1)
rack-protection/lib/rack/protection/version.rb (→1)
lib/sinatra/show_exceptions.rb (→1)
  +ShowExceptions (1)
```

Flagged:

- #3 looks like test/fixture/vendored/generated/example/doc: test/test_helper.rb (→39)
- #6 shows 0 exports: lib/sinatra.rb (→7)
- #7 shows 0 exports: lib/sinatra/main.rb (→1)
- #8 looks like test/fixture/vendored/generated/example/doc: test/contest.rb (→1)
- #8 shows 0 exports: test/contest.rb (→1)
- #9 shows 0 exports: rack-protection/lib/rack/protection/version.rb (→1)
- #13 shows 0 exports: rack-protection/spec/spec_helper.rb (→15)
- #16 shows 0 exports: lib/sinatra/version.rb (→1)

LSP probes failing or not pointing into the exporting file:

- `{"probe": "ref-site definition", "file": "rack-protection/lib/rack/protection.rb", "export": "Rack", "from": "rack-protection/lib/rack/protection/authenticity_token.rb:8", "ok": true, "points_into_exporter": false, "target": "rack-protection/lib/rack/protection/authenticity_token.rb"}`
- `{"probe": "ref-site definition", "file": "lib/sinatra/base.rb", "export": "Base", "from": "rack-protection/lib/rack/protection/base.rb:12", "ok": false, "points_into_exporter": false, "target": null}`
- `{"probe": "ref-site definition", "file": "test/test_helper.rb", "export": "Test", "from": "test/contest.rb:28", "ok": true, "points_into_exporter": false, "target": "test/contest.rb"}`

Incremental: `{"file": "rack-protection/lib/rack/protection.rb", "didOpen_ms": 1.816833000006568, "didOpen_sees_new_export": true, "didChange_revert_ms": 1.708624999999131, "didChange_revert_drops_export": true, "cold_reindex_ms": 174.28133299999615, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`

### titi

Last commit 2026-10-21T15:13:00+05:00; 351 files, 2185 edges. Ground truth (docs/ARCHITECTURE.md crate table: CLI main, engine turn loop, genome lib, providers, tools, core sessions): `crates/titi-cli/src/main.rs`, `crates/titi-engine/src/lib.rs`, `crates/titi-genome/src/lib.rs`, `crates/titi-genome/src/graph.rs`, `crates/titi-tools/src/lib.rs`, `crates/titi-providers/src/lib.rs`, `crates/titi-core/src/lib.rs`, `crates/titi-config/src/lib.rs`
Truth hits in top 24: ['crates/titi-genome/src/lib.rs', 'crates/titi-tools/src/lib.rs']; missed: ['crates/titi-cli/src/main.rs', 'crates/titi-engine/src/lib.rs', 'crates/titi-genome/src/graph.rs', 'crates/titi-providers/src/lib.rs', 'crates/titi-core/src/lib.rs', 'crates/titi-config/src/lib.rs']

Top 10 map entries:

```
crates/titi-providers/src/transport.rs (→93) [RECENT]
  +Transport (48)
  +ChatMessage (30)
  +ApiKind (22)
  +ToolSpec (21)
crates/titi-providers/src/stream.rs (→71)
  +StreamEvent (52)
  +StopReason (49)
  +ToolCallRef (41)
  +BlockId (37)
crates/titi-genome/src/lib.rs (→50) [RECENT]
  +Genome (33)
  +ExportSite (15)
  +UnresolvedImport (13)
  +Candidate (5)
crates/titi-providers/src/compat.rs (→32) [RECENT]
  +Effort (25)
  +Reasoning (9)
  +ModelCompat (7)
  +RequestOpts (5)
crates/titi-providers/src/creds.rs (→13)
  +Credential (6)
  +LadderLevel (6)
  +CredKind (5)
  +api_key (3)
crates/titi-genome/src/lang/mod.rs (→37) [RECENT]
  +Level (15)
  +capabilities (14)
  +ParsedFile (12)
  +Language (7)
crates/titi-secrets/src/store.rs (→15)
  +store (13)
  +AuthStore (3)
  +OAuthRecord (2)
  +DEFAULT_LABEL (1)
crates/titi-tools/src/lib.rs (→53) [RECENT]
  +ToolRegistry (31)
  +ToolHandler (23)
  +ApprovalTier (22)
  +ToolResult (22)
crates/titi-genome/src/live.rs (→19) [RECENT]
  +Request (7)
  +pending (6)
  +GenomeHandle (3)
  +shared (3)
crates/titi-tools/src/pty.rs (→12)
  +Interrupt (10)
  +OUTPUT_CAP (2)
  +DEFAULT_TIMEOUT_SECS (1)
  +Options (1)
```

Flagged:

- #1 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-providers/src/transport.rs (→93) [RECENT]
- #3 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-genome/src/lib.rs (→50) [RECENT]
- #4 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-providers/src/compat.rs (→32) [RECENT]
- #6 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-genome/src/lang/mod.rs (→37) [RECENT]
- #8 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-tools/src/lib.rs (→53) [RECENT]
- #9 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-genome/src/live.rs (→19) [RECENT]
- #11 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-config/src/settings.rs (→31) [RECENT]
- #13 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-engine/src/protocol.rs (→78) [RECENT]
- #14 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-tools/src/pipe.rs (→16) [RECENT]
- #15 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-engine/src/runtime.rs (→56) [RECENT]
- #16 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-tools/src/fs.rs (→22) [RECENT]
- #17 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-genome/src/lang/support.rs (→16) [RECENT]
- #19 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-engine/src/goal.rs (→7) [RECENT]
- #21 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-config/src/trust.rs (→8) [RECENT]
- #23 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-engine/src/registry.rs (→51) [RECENT]
- #24 [RECENT] (last commit 2026-10-21T15:13:00+05:00): crates/titi-engine/src/agent_defs.rs (→17) [RECENT]

LSP probes failing or not pointing into the exporting file:

- none

Incremental: `{"file": "crates/titi-providers/src/transport.rs", "didOpen_ms": 8.756624999996632, "didOpen_sees_new_export": true, "didChange_revert_ms": 12.04879200000164, "didChange_revert_drops_export": true, "cold_reindex_ms": 666.7987920000016, "disk_sees_new_export": true, "map_after": {"file_in_map": true, "recent_flag": true, "new_export_listed": false}}`
