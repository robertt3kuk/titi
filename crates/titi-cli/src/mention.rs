//! `@file` mentions: the picker a word-initial `@` opens, and what a submitted
//! `@path` hands the model.
//!
//! The picker is a function of the draft, like the emoji one: [`mention_at`]
//! reads the `@word` the caret is in, [`MentionState::sync`] ranks the
//! workspace's files against it, and the panel shows the result while there is
//! one. A word that matches no file never opens the picker, so `@scope/pkg`
//! stays the text it was typed as.
//!
//! On submit [`attach`] looks at every `@path` of the draft. The ones that name
//! a regular file inside the workspace travel to the model as an
//! `<attached-files>` block after the prompt; the draft on screen and the
//! transcript keep only the `@path`.

use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};
use std::process::{Command, Stdio};

use crate::pickers::fuzzy_score;

/// The most paths the picker holds. Shallow paths are kept first, so a huge
/// tree loses its deepest files rather than its top level.
pub(crate) const MAX_CANDIDATES: usize = 20_000;
/// The most rows a query keeps; the panel windows them.
const MAX_MATCHES: usize = 100;
/// What one attachment may inline: lines, then bytes.
pub(crate) const MAX_INLINE_LINES: usize = 200;
pub(crate) const MAX_INLINE_BYTES: usize = 16 * 1024;
/// Files per prompt, and bytes over all of them. A prompt that names more is
/// not refused: the rest are named to the model, which can read them.
const MAX_ATTACHMENTS: usize = 8;
const TOTAL_INLINE_BYTES: usize = 48 * 1024;

const ATTACHMENTS_OPEN: &str = "\n\n<attached-files>\n";
const ATTACHMENTS_CLOSE: &str = "</attached-files>";

/// Directories the fallback walk (a workspace with no git repository) skips.
const PRUNED_DIRS: &[&str] = &[
    "node_modules",
    "dist",
    "build",
    "target",
    "coverage",
    "vendor",
    "__pycache__",
];

// ---------------------------------------------------------------------------
// The word under the caret
// ---------------------------------------------------------------------------

/// The `@word` the caret is in the middle of or at the end of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mention<'a> {
    /// Where the `@` is.
    pub(crate) start: usize,
    /// What follows the `@` up to the caret.
    pub(crate) query: &'a str,
}

/// The mention the caret sits in: a whitespace-delimited word that *starts*
/// with `@`. `a@b` is an address and not a mention, and a space ends one.
pub(crate) fn mention_at(input: &str, caret: usize) -> Option<Mention<'_>> {
    let head = input.get(..caret.min(input.len()))?;
    let start = head
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(0, |(at, ch)| at + ch.len_utf8());
    let query = head[start..].strip_prefix('@')?;
    Some(Mention { start, query })
}

// ---------------------------------------------------------------------------
// The workspace's files
// ---------------------------------------------------------------------------

/// The files the picker offers, paths relative to `root` with `/` separators,
/// shallowest first.
///
/// Inside a git repository `git` is the authority on what is ignored, so the
/// list is its tracked and untracked-but-not-ignored files. Outside one there
/// is nothing to ignore by, and a plain walk skips hidden and build
/// directories. Names with whitespace or control characters are left out: an
/// `@path` ends at the first space, so they could not be written back.
pub(crate) fn list_workspace_files(root: &Path) -> Vec<String> {
    let mut paths = git_files(root).unwrap_or_else(|| walk_files(root));
    paths.retain(|path| {
        !path.is_empty() && !path.chars().any(|ch| ch.is_control() || ch.is_whitespace())
    });
    paths.sort_unstable_by(|left, right| {
        depth(left).cmp(&depth(right)).then_with(|| left.cmp(right))
    });
    paths.dedup();
    paths.truncate(MAX_CANDIDATES);
    paths
}

fn depth(path: &str) -> usize {
    path.matches('/').count()
}

fn git_files(root: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .filter_map(|path| std::str::from_utf8(path).ok())
            .map(str::to_owned)
            .collect(),
    )
}

fn walk_files(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // Not `metadata`: a link is not followed, so one pointing at `~`
            // cannot walk the list out of the workspace.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let child = if rel.is_empty() {
                name.to_owned()
            } else {
                format!("{rel}/{name}")
            };
            if kind.is_dir() {
                if !name.starts_with('.') && !PRUNED_DIRS.contains(&name) {
                    pending.push((entry.path(), child));
                }
            } else if kind.is_file() {
                found.push(child);
            }
        }
        // The depth sort keeps the shallow ones; this only bounds the walk.
        if found.len() >= MAX_CANDIDATES * 4 {
            break;
        }
    }
    found
}

