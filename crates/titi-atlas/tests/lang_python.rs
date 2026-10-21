#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! E3: Python's import extraction — absolute module paths, relative paths and
//! the package-submodule form, without warning on the stdlib.

use std::fs;
use std::path::Path;

use titi_atlas::{Atlas, Level};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn index(root: &Path) -> Atlas {
    Atlas::index(root).unwrap()
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

    let atlas = index(root);
    let service = &atlas.files["app/service.py"];
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
    let third_party = &atlas.files["app/third_party.py"];
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
        atlas.files["app/broken.py"].unresolved_imports,
        vec!["app.missing".to_owned()]
    );
    let warnings: Vec<String> = atlas
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

    let atlas = index(root);
    for path in ["app/consumer_dot.py", "app/consumer_pkg.py"] {
        let file = &atlas.files[path];
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

    let atlas = index(root);
    assert!(
        atlas.check().iter().all(|item| item.code != "capability"),
        "{:?}",
        atlas.check()
    );
    let python = Atlas::capabilities()
        .into_iter()
        .find(|capability| capability.language == "python")
        .expect("a python capability");
    assert_eq!(python.level, Level::Full, "{}", python.note);
}

/// A package facade re-exports: `from .models import Request, Response as R`
/// in `__init__.py`, an alias module of nothing but imports, and `__all__`.
/// The names are exports of the facade and its imports are re-exports, so a
/// user of `pkg.Request` reaches `models.py` through it.
#[test]
fn python_facades_and_alias_modules_publish_what_they_import() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "pkg/__init__.py",
        "from .models import Request, Response as Resp\nfrom .api import *\nfrom os import path\nimport pkg.utils as helpers\n",
    );
    write(
        root,
        "pkg/models.py",
        "class Request:\n    pass\n\nclass Response:\n    pass\n",
    );
    write(root, "pkg/api.py", "def get(url):\n    return url\n");
    write(root, "pkg/utils.py", "def slug(x):\n    return x\n");
    write(
        root,
        "pkg/compat.py",
        "from .models import Request\nfrom json import loads\n",
    );
    write(
        root,
        "pkg/all.py",
        "from .api import get\nfrom .models import Response\n__all__ = ['get']\nunrelated = 1\n",
    );
    write(
        root,
        "pkg/real.py",
        "from .models import Request\n\ndef work():\n    return Request()\n",
    );

    let atlas = index(root);
    let init = &atlas.files["pkg/__init__.py"];
    assert_eq!(
        init.exports,
        vec!["Request", "Resp", "helpers"],
        "{:?}",
        init.exports
    );
    for target in ["pkg/models.py", "pkg/api.py", "pkg/utils.py"] {
        assert!(
            init.reexports.contains(&target.to_owned()),
            "{target}: {:?}",
            init.reexports
        );
    }
    let compat = &atlas.files["pkg/compat.py"];
    assert_eq!(
        compat.exports,
        vec!["Request"],
        "an import-only module publishes workspace names"
    );
    assert_eq!(compat.reexports, vec!["pkg/models.py".to_owned()]);
    let all = &atlas.files["pkg/all.py"];
    assert_eq!(
        all.exports,
        vec!["get"],
        "__all__ decides: {:?}",
        all.exports
    );
    assert_eq!(all.reexports, vec!["pkg/api.py".to_owned()]);
    let real = &atlas.files["pkg/real.py"];
    assert_eq!(
        real.exports,
        vec!["work"],
        "a module with its own defs does not re-export"
    );
    assert!(real.reexports.is_empty());
}
