//! Markdown render benchmark: `cargo run --release -p titi-tui --example bench_render -- [--quick]`
//!
//! A synthetic long session — prose, headings, lists, fenced code, tables —
//! rendered through the public markdown renderer at 120 columns. What it
//! answers: the cost of rendering one message, the cost of rendering the
//! whole session once (what a screen pays per frame if it keeps no cache of
//! rendered rows), and the cost of the rows one 120x40 frame needs (the newest
//! messages that fill 40 rows). One JSON object per line on stdout.

use std::fmt::Write as _;
use std::time::Instant;

use titi_tui::chart::AutoGraph;
use titi_tui::markdown::render_markdown;
use titi_tui::theme::ColorMode;
use titi_tui::theme::loader::{CreateThemeOptions, load_theme};

const WIDTH: u16 = 120;
const FRAME_ROWS: usize = 40;

fn main() -> Result<(), String> {
    let quick = std::env::args().any(|arg| arg == "--quick");
    let messages: usize = if quick { 500 } else { 2_000 };
    let runs = if quick { 3 } else { 7 };
    let theme = load_theme(
        "titanium",
        &CreateThemeOptions {
            mode: Some(ColorMode::Truecolor),
            ..CreateThemeOptions::default()
        },
    )?;

    let session: Vec<String> = (0..messages).map(message).collect();
    let bytes: usize = session.iter().map(String::len).sum();
    let render = |text: &str| render_markdown(text, &theme, WIDTH, true, AutoGraph::Always);

    let mut rows = 0;
    let all = repeat(runs, || {
        rows = session.iter().map(|text| render(text).len()).sum::<usize>();
        rows
    });
    emit("render.session_full", &all, Some(bytes), Some(rows));

    // One message each: the per-message distribution, not the total.
    let mut per_message = Vec::with_capacity(session.len());
    for text in &session {
        let start = Instant::now();
        std::hint::black_box(render(text));
        per_message.push(start.elapsed().as_secs_f64() * 1e3);
    }
    emit("render.per_message", &per_message, None, None);

    // The rows one frame needs: render from the newest message back until 40
    // rows are covered.
    let frame = repeat(runs.max(5), || {
        let mut covered = 0;
        for text in session.iter().rev() {
            covered += render(text).len();
            if covered >= FRAME_ROWS {
                break;
            }
        }
        covered
    });
    emit("render.frame_tail", &frame, None, None);

    // One very long message, as a streamed answer grows: re-rendering it from
    // scratch on every delta is what a streaming reply costs per token.
    let mut growing = String::new();
    let mut deltas = Vec::new();
    for i in 0..300 {
        let _ = writeln!(
            growing,
            "Step {i}: **update** the `config_{i}` value and re-run `cargo test -p crate_{i}`; the result is stable."
        );
        if i % 30 == 29 {
            let _ = writeln!(growing, "\n```rust\nfn step_{i}() -> u32 {{ {i} }}\n```\n");
        }
        let start = Instant::now();
        std::hint::black_box(render(&growing));
        deltas.push(start.elapsed().as_secs_f64() * 1e3);
    }
    emit(
        "render.stream_delta_300",
        &deltas,
        Some(growing.len()),
        None,
    );
    Ok(())
}

/// A message of one of six shapes, sized like real answers.
fn message(i: usize) -> String {
    let mut out = String::new();
    match i % 6 {
        0 => {
            let _ = write!(
                out,
                "Looking at `module_{i}` the **parser** reads the input twice: once to \
                 find the delimiters and once to build the tree. That is why the \
                 profile shows two passes over the same bytes, and why the fix is to \
                 hand the first pass's offsets to the second. See *the notes* below."
            );
        }
        1 => {
            let _ = write!(
                out,
                "## Plan {i}\n\n1. Read `src/lib_{i}.rs`\n2. Change the **error** type\n3. Run the tests\n\n- keep the API\n- drop the shim\n- update the docs"
            );
        }
        2 => {
            let _ = write!(
                out,
                "```rust\npub fn handler_{i}(input: &str) -> Result<Vec<u32>, Error> {{\n    let mut out = Vec::new();\n    for (n, line) in input.lines().enumerate() {{\n        if line.starts_with('#') {{ continue; }}\n        out.push(line.parse::<u32>().map_err(|e| Error::Parse(n, e))?);\n    }}\n    Ok(out)\n}}\n```"
            );
        }
        3 => {
            let _ = write!(
                out,
                "| step | time | notes |\n|---|---:|---|\n| build {i} | 120 ms | cached |\n| test | 45 ms | `cargo test` |\n| lint | 8 ms | **clean** |\n| fmt | 2 ms | ok |"
            );
        }
        4 => {
            for k in 0..4 {
                let _ = write!(
                    out,
                    "Paragraph {k} of message {i}: the engine assembles a request from the \
                     system prompt, the tool specs and the history, then streams the \
                     answer back token by token.\n\n"
                );
            }
        }
        _ => {
            let _ = write!(out, "Done. `{i}` files changed.");
        }
    }
    out
}

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

fn emit(name: &str, samples: &[f64], bytes: Option<usize>, rows: Option<usize>) {
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
    if let Some(rows) = rows {
        let _ = write!(row, ",\"rows\":{rows}");
    }
    println!("{row}}}");
}
