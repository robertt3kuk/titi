//! Working-tree diff captured at the start of a turn.
//!
//! A completion engine treats the edit as its strongest signal. The genome
//! rank boost only reorders files; it does not show what changed. This is
//! that text, bounded and stripped of secrets. It does not typecheck, and it
//! does not complete at the cursor.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Files changed relative to `HEAD`, and the bounded diff text.
///
/// `files` is what the genome boost should see. It is capped with the text so
/// a huge tree cannot flood the touched set. A later line or byte cut may
/// omit a path's hunk; the path is still an edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSnapshot {
    pub files: Vec<String>,
    pub text: String,
}

const MAX_FILES: usize = 8;
const MAX_LINES: usize = 120;
const MAX_BYTES: usize = 6000;
const DIFF_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);
/// Kept stdout. The rest is still drained so a huge diff cannot fill the
/// pipe and turn into a timeout.
const READ_CAP: usize = 256 * 1024;
const STDERR_CAP: usize = 64 * 1024;
const TRUNCATED_LINE: &str = "… diff truncated";

/// Captures `git diff HEAD` for `root`.
///
/// `None` when `root` is not a git repository, git cannot be run, the command
/// fails, or it does not finish within two seconds. Never panics. An empty
/// tree is `None`, so a turn with nothing to show keeps today's prompt bytes.
///
/// Staged and unstaged changes are both included: `git diff` with no spec
/// would drop the index.
pub fn capture(root: &Path) -> Option<DiffSnapshot> {
    let (raw, read_truncated) = git_diff_head(root)?;
    assemble(&raw, read_truncated)
}

fn git_diff_head(root: &Path) -> Option<(String, bool)> {
    let mut child = Command::new("git")
        .arg("--no-pager")
        .args(["-c", "color.ui=never"])
        .args(["-c", "diff.mnemonicPrefix=false"])
        .args(["-c", "diff.noprefix=false"])
        .args(["-c", "core.quotePath=true"])
        .args(["diff", "--no-ext-diff", "HEAD"])
        .current_dir(root)
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let out_reader = std::thread::spawn(move || drain_capped(stdout, READ_CAP));
    let err_reader = std::thread::spawn(move || drain_capped(stderr, STDERR_CAP));

    let deadline = Instant::now() + DIFF_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };

    let stdout = out_reader.join().ok();
    let _stderr = err_reader.join();
    let status = status?;
    if !status.success() {
        return None;
    }
    stdout
}

/// Reads a pipe to the end, keeping at most `cap` bytes. The tail is still
/// read and dropped so the child never blocks on a full pipe.
fn drain_capped(mut source: impl Read, cap: usize) -> (String, bool) {
    let mut buffer = [0_u8; 8192];
    let mut kept = Vec::new();
    let mut truncated = false;
    loop {
        match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                let room = cap.saturating_sub(kept.len());
                if read > room {
                    truncated = true;
                    kept.extend_from_slice(&buffer[..room]);
                } else {
                    kept.extend_from_slice(&buffer[..read]);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }
    (String::from_utf8_lossy(&kept).into_owned(), truncated)
}

fn assemble(raw: &str, read_truncated: bool) -> Option<DiffSnapshot> {
    let sections = file_sections(raw);
    // A capped read ends mid-file. That tail is not a complete hunk, so it
    // cannot be secret-scanned; drop it rather than show a prefix of a file
    // whose key sits past the cap.
    let usable: &[String] = if read_truncated {
        sections.split_last().map(|(_, head)| head).unwrap_or(&[])
    } else {
        sections.as_slice()
    };
    let mut kept: Vec<(String, &str)> = Vec::new();
    for section in usable {
        let Some(path) = destination_path(section) else {
            continue;
        };
        if is_secret_path(&path) || hunk_has_secret(section) {
            continue;
        }
        kept.push((path, section.as_str()));
    }
    if kept.is_empty() {
        return None;
    }
    let file_truncated = kept.len() > MAX_FILES;
    kept.truncate(MAX_FILES);
    let files: Vec<String> = kept.iter().map(|(path, _)| path.clone()).collect();
    let mut body = String::new();
    for (_, section) in &kept {
        body.push_str(section);
    }
    let text = bound_text(&body, file_truncated || read_truncated);
    if text.trim().is_empty() {
        return None;
    }
    Some(DiffSnapshot { files, text })
}

fn file_sections(diff: &str) -> Vec<String> {
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in diff.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    let mut sections = Vec::with_capacity(starts.len());
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(diff.len());
        sections.push(diff[*start..end].to_owned());
    }
    sections
}

fn destination_path(section: &str) -> Option<String> {
    let header = section.split('\n').next().unwrap_or("");
    let rest = header.strip_prefix("diff --git ")?.trim_end();
    let (left, right) = split_pair(rest)?;
    let path = strip_side(&right, 'b').or_else(|| strip_side(&left, 'a'))?;
    if path.is_empty() || path == "/dev/null" {
        strip_side(&left, 'a')
    } else {
        Some(path)
    }
}

fn split_pair(rest: &str) -> Option<(String, String)> {
    let (left, after) = take_token(rest)?;
    let (right, _) = take_token(after)?;
    Some((left, right))
}

