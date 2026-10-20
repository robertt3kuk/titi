//! The composer's helpers, tested through the chat: the `@file` picker and the
//! attachments it leads to, shell-style prompt recall, fuzzy slash filtering,
//! and the draft stash.

use super::*;
use crate::draft_stash::{DraftStash, MAX_DRAFTS, worth_stashing};
use crate::history_nav::{Held, HistoryNav, Recall};
use crate::mention::{self, MAX_INLINE_BYTES, MAX_INLINE_LINES, mention_at};
use std::path::Path;

fn theme() -> Arc<Theme> {
    let options = titi_tui::theme::loader::CreateThemeOptions {
        mode: Some(titi_tui::theme::ColorMode::Truecolor),
        ..Default::default()
    };
    match titi_tui::theme::loader::load_theme("titanium", &options) {
        Ok(theme) => Arc::new(theme),
        Err(reason) => panic!("built-in theme: {reason}"),
    }
}

/// A chat whose workspace and agent directory are the test's own.
fn chat_in(workspace: &Path, agent: &Path) -> Chat {
    let mut chat = Chat::new("openai/gpt-4.1", "session-1", theme());
    chat.set_agent_dir(agent);
    chat.workspace = workspace.to_path_buf();
    chat
}

fn type_text(chat: &mut Chat, text: &str) {
    for ch in text.chars() {
        chat.on_key(Key::Char(ch), Instant::now());
    }
}

fn press(chat: &mut Chat, key: Key) -> Applied {
    chat.on_key(key, Instant::now())
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("dirs");
    }
    std::fs::write(path, body).expect("write");
}

/// `mention::attach` with no settings to speak of: an agent directory that
/// does not exist leaves the built-in privacy policy.
fn attach_here(workspace: &Path, draft: &str) -> mention::Attachments {
    mention::attach(workspace, Path::new("/nonexistent-titi-agent"), draft)
}

fn files(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn frame(chat: &mut Chat) -> String {
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal.draw(|f| draw(f, chat)).expect("draw");
    let buffer = terminal.backend().buffer();
    (0..24)
        .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn submitted(applied: &Applied) -> String {
    match &applied.effect {
        Some(ChatEffect::Send(EngineCommand::SubmitPrompt { text })) => text.to_string(),
        other => panic!("expected a submitted prompt, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// `@` detection
// ---------------------------------------------------------------------------

#[test]
fn an_at_opens_a_mention_only_at_a_word_start() {
    let at = |input: &str| mention_at(input, input.len()).map(|m| (m.start, m.query.to_owned()));
    assert_eq!(at("@"), Some((0, String::new())));
    assert_eq!(at("@src/ma"), Some((0, "src/ma".to_owned())));
    assert_eq!(at("look at @REA"), Some((8, "REA".to_owned())));
    assert_eq!(at("two\n@x"), Some((4, "x".to_owned())));
    assert_eq!(at("mail a@b"), None, "an address is not a mention");
    assert_eq!(at("a@b.com"), None);
    assert_eq!(at("@done and"), None, "a space ends the word");
    assert_eq!(at("plain"), None);
    // The caret decides the word: mid-sentence, the one it is in.
    assert_eq!(
        mention_at("see @src now", 8).map(|m| (m.start, m.query)),
        Some((4, "src"))
    );
}

// ---------------------------------------------------------------------------
// Ranking and the file list
// ---------------------------------------------------------------------------

#[test]
fn a_query_ranks_the_file_name_and_the_shorter_path_first() {
    let list = files(&[
        "docs/guide/README.md",
        "src/read_more.rs",
        "README.md",
        "docs/README.md",
        "src/lib.rs",
    ]);
    let ranked: Vec<&str> = mention::rank(&list, "READ")
        .into_iter()
        .map(|at| list[at].as_str())
        .collect();
    assert_eq!(ranked[0], "README.md");
    assert_eq!(ranked[1], "docs/README.md");
    assert!(!ranked.contains(&"src/lib.rs"), "{ranked:?}");
    // A scattering still finds the file: `mrs` is `src/read_more.rs`.
    let scattered = mention::rank(&list, "rdmrs");
    assert_eq!(
        scattered.first().map(|at| list[*at].as_str()),
        Some("src/read_more.rs")
    );
    // No query keeps the list's own order.
    assert_eq!(mention::rank(&list, ""), vec![0, 1, 2, 3, 4]);
}

#[test]
fn the_list_follows_gitignore_in_a_repository() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path();
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["init", "-q"])
            .status()
            .expect("git")
            .success()
    );
    write(root, ".gitignore", "secret.log\nbuild/\n");
    write(root, "README.md", "hi");
    write(root, "src/main.rs", "fn main() {}");
    write(root, "secret.log", "x");
    write(root, "build/out.txt", "x");
    write(root, "with space.txt", "x");
    let listed = mention::list_workspace_files(root);
    assert!(listed.contains(&"README.md".to_owned()), "{listed:?}");
    assert!(listed.contains(&"src/main.rs".to_owned()), "{listed:?}");
    assert!(listed.contains(&".gitignore".to_owned()), "{listed:?}");
    assert!(
        !listed.iter().any(|p| p.contains("secret.log")),
        "{listed:?}"
    );
    assert!(
        !listed.iter().any(|p| p.starts_with("build/")),
        "{listed:?}"
    );
    assert!(!listed.iter().any(|p| p.contains(' ')), "{listed:?}");
    let depth_of = |p: &String| p.matches('/').count();
    assert!(
        listed
            .windows(2)
            .all(|pair| depth_of(&pair[0]) <= depth_of(&pair[1])),
        "shallow first: {listed:?}"
    );
}

#[test]
fn outside_a_repository_the_walk_skips_hidden_and_build_directories() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path();
    write(root, "a.txt", "x");
    write(root, "node_modules/dep/index.js", "x");
    write(root, ".hidden/h.txt", "x");
    write(root, "target/debug/t", "x");
    write(root, "src/b.rs", "x");
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc", root.join("etc-link")).expect("link");
    let listed = mention::list_workspace_files(root);
    assert_eq!(listed, files(&["a.txt", "src/b.rs"]), "{listed:?}");
}

// ---------------------------------------------------------------------------
// The picker in the composer
// ---------------------------------------------------------------------------

fn workspace_with_files() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("dir");
    write(dir.path(), "README.md", "# titi\nhello\n");
    write(dir.path(), "src/main.rs", "fn main() {}\n");
    write(dir.path(), "src/lib.rs", "pub fn x() {}\n");
    dir
}

