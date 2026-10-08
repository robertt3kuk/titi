use std::collections::HashSet;
use std::time::Duration;

use crate::Genome;

/// Files modified within this window are marked `[RECENT]`. That means
/// recently modified, not newly created.
const NEW_WINDOW: Duration = Duration::from_secs(48 * 3600);

/// Symbol lines shown under one file in the prompt map. File ranking is
/// untouched. An export nobody uses is omitted: a zero is not a blast radius.
const PROMPT_EXPORTS: usize = 4;

/// Multiplier applied to files the session just edited or read, so the map
/// follows the work instead of the static graph alone.
const TOUCHED_BOOST: f64 = 3.0;

/// Render the prompt map.
///
/// `pending` is how much work the background indexer has accepted and not yet
/// folded in; `0` means the graph is current and the header is plain
/// `<genome>`. A non-zero count is named in the header — `<genome pending="3">`
/// — because the alternative to naming it is a map the model cannot tell from
/// a current one, and the cheap recovery (re-read the file) is only available
/// to a model that knows.
pub fn render(genome: &Genome, limit: usize, touched: &HashSet<String>, pending: usize) -> String {
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

    let mut out = if pending == 0 {
        String::from("<genome>\n")
    } else {
        format!("<genome pending=\"{pending}\">\n")
    };
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
        // `+Name (users)` — files that lean on that symbol, highest first.
        // Unused exports are omitted, and at most four are shown.
        let mut ranked_exports: Vec<(&String, usize)> = file
            .exports
            .iter()
            .filter_map(|name| {
                let users = genome.symbols.get(name).map_or(0, |s| s.users);
                (users > 0).then_some((name, users))
            })
            .collect();
        ranked_exports.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        for (name, users) in ranked_exports.into_iter().take(PROMPT_EXPORTS) {
            out.push_str("  +");
            out.push_str(name);
            out.push_str(" (");
            out.push_str(&users.to_string());
            out.push(')');
            out.push('\n');
        }
    }
    out.push_str("</genome>");
    out
}
