//! The `todo` tool: a checklist the agent keeps while it works through a
//! multi-step task.
//!
//! Read-tier: the list lives in this process and changes nothing outside the
//! session, so plan mode keeps it. It is not saved, so a restarted session
//! starts without one.

use std::fmt::Write as _;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};
use thiserror::Error;
use titi_providers::ToolSpec;

use crate::{ApprovalTier, ToolDefinition, ToolHandler, ToolResult};

/// Most items one list holds.
pub const MAX_TODO_ITEMS: usize = 50;
/// Most characters one item holds.
pub const MAX_TODO_CHARS: usize = 200;
/// Characters of the current item the tool chip shows.
const CHIP_CHARS: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[>]",
            Self::Completed => "[x]",
            Self::Cancelled => "[-]",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TodoItem {
    content: String,
    status: TodoStatus,
}

/// Why a call was refused. The text is what the model reads, so each one
/// names the item and what would be accepted.
#[derive(Debug, Error, PartialEq, Eq)]
enum TodoError {
    #[error("op must be write, update or view")]
    BadOp,
    #[error(
        "write needs items: the whole list, each {{\"content\": \"...\", \"status\": \"pending\"}}"
    )]
    MissingItems,
    #[error("a todo list holds at most {max} items, not {0}", max = MAX_TODO_ITEMS)]
    TooManyItems(usize),
    #[error("item {0} has no content")]
    EmptyContent(usize),
    #[error("item {id} is {chars} characters; keep each item to {max}", max = MAX_TODO_CHARS)]
    TooLong { id: usize, chars: usize },
    #[error("item {0}: status must be pending, in_progress, completed or cancelled")]
    BadItemStatus(usize),
    #[error("items {first} and {second} are both in_progress; keep one item in progress at a time")]
    TwoInProgress { first: usize, second: usize },
    #[error("update needs id, the item's number in the list")]
    MissingId,
    #[error("update needs status: pending, in_progress, completed or cancelled")]
    BadStatus,
    #[error("no item {id}; the list is numbered 1 to {len}")]
    NoSuchItem { id: u64, len: usize },
    #[error("the todo list is empty; write it first")]
    EmptyList,
    #[error("the todo list is unavailable: its lock was poisoned")]
    Poisoned,
}

enum Op {
    Write(Vec<TodoItem>),
    Update { id: u64, status: TodoStatus },
    View,
}

/// What a call leaves behind.
struct Applied {
    items: Vec<TodoItem>,
    /// The item a start took the in-progress mark from, by number.
    paused: Option<usize>,
}

/// One checklist per engine run. The registry hands every turn a clone of
/// the same `Arc`, so the list carries from turn to turn and across modes.
#[derive(Default)]
pub struct TodoTool {
    items: Mutex<Vec<TodoItem>>,
}

impl TodoTool {
    pub fn new() -> Self {
        Self::default()
    }

    /// The model's answer, and the checklist alone for a surface to draw.
    fn run(&self, args: &Value) -> Result<(String, String), TodoError> {
        let op = parse(args)?;
        let mut items = self.items.lock().map_err(|_| TodoError::Poisoned)?;
        let applied = apply(&items, op)?;
        *items = applied.items;
        let rows = checklist(&items);
        let mut output = summary(&items);
        if let Some(id) = applied.paused {
            let _ = write!(output, "\nitem {id} is back to pending");
        }
        if !rows.is_empty() {
            output.push('\n');
            output.push_str(&rows);
        }
        Ok((output, rows))
    }
}

