use serde_json::{json, Map, Value};
use titi_genome::Genome;
fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let g = Genome::index(&root).unwrap();
    let mut files = Map::new();
    for (path, r) in &g.files {
        let uses: Vec<Value> = r.uses.iter().map(|u| json!([u.name, u.definer, format!("{:?}", u.basis)])).collect();
        files.insert(path.clone(), json!({
            "exports": r.exports, "imports": r.imports, "reexports": r.reexports, "uses": uses,
            "rank": g.ranks.get(path), "deps": g.dependents.get(path),
        }));
    }
    println!("{}", Value::Object(files));
}
