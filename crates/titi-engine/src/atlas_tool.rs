//! The `atlas` tool: the model asking the repository index what it cannot see
//! in the prompt.
//!
//! The prompt carries a small map and a lens for the task. This is the rest: a
//! name to find, a file to outline, who uses a symbol, what a change would
//! touch, what is related to a query. One tool with an `op` keeps the schema
//! — which every request carries — small; each answer is a bounded list read
//! from the same snapshot the prompt was built from, so it is as current as
//! the map and says how far behind it may be.
//!
//! Read tier and local: the index is in memory, and the one place a call reads
//! files is `uses`, which quotes the lines that mention a name.

use async_trait::async_trait;
use serde_json::{Value, json};
use titi_atlas::{AskError, AtlasHandle, FindKind, clip_to_tokens};
use titi_providers::ToolSpec;
use titi_tools::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

/// The most an answer may cost, in estimated tokens.
const ANSWER_TOKENS: usize = 1500;

/// The model's door to the repository index.
pub struct AtlasTool {
    atlas: AtlasHandle,
}

impl AtlasTool {
    pub fn new(atlas: AtlasHandle) -> Self {
        Self { atlas }
    }
}

/// What a call asked, read out of its arguments.
struct Ask<'a> {
    op: &'a str,
    target: Option<&'a str>,
    query: Option<&'a str>,
    within: Option<&'a str>,
    kind: FindKind,
    limit: usize,
    offset: usize,
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn number(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn ask(args: &Value) -> Ask<'_> {
    Ask {
        op: text(args, "op").unwrap_or(""),
        target: text(args, "target"),
        query: text(args, "query"),
        within: text(args, "in"),
        kind: match text(args, "kind") {
            Some("symbol") => FindKind::Symbol,
            Some("file") => FindKind::File,
            _ => FindKind::Any,
        },
        limit: number(args, "limit").unwrap_or(titi_atlas::DEFAULT_LIMIT),
        offset: number(args, "offset").unwrap_or(0),
    }
}

/// Why this call cannot run, from its arguments alone.
fn missing(ask: &Ask<'_>) -> Option<String> {
    match ask.op {
        "find" | "related" if ask.query.is_none() => {
            Some(format!("atlas {} needs `query`", ask.op))
        }
        "outline" | "uses" | "impact" if ask.target.is_none() => Some(format!(
            "atlas {} needs `target`: {}",
            ask.op,
            if ask.op == "outline" {
                "a file path"
            } else {
                "a symbol name or a file path"
            }
        )),
        "find" | "related" | "outline" | "uses" | "impact" => None,
        "" => Some("atlas needs `op`: find, outline, uses, impact or related".to_owned()),
        other => Some(format!(
            "atlas has no op `{other}`; use find, outline, uses, impact or related"
        )),
    }
}

fn answer(atlas: &AtlasHandle, ask: &Ask<'_>) -> Result<String, String> {
    atlas.wait_ready();
    let pending = atlas.pending();
    let (snapshot, generation) = atlas.snapshot();
    let body = match ask.op {
        "find" => {
            let query = ask.query.unwrap_or_default();
            snapshot
                .find(query, ask.kind, ask.limit, ask.offset)
                .render(ask.offset)
        }
        "outline" => snapshot
            .outline(ask.target.unwrap_or_default())
            .map(|outline| outline.render())
            .map_err(explain)?,
        "uses" => snapshot
            .uses(ask.target.unwrap_or_default(), ask.within)
            .map(|uses| uses.render())
            .map_err(explain)?,
        "impact" => snapshot
            .impact(ask.target.unwrap_or_default())
            .map(|impact| impact.render())
            .map_err(explain)?,
        "related" => {
            let lens = snapshot.related(ask.query.unwrap_or_default(), ask.limit.min(24));
            if lens.is_empty() {
                "nothing in the index is related to that\n".to_owned()
            } else {
                lens.render_lines()
            }
        }
        _ => return Err("unknown op".to_owned()),
    };
    let header = if pending == 0 {
        format!("atlas gen={generation}\n")
    } else {
        format!(
            "atlas gen={generation} stale={pending} (the index is catching up; re-read a file you rely on)\n"
        )
    };
    Ok(clip_to_tokens(&format!("{header}{body}"), ANSWER_TOKENS))
}

fn explain(error: AskError) -> String {
    error.to_string()
}

