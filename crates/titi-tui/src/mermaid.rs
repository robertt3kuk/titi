//! Mermaid `flowchart`/`graph` fences → box-drawing rows.
//!
//! The first slice of mermaid support. [`draw`] reads a `flowchart TD|TB|LR` /
//! `graph TD|TB|LR` fence and returns the rows of a box-and-run drawing;
//! everything outside this subset — another diagram type, a line the parser
//! cannot read, a layout wider than the pane — returns `None`, and the caller
//! keeps painting today's fenced-code box. `None` is therefore *safe*: it is
//! never a half-drawn diagram.
//!
//! # The subset
//!
//! - Header: `flowchart TD`, `flowchart TB`, `graph TD`, `graph TB` (all
//!   top-down) and `flowchart LR` / `graph LR` (left-right). Nothing else on
//!   the first line parses — not `BT`, not `RL`, not a missing direction.
//! - Nodes, referenced by id (`A`, `A[text]`, `A(text)`, `A{text}`,
//!   `A["text"]`, `A['text']`), introduced on an edge line or alone on their
//!   own line. A node with no text is labelled with its id.
//! - Edges `-->`, `---`, `-.->`, `==>`, chainable (`A --> B --> C`), each with
//!   an optional label (`-->|yes|` or `-- yes -->`).
//! - `subgraph …`/`end`, whose own box is dropped while its nodes and edges
//!   are kept.
//! - Ignored: blank lines, `%%` comments, `classDef`/`class`/`linkStyle`/
//!   `style`/`click`/`direction` lines.
//!
//! # Degradations (deliberate, and visible in the drawing)
//!
//! - `A(text)` (rounded) and `A{text}` (diamond) draw as the one box style
//!   this slice has.
//! - `subgraph` draws no box of its own; its nodes and edges simply appear.
//! - `-.->` (dotted) and `==>` (thick) draw as plain runs; the run glyph is
//!   the box set's, which has no dotted or doubled form.
//! - `---` has no arrowhead anywhere it is drawn.
//! - A label with no room beside its run is dropped rather than pushed over
//!   the frame. On a left-right run it is truncated to the gutter that was
//!   reserved for it.
//! - Text is measured one column per character: a fence whose node text or
//!   label carries a double-width glyph is laid out wrong, so `draw` returns
//!   `None` for it and the code-box fallback keeps the fence readable.
//!
//! # Size
//!
//! A fence is model-written text, so the work it can ask for is bounded before
//! any of it happens: at most [`MAX_NODES`] nodes, [`MAX_EDGES`] edges,
//! [`MAX_LAYERS`] layers and [`MAX_ROWS`] drawn rows. A diagram past any of
//! them is one nobody could read in a pane, and it falls back to the code box
//! like every other thing this module declines. The bounds are what keep a
//! hostile or merely enormous fence from turning into a crash or a wall.
//!
//! # Cycles
//!
//! A depth-first pass finds the back edges; they are dropped from the layering
//! DAG, and each one is instead laid out as a run from its source down to a
//! *copy* of its target placed one layer below the source. The drawing is
//! therefore always a DAG whose runs point down (or right), and `A --> B --> A`
//! draws A, then B, then A again — one unroll, not a loop, and never `None`.
//!
//! Not ported: every other diagram type, node styling and colours, curves and
//! self-crossing runs, `&`-joins and `;`-separated statements, link ids, and
//! wide glyphs (see above).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::theme::{Theme, ThemeColor};
use crate::width::visible_width;

/// Which way the flow runs: down the layers, or across them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    TopDown,
    LeftRight,
}

/// Lay the fence's `source` out as box-drawing rows, or `None` for anything
/// this slice cannot draw.
///
/// `Some(rows)` means the fence parsed *and* the drawing fits `width` columns;
/// every returned row is at most `width` visible cells wide. Returns `None` for
/// another diagram type, an unparseable line, an empty diagram, or a layout
/// that cannot fit — the caller falls back to its fenced-code box.
// The markdown fence renderer is wired to this in the next slice; a plain
// build has no caller for it yet.
pub(crate) fn draw(source: &str, width: usize, theme: &Theme) -> Option<Vec<String>> {
    if width == 0 {
        return None;
    }
    let graph = parse(source)?;
    if graph.nodes.is_empty() {
        return None;
    }
    if graph.nodes.len() > MAX_NODES || graph.edges.len() > MAX_EDGES {
        return None;
    }
    let plan = layout(&graph, width)?;
    if plan.layers.len() > MAX_LAYERS {
        return None;
    }
    let rows = paint(&plan, width, theme)?;
    if rows.len() > MAX_ROWS {
        return None;
    }
    if rows.iter().any(|row| visible_width(row) > width) {
        return None;
    }
    Some(rows)
}

/// The most a fence may ask this module to draw. Past any of these the diagram
/// could not be read in a pane anyway, and the code box is the honest answer —
/// see the module's `# Size`. The numbers are generous for a transcript: a
/// flowchart a person writes is a handful of nodes.
const MAX_NODES: usize = 200;
const MAX_EDGES: usize = 400;
const MAX_LAYERS: usize = 200;
const MAX_ROWS: usize = 200;

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// A node, by the id edges reference it with and the text it draws.
struct Node {
    id: String,
    text: String,
}

/// A parsed edge. `kind` decides whether it draws an arrowhead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeKind {
    Arrow,
    Line,
    Dotted,
    Thick,
}

impl EdgeKind {
    /// `---` is the one edge kind with no head; the rest draw one.
    fn has_head(self) -> bool {
        !matches!(self, EdgeKind::Line)
    }
}

struct Edge {
    from: usize,
    to: usize,
    kind: EdgeKind,
    label: Option<String>,
}

struct Graph {
    direction: Direction,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

fn parse(source: &str) -> Option<Graph> {
    let mut lines = source.lines();
    let direction = header(lines.next()?)?;
    let mut nodes: Vec<Node> = Vec::new();
    let mut ids: BTreeMap<String, usize> = BTreeMap::new();
    let mut edges: Vec<Edge> = Vec::new();
    for raw in lines {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") || ignored(line) {
            continue;
        }
        if line == "end" {
            continue;
        }
        if keyword(line, "subgraph") {
            continue;
        }
        statement(line, &mut nodes, &mut ids, &mut edges)?;
    }
    Some(Graph {
        direction,
        nodes,
        edges,
    })
}

/// The direction from the fence's first line, or `None` for anything else.
fn header(line: &str) -> Option<Direction> {
    let mut words = line.split_whitespace();
    let kind = words.next()?;
    let dir = words.next()?;
    if words.next().is_some() {
        return None;
    }
    match (kind, dir) {
        ("flowchart" | "graph", "TD" | "TB") => Some(Direction::TopDown),
        ("flowchart" | "graph", "LR") => Some(Direction::LeftRight),
        _ => None,
    }
}

fn first_token(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or("")
}

/// Lines the subset drops entirely: styling, links and orientation hints.
fn ignored(line: &str) -> bool {
    matches!(
        first_token(line),
        "classDef" | "class" | "linkStyle" | "style" | "click" | "direction"
    )
}

