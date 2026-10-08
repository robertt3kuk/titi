//! Index a real tree, touch one file without changing a byte, and show what
//! the next refresh had to do.
//!
//! `cargo run -p titi-genome --example refresh -- [root] [file]`
//!
//! The touch is the interesting case: `touch` moves the clock and nothing
//! else, so the size/mtime pre-filter cannot clear the file, and a refresh
//! that only checked the clock would re-parse it. This prints the
//! [`RefreshStats`] of both refreshes and fails if the touched file was
//! re-parsed instead of cleared by content. The counters it prints are global,
//! so on a tree other writers are editing they can move between the two
//! refreshes; the assertions are about the touched file, which they cannot.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use titi_genome::Genome;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".to_owned());
    let target = args
        .next()
        .unwrap_or_else(|| "crates/titi-genome/src/lib.rs".to_owned());

    let mut genome = Genome::default();
    let built = genome.refresh(&root)?;
    println!("// first refresh of {root}: {built:?}");

    let path = PathBuf::from(&root).join(&target);
    let exports_before = genome.files[&target].exports.clone();
    let before = fs::metadata(&path)?.modified()?;
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_modified(before + Duration::from_secs(1))?;

    let touched = genome.refresh(&root)?;

    // Leave the tree as it was found; the clock is not content, and nothing
    // here needs the file to look newer.
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_modified(before)?;

    println!("// after a no-op touch of {target}: {touched:?}");
    // The global counters move if another writer edits this tree between the
    // two refreshes — this repo is worked on by several agents at once — but
    // the touched file's own facts cannot: it was read, cleared by hash, and
    // kept, and without the hash check it would have been re-parsed instead.
    assert!(
        touched.content_unchanged >= 1,
        "unchanged bytes must be cleared by hash, not re-parsed"
    );
    assert_eq!(genome.files[&target].exports, exports_before);
    assert_ne!(
        genome.files[&target].mtime, before,
        "the record took the clock"
    );
    if touched.parsed == 0 && !touched.graph_recomputed {
        println!("// {target}: content-unchanged, graph not recomputed");
    } else {
        println!(
            "// another writer moved the tree during the run; the touched file itself is unchanged"
        );
    }
    Ok(())
}