#[test]
fn typing_an_at_opens_the_file_picker_and_enter_inserts_the_path() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    type_text(&mut chat, "look at @READ");
    assert!(chat.compose.mention.is_visible(), "the picker is up");
    let shown = frame(&mut chat);
    assert!(shown.contains("files · 1 of 3 · @READ"), "{shown}");
    assert!(shown.contains("README.md"), "{shown}");
    assert!(
        shown.contains("tab inserts"),
        "the caption names the keys: {shown}"
    );

    let applied = press(&mut chat, Key::Enter);
    assert!(applied.effect.is_none(), "Enter picked, it did not send");
    assert_eq!(chat.input, "look at @README.md ");
    assert!(!chat.compose.mention.is_visible(), "and the picker closed");
    assert!(chat.lines.iter().all(|l| l.kind != LineKind::User));
}

#[test]
fn tab_picks_the_highlighted_row_and_arrows_move_it() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    type_text(&mut chat, "@src/");
    press(&mut chat, Key::Down);
    press(&mut chat, Key::Tab);
    assert_eq!(chat.input, "@src/main.rs ", "the second row, after ↓");
}

#[test]
fn escape_closes_the_picker_and_leaves_the_text() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    type_text(&mut chat, "@RE");
    assert!(chat.compose.mention.is_visible());
    press(&mut chat, Key::Esc);
    assert!(!chat.compose.mention.is_visible());
    assert_eq!(chat.input, "@RE", "Esc kept the draft");
    type_text(&mut chat, "A");
    assert!(
        !chat.compose.mention.is_visible(),
        "it stays shut for the word it was shut on"
    );
    // A new word opens it again.
    type_text(&mut chat, " @s");
    assert!(chat.compose.mention.is_visible());
}

#[test]
fn a_word_that_is_no_file_never_opens_the_picker() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    type_text(&mut chat, "install @scope/pkg");
    assert!(!chat.compose.mention.is_visible());
    let applied = press(&mut chat, Key::Enter);
    let sent = submitted(&applied);
    assert_eq!(sent, "install @scope/pkg", "plain text, nothing attached");
    // An address does not trigger it either.
    let mut chat = chat_in(ws.path(), agent.path());
    type_text(&mut chat, "mail me@README");
    assert!(!chat.compose.mention.is_visible());
}

#[test]
fn the_list_is_read_again_when_a_mention_opens_afresh() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    type_text(&mut chat, "@NEW");
    assert!(!chat.compose.mention.is_visible(), "no such file yet");
    write(ws.path(), "NEWS.md", "x");
    type_text(&mut chat, "S");
    assert!(
        !chat.compose.mention.is_visible(),
        "typing inside one word does not list again"
    );
    press(&mut chat, Key::Esc);
    for _ in 0..5 {
        press(&mut chat, Key::Backspace);
    }
    type_text(&mut chat, "@NEWS");
    assert!(
        chat.compose.mention.is_visible(),
        "a reopened mention sees it"
    );
}

// ---------------------------------------------------------------------------
// What a submitted mention hands the model
// ---------------------------------------------------------------------------

