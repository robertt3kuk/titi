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
    Comments, Placement, finish, first_known, internal, mask_comments, record, resolve_relative,
    resolve_suffix,
};
use crate::patterns::literal_regex;

use crate::UnresolvedImport;
use crate::symbols::{self, has_child_kind, push_field, push_site, text};

/// The files a specifier can land on, relative or resolved by module path.
const EXTS: &[&str] = &["ts", "tsx", "js", "jsx"];

pub(super) fn parse(path: &str, source: &str, files: &HashSet<String>) -> ParsedFile {
    static FROM: LazyLock<Regex> =
        LazyLock::new(|| literal_regex(r#"(?m)(?:^|\s)(?:from|import)\s+['"]([^'"]+)['"]"#));
    static CALL: LazyLock<Regex> =
        LazyLock::new(|| literal_regex(r#"\b(?:import|require)\s*\(\s*['"]([^'"]+)['"]"#));
    // `export * from './x'` and `export { a, b as c } from './x'`: the file
    // re-publishes what `./x` defines, which is how a barrel works.
    static REEXPORT: LazyLock<Regex> = LazyLock::new(|| {
        literal_regex(
            r#"\bexport\s+(?:type\s+)?(?:\*(?:\s+as\s+\w+)?|\{[^}]*\})\s*from\s*['"]([^'"]+)['"]"#,
        )
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
    cjs_module(tree.root_node(), source.as_bytes(), &mut sites);
    let masked = mask_comments(source, Comments::Slashes);
    let mut imports = Vec::new();
    let mut unresolved: Vec<UnresolvedImport> = Vec::new();
    for re in [&*FROM, &*CALL] {
        for cap in re.captures_iter(&masked) {
            let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            record(
                spec,
                placement(path, spec),
                files,
                &mut imports,
                &mut unresolved,
            );
        }
    }
    let mut reexports = Vec::new();
    for cap in REEXPORT.captures_iter(&masked) {
        let spec = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        if let Placement::Candidates(candidates) | Placement::Optional(candidates) =
            placement(path, spec)
        {
            reexports.extend(first_known(&candidates, files));
        }
    }
    reexports.sort();
    reexports.dedup();
    let mut parsed = finish(
        source,
        sites,
        imports,
        unresolved,
        symbols::error_count(tree.root_node()),
    );
    parsed.reexports = reexports;
    parsed
}

/// A relative specifier is resolved against the importing file and a miss is a
/// warning; a bare or alias specifier is resolved by module path and a miss
/// means it belongs to node_modules, not to this workspace.
fn placement(path: &str, spec: &str) -> Placement {
    if spec.starts_with('.') {
        return internal(resolve_relative(path, spec, EXTS));
    }
    Placement::Optional(resolve_suffix(spec, EXTS))
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

/// CommonJS: what `module.exports` and `exports` publish.
///
/// `exports.x = …`, `module.exports.x = …`, `module.exports = { a, b }`,
/// `module.exports = name` and `module.exports = function name() {}` each
/// publish names. A variable that is assigned `module.exports` — either in a
/// chain (`var app = exports = module.exports = {}`) or later
/// (`module.exports = res`) — is an alias, and `app.x = …` publishes `x`.
fn cjs_module(root: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    let mut aliases: HashSet<String> = HashSet::from(["exports".to_owned()]);
    let statements: Vec<Node> = {
        let mut cursor = root.walk();
        root.named_children(&mut cursor).collect()
    };
    // Pass one: the aliases.
    for statement in &statements {
        let Some((targets, value)) = cjs_chain(*statement) else {
            continue;
        };
        if !targets
            .iter()
            .any(|node| is_module_exports(*node, source, &aliases))
        {
            continue;
        }
        for target in &targets {
            if target.kind() == "identifier" {
                if let Some(name) = text(*target, source) {
                    aliases.insert(name);
                }
            }
        }
        if let Some(value) = value {
            if value.kind() == "identifier" {
                if let Some(name) = text(value, source) {
                    aliases.insert(name);
                }
            }
        }
    }
    // Pass two, in source order.
    for statement in &statements {
        let Some((targets, value)) = cjs_chain(*statement) else {
            continue;
        };
        let publishes = targets
            .iter()
            .any(|node| is_module_exports(*node, source, &aliases));
        if publishes {
            if let Some(value) = value {
                cjs_value(value, source, out);
            }
        }
        for target in &targets {
            if target.kind() != "member_expression" {
                continue;
            }
            let (Some(object), Some(property)) = (
                target.child_by_field_name("object"),
                target.child_by_field_name("property"),
            ) else {
                continue;
            };
            if property.kind() == "property_identifier"
                && is_module_exports(object, source, &aliases)
            {
                push_site(property, source, out);
            }
        }
    }
}

/// `a = b = value` or `var a = b = value`: the assignment targets, outermost
/// first, and the final value.
fn cjs_chain(statement: Node) -> Option<(Vec<Node>, Option<Node>)> {
    let mut targets = Vec::new();
    let mut current = match statement.kind() {
        "expression_statement" => statement.named_child(0)?,
        "variable_declaration" | "lexical_declaration" => {
            let declarator = statement.named_child(0)?;
            if declarator.kind() != "variable_declarator" {
                return None;
            }
            targets.push(declarator.child_by_field_name("name")?);
            declarator.child_by_field_name("value")?
        }
        _ => return None,
    };
    while current.kind() == "assignment_expression" {
        targets.push(current.child_by_field_name("left")?);
        current = current.child_by_field_name("right")?;
    }
    if targets.is_empty() {
        return None;
    }
    Some((targets, Some(current)))
}

fn is_module_exports(node: Node, source: &[u8], aliases: &HashSet<String>) -> bool {
    match node.kind() {
        "identifier" => text(node, source).is_some_and(|name| aliases.contains(&name)),
        "member_expression" => text(node, source).is_some_and(|text| text == "module.exports"),
        _ => false,
    }
}

/// The value assigned to `module.exports`: an object literal publishes its
/// keys, an identifier or a named function/class publishes its name.
fn cjs_value(value: Node, source: &[u8], out: &mut Vec<crate::ExportSite>) {
    match value.kind() {
        "identifier" => push_site(value, source, out),
        "function_expression" | "function" | "generator_function" | "class" => {
            push_field(value, source, out);
        }
        "object" => {
            let mut cursor = value.walk();
            for member in value.named_children(&mut cursor) {
                match member.kind() {
                    "shorthand_property_identifier" => push_site(member, source, out),
                    "pair" | "method_definition" => {
                        let key = member
                            .child_by_field_name("key")
                            .or_else(|| member.child_by_field_name("name"));
                        if let Some(key) = key {
                            if matches!(key.kind(), "property_identifier" | "identifier") {
                                push_site(key, source, out);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
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
    #[test]
    fn commonjs_exports_assignments_are_exports() {
        let names = names(
            "lib/utils.js",
            r#"
exports.methods = ['get'];
exports.etag = createETagGenerator({ weak: false })
exports.normalizeType = function(type){ return type }
function createETagGenerator() {}
function notExported() {}
"#,
        );
        for want in ["methods", "etag", "normalizeType"] {
            assert!(names.contains(&want.to_owned()), "{want}: {names:?}");
        }
        assert!(!names.contains(&"notExported".to_owned()));
        assert!(!names.contains(&"createETagGenerator".to_owned()));
    }

    #[test]
    fn commonjs_module_exports_shapes() {
        let object = names(
            "lib/a.js",
            "function one() {}\nmodule.exports = { one, two: function() {}, three() {} };\n",
        );
        for want in ["one", "two", "three"] {
            assert!(object.contains(&want.to_owned()), "{want}: {object:?}");
        }
        let ident = names(
            "lib/express.js",
            "exports = module.exports = createApplication;\nfunction createApplication() {}\nexports.Router = Router;\n",
        );
        assert!(ident.contains(&"createApplication".to_owned()), "{ident:?}");
        assert!(ident.contains(&"Router".to_owned()), "{ident:?}");
        let named = names(
            "lib/b.js",
            "module.exports = function build() {};\nmodule.exports.version = 1;\n",
        );
        assert!(named.contains(&"build".to_owned()), "{named:?}");
        assert!(named.contains(&"version".to_owned()), "{named:?}");
    }

    #[test]
    fn commonjs_alias_of_module_exports_publishes_its_members() {
        let app = names(
            "lib/application.js",
            "var app = exports = module.exports = {};\napp.init = function init() {};\napp.handle = function handle() {};\nvar local = {};\nlocal.x = 1;\n",
        );
        assert!(app.contains(&"init".to_owned()), "{app:?}");
        assert!(app.contains(&"handle".to_owned()), "{app:?}");
        assert!(!app.contains(&"x".to_owned()), "{app:?}");
        let res = names(
            "lib/response.js",
            "var res = Object.create(proto);\nmodule.exports = res;\nres.send = function send() {};\n",
        );
        assert!(res.contains(&"send".to_owned()), "{res:?}");
    }
}