#[async_trait]
impl ToolHandler for TodoTool {
    fn definition(&self) -> ToolDefinition {
        let status = json!(["pending", "in_progress", "completed", "cancelled"]);
        ToolDefinition {
            spec: ToolSpec {
                name: "todo".into(),
                description: "A checklist for multi-step work in this session. Use it when a \
                    task takes three or more steps or the user asks for several things: write \
                    the whole list before you start, keep exactly one item in_progress, and \
                    mark each item completed as soon as it is done, not all at the end. Every \
                    answer shows the list with each item's number. update sets one item's \
                    status by that number; write replaces the whole list, which is how to add, \
                    drop or reorder items; view shows it again. Mark an item cancelled when it \
                    no longer applies. Skip the list for a single quick step."
                    .into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "op": {
                            "type": "string",
                            "enum": ["write", "update", "view"],
                            "description": "write replaces the list, update sets one item's status, view shows the list."
                        },
                        "items": {
                            "type": "array",
                            "description": "For write: the whole list in order, at most 50 items.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "content": { "type": "string", "description": "One short, specific step." },
                                    "status": { "type": "string", "enum": status, "description": "Defaults to pending." }
                                },
                                "required": ["content"]
                            }
                        },
                        "id": { "type": "integer", "minimum": 1, "description": "For update: the item's number." },
                        "status": { "type": "string", "enum": status, "description": "For update: the new status." }
                    },
                    "required": ["op"]
                }),
            },
            approval: ApprovalTier::Read,
        }
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        match self.run(&args) {
            Ok((output, rows)) => ToolResult {
                output: output.into(),
                is_error: false,
                detail: (!rows.is_empty()).then(|| rows.into()),
            },
            Err(error) => ToolResult {
                output: error.to_string().into(),
                is_error: true,
                detail: None,
            },
        }
    }

    /// `todo 3/7 · Write the tests`: where the list will stand once this call
    /// lands, worked out on a copy so describing changes nothing.
    fn describe(&self, args: &Value) -> Option<String> {
        let items = self.items.lock().ok()?;
        let applied = parse(args).and_then(|op| apply(&items, op)).ok()?;
        Some(chip(&applied.items))
    }
}

fn parse(args: &Value) -> Result<Op, TodoError> {
    match args.get("op").and_then(Value::as_str) {
        Some("write") => parse_items(args.get("items")).map(Op::Write),
        Some("update") => {
            let id = match args.get("id") {
                Some(Value::Number(id)) => id.as_u64(),
                Some(Value::String(id)) => id.trim().parse().ok(),
                _ => None,
            }
            .ok_or(TodoError::MissingId)?;
            let status = args
                .get("status")
                .and_then(Value::as_str)
                .and_then(TodoStatus::parse)
                .ok_or(TodoError::BadStatus)?;
            Ok(Op::Update { id, status })
        }
        Some("view") => Ok(Op::View),
        _ => Err(TodoError::BadOp),
    }
}

fn parse_items(items: Option<&Value>) -> Result<Vec<TodoItem>, TodoError> {
    let Some(Value::Array(items)) = items else {
        return Err(TodoError::MissingItems);
    };
    if items.len() > MAX_TODO_ITEMS {
        return Err(TodoError::TooManyItems(items.len()));
    }
    let mut parsed = Vec::with_capacity(items.len());
    let mut in_progress = None;
    for (index, item) in items.iter().enumerate() {
        let id = index + 1;
        // A bare string is the shape models reach for first, and it can only
        // mean a pending item.
        let (content, status) = match item {
            Value::String(content) => (content.as_str(), Some(TodoStatus::Pending)),
            Value::Object(fields) => (
                fields.get("content").and_then(Value::as_str).unwrap_or(""),
                match fields.get("status") {
                    None | Some(Value::Null) => Some(TodoStatus::Pending),
                    Some(status) => status.as_str().and_then(TodoStatus::parse),
                },
            ),
            _ => ("", Some(TodoStatus::Pending)),
        };
        // Each item is one row of the checklist; a newline would split it.
        let content = content.split_whitespace().collect::<Vec<_>>().join(" ");
        if content.is_empty() {
            return Err(TodoError::EmptyContent(id));
        }
        let chars = content.chars().count();
        if chars > MAX_TODO_CHARS {
            return Err(TodoError::TooLong { id, chars });
        }
        let status = status.ok_or(TodoError::BadItemStatus(id))?;
        if status == TodoStatus::InProgress {
            if let Some(first) = in_progress {
                return Err(TodoError::TwoInProgress { first, second: id });
            }
            in_progress = Some(id);
        }
        parsed.push(TodoItem { content, status });
    }
    Ok(parsed)
}

fn apply(current: &[TodoItem], op: Op) -> Result<Applied, TodoError> {
    let items = match op {
        Op::Write(items) => items,
        Op::View => current.to_vec(),
        Op::Update { id, status } => return update(current, id, status),
    };
    Ok(Applied {
        items,
        paused: None,
    })
}

