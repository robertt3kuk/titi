//! Entry type and ULID-like id generation.

use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use titi_providers::ToolCallRef;

/// Author of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
    /// Output of one tool call, as the model sees it.
    Tool,
}

/// One node of the append-only session tree.
///
/// `parent_id` links to the entry this one was appended after (`None` for a
/// root); forking moves only the leaf pointer, so history is never rewritten.
/// `ts` is milliseconds since the Unix epoch. `tool_calls` is what an
/// assistant message asked for; a session written before tool traffic was
/// persisted has no such field, so it deserializes as empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub parent_id: Option<String>,
    pub role: Role,
    pub content: String,
    pub ts: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRef>,
}

static COUNTER: AtomicU16 = AtomicU16::new(0);

impl Entry {
    /// Creates a new entry with a fresh ULID-like id and the current timestamp.
    pub fn new(parent_id: Option<String>, role: Role, content: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            parent_id,
            role,
            content: content.into(),
            ts: now_ms(),
            tool_calls: Vec::new(),
        }
    }

    /// The same entry with the tool calls the assistant issued in it.
    pub fn with_tool_calls(mut self, tool_calls: Vec<ToolCallRef>) -> Self {
        self.tool_calls = tool_calls;
        self
    }
}

/// Current time in milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Generates a ULID-like, lexicographically sortable 32-hex-char id:
/// 48-bit millisecond timestamp, then subsecond nanos, a process-local
/// counter, and the pid for cross-process entropy.
pub(crate) fn new_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let ms = now.as_millis() as u64 & 0xFFFF_FFFF_FFFF;
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{ms:012x}{:08x}{counter:04x}{:08x}",
        now.subsec_nanos(),
        std::process::id()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_sorted_by_time() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a < b);
    }
}
