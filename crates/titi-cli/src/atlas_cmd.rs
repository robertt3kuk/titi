//! `titi atlas` — turn the prompt map on and off, cap it, and report status,
//! before the engine starts and without a model key.
//!
//! The same facts the chat's `/atlas` note states print here as plain lines,
//! so a user who cannot reach the chat still sees why the map is on or off.
//! `atlas.limit` is a separate key and this never touches it: enabling and
//! capping are one verb each, not one decision.
//!
//! Two reports go past the switches. `check` is a vetter: the findings in the
//! workspace, one line each, exit 1 on a Warning or Error. `capabilities` is
//! reference: what level each language's exports rest on, on demand. They are
//! separate verbs because a listing is not a finding, and a vetter that also
//! narrated its own reach could no longer say `atlas: clean`.

use std::path::Path;

use titi_config::settings::{ATLAS_ENABLED_KEY, ATLAS_LIMIT_KEY, Settings};

/// The exit every refusal uses: unknown subcommand and an out-of-range limit.
/// A failed `check`/`lsp` exits 1 instead: the command was understood, the
/// workspace was not.
const USAGE_EXIT: i32 = 2;

pub fn run(agent_dir: &Path, workspace: &Path) -> Option<()> {
    let mut words = std::env::args().skip(1);
    if words.next().as_deref() != Some("atlas") {
        return None;
    }
    dispatch(agent_dir, workspace, words.collect());
    Some(())
}

fn dispatch(agent_dir: &Path, workspace: &Path, args: Vec<String>) {
    let settings = titi_config::settings::Settings::load(agent_dir, workspace, &[]).ok();
    match args.first().map(String::as_str) {
        None => status(&settings, agent_dir),
        Some("on") => write_enabled(agent_dir, workspace, true),
        Some("off") => write_enabled(agent_dir, workspace, false),
        Some("limit") => match args.get(1).map(|raw| raw.parse::<i64>()) {
            Some(Ok(n)) if (1..=64).contains(&n) => {
                save_setting(
                    agent_dir,
                    workspace,
                    ATLAS_LIMIT_KEY,
                    serde_json::json!(n),
                    &format!("atlas limit: {n}"),
                );
            }
            // Out of range or not an integer: refused, and nothing written,
            // so a typo cannot seed `atlas.limit` with a value every later
            // run has to ignore.
            _ => {
                eprintln!("atlas limit: expected an integer from 1 to 64");
                std::process::exit(USAGE_EXIT);
            }
        },
        Some("check") => {
            check_cmd(workspace);
        }
        Some("dump") => {
            dump_cmd(workspace, &args[1..]);
        }
        Some("lens") => {
            lens_cmd(workspace, &args[1..]);
        }
        Some("capabilities") => {
            capabilities_cmd();
        }
        Some("lsp") => {
            // The blocks here are the server: a client is on the other side
            // of the pipe, so nothing prints before the frames.
            if let Err(reason) =
                titi_atlas::serve_lsp(workspace, std::io::stdin().lock(), std::io::stdout())
            {
                eprintln!("atlas: lsp failed ({reason})");
                std::process::exit(1);
            }
        }
        Some(name) => {
            eprintln!("atlas: unknown command {name}");
            eprintln!("usage: titi atlas [on|off|limit <n>|check|dump|lens|capabilities|lsp]");
            std::process::exit(USAGE_EXIT);
        }
    }
}

/// The four status lines, one formatter shared with the chat's `/atlas` so
/// the two surfaces cannot drift.
fn status(settings: &Option<Settings>, agent_dir: &Path) {
    println!("{}", crate::engine::atlas_note(settings, agent_dir));
    std::process::exit(0);
}

/// Diagnostic lines `check` prints before it says how many it left out.
const CHECK_LINES: usize = 200;

/// `titi atlas check`: index the workspace, print one line per diagnostic.
///
/// `path:line: code: message`, one line each; severity decides the exit: a
/// clean tree, or one whose only diagnostics are informational (an ambiguous
/// name is a hint about name-only resolution, not a defect), exits 0; a
/// Warning or Error exits 1. An index that cannot even be built blames
/// itself rather than reporting phantom clean trees.
fn check_cmd(workspace: &Path) {
    let atlas = match titi_atlas::Atlas::index(workspace) {
        Ok(atlas) => atlas,
        Err(reason) => {
            eprintln!("atlas: check failed ({reason})");
            std::process::exit(1);
        }
    };
    let diagnostics = atlas.check();
    if diagnostics.is_empty() {
        println!("atlas: clean");
        std::process::exit(0);
    }
    for line in titi_atlas::format_check_report(&diagnostics, CHECK_LINES) {
        println!("{line}");
    }
    let has_problem = diagnostics
        .iter()
        .any(|item| !matches!(item.severity, titi_atlas::Severity::Info));
    std::process::exit(i32::from(has_problem));
}

