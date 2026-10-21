#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! A C header and the file that implements it are one module.

use std::fs;
use std::path::Path;

use titi_atlas::Atlas;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

/// Everyone includes `net.h`; nothing includes `net.c`. By include edges alone
/// the implementation is the least important file of the module (hiredis: its
/// `hiredis.c`, `net.c`, `read.c` were absent from a 24-file map while their
/// headers led it).
#[test]
fn an_implementation_ranks_with_the_header_everyone_includes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "net.h", "int net_connect(int fd);\n");
    write(
        root,
        "net.c",
        "#include \"net.h\"\nint net_connect(int fd) { return fd; }\n",
    );
    for n in 0..8 {
        write(
            root,
            &format!("user{n}.c"),
            "#include \"net.h\"\nint f(void) { return net_connect(1); }\n",
        );
    }
    write(root, "lonely.c", "int lonely(void) { return 0; }\n");
    let atlas = Atlas::index(root).unwrap();

    assert!(
        atlas.ranks["net.c"] > atlas.ranks["lonely.c"],
        "net.c {} vs lonely.c {}",
        atlas.ranks["net.c"],
        atlas.ranks["lonely.c"]
    );
    assert!(atlas.ranks["net.c"] > atlas.ranks["user0.c"]);
    // It is not a dependent: `(→N)` still counts only files that include or use.
    assert_eq!(atlas.dependents["net.c"], 0);
}