fn update(current: &[TodoItem], id: u64, status: TodoStatus) -> Result<Applied, TodoError> {
    if current.is_empty() {
        return Err(TodoError::EmptyList);
    }
    let missing = || TodoError::NoSuchItem {
        id,
        len: current.len(),
    };
    let index = usize::try_from(id)
        .ok()
        .and_then(|id| id.checked_sub(1))
        .filter(|index| *index < current.len())
        .ok_or_else(missing)?;
    let mut items = current.to_vec();
    let mut paused = None;
    // Starting an item moves the mark rather than refusing: the one that had
    // it goes back to pending, not to done, since nothing says it is done.
    if status == TodoStatus::InProgress {
        for (other, item) in items.iter_mut().enumerate() {
            if other != index && item.status == TodoStatus::InProgress {
                item.status = TodoStatus::Pending;
                paused = Some(other + 1);
            }
        }
    }
    let item = items.get_mut(index).ok_or_else(missing)?;
    item.status = status;
    Ok(Applied { items, paused })
}

/// Completed items, the items still counted, and the cancelled ones. A
/// cancelled item is no longer work, so it leaves the count and is named
/// apart instead of passing for done.
fn tally(items: &[TodoItem]) -> (usize, usize, usize) {
    let done = items
        .iter()
        .filter(|item| item.status == TodoStatus::Completed)
        .count();
    let cancelled = items
        .iter()
        .filter(|item| item.status == TodoStatus::Cancelled)
        .count();
    (done, items.len() - cancelled, cancelled)
}

fn summary(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "todo list is empty".into();
    }
    let (done, total, cancelled) = tally(items);
    let mut summary = format!("{done}/{total} done");
    if cancelled > 0 {
        let _ = write!(summary, ", {cancelled} cancelled");
    }
    summary
}

fn checklist(items: &[TodoItem]) -> String {
    let mut rows = String::new();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            rows.push('\n');
        }
        let _ = write!(
            rows,
            "{} {}. {}",
            item.status.marker(),
            index + 1,
            item.content
        );
    }
    rows
}

