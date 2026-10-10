//! Placing the overview's node graph: nested containers laid out inside out,
//! the zoom tiers, which boxes a tier shows, edge lifting and bundling,
//! hit-testing, the breadcrumb and the viewport maths. Pure over a
//! [`Graph`], so every rule is unit-tested without a GUI.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::model::Graph;

/// The zoom tier the current scale selects; each shows one more level of
/// nesting than the one before (see [`visible_levels`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Far,
    Mid,
    Near,
    Close,
}

impl Tier {
    /// Every tier, from the furthest out.
    pub const ALL: [Tier; 4] = [Tier::Far, Tier::Mid, Tier::Near, Tier::Close];

    /// The tier a scale falls in: below 0.35 Far, below 0.7 Mid, below 1.4
    /// Near, else Close.
    pub fn for_scale(scale: f64) -> Tier {
        if scale < 0.35 {
            Tier::Far
        } else if scale < 0.7 {
            Tier::Mid
        } else if scale < 1.4 {
            Tier::Near
        } else {
            Tier::Close
        }
    }

    /// The scale the slider jumps to for this tier.
    pub fn scale(self) -> f64 {
        match self {
            Tier::Far => 0.25,
            Tier::Mid => 0.5,
            Tier::Near => 1.0,
            Tier::Close => 2.0,
        }
    }

    /// The slider mark.
    pub fn label(self) -> &'static str {
        match self {
            Tier::Far => "Far",
            Tier::Mid => "Mid",
            Tier::Near => "Near",
            Tier::Close => "Close",
        }
    }
}

/// An axis-aligned rectangle in world coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    /// Is the point inside, edges included?
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// The smallest rectangle holding both.
    pub fn union(&self, o: &Rect) -> Rect {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        let right = (self.x + self.w).max(o.x + o.w);
        let bottom = (self.y + self.h).max(o.y + o.h);
        Rect {
            x,
            y,
            w: right - x,
            h: bottom - y,
        }
    }
}

/// One drawn copy of a node. `parent` is the id of the copy's container
/// (`None` at top level). A node placed under a mirrored container shares
/// its key with the copy under the primary one; [`Layout::get`] prefers the
/// primary copy and [`Layout::copies`] lists every one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BoxKey {
    pub id: String,
    pub parent: Option<String>,
}

/// A placed copy of a node. `level` is the nesting of this copy (0 at top
/// level); `mirror` marks every copy but the primary one; `child_order` is
/// the copy's position among its container's children in row order.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedBox {
    pub key: BoxKey,
    pub rect: Rect,
    pub level: usize,
    pub mirror: bool,
    pub child_order: usize,
}

/// The sizes the layout works with, in world units.
#[derive(Debug, Clone, PartialEq)]
pub struct Metrics {
    pub card_w: f64,
    pub card_h: f64,
    pub header_h: f64,
    pub pad: f64,
    pub gap_x: f64,
    pub gap_y: f64,
    pub row_h: f64,
}

impl Default for Metrics {
    fn default() -> Metrics {
        Metrics {
            card_w: 220.0,
            card_h: 120.0,
            header_h: 44.0,
            pad: 16.0,
            gap_x: 48.0,
            gap_y: 40.0,
            row_h: 26.0,
        }
    }
}

/// Every placed copy and the rectangle enclosing them all.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    pub boxes: Vec<PlacedBox>,
    pub bounds: Rect,
}

impl Layout {
    /// The box with this key; when a node sits under a mirrored container as
    /// well as the primary one, the primary copy.
    pub fn get(&self, key: &BoxKey) -> Option<&PlacedBox> {
        let mut found = None;
        for b in self.boxes.iter().filter(|b| b.key == *key) {
            if !b.mirror {
                return Some(b);
            }
            found.get_or_insert(b);
        }
        found
    }

    /// Every copy of a node, in placement order (for the mirror hover
    /// highlight).
    pub fn copies(&self, id: &str) -> Vec<&PlacedBox> {
        self.boxes.iter().filter(|b| b.key.id == id).collect()
    }
}

/// The deepest level that is placed; children of a level-6 box are left out.
pub const MAX_LEVEL: usize = 6;

/// The `links` edges among one container's children, as index pairs into
/// `ids` (self-links and links leaving the container dropped), each once.
fn sibling_edges(graph: &Graph, ids: &[String]) -> BTreeSet<(usize, usize)> {
    let index: BTreeMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    let mut edges = BTreeSet::new();
    for (from, id) in ids.iter().enumerate() {
        let Some(node) = graph.node(id) else {
            continue;
        };
        for link in &node.links {
            if let Some(&to) = index.get(link.target.as_str())
                && to != from
            {
                edges.insert((from, to));
            }
        }
    }
    edges
}

/// Drop the back-edges a depth-first walk in index order finds, leaving a DAG.
fn without_back_edges(n: usize, edges: &BTreeSet<(usize, usize)>) -> Vec<(usize, usize)> {
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(from, to) in edges {
        out[from].push(to);
    }
    // 0 = unvisited, 1 = on the stack, 2 = done.
    let mut state = vec![0u8; n];
    let mut kept = Vec::new();
    for root in 0..n {
        if state[root] != 0 {
            continue;
        }
        // (node, index of the next out-edge to look at)
        let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
        state[root] = 1;
        while let Some(&mut (node, ref mut next)) = stack.last_mut() {
            if *next < out[node].len() {
                let to = out[node][*next];
                *next += 1;
                match state[to] {
                    1 => {} // back-edge: dropped
                    0 => {
                        kept.push((node, to));
                        state[to] = 1;
                        stack.push((to, 0));
                    }
                    _ => kept.push((node, to)),
                }
            } else {
                state[node] = 2;
                stack.pop();
            }
        }
    }
    kept
}

/// Longest-path layers over a DAG: sources in layer 0, every other node one
/// below its deepest predecessor.
fn layers(n: usize, dag: &[(usize, usize)]) -> Vec<usize> {
    let mut indegree = vec![0usize; n];
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(from, to) in dag {
        out[from].push(to);
        indegree[to] += 1;
    }
    let mut layer = vec![0usize; n];
    let mut ready: VecDeque<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
    while let Some(node) = ready.pop_front() {
        for &to in &out[node] {
            layer[to] = layer[to].max(layer[node] + 1);
            indegree[to] -= 1;
            if indegree[to] == 0 {
                ready.push_back(to);
            }
        }
    }
    layer
}

