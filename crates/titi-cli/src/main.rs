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
  --theme <name>              paint with a palette for this run; bare /theme
                              in the chat lists them and remembers a choice
  --mode <mode>               agent | plan | duck (default: agent)
                              plan: read-only tools; duck: repo-blind chat
  --record <path.ompcast>     record this session's events to a cast file
  --replay <path.ompcast>     play a recorded session and exit; no model runs
  --replay-fast               with --replay: no pauses between records
  --mouse <preset>            accepted, ignored (off | on | wheel | buttons | all)
  --set-key <provider> <key>  store an API key in the agent directory
  --list-keys                 list stored providers, kind and lifetime (never the keys)
  --login [provider]          sign in to a provider with OAuth; no argument lists them
  --device                    with --login: use the device code, no callback server
  --help, -h                  this text

  titi genome [on|off|limit <n>|check|lsp]
                              manage the prompt map build: on, off, or a file cap
                              (default: on, cap is 24; setting wins over default,
                              TITI_NO_GENOME=1 forces off for one run)

genome.limit in the agent config.yml or the project .titi/config.yml
(project wins) caps the map: an integer from 1 to 64; unset keeps 24,
out of range or not an integer keeps 24. TITI_NO_GENOME=1 omits the
map for one run without touching the limit.

genome.enabled in the agent config.yml or project .titi/config.yml
(project wins) turns the prompt map on or off; unset means on,
TITI_NO_GENOME=1 forces off for one run without touching this setting.

In the chat: Enter sends, and steers while a turn is running. Ctrl+C stops
the turn; press it twice to leave. y / n answers a write or a shell prompt.
Bare /model (or alt+m) opens a picker of the models by provider, and typing
filters it; /model <name> switches straight to one. Ctrl+X switches
sessions. Bare /login opens a picker of the subscriptions you can sign in
to; /login <provider> [key|device] goes straight to one. A / at the start of the line lists commands; up and down
move, enter picks, tab fills, esc closes.
";

