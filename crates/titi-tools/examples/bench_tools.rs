//! Tool benchmark: `cargo run --release -p titi-tools --example bench_tools -- [--quick] [repo-root]`
//!
//! Calls the workspace tools through their public `ToolHandler` interface on
//! `repo-root` and on a synthetic 5k-file tree. One JSON object per line on
//! stdout: `{"name", "n", "min_ms", "median_ms", "p95_ms", "bytes"}`, where
//! `bytes` is the size of the answer the model would be sent.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::Instant;

use serde_json::{Value, json};
use titi_tools::workspace_tools;

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
    let runs = if quick { 5 } else { 15 };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()?;
    let root = root.unwrap_or_else(|| ".".to_owned());
    runtime.block_on(bench_tree(
        "repo",
        Path::new(&root),
        "crates/titi-engine/src/runtime.rs",
        runs,
    ));

    let synth = tempfile::tempdir()?;
    write_synthetic(synth.path(), 5_000)?;
    runtime.block_on(bench_tree(
        "synth5k",
        synth.path(),
        "pkg_3/file_10.ts",
        runs,
    ));
    Ok(())
}

async fn bench_tree(label: &str, root: &Path, read_path: &str, runs: usize) {
    let cases: [(&str, &str, Value); 8] = [
        ("glob_rs", "glob", json!({"pattern": "**/*.rs"})),
        ("glob_name", "glob", json!({"pattern": "*.toml"})),
        ("grep_rare", "grep", json!({"pattern": "fn_3_10_0\\("})),
        (
            "grep_common",
            "grep",
            json!({"pattern": "pub (fn|struct)", "glob": "*.rs"}),
        ),
        // Every file is read and nothing matches: the cost of reading and
        // matching alone.
        (
            "grep_miss",
            "grep",
            json!({"pattern": "zz_no_such_text_zz"}),
        ),
        // Hits far past the 500-line cap, so the cap logic is on the path.
        ("grep_dense", "grep", json!({"pattern": "fn|let|use"})),
        (
            "grep_nocase",
            "grep",
            json!({"pattern": "RETURN", "ignore_case": true}),
        ),
        ("read_cold", "read", json!({"path": read_path})),
    ];
    for (name, tool, args) in cases {
        let mut samples = Vec::with_capacity(runs);
        let mut bytes = 0;
        for _ in 0..runs + 1 {
            // A fresh registry per call, so `read` never answers from the
            // shared cache and every sample is the cold path.
            let tools = workspace_tools(root);
            let handler = tools
                .iter()
                .find(|handler| handler.definition().spec.name == tool);
            let Some(handler) = handler else { return };
            let start = Instant::now();
            let result = handler.invoke(args.clone()).await;
            samples.push(start.elapsed().as_secs_f64() * 1e3);
            bytes = result.output.len();
        }
        samples.remove(0);
        emit(&format!("tools.{label}.{name}"), &samples, bytes);
    }

    // The warm read: one registry, the same file again.
    let tools = workspace_tools(root);
    if let Some(handler) = tools
        .iter()
        .find(|handler| handler.definition().spec.name == "read")
    {
        let args = json!({"path": read_path});
        handler.invoke(args.clone()).await;
        let mut samples = Vec::with_capacity(runs);
        let mut bytes = 0;
        for _ in 0..runs {
            let start = Instant::now();
            let result = handler.invoke(args.clone()).await;
            samples.push(start.elapsed().as_secs_f64() * 1e3);
            bytes = result.output.len();
        }
        emit(&format!("tools.{label}.read_warm"), &samples, bytes);
    }
}

fn emit(name: &str, samples: &[f64], bytes: usize) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
    println!(
        "{{\"name\":\"{name}\",\"n\":{},\"min_ms\":{:.3},\"median_ms\":{:.3},\"p95_ms\":{:.3},\"bytes\":{bytes}}}",
        sorted.len(),
        sorted[0],
        at(0.5),
        at(0.95)
    );
}

/// `files` files in 50 directories, half TypeScript and half Rust.
fn write_synthetic(root: &Path, files: usize) -> std::io::Result<()> {
    let dirs = 50;
    let per_dir = files / dirs;
    for d in 0..dirs {
        fs::create_dir_all(root.join(format!("pkg_{d}")))?;
        for f in 0..per_dir {
            let ts = f % 2 == 0;
            let mut body = String::new();
            for k in 0..6 {
                if ts {
                    let _ = writeln!(
                        body,
                        "export function fn_{d}_{f}_{k}(a: number, b: string): string {{\n  return b + a;\n}}"
                    );
                } else {
                    let _ = writeln!(
                        body,
                        "pub fn fn_{d}_{f}_{k}(a: u32, b: &str) -> String {{\n    format!(\"{{}}{{}}\", a, b)\n}}\npub struct Type_{d}_{f}_{k} {{ pub value: u32 }}"
                    );
                }
            }
            let ext = if ts { "ts" } else { "rs" };
            fs::write(root.join(format!("pkg_{d}/file_{f}.{ext}")), body)?;
        }
    }
    Ok(())
}
