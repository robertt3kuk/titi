//! Session, checkpoint, workspace and CLI-config helpers.
//!
//! These free functions used to live in `app.rs` — a file named after the dead
//! `App` stack. They are not part of `App`: `chat.rs` imports them 43 times,
//! `engine.rs`, `ompcast.rs` and `main.rs` too, and one new dead helper landed
//! in `app.rs` this week precisely because the live code and the dead stack
//! shared a file. Nothing here depends on `App`.

use titi_tui::caps::MousePreset;

/// The config key that stores the mouse-tracking preset.
pub const MOUSE_TRACKING_KEY: &str = "display.mouse_tracking";

/// Load the persisted mouse preset from the titi config.
///
/// `agent_dir` is the settings root (see [`titi_config::agent_dir`]).
/// Returns `None` when the key is absent or unparsable (caller falls back to
/// its own default).
pub fn load_mouse_preset_from(agent_dir: &std::path::Path) -> Option<MousePreset> {
    use titi_config::settings::Settings;
    let settings = Settings::load(
        agent_dir,
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        &[],
    )
    .ok()?;
    let value = settings.get(MOUSE_TRACKING_KEY)?;
    let name = match value {
        serde_json::Value::String(s) => s,
        _ => return None,
    };
    MousePreset::parse(&name)
}

/// Load the persisted mouse preset using the real agent directory.
pub fn load_mouse_preset() -> Option<MousePreset> {
    load_mouse_preset_from(&titi_config::agent_dir())
}

/// Persist the mouse preset to the titi config (`display.mouse_tracking`).
pub fn save_mouse_preset_to(
    agent_dir: &std::path::Path,
    preset: MousePreset,
) -> Result<(), String> {
    use titi_config::settings::Settings;
    let mut settings = Settings::load(
        agent_dir,
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        &[],
    )
    .map_err(|e| format!("{e}"))?;
    settings
        .set(MOUSE_TRACKING_KEY, serde_json::json!(preset.name()))
        .map_err(|e| format!("{e}"))
}

/// Persist the mouse preset using the real agent directory.
pub fn save_mouse_preset(preset: MousePreset) -> Result<(), String> {
    save_mouse_preset_to(&titi_config::agent_dir(), preset)
}

pub fn list_sessions_from(agent_dir: &std::path::Path) -> Vec<String> {
    let dir = agent_dir.join("sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut ids: Vec<(std::time::SystemTime, String)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((
                modified,
                e.path().file_stem()?.to_string_lossy().into_owned(),
            ))
        })
        .collect();
    ids.sort_by_key(|a| std::cmp::Reverse(a.0));
    ids.into_iter().map(|(_, id)| id).collect()
}

/// [`list_sessions_from`] against the real agent directory.
pub fn list_sessions() -> Vec<String> {
    list_sessions_from(&titi_config::agent_dir())
}

/// How much of a session file the welcome reads: the first prompt and a fair
/// count of turns, never the whole transcript of a long session.
const BRIEF_WINDOW: u64 = 128 * 1024;

/// The longest title kept, in characters. The welcome cuts it again to fit its
/// row; this only keeps a pasted prompt from being carried around whole.
const BRIEF_TITLE_CHARS: usize = 80;

/// What the welcome says about a stored session: the first thing the user
/// asked, how long ago it was last written, and how many turns it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBrief {
    /// The first prompt on one line, cut with `…`. `None` while the session
    /// has none, or when its first entry is larger than the window read.
    pub title: Option<String>,
    /// User prompts counted in the window read.
    pub turns: usize,
    /// The window ended before the file did, so `turns` is a floor.
    pub turns_open: bool,
    /// When the file was last written.
    pub modified: Option<std::time::SystemTime>,
}

impl SessionBrief {
    /// `4 turns`, `1 turn`, `12+ turns` when the count is a floor.
    pub fn turns_label(&self) -> String {
        let more = if self.turns_open { "+" } else { "" };
        let noun = if self.turns == 1 && !self.turns_open {
            "turn"
        } else {
            "turns"
        };
        format!("{}{more} {noun}", self.turns)
    }

    /// How long ago the session was last written, as [`relative_age`] words
    /// it; `None` when the file's time is unknown.
    pub fn age(&self, now: std::time::SystemTime) -> Option<String> {
        let modified = self.modified?;
        Some(relative_age(
            now.duration_since(modified).unwrap_or_default(),
        ))
    }
}

