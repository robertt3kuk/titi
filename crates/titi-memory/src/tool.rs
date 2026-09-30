//! The `memory` tool: the agent writes what it learned, and can search it.
//!
//! Read-tier. Remembering is not a side effect the user has to approve — it
//! is the point of the tool, and a wrong memory is deleted by writing a
//! better one, not by executing anything.

use std::path::PathBuf;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};
use titi_providers::ToolSpec;
use titi_tools::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

use crate::index::{MemoryIndex, Remembered, parse_remember, render_recall};
use crate::redact;

/// Remembers and recalls. The index lives in the agent directory.
pub struct MemoryTool {
    agent_dir: PathBuf,
    index: Mutex<Option<MemoryIndex>>,
    /// Connected provider ids, so `models` offers only usable embedders.
    providers: Vec<String>,
}

impl MemoryTool {
    pub fn new(agent_dir: impl Into<PathBuf>) -> Self {
        Self::with_providers(agent_dir, Vec::new())
    }

    pub fn with_providers(agent_dir: impl Into<PathBuf>, providers: Vec<String>) -> Self {
        Self {
            agent_dir: agent_dir.into(),
            index: Mutex::new(None),
            providers,
        }
    }

    fn with_index<T>(
        &self,
        f: impl FnOnce(&MemoryIndex) -> Result<T, crate::index::Error>,
    ) -> Result<T, String> {
        let mut guard = self
            .index
            .lock()
            .map_err(|_| "memory index lock poisoned".to_owned())?;
        if guard.is_none() {
            *guard = Some(MemoryIndex::open(&self.agent_dir).map_err(|e| e.to_string())?);
        }
        f(guard.as_ref().unwrap()).map_err(|e| e.to_string())
    }
}

#[async_trait]
impl ToolHandler for MemoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "memory".into(),
                description: "Remember a fact for later sessions, or search what was remembered. \
                    Use it for decisions, preferences and gotchas worth keeping. \
                    The same fact stored twice is counted, not duplicated."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["remember", "search", "list", "models"] },
                        "summary": { "type": "string", "description": "The fact, one line." },
                        "details": { "type": "string" },
                        "category": { "type": "string", "enum": ["pref", "decision", "gotcha", "context"] },
                        "query": { "type": "string", "description": "What to search for." }
                    },
                    "required": ["action"]
                }),
            },
            approval: ApprovalTier::Read,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("");
        let result = match action {
            "remember" => self.remember(&args),
            "search" => self.search(&args),
            "list" => self.list(),
            "models" => {
                let ids: Vec<&str> = self.providers.iter().map(String::as_str).collect();
                Ok(crate::embed::suggested_lines(&ids).join("\n"))
            }
            _ => Err("action must be remember, search, list or models".into()),
        };
        match result {
            Ok(output) => ToolResult {
                output: output.into(),
                is_error: false,
                detail: None,
            },
            Err(reason) => ToolResult {
                output: reason.into(),
                is_error: true,
                detail: None,
            },
        }
    }
}

impl MemoryTool {
    fn remember(&self, args: &Value) -> Result<String, String> {
        let (category, summary, details) =
            parse_remember(args).ok_or("remember needs a summary")?;
        if summary.trim().is_empty() {
            return Err("remember needs a summary".into());
        }
        // A key stored once is shown on every later turn. Mask it first.
        let summary = redact::redact(summary);
        let details = redact::redact(details);
        let masked = summary.removed + details.removed;
        let (summary, details) = (summary.text, details.text);
        if let Some(existing) = self.with_index(|idx| idx.similar(&summary))? {
            return Ok(format!(
                "not stored: too close to #{} \"{}\" — refine that one instead",
                existing.id, existing.summary
            ));
        }
        let note = if masked > 0 {
            format!(" ({masked} secret(s) masked)")
        } else {
            String::new()
        };
        match self.with_index(|idx| idx.remember(category, &summary, &details, &[]))? {
            Remembered::Added(id) => Ok(format!("remembered #{id}{note}")),
            Remembered::Duplicate(id) => {
                Ok(format!("already remembered as #{id}; counted again{note}"))
            }
        }
    }

    fn list(&self) -> Result<String, String> {
        let all = self.with_index(|idx| idx.list())?;
        Ok(if all.is_empty() {
            "nothing remembered yet".into()
        } else {
            render_recall(&all)
        })
    }

    fn search(&self, args: &Value) -> Result<String, String> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
        let found = self.with_index(|idx| idx.recall(query, &[]))?;
        let rendered = render_recall(&found);
        Ok(if rendered.is_empty() {
            "nothing remembered matches".into()
        } else {
            rendered
        })
    }
}
