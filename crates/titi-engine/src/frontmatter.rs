//! The YAML block at the top of a `SKILL.md` or a command file.
//!
//! Real YAML, so a folded `description: >` or a `metadata:` map reads as the
//! author wrote it. A file whose block does not parse is still a file: the
//! caller keeps it, and [`Parsed::problem`] says why its fields are missing.
//! Before giving up on the whole block each top-level key is tried alone, so
//! one bad line costs one field, not all of them.

use std::collections::BTreeMap;

use serde_yaml::Value;

/// What [`parse`] found in a file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Parsed<'a> {
    /// Top-level keys of the block that parsed.
    pub fields: BTreeMap<String, Value>,
    /// Everything after the block; the whole text when there is none.
    pub body: &'a str,
    /// Why some or all of the block was not read. `None` for a clean block or
    /// a file without one.
    pub problem: Option<String>,
}

/// Split `text` into its frontmatter fields and its body.
pub(crate) fn parse(text: &str) -> Parsed<'_> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(opened) = strip_fence(text) else {
        return Parsed {
            fields: BTreeMap::new(),
            body: text,
            problem: None,
        };
    };
    let Some((block, body)) = split_at_closing_fence(opened) else {
        return Parsed {
            fields: BTreeMap::new(),
            body: text,
            problem: Some("frontmatter is never closed by a --- line".to_owned()),
        };
    };
    match serde_yaml::from_str::<Value>(block) {
        Ok(Value::Mapping(map)) => Parsed {
            fields: string_keys(map),
            body,
            problem: None,
        },
        Ok(Value::Null) => Parsed {
            fields: BTreeMap::new(),
            body,
            problem: None,
        },
        Ok(_) => Parsed {
            fields: BTreeMap::new(),
            body,
            problem: Some("frontmatter is not a key: value map".to_owned()),
        },
        Err(error) => {
            let (fields, dropped) = recover_keys(block);
            let reason = one_line(&error.to_string());
            let problem = if fields.is_empty() {
                format!("invalid YAML ({reason}); no fields read")
            } else {
                format!(
                    "invalid YAML ({reason}); read {}, dropped {}",
                    fields.keys().cloned().collect::<Vec<_>>().join(", "),
                    dropped.join(", ")
                )
            };
            Parsed {
                fields,
                body,
                problem: Some(problem),
            }
        }
    }
}

/// `text` after an opening `---` line, or `None` when it does not open with one.
fn strip_fence(text: &str) -> Option<&str> {
    let first_end = text.find('\n').unwrap_or(text.len());
    if text[..first_end].trim_end() != "---" {
        return None;
    }
    Some(text.get(first_end + 1..).unwrap_or(""))
}