/// `line` starts with the whole word `kw` (so `subgraphx` is an id, not a
/// subgraph).
fn keyword(line: &str, kw: &str) -> bool {
    line.starts_with(kw)
        && (line.len() == kw.len() || line.as_bytes()[kw.len()].is_ascii_whitespace())
}

fn skip_ws(s: &str, p: &mut usize) {
    while *p < s.len() && s.as_bytes()[*p].is_ascii_whitespace() {
        *p += 1;
    }
}

/// `node [_node]*`: the id, then an optional shape/text, then edges.
fn statement(
    s: &str,
    nodes: &mut Vec<Node>,
    ids: &mut BTreeMap<String, usize>,
    edges: &mut Vec<Edge>,
) -> Option<()> {
    let mut p = 0usize;
    let (id, text) = node(s, &mut p)?;
    let mut from = intern(nodes, ids, id, text);
    loop {
        skip_ws(s, &mut p);
        if p >= s.len() {
            return Some(());
        }
        let (kind, label, next) = edge(s, p)?;
        p = next;
        skip_ws(s, &mut p);
        let (id, text) = node(s, &mut p)?;
        let to = intern(nodes, ids, id, text);
        edges.push(Edge {
            from,
            to,
            kind,
            label,
        });
        from = to;
    }
}

/// The id at `p` and its shape text (empty when the node is bare).
fn node(s: &str, p: &mut usize) -> Option<(String, String)> {
    let id = scan_id(s, p);
    if id.is_empty() {
        return None;
    }
    let text = match s[*p..].chars().next() {
        Some('[') => balanced(s, p, '[', ']')?,
        Some('(') => balanced(s, p, '(', ')')?,
        Some('{') => balanced(s, p, '{', '}')?,
        _ => String::new(),
    };
    Some((id, unquote(&text)))
}

/// An id is letters, digits, `_`, `.` and `-` — except a `-` that starts an
/// edge, so `A-->B` reads as node, edge, node.
fn scan_id(s: &str, p: &mut usize) -> String {
    let start = *p;
    while *p < s.len() {
        let rest = &s[*p..];
        // `*p < s.len()` and every advance is a whole character, so there is
        // one; the `else` is the guard spelled where a reader can see it.
        let Some(c) = rest.chars().next() else { break };
        let edge_dash = c == '-' && (rest.starts_with("--") || rest.starts_with("-."));
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' || (c == '-' && !edge_dash) {
            *p += c.len_utf8();
        } else {
            break;
        }
    }
    s[start..*p].to_owned()
}

/// The text inside the shape's brackets, up to the matching close.
fn balanced(s: &str, p: &mut usize, open: char, close: char) -> Option<String> {
    let start = *p;
    let mut depth = 0usize;
    let mut out = String::new();
    for (i, c) in s[start..].char_indices() {
        if c == open {
            depth += 1;
            if depth == 1 {
                continue;
            }
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                *p = start + i + c.len_utf8();
                return Some(out);
            }
        }
        out.push(c);
    }
    None
}

/// Strip one matching quote pair, when both ends carry one.
fn unquote(s: &str) -> String {
    let t = s.trim();
    let (Some(first), Some(last)) = (t.chars().next(), t.chars().next_back()) else {
        return t.to_owned();
    };
    if (first == '"' || first == '\'') && first == last && t.len() >= 2 {
        return t[first.len_utf8()..t.len() - last.len_utf8()].to_owned();
    }
    t.to_owned()
}

/// The edge operator at `p`, its label (mid-text or `|pipes|`), and where the
/// next node starts.
fn edge(s: &str, p: usize) -> Option<(EdgeKind, Option<String>, usize)> {
    let rest = &s[p..];
    let (kind, label, mut q) = if rest.starts_with("==>") {
        (EdgeKind::Thick, None, p + 3)
    } else if rest.starts_with("-.->") {
        (EdgeKind::Dotted, None, p + 4)
    } else if rest.starts_with("-->") {
        (EdgeKind::Arrow, None, p + 3)
    } else if rest.starts_with("---") {
        (EdgeKind::Line, None, p + 3)
    } else if let Some(tail) = rest.strip_prefix("==") {
        let at = tail.find("==>")?;
        (EdgeKind::Thick, Some(tail[..at].to_owned()), p + 2 + at + 3)
    } else if let Some(tail) = rest.strip_prefix("-.") {
        let at = tail.find(".->")?;
        (
            EdgeKind::Dotted,
            Some(tail[..at].to_owned()),
            p + 2 + at + 3,
        )
    } else if let Some(tail) = rest.strip_prefix("--") {
        let at = tail.find("-->")?;
        (EdgeKind::Arrow, Some(tail[..at].to_owned()), p + 2 + at + 3)
    } else {
        None?
    };
    skip_ws(s, &mut q);
    if let Some(tail) = s[q..].strip_prefix('|') {
        let at = tail.find('|')?;
        return Some((kind, Some(tail[..at].to_owned()), q + 1 + at + 1));
    }
    let label = label.map(|l| l.trim().to_owned()).filter(|l| !l.is_empty());
    Some((kind, label, q))
}

/// The node's index, adding it on first sight and keeping the first text it
/// was given (`A` then `A[Start]` keeps `Start`).
fn intern(
    nodes: &mut Vec<Node>,
    ids: &mut BTreeMap<String, usize>,
    id: String,
    text: String,
) -> usize {
    if let Some(&i) = ids.get(&id) {
        if nodes[i].text.is_empty() {
            nodes[i].text = text;
        }
        return i;
    }
    let i = nodes.len();
    ids.insert(id.clone(), i);
    nodes.push(Node { id, text });
    i
}

// ---------------------------------------------------------------------------
// Layering
// ---------------------------------------------------------------------------

/// One laid-out node: a box with text, or a one-cell pass-through inserted so
/// a run that skips a layer becomes a chain of layer-to-layer runs.
enum LNode {
    Box(String),
    Ghost,
}

impl LNode {
    fn text(&self) -> &str {
        match self {
            LNode::Box(text) => text,
            LNode::Ghost => "",
        }
    }
}

/// A run between two adjacent layers, as the layout sees it.
struct Hop {
    from: usize,
    to: usize,
    label: Option<String>,
    head: bool,
}

fn back_edges(n: usize, hops: &[Hop]) -> BTreeSet<(usize, usize)> {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for h in hops {
        adj[h.from].push(h.to);
    }
    let mut colour = vec![0u8; n];
    let mut back = BTreeSet::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for seed in 0..n {
        if colour[seed] != 0 {
            continue;
        }
        colour[seed] = 1;
        stack.push((seed, 0));
        while let Some(&(u, i)) = stack.last() {
            if i < adj[u].len() {
                if let Some(top) = stack.last_mut() {
                    top.1 += 1;
                }
                let v = adj[u][i];
                match colour[v] {
                    0 => {
                        colour[v] = 1;
                        stack.push((v, 0));
                    }
                    1 => {
                        back.insert((u, v));
                    }
                    _ => {}
                }
            } else {
                colour[u] = 2;
                stack.pop();
            }
        }
    }
    back
}

