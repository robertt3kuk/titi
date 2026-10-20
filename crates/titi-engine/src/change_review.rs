//! `/review`: an independent verdict on what a session changed.
//!
//! The reviewer is the one `/goal` uses ([`AgentReviewer`]): a fresh agent
//! that never sees the conversation. Here it is also handed read-only tools,
//! and a list of the files to look at rather than a patch, so it judges the
//! files as they are on disk.
//!
//! Which files: the ones the session's agent edited, else everything
//! `git diff HEAD` reports. Nothing changed means no model is called and no
//! verdict is made — "nothing to review" is never a pass.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use titi_tools::SensitivePolicy;

use crate::review::{ReviewRequest, Reviewer, Verdict};

/// Files named in one review request; the rest are counted, not listed.
pub const MAX_REVIEW_FILES: usize = 30;

/// Tool rounds the reviewer may spend reading before it must answer.
pub const REVIEW_ROUNDS: u32 = 12;

/// Present in every `/review` request. The scripted provider keys on it, and
/// it tells a reader of a request log what the agent was asked.
pub const REVIEW_MARKER: &str = "[titi-review]";

const REVIEW_BRIEF: &str = "\
Review the uncommitted change in this workspace, the files listed below. Open \
each one with your read-only tools (read, glob, grep) and look for bugs, missing \
tests, broken callers and unfinished work. The counts show how much of a file \
changed, not where: read the whole file. For a deleted file, grep for its path \
and module name and check that nothing still depends on it.\n\
Reply with the verdict on the first line, exactly as \
`VERDICT: PASS|FAIL|PARTIAL — <one-line reason>`, then one finding per line as \
`- path:line — what is wrong`. A PASS has no findings.";

/// Where the files under review came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeSource {
    /// The files the session's agent wrote or edited.
    EditSet,
    /// The uncommitted `git diff HEAD`.
    GitDiff,
}

impl ScopeSource {
    fn label(self) -> &'static str {
        match self {
            Self::EditSet => "files this session edited",
            Self::GitDiff => "uncommitted `git diff HEAD`",
        }
    }
}

/// One file under review and how much of it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub added: u32,
    pub removed: u32,
    pub deleted: bool,
}

impl FileChange {
    fn line(&self) -> String {
        let deleted = if self.deleted { " (deleted)" } else { "" };
        format!(
            "- {} +{}/\u{2212}{}{deleted}",
            self.path, self.added, self.removed
        )
    }
}

/// What a review looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewScope {
    pub source: ScopeSource,
    /// At most [`MAX_REVIEW_FILES`].
    pub files: Vec<FileChange>,
    /// Changed files past the cap, which the request counts but does not list.
    pub omitted: usize,
}

impl ReviewScope {
    /// The request the reviewer is handed: the brief as the goal, the file
    /// list as the evidence.
    pub fn request(&self) -> ReviewRequest {
        let total = self.files.len() + self.omitted;
        let mut evidence = format!("Changed files ({total}, {}):\n", self.source.label());
        for file in &self.files {
            evidence.push_str(&file.line());
            evidence.push('\n');
        }
        if self.omitted > 0 {
            evidence.push_str(&format!(
                "… and {} more files, not listed; find them with glob and grep if the \
                 listed ones point at them.\n",
                self.omitted
            ));
        }
        ReviewRequest::new(format!("{REVIEW_MARKER} {REVIEW_BRIEF}"), evidence)
    }

    /// The listed paths.
    pub fn paths(&self) -> Vec<SmolStr> {
        self.files
            .iter()
            .map(|file| file.path.as_str().into())
            .collect()
    }
}

