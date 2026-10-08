//! Schema-agnostic config file loader: `.yml`/`.yaml`/`.json`/`.jsonc`,
//! tri-state outcome, JSON→YAML migration.

use serde::de::DeserializeOwned;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid config in {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
    #[error("io error for {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Tri-state load result, mirroring `ConfigFile.tryLoad()` in omp.
pub enum LoadOutcome<T> {
    Ok(T),
    NotFound,
    Error(ConfigError),
}

/// Load and deserialize a single config file.
///
/// - `.json` / `.jsonc`: parsed as JSON; `.jsonc` first strips comments.
/// - `.yml` / `.yaml`: if the target is missing but a sibling `.json` exists,
///   it is migrated once (YAML written, JSON renamed to `.json.bak`).
pub fn try_load<T: DeserializeOwned>(path: &Path) -> LoadOutcome<T> {
    match fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if migrate_json_to_yaml::<T>(path) {
                match fs::read_to_string(path) {
                    Ok(text) => parse_yaml(path, &text),
                    Err(e) => LoadOutcome::Error(ConfigError::Io {
                        path: path.into(),
                        source: e,
                    }),
                }
            } else {
                LoadOutcome::NotFound
            }
        }
        Err(e) => LoadOutcome::Error(ConfigError::Io {
            path: path.into(),
            source: e,
        }),
        Ok(text) => parse_yaml(path, &text),
    }
}

fn parse_yaml<T: DeserializeOwned>(path: &Path, text: &str) -> LoadOutcome<T> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext == "json" || ext == "jsonc" {
        let stripped = if ext == "jsonc" {
            strip_jsonc_comments(text)
        } else {
            text.to_owned()
        };
        match serde_json::from_str::<T>(&stripped) {
            Ok(v) => LoadOutcome::Ok(v),
            Err(e) => LoadOutcome::Error(ConfigError::Invalid {
                path: path.into(),
                reason: e.to_string(),
            }),
        }
    } else {
        match serde_yaml::from_str::<T>(text) {
            Ok(v) => LoadOutcome::Ok(v),
            Err(e) => LoadOutcome::Error(ConfigError::Invalid {
                path: path.into(),
                reason: e.to_string(),
            }),
        }
    }
}

/// One-shot JSON→YAML migration for YAML-targeted paths.
fn migrate_json_to_yaml<T: DeserializeOwned>(yaml_path: &Path) -> bool {
    let ext = yaml_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext != "yml" && ext != "yaml" {
        return false;
    }
    let json_path = yaml_path.with_extension("json");
    let Ok(text) = fs::read_to_string(&json_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&strip_jsonc_comments(&text)) else {
        return false;
    };
    let Ok(yaml) = serde_yaml::to_string(&value) else {
        return false;
    };
    if fs::write(yaml_path, yaml).is_err() {
        return false;
    }
    let _ = fs::rename(&json_path, json_path.with_extension("json.bak"));
    true
}

/// Strip `//` line comments and `/* */` block comments, keeping string literals intact.
pub(crate) fn strip_jsonc_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut closed = false;
                while let Some(n) = chars.next() {
                    if n == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    break;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Acquire an exclusive advisory lock on `<path>.lock` while holding the guard.
///
/// The closure runs only while the lock is held. On Unix/POSIX, the lock
/// blocks until acquired; if it fails (e.g. due to signal interrupt), the
/// error is returned and the closure does not run. The lock file is
/// intentionally NOT unlinked after the guard is dropped: it is a rendezvous
/// point, and unlinking it while another process is waiting on its own open
/// descriptor would create a race where a lock holder believes it holds the
/// lock while a waiter acquires "the" lock on a different inode.
pub fn with_file_lock<T, E>(path: &Path, f: impl FnOnce() -> Result<T, E>) -> Result<T, E>
where
    E: From<std::io::Error>,
{
    let lock_path = path.with_extension({
        let mut s = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_owned();
        s.push_str(".lock");
        s
    });
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| E::from(e))?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| E::from(e))?;
    let mut guard = fd_lock::RwLock::new(file);
    match guard.write() {
        Ok(_w) => f(),
        Err(e) => Err(E::from(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    fn counter_file(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("counter.yml");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    fn bump(path: &std::path::Path) -> Result<(), io::Error> {
        with_file_lock(path, || -> Result<(), io::Error> {
            let n: u32 = std::fs::read_to_string(path)?
                .trim()
                .parse()
                .map_err(|e| io::Error::other(e))?;
            // Keep the critical section slow and wobbly so interleaving would
            // be near-certain without a real lock.
            thread::sleep(Duration::from_millis(1));
            std::fs::write(path, (n + 1).to_string())
        })
    }

    /// Two racing writers read-modify-write a counter under the lock; the
    /// final value must be exactly 2N. Fails on the old helper, which ran the
    /// body whether or not the lock was held.
    #[test]
    fn concurrent_writers_do_not_interleave() {
        let (_dir, path) = counter_file("0");
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..25 {
                    bump(&path).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "50");
    }

    /// Pin the failure path: when the lock file itself cannot be opened (here
    /// because a directory occupies `<path>.lock`), the body must not run and
    /// the error must reach the caller.
    #[test]
    fn refused_lock_does_not_run_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("counter.yml");
        std::fs::write(&target, "41").unwrap();
        std::fs::create_dir(dir.path().join("counter.yml.lock")).unwrap();

        let err: io::Error = with_file_lock(&target, || -> Result<(), io::Error> {
            let n: u32 = std::fs::read_to_string(&target)?
                .trim()
                .parse()
                .map_err(|e| io::Error::other(e))?;
            std::fs::write(&target, (n + 1).to_string())?;
            Ok(())
        })
        .unwrap_err();
        // open(2) on a directory yields EISDIR: the lock was never acquired.
        assert_eq!(err.kind(), io::ErrorKind::IsADirectory);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "41");
    }

    /// A closure returning Err<T>/Err<E> propagates to the caller unchanged.
    #[test]
    fn failing_closure_propagates_error() {
        let (_dir, path) = counter_file("7");
        let result: Result<(), io::Error> = with_file_lock(&path, || Err(io::Error::other("boom")));
        assert_eq!(result.unwrap_err().to_string(), "boom");
    }

    /// Corrupt YAML still loads to `Error(ConfigError::Invalid)`, so a
    /// quarantined settings file never wedges writes behind it.
    #[test]
    fn corrupt_yaml_file_is_reported_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::fs::write(&path, "invalid: [unclosed\n").unwrap();
        let outcome: LoadOutcome<serde_json::Value> = try_load(&path);
        match outcome {
            LoadOutcome::Error(ConfigError::Invalid { .. }) => {}
            _ => panic!("expected ConfigError::Invalid for corrupt YAML"),
        }
    }
}