#[test]
fn a_mentioned_file_travels_with_the_prompt_and_the_screen_keeps_the_path() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    type_text(&mut chat, "summarize @README.md, please");
    let applied = press(&mut chat, Key::Enter);
    let sent = submitted(&applied);
    assert!(
        sent.starts_with("summarize @README.md, please\n\n<attached-files>\n"),
        "{sent}"
    );
    assert!(
        sent.contains("<file path=\"README.md\">\n# titi\nhello\n</file>"),
        "{sent}"
    );
    assert!(sent.ends_with("</attached-files>"), "{sent}");
    let user = chat
        .lines
        .iter()
        .find(|l| l.kind == LineKind::User)
        .expect("user line");
    assert_eq!(user.text, "summarize @README.md, please");
    assert!(
        chat.lines
            .iter()
            .any(|l| l.text == "attached README.md · 2 lines"),
        "{:?}",
        chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
    let log = applied.log.expect("logged").text;
    assert!(log.contains("# titi"), "the log keeps what the model read");
    assert_eq!(
        mention::without_attachments(&log),
        "summarize @README.md, please",
        "history strips the block"
    );
}

#[test]
fn paths_outside_the_workspace_are_refused_and_never_read() {
    let outer = tempfile::tempdir().expect("outer");
    write(outer.path(), "secret.txt", "TOP SECRET");
    let ws = outer.path().join("ws");
    std::fs::create_dir_all(&ws).expect("ws");
    write(&ws, "in.txt", "inside");
    #[cfg(unix)]
    std::os::unix::fs::symlink(outer.path().join("secret.txt"), ws.join("link.txt")).expect("link");

    for spelled in ["@../secret.txt", "@sub/../../secret.txt"] {
        let attached = attach_here(&ws, spelled);
        assert!(attached.block.is_empty(), "{spelled}: {}", attached.block);
        assert_eq!(
            attached.notes,
            vec![format!("not attached: {spelled} is outside the workspace")]
        );
    }
    let absolute = format!("@{}", outer.path().join("secret.txt").display());
    let attached = attach_here(&ws, &absolute);
    assert!(
        attached.block.is_empty() && attached.notes.len() == 1,
        "{attached:?}"
    );
    let home = attach_here(&ws, "@~/notes");
    assert!(home.block.is_empty() && home.notes.len() == 1, "{home:?}");
    #[cfg(unix)]
    {
        let linked = attach_here(&ws, "@link.txt");
        assert!(
            linked.block.is_empty() && !linked.block.contains("SECRET"),
            "a link out of the workspace is not followed: {linked:?}"
        );
        assert_eq!(linked.notes.len(), 1);
    }
    let fine = attach_here(&ws, "@in.txt");
    assert!(fine.block.contains("inside"));
}

#[test]
fn a_long_file_is_cut_at_the_cap_with_a_marker() {
    let dir = tempfile::tempdir().expect("dir");
    let body: String = (1..=MAX_INLINE_LINES + 50)
        .map(|n| format!("line {n}\n"))
        .collect();
    write(dir.path(), "long.txt", &body);
    let attached = attach_here(dir.path(), "read @long.txt");
    assert!(
        attached.block.contains("truncated=\"true\""),
        "{}",
        attached.block
    );
    assert!(attached.block.contains("line 200\n"), "{}", attached.block);
    assert!(!attached.block.contains("line 201"), "cut at the line cap");
    assert!(
        attached.block.contains("[truncated · ")
            && attached.block.contains("first 200 lines shown"),
        "{}",
        attached.block
    );
    assert_eq!(
        attached.notes,
        vec!["attached long.txt · first 200 lines".to_owned()]
    );

    // The byte cap: one enormous line is dropped rather than shown half.
    write(dir.path(), "wide.txt", &"x".repeat(MAX_INLINE_BYTES * 3));
    let wide = attach_here(dir.path(), "@wide.txt");
    assert!(
        wide.block.len() < MAX_INLINE_BYTES * 2,
        "{}",
        wide.block.len()
    );
    assert!(wide.block.contains("[truncated"), "{}", wide.block);

    // A file exactly at the caps is whole.
    let exact: String = (1..=MAX_INLINE_LINES).map(|n| format!("{n}\n")).collect();
    write(dir.path(), "exact.txt", &exact);
    let whole = attach_here(dir.path(), "@exact.txt");
    assert!(!whole.block.contains("[truncated"), "{}", whole.block);
}