/// Picks what to review: the session's `edited` paths when any of them still
/// differ from what is committed, else the whole `git diff HEAD`. `None`
/// when neither has a file — there is nothing to judge.
///
/// A path the sensitive-file policy blocks is left out, as it is from the
/// diff the turn prompt carries: the reviewer's tools refuse it too.
pub fn select_scope(
    root: &Path,
    edited: &[String],
    policy: &SensitivePolicy,
) -> Option<ReviewScope> {
    let diff = git_numstat(root);
    let usable =
        |path: &str| !path.contains(|c: char| c.is_control()) && !policy.blocks(Path::new(path));

    let mut seen = Vec::new();
    let mut from_edits = Vec::new();
    let candidates: Vec<String> = edited
        .iter()
        .filter_map(|path| relative_to(root, path))
        .filter(|path| usable(path))
        .collect();
    let tracked = git_tracked(root, &candidates);
    for path in candidates {
        if seen.contains(&path) {
            continue;
        }
        seen.push(path.clone());
        let exists = root.join(&path).is_file();
        let change = match diff.as_ref().and_then(|map| map.get(&path)) {
            Some(&(added, removed)) => Some((added, removed)),
            // A file git does not know has no diff; its whole body is new.
            None if exists && !tracked.contains(&path) => Some((count_lines(&root.join(&path)), 0)),
            // Tracked and unchanged, or created and deleted again.
            None => None,
        };
        if let Some((added, removed)) = change {
            from_edits.push(FileChange {
                path,
                added,
                removed,
                deleted: !exists,
            });
        }
    }
    if !from_edits.is_empty() {
        return Some(capped(ScopeSource::EditSet, from_edits));
    }

    let mut from_diff: Vec<FileChange> = diff?
        .into_iter()
        .filter(|(path, _)| usable(path))
        .map(|(path, (added, removed))| FileChange {
            deleted: !root.join(&path).is_file(),
            path,
            added,
            removed,
        })
        .collect();
    from_diff.sort_by(|a, b| a.path.cmp(&b.path));
    (!from_diff.is_empty()).then(|| capped(ScopeSource::GitDiff, from_diff))
}

fn capped(source: ScopeSource, mut files: Vec<FileChange>) -> ReviewScope {
    let omitted = files.len().saturating_sub(MAX_REVIEW_FILES);
    files.truncate(MAX_REVIEW_FILES);
    ReviewScope {
        source,
        files,
        omitted,
    }
}

/// `path` as a workspace-relative path with no `.` or `..` parts, or `None`
/// when it points outside the workspace.
fn relative_to(root: &Path, path: &str) -> Option<String> {
    let given = Path::new(path.trim());
    let relative = given.strip_prefix(root).unwrap_or(given);
    let mut clean = PathBuf::new();
    for part in relative.components() {
        match part {
            Component::Normal(name) => clean.push(name),
            Component::CurDir => {}
            _ => return None,
        }
    }
    let text = clean.to_str()?;
    (!text.is_empty()).then(|| text.to_owned())
}

fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("--no-pager")
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .current_dir(root)
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// `path -> (added, removed)` for everything `git diff HEAD` reports, or
/// `None` outside a repository (or in one with no commit yet).
fn git_numstat(root: &Path) -> Option<HashMap<String, (u32, u32)>> {
    let raw = git(
        root,
        &[
            "diff",
            "HEAD",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-ext-diff",
        ],
    )?;
    let text = String::from_utf8_lossy(&raw);
    let mut map = HashMap::new();
    for record in text.split('\0').filter(|record| !record.is_empty()) {
        let mut parts = record.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        // A binary file reads `-`: changed, with no line counts.
        map.insert(
            path.to_owned(),
            (added.parse().unwrap_or(0), removed.parse().unwrap_or(0)),
        );
    }
    Some(map)
}

fn git_tracked(root: &Path, paths: &[String]) -> Vec<String> {
    if paths.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["ls-files", "-z", "--"];
    args.extend(paths.iter().map(String::as_str));
    git(root, &args)
        .map(|raw| {
            String::from_utf8_lossy(&raw)
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Newline-terminated lines in a text file; 0 for binary or unreadable.
fn count_lines(path: &Path) -> u32 {
    let Ok(mut file) = std::fs::File::open(path) else {
        return 0;
    };
    let mut buffer = [0_u8; 8192];
    let (mut lines, mut last) = (0_u32, b'\n');
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let chunk = &buffer[..read];
                if chunk.contains(&0) {
                    return 0;
                }
                lines = lines.saturating_add(chunk.iter().filter(|&&b| b == b'\n').count() as u32);
                last = chunk[read - 1];
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return 0,
        }
    }
    if last == b'\n' {
        lines
    } else {
        lines.saturating_add(1)
    }
}

/// One thing the reviewer found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    /// The file, when the line named one as `path:line`.
    #[serde(default)]
    pub file: Option<SmolStr>,
    #[serde(default)]
    pub line: Option<u32>,
    pub text: SmolStr,
}

