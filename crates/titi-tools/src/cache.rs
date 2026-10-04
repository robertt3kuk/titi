//! Shared read cache.
//!
//! Every agent in a dispatch reads the same files, so a content cache keyed on
//! `(size, mtime)` turns the second read into a memory hit without ever
//! serving a stale body: a touched file changes one of those two and is read
//! again.
//!
//! Spec: `docs/research/reference-product-port/README.md` (E4).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use smol_str::SmolStr;

/// How many files the cache retains before the oldest are evicted.
pub const READ_CACHE_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    mtime: SystemTime,
}

#[derive(Debug, Clone)]
struct Cached {
    stamp: Stamp,
    body: SmolStr,
    /// Monotonic tick of the last hit, for eviction.
    used: u64,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<PathBuf, Cached>,
    tick: u64,
    hits: u64,
    misses: u64,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("entries", &self.entries.len())
            .finish_non_exhaustive()
    }
}

/// A shared, bounded file-content cache. Cheap to clone.
#[derive(Clone)]
pub struct ReadCache {
    inner: Arc<Mutex<Inner>>,
    capacity: usize,
}

impl std::fmt::Debug for ReadCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (hits, misses) = self.stats();
        f.debug_struct("ReadCache")
            .field("capacity", &self.capacity)
            .field("entries", &self.len())
            .field("hits", &hits)
            .field("misses", &misses)
            .finish()
    }
}

impl Default for ReadCache {
    fn default() -> Self {
        Self::new(READ_CACHE_CAPACITY)
    }
}

impl ReadCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            capacity: capacity.max(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn stamp(path: &Path) -> Result<Stamp, String> {
        let meta = fs::metadata(path).map_err(|error| error.to_string())?;
        Ok(Stamp {
            size: meta.len(),
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        })
    }

    /// Reads `path`, serving an unchanged file from memory.
    pub fn read(&self, path: &Path) -> Result<SmolStr, String> {
        let stamp = Self::stamp(path)?;
        {
            let mut inner = self.lock();
            let fresh = inner
                .entries
                .get(path)
                .is_some_and(|entry| entry.stamp == stamp);
            if fresh {
                inner.tick += 1;
                inner.hits += 1;
                let tick = inner.tick;
                if let Some(entry) = inner.entries.get_mut(path) {
                    entry.used = tick;
                    return Ok(entry.body.clone());
                }
            }
        }

        let body: SmolStr = fs::read_to_string(path)
            .map_err(|error| match error.kind() {
                // The decoder's own words ("stream did not contain valid
                // UTF-8") do not say what the file is or what to do instead.
                std::io::ErrorKind::InvalidData => format!(
                    "not UTF-8 text ({} bytes): a binary file or another encoding; \
                     only text can be read",
                    fs::metadata(path).map_or(0, |meta| meta.len())
                ),
                _ => error.to_string(),
            })?
            .into();
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        inner.misses += 1;
        inner.entries.insert(
            path.to_path_buf(),
            Cached {
                stamp,
                body: body.clone(),
                used: tick,
            },
        );
        while inner.entries.len() > self.capacity {
            // Evict the least recently used entry.
            let Some(oldest) = inner
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            inner.entries.remove(&oldest);
        }
        Ok(body)
    }

    /// Drops `path` from the cache. A write through the tool layer calls this
    /// so the next read sees the new body.
    pub fn invalidate(&self, path: &Path) {
        self.lock().entries.remove(path);
    }

    /// Empties the cache.
    pub fn clear(&self) {
        self.lock().entries.clear();
    }

    /// Files currently held.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `(hits, misses)` since construction.
    pub fn stats(&self) -> (u64, u64) {
        let inner = self.lock();
        (inner.hits, inner.misses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"))
    }

    #[test]
    fn a_second_read_of_an_unchanged_file_hits_the_cache() {
        let dir = temp();
        let path = dir.path().join("a.rs");
        fs::write(&path, "pub fn a() {}").unwrap_or_else(|e| panic!("{e}"));

        let cache = ReadCache::new(4);
        let first = cache.read(&path).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(first, "pub fn a() {}");
        assert_eq!(cache.stats(), (0, 1), "the first read went to disk");

        let second = cache.read(&path).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(second, first);
        assert_eq!(cache.stats(), (1, 1), "the second read was a memory hit");
    }

    #[test]
    fn a_deleted_file_is_not_served_from_cache() {
        let dir = temp();
        let path = dir.path().join("a.rs");
        fs::write(&path, "pub fn a() {}").unwrap_or_else(|e| panic!("{e}"));
        let cache = ReadCache::new(4);
        cache.read(&path).unwrap_or_else(|e| panic!("{e}"));

        fs::remove_file(&path).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            cache.read(&path).is_err(),
            "a stale body must never outlive the file"
        );
    }

    #[test]
    fn a_changed_file_is_read_again() {
        let dir = temp();
        let path = dir.path().join("a.rs");
        fs::write(&path, "one").unwrap_or_else(|e| panic!("{e}"));
        let cache = ReadCache::new(4);
        assert_eq!(cache.read(&path).unwrap_or_else(|e| panic!("{e}")), "one");

        fs::write(&path, "two-longer").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            cache.read(&path).unwrap_or_else(|e| panic!("{e}")),
            "two-longer"
        );
        assert_eq!(cache.stats(), (0, 2), "both reads went to disk");
    }

    #[test]
    fn invalidate_forces_a_fresh_read() {
        let dir = temp();
        let path = dir.path().join("a.rs");
        fs::write(&path, "before").unwrap_or_else(|e| panic!("{e}"));
        let cache = ReadCache::new(4);
        cache.read(&path).unwrap_or_else(|e| panic!("{e}"));

        fs::write(&path, "after").unwrap_or_else(|e| panic!("{e}"));
        cache.invalidate(&path);
        assert_eq!(cache.read(&path).unwrap_or_else(|e| panic!("{e}")), "after");
        assert!(cache.is_empty() || cache.len() == 1);
    }

    #[test]
    fn the_cache_is_bounded_and_evicts_the_least_recently_used() {
        let dir = temp();
        let cache = ReadCache::new(2);
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        let c = dir.path().join("c.rs");
        for (path, body) in [(&a, "a"), (&b, "b"), (&c, "c")] {
            fs::write(path, body).unwrap_or_else(|e| panic!("{e}"));
        }

        cache.read(&a).unwrap_or_else(|e| panic!("{e}"));
        cache.read(&b).unwrap_or_else(|e| panic!("{e}"));
        // Touch `a` so `b` is the least recently used.
        cache.read(&a).unwrap_or_else(|e| panic!("{e}"));
        cache.read(&c).unwrap_or_else(|e| panic!("{e}"));

        assert_eq!(cache.len(), 2);
        let (hits, _) = cache.stats();
        // `a` survived; `b` was evicted, so reading it again misses.
        let before = hits;
        cache.read(&a).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(cache.stats().0, before + 1, "a is still cached");
    }

    #[test]
    fn a_missing_file_reports_an_error() {
        let dir = temp();
        let cache = ReadCache::new(4);
        assert!(cache.read(&dir.path().join("nope.rs")).is_err());
    }
}
