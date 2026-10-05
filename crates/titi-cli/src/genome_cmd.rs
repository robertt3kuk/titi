//! `titi genome` — turn the prompt map on and off, cap it, and report status,
//! before the engine starts and without a model key.
//!
//! The same facts the chat's `/genome` note states print here as plain lines,
//! so a user who cannot reach the chat still sees why the map is on or off.
//! `genome.limit` is a separate key and this never touches it: enabling and
//! capping are one verb each, not one decision.

use std::path::Path;

use titi_config::settings::{GENOME_ENABLED_KEY, GENOME_LIMIT_KEY, Settings};

/// The exit every refusal uses: unknown subcommand and an out-of-range limit.
/// A failed `check`/`lsp` exits 1 instead: the command was understood, the
/// workspace was not.
const USAGE_EXIT: i32 = 2;

pub fn run(agent_dir: &Path, workspace: &Path) -> Option<()> {
    let mut words = std::env::args().skip(1);
    if words.next().as_deref() != Some("genome") {
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
                    GENOME_LIMIT_KEY,
                    serde_json::json!(n),
                    &format!("genome limit: {n}"),
                );
            }
            // Out of range or not an integer: refused, and nothing written,
            // so a typo cannot seed `genome.limit` with a value every later
            // run has to ignore.
            _ => {
                eprintln!("genome limit: expected an integer from 1 to 64");
                std::process::exit(USAGE_EXIT);
            }
        },
        Some("check") => {
            check_cmd(workspace);
        }
        Some("lsp") => {
            // The blocks here are the server: a client is on the other side
            // of the pipe, so nothing prints before the frames.
            if let Err(reason) =
                titi_genome::serve_lsp(workspace, std::io::stdin().lock(), std::io::stdout())
            {
                eprintln!("genome: lsp failed ({reason})");
                std::process::exit(1);
            }
        }
        Some(name) => {
            eprintln!("genome: unknown command {name}");
            eprintln!("usage: titi genome [on|off|limit <n>|check|lsp]");
            std::process::exit(USAGE_EXIT);
        }
    }
}

/// The four status lines, one formatter shared with the chat's `/genome` so
/// the two surfaces cannot drift.
fn status(settings: &Option<Settings>, agent_dir: &Path) {
    println!("{}", crate::engine::genome_note(settings, agent_dir));
    std::process::exit(0);
}

/// `titi genome check`: index the workspace, print one line per diagnostic.
///
/// `path:line: code: message`, one line each; a clean tree says so and exits
/// 0, any diagnostic exits 1, and an index that cannot even be built blames
/// itself rather than reporting phantom clean trees.
fn check_cmd(workspace: &Path) {
    let genome = match titi_genome::Genome::index(workspace) {
        Ok(genome) => genome,
        Err(reason) => {
            eprintln!("genome: check failed ({reason})");
            std::process::exit(1);
        }
    };
    let diagnostics = genome.check();
    if diagnostics.is_empty() {
        println!("genome: clean");
        std::process::exit(0);
    }
    for diagnostic in &diagnostics {
        println!(
            "{}:{}: {}: {}",
            diagnostic.path, diagnostic.line, diagnostic.code, diagnostic.message
        );
    }
    std::process::exit(1);
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
        GENOME_ENABLED_KEY,
        serde_json::json!(enabled),
        if enabled { "genome: on" } else { "genome: off" },
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
            eprintln!("genome: not saved ({why})");
            std::process::exit(1);
        }
    }
}
