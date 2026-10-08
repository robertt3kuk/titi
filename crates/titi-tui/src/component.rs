//! Component contract and render cache
//! (contract: `omp://tui`, spec: `docs/research/tui-renderer/README.md` §3).
//!
//! A component is the unit of mutable viewport content: it renders its rows
//! at a given width and consumes raw input. The render cache memoizes the
//! last rendered frame per component so an unchanged component keeps
//! returning the same rows and downstream viewport diffing sees no change.

use std::hash::{Hash, Hasher};

/// Terminal component: renders rows and consumes input.
///
/// `render` returns the component's full frame at `width`; rows may carry
/// ANSI escapes. `handle_input`
/// receives raw decoded input (key text or paste chunk).
pub trait Component {
    /// Render this component's rows for the given terminal width.
    fn render(&mut self, width: u16) -> Vec<String>;
    /// Feed raw input to the component.
    fn handle_input(&mut self, data: &str);
    /// Whether the component also wants key-release events (default: no).
    fn wants_key_release(&self) -> bool {
        false
    }
    /// Clear cached render state (theme change). Default: no-op.
    fn invalidate(&mut self) {}
    /// Release resources when the component is unmounted. Default: no-op.
    fn dispose(&mut self) {}
}

/// Stable content hash of rendered rows (length + bytes of every row).
pub fn content_hash(rows: &[String]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rows.len().hash(&mut hasher);
    for row in rows {
        row.hash(&mut hasher);
    }
    hasher.finish()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedFrame {
    width: u16,
    hash: u64,
    rows: Vec<String>,
}

/// Per-component render cache: renders through the component, hashes the
/// output, and — when width and content hash are unchanged — returns the
/// previously stored rows so callers keep painting the same lines.
///
/// Note: the component is always asked to render (the trait exposes no
/// dirty flag); the cache saves the downstream work — stable rows mean the
/// viewport diff emits nothing and no reallocation happens on the hit path.
#[derive(Debug, Default)]
pub struct RenderCache {
    entry: Option<CachedFrame>,
    last_hit: bool,
    hits: u64,
    misses: u64,
}

impl RenderCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Render `component` at `width` through the cache. On a hit the stored
    /// rows are returned (identical strings, same allocation); on a miss the
    /// fresh frame is stored and returned.
    pub fn render(&mut self, component: &mut dyn Component, width: u16) -> &[String] {
        let rows = component.render(width);
        let hash = content_hash(&rows);
        let hit = self
            .entry
            .as_ref()
            .is_some_and(|e| e.width == width && e.hash == hash);
        if !hit {
            self.entry = Some(CachedFrame { width, hash, rows });
        }
        if hit {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        self.last_hit = hit;
        self.entry.as_ref().map_or(&[], |e| e.rows.as_slice())
    }

    /// Drop the cached frame; the next [`RenderCache::render`] is a miss.
    pub fn invalidate(&mut self) {
        self.entry = None;
        self.last_hit = false;
    }

    /// Whether the last [`RenderCache::render`] call was served from cache.
    pub fn is_hit(&self) -> bool {
        self.last_hit
    }

    /// Total cache hits since construction / last [`RenderCache::invalidate`]
    /// resets only the frame, not the counters.
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Total cache misses since construction.
    pub fn misses(&self) -> u64 {
        self.misses
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock component with settable content that counts renders and inputs.
    struct Mock {
        content: Vec<String>,
        render_calls: u32,
        input: Vec<String>,
        key_release: bool,
    }

    impl Mock {
        fn new(rows: &[&str]) -> Self {
            Mock {
                content: rows.iter().map(|s| (*s).to_owned()).collect(),
                render_calls: 0,
                input: Vec::new(),
                key_release: false,
            }
        }
    }

    impl Component for Mock {
        fn render(&mut self, _width: u16) -> Vec<String> {
            self.render_calls += 1;
            self.content.clone()
        }

        fn handle_input(&mut self, data: &str) {
            self.input.push(data.to_owned());
        }

        fn wants_key_release(&self) -> bool {
            self.key_release
        }
    }

    #[test]
    fn default_wants_key_release_is_false() {
        let mut m = Mock::new(&["x"]);
        assert!(!Component::wants_key_release(&m));
        m.key_release = true;
        assert!(Component::wants_key_release(&m));
    }

    #[test]
    fn unchanged_content_returns_the_same_rows() {
        let mut cache = RenderCache::new();
        let mut comp = Mock::new(&["hello", "world"]);

        let first = cache.render(&mut comp, 40).to_vec();
        assert!(!cache.is_hit());
        assert_eq!(first, vec!["hello".to_owned(), "world".to_owned()]);

        let second = cache.render(&mut comp, 40).to_vec();
        assert!(cache.is_hit());
        assert_eq!(first, second);
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn hit_returns_the_stored_rows_not_the_fresh_ones() {
        // On a hit the returned slice must point at the previously stored
        // rows — proven by pointer identity of the backing allocation
        // (a miss replaces the entry with a fresh Vec).
        let mut cache = RenderCache::new();
        let mut comp = Mock::new(&["a"]);

        let first_ptr = cache.render(&mut comp, 10).as_ptr();
        assert!(!cache.is_hit());
        let second_ptr = cache.render(&mut comp, 10).as_ptr();
        assert!(cache.is_hit());
        assert_eq!(first_ptr, second_ptr);
    }
    #[test]
    fn changed_content_misses_and_returns_new_rows() {
        let mut cache = RenderCache::new();
        let mut comp = Mock::new(&["old"]);

        assert_eq!(cache.render(&mut comp, 40), &["old".to_owned()]);
        comp.content = vec!["new".to_owned()];
        assert_eq!(cache.render(&mut comp, 40), &["new".to_owned()]);
        assert!(!cache.is_hit());
        assert_eq!(cache.hits(), 0);
        assert_eq!(cache.misses(), 2);
    }

    #[test]
    fn resize_width_misses_and_repaints() {
        let mut cache = RenderCache::new();
        let mut comp = Mock::new(&["fixed"]);

        assert_eq!(cache.render(&mut comp, 80), &["fixed".to_owned()]);
        // Same content, new width: the cache must miss so the frame is
        // re-rendered (and re-diffed) at the resized width.
        assert_eq!(cache.render(&mut comp, 120), &["fixed".to_owned()]);
        assert!(!cache.is_hit());
        assert_eq!(comp.render_calls, 2);

        // Back to a cached width is still a miss: only the latest frame is kept.
        assert_eq!(cache.render(&mut comp, 80), &["fixed".to_owned()]);
        assert!(!cache.is_hit());
    }

    #[test]
    fn invalidate_forces_a_miss() {
        let mut cache = RenderCache::new();
        let mut comp = Mock::new(&["a"]);

        cache.render(&mut comp, 10);
        cache.render(&mut comp, 10);
        assert!(cache.is_hit());

        cache.invalidate();
        cache.render(&mut comp, 10);
        assert!(!cache.is_hit());
    }

    #[test]
    fn empty_and_multiline_content_hash_differ() {
        let a = vec!["a".to_owned(), "b".to_owned()];
        let b = vec!["ab".to_owned()];
        assert_ne!(content_hash(&a), content_hash(&b));
        assert_ne!(content_hash(&[]), content_hash(&["".to_owned()]));
        assert_eq!(content_hash(&a), content_hash(&a.clone()));
    }
}
