//! The chat screen's tests, beside it.
//!
//! The module is declared in `chat.rs` with a `#[path]`, so this file is
//! `chat::tests` and `use super::*` reaches the screen it drives. It was moved
//! out verbatim: the bodies are the ones that ran before, only the indentation
//! of the wrapper is gone.

use super::*;
use std::path::Path;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use titi_engine::TurnId;
use titi_providers::StopReason;
use titi_tui::markdown::SectionMode;

/// A chat with the theme a test names, for the ones that need a palette
/// where two tokens are two different colours.
///
/// Every test chat gets its own fresh agent directory under a
/// process-lifetime temp root: a helper that left `Chat::new`'s default
/// (`~/.titi/agent`) in place let tests like the slash-command sweep run
/// `/logout openai` against the operator's real key store. The root is
/// owned by a `LazyLock` (never `Box::leak`); tests that set `agent_dir`
/// explicitly still override it.
fn chat_with_theme(theme: Arc<Theme>) -> Chat {
    static ROOT: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
        tempfile::TempDir::with_prefix("titi-cli-test-agent").expect("temp agent root")
    });
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = Path::new(ROOT.path()).join(format!("agent-{n}"));
    let mut chat = Chat::new("openai/gpt-4.1", "session-123", theme);
    chat.set_agent_dir(&dir);
    chat
}

fn chat() -> Chat {
    chat_with_theme(test_theme())
}

/// A chat with the vim keys on (`editor.vim`), in Insert mode: what the
/// setting produces at startup, and where every vim test starts.
fn vim_chat() -> Chat {
    let mut chat = chat();
    chat.vim = Some(crate::vim::VimState::default());
    chat
}

/// The same, with Normal already entered — Esc, the way a person gets
/// there.
fn vim_normal_chat() -> Chat {
    let mut chat = vim_chat();
    chat.on_key(Key::Esc, Instant::now());
    assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Normal));
    chat
}

/// A Normal-mode chat with `text` in the draft: typed in Insert, then Esc,
/// which is how a person gets there. The caret is left where Esc leaves
/// it — one character back, over a paste marker whole.
fn vim_normal_with(text: &str) -> Chat {
    let mut chat = vim_chat();
    type_text(&mut chat, text);
    chat.on_key(Key::Esc, Instant::now());
    assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Normal));
    chat
}

/// One Normal-mode key.
fn vim_key(chat: &mut Chat, ch: char) -> Applied {
    chat.on_key(Key::Char(ch), Instant::now())
}

/// Pins the invariant the helper above exists for: no helper-built chat
/// may ever point at the real agent directory, or any mutating command in
/// a test (`/logout`, `/export`, `/checkpoint`, `/fork`, ...) operates on
/// the developer's own `~/.titi`.
#[test]
fn helper_chat_isolated_from_real_agent_dir() {
    let chat = chat();
    let real = titi_config::agent_dir();
    assert_ne!(chat.agent_dir, real);
    assert!(
        chat.agent_dir.starts_with(std::env::temp_dir()),
        "agent dir {:?} not under {}",
        chat.agent_dir,
        std::env::temp_dir().display()
    );
}

/// A built-in theme, with the colour depth pinned so an assertion is about
/// the theme's tokens and not about this machine's `TERM`. Built-in names
/// win over `{agent_dir}/themes` (`theme::loader::load_theme_json_in`), so
/// a custom theme on the machine that runs the tests cannot change them.
fn test_theme_named(name: &str) -> Arc<Theme> {
    let options = titi_tui::theme::loader::CreateThemeOptions {
        mode: Some(titi_tui::theme::ColorMode::Truecolor),
        ..Default::default()
    };
    let theme = titi_tui::theme::loader::load_theme(name, &options);
    match theme {
        Ok(theme) => Arc::new(theme),
        Err(reason) => panic!("built-in theme {name}: {reason}"),
    }
}

/// The dark slot the live screen lands on by default (`AUTO_DARK_THEME`).
fn test_theme() -> Arc<Theme> {
    test_theme_named("titanium")
}

fn frame_text(chat: &mut Chat) -> String {
    frame_rows(chat, 80, 24).join("")
}

/// A chat whose catalog and settings are the test's own: its agent
/// directory is a fresh temp dir, so no machine's `config.yml` decides
/// what a picker row says. The dir is returned with the chat because
/// dropping it would take the agent directory away mid-test.
fn picker_chat(model: &str, session: &str) -> (tempfile::TempDir, Chat) {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = Chat::new(model, session, test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    (dir, chat)
}

/// $3/MTok in, $15/MTok out, $0.30/MTok cached read — the shape of a
/// price the engine's descriptor carries.
fn test_price() -> titi_engine::ModelPrice {
    titi_engine::ModelPrice {
        input: 3_000_000,
        output: 15_000_000,
        cached_input: Some(300_000),
    }
}

/// A chat whose current model is priced. No built-in model ships with a
/// price (`NO_PRICE_MODELS`), so the money paths are driven with one
/// written in by hand — the same route a user's `models` settings entry
/// takes.
fn priced_chat() -> Chat {
    let mut chat = chat();
    chat.catalog = crate::engine::ModelCatalog::fixed_priced(
        vec![chat.model.clone()],
        vec![(chat.model.clone(), test_price())],
    );
    chat
}

/// The transcript's model confirmations, in the order they landed.
fn confirmations(chat: &Chat) -> Vec<String> {
    chat.lines
        .iter()
        .filter(|line| line.text.starts_with("model "))
        .map(|line| line.text.clone())
        .collect()
}

/// Plays the `ModelSwitched` the engine answers a switch with, and
/// returns the confirmations *this* switch added: a switch that adds two
/// lines is a switch that was narrated twice.
fn confirmations_after_switch(chat: &mut Chat, to: &str) -> Vec<String> {
    let before = confirmations(chat).len();
    chat.on_event(EngineEvent::ModelSwitched {
        turn_id: None,
        from: chat.model.clone().into(),
        to: to.into(),
    });
    let mut after = confirmations(chat);
    after.split_off(before)
}

/// The elapsed-seconds token the status row is showing, if it shows one.
/// The row is the only place a live elapsed time is rendered.
fn shown_seconds(frame: &str) -> Option<f64> {
    frame
        .split([' ', '·'])
        .filter_map(|token| token.strip_suffix('s'))
        .find_map(|token| token.parse().ok())
}

/// The row directly above the composer box: the one line the status row
/// is drawn on. Read from the rendered frame, so a test sees what a user
/// sees and not what a helper promised.
fn above_composer(chat: &mut Chat, width: u16, height: u16) -> String {
    let rows = frame_rows(chat, width, height);
    rows[height as usize - 5].clone()
}

fn type_text(chat: &mut Chat, text: &str) {
    let now = Instant::now();
    for ch in text.chars() {
        chat.on_key(Key::Char(ch), now);
    }
}

/// The picker, the masthead and the composer hold at 60, 80 and 120
/// columns — every row exactly as wide as the screen, the composer's
/// border intact — and a screen too small to lay out still draws instead
/// of panicking.
#[test]
fn the_screen_holds_at_60_80_and_120_columns() {
    let (dir, mut chat) = picker_chat("openai-codex/gpt-daybreak-blue-latest-wm", "session-1234");
    crate::secrets::store_key(dir.path(), "openai-codex", "sk-test").expect("store");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai-codex/gpt-daybreak-blue-latest-wm".to_owned(),
        "openai-codex/gpt-5.5".to_owned(),
        "anthropic/claude-sonnet-4-5".to_owned(),
    ]);
    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());
    type_text(&mut chat, "codex");

    for (width, height) in [(60u16, 20u16), (80, 20), (120, 30)] {
        let rows = frame_rows(&mut chat, width, height);
        assert_eq!(rows.len(), height as usize);
        for row in &rows {
            assert_eq!(
                titi_tui::width::visible_width(row),
                width as usize,
                "{width}x{height}: {row:?}"
            );
        }
        let top = &rows[height as usize - 4];
        let bottom = &rows[height as usize - 1];
        assert!(top.starts_with('╭') && top.ends_with('╮'), "{top:?}");
        assert!(
            bottom.starts_with('╰') && bottom.ends_with('╯'),
            "{bottom:?}"
        );
    }

    // Smaller than the picker, the composer and the masthead can share.
    for (width, height) in [(20u16, 6u16), (16, 6), (60, 7)] {
        let rows = frame_rows(&mut chat, width, height);
        assert_eq!(rows.len(), height as usize);
        for row in &rows {
            assert_eq!(titi_tui::width::visible_width(row), width as usize);
        }
    }
}

/// The rendered text of each row, the way the transcript stacks them.
fn row_texts(rows: &[Line<'static>]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            row.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn enter_while_idle_submits_and_logs_the_user() {
    let mut chat = chat();
    type_text(&mut chat, "hi");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(matches!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SubmitPrompt { .. }))
    ));
    assert_eq!(
        applied.log,
        Some(LogWrite::text(Role::User, "hi".to_owned()))
    );
    assert!(chat.turn_active);
}

#[test]
fn enter_during_a_turn_steers_and_leaves_it_active() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    type_text(&mut chat, "look again");
    let applied = chat.on_key(Key::Enter, Instant::now());
    match applied.effect {
        Some(ChatEffect::Send(EngineCommand::Steer { text })) => {
            assert_eq!(text.as_str(), "look again");
        }
        other => panic!("expected steer, got {other:?}"),
    }
    assert!(chat.turn_active);
    assert_eq!(
        applied.log,
        Some(LogWrite::text(Role::User, "look again".to_owned()))
    );
}

/// The user cancelled, so the prompt waiting behind that turn never ran.
/// It has to come back somewhere the user can see it.
#[test]
fn a_returned_prompt_lands_in_an_empty_composer() {
    let mut chat = chat();
    chat.on_event(EngineEvent::PromptReturned {
        text: "the question nobody asked".into(),
    });
    assert_eq!(chat.input, "the question nobody asked");
    let frame = frame_text(&mut chat);
    assert!(
        frame.contains("the question nobody asked"),
        "the returned prompt is on screen: {frame}"
    );
}

/// The user already started typing something else. Overwriting that is
/// the same silent loss the event exists to prevent, so the composer is
/// left alone and the text goes to the transcript instead.
#[test]
fn a_returned_prompt_never_overwrites_what_the_user_is_typing() {
    let mut chat = chat();
    type_text(&mut chat, "already typing this");
    chat.on_event(EngineEvent::PromptReturned {
        text: "the question nobody asked".into(),
    });
    assert_eq!(chat.input, "already typing this");
    let frame = frame_text(&mut chat);
    assert!(
        frame.contains("already typing this"),
        "what the user typed survives: {frame}"
    );
    assert!(
        frame.contains("the question nobody asked"),
        "the returned prompt is still shown: {frame}"
    );
}

/// A chat with the model's question on screen: the one event the engine
/// sends while it waits, which is the whole of what a surface has to
/// answer.
fn asked(question_options: &[&str], multi: bool, free_text: bool) -> Chat {
    let mut chat = chat();
    chat.on_event(EngineEvent::AskRequested {
        request_id: "ask-1".into(),
        question: "Which database should I use?".into(),
        options: question_options
            .iter()
            .map(|option| (*option).into())
            .collect(),
        multi,
        free_text,
    });
    chat
}

/// The question arrives as a panel over the composer and as lines in the
/// transcript, so scrollback keeps what was asked after the panel is gone.
#[test]
fn a_question_arrives_as_a_panel_and_a_transcript_line() {
    let chat = asked(&["postgres", "sqlite"], false, true);
    let pending = chat.pending_ask.as_ref().expect("the question is up");
    assert_eq!(pending.request_id, "ask-1");
    assert_eq!(pending.selected, 0, "the first row is the cursor");
    assert!(!pending.answering(), "nothing has been typed yet");

    let said: Vec<&str> = chat.lines.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(
        said,
        [
            "ask · Which database should I use?",
            "ask · options: postgres · sqlite",
        ]
    );

    let view = panel_view_for(&chat, 30, 100).expect("the panel is up");
    let title = view.title.clone().unwrap_or_default();
    assert!(title.contains("Which database should I use?"), "{title}");
    assert_eq!(view.lines.len(), 2);
    assert_eq!(view.selected, Some(0));
    // The masthead says the session is waiting on a person, as it does for
    // an approval.
    assert_eq!(state_word(&chat), "needs you");
}

/// Enter takes the highlighted row of a question that offers one choice,
/// and the answer goes to the engine with the id it is waiting on.
#[test]
fn a_single_choice_is_taken_with_enter() {
    let mut chat = asked(&["postgres", "sqlite"], false, true);
    chat.on_key(Key::Down, Instant::now());
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::AnswerAsk {
            request_id: "ask-1".into(),
            answer: titi_tools::AskAnswer::Chosen(vec!["sqlite".to_owned()]),
        }))
    );
    assert!(chat.pending_ask.is_none(), "the panel closes on the answer");
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "ask · chose sqlite"),
        "{:?}",
        chat.lines.last()
    );
}

/// A question that takes several ticks rows with Space and sends the set
/// in the order it was offered; an empty set is not an answer.
#[test]
fn a_question_that_takes_several_ticks_rows() {
    let mut chat = asked(&["postgres", "sqlite", "duckdb"], true, true);

    // Nothing ticked yet: Enter refuses rather than answering nothing.
    let refused = chat.on_key(Key::Enter, Instant::now());
    assert!(refused.effect.is_none());
    assert!(chat.pending_ask.is_some());
    assert_eq!(chat.hint, "pick at least one, or type your own");

    // The third row first, then the first: the answer keeps the offered
    // order, not the order they were picked in.
    chat.on_key(Key::Down, Instant::now());
    chat.on_key(Key::Down, Instant::now());
    chat.on_key(Key::Char(' '), Instant::now());
    chat.on_key(Key::Up, Instant::now());
    chat.on_key(Key::Up, Instant::now());
    chat.on_key(Key::Char(' '), Instant::now());
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::AnswerAsk {
            request_id: "ask-1".into(),
            answer: titi_tools::AskAnswer::Chosen(
                vec!["postgres".to_owned(), "duckdb".to_owned(),]
            ),
        }))
    );

    // A ticked row is drawn as one, and un-ticking says so.
    let mut chat = asked(&["postgres", "sqlite"], true, true);
    chat.on_key(Key::Char(' '), Instant::now());
    let view = panel_view_for(&chat, 30, 100).expect("the panel is up");
    let rows: Vec<String> = view
        .lines
        .iter()
        .map(|line| match line {
            PanelLine::Row { text, .. } => text.clone(),
            PanelLine::Heading(text) => text.clone(),
        })
        .collect();
    assert_eq!(rows, ["[x] postgres", "[ ] sqlite"]);
    chat.on_key(Key::Char(' '), Instant::now());
    assert_eq!(
        chat.pending_ask.as_ref().expect("up").ticked(),
        Vec::<String>::new()
    );
}

/// A printable character starts answering in the composer, and Enter sends
/// those words: the list the model wrote cannot know it holds the answer.
#[test]
fn a_question_can_be_answered_in_the_users_own_words() {
    let mut chat = asked(&["postgres", "sqlite"], false, true);
    chat.on_key(Key::Char('m'), Instant::now());
    assert!(chat.pending_ask.as_ref().expect("up").answering());
    for ch in "ongo".chars() {
        chat.on_key(Key::Char(ch), Instant::now());
    }
    chat.on_key(Key::Backspace, Instant::now());
    chat.on_key(Key::Char('o'), Instant::now());
    assert_eq!(chat.input, "mongo");

    // The composer says what the row is for while the answer is written.
    let frame = frame_rows(&mut chat, 80, 24).join("");
    assert!(frame.contains("enter sends  ·  esc cancels"), "{frame}");

    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::AnswerAsk {
            request_id: "ask-1".into(),
            answer: titi_tools::AskAnswer::Text("mongo".to_owned()),
        }))
    );
    assert_eq!(chat.input, "", "the answer field is empty again");
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "ask · answered mongo"),
        "{:?}",
        chat.lines.last()
    );
}

/// A question with no list is answered in words from the first keystroke:
/// there is nothing else to say it with, whatever `free_text` said.
#[test]
fn a_question_with_no_list_is_answered_in_words() {
    let mut chat = asked(&[], false, false);
    assert!(chat.pending_ask.as_ref().expect("up").answering());
    let view = panel_view_for(&chat, 30, 100).expect("the panel is up");
    assert_eq!(view.selected, None, "nothing is pickable");
    let frame = frame_rows(&mut chat, 80, 24).join("");
    assert!(frame.contains("your answer…"), "{frame}");

    for ch in "the local one".chars() {
        chat.on_key(Key::Char(ch), Instant::now());
    }
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::AnswerAsk {
            request_id: "ask-1".into(),
            answer: titi_tools::AskAnswer::Text("the local one".to_owned()),
        }))
    );
}

/// Esc refuses to answer, and Ctrl+C interrupts the turn the question
/// belongs to — the engine answers `Cancelled` either way.
#[test]
fn esc_cancels_a_question() {
    let mut chat = asked(&["postgres", "sqlite"], false, true);
    let applied = chat.on_key(Key::Esc, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::AnswerAsk {
            request_id: "ask-1".into(),
            answer: titi_tools::AskAnswer::Cancelled,
        }))
    );
    assert!(chat.pending_ask.is_none());
    assert!(chat.lines.iter().any(|line| line.text == "ask · cancelled"));

    let mut chat = asked(&["postgres"], false, true);
    chat.turn_active = true;
    assert_eq!(
        chat.on_key(Key::CtrlC, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::Cancel))
    );
    assert!(
        chat.pending_ask.is_none(),
        "the question goes with the turn"
    );
}

/// No question outlives its turn: the panel closes when the turn ends, by
/// finishing, failing or being cancelled — the same rule the approval
/// panel follows.
#[test]
fn a_question_is_cleared_when_its_turn_ends() {
    let mut chat = asked(&["postgres"], false, true);
    chat.turn_active = true;
    chat.active_turn_id = Some(TurnId(1));
    chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });
    assert!(chat.pending_ask.is_none());
    assert_eq!(state_word(&chat), "ready");
}

#[test]
fn approval_yes_and_no() {
    let mut chat = chat();
    chat.on_event(EngineEvent::ToolApprovalNeeded {
        turn_id: TurnId(1),
        call_id: "call-1".into(),
        name: "bash".into(),
    });
    let yes = chat.on_key(Key::Char('y'), Instant::now());
    match yes.effect {
        Some(ChatEffect::Send(EngineCommand::ApproveTool { call_id, approved })) => {
            assert_eq!(call_id.as_str(), "call-1");
            assert!(approved);
        }
        other => panic!("expected approval, got {other:?}"),
    }
    assert!(chat.approval.is_none());

    chat.on_event(EngineEvent::ToolApprovalNeeded {
        turn_id: TurnId(1),
        call_id: "call-2".into(),
        name: "edit".into(),
    });
    chat.on_key(Key::Char('x'), Instant::now());
    assert!(chat.input.is_empty());
    let no = chat.on_key(Key::Char('n'), Instant::now());
    match no.effect {
        Some(ChatEffect::Send(EngineCommand::ApproveTool { approved, .. })) => {
            assert!(!approved);
        }
        other => panic!("expected refusal, got {other:?}"),
    }
}

#[test]
fn failed_event_without_turn_id_does_not_finish_turn() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.approval = Some(PendingApproval {
        call_id: "call-1".into(),
        name: "bash".into(),
        detail: None,
    });

    chat.on_event(EngineEvent::Failed {
        turn_id: None,
        reason: titi_providers::ErrorReason::Rejected,
        message: "nope".into(),
    });

    assert!(chat.turn_active);
    assert!(chat.approval.is_some());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.kind == LineKind::Error && line.text == "nope")
    );
}

#[test]
fn failed_event_with_matching_turn_id_finishes_turn() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.approval = Some(PendingApproval {
        call_id: "call-1".into(),
        name: "bash".into(),
        detail: None,
    });

    chat.on_event(EngineEvent::Failed {
        turn_id: Some(TurnId(1)),
        reason: titi_providers::ErrorReason::Rejected,
        message: "nope".into(),
    });

    assert!(!chat.turn_active);
    assert!(chat.approval.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.kind == LineKind::Error && line.text == "nope")
    );
}

#[test]
fn stream_accumulates_and_finish_logs_the_assistant() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(7),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(7),
        text: "hel".into(),
    });
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(7),
        text: "lo".into(),
    });
    let applied = chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(7),
        reason: StopReason::Stop,
    });
    assert_eq!(
        applied.log,
        Some(LogWrite::text(Role::Assistant, "hello".to_owned()))
    );
    assert!(!chat.turn_active);
}

/// A turn with a tool round is three entries, and the text before the
/// call is written once, not again at the end of the turn.
#[test]
fn a_tool_round_logs_the_call_its_output_and_the_answer() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(3),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(3),
        text: "let me look".into(),
    });
    let call = chat.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(3),
        call_id: "call-1".into(),
        name: "read".into(),
        detail: None,
    });
    assert_eq!(
        call.log,
        Some(LogWrite {
            role: Role::Assistant,
            text: "let me look".to_owned(),
            tool_calls: vec![titi_providers::ToolCallRef {
                call_id: "call-1".into(),
                name: "read".into(),
                ..Default::default()
            }],
        })
    );
    let result = chat.on_event(EngineEvent::ToolFinished {
        turn_id: TurnId(3),
        call_id: "call-1".into(),
        output: "[package]".into(),
        is_error: false,
        detail: None,
    });
    assert_eq!(
        result.log,
        Some(LogWrite::text(Role::Tool, "[package]".to_owned()))
    );
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(3),
        text: " it is the workspace".into(),
    });
    let finished = chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(3),
        reason: StopReason::Stop,
    });
    assert_eq!(
        finished.log,
        Some(LogWrite::text(
            Role::Assistant,
            " it is the workspace".to_owned()
        ))
    );
}

#[test]
fn second_ctrl_c_within_two_seconds_quits() {
    let mut chat = chat();
    let start = Instant::now();
    let first = chat.on_key(Key::CtrlC, start);
    assert!(first.effect.is_none());
    let second = chat.on_key(Key::CtrlC, start + Duration::from_millis(500));
    assert_eq!(second.effect, Some(ChatEffect::Quit));
}

#[test]
fn ctrl_c_during_a_turn_cancels() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    let applied = chat.on_key(Key::CtrlC, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::Cancel))
    );
}

/// A bare `exit` is the composer's own word for leaving (omp
/// `input.bareExitOnEmptySession`): it never becomes a prompt, and once
/// the session has something to lose the screen asks for a second Enter.
#[test]
fn a_bare_exit_asks_once_and_then_leaves() {
    // Nothing on the screen yet, so there is nothing to keep: the first
    // word leaves.
    let mut fresh = chat();
    type_text(&mut fresh, "exit");
    assert_eq!(
        fresh.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Quit)
    );

    // A turn in the session is worth asking about.
    let mut chat = chat();
    type_text(&mut chat, "hello");
    chat.on_key(Key::Enter, Instant::now());
    let before = chat.lines.len();
    type_text(&mut chat, "exit");
    let at = Instant::now();
    let first = chat.on_key(Key::Enter, at);
    assert!(
        first.effect.is_none(),
        "the first Enter only asks: {first:?}"
    );
    assert!(first.log.is_none(), "and writes nothing to the session");
    assert_eq!(chat.lines.len(), before, "no prompt reached the transcript");
    let frame = frame_text(&mut chat);
    assert!(frame.contains("press Enter again to quit"), "{frame}");
    let second = chat.on_key(Key::Enter, at + Duration::from_millis(200));
    assert_eq!(second.effect, Some(ChatEffect::Quit));
}

/// One word, three spellings. Short words that *start* with one of them
/// are prompts.
#[test]
fn only_the_whole_exit_word_leaves() {
    for word in ["exit", "quit", "q"] {
        let mut chat = chat();
        type_text(&mut chat, word);
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Quit),
            "{word:?} leaves an empty session"
        );
    }

    let mut chat = chat();
    type_text(&mut chat, "exit code");
    match chat.on_key(Key::Enter, Instant::now()).effect {
        Some(ChatEffect::Send(EngineCommand::SubmitPrompt { text })) => {
            assert_eq!(text.as_str(), "exit code");
        }
        other => panic!("expected a prompt, got {other:?}"),
    }
}

/// `/exit` and `/quit` are the same word with a slash, and both are
/// listed so they can be discovered.
#[test]
fn slash_exit_leaves_like_the_bare_word() {
    for command in ["/exit", "/quit"] {
        let mut chat = chat();
        type_text(&mut chat, command);
        assert_eq!(
            chat.on_key(Key::Enter, Instant::now()).effect,
            Some(ChatEffect::Quit),
            "{command} leaves"
        );
    }
    assert!(COMMANDS.iter().any(|command| command.name == "exit"));
    assert!(COMMANDS.iter().any(|command| command.name == "quit"));
}

/// Bare `/model` is the browser, not a cycle: Enter takes the row the
/// cursor is on, which starts as the model in use.
#[test]
fn bare_model_opens_the_browser_on_the_model_in_use() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "opencode-go/glm-5.3-flash".to_owned(),
    ]);
    type_text(&mut chat, "/model");
    let opened = chat.on_key(Key::Enter, Instant::now());
    assert!(opened.effect.is_none(), "opening sends nothing: {opened:?}");
    assert!(chat.model_picker.is_some());
    let frame = frame_text(&mut chat);
    assert!(frame.contains("models · 2"), "{frame}");
    assert!(frame.contains("✓ current"), "{frame}");

    let applied = chat.on_key(Key::Enter, Instant::now());
    match applied.effect {
        Some(ChatEffect::Send(EngineCommand::SwitchModel { model })) => {
            assert_eq!(model.as_str(), "openai/gpt-4.1");
        }
        other => panic!("expected a model switch, got {other:?}"),
    }
    assert!(applied.log.is_none());
    assert!(!chat.turn_active);
}

/// Rows are grouped, and each row states what titi knows: the id, the
/// declared window when there is one, and the credential behind the
/// provider.
#[test]
fn the_browser_groups_by_provider_and_states_the_known_facts() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "opencode-go/glm-5.3-flash".to_owned(),
        "anthropic/claude-sonnet-4-5".to_owned(),
    ]);
    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());

    let frame = frame_text(&mut chat);
    for expected in [
        "▾ openai",
        "▾ opencode-go",
        "▾ anthropic",
        "openai/gpt-4.1",
        "·openai",
        "1M",
        "·anthropic",
        "200k",
        // A model that declares no window gets no chip; the row still
        // names its provider.
        "opencode-go/glm-5.3-flash  ·opencode-go",
    ] {
        assert!(frame.contains(expected), "{expected} is missing: {frame}");
    }
}

/// A stored credential is named on its provider's rows, so a
/// subscription-backed model is recognizable without `/keys`.
#[test]
fn a_row_names_the_credential_the_provider_holds() {
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    crate::secrets::store_key(dir.path(), "openai", "sk-test").expect("store");
    crate::secrets::store_oauth(
        dir.path(),
        "openai-codex",
        &titi_providers::oauth::OAuthTokens {
            access: "sk-test".to_owned(),
            refresh: Some("sk-test-refresh".to_owned()),
            expires_at: Some(crate::secrets::now_secs() + 3_600),
            account_id: None,
            email: None,
            org_id: None,
            org_name: None,
        },
    )
    .expect("store oauth");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "openai-codex/gpt-5.5".to_owned(),
    ]);

    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());
    let frame = frame_text(&mut chat);
    assert!(
        frame.contains("openai/gpt-4.1  ·openai  1M  key"),
        "an api key reads as a key: {frame}"
    );
    assert!(
        frame.contains("openai-codex/gpt-5.5  ·openai-codex  oauth"),
        "a subscription reads as oauth: {frame}"
    );
}

/// Typing narrows the list, and the title says what is left and what was
/// typed. Matching is a subsequence, the repo's habit for `/switch`.
#[test]
fn typing_narrows_the_browser_and_the_title_shows_the_query() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "anthropic/claude-sonnet-4-5".to_owned(),
    ]);
    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());
    type_text(&mut chat, "snnt");

    let frame = frame_text(&mut chat);
    assert!(frame.contains("anthropic/claude-sonnet-4-5"), "{frame}");
    assert!(
        !frame.contains("·openai  1M"),
        "the filtered row is gone, the masthead keeps naming the model: {frame}"
    );
    assert!(frame.contains("1 of 2"), "{frame}");
    assert!(frame.contains("\"snnt\""), "{frame}");
}

/// A provider name spelled out ranks that provider's rows above a model
/// whose letters only happen to sit in the same order.
#[test]
fn a_provider_prefix_outranks_a_scattered_match() {
    let (_dir, mut chat) = picker_chat("openai-codex/gpt-5.5", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "cerebras/qwen3-coder-x".to_owned(),
        "openai-codex/gpt-5.5".to_owned(),
    ]);
    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());
    type_text(&mut chat, "codex");

    let picker = chat.model_picker.as_ref().expect("open");
    let matched = picker.matched();
    assert_eq!(matched.len(), 2, "both match, one ranks higher");
    assert_eq!(picker.offers[matched[0]].target(), "openai-codex/gpt-5.5");
    // The cursor starts on the best match, so Enter takes it.
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "openai-codex/gpt-5.5".into()
        }))
    );
}

/// Esc takes the query back first and closes on the second press: a
/// filter is cheap to undo, and closing on one key would make a narrow
/// search cost a reopen.
#[test]
fn esc_clears_the_query_then_closes_without_switching() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "anthropic/claude-sonnet-4-5".to_owned(),
    ]);
    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());
    type_text(&mut chat, "sonnet");
    assert!(frame_text(&mut chat).contains("1 of 2"));

    let first = chat.on_key(Key::Esc, Instant::now());
    assert!(first.effect.is_none());
    assert!(chat.model_picker.is_some(), "the picker is still open");
    let frame = frame_text(&mut chat);
    assert!(frame.contains("models · 2"), "the query is gone: {frame}");
    assert!(frame.contains("openai/gpt-4.1"), "{frame}");

    let second = chat.on_key(Key::Esc, Instant::now());
    assert!(second.effect.is_none());
    assert!(chat.model_picker.is_none(), "the second esc closes it");
    assert_eq!(chat.model, "openai/gpt-4.1", "nothing switched");
    assert!(
        !chat
            .lines
            .iter()
            .any(|line| line.text.starts_with("model ")),
        "closing says nothing"
    );
}

/// Esc on a draft clears it, as it always has — and clearing a draft is
/// not also the first press of the rewind chord.
#[test]
fn one_escape_clears_the_draft_and_arms_nothing() {
    let mut chat = chat();
    type_text(&mut chat, "a draft");
    let at = Instant::now();
    assert!(chat.on_key(Key::Esc, at).effect.is_none());
    assert!(chat.input.is_empty(), "the draft is gone");
    assert!(chat.lines.is_empty(), "nothing was sent or printed");

    // The next press on the now-empty composer only arms the chord: no
    // checkpoint exists, so a chord that fired would be an error line.
    let next = chat.on_key(Key::Esc, at + Duration::from_millis(200));
    assert!(next.effect.is_none());
    assert!(!chat.lines.iter().any(|line| line.kind == LineKind::Error));
}

/// Esc on the command list closes it and nothing else: the draft stays
/// exactly as typed, the way the emoji picker's Esc works. (The model
/// browser's Esc is its own: see
/// `esc_clears_the_query_then_closes_without_switching`.)
#[test]
fn escape_in_the_command_list_only_closes_it() {
    let mut chat = chat();
    type_text(&mut chat, "/mo");
    assert!(chat.picking(), "the command list is up");
    assert!(chat.on_key(Key::Esc, Instant::now()).effect.is_none());
    assert!(!chat.picking(), "esc closed the list");
    assert_eq!(chat.input, "/mo", "and left the draft as it was typed");
    assert!(!chat.lines.iter().any(|line| line.kind == LineKind::Error));

    // The list is a function of the draft, so the next keystroke brings
    // it back rather than leaving the composer in a hidden mode.
    type_text(&mut chat, "d");
    assert_eq!(chat.input, "/mod");
    assert!(chat.picking(), "typing again offers the list");
}

/// Esc twice on an empty composer is `/rewind` (omp
/// `doubleEscapeAction`, default `rewind`): the same cut the typed
/// command makes, because it is the same function.
#[test]
fn double_escape_on_an_empty_composer_rewinds() {
    let dir = tempfile::tempdir().expect("temp");
    let store = titi_core::session::SessionStore::new(dir.path()).expect("store");
    let id = store
        .create(titi_core::session::SessionMeta::default())
        .expect("session");
    store.append(&id, Role::User, "keep").expect("keep");
    store.checkpoint(&id).expect("checkpoint");
    store.append(&id, Role::User, "drop").expect("drop");
    let mut chat = Chat::new("openai/gpt-4.1", &id, test_theme());
    chat.agent_dir = dir.path().to_path_buf();

    let start = Instant::now();
    let first = chat.on_key(Key::Esc, start);
    assert!(first.effect.is_none(), "one Esc is not the chord");
    let second = chat.on_key(Key::Esc, start + Duration::from_millis(200));
    match second.effect {
        Some(ChatEffect::Send(EngineCommand::RestoreHistory { messages })) => {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].content.as_str(), "keep");
        }
        other => panic!("expected restore, got {other:?}"),
    }
    assert!(chat.lines.iter().any(|line| line.text == "keep"));
    assert!(!chat.lines.iter().any(|line| line.text == "drop"));
}

/// The chord is a window, not a chain: a second Esc after it has run out
/// only arms a new one.
#[test]
fn a_late_second_escape_does_not_rewind() {
    let mut chat = chat();
    let start = Instant::now();
    chat.on_key(Key::Esc, start);
    let later = chat.on_key(Key::Esc, start + QUIT_WINDOW + Duration::from_millis(1));
    assert!(later.effect.is_none());
    assert!(!chat.lines.iter().any(|line| line.kind == LineKind::Error));
}

/// Enter switches to the highlighted row and confirms in the words
/// `/model <id>` uses — the argument form keeps working unchanged.
#[test]
fn enter_switches_and_the_engine_confirms_once() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "anthropic/claude-sonnet-4-5".to_owned(),
    ]);
    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());
    type_text(&mut chat, "sonnet");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "anthropic/claude-sonnet-4-5".into()
        }))
    );
    assert_eq!(chat.model, "anthropic/claude-sonnet-4-5");
    assert!(chat.model_picker.is_none());
    assert!(
        confirmations(&chat).is_empty(),
        "the engine owns the confirmation: {:?}",
        chat.lines
    );
    assert_eq!(
        confirmations_after_switch(&mut chat, "anthropic/claude-sonnet-4-5"),
        ["model anthropic/claude-sonnet-4-5"]
    );
}

/// Bare `/switch` is the same browser, and one switch reads as one line —
/// the engine's, whether the model was picked or named.
#[test]
fn bare_switch_opens_the_browser_and_confirms_once() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "anthropic/claude-opus-5".to_owned(),
    ]);
    type_text(&mut chat, "/switch");
    let opened = chat.on_key(Key::Enter, Instant::now());
    assert!(opened.effect.is_none());
    assert!(chat.model_picker.is_some());
    type_text(&mut chat, "opus");

    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "anthropic/claude-opus-5".into()
        }))
    );
    assert_eq!(
        confirmations_after_switch(&mut chat, "anthropic/claude-opus-5"),
        ["model anthropic/claude-opus-5"]
    );
}

/// `/model <sel>` resolves the way it always did — a whole id or the last
/// segment — and an id it cannot place is the refusal it always was.
#[test]
fn a_model_argument_resolves_by_id_and_by_last_segment() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "anthropic/claude-sonnet-4-5".to_owned(),
    ]);

    type_text(&mut chat, "/model anthropic/claude-sonnet-4-5");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "anthropic/claude-sonnet-4-5".into()
        }))
    );
    assert_eq!(chat.model, "anthropic/claude-sonnet-4-5");

    type_text(&mut chat, "/model gpt-4.1");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "openai/gpt-4.1".into()
        }))
    );

    type_text(&mut chat, "/model nope");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(chat.model_picker.is_none(), "an argument is not a picker");
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "unknown model nope"),
        "{:?}",
        chat.lines
    );
}

/// The engine's `ModelSwitched` is the only confirmation, on every path:
/// a command that also narrated its own switch printed the line twice.
#[test]
fn a_switch_confirms_once_from_the_command_path_too() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "anthropic/claude-opus-5".to_owned(),
    ]);
    type_text(&mut chat, "/model anthropic/claude-opus-5");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "anthropic/claude-opus-5".into()
        }))
    );
    assert!(confirmations(&chat).is_empty(), "{:?}", chat.lines);
    assert_eq!(
        confirmations_after_switch(&mut chat, "anthropic/claude-opus-5"),
        ["model anthropic/claude-opus-5"]
    );

    type_text(&mut chat, "/switch opus");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "anthropic/claude-opus-5".into()
        }))
    );
    assert_eq!(
        confirmations_after_switch(&mut chat, "anthropic/claude-opus-5"),
        ["model anthropic/claude-opus-5"]
    );
}

/// A fallback inside a turn says which model gave up, so it cannot be
/// read as the user's own switch; the masthead follows it either way.
#[test]
fn a_fallback_names_the_model_it_left() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.on_event(EngineEvent::ModelSwitched {
        turn_id: Some(TurnId(7)),
        from: "openai/gpt-4.1".into(),
        to: "anthropic/claude-opus-5".into(),
    });
    assert_eq!(
        confirmations(&chat),
        ["model anthropic/claude-opus-5 · fallback from openai/gpt-4.1"]
    );
    assert_eq!(chat.model, "anthropic/claude-opus-5");
}

/// An argument is not a picker: the resolution tests above must not have
/// come to depend on a browser being open.
#[test]
fn a_command_with_an_argument_does_not_open_the_picker() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "anthropic/claude-opus-5".to_owned(),
    ]);
    type_text(&mut chat, "/model anthropic/claude-opus-5");
    chat.on_key(Key::Enter, Instant::now());
    assert!(chat.model_picker.is_none());

    type_text(&mut chat, "/switch opus");
    chat.on_key(Key::Enter, Instant::now());
    assert!(chat.model_picker.is_none());
}

/// A configured role is a row of its own, first in the list, and it
/// switches to the model it resolves to — `/switch @role`, spelled out.
#[test]
fn configured_roles_lead_the_list_and_switch_to_their_model() {
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    std::fs::write(
        dir.path().join("config.yml"),
        "modelRoles:\n  review: anthropic/claude-opus-5\n",
    )
    .expect("config");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "anthropic/claude-opus-5".to_owned(),
    ]);

    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());
    let frame = frame_text(&mut chat);
    assert!(frame.contains("▾ roles  1"), "{frame}");
    assert!(
        frame.contains("@review  →  anthropic/claude-opus-5"),
        "{frame}"
    );

    // `@` is the roles and nothing else.
    type_text(&mut chat, "@");
    let frame = frame_text(&mut chat);
    assert!(frame.contains("@review"), "{frame}");
    assert!(
        !frame.contains("·openai  1M"),
        "no model row survives an `@` query: {frame}"
    );

    // The cursor moves to the only match as the query narrows, so Enter
    // takes the role's model.
    type_text(&mut chat, "rev");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "anthropic/claude-opus-5".into()
        }))
    );
    assert_eq!(chat.model, "anthropic/claude-opus-5");
}

/// The crate's table binds `app.session.switch` to ctrl+x and
/// `app.model.select` to alt+m. A live screen whose mapping drops the
/// modifier leaves both chords unreachable, so the mapping itself is
/// asserted here, and not only through the screen.
#[test]
fn the_crates_chords_reach_the_live_screen() {
    assert_eq!(
        map_key(KeyCode::Char('x'), KeyModifiers::CONTROL),
        Some(Key::CtrlX),
        "ctrl+x is the session switcher"
    );
    assert_eq!(
        map_key(KeyCode::Char('m'), KeyModifiers::ALT),
        Some(Key::AltM),
        "alt+m opens the model selector"
    );
    assert_eq!(
        map_key(KeyCode::Char('m'), KeyModifiers::ALT | KeyModifiers::SHIFT),
        Some(Key::AltM),
        "a shifted chord is still the chord"
    );
    assert_eq!(
        map_key(KeyCode::Char('m'), KeyModifiers::NONE),
        Some(Key::Char('m')),
        "and a bare m is still a character"
    );
    assert_eq!(
        map_key(KeyCode::Char('x'), KeyModifiers::NONE),
        Some(Key::Char('x'))
    );
}

/// Alt+m reaches the model browser from the composer, exactly as bare
/// `/model` does.
#[test]
fn alt_m_opens_the_model_browser() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "anthropic/claude-opus-5".to_owned(),
    ]);
    type_text(&mut chat, "half-typed words");
    chat.on_key(Key::AltM, Instant::now());
    assert!(chat.model_picker.is_some(), "alt+m opens the browser");
    let frame = frame_text(&mut chat);
    assert!(frame.contains("▾ openai"), "and it is the browser: {frame}");
    assert!(
        frame.contains("half-typed words"),
        "the composer keeps its text: {frame}"
    );
}

/// The file a run appends to follows the screen.
///
/// The log is opened for one session id and stamps every write with it, and
/// nothing re-opened it: after a switch the new session stayed empty for the
/// rest of the run while the screen showed its history, and resuming it
/// later replayed nothing. The write after the switch must land in the new
/// session's file, and the file the screen left must gain nothing.
#[test]
fn the_session_log_follows_the_switch() {
    let dir = tempfile::tempdir().expect("temp");
    let store = titi_core::session::SessionStore::new(dir.path()).expect("session store");
    let create = |title: &str| {
        store
            .create(titi_core::session::SessionMeta {
                title: Some(title.to_owned()),
                source: Some("cli".to_owned()),
                ..Default::default()
            })
            .expect("create")
    };
    // The session the run starts on, with a file of its own, and another to
    // switch to.
    let left_behind = create("left");
    store
        .append(&left_behind, Role::User, "left question")
        .expect("append");
    let other = create("other");
    store
        .append(&other, Role::User, "other question")
        .expect("append");
    store
        .append(&other, Role::Assistant, "other answer")
        .expect("append");
    let mut chat = Chat::new("openai/gpt-4.1", &left_behind, test_theme());
    chat.agent_dir = dir.path().to_path_buf();

    // The run opens its log for the session it started on.
    let log = SessionLog::open(dir.path(), &left_behind).expect("a log");
    let mut log = session_log_for(Some(log), &chat);
    assert!(
        log.is_some(),
        "a run that started with a log keeps one while the screen has not moved"
    );
    record(
        &mut chat,
        &log,
        Some(LogWrite::text(Role::User, "before the switch".to_owned())),
    );
    let before = std::fs::read_to_string(dir.path().join(format!("sessions/{left_behind}.jsonl")))
        .expect("the session on screen is written");

    // The screen switches to the other session, exactly as Ctrl+X does.
    chat.session_picker = Some(
        chat.session_choices()
            .iter()
            .position(|id| *id == other)
            .expect("the other session is offered"),
    );
    chat.on_key(Key::Enter, Instant::now());
    assert_eq!(chat.session_id, other, "the screen moved");

    // The next write follows it.
    log = session_log_for(log, &chat);
    assert_eq!(
        log.as_ref().map(SessionLog::session_id),
        Some(other.as_str()),
        "the log is on the session on screen"
    );
    record(
        &mut chat,
        &log,
        Some(LogWrite::text(Role::User, "after the switch".to_owned())),
    );

    let moved = std::fs::read_to_string(dir.path().join(format!("sessions/{other}.jsonl")))
        .expect("the new session is written");
    assert!(
        moved.contains("after the switch"),
        "the write after the switch is in the new session: {moved}"
    );
    assert!(
        !moved.contains("before the switch"),
        "and not the message that came before it: {moved}"
    );
    let left = std::fs::read_to_string(dir.path().join(format!("sessions/{left_behind}.jsonl")))
        .expect("the session the screen left is still there");
    assert_eq!(
        left, before,
        "the session the screen left gains nothing after the switch"
    );
}

/// Ctrl+X lists the stored sessions, marks the one on screen, and Enter
/// switches: the session's history replaces the screen and the engine is
/// told to replay it, as a rewind does. Esc closes without switching.
#[test]
fn ctrl_x_switches_sessions() {
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    let store = titi_core::session::SessionStore::new(dir.path()).expect("session store");
    let older = store
        .create(titi_core::session::SessionMeta {
            title: Some("older".to_owned()),
            source: Some("cli".to_owned()),
            ..Default::default()
        })
        .expect("create older");
    store
        .append(&older, Role::User, "older question")
        .expect("append");
    store
        .append(&older, Role::Assistant, "older answer")
        .expect("append");
    let newer = store
        .create(titi_core::session::SessionMeta {
            title: Some("newer".to_owned()),
            source: Some("cli".to_owned()),
            ..Default::default()
        })
        .expect("create newer");
    store
        .append(&newer, Role::User, "newer question")
        .expect("append");

    chat.on_key(Key::CtrlX, Instant::now());
    let frame = frame_text(&mut chat);
    assert!(
        frame.contains("sessions · 2"),
        "both sessions are listed: {frame}"
    );
    assert!(
        frame.contains(&older),
        "the older session is a row: {frame}"
    );
    assert!(
        frame.contains(&newer),
        "the newer session is a row: {frame}"
    );

    chat.on_key(Key::Esc, Instant::now());
    assert!(
        !frame_text(&mut chat).contains("sessions · 2"),
        "esc closes it"
    );

    // Walk the cursor onto the older session and take it.
    chat.on_key(Key::CtrlX, Instant::now());
    for _ in 0..3 {
        if frame_text(&mut chat).contains(&format!("▶ {older}")) {
            break;
        }
        chat.on_key(Key::Down, Instant::now());
    }
    let applied = chat.on_key(Key::Enter, Instant::now());
    match applied.effect {
        Some(ChatEffect::Send(EngineCommand::RestoreHistory { messages })) => {
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.content.trim().to_owned())
                    .collect::<Vec<_>>(),
                ["older question", "older answer"],
                "the engine replays the session that was chosen"
            );
        }
        other => panic!("switching a session restores its history, got {other:?}"),
    }
    assert_eq!(
        chat.session_id, older,
        "the screen is on the chosen session"
    );
    let frame = frame_text(&mut chat);
    assert!(
        frame.contains("older question"),
        "the transcript is its history: {frame}"
    );
    assert!(
        !frame.contains("newer question"),
        "and not the other one: {frame}"
    );
}

/// The crate's theme is a process-wide global, so the tests that change it
/// take this lock. Nothing else in this binary touches the global: every
/// other test hands its chat its own palette.
static THEME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn theme_lock() -> std::sync::MutexGuard<'static, ()> {
    THEME_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The palettes this build carries, in the picker's own shape.
fn theme_frame(chat: &mut Chat) -> String {
    frame_at(chat, 80, 20)
}

/// `/theme` lists every palette the build carries, and typing narrows it.
#[test]
fn the_theme_picker_lists_every_palette_and_filters_by_name() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    let all = crate::themes::theme_names();
    assert!(all.len() > 50, "the registry is the list: {}", all.len());

    type_text(&mut chat, "/theme");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none(), "opening a picker runs nothing");
    let frame = theme_frame(&mut chat);
    assert!(frame.contains("themes ·"), "{frame}");
    assert!(
        frame.contains("auto  ✓ current"),
        "with nothing chosen, the mark is on the probe's own pick: {frame}"
    );
    // The window shows a slice of a hundred rows; the list behind it is the
    // whole registry, with `auto` in front of it.
    let picker = chat.theme_picker.as_ref().expect("open");
    assert_eq!(
        picker.names.len(),
        all.len() + 1,
        "every palette this build carries is a row"
    );
    for name in ["titanium", "alabaster", "dark-gruvbox"] {
        assert!(
            picker.names.iter().any(|row| row == name),
            "{name} is missing"
        );
    }

    // A query brings one into the window; esc clears it without closing,
    // the way the model browser's does.
    type_text(&mut chat, "titan");
    let frame = theme_frame(&mut chat);
    assert!(
        frame.contains("titanium"),
        "the query brings a preset up: {frame}"
    );
    assert!(
        frame.contains("themes · 1 of 101 · titan"),
        "and the title says what it is showing: {frame}"
    );
    chat.on_key(Key::Esc, Instant::now());
    assert!(
        chat.theme_picker.is_some(),
        "esc clears the query before it closes the picker"
    );

    type_text(&mut chat, "gruv");
    let frame = theme_frame(&mut chat);
    assert!(frame.contains("dark-gruvbox"), "{frame}");
    assert!(frame.contains("light-gruvbox"), "{frame}");
    assert!(
        !frame.contains("alabaster"),
        "a name the query drops is gone: {frame}"
    );
    assert!(
        frame.contains(" of "),
        "the title counts what it shows: {frame}"
    );

    // A query that matches nothing is refused, not applied.
    type_text(&mut chat, "zzzz");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none(), "nothing is applied");
    let last = chat.lines.last().expect("a line");
    assert_eq!(last.kind, LineKind::Error, "{:?}", chat.lines);
    assert!(last.text.contains("no theme matches"), "{}", last.text);
    assert!(
        chat.theme_picker.is_none(),
        "and the picker closed on the answer"
    );
}

/// Enter applies a palette to the very next frame, it is remembered, and
/// Esc leaves the one on screen alone — including a row the cursor was
/// arrowed past.
#[test]
fn enter_applies_a_theme_and_esc_keeps_the_one_on_screen() {
    let _guard = theme_lock();
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    let workspace = crate::session_fs::current_workspace();
    chat.theme = crate::themes::theme_for(dir.path(), &workspace, None).expect("a theme");
    let before_frame = theme_frame(&mut chat);
    let before_bg = frame_buffer(&mut chat, 80, 20)[(0, 0)].bg;

    // Open, walk past rows, leave: nothing about the screen changes.
    type_text(&mut chat, "/theme");
    chat.on_key(Key::Enter, Instant::now());
    chat.on_key(Key::Down, Instant::now());
    chat.on_key(Key::Down, Instant::now());
    chat.on_key(Key::Esc, Instant::now());
    assert_eq!(
        theme_frame(&mut chat),
        before_frame,
        "esc leaves the screen as it was"
    );
    assert_eq!(
        frame_buffer(&mut chat, 80, 20)[(0, 0)].bg,
        before_bg,
        "and the palette with it"
    );

    // `/theme <name>` applies one: the next frame is painted in it.
    let applied = chat.slash("/theme alabaster").expect("the command parses");
    assert!(applied.effect.is_none(), "a palette is a local change");
    let after_bg = frame_buffer(&mut chat, 80, 20)[(0, 0)].bg;
    assert_ne!(
        after_bg, before_bg,
        "the frame is painted in the new palette"
    );
    let frame = theme_frame(&mut chat);
    assert!(frame.contains("theme alabaster"), "and it says so: {frame}");
    let expected = titi_tui::theme::loader::load_theme("alabaster", &theme_options())
        .expect("alabaster is a preset of this build");
    assert_eq!(
        after_bg,
        rgb(&expected.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg)),
        "and the palette is that preset's, cell for cell"
    );

    // Remembered for the appearance slot the terminal reports, and read
    // back at startup.
    let settings =
        titi_config::settings::Settings::load(dir.path(), &workspace, &[]).expect("settings");
    let key = crate::themes::theme_slot(&titi_tui::theme::appearance::AppearanceInputs::from_env());
    assert_eq!(
        settings
            .get(key)
            .and_then(|value| value.as_str().map(str::to_owned)),
        Some("alabaster".to_owned()),
        "the choice is remembered in {key}"
    );
    let reloaded = crate::themes::theme_for(dir.path(), &workspace, None).expect("a theme");
    assert_eq!(
        reloaded.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
        expected.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
        "and the next run starts on it"
    );

    // `auto` is the absence of a choice, not a palette.
    chat.slash("/theme auto").expect("the command parses");
    let settings =
        titi_config::settings::Settings::load(dir.path(), &workspace, &[]).expect("settings");
    assert_eq!(settings.get(key), None, "auto clears the slot");
    let auto = theme_frame(&mut chat);
    assert!(auto.contains("(auto)"), "{auto}");
}

/// A closing `:` expands a known shortcode and the caret lands on the
/// glyph's far side; an unknown name stays exactly as it was typed.
#[test]
fn a_closing_colon_expands_the_shortcode_and_the_caret_follows_it() {
    {
        let mut chat = chat();
        type_text(&mut chat, ":tada:");
        let frame = frame_text(&mut chat);
        assert!(frame.contains('🎉'), "{frame}");
        assert!(
            !frame.contains(":tada:"),
            "the keystrokes are gone, the glyph is not: {frame}"
        );
        assert!(
            frame.contains("🎉▍"),
            "the caret sits directly after the glyph: {frame}"
        );
        assert!(!chat.emoji_picker.is_visible());
    }

    // Unknown names stay literal, and nothing was expanded for them.
    {
        let mut chat = chat();
        type_text(&mut chat, ":nope:");
        let frame = frame_text(&mut chat);
        assert!(frame.contains(":nope:▍"), "{frame}");
        assert!(
            !frame.contains("emoji ·"),
            "a name with no match opens no picker: {frame}"
        );
    }
}

/// A terminating space expands an emoticon, and so does Enter; a fenced
/// block keeps the keystrokes, and a URL's colon is never a shortcode.
#[test]
fn a_terminator_expands_an_emoticon_and_a_fence_or_url_keeps_the_text() {
    {
        let mut chat = chat();
        type_text(&mut chat, ":-)");
        type_text(&mut chat, " ");
        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("🙂 ▍"),
            "space replaced :-) and is kept before the caret: {frame}"
        );
        assert!(!frame.contains(":-)"), "{frame}");
    }

    // Enter is the other terminator: the sent line holds the glyph, not
    // the keystrokes.
    {
        let mut chat = chat();
        type_text(&mut chat, "<3");
        chat.on_key(Key::Enter, Instant::now());
        assert!(
            chat.lines.iter().any(|line| line.text == "❤️"),
            "the sent line holds the glyph: {:?}",
            chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
        );
        let frame = frame_text(&mut chat);
        assert!(frame.contains("❤️"), "{frame}");
    }

    // A fence is code-like: the text is shown, not expanded. A paste is
    // how a fence gets into the composer (Enter would send the line).
    {
        let mut chat = chat();
        chat.paste("```\n:-)");
        type_text(&mut chat, " ");
        let frame = frame_text(&mut chat);
        assert!(frame.contains(":-)"), "a fence keeps it: {frame}");
        assert!(!frame.contains("🙂"), "{frame}");
    }

    // The word-like character before the opening colon keeps a URL whole.
    {
        let mut chat = chat();
        type_text(&mut chat, "http://x:y:");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("http://x:y:"), "{frame}");
    }
}

/// `:xx` opens the picker with the matching rows; Tab takes the
/// highlighted glyph and consumes the query, Esc closes and leaves the
/// text exactly as it was typed.
#[test]
fn a_trailing_query_opens_the_picker_and_tab_takes_a_row() {
    {
        let mut chat = chat();
        type_text(&mut chat, ":sm");
        let frame = frame_text(&mut chat);
        assert!(frame.contains("emoji ·"), "the picker is up: {frame}");
        assert!(frame.contains("smiley"), "{frame}");
        assert!(frame.contains("smirk"), "{frame}");
        assert!(frame.contains("🙂"), "a row carries its glyph: {frame}");

        // Tab takes the highlighted row — `smiley`, the first match — and
        // the `:sm` is gone.
        chat.on_key(Key::Tab, Instant::now());
        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("😊"),
            "tab took the highlighted row: {frame}"
        );
        assert!(!frame.contains(":sm"), "the query is consumed: {frame}");
        assert!(!frame.contains("emoji ·"), "and the picker closed: {frame}");
        assert!(!chat.emoji_picker.is_visible());
    }

    // Backspace takes the query back and the picker follows it, the way
    // the model browser's does.
    {
        let mut chat = chat();
        type_text(&mut chat, ":smi");
        assert!(frame_text(&mut chat).contains("emoji ·"));
        chat.on_key(Key::Backspace, Instant::now());
        let frame = frame_text(&mut chat);
        assert!(frame.contains(":sm▍"), "{frame}");
        assert!(
            frame.contains("emoji ·"),
            "the query still stands, so the picker stays: {frame}"
        );
    }

    // Esc closes and leaves the text alone, the way the slash list's does.
    {
        let mut chat = chat();
        type_text(&mut chat, ":sm");
        chat.on_key(Key::Esc, Instant::now());
        let frame = frame_text(&mut chat);
        assert!(frame.contains(":sm▍"), "esc leaves the text: {frame}");
        assert!(!frame.contains("emoji ·"), "{frame}");
        assert!(!chat.emoji_picker.is_visible());
    }
}

/// While the emoji picker is up Enter belongs to it — the line is not
/// sent. The slash list's own Enter is untouched, and the two panels
/// never show at once.
#[test]
fn the_emoji_picker_owns_enter_and_the_slash_list_keeps_its_own() {
    {
        let mut chat = chat();
        type_text(&mut chat, ":sm");
        chat.on_key(Key::Enter, Instant::now());
        assert!(
            chat.lines.is_empty() && !chat.turn_active,
            "enter took the row instead of sending: {:?}",
            chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
        );
        let frame = frame_text(&mut chat);
        assert!(frame.contains("😊"), "{frame}");
        assert!(!frame.contains("emoji ·"), "{frame}");
    }

    // A `/` trigger opens the slash list, not the emoji picker, and its
    // Enter is the list's: `/he` completes and `/help` runs.
    {
        let mut chat = chat();
        type_text(&mut chat, "/he");
        assert!(
            !chat.emoji_picker.is_visible(),
            "a slash trigger does not open the emoji picker"
        );
        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("/help"),
            "the slash list owns the panel: {frame}"
        );
        assert!(!frame.contains("emoji ·"), "{frame}");
        chat.on_key(Key::Enter, Instant::now());
        let frame = frame_text(&mut chat);
        assert!(
            frame.contains("ask titi…"),
            "enter sent the completed command, so the composer is empty: {frame}"
        );
    }
}

/// An expansion — and the picker showing a glyph — leave every row exactly
/// the pane's width: a glyph is two cells, measured with the crate's
/// width model and never with `chars().count()`.
#[test]
fn a_frame_after_an_expansion_still_fits_the_pane() {
    {
        let mut chat = chat();
        type_text(&mut chat, "ship it :tada:");
        assert!(frame_text(&mut chat).contains("🎉"));
        for (width, height) in [(60u16, 20u16), (80, 20), (120, 30)] {
            let rows = frame_rows(&mut chat, width, height);
            assert_eq!(rows.len(), height as usize);
            for row in &rows {
                assert_eq!(
                    titi_tui::width::visible_width(row),
                    width as usize,
                    "{width}x{height}: {row:?}"
                );
            }
        }
    }

    // The picker's own rows carry glyphs too; they fit just the same.
    {
        let mut chat = chat();
        type_text(&mut chat, ":sm");
        for (width, height) in [(60u16, 20u16), (80, 20), (120, 30)] {
            let rows = frame_rows(&mut chat, width, height);
            assert_eq!(rows.len(), height as usize);
            for row in &rows {
                assert_eq!(
                    titi_tui::width::visible_width(row),
                    width as usize,
                    "{width}x{height}: {row:?}"
                );
            }
        }
    }
}

/// A theme this build does not carry is refused by name, never silently
/// replaced by another palette, and the refusal names what there is.
#[test]
fn an_unknown_theme_is_refused_by_name() {
    let reason = crate::themes::theme_named("nope").expect_err("nope is not a theme");
    assert!(reason.contains("unknown theme nope"), "{reason}");
    assert!(
        reason.contains(&crate::themes::theme_names().len().to_string()),
        "the refusal counts what exists: {reason}"
    );
    assert!(
        reason.contains("/theme"),
        "and says where the list is: {reason}"
    );

    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.slash("/theme nope").expect("the command parses");
    let last = chat.lines.last().expect("a line");
    assert_eq!(last.kind, LineKind::Error, "{:?}", chat.lines);
    assert!(last.text.contains("unknown theme nope"), "{}", last.text);
}

/// With nothing set, the screen resolves exactly as it did before there was
/// a setting: the crate's own pick for the appearance the terminal reports.
#[test]
fn auto_is_the_absence_of_a_choice() {
    let _guard = theme_lock();
    let dir = tempfile::tempdir().expect("temp");
    let workspace = crate::session_fs::current_workspace();
    let inputs = titi_tui::theme::appearance::AppearanceInputs::from_env();
    let picked = crate::themes::theme_for(dir.path(), &workspace, None).expect("auto");
    let expected = titi_tui::theme::loader::load_theme(
        &titi_tui::theme::appearance::resolve_auto_theme(
            titi_tui::theme::appearance::AUTO_DARK_THEME,
            titi_tui::theme::appearance::AUTO_LIGHT_THEME,
            &inputs,
        ),
        &theme_options(),
    )
    .expect("the crate's own pick loads");
    assert_eq!(
        picked.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
        expected.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
        "auto is the crate's pick, not a new default"
    );

    // Both slots set: whichever appearance the terminal reports, that name
    // is what the screen shows.
    let mut settings =
        titi_config::settings::Settings::load(dir.path(), &workspace, &[]).expect("settings");
    settings
        .set(
            titi_config::settings::THEME_DARK_KEY,
            serde_json::json!("alabaster"),
        )
        .expect("write");
    settings
        .set(
            titi_config::settings::THEME_LIGHT_KEY,
            serde_json::json!("alabaster"),
        )
        .expect("write");
    let chosen = crate::themes::theme_for(dir.path(), &workspace, None).expect("chosen");
    let alabaster =
        titi_tui::theme::loader::load_theme("alabaster", &theme_options()).expect("alabaster");
    assert_eq!(
        chosen.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
        alabaster.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
        "the setting is read at startup"
    );

    // `--theme` wins over the setting for one run.
    let forced =
        crate::themes::theme_for(dir.path(), &workspace, Some("titanium")).expect("a named theme");
    let titanium =
        titi_tui::theme::loader::load_theme("titanium", &theme_options()).expect("titanium");
    assert_eq!(
        forced.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
        titanium.get_bg_hex(titi_tui::theme::schema::ThemeBg::StatusLineBg),
        "the flag overrides the setting"
    );
    assert!(
        crate::themes::theme_for(dir.path(), &workspace, Some("nope")).is_err(),
        "and an unknown name is refused"
    );
}

/// The default the rest of the CLI builds its theme with.
fn theme_options() -> titi_tui::theme::loader::CreateThemeOptions {
    titi_tui::theme::loader::CreateThemeOptions {
        mode: Some(titi_tui::theme::ColorMode::Truecolor),
        ..Default::default()
    }
}

/// A chat on a temp agent directory with one model and a key stored for it:
/// the facts the welcome reads are the test's own, not this machine's.
fn welcome_chat() -> (tempfile::TempDir, Chat) {
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec!["openai/gpt-4.1".to_owned()]);
    crate::secrets::store_key(&chat.agent_dir, "openai", "sk-test").expect("store a key");
    (dir, chat)
}

/// Two stored sessions with a message each, so the welcome has a list of
/// recent ones to offer.
fn with_two_sessions(chat: &Chat) {
    let store = titi_core::session::SessionStore::new(&chat.agent_dir).expect("session store");
    for title in ["one", "two"] {
        let id = store
            .create(titi_core::session::SessionMeta {
                title: Some(title.to_owned()),
                source: Some("cli".to_owned()),
                ..Default::default()
            })
            .expect("create");
        store.append(&id, Role::User, title).expect("append");
    }
}

/// A frame as one string at `width` x `height`, for a test that is about
/// which rows are on screen rather than about a row's cells.
fn frame_at(chat: &mut Chat, width: u16, height: u16) -> String {
    frame_rows(chat, width, height).join("\n")
}

/// The rows between the masthead and the composer: where the welcome is
/// drawn on an idle screen with no panel open.
fn welcome_area(rows: &[String]) -> &[String] {
    rows.get(1..rows.len().saturating_sub(4))
        .unwrap_or_default()
}

/// The welcome's rows of a frame at `width` x `height`, as one string.
fn welcome_at(chat: &mut Chat, width: u16, height: u16) -> String {
    welcome_area(&frame_rows(chat, width, height)).join("\n")
}

/// Whether a fact row carrying `label` is on screen: a label opens its
/// row, so the masthead's model id or the composer's `/model` does not
/// count as the welcome's model row.
fn states_fact(welcome: &str, label: &str) -> bool {
    welcome
        .lines()
        .any(|row| row.trim_start().starts_with(&format!("{label} ")))
}

/// Whether there is a blank row between the welcome's first and last rows.
fn spaced(welcome: &str) -> bool {
    let rows: Vec<&str> = welcome.lines().collect();
    let first = rows.iter().position(|row| !row.trim().is_empty());
    let last = rows.iter().rposition(|row| !row.trim().is_empty());
    match (first, last) {
        (Some(first), Some(last)) => rows[first..=last].iter().any(|row| row.trim().is_empty()),
        _ => false,
    }
}

/// What the welcome states for the directory: the path the snapshot gives,
/// or its leaf behind the ellipsis a row shortens a long value to.
fn states_path(frame: &str, path: &str) -> bool {
    frame.contains(path)
        || (frame.contains('…')
            && !path.is_empty()
            && frame.contains(path.rsplit('/').next().unwrap_or(path)))
}

/// A branch is on screen whole or not at all: four cells of one is not a
/// branch, whatever the row's width happened to leave.
fn states_branch_whole(frame: &str, branch: &str) -> bool {
    frame.contains(branch) || !frame.contains(&branch.chars().take(4).collect::<String>())
}

/// The text of the fact row carrying `label`.
fn row_with_label(body: &[Line<'static>], label: &str) -> String {
    body.iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .find(|row| row.contains(label))
        .unwrap_or_else(|| panic!("no {label} row in {body:?}"))
}

/// The cells the welcome draws something on, as symbol and foreground.
fn welcome_cells(chat: &mut Chat, width: u16, height: u16) -> Vec<(String, Color)> {
    let buffer = frame_buffer(chat, width, height);
    (1..height.saturating_sub(4))
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .map(|(x, y)| (buffer[(x, y)].symbol().to_owned(), buffer[(x, y)].fg))
        .filter(|(symbol, _)| !symbol.trim().is_empty())
        .collect()
}

/// A colour's gray level, or `None` when its channels differ: a colour
/// with a hue in it is not black and white.
fn gray_level(color: Color) -> Option<u8> {
    match color {
        Color::Rgb(r, g, b) if r == g && g == b => Some(r),
        _ => None,
    }
}

/// BT.601 luma of a colour, 0 to 255.
fn luma(color: Color) -> f64 {
    match color {
        Color::Rgb(r, g, b) => 0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b),
        other => panic!("not an RGB colour: {other:?}"),
    }
}

/// The first screen states the build, the model behind the next turn, what
/// stands behind that model, and the directory — each read from the source
/// that owns it — under the TITI mark, the whole lockup centred in the pane
/// with no box around it.
#[test]
fn the_welcome_names_the_build_the_model_and_the_directory() {
    let (_dir, mut chat) = welcome_chat();
    let snapshot = masthead_snapshot(&chat);
    let build = format!("v{}", titi_tui::VERSION);
    for width in [60u16, 80, 120] {
        let rows = frame_rows(&mut chat, width, 20);
        let area = welcome_area(&rows);
        let frame = area.join("\n");
        assert!(
            frame.contains("say what you want done"),
            "{width}: and what to do: {frame}"
        );
        assert!(
            frame.contains(&chat.model),
            "{width}: the model that will answer: {frame}"
        );
        assert!(
            frame.contains("·  key"),
            "{width}: with the credential /keys reads for it: {frame}"
        );
        assert!(
            states_path(&frame, &snapshot.path),
            "{width}: the directory the masthead reads: {frame}"
        );
        if let Some(branch) = &snapshot.git_branch {
            assert!(
                states_branch_whole(&frame, branch),
                "{width}: and its git state, whole or not at all: {frame}"
            );
        }
        for expected in ["enter  send", "alt+m  models", "ctrl-c  quit"] {
            assert!(
                frame.contains(expected),
                "{width}: the chords that work are advertised: {expected} missing from {frame}"
            );
        }
        // The lockup: the mark with the build beside it, centred as one
        // block, and nothing drawn around it.
        let top = area
            .iter()
            .position(|row| row.contains("██████ ██████"))
            .unwrap_or_else(|| panic!("{width}: no mark: {frame}"));
        let lockup = &area[top..top + 5];
        assert!(
            lockup.iter().any(|row| row.contains(&build)),
            "{width}: the build is named beside the mark: {frame}"
        );
        let margin = |row: &String, trimmed: &str| {
            titi_tui::width::visible_width(row) - titi_tui::width::visible_width(trimmed)
        };
        let left = lockup
            .iter()
            .map(|row| margin(row, row.trim_start()))
            .min()
            .unwrap_or_default();
        let right = lockup
            .iter()
            .map(|row| margin(row, row.trim_end()))
            .min()
            .unwrap_or_default();
        assert!(
            left.abs_diff(right) <= 1,
            "{width}: the lockup is centred ({left} | {right}): {lockup:#?}"
        );
        for corner in ['╭', '╰', '│'] {
            assert!(!frame.contains(corner), "{width}: no box is drawn: {frame}");
        }
        for row in &rows {
            assert_eq!(
                titi_tui::width::visible_width(row),
                width as usize,
                "{width}: {row:?}"
            );
        }
    }
}

/// The welcome is black and white on every palette: every cell it draws
/// is a gray, and the mark stands off the page the way ink does — lighter
/// than a dark page, darker than a light one — rather than in whatever hue
/// a theme's accent or its git colours happen to be.
#[test]
fn the_welcome_is_black_and_white_on_every_palette() {
    let (mut dark, mut light) = (0, 0);
    for name in titi_tui::theme::builtin::list_builtin_themes() {
        let (_dir, mut chat) = welcome_chat();
        chat.theme = test_theme_named(name);
        let page = luma(bg(&chat.theme, ThemeBg::StatusLineBg));
        let cells = welcome_cells(&mut chat, 80, 24);
        for (symbol, color) in &cells {
            assert!(
                gray_level(*color).is_some(),
                "{name}: {symbol:?} is drawn in {color:?}, which is not a gray"
            );
        }
        let mark: Vec<f64> = cells
            .iter()
            .filter(|(symbol, _)| symbol == "█")
            .filter_map(|(_, color)| gray_level(*color).map(f64::from))
            .collect();
        assert!(!mark.is_empty(), "{name}: the mark is drawn");
        if page < 127.5 {
            let brightest = mark.iter().copied().fold(0.0, f64::max);
            assert!(
                brightest > page,
                "{name}: the mark ({brightest}) is lighter than a dark page ({page})"
            );
            dark += 1;
        } else {
            let darkest = mark.iter().copied().fold(255.0, f64::min);
            assert!(
                darkest < page,
                "{name}: the mark ({darkest}) is darker than a light page ({page})"
            );
            light += 1;
        }
    }
    assert!(
        dark > 0 && light > 0,
        "the presets hold dark pages ({dark}) and light ones ({light})"
    );
    for (name, page_is_dark) in [("titanium", true), ("alabaster", false)] {
        let page = luma(bg(&test_theme_named(name), ThemeBg::StatusLineBg));
        assert_eq!(page < 127.5, page_is_dark, "{name}: {page}");
    }
}

/// The facts are one block: left-aligned under one another, and the block
/// centred in the pane as a whole, so the values read down one column
/// instead of each row drifting to its own centre.
#[test]
fn the_welcome_facts_are_one_left_aligned_block() {
    let (_dir, mut chat) = welcome_chat();
    with_two_sessions(&chat);
    for width in [60u16, 80, 120] {
        let rows = frame_rows(&mut chat, width, 24);
        let area = welcome_area(&rows);
        let model = area
            .iter()
            .position(|row| row.trim_start().starts_with("model "))
            .unwrap_or_else(|| panic!("{width}: no model row: {area:#?}"));
        // The model, the directory, and the two sessions.
        let block = &area[model..model + 4];
        let indent = |row: &String| {
            titi_tui::width::visible_width(row) - titi_tui::width::visible_width(row.trim_start())
        };
        let left = indent(&block[0]);
        assert!(
            block[..3].iter().all(|row| indent(row) == left),
            "{width}: every label starts in one column: {block:#?}"
        );
        assert_eq!(
            indent(&block[3]),
            left + WELCOME_LABEL,
            "{width}: and a second session sits under the first: {block:#?}"
        );
        assert!(
            block[1].trim_start().starts_with("dir ")
                && block[2].trim_start().starts_with("recent "),
            "{width}: {block:#?}"
        );
        let widest = block
            .iter()
            .map(|row| titi_tui::width::visible_width(row.trim_end()) - left)
            .max()
            .unwrap_or_default();
        let right = width as usize - left - widest;
        assert!(
            left.abs_diff(right) <= 1,
            "{width}: the block is centred ({left} | {right}): {block:#?}"
        );
    }
}

/// A fact's tail is stated whole or dropped whole. A detached HEAD's short
/// sha, cut where the row happens to end, reads as whatever cells fitted —
/// so the welcome gives the git state up rather than print half of it, the way
/// the masthead gives it up when the pane is narrow. A value too long for
/// its row keeps its informative end behind an ellipsis, never a bare cut.
#[test]
fn the_welcome_states_a_fact_whole_or_not_at_all() {
    let grays = WelcomeGrays::of(&test_theme());
    let facts = WelcomeFacts {
        version: "0.0.0",
        model: "opencode-go/glm-5.3-flash".to_owned(),
        credential: Some("key".to_owned()),
        path: "/tmp/titi".to_owned(),
        git: Some(WelcomeGit {
            branch: "636c207".to_owned(),
            unstaged: 2,
            staged: 0,
            untracked: 0,
        }),
        sessions: Vec::new(),
    };
    let room = 59;

    let wide = welcome_fact_rows(&facts, 0, room, &grays);
    let row = row_with_label(&wide, "dir");
    assert!(
        row.contains("/tmp/titi") && row.contains("636c207") && row.contains("*2"),
        "a row with room states the fact and its tail: {row}"
    );
    let row = row_with_label(&wide, "model");
    assert!(
        row.contains("opencode-go/glm-5.3-flash") && row.contains("key"),
        "{row}"
    );

    // No room for the branch: it goes, the directory stays.
    let long_path = WelcomeFacts {
        path: format!("/{}", "deep/".repeat(12)),
        ..facts.clone()
    };
    let row = row_with_label(&welcome_fact_rows(&long_path, 0, room, &grays), "dir");
    assert!(
        !row.contains("636c207"),
        "the branch is gone, not cut: {row}"
    );
    assert!(!row.contains("636c"), "and no part of it is left: {row}");
    assert!(
        row.contains('…'),
        "the path itself is shortened honestly: {row}"
    );
    assert!(row.contains("deep"), "to something still readable: {row}");

    // Same for the credential word.
    let long_model = WelcomeFacts {
        model: "x".repeat(room),
        ..facts.clone()
    };
    let row = row_with_label(&welcome_fact_rows(&long_model, 0, room, &grays), "model");
    assert!(
        !row.contains("key"),
        "the credential word is not cut either: {row}"
    );
    assert!(
        row.contains('…'),
        "and the model keeps its informative end: {row}"
    );

    // The boundary is exact: the row is `room` cells, the label 9, and this
    // git tail is 15 (`  ·  ` + `636c207` + ` *2`).
    let fits = WelcomeFacts {
        path: "p".repeat(35),
        ..facts.clone()
    };
    let row = row_with_label(&welcome_fact_rows(&fits, 0, room, &grays), "dir");
    assert!(
        row.contains("636c207") && row.contains("*2"),
        "a tail that fits to the cell is stated: {row}"
    );
    let over = WelcomeFacts {
        path: "p".repeat(36),
        ..facts.clone()
    };
    let row = row_with_label(&welcome_fact_rows(&over, 0, room, &grays), "dir");
    assert!(!row.contains("636c207"), "and one cell over is not: {row}");

    // The session on screen keeps its mark whole; its name gives the room up.
    let long_session = WelcomeFacts {
        sessions: vec![("s".repeat(room), true)],
        ..facts.clone()
    };
    let row = row_with_label(&welcome_fact_rows(&long_session, 0, room, &grays), "recent");
    assert!(
        row.ends_with("✓ current") && row.contains('…'),
        "the mark is whole and the name is shortened: {row}"
    );
    assert_eq!(titi_tui::width::visible_width(&row), room, "{row}");
}

/// A session is named on the welcome the way the switcher names it.
#[test]
fn the_welcome_names_a_session_as_the_switcher_does() {
    let (_dir, mut chat) = welcome_chat();
    let store = titi_core::session::SessionStore::new(&chat.agent_dir).expect("session store");
    let named = store
        .create(titi_core::session::SessionMeta {
            title: Some("named".to_owned()),
            source: Some("cli".to_owned()),
            ..Default::default()
        })
        .expect("create");
    chat.session_id = named.clone();
    let frame = welcome_at(&mut chat, 80, 20);
    assert!(
        frame.contains(&session_row_text(&named, true)),
        "the welcome marks the session on screen the way the switcher does: {frame}"
    );
}

/// The credential word is the model picker's, not a second vocabulary.
#[test]
fn the_welcome_states_a_subscription_as_oauth() {
    let (_dir, mut chat) = welcome_chat();
    crate::secrets::remove_key(&chat.agent_dir, "openai").expect("remove the key");
    crate::secrets::store_oauth(
        &chat.agent_dir,
        "openai",
        &titi_providers::oauth::OAuthTokens {
            access: "sk-test".to_owned(),
            refresh: Some("sk-test".to_owned()),
            expires_at: None,
            account_id: None,
            email: None,
            org_id: None,
            org_name: None,
        },
    )
    .expect("store a sign-in");
    let frame = welcome_at(&mut chat, 80, 20);
    assert!(
        frame.contains("·  oauth"),
        "a subscription reads as oauth: {frame}"
    );
    assert!(!frame.contains("·  key"), "and not as a key: {frame}");
}

/// The welcome is an empty state: one transcript line takes the screen,
/// and emptying the transcript brings it back — a rewind to nothing, or a
/// switch to a session with no history.
#[test]
fn the_welcome_yields_the_screen_to_a_line() {
    let (_dir, mut chat) = welcome_chat();
    assert!(frame_at(&mut chat, 80, 20).contains("say what you want done"));

    chat.push(LineKind::User, "hello".to_owned());
    let frame = frame_at(&mut chat, 80, 20);
    assert!(
        !frame.contains("say what you want done") && !frame.contains("██████"),
        "one line is enough to take the screen: {frame}"
    );
    assert!(frame.contains("hello"), "{frame}");

    chat.show_history(&[]);
    let frame = frame_at(&mut chat, 80, 20);
    assert!(
        frame.contains("say what you want done") && frame.contains("██████ ██████"),
        "and an empty transcript brings it back: {frame}"
    );
}

/// A session with no history yet — a fresh agent directory — has no list to
/// offer, so the welcome states the facts it does have and no empty heading.
#[test]
fn a_session_with_no_history_shows_no_list() {
    let (_dir, mut chat) = welcome_chat();
    assert!(
        chat.session_choices().is_empty(),
        "a fresh directory has no sessions to list"
    );
    let frame = welcome_at(&mut chat, 80, 20);
    assert!(
        !states_fact(&frame, "recent"),
        "no list is offered: {frame}"
    );
    assert!(states_fact(&frame, "model"), "but the model is: {frame}");
    assert!(states_fact(&frame, "dir"), "and the directory: {frame}");
}

/// A short pane gives the welcome down in one order — the tip, the recent
/// sessions, then the directory, then the tagline, then the blank rows,
/// then the model — and the lockup with the chords outlasts all of them.
/// When even those two do not fit, the brand and its build fold into one
/// line.
#[test]
fn a_short_pane_gives_the_welcome_down_in_order() {
    let (_dir, mut chat) = welcome_chat();
    with_two_sessions(&chat);
    let build = format!("v{}", titi_tui::VERSION);
    let chords = |frame: &str| {
        frame.contains("enter  send")
            && frame.contains("alt+m  models")
            && frame.contains("ctrl-c  quit")
    };

    // The pane is the screen less the masthead and the composer's four
    // rows; each height below is the first that drops one more thing.
    let roomy = welcome_at(&mut chat, 80, 21);
    assert!(roomy.contains("Tip: "), "a roomy pane has a tip: {roomy}");

    let no_tip = welcome_at(&mut chat, 80, 19);
    assert!(!no_tip.contains("Tip: "), "the tip goes first: {no_tip}");
    for label in ["recent", "dir", "model"] {
        assert!(
            states_fact(&no_tip, label),
            "the facts are all there: {label} missing from {no_tip}"
        );
    }
    assert!(no_tip.contains("say what you want done"), "{no_tip}");

    let no_sessions = welcome_at(&mut chat, 80, 17);
    assert!(
        !states_fact(&no_sessions, "recent"),
        "then the list: {no_sessions}"
    );
    assert!(
        states_fact(&no_sessions, "dir"),
        "the directory is still there: {no_sessions}"
    );

    let no_directory = welcome_at(&mut chat, 80, 16);
    assert!(
        !states_fact(&no_directory, "dir"),
        "then the directory: {no_directory}"
    );
    assert!(
        no_directory.contains("say what you want done"),
        "the tagline is still there: {no_directory}"
    );

    let no_tagline = welcome_at(&mut chat, 80, 14);
    assert!(
        !no_tagline.contains("say what you want done"),
        "then the tagline: {no_tagline}"
    );
    assert!(
        states_fact(&no_tagline, "model") && spaced(&no_tagline),
        "the model and the blank rows around it are still there: {no_tagline}"
    );

    let tight = welcome_at(&mut chat, 80, 12);
    assert!(!spaced(&tight), "then the blank rows: {tight}");
    assert!(
        states_fact(&tight, "model") && chords(&tight),
        "which keeps the model with the chords: {tight}"
    );

    let bare = welcome_at(&mut chat, 80, 11);
    assert!(!states_fact(&bare, "model"), "then the model: {bare}");
    assert!(
        bare.contains("██████ ██████") && bare.contains(&build) && chords(&bare),
        "the lockup with the build, and the chords, outlast it: {bare}"
    );

    let brand = welcome_at(&mut chat, 80, 10);
    assert!(
        !brand.contains("██████"),
        "a pane with no room for the mark: {brand}"
    );
    assert!(
        brand.contains(&format!("titi {build}")) && chords(&brand),
        "states the brand and its build in one line, over the chords: {brand}"
    );

    // So does a pane too narrow for the mark, however tall it is.
    let narrow = welcome_at(&mut chat, 26, 24);
    assert!(
        !narrow.contains("██████") && narrow.contains(&format!("titi {build}")),
        "{narrow}"
    );
    // Its chords are the ones that fit, each one whole.
    assert!(narrow.contains("enter  send"), "{narrow}");
    for (key, chord) in [("alt+m", "alt+m  models"), ("ctrl-c", "ctrl-c  quit")] {
        assert!(
            !narrow.contains(key) || narrow.contains(chord),
            "{chord} is whole or absent: {narrow}"
        );
    }
}

/// The welcome offers one tip, and a true one: every command a tip names
/// is one this build runs, and every tip fits whole on the narrowest pane
/// that shows one. The tip is picked from the session, so a redraw keeps it
/// instead of flickering to another, and a pane too narrow for it shows
/// none rather than a sentence cut short.
#[test]
fn the_welcome_offers_one_true_tip_and_keeps_it() {
    for tip in WELCOME_TIPS {
        for word in tip.split_whitespace().filter(|word| word.starts_with('/')) {
            assert!(
                COMMANDS
                    .iter()
                    .any(|command| format!("/{}", command.name) == word),
                "{tip}: {word} is a command"
            );
        }
        assert!(
            titi_tui::width::visible_width(&format!("Tip: {tip}")) + 2 <= WELCOME_TIP_COLUMNS,
            "{tip} fits whole at {WELCOME_TIP_COLUMNS} columns"
        );
    }

    let (_dir, mut chat) = welcome_chat();
    let tip_of = |frame: &str| {
        frame
            .lines()
            .find_map(|row| row.trim().strip_prefix("Tip: ").map(str::to_owned))
    };
    let first = welcome_at(&mut chat, 80, 24);
    let tip = tip_of(&first).unwrap_or_else(|| panic!("a tip is offered: {first}"));
    assert!(
        WELCOME_TIPS.contains(&tip.as_str()),
        "{tip:?} is one of the listed tips"
    );
    assert_eq!(
        tip_of(&welcome_at(&mut chat, 80, 24)),
        Some(tip.clone()),
        "and the next frame offers the same one"
    );
    let rows = frame_rows(&mut chat, 80, 24);
    let (x, y) = cell_of(&rows, "Tip: ").unwrap_or_else(|| panic!("{rows:#?}"));
    assert!(
        frame_buffer(&mut chat, 80, 24)[(x, y)]
            .modifier
            .contains(Modifier::ITALIC),
        "the tip is set in italic"
    );

    let narrow = WELCOME_TIP_COLUMNS as u16;
    assert!(welcome_at(&mut chat, narrow, 24).contains("Tip: "));
    let narrower = welcome_at(&mut chat, narrow - 1, 24);
    assert!(!narrower.contains("Tip:"), "{narrower}");

    // Another session may offer another tip: the pick follows the session.
    let picked: HashSet<&str> = (0..64)
        .filter_map(|n| welcome_tip(&format!("session-{n}")))
        .collect();
    assert!(picked.len() > 1, "{picked:?}");
}

/// The welcome holds still until the live screen starts its intro: two
/// frames of a chat that never started one are the same cells, and an
/// intro that has run its course leaves exactly that resting frame.
#[test]
fn the_welcome_rests_unless_the_screen_starts_its_intro() {
    let (_dir, mut chat) = welcome_chat();
    let rest = frame_buffer(&mut chat, 80, 24);
    assert_eq!(
        frame_buffer(&mut chat, 80, 24),
        rest,
        "a chat that never started the intro draws one frame"
    );
    let long_ago = Instant::now()
        .checked_sub(WELCOME_INTRO * 2)
        .expect("the clock has run longer than two intros");
    chat.start_intro(long_ago);
    assert_eq!(
        frame_buffer(&mut chat, 80, 24),
        rest,
        "and a finished intro settles on it"
    );
}

/// The intro sweeps a shine across the mark, from the lit corner to the
/// far one, quick at first and slowing into the end, and is gone by the
/// time it has run: the band starts and ends off the mark, so its first
/// and last frames are the resting one. Every cell it lights stays a gray,
/// pushed toward the page's far end — whiter on a dark page, blacker on a
/// light one.
#[test]
fn the_intro_sweeps_a_shine_across_the_mark() {
    assert!(
        (Duration::from_millis(1200)..=Duration::from_millis(1500)).contains(&WELCOME_INTRO),
        "{WELCOME_INTRO:?}"
    );
    let at = |millis: u64| welcome_shine(Duration::from_millis(millis));
    assert_eq!(welcome_shine(WELCOME_INTRO), None, "it is over in time");
    assert_eq!(welcome_shine(WELCOME_INTRO * 3), None, "and stays over");
    let steps: Vec<f64> = (0..WELCOME_INTRO.as_millis() as u64)
        .step_by(50)
        .map(|millis| at(millis).expect("still sweeping"))
        .collect();
    assert!(
        steps.windows(2).all(|pair| pair[0] < pair[1]),
        "it moves one way: {steps:?}"
    );
    let half = WELCOME_INTRO.as_millis() as u64 / 2;
    let (start, middle, end) = (
        steps[0],
        at(half).unwrap_or_default(),
        steps[steps.len() - 1],
    );
    assert!(
        middle - start > end - middle,
        "and eases out: {start} → {middle} → {end}"
    );

    for name in ["titanium", "alabaster"] {
        let grays = WelcomeGrays::of(&test_theme_named(name));
        let levels = |shine: Option<f64>| -> Vec<u8> {
            welcome_mark(&grays, shine)
                .into_iter()
                .flatten()
                .filter(|span| span.content.trim() != "")
                .map(|span| {
                    let color = span.style.fg.unwrap_or(Color::Reset);
                    gray_level(color).unwrap_or_else(|| panic!("{name}: {color:?} is not a gray"))
                })
                .collect()
        };
        let rest = levels(None);
        assert_eq!(
            levels(at(0)),
            rest,
            "{name}: the first frame is the resting one"
        );
        let lit = levels(at(250));
        assert_ne!(lit, rest, "{name}: the shine shows mid-sweep");
        let dark = grays.bright > 0.5;
        for (lit, rest) in lit.iter().zip(&rest) {
            assert!(
                if dark { lit >= rest } else { lit <= rest },
                "{name}: the shine moves a cell toward the page's far end ({rest} → {lit})"
            );
        }
    }
}

/// The picker above the composer is a box: the title sits inset in the top
/// rule, and the cursor's row carries the theme's selection band as well as
/// the marker, so the choice is legible on a terminal whose colours are dim.
#[test]
fn the_panel_is_a_titled_box_and_the_selected_row_carries_the_band() {
    for width in [60u16, 80, 120] {
        let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
        chat.catalog = crate::engine::ModelCatalog::fixed(vec![
            "openai/gpt-4.1".to_owned(),
            "anthropic/claude-opus-5".to_owned(),
        ]);
        type_text(&mut chat, "/model");
        chat.on_key(Key::Enter, Instant::now());

        let buffer = frame_buffer(&mut chat, width, 20);
        let theme = Arc::clone(&chat.theme);
        let rows: Vec<String> = (0..20)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let (x, y) = cell_of(&rows, "╭─ models · 2")
            .unwrap_or_else(|| panic!("{width}: no titled rule: {rows:?}"));
        assert_eq!(x, 0, "{width}: the box starts at the left edge");
        assert!(
            rows[y as usize].trim_end().ends_with('╮'),
            "{width}: the rule closes: {:?}",
            rows[y as usize]
        );

        // The cursor's row: the marker, the theme's band across the row,
        // and the role's own colour on the label.
        let (mx, my) = cell_of(&rows, "▶ openai/gpt-4.1")
            .unwrap_or_else(|| panic!("{width}: the cursor row is unmarked: {rows:?}"));
        let band = bg(&theme, ThemeBg::SelectedBg);
        let page_bg = bg(&theme, ThemeBg::StatusLineBg);
        let rest: Vec<Style> = (2..width - 2).map(|x| buffer[(x, my)].style()).collect();
        for (at, style) in rest.iter().enumerate() {
            assert_eq!(
                style.bg.unwrap_or(Color::Reset),
                band,
                "{width}: column {at} of the cursor row is outside the band"
            );
        }
        assert_eq!(
            buffer[(mx, my)].fg,
            fg(&theme, ThemeColor::CustomMessageLabel)
                .fg
                .unwrap_or(Color::Reset),
            "{width}: the cursor row keeps its role's colour"
        );

        // A row the cursor is not on carries neither the marker nor a band,
        // and the box closes under the last row.
        let (x, y) = cell_of(&rows, "anthropic/claude-opus-5")
            .unwrap_or_else(|| panic!("{width}: the other row is missing: {rows:?}"));
        assert_eq!(
            buffer[(x, y)].bg,
            page_bg,
            "{width}: a row the cursor is not on stays unbanded"
        );
        assert!(
            rows[(y + 1) as usize].starts_with('╰'),
            "{width}: the box closes above the composer: {:?}",
            rows[(y + 1) as usize]
        );
    }
}

/// The bar beside the panel body is the crate's scrollbar: it takes the
/// pane's last column only when the list does not fit, and its thumb follows
/// the window as the cursor walks the list.
#[test]
fn the_panel_bar_appears_only_when_the_list_overflows_and_tracks_the_selection() {
    let width = 80u16;
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "openai/gpt-4.1".to_owned(),
        "anthropic/claude-opus-5".to_owned(),
    ]);
    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());
    let theme = Arc::clone(&chat.theme);
    let border = fg(&theme, ThemeColor::Border).fg.unwrap_or(Color::Reset);

    // Two rows fit: no bar, so the box's border is the pane's last column.
    let buffer = frame_buffer(&mut chat, width, 20);
    let rows: Vec<String> = (0..20)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    let (_, y) = cell_of(&rows, "╭─ models · 2").expect("a titled rule");
    assert_eq!(
        buffer[(width - 1, y)].symbol(),
        "╮",
        "the box reaches the pane's edge when nothing is hidden"
    );
    assert_eq!(
        buffer[(width - 1, y + 1)].fg,
        border,
        "and its border there"
    );

    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(
        (0..30)
            .map(|at| format!("openai/gpt-4.{at}"))
            .collect::<Vec<_>>(),
    );
    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());
    let theme = Arc::clone(&chat.theme);
    let accent = fg(&theme, ThemeColor::Accent).fg.unwrap_or(Color::Reset);
    let muted = fg(&theme, ThemeColor::Muted).fg.unwrap_or(Color::Reset);

    // Thirty-one lines with nine in the window, plus the two `… N more`
    // rows: the thumb covers three of the eleven body rows at the top, and
    // the track runs on below them.
    let (buffer, body) = panel_frame_and_body(&mut chat, width);
    assert_eq!(
        bar_thumb_rows(&buffer, width, body),
        [0, 1, 2],
        "the thumb starts at the top"
    );
    assert_eq!(
        buffer[(width - 1, body + 5)].fg,
        muted,
        "and the track runs on below it"
    );
    assert_eq!(
        buffer[(width - 1, body)].symbol(),
        "█",
        "the thumb is a solid cell"
    );

    for _ in 0..15 {
        chat.on_key(Key::Down, Instant::now());
    }
    let (buffer, body) = panel_frame_and_body(&mut chat, width);
    assert_eq!(
        bar_thumb_rows(&buffer, width, body),
        [5, 6, 7],
        "the thumb moved down with the window"
    );
    assert_eq!(
        accent,
        fg(&theme, ThemeColor::Accent).fg.unwrap_or(Color::Reset)
    );
}

/// A frame and the row just under the panel's top rule.
fn panel_frame_and_body(chat: &mut Chat, width: u16) -> (ratatui::buffer::Buffer, u16) {
    let buffer = frame_buffer(chat, width, 20);
    let rows: Vec<String> = (0..20)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    let (_, y) = cell_of(&rows, "╭─ models · 30").expect("the model panel is open");
    (buffer, y + 1)
}

/// The body rows the bar's thumb covers, read from the pane's last column.
fn bar_thumb_rows(buffer: &ratatui::buffer::Buffer, width: u16, body: u16) -> Vec<u16> {
    let theme = test_theme();
    let accent = fg(&theme, ThemeColor::Accent).fg.unwrap_or(Color::Reset);
    (0..11u16)
        .filter(|at| buffer[(width - 1, body + at)].fg == accent)
        .collect()
}

/// A long list is windowed around the cursor and says how much of itself
/// is out of sight, rather than ending as if that were all of it.
#[test]
fn a_long_list_is_windowed_and_says_what_it_hides() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.catalog = crate::engine::ModelCatalog::fixed(
        (0..30)
            .map(|at| format!("openai/gpt-4.{at}"))
            .collect::<Vec<_>>(),
    );
    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());
    let frame = frame_text(&mut chat);
    assert!(frame.contains("more below"), "something is hidden: {frame}");
    assert!(!frame.contains("more above"), "nothing above yet: {frame}");
    assert!(
        frame.contains("✓ current"),
        "the cursor row is shown: {frame}"
    );

    for _ in 0..15 {
        chat.on_key(Key::Down, Instant::now());
    }
    let frame = frame_text(&mut chat);
    assert!(frame.contains("more above"), "the window scrolled: {frame}");
    assert!(
        frame.contains("more below"),
        "and it still hides the rest: {frame}"
    );
}

/// The window keeps the cursor in view and never exceeds the lines it was
/// given, whatever the selection and the list length.
#[test]
fn the_window_always_contains_the_selection() {
    for len in 1..40usize {
        for selected in 0..len {
            for room in 1..=PICKER_MAX_ROWS {
                let window = panel_window(len, selected, room);
                let lines =
                    window.count + usize::from(window.above > 0) + usize::from(window.below > 0);
                assert!(
                    window.count > 0,
                    "len {len} selected {selected} room {room}"
                );
                assert!(
                    selected >= window.start && selected < window.start + window.count,
                    "len {len} selected {selected} room {room}: {window:?}"
                );
                assert!(window.start + window.count <= len, "{window:?}");
                assert!(lines <= room.max(PICKER_MIN_ROWS), "{window:?}");
            }
        }
    }
}

/// The rows of a narrowing screen give up the least useful part first,
/// and a cut id says it was cut.
#[test]
fn a_narrow_row_drops_the_provider_before_it_cuts_the_model() {
    let row = ModelRow {
        id: "openai-codex/gpt-daybreak-blue-latest-wm".to_owned(),
        provider: "openai-codex".to_owned(),
        context_window: Some(272_000),
        credential: Some("oauth".to_owned()),
    };
    assert_eq!(
        model_row_label(&row, false, 78),
        "openai-codex/gpt-daybreak-blue-latest-wm  ·openai-codex  272k  oauth"
    );
    // The provider is the id's own prefix and the heading above it, so it
    // goes first; then the window.
    assert_eq!(
        model_row_label(&row, false, 60),
        "openai-codex/gpt-daybreak-blue-latest-wm  272k  oauth"
    );
    assert_eq!(
        model_row_label(&row, false, 50),
        "openai-codex/gpt-daybreak-blue-latest-wm  oauth"
    );
    // The credential and the mark are what a narrow row must keep.
    let cut = model_row_label(&row, true, 40);
    assert!(titi_tui::width::visible_width(&cut) <= 40, "{cut}");
    assert!(cut.contains('…'), "{cut}");
    assert!(cut.ends_with("oauth  ✓ current"), "{cut}");
}

/// A turn in flight shows a moving glyph and the seconds it has been
/// running: `working` alone cannot tell a live turn from a stalled one.
#[test]
fn a_running_turn_shows_a_spinner_and_its_elapsed_seconds() {
    let mut chat = chat();
    chat.turn_active = true;
    chat.turn_started = Some(Instant::now() - Duration::from_secs(3));
    let frame = frame_text(&mut chat);
    assert!(frame.contains("working"), "{frame}");
    assert!(
        SPINNER.iter().any(|glyph| frame.contains(glyph)),
        "no spinner frame: {frame}"
    );
    let shown = shown_seconds(&frame).unwrap_or_else(|| panic!("no seconds: {frame}"));
    assert!((3.0..4.0).contains(&shown), "{shown} in {frame}");
}

/// Nothing about a turn is claimed when none is running.
#[test]
fn an_idle_frame_has_no_spinner_and_no_seconds() {
    let mut chat = chat();
    let frame = frame_text(&mut chat);
    assert!(!frame.contains("working"), "{frame}");
    assert!(
        !SPINNER.iter().any(|glyph| frame.contains(glyph)),
        "{frame}"
    );
    assert!(shown_seconds(&frame).is_none(), "{frame}");
}

/// The spinner steps on every loop tick, so consecutive frames differ.
#[test]
fn the_spinner_moves_frame_by_frame() {
    assert_eq!(spinner_frame(Duration::ZERO), SPINNER[0]);
    assert_eq!(spinner_frame(SPINNER_PERIOD), SPINNER[1]);
    assert_ne!(
        spinner_frame(SPINNER_PERIOD * 3),
        spinner_frame(SPINNER_PERIOD * 4)
    );
    assert_eq!(spinner_frame(SPINNER_PERIOD * 10), SPINNER[0], "it wraps");
    assert_eq!(elapsed_label(Duration::from_millis(3_200)), "3.2s");
    assert_eq!(elapsed_label(Duration::from_secs(12)), "12.0s");
}

/// A turn the engine started on its own still gets a start time, and one
/// that ends forgets it: no state outlives its turn.
#[test]
fn the_turn_clock_starts_and_stops_with_the_turn() {
    let mut chat = chat();
    assert!(chat.turn_elapsed().is_none());
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    assert!(chat.turn_elapsed().is_some());
    chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });
    assert!(chat.turn_elapsed().is_none());
    assert!(!chat.turn_active);
    assert!(!frame_text(&mut chat).contains("working"));
}

/// A muted part is not in the row, and muting everything leaves no row at
/// all — the turn's own totals are untouched, so `/usage` still knows what
/// the turn cost.
#[test]
fn a_muted_footer_states_only_what_is_left() {
    use titi_tui::status::TurnFooterSwitches;

    fn finished(switches: TurnFooterSwitches) -> Chat {
        let mut chat = chat();
        chat.turn_footer = switches;
        chat.turn_active = true;
        chat.turn_started = Some(Instant::now() - Duration::from_millis(1_400));
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "answer".into(),
        });
        chat.on_event(EngineEvent::TurnUsage {
            turn_id: TurnId(1),
            prompt_tokens: 3_400,
            completion_tokens: 250,
            cached_tokens: 2_900,
            // An unpriced model: the footer states no money.
            cost_micro_usd: None,
        });
        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });
        chat
    }

    let no_time = finished(TurnFooterSwitches {
        time: false,
        ..Default::default()
    });
    let row = last_footer(&no_time).unwrap_or_default();
    assert_eq!(row, "3.4k prompt (2.9k cached) · 250 out", "{row}");

    let no_tokens = finished(TurnFooterSwitches {
        tokens: false,
        ..Default::default()
    });
    let row = last_footer(&no_tokens).unwrap_or_default();
    assert!(shown_seconds(&row).is_some(), "{row}");
    assert!(!row.contains("prompt") && !row.contains("out"), "{row}");

    let nothing = finished(TurnFooterSwitches {
        time: false,
        tokens: false,
        cache_miss: false,
    });
    assert_eq!(last_footer(&nothing), None, "no row, not an empty one");
    assert_eq!(
        nothing.last_prompt_tokens, 3_400,
        "the totals `/usage` reads are untouched"
    );
}

/// The three keys are read once, and an unset one leaves its part on — a
/// typo in a cosmetic key must not change what the screen does.
#[test]
fn the_footer_switches_read_the_config() {
    let dir = tempfile::tempdir().expect("temp");
    std::fs::write(
        dir.path().join("config.yml"),
        "display:\n  turnFooter:\n    time: off\n    cacheMiss: false\n",
    )
    .expect("config");
    let settings =
        titi_config::settings::Settings::load(dir.path(), dir.path(), &[]).expect("load");
    let switches = footer_switches(Some(&settings));
    assert!(!switches.time, "the config turned the seconds off");
    assert!(switches.tokens, "and left its sibling on");
    assert!(!switches.cache_miss, "a false is off too");

    let switches = footer_switches(None);
    assert_eq!(
        switches,
        titi_tui::status::TurnFooterSwitches::default(),
        "unset means on"
    );
}

/// The last usage footer the transcript holds, as its text.
fn last_footer(chat: &Chat) -> Option<String> {
    chat.lines
        .iter()
        .rev()
        .find(|line| line.kind == LineKind::Usage)
        .map(|line| line.text.clone())
}

/// A finished turn shows its own time, the prompt it paid for, the share
/// the provider cached and what it answered, on one dim row under the
/// answer.
#[test]
fn a_finished_turn_shows_its_usage_under_the_answer() {
    let mut chat = chat();
    chat.turn_active = true;
    chat.turn_started = Some(Instant::now() - Duration::from_millis(1_400));
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "answer".into(),
    });
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(1),
        prompt_tokens: 3_400,
        completion_tokens: 250,
        cached_tokens: 2_900,
        // An unpriced model: the footer states no money.
        cost_micro_usd: None,
    });
    chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });

    let footer = last_footer(&chat).unwrap_or_default();
    assert!(
        footer.ends_with("3.4k prompt (2.9k cached) · 250 out"),
        "{footer}"
    );
    let seconds = shown_seconds(&footer).unwrap_or_default();
    assert!((1.4..1.5).contains(&seconds), "{footer}");

    // The row is drawn, not only held: the frame is where a user sees it,
    // and it is the last row of the turn's own block.
    let rows = frame_rows(&mut chat, 80, 24);
    let frame = rows.join("");
    assert!(
        frame.contains("3.4k prompt (2.9k cached) · 250 out"),
        "{frame}"
    );

    // …and it is dim, like every other metadata row: the theme's `Dim`
    // token is what every row of its kind carries.
    let at = rows.iter().position(|row| row.contains("3.4k prompt"));
    assert!(at.is_some(), "no footer row: {rows:?}");
    let colors = frame_colors(&mut chat, 80, 24);
    let (footer_fg, _) = colors[at.unwrap_or_default() * 80 + MARK_INDENT];
    assert_eq!(footer_fg, rgb(&chat.theme.get_color_hex(ThemeColor::Dim)));

    // The totals `/usage` reads are untouched by the footer.
    assert_eq!(chat.last_prompt_tokens, 3_400);
    assert_eq!(chat.session_completion_tokens, 250);
}

/// A turn whose request carried history and read nothing back from the
/// cache says so. The first request of a fresh session has no history to
/// re-read, so a cold cache there is a provider's norm, not a miss.
#[test]
fn a_cold_cache_over_history_is_named() {
    let mut chat = chat();
    chat.turn_active = true;
    chat.turn_started = Some(Instant::now());
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(1),
        prompt_tokens: 1_000,
        completion_tokens: 40,
        cached_tokens: 0,
        // An unpriced model: the footer states no money.
        cost_micro_usd: None,
    });
    chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });
    let first = last_footer(&chat).unwrap_or_default();
    assert!(!first.contains("cache miss"), "no history yet: {first}");
    assert!(first.ends_with("1k prompt · 40 out"), "{first}");
    assert!(!first.contains("(0 cached)"), "no zero as data: {first}");

    // The second turn carries the first: a cold cache is a miss now.
    type_text(&mut chat, "again");
    chat.on_key(Key::Enter, Instant::now());
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(2),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(2),
        prompt_tokens: 2_000,
        completion_tokens: 40,
        cached_tokens: 0,
        // An unpriced model: the footer states no money.
        cost_micro_usd: None,
    });
    chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(2),
        reason: StopReason::Stop,
    });
    let second = last_footer(&chat).unwrap_or_default();
    assert!(second.ends_with("cache miss"), "{second}");
}

/// A priced model states the turn's cost at the end of the footer row, and
/// the frame draws it: the money is part of the same dim row, not a line
/// of its own.
#[test]
fn a_priced_turn_states_its_cost_in_the_footer() {
    let mut chat = priced_chat();
    chat.turn_active = true;
    chat.turn_started = Some(Instant::now() - Duration::from_millis(1_400));
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "answer".into(),
    });
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(1),
        prompt_tokens: 1_000,
        completion_tokens: 250,
        cached_tokens: 800,
        // The engine's own figure for the turn ($0.00459, rounded to four
        // places); the footer states this, not one it computed.
        cost_micro_usd: Some(4_590),
    });
    chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });
    let footer = last_footer(&chat).unwrap_or_default();
    assert!(
        footer.ends_with("1k prompt (800 cached) · 250 out · $0.0046"),
        "{footer}"
    );

    // The row reaches the screen, money and all.
    let rows = frame_rows(&mut chat, 80, 24);
    assert!(
        rows.join("").contains("· $0.0046"),
        "the money is on the frame: {rows:?}"
    );
}

/// The figure is the engine's, not one this surface could compute: the
/// same turn on the same priced model, with the engine reporting another
/// number, prints the engine's.
#[test]
fn the_footer_states_the_engines_figure_not_its_own() {
    let mut chat = priced_chat();
    chat.turn_active = true;
    chat.turn_started = Some(Instant::now() - Duration::from_millis(1_400));
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "answer".into(),
    });
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(1),
        prompt_tokens: 1_000,
        completion_tokens: 250,
        cached_tokens: 800,
        // The catalogue price for these counts would say $0.0046.
        cost_micro_usd: Some(1_000_000),
    });
    chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });
    let footer = last_footer(&chat).unwrap_or_default();
    assert!(footer.ends_with("· $1.00"), "{footer}");

    // An unpriced report leaves no figure at all, and `/usage` states no
    // session total rather than `$0.00`, which would say the turn was
    // free. (The floor marker for a *mixed* session is
    // `usage_marks_a_total_that_leaves_turns_out`.)
    let mut unpriced = priced_chat();
    unpriced.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(1),
        prompt_tokens: 1_000,
        completion_tokens: 250,
        cached_tokens: 800,
        cost_micro_usd: None,
    });
    assert!(last_footer(&unpriced).is_none(), "no usage row is built");
    type_text(&mut unpriced, "/usage");
    unpriced.on_key(Key::Enter, Instant::now());
    let said = unpriced
        .lines
        .last()
        .map(|line| line.text.clone())
        .unwrap_or_default();
    assert!(!said.contains('$'), "{said}");
    assert!(!said.contains("session total"), "{said}");
}

/// An unpriced model's footer has no money part at all: the screen says
/// nothing rather than `$0.000`, which would read as free.
#[test]
fn an_unpriced_turn_states_no_cost() {
    let mut chat = chat();
    chat.turn_active = true;
    chat.turn_started = Some(Instant::now());
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(1),
        prompt_tokens: 1_000,
        completion_tokens: 250,
        cached_tokens: 800,
        // An unpriced model: the footer states no money.
        cost_micro_usd: None,
    });
    chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });
    let footer = last_footer(&chat).unwrap_or_default();
    assert!(
        footer.ends_with("1k prompt (800 cached) · 250 out"),
        "{footer}"
    );
    assert!(!footer.contains('$'), "no price, no figure: {footer}");
    assert!(!frame_text(&mut chat).contains("$0.000"), "no dollar zero");
}

/// A turn that reported no usage has no footer: a cancelled turn before
/// its first round has nothing to show, and the screen says nothing rather
/// than a row of zeros.
#[test]
fn a_turn_without_usage_shows_no_footer() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "answer".into(),
    });
    chat.on_event(EngineEvent::Cancelled { turn_id: TurnId(1) });
    assert!(last_footer(&chat).is_none());
    let frame = frame_text(&mut chat);
    assert!(!frame.contains("prompt"), "{frame}");
    assert!(!frame.contains("cached"), "{frame}");
    assert!(!frame.contains("cache miss"), "{frame}");
}

/// The tab title follows the run state, and the tick writes it exactly
/// once per state change — never once per tick.
#[test]
fn the_title_is_written_once_per_state_change() {
    let mut chat = chat();
    // The first tick claims the tab: it is the user's turn.
    let first = chat.terminal_tick().unwrap_or_default();
    assert!(first.starts_with("\x1b]2;titi "), "{first:?}");
    assert_eq!(first.matches('\x07').count(), 1, "{first:?}");

    // Five ticks in the same state: not one of them writes.
    let idle_writes = (0..5).filter(|_| chat.terminal_tick().is_some()).count();
    assert_eq!(idle_writes, 0, "an unchanged tick writes nothing");

    // The turn starts — before the first token — and the tab says working.
    chat.turn_active = true;
    chat.turn_started = Some(Instant::now());
    chat.phase = WorkPhase::Waiting;
    let writes = (0..5).filter(|_| chat.terminal_tick().is_some()).count();
    assert_eq!(
        writes, 1,
        "one write for the one state change, not per tick"
    );
    let working = chat.last_title.clone().unwrap_or_default();
    assert!(working.contains("titi "), "{working}");

    // Every working phase is the same title: still no second write.
    chat.phase = WorkPhase::Streaming;
    assert!(chat.terminal_tick().is_none(), "{working}");
    chat.phase = WorkPhase::Thinking;
    assert!(chat.terminal_tick().is_none());
    chat.phase = WorkPhase::Tool {
        call_id: "call-1".to_owned(),
        name: "read".to_owned(),
        detail: None,
        since: Instant::now(),
    };
    assert!(
        chat.terminal_tick().is_some(),
        "a running tool is its own state"
    );

    // The turn ends: the tab goes back to the user's turn.
    chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });
    let ended = chat.terminal_tick();
    assert!(ended.is_some(), "the turn ended");
    assert!(chat.terminal_tick().is_none());
}

/// A failed turn leaves the tab saying so, and the next turn clears it.
#[test]
fn a_failed_turn_shows_in_the_title_until_the_next_one() {
    use crate::title::TitleState;
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::Failed {
        turn_id: Some(TurnId(1)),
        reason: titi_providers::ErrorReason::Connection,
        message: "no route to host".into(),
    });
    assert_eq!(chat.title_state(), TitleState::Error);
    let failed = chat.terminal_tick().unwrap_or_default();
    assert!(failed.contains('✘'), "the error mark: {failed:?}");
    assert!(chat.terminal_tick().is_none());

    type_text(&mut chat, "again");
    chat.on_key(Key::Enter, Instant::now());
    assert_eq!(chat.title_state(), TitleState::Waiting);
    assert!(
        chat.terminal_tick().is_some(),
        "the next turn clears the tab"
    );
}

/// A finished turn raises exactly one OSC 777 — the brand as its title,
/// the session's label and one short fact as its body — and not one byte
/// on the deltas that led there.
#[test]
fn a_finished_turn_notifies_once_with_the_label_and_the_fact() {
    use titi_tui::caps::NotifyChannel;
    let mut chat = chat();
    chat.session_label = "blue-otter".to_owned();
    chat.terminal = TerminalFeatures {
        channel: NotifyChannel::Osc777,
        ..TerminalFeatures::default()
    };
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    // Streaming is not an event that notifies: the deltas write the tab
    // and the bar, never a toast.
    for text in ["Hel", "lo, world"] {
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: text.into(),
        });
        let tick = chat.terminal_tick().unwrap_or_default();
        assert!(!tick.contains("777"), "a delta notified: {tick:?}");
    }

    chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });
    let tick = chat.terminal_tick().expect("the turn's end writes");
    assert_eq!(tick.matches("\x1b]777;notify;").count(), 1, "{tick:?}");
    assert!(
        tick.contains("\x1b]777;notify;titi;blue-otter · turn finished\x07"),
        "{tick:?}"
    );
    // Once: the next tick has nothing left to say about that turn.
    let again = chat.terminal_tick().unwrap_or_default();
    assert!(!again.contains("777"), "{again:?}");
}

/// A turn that ended in a failure has its own fact, and an approval its
/// own — with the tool's name and nothing of what the tool would read.
#[test]
fn a_failed_turn_and_an_approval_each_notify_with_their_own_fact() {
    use titi_tui::caps::NotifyChannel;
    let mut failing = chat();
    failing.session_label = "blue-otter".to_owned();
    failing.terminal = TerminalFeatures {
        channel: NotifyChannel::Osc777,
        ..TerminalFeatures::default()
    };
    failing.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    failing.on_event(EngineEvent::Failed {
        turn_id: Some(TurnId(1)),
        reason: titi_providers::ErrorReason::Connection,
        message: "no route to host".into(),
    });
    let failed = failing.terminal_tick().unwrap_or_default();
    assert!(
        failed.contains("\x1b]777;notify;titi;blue-otter · turn failed\x07"),
        "{failed:?}"
    );
    assert!(!failed.contains("no route"), "the failure is not the toast");

    // An approval of the next turn: the fact names the tool, never the
    // call's arguments.
    let mut asking = chat();
    asking.session_label = "blue-otter".to_owned();
    asking.terminal = TerminalFeatures {
        channel: NotifyChannel::Osc777,
        ..TerminalFeatures::default()
    };
    asking.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    asking.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        name: "bash".into(),
        detail: Some("bash rm -rf /tmp/secret".into()),
    });
    asking.on_event(EngineEvent::ToolApprovalNeeded {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        name: "bash".into(),
    });
    let asked = asking.terminal_tick().unwrap_or_default();
    assert!(
        asked.contains("\x1b]777;notify;titi;blue-otter · needs approval: bash\x07"),
        "{asked:?}"
    );
    assert!(!asked.contains("secret"), "{asked:?}");
}

/// A terminal that does not speak OSC 777 rings instead — and the BEL is
/// exactly the difference between that terminal and one with no channel
/// at all, so nothing else creeps into the byte stream.
#[test]
fn a_terminal_without_osc_777_gets_the_bell_and_nothing_else() {
    use titi_tui::caps::{BEL, NotifyChannel};
    /// The tick a finished turn writes on a channel, as a string.
    fn finished_tick(channel: NotifyChannel) -> String {
        let mut chat = chat();
        chat.session_label = "blue-otter".to_owned();
        chat.terminal = TerminalFeatures {
            channel,
            ..TerminalFeatures::default()
        };
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "done".into(),
        });
        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });
        chat.terminal_tick().unwrap_or_default()
    }

    let bell = finished_tick(NotifyChannel::Bell);
    let silent = finished_tick(NotifyChannel::None);
    assert!(!bell.contains("777"), "{bell:?}");
    assert_eq!(bell, format!("{silent}{BEL}"), "one bell, nothing more");
}

/// The switch turns one event off and leaves the others alone; a cancel is
/// nobody's event to notify about.
#[test]
fn a_disabled_switch_emits_nothing_and_a_cancel_notifies_nobody() {
    use titi_tui::caps::NotifyChannel;
    /// The tick a finished turn writes, with the completion switch and the
    /// channel the test asks for.
    fn completion_tick(switch: bool, channel: NotifyChannel) -> String {
        let mut chat = chat();
        chat.session_label = "blue-otter".to_owned();
        chat.terminal = TerminalFeatures {
            notify_completion: switch,
            channel,
            ..TerminalFeatures::default()
        };
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });
        chat.terminal_tick().unwrap_or_default()
    }

    let off = completion_tick(false, NotifyChannel::Osc777);
    // Nothing of the notification is there — and the tick is byte for byte
    // the one a terminal with no channel at all would get, so the switch
    // removed the notification and nothing else.
    assert!(!off.contains("777"), "{off:?}");
    assert_eq!(off, completion_tick(false, NotifyChannel::None));
    assert_ne!(off, completion_tick(true, NotifyChannel::Osc777));

    // A cancel is the user's own hand on the screen: no notification.
    let mut cancelled = chat();
    cancelled.session_label = "blue-otter".to_owned();
    cancelled.terminal = TerminalFeatures {
        channel: NotifyChannel::Osc777,
        ..TerminalFeatures::default()
    };
    cancelled.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    cancelled.on_event(EngineEvent::Cancelled { turn_id: TurnId(1) });
    let tick = cancelled.terminal_tick().unwrap_or_default();
    assert!(!tick.contains("777"), "{tick:?}");
}

/// The bar is raised with the turn and cleared on every way out of it —
/// finish, failure, cancel — exactly once each, and never raised twice.
#[test]
fn the_progress_bar_is_raised_with_the_turn_and_cleared_on_every_exit() {
    use crate::title::{PROGRESS_CLEAR, PROGRESS_SET};
    #[derive(Debug, Clone, Copy)]
    enum Exit {
        Finished,
        Failed,
        Cancelled,
    }
    for exit in [Exit::Finished, Exit::Failed, Exit::Cancelled] {
        let mut chat = chat();
        chat.terminal = TerminalFeatures {
            channel: titi_tui::caps::NotifyChannel::None,
            ..TerminalFeatures::default()
        };
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        let start = chat.terminal_tick().unwrap_or_default();
        assert_eq!(
            start.matches(PROGRESS_SET).count(),
            1,
            "{exit:?}: {start:?}"
        );
        assert_eq!(start.matches(PROGRESS_CLEAR).count(), 0, "{exit:?}");
        // A second tick in the same state raises no second bar.
        let steady = chat.terminal_tick().unwrap_or_default();
        assert!(!steady.contains(PROGRESS_SET), "{exit:?}: {steady:?}");

        match exit {
            Exit::Finished => chat.on_event(EngineEvent::TurnFinished {
                turn_id: TurnId(1),
                reason: StopReason::Stop,
            }),
            Exit::Failed => chat.on_event(EngineEvent::Failed {
                turn_id: Some(TurnId(1)),
                reason: titi_providers::ErrorReason::Connection,
                message: "no route to host".into(),
            }),
            Exit::Cancelled => chat.on_event(EngineEvent::Cancelled { turn_id: TurnId(1) }),
        };
        let end = chat.terminal_tick().unwrap_or_default();
        assert_eq!(end.matches(PROGRESS_CLEAR).count(), 1, "{exit:?}: {end:?}");
        assert_eq!(end.matches(PROGRESS_SET).count(), 0, "{exit:?}: {end:?}");
        // The clear is written once: nothing keeps clearing a bar that is
        // already down.
        let after = chat.terminal_tick().unwrap_or_default();
        assert!(!after.contains(PROGRESS_CLEAR), "{exit:?}: {after:?}");
    }
}

/// With the switch off nothing is raised and nothing is cleared: the bar
/// belongs to the terminal, and a user who turned it off wants no bytes.
#[test]
fn the_progress_switch_off_writes_no_bar() {
    let mut chat = chat();
    chat.terminal = TerminalFeatures {
        progress: false,
        ..TerminalFeatures::default()
    };
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    let start = chat.terminal_tick().unwrap_or_default();
    assert!(!start.contains("\x1b]9;4"), "{start:?}");
    chat.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });
    let end = chat.terminal_tick().unwrap_or_default();
    assert!(!end.contains("\x1b]9;4"), "{end:?}");
}

/// The switches come from the config and the terminal together: the config
/// turns one off, and a terminal with no bar is quiet whatever the config
/// says.
#[test]
fn the_resolved_features_read_the_config_and_the_terminal() {
    use titi_tui::caps::{NotifyChannel, TermEnv};
    let dir = tempfile::tempdir().expect("temp");
    std::fs::write(
        dir.path().join("config.yml"),
        "notify:\n  ask: off\nterminal:\n  progress: false\n",
    )
    .expect("config");
    let settings =
        titi_config::settings::Settings::load(dir.path(), dir.path(), &[]).expect("load");
    let plain = TermEnv {
        term: Some("xterm-256color".to_owned()),
        ..TermEnv::default()
    };
    let features = TerminalFeatures::resolve(Some(&settings), &plain);
    assert!(!features.notify_ask, "the config turned it off");
    assert!(features.notify_completion, "and left its siblings on");
    assert!(!features.progress, "the config turned the bar off");
    assert!(features.token_rate);
    // An unnamed terminal gets the BEL, which it certainly understands.
    assert_eq!(features.channel, NotifyChannel::Bell);

    // Unset means on, and the terminal then decides what it can take.
    let wezterm = TermEnv {
        term_program: Some("WezTerm".to_owned()),
        term: Some("xterm-256color".to_owned()),
        ..TermEnv::default()
    };
    let features = TerminalFeatures::resolve(None, &wezterm);
    assert!(features.progress, "WezTerm has a bar");
    assert_eq!(features.channel, NotifyChannel::Osc777);
    let no_bar = TerminalFeatures::resolve(None, &plain);
    assert!(!no_bar.progress, "a terminal without a bar stays quiet");
}

/// The key hints are not what the status row replaces: whatever the row
/// says, the composer's caption is still there.
#[test]
fn the_caption_survives_every_phase() {
    let mut chat = chat();
    assert!(frame_text(&mut chat).contains("enter sends  ·  /model  ·  ctrl-c quits"));
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    assert!(frame_text(&mut chat).contains("enter steers  ·  ctrl-c stops"));
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "hello".into(),
    });
    assert!(frame_text(&mut chat).contains("enter steers  ·  ctrl-c stops"));
    chat.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        name: "bash".into(),
        detail: None,
    });
    assert!(frame_text(&mut chat).contains("enter steers  ·  ctrl-c stops"));
}

/// An idle screen is the screen from before this row existed: no row is
/// laid out at all, so not even a blank line is left above the composer.
#[test]
fn an_idle_screen_has_no_status_row() {
    let mut chat = chat();
    assert!(work_row(&chat, 80, &test_theme()).is_none());
    let rows = frame_rows(&mut chat, 80, 24);
    // The composer still starts where it did: four rows from the bottom.
    assert!(rows[20].starts_with('╭'), "{:?}", rows[20]);
    assert!(rows[23].starts_with('╰'), "{:?}", rows[23]);
    for row in &rows {
        assert!(!row.contains("streaming"), "{row:?}");
        assert!(!row.contains("thinking"), "{row:?}");
        assert!(!row.contains("waiting for the first token"), "{row:?}");
        assert!(!row.contains("chars"), "{row:?}");
    }
}

/// From the prompt to the first token the row says what it is waiting
/// for, and for how long: `working` alone cannot tell a live turn from a
/// stalled one.
#[test]
fn a_submitted_turn_waits_for_the_first_token() {
    let mut chat = chat();
    type_text(&mut chat, "read Cargo.toml");
    chat.on_key(Key::Enter, Instant::now());
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("waiting for the first token"), "{row:?}");
    assert!(!row.contains("chars"), "nothing has arrived yet: {row:?}");
    assert!(shown_seconds(&row).is_some_and(|s| s < 1.0), "{row:?}");
    assert!(
        SPINNER.iter().any(|glyph| row.contains(glyph)),
        "no spinner: {row:?}"
    );
    // The masthead keeps the state word but not the clock.
    let masthead = frame_rows(&mut chat, 80, 20)[0].clone();
    assert!(masthead.contains("working"), "{masthead:?}");
    assert!(
        shown_seconds(&masthead).is_none(),
        "the clock is the row's alone: {masthead:?}"
    );
}

/// Text arriving moves the row to streaming, and the count is the text
/// the chat received — it grows with every delta.
#[test]
fn the_first_delta_streams_and_the_count_grows() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "Hello".into(),
    });
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("streaming"), "{row:?}");
    assert!(row.contains("5 chars"), "{row:?}");

    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: ", world".into(),
    });
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("12 chars"), "{row:?}");
}

/// Reasoning is not the answer: `ThinkingDelta` gets its own word, and
/// the first answer text takes it back.
#[test]
fn reasoning_is_a_phase_of_its_own() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "anthropic/claude-sonnet-4-5".into(),
    });
    chat.on_event(EngineEvent::ThinkingDelta {
        turn_id: TurnId(1),
        text: "weighing it".into(),
    });
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("thinking"), "{row:?}");
    assert!(row.contains("11 chars"), "{row:?}");
    assert!(!row.contains("streaming"), "{row:?}");

    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "Hi".into(),
    });
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("streaming"), "{row:?}");
    assert!(!row.contains("thinking"), "{row:?}");
}

/// The generation rate stands next to the phase word, marked as the
/// estimate it is: characters the row already counts, over four.
#[test]
fn the_working_row_shows_the_rate_the_estimate_earned() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "x".repeat(400).into(),
    });
    // One sample is not a rate: the row shows the count alone.
    let start = Instant::now();
    chat.sample_rate(start);
    let row = above_composer(&mut chat, 80, 20);
    assert!(!row.contains("tok/s"), "{row:?}");
    assert!(row.contains("400 chars"), "{row:?}");

    // A second of streaming later the characters over that second are the
    // reading — 400 more over four characters a token: ~100 tok/s.
    chat.reply.push_str(&"y".repeat(400));
    chat.sample_rate(start + Duration::from_secs(1));
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("~100 tok/s"), "{row:?}");
    assert!(row.contains("800 chars"), "{row:?}");
    assert!(
        row.contains("streaming · ~100 tok/s · 800 chars"),
        "{row:?}"
    );

    // The reading is kept between bursts: a later tick that streams
    // nothing new leaves the number standing.
    chat.sample_rate(start + Duration::from_secs(2));
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("~100 tok/s"), "{row:?}");
}

/// A new turn starts with no number — nothing has streamed yet — and the
/// switch takes the segment off the row entirely.
#[test]
fn a_new_turn_and_a_disabled_switch_show_no_rate() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "x".repeat(400).into(),
    });
    let start = Instant::now();
    chat.sample_rate(start);
    chat.reply.push_str(&"y".repeat(400));
    chat.sample_rate(start + Duration::from_secs(1));
    assert!(
        above_composer(&mut chat, 80, 20).contains("tok/s"),
        "the reading the next assertions are about"
    );

    // The next turn clears it: a number from the last turn would be a lie
    // about this one.
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(2),
        model: "openai/gpt-4.1".into(),
    });
    let row = above_composer(&mut chat, 80, 20);
    assert!(!row.contains("tok/s"), "{row:?}");
    assert!(row.contains("waiting for the first token"), "{row:?}");

    // With the switch off the segment is not painted at all, even with a
    // reading standing behind it.
    chat.terminal.token_rate = false;
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(2),
        text: "z".repeat(400).into(),
    });
    chat.sample_rate(start);
    chat.reply.push_str(&"w".repeat(400));
    chat.sample_rate(start + Duration::from_secs(1));
    assert!(chat.token_rate.reading().is_some(), "the reading is there");
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("streaming"), "{row:?}");
    assert!(!row.contains("tok/s"), "the switch is off: {row:?}");
}

/// A tool call closes the reply line: the text of the round after it is
/// a new line under the call and its result, not an addition to the text
/// the model wrote before it asked for the tool.
#[test]
fn the_round_after_a_tool_starts_a_new_reply_line() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "Running it.".into(),
    });
    chat.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(1),
        call_id: "call-1".into(),
        name: "bash".into(),
        detail: Some("bash echo hi".into()),
    });
    chat.on_event(EngineEvent::ToolFinished {
        turn_id: TurnId(1),
        call_id: "call-1".into(),
        output: "hi".into(),
        is_error: false,
        detail: None,
    });
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "It printed hi.".into(),
    });
    let shown: Vec<(LineKind, &str)> = chat
        .lines
        .iter()
        .map(|line| (line.kind, line.text.as_str()))
        .collect();
    let first = shown
        .iter()
        .position(|line| *line == (LineKind::Assistant, "Running it."))
        .expect("the first round's line");
    let tool = shown
        .iter()
        .position(|(kind, _)| *kind == LineKind::Tool)
        .expect("the tool chip");
    let second = shown
        .iter()
        .position(|line| *line == (LineKind::Assistant, "It printed hi."))
        .expect("the second round's own line");
    assert!(first < tool && tool < second, "{shown:?}");
}

/// An approval names what it approves: the command or the path the tool
/// described, on the chip, in the status row and on the composer line —
/// not only the tool's name.
#[test]
fn an_approval_shows_what_the_call_will_do() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(1),
        call_id: "call-1".into(),
        name: "bash".into(),
        detail: Some("bash rm -rf build".into()),
    });
    chat.on_event(EngineEvent::ToolApprovalNeeded {
        turn_id: TurnId(1),
        call_id: "call-1".into(),
        name: "bash".into(),
    });
    let frame = frame_rows(&mut chat, 80, 20).join("\n");
    assert!(frame.contains("▸ bash rm -rf build"), "{frame}");
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("needs you · bash rm -rf build"), "{row:?}");
    let rows = frame_rows(&mut chat, 80, 20);
    let prompt = &rows[17];
    assert!(prompt.contains("bash rm -rf build"), "{prompt:?}");
    assert!(prompt.contains("y allow"), "{prompt:?}");
}

/// A long command is cut, never the keys that answer it.
#[test]
fn a_long_approval_keeps_its_keys_on_a_narrow_screen() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    let long = format!("bash {}", "x".repeat(200));
    chat.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(1),
        call_id: "call-1".into(),
        name: "bash".into(),
        detail: Some(long.into()),
    });
    chat.on_event(EngineEvent::ToolApprovalNeeded {
        turn_id: TurnId(1),
        call_id: "call-1".into(),
        name: "bash".into(),
    });
    let rows = frame_rows(&mut chat, 60, 20);
    let prompt = &rows[17];
    assert!(prompt.contains("y allow"), "{prompt:?}");
    assert!(prompt.contains("n refuse"), "{prompt:?}");
    assert!(prompt.contains("bash xx"), "{prompt:?}");
    assert!(prompt.contains("…"), "{prompt:?}");
}

/// A tool borrows the row and gives it back when its own call finishes.
#[test]
fn a_running_tool_owns_the_row_until_its_call_finishes() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "reading".into(),
    });
    chat.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        name: "read".into(),
        detail: None,
    });
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("read ·"), "{row:?}");
    assert!(shown_seconds(&row).is_some(), "{row:?}");

    // A result for a call that is not the one on the row leaves it be:
    // two calls can never overwrite each other's state.
    chat.on_event(EngineEvent::ToolFinished {
        turn_id: TurnId(1),
        call_id: "c2".into(),
        output: "something else".into(),
        is_error: false,
        detail: None,
    });
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("read ·"), "{row:?}");

    chat.on_event(EngineEvent::ToolFinished {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        output: "fn main() {}".into(),
        is_error: false,
        detail: None,
    });
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("streaming"), "{row:?}");
    assert!(row.contains("7 chars"), "{row:?}");
    assert!(!row.contains("read ·"), "{row:?}");
}

/// While the engine holds a call for an answer, the row is the engine
/// waiting on a person and names the tool it stopped on.
#[test]
fn a_pending_approval_names_the_tool_and_hands_the_row_back() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        name: "bash".into(),
        detail: None,
    });
    chat.on_event(EngineEvent::ToolApprovalNeeded {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        name: "bash".into(),
    });
    let row = above_composer(&mut chat, 80, 20);
    assert!(row.contains("needs you"), "{row:?}");
    assert!(row.contains("bash"), "{row:?}");

    // Answering it is the composer's `y`; the row goes back to the call
    // that was interrupted, still running.
    chat.on_key(Key::Char('y'), Instant::now());
    let row = above_composer(&mut chat, 80, 20);
    assert!(!row.contains("needs you"), "{row:?}");
    assert!(row.contains("bash ·"), "{row:?}");
}

/// No phase outlives its turn, however the turn ended.
#[test]
fn no_status_row_survives_a_finished_turn() {
    let started = |chat: &mut Chat| {
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            name: "bash".into(),
            detail: None,
        });
        let row = above_composer(chat, 80, 20);
        assert!(row.contains("bash ·"), "{row:?}");
        row
    };

    let mut finished = chat();
    started(&mut finished);
    finished.on_event(EngineEvent::TurnFinished {
        turn_id: TurnId(1),
        reason: StopReason::Stop,
    });
    let row = above_composer(&mut finished, 80, 20);
    assert!(!row.contains("bash ·"), "{row:?}");
    assert!(work_row(&finished, 80, &test_theme()).is_none());

    let mut failed = chat();
    started(&mut failed);
    failed.on_event(EngineEvent::Failed {
        turn_id: Some(TurnId(1)),
        message: "no such model".into(),
        reason: titi_providers::ErrorReason::Rejected,
    });
    assert!(work_row(&failed, 80, &test_theme()).is_none());

    let mut cancelled = chat();
    started(&mut cancelled);
    cancelled.on_event(EngineEvent::Cancelled { turn_id: TurnId(1) });
    assert!(work_row(&cancelled, 80, &test_theme()).is_none());
}

/// A long tool name is cut with an ellipsis on the row, never wrapped:
/// every row is exactly as wide as the screen and the composer keeps its
/// four rows at 60, 80 and 120 columns.
#[test]
fn the_status_row_is_cut_to_fit_at_60_80_and_120_columns() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        name: "mcp__a_tool_name_that_is_far_too_long_for_any_narrow_screen_to_show_whole".into(),
        detail: None,
    });
    for (width, height) in [(60u16, 20u16), (80, 20), (120, 30)] {
        let rows = frame_rows(&mut chat, width, height);
        let at = height as usize;
        for row in &rows {
            assert_eq!(
                titi_tui::width::visible_width(row),
                width as usize,
                "{width}x{height}: {row:?}"
            );
        }
        let row = &rows[at - 5];
        assert!(row.contains("mcp__a_tool_name"), "{width}: {row:?}");
        if width == 120 {
            assert!(!row.contains('…'), "{width} fits it whole: {row:?}");
        } else {
            assert!(row.contains('…'), "{width}: {row:?}");
        }
        // Nothing of the row leaked onto the line above it, and the
        // composer's borders are still its own four rows.
        assert!(
            !rows[at - 6].contains("mcp__"),
            "{width}: {:?}",
            rows[at - 6]
        );
        assert!(rows[at - 4].starts_with('╭'), "{width}: {:?}", rows[at - 4]);
        assert!(rows[at - 1].starts_with('╰'), "{width}: {:?}", rows[at - 1]);
    }
}

/// The extra row must not break a screen too small to hold it: the layout
/// still draws, every row is exactly as wide as the screen, and nothing
/// panics with a turn running.
#[test]
fn a_tiny_screen_still_draws_while_a_turn_runs() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    chat.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        name: "bash".into(),
        detail: None,
    });
    for (width, height) in [(20u16, 6u16), (16, 6), (60, 7), (16, 3)] {
        let rows = frame_rows(&mut chat, width, height);
        assert_eq!(rows.len(), height as usize);
        for row in &rows {
            assert_eq!(
                titi_tui::width::visible_width(row),
                width as usize,
                "{width}x{height}: {row:?}"
            );
        }
    }
}

/// A snapshot a test controls: the layout must not depend on this
/// machine's working directory or git state.
fn snapshot_for(chat: &Chat, context_pct: Option<u8>) -> StatusSnapshot {
    // Everything the screen states about the session comes from the live
    // helper, so a preset test measures the real line; only this machine's
    // working directory and git state are pinned. Untracked files are part
    // of that state: they are counted from the checkout the tests run in,
    // so a scratch file anywhere in the repository would otherwise add a
    // `?n` to the line and fail a golden that is about the layout.
    StatusSnapshot {
        path: "~/proj/titi/crates/titi-cli".to_owned(),
        git_branch: Some("master".to_owned()),
        git_unstaged: 2,
        git_untracked: 0,
        context_pct,
        ..masthead_snapshot(chat)
    }
}

/// The default status line is today's line, byte for byte.
///
/// The promise the preset table makes is a compatibility one: a user who
/// sets nothing sees exactly the frame they saw before the table existed.
/// These strings were captured from the screen before the presets landed
/// (`masthead_at` over a pinned snapshot, at the three widths the layout is
/// judged at), and the last case is the context slot with a percentage in
/// it — the one thing that changes between two frames of this line.
#[test]
fn the_default_status_line_is_todays_line() {
    let mut chat = chat();
    chat.model = "glm-5.3-flash".to_owned();
    chat.session_label = "blue-otter".to_owned();
    let golden = [
        (
            60u16,
            None,
            r#" titi  ready             blue-otter > ⬢ glm-5.3-flash >     "#,
        ),
        (
            60,
            Some(42),
            r#" titi  ready             blue-otter > ⬢ glm-5.3-flash >  42%"#,
        ),
        (
            80,
            None,
            r#" titi  ready                                 blue-otter > ⬢ glm-5.3-flash >     "#,
        ),
        (
            80,
            Some(42),
            r#" titi  ready                                 blue-otter > ⬢ glm-5.3-flash >  42%"#,
        ),
        (
            120,
            None,
            r#" titi  ready > 📁 ~/proj/titi/crates/titi-cli > ⑂ master *2                          blue-otter > ⬢ glm-5.3-flash >     "#,
        ),
        (
            120,
            Some(42),
            r#" titi  ready > 📁 ~/proj/titi/crates/titi-cli > ⑂ master *2                          blue-otter > ⬢ glm-5.3-flash >  42%"#,
        ),
    ];
    for (width, percent, expected) in golden {
        let line = masthead_at(&chat, width, &snapshot_for(&chat, percent));
        assert_eq!(line, expected, "{width} at {percent:?}");
    }
}

/// A preset is a row of the table, and the row decides the segments: what
/// the masthead paints is what the row names, and `ascii` paints it in
/// printable glyphs only.
#[test]
fn the_masthead_paints_the_preset_it_is_set_to() {
    let mut chat = chat();
    chat.model = "glm-5.3-flash".to_owned();
    chat.session_label = "blue-otter".to_owned();
    let at = |chat: &mut Chat, preset: StatusLinePreset| {
        chat.status_line.preset = preset;
        let snapshot = snapshot_for(chat, Some(42));
        masthead_at(chat, 120, &snapshot)
    };
    let default = at(&mut chat, StatusLinePreset::Default);
    for expected in ["titi", "ready", "blue-otter", "glm-5.3-flash", "~/proj"] {
        assert!(
            default.contains(expected),
            "{expected:?} missing: {default:?}"
        );
    }
    let minimal = at(&mut chat, StatusLinePreset::Minimal);
    assert!(minimal.contains("glm-5.3-flash"), "{minimal:?}");
    assert!(!minimal.contains("titi"), "{minimal:?}");
    assert!(!minimal.contains("blue-otter"), "{minimal:?}");
    let compact = at(&mut chat, StatusLinePreset::Compact);
    assert!(
        compact.contains("glm-5.3-flash") && compact.contains("~/proj"),
        "{compact:?}"
    );
    assert!(!compact.contains("blue-otter"), "{compact:?}");
    let full = at(&mut chat, StatusLinePreset::Full);
    assert!(full.contains("blue-otter"), "{full:?}");
    let ascii = at(&mut chat, StatusLinePreset::Ascii);
    assert!(ascii.is_ascii(), "{ascii:?}");
    assert!(ascii.contains("[D]") || ascii.contains("[M]"), "{ascii:?}");
    // Every preset fills the pane it is given.
    for preset in [
        StatusLinePreset::Default,
        StatusLinePreset::Minimal,
        StatusLinePreset::Compact,
        StatusLinePreset::Full,
        StatusLinePreset::Ascii,
    ] {
        assert_eq!(
            titi_tui::width::visible_width(&at(&mut chat, preset)),
            120,
            "{preset:?}"
        );
    }
}

/// Bare `/statusline` states the preset in force and lists every one, the
/// way `/help` lists the commands.
#[test]
fn bare_statusline_states_the_current_preset_and_lists_them() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.slash("/statusline").expect("the command parses");
    let text: Vec<String> = chat.lines.iter().map(|line| line.text.clone()).collect();
    assert!(
        text.iter()
            .any(|line| line == "status line: default · context off"),
        "{text:?}"
    );
    for id in StatusLinePreset::IDS {
        assert!(
            text.iter()
                .any(|line| line.starts_with(&format!("/{id}  "))),
            "{id} is not listed: {text:?}"
        );
    }
}

/// A name this build does not carry is refused with the list, and nothing
/// is written: a typo must not become the remembered preset.
#[test]
fn an_unknown_preset_is_refused_with_the_list_and_writes_nothing() {
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    let applied = chat.slash("/statusline nope").expect("the command parses");
    assert!(applied.effect.is_none(), "nothing is dispatched");
    let refusal = chat.lines.last().expect("a line").text.clone();
    assert!(refusal.contains("nope"), "{refusal:?}");
    for id in StatusLinePreset::IDS {
        assert!(refusal.contains(id), "{id} is not in {refusal:?}");
    }
    assert_eq!(chat.status_line.preset, StatusLinePreset::Default);
    let settings =
        titi_config::settings::Settings::load(dir.path(), dir.path(), &[]).expect("settings");
    assert_eq!(
        settings.get(titi_config::settings::STATUS_LINE_PRESET_KEY),
        None,
        "a refused name writes no key"
    );
}

/// A preset that is set is remembered and painted: the write lands on the
/// canonical file, and the next frame is the new line.
#[test]
fn a_preset_is_remembered_and_the_next_frame_uses_it() {
    let (dir, mut chat) = picker_chat("glm-5.3-flash", "session-123");
    chat.session_label = "blue-otter".to_owned();
    let before = masthead_at(&chat, 120, &snapshot_for(&chat, Some(42)));
    assert!(
        before.contains("titi"),
        "the default opens with the mark: {before:?}"
    );
    chat.slash("/statusline minimal")
        .expect("the command parses");
    assert_eq!(chat.status_line.preset, StatusLinePreset::Minimal);
    let after = masthead_at(&chat, 120, &snapshot_for(&chat, Some(42)));
    assert!(!after.contains("titi"), "the mark is gone: {after:?}");
    assert!(
        !after.contains("blue-otter"),
        "and so is the name: {after:?}"
    );
    assert!(after.contains("glm-5.3-flash"), "{after:?}");
    let settings =
        titi_config::settings::Settings::load(dir.path(), dir.path(), &[]).expect("settings");
    assert_eq!(
        settings
            .get(titi_config::settings::STATUS_LINE_PRESET_KEY)
            .and_then(|value| value.as_str().map(str::to_owned)),
        Some("minimal".to_owned())
    );
    // What the next run resolves from that file is the same preset.
    let stored = settings
        .get(titi_config::settings::STATUS_LINE_PRESET_KEY)
        .and_then(|value| value.as_str().map(str::to_owned));
    assert_eq!(
        StatusLineStyle::resolve(stored.as_deref(), None, None, false, false, false).preset,
        StatusLinePreset::Minimal
    );
}

/// Maths is markdown: an answer whose only markup is a formula takes the
/// renderer's path, so the `$…$` reaches the screen as the formula rather
/// than as its own TeX — while a price that only looks like maths keeps the
/// plain block it has always had.
#[test]
fn a_maths_only_answer_takes_the_markdown_path() {
    let rows = reply_rows_of(r"Binary search is $O(\log n)$.", 80);
    let texts = row_texts(&rows);
    assert!(
        texts.iter().any(|row| row.contains("O(log n)")),
        "the formula is rendered: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .all(|row| !row.contains(r"\log") && !row.contains('$')),
        "no TeX and no marker reached the screen: {texts:?}"
    );
    // A price is not maths, so the answer stays the plain speech block.
    let plain = row_texts(&reply_rows_of("It costs $5 and $6.", 80));
    assert!(
        plain.iter().any(|row| row.contains("$5 and $6")),
        "{plain:?}"
    );
}

/// The masthead's line, as the text a terminal would show.
fn masthead_at(chat: &Chat, width: u16, snapshot: &StatusSnapshot) -> String {
    masthead_spans(chat, width, &test_theme(), snapshot)
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// The column `needle` starts in — a cell count, so a wide glyph before it
/// does not count as one.
fn column_of(line: &str, needle: &str) -> usize {
    let at = line
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} is not on the masthead: {line:?}"));
    titi_tui::width::visible_width(&line[..at])
}

/// The model sits in the same column whether or not a percentage has been
/// reported: the context slot is drawn either way. The plan's §1.8 frames
/// are the defect — the whole right block slid three cells left the moment
/// the first `3%` appeared.
#[test]
fn the_context_percent_does_not_move_the_right_block() {
    let mut chat = chat();
    chat.model = "openai-codex/gpt-5.5".to_owned();
    chat.session_label = "blue-otter".to_owned();

    let without = masthead_at(&chat, 80, &snapshot_for(&chat, None));
    let with = masthead_at(&chat, 80, &snapshot_for(&chat, Some(3)));
    assert_eq!(
        column_of(&without, "gpt-5.5"),
        column_of(&with, "gpt-5.5"),
        "the model moved:\n{without}\n{with}"
    );
    assert_eq!(
        column_of(&without, "blue-otter"),
        column_of(&with, "blue-otter"),
        "the name moved:\n{without}\n{with}"
    );
    // The slot is drawn either way: the tail after the last separator has
    // the same width in both lines — that is what the two column
    // assertions above are measuring through — and holds nothing until a
    // report arrives, then the digits in the same place.
    let after_separator = |line: &str| {
        let at = line.rfind('>').expect("the separator before the slot") + 1;
        line[at..].to_owned()
    };
    assert_eq!(
        titi_tui::width::visible_width(&after_separator(&without)),
        titi_tui::width::visible_width(&after_separator(&with)),
        "the context slot moved:\n{without}\n{with}"
    );
    assert!(
        after_separator(&without).trim().is_empty(),
        "the slot is blank before a report: {without:?}"
    );
    assert_eq!(after_separator(&with).trim(), "3%", "{with:?}");
    for line in [&without, &with] {
        assert_eq!(titi_tui::width::visible_width(line), 80, "{line:?}");
    }
}

/// What the masthead gives up when the pane narrows, in the order it
/// promises: the git state, the working directory, the session's name, the
/// mode, the loop count, and then the model's provider prefix — and the
/// model is never cut in the middle of a token while its short form fits.
#[test]
fn a_narrow_masthead_gives_up_in_the_documented_order() {
    let mut chat = chat();
    chat.model = "openai-codex/gpt-daybreak-blue-latest-wm".to_owned();
    chat.session_label = "a-very-long-session-name".to_owned();
    chat.mode = SessionMode::Plan;
    chat.jobs = vec![JobInfo {
        id: "job-1".into(),
        prompt: "watch CI".into(),
        interval_secs: 60,
        runs: 0,
    }];
    // Wide enough for everything but this machine's git state, since the
    // snapshot is the test's own.
    let wide = masthead_at(&chat, 160, &snapshot_for(&chat, Some(42)));
    for segment in [
        "titi  ready",
        "plan",
        "1 loop(s)",
        "master",
        "a-very-long-sessi",
        "openai-codex",
        "42%",
    ] {
        assert!(wide.contains(segment), "{segment:?} is missing: {wide}");
    }

    // Each step of the order, one at a time.
    let steps = |width: u16| masthead_at(&chat, width, &snapshot_for(&chat, Some(42)));
    let git_gone = steps(145);
    assert!(!git_gone.contains("master"), "{git_gone}");
    assert!(git_gone.contains("titi-cli"), "the path stays: {git_gone}");
    let path_gone = steps(125);
    assert!(!path_gone.contains("titi-cli"), "{path_gone}");
    assert!(
        path_gone.contains("a-very-long-sessi"),
        "the name stays: {path_gone}"
    );
    let name_gone = steps(95);
    assert!(!name_gone.contains("a-very-long-sessi"), "{name_gone}");
    assert!(name_gone.contains("plan"), "the mode stays: {name_gone}");
    let mode_gone = steps(80);
    assert!(!mode_gone.contains("plan"), "{mode_gone}");
    assert!(mode_gone.contains("loop(s)"), "the loops stay: {mode_gone}");
    let loops_gone = steps(70);
    assert!(!loops_gone.contains("loop(s)"), "{loops_gone}");
    assert!(
        loops_gone.contains("openai-codex/gpt-daybreak-blue-latest-wm"),
        "the model is still whole: {loops_gone}"
    );

    // The provider prefix is the next to go, and the short form is whole.
    let short = steps(55);
    assert!(!short.contains("openai-codex"), "{short}");
    assert!(short.contains("gpt-daybreak-blue-latest-wm"), "{short}");
    assert!(!short.contains('…'), "nothing is cut yet: {short}");

    // Last resort: the short form itself is cut, visibly.
    let cut = steps(40);
    assert!(cut.contains('…'), "{cut}");
    assert!(!cut.contains("gpt-daybreak-blue-latest-wm"), "{cut}");
    assert!(cut.contains("titi  ready"), "the state word stays: {cut}");
}

/// The masthead never runs past the pane, at any width a terminal can have.
#[test]
fn the_masthead_never_exceeds_the_pane() {
    let mut chat = chat();
    chat.model = "openai-codex/gpt-daybreak-blue-latest-wm".to_owned();
    chat.session_label = "a-very-long-session-name".to_owned();
    chat.mode = SessionMode::Plan;
    for width in 16..=200u16 {
        for percent in [None, Some(100)] {
            let line = masthead_at(&chat, width, &snapshot_for(&chat, percent));
            assert!(
                titi_tui::width::visible_width(&line) <= width as usize,
                "{width}: {} wide: {line:?}",
                titi_tui::width::visible_width(&line)
            );
        }
    }
}

/// The frame's own masthead: the segments stand in the documented order,
/// and the model is not cut at a width that has room for it.
#[test]
fn the_frame_masthead_stands_in_order() {
    let mut chat = chat();
    chat.model = "openai-codex/gpt-5.5".to_owned();
    chat.session_label = "blue-otter".to_owned();
    chat.mode = SessionMode::Plan;
    chat.context_percent = Some(7);
    let row = frame_rows(&mut chat, 100, 20)[0].clone();

    let mut last = 0usize;
    for segment in ["titi", "ready", "plan", "blue-otter", "gpt-5.5", "7%"] {
        let at = column_of(&row, segment);
        assert!(
            at >= last,
            "{segment:?} is out of order in {row:?} (at {at}, after {last})"
        );
        last = at;
    }
    assert!(!row.contains('…'), "nothing is cut at 100 columns: {row:?}");
}

/// A row of the status line, as the text a terminal would show.
fn work_row_text(glyph: &str, fact: &WorkFact, color: ThemeColor, width: u16) -> String {
    row_texts(&[work_line(glyph, fact, color, width, &test_theme())]).remove(0)
}

/// The colour a row paints itself with.
fn work_row_color(glyph: &str, fact: &WorkFact, color: ThemeColor, width: u16) -> Option<Color> {
    work_line(glyph, fact, color, width, &test_theme())
        .spans
        .first()
        .and_then(|span| span.style.fg)
}

/// Each state of the row is told apart by its token, not only by its glyph:
/// activity in the accent, a running tool in the theme's own tool-title
/// token, and an approval in the warning one.
#[test]
fn each_state_of_the_row_has_its_own_token() {
    let theme = test_theme();
    let fact = |wording: &str| WorkFact {
        wording: wording.to_owned(),
        argument: None,
        compact: wording.to_owned(),
        seconds: Some("0.1s".to_owned()),
    };
    let activity = work_row_color("*", &fact("streaming"), ThemeColor::Accent, 80);
    let tool = work_row_color(
        "\u{2699}",
        &tool_fact("read", Some("read docs/README.md"), "0.1s"),
        ThemeColor::ToolOutput,
        80,
    );
    let approval = work_row_color("\u{26a0}", &fact("needs you"), ThemeColor::Warning, 80);
    assert_eq!(activity, fg(&theme, ThemeColor::Accent).fg);
    assert_eq!(tool, fg(&theme, ThemeColor::ToolOutput).fg);
    assert_eq!(approval, fg(&theme, ThemeColor::Warning).fg);
    // The three are visibly different states, not three words in one
    // colour: this is the defect the plan's §3.5 names.
    for (left, right) in [(activity, tool), (tool, approval), (activity, approval)] {
        assert_ne!(left, right, "two states share a colour");
    }
}

/// The frames of the four live states, with the token each is painted with:
/// this is the whole claim of the step, read off the screen rather than off
/// a helper.
///
/// The needle is the row's stable half. Two things in a live row move on
/// their own: the spinner turns every 50 ms and the seconds are the wall
/// clock, so a needle built from one drawn row (`⠋ … · 0.0s`) misses the
/// next draw as soon as the two straddle a tick — which is how this test
/// flaked on a loaded runner. The label and the token are the state; the
/// clock is checked by shape, through the same [`shown_seconds`] the other
/// row tests use.
#[test]
fn the_frame_paints_each_state_with_its_own_token() {
    let theme = test_theme();
    /// Drives a chat into one state of the row.
    type Drive = fn(&mut Chat);
    // `(label, drive, the row's stable text, its token, whether it runs a clock)`.
    let states: [(&str, Drive, &str, ThemeColor, bool); 5] = [
        (
            "waiting",
            |chat| {
                chat.on_event(EngineEvent::TurnStarted {
                    turn_id: TurnId(1),
                    model: "openai/gpt-4.1".into(),
                });
            },
            "waiting for the first token",
            ThemeColor::Accent,
            true,
        ),
        (
            "streaming",
            |chat| {
                chat.on_event(EngineEvent::TurnStarted {
                    turn_id: TurnId(1),
                    model: "openai/gpt-4.1".into(),
                });
                chat.on_event(EngineEvent::StreamDelta {
                    turn_id: TurnId(1),
                    text: "hello".into(),
                });
            },
            "streaming · 5 chars",
            ThemeColor::Accent,
            true,
        ),
        (
            "thinking",
            |chat| {
                chat.on_event(EngineEvent::TurnStarted {
                    turn_id: TurnId(1),
                    model: "openai/gpt-4.1".into(),
                });
                chat.on_event(EngineEvent::ThinkingDelta {
                    turn_id: TurnId(1),
                    text: "weighing it".into(),
                });
            },
            "thinking · 11 chars",
            ThemeColor::Accent,
            true,
        ),
        (
            "tool",
            |chat| {
                chat.on_event(EngineEvent::TurnStarted {
                    turn_id: TurnId(1),
                    model: "openai/gpt-4.1".into(),
                });
                chat.on_event(EngineEvent::ToolStarted {
                    turn_id: TurnId(1),
                    call_id: "c1".into(),
                    name: "read".into(),
                    detail: Some("read docs/README.md".into()),
                });
            },
            "read docs/README.md",
            ThemeColor::ToolOutput,
            true,
        ),
        (
            "needs you",
            |chat| {
                chat.on_event(EngineEvent::ToolStarted {
                    turn_id: TurnId(1),
                    call_id: "c1".into(),
                    name: "write".into(),
                    detail: Some("write notes/probe.txt".into()),
                });
                chat.on_event(EngineEvent::ToolApprovalNeeded {
                    turn_id: TurnId(1),
                    call_id: "c1".into(),
                    name: "write".into(),
                });
            },
            "needs you · write",
            ThemeColor::Warning,
            false,
        ),
    ];
    for (label, drive, needle, token, clocked) in states {
        let mut chat = chat();
        drive(&mut chat);
        // The work row is the line above the composer box, and only that
        // line is searched: a tool's own chip in the transcript carries the
        // same words (`tool read docs/README.md`), and this test is about
        // the row, not the chip.
        const ROWS: u16 = 20;
        const ROW: usize = (ROWS - 5) as usize;
        let row = above_composer(&mut chat, 80, ROWS);
        let buffer = frame_buffer(&mut chat, 80, ROWS);
        let symbols: Vec<String> = (0..ROWS)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let (x, _) = cell_of(&symbols[ROW..=ROW], needle)
            .unwrap_or_else(|| panic!("{label}: the row is not on screen: {row:?}"));
        assert_eq!(
            buffer[(x, ROW as u16)].fg,
            fg(&theme, token).fg.unwrap_or(Color::Reset),
            "{label} is not painted in {token:?}: {row:?}"
        );
        // The clock is the row's shape and never its value: the value is
        // the wall clock, and pinning it is what this test used to do.
        if clocked {
            assert!(
                shown_seconds(&row).is_some(),
                "{label}: the row carries no running clock: {row:?}"
            );
        } else {
            assert!(
                shown_seconds(&row).is_none(),
                "{label}: a row with no clock printed one: {row:?}"
            );
        }
    }
}

/// A tool's row carries what the tool said it was doing, and falls back to
/// its name alone when it had nothing to say.
#[test]
fn a_tool_row_carries_the_call_or_the_name_alone() {
    let described = tool_fact("read", Some("read docs/README.md"), "0.4s");
    assert_eq!(
        work_row_text("\u{2699}", &described, ThemeColor::ToolOutput, 80),
        " \u{2699} read docs/README.md · 0.4s"
    );
    let bare = tool_fact("read", None, "0.4s");
    assert_eq!(
        work_row_text("\u{2699}", &bare, ThemeColor::ToolOutput, 80),
        " \u{2699} read · 0.4s"
    );
    // A description that is only a word has no argument to show.
    let wordy = tool_fact("git", Some("git"), "0.2s");
    assert_eq!(
        work_row_text("\u{2699}", &wordy, ThemeColor::ToolOutput, 80),
        " \u{2699} git · 0.2s"
    );
}

/// The row shortens the fact instead of cutting it, and the seconds are the
/// last thing to go — the moving clock is what says the screen is alive.
#[test]
fn a_narrow_work_row_shortens_the_fact_and_keeps_the_seconds() {
    let tool = tool_fact("read", Some("read docs/README.md"), "0.4s");
    let row = |width: u16| work_row_text("\u{2699}", &tool, ThemeColor::ToolOutput, width);
    assert_eq!(
        row(80),
        " \u{2699} read docs/README.md · 0.4s",
        "the whole fact"
    );
    assert_eq!(
        row(25),
        " \u{2699} read docs/… · 0.4s",
        "the argument's head"
    );
    assert_eq!(row(19), " \u{2699} read · 0.4s", "the argument goes");

    let waiting = WorkFact {
        wording: "waiting for the first token".to_owned(),
        argument: None,
        compact: "waiting".to_owned(),
        seconds: Some("0.1s".to_owned()),
    };
    let waiting_row = |width: u16| work_row_text("*", &waiting, ThemeColor::Accent, width);
    assert_eq!(
        waiting_row(40),
        " * waiting for the first token · 0.1s",
        "the long wording while it fits"
    );
    assert_eq!(waiting_row(30), " * waiting · 0.1s", "the compact wording");
    let cut = waiting_row(12);
    assert!(cut.ends_with("· 0.1s"), "the seconds stay whole: {cut:?}");
    assert!(cut.contains('…'), "the wording is what was cut: {cut:?}");

    // And never wider than the pane, at any width.
    for width in 8..=120u16 {
        for fact in [&tool, &waiting] {
            let text = work_row_text("\u{2699}", fact, ThemeColor::ToolOutput, width);
            assert!(
                titi_tui::width::visible_width(&text) <= width as usize,
                "{width}: {text:?}"
            );
        }
    }
}

/// The column a row's body starts in: where its first word begins, with the
/// gutter measured as the cells before it.
fn body_column(row: &str, first_word: &str) -> usize {
    let at = row
        .find(first_word)
        .unwrap_or_else(|| panic!("{first_word:?} is not in {row:?}"));
    titi_tui::width::visible_width(&row[..at])
}

/// The transcript's geometry is one rule: the gutters measure what the
/// indents say, and every block kind starts its body in its kind's column —
/// on its first row and on every row it wraps to, the markdown answer and
/// the diff included.
#[test]
fn the_geometry_is_one_rule() {
    let (air, tag, bar, hang) = message_gutter("you");
    assert_eq!(
        titi_tui::width::visible_width(&format!("{air}{tag}{bar}")),
        MESSAGE_INDENT,
        "the message gutter's pieces do not add up"
    );
    assert_eq!(
        titi_tui::width::visible_width(&hang),
        MESSAGE_INDENT,
        "a wrapped message row does not hang under its body"
    );
    let (air, mark, after, hang) = mark_gutter("\u{25b8}");
    assert_eq!(
        titi_tui::width::visible_width(&format!("{air}{mark}{after}")),
        MARK_INDENT,
        "the mark gutter's pieces do not add up"
    );
    assert_eq!(
        titi_tui::width::visible_width(&hang),
        MARK_INDENT,
        "a wrapped mark row does not hang under its body"
    );

    let theme = test_theme();
    let long = "word ".repeat(30);
    let wrapped = |kind: LineKind, text: String, indent: usize| {
        let rows = row_texts(&message_rows(&TranscriptLine { kind, text }, 60, &theme, false).0);
        assert!(rows.len() >= 2, "{kind:?} did not wrap: {rows:?}");
        for (at, row) in rows.iter().enumerate() {
            assert_eq!(
                body_column(row, "word"),
                indent,
                "{kind:?} row {at} starts in the wrong column: {row:?}"
            );
        }
    };
    for kind in [
        LineKind::User,
        LineKind::Tool,
        LineKind::Error,
        LineKind::Note,
    ] {
        let text = match kind {
            LineKind::Tool => format!("tool done  {long}"),
            _ => long.clone(),
        };
        let indent = if kind == LineKind::User {
            MESSAGE_INDENT
        } else {
            MARK_INDENT
        };
        wrapped(kind, text, indent);
    }

    // The plain message block and the markdown one are the same block: an
    // answer cannot re-wrap because the renderer took it.
    for rows in [
        speech(
            "titi",
            ThemeColor::Accent,
            ThemeColor::Text,
            Surface::Page,
            &long,
            60,
            &theme,
        ),
        message_rows(
            &TranscriptLine {
                kind: LineKind::Assistant,
                text: format!("## {long}"),
            },
            60,
            &theme,
            false,
        )
        .0,
    ] {
        let rows = row_texts(&rows);
        assert!(rows.len() >= 2, "{rows:?}");
        for (at, row) in rows.iter().enumerate() {
            assert_eq!(
                body_column(row, "word"),
                MESSAGE_INDENT,
                "an answer row starts in the wrong column ({at}): {row:?}"
            );
        }
    }

    // A diff's rows sit at the mark's inset, and its chip's mark at the
    // column a chip's own mark has.
    let rows = row_texts(
        &message_rows(
            &TranscriptLine {
                kind: LineKind::Diff,
                text: edit_result().to_owned(),
            },
            60,
            &theme,
            false,
        )
        .0,
    );
    assert!(rows.len() >= 2, "{rows:?}");
    let mark_column = MARK_INDENT - 2;
    assert_eq!(
        body_column(&rows[0], "\u{2713}"),
        mark_column,
        "the diff's chip is not a chip: {:?}",
        rows[0]
    );
    for row in &rows[1..] {
        assert!(
            row.starts_with(&" ".repeat(MARK_INDENT)),
            "a diff row is not at the mark's inset: {row:?}"
        );
    }
}

/// A detail that is not a diff — the todo checklist — keeps its rows: a
/// list flattened onto one line reads as a sentence, not a checklist.
#[test]
fn a_checklist_detail_keeps_one_row_per_item() {
    let mut chat = chat();
    chat.push(LineKind::Tool, "todo 1/3 · Fix the parser".to_owned());
    chat.push(
        LineKind::Diff,
        "[x] 1. Read the failing test\n[>] 2. Fix the parser\n[ ] 3. Run the suite".to_owned(),
    );
    let rows = frame_rows(&mut chat, 80, 12);
    for item in [
        "✓ [x] 1. Read the failing test",
        "[>] 2. Fix the parser",
        "[ ] 3. Run the suite",
    ] {
        let row = rows
            .iter()
            .find(|row| row.contains(item))
            .unwrap_or_else(|| panic!("no row for {item:?}: {rows:#?}"));
        assert_eq!(
            row.trim(),
            item,
            "an item shares its row with another: {rows:#?}"
        );
    }
    // The rows after the first hang under its text, not under the mark.
    let column = |item: &str| {
        rows.iter().find_map(|row| {
            row.find(item)
                .map(|byte| titi_tui::width::visible_width(&row[..byte]))
        })
    };
    assert_eq!(column("[x] 1."), column("[>] 2."), "{rows:#?}");
}

/// Air lands where the writer changes, and nowhere else: not between a
/// turn's own text and its tool chips, not between a chip and the diff under
/// it, not between a note and the answer it belongs to.
#[test]
fn air_lands_on_a_role_change_and_nowhere_else() {
    let mut chat = chat();
    chat.push(LineKind::User, "what is in the repo?".to_owned());
    chat.push(LineKind::Assistant, "## Files".to_owned());
    chat.push(LineKind::Tool, "tool done  read".to_owned());
    chat.push(LineKind::Diff, edit_result().to_owned());
    chat.push(LineKind::Note, "a note".to_owned());
    chat.push(LineKind::User, "and the tests?".to_owned());
    chat.push(LineKind::Assistant, "All green.".to_owned());

    let rows = frame_rows(&mut chat, 80, 30);
    // Every blank row with content above and below it that is not the
    // composer's frame, as what it sits between.
    let gaps: Vec<(String, String)> = rows
        .windows(3)
        .filter(|triple| {
            triple[1].trim().is_empty()
                && !triple[0].trim().is_empty()
                && !triple[2].trim().is_empty()
                && !triple[2].starts_with('╭')
        })
        .map(|triple| (triple[0].trim().to_owned(), triple[2].trim().to_owned()))
        .collect();
    assert_eq!(gaps.len(), 3, "air in the wrong places: {gaps:?}");
    assert!(
        gaps.iter()
            .any(|(above, below)| above.contains("what is in the repo?")
                && below.contains("Files")),
        "no air between the question and the answer: {gaps:?}"
    );
    assert!(
        gaps.iter()
            .any(|(above, below)| above.contains("a note") && below.contains("and the tests?")),
        "no air between the turn and the next question: {gaps:?}"
    );
    assert!(
        gaps.iter()
            .any(|(above, below)| above.contains("and the tests?") && below.contains("All green")),
        "no air between the question and the answer: {gaps:?}"
    );
    // And none inside the turn: the reply, its chip, its diff and its note
    // are one body.
    for (above, below) in &gaps {
        for joined in [("Files", "read"), ("read", "@@"), ("@@", "a note")] {
            assert!(
                !(above.contains(joined.0) && below.contains(joined.1)),
                "air inside one turn's body between {joined:?}: {gaps:?}"
            );
        }
    }
}

/// The user's own question is the one block on a surface of its own: the
/// theme's `userMessageBg` across exactly its rows, `userMessageText` on the
/// body, and the label's colour unchanged.
///
/// Run against the default preset and against `dark`: the palette titi
/// starts on by itself has to show the band, which `titanium` did not until
/// its `userMessageBg` was given a surface of its own.
#[test]
fn the_user_block_carries_its_own_surface() {
    for name in ["titanium", "dark"] {
        assert_user_block_surface(name);
    }
}

/// The band's extent and colours on one palette.
fn assert_user_block_surface(name: &str) {
    let theme = test_theme_named(name);
    let band = bg(&theme, ThemeBg::UserMessageBg);
    let page_bg = bg(&theme, ThemeBg::StatusLineBg);
    assert_ne!(
        band, page_bg,
        "{name}: this test needs a palette where the band shows"
    );

    let mut chat = chat_with_theme(theme.clone());
    chat.push(LineKind::User, "word ".repeat(20));
    chat.push(LineKind::Assistant, "All green.".to_owned());

    let buffer = frame_buffer(&mut chat, 80, 30);
    let rows: Vec<String> = (0..30)
        .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    let user_rows: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains("word word"))
        .map(|(at, _)| at)
        .collect();
    assert!(user_rows.len() >= 2, "the question wrapped: {rows:?}");

    // The band covers every row of the block, to the layout's width.
    let layout = 78;
    for at in &user_rows {
        for x in [0, 40, layout - 1] {
            assert_eq!(
                buffer[(x as u16, *at as u16)].bg,
                band,
                "row {at} column {x} is not on the band"
            );
        }
        assert_ne!(
            buffer[(79, *at as u16)].bg,
            band,
            "the band ran to the pane's last column"
        );
    }
    // The body and the label keep their own colours, on the band.
    let (x, y) = cell_of(&rows, "word").expect("the question is on screen");
    assert_eq!(
        buffer[(x, y)].fg,
        fg(&theme, ThemeColor::UserMessageText)
            .fg
            .unwrap_or(Color::Reset)
    );
    assert_eq!(buffer[(x, y)].bg, band);
    let (x, y) = cell_of(&rows, "you").expect("the label is on screen");
    assert_eq!(
        buffer[(x, y)].fg,
        fg(&theme, ThemeColor::CustomMessageLabel)
            .fg
            .unwrap_or(Color::Reset),
        "the label's colour changed"
    );
    assert_eq!(buffer[(x, y)].bg, band, "the label is not on the band");

    // The rows around it are not banded: the air, and the answer.
    let after = user_rows.last().expect("a row") + 1;
    assert!(
        rows[after].trim().is_empty(),
        "no air after the block: {rows:?}"
    );
    assert_eq!(
        buffer[(0, after as u16)].bg,
        page_bg,
        "the air is on the band"
    );
    let answer = rows
        .iter()
        .position(|row| row.contains("All green"))
        .expect("the answer is on screen");
    assert_eq!(
        buffer[(0, answer as u16)].bg,
        page_bg,
        "the answer is on the band"
    );
}

/// The band is exactly as wide as the block's own column range at any pane
/// width, and nothing runs past the pane.
#[test]
fn the_band_covers_the_blocks_own_width() {
    for name in ["titanium", "dark"] {
        assert_band_width(&test_theme_named(name), name);
    }
}

/// The band's width at each pane size, on one palette.
fn assert_band_width(theme: &Arc<Theme>, name: &str) {
    let band = bg(theme, ThemeBg::UserMessageBg);
    for width in [60u16, 80, 120] {
        let mut chat = chat_with_theme(Arc::clone(theme));
        chat.push(LineKind::User, "word ".repeat(30));
        let buffer = frame_buffer(&mut chat, width, 30);
        let mut banded = 0;
        for y in 0..30 {
            if buffer[(0, y)].bg == band {
                banded += 1;
                assert_ne!(
                    buffer[(width - 1, y)].bg,
                    band,
                    "{name} at {width}: the band reached the pane's edge"
                );
                assert_eq!(
                    buffer[(width - 3, y)].bg,
                    band,
                    "{name} at {width}: the band stopped short of the layout's width"
                );
            }
        }
        assert!(
            banded >= 2,
            "{width}: the block did not wrap: {banded} rows"
        );
    }
}

/// A provider that refused the key contributes no models and looks
/// exactly like a provider that has none. `/model` has to say which.
#[test]
fn slash_model_shows_why_a_provider_listed_nothing() {
    let mut chat = chat();
    chat.catalog = crate::engine::ModelCatalog::fixed_with_failures(
        vec!["openai/gpt-4.1".to_owned()],
        vec![titi_providers::DiscoveryError::Unauthorized {
            provider: "opencode-go".into(),
            status: 401,
        }],
    );
    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());

    let reason = chat
        .lines
        .iter()
        .find(|line| line.text.contains("opencode-go"))
        .unwrap_or_else(|| panic!("no reason in the transcript: {:?}", chat.lines));
    assert_eq!(reason.kind, LineKind::Error);
    assert!(reason.text.contains("401"), "{}", reason.text);
    assert!(
        reason.text.contains("titi --set-key"),
        "the user is not told what to do: {}",
        reason.text
    );
}

/// An empty list with a reason behind it must not read as "no models"
/// alone: that is the case the reason exists for.
#[test]
fn slash_model_with_nothing_left_still_names_the_refusal() {
    let mut chat = chat();
    chat.catalog = crate::engine::ModelCatalog::fixed_with_failures(
        Vec::new(),
        vec![titi_providers::DiscoveryError::Forbidden {
            provider: "openai".into(),
            status: 403,
        }],
    );
    type_text(&mut chat, "/model");
    chat.on_key(Key::Enter, Instant::now());

    let texts: Vec<&str> = chat.lines.iter().map(|line| line.text.as_str()).collect();
    assert!(
        texts.iter().any(|text| text.contains("openai")),
        "{texts:?}"
    );
    assert!(texts.iter().any(|text| *text == "no models"), "{texts:?}");
}

#[test]
fn slash_pause_blocks_a_prompt_until_resumed() {
    let mut chat = chat();
    type_text(&mut chat, "/pause");
    let paused = chat.on_key(Key::Enter, Instant::now());
    assert!(paused.effect.is_none());
    assert!(chat.paused);
    type_text(&mut chat, "hi");
    let blocked = chat.on_key(Key::Enter, Instant::now());
    assert!(blocked.effect.is_none());
    assert!(blocked.log.is_none());
    type_text(&mut chat, "/pause");
    chat.on_key(Key::Enter, Instant::now());
    assert!(!chat.paused);
}

#[test]
fn slash_pause_cancels_a_running_turn() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnStarted {
        turn_id: TurnId(1),
        model: "openai/gpt-4.1".into(),
    });
    type_text(&mut chat, "/pause");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::Cancel))
    );
    assert!(chat.paused);
}

#[test]
fn a_leading_slash_lists_commands() {
    let mut chat = chat();
    type_text(&mut chat, "/");
    let view = frame_text(&mut chat);
    assert!(view.contains("/usage"), "{view}");
    assert!(view.contains("show token usage"), "{view}");
}

#[test]
fn goal_is_listed_after_a_slash() {
    let mut chat = chat();
    type_text(&mut chat, "/go");
    let view = frame_text(&mut chat);
    assert!(view.contains("/goal"), "{view}");
    assert!(view.contains("coder and reviewer"), "{view}");
}

/// A command that dispatches but is missing from the listing works yet
/// cannot be discovered; /goal shipped that way once.
#[test]
fn guard_every_listed_command_dispatches_and_does_something() {
    let dispatched = [
        "checkpoint",
        "checkpoints",
        "compact",
        "context",
        "goal",
        "help",
        "hotkeys",
        "keys",
        "usage",
        "login",
        "logout",
        "model",
        "pause",
        "memory",
        "advisor",
        "loop",
        "jobs",
        "tree",
        "recap",
        "rewind",
        "fork",
        "export",
        "btw",
        "settings",
        "switch",
        "budget",
        "duck",
        "hub",
        "join",
        "leave",
        "plan",
        "done",
        "whoami",
        "council",
        "graph",
        "git",
        "diagnose",
        "exit",
        "quit",
    ];

    for name in dispatched {
        assert!(
            COMMANDS.iter().any(|command| command.name == name),
            "/{name} dispatches but is not listed"
        );
    }

    for command in COMMANDS {
        let mut chat = chat();
        let arg = match command.name {
            "loop" => "90s ping",
            "budget" => "200k",
            "goal" | "council" | "graph" | "btw" => "task",
            "rewind" => "1",
            "login" | "logout" => "openai",
            "switch" => "openai/gpt-4.1",
            "memory" => "list",
            "export" => "path.md",
            "git" => "status",
            "jobs" => "list",
            _ => "",
        };

        let text = if arg.is_empty() {
            format!("/{}", command.name)
        } else {
            format!("/{} {}", command.name, arg)
        };

        let lines_before = chat.lines.len();
        let applied = chat.slash(&text).expect("failed to parse slash command");

        let is_unknown = chat.lines.iter().any(|l| {
            l.text
                .contains(&format!("unknown command /{}", command.name))
        });
        assert!(
            !is_unknown,
            "/{} is listed but not dispatched",
            command.name
        );

        let did_something = applied.effect.is_some()
            || applied.log.is_some()
            || chat.lines.len() > lines_before
            // A picker is an answer too: `/model`, `/login` and `/theme`
            // open one instead of printing.
            || chat.model_picker.is_some()
            || chat.login_picker.is_some()
            || chat.theme_picker.is_some();
        assert!(
            did_something,
            "/{} does nothing (no effect, no log, no output)",
            command.name
        );
    }
}

/// Every row of the table reaches the screen through `/hotkeys`: a heading
/// per group, then each binding spelled with its keys and what they do. The
/// listing is the table's own, so nothing about the keys is written twice.
#[test]
fn hotkeys_lists_the_bindings_by_group() {
    let mut chat = chat();
    command(&mut chat, "/hotkeys");
    let rows: Vec<String> = chat.lines.iter().map(|line| line.text.clone()).collect();

    for group in HotkeyGroup::ALL {
        let heading = format!("hotkeys · {}", group.title());
        assert!(
            rows.iter().any(|row| row.trim() == heading),
            "no heading for {}: {rows:#?}",
            group.title()
        );
    }
    for row in HOTKEYS {
        assert!(
            rows.iter()
                .any(|line| line.contains(row.keys) && line.contains(row.what)),
            "{} — {} never reached the screen: {rows:#?}",
            row.keys,
            row.what
        );
    }
}

/// Every key a terminal can hand the screen: `map_key` over the printable
/// characters and the named codes, under each modifier set a crossterm
/// event carries. The guard below walks what a person can press, not what
/// the `Key` enum happens to hold.
fn pressable_keys() -> Vec<Key> {
    let mut codes: Vec<KeyCode> = (' '..='~').map(KeyCode::Char).collect();
    codes.extend([
        KeyCode::Backspace,
        KeyCode::Enter,
        KeyCode::Esc,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Tab,
        KeyCode::BackTab,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Delete,
        KeyCode::Insert,
        KeyCode::F(1),
        KeyCode::Null,
    ]);
    let mods = [
        KeyModifiers::NONE,
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::SHIFT,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
        KeyModifiers::ALT | KeyModifiers::SHIFT,
    ];
    let mut keys: Vec<Key> = Vec::new();
    for code in codes {
        for modifiers in mods {
            let Some(key) = map_key(code, modifiers) else {
                continue;
            };
            // One printable character stands for them all: the listing
            // names typing once, as "any character".
            if matches!(key, Key::Char(_)) && keys.iter().any(|seen| matches!(seen, Key::Char(_))) {
                continue;
            }
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys
}

/// The states `on_key` branches on, one fresh chat each: a bare composer, a
/// draft, the `/` list, a running turn, a tool call waiting on yes and the
/// open pickers.
fn probe_chats() -> Vec<(&'static str, Chat)> {
    let mut draft = chat();
    draft.input = "two words".to_owned();
    draft.caret_to_end();
    draft.scroll_offset = 3;
    draft.last_transcript_height = 10;

    let mut list = chat();
    list.input = "/hel".to_owned();
    list.caret_to_end();

    let mut turn = chat();
    turn.turn_active = true;

    let mut approval = chat();
    approval.approval = Some(PendingApproval {
        call_id: "call-1".to_owned(),
        name: "bash".to_owned(),
        detail: Some("echo hi".to_owned()),
    });

    let mut model = chat();
    model.open_model_picker();

    let mut history = chat();
    history.history_picker = Some(HistoryPicker::open(vec!["a prompt".to_owned()]));

    let mut sessions = chat();
    sessions.session_picker = Some(0);

    // The states added with the paste menu and the tree: a paste long
    // enough to be offered a way to attach it.
    // The question the model is waiting on, and the same question with an
    // answer being typed into the composer.
    let mut asked = chat();
    asked.on_event(EngineEvent::AskRequested {
        request_id: "ask-1".into(),
        question: "which one?".into(),
        options: vec!["a".into(), "b".into()],
        multi: true,
        free_text: true,
    });

    let mut pasted = chat();
    pasted.paste(
        &(1..=120)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    // The vim mode, on: Normal swallows printable keys, so a character
    // that types in every other state moves the caret in this one.
    let mut vimming = chat();
    vimming.vim = Some(crate::vim::VimState::default());

    vec![
        ("bare", chat()),
        ("draft", draft),
        ("vim insert", vimming),
        ("vim normal", vim_normal_chat()),
        ("list", list),
        ("turn", turn),
        ("approval", approval),
        ("model picker", model),
        ("history", history),
        ("session picker", sessions),
        ("paste menu", pasted),
        ("question", asked),
    ]
}

/// Everything a key can move, as one string: the panel on screen — which
/// carries every open picker's own state, its cursor and its query — the
/// draft, the scroll, the hint, the two two-press timers, the transcript,
/// the approval and the model in use.
fn key_fingerprint(chat: &Chat) -> String {
    format!(
        "{:?}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        panel_view_for(chat, 30, 80),
        chat.lines.len(),
        chat.input,
        chat.scroll_offset,
        chat.hint,
        chat.quit_armed.is_some(),
        chat.esc_armed.is_some(),
        chat.picker_hidden,
        chat.selection.is_some(),
        chat.turn_active,
        chat.approval.is_some(),
        chat.model,
    )
}

/// How `/hotkeys` spells a key in its keys column. The match is exhaustive:
/// a new `Key` variant does not compile until it is spelled here, so the
/// guard below cannot quietly skip one.
///
/// `PageUpHalf` and `PageDownHalf` have no spelling because no terminal
/// can press them: `map_key` takes ctrl+u for the caret's own delete and
/// ctrl+d for quit before the half-page arms are reached.
fn hotkey_spelling(key: Key) -> Option<&'static str> {
    Some(match key {
        // A space is its own row: the one listing where it does something
        // (ticking a row of a question that takes several).
        Key::Char(' ') => "space",
        Key::Char(_) => "any character",
        Key::Backspace => "backspace",
        Key::Enter => "enter",
        Key::Esc => "esc",
        Key::Tab => "tab",
        Key::Up => "↑",
        Key::Down => "↓",
        Key::PageUp => "page up",
        Key::PageDown => "page down",
        Key::Left => "←",
        Key::Right => "→",
        Key::WordLeft => "alt+←",
        Key::WordRight => "alt+→",
        Key::Home => "home",
        Key::End => "end",
        Key::Delete => "delete",
        Key::DeleteToStart => "ctrl+u",
        Key::DeleteWord => "alt+backspace",
        Key::CtrlC => "ctrl+c",
        Key::CtrlD => "ctrl+d",
        Key::CtrlX => "ctrl+x",
        Key::CtrlR => "ctrl+r",
        Key::AltM => "alt+m",
        Key::AltF => "alt+f",
        Key::AltA => "alt+a",
        // Neither half-page key can be pressed any more: the crate's table
        // gives ctrl+u to the caret's own delete and ctrl+d to quit, and
        // `map_key` reaches the half-page arms after both.
        Key::PageUpHalf => return None,
        Key::PageDownHalf => return None,
    })
}

/// Whether the listing names `label` as one of the keys of a row: the keys
/// column of a `/hotkeys` line is what stands before the two spaces that
/// open its description, and `·` separates the alternatives in it.
fn hotkeys_name(lines: &[String], label: &str) -> bool {
    lines.iter().any(|line| {
        line.trim_start()
            .split("  ")
            .next()
            .is_some_and(|keys| keys.split('·').any(|cell| cell.trim() == label))
    })
}

/// The guard: a key the screen answers, in any state it branches on, is a
/// key `/hotkeys` names. A binding added to `on_key` — or to the mapper
/// above it — fails this until the table in `keys.rs` carries it, so the
/// listing cannot go stale in silence.
///
/// It is a key-level guard: which words a modal state uses for a key that is
/// already named (the approval prompt's `y`, say) is not machine-checked,
/// and neither are the chord, the mouse and the paste rows, which are no
/// single `Key` a probe can press.
#[test]
fn every_key_the_screen_answers_is_named_in_the_hotkeys_listing() {
    let now = Instant::now();
    let mut unlisted: Vec<String> = Vec::new();
    for key in pressable_keys() {
        let Some(spelling) = hotkey_spelling(key) else {
            continue;
        };
        for (state, mut chat) in probe_chats() {
            // Each state is checked against the listing its own config
            // prints: the vim rows are there only while the mode is on.
            let listing = if chat.vim.is_some() {
                hotkey_lines(crate::vim::VIM_HOTKEYS)
            } else {
                hotkey_lines(&[])
            };
            let before = key_fingerprint(&chat);
            let applied = chat.on_key(key, now);
            let answered = key_fingerprint(&chat) != before || applied.effect.is_some();
            if answered && !hotkeys_name(&listing, spelling) {
                unlisted.push(format!("{spelling} ({key:?}) in the {state} state"));
            }
        }
    }
    assert!(
        unlisted.is_empty(),
        "answered but not in the `/hotkeys` listing (crates/titi-cli/src/keys.rs): {unlisted:#?}"
    );
}

/// The badge is the engine's answer, not the keystroke: a mode the
/// engine never entered must not show as entered.
#[test]
fn plan_mode_enters_on_the_engines_word_and_done_leaves() {
    /// The masthead row, where the badge lives. The transcript below it
    /// also says "mode: plan", and that line is not the badge; the test
    /// backend is 80 columns wide, so the first row is the first 80

    /// characters of the frame.
    fn badge(chat: &mut Chat) -> String {
        frame_text(chat).chars().take(80).collect()
    }

    let mut chat = chat();
    type_text(&mut chat, "/plan");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::SetMode {
            mode: SessionMode::Plan
        }))
    );
    assert_eq!(chat.mode, SessionMode::Agent);
    assert!(!badge(&mut chat).contains("plan"), "badge moved too early");

    chat.on_event(EngineEvent::ModeChanged {
        mode: SessionMode::Plan,
    });
    assert_eq!(chat.mode, SessionMode::Plan);
    let shown = badge(&mut chat);
    assert!(shown.contains("plan"), "{shown}");

    type_text(&mut chat, "/done");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::SetMode {
            mode: SessionMode::Agent
        }))
    );
    chat.on_event(EngineEvent::ModeChanged {
        mode: SessionMode::Agent,
    });
    assert_eq!(chat.mode, SessionMode::Agent);
    let shown = badge(&mut chat);
    assert!(!shown.contains("plan"), "{shown}");
}

#[test]
fn done_outside_plan_mode_says_so_and_sends_nothing() {
    let mut chat = chat();
    type_text(&mut chat, "/done");
    assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("already in agent mode"))
    );
}

#[test]
fn duck_enters_its_own_mode_and_done_leaves_it() {
    let mut chat = chat();
    type_text(&mut chat, "/duck");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::SetMode {
            mode: SessionMode::Duck
        }))
    );
    chat.on_event(EngineEvent::ModeChanged {
        mode: SessionMode::Duck,
    });
    let badge: String = frame_text(&mut chat).chars().take(80).collect();
    assert!(badge.contains("duck"), "{badge}");

    type_text(&mut chat, "/done");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::SetMode {
            mode: SessionMode::Agent
        }))
    );
}

/// With host-on-demand, joining an empty hub binds the broker automatically.
#[test]
fn join_without_a_broker_hosts_on_demand() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();
    type_text(&mut chat, "/join");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(chat.hub.joined());
    assert!(chat.hub.peers().contains(&"session-123".to_string()));
}

#[test]
fn presence_fills_the_roster_panel_and_hub_toggles_it() {
    let mut chat = chat();
    chat.hub.ingest(titi_core::hub::HubEvent::Presence {
        agents: vec!["session-123".into(), "scout".into()],
    });
    // The panel is hidden until /hub asks for it.
    assert!(!frame_text(&mut chat).contains("scout"));

    type_text(&mut chat, "/hub");
    assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
    let view = frame_text(&mut chat);
    assert!(view.contains("scout"), "{view}");
    assert!(view.contains("2 peer(s)"), "{view}");

    type_text(&mut chat, "/hub");
    chat.on_key(Key::Enter, Instant::now());
    assert!(!frame_text(&mut chat).contains("scout"));
}

/// A peer that leaves has to leave the roster too.
#[test]
fn a_peer_leaving_drops_off_the_roster() {
    let mut chat = chat();
    chat.hub.ingest(titi_core::hub::HubEvent::Presence {
        agents: vec!["scout".into(), "builder".into()],
    });
    chat.hub.ingest(titi_core::hub::HubEvent::Left {
        agent_id: "scout".into(),
    });
    chat.hub_open = true;
    let view = frame_text(&mut chat);
    assert!(view.contains("builder"), "{view}");
    assert!(!view.contains("scout"), "{view}");
}

/// The whole path against a real broker: `/join` registers, the roster
/// fills, and a peer's broadcast reaches the transcript through the same
/// non-blocking poll the pump runs.
#[test]
fn a_joined_session_hears_its_peers() {
    let dir = tempfile::tempdir().expect("temp");
    let broker = titi_core::hub::HubBroker::bind(dir.path()).expect("broker");
    let peer = titi_core::hub::HubClient::connect(dir.path(), "scout").expect("peer");

    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();
    type_text(&mut chat, "/join");
    assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
    assert!(chat.hub.joined());
    assert_eq!(chat.hub.agent_id(), Some("session-123"));
    let view = frame_text(&mut chat);
    assert!(view.contains("scout"), "{view}");

    peer.broadcast("ci is red").expect("broadcast");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        chat.poll_hub();
        if chat
            .lines
            .iter()
            .any(|line| line.text.contains("ci is red"))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        chat.lines
            .iter()
            .any(|line| line.kind == LineKind::Note && line.text == "hub scout (all): ci is red"),
        "{:?}",
        chat.lines
    );

    type_text(&mut chat, "/leave");
    chat.on_key(Key::Enter, Instant::now());
    assert!(!chat.hub.joined());
    drop(peer);
    broker.shutdown();
}

#[test]
fn leave_without_a_hub_says_so() {
    let mut chat = chat();
    type_text(&mut chat, "/leave");
    assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("hub: not joined"))
    );
}

#[test]
fn budget_sets_clears_and_reports_a_cap() {
    let mut chat = chat();
    type_text(&mut chat, "/budget 200k");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::SetBudget {
            tokens: Some(200_000)
        }))
    );

    // "no cap" is one intent and the engine keeps two bounds: one
    // keystroke lifts both.
    type_text(&mut chat, "/budget off");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::SendAll(vec![
            EngineCommand::SetBudget { tokens: None },
            EngineCommand::SetMoneyBudget { micro_usd: None },
        ]))
    );

    chat.on_event(EngineEvent::BudgetUpdated {
        spent: 1_200,
        limit: Some(4_000),
    });
    type_text(&mut chat, "/budget");
    assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("1200 of 4000 tokens spent (30%)")),
        "{:?}",
        chat.lines.last()
    );
    // The spend is the provider's count wherever it reports one; calling
    // all of it an estimate undersells the number.
    assert!(
        !chat
            .lines
            .last()
            .is_some_and(|line| line.text.contains("estimated")),
        "{:?}",
        chat.lines.last()
    );
}

/// A cap in money is a cap: `$2` reads as exactly two million
/// micro-dollars — no float, no rounding — and goes to the engine as its
/// own command, beside the token one.
#[test]
fn budget_caps_money_as_well_as_tokens() {
    let mut chat = chat();
    type_text(&mut chat, "/budget $2");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::SetMoneyBudget {
            micro_usd: Some(2_000_000)
        }))
    );
    assert!(
        chat.lines.iter().any(|line| line.text == "budget: $2.00"),
        "{:?}",
        chat.lines.last()
    );

    // A fraction of a cent is a cap too, and prints as one rather than as
    // `$0.00`.
    type_text(&mut chat, "/budget $0.000001");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::SetMoneyBudget {
            micro_usd: Some(1)
        }))
    );
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "budget: $0.000001"),
        "{:?}",
        chat.lines.last()
    );
}

/// The three money events, each in the shape the token bound's already is:
/// the state `/budget` reports, the pause the cap leaves, and the model the
/// engine cannot measure.
#[test]
fn the_money_budget_events_state_themselves() {
    let mut chat = chat();
    chat.on_event(EngineEvent::MoneyBudgetUpdated {
        spent_micro_usd: 380_000,
        limit_micro_usd: Some(2_000_000),
    });
    type_text(&mut chat, "/budget");
    chat.on_key(Key::Enter, Instant::now());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("$0.38 of $2.00 in money")),
        "{:?}",
        chat.lines.last()
    );

    // The cap reached: the same paused screen the token cap leaves.
    chat.on_event(EngineEvent::MoneyBudgetExceeded {
        spent_micro_usd: 2_010_000,
        limit_micro_usd: 2_000_000,
    });
    assert!(chat.paused);
    assert!(
        chat.lines.iter().any(|line| line.kind == LineKind::Error
            && line
                .text
                .contains("budget reached: $2.01 of $2.00 in money")),
        "{:?}",
        chat.lines.last()
    );

    // A cap over a model with no price is named, not silently unenforced.
    let mut unpriced = chat_with_theme(test_theme());
    unpriced.on_event(EngineEvent::MoneyBudgetUnpriced {
        model: "local/llama".into(),
    });
    assert!(
        unpriced
            .lines
            .iter()
            .any(|line| line.kind == LineKind::Error
                && line.text
                    == "budget: local/llama has no price, so a cap in money cannot be enforced \
                    over it"),
        "{:?}",
        unpriced.lines.last()
    );
}

/// A cap in money cannot be enforced by a token cap — the engine counts
/// tokens, and those bill at different rates — so it is refused, and the
/// refusal names what is missing instead of converting at a guessed rate.
#[test]
fn budget_reads_tokens_and_money_exactly() {
    // Tokens keep their own suffixes and their own rounding.
    assert_eq!(parse_budget("200k"), Ok(Budget::Tokens(200_000)));
    assert_eq!(parse_budget("1.5m"), Ok(Budget::Tokens(1_500_000)));
    assert_eq!(parse_budget("500"), Ok(Budget::Tokens(500)));

    // Money is digits into micro-dollars: exact, never a float.
    assert_eq!(parse_budget("$2"), Ok(Budget::Money(2_000_000)));
    assert_eq!(parse_budget("$0.50"), Ok(Budget::Money(500_000)));
    assert_eq!(parse_budget("$0.5"), Ok(Budget::Money(500_000)));
    assert_eq!(parse_budget("$0.000001"), Ok(Budget::Money(1)));
    assert_eq!(parse_budget("$.50"), Ok(Budget::Money(500_000)));
    assert_eq!(parse_budget("$12"), Ok(Budget::Money(12_000_000)));

    // A typo is a typo in either unit.
    for word in ["plenty", "$", "$-2", "$2.5.5", "$1e3", "$ 2", ""] {
        assert!(
            matches!(parse_budget(word), Err(BudgetArgError::Unreadable(_))),
            "{word:?} parsed"
        );
    }
    // Zero is no cap in either unit.
    assert_eq!(parse_budget("0"), Err(BudgetArgError::Zero));
    assert_eq!(parse_budget("$0"), Err(BudgetArgError::Zero));
    assert_eq!(parse_budget("$0.00"), Err(BudgetArgError::Zero));
    // Seven decimals of zero is refused for its shape: the precision is
    // checked before the amount, because that is the typo it is.
    assert_eq!(
        parse_budget("$0.0000000"),
        Err(BudgetArgError::Finer("$0.0000000".to_owned()))
    );
    // Finer than the unit a cap is kept in, so it is refused rather than
    // rounded into a different cap.
    assert_eq!(
        parse_budget("$0.0000001"),
        Err(BudgetArgError::Finer("$0.0000001".to_owned()))
    );

    // …and the command says which of them happened.
    let mut chat = chat();
    type_text(&mut chat, "/budget $0.0000001");
    assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.kind == LineKind::Error
                && line.text.contains("finer than a micro-dollar")),
        "{:?}",
        chat.lines.last()
    );
    type_text(&mut chat, "/budget plenty");
    assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.kind == LineKind::Error && line.text.contains("neither tokens")),
        "{:?}",
        chat.lines.last()
    );
}

/// Hitting the cap pauses: the next prompt is held instead of sent.
#[test]
fn a_reached_budget_pauses_the_screen() {
    let mut chat = chat();
    chat.on_event(EngineEvent::BudgetExceeded {
        spent: 4_100,
        limit: 4_000,
    });
    assert!(chat.paused);
    assert!(
        chat.lines
            .iter()
            .any(|line| line.kind == LineKind::Error && line.text.contains("budget reached"))
    );

    type_text(&mut chat, "carry on then");
    let blocked = chat.on_key(Key::Enter, Instant::now());
    assert!(blocked.effect.is_none());
    assert!(blocked.log.is_none());

    // Raising the cap is the way out, and it goes to the engine.
    type_text(&mut chat, "/budget 1m");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::SetBudget {
            tokens: Some(1_000_000)
        }))
    );
}

#[test]
fn loop_hands_the_interval_and_prompt_to_the_engine() {
    let mut chat = chat();
    type_text(&mut chat, "/loop 5m check the CI run");
    match chat.on_key(Key::Enter, Instant::now()).effect {
        Some(ChatEffect::Send(EngineCommand::StartLoop {
            interval_secs,
            prompt,
        })) => {
            assert_eq!(interval_secs, 300);
            assert_eq!(prompt.as_str(), "check the CI run");
        }
        other => panic!("expected a loop, got {other:?}"),
    }
}

/// A guessed schedule is worse than none: an unreadable interval has to
/// refuse instead of falling back to a default.
#[test]
fn loop_refuses_an_unreadable_interval_and_a_missing_prompt() {
    let mut chat = chat();
    type_text(&mut chat, "/loop soon do the thing");
    assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.kind == LineKind::Error && line.text.contains("soon"))
    );

    type_text(&mut chat, "/loop 30s");
    assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("usage: /loop"))
    );

    type_text(&mut chat, "/loop 0s tick");
    assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("at least one second"))
    );
}

#[test]
fn advisor_consults_with_and_without_a_question() {
    let mut chat = chat();
    type_text(&mut chat, "/advisor");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::Consult { question: None }))
    );

    type_text(&mut chat, "/advisor is the migration safe?");
    match chat.on_key(Key::Enter, Instant::now()).effect {
        Some(ChatEffect::Send(EngineCommand::Consult {
            question: Some(question),
        })) => assert_eq!(question.as_str(), "is the migration safe?"),
        other => panic!("expected a consult, got {other:?}"),
    }
}

/// The advisor answers, it never acts: a consult is not a turn and
/// nothing it says is logged as the assistant's.
#[test]
fn an_advisor_answer_is_shown_without_starting_a_turn() {
    let mut chat = chat();
    let applied = chat.on_event(EngineEvent::AdvisorAnswer {
        text: "  you skipped the migration  ".into(),
    });
    assert!(applied.effect.is_none());
    assert!(applied.log.is_none());
    assert!(!chat.turn_active);
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "advisor · you skipped the migration")
    );
}

/// Silence from an advisor reads like agreement, so it is reported as a
/// failure instead.
#[test]
fn an_empty_or_failed_consult_is_reported_as_a_failure() {
    let mut silent = chat();
    silent.on_event(EngineEvent::AdvisorAnswer { text: "  ".into() });
    assert!(
        silent
            .lines
            .iter()
            .any(|line| line.kind == LineKind::Error && line.text.contains("failed consult"))
    );

    let mut broken = chat();
    broken.on_event(EngineEvent::AdvisorFailed {
        reason: "advisor model gpt-x is unavailable: no key".into(),
    });
    assert!(broken.lines.iter().any(|line| {
        line.kind == LineKind::Error
            && line.text.contains("failed consult")
            && line.text.contains("no key")
    }));
}

#[test]
fn jobs_lists_and_cancels() {
    let mut chat = chat();
    type_text(&mut chat, "/jobs");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::ListJobs))
    );

    type_text(&mut chat, "/jobs cancel job-2");
    match chat.on_key(Key::Enter, Instant::now()).effect {
        Some(ChatEffect::Send(EngineCommand::CancelJob { job_id })) => {
            assert_eq!(job_id.as_str(), "job-2");
        }
        other => panic!("expected a cancel, got {other:?}"),
    }

    type_text(&mut chat, "/jobs cancel");
    assert!(chat.on_key(Key::Enter, Instant::now()).effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("usage: /jobs cancel"))
    );
}

/// A background loop the user cannot see is a loop they cannot stop.
#[test]
fn a_running_loop_shows_in_the_status_bar_until_it_stops() {
    let mut chat = chat();
    chat.on_event(EngineEvent::JobStarted {
        job: JobInfo {
            id: "job-1".into(),
            prompt: "watch CI".into(),
            interval_secs: 60,
            runs: 0,
        },
    });
    let view = frame_text(&mut chat);
    assert!(view.contains("1 loop(s)"), "{view}");

    chat.on_event(EngineEvent::JobFinished {
        job_id: "job-1".into(),
    });
    let view = frame_text(&mut chat);
    assert!(!view.contains("loop(s)"), "{view}");
}

#[test]
fn an_empty_job_list_says_so() {
    let mut chat = chat();
    chat.on_event(EngineEvent::JobList { jobs: Vec::new() });
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("no background jobs"))
    );
}

/// The tokens of one breakdown line, `None` for anything else.
fn tokens_in(line: &str) -> Option<u64> {
    line.split(" tokens")
        .next()?
        .split_whitespace()
        .next_back()?
        .parse()
        .ok()
}

/// A breakdown whose parts do not add up to its total is worse than no
/// breakdown: it reads as if something were hiding in the window.
#[test]
fn context_renders_parts_that_sum_to_the_reported_total() {
    let mut chat = chat();
    chat.on_event(EngineEvent::ContextBreakdown {
        parts: vec![
            ContextPart {
                label: "system prompt".into(),
                tokens: 300,
            },
            ContextPart {
                label: "genome map".into(),
                tokens: 500,
            },
            ContextPart {
                label: "history".into(),
                tokens: 200,
            },
        ],
        window: 10_000,
    });

    let rendered: Vec<String> = chat.lines.iter().map(|line| line.text.clone()).collect();
    let Some(total) = rendered.iter().find(|line| line.starts_with("total")) else {
        panic!("no total line: {rendered:?}");
    };
    let summed: u64 = rendered
        .iter()
        .filter(|line| !line.starts_with("total"))
        .filter_map(|line| tokens_in(line))
        .sum();

    assert_eq!(tokens_in(total), Some(summed), "{rendered:?}");
    assert_eq!(summed, 1000, "{rendered:?}");
    assert!(total.contains("10% of 10000"), "{rendered:?}");
    assert!(
        rendered.iter().any(|line| line.contains("estimates")),
        "the numbers are estimates and must say so: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|line| line.starts_with("genome map") && line.contains("50%")),
        "{rendered:?}"
    );
}

#[test]
fn context_takes_no_argument() {
    let mut chat = chat();
    type_text(&mut chat, "/context");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::DescribeContext))
    );

    type_text(&mut chat, "/context now");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none(), "{:?}", applied.effect);
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("usage: /context")),
        "a stray argument was swallowed"
    );
}

#[test]
fn compact_dispatches_with_and_without_a_focus() {
    let mut chat = chat();
    type_text(&mut chat, "/compact");
    assert_eq!(
        chat.on_key(Key::Enter, Instant::now()).effect,
        Some(ChatEffect::Send(EngineCommand::Compact { focus: None }))
    );

    type_text(&mut chat, "/compact the auth refactor");
    match chat.on_key(Key::Enter, Instant::now()).effect {
        Some(ChatEffect::Send(EngineCommand::Compact { focus: Some(focus) })) => {
            assert_eq!(focus.as_str(), "the auth refactor");
        }
        other => panic!("expected a focused compaction, got {other:?}"),
    }
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("focus: the auth refactor")),
        "the focus never reached the transcript"
    );
}

#[test]
fn a_slash_inside_a_sentence_does_not_list_commands() {
    let mut chat = chat();
    type_text(&mut chat, "see /rewind");
    let view = frame_text(&mut chat);
    assert!(!view.contains("cut back to a rewind point"), "{view}");
}

#[test]
fn tab_fills_the_highlighted_command() {
    let mut chat = chat();
    type_text(&mut chat, "/");
    chat.on_key(Key::Down, Instant::now());
    chat.on_key(Key::Tab, Instant::now());
    assert_eq!(chat.input, "/checkpoints ");
}

#[test]
fn enter_on_a_prefix_runs_the_highlighted_command() {
    let mut chat = chat();
    type_text(&mut chat, "/re");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(chat.lines.iter().any(|line| line.text.contains("recap")));
}

/// The row the arrows are on is the choice, not the word under the caret:
/// `/paseo` names a skill of its own *and* starts `/paseo-advisor`, so
/// Enter used to send the typed word and leave the highlighted row alone.
/// A pre-fix run dispatched `SubmitPrompt("/paseo")` here.
#[test]
fn enter_runs_the_highlighted_row_over_an_exact_word() {
    let mut chat = chat_with_prefix_skills();
    type_text(&mut chat, "/paseo");
    assert!(
        chat.picking(),
        "the list has the typed skill and the longer one"
    );
    chat.on_key(Key::Down, Instant::now());
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SubmitPrompt {
            text: "/paseo-advisor".into(),
        })),
        "the highlighted row is what Enter sends"
    );
}

/// The other half of the same rule: when the highlight already is the
/// typed word there is nothing to apply, so a sentence that names a skill
/// exactly still sends on one Enter rather than finishing the token first.
#[test]
fn enter_keeps_the_typed_word_when_the_highlight_is_on_it() {
    let mut chat = chat_with_skills();
    type_text(&mut chat, "please run /code-review");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SubmitPrompt {
            text: "please run /code-review".into(),
        })),
        "the highlighted row is the typed word, so it sent as typed"
    );
}

fn chat_with_skills() -> Chat {
    let mut chat = chat();
    chat.skills = vec![SkillRow {
        name: "code-review".to_owned(),
        about: "check a diff".to_owned(),
    }];
    chat
}

/// Two skills of the shape the command list has in `/checkpoint` and
/// `/checkpoints`: one name is a whole row and a prefix of the next.
fn chat_with_prefix_skills() -> Chat {
    let mut chat = chat();
    chat.skills = vec![
        SkillRow {
            name: "paseo".to_owned(),
            about: "a group of skills".to_owned(),
        },
        SkillRow {
            name: "paseo-advisor".to_owned(),
            about: "a second opinion".to_owned(),
        },
    ];
    chat
}

#[test]
fn a_slash_inside_a_sentence_lists_skills() {
    let mut chat = chat_with_skills();
    type_text(&mut chat, "please run /cod");
    let view = frame_text(&mut chat);
    assert!(view.contains("code-review"), "{view}");
    assert!(view.contains("·skill"), "{view}");
}

#[test]
fn completing_a_skill_keeps_the_rest_of_the_line() {
    let mut chat = chat_with_skills();
    type_text(&mut chat, "please run /cod");
    chat.on_key(Key::Tab, Instant::now());
    assert_eq!(chat.input, "please run /code-review ");
}

#[test]
fn enter_mid_sentence_completes_the_skill_instead_of_sending() {
    let mut chat = chat_with_skills();
    type_text(&mut chat, "please run /cod");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert_eq!(chat.input, "please run /code-review ");
}

#[test]
fn a_leading_skill_name_is_sent_as_typed() {
    let mut chat = chat_with_skills();
    type_text(&mut chat, "/code-review this diff");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SubmitPrompt {
            text: "/code-review this diff".into(),
        }))
    );
    assert_eq!(
        applied.log,
        Some(LogWrite::text(
            Role::User,
            "/code-review this diff".to_owned()
        ))
    );
    assert!(
        !chat
            .lines
            .iter()
            .any(|line| line.text.contains("unknown command")),
        "a known skill must not be refused as a command"
    );
}

#[test]
fn a_command_still_wins_over_a_skill_of_the_same_prefix() {
    let mut chat = chat_with_skills();
    chat.skills.push(SkillRow {
        name: "recap-notes".to_owned(),
        about: "notes".to_owned(),
    });
    type_text(&mut chat, "/recap");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(chat.lines.iter().any(|line| line.text.contains("recap")));
}

#[test]
fn login_masks_the_key_and_stores_it() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    type_text(&mut chat, "/login openai");
    chat.on_key(Key::Enter, Instant::now());
    assert_eq!(chat.login_for.as_deref(), Some("openai"));
    type_text(&mut chat, "sk-secret-value");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.log.is_none());
    assert!(chat.login_for.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("key stored"))
    );
    assert!(
        !chat
            .lines
            .iter()
            .any(|line| line.text.contains("sk-secret"))
    );
    let keys = crate::secrets::list_keys(dir.path()).expect("keys");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].provider, "openai");
}

#[test]
fn logout_forgets_the_stored_key() {
    let dir = tempfile::tempdir().expect("temp");
    crate::secrets::store_key(dir.path(), "openai", "sk-test").expect("store");
    let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    type_text(&mut chat, "/logout openai");
    chat.on_key(Key::Enter, Instant::now());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("signed out"))
    );
    assert!(crate::secrets::list_keys(dir.path()).unwrap().is_empty());
}

#[test]
fn an_inline_login_does_not_echo_the_key() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    type_text(&mut chat, "/login openai sk-one-line");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.log.is_none());
    assert!(
        !chat
            .lines
            .iter()
            .any(|line| line.text.contains("sk-one-line"))
    );
    assert_eq!(
        crate::secrets::list_keys(dir.path()).unwrap()[0].provider,
        "openai"
    );
}

/// A flow that never answers. The screen's own half of a login is all
/// this test drives, and it must not open a socket to do it.
struct NoNetworkFlow;

impl LoginDriver for NoNetworkFlow {
    fn begin(&self, _provider: &'static OAuthProvider) -> Result<LoginFlow, String> {
        let (_urls, events) = tokio::sync::mpsc::unbounded_channel();
        let (codes, _lines) = tokio::sync::mpsc::unbounded_channel();
        Ok(LoginFlow { events, codes })
    }

    fn begin_device(&self, _provider: &'static OAuthProvider) -> Result<LoginFlow, String> {
        let (_urls, events) = tokio::sync::mpsc::unbounded_channel();
        let (codes, _lines) = tokio::sync::mpsc::unbounded_channel();
        Ok(LoginFlow { events, codes })
    }
}

/// Bare `/login` is the subscription picker: the rows sit above the
/// composer and go away on Esc.
#[test]
fn bare_login_paints_the_subscription_picker() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    chat.set_login_driver(Arc::new(NoNetworkFlow));

    type_text(&mut chat, "/login");
    chat.on_key(Key::Enter, Instant::now());
    let frame = frame_text(&mut chat);
    for expected in [
        "Anthropic (Claude Pro/Max)",
        "ChatGPT Plus/Pro (Codex Subscription)",
        "·browser",
        "·device code",
    ] {
        assert!(frame.contains(expected), "{expected} is missing: {frame}");
    }

    chat.on_key(Key::Esc, Instant::now());
    let frame = frame_text(&mut chat);
    assert!(
        !frame.contains("device code"),
        "esc closed the picker: {frame}"
    );
}

/// The device grant finishes in the browser, so the composer asks for no
/// code and offers no Enter: only the way out.
#[test]
fn the_device_login_asks_for_no_code() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    chat.set_login_driver(Arc::new(NoNetworkFlow));

    type_text(&mut chat, "/login openai-codex device");
    chat.on_key(Key::Enter, Instant::now());
    type_text(&mut chat, "abc");

    let frame = frame_text(&mut chat);
    assert!(!frame.contains("paste the code"), "{frame}");
    assert!(!frame.contains("enter submits"), "{frame}");
    assert!(
        !frame.contains('•'),
        "no line is typed in device mode: {frame}"
    );
    assert!(frame.contains("esc cancels"), "{frame}");
}

/// A provider with an OAuth descriptor asks for a code, not a key: the
/// composer says which, and Enter hands the line to the flow — never to
/// the store, which would keep a half of the login as a credential.
#[test]
fn login_for_an_oauth_provider_enters_code_mode() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    chat.set_login_driver(Arc::new(NoNetworkFlow));

    type_text(&mut chat, "/login anthropic");
    chat.on_key(Key::Enter, Instant::now());

    let frame = frame_text(&mut chat);
    assert!(
        frame.contains("paste the code or the redirect URL"),
        "{frame}"
    );
    assert!(frame.contains("enter submits"), "{frame}");

    type_text(&mut chat, "sk-test-code");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(chat.login_for.is_some(), "the login is still open");
    assert!(
        crate::secrets::list_keys(dir.path())
            .expect("keys")
            .is_empty(),
        "a pasted code is not a key"
    );
}

/// An agent directory whose config declares a provider the builtin table
/// does not know — how a user adds a gateway of their own.
fn agent_dir_with_extra_provider() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp");
    std::fs::write(
        dir.path().join("config.yml"),
        "providers:\n  \
         - id: zai\n    \
         api: openai-completions\n    \
         base_url: https://api.example.invalid/v1\n    \
         credential_env: ZAI_API_KEY\n    \
         credential_required: true\n\
         models:\n  \
         - id: zai/glm-4.6\n    \
         provider: zai\n    \
         wire_model: glm-4.6\n",
    )
    .expect("config");
    dir
}

/// The engine runs on the merged registry, so the screen must too: a
/// provider the user declared is one `/login` has to take a key for.
#[test]
fn login_accepts_a_provider_the_config_declares() {
    let dir = agent_dir_with_extra_provider();
    let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    type_text(&mut chat, "/login zai");
    chat.on_key(Key::Enter, Instant::now());
    assert_eq!(chat.login_for.as_deref(), Some("zai"));
    assert!(
        !chat
            .lines
            .iter()
            .any(|line| line.text.contains("unknown provider")),
        "{:?}",
        chat.lines
    );
    type_text(&mut chat, "sk-test");
    chat.on_key(Key::Enter, Instant::now());
    let keys = crate::secrets::list_keys(dir.path()).expect("keys");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].provider, "zai");
}

#[test]
fn keys_lists_a_provider_the_config_declares() {
    let dir = agent_dir_with_extra_provider();
    let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    type_text(&mut chat, "/keys");
    chat.on_key(Key::Enter, Instant::now());
    for provider in ["zai ", "openai "] {
        assert!(
            chat.lines
                .iter()
                .any(|line| line.text.starts_with(provider)),
            "{provider}missing: {:?}",
            chat.lines
        );
    }
}

/// A local server that takes no credential is ready as it is: `/keys`
/// and `/diagnose` must not list it as missing something.
#[test]
fn a_provider_without_a_credential_is_not_missing_a_key() {
    let dir = agent_dir_with_extra_provider();
    let mut chat = Chat::new("openai/gpt-4.1", "session-123", test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    type_text(&mut chat, "/keys");
    chat.on_key(Key::Enter, Instant::now());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "ollama  no key needed"),
        "{:?}",
        chat.lines
    );
    // A provider that does need one and has none still says so (the
    // test environment sets no ZAI_API_KEY).
    assert!(
        chat.lines.iter().any(|line| line.text == "zai  no key"),
        "{:?}",
        chat.lines
    );

    type_text(&mut chat, "/diagnose");
    chat.on_key(Key::Enter, Instant::now());
    let summary = chat.lines.last().expect("a transcript line");
    assert!(
        summary.text.contains("ollama (no key needed)"),
        "{summary:?}"
    );
}

#[test]
fn diagnose_lists_a_provider_the_config_declares() {
    let dir = agent_dir_with_extra_provider();
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();
    type_text(&mut chat, "/diagnose");
    chat.on_key(Key::Enter, Instant::now());
    let summary = chat.lines.last().expect("a transcript line");
    for provider in ["zai (", "openai ("] {
        assert!(
            summary.text.contains(provider),
            "{provider} missing: {summary:?}"
        );
    }
}

#[test]
fn a_path_is_not_a_slash_command() {
    let mut chat = chat();
    type_text(&mut chat, "/tmp/photo.png");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(matches!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SubmitPrompt { .. }))
    ));
}

#[test]
fn unknown_slash_is_not_sent_to_the_model() {
    let mut chat = chat();
    type_text(&mut chat, "/nope");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(chat.lines.iter().any(|line| line.text.contains("unknown")));
}

#[test]
fn rewind_restores_the_history_and_tells_the_engine() {
    let dir = tempfile::tempdir().expect("temp");
    let store = titi_core::session::SessionStore::new(dir.path()).expect("store");
    let id = store
        .create(titi_core::session::SessionMeta::default())
        .expect("session");
    store.append(&id, Role::User, "keep").expect("keep");
    store.checkpoint(&id).expect("checkpoint");
    store.append(&id, Role::User, "drop").expect("drop");
    let mut chat = Chat::new("openai/gpt-4.1", &id, test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    type_text(&mut chat, "/rewind");
    let applied = chat.on_key(Key::Enter, Instant::now());
    match applied.effect {
        Some(ChatEffect::Send(EngineCommand::RestoreHistory { messages })) => {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].content.as_str(), "keep");
        }
        other => panic!("expected restore, got {other:?}"),
    }
    assert!(chat.lines.iter().any(|line| line.text == "keep"));
    assert!(!chat.lines.iter().any(|line| line.text == "drop"));
}

/// A resumed session is replayed into the engine, so the model answers
/// with that conversation in mind; the screen shows the same conversation
/// rather than a welcome that reads as a fresh start.
#[test]
fn a_resumed_session_shows_its_conversation() {
    let dir = tempfile::tempdir().expect("temp");
    let store = titi_core::session::SessionStore::new(dir.path()).expect("store");
    let id = store
        .create(titi_core::session::SessionMeta::default())
        .expect("session");
    store
        .append(&id, Role::User, "remember the word banana")
        .expect("user");
    store
        .append(&id, Role::Assistant, "noted: banana")
        .expect("assistant");
    let mut chat = Chat::new("openai/gpt-4.1", &id, test_theme());
    chat.agent_dir = dir.path().to_path_buf();

    chat.show_stored_history();

    let shown: Vec<(LineKind, &str)> = chat
        .lines
        .iter()
        .map(|line| (line.kind, line.text.as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            (LineKind::User, "remember the word banana"),
            (LineKind::Assistant, "noted: banana"),
        ]
    );
    let frame = frame_rows(&mut chat, 80, 20).join("\n");
    assert!(!frame.contains("say what you want done"), "{frame}");

    // A session with nothing in it keeps the welcome.
    let fresh = store
        .create(titi_core::session::SessionMeta::default())
        .expect("fresh");
    let mut chat = Chat::new("openai/gpt-4.1", &fresh, test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    chat.show_stored_history();
    assert!(chat.lines.is_empty(), "{:?}", chat.lines);
}

/// A paste is usually code or a log, and its line breaks are part of it:
/// they reach the model as written, whatever the terminal's line ending,
/// and a tab stays a tab.
#[test]
fn a_pasted_block_keeps_its_lines() {
    let mut chat = chat();
    chat.paste("fn main() {\r\n\tprintln!(\"hi\");\r}\n");
    assert_eq!(chat.input, "fn main() {\n\tprintln!(\"hi\");\n}\n");

    // One row in the composer, each break shown, nothing cut mid-word.
    let rows = frame_rows(&mut chat, 80, 20);
    assert!(
        rows[17].contains("fn main() {↵    println!(\"hi\");↵}"),
        "{:?}",
        rows[17]
    );

    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SubmitPrompt {
            text: "fn main() {\n\tprintln!(\"hi\");\n}".into()
        }))
    );
    // The transcript shows it as the lines it is.
    let frame = frame_rows(&mut chat, 80, 20);
    let first = frame
        .iter()
        .position(|row| row.contains("fn main() {"))
        .expect("the first line");
    assert!(frame[first + 1].contains("│     println!"), "{frame:?}");
    assert!(!frame[first].contains("println!"), "{frame:?}");
}

/// The text a send carries, whichever command it is: a prompt opens a
/// turn and a later one steers it, and a paste test is about the text.
fn sent_text(applied: &Applied) -> Option<String> {
    match applied.effect.as_ref()? {
        ChatEffect::Send(EngineCommand::SubmitPrompt { text })
        | ChatEffect::Send(EngineCommand::Steer { text }) => Some(text.to_string()),
        _ => None,
    }
}

/// A body taller than the composer's threshold: eight lines of a stack
/// trace, which is the shape the collapse exists for.
fn stack_trace() -> String {
    (1..=8)
        .map(|n| format!("  at frame {n} (module.rs:{n})"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A paste above the threshold leaves one marker in the draft, not the
/// wall: the draft is what the user can still edit and send.
#[test]
fn a_long_paste_collapses_to_a_marker() {
    let mut chat = chat();
    chat.paste(&stack_trace());
    assert_eq!(
        chat.input, "[Paste #1 · 8 lines]",
        "the draft is the marker, not the body"
    );
    // The frame draws the marker whole — and none of the wall.
    let frame = frame_text(&mut chat);
    assert!(frame.contains("[Paste #1 · 8 lines]"), "{frame}");
    assert!(
        !frame.contains("frame 5"),
        "the body is not on screen: {frame}"
    );
}

/// The boundary the collapse turns on: the threshold sits between the two.
#[test]
fn a_paste_at_the_threshold_stays_inline() {
    let six = (1..=6)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut chat = chat();
    chat.paste(&six);
    assert_eq!(chat.input, six, "six lines are still a draft");

    let mut chat = chat_with_theme(test_theme());
    let seven = (1..=7)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    chat.paste(&seven);
    assert_eq!(chat.input, "[Paste #1 · 7 lines]");
}

/// Sending expands the marker: the model reads the whole paste, the
/// transcript echoes the marker, and the session file records what was
/// sent — so the wall is never lost and the screen is never the wall.
#[test]
fn a_collapsed_paste_expands_when_it_is_sent() {
    let mut chat = chat();
    chat.insert_at_caret("what is this trace? ");
    chat.paste(&stack_trace());
    let applied = chat.on_key(Key::Enter, Instant::now());

    let text = sent_text(&applied).expect("the prompt goes out");
    assert_eq!(
        text,
        "what is this trace?   at frame 1 (module.rs:1)\n  at frame 2 (module.rs:2)\n  at frame 3 (module.rs:3)\n  at frame 4 (module.rs:4)\n  at frame 5 (module.rs:5)\n  at frame 6 (module.rs:6)\n  at frame 7 (module.rs:7)\n  at frame 8 (module.rs:8)",
        "the whole paste is sent"
    );
    let log = applied.log.expect("the prompt is recorded");
    assert_eq!(log.text, text, "the session file holds what was sent");
    assert!(
        !log.text.contains("[Paste #"),
        "the marker is never text the model reads"
    );

    // The transcript echo is the marker, and no line of the wall follows.
    let frame = frame_text(&mut chat);
    assert!(
        frame.contains("what is this trace? [Paste #1 · 8 lines]"),
        "{frame}"
    );
    assert!(!frame.contains("frame 4"), "{frame}");
}

/// The guard: only a marker the open draft registered expands. Once the
/// draft is gone the same characters are text, so a marker can never ship
/// a body pasted into some earlier draft.
#[test]
fn a_marker_stops_expanding_once_its_draft_is_gone() {
    let mut chat = chat();
    chat.paste(&stack_trace());
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.log.is_some(), "the first send expands the marker");

    for ch in "[Paste #1 · 8 lines]".chars() {
        chat.on_key(Key::Char(ch), Instant::now());
    }
    assert_eq!(chat.input, "[Paste #1 · 8 lines]");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        sent_text(&applied).as_deref(),
        Some("[Paste #1 · 8 lines]"),
        "an unregistered marker is literal text"
    );
}

/// A pasted body is sent exactly as it was pasted: a break and a tab are
/// part of the paste, so the wall that reaches the model is byte for byte
/// what the clipboard held (bar `\r\n`).
#[test]
fn a_collapsed_paste_keeps_its_tabs_and_lines() {
    let body = "fn main() {\n\tprintln!(\"a\");\n\tprintln!(\"b\");\n\tprintln!(\"c\");\n\tprintln!(\"d\");\n\tprintln!(\"e\");\r\n\tprintln!(\"f\");\r}\n";
    let mut chat = chat();
    chat.paste(body);
    assert_eq!(chat.input, "[Paste #1 · 8 lines]");
    let applied = chat.on_key(Key::Enter, Instant::now());
    let text = sent_text(&applied).expect("the prompt goes out");
    // Every `\r\n` and lone `\r` is one `\n`; the tabs are the paste's own.
    assert_eq!(text, body.replace("\r\n", "\n").replace('\r', "\n"));
}

/// The boundary the marker draws around `/`: a marker is one line with no
/// slash in it, so it is never a command token itself, and the completion
/// still works on the draft the composer returns to.
#[test]
fn the_slash_list_is_unaffected_by_a_marker() {
    let mut chat = chat();
    chat.paste(&stack_trace());
    assert!(
        !chat.input.contains('\n'),
        "a marker is one line: {:?}",
        chat.input
    );
    assert!(
        picker_rows(&chat).is_empty(),
        "a marker alone is not a slash token"
    );

    // Esc takes the draft — and the body behind the marker — away, and the
    // command list is what it always was.
    chat.on_key(Key::Esc, Instant::now());
    assert!(chat.input.is_empty(), "esc leaves an empty draft");
    for ch in "/comp".chars() {
        chat.on_key(Key::Char(ch), Instant::now());
    }
    let rows = picker_rows(&chat);
    assert!(
        rows.iter()
            .any(|row| matches!(row, PickRow::Command(command) if command.name == "compact")),
        "the command list still completes"
    );
    let frame = frame_text(&mut chat);
    assert!(frame.contains("/compact"), "{frame}");
    // Tab completes the token into the draft.
    chat.on_key(Key::Tab, Instant::now());
    assert_eq!(chat.input, "/compact ");
}

#[test]
fn a_late_ctrl_c_does_not_quit() {
    let mut chat = chat();
    let start = Instant::now();
    chat.on_key(Key::CtrlC, start);
    let later = chat.on_key(Key::CtrlC, start + Duration::from_secs(3));
    assert!(later.effect.is_none());
    assert!(chat.quit_armed.is_some());
}

#[test]
fn a_frame_shows_the_model_the_reply_and_the_composer() {
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = match ratatui::Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => panic!("test backend: {error}"),
    };
    let mut chat = chat();
    type_text(&mut chat, "hi");
    chat.on_key(Key::Enter, Instant::now());
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "hello".into(),
    });
    assert!(terminal.draw(|frame| draw(frame, &mut chat)).is_ok());
    let view: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(view.contains("titi"), "{view}");
    assert!(view.contains("openai/gpt-4.1"), "{view}");
    assert!(view.contains("you"), "{view}");
    assert!(view.contains("hi"), "{view}");
    assert!(view.contains("hello"), "{view}");
}

#[test]
fn a_local_png_is_placed_when_kitty_is_on() {
    let path = std::env::temp_dir().join(format!("titi-kitty-{}.png", std::process::id()));
    std::fs::write(&path, PNG_2X2).expect("write png");
    let mut chat = chat();
    chat.kitty = true;
    chat.push(LineKind::User, path.display().to_string());
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = match ratatui::Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => panic!("test backend: {error}"),
    };
    assert!(terminal.draw(|frame| draw(frame, &mut chat)).is_ok());
    let flush = chat.take_output_flush();
    assert!(flush.contains("f=32"), "{flush}");
    assert!(flush.contains("a=p,U=1"), "{flush}");
    let symbols: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(symbols.contains('\u{10EEEE}'), "{symbols}");
    assert!(terminal.draw(|frame| draw(frame, &mut chat)).is_ok());
    assert!(chat.take_output_flush().is_empty());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn goal_is_reserved_and_does_not_submit() {
    let mut chat = chat();
    type_text(&mut chat, "/goal fix the parser");
    let applied = chat.on_key(Key::Enter, Instant::now());
    match applied.effect {
        Some(ChatEffect::Send(EngineCommand::RunGoal { text })) => {
            assert_eq!(text.as_str(), "fix the parser");
        }
        other => panic!("expected RunGoal, got {other:?}"),
    }
    assert!(applied.log.is_none(), "a goal is not a user prompt");
    assert!(!chat.turn_active);
}

/// Cached input is cheaper input; /usage says how much of the prompt
/// the provider served from its cache, and says nothing when none was.
#[test]
fn usage_names_the_cached_share_of_the_prompt() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(1),
        prompt_tokens: 1_000,
        completion_tokens: 50,
        cached_tokens: 800,
        // An unpriced model: the footer states no money.
        cost_micro_usd: None,
    });
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(2),
        prompt_tokens: 1_200,
        completion_tokens: 40,
        cached_tokens: 1_000,
        // An unpriced model: the footer states no money.
        cost_micro_usd: None,
    });
    type_text(&mut chat, "/usage");
    chat.on_key(Key::Enter, Instant::now());
    let note = chat.lines.last().map(|line| line.text.clone());
    assert_eq!(
        note.as_deref(),
        Some("Turn: 1200 prompt (1000 cached) + 40 completion. Session: 2200 (1800 cached) / 90.")
    );
}

#[test]
fn usage_command_prints_tokens() {
    let mut chat = chat();
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(1),
        prompt_tokens: 100,
        completion_tokens: 50,
        cached_tokens: 0,
        // An unpriced model: the footer states no money.
        cost_micro_usd: None,
    });
    type_text(&mut chat, "/usage");
    chat.on_key(Key::Enter, Instant::now());
    let view = frame_text(&mut chat);
    assert!(
        view.contains("Turn: 100 prompt + 50 completion. Session: 100 / 50."),
        "View: {view}"
    );
    // An unpriced model has no total to state, and `$0.00` is not it.
    assert!(!view.contains("session total"), "View: {view}");
}

/// A priced model puts the session's cost next to its token totals,
/// rounded to cents.
#[test]
fn usage_states_the_session_cost_when_the_model_is_priced() {
    let mut chat = priced_chat();
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(1),
        prompt_tokens: 100_000,
        completion_tokens: 5_000,
        cached_tokens: 0,
        // The engine's figures, turn by turn: $0.375 and $0.0015.
        cost_micro_usd: Some(375_000),
    });
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(2),
        prompt_tokens: 1_200,
        completion_tokens: 40,
        cached_tokens: 1_000,
        cost_micro_usd: Some(1_500),
    });
    type_text(&mut chat, "/usage");
    chat.on_key(Key::Enter, Instant::now());
    // $0.375 for the first turn, $0.0015 for the second: $0.37650.
    assert_eq!(
        chat.lines.last().map(|line| line.text.clone()).as_deref(),
        Some(
            "Turn: 1200 prompt (1000 cached) + 40 completion. \
             Session: 101200 (1000 cached) / 5040 · session total $0.38."
        )
    );
}

/// A session that switched from a priced model to an unpriced one states
/// its total as a floor: the turns it could not price are named, not
/// silently dropped.
#[test]
fn usage_marks_a_total_that_leaves_turns_out() {
    let mut chat = priced_chat();
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(1),
        prompt_tokens: 100_000,
        completion_tokens: 5_000,
        cached_tokens: 0,
        // The engine's own figure: $0.375.
        cost_micro_usd: Some(375_000),
    });
    // The switch a `/model ollama/qwen3` makes: the next turn's report
    // carries no figure at all, because the engine cannot price it.
    chat.model = "ollama/qwen3".to_owned();
    chat.on_event(EngineEvent::TurnUsage {
        turn_id: TurnId(2),
        prompt_tokens: 500,
        completion_tokens: 20,
        cached_tokens: 0,
        cost_micro_usd: None,
    });
    type_text(&mut chat, "/usage");
    chat.on_key(Key::Enter, Instant::now());
    let said = chat
        .lines
        .last()
        .map(|line| line.text.clone())
        .unwrap_or_default();
    assert!(
        said.ends_with("· session total $0.38+ (unpriced turns excluded)."),
        "{said}"
    );
}

#[test]
fn goal_without_text_does_not_submit() {
    let mut chat = chat();
    type_text(&mut chat, "/goal");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("usage: /goal")),
        "{:?}",
        chat.lines
    );
}

#[test]
fn a_goal_report_lands_on_the_transcript() {
    let mut chat = chat();
    chat.on_event(EngineEvent::GoalFinished {
        report: "goal: passed · 1 round · verdict pass".into(),
    });
    assert!(chat.lines.iter().any(|line| line.text.contains("passed")));
    assert!(!chat.turn_active);
}

#[test]
fn council_is_reserved_and_does_not_submit() {
    let mut chat = chat();
    type_text(&mut chat, "/council do we rewrite the parser?");
    let applied = chat.on_key(Key::Enter, Instant::now());
    match applied.effect {
        Some(ChatEffect::Send(EngineCommand::RunCouncil { question })) => {
            assert_eq!(question.as_str(), "do we rewrite the parser?");
        }
        other => panic!("expected RunCouncil, got {other:?}"),
    }
    assert!(applied.log.is_none(), "a council is not a user prompt");
    assert!(!chat.turn_active);
}

#[test]
fn council_without_a_question_does_not_submit() {
    let mut chat = chat();
    type_text(&mut chat, "/council");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("usage: /council")),
        "{:?}",
        chat.lines
    );
}

#[test]
fn graph_is_reserved_and_does_not_submit() {
    let mut chat = chat();
    type_text(&mut chat, "/graph ship the release");
    let applied = chat.on_key(Key::Enter, Instant::now());
    match applied.effect {
        Some(ChatEffect::Send(EngineCommand::RunGraph { task })) => {
            assert_eq!(task.as_str(), "ship the release");
        }
        other => panic!("expected RunGraph, got {other:?}"),
    }
    assert!(applied.log.is_none(), "a graph is not a user prompt");
    assert!(!chat.turn_active);
}

#[test]
fn graph_without_a_task_does_not_submit() {
    let mut chat = chat();
    type_text(&mut chat, "/graph");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("usage: /graph")),
        "{:?}",
        chat.lines
    );
}

#[test]
fn a_graph_report_lands_on_the_transcript() {
    let mut chat = chat();
    chat.on_event(EngineEvent::GraphFinished {
        report: "graph: council → goal · verdict pass".into(),
    });
    assert!(chat.lines.iter().any(|line| line.text.contains("verdict")));
    assert!(!chat.turn_active);
}

/// `/git` answers on the spot: nothing goes to the engine, and the git
/// view lands in the transcript under the op that produced it.
#[test]
fn git_status_reports_in_the_transcript() {
    let mut chat = chat();
    type_text(&mut chat, "/git");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none(), "/git never reaches the engine");
    assert!(applied.log.is_none(), "/git is not a user prompt");
    assert!(!chat.turn_active);
    let last = chat.lines.last().expect("a transcript line");
    assert!(last.text.starts_with("git status"), "{:?}", chat.lines);
}

/// The bare `/git` and `/git status` are the same view.
#[test]
fn git_status_is_the_default_op() {
    let mut bare = chat();
    type_text(&mut bare, "/git");
    bare.on_key(Key::Enter, Instant::now());
    let mut named = chat();
    type_text(&mut named, "/git status");
    named.on_key(Key::Enter, Instant::now());
    assert_eq!(bare.lines.last(), named.lines.last());
}

/// A commit is a write, and writes stay behind the tool's approval
/// prompt. The slash command must not become the way around it.
#[test]
fn git_commit_is_refused_and_stays_tool_gated() {
    let mut chat = chat();
    type_text(&mut chat, "/git commit -m oops");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(!chat.turn_active);
    let last = chat.lines.last().expect("a transcript line");
    assert_eq!(last.kind, LineKind::Error);
    assert!(
        last.text.contains("status or diff") && last.text.contains("approval"),
        "{:?}",
        last
    );
}

#[test]
fn git_push_is_refused_too() {
    let mut chat = chat();
    type_text(&mut chat, "/git push");
    chat.on_key(Key::Enter, Instant::now());
    let last = chat.lines.last().expect("a transcript line");
    assert_eq!(last.kind, LineKind::Error);
    assert!(last.text.contains("status or diff"), "{:?}", last);
}

/// The block exists to be pasted into a public bug report, so a stored
/// key must not be anywhere in it.
#[test]
fn diagnose_summarises_and_never_prints_a_key() {
    let dir = tempfile::tempdir().expect("temp");
    crate::secrets::store_key(dir.path(), "openai", "sk-test").expect("store");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();
    type_text(&mut chat, "/diagnose");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(
        applied.effect.is_none(),
        "/diagnose never reaches the engine"
    );
    assert!(applied.log.is_none());
    let summary = chat.lines.last().expect("a transcript line");
    assert_eq!(summary.kind, LineKind::Note);
    for part in [
        "titi ",
        "model: openai/gpt-4.1",
        "session: session-123",
        "providers: ",
        "openai (",
        "config: ",
        "genome: ",
        "repo: ",
    ] {
        assert!(summary.text.contains(part), "{part} missing: {summary:?}");
    }
    assert!(
        !chat.lines.iter().any(|line| line.text.contains("sk-test")),
        "a stored key reached the diagnostics block: {:?}",
        chat.lines
    );
}

#[test]
fn diagnose_refuses_a_stray_argument() {
    let mut chat = chat();
    type_text(&mut chat, "/diagnose everything");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    let last = chat.lines.last().expect("a transcript line");
    assert_eq!(last.kind, LineKind::Error);
    assert!(last.text.contains("usage: /diagnose"), "{:?}", last);
}

/// 2×2 red PNG. Small enough to keep the kitty transmit in the test.
const PNG_2X2: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 6, 0,
    0, 0, 114, 182, 13, 36, 0, 0, 0, 17, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240, 31, 132,
    25, 96, 12, 0, 71, 202, 7, 249, 103, 89, 110, 183, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96,
    130,
];

#[test]
fn switch_exact_id() {
    let mut chat = chat();
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "anthropic/claude-opus-5".to_owned(),
        "openai/gpt-4.1".to_owned(),
    ]);
    type_text(&mut chat, "/switch anthropic/claude-opus-5");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "anthropic/claude-opus-5".into()
        }))
    );
    // The command sends the switch; the engine's event is what confirms
    // it, once.
    assert!(confirmations(&chat).is_empty(), "{:?}", chat.lines);
    assert_eq!(
        confirmations_after_switch(&mut chat, "anthropic/claude-opus-5"),
        ["model anthropic/claude-opus-5"]
    );
}

#[test]
fn switch_fuzzy_opus() {
    let mut chat = chat();
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "anthropic/claude-opus-5".to_owned(),
        "openai/gpt-4.1".to_owned(),
    ]);
    type_text(&mut chat, "/switch opus");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "anthropic/claude-opus-5".into()
        }))
    );
}

#[test]
fn switch_fuzzy_does_not_match_subsequences() {
    let mut chat = chat();
    chat.catalog =
        crate::engine::ModelCatalog::fixed(vec!["anthropic/claude-sonnet-4-5".to_owned()]);
    type_text(&mut chat, "/switch opus");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("no model matches \"opus\""))
    );
}

#[test]
fn switch_with_colon_id() {
    let mut chat = chat();
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "myco/llama3:8b".to_owned(),
        "openai/gpt-4.1".to_owned(),
    ]);
    type_text(&mut chat, "/switch myco/llama3:8b");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "myco/llama3:8b".into()
        }))
    );
}

#[test]
fn switch_with_level() {
    let mut chat = chat();
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "anthropic/claude-opus-5".to_owned(),
        "openai/gpt-4.1".to_owned(),
    ]);
    type_text(&mut chat, "/switch opus:high");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "anthropic/claude-opus-5:high".into()
        }))
    );
}

#[test]
fn switch_role_alias() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();
    chat.catalog = crate::engine::ModelCatalog::fixed(vec!["anthropic/claude-opus-5".to_owned()]);
    std::fs::create_dir_all(&chat.agent_dir).unwrap();
    std::fs::write(
        chat.agent_dir.join("config.yml"),
        "modelRoles:\n  review: anthropic/claude-opus-5\n",
    )
    .unwrap();

    type_text(&mut chat, "/switch @review");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::SwitchModel {
            model: "anthropic/claude-opus-5".into()
        }))
    );
}

#[test]
fn switch_role_without_model_roles_fails() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();
    chat.catalog = crate::engine::ModelCatalog::fixed(vec!["anthropic/claude-opus-5".to_owned()]);
    std::fs::create_dir_all(&chat.agent_dir).unwrap();

    type_text(&mut chat, "/switch @review");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("no model roles configured"))
    );
}

#[test]
fn switch_role_unknown_fails() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();
    chat.catalog = crate::engine::ModelCatalog::fixed(vec!["anthropic/claude-opus-5".to_owned()]);
    std::fs::create_dir_all(&chat.agent_dir).unwrap();
    std::fs::write(
        chat.agent_dir.join("config.yml"),
        "modelRoles:\n  review: anthropic/claude-opus-5\n",
    )
    .unwrap();

    type_text(&mut chat, "/switch @unknown");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("no such role @unknown"))
    );
}

/// `/genome off` writes a false `genome.enabled` on the agent's own
/// config, and a following `/genome` note reads its own file back: the
/// note is the file's word, not the input echoed.
#[test]
fn genome_off_persists_and_the_next_note_reads_it_back() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(&chat.agent_dir).unwrap();

    type_text(&mut chat, "/genome off");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(chat.lines.iter().any(|line| line.text == "genome: off"));
    let config = std::fs::read_to_string(chat.agent_dir.join("config.yml")).unwrap();
    assert_eq!(config, "genome:\n  enabled: false\n");

    chat.lines.clear();
    type_text(&mut chat, "/genome");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    let note = chat
        .lines
        .iter()
        .find(|line| line.text.contains("genome: off"))
        .expect("the status note names the off state");
    assert!(note.text.contains("reason: setting"), "{}", note.text);
    assert!(note.text.contains("limit: 24"), "{}", note.text);
}

/// An in-range `/genome limit` persists on its own key and the note
/// reports it; the limit is not coupled to the enabled state.
#[test]
fn genome_limit_persists_and_is_reported() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(&chat.agent_dir).unwrap();

    type_text(&mut chat, "/genome limit 4");
    chat.on_key(Key::Enter, Instant::now());
    let config = std::fs::read_to_string(chat.agent_dir.join("config.yml")).unwrap();
    assert!(config.contains("limit: 4"), "{config}");
    assert!(!config.contains("enabled"), "{config}");

    chat.lines.clear();
    type_text(&mut chat, "/genome");
    chat.on_key(Key::Enter, Instant::now());
    let note = chat
        .lines
        .iter()
        .find(|line| line.text.contains("limit: 4"))
        .expect("the note states the saved cap");
    assert!(note.text.contains("genome: on"), "{}", note.text);
}

/// An out-of-range `/genome limit` writes nothing and says the range.
#[test]
fn genome_limit_out_of_range_is_refused_without_a_write() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(&chat.agent_dir).unwrap();

    type_text(&mut chat, "/genome limit 0");
    chat.on_key(Key::Enter, Instant::now());
    assert!(
        !chat.agent_dir.join("config.yml").exists(),
        "nothing was written"
    );
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "genome limit: expected an integer from 1 to 64")
    );
}

/// `/genome check` runs the real index over the workspace and pushes the
/// diagnostic lines as a note; a broken import names itself with its code.
#[test]
fn genome_check_reports_diagnostics_from_the_workspace() {
    let dir = tempfile::tempdir().expect("temp");
    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("lib.rs"),
        "use crate::missing::Thing;\npub fn present() {}\n",
    )
    .unwrap();
    let agent = tempfile::tempdir().expect("temp agent");
    let mut chat = chat();
    chat.agent_dir = agent.path().to_path_buf();
    std::fs::create_dir_all(&chat.agent_dir).unwrap();

    // The workspace comes in as a value: the test hands it the temp tree
    // directly instead of moving the process `current_dir`, which every
    // parallel test reads.
    local_genome_note(&mut chat, "check", dir.path());
    let names: Vec<&str> = chat.lines.iter().map(|line| line.text.as_str()).collect();
    let hit = names
        .iter()
        .find(|text| text.contains("unresolved-import"))
        .expect("the broken import names itself");
    assert!(hit.contains("missing"), "{names:?}");
    assert!(hit.contains("src/lib.rs:1:"), "{names:?}");
    assert!(
        hit.starts_with("src/lib.rs:1: unresolved-import: "),
        "{names:?}"
    );
}

/// `/genome lsp` never starts a stdio server inside the chat: the note
/// names the terminal command, exactly, instead of pretending.
#[test]
fn genome_lsp_names_the_terminal_command() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(&chat.agent_dir).unwrap();

    type_text(&mut chat, "/genome lsp");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "genome: lsp is 'titi genome lsp', not a chat command"),
        "{:?}",
        chat.lines
    );
}

/// An unknown `/genome` word names itself and shows the usage line.
#[test]
fn genome_unknown_subcommand_shows_usage() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(&chat.agent_dir).unwrap();

    type_text(&mut chat, "/genome wat");
    chat.on_key(Key::Enter, Instant::now());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "genome: unknown command wat")
    );
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "usage: titi genome [on|off|limit <n>|check|lsp]")
    );
}

#[test]
fn switch_multiple_matches_prints_candidates() {
    let mut chat = chat();
    chat.catalog = crate::engine::ModelCatalog::fixed(vec![
        "anthropic/claude-opus-5".to_owned(),
        "aws/claude-opus-5".to_owned(),
    ]);
    type_text(&mut chat, "/switch opus");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());

    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("multiple models match"))
    );
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("anthropic/claude-opus-5"))
    );
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("aws/claude-opus-5"))
    );
}

#[test]
fn memory_list() {
    let mut chat = chat();
    type_text(&mut chat, "/memory list");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::MemoryList))
    );

    type_text(&mut chat, "/memory");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::MemoryList))
    );
}

#[test]
fn memory_search() {
    let mut chat = chat();
    type_text(&mut chat, "/memory search rust");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::MemorySearch {
            query: "rust".into()
        }))
    );
}

#[test]
fn memory_forget() {
    let mut chat = chat();
    type_text(&mut chat, "/memory forget 42");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        applied.effect,
        Some(ChatEffect::Send(EngineCommand::MemoryForget { id: 42 }))
    );

    type_text(&mut chat, "/memory forget not_an_id");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("usage: /memory forget <id>"))
    );
}

#[test]
fn memory_result_prints_note() {
    let mut chat = chat();
    chat.on_event(EngineEvent::MemoryResult {
        output: "memory data".into(),
    });
    let view = frame_text(&mut chat);
    assert!(view.contains("memory data"), "{view}");
}

#[test]
fn session_named_updates_label() {
    let mut chat = chat();
    chat.on_event(EngineEvent::SessionNamed {
        session_id: "session-123".into(),
        title: "blue-otter".into(),
    });
    assert_eq!(chat.session_label, "blue-otter");
}

/// Types a line and presses Enter: the shape every command test uses.
fn command(chat: &mut Chat, line: &str) -> Applied {
    type_text(chat, line);
    chat.on_key(Key::Enter, Instant::now())
}

/// `/details` reaches the four named sections the deleted `transcript.rs`
/// carried: expanded draws a section's lines, collapsed stands them up as
/// one counted row, hidden draws neither.
#[test]
fn details_sets_a_sections_visibility() {
    let mut chat = chat();
    chat.push(LineKind::Tool, "tool done  read src/main.rs".to_owned());
    chat.push(LineKind::Tool, "tool done  write src/lib.rs".to_owned());
    assert!(
        frame_text(&mut chat).contains("read src/main.rs"),
        "tools start expanded, as the old module's DoD had them"
    );

    command(&mut chat, "/details tools collapsed");
    let frame = frame_text(&mut chat);
    assert!(frame.contains("▸ tools (2)"), "one counted row: {frame}");
    assert!(
        !frame.contains("read src/main.rs"),
        "and the lines are behind it: {frame}"
    );
    assert!(
        frame.contains("details: tools collapsed"),
        "the command answers with the mode the next frame draws: {frame}"
    );

    command(&mut chat, "/details tools hidden");
    let frame = frame_text(&mut chat);
    assert!(
        !frame.contains("tools (2)") && !frame.contains("read src/main.rs"),
        "neither the header nor the lines: {frame}"
    );

    command(&mut chat, "/details tools expanded");
    let frame = frame_text(&mut chat);
    assert!(frame.contains("read src/main.rs"), "{frame}");
    assert!(
        !frame.contains("tools (2)"),
        "no header when expanded: {frame}"
    );
}

/// The conversation is not a section: no `/details` word takes the user's
/// own question or the answer off the screen.
#[test]
fn details_never_hides_the_conversation() {
    let mut chat = chat();
    chat.push(LineKind::User, "a question that stays".to_owned());
    chat.push(LineKind::Assistant, "an answer that stays".to_owned());
    for word in ["hidden", "collapsed", "cycle"] {
        command(&mut chat, &format!("/details {word}"));
        let frame = frame_text(&mut chat);
        assert!(frame.contains("a question that stays"), "{word}: {frame}");
        assert!(frame.contains("an answer that stays"), "{word}: {frame}");
    }
}

/// Bare `/details` is the only way to read the state back, so it lists
/// every section and the mode it is on.
#[test]
fn details_lists_every_section_when_bare() {
    let mut chat = chat();
    command(&mut chat, "/details");
    let frame = frame_text(&mut chat);
    for needle in [
        "thinking expanded",
        "tools expanded",
        "subagents collapsed",
        "activity expanded",
        "folded collapsed",
    ] {
        assert!(frame.contains(needle), "{needle} is missing: {frame}");
    }
}

/// The defaults are the old module's (`SectionVisibility::default`) with
/// one departure — activity — because on this surface those lines are the
/// answers `/usage`, `/context` and `/jobs` give.
#[test]
fn the_default_visibility_follows_the_old_module() {
    let chat = chat();
    assert_eq!(chat.details.mode("thinking"), Some(SectionMode::Expanded));
    assert_eq!(chat.details.mode("tools"), Some(SectionMode::Expanded));
    assert_eq!(chat.details.mode("subagents"), Some(SectionMode::Collapsed));
    assert_eq!(chat.details.mode("activity"), Some(SectionMode::Expanded));
    assert_eq!(chat.details.mode("folded"), Some(SectionMode::Collapsed));
    assert_eq!(chat.details.mode("bogus"), None);
}

/// `cycle` walks the three modes the old `SectionVisibility::apply` walked
/// (hidden → collapsed → expanded), and a word that is not a mode leaves
/// the state where it was.
#[test]
fn details_cycles_and_refuses_what_it_cannot_read() {
    let mut chat = chat();
    command(&mut chat, "/details tools cycle");
    assert_eq!(chat.details.mode("tools"), Some(SectionMode::Hidden));
    command(&mut chat, "/details tools cycle");
    assert_eq!(chat.details.mode("tools"), Some(SectionMode::Collapsed));
    command(&mut chat, "/details tools cycle");
    assert_eq!(chat.details.mode("tools"), Some(SectionMode::Expanded));

    command(&mut chat, "/details bogus expanded");
    let frame = frame_text(&mut chat);
    assert!(frame.contains("no such section or mode"), "{frame}");
    assert_eq!(chat.details.mode("tools"), Some(SectionMode::Expanded));
}

/// Subagent chatter is the section the default folds, and the header says
/// how much is behind it.
#[test]
fn subagents_collapse_behind_their_header_by_default() {
    let mut chat = chat();
    chat.on_event(EngineEvent::AgentStarted {
        agent_id: "agent-1".into(),
        name: "worker".into(),
        parent_id: None,
        kind: titi_engine::protocol::AgentKind::Subagent,
    });
    let frame = frame_text(&mut chat);
    assert!(frame.contains("▸ subagents (1)"), "{frame}");
    assert!(!frame.contains("agent worker: started"), "{frame}");

    command(&mut chat, "/details subagents expanded");
    let frame = frame_text(&mut chat);
    assert!(frame.contains("agent worker: started"), "{frame}");
}

/// Reasoning is a section like any other: the live row is drawn while
/// thinking is expanded and gone when it is hidden.
#[test]
fn the_reasoning_row_is_the_thinking_section() {
    let mut chat = chat();
    chat.on_event(EngineEvent::ThinkingDelta {
        turn_id: titi_engine::TurnId(1),
        text: "weighing the options".into(),
    });
    assert!(
        frame_text(&mut chat).contains("weighing the options"),
        "reasoning is on screen while it is the newest thing"
    );
    command(&mut chat, "/details thinking hidden");
    let frame = frame_text(&mut chat);
    assert!(!frame.contains("weighing the options"), "{frame}");
}

/// A compaction leaves a divider and takes the history it folded off the
/// screen: the point of the fold is a short transcript, not a note.
#[test]
fn a_compaction_folds_the_history_behind_a_divider() {
    let mut chat = chat();
    chat.push(LineKind::User, "the question that was folded".to_owned());
    chat.push(LineKind::Assistant, "the answer that was folded".to_owned());
    chat.on_event(EngineEvent::Compacted {
        turn_id: titi_engine::TurnId(1),
        folded: 14,
        tokens_before: 22_000,
        strategy: "digest".into(),
    });

    let frame = frame_text(&mut chat);
    assert!(frame.contains("▸ folded 14 turns · 22k tokens"), "{frame}");
    assert!(
        !frame.contains("the question that was folded"),
        "the folded history is not on screen: {frame}"
    );

    command(&mut chat, "/details folded expanded");
    let frame = frame_text(&mut chat);
    assert!(frame.contains("▾ folded 14 turns · 22k tokens"), "{frame}");
    assert!(
        frame.contains("the question that was folded"),
        "expanding the fold brings the history back: {frame}"
    );

    command(&mut chat, "/details folded hidden");
    let frame = frame_text(&mut chat);
    assert!(!frame.contains("folded 14 turns"), "{frame}");
    assert!(!frame.contains("the question that was folded"), "{frame}");
}

/// The divider is furniture: the dim chip the theme gives a note's mark,
/// with the chevron the fold's mode decides.
#[test]
fn the_fold_divider_is_drawn_from_the_payload() {
    let mut chat = chat();
    chat.on_event(EngineEvent::Compacted {
        turn_id: titi_engine::TurnId(3),
        folded: 2,
        tokens_before: 900,
        strategy: "digest".into(),
    });
    assert_eq!(fold_divider_label(2, 900), "folded 2 turns · 900 tokens");

    let rows = frame_rows(&mut chat, 80, 24);
    let (x, y) = cell_of(&rows, "▸ folded 2 turns").expect("the divider is on screen");
    let buffer = frame_buffer(&mut chat, 80, 24);
    assert_eq!(
        buffer[(x, y)].fg,
        fg(&test_theme(), ThemeColor::Dim)
            .fg
            .unwrap_or(Color::Reset),
        "the divider is drawn as the furniture it is"
    );

    command(&mut chat, "/details folded expanded");
    let rows = frame_rows(&mut chat, 80, 24);
    assert!(
        rows.iter().any(|row| row.contains("▾ folded 2 turns")),
        "an expanded fold opens its chevron: {rows:?}"
    );
}

/// Two compactions keep their own numbers: each divider is the payload of
/// its own event, and only the newest one stands for history that is still
/// on the transcript.
#[test]
fn each_fold_divider_carries_its_own_event() {
    let mut chat = chat();
    chat.push(LineKind::User, "the oldest question".to_owned());
    chat.on_event(EngineEvent::Compacted {
        turn_id: titi_engine::TurnId(1),
        folded: 3,
        tokens_before: 1_500,
        strategy: "digest".into(),
    });
    chat.push(LineKind::Assistant, "an answer between folds".to_owned());
    chat.on_event(EngineEvent::Compacted {
        turn_id: titi_engine::TurnId(2),
        folded: 40,
        tokens_before: 120_000,
        strategy: "digest".into(),
    });

    let frame = frame_text(&mut chat);
    assert!(frame.contains("▸ folded 40 turns · 120k tokens"), "{frame}");
    assert!(!frame.contains("folded 3 turns"), "{frame}");

    command(&mut chat, "/details folded expanded");
    let frame = frame_text(&mut chat);
    assert!(frame.contains("folded 3 turns · 1.5k tokens"), "{frame}");
    assert!(frame.contains("folded 40 turns · 120k tokens"), "{frame}");
    assert!(frame.contains("the oldest question"), "{frame}");
}

#[test]
fn agent_events_produce_transcript_lines() {
    let mut chat = chat();
    chat.on_event(EngineEvent::AgentStarted {
        agent_id: "agent-1".into(),
        name: "worker".into(),
        parent_id: None,
        kind: titi_engine::protocol::AgentKind::Subagent,
    });
    assert!(
        chat.lines
            .last()
            .unwrap()
            .text
            .contains("agent worker: started")
    );

    chat.on_event(EngineEvent::AgentFinished {
        agent_id: "agent-1".into(),
        summary: "all done".into(),
        success: true,
    });
    assert!(
        chat.lines
            .last()
            .unwrap()
            .text
            .contains("agent agent-1: all done")
    );
    // Its own kind, not `Tool`: subagent chatter is a section `/details`
    // can fold away, and a section needs lines it can tell apart.
    assert_eq!(chat.lines.last().unwrap().kind, LineKind::Agent);
}

#[test]
fn btw_sends_without_log() {
    let mut chat = chat();
    type_text(&mut chat, "/btw hello there");
    let applied = chat.on_key(Key::Enter, Instant::now());
    match applied.effect {
        Some(ChatEffect::Send(EngineCommand::SubmitPrompt { text })) => {
            assert_eq!(text.as_str(), "btw: hello there");
        }
        _ => panic!("expected SubmitPrompt"),
    }
    assert!(applied.log.is_none());
}

/// Every physical line of a note is its own row, and a row too long for
/// the pane wraps instead of being cut: `/diagnose`, `/git diff` and
/// `/settings` each push one multi-line note, and a single truncated row
/// threw everything past the first screen width away.
#[test]
fn a_multi_line_note_is_one_row_per_line() {
    let theme = test_theme();
    let line = TranscriptLine {
        kind: LineKind::Note,
        text: "alpha\nbeta\n\ngamma".to_owned(),
    };
    let rows = row_texts(&message_rows(&line, 40, &theme, false).0);
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert!(rows[0].contains("alpha"), "{rows:?}");
    assert!(rows[1].contains("beta"), "{rows:?}");
    assert!(rows[2].trim().is_empty(), "blank line kept: {rows:?}");
    assert!(rows[3].contains("gamma"), "{rows:?}");
}

/// A long line is wrapped over several rows, and no row overflows the
/// pane — the old renderer dropped the tail instead.
#[test]
fn a_long_note_wraps_within_the_width() {
    let theme = test_theme();
    let words = std::iter::repeat_n("token", 60)
        .collect::<Vec<_>>()
        .join(" ");
    let line = TranscriptLine {
        kind: LineKind::Note,
        text: words.clone(),
    };
    let rows = row_texts(&message_rows(&line, 40, &theme, false).0);
    assert!(rows.len() >= 8, "{rows:?}");
    for row in &rows {
        assert!(
            titi_tui::width::visible_width(row) <= 40,
            "row wider than the pane: {row:?}"
        );
    }
    let joined: String = rows
        .iter()
        .map(|row| row.trim())
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        joined.split_whitespace().filter(|w| *w == "token").count(),
        60
    );
}

/// An error and a tool chip split the same way a note does.
#[test]
fn errors_and_tool_chips_split_too() {
    let theme = test_theme();
    for kind in [LineKind::Error, LineKind::Tool] {
        let line = TranscriptLine {
            kind,
            text: "first\nsecond".to_owned(),
        };
        let rows = row_texts(&message_rows(&line, 40, &theme, false).0);
        assert_eq!(rows.len(), 2, "{kind:?}: {rows:?}");
        assert!(rows[1].contains("second"), "{kind:?}: {rows:?}");
    }
}

fn assistant_line(text: &str) -> TranscriptLine {
    TranscriptLine {
        kind: LineKind::Assistant,
        text: text.to_owned(),
    }
}

/// A frame's buffer, for a test that has to read a cell's own style.
fn frame_buffer(chat: &mut Chat, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = match ratatui::Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => panic!("test backend: {error}"),
    };
    assert!(terminal.draw(|frame| draw(frame, chat)).is_ok());
    terminal.backend().buffer().clone()
}

/// Where the first character of `needle` sits in a frame given as rows of
/// symbols: the column is the characters before it, since a frame row
/// carries no escapes.
fn cell_of(rows: &[String], needle: &str) -> Option<(u16, u16)> {
    rows.iter().enumerate().find_map(|(y, row)| {
        row.find(needle)
            .map(|at| (row[..at].chars().count() as u16, y as u16))
    })
}

/// The span that holds `needle`, so a test can read a row's styling.
fn span_with<'a>(rows: &'a [Line<'static>], needle: &str) -> &'a Span<'static> {
    rows.iter()
        .flat_map(|row| row.spans.iter())
        .find(|span| span.content.contains(needle))
        .unwrap_or_else(|| panic!("no span holds {needle:?}"))
}

fn reply_rows_of(text: &str, width: usize) -> Vec<Line<'static>> {
    message_rows(&assistant_line(text), width, &test_theme(), false).0
}

/// A heading is rendered as the theme's heading, and its `#` run is syntax:
/// no hash reaches the screen. Level 2 carries bold, which the renderer
/// emits as one combined escape and the frame has to read back as a style.
#[test]
fn a_heading_in_a_reply_is_styled_without_its_hashes() {
    let theme = test_theme();
    let rows = reply_rows_of("## What is here", 80);
    let texts = row_texts(&rows);
    assert!(
        texts.iter().all(|row| !row.contains('#')),
        "a hash reached the screen: {texts:?}"
    );
    let heading = span_with(&rows, "What is here");
    assert_eq!(heading.style.fg, fg(&theme, ThemeColor::MdHeading).fg);
    assert!(
        heading.style.has_modifier(Modifier::BOLD),
        "level 2 has no bold: {:?}",
        heading.style
    );
    // The label and the gutter are the plain block's, unchanged.
    assert!(texts[0].starts_with("  titi │ "), "{texts:?}");
    assert_eq!(
        span_with(&rows, "titi").style,
        fg(&theme, ThemeColor::Accent).add_modifier(Modifier::BOLD)
    );
}

/// A fenced block is drawn as the renderer's box, with the language named
/// once on the top rule; the fence itself never reaches the screen, and the
/// box is narrower than the pane the reply is drawn in.
#[test]
fn a_fenced_block_in_a_reply_is_a_box() {
    let theme = test_theme();
    let reply = "before\n\n```rust\nlet plan = 1;\n```\n\nafter";
    let rows = reply_rows_of(reply, 80);
    let texts = row_texts(&rows);
    assert!(
        texts.iter().all(|row| !row.contains("```")),
        "a fence reached the screen: {texts:?}"
    );
    let top = texts
        .iter()
        .find(|row| row.contains("╭"))
        .unwrap_or_else(|| panic!("no box in {texts:?}"));
    assert!(top.contains(" rust "), "the language is not named: {top:?}");
    assert_eq!(
        texts.iter().filter(|row| row.contains(" rust ")).count(),
        1,
        "the language is named more than once: {texts:?}"
    );
    assert_eq!(
        texts.iter().filter(|row| row.contains('╰')).count(),
        1,
        "the box is not closed: {texts:?}"
    );
    assert!(texts.iter().any(|row| row.contains("let plan = 1;")));
    assert_eq!(
        span_with(&rows, "let plan = 1;").style.fg,
        fg(&theme, ThemeColor::MdCodeBlock).fg
    );
    assert_eq!(
        span_with(&rows, "╭").style.fg,
        fg(&theme, ThemeColor::MdCodeBlockBorder).fg
    );
}

/// A reply that is not markdown is the plain block, byte for byte: the same
/// spans, in the same styles, as the rendering the screen had before the
/// renderer existed.
#[test]
fn a_plain_reply_is_the_block_it_always_was() {
    let theme = test_theme();
    let rows = reply_rows_of("done", 80);
    assert_eq!(
        rows,
        vec![Line::from(vec![
            Span::styled("  ", page(&theme)),
            Span::styled(
                "titi",
                fg(&theme, ThemeColor::Accent).add_modifier(Modifier::BOLD)
            ),
            Span::styled(" │ ", fg(&theme, ThemeColor::Accent)),
            Span::styled("done", fg(&theme, ThemeColor::Text)),
        ])]
    );
    // A long plain answer wraps exactly as `speech` wraps it, row for row.
    let long = "word ".repeat(40);
    assert_eq!(
        reply_rows_of(&long, 60),
        speech(
            "titi",
            ThemeColor::Accent,
            ThemeColor::Text,
            Surface::Page,
            &long,
            60,
            &theme
        )
    );
    // Arithmetic and identifiers are not emphasis.
    for plain in ["2 * 3 * 4", "the snake_case_name field", "a_trailing_"] {
        assert_eq!(
            reply_rows_of(plain, 80),
            speech(
                "titi",
                ThemeColor::Accent,
                ThemeColor::Text,
                Surface::Page,
                plain,
                80,
                &theme
            ),
            "{plain:?}"
        );
    }
}

/// Inside a markdown reply, a styled run ends where its marker does: the
/// text after `**bold**` carries the pane's own colour, not the bold run's.
#[test]
fn a_bold_run_ends_where_its_marker_does() {
    let rows = reply_rows_of("**bold** and plain", 80);
    assert_eq!(row_texts(&rows)[0], "  titi │ bold and plain");
    let bold = span_with(&rows, "bold");
    assert!(bold.style.has_modifier(Modifier::BOLD), "{:?}", bold.style);
    assert_eq!(bold.style.fg, None, "bold carries no colour of its own");
    let plain = span_with(&rows, "and plain");
    assert!(
        !plain.style.has_modifier(Modifier::BOLD),
        "the reset was lost: {:?}",
        plain.style
    );
}

/// A markdown reply is drawn inside the pane at every width, and its body
/// starts in the same column a plain reply's body does.
#[test]
fn a_markdown_reply_stays_inside_the_pane() {
    let reply = "## Title\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n\n> quoted\n\nA paragraph that is long enough to wrap more than once at a narrow width, so the wrap has to be measured against the pane the row is drawn in.";
    for width in [60usize, 80, 120] {
        let rows = reply_rows_of(reply, width);
        let texts = row_texts(&rows);
        assert!(texts.len() > 4, "{width}: {texts:?}");
        for (at, row) in texts.iter().enumerate() {
            assert!(
                titi_tui::width::visible_width(row) <= width,
                "{width}: row {at} is {} wide: {row:?}",
                titi_tui::width::visible_width(row)
            );
        }
        assert!(texts[0].starts_with("  titi │ Title"), "{width}: {texts:?}");
        for row in &texts[1..] {
            assert!(
                row.starts_with("       │ ") || row.trim().is_empty(),
                "{width}: a continuation row lost the gutter: {row:?}"
            );
        }
    }
}

/// The frame itself carries the reply's styling: the cells under a heading,
/// a code body and a list bullet hold the tokens the renderer assigned them,
/// and no syntax character reaches the screen.
#[test]
fn a_frame_draws_the_reply_with_the_renderers_tokens() {
    let theme = test_theme();
    let mut chat = chat();
    let reply = "## Title\n\n- one\n\n```rust\nlet plan = 1;\n```";
    chat.push(LineKind::Assistant, reply.to_owned());
    let width = 80u16;
    let height = 24u16;
    let buffer = frame_buffer(&mut chat, width, height);
    let symbols: Vec<String> = (0..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    let text = symbols.join("\n");
    for syntax in ["##", "```", "- one"] {
        assert!(
            !text.contains(syntax),
            "{syntax:?} reached the screen:\n{text}"
        );
    }
    assert!(text.contains("  titi │ Title"), "{text}");
    assert!(text.contains("let plan = 1;"), "{text}");

    // Where a cell of a named run sits, it carries that run's token.
    for (needle, token) in [
        ("Title", ThemeColor::MdHeading),
        ("let plan = 1;", ThemeColor::MdCodeBlock),
        ("•", ThemeColor::MdListBullet),
    ] {
        let (x, y) = cell_of(&symbols, needle)
            .unwrap_or_else(|| panic!("{needle:?} is not on screen:\n{text}"));
        assert_eq!(
            buffer[(x, y)].fg,
            fg(&theme, token).fg.unwrap_or(Color::Reset),
            "{needle:?} is not drawn in {token:?}"
        );
    }
}

/// The rows kept for the newest reply are only used for the reply they were
/// rendered from, at the width they were rendered for: an answer of the
/// same length, and the same answer in a wider pane, are both re-rendered.
#[test]
fn a_kept_reply_is_not_re_used_for_another_answer_or_width() {
    let mut chat = chat();
    let theme = test_theme();
    let first = chat.assistant_rows(true, "# alpha", 40, &theme);
    let same = chat.assistant_rows(true, "# alpha", 40, &theme);
    assert_eq!(first, same, "the reply was rendered differently");
    let other = chat.assistant_rows(true, "# bravo", 40, &theme);
    assert_ne!(first, other, "another reply re-used the kept rows");
    assert!(row_texts(&other).iter().any(|row| row.contains("bravo")));

    // A wider pane wraps the same answer into fewer rows, so a pane that
    // changed width cannot have been served from the kept rows.
    let long = "word ".repeat(40);
    let long = long.trim();
    let narrow = chat.assistant_rows(true, long, 40, &theme);
    let wide = chat.assistant_rows(true, long, 80, &theme);
    assert_ne!(narrow, wide, "a wider pane re-used the kept rows");
    assert!(
        narrow.len() > wide.len(),
        "the narrower pane wrapped into fewer rows: {} vs {}",
        narrow.len(),
        wide.len()
    );
}

/// An `edit` result the tool reports with a diff becomes a diff line: the
/// chip row names the file, and the rows under it are the renderer's, in
/// the diff tokens.
#[test]
fn an_edit_result_is_drawn_as_its_file_and_its_diff() {
    let theme = test_theme();
    let mut chat = chat();
    chat.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        name: "edit".into(),
        detail: None,
    });
    chat.on_event(EngineEvent::ToolFinished {
        turn_id: TurnId(1),
        call_id: "c1".into(),
        output: "edited".into(),
        is_error: false,
        detail: Some(edit_result().into()),
    });
    let line = chat.transcript().last().expect("a line").clone();
    assert_eq!(line.kind, LineKind::Diff, "{:?}", line);
    assert_eq!(
        line.text,
        edit_result(),
        "the diff line carries the detail, and only the detail"
    );

    let rows = message_rows(&line, 80, &theme, false).0;
    let texts = row_texts(&rows);
    assert!(texts[0].contains("notes/kept.txt"), "{texts:?}");
    assert_eq!(texts[0], "   ✓ notes/kept.txt", "{texts:?}");
    assert!(
        texts.iter().any(|row| row.contains("removed_line")),
        "{texts:?}"
    );
    assert!(
        texts.iter().any(|row| row.contains("added_line")),
        "{texts:?}"
    );
    assert!(
        texts.iter().any(|row| row.contains("@@ -1,3 +1,3 @@")),
        "the hunk header is drawn: {texts:?}"
    );
    assert!(
        !texts
            .iter()
            .any(|row| row.contains("+++ ") || row.contains("--- ")),
        "the file headers are the chip's job: {texts:?}"
    );

    // The frame's own cells carry the diff tokens.
    let buffer = frame_buffer(&mut chat, 80, 24);
    let symbols: Vec<String> = (0..24)
        .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    for (needle, token) in [
        ("removed_line", ThemeColor::ToolDiffRemoved),
        ("added_line", ThemeColor::ToolDiffAdded),
        ("line one", ThemeColor::ToolDiffContext),
    ] {
        let (x, y) =
            cell_of(&symbols, needle).unwrap_or_else(|| panic!("{needle:?} is not on screen"));
        assert_eq!(
            buffer[(x, y)].fg,
            fg(&theme, token).fg.unwrap_or(Color::Reset),
            "{needle:?} is not drawn in {token:?}"
        );
    }
}

/// The diff an edit reports as its detail, and the tests draw.
fn edit_result() -> &'static str {
    "--- a/notes/kept.txt\n\
     +++ b/notes/kept.txt\n\
     @@ -1,3 +1,3 @@\n line one\n-removed_line\n+added_line\n line three\n"
}

/// The detail decides, not the tool's name or the shape of the answer: a
/// result that carries one is a diff line even from a tool nobody expects a
/// diff from, and an `edit` that reports none — or an answer that merely
/// looks like a diff — stays the chip it always was.
#[test]
fn only_a_result_with_a_detail_becomes_a_diff_line() {
    let cases: [(&str, &str, Option<&str>, bool); 4] = [
        (
            "read",
            "the file\n-removed_line\n+added_line\n",
            None,
            false,
        ),
        ("edit", "edited", None, false),
        ("edit", "edited", Some(edit_result()), true),
        (
            "write",
            "wrote out.txt",
            Some("--- /dev/null\n+++ b/out.txt\n@@ -0,0 +1,1 @@\n+one\n"),
            true,
        ),
    ];
    for (tool, output, detail, expected) in cases {
        let mut chat = chat();
        chat.on_event(EngineEvent::ToolStarted {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            name: tool.into(),
            detail: None,
        });
        chat.on_event(EngineEvent::ToolFinished {
            turn_id: TurnId(1),
            call_id: "c1".into(),
            output: output.into(),
            is_error: false,
            detail: detail.map(Into::into),
        });
        let line = chat.transcript().last().expect("a line");
        assert_eq!(
            line.kind == LineKind::Diff,
            expected,
            "{tool}: {:?}",
            line.kind
        );
        if let Some(detail) = detail
            && expected
        {
            assert_eq!(line.text, detail, "{tool}: the detail is the line");
        }
        // The chip the screen draws when there is no detail keeps saying
        // what it always said.
        if !expected {
            let rows = message_rows(line, 80, &test_theme(), false).0;
            assert!(
                row_texts(&rows)[0].starts_with("   ✓ "),
                "{tool}: {:?}",
                row_texts(&rows)
            );
        }
    }
}

/// A tool result that is not a diff is the chip it has always been, byte
/// for byte: the same spans, in the same styles.
#[test]
fn a_result_without_a_diff_is_the_chip_it_always_was() {
    let theme = test_theme();
    let rows = message_rows(
        &TranscriptLine {
            kind: LineKind::Tool,
            text: "tool done  read".to_owned(),
        },
        80,
        &theme,
        false,
    )
    .0;
    assert_eq!(
        rows,
        vec![Line::from(vec![
            Span::styled("   ", page(&theme)),
            Span::styled("✓", fg(&theme, ThemeColor::Success)),
            Span::styled(" ", page(&theme)),
            Span::styled("read", fg(&theme, ThemeColor::Muted)),
        ])]
    );
}

/// A diff line the renderer cannot read as a diff falls back to the plain
/// chip: the result is still on screen, line by line, and nothing is
/// invented.
#[test]
fn a_diff_line_that_is_not_a_diff_is_the_plain_chip() {
    let theme = test_theme();
    let line = TranscriptLine {
        kind: LineKind::Diff,
        text: "not a diff at all\nsecond line".to_owned(),
    };
    let rows = message_rows(&line, 80, &theme, false).0;
    assert_eq!(
        row_texts(&rows)[..2],
        ["   ✓ not a diff at all", "     second line"],
        "each line of the detail keeps its row under the chip"
    );
}

/// A diff block keeps every row inside the pane at 60, 80 and 120 columns:
/// the renderer is handed the pane minus the block's inset.
#[test]
fn a_diff_block_stays_inside_the_pane() {
    let line = TranscriptLine {
        kind: LineKind::Diff,
        text: edit_result().to_owned(),
    };
    for width in [60usize, 80, 120] {
        let rows = message_rows(&line, width, &test_theme(), false).0;
        let texts = row_texts(&rows);
        assert!(texts.len() > 4, "{width}: {texts:?}");
        for (at, row) in texts.iter().enumerate() {
            assert!(
                titi_tui::width::visible_width(row) <= width,
                "{width}: row {at} is {} wide: {row:?}",
                titi_tui::width::visible_width(row)
            );
        }
    }
}

/// A diff longer than the cap is cut where the cap says, and the row that
/// says how much is left counts exactly what the screen did not draw.
#[test]
fn a_very_long_diff_is_capped_and_says_so() {
    let mut text = String::from("edited\n--- a/big.txt\n+++ b/big.txt\n@@ -0,0 +1,300 @@\n");
    for line in 0..300 {
        text.push_str(&format!("+line {line}\n"));
    }
    let line = TranscriptLine {
        kind: LineKind::Diff,
        text,
    };
    let rows = row_texts(&message_rows(&line, 80, &test_theme(), false).0);
    // The chip row, `DIFF_MAX_ROWS` renderer rows, then the note.
    assert_eq!(rows.len(), DIFF_MAX_ROWS + 2, "{:?}", rows.len());
    let last = rows.last().expect("a note");
    assert_eq!(last.trim(), "… 101 more diff lines", "{last:?}");
}

/// The URL of a sign-in note is never cut mid-token and never carries the
/// chip's mark or indent: every row of it is a slice of the URL, so the
/// rows join back to the URL byte for byte.
#[test]
fn a_long_login_url_is_one_slice_per_row() {
    let theme = test_theme();
    let url = format!(
        "https://auth.openai.com/oauth/authorize?client_id=app_EMoamEEZ73f0CkXaXp7hrann\
         &response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback\
         &scope=openid%20profile%20email%20offline_access&state={}",
        "b".repeat(60)
    );
    let line = TranscriptLine {
        kind: LineKind::Note,
        text: format!("login openai-codex: open this URL in your browser\n{url}\nEnter code: WXYZ"),
    };
    let (rows, links) = message_rows(&line, 78, &theme, false);
    let texts = row_texts(&rows);
    assert!(
        links.len() >= 4,
        "a 300-char URL spans rows: {}",
        links.len()
    );
    let visible: String = links.iter().map(|link| link.text.as_str()).collect();
    assert_eq!(visible, url, "rows lost or changed a byte");
    for link in &links {
        assert_eq!(link.url, url, "every row targets the whole URL");
        assert_eq!(texts[link.row], link.text, "the drawn row is the URL slice");
        assert!(
            titi_tui::width::visible_width(&texts[link.row]) <= 78,
            "row over the pane: {:?}",
            texts[link.row]
        );
        assert!(
            !texts[link.row].contains('·'),
            "the chip mark is not part of the link: {:?}",
            texts[link.row]
        );
    }
    assert!(texts[0].contains("login openai-codex"), "{texts:?}");
    assert!(texts[0].contains('·'), "{texts:?}");
    assert!(
        texts[rows.len() - 1].contains("Enter code: WXYZ"),
        "{texts:?}"
    );
}

/// A URL that fits one row is one link row, and the row is the URL.
#[test]
fn a_short_login_url_is_one_row() {
    let theme = test_theme();
    let url = "https://auth.openai.com/codex/device";
    let line = TranscriptLine {
        kind: LineKind::Note,
        text: format!("login openai-codex: open this URL on any device\n{url}\nEnter code: WXYZ"),
    };
    let (rows, links) = message_rows(&line, 78, &theme, false);
    assert_eq!(links.len(), 1, "{:?}", row_texts(&rows));
    assert_eq!(row_texts(&rows)[links[0].row], url);
    assert_eq!(links[0].url, url);
}

/// Every other note, and every error, stays exactly as it was: no OSC 8,
/// even when its text happens to hold a URL.
#[test]
fn other_lines_get_no_link_rows() {
    let theme = test_theme();
    for (kind, text) in [
        (
            LineKind::Note,
            "see https://example.invalid/docs for the rest",
        ),
        (LineKind::Error, "login: https://example.invalid/failed"),
        (
            LineKind::Note,
            "login openai-codex: waiting for the browser",
        ),
        (
            LineKind::Note,
            "login openai-codex: no URL\nEnter code: WXYZ",
        ),
        (
            LineKind::Assistant,
            "login x: open this URL\nhttps://example.invalid\nnow",
        ),
    ] {
        let line = TranscriptLine {
            kind,
            text: text.to_owned(),
        };
        let (_, links) = message_rows(&line, 78, &theme, false);
        assert!(links.is_empty(), "{kind:?} {text:?} grew a link: {links:?}");
    }
}

/// The frame's own cells carry the link: the open sequence and the whole
/// URL sit on the first cell of every row the URL spans, the close on the
/// last, and what the rows spell is still exactly the URL.
#[test]
fn a_frame_hangs_the_whole_url_on_every_row() {
    let url = long_authorize_url('c');
    let mut chat = chat();
    chat.push(
        LineKind::Note,
        format!("login openai-codex: open this URL in your browser\n{url}\nEnter code: WXYZ"),
    );
    let rows = frame_rows(&mut chat, 80, 20);
    let open = format!("\x1b]8;;{url}\x1b\\");
    let link_rows: Vec<&String> = rows.iter().filter(|row| row.contains("\x1b]8;;")).collect();
    let chunks = wrap_url(&url, 78);
    assert!(chunks.len() >= 4, "the URL spans rows: {chunks:?}");
    assert_eq!(link_rows.len(), chunks.len(), "{link_rows:?}");
    let mut shown = String::new();
    for (row, chunk) in link_rows.iter().zip(&chunks) {
        assert_eq!(
            row.matches(&open).count(),
            1,
            "the row targets the whole URL: {row:?}"
        );
        assert_eq!(
            row.matches(titi_tui::caps::OSC8_CLOSE).count(),
            1,
            "{row:?}"
        );
        let visible = strip_escapes(row);
        assert_eq!(visible.trim_end(), *chunk, "row shows its slice: {row:?}");
        assert!(!visible.contains('…'), "nothing elided: {row:?}");
        shown.push_str(visible.trim_end());
    }
    assert_eq!(shown, url, "the rows spell the URL byte for byte");
    assert!(
        rows.iter()
            .any(|row| row.contains("login openai-codex: open this URL")),
        "the head line is still there: {rows:?}"
    );
}

/// A URL that fits one row gets exactly one open and one close.
#[test]
fn a_one_row_link_has_one_pair() {
    let url = "https://auth.openai.com/codex/device";
    let mut chat = chat();
    chat.push(
        LineKind::Note,
        format!("login openai-codex: open this URL on any device\n{url}\nEnter code: WXYZ"),
    );
    let rows = frame_rows(&mut chat, 80, 20);
    let link_rows: Vec<&String> = rows.iter().filter(|row| row.contains("\x1b]8;;")).collect();
    assert_eq!(link_rows.len(), 1, "{link_rows:?}");
    assert_eq!(link_rows[0].matches("\x1b]8;;").count(), 2);
    assert_eq!(strip_escapes(link_rows[0]).trim_end(), url);
}

/// End to end through a real backend: the bytes the terminal receives
/// carry the whole URL on every row, and a screen that ignores OSC 8
/// shows the URL, whole, with no ellipsis.
#[test]
fn a_frame_and_the_backend_leave_the_url_whole() {
    let url = long_authorize_url('d');
    let mut chat = chat();
    chat.push(
        LineKind::Note,
        format!("login openai-codex: open this URL in your browser\n{url}\nEnter code: WXYZ"),
    );
    let sink = Sink::default();
    // A fixed viewport, not the fullscreen one: `Terminal::new` asks the
    // backend for its size, and a real crossterm backend answers that by
    // querying the terminal, which a CI runner without a tty refuses with
    // `EAGAIN`. The bytes that leave the backend are the same either way.
    let viewport = ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 20));
    let mut terminal = match Terminal::with_options(
        CrosstermBackend::new(sink.clone()),
        ratatui::TerminalOptions { viewport },
    ) {
        Ok(terminal) => terminal,
        Err(error) => panic!("test backend: {error}"),
    };
    assert!(terminal.draw(|frame| draw(frame, &mut chat)).is_ok());
    let raw = String::from_utf8_lossy(&sink.0.borrow()).into_owned();
    let open = format!("\x1b]8;;{url}\x1b\\");
    let chunks = wrap_url(&url, 78);
    assert_eq!(
        raw.matches(&open).count(),
        chunks.len(),
        "every row targets the whole URL"
    );
    assert_eq!(
        raw.matches(titi_tui::caps::OSC8_CLOSE).count(),
        chunks.len()
    );
    let view = screen(&raw, 80, 20);
    let first = match view.iter().position(|row| row.starts_with("https://")) {
        Some(first) => first,
        None => panic!("no URL row on the screen: {view:?}"),
    };
    let shown: String = view[first..first + chunks.len()]
        .iter()
        .map(|row| row.trim_end())
        .collect();
    assert_eq!(
        shown, url,
        "escapes ignored, the screen shows the URL whole"
    );
    assert!(
        view.iter().any(|row| row.contains("login openai-codex")),
        "the head line is on the screen: {view:?}"
    );
}

/// A frame with an ordinary note and an error carries no hyperlink at all,
/// even when the text holds a URL.
#[test]
fn a_frame_without_a_login_url_has_no_osc8() {
    let mut chat = chat();
    chat.push(LineKind::Note, "model openai/gpt-4.1".to_owned());
    chat.push(
        LineKind::Error,
        "login: see https://example.invalid/trouble".to_owned(),
    );
    for row in frame_rows(&mut chat, 80, 20) {
        assert!(!row.contains("\x1b]8;;"), "an OSC 8 leaked: {row:?}");
    }
}

fn long_authorize_url(fill: char) -> String {
    format!(
        "https://auth.openai.com/oauth/authorize?client_id=app_EMoamEEZ73f0CkXaXp7hrann\
         &response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback\
         &scope=openid%20profile%20email%20offline_access&state={}",
        fill.to_string().repeat(40)
    )
}

/// The rendered cells of every row of a frame, escapes included.
///
/// A cell a wide glyph draws across is left out: ratatui writes a blank
/// there and the glyph itself covers both columns, so counting that cell
/// would make a row measure one column wider than a terminal renders it.
fn frame_rows(chat: &mut Chat, width: u16, height: u16) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = match ratatui::Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => panic!("test backend: {error}"),
    };
    assert!(terminal.draw(|frame| draw(frame, chat)).is_ok());
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            let mut row = String::new();
            let mut hidden = 0usize;
            for x in 0..width {
                let symbol = buffer[(x, y)].symbol();
                if hidden > 0 {
                    hidden -= 1;
                    continue;
                }
                hidden = titi_tui::width::visible_width(symbol).saturating_sub(1);
                row.push_str(symbol);
            }
            row
        })
        .collect()
}

/// Every colour a frame's cells carry, foreground and background, in row
/// order.
fn frame_colors(chat: &mut Chat, width: u16, height: u16) -> Vec<(Color, Color)> {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = match ratatui::Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => panic!("test backend: {error}"),
    };
    assert!(terminal.draw(|frame| draw(frame, chat)).is_ok());
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| (cell.fg, cell.bg))
        .collect()
}

/// A screen of every kind of row, so one frame exercises every colour role
/// the chat has: a user block, a pending tool, a finished tool, an error and
/// a note, under the masthead, the composer and its caption.
fn colored_chat(theme: Arc<Theme>) -> Chat {
    let mut chat = Chat::new("openai/gpt-4.1", "session-123", theme);
    chat.push(LineKind::User, "hi".to_owned());
    chat.push(LineKind::Tool, "tool bash".to_owned());
    chat.push(LineKind::Tool, "tool done  read".to_owned());
    chat.push(LineKind::Error, "broken".to_owned());
    chat.push(LineKind::Note, "a note".to_owned());
    chat
}

/// Every colour the live screen draws is a token of the active theme: the
/// same frame under two themes carries two palettes, each theme's own
/// accent lands in the cells, and each role resolves to the value its
/// theme file declares.
#[test]
fn a_frame_takes_every_colour_from_the_theme() {
    let mut titanium = colored_chat(test_theme_named("titanium"));
    let mut light = colored_chat(test_theme_named("light"));
    let titanium_colors = frame_colors(&mut titanium, 80, 20);
    let light_colors = frame_colors(&mut light, 80, 20);
    assert_ne!(
        titanium_colors, light_colors,
        "the frame ignored the theme it was given"
    );

    // The accent lands in the cells, and the same role under the other
    // theme is the other theme's accent: the wiring, not just a palette.
    assert!(
        titanium_colors
            .iter()
            .any(|(fg, _)| *fg == Color::Rgb(0, 180, 255)),
        "titanium's accent is not on the screen"
    );
    assert!(
        light_colors
            .iter()
            .any(|(fg, _)| *fg == Color::Rgb(90, 128, 128)),
        "light's accent is not on the screen"
    );

    // The escapes those cells become are crossterm's, not the screen's —
    // and crossterm honours `NO_COLOR` through a process-wide switch
    // (`style::force_color_output`), which a test has no business throwing
    // for every other test in this binary. The colour a cell carries is the
    // part this module decides, so that is what is asserted.

    // One role per token, with the token's own value: the theme files
    // declare these, and a change to one has to be a change here too.
    let theme = test_theme_named("titanium");
    for (token, rgb) in [
        (ThemeColor::Accent, (0, 180, 255)),               // electricBlue
        (ThemeColor::CustomMessageLabel, (212, 192, 144)), // titaniumGold
        (ThemeColor::Warning, (255, 179, 71)),             // warningAmber
        (ThemeColor::Success, (0, 255, 136)),              // readoutGreen
        (ThemeColor::Error, (255, 71, 87)),                // alertRed
        (ThemeColor::Muted, (156, 163, 176)),              // dimAluminum
        (ThemeColor::Dim, (107, 114, 128)),
        (ThemeColor::Border, (42, 48, 56)), // subtleGray
        // `text` is the terminal default on a dark page; the theme answers
        // with the dark default so the screen never loses its body colour.
        (ThemeColor::Text, (229, 229, 231)),
    ] {
        let (r, g, b) = rgb;
        assert_eq!(fg(&theme, token).fg, Some(Color::Rgb(r, g, b)), "{token:?}");
    }
    assert_eq!(bg(&theme, ThemeBg::StatusLineBg), Color::Rgb(15, 18, 22));
    assert_eq!(bg(&theme, ThemeBg::CustomMessageBg), Color::Rgb(42, 48, 56));
}

/// A backend that keeps what it was given, so a test can read the bytes
/// the terminal would have received.
#[derive(Clone, Default)]
struct Sink(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// What a screen renderer shows: CSI and OSC sequences consumed, CUP
/// moves the cursor, every other character lands in the grid. A terminal
/// that ignores OSC 8 sees exactly this.
fn screen(text: &str, width: usize, height: usize) -> Vec<String> {
    let mut grid = vec![vec![' '; width]; height];
    let mut row = 0usize;
    let mut col = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            if row < height && col < width {
                grid[row][col] = ch;
            }
            col += 1;
            if col >= width {
                col = 0;
                row += 1;
            }
            continue;
        }
        match chars.next() {
            Some('[') => {
                let mut params = String::new();
                let mut command = ' ';
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        command = next;
                        break;
                    }
                    params.push(next);
                }
                if command == 'H' {
                    let mut parts = params.split(';');
                    row = parts
                        .next()
                        .and_then(|part| part.parse::<usize>().ok())
                        .unwrap_or(1)
                        .saturating_sub(1);
                    col = parts
                        .next()
                        .and_then(|part| part.parse::<usize>().ok())
                        .unwrap_or(1)
                        .saturating_sub(1);
                }
            }
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    grid.into_iter()
        .map(|row| row.into_iter().collect())
        .collect()
}

/// What a screen renderer shows: CSI and OSC sequences removed, the rest
/// kept in order.
fn strip_escapes(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The whole block reaches the screen, each piece on a row of its own.
#[test]
fn a_multi_line_note_reaches_the_frame() {
    let mut chat = chat();
    chat.push(LineKind::Note, "one\ntwo\nthree".to_owned());
    let view = frame_text(&mut chat);
    let rows: Vec<String> = view
        .chars()
        .collect::<Vec<_>>()
        .chunks(80)
        .map(|row| row.iter().collect())
        .collect();
    let mut at = Vec::new();
    for want in ["one", "two", "three"] {
        let found: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.contains(want))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(found.len(), 1, "{want} once: {rows:?}");
        at.push(found[0]);
    }
    assert!(
        at[0] < at[1] && at[1] < at[2],
        "the three lines share a row: {at:?} {rows:?}"
    );
}
#[test]
fn recap_reports_sections() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();

    let store = titi_core::session::SessionStore::new(&chat.agent_dir).unwrap();
    let session_id = store
        .create(titi_core::session::SessionMeta::default())
        .unwrap();
    chat.session_id = session_id;

    type_text(&mut chat, "/recap");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(chat.lines.iter().any(|line| line.text.contains("Session")));
    assert!(chat.lines.iter().any(|line| line.text.contains("Turns")));
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("roles: 0 user, 0 assistant, 0 system"))
    );
}
#[test]
fn fork_creates_a_new_session_and_says_so() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();

    let store = titi_core::session::SessionStore::new(&chat.agent_dir).unwrap();
    let session_id = store
        .create(titi_core::session::SessionMeta::default())
        .unwrap();
    chat.session_id = session_id;

    type_text(&mut chat, "/fork");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("forked to")
                && line.text.contains("restart to resume it"))
    );
}

#[test]
fn export_defaults_to_agent_dir_exports() {
    let dir = tempfile::tempdir().expect("temp");
    let mut chat = chat();
    chat.agent_dir = dir.path().to_path_buf();

    let store = titi_core::session::SessionStore::new(&chat.agent_dir).unwrap();
    let session_id = store
        .create(titi_core::session::SessionMeta::default())
        .unwrap();
    chat.session_id = session_id;

    type_text(&mut chat, "/export");
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_none());
    assert!(chat.lines.iter().any(|line| line.text.contains("exports")));
}
#[test]
fn transcript_scrolls_with_page_keys() {
    let mut chat = chat();
    for i in 0..50 {
        chat.push(LineKind::Note, format!("line {i}"));
    }

    let view = frame_text(&mut chat);
    assert!(view.contains("line 49"), "bottom line visible");
    assert!(!view.contains("line 0"), "top line hidden");

    chat.on_key(Key::PageUp, Instant::now());
    chat.on_key(Key::PageUp, Instant::now());
    chat.on_key(Key::PageUp, Instant::now());
    let view_scrolled = frame_text(&mut chat);
    assert!(
        view_scrolled.contains("line 0"),
        "top line visible after scroll"
    );
    assert!(
        !view_scrolled.contains("line 49"),
        "bottom line hidden after scroll"
    );

    chat.on_key(Key::PageDown, Instant::now());
    chat.on_key(Key::PageDown, Instant::now());
    chat.on_key(Key::PageDown, Instant::now());
    let view_down = frame_text(&mut chat);
    assert!(view_down.contains("line 49"), "bottom line visible again");

    chat.on_key(Key::PageUp, Instant::now());
    chat.on_key(Key::Char('a'), Instant::now());
    let view_reset = frame_text(&mut chat);
    assert!(view_reset.contains("line 49"), "typing resets to bottom");
}

// ---- Mouse selection and copy ---------------------------------------

/// Draw one frame and hand the terminal back, so a test can read the
/// buffer (what a terminal would receive) and the chat's own rows (what a
/// copy would carry).
fn drawn(
    chat: &mut Chat,
    width: u16,
    height: u16,
) -> ratatui::Terminal<ratatui::backend::TestBackend> {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = match ratatui::Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => panic!("test backend: {error}"),
    };
    assert!(terminal.draw(|frame| draw(frame, chat)).is_ok());
    terminal
}

/// A chat whose reply wraps over several rows at 40 columns, drawn once.
///
/// The reply is markdown-less, so it goes through the plain speech path:
/// the rows are the text, wrapping, and nothing else.
fn wrapped_reply_chat() -> (Chat, ratatui::Terminal<ratatui::backend::TestBackend>) {
    let mut chat = chat();
    chat.push(LineKind::User, "wrap it".to_owned());
    chat.push(
        LineKind::Assistant,
        "the quick brown fox jumps over the lazy dog and keeps going".to_owned(),
    );
    let terminal = drawn(&mut chat, 40, 20);
    (chat, terminal)
}
/// The transcript rows holding `needle`, and where the transcript starts.
fn selected_rows(chat: &Chat, needle: &str) -> usize {
    chat.last_rows
        .iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} is on screen: {:?}", chat.last_rows))
}

/// A drag over a wrapped reply copies its text: the wrapping is the
/// screen's, so it is not in the copy; the gutter and the theme's colours
/// are the screen's too, so they are not either.
#[test]
fn a_drag_over_a_wrapped_reply_copies_the_text() {
    let (mut chat, _terminal) = wrapped_reply_chat();
    let first = selected_rows(&chat, "the quick brown fox");
    let last = selected_rows(&chat, "going");
    assert!(last > first, "the reply wrapped: {:?}", chat.last_rows);
    let top = chat.transcript_top;

    // The body column: the nine cells of `  titi │ ` are the gutter.
    chat.mouse_press(9, top + first as u16);
    chat.mouse_drag(39, top + last as u16);
    let copied = chat.mouse_release().expect("a drag copies");

    assert!(!copied.contains('\u{1b}'), "no styling: {copied:?}");
    assert_eq!(copied.lines().count(), last - first + 1, "{copied:?}");
    assert!(copied.starts_with("the quick brown fox"), "{copied:?}");
    assert!(copied.ends_with("going"), "{copied:?}");
    for line in copied.lines() {
        assert_eq!(line, line.trim(), "no padding: {line:?}");
    }
    // The selection stands after the release, the way a terminal's does.
    assert!(chat.selection().is_some_and(|sel| !sel.active));
}

/// The frame paints the theme's `selectedBg` behind the selected cells and
/// leaves the guttter and the rows outside the selection alone.
#[test]
fn the_selection_paints_the_selected_cells_with_selected_bg() {
    let (mut chat, mut terminal) = wrapped_reply_chat();
    let first = selected_rows(&chat, "the quick brown fox");
    let last = selected_rows(&chat, "going");
    let top = chat.transcript_top;

    chat.mouse_press(9, top + first as u16);
    chat.mouse_drag(39, top + last as u16);
    assert!(terminal.draw(|frame| draw(frame, &mut chat)).is_ok());

    let theme = Arc::clone(&chat.theme);
    let selected = bg(&theme, ThemeBg::SelectedBg);
    let page_bg = bg(&theme, ThemeBg::StatusLineBg);
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(20u16, top + first as u16)].bg, selected);
    assert_eq!(buffer[(20u16, top + last as u16)].bg, selected);
    // The gutter is left of the selection's first column.
    assert_eq!(buffer[(4u16, top + first as u16)].bg, page_bg);
    // And the row below the selection is untouched.
    assert_eq!(buffer[(20u16, top + last as u16 + 1)].bg, page_bg);
}

/// A click selects nothing, so nothing is copied; a drag never scrolls.
#[test]
fn a_click_copies_nothing_and_a_drag_does_not_scroll() {
    let (mut chat, _terminal) = wrapped_reply_chat();
    let first = selected_rows(&chat, "the quick brown fox");
    let top = chat.transcript_top;
    let before = chat.scroll_offset;

    chat.mouse_press(12, top + first as u16);
    assert!(chat.mouse_release().is_none(), "a click copies nothing");
    assert_eq!(chat.selection_text(), "");

    chat.mouse_press(12, top + first as u16);
    chat.mouse_drag(20, top + first as u16 + 1);
    assert!(chat.mouse_release().is_some());
    assert_eq!(chat.scroll_offset, before, "a drag does not scroll");
}

/// A key takes a standing selection away, and the wheel scrolls the
/// transcript — but moves a panel's cursor while one is open.
#[test]
fn a_key_clears_the_selection_and_the_wheel_scrolls() {
    let mut chat = chat();
    for i in 0..50 {
        chat.push(LineKind::Note, format!("line {i}"));
    }
    drawn(&mut chat, 80, 20);
    chat.mouse_press(2, 3);
    chat.mouse_drag(8, 5);
    assert!(chat.selection().is_some());
    chat.on_key(Key::Char('x'), Instant::now());
    assert!(chat.selection().is_none(), "a key clears the selection");

    let before = chat.scroll_offset;
    chat.mouse_wheel(1, Instant::now());
    assert_eq!(chat.scroll_offset, before + 1);
    chat.mouse_wheel(-1, Instant::now());
    assert_eq!(chat.scroll_offset, before);

    // An open slash list takes the wheel as its cursor, not the transcript.
    chat.input.clear();
    chat.on_key(Key::Char('/'), Instant::now());
    assert!(chat.picking(), "input {:?}", chat.input);
    let scroll = chat.scroll_offset;
    chat.mouse_wheel(1, Instant::now());
    assert_eq!(chat.scroll_offset, scroll, "the wheel moved the list");
}

/// `/mouse off` persists the preset and queues the sequence that turns
/// reporting off; a preset is the one the next run reads back.
#[test]
fn slash_mouse_persists_the_preset_and_switches_reporting_over() {
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    type_text(&mut chat, "/mouse off");
    chat.on_key(Key::Enter, Instant::now());

    assert_eq!(chat.mouse_preset(), MousePreset::Off);
    assert_eq!(
        crate::session_fs::load_mouse_preset_from(dir.path()),
        Some(MousePreset::Off)
    );
    let flush = chat.take_output_flush();
    assert!(flush.contains("\x1b[?1002l"), "drags off: {flush:?}");
    assert!(flush.contains("\x1b[?1003l"), "motion off: {flush:?}");
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("mouse: off")),
        "the screen says so: {:?}",
        chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
    );

    // A preset that is not one says what there is and changes nothing.
    let before = chat.mouse_preset();
    type_text(&mut chat, "/mouse sideways");
    chat.on_key(Key::Enter, Instant::now());
    assert_eq!(chat.mouse_preset(), before);
    assert!(chat.take_output_flush().is_empty());
}

/// The clipboard writer is found on the search path it is given, and the
/// copy it makes is the one a paste would find.
#[test]
fn a_copy_with_an_os_writer_goes_to_it() {
    let dir = tempfile::tempdir().expect("temp");
    let out = dir.path().join("copied.txt");
    let bin = dir.path().join("pbcopy");
    std::fs::write(&bin, format!("#!/bin/sh\ncat > {}\n", out.display())).expect("write");
    let mut perms = std::fs::metadata(&bin).expect("meta").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&bin, perms).expect("chmod");

    let mut chat = chat();
    copy_selection(
        &mut chat,
        "hello clipboard",
        &dir.path().display().to_string(),
    );
    assert!(
        chat.take_output_flush().is_empty(),
        "a tool took it, so nothing goes out as OSC 52"
    );
    assert_eq!(
        std::fs::read_to_string(&out).expect("the copy landed"),
        "hello clipboard"
    );
    assert!(chat.hint.contains("pbcopy"), "{:?}", chat.hint);
}

/// With no writer on the path, the copy goes out as OSC 52 — the route a
/// terminal over SSH can still reach.
#[test]
fn a_copy_with_no_os_writer_goes_out_as_osc52() {
    let mut chat = chat();
    copy_selection(&mut chat, "over ssh", "/nonexistent");
    assert_eq!(
        chat.take_output_flush(),
        titi_tui::caps::osc52_copy("over ssh")
    );
    assert!(chat.hint.contains("OSC 52"), "{:?}", chat.hint);
}

/// The capability check is a property of the path it is handed.
#[test]
fn the_clipboard_writer_is_the_first_one_on_the_path() {
    let dir = tempfile::tempdir().expect("temp");
    let path = dir.path().display().to_string();
    assert_eq!(clipboard_writer(&path), None);
    std::fs::write(dir.path().join("wl-copy"), b"").expect("write");
    assert_eq!(
        executable_path("wl-copy", &path),
        Some(dir.path().join("wl-copy"))
    );
    assert_eq!(executable_path("pbcopy", &path), None);
    assert_eq!(
        clipboard_writer(&path).map(|(bin, _, program)| (bin, program)),
        Some(("wl-copy", dir.path().join("wl-copy")))
    );
}

// ---- Terminal appearance --------------------------------------------

/// A white reply (light) and a black one (dark), as the wire carries them.
const LIGHT_REPLY: &[u8] = b"\x1b]11;rgb:ffff/ffff/ffff\x07";
const DARK_REPLY: &[u8] = b"\x1b]11;rgb:0000/0000/0000\x07";

fn surface_hex(theme: &Theme) -> String {
    theme.get_bg_hex(ThemeBg::StatusLineBg)
}

/// An OSC 11 reply moves the screen to the palette that appearance's slot
/// holds; the same appearance twice is not a second repaint, and a payload
/// that is not a reply changes nothing.
#[test]
fn an_osc11_reply_moves_the_palette_to_that_slot() {
    let _guard = theme_lock();
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.theme = test_theme();
    chat.set_starting_appearance(Appearance::Dark);
    let light = crate::themes::theme_named("light").expect("the crate carries light");
    let dark = test_theme();
    assert_ne!(surface_hex(&light), surface_hex(&dark));

    assert_eq!(
        chat.ingest_probe_reply(LIGHT_REPLY),
        ProbeOutcome::ThemeChanged
    );
    assert_eq!(surface_hex(&chat.theme), surface_hex(&light));
    assert_eq!(
        chat.ingest_probe_reply(LIGHT_REPLY),
        ProbeOutcome::Unchanged,
        "the same appearance is already on screen"
    );
    assert_eq!(
        chat.ingest_probe_reply(DARK_REPLY),
        ProbeOutcome::ThemeChanged
    );
    assert_eq!(surface_hex(&chat.theme), surface_hex(&dark));
    assert_eq!(
        chat.ingest_probe_reply(b"11;rgb:nonsense"),
        ProbeOutcome::Unchanged
    );
}

/// The slot's own choice wins over the crate's pick for that appearance,
/// and an explicit `--theme` is not the probe's to move.
#[test]
fn the_slot_choice_wins_and_a_theme_flag_keeps_the_palette() {
    let _guard = theme_lock();
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.theme = test_theme();
    chat.set_starting_appearance(Appearance::Dark);
    let workspace = crate::session_fs::current_workspace();
    let mut settings =
        titi_config::settings::Settings::load(dir.path(), &workspace, &[]).expect("settings");
    settings
        .set(
            titi_config::settings::THEME_LIGHT_KEY,
            serde_json::json!("amethyst"),
        )
        .expect("the light slot is written");
    let chosen = crate::themes::theme_named("amethyst").expect("the crate carries amethyst");

    assert_eq!(
        chat.ingest_probe_reply(LIGHT_REPLY),
        ProbeOutcome::ThemeChanged
    );
    assert_eq!(surface_hex(&chat.theme), surface_hex(&chosen));

    // And with the loop off — `--theme` — nothing moves at all.
    chat.set_appearance_auto(false);
    assert_eq!(
        chat.ingest_probe_reply(DARK_REPLY),
        ProbeOutcome::Unchanged,
        "the palette the run was started with stands"
    );
    assert_eq!(surface_hex(&chat.theme), surface_hex(&chosen));
}

/// The reply arrives on the keyboard's own stream: it is reassembled and
/// swallowed, and a key typed in the same window still reaches the
/// composer.
#[test]
fn a_probe_reply_arrives_as_keys_and_never_reaches_the_composer() {
    let _guard = theme_lock();
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.theme = test_theme();
    chat.set_starting_appearance(Appearance::Dark);
    let light = crate::themes::theme_named("light").expect("the crate carries light");

    let now = Instant::now();
    chat.on_focus_gained(now);
    assert!(
        chat.take_output_flush()
            .contains(titi_tui::caps::OSC11_QUERY),
        "the focus gain asks the terminal"
    );

    // A key typed in the window is a key, not a reply.
    let typed = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
    assert!(!chat.absorb_probe_key(&typed, now), "a plain key is a key");

    // What the terminal answers with, as the event layer reads it: the
    // ESC of `ESC ]` comes back as an alt-modified `]`.
    // The BEL of `\x1b]11;rgb:…\x07` is a C0 byte the event layer reads as
    // the control chord it is a key code for: ctrl-`g`.
    let mut reply: Vec<(char, KeyModifiers)> = "]11;rgb:ffff/ffff/ffff"
        .chars()
        .enumerate()
        .map(|(at, ch)| {
            let modifiers = if at == 0 {
                KeyModifiers::ALT
            } else {
                KeyModifiers::NONE
            };
            (ch, modifiers)
        })
        .collect();
    reply.push(('g', KeyModifiers::CONTROL));
    for (ch, modifiers) in reply {
        let key = KeyEvent::new(KeyCode::Char(ch), modifiers);
        assert!(
            chat.absorb_probe_key(&key, now),
            "the reply's {ch:?} is swallowed"
        );
    }
    assert_eq!(chat.input, "", "no character of the reply was typed");
    assert_eq!(surface_hex(&chat.theme), surface_hex(&light));

    // The window closed with the reply: a later alt-`]` is not a reply.
    let late = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT);
    assert!(!chat.absorb_probe_key(&late, now));
    // And neither is one after the window has run out.
    chat.on_focus_gained(now);
    let later = now + PROBE_REPLY_WINDOW + Duration::from_millis(1);
    let late = KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT);
    assert!(!chat.absorb_probe_key(&late, later), "the window expired");
}

/// A mode-2031 notification is a re-query trigger: the reply to the fresh
/// query is what decides the palette.
#[test]
fn a_mode_2031_report_asks_for_a_fresh_query() {
    let (_dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    chat.set_starting_appearance(Appearance::Dark);
    assert_eq!(
        chat.ingest_probe_reply(b"\x1b[?997;1n"),
        ProbeOutcome::NeedOsc11Query
    );
    assert_eq!(
        chat.ingest_probe_reply(b"\x1b[?997;2n"),
        ProbeOutcome::NeedOsc11Query
    );
}

// ---- Prompt history -------------------------------------------------

/// A chat whose session exists in its own store, so the prompts it is asked
/// land where the history reads them from (the live run's own path: the
/// session is made before the screen opens).
fn history_chat() -> (tempfile::TempDir, Chat) {
    let dir = tempfile::tempdir().expect("temp");
    let store = titi_core::session::SessionStore::new(dir.path()).expect("store");
    let session_id = store
        .create(titi_core::session::SessionMeta::default())
        .expect("a session");
    let mut chat = Chat::new("openai/gpt-4.1", &session_id, test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    (dir, chat)
}

/// Ask `chat` something the way the live run does: the key, then the store
/// write the run records (`record`), so the session's own history has the
/// prompt and nothing else does.
fn ask(chat: &mut Chat, log: &Option<SessionLog>, prompt: &str) {
    type_text(chat, prompt);
    let applied = chat.on_key(Key::Enter, Instant::now());
    record(chat, log, applied.log);
}

/// ↑ at an empty composer opens the browser over the session's own prompts,
/// newest first; a query narrows it; Enter puts the chosen prompt in the
/// composer and sends nothing; Esc leaves the text alone.
#[test]
fn the_history_browser_puts_a_past_prompt_in_the_composer_unsent() {
    let (_dir, mut chat) = history_chat();
    let log = SessionLog::open(&chat.agent_dir, &chat.session_id);
    ask(&mut chat, &log, "first prompt");
    ask(&mut chat, &log, "second prompt");
    assert!(chat.input.is_empty(), "a submit clears the composer");

    // ↑ at the empty composer: the browser, not the transcript's scroll.
    let opened = chat.on_key(Key::Up, Instant::now());
    assert!(opened.effect.is_none(), "opening sends nothing");
    assert!(chat.history_picker.is_some(), "the browser is up");
    let frame = frame_rows(&mut chat, 80, 20).join("\n");
    assert!(frame.contains("history · 2"), "{frame}");
    assert!(frame.contains("second prompt"), "{frame}");
    assert!(frame.contains("first prompt"), "{frame}");
    // Newest first *in the panel*: the transcript above it holds the same
    // two strings in the order they were asked.
    let panel = &frame[frame.find("history · 2").expect("the browser")..];
    let newest = panel.find("second prompt").expect("newest listed");
    let oldest = panel.find("first prompt").expect("oldest listed");
    assert!(newest < oldest, "newest first: {panel}");

    // Typing narrows it, the way the model browser does.
    type_text(&mut chat, "first");
    let narrowed = frame_rows(&mut chat, 80, 20).join("\n");
    let panel = &narrowed[narrowed.find("history · ").expect("the browser")..];
    assert!(panel.contains("history · 1 of 2 · first"), "{panel}");
    assert!(!panel.contains("second prompt"), "{panel}");
    assert!(panel.contains("first prompt"), "{panel}");

    // Enter takes the row into the composer, and the turn is not started.
    let taken = chat.on_key(Key::Enter, Instant::now());
    assert!(taken.effect.is_none(), "a pick never sends");
    assert!(taken.log.is_none(), "and never logs");
    assert!(chat.history_picker.is_none(), "the browser closed");
    assert_eq!(chat.input, "first prompt");

    // Ctrl+R opens it again, and Esc closes without touching the draft.
    chat.on_key(Key::CtrlR, Instant::now());
    assert!(chat.history_picker.is_some());
    type_text(&mut chat, "second");
    chat.on_key(Key::Esc, Instant::now());
    assert!(chat.history_picker.is_none());
    assert_eq!(chat.input, "first prompt", "Esc left the draft alone");
}

/// The caret's own keys, one family at a time: a character, a word, and the
/// two ends of the draft.
#[test]
fn the_caret_moves_by_a_character_a_word_and_to_the_ends() {
    let mut chat = chat();
    type_text(&mut chat, "fix the parser now");
    assert_eq!(
        chat.caret(),
        chat.input.len(),
        "typing leaves it at the end"
    );

    chat.on_key(Key::Left, Instant::now());
    assert_eq!(chat.caret(), chat.input.len() - 1);
    chat.on_key(Key::Home, Instant::now());
    assert_eq!(chat.caret(), 0);
    chat.on_key(Key::Right, Instant::now());
    assert_eq!(chat.caret(), 1);
    chat.on_key(Key::End, Instant::now());
    assert_eq!(chat.caret(), chat.input.len());

    // ctrl+a and ctrl+e are the same two ends: the crate's own table binds
    // them (`tui.editor.cursorLineStart`/`cursorLineEnd`).
    assert_eq!(
        map_key(KeyCode::Char('a'), KeyModifiers::CONTROL),
        Some(Key::Home)
    );
    assert_eq!(
        map_key(KeyCode::Char('e'), KeyModifiers::CONTROL),
        Some(Key::End)
    );

    // A word at a time, from the end: `now`, then the spaces, then `parser`.
    chat.on_key(Key::WordLeft, Instant::now());
    assert_eq!(&chat.input[chat.caret()..], "now");
    chat.on_key(Key::WordLeft, Instant::now());
    assert_eq!(&chat.input[chat.caret()..], "parser now");
    chat.on_key(Key::WordRight, Instant::now());
    assert_eq!(&chat.input[chat.caret()..], " now");

    // A wide character is one character to the caret, not three bytes.
    let mut wide = chat_with_theme(test_theme());
    type_text(&mut wide, "日本語");
    wide.on_key(Key::Left, Instant::now());
    assert_eq!(&wide.input[wide.caret()..], "語");
    wide.on_key(Key::WordLeft, Instant::now());
    assert_eq!(wide.caret(), 0);
}

/// Typing, backspace and delete happen where the caret is, not at the end
/// of the draft.
#[test]
fn typing_and_deleting_happen_at_the_caret() {
    let mut chat = chat();
    type_text(&mut chat, "fix the parser");
    chat.on_key(Key::Home, Instant::now());
    type_text(&mut chat, "please ");
    assert_eq!(chat.input, "please fix the parser");
    assert_eq!(chat.caret(), "please ".len());

    // Backspace takes what is behind it; delete takes what is ahead.
    chat.on_key(Key::Backspace, Instant::now());
    assert_eq!(chat.input, "pleasefix the parser");
    chat.on_key(Key::Delete, Instant::now());
    assert_eq!(chat.input, "pleaseix the parser");

    // ctrl+u is the crate's `deleteToLineStart`: everything before the
    // caret, wherever the caret is.
    chat.on_key(Key::End, Instant::now());
    chat.on_key(Key::DeleteToStart, Instant::now());
    assert_eq!(chat.input, "");
    assert_eq!(chat.caret(), 0);
    assert_eq!(
        map_key(KeyCode::Char('u'), KeyModifiers::CONTROL),
        Some(Key::DeleteToStart)
    );

    // alt+backspace and ctrl+w take the word before the caret too.
    let mut words = chat_with_theme(test_theme());
    type_text(&mut words, "fix the parser now");
    words.on_key(Key::WordLeft, Instant::now());
    words.on_key(Key::DeleteWord, Instant::now());
    assert_eq!(words.input, "fix the now");
}

/// A completion completes the token the caret is in, and the sentence
/// around it stays where it was.
#[test]
fn the_slash_list_completes_the_token_the_caret_is_in() {
    let mut chat = chat();
    type_text(&mut chat, "/help now");
    // Five lefts put the caret inside `/help`: after `/hel`.
    for _ in 0..5 {
        chat.on_key(Key::Left, Instant::now());
    }
    assert!(chat.picking(), "the list is up for the token at the caret");
    chat.on_key(Key::Tab, Instant::now());
    assert_eq!(
        chat.input, "/help now",
        "the token became `/help` and the sentence kept its own space"
    );
    assert_eq!(&chat.input[chat.caret()..], " now", "the caret follows it");

    // Mid-sentence a command is not a command — the rule that keeps a
    // path out of the list — but a skill still completes where the caret
    // is.
    let mut skills = chat_with_skills();
    type_text(&mut skills, "please use /");
    assert!(skills.picking(), "the skill list is up at the caret");
    let wanted = skills.row_name(&picker_rows(&skills)[0]).to_owned();
    skills.on_key(Key::Tab, Instant::now());
    assert_eq!(
        skills.input,
        format!("please use /{wanted} "),
        "the skill completed where the caret was"
    );
}

/// An emoji expands where the caret is, and the picker follows the caret
/// rather than the end of the draft.
#[test]
fn an_emoji_expands_where_the_caret_is() {
    let mut chat = chat();
    type_text(&mut chat, "say :tada");
    assert!(
        chat.emoji_picker.is_visible(),
        "the picker follows the caret"
    );
    chat.on_key(Key::Home, Instant::now());
    assert!(
        !chat.emoji_picker.is_visible(),
        "the caret left the query, so the picker went"
    );

    // Typed before existing text, the expansion leaves that text alone.
    let mut early = chat_with_theme(test_theme());
    type_text(&mut early, "now");
    early.on_key(Key::Home, Instant::now());
    type_text(&mut early, ":tada:");
    assert_eq!(early.input, "🎉now");
    assert_eq!(
        &early.input[early.caret()..],
        "now",
        "the caret follows the glyph"
    );
}

/// A paste lands where the caret is.
#[test]
fn a_paste_lands_at_the_caret() {
    let mut chat = chat();
    type_text(&mut chat, "fix the parser");
    chat.on_key(Key::Home, Instant::now());
    chat.paste("README");
    assert_eq!(chat.input, "READMEfix the parser");
    assert_eq!(chat.caret(), "README".len());
}

/// A paste marker is one unit to the caret: it cannot be landed inside, a
/// backspace through it takes the whole thing (its body goes with it), and
/// so does a delete in front of it.
#[test]
fn the_caret_crosses_a_paste_marker_as_one_unit() {
    let mut chat = chat();
    chat.paste(&stack_trace());
    assert_eq!(chat.input, "[Paste #1 · 8 lines]");
    let whole = chat.input.len();

    chat.on_key(Key::Home, Instant::now());
    chat.on_key(Key::Right, Instant::now());
    assert_eq!(chat.caret(), whole, "the marker moved as one unit");
    chat.on_key(Key::Left, Instant::now());
    assert_eq!(chat.caret(), 0, "and back as one unit");
    chat.on_key(Key::End, Instant::now());
    chat.on_key(Key::WordLeft, Instant::now());
    assert_eq!(chat.caret(), 0, "a word motion crosses it too");

    chat.on_key(Key::End, Instant::now());
    chat.on_key(Key::Backspace, Instant::now());
    assert_eq!(chat.input, "", "the marker went whole, body and all");
    assert_eq!(chat.caret(), 0);

    let mut front = chat_with_theme(test_theme());
    front.paste(&stack_trace());
    front.on_key(Key::Home, Instant::now());
    front.on_key(Key::Delete, Instant::now());
    assert_eq!(front.input, "", "delete in front of it takes it whole");
}

/// A draft longer than the row keeps the caret in view: the head when the
/// caret is at the start, the tail when it is at the end.
#[test]
fn a_long_draft_keeps_the_caret_visible() {
    let mut chat = chat();
    chat.input = format!("{}the end", "x".repeat(200));
    chat.caret_to_end();
    let frame = frame_rows(&mut chat, 40, 12).join("\n");
    assert!(
        frame.contains("the end▍"),
        "the caret is at the end: {frame}"
    );

    chat.on_key(Key::Home, Instant::now());
    let frame = frame_rows(&mut chat, 40, 12).join("\n");
    assert!(
        frame.contains("› ▍xxxx"),
        "the caret is at the start: {frame}"
    );
    assert!(!frame.contains("the end"), "{frame}");

    // A caret in the middle keeps a few cells of what follows it visible.
    chat.set_caret(120);
    let frame = frame_rows(&mut chat, 40, 12).join("\n");
    assert!(
        frame.contains("▍xxx"),
        "the caret is in the middle: {frame}"
    );
}

/// The setting is off unless it is asked for: with `editor.vim` unset the
/// composer is exactly the composer it was, and no key here is special.
#[test]
fn without_the_setting_the_composer_is_the_one_it_always_was() {
    let mut chat = chat();
    assert_eq!(chat.vim_mode(), None, "off by default");
    // Every key of the vim vocabulary types or edits as it always has:
    // `h` is a letter, `0` is a digit, Esc clears the draft.
    for ch in "hlwbe0^$xXdcDCsSiIaA".chars() {
        type_text(&mut chat, &ch.to_string());
    }
    assert_eq!(chat.input, "hlwbe0^$xXdcDCsSiIaA");
    assert_eq!(chat.caret(), chat.input.len(), "the caret is at the end");
    assert!(chat.on_key(Key::Esc, Instant::now()).effect.is_none());
    assert_eq!(chat.input, "", "Esc still clears the draft");
    // And the listing has no vim block.
    assert!(
        !hotkey_lines(&[])
            .iter()
            .any(|line| line.contains("editor.vim")),
        "the vim rows are not listed while the mode is off"
    );
}

/// Esc leaves Insert for Normal and steps the caret back one, the way
/// vim's does; `i`, `a`, `I` and `A` come back at the right place.
#[test]
fn esc_enters_normal_and_the_insert_keys_come_back_where_they_say() {
    let mut chat = vim_chat();
    type_text(&mut chat, "one two");
    assert_eq!(chat.caret(), 7);
    chat.on_key(Key::Esc, Instant::now());
    assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Normal));
    assert_eq!(chat.caret(), 6, "the caret steps back one");

    // `i` at the caret, and Insert types there.
    vim_key(&mut chat, 'i');
    assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Insert));
    assert_eq!(chat.caret(), 6);
    type_text(&mut chat, "X");
    assert_eq!(chat.input, "one twXo");
    chat.on_key(Key::Esc, Instant::now());
    assert_eq!(chat.caret(), 6, "and Esc steps back over it");

    // `a` after the character under the caret.
    vim_key(&mut chat, 'a');
    assert_eq!(chat.caret(), 7, "after the character under the caret");

    // `I` at the first non-blank, `A` at the end.
    chat.on_key(Key::Esc, Instant::now());
    vim_key(&mut chat, 'I');
    assert_eq!(chat.caret(), 0);
    chat.on_key(Key::Esc, Instant::now());
    assert_eq!(chat.caret(), 0, "nothing before the first character");
    vim_key(&mut chat, 'A');
    assert_eq!(chat.caret(), 8);
    assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Insert));

    // And Normal swallows a printable key rather than typing it.
    chat.on_key(Key::Esc, Instant::now());
    vim_key(&mut chat, 'z');
    assert_eq!(chat.input, "one twXo", "nothing was typed");
}

/// The motions, through the key path: a character, a word, and the three
/// edges of the draft.
#[test]
fn normal_motions_move_the_caret() {
    let mut chat = vim_normal_with("one two");
    assert_eq!(chat.caret(), 6, "where Esc left it");

    vim_key(&mut chat, 'h');
    assert_eq!(chat.caret(), 5, "h");
    vim_key(&mut chat, 'l');
    assert_eq!(chat.caret(), 6, "l");
    vim_key(&mut chat, 'b');
    assert_eq!(chat.caret(), 4, "b: the word before");
    vim_key(&mut chat, 'w');
    assert_eq!(chat.caret(), 7, "w: the next word's start");
    vim_key(&mut chat, '0');
    assert_eq!(chat.caret(), 0, "0");
    vim_key(&mut chat, '$');
    assert_eq!(chat.caret(), 7, "$");
    vim_key(&mut chat, '0');
    vim_key(&mut chat, 'e');
    assert_eq!(chat.caret(), 2, "e: the end of the word");

    // `^` skips the spaces the draft opens with.
    let mut chat = vim_normal_with("   two");
    vim_key(&mut chat, '0');
    assert_eq!(chat.caret(), 0);
    vim_key(&mut chat, '^');
    assert_eq!(chat.caret(), 3, "^: the first non-blank");
}

/// A count repeats the motion: `3w`, and a digit that extends one.
#[test]
fn a_count_repeats_the_motion() {
    let mut chat = vim_normal_with("a b c d");
    vim_key(&mut chat, '0');
    vim_key(&mut chat, '3');
    vim_key(&mut chat, 'w');
    assert_eq!(chat.caret(), 6, "3w");
    vim_key(&mut chat, '1');
    vim_key(&mut chat, '0');
    vim_key(&mut chat, 'h');
    assert_eq!(chat.caret(), 0, "10h past the start stops there");
    vim_key(&mut chat, '2');
    vim_key(&mut chat, 'l');
    assert_eq!(chat.caret(), 2, "2l");
}

/// The edits: the character under the caret and the one before it, with
/// the caret left where vim leaves it.
#[test]
fn normal_edits_take_the_character_under_the_caret() {
    let mut chat = vim_normal_with("abc");
    assert_eq!(chat.caret(), 2);
    vim_key(&mut chat, '0');
    vim_key(&mut chat, 'x');
    assert_eq!(chat.input, "bc", "x");
    assert_eq!(chat.caret(), 0);
    vim_key(&mut chat, 'X');
    assert_eq!(chat.input, "bc", "X at the start takes nothing");
    vim_key(&mut chat, 'l');
    vim_key(&mut chat, 'X');
    assert_eq!(chat.input, "c", "X");
    assert_eq!(chat.caret(), 0);

    // `2x` takes two characters, and `s` opens Insert where they were.
    let mut chat = vim_normal_with("abcd");
    vim_key(&mut chat, '0');
    vim_key(&mut chat, '2');
    vim_key(&mut chat, 'x');
    assert_eq!(chat.input, "cd", "2x");
    assert_eq!(
        chat.vim_mode(),
        Some(crate::vim::VimMode::Normal),
        "x stays"
    );
    vim_key(&mut chat, 's');
    assert_eq!(chat.input, "d", "s takes the character under the caret");
    assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Insert));
    type_text(&mut chat, "Z");
    assert_eq!(chat.input, "Zd");
}

/// The operators with a motion: `dw`, `db`, and the whole draft for `dd`.
#[test]
fn normal_operators_take_a_motion() {
    let mut chat = vim_normal_with("one two three");
    vim_key(&mut chat, '0');
    vim_key(&mut chat, 'd');
    vim_key(&mut chat, 'w');
    assert_eq!(chat.input, "two three", "dw");
    assert_eq!(chat.caret(), 0);
    assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Normal));

    vim_key(&mut chat, 'd');
    vim_key(&mut chat, 'b');
    assert_eq!(chat.input, "two three", "db at the start takes nothing");

    vim_key(&mut chat, 'l');
    vim_key(&mut chat, 'd');
    vim_key(&mut chat, 'b');
    assert_eq!(chat.input, "wo three", "db takes the word before");

    // `dd` is the whole draft, and `2dw` is the motion with a count.
    let mut chat = vim_normal_with("one two three");
    vim_key(&mut chat, '0');
    vim_key(&mut chat, '2');
    vim_key(&mut chat, 'd');
    vim_key(&mut chat, 'w');
    assert_eq!(chat.input, "three", "2dw");
    vim_key(&mut chat, 'd');
    vim_key(&mut chat, 'd');
    assert_eq!(chat.input, "", "dd");
    assert_eq!(chat.caret(), 0);
}

/// The change operators open Insert where the text was: `cw`, `cc`, `C`
/// and `S`. `cw` keeps vim's quirk — on a word it stops at the word's end
/// rather than taking the space after it.
#[test]
fn normal_changes_open_insert_where_the_text_was() {
    let mut chat = vim_normal_with("one two");
    vim_key(&mut chat, '0');
    vim_key(&mut chat, 'c');
    vim_key(&mut chat, 'w');
    assert_eq!(chat.input, " two", "cw: the word, not the space after it");
    assert_eq!(chat.caret(), 0);
    assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Insert));
    type_text(&mut chat, "ONE");
    assert_eq!(chat.input, "ONE two");

    // `C` takes the tail of the draft, `cc` and `S` take all of it.
    let mut chat = vim_normal_with("one two");
    vim_key(&mut chat, '0');
    vim_key(&mut chat, 'l');
    vim_key(&mut chat, 'C');
    assert_eq!(chat.input, "o", "C: to the end");
    assert_eq!(chat.caret(), 1);
    assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Insert));

    for key in ['c', 'S'] {
        let mut chat = vim_normal_with("one two");
        vim_key(&mut chat, key);
        if key == 'c' {
            vim_key(&mut chat, 'c');
        }
        assert_eq!(chat.input, "", "{key}: the whole draft");
        assert_eq!(chat.caret(), 0);
        assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Insert));
    }

    // `D` takes the tail and stays in Normal.
    let mut chat = vim_normal_with("one two");
    vim_key(&mut chat, '0');
    vim_key(&mut chat, 'D');
    assert_eq!(chat.input, "");
    assert_eq!(
        chat.vim_mode(),
        Some(crate::vim::VimMode::Normal),
        "D stays"
    );
}

/// The named keys a terminal has for vim's motions: the arrows are `h`/`l`,
/// home/end are `0`/`$`, delete is `x` and backspace is `h`. ↑/↓ keep
/// scrolling the transcript, which is what they do everywhere else.
#[test]
fn normal_reads_the_named_keys_as_motions() {
    let mut chat = vim_normal_with("abc");
    chat.on_key(Key::Left, Instant::now());
    assert_eq!(chat.caret(), 1, "← is h");
    chat.on_key(Key::Right, Instant::now());
    assert_eq!(chat.caret(), 2, "→ is l");
    chat.on_key(Key::Home, Instant::now());
    assert_eq!(chat.caret(), 0, "home is 0");
    chat.on_key(Key::End, Instant::now());
    assert_eq!(chat.caret(), 3, "end is $");
    chat.on_key(Key::Backspace, Instant::now());
    assert_eq!(chat.caret(), 2, "backspace is h");
    assert_eq!(chat.input, "abc", "and takes nothing");
    chat.on_key(Key::Delete, Instant::now());
    assert_eq!(chat.input, "ab", "delete is x");

    // ↑ still scrolls the transcript.
    let mut chat = vim_normal_with("abc");
    chat.on_key(Key::Up, Instant::now());
    assert_eq!(chat.scroll_offset, 1, "↑ is the transcript's");
}

/// Enter is not the mode's: in Normal it sends the draft, exactly as it
/// does in Insert (omp's editor leaves Enter to its host too).
#[test]
fn enter_in_normal_sends_the_draft() {
    let mut chat = vim_normal_with("hello");
    assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Normal));
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(applied.effect.is_some(), "the draft went to the engine");
    assert!(chat.input.is_empty(), "and the composer is clear");
}

/// Esc keeps its precedence: a list that is open is the list's, then the
/// mode's, and only a quiet Normal Esc reaches the screen's own clear and
/// rewind.
#[test]
fn esc_is_the_lists_then_the_modes_then_the_screens() {
    // A list that is open: Esc hides it and the mode does not move.
    let mut chat = vim_chat();
    type_text(&mut chat, "/he");
    assert!(chat.picking());
    chat.on_key(Key::Esc, Instant::now());
    assert!(chat.picker_hidden, "the list went");
    assert_eq!(
        chat.vim_mode(),
        Some(crate::vim::VimMode::Insert),
        "still Insert"
    );
    assert_eq!(chat.input, "/he", "and the draft stayed");

    // Insert: Esc is the mode's, and it does not clear the draft.
    let mut chat = vim_chat();
    type_text(&mut chat, "draft");
    chat.on_key(Key::Esc, Instant::now());
    assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Normal));
    assert_eq!(chat.input, "draft", "the draft stayed");

    // A half-typed operator is cancelled first, and stays in Normal.
    let mut chat = vim_normal_with("draft");
    vim_key(&mut chat, 'd');
    chat.on_key(Key::Esc, Instant::now());
    assert_eq!(chat.vim_mode(), Some(crate::vim::VimMode::Normal));
    assert_eq!(chat.input, "draft", "and nothing was cut");
    vim_key(&mut chat, 'd');
    assert_eq!(
        chat.input, "draft",
        "the `d` was cancelled, so this is not `dd`"
    );

    // A quiet Normal Esc on a draft is the screen's: it clears it.
    let mut chat = vim_normal_with("draft");
    chat.on_key(Key::Esc, Instant::now());
    assert_eq!(chat.input, "", "the screen's clear");

    // And on an empty draft the rewind chord is still the screen's: two
    // Normal presses inside the window, the first arming and the second
    // firing.
    let mut chat = vim_normal_chat();
    let now = Instant::now();
    chat.on_key(Key::Esc, now);
    assert!(chat.esc_armed.is_some(), "the first Normal Esc arms");
    chat.on_key(Key::Esc, now + Duration::from_millis(50));
    assert!(chat.esc_armed.is_none(), "the second spends the window");
    assert_eq!(
        chat.vim_mode(),
        Some(crate::vim::VimMode::Normal),
        "and it was not a mode change"
    );
}

/// A motion and an operator cross a paste marker whole, the same way
/// `ctrl+w` does: the caret never rests inside one and no cut leaves half
/// of one behind.
#[test]
fn a_marker_is_one_unit_to_a_vim_motion_and_a_vim_cut() {
    // Esc out of Insert steps back over the marker whole, so the caret
    // lands before it rather than inside it.
    let mut chat = vim_chat();
    type_text(&mut chat, "see ");
    chat.paste(&stack_trace());
    assert_eq!(chat.input, "see [Paste #1 · 8 lines]");
    let len = chat.input.len();
    chat.on_key(Key::Esc, Instant::now());
    assert_eq!(chat.caret(), 4, "Esc stepped back over the marker");

    // `w` from before the marker lands on its first character; the next
    // `w` would land on the space inside it, so it crosses it whole
    // instead of resting inside.
    vim_key(&mut chat, '0');
    vim_key(&mut chat, 'w');
    assert_eq!(chat.caret(), 4, "w: the marker's first character");
    vim_key(&mut chat, 'w');
    assert_eq!(chat.caret(), len, "w: across it whole");

    // `b` crosses back, and `e` rests on the marker's last character.
    vim_key(&mut chat, 'b');
    assert_eq!(chat.caret(), 4, "b: across it back");
    vim_key(&mut chat, 'e');
    assert_eq!(chat.caret(), len - 1, "e: its last character, not one past");

    // `x` there takes the whole marker, body and all.
    vim_key(&mut chat, 'x');
    assert_eq!(chat.input, "see ", "x took the marker whole");
    assert!(!chat.input.contains("[Paste"), "no half marker");
    assert!(chat.pastes.is_empty(), "and the body went with it");

    // The same cut through `dw`, from the marker's last character.
    let mut chat = vim_chat();
    type_text(&mut chat, "see ");
    chat.paste(&stack_trace());
    chat.on_key(Key::Esc, Instant::now());
    vim_key(&mut chat, '0');
    vim_key(&mut chat, 'w');
    vim_key(&mut chat, 'e');
    vim_key(&mut chat, 'd');
    vim_key(&mut chat, 'w');
    assert_eq!(chat.input, "see ", "dw took the marker whole");
    assert!(chat.pastes.is_empty());

    // `dd` takes a draft that is nothing but a marker, and `D` from before
    // it takes its tail.
    let mut chat = vim_normal_chat();
    chat.paste(&stack_trace());
    vim_key(&mut chat, 'd');
    vim_key(&mut chat, 'd');
    assert_eq!(chat.input, "");
    assert!(chat.pastes.is_empty(), "dd took the body with it");
}

/// The mode is in the composer's own border, and the draft it shows is the
/// same draft: `NORMAL` is up while Normal swallows typing, `INSERT` after
/// `i`.
#[test]
fn the_mode_is_shown_in_the_composer() {
    let mut vim = vim_chat();
    let insert = frame_text(&mut vim);
    assert!(insert.contains("INSERT"), "{insert}");
    assert!(!insert.contains("NORMAL"));
    vim.on_key(Key::Esc, Instant::now());
    let normal = frame_text(&mut vim);
    assert!(normal.contains("NORMAL"), "{normal}");
    assert!(!normal.contains("INSERT"));
    vim_key(&mut vim, 'i');
    assert!(frame_text(&mut vim).contains("INSERT"));

    // A chat without the setting has no chip at all.
    let mut bare = chat();
    let frame = frame_text(&mut bare);
    assert!(
        !frame.contains("NORMAL") && !frame.contains("INSERT"),
        "{frame}"
    );
}

/// `/hotkeys` lists the vim keys while the mode is on and nothing while it
/// is off, in the same column as the rest of the listing.
#[test]
fn hotkeys_lists_the_vim_keys_only_while_the_mode_is_on() {
    let mut vim = vim_normal_chat();
    vim.hotkeys();
    let text: String = vim
        .lines
        .iter()
        .map(|line| line.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("hotkeys · vim (editor.vim)"), "{text}");
    for key in ["h · l", "w · b · e", "x · X", "d w · d b · d d", "esc"] {
        assert!(text.contains(key), "{key} missing from {text}");
    }
    assert!(text.contains("d w · d b · d d  "), "the keys are padded");

    let mut bare = chat();
    bare.hotkeys();
    let text: String = bare
        .lines
        .iter()
        .map(|line| line.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains("editor.vim"), "{text}");
    assert!(text.contains("any character"), "the rest is still listed");
}

/// A word delete takes a paste marker whole: a marker holds spaces, so a
/// word taken out of the draft can be a piece of one — and half a marker
/// stands for nothing.
#[test]
fn a_word_delete_keeps_a_paste_marker_whole() {
    let mut chat = chat();
    type_text(&mut chat, "see ");
    chat.paste(&stack_trace());
    assert_eq!(chat.input, "see [Paste #1 · 8 lines]");
    assert_eq!(chat.pastes.len(), 1);

    // The caret is after the marker, so the word it is in is the marker's
    // own `lines]`: ctrl+w takes the marker whole and leaves the spaces
    // before it, which are not part of that word.
    chat.on_key(Key::DeleteWord, Instant::now());
    assert_eq!(chat.input, "see ");
    assert!(chat.pastes.is_empty(), "the body went with its marker");
    assert!(!chat.input.contains("[Paste"), "no half marker is left");

    // A marker in the middle of a word run goes whole too: the word is
    // `lines]bbb`, and the cut widens to the marker it runs through.
    let mut glued = chat_with_theme(test_theme());
    type_text(&mut glued, "aaa");
    glued.paste(&stack_trace());
    type_text(&mut glued, "bbb");
    assert_eq!(glued.input, "aaa[Paste #1 · 8 lines]bbb");
    glued.on_key(Key::DeleteWord, Instant::now());
    assert_eq!(glued.input, "aaa");
    assert!(!glued.input.contains("[Paste"), "no half marker is left");
    assert!(glued.pastes.is_empty());
}

/// The same rule for every other cut: backspace and delete take the marker
/// the character belongs to, and ctrl+u takes every marker it covers —
/// bodies and all.
#[test]
fn every_cut_takes_a_marker_whole() {
    // Backspace from the end: the `]` is inside the marker.
    let mut chat = chat();
    chat.paste(&stack_trace());
    chat.on_key(Key::Backspace, Instant::now());
    assert_eq!(chat.input, "");
    assert!(chat.pastes.is_empty());

    // Delete from the front: the `[` is inside the marker.
    let mut front = chat_with_theme(test_theme());
    front.paste(&stack_trace());
    front.on_key(Key::Home, Instant::now());
    front.on_key(Key::Delete, Instant::now());
    assert_eq!(front.input, "");
    assert!(front.pastes.is_empty());

    // ctrl+u takes everything before the caret, and the bodies of the
    // markers it took go with them.
    let mut kept = chat_with_theme(test_theme());
    type_text(&mut kept, "keep ");
    kept.paste(&stack_trace());
    type_text(&mut kept, " and this");
    kept.on_key(Key::End, Instant::now());
    kept.on_key(Key::DeleteToStart, Instant::now());
    assert_eq!(kept.input, "");
    assert!(kept.pastes.is_empty());
}

/// A completion cannot cut a marker: a marker opens with `[`, which is not
/// a name character, so no `/token` or `:query` ever spans one.
#[test]
fn a_completion_never_cuts_a_paste_marker() {
    let mut chat = chat();
    type_text(&mut chat, "/he");
    chat.paste(&stack_trace());
    assert_eq!(chat.input, "/he[Paste #1 · 8 lines]");
    assert!(
        !chat.picking(),
        "a token that would span the marker is not a token"
    );
    chat.on_key(Key::Tab, Instant::now());
    assert_eq!(chat.input, "/he[Paste #1 · 8 lines]", "nothing was cut");
    assert_eq!(chat.pastes.len(), 1, "and the body is still registered");
}

/// The browser lists the session's prompts, not the screen's lines: a note
/// or an assistant reply is not a prompt, and a session that was never
/// asked anything says so rather than opening an empty panel.
#[test]
fn the_history_is_the_sessions_prompts_and_an_empty_one_says_so() {
    let (_dir, mut chat) = history_chat();
    chat.push(LineKind::Note, "/help".to_owned());
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "not a prompt".into(),
    });
    chat.on_key(Key::CtrlR, Instant::now());
    assert!(chat.history_picker.is_none(), "nothing to browse");
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("history: this session has no prompts")),
        "{:?}",
        chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
    );

    // A prompt the session did carry is listed once, whole.
    let log = SessionLog::open(&chat.agent_dir, &chat.session_id);
    ask(&mut chat, &log, "what did I ask earlier");
    chat.clear_input();
    chat.on_key(Key::CtrlR, Instant::now());
    let frame = frame_rows(&mut chat, 80, 20).join("\n");
    let panel = &frame[frame.find("history · 1").expect("the browser")..];
    assert!(panel.contains("what did I ask earlier"), "{panel}");
}
/// Alt+Backspace and Ctrl+W delete a word at a time; an empty composer (or
/// one holding only spaces) costs nothing.
#[test]
fn delete_word_takes_the_word_before_the_caret() {
    let mut chat = chat();
    type_text(&mut chat, "fix the parser now");
    chat.on_key(Key::DeleteWord, Instant::now());
    assert_eq!(chat.input, "fix the parser ");
    chat.on_key(Key::DeleteWord, Instant::now());
    assert_eq!(chat.input, "fix the ");
    type_text(&mut chat, "now   ");
    chat.on_key(Key::DeleteWord, Instant::now());
    assert_eq!(chat.input, "fix the ", "the spaces go with the word");

    // Nothing to delete is not an error and not a panic.
    chat.clear_input();
    chat.on_key(Key::DeleteWord, Instant::now());
    assert_eq!(chat.input, "");
    chat.input = "   ".to_owned();
    chat.caret_to_end();
    chat.on_key(Key::DeleteWord, Instant::now());
    assert_eq!(chat.input, "");

    // A wide character is one character, not two cells' worth of bytes.
    chat.input = "日本 語".to_owned();
    chat.caret_to_end();
    chat.on_key(Key::DeleteWord, Instant::now());
    assert_eq!(chat.input, "日本 ");
}

/// A stored session with one question in it, named the way the index
/// remembers a name a person or the namer gave it — a title at creation is
/// the placeholder a session is made with, not a name (`needs_auto_title`).
fn seed_session(agent_dir: &Path, title: &str, question: &str) -> String {
    let store = titi_core::session::SessionStore::new(agent_dir).expect("session store");

    let id = store
        .create(titi_core::session::SessionMeta {
            title: Some(title.to_owned()),
            ..Default::default()
        })
        .expect("create");
    store.append(&id, Role::User, question).expect("append");
    titi_core::session::SessionIndex::open(&agent_dir.join("state.db"))
        .expect("index")
        .set_title(&id, title)
        .expect("title");
    id
}

/// The tree shows what the store holds: every branch, indented by depth,
/// the path to the leaf marked and the leaf named. A session that branched
/// has entries off the path, and the title says how many.
#[test]
fn the_tree_shows_every_branch_with_the_leaf_marked() {
    let (dir, id, store) = branched_session();
    let mut chat = Chat::new("openai/gpt-4.1", &id, test_theme());
    chat.agent_dir = dir.path().to_path_buf();

    type_text(&mut chat, "/tree");
    chat.on_key(Key::Enter, Instant::now());
    let picker = chat.tree_picker.as_ref().expect("the tree is open");
    let rows: Vec<&str> = picker.rows.iter().map(|row| row.text.as_str()).collect();
    assert_eq!(
        rows,
        [
            "• you   one",
            "  • titi  two",
            "      you   three",
            "    • titi  other  ✓ current",
        ],
        "the branch left behind is a row of its own"
    );
    assert_eq!(picker.off_path, 1, "`three` is off the path now");
    assert_eq!(picker.selected, 3, "the cursor starts on the leaf");

    // The panel draws it, and the title counts what a move would leave.
    let view = panel_view_for(&chat, 30, 100).expect("a panel");
    let title = view.title.clone().unwrap_or_default();
    assert!(title.contains("4 entries · 1 off this path"), "{title:?}");
    // Esc closes it and leaves the store alone.
    chat.on_key(Key::Esc, Instant::now());
    assert!(chat.tree_picker.is_none());
    assert_eq!(store.open(&id).expect("entries").len(), 4);
}

/// A session with the shape a working turn leaves: your prompt, the
/// answer, the tool call, and the answer that followed it.
fn tool_session() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("temp");
    let store = titi_core::session::SessionStore::new(dir.path()).expect("store");
    let id = store
        .create(titi_core::session::SessionMeta::default())
        .expect("session");
    store
        .append(&id, Role::User, "read the config")
        .expect("you");
    store
        .append(&id, Role::Assistant, "reading it")
        .expect("titi");
    store.append(&id, Role::Tool, "fn main() {}").expect("tool");
    store
        .append(&id, Role::Assistant, "the config reads fine")
        .expect("after");
    (dir, id)
}

/// The tree rows of an open picker, as the panel draws them.
fn tree_rows(chat: &Chat) -> Vec<String> {
    chat.tree_picker
        .as_ref()
        .expect("the tree is open")
        .rows
        .iter()
        .map(|row| row.text.clone())
        .collect()
}

/// `alt+f` narrows the tree, and the tree stays one tree: an entry whose
/// parent the filter hides hangs from the nearest entry the filter keeps
/// instead of breaking off the trunk and landing at depth 0.
#[test]
fn the_tree_filter_hides_the_tool_traffic_and_keeps_the_tree_whole() {
    let (dir, id) = tool_session();
    let mut chat = Chat::new("openai/gpt-4.1", &id, test_theme());
    chat.agent_dir = dir.path().to_path_buf();

    type_text(&mut chat, "/tree");
    chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        tree_rows(&chat),
        [
            "• you   read the config",
            "  • titi  reading it",
            "    • tool  fn main() {}",
            "      • titi  the config reads fine  ✓ current",
        ]
    );

    // alt+f: everything but the tool traffic. The answer that followed the
    // tool call keeps its place under the message above it.
    chat.on_key(Key::AltF, Instant::now());
    assert_eq!(
        tree_rows(&chat),
        [
            "• you   read the config",
            "  • titi  reading it",
            "    • titi  the config reads fine  ✓ current",
        ],
        "the tool row is gone and the tree is still one tree"
    );
    let picker = chat.tree_picker.as_ref().expect("the tree is open");
    assert_eq!(picker.filter, TreeFilter::NoTools);
    assert_eq!(picker.selected, 2, "the cursor followed the leaf");
    assert_eq!(picker.off_path, 0, "everything left is on the path");

    // Once more: only what you said, and the cursor is on the entry it was
    // on when that entry survives.
    chat.on_key(Key::AltF, Instant::now());
    assert_eq!(tree_rows(&chat), ["• you   read the config"]);
    let picker = chat.tree_picker.as_ref().expect("the tree is open");
    assert_eq!(picker.filter, TreeFilter::UserOnly);
    assert_eq!(picker.selected, 0, "the only row there is");
    assert_eq!(picker.off_path, 0);

    // And round again: the whole tree, with the cursor on the entry it
    // was on — your prompt, which every filter shows.
    chat.on_key(Key::AltF, Instant::now());
    assert_eq!(tree_rows(&chat).len(), 4);
    let picker = chat.tree_picker.as_ref().expect("the tree is open");
    assert_eq!(picker.filter, TreeFilter::Default);
    assert_eq!(picker.selected, 0, "the entry the cursor was on");
}

/// The filter is a view, not a move: Enter after a filter still branches
/// on the row the cursor is on, and Esc leaves the store alone.
#[test]
fn a_filtered_tree_still_branches_where_the_cursor_is() {
    let (dir, id) = tool_session();
    let mut chat = Chat::new("openai/gpt-4.1", &id, test_theme());
    chat.agent_dir = dir.path().to_path_buf();

    type_text(&mut chat, "/tree");
    chat.on_key(Key::Enter, Instant::now());
    chat.on_key(Key::AltF, Instant::now());
    assert_eq!(
        chat.tree_picker.as_ref().expect("open").filter,
        TreeFilter::NoTools
    );

    // Up twice: from the leaf (the last row) to your own prompt.
    chat.on_key(Key::Up, Instant::now());
    chat.on_key(Key::Up, Instant::now());
    let picker = chat.tree_picker.as_ref().expect("the tree is open");
    assert_eq!(picker.selected, 0);
    assert_eq!(picker.selected_id(), Some(picker.rows[0].entry_id.as_str()));
    chat.on_key(Key::Esc, Instant::now());
    assert!(chat.tree_picker.is_none());
    let store = titi_core::session::SessionStore::new(dir.path()).expect("store");
    assert_eq!(store.open(&id).expect("entries").len(), 4, "nothing moved");
}

/// The setting names the filter the panel opens in, and an unknown name is
/// the whole tree rather than a refusal to start.
#[test]
fn the_tree_filter_mode_setting_names_the_opening_filter() {
    let dir = tempfile::tempdir().expect("temp");
    let project = tempfile::tempdir().expect("temp");
    let settings = |text: &str| {
        std::fs::write(dir.path().join("config.yml"), text).expect("write");
        titi_config::settings::Settings::load(dir.path(), project.path(), &[]).expect("load")
    };
    assert_eq!(tree_filter(None), TreeFilter::Default);
    assert_eq!(
        tree_filter(Some(&settings("treeFilterMode: no-tools\n"))),
        TreeFilter::NoTools
    );
    assert_eq!(
        tree_filter(Some(&settings("treeFilterMode: user-only\n"))),
        TreeFilter::UserOnly
    );
    assert_eq!(
        tree_filter(Some(&settings("treeFilterMode: default\n"))),
        TreeFilter::Default
    );
    assert_eq!(
        tree_filter(Some(&settings("treeFilterMode: everything\n"))),
        TreeFilter::Default,
        "a typo leaves the tree as it was"
    );
    assert_eq!(
        tree_filter(Some(&settings("treeFilterMode: 7\n"))),
        TreeFilter::Default
    );

    // And the panel opens in it, and says so in the title.
    let (dir, id) = tool_session();
    let mut chat = Chat::new("openai/gpt-4.1", &id, test_theme());
    chat.agent_dir = dir.path().to_path_buf();
    chat.tree_filter = TreeFilter::NoTools;
    type_text(&mut chat, "/tree");
    chat.on_key(Key::Enter, Instant::now());
    assert_eq!(tree_rows(&chat).len(), 3, "the tool row is not drawn");
    let view = panel_view_for(&chat, 30, 100).expect("a panel");
    let title = view.title.clone().unwrap_or_default();
    assert!(
        title.contains("tree · 3 entries · no-tools · 0 off this path"),
        "{title:?}"
    );

    // The default view does not name a filter it is not.
    chat.tree_filter = TreeFilter::Default;
    chat.tree_picker = None;
    type_text(&mut chat, "/tree");
    chat.on_key(Key::Enter, Instant::now());
    let view = panel_view_for(&chat, 30, 100).expect("a panel");
    let title = view.title.clone().unwrap_or_default();
    assert!(
        title.contains("tree · 4 entries · 0 off this path"),
        "{title:?}"
    );
}

/// One live agent, started by the engine.
fn agent_started(name: &str) -> EngineEvent {
    EngineEvent::AgentStarted {
        agent_id: name.to_owned().into(),
        name: name.into(),
        parent_id: Some("Main".into()),
        kind: titi_engine::AgentKind::Subagent,
    }
}

fn agent_finished(name: &str, success: bool) -> EngineEvent {
    EngineEvent::AgentFinished {
        agent_id: name.to_owned().into(),
        summary: format!("{name} did the thing").into(),
        success,
    }
}

/// The strip lists the live agents — several at once, finishing out of
/// order — and an agent that ends leaves it. `off` draws nothing, `full`
/// draws all of them, and `collapsed` counts the overflow.
#[test]
fn the_strip_lists_live_agents_and_they_leave_in_any_order() {
    let strip = |frame: &[String]| {
        frame
            .iter()
            .filter(|row| {
                ["alpha", "beta", "gamma", "delta"]
                    .iter()
                    .any(|n| row.contains(n))
            })
            .count()
    };
    let mut chat = chat();
    chat.on_event(agent_started("alpha"));
    chat.on_event(agent_started("beta"));
    chat.on_event(agent_started("gamma"));
    let frame = frame_rows(&mut chat, 80, 24);
    assert_eq!(strip(&frame), 3, "{frame:#?}");
    assert!(
        frame.iter().any(|row| row.contains("alpha")),
        "a row per live agent: {frame:#?}"
    );

    // The fourth is one too many for `collapsed`: it is counted, not drawn.
    chat.on_event(agent_started("delta"));
    let frame = frame_rows(&mut chat, 80, 24);
    assert_eq!(strip(&frame), 3);
    assert!(
        frame.iter().any(|row| row.contains("… 1 more")),
        "{frame:#?}"
    );

    // Out of order: the middle agent finishes on its own.
    chat.on_event(EngineEvent::AgentStatusChanged {
        agent_id: "beta".into(),
        status: titi_engine::AgentStatus::Completed,
    });
    assert_eq!(chat.agents.len(), 3);
    let frame = frame_rows(&mut chat, 80, 24);
    assert!(!frame.iter().any(|row| row.contains("beta")), "{frame:#?}");
    assert!(frame.iter().any(|row| row.contains("gamma")), "{frame:#?}");

    // `full` lists every one the pane has room for.
    chat.pinned.mode = PinnedAgents::Full;
    let frame = frame_rows(&mut chat, 80, 24);
    assert_eq!(strip(&frame), 3, "alpha, gamma and delta: {frame:#?}");
    assert!(!frame.iter().any(|row| row.contains("more")), "{frame:#?}");

    // A focused agent's row carries the marker.
    chat.agent_focus = Some("gamma".to_owned());
    let frame = frame_rows(&mut chat, 80, 24);
    assert!(
        frame
            .iter()
            .any(|row| row.contains('▸') && row.contains("gamma")),
        "the focused row is marked: {frame:#?}"
    );
    chat.agent_focus = None;

    // `off` draws no strip at all, and the turn's own rows are untouched.
    chat.pinned.mode = PinnedAgents::Off;
    let frame = frame_rows(&mut chat, 80, 24);
    assert_eq!(strip(&frame), 0, "{frame:#?}");
    chat.pinned.mode = PinnedAgents::Collapsed;

    // A finished agent leaves the strip and stays a note: the outcome is
    // the one thing about it the strip cannot show.
    chat.on_event(agent_finished("alpha", true));
    assert_eq!(chat.agents.len(), 2);
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("tool done")),
        "the outcome is still a line in the transcript: {:#?}",
        chat.lines
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>()
    );
}

/// `AgentProgress` is the agent's own text: the preview may show its tail,
/// but it never becomes a line of the parent's transcript.
#[test]
fn an_agents_words_go_to_its_row_and_never_to_the_transcript() {
    let mut chat = chat();
    chat.on_event(agent_started("alpha"));
    chat.on_event(EngineEvent::AgentProgress {
        agent_id: "alpha".into(),
        text: "looking at the parser now".into(),
    });
    assert_eq!(chat.agents[0].answer, "looking at the parser now");
    assert!(
        !chat
            .lines
            .iter()
            .any(|line| line.text.contains("parser now")),
        "the parent's transcript is untouched"
    );
    let frame = frame_rows(&mut chat, 80, 24);
    // The preview is off by default, so the row is a name and a state.
    assert!(
        !frame.iter().any(|row| row.contains("parser now")),
        "{frame:#?}"
    );

    chat.pinned.preview = true;
    let frame = frame_rows(&mut chat, 80, 24);
    assert!(
        frame
            .iter()
            .any(|row| row.contains("looking at the parser now")),
        "the preview shows what it said when the engine sent no activity: {frame:#?}"
    );

    // The engine's own activity line wins over the answer's tail.
    chat.on_event(EngineEvent::AgentActivity {
        agent_id: "alpha".into(),
        text: "tools: read, grep".into(),
    });
    let frame = frame_rows(&mut chat, 80, 24);
    assert!(
        frame.iter().any(|row| row.contains("tools: read, grep")),
        "{frame:#?}"
    );
    assert!(
        !frame.iter().any(|row| row.contains("parser now")),
        "{frame:#?}"
    );
}

/// `alt+a` walks the jump list — an agent's pane, the next one, and back to
/// the turn — and Esc returns from a pane.
#[test]
fn alt_a_walks_the_agents_and_esc_comes_back() {
    let mut chat = chat();
    chat.on_event(agent_started("alpha"));
    chat.on_event(agent_started("beta"));
    chat.on_event(EngineEvent::AgentProgress {
        agent_id: "beta".into(),
        text: "the second agent's answer".into(),
    });

    // No live agent: the key is the main turn either way.
    let mut empty = chat_with_theme(test_theme());
    assert!(empty.on_key(Key::AltA, Instant::now()).effect.is_none());
    assert!(empty.agent_focus.is_none());

    let focus_of = |applied: &Applied| match &applied.effect {
        Some(ChatEffect::Send(EngineCommand::FocusAgent { agent_id })) => {
            Some(agent_id.to_string())
        }
        _ => None,
    };

    let applied = chat.on_key(Key::AltA, Instant::now());
    assert_eq!(focus_of(&applied).as_deref(), Some("alpha"));
    assert_eq!(chat.agents.len(), 2);
    assert!(chat.focused_agent().is_some());
    // The pane: the agent's own header, its state, and — because it has
    // said nothing yet — that it has said nothing yet.
    let head = |rows: &[String]| rows.iter().take(6).cloned().collect::<Vec<_>>();
    let frame = frame_rows(&mut chat, 80, 24);
    assert!(
        frame
            .iter()
            .any(|row| row.contains("agent alpha · running")),
        "the pane's header: {:#?}",
        head(&frame)
    );
    assert!(
        frame
            .iter()
            .any(|row| row.contains("waiting for its first words")),
        "and its empty body says so: {:#?}",
        head(&frame)
    );
    assert!(
        !frame.iter().any(|row| row.contains("subagents (")),
        "the transcript is not the body while a pane has it: {:#?}",
        head(&frame)
    );

    let applied = chat.on_key(Key::AltA, Instant::now());
    assert_eq!(focus_of(&applied).as_deref(), Some("beta"));
    assert_eq!(chat.agent_focus.as_deref(), Some("beta"));
    let frame = frame_rows(&mut chat, 80, 24);
    assert!(
        frame.iter().any(|row| row.contains("agent beta · running")),
        "the next agent's pane: {:#?}",
        head(&frame)
    );
    assert!(
        frame
            .iter()
            .any(|row| row.contains("the second agent's answer")),
        "the pane is its own streamed text: {:#?}",
        head(&frame)
    );

    // Round to the main turn: the screen's own move, and no command.
    let applied = chat.on_key(Key::AltA, Instant::now());
    assert!(applied.effect.is_none(), "the engine has no form for it");
    assert!(chat.agent_focus.is_none());
    let frame = frame_rows(&mut chat, 80, 24);
    assert!(
        !frame.iter().any(|row| row.contains("agent beta ·")),
        "{frame:#?}"
    );

    // A pane is Esc's next stop after an open list.
    chat.on_key(Key::AltA, Instant::now());
    assert_eq!(chat.agent_focus.as_deref(), Some("alpha"));
    chat.on_key(Key::Esc, Instant::now());
    assert!(chat.agent_focus.is_none(), "Esc comes back to the turn");

    // An agent that ends while its pane has the view takes the view with
    // it, so the screen never shows a pane that is gone.
    chat.on_key(Key::AltA, Instant::now());
    chat.on_event(agent_finished("alpha", false));
    assert!(chat.agent_focus.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.contains("tool error")),
        "the outcome is a transcript line (the section folds it): {:#?}",
        chat.lines
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>()
    );
}

/// A click on a pinned row focuses that agent, and a click anywhere else is
/// still a selection.
#[test]
fn a_click_on_a_pinned_row_focuses_that_agent() {
    let mut chat = chat();
    chat.on_event(agent_started("alpha"));
    chat.on_event(agent_started("beta"));
    let _ = frame_rows(&mut chat, 80, 24);
    let first = chat.pinned_top;
    assert!(chat.pinned_rows >= 2, "the strip drew its rows");

    let applied = chat.mouse_press(2, first).expect("a jump, not a selection");
    match applied.effect {
        Some(ChatEffect::Send(EngineCommand::FocusAgent { agent_id })) => {
            assert_eq!(agent_id.as_str(), "alpha");
        }
        other => panic!("expected FocusAgent, got {other:?}"),
    }
    assert_eq!(chat.agent_focus.as_deref(), Some("alpha"));
    assert!(
        chat.selection.is_none(),
        "the strip is chrome, not transcript"
    );

    // Below the strip the press is a selection as it always was.
    assert!(chat.mouse_press(2, chat.transcript_top + 1).is_none());
    assert!(chat.selection.is_some());
}

/// The two settings, from a real config file, and the glyph per state.
#[test]
fn the_pinned_settings_and_the_state_glyphs() {
    let dir = tempfile::tempdir().expect("temp");
    let project = tempfile::tempdir().expect("temp");
    let read = |text: &str| {
        std::fs::write(dir.path().join("config.yml"), text).expect("write");
        titi_config::settings::Settings::load(dir.path(), project.path(), &[]).expect("load")
    };
    assert_eq!(
        pinned_agents(None),
        PinnedStrip::default(),
        "unset = collapsed"
    );
    assert_eq!(
        pinned_agents(Some(&read("display:\n  pinnedAgents: off\n"))).mode,
        PinnedAgents::Off
    );
    assert_eq!(
        pinned_agents(Some(&read("display:\n  pinnedAgents: full\n"))).mode,
        PinnedAgents::Full
    );
    assert_eq!(
        pinned_agents(Some(&read("display:\n  pinnedAgents: hoops\n"))).mode,
        PinnedAgents::Collapsed,
        "a typo leaves the strip as it was"
    );
    assert!(!pinned_agents(Some(&read("theme:\n  dark: titanium\n"))).preview);
    assert!(
        pinned_agents(Some(&read("display:\n  subagentLivePreview: true\n"))).preview,
        "the preview is its own switch"
    );

    // The names round-trip, and a running agent's glyph moves with the
    // clock the working row already uses.
    for mode in [
        PinnedAgents::Off,
        PinnedAgents::Collapsed,
        PinnedAgents::Full,
    ] {
        assert_eq!(PinnedAgents::parse(mode.id()), Some(mode));
    }
    assert_eq!(
        agent_glyph(titi_engine::AgentStatus::Parked, Duration::ZERO),
        "‖"
    );
    assert_eq!(
        agent_glyph(titi_engine::AgentStatus::Failed, Duration::ZERO),
        "✗"
    );
    assert_eq!(
        agent_glyph(titi_engine::AgentStatus::Running, Duration::from_millis(0)),
        spinner_frame(Duration::ZERO)
    );
    assert_ne!(
        agent_glyph(titi_engine::AgentStatus::Running, Duration::ZERO),
        agent_glyph(
            titi_engine::AgentStatus::Running,
            Duration::from_millis(200)
        ),
        "and it moves"
    );
}

/// A session switch forgets the session it left — its live agents and any
/// focus on one of them included. The strip is that session's state, and a
/// switch that kept it would show a dead session's agents under the new
/// one's masthead.
#[test]
fn a_session_switch_forgets_the_live_agents_it_left() {
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    let other = seed_session(dir.path(), "other", "another question");

    chat.on_event(agent_started("alpha"));
    chat.on_event(agent_started("beta"));
    chat.agent_focus = Some("alpha".to_owned());
    assert_eq!(chat.agents.len(), 2);

    chat.switch_to_session(other);
    assert!(chat.agents.is_empty(), "the old session's agents are gone");
    assert!(chat.agent_focus.is_none(), "and so is its focus");
    let frame = frame_rows(&mut chat, 80, 24);
    assert!(
        !frame.iter().any(|row| row.contains("alpha")),
        "the strip does not carry them over: {frame:#?}"
    );
}

/// A mermaid fence in an answer reaches the screen as a diagram, and the
/// setting is what decides: off, the same fence is the code box it always was.
///
/// This is the integration the module's own tests cannot show: the fence goes
/// through the markdown renderer, the transcript and the frame.
#[test]
fn a_mermaid_fence_is_drawn_and_the_setting_turns_it_off() {
    let fence = "```mermaid\nflowchart TD\n  A[Start] --> B[Done]\n```";
    let frame = |mermaid: bool| {
        let mut chat = chat_with_theme(test_theme());
        chat.mermaid = mermaid;
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: fence.into(),
        });
        chat.on_event(EngineEvent::TurnFinished {
            turn_id: TurnId(1),
            reason: StopReason::Stop,
        });
        frame_rows(&mut chat, 80, 30)
            .iter()
            .map(|row| row.trim_end().to_owned())
            .collect::<Vec<_>>()
    };

    let drawn = frame(true);
    assert!(
        drawn.iter().any(|row| row.contains("Start")),
        "the node text is drawn: {drawn:#?}"
    );
    assert!(
        drawn
            .iter()
            .any(|row| row.contains('┌') || row.contains('+')),
        "and a box: {drawn:#?}"
    );
    assert!(
        !drawn.iter().any(|row| row.contains("flowchart TD")),
        "the source is not drawn: {drawn:#?}"
    );

    let off = frame(false);
    assert!(
        off.iter().any(|row| row.contains("flowchart TD")),
        "off, the fence is its own source, as it always was: {off:#?}"
    );
    assert!(
        !off.iter().any(|row| row.contains('┌')),
        "and no diagram: {off:#?}"
    );
}

/// `display.smoothStreaming` off — the default — is the delta path exactly as
/// it was: the frame after a delta draws the whole buffer.
#[test]
fn smooth_streaming_off_is_the_frame_it_always_was() {
    let mut chat = chat();
    type_text(&mut chat, "hello");
    chat.on_key(Key::Enter, Instant::now());
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "the whole answer".into(),
    });
    assert!(!chat.smooth, "unset is off");
    let frame = frame_rows(&mut chat, 80, 24);
    assert!(
        frame.iter().any(|row| row.contains("the whole answer")),
        "the whole buffer, at once: {frame:#?}"
    );
    // And the hundreds of frames a turn would tick change nothing.
    for _ in 0..20 {
        chat.reveal_tick(Instant::now());
    }
    let frame = frame_rows(&mut chat, 80, 24);
    assert!(
        frame.iter().any(|row| row.contains("the whole answer")),
        "and still the whole buffer: {frame:#?}"
    );
}

/// On, the reveal paces the text: the frame after a delta shows a prefix, the
/// next frame more, and everything at a tool call — nothing is lost or held
/// past the line's end.
#[test]
fn smooth_streaming_reveals_a_prefix_and_settles_at_a_tool_call() {
    let mut pacing = chat_with_theme(test_theme());
    pacing.smooth = true;
    type_text(&mut pacing, "hello");
    pacing.on_key(Key::Enter, Instant::now());
    pacing.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "abcdefghijklmnopqrstuvwxyz".into(),
    });

    // The first frame reveals the minimum, not the whole buffer.
    let at = Instant::now();
    pacing.reveal_tick(at);
    pacing.reveal_tick(at + crate::reveal::FRAME);
    let frame = frame_rows(&mut pacing, 80, 24);
    assert!(
        !frame.iter().any(|row| row.contains("abcdefghij")),
        "a prefix, not the buffer: {frame:#?}"
    );
    assert!(
        frame.iter().any(|row| row.contains("abc")),
        "and at least the minimum step: {frame:#?}"
    );
    assert!(
        !frame.iter().any(|row| row.contains("xyz")),
        "the tail waits: {frame:#?}"
    );

    // Enough frames and the whole answer is there.
    for step in 0..40 {
        pacing.reveal_tick(at + crate::reveal::FRAME * (step + 2));
    }
    let frame = frame_rows(&mut pacing, 80, 24);
    assert!(
        frame
            .iter()
            .any(|row| row.contains("abcdefghijklmnopqrstuvwxyz")),
        "caught up: {frame:#?}"
    );

    // A tool call closes the line, so a reveal still in flight settles: the
    // line is whole from the frame the tool call lands in.
    let mut chat = chat();
    pacing.smooth = true;
    type_text(&mut pacing, "hello");
    pacing.on_key(Key::Enter, Instant::now());
    pacing.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: "abcdefghijklmnopqrstuvwxyz".into(),
    });
    pacing.reveal_tick(at);
    pacing.on_event(EngineEvent::ToolStarted {
        turn_id: TurnId(1),
        call_id: "call-1".into(),
        name: "bash".into(),
        detail: None,
    });
    let frame = frame_rows(&mut pacing, 80, 24);
    assert!(
        frame
            .iter()
            .any(|row| row.contains("abcdefghijklmnopqrstuvwxyz")),
        "settled at the tool call: {frame:#?}"
    );
}

/// The turn's end, a failure and a cancel settle too: a finished answer is
/// whole, whatever the reveal had reached.
#[test]
fn a_finished_turn_is_never_left_half_revealed() {
    let ended = |end: &str| {
        let mut chat = chat_with_theme(test_theme());
        chat.smooth = true;
        type_text(&mut chat, "hello");
        chat.on_key(Key::Enter, Instant::now());
        chat.on_event(EngineEvent::StreamDelta {
            turn_id: TurnId(1),
            text: "abcdefghijklmnopqrstuvwxyz".into(),
        });
        chat.reveal_tick(Instant::now());
        // A failure ends the turn only for the turn it names
        // (`finish_turn`'s guard), so the engine's start has to be here for
        // the failed case to reach the same settle the other two do.
        chat.on_event(EngineEvent::TurnStarted {
            turn_id: TurnId(1),
            model: "openai/gpt-4.1".into(),
        });
        chat.turn_active = true;
        match end {
            "finished" => chat.on_event(EngineEvent::TurnFinished {
                turn_id: TurnId(1),
                reason: StopReason::Stop,
            }),
            "failed" => chat.on_event(EngineEvent::Failed {
                turn_id: Some(TurnId(1)),
                reason: titi_providers::ErrorReason::Rejected,
                message: "no".into(),
            }),
            _ => chat.on_event(EngineEvent::Cancelled { turn_id: TurnId(1) }),
        };
        let frame = frame_rows(&mut chat, 80, 24);
        assert!(
            frame
                .iter()
                .any(|row| row.contains("abcdefghijklmnopqrstuvwxyz")),
            "{end}: the answer is whole after it: {frame:#?}"
        );
    };
    ended("finished");
    ended("failed");
    ended("cancelled");
}

/// The prefix the screen slices is always a character boundary, whatever the
/// reveal has reached — a multi-byte answer is never cut in half.
#[test]
fn the_revealed_prefix_never_splits_a_character() {
    let mut chat = chat();
    chat.smooth = true;
    type_text(&mut chat, "hello");
    chat.on_key(Key::Enter, Instant::now());
    let answer = "aé世🎉z";
    chat.on_event(EngineEvent::StreamDelta {
        turn_id: TurnId(1),
        text: answer.into(),
    });
    let at = Instant::now();
    for step in 0..60 {
        chat.reveal_tick(at + crate::reveal::FRAME * (step + 1));
        let shown = chat.revealed_prefix(&chat.reply);
        assert!(answer.starts_with(shown), "a prefix: {shown:?}");
        assert_eq!(
            shown.chars().count(),
            chat.revealed.min(answer.chars().count())
        );
    }
    assert_eq!(
        chat.revealed_prefix(&chat.reply),
        answer,
        "and it ends whole"
    );
}

/// `/changelog` renders the notes this build carries, and an argument it
/// does not know gets the usage line rather than silence.
#[test]
fn the_changelog_command_renders_the_builds_notes() {
    let notes = |chat: &Chat| {
        chat.lines
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut chat = chat();
    command(&mut chat, "/changelog");
    let text = notes(&chat);
    assert!(text.contains("changelog · Unreleased"), "{text}");
    assert!(text.contains("- "), "the entries themselves: {text}");

    // `full` is at least as much, and `last 1` no more.
    let mut full = chat_with_theme(test_theme());
    command(&mut full, "/changelog full");
    assert!(notes(&full).len() >= text.len());

    let mut last = chat_with_theme(test_theme());
    command(&mut last, "/changelog last 1");
    assert!(notes(&last).contains("changelog · "));

    // Anything else is the usage line, as an error rather than a note.
    let mut bad = chat_with_theme(test_theme());
    command(&mut bad, "/changelog everything");
    assert!(
        bad.lines
            .iter()
            .any(|line| line.kind == LineKind::Error && line.text.contains("usage: /changelog")),
        "{:#?}",
        bad.lines
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>()
    );
}

/// `tui.tight` drops one cell of horizontal padding from the composer box
/// and from the status row's left edge, and unset is every frame byte for
/// byte what it was.
#[test]
fn tight_packs_the_composer_and_the_status_row() {
    let mut chat = chat();
    type_text(&mut chat, "one two");
    let before = frame_rows(&mut chat, 60, 20);
    assert!(before[0].starts_with(" titi"), "{:?}", before[0]);
    assert!(
        before.iter().any(|row| row.starts_with("│ › one two")),
        "{before:#?}"
    );

    // Tight: the status row loses its leading cell and the draft starts one
    // column further left. Everything else is the same row.
    let mut packed_chat = chat_with_theme(test_theme());
    type_text(&mut packed_chat, "one two");
    packed_chat.tight = true;
    packed_chat.status_line.tight = true;
    let packed = frame_rows(&mut packed_chat, 60, 20);
    assert!(packed[0].starts_with("titi"), "{:?}", packed[0]);
    assert!(
        packed.iter().any(|row| row.starts_with("│› one two")),
        "{packed:#?}"
    );
    assert_eq!(before.len(), packed.len(), "the same screen, packed left");

    // The right group is where it was: the cell that went came off the
    // left, and the gap between the groups took it.
    assert_eq!(
        before[0].find('⬢'),
        packed[0].find('⬢'),
        "the status row's right group sits at the same column"
    );

    // And the composer's prompt sits one column further left: that cell is
    // the box padding the key drops.
    let prompt = |rows: &[String]| {
        rows.iter()
            .find_map(|row| row.find('›'))
            .expect("the composer's prompt")
    };
    assert_eq!(
        prompt(&packed) + 1,
        prompt(&before),
        "one cell of box padding"
    );
}

/// The panel boxes lose their padding too, on both sides, and keep the
/// list's own indent.
#[test]
fn tight_packs_a_panel_box() {
    let mut chat = chat();
    type_text(&mut chat, "/he");
    let box_row = |rows: &[String]| {
        rows.iter()
            .find(|row| row.contains("help"))
            .expect("a row")
            .to_owned()
    };
    let before = box_row(&frame_rows(&mut chat, 60, 20));
    assert!(
        before.starts_with("│ ▶ ") || before.starts_with("│  ▶ "),
        "{before:?}"
    );
    assert!(before.trim_end().ends_with('│'), "{before:?}");

    let mut packed_chat = chat_with_theme(test_theme());
    type_text(&mut packed_chat, "/he");
    packed_chat.tight = true;
    let packed = box_row(&frame_rows(&mut packed_chat, 60, 20));
    // The box is full width either way, so what moves is the row inside
    // it: one cell left, and one more cell of room for the text.
    let cursor = |row: &str| row.find('▶').expect("the cursor");
    assert_eq!(cursor(&packed) + 1, cursor(&before), "one cell of padding");
    assert!(packed.contains("/help"), "the same row, packed: {packed:?}");
}

/// The key itself: unset is off, `true`/`on` are on, and anything else
/// leaves the screen as it was.
#[test]
fn the_tight_key_is_a_switch() {
    use titi_config::settings::{Settings, switch_on};

    let dir = tempfile::tempdir().expect("temp");
    let project = tempfile::tempdir().expect("temp");
    let read = |text: &str| {
        std::fs::write(dir.path().join("config.yml"), text).expect("write");
        Settings::load(dir.path(), project.path(), &[]).expect("load")
    };
    assert!(!switch_on(
        &read("theme:\n  dark: titanium\n"),
        titi_config::settings::TUI_TIGHT_KEY
    ));
    for text in ["tui:\n  tight: true\n", "tui:\n  tight: on\n"] {
        assert!(
            switch_on(&read(text), titi_config::settings::TUI_TIGHT_KEY),
            "{text}"
        );
    }
    for text in ["tui:\n  tight: false\n", "tui:\n  tight: 7\n"] {
        assert!(
            !switch_on(&read(text), titi_config::settings::TUI_TIGHT_KEY),
            "{text}"
        );
    }
}

/// The three status-line keys, read from a real config file: the separator
/// by name, the accent and the transparent background as switches.
#[test]
fn the_status_line_keys_resolve_from_the_settings() {
    let dir = tempfile::tempdir().expect("temp");
    let project = tempfile::tempdir().expect("temp");
    let settings = |text: &str| {
        std::fs::write(dir.path().join("config.yml"), text).expect("write");
        titi_config::settings::Settings::load(dir.path(), project.path(), &[]).expect("load")
    };
    let resolve = |text: &str| {
        let settings = Some(settings(text));
        let it = settings.as_ref();
        StatusLineStyle::resolve(
            setting_string(it, titi_config::settings::STATUS_LINE_PRESET_KEY).as_deref(),
            setting_string(it, titi_config::settings::STATUS_LINE_CONTEXT_LINE_KEY).as_deref(),
            setting_string(it, titi_config::settings::STATUS_LINE_SEPARATOR_KEY).as_deref(),
            it.is_some_and(|settings| {
                titi_config::settings::switch_on(
                    settings,
                    titi_config::settings::STATUS_LINE_SESSION_ACCENT_KEY,
                )
            }),
            it.is_some_and(|settings| {
                titi_config::settings::switch_on(
                    settings,
                    titi_config::settings::STATUS_LINE_TRANSPARENT_KEY,
                )
            }),
            false,
        )
    };

    // Unset: the preset's own separator, no accent, no transparency — the
    // frame this screen has always drawn.
    let plain = resolve("theme:\n  dark: titanium\n");
    assert_eq!(plain.separator, None);
    assert!(!plain.session_accent && !plain.transparent);

    let named =
        resolve("statusLine:\n  separator: slash\n  sessionAccent: true\n  transparent: true\n");
    assert_eq!(
        named.separator,
        Some(titi_tui::status_bar::Separator::Slash)
    );
    assert!(named.session_accent && named.transparent);

    // A typo in any of the three leaves that one as it was.
    let typo =
        resolve("statusLine:\n  separator: hoops\n  sessionAccent: maybe\n  transparent: 7\n");
    assert_eq!(typo.separator, None);
    assert!(!typo.session_accent && !typo.transparent);
}

/// The separator key replaces the glyph between the segments, and an unset
/// key leaves the row byte for byte what it was.
#[test]
fn the_separator_key_redraws_the_status_row() {
    let mut plain = chat();
    let before = frame_text(&mut plain);
    assert!(before.contains(" > ") || before.contains('>'), "{before}");

    let mut piped = chat();
    piped.status_line.separator = Some(titi_tui::status_bar::Separator::Pipe);
    let after = frame_text(&mut piped);
    assert!(after.contains(" │ "), "{after}");
    assert!(
        !after.contains("tit > "),
        "the preset's chevron is gone: {after}"
    );

    // And the row is otherwise the same length: the segments are all there.
    let row = |frame: &str| frame.lines().next().unwrap_or_default().to_owned();
    assert_eq!(
        row(&before).chars().count(),
        row(&after).chars().count(),
        "the same cells, a different glyph"
    );
}

/// `statusLine.sessionAccent` takes the idle editor border, and leaves the
/// states that mean something alone.
#[test]
fn the_session_accent_colours_the_composer_border() {
    let chat_with_accent = || {
        let mut chat = chat();
        chat.status_line.session_accent = true;
        chat
    };
    let mut chat = chat();
    assert_eq!(
        composer_colors(&chat),
        (ThemeColor::Border, ThemeColor::Dim),
        "idle, unset: the border it has always been"
    );
    chat.status_line.session_accent = true;
    assert_eq!(
        composer_colors(&chat),
        (ThemeColor::Accent, ThemeColor::Dim),
        "idle, asked for: the accent"
    );

    // A running turn keeps its own colour with the key on.
    let mut running = chat_with_accent();
    running.turn_active = true;
    running.turn_started = Some(Instant::now());
    assert_eq!(
        composer_colors(&running),
        (ThemeColor::Accent, ThemeColor::Accent),
        "running is the accent pair, not the idle border"
    );

    // And needs-you is the warning, whatever the key says.
    let mut asked = chat_with_accent();
    asked.on_event(EngineEvent::AskRequested {
        request_id: "ask-1".into(),
        question: "which one?".into(),
        options: vec!["a".into()],
        multi: false,
        free_text: false,
    });
    assert_eq!(composer_colors(&asked).0, ThemeColor::Warning);
}

/// `statusLine.transparent` leaves the status row's cells to the terminal,
/// where the rest of the screen keeps the theme's background.
#[test]
fn the_transparent_status_row_has_no_background() {
    let theme = test_theme();
    let page = bg(&theme, ThemeBg::StatusLineBg);

    let mut plain = chat();
    let colors = frame_colors(&mut plain, 60, 20);
    assert_eq!(colors[0].1, page, "the row paints the theme's background");

    let mut clear = chat();
    clear.status_line.transparent = true;
    let colors = frame_colors(&mut clear, 60, 20);
    assert_eq!(colors[0].1, Color::Reset, "and clears it when asked");
    assert_eq!(colors[60].1, page, "the row below it is untouched");
}

/// alt+f with no tree open does nothing: the key belongs to the panel, and
/// must not surprise the composer.
#[test]
fn the_tree_filter_key_is_the_panels_own() {
    let mut chat = chat();
    type_text(&mut chat, "draft");
    assert!(chat.on_key(Key::AltF, Instant::now()).effect.is_none());
    assert_eq!(chat.input, "draft", "the composer is untouched");
    assert!(chat.tree_picker.is_none(), "and no tree opened");
}

/// Enter on a row moves the leaf there: the path through it is what the
/// screen and the engine get, and the branch that is left stays in the
/// store — a branch, not a rewind.
#[test]
fn enter_on_a_tree_row_branches_there_and_replays_the_path() {
    let (dir, id, store) = branched_session();
    let mut chat = Chat::new("openai/gpt-4.1", &id, test_theme());
    chat.agent_dir = dir.path().to_path_buf();

    type_text(&mut chat, "/tree");
    chat.on_key(Key::Enter, Instant::now());
    // Up twice: from the leaf (`other`) back to `two`, the branch point.
    chat.on_key(Key::Up, Instant::now());
    chat.on_key(Key::Up, Instant::now());
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert!(chat.tree_picker.is_none(), "the panel closes on Enter");

    match applied.effect {
        Some(ChatEffect::Send(EngineCommand::RestoreHistory { messages })) => {
            let shown: Vec<&str> = messages.iter().map(|m| m.content.as_str()).collect();
            assert_eq!(shown, ["one", "two"], "the path through the row");
        }
        other => panic!("expected restore, got {other:?}"),
    }
    let shown: Vec<&str> = chat.lines.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(
        shown,
        [
            "one",
            "two",
            "branched at entry 2 · 2 entries are off the path now",
        ]
    );

    // The abandoned branch is still stored: branching never rewrites it.
    let entries = store.open(&id).expect("entries");
    assert_eq!(entries.len(), 4);
    assert!(
        entries.iter().any(|entry| entry.content == "three"),
        "the entry left behind is still there"
    );
}

/// A session with nothing in it answers rather than opening an empty panel.
#[test]
fn the_tree_of_an_empty_session_says_so() {
    let dir = tempfile::tempdir().expect("temp");
    let store = titi_core::session::SessionStore::new(dir.path()).expect("store");
    let id = store
        .create(titi_core::session::SessionMeta::default())
        .expect("session");
    let mut chat = Chat::new("openai/gpt-4.1", &id, test_theme());
    chat.agent_dir = dir.path().to_path_buf();

    type_text(&mut chat, "/tree");
    chat.on_key(Key::Enter, Instant::now());
    assert!(chat.tree_picker.is_none());
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "tree: this session has no entries yet"),
        "{:?}",
        chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
}

/// The body of the paste the tests stage: long enough to cross the menu's
/// own threshold (`PASTE_MENU_AFTER`), which is what a log looks like.
fn long_paste(lines: usize) -> String {
    (1..=lines)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A paste past the threshold offers the menu, and nothing is lost while it
/// is up: the draft already stands for the whole body, the way a short
/// paste's marker does.
#[test]
fn a_large_paste_offers_the_menu_and_keeps_the_marker() {
    let body = long_paste(150);
    let mut chat = chat();
    chat.paste(&body);

    let menu = chat.paste_menu.as_ref().expect("the menu is offered");
    assert_eq!(menu.lines, 150);
    assert_eq!(menu.selected, 0, "the first row is the cursor");
    assert_eq!(chat.input, "[Paste #1 · 150 lines]");
    assert_eq!(chat.expand_pastes(&chat.input), body, "nothing is lost");

    let view = panel_view_for(&chat, 30, 100).expect("a panel");
    let title = view.title.clone().unwrap_or_default();
    assert!(title.contains("pasted 150 lines"), "{title}");
    assert!(title.contains("esc keeps the marker"), "{title}");
    assert_eq!(view.lines.len(), PasteMenu::ROWS.len());
}

/// Below the threshold there is no menu, and a paste that only reaches the
/// collapse threshold still becomes its marker: the menu is an offer, not a
/// change to what a paste does.
#[test]
fn a_paste_below_the_threshold_never_offers_the_menu() {
    let mut chat = chat();
    chat.paste(&long_paste(8));
    assert!(chat.paste_menu.is_none());
    assert_eq!(chat.input, "[Paste #1 · 8 lines]");

    // …and a threshold of 0 turns the offer off for a paste of any size.
    let mut off = chat_with_theme(test_theme());
    off.paste_menu_after = 0;
    off.paste(&long_paste(400));
    assert!(off.paste_menu.is_none());
    assert_eq!(off.input, "[Paste #1 · 400 lines]");
}

/// Taking the block row fences what the marker stands for: the draft is
/// untouched, and what would be sent is the body between fences.
#[test]
fn attaching_a_paste_as_a_block_fences_it_at_send() {
    let mut chat = chat();
    chat.paste(&long_paste(150));
    chat.on_key(Key::Enter, Instant::now());
    assert!(chat.paste_menu.is_none(), "the menu closes on Enter");
    assert_eq!(
        chat.input, "[Paste #1 · 150 lines]",
        "the draft is untouched"
    );

    let sent = chat.expand_pastes(&chat.input);
    assert!(sent.starts_with("```\nline 1\n"), "{sent:.40}");
    assert!(
        sent.ends_with("line 150\n```"),
        "{}",
        &sent[sent.len() - 20..]
    );
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "paste: 150 lines will be sent as a fenced block"),
        "{:?}",
        chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
}

/// Taking the file row writes the body under the workspace and leaves its
/// path where the marker was, so the model can `read` it instead of paying
/// for it in every request — and the marker stops standing for anything.
#[test]
fn attaching_a_paste_as_a_file_names_its_path() {
    let dir = tempfile::tempdir().expect("temp");
    let body = long_paste(150);
    let mut chat = chat();
    chat.workspace = dir.path().to_path_buf();
    chat.paste(&body);
    chat.on_key(Key::Down, Instant::now());
    chat.on_key(Key::Enter, Instant::now());

    assert!(chat.paste_menu.is_none());
    assert_eq!(chat.input, ".titi/pastes/paste-1.txt");
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".titi/pastes/paste-1.txt")).expect("the file"),
        body
    );
    assert_eq!(
        chat.expand_pastes(&chat.input),
        ".titi/pastes/paste-1.txt",
        "a path is sent as written, not expanded"
    );
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text == "paste: wrote .titi/pastes/paste-1.txt (150 lines)"),
        "{:?}",
        chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
}

/// Esc keeps what a short paste would have left: the marker, expanding to
/// the body as it was pasted. A workspace that cannot be written does the
/// same, and says so.
#[test]
fn esc_or_a_failed_write_keeps_the_verbatim_marker() {
    let body = long_paste(150);
    let mut chat = chat();
    chat.paste(&body);
    chat.on_key(Key::Esc, Instant::now());
    assert!(chat.paste_menu.is_none());
    assert_eq!(chat.input, "[Paste #1 · 150 lines]");
    assert_eq!(chat.expand_pastes(&chat.input), body);

    // A workspace that does not exist cannot hold the file: the marker
    // stays registered and the error is said, rather than a path to
    // nothing being left in the draft.
    let mut broken = chat_with_theme(test_theme());
    broken.workspace = std::path::PathBuf::from("/nonexistent-titi-workspace");
    broken.paste(&body);
    broken.on_key(Key::Down, Instant::now());
    broken.on_key(Key::Enter, Instant::now());
    assert_eq!(broken.input, "[Paste #1 · 150 lines]");
    assert_eq!(broken.expand_pastes(&broken.input), body);
    assert!(
        broken
            .lines
            .iter()
            .any(|line| line.text.starts_with("paste: not written")),
        "{:?}",
        broken.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
}

/// A pasted body lands in the workspace, and it must not be something
/// `git add -A` would commit: the directory writes its own `.gitignore`
/// first, so `git status` never sees the paste.
#[test]
fn an_attached_paste_keeps_itself_out_of_git() {
    let dir = tempfile::tempdir().expect("temp");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("git")
    };
    assert!(git(&["init", "-q"]).status.success(), "git init");

    let mut chat = chat();
    chat.workspace = dir.path().to_path_buf();
    chat.paste(&long_paste(120));
    chat.on_key(Key::Down, Instant::now());
    chat.on_key(Key::Enter, Instant::now());
    assert_eq!(chat.input, ".titi/pastes/paste-1.txt");

    let ignore = std::fs::read_to_string(dir.path().join(".titi/pastes/.gitignore"))
        .expect("the directory ignores itself");
    assert_eq!(ignore.trim(), "*");
    let status =
        String::from_utf8(git(&["status", "--porcelain"]).stdout).expect("git writes utf-8");
    assert!(status.trim().is_empty(), "git sees the paste: {status:?}");

    // A directory that already carries one keeps it: the file is titi's
    // only while there is none.
    std::fs::write(dir.path().join(".titi/pastes/.gitignore"), "# mine\n").expect("theirs");
    chat.paste(&long_paste(120));
    chat.on_key(Key::Down, Instant::now());
    chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".titi/pastes/.gitignore")).expect("read"),
        "# mine\n"
    );
}

/// A directory that will not take the ignore file fails the attach: the
/// paste stays its marker, nothing is written, and the reason is said —
/// never a paste file sitting un-ignored.
#[test]
fn a_paste_that_cannot_ignore_itself_is_not_written() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("temp");
    let pastes = dir.path().join(".titi/pastes");
    std::fs::create_dir_all(&pastes).expect("pastes");
    let mut read_only = std::fs::metadata(&pastes).expect("meta").permissions();
    read_only.set_mode(0o555);
    std::fs::set_permissions(&pastes, read_only).expect("chmod");

    let mut chat = chat();
    chat.workspace = dir.path().to_path_buf();
    chat.paste(&long_paste(120));
    chat.on_key(Key::Down, Instant::now());
    chat.on_key(Key::Enter, Instant::now());

    assert_eq!(chat.input, "[Paste #1 · 120 lines]", "the marker stays");
    assert!(
        !pastes.join("paste-1.txt").exists(),
        "no paste file was left behind"
    );
    assert!(
        chat.lines
            .iter()
            .any(|line| line.text.starts_with("paste: not written")),
        "{:?}",
        chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
    );

    // Leave the directory writable so the temp dir can be removed.
    let mut writable = std::fs::metadata(&pastes).expect("meta").permissions();
    writable.set_mode(0o755);
    std::fs::set_permissions(&pastes, writable).expect("restore");
}

/// Every row attaches what it says it does. Adding a row to `PasteMenu::ROWS`
/// without teaching `accept_paste_menu` about it fails here.
#[test]
fn every_paste_menu_row_attaches_something() {
    let dir = tempfile::tempdir().expect("temp");
    let body = long_paste(120);
    for row in 0..PasteMenu::ROWS.len() {
        let mut chat = chat();
        chat.workspace = dir.path().to_path_buf();
        chat.paste(&body);
        chat.paste_menu.as_mut().expect("open").selected = row;
        chat.on_key(Key::Enter, Instant::now());
        match row {
            PasteMenu::BLOCK => assert!(
                chat.expand_pastes(&chat.input).contains("```"),
                "the block row fences the body"
            ),
            _ => assert_eq!(
                chat.input, ".titi/pastes/paste-1.txt",
                "the file row leaves the path"
            ),
        }
    }
}

/// The key is read once, from the same settings as everything else: unset
/// leaves the screen's own default, a number is the number, and a typo
/// leaves the default rather than refusing to start.
#[test]
fn the_paste_menu_threshold_reads_the_config() {
    let dir = tempfile::tempdir().expect("temp");
    let read = |yml: &str| {
        std::fs::write(dir.path().join("config.yml"), yml).expect("config");
        titi_config::settings::Settings::load(dir.path(), dir.path(), &[])
            .expect("load")
            .paste_menu_threshold()
    };
    assert_eq!(read("paste:\n  menuThreshold: 12\n"), Some(12));
    assert_eq!(read("paste:\n  menuThreshold: 0\n"), Some(0), "0 is off");
    assert_eq!(
        read("paste: {}\n"),
        None,
        "unset leaves the screen's default"
    );
    assert_eq!(
        read("paste:\n  menuThreshold: often\n"),
        None,
        "a typo is no number"
    );
    assert_eq!(read("paste:\n  menuThreshold: -3\n"), None);
}

/// One session that took a second branch: `one` → `two` → `three`, then the
/// leaf moves back to `two` and the conversation continues as `other`. The
/// store holds all four entries; the path holds `one`, `two`, `other`.
fn branched_session() -> (tempfile::TempDir, String, titi_core::session::SessionStore) {
    let dir = tempfile::tempdir().expect("temp");
    let store = titi_core::session::SessionStore::new(dir.path()).expect("store");
    let id = store
        .create(titi_core::session::SessionMeta::default())
        .expect("session");
    store.append(&id, Role::User, "one").expect("one");
    let two = store.append(&id, Role::Assistant, "two").expect("two");
    store.append(&id, Role::User, "three").expect("three");
    store.fork(&id, &two.id).expect("fork");
    store.append(&id, Role::Assistant, "other").expect("other");
    (dir, id, store)
}

/// `/sessions` bare is the list Ctrl+X opens: the same rows, the same
/// switch, the same title — one list, two ways in.
#[test]
fn sessions_bare_opens_the_ctrl_x_list() {
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    let older = seed_session(dir.path(), "older", "older question");
    let newer = seed_session(dir.path(), "newer", "newer question");

    command(&mut chat, "/sessions");
    assert!(chat.session_picker.is_some(), "the list is up, as Ctrl+X");
    let frame = frame_text(&mut chat);
    assert!(frame.contains("sessions · 2"), "{frame}");
    assert!(frame.contains(&older) && frame.contains(&newer), "{frame}");
    assert!(chat.session_search.is_none(), "the list is not the search");

    // And Enter takes the row the cursor is on through the same switch.
    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_ne!(chat.session_id, "session-123", "the screen moved");
    assert!(
        matches!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::RestoreHistory { .. }))
        ),
        "and the engine was told to replay it"
    );
}

/// `/sessions <query>` searches the entries the FTS index holds — the
/// capability that had no caller — and a hit row names the session, when it
/// was written, and the line that matched.
#[test]
fn sessions_query_finds_the_matching_line_and_enter_switches() {
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    let kafka = seed_session(
        dir.path(),
        "kafka-talk",
        "how do we size the kafka consumers",
    );
    seed_session(
        dir.path(),
        "postgres-talk",
        "which postgres index does the planner pick",
    );

    command(&mut chat, "/sessions kafka");
    let frame = frame_text(&mut chat);
    assert!(frame.contains("kafka-talk"), "{frame}");
    assert!(
        frame.contains("how do we size the kafka consumers"),
        "the matching line is the row: {frame}"
    );
    assert!(
        !frame.contains("postgres-talk") && !frame.contains("the planner pick"),
        "the session that did not match is not offered: {frame}"
    );
    assert!(
        frame.contains("ago") || frame.contains("just now"),
        "the row says when the session was written: {frame}"
    );

    let applied = chat.on_key(Key::Enter, Instant::now());
    assert_eq!(
        chat.session_id, kafka,
        "Enter switches to the hit's session"
    );
    assert!(
        matches!(
            applied.effect,
            Some(ChatEffect::Send(EngineCommand::RestoreHistory { .. }))
        ),
        "through the switch the list uses"
    );
    assert!(chat.session_search.is_none(), "and the picker closes");
}

/// A search row is dated by the line that matched — the hit's own time,
/// not the session file's — and named by the title the hit carries.
#[test]
fn a_search_hit_is_dated_by_the_line_that_matched() {
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    let index =
        titi_core::session::SessionIndex::open(&dir.path().join("state.db")).expect("index");
    // A session with a real name and one old entry, and no file of its
    // own: the row must not need one to say when the line was said.
    index
        .insert_session(
            "kafka-talk",
            1_000,
            &titi_core::session::SessionMeta::default(),
        )
        .expect("session");
    index
        .set_title("kafka-talk", "kafka sizing")
        .expect("title");
    let mut entry =
        titi_core::session::Entry::new(None, Role::User, "how do we size the kafka consumers");
    // Two weeks before this test's own clock, in the milliseconds the
    // store keeps: the row reads weeks, not the moment the index was
    // written.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64;
    entry.ts = now_ms - 14 * 86_400 * 1_000;
    index.index_entry("kafka-talk", &entry).expect("entry");

    command(&mut chat, "/sessions kafka");
    let frame = frame_text(&mut chat);
    assert!(
        frame.contains("kafka sizing"),
        "the hit's title names the row: {frame}"
    );
    assert!(
        frame.contains("2w ago"),
        "dated by the line that matched, not by the file: {frame}"
    );
}

/// No hits says so, and a row says the id when the index holds no real
/// title for that session.
#[test]
fn sessions_query_reports_no_hits_and_falls_back_to_the_id() {
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    let store = titi_core::session::SessionStore::new(dir.path()).expect("session store");
    let unnamed = store
        .create(titi_core::session::SessionMeta {
            title: Some("titi".to_owned()),
            ..Default::default()
        })
        .expect("create");
    store
        .append(&unnamed, Role::User, "a shared word about sockets")
        .expect("append");

    command(&mut chat, "/sessions zzzz-nothing-matches");
    let frame = frame_text(&mut chat);
    assert!(
        frame.contains("no sessions match \"zzzz-nothing-matches\""),
        "{frame}"
    );
    // Enter with nothing to take says so instead of closing in silence.
    chat.on_key(Key::Enter, Instant::now());
    assert!(chat.session_search.is_none(), "the panel closes");
    let frame = frame_text(&mut chat);
    assert!(
        frame.contains("sessions: no match for \"zzzz-nothing-matches\""),
        "{frame}"
    );

    // The session nobody named is offered by its id, not by the product
    // name it was created with.
    command(&mut chat, "/sessions sockets");
    let frame = frame_text(&mut chat);
    assert!(frame.contains(&unnamed), "the id is the row: {frame}");
    assert!(
        !frame.contains("titi ·") && !frame.contains("titi  ✓"),
        "the placeholder title is not a name: {frame}"
    );
}

/// The query narrows as it is typed, the way the model, theme and history
/// browsers narrow theirs: a character re-runs it, a backspace takes one
/// back, the first Esc clears it, the second closes.
#[test]
fn the_session_query_narrows_as_it_is_typed() {
    let (dir, mut chat) = picker_chat("openai/gpt-4.1", "session-123");
    seed_session(dir.path(), "kafka-talk", "kafka consumer groups");
    command(&mut chat, "/sessions kafka");
    assert!(frame_text(&mut chat).contains("kafka-talk"));

    chat.on_key(Key::Char('x'), Instant::now());
    let frame = frame_text(&mut chat);
    assert!(frame.contains("no sessions match \"kafkax\""), "{frame}");
    assert!(!frame.contains("kafka-talk"), "{frame}");

    chat.on_key(Key::Backspace, Instant::now());
    assert!(
        frame_text(&mut chat).contains("kafka-talk"),
        "backspace brings the hit back"
    );

    chat.on_key(Key::Esc, Instant::now());
    assert!(
        chat.session_search.is_some(),
        "the first Esc clears the query, it does not close"
    );
    assert!(
        frame_text(&mut chat).contains("sessions · 1"),
        "and the cleared query is the whole list"
    );
    chat.on_key(Key::Esc, Instant::now());
    assert!(chat.session_search.is_none(), "the second Esc closes");
}

/// The age a row carries is computed from the two clocks, so it is testable
/// without a wall clock of its own.
#[test]
fn the_age_label_is_a_shape_not_a_clock() {
    let now = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000);
    assert_eq!(age_label(now, 1_000_000), "just now");
    assert_eq!(age_label(now, 999_941), "just now", "under a minute");
    assert_eq!(age_label(now, 999_940), "1m ago");
    assert_eq!(age_label(now, 1_000_000 - 3_600), "1h ago");
    assert_eq!(age_label(now, 1_000_000 - 86_400), "1d ago");
    assert_eq!(age_label(now, 1_000_000 - 604_800), "1w ago");
    // A file stamped ahead of this machine's clock is not a negative age.
    assert_eq!(age_label(now, 1_000_001), "just now");
}