/// The files `query` matches, best first, as indexes into `files`.
///
/// [`fuzzy_score`] scores the whole path; a query that also spells out the
/// file's own name scores above one that only scatters over the directories,
/// and the shorter path wins a tie. An empty query keeps the list's order.
pub(crate) fn rank(files: &[String], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..files.len().min(MAX_MATCHES)).collect();
    }
    let mut scored: Vec<(i32, usize)> = files
        .iter()
        .enumerate()
        .filter_map(|(at, path)| {
            let whole = fuzzy_score(query, path)?;
            let name = path.rsplit('/').next().unwrap_or(path);
            let named = if fuzzy_score(query, name).is_some() {
                8
            } else {
                0
            };
            Some((whole + named, at))
        })
        .collect();
    scored.sort_unstable_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| files[left.1].len().cmp(&files[right.1].len()))
            .then_with(|| left.1.cmp(&right.1))
    });
    scored.truncate(MAX_MATCHES);
    scored.into_iter().map(|(_, at)| at).collect()
}

// ---------------------------------------------------------------------------
// The picker
// ---------------------------------------------------------------------------

/// What the panel shows while a mention is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MentionPicker {
    /// Where the `@` is in the draft.
    pub(crate) start: usize,
    pub(crate) query: String,
    /// Indexes into the state's file list, best first.
    pub(crate) matches: Vec<usize>,
    pub(crate) selected: usize,
}

/// The files listed for the mention in progress and the picker over them.
#[derive(Debug, Default)]
pub(crate) struct MentionState {
    files: Vec<String>,
    /// The `@` the list was built for. A new `@` — or the same word typed
    /// again after the caret left it — lists the workspace afresh, so a file
    /// created since is offered; typing inside one word never does.
    active: Option<usize>,
    /// Esc closed the picker for this word; it stays closed until the word
    /// goes.
    dismissed: bool,
    pub(crate) picker: Option<MentionPicker>,
}

impl MentionState {
    pub(crate) fn is_visible(&self) -> bool {
        self.picker.is_some()
    }

    /// Follow the draft: open, narrow or close the picker for the mention under
    /// the caret. `list` builds the file list when a mention opens.
    pub(crate) fn sync(&mut self, token: Option<Mention<'_>>, list: impl FnOnce() -> Vec<String>) {
        let Some(token) = token else {
            self.active = None;
            self.dismissed = false;
            self.picker = None;
            return;
        };
        if self.active != Some(token.start) {
            self.files = list();
            self.active = Some(token.start);
            self.dismissed = false;
        }
        if self.dismissed {
            self.picker = None;
            return;
        }
        let matches = rank(&self.files, token.query);
        if matches.is_empty() {
            self.picker = None;
            return;
        }
        let selected = match &self.picker {
            Some(open) if open.query == token.query && open.start == token.start => {
                open.selected.min(matches.len() - 1)
            }
            _ => 0,
        };
        self.picker = Some(MentionPicker {
            start: token.start,
            query: token.query.to_owned(),
            matches,
            selected,
        });
    }

    /// Esc: close the picker and leave the text.
    pub(crate) fn dismiss(&mut self) {
        self.dismissed = true;
        self.picker = None;
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        if let Some(picker) = self.picker.as_mut() {
            let len = picker.matches.len() as isize;
            picker.selected = (picker.selected as isize + delta).rem_euclid(len.max(1)) as usize;
        }
    }

    /// The highlighted file.
    pub(crate) fn selected_path(&self) -> Option<&str> {
        let picker = self.picker.as_ref()?;
        let at = *picker.matches.get(picker.selected)?;
        self.files.get(at).map(String::as_str)
    }

    pub(crate) fn path_at(&self, index: usize) -> Option<&str> {
        self.files.get(index).map(String::as_str)
    }

    pub(crate) fn file_count(&self) -> usize {
        self.files.len()
    }
}

// ---------------------------------------------------------------------------
// What a submitted `@path` hands the model
// ---------------------------------------------------------------------------

/// What [`attach`] made of a draft.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Attachments {
    /// Appended to the prompt the model reads; empty when nothing attached.
    pub(crate) block: String,
    /// One line per thing worth telling the user, for the transcript.
    pub(crate) notes: Vec<String>,
}

/// What an `@path` of the draft points at.
#[derive(Debug, PartialEq, Eq)]
enum Resolved {
    /// A regular file inside the workspace.
    File(std::path::PathBuf),
    /// A path that leaves the workspace — absolute, `..`, `~`, or a link out.
    Outside,
    /// Not a file: plain text, as typed.
    Missing,
}

