//! `titi` — terminal coding agent.
//!
//! Headless JSONL and one full-screen chat share the engine. The screen is
//! ratatui; it sends `EngineCommand` and paints `EngineEvent`.

use std::io;

const USAGE: &str = "\
usage: titi [options]

  --headless, -p [prompt]     read EngineCommand JSONL on stdin, or run one prompt
  --prompt <text>             run one prompt headless and print the reply
  --goal <text>               run the coder/reviewer goal loop headless and exit
                              0 (pass), 1 (partial) or 3 (fail) for CI
  --approval <mode>           always-ask | write | yolo (default: write)
  --mode <mode>               agent | plan | duck (default: agent)
                              plan: read-only tools; duck: repo-blind chat
  --record <path.ompcast>     record this session's events to a cast file
  --replay <path.ompcast>     play a recorded session and exit; no model runs
  --replay-fast               with --replay: no pauses between records
  --mouse <preset>            accepted, ignored (off | on | wheel | buttons | all)
  --set-key <provider> <key>  store an API key in the agent directory
  --list-keys                 list stored providers (never the keys)
  --help, -h                  this text

In the chat: Enter sends, and steers while a turn is running. Ctrl+C stops
the turn; press it twice to leave. y / n answers a write or a shell prompt.
/model switches to the next model that has a key. /login stores a key.
A / at the start of the line lists commands; up and down move, tab fills.
";

