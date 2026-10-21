#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A test is linked to the source it is named for, even when it imports
//! nothing from it, and the lens, `outline` and `impact` say so.

use std::fs;
use std::path::Path;

use titi_atlas::{Atlas, LensBudget, SeedInput};

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn fixture(root: &Path) {
    write(
        root,
        "pkg/ledger.py",
        "def settle_accounts(x):\n    return x\n",
    );
    // The test drives the code through a shell-out, importing nothing.
    write(
        root,
        "tests/test_ledger.py",
        "import subprocess\n\ndef test_settle():\n    subprocess.run(['python', 'cli.py'])\n",
    );
    write(
        root,
        "pkg/other.py",
        "def unrelated_thing(x):\n    return x\n",
    );
    write(root, "pkg/cli.py", "def entry():\n    return 1\n");
}

#[test]
fn outline_and_impact_name_the_test_a_file_is_paired_with() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let atlas = Atlas::index(dir.path()).unwrap();
    let outline = atlas.outline("pkg/ledger.py").unwrap();
    assert_eq!(outline.tests, vec!["tests/test_ledger.py".to_owned()]);
    let impact = atlas.impact("pkg/ledger.py").unwrap();
    assert_eq!(impact.tests, vec!["tests/test_ledger.py".to_owned()]);
}

#[test]
fn a_task_naming_a_test_lists_its_source_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let atlas = Atlas::index(dir.path()).unwrap();
    let seeds = atlas.seeds(&SeedInput {
        prompt: "test_settle fails with a wrong total",
        touched: &[],
        changed: &[],
    });
    let lens = atlas.lens(&seeds, LensBudget::tokens(400));
    let source = lens
        .entries
        .iter()
        .find(|entry| entry.path == "pkg/ledger.py")
        .expect("the source of the failing test is listed");
    assert!(
        source.why.contains("tested by tests/test_ledger.py"),
        "{}",
        source.why
    );
    assert!(
        lens.entries
            .iter()
            .all(|entry| entry.path != "pkg/other.py")
    );
}
