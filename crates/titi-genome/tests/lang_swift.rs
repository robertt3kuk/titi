#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! Swift extraction through [`Genome`]: `import` module resolution by the
//! module-file convention, declaration kinds, and `public` properties.
//!
//! Every assertion is on a real extracted name or path, so deleting the
//! per-language handling in `src/lang/swift.rs` fails this file.

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

const MYLIB: &str = r#"
import Foundation

// public func ghost()

public func greet() {}

public struct Widget {}

public let token = 1

actor Counter {}

let doc = """
public func quoted() {}
"""
"#;

#[test]
fn swift_resolves_module_imports_and_reads_declarations() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "Sources/MyLib/MyLib.swift", MYLIB);
    write(
        root,
        "Sources/App/App.swift",
        "import MyLib\n@testable import MyLib\n\nfunc run() {\n    var local = 0\n}\n",
    );
    // A kind-qualified import names the module, not the member after the dot.
    write(
        root,
        "Sources/App/Detail.swift",
        "import class MyLib.Widget\n",
    );
    write(root, "Sources/Other/Thing.swift", "import Foundation\n");

    let genome = Genome::index(root).unwrap();
    let mylib = &genome.files["Sources/MyLib/MyLib.swift"];

    for name in ["greet", "Widget", "Counter", "token"] {
        assert!(
            mylib.exports.contains(&name.to_owned()),
            "missing {name}: {:?}",
            mylib.exports
        );
    }
    assert!(
        !mylib.exports.contains(&"ghost".to_owned()),
        "a commented-out declaration is not an export: {:?}",
        mylib.exports
    );
    // A `public func` written inside a multi-line `"""` string is string
    // content: the pattern read it (its odd quote count confused the masker),
    // the grammar does not.
    assert!(
        !mylib.exports.contains(&"quoted".to_owned()),
        "a multi-line string's content is not a declaration: {:?}",
        mylib.exports
    );

    // `import MyLib` resolves to the module's own file and becomes an edge.
    let app = &genome.files["Sources/App/App.swift"];
    assert!(
        app.imports
            .contains(&"Sources/MyLib/MyLib.swift".to_owned()),
        "module import must resolve to its primary file: {:?}",
        app.imports
    );
    assert!(
        app.exports.contains(&"run".to_owned()),
        "declarations are exported: {:?}",
        app.exports
    );
    assert!(
        !app.exports.contains(&"local".to_owned()),
        "a local `var` is not an export: {:?}",
        app.exports
    );

    // `import class MyLib.Widget` is a module import: the module is `MyLib`.
    let detail = &genome.files["Sources/App/Detail.swift"];
    assert!(
        detail
            .imports
            .contains(&"Sources/MyLib/MyLib.swift".to_owned()),
        "the module is the first dotted name, not the member: {:?}",
        detail.imports
    );

    // A dependency module names no workspace file: no edge, no warning.
    let thing = &genome.files["Sources/Other/Thing.swift"];
    assert!(
        thing.imports.is_empty(),
        "`import Foundation` is not an edge: {:?}",
        thing.imports
    );
    assert!(
        thing.unresolved_imports.is_empty(),
        "`import Foundation` is not unresolved: {:?}",
        thing.unresolved_imports
    );

    let diagnostics = genome.check();
    assert!(
        diagnostics
            .iter()
            .all(|item| !item.message.contains("Foundation")),
        "{diagnostics:?}"
    );
    let swift = Genome::capabilities()
        .into_iter()
        .find(|capability| capability.language == "swift")
        .expect("a swift capability");
    assert_eq!(swift.level, Level::Full, "{}", swift.note);
    assert!(swift.note.contains("syntax tree"), "{}", swift.note);
}