/// Arrange sibling boxes of the given sizes: rows by layer, stacked
/// vertically, boxes left to right ordered by the barycenter of their
/// predecessors (ties by index, which is id order). Returns the rects relative
/// to the arrangement's top-left corner, its width and height, and the
/// row order of every index.
fn arrange(
    edges: &BTreeSet<(usize, usize)>,
    sizes: &[(f64, f64)],
    m: &Metrics,
) -> (Vec<Rect>, f64, f64, Vec<usize>) {
    let n = sizes.len();
    let dag = without_back_edges(n, edges);
    let layer = layers(n, &dag);
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(from, to) in &dag {
        preds[to].push(from);
    }
    let row_count = layer.iter().max().map_or(0, |l| l + 1);
    let mut rows: Vec<Vec<usize>> = vec![Vec::new(); row_count];
    for (i, &l) in layer.iter().enumerate() {
        rows[l].push(i);
    }
    let mut rects = vec![Rect::default(); n];
    let mut order = vec![0usize; n];
    let mut y = 0.0;
    let mut width: f64 = 0.0;
    let mut placed = 0;
    for row in &mut rows {
        // Predecessors sit in earlier rows and are already positioned.
        let bary = |i: usize| -> f64 {
            let ps = &preds[i];
            if ps.is_empty() {
                return 0.0;
            }
            ps.iter().map(|&p| rects[p].center().0).sum::<f64>() / ps.len() as f64
        };
        row.sort_by(|&a, &b| {
            bary(a)
                .partial_cmp(&bary(b))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(&b))
        });
        let mut x = 0.0;
        let mut row_h: f64 = 0.0;
        for &i in row.iter() {
            let (w, h) = sizes[i];
            rects[i] = Rect { x, y, w, h };
            order[i] = placed;
            placed += 1;
            x += w + m.gap_x;
            row_h = row_h.max(h);
        }
        width = width.max(x - m.gap_x);
        y += row_h + m.gap_y;
    }
    let height = if row_count == 0 { 0.0 } else { y - m.gap_y };
    (rects, width.max(0.0), height, order)
}

/// A laid-out subtree: its boxes relative to its own top-left corner.
struct Subtree {
    w: f64,
    h: f64,
    boxes: Vec<PlacedBox>,
}

/// Lay out one copy of `id` at `level`, children included (down to
/// [`MAX_LEVEL`]). `primary` is false inside a mirror or for every copy but
/// the one under the node's first parent.
fn place(graph: &Graph, id: &str, level: usize, primary: bool, m: &Metrics) -> Subtree {
    let children: &[String] = if level < MAX_LEVEL {
        graph.children(id)
    } else {
        &[]
    };
    let mut boxes = Vec::new();
    let (w, h) = if children.is_empty() {
        (m.card_w, m.card_h)
    } else {
        let subtrees: Vec<Subtree> = children
            .iter()
            .map(|child| {
                let first = graph.parents(child).first().is_some_and(|p| p == id);
                place(graph, child, level + 1, primary && first, m)
            })
            .collect();
        let sizes: Vec<(f64, f64)> = subtrees.iter().map(|s| (s.w, s.h)).collect();
        let (rects, inner_w, inner_h, order) = arrange(&sibling_edges(graph, children), &sizes, m);
        let origin_x = m.pad;
        let origin_y = m.header_h + m.pad;
        for (i, (child, sub)) in children.iter().zip(subtrees).enumerate() {
            let at = rects[i];
            boxes.push(PlacedBox {
                key: BoxKey {
                    id: child.clone(),
                    parent: Some(id.to_string()),
                },
                rect: Rect {
                    x: origin_x + at.x,
                    y: origin_y + at.y,
                    w: at.w,
                    h: at.h,
                },
                level: level + 1,
                mirror: sub.boxes.first().is_some_and(|b| b.mirror),
                child_order: order[i],
            });
            // The subtree's own entry for `child` is its first box; the rest
            // are its descendants, shifted into place.
            boxes.extend(sub.boxes.into_iter().skip(1).map(|mut b| {
                b.rect.x += origin_x + at.x;
                b.rect.y += origin_y + at.y;
                b
            }));
        }
        (
            (inner_w + 2.0 * m.pad).max(m.card_w),
            m.header_h + 2.0 * m.pad + inner_h,
        )
    };
    boxes.insert(
        0,
        PlacedBox {
            key: BoxKey {
                id: id.to_string(),
                parent: None,
            },
            rect: Rect {
                x: 0.0,
                y: 0.0,
                w,
                h,
            },
            level,
            mirror: !primary,
            child_order: 0,
        },
    );
    Subtree { w, h, boxes }
}

/// Place every node: the top level arranged like a container's children,
/// each container sized inside out to hold its own arrangement. A node with
/// several parents is placed in each (its subtree too); the copy under its
/// first parent in frontmatter order, inside primary containers, is the
/// primary one and every other copy is a mirror. Deterministic: ids order
/// everything that the layering does not.
pub fn layout(graph: &Graph, m: &Metrics) -> Layout {
    let top = graph.top_level();
    let subtrees: Vec<Subtree> = top.iter().map(|id| place(graph, id, 0, true, m)).collect();
    let sizes: Vec<(f64, f64)> = subtrees.iter().map(|s| (s.w, s.h)).collect();
    let (rects, _, _, order) = arrange(&sibling_edges(graph, top), &sizes, m);
    let mut boxes = Vec::new();
    for (i, sub) in subtrees.into_iter().enumerate() {
        let at = rects[i];
        boxes.extend(sub.boxes.into_iter().enumerate().map(|(j, mut b)| {
            b.rect.x += at.x;
            b.rect.y += at.y;
            if j == 0 {
                b.child_order = order[i];
            }
            b
        }));
    }
    let bounds = boxes
        .iter()
        .filter(|b| b.level == 0)
        .map(|b| b.rect)
        .reduce(|a, b| a.union(&b))
        .unwrap_or_default();
    Layout { boxes, bounds }
}

/// How many nesting levels a tier shows: Far the top level only, Mid one
/// level of rows below it, Near two (cards and their rows), Close three (the
/// spec's "Deeper: as Near": the cards' children take Near's card form and
/// their own children are rows).
pub fn visible_levels(tier: Tier) -> usize {
    match tier {
        Tier::Far => 1,
        Tier::Mid => 2,
        Tier::Near => 3,
        Tier::Close => 4,
    }
}