/// `now`, `2m`, `3h`, `yesterday`, `5d`: one unit, the largest that fits.
pub fn relative_age(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..60 => "now".to_owned(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        86_400..172_800 => "yesterday".to_owned(),
        _ => format!("{}d", secs / 86_400),
    }
}

/// A prompt as a one-line title: whitespace collapsed to single spaces, cut
/// with `…` past [`BRIEF_TITLE_CHARS`]. `None` for a prompt with no words.
pub fn prompt_title(prompt: &str) -> Option<String> {
    let line = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= BRIEF_TITLE_CHARS {
        return Some(line);
    }
    let head: String = line.chars().take(BRIEF_TITLE_CHARS - 1).collect();
    Some(format!("{}…", head.trim_end()))
}

/// The last brief read of each session file, valid while its modification time
/// and length stand: the welcome draws every 50 ms and must not reread files.
type BriefStamp = (Option<std::time::SystemTime>, u64);
static BRIEFS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, (BriefStamp, SessionBrief)>>,
> = std::sync::LazyLock::new(Default::default);

/// The brief of session `id`: at most the first [`BRIEF_WINDOW`] bytes of its
/// file, and nothing at all when the file is unchanged since the last read.
pub fn session_brief(agent_dir: &std::path::Path, id: &str) -> SessionBrief {
    let path = agent_dir.join("sessions").join(format!("{id}.jsonl"));
    let stamp = std::fs::metadata(&path)
        .ok()
        .map(|meta| (meta.modified().ok(), meta.len()));
    if let Some(stamp) = stamp
        && let Ok(cache) = BRIEFS.lock()
        && let Some((seen, brief)) = cache.get(&path)
        && *seen == stamp
    {
        return brief.clone();
    }
    let brief = read_brief(&path);
    if let Some(stamp) = stamp
        && let Ok(mut cache) = BRIEFS.lock()
    {
        cache.insert(path, (stamp, brief.clone()));
    }
    brief
}

fn read_brief(path: &std::path::Path) -> SessionBrief {
    use std::io::Read;
    let mut brief = SessionBrief {
        title: None,
        turns: 0,
        turns_open: false,
        modified: std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok(),
    };
    let Ok(file) = std::fs::File::open(path) else {
        return brief;
    };
    let mut window = Vec::new();
    if file
        .take(BRIEF_WINDOW + 1)
        .read_to_end(&mut window)
        .is_err()
    {
        return brief;
    }
    if window.len() as u64 > BRIEF_WINDOW {
        // Whole lines only: the last one is cut wherever the window ended.
        brief.turns_open = true;
        let end = window
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        window.truncate(end);
    }
    for line in String::from_utf8_lossy(&window).lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if entry.get("role").and_then(serde_json::Value::as_str) != Some("user") {
            continue;
        }
        brief.turns += 1;
        if brief.title.is_none() {
            brief.title = entry
                .get("content")
                .and_then(serde_json::Value::as_str)
                .and_then(prompt_title);
        }
    }
    brief
}

/// The session a `--continue` or `session.autoResume` launch resumes: the
/// newest session recorded in the workspace titi is running in.
///
/// [`newest_session_in`] is the whole rule — including the fallback that keeps
/// a session from before workspaces were recorded reachable; this passes the
/// directory titi runs in, the same one [`current_workspace`] pins a
/// checkpoint to.
pub fn newest_session(agent_dir: &std::path::Path) -> Option<String> {
    newest_session_in(agent_dir, &current_workspace())
}

/// The newest session recorded in `workspace`, or the newest session overall
/// when this workspace has none.
///
/// Both branches answer in the order the picker lists — the sessions
/// directory by modification time, which is the first row Ctrl+X shows — so
/// `--continue` resumes the row the user would have picked, not merely the
/// one created last:
///
/// * a session whose recorded workspace is `workspace` answers first, however
///   old it is. This is the branch that makes two projects separate: the
///   session of the project you are in wins over a newer one from another;
/// * nothing recorded for this workspace falls back to the newest session
///   anywhere. That is the answer this used to give unconditionally, and it
///   is what serves a session written before sessions carried a workspace, or
///   a first run in a new project — such a session is reachable rather than
///   hidden behind a filter it cannot satisfy;
/// * an index that cannot be read at all (absent, or written by a newer
///   release) falls back the same way, because the JSONL files, not the
///   index, are the sessions.
///
/// `None` means there is nothing to resume at all.
pub fn newest_session_in(
    agent_dir: &std::path::Path,
    workspace: &std::path::Path,
) -> Option<String> {
    let listed = list_sessions_from(agent_dir);
    if let Some(here) = ids_in_workspace(agent_dir, workspace)
        && let Some(id) = listed.iter().find(|id| here.contains(id.as_str()))
    {
        return Some(id.clone());
    }
    listed.into_iter().next()
}

