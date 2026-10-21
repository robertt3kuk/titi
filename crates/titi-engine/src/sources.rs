//! Where skills and commands are looked for, in the order that decides which
//! of two same-named files wins.
//!
//! Project first, then user. Inside a scope the native `.titi` (or the agent
//! directory) comes before the shared `.agents` and the foreign `.claude`.
//! Project directories are read from the working directory up to the
//! repository root, nearest first, so a sub-package can override the root.
//! Foreign *user* directories (`~/.agents`, `~/.claude`) are read only when
//! the user turned `skills.foreignUserDirs` on: a project's own files are
//! the repository's, but `~/.claude` is another tool's private state.
//! Nothing is ever written to a foreign directory.

use std::path::{Path, PathBuf};

/// The most directories a walk to the repository root visits.
const WALK_CAP: usize = 32;

/// Everything discovery needs to know about the machine and the session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sources {
    /// The working directory. `None` reads no project directories.
    pub cwd: Option<PathBuf>,
    /// The agent directory (`~/.titi/agent`).
    pub agent_dir: Option<PathBuf>,
    /// The home directory, where the foreign user directories live. Only
    /// read when [`Sources::foreign_user_dirs`] is set.
    pub home: Option<PathBuf>,
    /// `skills.foreignUserDirs`: also read `~/.agents` and `~/.claude`.
    pub foreign_user_dirs: bool,
}

/// What a discovered file sits under, for the panel to name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// The directory as a person would write it: `.claude/skills`,
    /// `../.agents/skills`, `~/.titi/agent/skills`.
    pub label: String,
    /// The directory itself.
    pub dir: PathBuf,
    /// Whether it is a user directory rather than the project's.
    pub user: bool,
}

/// Which family of files is being looked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Skills,
    Commands,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::Skills => "skills",
            Kind::Commands => "commands",
        }
    }
}

/// One directory to read and the directory every real file in it must stay
/// under.
#[derive(Debug, Clone)]
pub(crate) struct Root {
    pub origin: Origin,
    pub bound: PathBuf,
}

impl Sources {
    /// The directories for `kind`, highest precedence first.
    pub(crate) fn roots(&self, kind: Kind) -> Vec<Root> {
        let leaf = kind.dir();
        let mut roots = Vec::new();
        if let Some(cwd) = &self.cwd {
            for (depth, level) in levels(cwd).into_iter().enumerate() {
                let prefix = "../".repeat(depth);
                for parent in [".titi", ".agents", ".claude"] {
                    roots.push(Root {
                        origin: Origin {
                            label: format!("{prefix}{parent}/{leaf}"),
                            dir: level.join(parent).join(leaf),
                            user: false,
                        },
                        bound: level.clone(),
                    });
                }
            }
        }
        if let Some(agent_dir) = &self.agent_dir {
            let dir = agent_dir.join(leaf);
            roots.push(Root {
                origin: Origin {
                    label: tilde(&dir, self.home.as_deref()),
                    dir,
                    user: true,
                },
                bound: agent_dir.clone(),
            });
        }
        if self.foreign_user_dirs
            && let Some(home) = &self.home
        {
            let foreign: &[&str] = match kind {
                Kind::Skills => &[".agents", ".claude"],
                Kind::Commands => &[".claude"],
            };
            for parent in foreign {
                roots.push(Root {
                    origin: Origin {
                        label: format!("~/{parent}/{leaf}"),
                        dir: home.join(parent).join(leaf),
                        user: true,
                    },
                    bound: home.clone(),
                });
            }
        }
        roots
    }
}

/// The working directory and each parent up to and including the one that
/// holds `.git`. Outside a repository only the working directory counts:
/// walking to `/` would read whatever a parent directory happens to hold.
fn levels(cwd: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        found.push(current.to_path_buf());
        if current.join(".git").exists() {
            return found;
        }
        if found.len() >= WALK_CAP {
            break;
        }
        dir = current.parent();
    }
    vec![cwd.to_path_buf()]
}

/// `path` with the home directory written as `~`.
pub(crate) fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Only files whose real path is under `bound` are read.
pub(crate) fn stays_inside(path: &Path, bound: &Path) -> bool {
    match (path.canonicalize(), bound.canonicalize()) {
        (Ok(path), Ok(bound)) => path.starts_with(bound),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(sources: &Sources, kind: Kind) -> Vec<String> {
        sources
            .roots(kind)
            .into_iter()
            .map(|root| root.origin.label)
            .collect()
    }

    #[test]
    fn project_levels_walk_to_the_repository_root_nearest_first() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let deep = repo.path().join("pkg/inner");
        std::fs::create_dir_all(&deep).unwrap();
        let sources = Sources {
            cwd: Some(deep),
            ..Sources::default()
        };
        assert_eq!(
            labels(&sources, Kind::Skills),
            [
                ".titi/skills",
                ".agents/skills",
                ".claude/skills",
                "../.titi/skills",
                "../.agents/skills",
                "../.claude/skills",
                "../../.titi/skills",
                "../../.agents/skills",
                "../../.claude/skills",
            ]
        );
    }

    #[test]
    fn outside_a_repository_only_the_working_directory_counts() {
        let dir = tempfile::tempdir().unwrap();
        let sources = Sources {
            cwd: Some(dir.path().join("a")),
            ..Sources::default()
        };
        assert_eq!(labels(&sources, Kind::Commands).len(), 3);
    }

    #[test]
    fn foreign_user_directories_need_the_setting() {
        let home = tempfile::tempdir().unwrap();
        let agent = home.path().join(".titi/agent");
        let mut sources = Sources {
            agent_dir: Some(agent),
            home: Some(home.path().to_path_buf()),
            ..Sources::default()
        };
        assert_eq!(
            labels(&sources, Kind::Skills),
            ["~/.titi/agent/skills".to_owned()]
        );
        sources.foreign_user_dirs = true;
        assert_eq!(
            labels(&sources, Kind::Skills),
            [
                "~/.titi/agent/skills",
                "~/.agents/skills",
                "~/.claude/skills"
            ]
        );
        assert_eq!(
            labels(&sources, Kind::Commands),
            ["~/.titi/agent/commands", "~/.claude/commands"]
        );
    }
}
