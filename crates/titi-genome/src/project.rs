use std::collections::HashSet;
use std::time::Duration;

use crate::Genome;

/// Files modified within this window are marked `[RECENT]`. That means
/// recently modified, not newly created.
const NEW_WINDOW: Duration = Duration::from_secs(48 * 3600);

/// Multiplier applied to files the session just edited or read, so the map
/// follows the work instead of the static graph alone.
const TOUCHED_BOOST: f64 = 3.0;

pub fn render(genome: &Genome, limit: usize, touched: &HashSet<String>) -> String {
    let mut ranked: Vec<(&str, f64)> = genome
        .files
        .keys()
        // Angle brackets would forge a closing tag and break the frame.
        .filter(|path| !path.contains(['<', '>']))
        .map(|path| {
            let base = genome.ranks.get(path).copied().unwrap_or(0.0);
            let score = if touched.contains(path) {
                base * TOUCHED_BOOST
            } else {
                base
            };
            (path.as_str(), score)
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });
    ranked.truncate(limit.max(1));

    let mut out = String::from("<genome>\n");
    for (path, _) in ranked {
        let file = &genome.files[path];
        let dependents = genome.dependents.get(&file.path).copied().unwrap_or(0);
        let recent = file
            .mtime
            .elapsed()
            .ok()
            .is_some_and(|elapsed| elapsed <= NEW_WINDOW);
        out.push_str(&file.path);
        out.push_str(":(→");
        out.push_str(&dependents.to_string());
        out.push(')');
        if recent {
            out.push_str(" [RECENT]");
        }
        out.push('\n');
        // `+Name (users)` — how many files lean on that symbol. That is the
        // symbol-level blast radius, next to the file-level `(→N)`.
        let mut exports: Vec<&String> = file.exports.iter().collect();
        exports.sort_by(|a, b| {
            let ua = genome.symbols.get(*a).map_or(0, |s| s.users);
            let ub = genome.symbols.get(*b).map_or(0, |s| s.users);
            ub.cmp(&ua).then_with(|| a.cmp(b))
        });
        for name in exports.into_iter().take(8) {
            let users = genome.symbols.get(name).map_or(0, |s| s.users);
            out.push_str("  +");
            out.push_str(name);
            if users > 0 {
                out.push_str(" (");
                out.push_str(&users.to_string());
                out.push(')');
            }
            out.push('\n');
        }
    }
    out.push_str("</genome>");
    out
}