fn main() -> io::Result<()> {
    let mut headless = false;
    let mut prompt: Option<String> = None;
    let mut goal: Option<String> = None;
    let mut set_key: Option<(String, String)> = None;
    let mut list_keys = false;
    let mut record: Option<std::path::PathBuf> = None;
    let mut replay: Option<std::path::PathBuf> = None;
    let mut replay_fast = false;
    let mut approval = titi_tools::ApprovalMode::Write;
    let mut mode = titi_engine::protocol::SessionMode::Agent;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--mouse" {
            // Kept so older scripts still parse. The chat does not track the mouse.
            let Some(raw) = args.next() else {
                eprintln!("usage: titi --mouse <off|on|wheel|buttons|all>");
                std::process::exit(2);
            };
            if titi_tui::caps::MousePreset::parse(&raw).is_none() {
                eprintln!("unknown mouse preset {raw}\n\n{USAGE}");
                std::process::exit(2);
            }
        } else if arg == "--headless" || arg == "-p" {
            headless = true;
        } else if arg == "--prompt" {
            let Some(text) = args.next() else {
                eprintln!("usage: titi --prompt <text>");
                std::process::exit(2);
            };
            prompt = Some(text);
            headless = true;
        } else if arg == "--goal" {
            let Some(text) = args.next() else {
                eprintln!("usage: titi --goal <text>");
                std::process::exit(2);
            };
            goal = Some(text);
            headless = true;
        } else if arg == "--set-key" {
            match (args.next(), args.next()) {
                (Some(provider), Some(key)) => set_key = Some((provider, key)),
                _ => {
                    eprintln!("usage: titi --set-key <provider> <key>");
                    std::process::exit(2);
                }
            }
        } else if arg == "--list-keys" {
            list_keys = true;
        } else if arg == "--record" {
            let Some(path) = args.next() else {
                eprintln!("usage: titi --record <path.ompcast>");
                std::process::exit(2);
            };
            record = Some(std::path::PathBuf::from(path));
        } else if arg == "--replay" {
            let Some(path) = args.next() else {
                eprintln!("usage: titi --replay <path.ompcast>");
                std::process::exit(2);
            };
            replay = Some(std::path::PathBuf::from(path));
        } else if arg == "--replay-fast" {
            replay_fast = true;
        } else if arg == "--approval" {
            let Some(raw) = args.next() else {
                eprintln!("usage: titi --approval <always-ask|write|yolo>");
                std::process::exit(2);
            };
            match titi_cli::engine::parse_approval(&raw) {
                Ok(mode) => approval = mode,
                Err(reason) => {
                    eprintln!("{reason}");
                    std::process::exit(2);
                }
            }
        } else if arg == "--mode" {
            let Some(raw) = args.next() else {
                eprintln!("usage: titi --mode <agent|plan|duck>");
                std::process::exit(2);
            };
            match raw.as_str() {
                "agent" => mode = titi_engine::protocol::SessionMode::Agent,
                "plan" => mode = titi_engine::protocol::SessionMode::Plan,
                "duck" => mode = titi_engine::protocol::SessionMode::Duck,
                other => {
                    eprintln!("unknown mode {other}\n\n{USAGE}");
                    std::process::exit(2);
                }
            }
        } else if arg == "--help" || arg == "-h" {
            println!("{USAGE}");
            return Ok(());
        } else if headless && goal.is_none() && prompt.is_none() && !arg.starts_with('-') {
            prompt = Some(arg);
        } else if arg.starts_with('-') {
            eprintln!("unknown option {arg}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
    if goal.is_some() && prompt.is_some() {
        eprintln!("use --goal or a prompt, not both\n\n{USAGE}");
        std::process::exit(2);
    }

    if let Some((provider, key)) = set_key {
        return match titi_cli::secrets::store_key(&titi_config::agent_dir(), &provider, &key) {
            Ok(()) => {
                eprintln!("stored an API key for {provider}");
                Ok(())
            }
            Err(reason) => {
                eprintln!("not stored: {reason}");
                std::process::exit(1);
            }
        };
    }
    if list_keys {
        return match titi_cli::secrets::list_keys(&titi_config::agent_dir()) {
            Ok(keys) if keys.is_empty() => {
                eprintln!("no stored keys");
                Ok(())
            }
            Ok(keys) => {
                for key in keys {
                    eprintln!("{}  ({})", key.provider, key.kind);
                }
                Ok(())
            }
            Err(reason) => {
                eprintln!("could not read keys: {reason}");
                std::process::exit(1);
            }
        };
    }
    // A replay is a file and a screen: it never starts the engine, so it runs
    // with no key, no network and no tools.
    if let Some(path) = replay {
        let pace = if replay_fast {
            titi_cli::ompcast::Pace::Fast
        } else {
            titi_cli::ompcast::Pace::Realtime
        };
        let mut out = io::stdout();
        return match titi_cli::ompcast::replay(&path, pace, &mut out) {
            Ok(()) => Ok(()),
            Err(error) => {
                eprintln!("replay failed: {error}");
                std::process::exit(1);
            }
        };
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;
    let _enter = runtime.enter();

    let (engine, models, session_id) =
        titi_cli::engine::start_engine_with(approval, mode).map_err(io::Error::other)?;
    let session_log =
        titi_cli::session_log::SessionLog::open(&titi_config::agent_dir(), &session_id);
    if session_log.is_none() {
        eprintln!("session: transcript writes are off (store unavailable)");
    }
    if headless && record.is_some() {
        // A cast is what a screen showed; a headless run has no screen, and
        // silently writing an empty file would read as a recording.
        eprintln!(
            "record: --record needs the chat screen; this run is headless and is not recorded"
        );
    }
    if headless {
        let code = match (goal, prompt) {
            (Some(text), _) => {
                runtime.block_on(titi_cli::headless::run_goal(engine, session_log, &text))?
            }
            (None, Some(text)) => {
                runtime.block_on(titi_cli::headless::run_prompt(engine, session_log, &text))?
            }
            (None, None) => runtime.block_on(titi_cli::headless::run(engine, session_log))?,
        };
        std::process::exit(code);
    }

    let cast = match record {
        Some(path) => match titi_cli::ompcast::CastWriter::create(&path) {
            Ok(writer) => {
                eprintln!("recording to {}", path.display());
                Some(writer)
            }
            Err(error) => {
                // A recording is a convenience; refusing to start the session
                // over it would not be.
                eprintln!("record: {error} · this session is not being recorded");
                None
            }
        },
        None => None,
    };
    titi_cli::chat::run(engine, session_log, models, session_id, cast)
}