/// `titi atlas capabilities`: one line per language this index understands —
/// its level, the extensions it answers to, and what that level costs.
///
/// A verb of its own rather than `check --capabilities`. `check` is a vetter:
/// its exit code and its one-line `atlas: clean` answer are about findings,
/// and a reference listing is not a finding — mixing them either floods the
/// findings or makes `clean` unreachable. The roster is a property of the
/// build, so this reads no directory and cannot fail on a workspace it cannot
/// walk, unlike `check` and `lsp`.
fn capabilities_cmd() {
    let roster = titi_atlas::Atlas::capabilities();
    let extensions = |capability: &titi_atlas::Capability| {
        capability
            .extensions
            .iter()
            .map(|extension| format!(".{extension}"))
            .collect::<Vec<String>>()
            .join(" ")
    };
    let width = |field: &dyn Fn(&titi_atlas::Capability) -> String| {
        roster
            .iter()
            .map(|capability| field(capability).chars().count())
            .max()
            .unwrap_or(0)
    };
    let (language_width, level_width) = (
        width(&|capability| capability.language.to_owned()),
        width(&|capability| capability.level.as_str().to_owned()),
    );
    let extension_width = width(&|capability| extensions(capability));
    for capability in &roster {
        println!(
            "{:<language_width$} {:<level_width$} {:<extension_width$} {}",
            capability.language,
            capability.level.as_str(),
            extensions(capability),
            capability.note,
        );
    }
    let full = roster
        .iter()
        .filter(|capability| matches!(capability.level, titi_atlas::Level::Full))
        .count();
    println!(
        "{} languages: {full} full, {} heuristic",
        roster.len(),
        roster.len() - full,
    );
    std::process::exit(0);
}

/// Writes the boolean to the agent's own config: the canonical global file,
/// never the project's `.titi`, which this API cannot write.
///
/// A load that failed — a quarantined config, say — still writes, on a fresh
/// view whose target is the agent's `config.yml`: a broken file puts the user
/// one switch away from a working one rather than behind it. The write error
/// and the load failure are named together, so a `not saved` never hides why
/// the load was odd too.
fn write_enabled(agent_dir: &Path, workspace: &Path, enabled: bool) {
    save_setting(
        agent_dir,
        workspace,
        ATLAS_ENABLED_KEY,
        serde_json::json!(enabled),
        if enabled { "atlas: on" } else { "atlas: off" },
    );
}

fn save_setting(
    agent_dir: &Path,
    workspace: &Path,
    key: &str,
    value: serde_json::Value,
    line: &str,
) {
    // A load that failed — a quarantined config, say — still writes, on a
    // fresh view whose target is the agent's own `config.yml`: a broken file
    // must not wedge the switch behind it.
    let mut settings = titi_config::settings::Settings::load(agent_dir, workspace, &[])
        .unwrap_or_else(|_| {
            let mut fresh = Settings::default();
            fresh.global_path = agent_dir.join("config.yml");
            fresh
        });
    match settings.set(key, value) {
        Ok(()) => println!("{line}"),
        Err(why) => {
            eprintln!("atlas: not saved ({why})");
            std::process::exit(1);
        }
    }
}