/// The session ids the index records as started in `workspace`.
///
/// `None` when the index cannot be read — there is none yet, it is
/// unreadable, or it was written by a newer release — because no session can
/// be shown to belong to `workspace` without it. The lookup answers "which
/// sessions are this project's", so it does not create an index to do it:
/// `SessionStore::new` would, and an agent directory that has none is
/// answered from its files alone, which is what the caller falls back to.
pub(crate) fn ids_in_workspace(
    agent_dir: &std::path::Path,
    workspace: &std::path::Path,
) -> Option<std::collections::HashSet<String>> {
    if !agent_dir.join("state.db").exists() {
        return None;
    }
    let store = titi_core::session::SessionStore::new(agent_dir).ok()?;
    let root = workspace.to_string_lossy();
    let ids = store.sessions_in(Some(root.as_ref())).ok()?;
    Some(ids.into_iter().collect())
}

/// Delete a session: its JSONL transcript and the side files that belong to it.
///
/// The transcript is the session; its trajectory and its traces are what the
/// session recorded, and deleting one while leaving the others keeps exactly
/// what the user asked to remove — a trajectory replayable after its session
/// was "deleted" is not deleted. Callers must gate this behind an approval
/// prompt — Esc never reaches here.
pub fn delete_session_from(agent_dir: &std::path::Path, id: &str) -> Result<(), String> {
    let path = agent_dir.join("sessions").join(format!("{id}.jsonl"));
    std::fs::remove_file(&path).map_err(|e| format!("{e}"))?;
    // A side file that is already gone is not an error — a session written
    // before trajectories existed has none. One that cannot be removed is
    // reported even though the transcript is already gone.
    remove_if_present(&agent_dir.join("trajectories").join(format!("{id}.jsonl")))?;
    remove_if_present(&titi_core::trace::session_dir(agent_dir, id))?;
    Ok(())
}

/// Removes a file or a directory tree if it exists; a missing path is `Ok`.
fn remove_if_present(path: &std::path::Path) -> Result<(), String> {
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{e}")),
    }
}

/// [`delete_session_from`] against the real agent directory.
pub fn delete_session(id: &str) -> Result<(), String> {
    delete_session_from(&titi_config::agent_dir(), id)
}

/// Creates an empty session and returns its id.
pub fn new_session(agent_dir: &std::path::Path) -> Result<String, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    store
        .create(titi_core::session::SessionMeta {
            title: Some("titi".into()),
            source: Some("cli".into()),
            ..Default::default()
        })
        .map_err(|e| e.to_string())
}

pub fn fork_session(agent_dir: &std::path::Path, session_id: &str) -> Result<String, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    let new_id = store
        .fork_session(session_id, titi_core::session::SessionMeta::default())
        .map_err(|e| e.to_string())?;
    Ok(format!("forked to {new_id} · restart to resume it"))
}

pub fn export_session(
    agent_dir: &std::path::Path,
    session_id: &str,
    path: &str,
) -> Result<String, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;

    // Default to markdown if not specified in path
    let format = if path.ends_with(".jsonl") {
        titi_core::session::export::ExportFormat::Jsonl
    } else {
        titi_core::session::export::ExportFormat::Markdown
    };

    let path_val = if path.is_empty() {
        let exports_dir = agent_dir.join("exports");
        let _ = std::fs::create_dir_all(&exports_dir);
        exports_dir.join(format!("{session_id}.md"))
    } else {
        std::path::PathBuf::from(path.to_owned())
    };

    store
        .export_to_file(session_id, format, &path_val)
        .map_err(|e| e.to_string())?;

    Ok(format!("exported to {}", path_val.display()))
}