#[test]
fn binary_credential_and_missing_files_are_named_or_left_alone() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(dir.path().join("pic.bin"), [0u8, 159, 146, 150, 0]).expect("bin");
    write(dir.path(), ".env", "API_KEY=sk-test");
    write(dir.path(), "a.txt", "alpha\n");
    let attached = attach_here(
        dir.path(),
        "@pic.bin @.env @nope.txt @a.txt. me@a.txt @a.txt",
    );
    assert!(
        attached
            .block
            .contains("<file path=\"pic.bin\" note=\"binary file"),
        "{}",
        attached.block
    );
    assert!(
        attached
            .block
            .contains("<file path=\".env\" note=\"not inlined: looks like credentials\"/>")
    );
    assert!(!attached.block.contains("sk-test"), "{}", attached.block);
    assert!(!attached.block.contains("nope.txt"), "{}", attached.block);
    assert_eq!(
        attached.block.matches("<file path=\"a.txt\">").count(),
        1,
        "once, trailing dot trimmed, address ignored: {}",
        attached.block
    );
    assert_eq!(
        attach_here(dir.path(), "no mentions here"),
        mention::Attachments::default()
    );
}

#[test]
fn too_many_files_name_the_rest_instead_of_inlining_them() {
    let dir = tempfile::tempdir().expect("dir");
    let mut draft = String::new();
    for n in 0..10 {
        write(dir.path(), &format!("f{n}.txt"), "x\n");
        draft.push_str(&format!("@f{n}.txt "));
    }
    let attached = attach_here(dir.path(), &draft);
    assert_eq!(
        attached.block.matches("<file path=").count(),
        8,
        "{}",
        attached.block
    );
    assert!(
        attached.notes.iter().any(|n| n.contains("2 more @files")),
        "{:?}",
        attached.notes
    );
}

// ---------------------------------------------------------------------------
// Prompt recall
// ---------------------------------------------------------------------------

#[test]
fn the_walk_goes_back_and_ends_on_the_unsent_draft() {
    let mut nav = HistoryNav::default();
    let held = Held {
        text: "half typed".to_owned(),
        ..Held::default()
    };
    nav.start(files(&["newest", "middle", "oldest"]), held.clone());
    assert!(!nav.browsing("half typed"), "nothing recalled yet");
    assert_eq!(nav.older().as_deref(), Some("newest"));
    assert!(nav.browsing("newest"));
    assert!(!nav.browsing("newest edited"), "an edit ends the walk");
    assert_eq!(nav.older().as_deref(), Some("middle"));
    assert_eq!(nav.older().as_deref(), Some("oldest"));
    assert_eq!(nav.older(), None, "the oldest stays");
    assert_eq!(nav.newer(), Some(Recall::Entry("middle".to_owned())));
    assert_eq!(nav.newer(), Some(Recall::Entry("newest".to_owned())));
    assert_eq!(nav.newer(), Some(Recall::Draft(held)));
    assert_eq!(nav.newer(), None, "the walk is over");
    assert!(!nav.browsing("newest"));
}

fn asked(chat: &mut Chat, log: &Option<SessionLog>, prompt: &str) {
    type_text(chat, prompt);
    let applied = press(chat, Key::Enter);
    record(chat, log, applied.log);
}

fn history_chat(ws: &Path, agent: &Path) -> (Chat, Option<SessionLog>) {
    let store = titi_core::session::SessionStore::new(agent).expect("store");
    let id = store
        .create(titi_core::session::SessionMeta::default())
        .expect("session");
    let mut chat = chat_in(ws, agent);
    chat.session_id = id.clone();
    let log = SessionLog::open(agent, &id);
    (chat, log)
}

#[test]
fn up_recalls_in_place_and_down_comes_back_to_the_draft() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let (mut chat, log) = history_chat(ws.path(), agent.path());
    asked(&mut chat, &log, "first prompt");
    asked(&mut chat, &log, "second prompt");

    type_text(&mut chat, "my draft");
    press(&mut chat, Key::Up);
    assert_eq!(
        chat.input, "second prompt",
        "the caret is on line 1: recall"
    );
    assert!(chat.history_picker.is_none(), "in place, no panel");
    assert_eq!(chat.scroll_offset, 0, "and the transcript did not scroll");
    press(&mut chat, Key::Up);
    assert_eq!(chat.input, "first prompt");
    press(&mut chat, Key::Up);
    assert_eq!(chat.input, "first prompt", "the oldest stays put");
    assert_eq!(chat.scroll_offset, 0);
    press(&mut chat, Key::Down);
    assert_eq!(chat.input, "second prompt");
    press(&mut chat, Key::Down);
    assert_eq!(chat.input, "my draft", "the walk ends on the unsent draft");
    press(&mut chat, Key::Down);
    assert_eq!(
        chat.scroll_offset, 0,
        "↓ is the scroll again, which is at the bottom"
    );
}

#[test]
fn up_on_an_empty_draft_recalls_and_down_leaves_it_empty() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let (mut chat, log) = history_chat(ws.path(), agent.path());
    asked(&mut chat, &log, "only prompt");
    press(&mut chat, Key::Up);
    assert_eq!(chat.input, "only prompt");
    press(&mut chat, Key::Down);
    assert_eq!(chat.input, "");
}

