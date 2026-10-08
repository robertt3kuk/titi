//! Index a real tree, touch one file without changing a byte, and show what
//! the next refresh had to do.
//!
//! `cargo run -p titi-genome --example refresh -- [root] [file]`
//!
//! The touch is the interesting case: `touch` moves the clock and nothing
//! else, so the size/mtime pre-filter cannot clear the file, and a refresh
//! that only checked the clock would re-parse it. This prints the
//! [`RefreshStats`] of both refreshes and fails if the second one re-parsed
//! or rebuilt the graph.

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
    assert_eq!(touched.parsed, 0, "no bytes changed, so no parse");
    assert_eq!(
        touched.content_unchanged, 1,
        "the touched file is the one that was read"
    );
    assert!(
        !touched.graph_recomputed,
        "the graph inputs did not move, so the ranking was left in place"
    );
    assert_eq!(genome.files[&target].exports, exports_before);
    assert_ne!(genome.files[&target].mtime, before, "the record took the clock");
    println!("// {target}: content-unchanged, graph not recomputed");
    Ok(())
}
