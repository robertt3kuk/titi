//! E3: Python's import extraction — absolute module paths, relative paths and
//! the package-submodule form, without warning on the stdlib.

#![allow(clippy::unwrap_used, clippy::expect_used)]

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

fn index(root: &Path) -> Genome {
    Genome::index(root).unwrap()
}

const SERVICE: &str = r#"
import os
import app.util
from app.util import helper
from app import sibling
from . import sibling

# from app.legacy import old_thing


def run():
    return helper()


def _hidden():
    return run()


# def gone():
#     pass
"#;

/// Absolute intra-repo imports are the way most Python is written; before this
/// they left no edge at all, and a naive `import os` would have warned.
#[test]
fn python_absolute_imports_reach_the_graph_and_the_stdlib_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "app/service.py", SERVICE);
    write(root, "app/util.py", "def helper():\n    return 1\n");
    write(root, "app/sibling.py", "def pair():\n    return 2\n");
    write(root, "app/broken.py", "from app.missing import x\n");
    write(
        root,
        "app/third_party.py",
        "import nosuchlib\n\n\ndef use():\n    return 1\n",
    );

    let genome = index(root);
    let service = &genome.files["app/service.py"];
    assert!(
        service.imports.contains(&"app/util.py".to_owned()),
        "imports: {:?}",
        service.imports
    );
    assert!(
        service.imports.contains(&"app/sibling.py".to_owned()),
        "imports: {:?}",
        service.imports
    );
    assert!(
        !service.imports.iter().any(|path| path.contains("os.py")),
        "imports: {:?}",
        service.imports
    );
    // `os` is std and `nosuchlib` names no directory of this workspace: neither
    // is an edge, and neither is a warning.
    assert!(
        service.unresolved_imports.is_empty(),
        "{:?}",
        service.unresolved_imports
    );
    let third_party = &genome.files["app/third_party.py"];
    assert!(third_party.imports.is_empty(), "{:?}", third_party.imports);
    assert!(
        third_party.unresolved_imports.is_empty(),
        "{:?}",
        third_party.unresolved_imports
    );

    // The exports are the grammar's, so a leading underscore and a commented
    // definition stay out.
    assert_eq!(service.exports, vec!["run".to_owned()]);

    // A workspace-shaped specifier that names no file is the only warning.
    assert_eq!(
        genome.files["app/broken.py"].unresolved_imports,
        vec!["app.missing".to_owned()]
    );
    let warnings: Vec<String> = genome
        .check()
        .into_iter()
        .filter(|item| item.code == "unresolved-import")
        .map(|item| item.message)
        .collect();
    assert_eq!(warnings, vec!["unresolved import `app.missing`".to_owned()]);
}

/// `from . import sibling` has no module after the dots, and
/// `from app import sibling` names a package whose submodule is the file; both
/// must reach `app/sibling.py` instead of being called missing.
#[test]
fn python_package_and_dotted_only_imports_reach_the_sibling_module() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app/consumer_dot.py",
        "from . import sibling\n\n\ndef use():\n    return sibling.pair()\n",
    );
    write(
        root,
        "app/consumer_pkg.py",
        "from app import sibling\n\n\ndef use():\n    return sibling.pair()\n",
    );
    write(root, "app/sibling.py", "def pair():\n    return 2\n");

    let genome = index(root);
    for path in ["app/consumer_dot.py", "app/consumer_pkg.py"] {
        let file = &genome.files[path];
        assert_eq!(
            file.imports,
            vec!["app/sibling.py".to_owned()],
            "{path} imports: {:?}",
            file.imports
        );
        assert!(
            file.unresolved_imports.is_empty(),
            "{path} unresolved: {:?}",
            file.unresolved_imports
        );
    }
}

/// The level a user sees comes from the roster; `check` reports findings only,
/// so a healthy tree says nothing at all.
#[test]
fn python_reports_its_level_in_the_capability_roster() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "app/service.py", SERVICE);
    write(root, "app/util.py", "def helper():\n    return 1\n");
    write(root, "app/sibling.py", "def pair():\n    return 2\n");

    let genome = index(root);
    assert!(
        genome.check().iter().all(|item| item.code != "capability"),
        "{:?}",
        genome.check()
    );
    let python = Genome::capabilities()
        .into_iter()
        .find(|capability| capability.language == "python")
        .expect("a python capability");
    assert_eq!(python.level, Level::Full, "{}", python.note);
}