/// Rows a container shows at Mid before the "+ N more" row.
pub const MID_ROWS: usize = 12;

/// The boxes a tier draws, in placement order (containers before their
/// contents).
pub fn visible(layout: &Layout, tier: Tier) -> Vec<&PlacedBox> {
    let limit = visible_levels(tier);
    layout.boxes.iter().filter(|b| b.level < limit).collect()
}

/// The container copy a placed box sits in: the copy of `key.parent` whose
/// rectangle encloses it (copies of one node never nest, so there is one).
fn container_of<'a>(layout: &'a Layout, b: &PlacedBox) -> Option<&'a PlacedBox> {
    let parent = b.key.parent.as_deref()?;
    let (cx, cy) = b.rect.center();
    layout
        .boxes
        .iter()
        .find(|c| c.key.id == parent && c.level + 1 == b.level && c.rect.contains(cx, cy))
}

/// The primary copy of a node: its first non-mirror copy, else its first copy.
fn primary_copy<'a>(layout: &'a Layout, id: &str) -> Option<&'a PlacedBox> {
    let mut found = None;
    for b in layout.boxes.iter().filter(|b| b.key.id == id) {
        if !b.mirror {
            return Some(b);
        }
        found.get_or_insert(b);
    }
    found
}

/// Walk up from `b` to the first box the tier shows.
fn lift<'a>(layout: &'a Layout, mut b: &'a PlacedBox, tier: Tier) -> Option<&'a PlacedBox> {
    let limit = visible_levels(tier);
    while b.level >= limit {
        b = container_of(layout, b)?;
    }
    Some(b)
}

/// The box itself when the tier shows it, else the nearest container copy
/// above it that the tier shows. A key the layout does not hold (stale after
/// a rebuild) is resolved through the graph: the node's primary copy, or the
/// first placed node up its first-parent chain.
pub fn visible_ancestor<'a>(
    layout: &'a Layout,
    graph: &Graph,
    key: &BoxKey,
    tier: Tier,
) -> Option<&'a PlacedBox> {
    let start = layout.get(key).or_else(|| {
        let mut id = key.id.as_str();
        let mut steps = 0;
        loop {
            if let Some(b) = primary_copy(layout, id) {
                return Some(b);
            }
            id = graph.parents(id).first()?;
            steps += 1;
            if steps > graph.len() {
                return None;
            }
        }
    })?;
    lift(layout, start, tier)
}

/// A bundle of links drawn as one line between two visible boxes. `labels`
/// are the distinct link names (empty for a warning), `count` the number of
/// links bundled; the renderer shows the name when `count == 1`, else
/// "N links". `warning` marks a missing-link pair, drawn dashed.
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub from: BoxKey,
    pub to: BoxKey,
    pub labels: Vec<String>,
    pub count: usize,
    pub warning: bool,
}

/// The edges a tier draws. Every `links` edge a→b runs between the visible
/// ancestors (at this tier) of the primary copies of a and b; one whose ends
/// lift to the same box is dropped; the rest are bundled per (from, to) pair
/// with their distinct names and a count. `warning_pairs` (missing-link node
/// pairs) are lifted and bundled the same way into separate edges with
/// `warning: true` and no labels; they are produced at every tier so the
/// renderer decides where to draw them (the spec dashes them from Near on).
/// Pairs naming unknown nodes are ignored. Order: by (from, to), warnings
/// after the plain edge of the same pair.
pub fn edges(
    graph: &Graph,
    layout: &Layout,
    tier: Tier,
    warning_pairs: &[(String, String)],
) -> Vec<Edge> {
    let mut primaries: BTreeMap<&str, &PlacedBox> = BTreeMap::new();
    for b in &layout.boxes {
        match primaries.get(b.key.id.as_str()) {
            Some(existing) if !existing.mirror || b.mirror => {}
            _ => {
                primaries.insert(b.key.id.as_str(), b);
            }
        }
    }
    let ends = |a: &str, b: &str| -> Option<(BoxKey, BoxKey)> {
        let from = lift(layout, primaries.get(a)?, tier)?;
        let to = lift(layout, primaries.get(b)?, tier)?;
        if std::ptr::eq(from, to) {
            return None;
        }
        Some((from.key.clone(), to.key.clone()))
    };
    let mut bundles: BTreeMap<(BoxKey, BoxKey, bool), (BTreeSet<String>, usize)> = BTreeMap::new();
    for node in graph.nodes() {
        for link in &node.links {
            if link.target == node.id || graph.node(&link.target).is_none() {
                continue;
            }
            if let Some((from, to)) = ends(&node.id, &link.target) {
                let entry = bundles.entry((from, to, false)).or_default();
                entry.0.insert(link.name.clone());
                entry.1 += 1;
            }
        }
    }
    for (a, b) in warning_pairs {
        if let Some((from, to)) = ends(a, b) {
            bundles.entry((from, to, true)).or_default().1 += 1;
        }
    }
    bundles
        .into_iter()
        .map(|((from, to, warning), (labels, count))| Edge {
            from,
            to,
            labels: labels.into_iter().collect(),
            count,
            warning,
        })
        .collect()
}

/// The deepest box the tier shows that contains the world point.
pub fn hit_test(layout: &Layout, tier: Tier, x: f64, y: f64) -> Option<&PlacedBox> {
    visible(layout, tier)
        .into_iter()
        .filter(|b| b.rect.contains(x, y))
        .fold(None, |best: Option<&PlacedBox>, b| match best {
            Some(best) if best.level >= b.level => Some(best),
            _ => Some(b),
        })
}

/// The visible level-1 box (the ones drawn as cards) whose centre is nearest
/// `(cx, cy)`: the card the Close tier opens. Deeper boxes are drawn as rows
/// inside their card, so they are never the focus. `None` when the tier
/// shows no level-1 box.
pub fn focused(layout: &Layout, tier: Tier, cx: f64, cy: f64) -> Option<&PlacedBox> {
    let distance = |b: &PlacedBox| {
        let (x, y) = b.rect.center();
        (x - cx).powi(2) + (y - cy).powi(2)
    };
    visible(layout, tier)
        .into_iter()
        .filter(|b| b.level == 1)
        .fold(None, |best: Option<&PlacedBox>, b| match best {
            Some(best) if distance(best) <= distance(b) => Some(best),
            _ => Some(b),
        })
}