#[test]
fn editing_a_recalled_prompt_starts_a_new_walk_from_the_edit() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let (mut chat, log) = history_chat(ws.path(), agent.path());
    asked(&mut chat, &log, "one");
    asked(&mut chat, &log, "two");
    press(&mut chat, Key::Up);
    type_text(&mut chat, "!");
    assert_eq!(chat.input, "two!");
    press(&mut chat, Key::Down);
    assert_eq!(chat.input, "two!", "↓ is not a walk any more: it scrolls");
    press(&mut chat, Key::Up);
    assert_eq!(chat.input, "two", "a new walk, from the edit");
    press(&mut chat, Key::Down);
    assert_eq!(chat.input, "two!", "and it ends on the edit");
}

#[test]
fn up_scrolls_the_transcript_when_the_caret_is_below_the_first_line() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let (mut chat, log) = history_chat(ws.path(), agent.path());
    asked(&mut chat, &log, "earlier");
    chat.input = "line one\nline two".to_owned();
    chat.caret_to_end();
    press(&mut chat, Key::Up);
    assert_eq!(chat.input, "line one\nline two", "not recalled");
    assert_eq!(chat.scroll_offset, 1, "scrolled");
    chat.set_caret(3);
    press(&mut chat, Key::Up);
    assert_eq!(chat.input, "earlier", "on line 1 again, it recalls");
}

#[test]
fn up_scrolls_when_there_is_nothing_to_recall() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    press(&mut chat, Key::Up);
    assert_eq!(chat.scroll_offset, 1);
}

#[test]
fn history_reaches_back_across_this_projects_sessions_and_hides_attachments() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let workspace = ws.path().to_string_lossy().into_owned();
    let store = titi_core::session::SessionStore::new(agent.path()).expect("store");
    let session_in = |cwd: &str| {
        store
            .create(titi_core::session::SessionMeta {
                cwd: Some(cwd.to_owned()),
                ..Default::default()
            })
            .expect("session")
    };
    let old = session_in(&workspace);
    let other = session_in("/somewhere/else");
    SessionLog::open(agent.path(), &old)
        .expect("log")
        .user("look at @README.md\n\n<attached-files>\n<file path=\"README.md\">\nx\n</file>\n</attached-files>")
        .expect("write");
    SessionLog::open(agent.path(), &other)
        .expect("log")
        .user("another project's prompt")
        .expect("write");
    let id = session_in(&workspace);
    let mut chat = chat_in(ws.path(), agent.path());
    chat.session_id = id.clone();
    let log = SessionLog::open(agent.path(), &id);
    asked(&mut chat, &log, "this session");
    press(&mut chat, Key::Up);
    assert_eq!(chat.input, "this session");
    press(&mut chat, Key::Up);
    assert_eq!(
        chat.input, "look at @README.md",
        "an earlier session, as typed"
    );
    press(&mut chat, Key::Up);
    assert_eq!(
        chat.input, "look at @README.md",
        "the other project's is not here"
    );
}

#[test]
fn a_recalled_slash_prompt_does_not_turn_up_into_the_command_list() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let (mut chat, log) = history_chat(ws.path(), agent.path());
    chat.skills.push(SkillRow {
        name: "inspect".to_owned(),
        about: String::new(),
    });
    asked(&mut chat, &log, "first");
    asked(&mut chat, &log, "/inspect this change");
    press(&mut chat, Key::Up);
    assert_eq!(chat.input, "/inspect this change");
    press(&mut chat, Key::Up);
    assert_eq!(chat.input, "first", "↑ kept walking the history");
}

// ---------------------------------------------------------------------------
// Fuzzy slash filtering
// ---------------------------------------------------------------------------

fn rows_for(typed: &str) -> Vec<String> {
    let mut chat = Chat::new("openai/gpt-4.1", "session-1", theme());
    type_text(&mut chat, typed);
    picker_rows(&chat)
        .iter()
        .map(|row| chat.row_name(row).to_owned())
        .collect()
}

#[test]
fn the_command_list_is_fuzzy_and_prefix_matches_come_first() {
    assert!(
        rows_for("/ctx").contains(&"context".to_owned()),
        "{:?}",
        rows_for("/ctx")
    );
    assert!(
        rows_for("/chp").contains(&"checkpoint".to_owned()),
        "{:?}",
        rows_for("/chp")
    );
    let che = rows_for("/che");
    assert_eq!(
        che.first().map(String::as_str),
        Some("checkpoint"),
        "{che:?}"
    );
    // Prefix hits precede scattered ones: `/co` starts compact, context, …
    let co = rows_for("/co");
    let first_scattered = co
        .iter()
        .position(|name| !name.starts_with("co"))
        .unwrap_or(co.len());
    assert!(co[..first_scattered].iter().all(|n| n.starts_with("co")));
    assert!(
        co[first_scattered..].iter().all(|n| !n.starts_with("co")),
        "{co:?}"
    );
    // The exact name leads.
    assert_eq!(
        rows_for("/checkpoints").first().map(String::as_str),
        Some("checkpoints")
    );
    // One letter is a prefix, never a scattering.
    assert!(
        rows_for("/q").iter().all(|n| n.starts_with('q')),
        "{:?}",
        rows_for("/q")
    );
    assert!(rows_for("/zzzzz").is_empty());
}