fn take_token(input: &str) -> Option<(String, &str)> {
    let input = input.trim_start();
    if input.starts_with('"') {
        take_quoted(input)
    } else {
        take_unquoted(input)
    }
}

fn take_unquoted(input: &str) -> Option<(String, &str)> {
    if input.is_empty() {
        return None;
    }
    let end = input.find(' ').unwrap_or(input.len());
    if end == 0 {
        return None;
    }
    Some((input[..end].to_owned(), &input[end..]))
}

fn take_quoted(input: &str) -> Option<(String, &str)> {
    let body = input.strip_prefix('"')?;
    let bytes = body.as_bytes();
    let mut index = 0;
    let mut out = Vec::new();
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                let path = String::from_utf8(out).ok()?;
                return Some((path, &body[index + 1..]));
            }
            b'\\' => {
                index += 1;
                if index >= bytes.len() {
                    return None;
                }
                match bytes[index] {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'\\' => out.push(b'\\'),
                    b'"' => out.push(b'"'),
                    digit if (b'0'..=b'7').contains(&digit) => {
                        let mut value = u16::from(digit - b'0');
                        let mut count = 1;
                        while count < 3
                            && index + 1 < bytes.len()
                            && (b'0'..=b'7').contains(&bytes[index + 1])
                        {
                            index += 1;
                            value = value * 8 + u16::from(bytes[index] - b'0');
                            count += 1;
                        }
                        if value > 255 {
                            return None;
                        }
                        out.push(value as u8);
                    }
                    _ => return None,
                }
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    None
}

fn strip_side(token: &str, side: char) -> Option<String> {
    let prefix = format!("{side}/");
    token.strip_prefix(&prefix).map(str::to_owned)
}

fn is_secret_path(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    lower == ".env"
        || lower.starts_with(".env.")
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
}

/// `(?i)(api[_-]?key|secret|token|password)\s*[:=]` — the whole file goes,
/// not just the line. No `regex` crate in this package; the scan is the
/// pattern.
fn hunk_has_secret(section: &str) -> bool {
    section.lines().any(line_has_secret)
}

fn line_has_secret(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    for needle in [
        "api_key", "api-key", "apikey", "secret", "token", "password",
    ] {
        let mut from = 0;
        while let Some(at) = lower[from..].find(needle) {
            let after = from + at + needle.len();
            let rest = lower[after..].trim_start();
            if rest.starts_with([':', '=']) {
                return true;
            }
            from += at + 1;
            if from >= lower.len() {
                break;
            }
        }
    }
    false
}