/// The ids of the visible boxes containing the point, outermost first;
/// empty when the point is in no box. (`graph` is unused: containment is
/// read from the placed boxes.)
pub fn breadcrumb(layout: &Layout, _graph: &Graph, tier: Tier, cx: f64, cy: f64) -> Vec<String> {
    let mut hits: Vec<&PlacedBox> = visible(layout, tier)
        .into_iter()
        .filter(|b| b.rect.contains(cx, cy))
        .collect();
    hits.sort_by_key(|b| b.level);
    hits.into_iter().map(|b| b.key.id.clone()).collect()
}

/// The scale range a viewport may take.
pub const MIN_SCALE: f64 = 0.05;
pub const MAX_SCALE: f64 = 8.0;

/// The mapping from world to screen: `screen = world * scale + (ox, oy)`,
/// over a canvas of `w` by `h` pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub scale: f64,
    pub ox: f64,
    pub oy: f64,
    pub w: f64,
    pub h: f64,
}

impl Viewport {
    /// Scale 1 with the world origin in the top-left corner.
    pub fn new(w: f64, h: f64) -> Viewport {
        Viewport {
            scale: 1.0,
            ox: 0.0,
            oy: 0.0,
            w,
            h,
        }
    }

    pub fn to_world(&self, sx: f64, sy: f64) -> (f64, f64) {
        ((sx - self.ox) / self.scale, (sy - self.oy) / self.scale)
    }

    pub fn to_screen(&self, wx: f64, wy: f64) -> (f64, f64) {
        (wx * self.scale + self.ox, wy * self.scale + self.oy)
    }

    /// The viewport at `scale` (clamped) with the world point under the
    /// screen point `(sx, sy)` left where it is.
    fn with_scale_at(&self, sx: f64, sy: f64, scale: f64) -> Viewport {
        let scale = if scale.is_finite() {
            scale.clamp(MIN_SCALE, MAX_SCALE)
        } else {
            self.scale
        };
        let (wx, wy) = self.to_world(sx, sy);
        Viewport {
            scale,
            ox: sx - wx * scale,
            oy: sy - wy * scale,
            ..*self
        }
    }

    /// Multiply the scale by `factor`, keeping the world point under the
    /// screen point `(sx, sy)` fixed; the scale is clamped to
    /// `MIN_SCALE..=MAX_SCALE`.
    pub fn zoom_at(&self, sx: f64, sy: f64, factor: f64) -> Viewport {
        self.with_scale_at(sx, sy, self.scale * factor)
    }

    /// Jump to `scale` (clamped), keeping the world point at the centre.
    pub fn set_scale_at_center(&self, scale: f64) -> Viewport {
        self.with_scale_at(self.w / 2.0, self.h / 2.0, scale)
    }

    /// Move the picture by `(dx, dy)` screen pixels.
    pub fn pan(&self, dx: f64, dy: f64) -> Viewport {
        Viewport {
            ox: self.ox + dx,
            oy: self.oy + dy,
            ..*self
        }
    }

    /// Scale and offset so `r` fills the viewport minus `margin` on every
    /// side, centred. A degenerate rect or viewport (nothing to divide by)
    /// gives scale 1 centred on `r`.
    pub fn fit(&self, r: &Rect, margin: f64) -> Viewport {
        let avail_w = self.w - 2.0 * margin;
        let avail_h = self.h - 2.0 * margin;
        let scale = if r.w > 0.0 && r.h > 0.0 && avail_w > 0.0 && avail_h > 0.0 {
            (avail_w / r.w).min(avail_h / r.h)
        } else {
            1.0
        };
        let scale = if scale.is_finite() {
            scale.clamp(MIN_SCALE, MAX_SCALE)
        } else {
            1.0
        };
        let (cx, cy) = r.center();
        Viewport {
            scale,
            ox: self.w / 2.0 - cx * scale,
            oy: self.h / 2.0 - cy * scale,
            ..*self
        }
    }

    /// The world point at the centre of the canvas.
    pub fn center_world(&self) -> (f64, f64) {
        self.to_world(self.w / 2.0, self.h / 2.0)
    }