#[test]
fn enter_on_a_fuzzy_match_runs_the_highlighted_command() {
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = Chat::new("openai/gpt-4.1", "session-1", theme());
    chat.set_agent_dir(agent.path());
    type_text(&mut chat, "/hotk");
    press(&mut chat, Key::Enter);
    assert!(
        chat.lines
            .iter()
            .any(|l| l.text.contains("hotkeys · composer")),
        "{:?}",
        chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// The draft stash
// ---------------------------------------------------------------------------

#[test]
fn the_stash_persists_per_project_newest_first_and_is_capped() {
    let agent = tempfile::tempdir().expect("agent");
    let here = DraftStash::new(agent.path(), Path::new("/work/a"));
    let there = DraftStash::new(agent.path(), Path::new("/work/b"));
    assert!(here.list().expect("empty").is_empty());
    here.push("one").expect("push");
    here.push("two").expect("push");
    here.push("   ").expect("blank is not stored");
    assert_eq!(here.list().expect("list"), files(&["two", "one"]));
    assert!(there.list().expect("other project").is_empty());
    // A new handle reads what a previous run left.
    let again = DraftStash::new(agent.path(), Path::new("/work/a"));
    assert_eq!(
        again.take_newest(None).expect("take"),
        Some("two".to_owned())
    );
    assert_eq!(again.list().expect("list"), files(&["one"]));
    assert_eq!(
        DraftStash::new(agent.path(), Path::new("/work/z"))
            .take_newest(None)
            .expect("empty"),
        None
    );

    for n in 0..MAX_DRAFTS + 5 {
        there.push(&format!("draft {n}")).expect("push");
    }
    let kept = there.list().expect("list");
    assert_eq!(kept.len(), MAX_DRAFTS);
    assert_eq!(kept[0], format!("draft {}", MAX_DRAFTS + 4), "newest first");
    assert_eq!(kept[MAX_DRAFTS - 1], "draft 5", "the oldest fell off");
}

#[cfg(unix)]
#[test]
fn the_stash_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let agent = tempfile::tempdir().expect("agent");
    let stash = DraftStash::new(agent.path(), Path::new("/work/a"));
    stash.push("secret draft").expect("push");
    let dir = agent.path().join("draft-stash");
    let file = std::fs::read_dir(&dir)
        .expect("dir")
        .flatten()
        .next()
        .expect("one file");
    let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&file.path()), 0o600);
    assert_eq!(mode(&dir), 0o700);
}

#[test]
fn a_stash_that_cannot_be_read_or_written_is_a_typed_error() {
    let agent = tempfile::tempdir().expect("agent");
    let stash = DraftStash::new(agent.path(), Path::new("/work/a"));
    stash.push("kept").expect("push");
    let dir = agent.path().join("draft-stash");
    let file = std::fs::read_dir(&dir)
        .expect("dir")
        .flatten()
        .next()
        .expect("file")
        .path();
    std::fs::write(&file, "not json").expect("corrupt");
    let error = stash.list().expect_err("corrupt");
    assert!(
        error.to_string().contains("not a readable stash"),
        "{error}"
    );
    assert!(
        stash.push("more").is_err(),
        "a corrupt stash is not overwritten silently"
    );

    // The agent directory is a file: nothing can be created under it.
    let blocked = agent.path().join("blocked");
    std::fs::write(&blocked, "x").expect("file");
    let error = DraftStash::new(&blocked, Path::new("/work/a"))
        .push("draft")
        .expect_err("io");
    assert!(
        matches!(error, crate::draft_stash::StashError::Io { .. }),
        "{error}"
    );
}

#[test]
fn a_draft_is_worth_saving_from_two_lines_or_eighty_characters() {
    assert!(!worth_stashing(""));
    assert!(!worth_stashing("short"));
    assert!(!worth_stashing(&"x".repeat(79)));
    assert!(worth_stashing(&"x".repeat(80)));
    assert!(worth_stashing("one\ntwo"));
    assert!(
        !worth_stashing("one\n"),
        "a trailing newline is not a second line"
    );
}