/// The block before the first line that is just `---`, and what follows it.
fn split_at_closing_fence(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if matches!(line.trim_end(), "---" | "...") {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

fn string_keys(map: serde_yaml::Mapping) -> BTreeMap<String, Value> {
    map.into_iter()
        .filter_map(|(key, value)| Some((key.as_str()?.to_owned(), value)))
        .collect()
}

/// Parse each top-level key on its own. Returns what read and the keys that
/// did not.
fn recover_keys(block: &str) -> (BTreeMap<String, Value>, Vec<String>) {
    let mut chunks: Vec<(String, String)> = Vec::new();
    for line in block.lines() {
        let top_level_key = !line.starts_with([' ', '\t', '#', '-']) && line.contains(':');
        if top_level_key {
            let key = line.split(':').next().unwrap_or("").trim().to_owned();
            chunks.push((key, format!("{line}\n")));
        } else if let Some((_, text)) = chunks.last_mut() {
            text.push_str(line);
            text.push('\n');
        }
    }
    let mut fields = BTreeMap::new();
    let mut dropped = Vec::new();
    for (key, text) in chunks {
        match serde_yaml::from_str::<Value>(&text) {
            Ok(Value::Mapping(map)) => fields.extend(string_keys(map)),
            _ => dropped.push(key),
        }
    }
    (fields, dropped)
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A scalar as text. Lists and maps have no single text and give `None`.
pub(crate) fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// `true`/`false`, as a YAML bool or as the word.
pub(crate) fn flag_of(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Collapse a field to one printable line: whitespace runs become one space
/// and other control characters (an escape sequence in a description would
/// reach the terminal) are dropped.
pub(crate) fn clean_line(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|ch| !ch.is_control())
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Keys other agents honour that titi reads and then ignores.
///
/// `allowed-tools` would pre-approve tools and `hooks` would run commands on
/// a repository's say-so; `model` and the rest change cost or isolation.
/// None of them is the author's to decide for the person running the file.
/// The panel lists them so a skill written for Claude Code is not silently
/// half-applied.
const IGNORED_KEYS: [&str; 8] = [
    "allowed-tools",
    "disallowed-tools",
    "model",
    "hooks",
    "context",
    "agent",
    "shell",
    "effort",
];

/// The [`IGNORED_KEYS`] a file sets, plus `` !`cmd` `` when its body has one:
/// that is how another agent runs a shell command while loading a file.
pub(crate) fn ignored(parsed: &Parsed<'_>) -> Vec<String> {
    let mut found: Vec<String> = IGNORED_KEYS
        .iter()
        .filter(|key| parsed.fields.contains_key(**key))
        .map(|key| (*key).to_owned())
        .collect();
    if parsed.body.contains("!`") {
        found.push("!`cmd` in body".to_owned());
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folded_description_is_one_value() {
        let parsed = parse("---\nname: x\ndescription: >\n  first\n  second\n---\nbody\n");
        assert_eq!(parsed.problem, None);
        assert_eq!(
            text_of(&parsed.fields["description"]).map(|text| clean_line(&text)),
            Some("first second".to_owned())
        );
        assert_eq!(parsed.body, "body\n");
    }

    #[test]
    fn one_bad_key_costs_one_field() {
        let parsed =
            parse("---\nname: x\ndescription: Use when: a thing\nlicense: MIT\n---\nbody\n");
        assert_eq!(text_of(&parsed.fields["name"]).as_deref(), Some("x"));
        assert_eq!(text_of(&parsed.fields["license"]).as_deref(), Some("MIT"));
        assert!(!parsed.fields.contains_key("description"));
        let problem = parsed.problem.expect("a problem is reported");
        assert!(problem.contains("dropped description"), "{problem}");
        assert_eq!(parsed.body, "body\n");
    }

    #[test]
    fn a_hopeless_block_reads_no_fields_but_keeps_the_body() {
        let parsed = parse("---\nname: [oops\ndescription: a: b\n---\nbody\n");
        assert!(parsed.fields.is_empty());
        assert!(parsed.problem.expect("problem").contains("no fields read"));
        assert_eq!(parsed.body, "body\n");
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let parsed = parse("# Title\n---\nnot yaml\n");
        assert_eq!(parsed.problem, None);
        assert_eq!(parsed.body, "# Title\n---\nnot yaml\n");
    }

    #[test]
    fn an_unclosed_block_keeps_the_whole_text() {
        let parsed = parse("---\nname: x\nbody that never closes\n");
        assert!(parsed.fields.is_empty());
        assert!(parsed.problem.expect("problem").contains("never closed"));
        assert!(parsed.body.contains("body that never closes"));
    }

    #[test]
    fn crlf_files_parse() {
        let parsed = parse("---\r\nname: x\r\n---\r\nbody\r\n");
        assert_eq!(text_of(&parsed.fields["name"]).as_deref(), Some("x"));
        assert_eq!(parsed.body, "body\r\n");
    }

    #[test]
    fn control_characters_do_not_reach_a_field() {
        assert_eq!(clean_line("a\u{1b}[31m  b\n c"), "a[31m b c");
    }
}
