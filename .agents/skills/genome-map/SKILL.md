---
name: genome-map
description: Orient in a codebase with titi's genome — a ranked map of files, exported symbols, dependency edges, and PageRank. Use when you need to know which files matter most before diving in.
---

# genome-map

titi ships its own code-orientation tool: `titi-genome` indexes a
workspace into a ranked map — files, the symbols each defines and uses,
the dependency edges between them, and a PageRank over those edges — then
projects the top-N into a compact prompt-sized listing.

## Run it

From the titi repo (compiles the example on first run):

```bash
cargo run -p titi-genome --example map -- <path> <N>
```

Or the prebuilt release binary (no compile), if installed at
`~/bin/titi-map` (it may be older than the source; otherwise use the
`cargo run` form above):

```bash
titi-map <path> <N>
```

- `<path>` — workspace root to index (defaults to `.`).
- `<N>` — how many top-ranked files to print (defaults to 24).

It first prints `<files> files, <edges> edges` to stderr, then the ranked
projection to stdout. Read the top of that list to find the load-bearing
files before editing.

## The genome from titi itself

The same index answers two more questions, without the example binary:

```bash
titi genome            # status: on/off, why, the cap, the agent config path
titi genome on|off     # write the boolean to the agent config.yml
titi genome limit <n>  # write the cap (1..=64, else 24)
titi genome check      # index diagnostics: path:line: code: message, exit 1 on any
titi genome lsp        # stdio LSP: documentSymbol, definition, references, diagnostic
```

The same verbs work in the chat as `/genome` (`check` runs locally there;
`lsp` only names the terminal command). Two settings decide what a turn
gets: `genome.enabled` (unset means on) and `genome.limit` (1..=64, the
project `.titi/config.yml` winning over the agent's); `TITI_NO_GENOME=1`
forces the map off for one run without touching either.