    pub fn tier(&self) -> Tier {
        Tier::for_scale(self.scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::model::Source;
    use std::path::PathBuf;

    /// A graph from `(id, extra frontmatter lines)` pairs, one file per node.
    fn graph(files: &[(&str, &str)]) -> Graph {
        let sources: Vec<Source> = files
            .iter()
            .map(|(id, extra)| {
                (
                    PathBuf::from(format!("/r/{id}.md")),
                    Ok(format!("---\nid: {id}\n{extra}---\n")),
                )
            })
            .collect();
        Graph::build(sources)
    }

    fn key(id: &str, parent: Option<&str>) -> BoxKey {
        BoxKey {
            id: id.to_string(),
            parent: parent.map(str::to_string),
        }
    }

    fn rect_of(l: &Layout, id: &str, parent: Option<&str>) -> Rect {
        l.get(&key(id, parent))
            .unwrap_or_else(|| panic!("no box {id} in {parent:?}"))
            .rect
    }

    fn inside(inner: &Rect, outer: &Rect) -> bool {
        inner.x >= outer.x
            && inner.y >= outer.y
            && inner.x + inner.w <= outer.x + outer.w
            && inner.y + inner.h <= outer.y + outer.h
    }

    fn disjoint(a: &Rect, b: &Rect) -> bool {
        a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y
    }

    // --- layout ---

    #[test]
    fn a_single_node_fills_one_card_at_the_origin() {
        let m = Metrics::default();
        assert_eq!((m.card_w, m.card_h), (220.0, 120.0));
        assert_eq!((m.header_h, m.pad), (44.0, 16.0));
        assert_eq!((m.gap_x, m.gap_y, m.row_h), (48.0, 40.0, 26.0));
        let l = layout(&graph(&[("solo", "")]), &m);
        assert_eq!(l.boxes.len(), 1);
        let b = &l.boxes[0];
        assert_eq!(b.key, key("solo", None));
        assert_eq!(
            b.rect,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 220.0,
                h: 120.0
            }
        );
        assert_eq!((b.level, b.mirror, b.child_order), (0, false, 0));
        assert_eq!(l.bounds, b.rect);
        assert_eq!(layout(&Graph::default(), &m), Layout::default());
    }

    #[test]
    fn two_linked_siblings_land_in_different_rows_with_the_source_above() {
        let m = Metrics::default();
        let l = layout(&graph(&[("b", ""), ("a", "links:\n  blocks: b\n")]), &m);
        let a = rect_of(&l, "a", None);
        let b = rect_of(&l, "b", None);
        assert!(a.y + a.h <= b.y, "a {a:?} must be above b {b:?}");
        assert_eq!(b.y, a.y + a.h + m.gap_y);
        // Unlinked siblings share a row, left to right by id.
        let l = layout(&graph(&[("b", ""), ("a", "")]), &m);
        let a = rect_of(&l, "a", None);
        let b = rect_of(&l, "b", None);
        assert_eq!(a.y, b.y);
        assert_eq!(b.x, a.x + a.w + m.gap_x);
        assert_eq!(l.bounds.w, 2.0 * m.card_w + m.gap_x);
    }

    #[test]
    fn a_cycle_among_siblings_still_lays_out() {
        let l = layout(
            &graph(&[
                ("a", "links:\n  next: b\n"),
                ("b", "links:\n  next: c\n"),
                ("c", "links:\n  next: a\n"),
            ]),
            &Metrics::default(),
        );
        assert_eq!(l.boxes.len(), 3);
        // The back-edge c→a is dropped: a, b, c form three rows.
        let ys: Vec<f64> = ["a", "b", "c"]
            .iter()
            .map(|id| rect_of(&l, id, None).y)
            .collect();
        assert!(ys[0] < ys[1] && ys[1] < ys[2], "{ys:?}");
        // Self-links are not edges.
        let l = layout(&graph(&[("a", "links:\n  self: a\n")]), &Metrics::default());
        assert_eq!(l.boxes.len(), 1);
    }

    #[test]
    fn a_container_is_sized_to_hold_its_children_with_header_and_padding() {
        let m = Metrics::default();
        let l = layout(
            &graph(&[
                ("p", ""),
                ("a", "parent: p\nlinks:\n  blocks: b\n"),
                ("b", "parent: p\n"),
                ("c", "parent: p\n"),
            ]),
            &m,
        );
        let p = rect_of(&l, "p", None);
        let a = rect_of(&l, "a", Some("p"));
        let b = rect_of(&l, "b", Some("p"));
        let c = rect_of(&l, "c", Some("p"));
        for (id, r) in [("a", &a), ("b", &b), ("c", &c)] {
            assert!(inside(r, &p), "{id} {r:?} not inside p {p:?}");
            assert!(
                r.x >= p.x + m.pad && r.y >= p.y + m.header_h + m.pad,
                "{id}"
            );
        }
        // Rows: a; b c (c has no edges, so it is a source in row 0 with a).
        assert_eq!(a.y, c.y);
        assert!(b.y > a.y);
        assert_eq!(p.w, 2.0 * m.card_w + m.gap_x + 2.0 * m.pad);
        assert_eq!(p.h, m.header_h + 2.0 * m.pad + 2.0 * m.card_h + m.gap_y);
        assert_eq!(l.bounds, p);
        // Levels and child order follow the arrangement.
        let get = |id: &str| l.get(&key(id, Some("p"))).unwrap();
        assert_eq!(get("a").level, 1);
        assert_eq!(l.get(&key("p", None)).unwrap().level, 0);
        assert_eq!(get("a").child_order, 0);
        assert_eq!(get("c").child_order, 1);
        assert_eq!(get("b").child_order, 2);
    }

    #[test]
    fn nested_containers_are_laid_out_inside_out() {
        let m = Metrics::default();
        let l = layout(
            &graph(&[
                ("top", ""),
                ("mid", "parent: top\n"),
                ("leaf", "parent: mid\n"),
                ("other", "parent: top\n"),
            ]),
            &m,
        );
        let top = rect_of(&l, "top", None);
        let mid = rect_of(&l, "mid", Some("top"));
        let leaf = rect_of(&l, "leaf", Some("mid"));
        let other = rect_of(&l, "other", Some("top"));
        assert!(inside(&leaf, &mid) && inside(&mid, &top) && inside(&other, &top));
        assert!(disjoint(&mid, &other));
        assert_eq!(mid.w, m.card_w + 2.0 * m.pad);
        assert_eq!(mid.h, m.header_h + 2.0 * m.pad + m.card_h);
        assert_eq!(top.w, mid.w + other.w + m.gap_x + 2.0 * m.pad);
        assert_eq!(top.h, m.header_h + 2.0 * m.pad + mid.h);
        assert_eq!(l.get(&key("leaf", Some("mid"))).unwrap().level, 2);
        assert_eq!(l.bounds, top);
    }

    #[test]
    fn a_node_with_two_parents_is_placed_in_each_and_later_copies_are_mirrors() {
        let m = Metrics::default();
        let l = layout(
            &graph(&[
                ("p", ""),
                ("q", ""),
                ("m", "parent: [q, p]\n"),
                ("c", "parent: m\n"),
            ]),
            &m,
        );
        let copies = l.copies("m");
        assert_eq!(copies.len(), 2);
        let in_q = l.get(&key("m", Some("q"))).unwrap();
        let in_p = l.get(&key("m", Some("p"))).unwrap();
        // The first parent in frontmatter order holds the primary copy.
        assert!(!in_q.mirror);
        assert!(in_p.mirror);
        assert_eq!((in_q.level, in_p.level), (1, 1));
        assert!(disjoint(&in_q.rect, &in_p.rect));
        assert!(inside(&in_q.rect, &rect_of(&l, "q", None)));
        assert!(inside(&in_p.rect, &rect_of(&l, "p", None)));
        // The subtree is laid out in each copy; inside a mirror it is a mirror.
        let cs = l.copies("c");
        assert_eq!(cs.len(), 2);
        let c_primary = cs.iter().find(|c| !c.mirror).unwrap();
        let c_mirror = cs.iter().find(|c| c.mirror).unwrap();
        assert!(inside(&c_primary.rect, &in_q.rect));
        assert!(inside(&c_mirror.rect, &in_p.rect));
        assert_eq!(c_mirror.level, 2);
        // `get` prefers the primary copy when the key is ambiguous.
        assert!(!l.get(&key("c", Some("m"))).unwrap().mirror);
        assert!(l.copies("nope").is_empty());
        assert_eq!(l.get(&key("m", None)), None);
    }

    #[test]
    fn nesting_stops_at_the_level_cap() {
        let files: Vec<(String, String)> = (0..10)
            .map(|i| {
                let extra = if i == 0 {
                    String::new()
                } else {
                    format!("parent: n{}\n", i - 1)
                };
                (format!("n{i}"), extra)
            })
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let l = layout(&graph(&refs), &Metrics::default());
        assert_eq!(MAX_LEVEL, 6);
        assert_eq!(l.boxes.len(), MAX_LEVEL + 1);
        assert_eq!(l.boxes.iter().map(|b| b.level).max(), Some(MAX_LEVEL));
        assert!(l.copies("n7").is_empty());
    }

    #[test]
    fn sibling_boxes_never_overlap_and_the_layout_is_deterministic() {
        // A dozen top-level nodes with pseudo-random links (LCG, fixed seed).
        let mut seed: u64 = 12345;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        let mut files = Vec::new();
        for i in 0..12 {
            let mut links = String::from("links:\n");
            for j in 0..3 {
                links.push_str(&format!("  l{j}-{}: n{}\n", next() % 4, next() % 12));
            }
            // Give a few of them a child so sizes differ.
            if i % 4 == 0 {
                files.push((format!("n{i}-kid"), format!("parent: n{i}\n")));
            }
            files.push((format!("n{i}"), links));
        }
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let g = graph(&refs);
        let l = layout(&g, &Metrics::default());
        let tops: Vec<&PlacedBox> = l.boxes.iter().filter(|b| b.level == 0).collect();
        assert_eq!(tops.len(), 12);
        for (i, a) in tops.iter().enumerate() {
            for b in &tops[i + 1..] {
                assert!(
                    disjoint(&a.rect, &b.rect),
                    "{} {:?} overlaps {} {:?}",
                    a.key.id,
                    a.rect,
                    b.key.id,
                    b.rect
                );
            }
            assert!(inside(&a.rect, &l.bounds));
        }
        assert_eq!(layout(&g, &Metrics::default()), l);
    }

    // --- tiers ---

    #[test]
    fn tiers_follow_the_scale_thresholds_at_the_boundaries() {
        assert_eq!(Tier::for_scale(0.05), Tier::Far);
        assert_eq!(Tier::for_scale(0.349), Tier::Far);
        assert_eq!(Tier::for_scale(0.35), Tier::Mid);
        assert_eq!(Tier::for_scale(0.699), Tier::Mid);
        assert_eq!(Tier::for_scale(0.7), Tier::Near);
        assert_eq!(Tier::for_scale(1.399), Tier::Near);
        assert_eq!(Tier::for_scale(1.4), Tier::Close);
        assert_eq!(Tier::for_scale(8.0), Tier::Close);
        assert!(Tier::Far < Tier::Mid && Tier::Mid < Tier::Near && Tier::Near < Tier::Close);
    }

    #[test]
    fn tier_jump_scales_round_trip_and_labels_are_the_slider_marks() {
        assert_eq!(Tier::ALL, [Tier::Far, Tier::Mid, Tier::Near, Tier::Close]);
        for t in Tier::ALL {
            assert_eq!(Tier::for_scale(t.scale()), t, "{t:?}");
        }
        assert_eq!(Tier::Far.scale(), 0.25);
        assert_eq!(Tier::Close.scale(), 2.0);
        let labels: Vec<&str> = Tier::ALL.iter().map(|t| t.label()).collect();
        assert_eq!(labels, vec!["Far", "Mid", "Near", "Close"]);
    }

    // --- tiers over a layout ---

    /// p{a{deep}, b}, q{c}; a →related→ c, a →uses→ c, b →blocks→ a, deep →sees→ c.
    fn nested() -> Graph {
        graph(&[
            ("p", ""),
            ("q", ""),
            ("a", "parent: p\nlinks:\n  related: c\n  uses: c\n"),
            ("deep", "parent: a\nlinks:\n  sees: c\n"),
            ("b", "parent: p\nlinks:\n  blocks: a\n"),
            ("c", "parent: q\n"),
        ])
    }

    /// [`nested`] plus a level-3 node: deep{deeper}.
    fn nested_deeper() -> Graph {
        graph(&[
            ("p", ""),
            ("q", ""),
            ("a", "parent: p\nlinks:\n  related: c\n  uses: c\n"),
            ("deep", "parent: a\nlinks:\n  sees: c\n"),
            ("deeper", "parent: deep\n"),
            ("b", "parent: p\nlinks:\n  blocks: a\n"),
            ("c", "parent: q\n"),
        ])
    }

    fn ids_of(boxes: &[&PlacedBox]) -> Vec<String> {
        let mut ids: Vec<String> = boxes.iter().map(|b| b.key.id.clone()).collect();
        ids.sort();
        ids
    }

    #[test]
    fn visible_boxes_follow_the_tier_table() {
        assert_eq!(visible_levels(Tier::Far), 1);
        assert_eq!(visible_levels(Tier::Mid), 2);
        assert_eq!(visible_levels(Tier::Near), 3);
        assert_eq!(visible_levels(Tier::Close), 4);
        assert_eq!(MID_ROWS, 12);
        let l = layout(&nested(), &Metrics::default());
        assert_eq!(ids_of(&visible(&l, Tier::Far)), vec!["p", "q"]);
        assert_eq!(
            ids_of(&visible(&l, Tier::Mid)),
            vec!["a", "b", "c", "p", "q"]
        );
        assert_eq!(
            ids_of(&visible(&l, Tier::Near)),
            vec!["a", "b", "c", "deep", "p", "q"]
        );
        assert_eq!(visible(&l, Tier::Close).len(), 6);
        // A level-3 box (deeper, in deep) shows at Close only: Near's
        // children at Close take Near's form, so their own children are rows.
        let l = layout(&nested_deeper(), &Metrics::default());
        assert_eq!(l.get(&key("deeper", Some("deep"))).unwrap().level, 3);
        assert_eq!(
            ids_of(&visible(&l, Tier::Near)),
            vec!["a", "b", "c", "deep", "p", "q"]
        );
        assert_eq!(
            ids_of(&visible(&l, Tier::Close)),
            vec!["a", "b", "c", "deep", "deeper", "p", "q"]
        );
    }

    #[test]
    fn visible_ancestor_lifts_a_hidden_box_to_its_nearest_visible_container() {
        let g = nested();
        let l = layout(&g, &Metrics::default());
        let deep = key("deep", Some("a"));
        let far = visible_ancestor(&l, &g, &deep, Tier::Far).unwrap();
        assert_eq!(far.key, key("p", None));
        let mid = visible_ancestor(&l, &g, &deep, Tier::Mid).unwrap();
        assert_eq!(mid.key, key("a", Some("p")));
        let near = visible_ancestor(&l, &g, &deep, Tier::Near).unwrap();
        assert_eq!(near.key, deep);
        // A top-level box is its own ancestor at every tier; unknown keys are None.
        assert_eq!(
            visible_ancestor(&l, &g, &key("q", None), Tier::Far)
                .unwrap()
                .key,
            key("q", None)
        );
        assert_eq!(visible_ancestor(&l, &g, &key("zz", None), Tier::Far), None);
        // A stale key (wrong container) still resolves through the graph.
        let stale = key("deep", Some("q"));
        assert_eq!(
            visible_ancestor(&l, &g, &stale, Tier::Far).unwrap().key,
            key("p", None)
        );
    }

    #[test]
    fn edges_lift_to_visible_boxes_bundle_per_pair_and_drop_self_edges() {
        let g = nested();
        let l = layout(&g, &Metrics::default());
        // Far: a→c, a→c, deep→c lift to p→q (3 links); b→a lifts to p→p and drops.
        let far = edges(&g, &l, Tier::Far, &[]);
        assert_eq!(far.len(), 1, "{far:?}");
        assert_eq!(far[0].from, key("p", None));
        assert_eq!(far[0].to, key("q", None));
        assert_eq!(far[0].count, 3);
        assert_eq!(far[0].labels, vec!["related", "sees", "uses"]);
        assert!(!far[0].warning);
        // Near: every box is visible; two links a→c bundle, b→a and deep→c stand alone.
        let near = edges(&g, &l, Tier::Near, &[]);
        assert_eq!(near.len(), 3, "{near:?}");
        let ac = near
            .iter()
            .find(|e| e.from.id == "a" && e.to.id == "c")
            .unwrap();
        assert_eq!(ac.count, 2);
        assert_eq!(ac.labels, vec!["related", "uses"]);
        assert_eq!(ac.from, key("a", Some("p")));
        assert_eq!(ac.to, key("c", Some("q")));
        let ba = near
            .iter()
            .find(|e| e.from.id == "b" && e.to.id == "a")
            .unwrap();
        assert_eq!(
            (ba.count, ba.labels.as_slice()),
            (1, &["blocks".to_string()][..])
        );
        assert!(near.iter().any(|e| e.from.id == "deep" && e.to.id == "c"));
        // Links to unknown nodes and self-links draw nothing.
        let g2 = graph(&[("a", "links:\n  x: [ghost, a]\n")]);
        let l2 = layout(&g2, &Metrics::default());
        assert!(edges(&g2, &l2, Tier::Near, &[]).is_empty());
    }

    #[test]
    fn edges_from_a_mirror_leave_from_the_primary_copy() {
        let g = graph(&[
            ("p", ""),
            ("q", ""),
            ("m", "parent: [q, p]\nlinks:\n  to: x\n"),
            ("x", "parent: p\n"),
        ]);
        let l = layout(&g, &Metrics::default());
        let near = edges(&g, &l, Tier::Near, &[]);
        assert_eq!(near.len(), 1, "{near:?}");
        assert_eq!(near[0].from, key("m", Some("q")));
        assert_eq!(near[0].to, key("x", Some("p")));
        // At Far the edge runs q→p: the primary copy of m is in q.
        let far = edges(&g, &l, Tier::Far, &[]);
        assert_eq!(far.len(), 1);
        assert_eq!((far[0].from.id.as_str(), far[0].to.id.as_str()), ("q", "p"));
    }

    #[test]
    fn warning_pairs_become_dashed_warning_edges_at_every_tier() {
        let g = nested();
        let l = layout(&g, &Metrics::default());
        let pairs = vec![
            ("b".to_string(), "c".to_string()),
            ("deep".to_string(), "b".to_string()),
            ("a".to_string(), "b".to_string()),
        ];
        let near = edges(&g, &l, Tier::Near, &pairs);
        let warnings: Vec<&Edge> = near.iter().filter(|e| e.warning).collect();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        let bc = warnings
            .iter()
            .find(|e| e.from.id == "b" && e.to.id == "c")
            .unwrap();
        assert!(bc.labels.is_empty());
        assert_eq!(bc.count, 1);
        // Regular edges are untouched and kept apart from warnings.
        assert_eq!(near.iter().filter(|e| !e.warning).count(), 3);
        // Far: b–c lifts to p→q (one warning beside the bundled real edge);
        // deep–b and a–b lift into p and drop.
        let far = edges(&g, &l, Tier::Far, &pairs);
        assert_eq!(far.len(), 2, "{far:?}");
        let w: Vec<&Edge> = far.iter().filter(|e| e.warning).collect();
        assert_eq!(w.len(), 1);
        assert_eq!((w[0].from.id.as_str(), w[0].to.id.as_str()), ("p", "q"));
        assert_eq!(w[0].count, 1);
        // Unknown ids in a pair are ignored.
        let bad = vec![("ghost".to_string(), "c".to_string())];
        assert_eq!(edges(&g, &l, Tier::Near, &bad).len(), 3);
    }

    #[test]
    fn hit_test_picks_the_deepest_visible_box_and_respects_the_tier() {
        let g = nested();
        let l = layout(&g, &Metrics::default());
        let deep = rect_of(&l, "deep", Some("a"));
        let (x, y) = deep.center();
        assert_eq!(
            hit_test(&l, Tier::Near, x, y).unwrap().key,
            key("deep", Some("a"))
        );
        assert_eq!(
            hit_test(&l, Tier::Mid, x, y).unwrap().key,
            key("a", Some("p"))
        );
        assert_eq!(hit_test(&l, Tier::Far, x, y).unwrap().key, key("p", None));
        // The container's header is the container itself.
        let p = rect_of(&l, "p", None);
        assert_eq!(
            hit_test(&l, Tier::Near, p.x + 1.0, p.y + 1.0).unwrap().key,
            key("p", None)
        );
        assert_eq!(hit_test(&l, Tier::Near, -1.0, -1.0), None);
        assert_eq!(hit_test(&l, Tier::Near, l.bounds.w + 1.0, 0.0), None);
    }

    #[test]
    fn focused_is_the_nearest_level_one_card_never_a_deeper_box() {
        let g = nested();
        let l = layout(&g, &Metrics::default());
        let c = rect_of(&l, "c", Some("q"));
        let (x, y) = c.center();
        assert_eq!(
            focused(&l, Tier::Near, x + 5.0, y - 5.0).unwrap().key,
            key("c", Some("q"))
        );
        assert_eq!(
            focused(&l, Tier::Mid, x, y).unwrap().key,
            key("c", Some("q"))
        );
        // Nothing below the top level is visible at Far.
        assert_eq!(focused(&l, Tier::Far, x, y), None);
        // Only level-1 boxes are drawn as cards: with the centre on deep's
        // placed centre (nearer deep than its card a), the card a still wins,
        // at every tier that shows deep.
        let deep = rect_of(&l, "deep", Some("a"));
        let a = rect_of(&l, "a", Some("p"));
        let (dx, dy) = deep.center();
        let (ax, ay) = a.center();
        assert!((dx - ax).abs() + (dy - ay).abs() > 1.0, "{deep:?} vs {a:?}");
        for tier in [Tier::Mid, Tier::Near, Tier::Close] {
            assert_eq!(
                focused(&l, tier, dx, dy).unwrap().key,
                key("a", Some("p")),
                "{tier:?}"
            );
        }
        assert_eq!(focused(&Layout::default(), Tier::Near, 0.0, 0.0), None);
    }

    #[test]
    fn breadcrumb_lists_the_containing_boxes_outermost_first() {
        let g = nested();
        let l = layout(&g, &Metrics::default());
        let deep = rect_of(&l, "deep", Some("a"));
        let (x, y) = deep.center();
        assert_eq!(breadcrumb(&l, &g, Tier::Near, x, y), vec!["p", "a", "deep"]);
        assert_eq!(breadcrumb(&l, &g, Tier::Mid, x, y), vec!["p", "a"]);
        assert_eq!(breadcrumb(&l, &g, Tier::Far, x, y), vec!["p"]);
        assert!(breadcrumb(&l, &g, Tier::Near, -10.0, -10.0).is_empty());
    }

    #[test]
    fn layout_and_edges_cope_with_hundreds_of_nodes_and_a_thousand_links() {
        let mut files = Vec::new();
        for i in 0..400 {
            let mut extra = String::new();
            if i >= 20 {
                extra.push_str(&format!("parent: n{}\n", i % 20));
            }
            extra.push_str("links:\n");
            for j in 0..3 {
                extra.push_str(&format!("  l{j}: n{}\n", (i * 7 + j * 131) % 400));
            }
            files.push((format!("n{i}"), extra));
        }
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let g = graph(&refs);
        assert_eq!(g.len(), 400);
        let started = std::time::Instant::now();
        let l = layout(&g, &Metrics::default());
        assert_eq!(l.boxes.len(), 400);
        for tier in Tier::ALL {
            let es = edges(&g, &l, tier, &[]);
            assert!(!es.is_empty());
            assert!(es.iter().all(|e| e.from != e.to));
        }
        assert!(started.elapsed().as_secs() < 5);
    }

    // --- rect ---

    #[test]
    fn rect_contains_its_inside_and_edges_centres_and_unions() {
        let r = Rect {
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 50.0,
        };
        assert!(r.contains(10.0, 20.0));
        assert!(r.contains(50.0, 40.0));
        assert!(r.contains(110.0, 70.0));
        assert!(!r.contains(9.9, 40.0));
        assert!(!r.contains(50.0, 70.1));
        assert_eq!(r.center(), (60.0, 45.0));
        let o = Rect {
            x: 0.0,
            y: 60.0,
            w: 20.0,
            h: 20.0,
        };
        assert_eq!(
            r.union(&o),
            Rect {
                x: 0.0,
                y: 20.0,
                w: 110.0,
                h: 60.0
            }
        );
    }

    // --- viewport ---

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6
    }