/// The conversation a resumed session replays: the path to its current leaf,
/// capped at a boundary that keeps every tool round whole, so an old
/// transcript cannot crowd out the workspace map or replay an orphan call.
pub fn session_history(
    agent_dir: &std::path::Path,
    session_id: &str,
) -> Result<Vec<titi_providers::ChatMessage>, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    let entries = store.walk(session_id, None).map_err(|e| e.to_string())?;
    Ok(crate::engine::restore_window(
        titi_core::session::entries_to_messages(&entries),
        crate::engine::MAX_RESTORED_MESSAGES,
    ))
}

/// The directory a checkpoint pins and a rewind restores: where titi runs.
pub fn current_workspace() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| ".".into())
}

/// Record a rewind point on a session; returns a human summary.
///
/// `workspace` is explicit: taking the process cwd here made the tests
/// commit into whatever checkout ran them.
pub fn checkpoint_session(
    agent_dir: &std::path::Path,
    workspace: &std::path::Path,
    session_id: &str,
) -> Result<String, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    let mut checkpoint = store.checkpoint(session_id).map_err(|e| e.to_string())?;
    // Also pin the workspace, so a later rewind can undo code and not only
    // the transcript. A directory that is not a repo stays session-only.
    let git = crate::git_checkpoint::snapshot(
        workspace,
        &format!("{session_id} · {} entries", checkpoint.entries),
    );
    if let Ok(commit) = &git {
        checkpoint.git_commit = Some(commit.clone());
        let _ = store.record_git_commit(session_id, commit);
    }
    let suffix = match &git {
        Ok(commit) => format!(" · git {}", &commit[..7.min(commit.len())]),
        Err(_) => String::new(),
    };
    Ok(format!(
        "checkpoint: {} entries{suffix}",
        checkpoint.entries
    ))
}

/// List a session's rewind points, oldest first.
pub fn list_checkpoints(agent_dir: &std::path::Path, session_id: &str) -> Result<String, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    let all = store.checkpoints(session_id).map_err(|e| e.to_string())?;
    if all.is_empty() {
        return Ok("checkpoints: none".into());
    }
    let rows: Vec<String> = all
        .iter()
        .enumerate()
        .map(|(i, cp)| format!("#{} · {} entries", i + 1, cp.entries))
        .collect();
    Ok(format!("checkpoints: {}", rows.join(" | ")))
}

/// Rewind a session to checkpoint `index` (1-based); the newest when `None`.
pub fn rewind_session(
    agent_dir: &std::path::Path,
    workspace: &std::path::Path,
    session_id: &str,
    index: Option<usize>,
) -> Result<String, String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    let all = store.checkpoints(session_id).map_err(|e| e.to_string())?;
    if all.is_empty() {
        return Err("no checkpoints recorded".into());
    }
    let position = match index {
        None => all.len() - 1,
        Some(0) => return Err("checkpoints are numbered from 1".into()),
        Some(n) if n <= all.len() => n - 1,
        Some(n) => return Err(format!("no checkpoint #{n} (have {})", all.len())),
    };
    let target = all[position].clone();
    store
        .rewind(session_id, &target)
        .map_err(|e| e.to_string())?;
    // Put the files back too, when the checkpoint pinned a commit and the
    // tree is clean. A dirty tree is reported rather than overwritten.
    let git = match &target.git_commit {
        Some(commit) => match crate::git_checkpoint::restore(workspace, commit) {
            Ok(()) => format!(" · git {}", &commit[..7.min(commit.len())]),
            Err(reason) => format!(" · git not restored: {reason}"),
        },
        None => String::new(),
    };
    Ok(format!(
        "rewound to checkpoint #{} ({} entries){git}",
        position + 1,
        target.entries
    ))
}