fn bound_text(body: &str, force_truncated: bool) -> String {
    let mut lines: Vec<&str> = body.split('\n').collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    if !force_truncated && lines.len() <= MAX_LINES && body.len() <= MAX_BYTES {
        return body.to_owned();
    }

    let marker_bytes = TRUNCATED_LINE.len() + 1;
    let mut kept: Vec<&str> = Vec::new();
    let mut used = marker_bytes;
    for line in &lines {
        if kept.len() + 1 >= MAX_LINES {
            break;
        }
        let cost = line.len() + 1;
        if used + cost > MAX_BYTES {
            break;
        }
        kept.push(*line);
        used += cost;
    }

    let mut out = String::new();
    if kept.is_empty() && lines.first().is_some_and(|line| !line.is_empty()) {
        let budget = MAX_BYTES.saturating_sub(marker_bytes + 1);
        let prefix = cut_chars(lines[0], budget);
        if !prefix.is_empty() {
            out.push_str(prefix);
            out.push('\n');
        }
    } else {
        for line in kept {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(TRUNCATED_LINE);
    out.push('\n');
    out
}

fn cut_chars(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use super::*;

    fn line_count(text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        let newlines = text.bytes().filter(|byte| *byte == b'\n').count();
        if text.ends_with('\n') {
            newlines
        } else {
            newlines + 1
        }
    }

    #[test]
    fn a_directory_that_is_not_a_repository_yields_nothing() {
        let dir = tempfile::tempdir().expect("temp");
        assert!(capture(dir.path()).is_none());
    }

    #[test]
    fn an_unstaged_edit_is_captured_and_secret_files_are_not() {
        let dir = tempfile::tempdir().expect("temp");
        let root = dir.path();
        git(root, &["init", "-q", "-b", "master"]);
        fs::write(root.join("parser.rs"), "fn parse() {}\n").expect("write");
        git(root, &["add", "parser.rs"]);
        git(root, &["commit", "-qm", "first"]);

        fs::write(
            root.join("parser.rs"),
            "fn parse() {}\nfn added_for_the_turn() {}\n",
        )
        .expect("write");
        fs::write(root.join(".env"), "API_KEY=sk-test\n").expect("write");
        fs::write(root.join("notes.txt"), "password=sk-test\n").expect("write");
        git(root, &["add", "-f", ".env", "notes.txt"]);

        let raw = git_stdout(root, &["-c", "color.ui=never", "diff", "HEAD"]);
        assert!(
            raw.contains(".env"),
            "the fixture must show .env in git diff HEAD, got {raw}"
        );
        assert!(
            raw.contains("sk-test"),
            "the fixture must contain the token-shaped line, got {raw}"
        );
        assert!(
            raw.contains("+fn added_for_the_turn() {}"),
            "the unstaged edit must be in git diff HEAD, got {raw}"
        );

        let shot = capture(root).expect("a snapshot");
        assert!(
            shot.files.iter().any(|path| path == "parser.rs"),
            "{:?}",
            shot.files
        );
        assert!(
            shot.text.contains("+fn added_for_the_turn() {}"),
            "{}",
            shot.text
        );
        assert!(
            shot.files.iter().all(|path| !is_secret_path(path)),
            "{:?}",
            shot.files
        );
        assert!(
            !shot.files.iter().any(|path| path == "notes.txt"),
            "{:?}",
            shot.files
        );
        assert!(!shot.text.contains(".env"), "{}", shot.text);
        assert!(!shot.text.contains("notes.txt"), "{}", shot.text);
        assert!(!shot.text.contains("sk-test"), "{}", shot.text);
    }

    #[test]
    fn a_secret_name_or_assignment_drops_the_whole_file() {
        let keeper = section("parser.rs", "+fn added_for_the_turn() {}");
        let raw = [
            keeper,
            section("app/.env", "+API_KEY=sk-test"),
            section(".env.local", "+TOKEN=sk-test"),
            section("keys/id.key", "+x"),
            section("cert.pem", "+x"),
            section("notes.txt", "+password=sk-test"),
            quoted_section("secrets/.env", "+API_KEY=sk-test"),
        ]
        .join("");
        let shot = assemble(&raw, false).expect("keeper survives");
        assert_eq!(shot.files, vec!["parser.rs".to_owned()]);
        assert!(shot.text.contains("+fn added_for_the_turn() {}"));
        assert!(!shot.text.contains("sk-test"), "{}", shot.text);
        assert!(!shot.text.contains(".env"), "{}", shot.text);
        assert!(!shot.text.contains(".key"), "{}", shot.text);
        assert!(!shot.text.contains(".pem"), "{}", shot.text);
    }

    #[test]
    fn a_quoted_path_with_a_space_is_kept() {
        let raw = quoted_section("my file.rs", "+fn added_for_the_turn() {}");
        let shot = assemble(&raw, false).expect("quoted path");
        assert_eq!(shot.files, vec!["my file.rs".to_owned()]);
        assert!(shot.text.contains("+fn added_for_the_turn() {}"));
    }

    #[test]
    fn a_bare_word_is_not_a_secret_and_an_assignment_is() {
        assert!(!line_has_secret("// password handling stays"));
        assert!(!line_has_secret("let token_count = 1"));
        assert!(!line_has_secret("api key = sk-test"));
        assert!(line_has_secret("+API_KEY=sk-test"));
        assert!(line_has_secret("api-key: sk-test"));
        assert!(line_has_secret("apikey=sk-test"));
        assert!(line_has_secret("token = sk-test"));
        assert!(line_has_secret("SECRET: sk-test"));
    }

    #[test]
    fn more_than_eight_files_is_cut_and_marked() {
        let mut raw = String::new();
        for index in 0..9 {
            raw.push_str(&section(&format!("f{index}.rs"), &format!("+line {index}")));
        }
        let shot = assemble(&raw, false).expect("bounded snapshot");
        assert_eq!(shot.files.len(), 8);
        assert!(!shot.files.iter().any(|path| path == "f8.rs"));
        assert!(shot.text.contains(TRUNCATED_LINE), "{}", shot.text);
        assert!(
            line_count(&shot.text) <= MAX_LINES,
            "{}",
            line_count(&shot.text)
        );
        assert!(shot.text.len() <= MAX_BYTES, "{}", shot.text.len());
    }

    #[test]
    fn a_long_hunk_stays_inside_the_line_and_byte_caps() {
        let mut lines = String::from("diff --git a/wide.rs b/wide.rs\n");
        for _ in 0..10 {
            lines.push('+');
            lines.push_str(&"x".repeat(999));
            lines.push('\n');
        }
        let shot = assemble(&lines, false).expect("wide snapshot");
        assert_eq!(shot.files, vec!["wide.rs".to_owned()]);
        assert!(shot.text.contains(TRUNCATED_LINE), "{}", shot.text);
        assert!(line_count(&shot.text) <= MAX_LINES);
        assert!(shot.text.len() <= MAX_BYTES, "{}", shot.text.len());
        assert!(shot.text.ends_with(&format!("{TRUNCATED_LINE}\n")));
    }

    fn section(path: &str, line: &str) -> String {
        format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -0,0 +1 @@\n{line}\n")
    }

    fn quoted_section(path: &str, line: &str) -> String {
        format!(
            "diff --git \"a/{path}\" \"b/{path}\"\n--- \"a/{path}\"\n+++ \"b/{path}\"\n@@ -0,0 +1 @@\n{line}\n"
        )
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = git_output(dir, args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let output = git_output(dir, args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn git_output(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=titi@example.invalid"])
            .args(["-c", "user.name=titi test"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("git runs")
    }
}