    #[test]
    fn viewport_zoom_at_keeps_the_point_under_the_cursor_fixed_and_clamps() {
        let v = Viewport::new(800.0, 600.0).pan(30.0, -10.0);
        assert_eq!(v.scale, 1.0);
        assert!(close(v.to_screen(0.0, 0.0), (30.0, -10.0)));
        let world = v.to_world(100.0, 50.0);
        let z = v.zoom_at(100.0, 50.0, 2.0);
        assert!((z.scale - 2.0).abs() < 1e-9);
        assert!(close(z.to_world(100.0, 50.0), world), "{z:?}");
        let (sx, sy) = z.to_screen(world.0, world.1);
        assert!(close((sx, sy), (100.0, 50.0)));
        assert_eq!(v.zoom_at(0.0, 0.0, 1000.0).scale, 8.0);
        assert_eq!(v.zoom_at(0.0, 0.0, 0.0001).scale, 0.05);
        assert_eq!(z.tier(), Tier::Close);
        assert_eq!(v.zoom_at(0.0, 0.0, 0.3).tier(), Tier::Far);
        // set_scale_at_center keeps the world point at the centre.
        let centre = v.center_world();
        let s = v.set_scale_at_center(0.5);
        assert_eq!(s.scale, 0.5);
        assert!(close(s.center_world(), centre), "{s:?}");
    }