/// Moves the session's leaf to `entry_id` — the branch point `/tree` picks —
/// and returns the history the path through it now holds, with the line the
/// screen says about it.
///
/// The store is append-only: the entries the old branch held are still there,
/// and moving the leaf back only changes which path the next turn continues.
/// The history is the same walk [`session_history`] does after the move, so a
/// branch and a resume cannot disagree about what the model is shown.
pub fn branch_at(
    agent_dir: &std::path::Path,
    session_id: &str,
    entry_id: &str,
) -> Result<(Vec<titi_providers::ChatMessage>, String), String> {
    let store = titi_core::session::SessionStore::new(agent_dir).map_err(|e| e.to_string())?;
    let entries = store.open(session_id).map_err(|e| e.to_string())?;
    let at = entries
        .iter()
        .position(|entry| entry.id == entry_id)
        .ok_or_else(|| format!("{entry_id} is not an entry of this session"))?;
    store
        .fork(session_id, entry_id)
        .map_err(|e| e.to_string())?;
    let path = store.walk(session_id, None).map_err(|e| e.to_string())?;
    let messages = crate::engine::restore_window(
        titi_core::session::entries_to_messages(&path),
        crate::engine::MAX_RESTORED_MESSAGES,
    );
    // Everything the path no longer holds: the branch left behind, counted so
    // the line says what the move cost rather than only where it went.
    let off = match entries.len().saturating_sub(path.len()) {
        1 => "1 entry is off the path now".to_owned(),
        other => format!("{other} entries are off the path now"),
    };
    let where_ = at + 1;
    Ok((messages, format!("branched at entry {where_} · {off}")))
}