/// The `@path` words of a draft, in order, once each.
fn mentioned(draft: &str) -> Vec<&str> {
    let mut seen: Vec<&str> = Vec::new();
    for word in draft.split_whitespace() {
        if let Some(path) = word.strip_prefix('@')
            && !path.is_empty()
            && !seen.contains(&path)
        {
            seen.push(path);
        }
    }
    seen
}

fn resolve_exact(workspace: &Path, raw: &str) -> Resolved {
    if raw.is_empty() {
        return Resolved::Missing;
    }
    let path = Path::new(raw);
    let escapes = path.is_absolute()
        || raw == "~"
        || raw.starts_with("~/")
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
    if escapes {
        return Resolved::Outside;
    }
    let (Ok(real), Ok(root)) = (
        workspace.join(path).canonicalize(),
        workspace.canonicalize(),
    ) else {
        return Resolved::Missing;
    };
    if !real.starts_with(&root) {
        return Resolved::Outside;
    }
    if real.is_file() {
        Resolved::File(real)
    } else {
        Resolved::Missing
    }
}

/// `raw` as written, then with trailing sentence punctuation taken off one
/// character at a time: `@src/lib.rs,` names `src/lib.rs`. Returns what it
/// resolved to and the path as it stands after trimming.
fn resolve<'a>(workspace: &Path, raw: &'a str) -> (Resolved, &'a str) {
    let mut candidate = raw;
    loop {
        match resolve_exact(workspace, candidate) {
            Resolved::Missing => {}
            found => return (found, candidate),
        }
        match candidate.chars().next_back() {
            Some(ch) if is_trailing_punctuation(ch) => {
                candidate = &candidate[..candidate.len() - ch.len_utf8()];
            }
            _ => return (Resolved::Missing, raw),
        }
    }
}

fn is_trailing_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'' | '`'
    )
}

/// How a file reads into a prompt.
enum Body {
    Text {
        text: String,
        lines: usize,
        truncated: bool,
    },
    Binary,
    Unreadable(String),
}

/// The head of a file, within `cap` bytes and [`MAX_INLINE_LINES`] lines.
fn load(path: &Path, cap: usize) -> (Body, u64) {
    let opened = File::open(path).and_then(|file| {
        let size = file.metadata()?.len();
        let mut bytes = Vec::new();
        file.take(cap as u64 + 1).read_to_end(&mut bytes)?;
        Ok((bytes, size))
    });
    let (bytes, size) = match opened {
        Ok(read) => read,
        Err(error) => return (Body::Unreadable(error.to_string()), 0),
    };
    if bytes.contains(&0) {
        return (Body::Binary, size);
    }
    let over_bytes = bytes.len() > cap;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(cap)]);
    let mut lines: Vec<&str> = text.split_inclusive('\n').collect();
    // A cut that lands mid-line drops the partial line rather than showing
    // half of it as if it were whole.
    if over_bytes && !text.ends_with('\n') && lines.len() > 1 {
        lines.pop();
    }
    let over_lines = lines.len() > MAX_INLINE_LINES;
    lines.truncate(MAX_INLINE_LINES);
    let shown = lines.len();
    (
        Body::Text {
            text: lines.concat(),
            lines: shown,
            truncated: over_bytes || over_lines,
        },
        size,
    )
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// The privacy the runtime tools read with: the user's credential policy and
/// whether addresses are masked too, from the same settings (`privacy.*`).
struct Privacy {
    policy: titi_tools::SensitivePolicy,
    mask_ips: bool,
}

impl Privacy {
    fn load(agent_dir: &Path, workspace: &Path) -> Self {
        let settings =
            titi_config::settings::Settings::load(agent_dir, workspace, &[]).unwrap_or_default();
        let (policy, mask_ips) = crate::engine::privacy_policy(&settings);
        Self { policy, mask_ips }
    }

    /// The same pass a tool result gets before it reaches the model.
    fn mask(&self, text: &str) -> titi_memory::redact::Redaction {
        if self.mask_ips {
            titi_memory::redact::redact_for_model(text)
        } else {
            titi_memory::redact::redact(text)
        }
    }
}