/// `titi atlas lens (--task <text> | --task-file <path>) [--touched a,b]
/// [--changed a,b] [--tokens <n>] [--json]`: the files a task is about, read-only.
///
/// The same seeds and walk the turn runs before each prompt, for a task given
/// on the command line: what an evaluation replays and what a person can ask
/// "what would the model have been shown for this prompt". Plain output is the
/// block the model reads; `--json` is the ranked entries with their reasons.
fn lens_cmd(workspace: &Path, args: &[String]) {
    let mut task = None;
    let mut touched: Vec<String> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    let mut tokens = 400usize;
    let mut files = titi_atlas::MAX_LENS_FILES;
    let mut json = false;
    let usage = "usage: titi atlas lens (--task <text> | --task-file <path>) [--touched a,b] [--changed a,b] [--tokens <n>] [--files <n>] [--json]";
    let mut words = args.iter();
    while let Some(word) = words.next() {
        let mut value = |what: &str| -> String {
            match words.next() {
                Some(value) => value.clone(),
                None => {
                    eprintln!("atlas lens: {what} expects a value");
                    eprintln!("{usage}");
                    std::process::exit(USAGE_EXIT);
                }
            }
        };
        match word.as_str() {
            "--json" => json = true,
            "--task" => task = Some(value("--task")),
            "--task-file" => {
                let path = value("--task-file");
                match std::fs::read_to_string(&path) {
                    Ok(text) => task = Some(text),
                    Err(reason) => {
                        eprintln!("atlas lens: cannot read {path} ({reason})");
                        std::process::exit(USAGE_EXIT);
                    }
                }
            }
            "--touched" => touched = value("--touched").split(',').map(str::to_owned).collect(),
            "--changed" => changed = value("--changed").split(',').map(str::to_owned).collect(),
            "--tokens" => match value("--tokens").parse::<usize>() {
                Ok(n) if (40..=4000).contains(&n) => tokens = n,
                _ => {
                    eprintln!("atlas lens: --tokens expects an integer from 40 to 4000");
                    std::process::exit(USAGE_EXIT);
                }
            },
            "--files" => match value("--files").parse::<usize>() {
                Ok(n) if (1..=64).contains(&n) => files = n,
                _ => {
                    eprintln!("atlas lens: --files expects an integer from 1 to 64");
                    std::process::exit(USAGE_EXIT);
                }
            },
            other => {
                eprintln!("atlas lens: unknown argument {other}");
                eprintln!("{usage}");
                std::process::exit(USAGE_EXIT);
            }
        }
    }
    let Some(task) = task else {
        eprintln!("{usage}");
        std::process::exit(USAGE_EXIT);
    };
    let atlas = match titi_atlas::Atlas::index(workspace) {
        Ok(atlas) => atlas,
        Err(reason) => {
            eprintln!("atlas: lens failed ({reason})");
            std::process::exit(1);
        }
    };
    let seeds = atlas.seeds(&titi_atlas::SeedInput {
        prompt: &task,
        touched: &touched,
        changed: &changed,
    });
    let lens = atlas.lens(&seeds, titi_atlas::LensBudget { tokens, files });
    if json {
        let entries: Vec<serde_json::Value> = lens
            .entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "path": entry.path,
                    "score": entry.score,
                    "why": entry.why,
                    "dependents": entry.dependents,
                    "symbols": entry.symbols,
                })
            })
            .collect();
        let paths: Vec<&str> = lens
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect();
        // What the model holds when the lens rides on the prompt map: the lens
        // files first, then the map's own order for the rest.
        let mut union: Vec<&str> = paths.clone();
        for entry in titi_atlas::ranking(&atlas, &std::collections::HashSet::new()) {
            if union.len() >= files {
                break;
            }
            if !union.contains(&entry.path) {
                union.push(entry.path);
            }
        }
        println!(
            "{}",
            serde_json::json!({
                "paths": paths,
                "union": union,
                "entries": entries,
                "unmapped": lens.unmapped,
            })
        );
    } else if lens.is_empty() {
        println!("atlas: no seeds in that task");
    } else {
        println!("{}", lens.render(0));
    }
}

/// `titi atlas dump [--json] [--limit <n>]`: the whole index, read-only.
///
/// One entry per file in the order the prompt map lists it, with the facts the
/// ranking rests on: role, rank, the map's sort score, dependents, exports and
/// the edges (imports, re-exports, resolved uses). Plain output is one line per
/// file; `--json` is one object with the files and the map text for `--limit`
/// (default 24, the prompt's), which is what an evaluation replays.
fn dump_cmd(workspace: &Path, args: &[String]) {
    let mut json = false;
    let mut limit = 24usize;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--json" => json = true,
            "--limit" => match words.next().and_then(|raw| raw.parse::<usize>().ok()) {
                Some(n) if (1..=64).contains(&n) => limit = n,
                _ => {
                    eprintln!("atlas dump: --limit expects an integer from 1 to 64");
                    std::process::exit(USAGE_EXIT);
                }
            },
            other => {
                eprintln!("atlas dump: unknown argument {other}");
                eprintln!("usage: titi atlas dump [--json] [--limit <n>]");
                std::process::exit(USAGE_EXIT);
            }
        }
    }
    let atlas = match titi_atlas::Atlas::index(workspace) {
        Ok(atlas) => atlas,
        Err(reason) => {
            eprintln!("atlas: dump failed ({reason})");
            std::process::exit(1);
        }
    };
    let ranked = titi_atlas::ranking(&atlas, &std::collections::HashSet::new());
    let dependents = |path: &str| atlas.dependents.get(path).copied().unwrap_or(0);
    if !json {
        for (order, entry) in ranked.iter().enumerate() {
            let Some(file) = atlas.files.get(entry.path) else {
                continue;
            };
            println!(
                "{} {} {} rank={:.6} deps={} exports={} imports={}",
                order + 1,
                entry.path,
                entry.role,
                entry.rank,
                dependents(entry.path),
                file.exports.len(),
                file.imports.len()
            );
        }
        return;
    }
    let files: Vec<serde_json::Value> = ranked
        .iter()
        .enumerate()
        .filter_map(|(order, entry)| {
            let file = atlas.files.get(entry.path)?;
            let uses: Vec<serde_json::Value> = file
                .uses
                .iter()
                .map(|u| serde_json::json!([u.name, u.definer, format!("{:?}", u.basis)]))
                .collect();
            Some(serde_json::json!({
                "path": entry.path,
                "order": order + 1,
                "role": entry.role,
                "rank": entry.rank,
                "score": entry.score,
                "dependents": dependents(entry.path),
                "exports": file.exports,
                "imports": file.imports,
                "reexports": file.reexports,
                "inherits": file.inherits,
                "uses": uses,
            }))
        })
        .collect();
    let out = serde_json::json!({
        "files": files,
        "map": { "limit": limit, "text": atlas.project(limit) },
    });
    println!("{out}");
}