impl ReviewFinding {
    /// `path:line — text`, or just the text when the reviewer gave no place.
    pub fn render(&self) -> String {
        match (&self.file, self.line) {
            (Some(file), Some(line)) => format!("{file}:{line} \u{2014} {}", self.text),
            (Some(file), None) => format!("{file} \u{2014} {}", self.text),
            _ => self.text.to_string(),
        }
    }
}

/// The outcome of a `/review`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReport {
    /// `None` when there was nothing to review: no model ran, and no verdict
    /// exists to be mistaken for a pass.
    #[serde(default)]
    pub verdict: Option<Verdict>,
    #[serde(default)]
    pub reason: SmolStr,
    #[serde(default)]
    pub findings: Vec<ReviewFinding>,
    /// The files the request listed.
    #[serde(default)]
    pub files: Vec<SmolStr>,
    /// The reviewer's reply, verbatim.
    #[serde(default)]
    pub raw: SmolStr,
}

impl ReviewReport {
    /// No file changed, or the workspace could not be asked.
    pub fn nothing(reason: impl Into<SmolStr>) -> Self {
        Self {
            verdict: None,
            reason: reason.into(),
            findings: Vec::new(),
            files: Vec::new(),
            raw: SmolStr::default(),
        }
    }

    /// Reads a reviewer's reply. A reply that names no verdict is
    /// [`Verdict::Partial`] with the text kept in `raw`: silence is not
    /// approval.
    pub fn parse(reply: &str, files: Vec<SmolStr>) -> Self {
        let raw: SmolStr = reply.trim().into();
        let lines: Vec<&str> = reply.lines().collect();
        // The last verdict line wins: a reviewer that read files first may
        // have said things before it answered.
        let first = lines.iter().position(|line| !line.trim().is_empty());
        let found = lines.iter().enumerate().rev().find_map(|(at, line)| {
            verdict_line(line, first == Some(at)).map(|(verdict, reason)| (at, verdict, reason))
        });
        let Some((at, verdict, reason)) = found else {
            return Self {
                verdict: Some(Verdict::Partial),
                reason: "the reviewer named no verdict".into(),
                findings: Vec::new(),
                files,
                raw,
            };
        };
        let findings = lines[at + 1..]
            .iter()
            .filter_map(|line| finding(line))
            .collect();
        Self {
            verdict: Some(verdict),
            reason: reason.into(),
            findings,
            files,
            raw,
        }
    }

    /// `review · FAIL · 3 findings`.
    pub fn banner(&self) -> String {
        match self.verdict {
            None => "review · nothing to review".to_owned(),
            Some(verdict) => {
                let n = self.findings.len();
                let noun = if n == 1 { "finding" } else { "findings" };
                format!("review · {} · {n} {noun}", verdict.as_str())
            }
        }
    }
}

/// Strips markdown emphasis and quoting from both ends of a line.
fn plain(line: &str) -> &str {
    line.trim()
        .trim_matches(|c: char| matches!(c, '*' | '_' | '`' | '#' | '>') || c.is_whitespace())
}

/// `VERDICT: FAIL — missing test` → (Fail, "missing test"). A bare `FAIL` /
/// `PASS — …` counts only as the reply's first line (`bare_ok`), because the
/// brief the reviewer also carries asks for that form; further down, a prose
/// line that happens to open with "pass" is not a verdict.
fn verdict_line(line: &str, bare_ok: bool) -> Option<(Verdict, String)> {
    let text = plain(line);
    let body = match text.get(..7) {
        Some(head) if head.eq_ignore_ascii_case("verdict") => {
            text[7..].trim_start_matches(|c: char| {
                matches!(c, ':' | '-' | '\u{2014}' | '\u{2013}' | '*' | '`') || c.is_whitespace()
            })
        }
        _ if bare_ok => text,
        _ => return None,
    };
    let end = body
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(body.len());
    let verdict = match body[..end].to_ascii_uppercase().as_str() {
        "PASS" => Verdict::Pass,
        "FAIL" => Verdict::Fail,
        "PARTIAL" => Verdict::Partial,
        _ => return None,
    };
    let reason = body[end..].trim_matches(|c: char| {
        matches!(c, ':' | '-' | '\u{2014}' | '\u{2013}' | '*' | '`') || c.is_whitespace()
    });
    Some((verdict, reason.to_owned()))
}