/// Writes a pasted body under the workspace's `.titi/pastes/` and returns the
/// path as the draft — and so the model — should read it: relative to the
/// workspace, because that is the form `read` resolves and the form the person
/// sees.
///
/// A paste attached this way is a file the workspace holds like any other: the
/// point is that the model can read it in ranges instead of paying for it in
/// every request. The directory is titi's own (the one the project layer
/// already uses), so a project that ignores `.titi/` ignores these too.
///
/// It also ignores *itself*: a pasted log, config or stack trace often carries
/// a secret, and `git add -A` in the workspace must not sweep one in. The
/// `.gitignore` is written first and only when there is none — the repository's
/// own `.gitignore` is never touched — and a directory that will not take it
/// fails the attach rather than leaving a paste file un-ignored.
pub fn write_paste(workspace: &std::path::Path, seq: u32, body: &str) -> Result<String, String> {
    let name = format!("paste-{seq}.txt");
    let dir = workspace.join(".titi").join("pastes");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n").map_err(|e| format!(".gitignore: {e}"))?;
    }
    std::fs::write(dir.join(&name), body).map_err(|e| e.to_string())?;
    Ok(format!(".titi/pastes/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &std::path::Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("{e}"));
        }
        std::fs::write(path, "{}\n").unwrap_or_else(|e| panic!("{e}"));
    }

    #[test]
    fn deleting_a_session_removes_its_trajectory_and_its_traces() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let sessions = dir.path().join("sessions");
        touch(&sessions.join("s1.jsonl"));
        touch(&sessions.join("s2.jsonl"));
        let trajectory = dir.path().join("trajectories").join("s1.jsonl");
        touch(&trajectory);
        let traces = titi_core::trace::session_dir(dir.path(), "s1");
        touch(&traces.join("1.jsonl"));

        delete_session_from(dir.path(), "s1").unwrap_or_else(|e| panic!("delete: {e}"));

        assert!(!sessions.join("s1.jsonl").exists());
        assert!(!trajectory.exists(), "the trajectory outlived its session");
        assert!(!traces.exists(), "the traces outlived their session");
        // A sibling session keeps everything of its own.
        assert!(sessions.join("s2.jsonl").exists());
    }

    #[test]
    fn deleting_a_session_with_no_side_files_is_still_a_success() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let sessions = dir.path().join("sessions");
        touch(&sessions.join("s1.jsonl"));
        delete_session_from(dir.path(), "s1").unwrap_or_else(|e| panic!("delete: {e}"));
        assert!(!sessions.join("s1.jsonl").exists());
    }

    /// One `user` entry as the session file writes it.
    fn user_line(text: &str) -> String {
        serde_json::json!({
            "id": "e", "parent_id": null, "role": "user", "content": text, "ts": 1
        })
        .to_string()
    }

    fn write_session(agent_dir: &std::path::Path, id: &str, lines: &[String]) {
        let dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(dir.join(format!("{id}.jsonl")), lines.join("\n") + "\n")
            .unwrap_or_else(|e| panic!("{e}"));
    }

    #[test]
    fn a_title_is_the_first_prompt_on_one_line() {
        assert_eq!(
            prompt_title("  fix the\n\nflaky   test\tplease "),
            Some("fix the flaky test please".to_owned())
        );
        assert_eq!(prompt_title(" \n\t "), None);
        let long = "word ".repeat(40);
        let title = prompt_title(&long).unwrap_or_default();
        assert!(
            title.ends_with("word…") && title.chars().count() <= 80,
            "{title}"
        );
        let exact = "x".repeat(80);
        assert_eq!(prompt_title(&exact), Some(exact));
    }

    #[test]
    fn a_brief_reads_the_first_prompt_and_counts_the_turns() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let assistant = serde_json::json!({
            "id": "a", "parent_id": "e", "role": "assistant", "content": "hi", "ts": 2
        })
        .to_string();
        write_session(
            dir.path(),
            "s1",
            &[
                user_line("read the readme\nand summarise it"),
                assistant.clone(),
                user_line("now run the tests"),
            ],
        );
        let brief = session_brief(dir.path(), "s1");
        assert_eq!(
            brief.title.as_deref(),
            Some("read the readme and summarise it")
        );
        assert_eq!((brief.turns, brief.turns_open), (2, false));
        assert_eq!(brief.turns_label(), "2 turns");

        write_session(dir.path(), "s2", &[user_line("one")]);
        assert_eq!(session_brief(dir.path(), "s2").turns_label(), "1 turn");

        // No prompt yet: no title, and zero turns.
        write_session(dir.path(), "s3", &[assistant]);
        let empty = session_brief(dir.path(), "s3");
        assert_eq!((empty.title.as_deref(), empty.turns), (None, 0));
        assert_eq!(empty.turns_label(), "0 turns");

        // A file that is gone is an empty brief, not a failure.
        assert_eq!(session_brief(dir.path(), "nope").title, None);
    }

    #[test]
    fn a_brief_never_reads_past_its_window() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        // 200 prompts of ~1 KiB: well past the 128 KiB window.
        let lines: Vec<String> = (0..200)
            .map(|n| user_line(&format!("prompt {n} {}", "x".repeat(1_000))))
            .collect();
        write_session(dir.path(), "big", &lines);
        let brief = session_brief(dir.path(), "big");
        assert!(brief.turns_open, "the window ended first");
        assert!(brief.turns > 100 && brief.turns < 200, "{}", brief.turns);
        assert!(
            brief.turns_label().contains("+ turns"),
            "{}",
            brief.turns_label()
        );
        assert!(
            brief
                .title
                .as_deref()
                .is_some_and(|t| t.starts_with("prompt 0 x"))
        );

        // A first entry larger than the whole window cannot be parsed: the
        // session reads as untitled rather than as a cut sentence.
        write_session(dir.path(), "huge", &[user_line(&"y".repeat(200_000))]);
        let huge = session_brief(dir.path(), "huge");
        assert_eq!((huge.title, huge.turns, huge.turns_open), (None, 0, true));
    }

    #[test]
    fn a_changed_file_is_read_again() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        write_session(dir.path(), "s", &[user_line("first")]);
        assert_eq!(session_brief(dir.path(), "s").turns, 1);
        write_session(dir.path(), "s", &[user_line("first"), user_line("second")]);
        assert_eq!(session_brief(dir.path(), "s").turns, 2);
    }

    #[test]
    fn ages_read_in_the_largest_unit() {
        use std::time::Duration;
        let age = |secs: u64| relative_age(Duration::from_secs(secs));
        assert_eq!(age(0), "now");
        assert_eq!(age(59), "now");
        assert_eq!(age(60), "1m");
        assert_eq!(age(2 * 60 + 30), "2m");
        assert_eq!(age(59 * 60), "59m");
        assert_eq!(age(3_600), "1h");
        assert_eq!(age(3 * 3_600 + 59), "3h");
        assert_eq!(age(23 * 3_600 + 3_599), "23h");
        assert_eq!(age(86_400), "yesterday");
        assert_eq!(age(47 * 3_600), "yesterday");
        assert_eq!(age(2 * 86_400), "2d");
        assert_eq!(age(5 * 86_400 + 1), "5d");

        let now = std::time::SystemTime::now();
        let brief = |modified| SessionBrief {
            title: None,
            turns: 0,
            turns_open: false,
            modified,
        };
        assert_eq!(
            brief(Some(now - Duration::from_secs(180)))
                .age(now)
                .as_deref(),
            Some("3m")
        );
        assert_eq!(
            brief(Some(now + Duration::from_secs(60)))
                .age(now)
                .as_deref(),
            Some("now")
        );
        assert_eq!(brief(None).age(now), None);
    }
}
