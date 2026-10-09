//! What changed, and when to say so.
//!
//! The changelog is the repository's `CHANGELOG.md`, embedded at compile time
//! with `include_str!`: the binary carries the notes for its own build, so
//! `/changelog` reads no file at runtime and a moved or deleted checkout
//! changes nothing. That also settles the dev build: its version is the
//! crate's (`titi_tui::VERSION`), so the file's `## Unreleased` section is what
//! a working tree is building, and it is shown as the newest block.
//!
//! `/changelog` takes omp's shapes (`slash-commands/builtin-session.ts:467`):
//! bare, the last three releases; `full`, all of them; `last N`, N of them.
//!
//! The startup notice is omp's `startup.changelogMode` in its `summary` form
//! (`modes/settings.ts:1116`): when the version this build carries is not the
//! one the last run recorded, **one** line says what changed and points at
//! `/changelog`. The marker is a file in the agent directory
//! (`last-changelog-version`), the way the other small state beside it is kept
//! — never in the repo, never in the settings — and a first run writes it
//! silently: nothing has changed *for* a user who has never run this before.

use std::path::Path;
use std::sync::LazyLock;

/// The notes for this build, embedded.
pub(crate) const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

/// Releases the bare `/changelog` shows (omp's `RECENT_CHANGELOG_ENTRY_LIMIT`).
pub(crate) const RECENT: usize = 3;

/// One release: its heading's name and date, and its lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Release {
    /// `Unreleased`, or the version the heading names.
    pub(crate) version: String,
    /// What followed the version on the heading line (`— 2026-10-08`).
    pub(crate) date: String,
    /// The section's own lines, as written.
    pub(crate) lines: Vec<String>,
}

impl Release {
    /// Bullets the release carries: one change each, counted the way a reader
    /// counts them.
    pub(crate) fn changes(&self) -> usize {
        self.lines
            .iter()
            .filter(|line| line.trim_start().starts_with("- "))
            .count()
    }

    /// The name a listing uses.
    pub(crate) fn label(&self) -> String {
        if self.date.is_empty() {
            self.version.clone()
        } else {
            format!("{} · {}", self.version, self.date)
        }
    }
}

/// Every release in the embedded file, newest first.
pub(crate) fn releases() -> &'static [Release] {
    static RELEASES: LazyLock<Vec<Release>> = LazyLock::new(|| parse(CHANGELOG));
    &RELEASES
}

/// The releases a `/changelog` line parses to the file's own order.
fn parse(text: &str) -> Vec<Release> {
    let mut out: Vec<Release> = Vec::new();
    for line in text.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            // `Unreleased — 2026-10-08`, `0.1.0 — 2026-09-30`.
            let (version, date) = match heading.split_once('—') {
                Some((version, date)) => (version.trim(), date.trim()),
                None => (heading.trim(), ""),
            };
            out.push(Release {
                version: version.to_owned(),
                date: date.to_owned(),
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(release) = out.last_mut() {
            release.lines.push(line.to_owned());
        }
    }
    out
}

/// Which releases a `/changelog` invocation asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    /// The bare command: the newest few.
    Recent,
    /// Every release in the file.
    Full,
    /// The newest `n`.
    Last(usize),
}

/// The view a `/changelog` argument string asks for, or the usage line omp
/// prints for anything else.
pub(crate) fn parse_args(args: &str) -> Result<View, String> {
    let trimmed = args.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Ok(View::Recent);
    }
    if trimmed == "full" {
        return Ok(View::Full);
    }
    if let Some(rest) = trimmed.strip_prefix("last") {
        let rest = rest.trim();
        if rest.is_empty() {
            return Ok(View::Last(1));
        }
        return match rest.parse::<usize>() {
            Ok(n) if n > 0 => Ok(View::Last(n)),
            _ => Err(usage()),
        };
    }
    Err(usage())
}

/// What omp prints for an argument it does not know.
pub(crate) fn usage() -> String {
    "usage: /changelog [full|last [n]]".to_owned()
}

/// The lines `/changelog` pushes for a view: one heading per release and its
/// own lines under it.
pub(crate) fn render(view: View) -> Vec<String> {
    let all = releases();
    let chosen = match view {
        View::Full => all.len(),
        View::Recent => RECENT.min(all.len()),
        View::Last(n) => n.min(all.len()),
    };
    if chosen == 0 {
        return vec!["no changelog entries in this build".to_owned()];
    }
    let mut out = Vec::new();
    for release in all.iter().take(chosen) {
        out.push(format!("changelog · {}", release.label()));
        for line in &release.lines {
            let line = line.trim_end();
            if !line.trim().is_empty() {
                out.push(line.to_owned());
            }
        }
    }
    out
}

/// Where the last run's version is kept: a small marker in the agent
/// directory, beside the other state this build writes there.
pub(crate) fn marker(agent_dir: &Path) -> std::path::PathBuf {
    agent_dir.join("last-changelog-version")
}