#[async_trait]
impl ToolHandler for AtlasTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            spec: ToolSpec {
                name: "atlas".into(),
                description: "Ask the repository index; it answers from memory, so it is cheaper than \
                              grep or a file read for these. op=find: symbols and files matching \
                              `query`. op=outline: every symbol of a file `target` with line ranges \
                              and signatures, what it imports and what depends on it. op=uses: who \
                              uses the symbol `target`, with the lines. op=impact: what changing a \
                              symbol or file `target` would touch, tests included. op=related: files \
                              related to `query`. Use it before changing an exported symbol."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "op": {
                            "type": "string",
                            "enum": ["find", "outline", "uses", "impact", "related"]
                        },
                        "query": {
                            "type": "string",
                            "description": "find, related: words or identifiers to look for."
                        },
                        "target": {
                            "type": "string",
                            "description": "outline: a file path. uses, impact: a symbol name or file path."
                        },
                        "in": {
                            "type": "string",
                            "description": "uses: the file that defines the symbol, when several do."
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["any", "symbol", "file"],
                            "description": "find: what to look for. Default any."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "find, related: how many results, at most 40. Default 20."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "find: skip this many results to page."
                        }
                    },
                    "required": ["op"]
                }),
            },
            approval: ApprovalTier::Read,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        if let Some(message) = missing(&ask(&args)) {
            return ToolResult {
                output: message.into(),
                is_error: true,
                detail: None,
            };
        }
        let atlas = self.atlas.clone();
        let outcome = tokio::task::spawn_blocking(move || answer(&atlas, &ask(&args)))
            .await
            .unwrap_or_else(|_| Err("atlas lookup failed".to_owned()));
        match outcome {
            Ok(output) => ToolResult {
                output: output.into(),
                is_error: false,
                detail: None,
            },
            Err(message) => ToolResult {
                output: message.into(),
                is_error: true,
                detail: None,
            },
        }
    }

    fn describe(&self, args: &Value) -> Option<String> {
        let ask = ask(args);
        let subject = ask.query.or(ask.target)?;
        Some(format!("atlas {} {subject}", ask.op))
    }

    fn refusal(&self, args: &Value) -> Option<String> {
        missing(&ask(args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> (tempfile::TempDir, AtlasHandle) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/store.rs"),
            "pub struct SessionStore;\npub fn flush_all() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/service.rs"),
            "use crate::store::SessionStore;\npub fn run() { let _ = SessionStore; }\n",
        )
        .unwrap();
        let handle = AtlasHandle::spawn(root, titi_atlas::live::Options::default()).unwrap();
        (dir, handle)
    }

    async fn call(tool: &AtlasTool, args: Value) -> ToolResult {
        tool.invoke(args).await
    }

    #[tokio::test]
    async fn each_op_answers_from_the_index_and_names_its_generation() {
        let (_dir, atlas) = handle();
        let tool = AtlasTool::new(atlas);

        let found = call(&tool, json!({"op": "find", "query": "SessionStore"})).await;
        assert!(!found.is_error, "{}", found.output);
        assert!(found.output.starts_with("atlas gen="), "{}", found.output);
        assert!(found.output.contains("src/store.rs:1"), "{}", found.output);

        let outline = call(&tool, json!({"op": "outline", "target": "service.rs"})).await;
        assert!(
            outline.output.contains("src/service.rs") && outline.output.contains("imports"),
            "{}",
            outline.output
        );

        let uses = call(&tool, json!({"op": "uses", "target": "SessionStore"})).await;
        assert!(uses.output.contains("src/service.rs"), "{}", uses.output);
        assert!(
            uses.output.contains("1: use crate::store::SessionStore;"),
            "{}",
            uses.output
        );

        let impact = call(&tool, json!({"op": "impact", "target": "src/store.rs"})).await;
        assert!(
            impact.output.contains("src/service.rs"),
            "{}",
            impact.output
        );

        let related = call(&tool, json!({"op": "related", "query": "SessionStore"})).await;
        assert!(
            related.output.contains("src/store.rs"),
            "{}",
            related.output
        );
    }

    #[tokio::test]
    async fn a_bad_call_is_refused_before_it_runs_and_an_unknown_name_is_an_error() {
        let (_dir, atlas) = handle();
        let tool = AtlasTool::new(atlas);
        for (args, needle) in [
            (json!({}), "needs `op`"),
            (json!({"op": "find"}), "needs `query`"),
            (json!({"op": "uses"}), "needs `target`"),
            (json!({"op": "dance"}), "no op `dance`"),
        ] {
            assert!(
                tool.refusal(&args).is_some_and(|m| m.contains(needle)),
                "{args}"
            );
            let result = call(&tool, args).await;
            assert!(
                result.is_error && result.output.contains(needle),
                "{}",
                result.output
            );
        }
        let unknown = call(&tool, json!({"op": "uses", "target": "NoSuchThing"})).await;
        assert!(
            unknown.is_error && unknown.output.contains("NoSuchThing"),
            "{}",
            unknown.output
        );
        assert!(tool.refusal(&json!({"op": "find", "query": "x"})).is_none());
    }

    #[test]
    fn the_definition_is_a_read_tier_tool_with_a_small_schema() {
        let (_dir, atlas) = handle();
        let definition = AtlasTool::new(atlas).definition();
        assert_eq!(definition.spec.name, "atlas");
        assert_eq!(definition.approval, ApprovalTier::Read);
        let size = definition.spec.description.len() + definition.spec.parameters.to_string().len();
        assert!(
            size < 2200,
            "the spec is sent on every request: {size} bytes"
        );
    }
}