/// A list item (`- `, `* `, `1. `) or a line that opens with `path:line`.
fn finding(line: &str) -> Option<ReviewFinding> {
    let trimmed = line.trim();
    let (body, bulleted) = strip_bullet(trimmed);
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let first_end = body.find(char::is_whitespace).unwrap_or(body.len());
    let token = body[..first_end].trim_matches(|c: char| matches!(c, '`' | '*'));
    let token = token.trim_end_matches([':', ',', ';']);
    if let Some((file, line_no)) = location(token) {
        let text = body[first_end..].trim_start_matches(|c: char| {
            matches!(c, ':' | '-' | '\u{2014}' | '\u{2013}' | '*' | '`') || c.is_whitespace()
        });
        return Some(ReviewFinding {
            file: Some(file.into()),
            line: Some(line_no),
            text: text.into(),
        });
    }
    bulleted.then(|| ReviewFinding {
        file: None,
        line: None,
        text: body.into(),
    })
}

fn strip_bullet(line: &str) -> (&str, bool) {
    for marker in ["- ", "* ", "\u{2022} "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return (rest, true);
        }
    }
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = line[digits..]
            .strip_prefix(". ")
            .or_else(|| line[digits..].strip_prefix(") "))
    {
        return (rest, true);
    }
    (line, false)
}

/// `src/a.rs:12` or `src/a.rs:12-20` → ("src/a.rs", 12).
fn location(token: &str) -> Option<(&str, u32)> {
    let (file, tail) = token.rsplit_once(':')?;
    let first = tail.split('-').next()?;
    let line = first.parse::<u32>().ok()?;
    (!file.is_empty() && !file.contains(char::is_whitespace)).then_some((file, line))
}