/// Longest path from a source, over the edges that are not back edges.
fn longest_path(n: usize, hops: &[Hop], back: &BTreeSet<(usize, usize)>) -> Vec<usize> {
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut indegree = vec![0usize; n];
    for h in hops {
        if back.contains(&(h.from, h.to)) {
            continue;
        }
        adj[h.from].push(h.to);
        indegree[h.to] += 1;
    }
    let mut layer = vec![0usize; n];
    let mut queue: VecDeque<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
    while let Some(u) = queue.pop_front() {
        for &v in &adj[u] {
            layer[v] = layer[v].max(layer[u] + 1);
            indegree[v] -= 1;
            if indegree[v] == 0 {
                queue.push_back(v);
            }
        }
    }
    layer
}

/// The mean position of `node`'s neighbours that sit in layer `want`, falling
/// back to the node's own position when it has none there.
fn mean(at: &[usize], neighbours: &[usize], layer: &[usize], want: usize, own: usize) -> f64 {
    let mut sum = 0.0f64;
    let mut count = 0usize;
    for &other in neighbours {
        if layer[other] == want {
            sum += at[other] as f64;
            count += 1;
        }
    }
    if count > 0 {
        sum / count as f64
    } else {
        own as f64
    }
}

/// One barycentric pass per layer, down then up: order by the mean position of
/// the neighbours in the adjacent layer, ties by first-seen order.
fn order(layers: &mut [Vec<usize>], hops: &[Hop], layer: &[usize], n: usize) {
    let mut at = vec![0usize; n];
    for nodes in layers.iter() {
        for (i, &node) in nodes.iter().enumerate() {
            at[node] = i;
        }
    }
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); n];
    for h in hops {
        preds[h.to].push(h.from);
        succs[h.from].push(h.to);
    }
    // The loop writes the layer it indexes, so it cannot walk the slice as
    // items; the index is the point.
    #[allow(clippy::needless_range_loop)]
    for li in 1..layers.len() {
        let mut keyed: Vec<(f64, usize)> = layers[li]
            .iter()
            .map(|&node| (mean(&at, &preds[node], layer, li - 1, at[node]), node))
            .collect();
        keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let ordered: Vec<usize> = keyed.into_iter().map(|(_, node)| node).collect();
        layers[li] = ordered;
        for (i, &node) in layers[li].iter().enumerate() {
            at[node] = i;
        }
    }
    for li in (0..layers.len().saturating_sub(1)).rev() {
        let mut keyed: Vec<(f64, usize)> = layers[li]
            .iter()
            .map(|&node| (mean(&at, &succs[node], layer, li + 1, at[node]), node))
            .collect();
        keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        let ordered: Vec<usize> = keyed.into_iter().map(|(_, node)| node).collect();
        layers[li] = ordered;
        for (i, &node) in layers[li].iter().enumerate() {
            at[node] = i;
        }
    }
}

/// Cells between two boxes of one top-down layer.
const GAP: usize = 3;

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// The laid-out drawing: what goes where, before any glyph or colour.
struct Plan {
    direction: Direction,
    nodes: Vec<LNode>,
    hops: Vec<Hop>,
    /// Nodes per layer, in drawing order.
    layers: Vec<Vec<usize>>,
    /// The layer each node sits in.
    layer: Vec<usize>,
    /// Top-down: the node's left column. Left-right: its first row inside the
    /// band, already centred against the tallest band.
    pos: Vec<usize>,
    /// Top-down: the layer band's first row. Left-right: its first column.
    band: Vec<usize>,
    /// Left-right only: each band's uniform width.
    band_width: Vec<usize>,
    /// Per gap: the columns reserved for labels, left-right only.
    label_width: Vec<usize>,
    width: usize,
    height: usize,
}

fn box_width(text: &str) -> usize {
    text.chars().count() + 4
}

/// A node's extent across the run axis (columns for top-down, rows for
/// left-right).
fn extent(node: &LNode) -> usize {
    match node {
        LNode::Box(text) => box_width(text),
        LNode::Ghost => 1,
    }
}

fn slot_height(node: &LNode) -> usize {
    match node {
        LNode::Box(_) => 3,
        LNode::Ghost => 1,
    }
}

/// Where a run leaves a node in a top-down drawing.
fn centre_x(nodes: &[LNode], pos: &[usize], node: usize) -> usize {
    pos[node] + extent(&nodes[node]) / 2
}

/// Where a run leaves a node in a left-right drawing.
fn centre_y(nodes: &[LNode], pos: &[usize], node: usize) -> usize {
    pos[node] + (slot_height(&nodes[node]) - 1) / 2
}