fn chip(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "todo empty".into();
    }
    let (done, total, _) = tally(items);
    let mut chip = format!("todo {done}/{total}");
    if let Some(current) = items
        .iter()
        .find(|item| item.status == TodoStatus::InProgress)
    {
        chip.push_str(" · ");
        chip.extend(current.content.chars().take(CHIP_CHARS));
        if current.content.chars().count() > CHIP_CHARS {
            chip.push('…');
        }
    }
    chip
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolRegistry;
    use serde_json::json;
    use std::sync::Arc;

    async fn call(tool: &TodoTool, args: Value) -> ToolResult {
        tool.invoke(args).await
    }

    async fn ok(tool: &TodoTool, args: Value) -> String {
        let result = call(tool, args).await;
        assert!(!result.is_error, "{}", result.output);
        result.output.to_string()
    }

    async fn view(tool: &TodoTool) -> String {
        ok(tool, json!({ "op": "view" })).await
    }

    fn write(items: &[(&str, &str)]) -> Value {
        let items: Vec<Value> = items
            .iter()
            .map(|(content, status)| json!({ "content": content, "status": status }))
            .collect();
        json!({ "op": "write", "items": items })
    }

    /// The answer is a one-line count and then one numbered row per item, so
    /// the model can address an item by the number it was just shown.
    #[tokio::test]
    async fn write_numbers_the_items_and_counts_what_is_done() {
        let tool = TodoTool::new();
        let output = ok(
            &tool,
            write(&[
                ("Read the spec", "completed"),
                ("Write the tests", "in_progress"),
                ("Implement it", "pending"),
                ("Port the HUD", "cancelled"),
            ]),
        )
        .await;
        assert_eq!(
            output,
            "1/3 done, 1 cancelled\n\
             [x] 1. Read the spec\n\
             [>] 2. Write the tests\n\
             [ ] 3. Implement it\n\
             [-] 4. Port the HUD"
        );
    }

    /// `write` states the whole list: what was there before is gone, and an
    /// item without a status starts pending.
    #[tokio::test]
    async fn write_replaces_the_whole_list() {
        let tool = TodoTool::new();
        ok(&tool, write(&[("one", "completed"), ("two", "pending")])).await;
        let output = ok(
            &tool,
            json!({ "op": "write", "items": [{ "content": "three" }, "four"] }),
        )
        .await;
        assert_eq!(output, "0/2 done\n[ ] 1. three\n[ ] 2. four");
        assert_eq!(view(&tool).await, output);
    }

    #[tokio::test]
    async fn an_empty_write_clears_the_list() {
        let tool = TodoTool::new();
        ok(&tool, write(&[("one", "pending")])).await;
        let output = ok(&tool, json!({ "op": "write", "items": [] })).await;
        assert_eq!(output, "todo list is empty");
        assert_eq!(view(&tool).await, "todo list is empty");
    }

    #[tokio::test]
    async fn update_changes_one_item_by_its_number() {
        let tool = TodoTool::new();
        ok(&tool, write(&[("one", "in_progress"), ("two", "pending")])).await;
        let output = ok(
            &tool,
            json!({ "op": "update", "id": 1, "status": "completed" }),
        )
        .await;
        assert_eq!(output, "1/2 done\n[x] 1. one\n[ ] 2. two");
        let output = ok(
            &tool,
            json!({ "op": "update", "id": "2", "status": "cancelled" }),
        )
        .await;
        assert_eq!(output, "1/1 done, 1 cancelled\n[x] 1. one\n[-] 2. two");
    }

    /// A full list that claims two items in progress contradicts itself and
    /// is refused whole.
    #[tokio::test]
    async fn write_refuses_two_items_in_progress() {
        let tool = TodoTool::new();
        ok(&tool, write(&[("keep", "pending")])).await;
        let result = call(
            &tool,
            write(&[("a", "in_progress"), ("b", "pending"), ("c", "in_progress")]),
        )
        .await;
        assert!(result.is_error);
        assert!(
            result.output.contains("1 and 3"),
            "names the two items: {}",
            result.output
        );
        assert_eq!(view(&tool).await, "0/1 done\n[ ] 1. keep");
    }

    /// Starting one item is a switch of focus: the item that was in progress
    /// goes back to pending rather than being passed off as done, and the
    /// answer says so.
    #[tokio::test]
    async fn starting_an_item_moves_the_in_progress_mark() {
        let tool = TodoTool::new();
        ok(&tool, write(&[("a", "in_progress"), ("b", "pending")])).await;
        let output = ok(
            &tool,
            json!({ "op": "update", "id": 2, "status": "in_progress" }),
        )
        .await;
        assert_eq!(
            output,
            "0/2 done\nitem 1 is back to pending\n[ ] 1. a\n[>] 2. b"
        );
        let output = ok(
            &tool,
            json!({ "op": "update", "id": 2, "status": "in_progress" }),
        )
        .await;
        assert_eq!(output, "0/2 done\n[ ] 1. a\n[>] 2. b");
    }

    /// Every malformed call is an error that names the problem, and none of
    /// them touches the list.
    #[tokio::test]
    async fn malformed_calls_are_refused_and_leave_the_list_alone() {
        let tool = TodoTool::new();
        ok(&tool, write(&[("keep", "pending")])).await;
        let long = "x".repeat(201);
        let many: Vec<Value> = (0..51).map(|i| json!(format!("step {i}"))).collect();
        for (args, needle) in [
            (json!({}), "op"),
            (json!({ "op": "delete" }), "op"),
            (json!({ "op": "write" }), "items"),
            (json!({ "op": "write", "items": "a, b" }), "items"),
            (
                json!({ "op": "write", "items": [{ "content": "" }] }),
                "item 1",
            ),
            (
                json!({ "op": "write", "items": ["ok", { "content": " \n\t " }] }),
                "item 2",
            ),
            (
                json!({ "op": "write", "items": [{ "status": "pending" }] }),
                "item 1",
            ),
            (json!({ "op": "write", "items": [long] }), "200"),
            (json!({ "op": "write", "items": many }), "50"),
            (
                json!({ "op": "write", "items": [{ "content": "a", "status": "done" }] }),
                "in_progress",
            ),
            (json!({ "op": "update", "status": "completed" }), "id"),
            (
                json!({ "op": "update", "id": 0, "status": "completed" }),
                "no item 0",
            ),
            (
                json!({ "op": "update", "id": 2, "status": "completed" }),
                "no item 2",
            ),
            (json!({ "op": "update", "id": 1 }), "status"),
            (
                json!({ "op": "update", "id": 1, "status": "blocked" }),
                "status",
            ),
        ] {
            let result = call(&tool, args.clone()).await;
            assert!(result.is_error, "{args} was accepted");
            assert!(
                result.output.contains(needle),
                "{args}: {} does not mention {needle}",
                result.output
            );
            assert_eq!(result.detail, None, "{args}");
        }
        assert_eq!(view(&tool).await, "0/1 done\n[ ] 1. keep");
    }

    #[tokio::test]
    async fn updating_an_empty_list_says_to_write_it_first() {
        let tool = TodoTool::new();
        let result = call(
            &tool,
            json!({ "op": "update", "id": 1, "status": "completed" }),
        )
        .await;
        assert!(result.is_error);
        assert!(result.output.contains("write"), "{}", result.output);
    }

    /// One item is one row: a newline in an item cannot split it, and the
    /// length limit counts characters, not bytes.
    #[tokio::test]
    async fn item_text_is_kept_to_one_line() {
        let tool = TodoTool::new();
        let output = ok(
            &tool,
            json!({ "op": "write", "items": ["  fix the\n  parser  ", "é".repeat(200)] }),
        )
        .await;
        let first = output.lines().nth(1).unwrap_or_default();
        assert_eq!(first, "[ ] 1. fix the parser");
        assert_eq!(output.lines().count(), 3);
    }

    /// The surface gets the checklist to draw; the model's answer is the
    /// count plus the same rows. An empty list has nothing to draw.
    #[tokio::test]
    async fn a_success_carries_the_checklist_as_its_detail() {
        let tool = TodoTool::new();
        let result = call(&tool, write(&[("a", "completed"), ("b", "in_progress")])).await;
        assert_eq!(result.detail.as_deref(), Some("[x] 1. a\n[>] 2. b"));
        let result = call(&tool, json!({ "op": "write", "items": [] })).await;
        assert!(!result.is_error);
        assert_eq!(result.detail, None);
    }

    /// The chip shows where the list will stand once the call lands, and
    /// working that out changes nothing.
    #[tokio::test]
    async fn describe_shows_the_count_and_the_current_item() {
        let tool = TodoTool::new();
        let args = write(&[
            ("Read the spec", "completed"),
            ("Write the tests", "in_progress"),
            ("Implement it", "pending"),
        ]);
        assert_eq!(
            tool.describe(&args).as_deref(),
            Some("todo 1/3 · Write the tests")
        );
        assert_eq!(view(&tool).await, "todo list is empty");
        ok(&tool, args).await;

        let finish = json!({ "op": "update", "id": 2, "status": "completed" });
        assert_eq!(tool.describe(&finish).as_deref(), Some("todo 2/3"));
        assert_eq!(
            tool.describe(&json!({ "op": "view" })).as_deref(),
            Some("todo 1/3 · Write the tests")
        );
        let long = "y".repeat(120);
        let chip = tool
            .describe(
                &json!({ "op": "write", "items": [{ "content": long, "status": "in_progress" }] }),
            )
            .unwrap_or_default();
        assert!(chip.chars().count() < 70, "{chip}");
        assert!(chip.ends_with('…'), "{chip}");
        assert_eq!(
            tool.describe(&json!({ "op": "write", "items": [] }))
                .as_deref(),
            Some("todo empty")
        );
        assert_eq!(tool.describe(&json!({ "op": "update", "id": 9 })), None);
    }

    /// The list changes nothing outside the session, so it is read-tier and
    /// survives the tier filter plan mode applies.
    #[test]
    fn todo_is_read_tier_and_stays_in_plan_mode() {
        let tool = TodoTool::new();
        let definition = tool.definition();
        assert_eq!(definition.approval, ApprovalTier::Read);
        assert_eq!(definition.spec.name, "todo");
        assert!(!definition.spec.description.is_empty());
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(TodoTool::new()));
        registry.retain_tiers(&[ApprovalTier::Read]);
        assert_eq!(registry.names(), ["todo"]);
    }

    /// The registry clones its handlers per turn; the list is the tool's, so
    /// every clone sees the same one.
    #[tokio::test]
    async fn the_list_outlives_a_registry_clone() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(TodoTool::new()));
        let turn = registry.clone();
        let Some(handler) = turn.get("todo") else {
            panic!("todo is registered");
        };
        let result = handler.invoke(write(&[("a", "pending")])).await;
        assert!(!result.is_error, "{}", result.output);
        let Some(handler) = registry.get("todo") else {
            panic!("todo is registered");
        };
        let result = handler.invoke(json!({ "op": "view" })).await;
        assert_eq!(result.output, "0/1 done\n[ ] 1. a");
    }
}