#[test]
fn alt_s_stashes_the_draft_and_alt_p_brings_it_back() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    press(&mut chat, Key::AltS);
    assert_eq!(chat.hint, "nothing to stash");
    type_text(&mut chat, "a thought for later");
    press(&mut chat, Key::AltS);
    assert!(chat.input.is_empty(), "the box is clear");
    assert_eq!(chat.hint, "draft saved · alt+p restores");
    let shown = frame(&mut chat);
    assert!(shown.contains("draft saved · alt+p restores"), "{shown}");

    // A fresh screen on the same project — the next run — finds it.
    let mut next = chat_in(ws.path(), agent.path());
    press(&mut next, Key::AltP);
    assert_eq!(next.input, "a thought for later");
    assert_eq!(next.hint, "draft restored");
    press(&mut next, Key::AltP);
    assert_eq!(next.hint, "no stashed draft");
    assert_eq!(next.input, "a thought for later", "nothing to swap with");
}

#[test]
fn restoring_over_a_draft_stashes_that_draft_in_its_place() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    type_text(&mut chat, "first");
    press(&mut chat, Key::AltS);
    type_text(&mut chat, "second");
    press(&mut chat, Key::AltP);
    assert_eq!(chat.input, "first");
    assert_eq!(chat.hint, "draft restored · yours is stashed");
    press(&mut chat, Key::AltP);
    assert_eq!(chat.input, "second", "swapped back");
}

#[test]
fn a_pasted_marker_is_stashed_as_the_text_it_stands_for() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    let body: String = (0..12).map(|n| format!("row {n}\n")).collect();
    chat.paste(&body);
    assert!(chat.input.contains("[Paste #"), "{}", chat.input);
    press(&mut chat, Key::AltS);
    press(&mut chat, Key::AltP);
    assert!(
        chat.input.contains("row 11"),
        "the body came back: {}",
        chat.input
    );
    assert!(!chat.input.contains("[Paste #"));
}

#[test]
fn esc_saves_a_long_draft_first_and_says_so_but_not_a_short_one() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    type_text(&mut chat, "short");
    press(&mut chat, Key::Esc);
    assert!(chat.input.is_empty());
    assert!(chat.hint.is_empty(), "a short draft is not worth a stash");
    press(&mut chat, Key::AltP);
    assert_eq!(chat.hint, "no stashed draft");

    let long = "x".repeat(80);
    type_text(&mut chat, &long);
    press(&mut chat, Key::Esc);
    assert!(chat.input.is_empty());
    assert_eq!(chat.hint, "draft saved · alt+p restores");
    press(&mut chat, Key::AltP);
    assert_eq!(chat.input, long);

    // Two lines count too.
    chat.input = "a\nb".to_owned();
    chat.caret_to_end();
    press(&mut chat, Key::Esc);
    assert_eq!(chat.hint, "draft saved · alt+p restores");
}

#[test]
fn quitting_with_a_long_draft_saves_it_and_a_recalled_prompt_is_not_saved() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    let long = "y".repeat(100);
    type_text(&mut chat, &long);
    let start = Instant::now();
    chat.on_key(Key::CtrlC, start);
    assert!(chat.input == long, "the first press only arms the quit");
    let second = chat.on_key(Key::CtrlC, start + Duration::from_millis(300));
    assert_eq!(second.effect, Some(ChatEffect::Quit));
    let stash = DraftStash::new(agent.path(), ws.path());
    assert_eq!(stash.list().expect("list"), vec![long]);

    // History's own prompts are already kept: Esc on a recalled one saves nothing.
    let agent2 = tempfile::tempdir().expect("agent");
    let (mut chat, log) = history_chat(ws.path(), agent2.path());
    asked(&mut chat, &log, &"z".repeat(90));
    press(&mut chat, Key::Up);
    press(&mut chat, Key::Esc);
    assert!(chat.input.is_empty());
    assert!(chat.hint.is_empty(), "{}", chat.hint);
}

