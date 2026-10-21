#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! C and C++ extraction: masked comments, decorated and qualified
//! declarations, `static` privacy, and quoted-include resolution.

use std::fs;
use std::path::Path;

use titi_genome::{Genome, Level};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

/// A header with a plain declaration, a pointer-decorated one and a
/// file-private `static` one; a C translation unit and a C++ one that both
/// include the header and define what it declares, plus a missing quoted
/// include, a system include and a commented-out declaration.
fn c_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/util/hash.h",
        r#"#ifndef HASH_H
#define HASH_H

int hash(const char *key);
int* compact(void);
int first(void), second(void);

struct Hash {
    int size;
};

static int helper(void);

// int Ghost(void);

/*
int Gone(void);
*/

#endif
"#,
    );
    write(
        root,
        "src/util/hash.c",
        r#"#include "hash.h"

int hash(const char *key) {
    return key ? (int)key[0] : 0;
}
"#,
    );
    write(
        root,
        "src/main.c",
        r#"#include "util/hash.h"
#include "missing/gone.h"
#include <stdio.h>

int main(void) {
    printf("%d\n", hash("key"));
    return 0;
}
"#,
    );
    write(
        root,
        "src/engine.h",
        r#"#ifndef ENGINE_H
#define ENGINE_H

#include <string>

class Engine {
public:
    void start();
    int *slots();
    std::string name() const;
};

#endif
"#,
    );
    write(
        root,
        "src/engine.cpp",
        r#"#include "util/hash.h"

const char *doc = R"(
int quoted(void);
)";

void Engine::start() {
}

int *Engine::slots() {
    return 0;
}
"#,
    );
    dir
}

#[test]
fn c_declarations_survive_pointers_and_qualifiers() {
    let dir = c_repo();
    let genome = Genome::index(dir.path()).unwrap();
    let header = &genome.files["src/util/hash.h"];

    assert!(
        header.exports.contains(&"hash".to_owned()),
        "{:?}",
        header.exports
    );
    assert!(
        header.exports.contains(&"Hash".to_owned()),
        "{:?}",
        header.exports
    );
    // `int* compact(void)`: a pointer decoration with no space before it.
    assert!(
        header.exports.contains(&"compact".to_owned()),
        "a pointer-decorated return type must be read: {:?}",
        header.exports
    );
    // `int first(void), second(void);` is one declaration with two
    // declarators; the line patterns only ever saw the first of them.
    for name in ["first", "second"] {
        assert!(
            header.exports.contains(&name.to_owned()),
            "`{name}` is declared on the same line as another function: {:?}",
            header.exports
        );
    }
    // `static` is file-private in C, so it is not an export.
    assert!(
        !header.exports.contains(&"helper".to_owned()),
        "static declarations are file-private: {:?}",
        header.exports
    );
    // A commented-out declaration is not a declaration — line or block.
    assert!(
        !header.exports.contains(&"Ghost".to_owned()),
        "{:?}",
        header.exports
    );
    assert!(
        !header.exports.contains(&"Gone".to_owned()),
        "a declaration inside a block comment is not one: {:?}",
        header.exports
    );

    // `std::string name() const`: a qualified return type before the name.
    let engine_header = &genome.files["src/engine.h"];
    assert!(
        engine_header.exports.contains(&"name".to_owned()),
        "a qualified return type must be read: {:?}",
        engine_header.exports
    );
    assert!(
        engine_header.exports.contains(&"Engine".to_owned()),
        "{:?}",
        engine_header.exports
    );

    let cpp = &genome.files["src/engine.cpp"];
    assert!(
        cpp.exports.contains(&"start".to_owned()),
        "{:?}",
        cpp.exports
    );
    assert!(
        cpp.exports.contains(&"slots".to_owned()),
        "{:?}",
        cpp.exports
    );
    // A declaration written inside a raw string literal is string content: the
    // line patterns read `int quoted(void);` as a declaration, the grammar
    // does not.
    assert!(
        !cpp.exports.contains(&"quoted".to_owned()),
        "a raw string's content is not a declaration: {:?}",
        cpp.exports
    );

    // The definition in the .c file is exported there too.
    assert!(
        genome.files["src/util/hash.c"]
            .exports
            .contains(&"hash".to_owned()),
        "{:?}",
        genome.files["src/util/hash.c"].exports
    );
}

#[test]
fn c_quoted_includes_resolve_and_system_ones_stay_quiet() {
    let dir = c_repo();
    let genome = Genome::index(dir.path()).unwrap();

    let main = &genome.files["src/main.c"];
    assert!(
        main.imports.contains(&"src/util/hash.h".to_owned()),
        "imports: {:?}",
        main.imports
    );
    assert!(
        genome.files["src/engine.cpp"]
            .imports
            .contains(&"src/util/hash.h".to_owned()),
        "imports: {:?}",
        genome.files["src/engine.cpp"].imports
    );

    // `<stdio.h>` is not a workspace file: no edge and no diagnostic.
    assert!(!main.imports.iter().any(|path| path.contains("stdio")));
    assert!(
        !main
            .unresolved_imports
            .iter()
            .any(|spec| spec.contains("stdio")),
        "{:?}",
        main.unresolved_imports
    );

    // A quoted include that names no file is a real error.
    assert!(
        main.unresolved_imports
            .contains(&"missing/gone.h".to_owned()),
        "unresolved: {:?}",
        main.unresolved_imports
    );

    let diagnostics = genome.check();
    let unresolved: Vec<_> = diagnostics
        .iter()
        .filter(|item| item.code == "unresolved-import")
        .collect();
    assert!(
        unresolved
            .iter()
            .any(|item| item.message.contains("missing/gone.h")),
        "{unresolved:?}"
    );
    assert!(
        !unresolved.iter().any(|item| item.message.contains("stdio")),
        "a system header must not be reported: {unresolved:?}"
    );

    // Both rows this module serves read their names off a syntax tree.
    for name in ["c", "c++"] {
        let capability = Genome::capabilities()
            .into_iter()
            .find(|capability| capability.language == name)
            .unwrap_or_else(|| panic!("no `{name}` capability"));
        assert_eq!(capability.level, Level::Full, "{name}");
        assert!(
            capability.note.contains("syntax tree"),
            "{}",
            capability.note
        );
    }
}