/// The attachment block for the `@path`s of `draft`, and what to tell the
/// user about it.
///
/// A small text file is inlined whole; a longer one is inlined up to
/// [`MAX_INLINE_LINES`] lines / [`MAX_INLINE_BYTES`] bytes with a `[truncated`
/// marker that tells the model to read the rest; a binary file, or one the
/// user's credential policy blocks, is only named. What is inlined is masked
/// exactly as a `read` result is, and the note says when something was. A path
/// outside the workspace is refused outright and never opened, and a word that
/// is no file at all is left alone.
pub(crate) fn attach(workspace: &Path, agent_dir: &Path, draft: &str) -> Attachments {
    let mut privacy: Option<Privacy> = None;
    let mut block = String::new();
    let mut notes = Vec::new();
    let mut summary: Vec<String> = Vec::new();
    let mut budget = TOTAL_INLINE_BYTES;
    let mut attached = 0usize;
    let mut skipped = 0usize;
    let mut done: Vec<std::path::PathBuf> = Vec::new();
    for raw in mentioned(draft) {
        let (resolved, shown) = resolve(workspace, raw);
        let real = match resolved {
            Resolved::Missing => continue,
            Resolved::Outside => {
                notes.push(format!("not attached: @{raw} is outside the workspace"));
                continue;
            }
            Resolved::File(real) => real,
        };
        // Two spellings of one file are one attachment.
        if done.contains(&real) {
            continue;
        }
        done.push(real.clone());
        let shown = shown.strip_prefix("./").unwrap_or(shown);
        if attached == MAX_ATTACHMENTS {
            skipped += 1;
            continue;
        }
        attached += 1;
        let privacy = privacy.get_or_insert_with(|| Privacy::load(agent_dir, workspace));
        // The link's own name and where it leads are both judged: a link
        // called `notes.txt` that points at `.env` is still `.env`.
        let target = workspace
            .canonicalize()
            .ok()
            .and_then(|root| real.strip_prefix(root).ok().map(Path::to_path_buf));
        if privacy.policy.blocks(Path::new(shown))
            || target.is_some_and(|target| privacy.policy.blocks(&target))
        {
            block.push_str(&format!(
                "<file path=\"{shown}\" note=\"not inlined: looks like credentials\"/>\n"
            ));
            notes.push(format!("not attached: @{shown} looks like credentials"));
            continue;
        }
        if budget == 0 {
            block.push_str(&format!(
                "<file path=\"{shown}\" note=\"not inlined: attachment budget spent · read it with the read tool\"/>\n"
            ));
            summary.push(format!("{shown} · not inlined"));
            continue;
        }
        let (body, size) = load(&real, MAX_INLINE_BYTES.min(budget));
        match body {
            Body::Text {
                text,
                lines,
                truncated,
            } => {
                let masked = privacy.mask(&text);
                budget = budget.saturating_sub(masked.text.len());
                let text = masked.text.trim_end_matches('\n');
                let hidden = if masked.removed > 0 {
                    format!(" · masked {}", masked.removed)
                } else {
                    String::new()
                };
                if truncated {
                    block.push_str(&format!(
                        "<file path=\"{shown}\" truncated=\"true\">\n{text}\n[truncated · {} · first {lines} lines shown · read {shown} for the rest]\n</file>\n",
                        human_size(size)
                    ));
                    summary.push(format!("{shown} · first {lines} lines{hidden}"));
                } else {
                    block.push_str(&format!("<file path=\"{shown}\">\n{text}\n</file>\n"));
                    summary.push(format!("{shown} · {lines} lines{hidden}"));
                }
            }
            Body::Binary => {
                block.push_str(&format!(
                    "<file path=\"{shown}\" note=\"binary file, {} · not inlined\"/>\n",
                    human_size(size)
                ));
                summary.push(format!("{shown} · binary, not inlined"));
            }
            Body::Unreadable(reason) => {
                notes.push(format!(
                    "not attached: @{shown} could not be read ({reason})"
                ));
            }
        }
    }
    if skipped > 0 {
        notes.push(format!(
            "not attached: {skipped} more @files past the limit of {MAX_ATTACHMENTS}"
        ));
    }
    if !summary.is_empty() {
        notes.insert(0, format!("attached {}", summary.join(" · ")));
    }
    if block.is_empty() {
        return Attachments { block, notes };
    }
    block.insert_str(0, ATTACHMENTS_OPEN);
    block.push_str(ATTACHMENTS_CLOSE);
    Attachments { block, notes }
}

/// `text` as the person wrote it: the attachment block [`attach`] appended is
/// cut off, so history and a resumed transcript show the prompt and not the
/// files it carried.
pub(crate) fn without_attachments(text: &str) -> &str {
    if text.ends_with(ATTACHMENTS_CLOSE)
        && let Some(at) = text.find(ATTACHMENTS_OPEN)
    {
        return &text[..at];
    }
    text
}