fn main() -> io::Result<()> {
    install_panic_report();
    #[cfg(debug_assertions)]
    if std::env::args().any(|arg| arg == "--panic-test") {
        panic!("deliberate panic for the panic-report check\x07");
    }
    // `titi genome` never reaches the engine and never needs a model key, so
    // it is short-circuited before the flag loop touched it or could mistake
    // its subcommands for positionals.
    if titi_cli::genome_cmd::run(
        &titi_config::agent_dir(),
        &titi_cli::session_fs::current_workspace(),
    )
    .is_some()
    {
        return Ok(());
    }
    let mut headless = false;
    let mut prompt: Option<String> = None;
    let mut goal: Option<String> = None;
    let mut set_key: Option<(String, String)> = None;
    let mut list_keys = false;
    let mut login: Option<Option<String>> = None;
    let mut login_device = false;
    let mut record: Option<std::path::PathBuf> = None;
    let mut replay: Option<std::path::PathBuf> = None;
    let mut replay_fast = false;
    let mut approval = titi_tools::ApprovalMode::Write;
    let mut mode = titi_engine::protocol::SessionMode::Agent;
    let mut theme: Option<String> = None;
    // Collected: `--login` needs to look at the next argument without eating
    // it, and `Skip<Args>` is not cloneable.
    let mut args = std::env::args()
        .skip(1)
        .collect::<Vec<String>>()
        .into_iter();
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
        } else if arg == "--login" {
            // The provider is optional, so the flag must not eat the next
            // option when it is absent. `--device` in between picks the
            // device grant, which needs no callback server.
            let device = args.clone().next().is_some_and(|value| value == "--device");
            if device {
                let _ = args.next();
            }
            login = Some(match args.clone().next() {
                Some(value) if !value.starts_with('-') => {
                    let _ = args.next();
                    Some(value)
                }
                _ => None,
            });
            if device {
                login_device = true;
                if matches!(login, Some(None)) {
                    eprintln!("usage: titi --login --device <provider>");
                    std::process::exit(2);
                }
            }
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
        } else if arg == "--theme" {
            let Some(raw) = args.next() else {
                eprintln!("usage: titi --theme <name>  (bare /theme in the chat opens the list)");
                std::process::exit(2);
            };
            if !titi_cli::themes::theme_names()
                .iter()
                .any(|known| known == &raw)
            {
                eprintln!("{}\n\n{USAGE}", titi_cli::themes::unknown_theme(&raw));
                std::process::exit(2);
            }
            theme = Some(raw);
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
                let now = titi_cli::secrets::now_secs();
                for key in keys {
                    let kind = titi_cli::secrets::describe_key(&key, now);
                    eprintln!("{}  ({kind})", key.provider);
                }
                Ok(())
            }
            Err(reason) => {
                eprintln!("could not read keys: {reason}");
                std::process::exit(1);
            }
        };
    }
    if let Some(provider) = login {
        let Some(id) = provider else {
            for provider in titi_cli::login::providers() {
                eprintln!("{}  {}", provider.id, provider.name);
            }
            return Ok(());
        };
        if titi_cli::login::find(&id).is_none() {
            eprintln!("unknown oauth provider {id}");
            std::process::exit(2);
        }
        let dir = titi_config::agent_dir();
        let result = if login_device {
            titi_cli::login::run_login_device(&id, &dir)
        } else {
            titi_cli::login::run_login(&id, &dir)
        };
        return match result {
            Ok(()) => Ok(()),
            Err(reason) => {
                eprintln!("login failed: {reason}");
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
    titi_cli::chat::run(engine, session_log, models, session_id, cast, theme)
}

/// Installed before the screen opens, so a panic is reported rather than
/// silently erased.
///
/// The chat owns the alternate screen and `Screen::Drop` restores the
/// terminal on the way out. The default panic behaviour works against that:
/// its hook prints the message into the alt screen, the unwind then runs
/// `Drop`, and the leave-alternate-screen sequence wipes the message — the
/// user sees a clean, restored terminal and no reason for it.
///
/// This hook turns it around: it restores what `Screen::Drop` restores (raw
/// mode, the alternate screen, the cursor, focus reporting — the same order
/// of things the Drop does when the panic happens *before* the screen, or
/// when a `panic = "abort"` profile would skip the unwind entirely), then
/// prints one plain line to stderr. Nothing else changes: the hook does not
/// exit, so the panic still unwinds / aborts exactly as the default build
/// does, ending in a non-zero exit (101).
///
/// The payload is sanitised: a panic message can carry text the input put
/// there, and control bytes in it would be resolve into escape sequences in
/// the very terminal this report is meant to leave clean.
fn flatten_panic_text(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        // The strip below loses its use on a line longer than the terminal,
        // and the location after it matters more than a long message.
        .chars()
        .take(200)
        .collect::<String>()
}

/// The one line a panic reports with: what panicked, where.
fn panic_line(info: &std::panic::PanicHookInfo) -> String {
    panic_report(info.payload(), info.location().copied())
}

/// `panic_line` for a payload and location already peeled out of the hook
/// info — constructible as the hook info itself is, so the shape a real
/// panic would produce is testable without a process going down.
fn panic_report(
    payload: &(dyn std::any::Any + Send),
    location: Option<std::panic::Location<'_>>,
) -> String {
    let payload = match payload.downcast_ref::<&str>() {
        Some(text) => (*text).to_owned(),
        None => match payload.downcast_ref::<String>() {
            Some(text) => text.clone(),
            None => "unknown panic payload".to_owned(),
        },
    };
    let location = match location {
        Some(at) => format!(" {}:{}:{}", at.file(), at.line(), at.column()),
        None => String::new(),
    };
    format!("panic: {}{location}", flatten_panic_text(&payload))
}

/// Leaves the console in a shape the message can be read in, once, best
/// effort: the panic may have hit before the screen existed, in which case
/// these disables are harmless, or after, in which case `Screen::Drop` runs
/// this again during the unwind and the double restore is idempotent.
fn restore_console() {
    use crossterm::{
        cursor::Show,
        event::DisableFocusChange,
        terminal::{LeaveAlternateScreen, disable_raw_mode},
    };
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = crossterm::execute!(stdout, Show, LeaveAlternateScreen, DisableFocusChange);
}

/// The installation itself: swap the default hook for the restored-screen
/// report. Keeps the single place the hook body lives, so the reason the
/// message survives is in one place.
fn install_panic_report() {
    std::panic::set_hook(Box::new(|info| {
        restore_console();
        eprintln!("{}", panic_line(info));
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_line_carries_the_payload_and_location_sanitsed() {
        let line = panic_report(
            &"deliberate panic for the panic-report check\x07",
            Some(*std::panic::Location::caller()),
        );
        assert!(
            line.starts_with("panic: deliberate panic for the panic-report check "),
            "{line}"
        );
        assert!(!line.chars().any(char::is_control), "{line}");
        assert!(
            line.contains(&format!(" {}:", file!())),
            "location should name this file: {line}"
        );
    }

    #[test]
    fn control_characters_are_flattened_not_emitted() {
        let flattened = flatten_panic_text("line\nbreak\x1b[2;1Htab\there");
        assert_eq!(flattened, "line break [2;1Htab here");
    }

    #[test]
    fn a_string_payload_is_reported_too() {
        let line = panic_report(
            &"boxed: one\x1b]0;evil".to_owned(),
            Some(*std::panic::Location::caller()),
        );
        assert!(line.starts_with("panic: boxed: one ]0;evil "), "{line}");
        assert!(!line.contains('\x1b'), "{line}");
    }
}