    #[test]
    fn viewport_fit_maps_the_rect_inside_the_viewport_minus_the_margin() {
        let v = Viewport::new(800.0, 600.0);
        let r = Rect {
            x: 100.0,
            y: -50.0,
            w: 400.0,
            h: 200.0,
        };
        let f = v.fit(&r, 20.0);
        assert!((f.scale - 1.9).abs() < 1e-9, "{f:?}");
        let (l, t) = f.to_screen(r.x, r.y);
        let (rr, b) = f.to_screen(r.x + r.w, r.y + r.h);
        assert!(l >= 20.0 - 1e-6 && t >= 20.0 - 1e-6, "{f:?}");
        assert!(rr <= 780.0 + 1e-6 && b <= 580.0 + 1e-6, "{f:?}");
        assert!(close(f.center_world(), r.center()), "{f:?}");
        // Degenerate rects never divide by zero: scale 1, centred.
        let empty = v.fit(&Rect::default(), 20.0);
        assert_eq!(empty.scale, 1.0);
        assert!(close(empty.center_world(), (0.0, 0.0)), "{empty:?}");
        let tiny = Viewport::new(10.0, 10.0).fit(&r, 20.0);
        assert_eq!(tiny.scale, 1.0);
        assert!(close(tiny.center_world(), r.center()), "{tiny:?}");
        // A huge rect clamps to the minimum scale.
        let huge = v.fit(
            &Rect {
                x: 0.0,
                y: 0.0,
                w: 1e6,
                h: 1e6,
            },
            0.0,
        );
        assert_eq!(huge.scale, 0.05);
    }
}
