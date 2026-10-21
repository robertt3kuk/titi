#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A file that only re-publishes a name is the road to it, not a second
//! definition of it and not a user of it.

use std::fs;
use std::path::Path;

use titi_genome::Genome;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn export_lines(genome: &Genome, path: &str) -> Vec<String> {
    let map = genome.project(64);
    let mut lines = map.lines();
    let header = format!("{path} (→");
    if !lines.any(|line| line.starts_with(&header)) {
        return Vec::new();
    }
    lines
        .take_while(|line| line.starts_with("  +"))
        .map(|line| line.trim().to_owned())
        .collect()
}

/// requests: `__init__.py` imports `Session` from `sessions.py` for its
/// callers, which made `check` report fifteen names defined twice.
#[test]
fn a_python_facade_does_not_define_or_use_what_it_re_exports() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "pkg/__init__.py", "from .core import Engine\n");
    write(root, "pkg/core.py", "class Engine:\n    pass\n");
    write(
        root,
        "app.py",
        "from pkg import Engine\n\n\ndef main():\n    Engine()\n",
    );
    let genome = Genome::index(root).unwrap();

    assert!(
        genome
            .check()
            .iter()
            .all(|item| item.code != "ambiguous-symbol"),
        "{:?}",
        genome.check()
    );
    assert_eq!(export_lines(&genome, "pkg/core.py"), vec!["+Engine (1)"]);
}

/// `pub use inner::helper;` in a module file is not a call of `helper`.
#[test]
fn a_rust_re_export_is_not_a_user() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        "mod util;\npub fn run() { util::helper(); }\n",
    );
    write(
        root,
        "src/util/mod.rs",
        "mod inner;\npub use inner::helper;\n",
    );
    write(root, "src/util/inner.rs", "pub fn helper() {}\n");
    let genome = Genome::index(root).unwrap();

    assert_eq!(
        export_lines(&genome, "src/util/inner.rs"),
        vec!["+helper (1)"]
    );
}
