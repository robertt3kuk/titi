//! Print the Atlas projection for a workspace: `cargo run -p titi-atlas --example map -- [path]`

fn main() -> std::io::Result<()> {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".to_owned());
    let atlas = titi_atlas::Atlas::index(&root)?;
    let limit = std::env::args()
        .nth(2)
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(24);
    eprintln!(
        "{} files, {} edges",
        atlas.files.len(),
        atlas.dependents.values().sum::<usize>()
    );
    print!("{}", atlas.project(limit));
    Ok(())
}
