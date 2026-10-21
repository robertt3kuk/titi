//! Atlas benchmark: `cargo run --release -p titi-atlas --example bench_atlas -- [--quick] [repo-root]`
//!
//! Times the cold index, the prompt-map render, a no-op refresh (the per-turn
//! cost of the engine's fallback walk), and the two ways a single edited file
//! reaches the index, on `repo-root` and on a synthetic 5k-file tree built in
//! a temp dir. One JSON object per line on stdout; `scripts/bench.py` collects
//! them. Rows: `{"name", "n", "min_ms", "median_ms", "p95_ms", "bytes"?}`.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::Instant;

use titi_atlas::{Atlas, SharedAtlas};

/// Prompt-map size the benchmark renders, in files (`ranking` order).
const MAP_LIMIT: usize = 24;
const SYNTH_FILES: usize = 5_000;

fn main() -> std::io::Result<()> {
    let mut quick = false;
    let mut root = None;
    for arg in std::env::args().skip(1) {
        if arg == "--quick" {
            quick = true;
        } else {
            root = Some(arg);
        }
    }
    let runs = if quick { 3 } else { 7 };
    let root = root.unwrap_or_else(|| ".".to_owned());
    bench_tree("repo", Path::new(&root), runs)?;

    let synth = tempfile::tempdir()?;
    write_synthetic(synth.path(), SYNTH_FILES)?;
    bench_tree("synth5k", synth.path(), runs)?;
    Ok(())
}

fn bench_tree(label: &str, root: &Path, runs: usize) -> std::io::Result<()> {
    let files = Atlas::index(root)?.files.len();
    emit(&format!("atlas.{label}.files"), &[files as f64], None);

    let times = repeat(runs, || {
        Atlas::index(root).map(|g| g.files.len()).unwrap_or(0)
    });
    emit(&format!("atlas.{label}.index_cold"), &times, None);

    let atlas = Atlas::index(root)?;
    let mut bytes = 0;
    let times = repeat(runs.max(5), || {
        let map = atlas.project(MAP_LIMIT);
        bytes = map.len();
        bytes
    });
    emit(&format!("atlas.{label}.project"), &times, Some(bytes));

    let mut atlas = atlas;
    let times = repeat(runs.max(5), || {
        atlas.refresh(root).map(|s| s.parsed).unwrap_or(0)
    });
    emit(&format!("atlas.{label}.refresh_noop"), &times, None);

    // The engine's path: a refresh through the publish point, which works on a
    // private copy of the graph.
    let shared = SharedAtlas::index(root)?;
    let times = repeat(runs.max(5), || {
        shared.refresh(root).map(|s| s.parsed).unwrap_or(0)
    });
    emit(&format!("atlas.{label}.refresh_shared_noop"), &times, None);

    // One edited file: a probe file in the tree's root that is rewritten with
    // growing content, so its size changes and the stat gate cannot clear it,
    // and removed at the end.
    let scratch = root.join("bench_scratch_probe.rs");
    let mut tick = 0usize;
    let mut edit = |tick: usize| -> std::io::Result<()> {
        let mut body = String::new();
        for i in 0..=tick {
            let _ = writeln!(body, "pub fn probe_{i}(x: u32) -> u32 {{ x + {i} }}");
        }
        fs::write(&scratch, body)
    };
    edit(0)?;
    atlas.refresh(root)?;
    let times = repeat(runs.max(5), || {
        tick += 1;
        edit(tick).ok();
        atlas.refresh(root).map(|s| s.parsed).unwrap_or(0)
    });
    emit(&format!("atlas.{label}.refresh_one_edit"), &times, None);
    let times = repeat(runs.max(5), || {
        tick += 1;
        edit(tick).ok();
        atlas
            .apply_changes(&["bench_scratch_probe.rs".to_owned()])
            .map(|s| s.parsed)
            .unwrap_or(0)
    });
    emit(&format!("atlas.{label}.apply_changes_one"), &times, None);
    let _ = fs::remove_file(&scratch);
    Ok(())
}

/// Calls `work` once to warm caches, then `runs` times; milliseconds each.
fn repeat<T>(runs: usize, mut work: impl FnMut() -> T) -> Vec<f64> {
    std::hint::black_box(work());
    (0..runs)
        .map(|_| {
            let start = Instant::now();
            std::hint::black_box(work());
            start.elapsed().as_secs_f64() * 1e3
        })
        .collect()
}

fn emit(name: &str, samples: &[f64], bytes: Option<usize>) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
    let mut row = format!(
        "{{\"name\":\"{name}\",\"n\":{},\"min_ms\":{:.3},\"median_ms\":{:.3},\"p95_ms\":{:.3}",
        sorted.len(),
        sorted[0],
        at(0.5),
        at(0.95)
    );
    if let Some(bytes) = bytes {
        let _ = write!(row, ",\"bytes\":{bytes}");
    }
    println!("{row}}}");
}

/// `files` source files in 50 directories, half TypeScript and half Rust,
/// each importing a few others so the graph has edges to rank.
fn write_synthetic(root: &Path, files: usize) -> std::io::Result<()> {
    let dirs = 50;
    let per_dir = files / dirs;
    for d in 0..dirs {
        fs::create_dir_all(root.join(format!("pkg_{d}")))?;
    }
    for d in 0..dirs {
        for f in 0..per_dir {
            let mut body = String::new();
            let ts = f % 2 == 0;
            for k in 1..=3 {
                let (td, tf) = ((d + k) % dirs, (f * 7 + k * 13) % per_dir);
                if ts {
                    if tf % 2 == 0 {
                        let _ = writeln!(
                            body,
                            "import {{ fn_{td}_{tf} }} from \"../pkg_{td}/file_{tf}\";"
                        );
                    }
                } else if tf % 2 == 1 {
                    let _ = writeln!(body, "use crate::pkg_{td}::file_{tf}::fn_{td}_{tf};");
                }
            }
            for k in 0..6 {
                if ts {
                    let _ = writeln!(
                        body,
                        "export function fn_{d}_{f}{} (a: number, b: string): string {{\n  return b + a + {k};\n}}",
                        if k == 0 {
                            String::new()
                        } else {
                            format!("_{k}")
                        }
                    );
                    let _ = writeln!(
                        body,
                        "export class Type_{d}_{f}_{k} {{ value = {k}; method() {{ return this.value; }} }}"
                    );
                } else {
                    let _ = writeln!(
                        body,
                        "pub fn fn_{d}_{f}{}(a: u32, b: &str) -> String {{\n    format!(\"{{}}{{}}\", a, b)\n}}",
                        if k == 0 {
                            String::new()
                        } else {
                            format!("_{k}")
                        }
                    );
                    let _ = writeln!(body, "pub struct Type_{d}_{f}_{k} {{ pub value: u32 }}");
                }
            }
            let ext = if ts { "ts" } else { "rs" };
            fs::write(root.join(format!("pkg_{d}/file_{f}.{ext}")), body)?;
        }
    }
    Ok(())
}