fn layout(graph: &Graph, width: usize) -> Option<Plan> {
    let n = graph.nodes.len();
    if n == 0 {
        return None;
    }
    let mut nodes: Vec<LNode> = graph
        .nodes
        .iter()
        .map(|nd| {
            LNode::Box(if nd.text.is_empty() {
                nd.id.clone()
            } else {
                nd.text.clone()
            })
        })
        .collect();
    if nodes.iter().any(|nd| !narrow(nd.text())) {
        return None;
    }
    if graph
        .edges
        .iter()
        .any(|e| e.label.as_deref().is_some_and(|l| !narrow(l)))
    {
        return None;
    }

    let mut hops: Vec<Hop> = graph
        .edges
        .iter()
        .map(|e| Hop {
            from: e.from,
            to: e.to,
            label: e.label.clone(),
            head: e.kind.has_head(),
        })
        .collect();

    // Layering, then one copy of the target per back edge, one layer below its
    // source: the cycle is unrolled once and every run points down.
    let back = back_edges(n, &hops);
    let mut layer = longest_path(n, &hops, &back);
    let mut unrolled: Vec<Hop> = Vec::with_capacity(hops.len());
    for h in hops.drain(..) {
        if !back.contains(&(h.from, h.to)) {
            unrolled.push(h);
            continue;
        }
        let text = nodes[h.to].text().to_owned();
        let copy = nodes.len();
        nodes.push(LNode::Box(text));
        let below = layer[h.from] + 1;
        layer.push(below);
        unrolled.push(Hop {
            from: h.from,
            to: copy,
            label: h.label,
            head: h.head,
        });
    }

    // A run that skips a layer gets a ghost per layer it crosses, so every run
    // is layer-to-layer and the label sits on the hop into the target.
    let mut hops: Vec<Hop> = Vec::with_capacity(unrolled.len());
    for h in unrolled {
        let from = layer[h.from];
        let to = layer[h.to];
        if to <= from {
            return None;
        }
        if to == from + 1 {
            hops.push(h);
            continue;
        }
        let mut prev = h.from;
        for at in (from + 1)..to {
            let ghost = nodes.len();
            nodes.push(LNode::Ghost);
            layer.push(at);
            hops.push(Hop {
                from: prev,
                to: ghost,
                label: None,
                head: false,
            });
            prev = ghost;
        }
        hops.push(Hop {
            from: prev,
            to: h.to,
            label: h.label,
            head: h.head,
        });
    }

    let count = nodes.len();
    let depth = layer.iter().copied().max().unwrap_or(0) + 1;
    let mut layers = vec![Vec::new(); depth];
    for (i, &at) in layer.iter().enumerate() {
        layers[at].push(i);
    }
    order(&mut layers, &hops, &layer, count);

    if graph.direction == Direction::TopDown {
        let mut pos = vec![0usize; count];
        let mut total = 0usize;
        for (li, band) in layers.iter().enumerate() {
            let mut cursor: Option<usize> = None;
            for &node in band {
                let w = extent(&nodes[node]);
                let mut x = 0usize;
                if li > 0 {
                    let mut sum = 0usize;
                    let mut count = 0usize;
                    for h in &hops {
                        if h.to == node && layer[h.from] == li - 1 {
                            sum += centre_x(&nodes, &pos, h.from);
                            count += 1;
                        }
                    }
                    x = sum
                        .checked_div(count)
                        .map(|mean| mean.saturating_sub(w / 2))
                        .unwrap_or(x);
                }
                if let Some(at) = cursor {
                    x = x.max(at);
                }
                pos[node] = x;
                cursor = Some(x + w + GAP);
                total = total.max(x + w);
            }
        }
        if total > width {
            return None;
        }
        let band: Vec<usize> = (0..layers.len()).map(|li| li * 5).collect();
        let height = (layers.len() - 1) * 5 + 3;
        return Some(Plan {
            direction: Direction::TopDown,
            nodes,
            hops,
            layers,
            layer,
            pos,
            band,
            band_width: Vec::new(),
            label_width: Vec::new(),
            width: total,
            height,
        });
    }

    let mut pos = vec![0usize; count];
    let mut stack_heights = vec![0usize; layers.len()];
    for (li, band) in layers.iter().enumerate() {
        let mut y = 0usize;
        for &node in band {
            let slot = slot_height(&nodes[node]);
            let mut top = 0usize;
            if li > 0 {
                let mut sum = 0usize;
                let mut count = 0usize;
                for h in &hops {
                    if h.to == node && layer[h.from] == li - 1 {
                        sum += centre_y(&nodes, &pos, h.from);
                        count += 1;
                    }
                }
                top = sum
                    .checked_div(count)
                    .map(|mean| mean.saturating_sub((slot - 1) / 2))
                    .unwrap_or(top);
            }
            top = top.max(y);
            pos[node] = top;
            y = top + slot + 1;
        }
        stack_heights[li] = y.saturating_sub(1);
    }
    let height = stack_heights.iter().copied().max().unwrap_or(0);
    for (li, band) in layers.iter().enumerate() {
        let shift = (height - stack_heights[li]) / 2;
        for &node in band {
            pos[node] += shift;
        }
    }

    let mut label_width = Vec::with_capacity(layers.len().saturating_sub(1));
    for li in 0..layers.len().saturating_sub(1) {
        label_width.push(
            hops.iter()
                .filter(|h| layer[h.from] == li && layer[h.to] == li + 1)
                .filter_map(|h| h.label.as_deref())
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(0),
        );
    }
    let mut band = Vec::with_capacity(layers.len());
    let mut band_width = Vec::with_capacity(layers.len());
    let mut total = 0usize;
    for (li, layer_nodes) in layers.iter().enumerate() {
        let bw = layer_nodes
            .iter()
            .map(|&node| match &nodes[node] {
                LNode::Box(text) => box_width(text),
                LNode::Ghost => GAP,
            })
            .max()
            .unwrap_or(GAP)
            .max(GAP);
        band.push(total);
        band_width.push(bw);
        total += bw;
        if li < label_width.len() {
            total += gap_width(label_width[li]);
        }
    }
    if total > width {
        return None;
    }
    Some(Plan {
        direction: Direction::LeftRight,
        nodes,
        hops,
        layers,
        layer,
        pos,
        band,
        band_width,
        label_width,
        width: total,
        height,
    })
}

/// Columns between two left-right bands: the trunk, the head, and room for the
/// gap's labels when it has any.
fn gap_width(labels: usize) -> usize {
    if labels == 0 { GAP } else { labels + 4 }
}