/// Asks `reviewer` about `scope`. A reviewer that cannot answer — the
/// provider failed, the tool rounds ran out — is [`Verdict::Partial`], never
/// a pass: the change is unjudged, not approved.
pub async fn review_change(reviewer: &dyn Reviewer, scope: &ReviewScope) -> ReviewReport {
    let files = scope.paths();
    match reviewer.review(scope.request()).await {
        Ok(review) => ReviewReport::parse(&review.notes, files),
        Err(error) => ReviewReport {
            verdict: Some(Verdict::Partial),
            reason: format!("the reviewer could not answer: {error}").into(),
            findings: Vec::new(),
            files,
            raw: SmolStr::default(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::Review;
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn git_ok(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A repository with `a.txt` (2 lines) and `b.txt` (1 line) committed.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_ok(root, &["init", "-q"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("b.txt"), "only\n").unwrap();
        git_ok(root, &["add", "."]);
        git_ok(root, &["commit", "-q", "-m", "init"]);
        dir
    }

    fn paths(scope: &ReviewScope) -> Vec<&str> {
        scope.files.iter().map(|f| f.path.as_str()).collect()
    }

    fn edited(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn nothing_changed_is_no_scope() {
        let dir = repo();
        assert_eq!(
            select_scope(dir.path(), &[], &SensitivePolicy::default()),
            None
        );
        // An edit that was committed away leaves nothing either.
        assert_eq!(
            select_scope(dir.path(), &edited(&["a.txt"]), &SensitivePolicy::default()),
            None
        );
    }

    #[test]
    fn the_edit_set_is_reviewed_before_the_diff() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(root.join("b.txt"), "changed\n").unwrap();
        let scope = select_scope(root, &edited(&["a.txt"]), &SensitivePolicy::default()).unwrap();
        assert_eq!(scope.source, ScopeSource::EditSet);
        assert_eq!(paths(&scope), ["a.txt"], "b.txt is not in the edit set");
        assert_eq!((scope.files[0].added, scope.files[0].removed), (2, 1));
    }

    #[test]
    fn without_an_edit_set_the_git_diff_is_the_scope() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("b.txt"), "changed\nmore\n").unwrap();
        std::fs::remove_file(root.join("a.txt")).unwrap();
        let scope = select_scope(root, &[], &SensitivePolicy::default()).unwrap();
        assert_eq!(scope.source, ScopeSource::GitDiff);
        assert_eq!(paths(&scope), ["a.txt", "b.txt"]);
        assert!(scope.files[0].deleted);
        assert_eq!((scope.files[0].added, scope.files[0].removed), (0, 2));
        assert_eq!((scope.files[1].added, scope.files[1].removed), (2, 1));
        assert!(!scope.files[1].deleted);
    }

    #[test]
    fn a_created_file_counts_all_its_lines_as_added() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("new.txt"), "x\ny\nz").unwrap();
        let scope = select_scope(
            root,
            &edited(&["./new.txt", "new.txt"]),
            &SensitivePolicy::default(),
        )
        .unwrap();
        assert_eq!(paths(&scope), ["new.txt"], "two spellings, one file");
        assert_eq!((scope.files[0].added, scope.files[0].removed), (3, 0));
    }

    #[test]
    fn an_edit_that_left_the_workspace_or_is_blocked_is_not_reviewed() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join(".env"), "KEY=1\n").unwrap();
        let scope = select_scope(
            root,
            &edited(&["../outside.txt", "/etc/passwd", ".env"]),
            &SensitivePolicy::default(),
        );
        assert_eq!(scope, None);
    }

    #[test]
    fn a_directory_that_is_not_a_repository_reviews_the_edit_set_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "1\n2\n").unwrap();
        assert_eq!(select_scope(root, &[], &SensitivePolicy::default()), None);
        let scope = select_scope(root, &edited(&["a.txt"]), &SensitivePolicy::default()).unwrap();
        assert_eq!(scope.source, ScopeSource::EditSet);
        assert_eq!(scope.files[0].added, 2);
    }

    #[test]
    fn the_request_lists_at_most_thirty_files_and_counts_the_rest() {
        let dir = repo();
        let root = dir.path();
        for n in 0..34 {
            std::fs::write(root.join(format!("f{n:02}.txt")), "x\n").unwrap();
        }
        git_ok(root, &["add", "."]);
        let scope = select_scope(root, &[], &SensitivePolicy::default()).unwrap();
        assert_eq!(scope.files.len(), MAX_REVIEW_FILES);
        assert_eq!(scope.omitted, 4);
        let request = scope.request();
        assert!(request.evidence.contains("Changed files (34,"));
        assert!(request.evidence.contains("- f00.txt +1/\u{2212}0"));
        assert!(request.evidence.contains("and 4 more files"));
        assert_eq!(
            request.evidence.matches("\n- ").count(),
            MAX_REVIEW_FILES,
            "{}",
            request.evidence
        );
    }

    #[test]
    fn the_request_carries_the_marker_the_counts_and_deleted_files() {
        let scope = ReviewScope {
            source: ScopeSource::EditSet,
            files: vec![
                FileChange {
                    path: "src/a.rs".into(),
                    added: 10,
                    removed: 2,
                    deleted: false,
                },
                FileChange {
                    path: "src/old.rs".into(),
                    added: 0,
                    removed: 40,
                    deleted: true,
                },
            ],
            omitted: 0,
        };
        let request = scope.request();
        assert!(request.goal.starts_with(REVIEW_MARKER));
        assert!(request.goal.contains("grep for its path"));
        assert!(request.evidence.contains("- src/a.rs +10/\u{2212}2"));
        assert!(
            request
                .evidence
                .contains("- src/old.rs +0/\u{2212}40 (deleted)")
        );
        assert!(request.prompt().contains(crate::REVIEWER_BRIEF));
    }

    #[test]
    fn a_verdict_line_with_findings_is_read() {
        let report = ReviewReport::parse(
            "VERDICT: FAIL — missing test\n- src/a.rs:12 — the empty case panics\n\
             - src/b.rs:3-9: no caller updated\n- general: names are unclear\nprose line\n",
            vec!["src/a.rs".into()],
        );
        assert_eq!(report.verdict, Some(Verdict::Fail));
        assert_eq!(report.reason, "missing test");
        assert_eq!(report.banner(), "review · FAIL · 3 findings");
        assert_eq!(
            report.findings[0],
            ReviewFinding {
                file: Some("src/a.rs".into()),
                line: Some(12),
                text: "the empty case panics".into()
            }
        );
        assert_eq!(report.findings[1].file.as_deref(), Some("src/b.rs"));
        assert_eq!(report.findings[1].line, Some(3));
        assert_eq!(report.findings[1].text, "no caller updated");
        assert_eq!(report.findings[2].file, None);
        assert_eq!(report.findings[2].text, "general: names are unclear");
        assert_eq!(report.files, ["src/a.rs"]);
    }

    #[test]
    fn pass_and_partial_are_read_and_a_single_finding_is_singular() {
        let pass = ReviewReport::parse("**VERDICT: PASS** — looks right", Vec::new());
        assert_eq!(pass.verdict, Some(Verdict::Pass));
        assert_eq!(pass.reason, "looks right");
        assert_eq!(pass.banner(), "review · PASS · 0 findings");
        let partial = ReviewReport::parse(
            "verdict: partial - no tests run\n1. a.rs:4 — untested\n",
            Vec::new(),
        );
        assert_eq!(partial.verdict, Some(Verdict::Partial));
        assert_eq!(partial.reason, "no tests run");
        assert_eq!(partial.banner(), "review · PARTIAL · 1 finding");
    }

    #[test]
    fn the_last_verdict_line_wins_after_the_reviewer_thinks_aloud() {
        let report = ReviewReport::parse(
            "Let me read the files.\nVERDICT: PASS is my first guess\nChecked callers.\n\
             VERDICT: FAIL — a caller broke\n- main.rs:1 — still calls old()",
            Vec::new(),
        );
        assert_eq!(report.verdict, Some(Verdict::Fail));
        assert_eq!(report.findings.len(), 1);
    }

    #[test]
    fn a_bare_verdict_token_on_the_first_line_is_accepted() {
        let report = ReviewReport::parse("FAIL\n- a.rs:2 — wrong", Vec::new());
        assert_eq!(report.verdict, Some(Verdict::Fail));
        assert_eq!(report.findings.len(), 1);
    }

    #[test]
    fn a_reply_with_no_verdict_is_partial_with_the_raw_text() {
        for reply in ["", "looks fine to me", "Sure!\nall good\n"] {
            let report = ReviewReport::parse(reply, Vec::new());
            assert_eq!(report.verdict, Some(Verdict::Partial), "{reply:?}");
            assert_eq!(report.raw, reply.trim());
            assert!(report.findings.is_empty());
        }
        let report = ReviewReport::parse("looks fine to me", Vec::new());
        assert_eq!(report.reason, "the reviewer named no verdict");
    }

    struct Scripted {
        reply: Result<&'static str, &'static str>,
        seen: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Reviewer for Scripted {
        async fn review(&self, request: ReviewRequest) -> Result<Review, SmolStr> {
            self.seen.lock().unwrap().push(request.prompt().to_string());
            match self.reply {
                Ok(text) => Ok(Review {
                    verdict: Verdict::Pass,
                    notes: text.into(),
                }),
                Err(error) => Err(error.into()),
            }
        }
    }

    fn one_file() -> ReviewScope {
        capped(
            ScopeSource::EditSet,
            vec![FileChange {
                path: "a.rs".into(),
                added: 1,
                removed: 0,
                deleted: false,
            }],
        )
    }

    #[tokio::test]
    async fn review_change_reads_the_reply_not_the_reviewers_own_verdict() {
        let reviewer = Scripted {
            reply: Ok("VERDICT: FAIL — bad\n- a.rs:1 — oops"),
            seen: Mutex::default(),
        };
        let report = review_change(&reviewer, &one_file()).await;
        assert_eq!(report.verdict, Some(Verdict::Fail));
        assert_eq!(report.files, ["a.rs"]);
        let seen = reviewer.seen.lock().unwrap();
        assert!(seen[0].contains(REVIEW_MARKER));
        assert!(seen[0].contains("- a.rs +1/\u{2212}0"));
    }

    #[tokio::test]
    async fn a_reviewer_that_cannot_answer_is_partial_never_pass() {
        let reviewer = Scripted {
            reply: Err("provider down"),
            seen: Mutex::default(),
        };
        let report = review_change(&reviewer, &one_file()).await;
        assert_eq!(report.verdict, Some(Verdict::Partial));
        assert!(report.reason.contains("provider down"), "{}", report.reason);
    }

    #[test]
    fn a_report_round_trips_and_an_old_frame_still_reads() {
        let report = ReviewReport::parse("VERDICT: FAIL — x\n- a.rs:2 — y", vec!["a.rs".into()]);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""verdict":"FAIL""#), "{json}");
        assert_eq!(serde_json::from_str::<ReviewReport>(&json).unwrap(), report);
        let bare: ReviewReport = serde_json::from_str("{}").unwrap();
        assert_eq!(bare.verdict, None);
        assert_eq!(bare.banner(), "review · nothing to review");
    }
}
