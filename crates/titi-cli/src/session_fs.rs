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
fn ids_in_workspace(
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

/// Delete a session's JSONL file.  Callers must gate this behind an
/// approval prompt — Esc never reaches here.
pub fn delete_session_from(agent_dir: &std::path::Path, id: &str) -> Result<(), String> {
    let path = agent_dir.join("sessions").join(format!("{id}.jsonl"));
    std::fs::remove_file(&path).map_err(|e| format!("{e}"))
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
pub fn write_paste(workspace: &std::path::Path, seq: u32, body: &str) -> Result<String, String> {
    let name = format!("paste-{seq}.txt");
    let dir = workspace.join(".titi").join("pastes");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(&name), body).map_err(|e| e.to_string())?;
    Ok(format!(".titi/pastes/{name}"))
}