/// One column per character: a double-width glyph would break the box maths.
fn narrow(s: &str) -> bool {
    visible_width(s) == s.chars().count()
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

const LEFT: u8 = 1;
const RIGHT: u8 = 2;
const UP: u8 = 4;
const DOWN: u8 = 8;
/// A terminating arrowhead.
const HEAD_DOWN: u8 = 16;
const HEAD_RIGHT: u8 = 32;

/// The box set, as the theme has it (ASCII `+ - |` when it has no glyph).
struct Glyphs {
    horizontal: String,
    vertical: String,
    top_left: String,
    top_right: String,
    bottom_left: String,
    bottom_right: String,
    tee_down: String,
    tee_up: String,
    tee_right: String,
    tee_left: String,
    cross: String,
    head_down: String,
    head_right: String,
}

impl Glyphs {
    fn from_theme(theme: &Theme) -> Glyphs {
        let pick = |key: &str, ascii: &str| {
            let glyph = theme.symbol(key);
            if glyph.is_empty() {
                ascii.to_owned()
            } else {
                glyph.to_owned()
            }
        };
        let vertical = pick("boxSharp.vertical", "|");
        let ascii = vertical == "|";
        Glyphs {
            horizontal: pick("boxSharp.horizontal", "-"),
            vertical,
            top_left: pick("boxSharp.topLeft", "+"),
            top_right: pick("boxSharp.topRight", "+"),
            bottom_left: pick("boxSharp.bottomLeft", "+"),
            bottom_right: pick("boxSharp.bottomRight", "+"),
            tee_down: pick("boxSharp.teeDown", "+"),
            tee_up: pick("boxSharp.teeUp", "+"),
            tee_right: pick("boxSharp.teeRight", "+"),
            tee_left: pick("boxSharp.teeLeft", "+"),
            cross: pick("boxSharp.cross", "+"),
            head_down: if ascii {
                "v".to_owned()
            } else {
                "▼".to_owned()
            },
            head_right: if ascii {
                ">".to_owned()
            } else {
                "▶".to_owned()
            },
        }
    }

    /// The glyph for a cell's line flags.
    fn glyph(&self, flags: u8) -> &str {
        if flags & HEAD_DOWN != 0 {
            return &self.head_down;
        }
        if flags & HEAD_RIGHT != 0 {
            return &self.head_right;
        }
        let l = flags & LEFT != 0;
        let r = flags & RIGHT != 0;
        let u = flags & UP != 0;
        let d = flags & DOWN != 0;
        match (l, r, u, d) {
            (true, true, true, true) => &self.cross,
            (true, true, true, false) => &self.tee_up,
            (true, true, false, true) => &self.tee_down,
            (false, true, true, true) => &self.tee_right,
            (true, false, true, true) => &self.tee_left,
            (true, true, false, false) => &self.horizontal,
            (false, false, true, true) => &self.vertical,
            (false, true, false, true) => &self.top_left,
            (true, false, false, true) => &self.top_right,
            (false, true, true, false) => &self.bottom_left,
            (true, false, true, false) => &self.bottom_right,
            (true, false, false, false) | (false, true, false, false) => &self.horizontal,
            (false, false, true, false) | (false, false, false, true) => &self.vertical,
            (false, false, false, false) => " ",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cell {
    Empty,
    Ink(char),
    Line(u8),
}

struct Canvas {
    w: usize,
    h: usize,
    cells: Vec<Cell>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Canvas {
        Canvas {
            w,
            h,
            cells: vec![Cell::Empty; w * h],
        }
    }

    /// Add line flags to a cell, merging them with whatever is already drawn.
    /// Ink always wins: runs are never routed over text.
    fn line(&mut self, x: usize, y: usize, flags: u8) {
        if x >= self.w || y >= self.h {
            return;
        }
        let i = y * self.w + x;
        match self.cells[i] {
            Cell::Empty => self.cells[i] = Cell::Line(flags),
            Cell::Line(held) => self.cells[i] = Cell::Line(held | flags),
            Cell::Ink(_) => {}
        }
    }

    /// Write a character if the cell is still free. False when it is not, so a
    /// label stops at the first thing in its way.
    fn ink(&mut self, x: usize, y: usize, c: char) -> bool {
        if x >= self.w || y >= self.h {
            return false;
        }
        let i = y * self.w + x;
        if self.cells[i] == Cell::Empty {
            self.cells[i] = Cell::Ink(c);
            return true;
        }
        false
    }

    fn render(&self, theme: &Theme, glyphs: &Glyphs) -> Vec<String> {
        let mut rows = Vec::with_capacity(self.h);
        for y in 0..self.h {
            let mut last = 0usize;
            for x in 0..self.w {
                if self.cells[y * self.w + x] != Cell::Empty {
                    last = x + 1;
                }
            }
            let mut out = String::new();
            let mut x = 0usize;
            while x < last {
                let i = y * self.w + x;
                match self.cells[i] {
                    Cell::Empty => {
                        let from = x;
                        while x < last && self.cells[y * self.w + x] == Cell::Empty {
                            x += 1;
                        }
                        out.push_str(&" ".repeat(x - from));
                    }
                    Cell::Ink(_) => {
                        let from = x;
                        while x < last && matches!(self.cells[y * self.w + x], Cell::Ink(_)) {
                            x += 1;
                        }
                        let text: String = (from..x)
                            .map(|x| match self.cells[y * self.w + x] {
                                Cell::Ink(c) => c,
                                _ => ' ',
                            })
                            .collect();
                        out.push_str(&theme.fg(ThemeColor::MdCodeBlock, &text));
                    }
                    Cell::Line(_) => {
                        let from = x;
                        while x < last && matches!(self.cells[y * self.w + x], Cell::Line(_)) {
                            x += 1;
                        }
                        let text: String = (from..x)
                            .map(|x| match self.cells[y * self.w + x] {
                                Cell::Line(flags) => glyphs.glyph(flags),
                                _ => " ",
                            })
                            .collect();
                        out.push_str(&theme.fg(ThemeColor::MdCodeBlockBorder, &text));
                    }
                }
            }
            rows.push(out);
        }
        rows
    }
}

fn paint(plan: &Plan, width: usize, theme: &Theme) -> Option<Vec<String>> {
    let glyphs = Glyphs::from_theme(theme);
    let extra = plan
        .hops
        .iter()
        .filter_map(|h| h.label.as_deref())
        .map(|l| l.chars().count() + 2)
        .max()
        .unwrap_or(0);
    let canvas_width = match plan.direction {
        Direction::TopDown => (plan.width + extra).min(width),
        Direction::LeftRight => plan.width,
    }
    .max(plan.width);
    let mut canvas = Canvas::new(canvas_width, plan.height);
    match plan.direction {
        Direction::TopDown => {
            boxes_down(plan, &mut canvas);
            runs_down(plan, &mut canvas);
        }
        Direction::LeftRight => {
            boxes_across(plan, &mut canvas);
            runs_across(plan, &mut canvas);
        }
    }
    Some(canvas.render(theme, &glyphs))
}

/// The boxes and ghost pass-throughs of a top-down drawing.
fn boxes_down(plan: &Plan, canvas: &mut Canvas) {
    for (li, band) in plan.layers.iter().enumerate() {
        let top = plan.band[li];
        for &node in band {
            let x = plan.pos[node];
            match &plan.nodes[node] {
                LNode::Box(text) => {
                    let w = box_width(text);
                    canvas.line(x, top, RIGHT | DOWN);
                    for at in (x + 1)..(x + w - 1) {
                        canvas.line(at, top, LEFT | RIGHT);
                    }
                    canvas.line(x + w - 1, top, LEFT | DOWN);
                    canvas.line(x, top + 1, UP | DOWN);
                    for (i, c) in text.chars().enumerate() {
                        canvas.ink(x + 2 + i, top + 1, c);
                    }
                    canvas.line(x + w - 1, top + 1, UP | DOWN);
                    canvas.line(x, top + 2, RIGHT | UP);
                    for at in (x + 1)..(x + w - 1) {
                        canvas.line(at, top + 2, LEFT | RIGHT);
                    }
                    canvas.line(x + w - 1, top + 2, LEFT | UP);
                }
                LNode::Ghost => {
                    for row in top..(top + 3) {
                        canvas.line(x, row, UP | DOWN);
                    }
                }
            }
        }
    }
}

/// The runs between two top-down layers: a trunk row that turns off the boxes
/// and distributes, then a row of arrowheads (or the plain continuation of a
/// headless run) into the next band.
fn runs_down(plan: &Plan, canvas: &mut Canvas) {
    for li in 0..plan.layers.len().saturating_sub(1) {
        let trunk = plan.band[li] + 3;
        let heads = trunk + 1;
        for h in &plan.hops {
            if plan.layer[h.from] != li {
                continue;
            }
            let from = centre_x(&plan.nodes, &plan.pos, h.from);
            let to = centre_x(&plan.nodes, &plan.pos, h.to);
            canvas.line(from, trunk - 1, DOWN);
            canvas.line(from, trunk, UP);
            canvas.line(to, trunk, DOWN);
            let (lo, hi) = (from.min(to), from.max(to));
            for x in lo..=hi {
                let mut flags = 0;
                if x > lo {
                    flags |= LEFT;
                }
                if x < hi {
                    flags |= RIGHT;
                }
                canvas.line(x, trunk, flags);
            }
            if h.head {
                canvas.line(to, heads, HEAD_DOWN);
            } else {
                canvas.line(to, heads, UP | DOWN);
                canvas.line(to, heads + 1, UP);
            }
        }
        for h in &plan.hops {
            if plan.layer[h.from] != li {
                continue;
            }
            let Some(label) = h.label.as_deref() else {
                continue;
            };
            let to = centre_x(&plan.nodes, &plan.pos, h.to);
            write_label(canvas, to + 2, heads, label, usize::MAX);
        }
    }
}

/// The boxes and ghost pass-throughs of a left-right drawing.
fn boxes_across(plan: &Plan, canvas: &mut Canvas) {
    for (li, band) in plan.layers.iter().enumerate() {
        let left = plan.band[li];
        let w = plan.band_width[li];
        for &node in band {
            let y = plan.pos[node];
            match &plan.nodes[node] {
                LNode::Box(text) => {
                    canvas.line(left, y, RIGHT | DOWN);
                    for at in (left + 1)..(left + w - 1) {
                        canvas.line(at, y, LEFT | RIGHT);
                    }
                    canvas.line(left + w - 1, y, LEFT | DOWN);
                    canvas.line(left, y + 1, UP | DOWN);
                    for (i, c) in text.chars().enumerate() {
                        canvas.ink(left + 2 + i, y + 1, c);
                    }
                    canvas.line(left + w - 1, y + 1, UP | DOWN);
                    canvas.line(left, y + 2, RIGHT | UP);
                    for at in (left + 1)..(left + w - 1) {
                        canvas.line(at, y + 2, LEFT | RIGHT);
                    }
                    canvas.line(left + w - 1, y + 2, LEFT | UP);
                }
                LNode::Ghost => {
                    for x in left..(left + w) {
                        canvas.line(x, y, LEFT | RIGHT);
                    }
                }
            }
        }
    }
}

/// The runs between two left-right bands: a trunk column that distributes,
/// then a column of arrowheads into the next band.
fn runs_across(plan: &Plan, canvas: &mut Canvas) {
    for li in 0..plan.layers.len().saturating_sub(1) {
        let trunk = plan.band[li] + plan.band_width[li];
        let heads = trunk + 1;
        let label_width = plan.label_width.get(li).copied().unwrap_or(0);
        for h in &plan.hops {
            if plan.layer[h.from] != li {
                continue;
            }
            let from = centre_y(&plan.nodes, &plan.pos, h.from);
            let to = centre_y(&plan.nodes, &plan.pos, h.to);
            canvas.line(trunk - 1, from, RIGHT);
            canvas.line(trunk, from, LEFT);
            canvas.line(trunk, to, RIGHT);
            let (lo, hi) = (from.min(to), from.max(to));
            for y in lo..=hi {
                let mut flags = 0;
                if y > lo {
                    flags |= UP;
                }
                if y < hi {
                    flags |= DOWN;
                }
                canvas.line(trunk, y, flags);
            }
            if h.head {
                canvas.line(heads, to, HEAD_RIGHT);
            } else {
                canvas.line(heads, to, LEFT | RIGHT);
                canvas.line(plan.band[li + 1], to, LEFT);
            }
        }
        for h in &plan.hops {
            if plan.layer[h.from] != li {
                continue;
            }
            let Some(label) = h.label.as_deref() else {
                continue;
            };
            let to = centre_y(&plan.nodes, &plan.pos, h.to);
            if write_label(canvas, heads + 2, to, label, label_width).is_none() {
                continue;
            }
        }
    }
}

/// Write `label` after a one-cell gap at `x`, stopping at `room` characters or
/// at the first cell that is taken. `None` when even the gap is taken.
fn write_label(canvas: &mut Canvas, x: usize, y: usize, label: &str, room: usize) -> Option<()> {
    if !canvas.ink(x - 1, y, ' ') {
        return None;
    }
    for (offset, c) in label.chars().take(room).enumerate() {
        if !canvas.ink(x + offset, y, c) {
            break;
        }
    }
    Some(())
}

#[cfg(test)]
mod hostile {
    use super::*;

    /// A fence is model-written text, so this drives `draw` with the shapes
    /// that make a hand-written parser fall over: an empty label, an unmatched
    /// `|`, a bare `-->`, a five-hundred-node chain, an id reused with two
    /// shapes, unicode in an id, CRLF, tabs, an extremely long label, and
    /// widths one, two and three. Every case either draws inside its width or
    /// declines — and none of them panics, which is the point of the test.
    #[test]
    fn hostile_fences_draw_or_decline_but_never_panic() {
        let mut cases: Vec<String> = vec![
            // The shapes the parser can be surprised by.
            "flowchart TD\nA[] --> B\n".to_owned(),
            "flowchart TD\nA[|] --> B\n".to_owned(),
            "flowchart TD\nA --> |unclosed B\n".to_owned(),
            "flowchart TD\nA -->|yes B\n".to_owned(),
            "flowchart TD\n-->\n".to_owned(),
            "flowchart TD\nA -->\n".to_owned(),
            "flowchart TD\nA[one] --> B[two]\nA{three} --> B(four)\n".to_owned(),
            "flowchart TD\n\u{4f60}\u{597d}[\u{4e16}\u{754c}] --> \u{1f389}\n".to_owned(),
            "flowchart TD\r\nA[crlf] --> B\r\n".to_owned(),
            "flowchart TD\n\tA[tab] -->\tB\n".to_owned(),
            "flowchart TD\nA --> B\n".to_owned(),
            "flowchart TD\nA[x] --> A[y]\n".to_owned(),
            "flowchart TD\nA --> B\nB --> A\nA --> B\n".to_owned(),
            "flowchart LR\nA --> B --> C --> D\n".to_owned(),
            "flowchart TD\n".to_owned(),
            "flowchart TD\nsubgraph s\nA --> B\nend\n".to_owned(),
            "flowchart TD\nsubgraph s\nsubgraph t\nA --> B\nend\nend\n".to_owned(),
            "flowchart TD\n%% comment\nA --> B\n".to_owned(),
            "graph TB\nA --> B\n".to_owned(),
            "sequenceDiagram\nA->>B: hi\n".to_owned(),
            "\u{0}\u{1}\u{2}".to_owned(),
        ];
        // An extremely long label, and the chain the cap exists for.
        cases.push(format!("flowchart TD\nA[{}] --> B\n", "x".repeat(4000)));
        let mut chain = String::from("flowchart TD\nA0");
        for n in 1..500 {
            chain.push_str(&format!(" --> A{n}"));
        }
        chain.push('\n');
        cases.push(chain);

        for source in &cases {
            for width in [1usize, 2, 3, 8, 40, 200] {
                let drawn = draw(source, width, &theme());
                if let Some(rows) = drawn {
                    assert!(!rows.is_empty(), "an empty drawing: {source:?}");
                    for row in &rows {
                        assert!(
                            visible_width(row) <= width,
                            "row wider than {width} for {source:?}: {row:?}"
                        );
                    }
                }
            }
        }
    }

    /// The size cap, named: past [`MAX_NODES`] nodes or [`MAX_ROWS`] rows the
    /// fence falls back to code rather than drawing a wall.
    #[test]
    fn a_fence_past_the_cap_declines() {
        let chain = |nodes: usize| {
            let mut source = String::from("flowchart TD\nA0");
            for n in 1..nodes {
                source.push_str(&format!(" --> A{n}"));
            }
            source.push('\n');
            source
        };
        // Inside the cap: a drawing (at a width that can hold a small chain).
        // A chain of MAX_NODES is a layer per node, which is past the row cap
        // even before the node cap; either way it declines rather than drawing
        // a wall.
        assert!(
            draw(&chain(MAX_NODES), 40, &theme()).is_none(),
            "a chain of {MAX_NODES} nodes declines"
        );
        // Past the node cap: nothing.
        assert!(
            draw(&chain(MAX_NODES + 1), 200, &theme()).is_none(),
            "past {MAX_NODES} nodes"
        );
        // A wide fan-out is a drawing while it fits, and declines when it does
        // not; either way it never panics.
        let fan = |n: usize| {
            let mut source = String::from("flowchart TD\nR --> A0");
            for i in 1..n {
                source.push_str(&format!("\nR --> A{i}"));
            }
            source.push('\n');
            source
        };
        assert!(draw(&fan(3), 40, &theme()).is_some(), "a small fan draws");
        assert!(
            draw(&fan(MAX_NODES + 1), 400, &theme()).is_none(),
            "a huge fan declines"
        );
    }

    /// The module's own theme, so a hostile fence is drawn on the palette the
    /// real tests use rather than a second one.
    fn theme() -> Theme {
        Theme::new(
            "test".to_owned(),
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            crate::theme::ColorMode::Truecolor,
            crate::theme::SymbolPreset::Unicode,
            std::collections::HashMap::new(),
            None,
            None,
        )
        .expect("theme builds")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorMode, SymbolPreset, Theme};
    use std::collections::HashMap;

    fn theme() -> Theme {
        Theme::new(
            "test".to_owned(),
            HashMap::new(),
            HashMap::new(),
            ColorMode::Truecolor,
            SymbolPreset::Unicode,
            HashMap::new(),
            None,
            None,
        )
        .expect("theme builds")
    }

    /// What a terminal shows: the painted row without its SGR escapes.
    fn visible(row: &str) -> String {
        crate::width::spans(row)
            .filter_map(|piece| match piece {
                crate::width::Span::Text(text) => Some(text),
                crate::width::Span::Escape(_) => None,
            })
            .collect()
    }

    fn rows(source: &str, width: usize) -> Vec<String> {
        draw(source, width, &theme())
            .expect("fence draws")
            .iter()
            .map(|row| visible(row).trim_end().to_owned())
            .collect()
    }

    fn ids(graph: &Graph) -> Vec<(&str, &str)> {
        graph
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.text.as_str()))
            .collect()
    }

    fn labels(graph: &Graph) -> Vec<Option<&str>> {
        graph.edges.iter().map(|e| e.label.as_deref()).collect()
    }

    #[test]
    fn the_header_picks_the_direction() {
        for source in ["flowchart TD", "flowchart TB", "graph TD", "graph TB"] {
            assert_eq!(
                parse(source).expect("parses").direction,
                Direction::TopDown,
                "{source}"
            );
        }
        for source in ["flowchart LR", "graph LR"] {
            assert_eq!(
                parse(source).expect("parses").direction,
                Direction::LeftRight,
                "{source}"
            );
        }
    }

    #[test]
    fn every_shape_parses() {
        let graph = parse(
            "flowchart TD\n\
             A[box] --> B(round)\n\
             B --> C{diamond}\n\
             C --> D[\"double\"]\n\
             D --> E['single']\n\
             E -->  F\n",
        )
        .expect("parses");
        assert_eq!(
            ids(&graph),
            vec![
                ("A", "box"),
                ("B", "round"),
                ("C", "diamond"),
                ("D", "double"),
                ("E", "single"),
                ("F", ""),
            ]
        );
        assert_eq!(graph.edges.len(), 5);
    }

    #[test]
    fn a_first_seen_text_survives_a_bare_reference() {
        let graph = parse("flowchart TD\nA[Start] --> B\nB --> A\n").expect("parses");
        assert_eq!(ids(&graph), vec![("A", "Start"), ("B", "")]);
    }

    #[test]
    fn a_chain_makes_one_edge_per_hop() {
        let graph = parse("flowchart TD\nA --> B --> C\n").expect("parses");
        assert_eq!(graph.edges.len(), 2);
        assert_eq!(
            graph
                .edges
                .iter()
                .map(|e| (e.from, e.to))
                .collect::<Vec<_>>(),
            vec![(0, 1), (1, 2)]
        );
    }

    #[test]
    fn every_edge_kind_parses() {
        let graph = parse("flowchart TD\nA --> B\nB --- C\nC -.-> D\nD ==> E\n").expect("parses");
        assert_eq!(
            graph.edges.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![
                EdgeKind::Arrow,
                EdgeKind::Line,
                EdgeKind::Dotted,
                EdgeKind::Thick
            ]
        );
        assert!(graph.edges.iter().all(|e| e.label.is_none()));
    }

    #[test]
    fn both_label_forms_parse() {
        let graph =
            parse("flowchart TD\nA -->|yes| B\nB -- no --> C\nC ---|line| D\n").expect("parses");
        assert_eq!(labels(&graph), vec![Some("yes"), Some("no"), Some("line")]);
    }

    #[test]
    fn a_subgraph_is_dropped_but_its_content_stays() {
        let graph = parse(
            "flowchart TD\n\
             %% a comment\n\
             subgraph one\n\
             direction LR\n\
             A --> B\n\
             end\n\
             classDef red fill:#f00\n\
             class A red\n\
             style B fill:#00f\n\
             linkStyle 0 stroke:#000\n\
             click A href\n\
             C\n",
        )
        .expect("parses");
        assert_eq!(ids(&graph), vec![("A", ""), ("B", ""), ("C", "")]);
        assert_eq!(graph.edges.len(), 1);
    }

    #[test]
    fn a_quoted_pipe_label_is_kept_verbatim() {
        let graph = parse("flowchart TD\nA -->|\"quoted\"| B\n").expect("parses");
        assert_eq!(labels(&graph), vec![Some("\"quoted\"")]);
    }

    #[test]
    fn an_id_may_carry_dots_dashes_and_underscores() {
        let graph = parse("flowchart TD\nfoo.bar --> baz_qux\nbaz_qux --> a-b\n").expect("parses");
        assert_eq!(
            ids(&graph),
            vec![("foo.bar", ""), ("baz_qux", ""), ("a-b", "")]
        );
    }

    #[test]
    fn tight_edge_spacing_still_parses() {
        let graph = parse("flowchart TD\nA-->B---C-.->D==>E\n").expect("parses");
        assert_eq!(graph.edges.len(), 4);
        assert_eq!(ids(&graph).len(), 5);
    }

    #[test]
    fn nothing_else_parses() {
        for source in [
            "",
            "\n",
            "pie\n\"x\" : 1\n",
            "sequenceDiagram\nAlice->>Bob: hi\n",
            "flowchart RL\nA --> B\n",
            "flowchart\nA --> B\n",
            "flowchart TD extra\nA --> B\n",
            "flowchart TD\nA -->\n",
            "flowchart TD\nA -> B\n",
            "flowchart TD\nA --> B\nA --> \n",
        ] {
            assert!(parse(source).is_none(), "{source:?} is not in the subset");
        }
        // A bare header parses, but draws nothing.
        assert!(parse("flowchart TD\n").expect("parses").nodes.is_empty());
    }

    #[test]
    fn a_bare_header_draws_nothing() {
        assert!(draw("flowchart TD\n", 80, &theme()).is_none());
        assert!(draw("", 80, &theme()).is_none());
    }

    #[test]
    fn top_down_two_nodes() {
        let expected = vec![
            "┌───────┐",
            "│ Start │",
            "└───┬───┘",
            "    │",
            "    ▼",
            " ┌─────┐",
            " │ End │",
            " └─────┘",
        ];
        assert_eq!(rows("flowchart TD\n A[Start] --> B[End]\n", 80), expected);
    }

    #[test]
    fn left_right_two_nodes() {
        let expected = vec![
            "┌───────┐   ┌─────┐",
            "│ Start ├─▶ │ End │",
            "└───────┘   └─────┘",
        ];
        assert_eq!(rows("flowchart LR\n A[Start] --> B[End]\n", 80), expected);
    }

    #[test]
    fn top_down_fan_out() {
        let expected = vec![
            "┌───┐",
            "│ A │",
            "└─┬─┘",
            "  ├───────┬───────┐",
            "  ▼       ▼       ▼",
            "┌───┐   ┌───┐   ┌───┐",
            "│ B │   │ C │   │ D │",
            "└───┘   └───┘   └───┘",
        ];
        assert_eq!(
            rows("flowchart TD\n A --> B\n A --> C\n A --> D\n", 80),
            expected
        );
    }

    #[test]
    fn top_down_diamond() {
        let expected = vec![
            "┌───┐",
            "│ A │",
            "└─┬─┘",
            "  ├───────┐",
            "  ▼       ▼",
            "┌───┐   ┌───┐",
            "│ B │   │ C │",
            "└─┬─┘   └─┬─┘",
            "  └───┬───┘",
            "      ▼",
            "    ┌───┐",
            "    │ D │",
            "    └───┘",
        ];
        assert_eq!(
            rows("flowchart TD\n A --> B\n A --> C\n B --> D\n C --> D\n", 80),
            expected
        );
    }

    #[test]
    fn a_labelled_edge_puts_the_label_beside_its_head() {
        let expected = vec![
            "┌───────┐",
            "│ Start │",
            "└───┬───┘",
            "    │",
            "    ▼ yes",
            " ┌─────┐",
            " │ End │",
            " └─────┘",
        ];
        assert_eq!(
            rows("flowchart TD\n A[Start] -->|yes| B[End]\n", 80),
            expected
        );
    }

    #[test]
    fn a_left_right_label_sits_in_its_gutter() {
        let expected = vec![
            "┌───────┐       ┌─────┐",
            "│ Start ├─▶ yes │ End │",
            "└───────┘       └─────┘",
        ];
        assert_eq!(
            rows("flowchart LR\n A[Start] -->|yes| B[End]\n", 80),
            expected
        );
    }

    /// `A --> B --> A` is a cycle. The depth-first pass calls `B --> A` a back
    /// edge; it is dropped from the layering (A source, B one below) and drawn
    /// as a run from B into a *copy* of A placed one layer below B. So the
    /// drawing is the unrolled cycle: A, then B, then A again.
    #[test]
    fn a_cycle_is_unrolled_once() {
        let expected = vec![
            "┌───┐",
            "│ A │",
            "└─┬─┘",
            "  │",
            "  ▼",
            "┌───┐",
            "│ B │",
            "└─┬─┘",
            "  │",
            "  ▼",
            "┌───┐",
            "│ A │",
            "└───┘",
        ];
        assert_eq!(rows("flowchart TD\n A --> B --> A\n", 80), expected);
    }

    #[test]
    fn a_headless_run_enters_its_box() {
        let expected = vec![
            "┌───┐",
            "│ A │",
            "└─┬─┘",
            "  │",
            "  │",
            "┌─┴─┐",
            "│ B │",
            "└───┘",
        ];
        assert_eq!(rows("flowchart TD\n A --- B\n", 80), expected);
    }

    /// A run that skips a layer crosses it through a one-cell pass-through in
    /// the layer it skips, so it never draws over a box.
    #[test]
    fn a_run_that_skips_a_layer_crosses_a_pass_through() {
        let expected = vec![
            "┌───┐",
            "│ A │",
            "└─┬─┘",
            "  ├─────┐",
            "  ▼     │",
            "┌───┐   │",
            "│ B │   │",
            "└─┬─┘   │",
            "  └──┬──┘",
            "     ▼",
            "   ┌───┐",
            "   │ C │",
            "   └───┘",
        ];
        assert_eq!(
            rows("flowchart TD\n A --> B\n B --> C\n A --> C\n", 80),
            expected
        );
    }

    #[test]
    fn the_fallbacks_return_none() {
        // Another diagram type.
        assert!(draw("sequenceDiagram\nAlice->>Bob: hi\n", 80, &theme()).is_none());
        // A line the subset cannot read.
        assert!(draw("flowchart TD\nA -> B\n", 80, &theme()).is_none());
        // A fence too wide for the pane.
        assert!(draw("flowchart TD\n A[Start] --> B[End]\n", 8, &theme()).is_none());
        assert!(draw("flowchart TD\n A[Start] --> B[End]\n", 0, &theme()).is_none());
    }

    #[test]
    fn every_golden_fits_the_width_it_was_drawn_at() {
        for (source, width) in [
            ("flowchart TD\n A[Start] --> B[End]\n", 80usize),
            ("flowchart LR\n A[Start] --> B[End]\n", 80),
            ("flowchart TD\n A --> B\n A --> C\n A --> D\n", 80),
            ("flowchart TD\n A --> B\n A --> C\n B --> D\n C --> D\n", 80),
            ("flowchart TD\n A[Start] -->|yes| B[End]\n", 80),
            ("flowchart TD\n A --> B --> A\n", 80),
            ("flowchart TD\n A --- B\n", 80),
        ] {
            let drawn = draw(source, width, &theme()).expect("draws");
            for row in &drawn {
                assert!(
                    visible_width(row) <= width,
                    "{source:?} at {width}: {row:?} is too wide"
                );
            }
        }
    }

    #[test]
    fn a_pane_one_column_narrower_is_the_fallback() {
        for source in [
            "flowchart TD\n A[Start] --> B[End]\n",
            "flowchart LR\n A[Start] --> B[End]\n",
            "flowchart TD\n A --> B\n A --> C\n A --> D\n",
            "flowchart TD\n A --> B --> A\n",
        ] {
            let drawn = draw(source, 80, &theme()).expect("draws");
            let needed = drawn
                .iter()
                .map(|row| visible_width(row))
                .max()
                .expect("rows");
            assert_eq!(rows(source, needed), {
                drawn
                    .iter()
                    .map(|row| visible(row).trim_end().to_owned())
                    .collect::<Vec<_>>()
            });
            assert!(
                draw(source, needed - 1, &theme()).is_none(),
                "{source:?} fits {needed} but drew at {}",
                needed - 1
            );
        }
    }

    #[test]
    fn left_right_fan_out() {
        let expected = vec![
            "        ┌───┐",
            "     ┌▶ │ B │",
            "     │  └───┘",
            "     │",
            "┌───┐│  ┌───┐",
            "│ A ├┼▶ │ C │",
            "└───┘│  └───┘",
            "     │",
            "     │  ┌───┐",
            "     └▶ │ D │",
            "        └───┘",
        ];
        assert_eq!(
            rows("flowchart LR\n A --> B\n A --> C\n A --> D\n", 80),
            expected
        );
    }

    /// The glyphs come out of the theme's symbol table, so a preset with no
    /// box glyphs draws printable ASCII — arrowheads included.
    #[test]
    fn an_ascii_symbol_preset_draws_ascii() {
        let theme = Theme::new(
            "ascii".to_owned(),
            HashMap::new(),
            HashMap::new(),
            ColorMode::Truecolor,
            SymbolPreset::Ascii,
            HashMap::new(),
            None,
            None,
        )
        .expect("theme builds");
        let expected = vec![
            "+-------+",
            "| Start |",
            "+---+---+",
            "    |",
            "    v",
            " +-----+",
            " | End |",
            " +-----+",
        ];
        let drawn: Vec<String> = draw("flowchart TD\n A[Start] --> B[End]\n", 80, &theme)
            .expect("draws")
            .iter()
            .map(|row| visible(row).trim_end().to_owned())
            .collect();
        assert_eq!(drawn, expected);
        assert!(drawn.iter().all(|row| row.is_ascii()));
    }
}