#[test]
fn a_stash_that_cannot_be_written_keeps_the_draft_in_the_box() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let blocked = agent.path().join("blocked");
    std::fs::write(&blocked, "x").expect("file");
    let mut chat = chat_in(ws.path(), &blocked);
    type_text(&mut chat, "do not lose me");
    press(&mut chat, Key::AltS);
    assert_eq!(chat.input, "do not lose me");
    assert!(
        chat.lines
            .iter()
            .any(|l| l.kind == LineKind::Error && l.text.starts_with("draft not stashed:")),
        "{:?}",
        chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// The caption and the listings
// ---------------------------------------------------------------------------

#[test]
fn the_caption_no_longer_repeats_the_context_percentage() {
    let mut chat = Chat::new("openai/gpt-4.1", "session-1", theme());
    chat.context_percent = Some(42);
    let shown = frame(&mut chat);
    assert!(shown.contains("enter sends"), "the key hints stay: {shown}");
    let caption_row = shown
        .lines()
        .find(|row| row.contains("enter sends"))
        .expect("caption row");
    assert!(!caption_row.contains("42%"), "{caption_row}");
}

#[test]
fn hotkeys_and_help_list_the_new_keys() {
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = Chat::new("openai/gpt-4.1", "session-1", theme());
    chat.set_agent_dir(agent.path());
    type_text(&mut chat, "/hotkeys");
    press(&mut chat, Key::Enter);
    type_text(&mut chat, "/help");
    press(&mut chat, Key::Enter);
    let all: Vec<&str> = chat.lines.iter().map(|l| l.text.as_str()).collect();
    for wanted in ["alt+s", "alt+p", "  @ ", "recall earlier prompts"] {
        assert!(
            all.iter().any(|row| row.contains(wanted)),
            "{wanted}: {all:#?}"
        );
    }
    assert!(
        all.iter().any(|row| row.starts_with("keys  @file")),
        "/help ends with the keys line: {all:#?}"
    );
}

// ---------------------------------------------------------------------------
// Privacy: the same policy and masking a `read` gets
// ---------------------------------------------------------------------------

#[test]
fn the_users_credential_policy_decides_what_an_at_path_may_inline() {
    let ws = tempfile::tempdir().expect("ws");
    let agent = tempfile::tempdir().expect("agent");
    write(
        agent.path(),
        "config.yml",
        "privacy:\n  sensitive: [\"*.vault\"]\n  allow: [\".env\"]\n",
    );
    write(ws.path(), "team.vault", "TOP SECRET PLAN");
    write(ws.path(), ".env", "PLAIN=allowed by the user");
    write(ws.path(), "plain.txt", "fine");
    #[cfg(unix)]
    std::os::unix::fs::symlink(ws.path().join("team.vault"), ws.path().join("notes.txt"))
        .expect("link");
    let attached = mention::attach(ws.path(), agent.path(), "@team.vault @.env @plain.txt");
    assert!(
        attached
            .block
            .contains("<file path=\"team.vault\" note=\"not inlined: looks like credentials\"/>"),
        "{}",
        attached.block
    );
    assert!(!attached.block.contains("TOP SECRET"), "{}", attached.block);
    #[cfg(unix)]
    {
        let linked = mention::attach(ws.path(), agent.path(), "@notes.txt");
        assert!(
            linked
                .block
                .contains("<file path=\"notes.txt\" note=\"not inlined")
                && !linked.block.contains("TOP SECRET"),
            "a link to a blocked file is blocked: {}",
            linked.block
        );
    }
    assert!(
        attached.block.contains("allowed by the user"),
        "the user's allow list opens .env"
    );
    assert!(attached.block.contains("<file path=\"plain.txt\">"));
    assert!(
        attached
            .notes
            .iter()
            .any(|n| n.contains("@team.vault looks like credentials"))
    );
}

#[test]
fn an_inlined_body_is_masked_like_a_tool_result_and_the_note_says_so() {
    let ws = workspace_with_files();
    let agent = tempfile::tempdir().expect("agent");
    let mut chat = chat_in(ws.path(), agent.path());
    write(
        ws.path(),
        "settings.toml",
        "api_key = \"sk-test-0123456789abcdefghijklmnop\"\nhost = \"10.1.2.3\"\nname = \"ok\"\n",
    );
    type_text(&mut chat, "check @settings.toml now");
    let applied = press(&mut chat, Key::Enter);
    let sent = submitted(&applied);
    assert!(sent.contains("name = \"ok\""), "{sent}");
    assert!(
        !sent.contains("sk-test-0123456789abcdefghijklmnop"),
        "{sent}"
    );
    assert!(
        !sent.contains("10.1.2.3"),
        "addresses are masked as for any tool: {sent}"
    );
    let log = applied.log.expect("logged").text;
    assert!(
        !log.contains("sk-test-"),
        "the log carries what the model read"
    );
    assert!(
        chat.lines.iter().any(|l| l
            .text
            .starts_with("attached settings.toml · 3 lines · masked ")),
        "{:?}",
        chat.lines.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
    // The draft on screen is what was typed.
    assert!(
        chat.lines
            .iter()
            .any(|l| l.kind == LineKind::User && l.text == "check @settings.toml now")
    );
}

#[test]
fn privacy_mask_ips_off_still_masks_secrets_but_keeps_addresses() {
    let ws = tempfile::tempdir().expect("ws");
    let agent = tempfile::tempdir().expect("agent");
    write(agent.path(), "config.yml", "privacy:\n  maskIps: false\n");
    write(
        ws.path(),
        "a.toml",
        "api_key = \"sk-test-0123456789abcdefghijklmnop\"\nhost = \"10.1.2.3\"\n",
    );
    let attached = mention::attach(ws.path(), agent.path(), "@a.toml");
    assert!(!attached.block.contains("sk-test-"), "{}", attached.block);
    assert!(attached.block.contains("10.1.2.3"), "{}", attached.block);
}