/// The version the last run recorded, if one did.
pub(crate) fn last_seen(agent_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(marker(agent_dir)).ok()?;
    let version = text.trim().to_owned();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

/// Record this build's version as the one seen.
pub(crate) fn remember(agent_dir: &Path, version: &str) {
    if let Some(parent) = marker(agent_dir).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(marker(agent_dir), format!("{version}\n"));
}

/// The one line a startup owes, when the version moved since the last run.
///
/// `None` covers all three ways there is nothing to say: a first run (no
/// marker — nothing has changed *for* this user), the same version, and a
/// version the file does not carry (a build ahead of its own notes).
pub(crate) fn notice(last: Option<&str>, version: &str) -> Option<String> {
    let last = last?;
    if last == version {
        return None;
    }
    let all = releases();
    // The releases newer than the one the user last ran: the file is newest
    // first, so this is everything above that heading. A version the file does
    // not carry at all — a build ahead of its own notes, which is every build
    // of a tree whose changelog is still one `Unreleased` section — counts the
    // whole file instead of saying nothing: the notes are what they are.
    let unseen: Vec<&Release> = all
        .iter()
        .take_while(|release| release.version != last)
        .collect();
    let unseen = if unseen.is_empty() {
        all.iter().collect()
    } else {
        unseen
    };
    if unseen.is_empty() {
        return None;
    }
    let changes: usize = unseen.iter().map(|release| release.changes()).sum();
    let releases = unseen.len();
    Some(format!(
        "titi {version}: {changes} {} across {releases} {} since {last} · /changelog",
        if changes == 1 { "change" } else { "changes" },
        if releases == 1 { "release" } else { "releases" },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parser, on a file with releases in it: newest first, each heading
    /// named and dated, each bullet counted.
    #[test]
    fn the_parser_reads_releases_and_counts_their_changes() {
        let text = "# Changelog\n\nIntro line.\n\n## Unreleased — 2026-10-09\n\n### Added\n\n- one\n- two\n\n## 0.2.0 — 2026-10-01\n\n### Fixed\n\n- three\n";
        let all = parse(text);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].version, "Unreleased");
        assert_eq!(all[0].date, "2026-10-09");
        assert_eq!(all[0].changes(), 2);
        assert_eq!(all[1].version, "0.2.0");
        assert_eq!(all[1].changes(), 1);
        assert_eq!(all[0].label(), "Unreleased · 2026-10-09");
        // The preamble is not a release.
        assert!(all.iter().all(|release| !release.version.starts_with('#')));
    }

    /// The file this build carries parses, and what it carries is what the
    /// views render. This repo's changelog is one `Unreleased` section until
    /// the first release, so that is what the assertions hold it to — a
    /// versioned section added later parses by the same code.
    #[test]
    fn the_embedded_file_parses_into_its_releases() {
        let all = releases();
        assert!(!all.is_empty(), "the changelog is embedded");
        assert_eq!(all[0].version, "Unreleased");
        assert!(!all[0].date.is_empty(), "the section carries its date");
        assert!(all[0].changes() > 0, "and its changes");
        assert!(
            all.iter().all(|release| !release.version.is_empty()),
            "every release is named"
        );
        // The embedded text and the parsed releases are the same file.
        assert!(CHANGELOG.starts_with("# Changelog"));
        assert!(CHANGELOG.contains("## Unreleased"));
    }

    /// The three views, and the usage line for anything else.
    #[test]
    fn the_arguments_choose_the_view() {
        assert_eq!(parse_args(""), Ok(View::Recent));
        assert_eq!(parse_args("  "), Ok(View::Recent));
        assert_eq!(parse_args("full"), Ok(View::Full));
        assert_eq!(parse_args("FULL"), Ok(View::Full));
        assert_eq!(parse_args("last"), Ok(View::Last(1)));
        assert_eq!(parse_args("last 5"), Ok(View::Last(5)));
        assert!(parse_args("last 0").is_err(), "zero is not a count");
        assert!(parse_args("everything").is_err());
        assert!(
            parse_args("nope")
                .unwrap_err()
                .contains("usage: /changelog")
        );

        // Every view opens with a release heading and carries its lines; the
        // bare view is the newest few, and `full` is never shorter.
        let recent = render(View::Recent);
        let full = render(View::Full);
        assert!(!recent.is_empty());
        assert!(recent[0].starts_with("changelog · "));
        assert!(full[0].starts_with("changelog · "));
        assert!(full.len() >= recent.len());
        assert!(render(View::Last(1)).len() >= 1);
        assert!(
            recent.iter().any(|line| line.starts_with("- ")),
            "the entries themselves are rendered: {recent:#?}"
        );
    }

    /// The notice fires once per version change: a first run says nothing, the
    /// same version says nothing, and a moved one names the count and the
    /// command.
    #[test]
    fn the_notice_is_for_a_version_that_moved() {
        let version = titi_tui::VERSION;
        assert_eq!(notice(None, version), None, "a first run says nothing");
        assert_eq!(notice(Some(version), version), None, "the same version");

        let line = notice(Some("0.0.0-nonexistent"), version).expect("a moved version");
        assert!(line.contains(version), "{line}");
        assert!(line.contains("/changelog"), "{line}");
        assert!(line.contains("since 0.0.0-nonexistent"), "{line}");
        assert!(!line.contains('\n'), "one line, as omp's summary is");

        // A version the file does carry counts only what is above it.
        let text =
            "## Unreleased — 2026-10-09\n\n- one\n- two\n\n## 0.2.0 — 2026-10-01\n\n- three\n";
        let unseen = parse(text);
        let above: Vec<&Release> = unseen
            .iter()
            .take_while(|release| release.version != "0.2.0")
            .collect();
        assert_eq!(above.len(), 1);
        assert_eq!(above[0].changes(), 2, "only Unreleased's changes");
    }

    /// The marker lives in the agent directory and round-trips.
    #[test]
    fn the_marker_is_small_state_beside_the_others() {
        let dir = tempfile::tempdir().expect("temp");
        assert_eq!(last_seen(dir.path()), None, "nothing recorded yet");
        remember(dir.path(), "0.1.0");
        assert_eq!(last_seen(dir.path()), Some("0.1.0".to_owned()));
        assert_eq!(
            marker(dir.path()),
            dir.path().join("last-changelog-version")
        );
        assert!(!dir.path().join("last-changelog-version").is_dir());
    }
}
