//! TypeScript and JavaScript: what the TS/TSX grammars publish.
//!
//! Both rows in the language table point here, because one grammar family
//! parses them: `.ts`/`.mts`/`.cts` use the TypeScript grammar and
//! `.tsx`/`.js`/`.jsx`/`.mjs`/`.cjs` use the TSX grammar, which accepts plain
//! JavaScript and JSX as well. Import specifiers are still matched by pattern
//! — a specifier is a string, not a node this crate walks — but every export
//! is a node the grammar produced.
//!
//! Three specifier shapes reach one resolution rule: a static `from '…'`, an
//! `import '…'` for its side effect, and a call — `require('./legacy')` or a
//! dynamic `import('./x')`. A relative specifier stays exact, so one that
//! names no file is a real broken path and warns. Anything else is a package
//! (`react`) or a monorepo alias (`widgets/Widget`); resolving it by module
//! path makes the alias an edge, and naming no file makes it external rather
//! than a missing workspace file.
use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::Node;

use super::ParsedFile;
use super::support::{
    Comments, Placement, finish, internal, mask_comments, record, resolve_relative, resolve_suffix,
};

use crate::symbols::{self, has_child_kind, push_field, push_site, text};

/// The files a specifier can land on, relative or resolved by module path.
const EXTS: &[&str] = &["ts", "tsx", "js", "jsx"];

pub(super) fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static FROM: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?m)(?:^|\s)(?:from|import)\s+['"]([^'"]+)['"]"#).expect("ts from")
    });
    static CALL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\b(?:import|require)\s*\(\s*['"]([^'"]+)['"]"#).expect("ts require")
    });
    let Some(grammar) = super::grammar_for(path) else {
        return ParsedFile::default();
    };
    let Some(tree) = symbols::parse(grammar, source) else {
        return ParsedFile {
            syntax_errors: 1,
            ..ParsedFile::default()
        };
    };
    let mut sites = Vec::new();
    ts_module(tree.root_node(), source.as_bytes(), &mut sites);
    let masked = mask_comments(source, Comments::Slashes);
    let mut imports = Vec::new();
    let mut unresolved = Vec::new();
    for re in [&*FROM, &*CALL] {
        for cap in re.captures_iter(&masked) {
            let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            record(
                spec,
                placement(path, spec, files),
                &mut imports,
                &mut unresolved,
            );
        }
    }
    finish(
        source,
        sites,
        imports,
        unresolved,
        symbols::error_count(tree.root_node()),
    )
}

/// A relative specifier is resolved against the importing file and a miss is a
/// warning; a bare or alias specifier is resolved by module path and a miss
/// means it belongs to node_modules, not to this workspace.
fn placement(path: &str, spec: &str, files: &HashSet<String>) -> Placement {
    if spec.starts_with('.') {
        return internal(resolve_relative(path, spec, files, EXTS));
    }
    match resolve_suffix(spec, EXTS, files) {
        Some(resolved) => Placement::Resolved(resolved),
        None => Placement::External,
    }
}

/// TypeScript and JavaScript: what an `export` publishes, plus the public
/// members of an exported class — `client.send(…)` names a symbol the file
/// owns just as much as the class does.
fn ts_module(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "export_statement" {
            ts_export(child, source, out);
        }
    }
}

fn ts_export(node: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            // `export { a, b as c }`: the exported name is the alias.
            "export_clause" => {
                let mut specs = child.walk();
                for spec in child.named_children(&mut specs) {
                    if spec.kind() != "export_specifier" {
                        continue;
                    }
                    let name = spec
                        .child_by_field_name("alias")
                        .or_else(|| spec.child_by_field_name("name"));
                    if let Some(name) = name {
                        push_site(name, source, out);
                    }
                }
            }
            "class_declaration" | "abstract_class_declaration" => {
                push_field(child, source, out);
                if let Some(body) = child.child_by_field_name("body") {
                    ts_class_members(body, source, out);
                }
            }
            "function_declaration"
            | "generator_function_declaration"
            | "enum_declaration"
            | "type_alias_declaration"
            | "interface_declaration"
            | "internal_module"
            | "module" => {
                push_field(child, source, out);
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut declarators = child.walk();
                for declarator in child.named_children(&mut declarators) {
                    if declarator.kind() != "variable_declarator" {
                        continue;
                    }
                    let Some(name) = declarator.child_by_field_name("name") else {
                        continue;
                    };
                    // Destructuring publishes several names; the pattern's own
                    // identifiers are the exported ones.
                    if name.kind() == "identifier" {
                        push_site(name, source, out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn ts_class_members(body: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if !matches!(
            member.kind(),
            "method_definition" | "public_field_definition" | "method_signature"
        ) {
            continue;
        }
        if has_child_kind(member, "accessibility_modifier")
            && member
                .child_by_field_name("name")
                .is_none_or(|name| name.kind() == "private_property_identifier")
        {
            continue;
        }
        let Some(name) = member.child_by_field_name("name") else {
            continue;
        };
        // `#private` members and the constructor are not names a caller uses.
        if name.kind() == "private_property_identifier" {
            continue;
        }
        if text(name, source).is_some_and(|name| name != "constructor") {
            push_site(name, source, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::lang::test_support::exports;

    fn names(path: &str, source: &str) -> Vec<String> {
        exports(path, source).unwrap_or_else(|| panic!("{path} parses"))
    }

    #[test]
    fn typescript_symbols_include_class_members() {
        let names = names(
            "src/client.ts",
            r#"
export class Client {
  private secret = 1;
  constructor(private url: string) {}
  send(body: string) {}
  static create() { return new Client(""); }
  #hidden() {}
}
export function connect() {}
export const TIMEOUT = 30;
export interface Options { retries: number }
export type Handler = (x: number) => void;
export enum Level { Info }
const notExported = 2;
"#,
        );
        assert!(names.contains(&"Client".to_owned()));
        // Members of an exported class, which the regex never saw.
        assert!(names.contains(&"send".to_owned()), "{names:?}");
        assert!(names.contains(&"create".to_owned()));
        assert!(names.contains(&"connect".to_owned()));
        assert!(names.contains(&"TIMEOUT".to_owned()));
        assert!(names.contains(&"Options".to_owned()));
        assert!(names.contains(&"Handler".to_owned()));
        assert!(names.contains(&"Level".to_owned()));
        assert!(!names.contains(&"constructor".to_owned()));
        assert!(!names.contains(&"hidden".to_owned()));
        assert!(!names.contains(&"notExported".to_owned()));
    }

    #[test]
    fn javascript_and_jsx_parse_with_the_tsx_grammar() {
        let names = names(
            "src/view.jsx",
            r#"
export default class View {
  render() { return <div className="x">hi</div>; }
}
export const widgets = [];
export { helper as publicHelper };
function helper() {}
"#,
        );
        assert!(names.contains(&"View".to_owned()), "{names:?}");
        assert!(names.contains(&"render".to_owned()));
        assert!(names.contains(&"widgets".to_owned()));
        // Re-exports publish the alias, not the local name.
        assert!(names.contains(&"publicHelper".to_owned()));
        assert!(!names.contains(&"helper".to_owned()));
    }
}
