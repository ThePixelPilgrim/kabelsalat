//! The overview's node graph: frontmatter parsing, containment, typed edges,
//! kind styling and the format issues a set of files raises. Pure logic over
//! file contents handed in (`scan_root` is the one thin I/O helper), so every
//! rule is unit-tested without a GUI.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use serde_yaml_ng::{Mapping, Value};

/// The eight colours a kind node may name. `Grey` is the "unlisted status"
/// colour and is never derived for a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    Blue,
    Green,
    Amber,
    Red,
    Purple,
    Pink,
    Teal,
    Grey,
}

impl Color {
    /// Every colour, in the order the spec lists them.
    pub const ALL: [Color; 8] = [
        Color::Blue,
        Color::Green,
        Color::Amber,
        Color::Red,
        Color::Purple,
        Color::Pink,
        Color::Teal,
        Color::Grey,
    ];

    /// The colour a kind node names (`color: amber`). Case and surrounding
    /// whitespace are forgiven; anything else is `None`.
    pub fn parse(name: &str) -> Option<Color> {
        let wanted = name.trim().to_ascii_lowercase();
        Color::ALL.into_iter().find(|c| c.name() == wanted)
    }

    /// The lower-case name of the colour, as written in a kind node.
    pub fn name(self) -> &'static str {
        match self {
            Color::Blue => "blue",
            Color::Green => "green",
            Color::Amber => "amber",
            Color::Red => "red",
            Color::Purple => "purple",
            Color::Pink => "pink",
            Color::Teal => "teal",
            Color::Grey => "grey",
        }
    }

    /// The colour of a kind that has no kind node: a stable function of the
    /// kind's name (FNV-1a, not the std hasher, so it never changes between
    /// Rust releases), never `Grey`.
    pub fn derived(kind_name: &str) -> Color {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in kind_name.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // Everything but the trailing Grey.
        let palette = &Color::ALL[..Color::ALL.len() - 1];
        let index = (hash % palette.len() as u64) as usize;
        palette[index]
    }

    /// Canvas colour, components in `0..=1`.
    pub fn rgb(self) -> (f32, f32, f32) {
        match self {
            Color::Blue => (0.21, 0.52, 0.89),
            Color::Green => (0.18, 0.63, 0.36),
            Color::Amber => (0.90, 0.62, 0.11),
            Color::Red => (0.86, 0.24, 0.24),
            Color::Purple => (0.56, 0.36, 0.84),
            Color::Pink => (0.87, 0.36, 0.65),
            Color::Teal => (0.13, 0.62, 0.62),
            Color::Grey => (0.55, 0.55, 0.57),
        }
    }
}

/// The value of an extra frontmatter key, shown as a field row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    Scalar(String),
    List(Vec<String>),
}

/// One typed, directed edge from the node that declares it to `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub name: String,
    pub target: String,
}

/// A node as read from one file. `parents` and `links` are kept as written,
/// dangling targets included; the [`Graph`] decides what they mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub summary: Option<String>,
    pub status: Option<String>,
    pub parents: Vec<String>,
    pub links: Vec<Link>,
    /// Extra keys in file order.
    pub fields: Vec<(String, FieldValue)>,
    pub body: String,
    pub file: PathBuf,
}

/// How a kind node styles its kind: the box colour and the status colours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindStyle {
    pub color: Color,
    pub statuses: Vec<(String, Color)>,
}

/// What one file turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Node(Node),
    /// A `kind: kind` node; `name` is the kind it styles.
    Kind {
        name: String,
        style: KindStyle,
    },
    /// No frontmatter block, or one without an `id`.
    NotANode,
}

/// Is a line the `---` fence? Trailing spaces, tabs and a `\r` are allowed.
fn is_fence(line: &str) -> bool {
    line.trim_end() == "---"
}

/// Split a file into its frontmatter YAML and the Markdown body. `None` when
/// the text does not start with a `---` line or the block is never closed. A
/// UTF-8 BOM before the first fence is tolerated.
pub fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let first_end = text.find('\n').unwrap_or(text.len());
    if !is_fence(&text[..first_end]) {
        return None;
    }
    if first_end == text.len() {
        // A lone `---` with nothing after it.
        return None;
    }
    let rest = &text[first_end + 1..];
    let mut offset = 0;
    while offset <= rest.len() {
        let line_end = rest[offset..].find('\n').map_or(rest.len(), |i| offset + i);
        let line = &rest[offset..line_end];
        if is_fence(line) {
            let body_start = (line_end + 1).min(rest.len());
            return Some((&rest[..offset], &rest[body_start..]));
        }
        if line_end == rest.len() {
            break;
        }
        offset = line_end + 1;
    }
    None
}

/// Node ids are non-empty and drawn from `A-Z a-z 0-9 . _ -`.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The word that selects a status colour: the first whitespace-separated word
/// of the status, lower-cased (empty for an empty status).
pub fn status_word(status: &str) -> String {
    status
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase()
}

/// A scalar as text: strings as they are, numbers and booleans in their YAML
/// spelling. `None` for null and for anything that is not a scalar.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Tagged(t) => scalar_text(&t.value),
        Value::Null | Value::Sequence(_) | Value::Mapping(_) => None,
    }
}

/// A scalar or a list of scalars as a list of strings; items that are not
/// scalars are skipped. `None` when the value is neither.
fn string_list(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Sequence(items) => Some(items.iter().filter_map(scalar_text).collect()),
        Value::Tagged(t) => string_list(&t.value),
        other => scalar_text(other).map(|s| vec![s]),
    }
}

/// Look a key up by name in a YAML mapping.
fn key<'a>(mapping: &'a Mapping, name: &str) -> Option<&'a Value> {
    mapping.get(name)
}

/// The first `# ` heading of a Markdown body, trimmed.
fn first_heading(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim_start)
        .find_map(|line| line.strip_prefix("# "))
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

/// The links of a node: `links` must be a mapping of edge name to id or list
/// of ids; any other shape yields no links.
fn parse_links(value: Option<&Value>) -> Vec<Link> {
    let Some(Value::Mapping(map)) = value else {
        return Vec::new();
    };
    let mut links = Vec::new();
    for (name, targets) in map {
        let Some(name) = scalar_text(name) else {
            continue;
        };
        let Some(targets) = string_list(targets) else {
            continue;
        };
        links.extend(targets.into_iter().map(|target| Link {
            name: name.clone(),
            target,
        }));
    }
    links
}

/// The `statuses` map of a kind node; a colour that does not parse is Grey.
fn parse_statuses(value: Option<&Value>) -> Vec<(String, Color)> {
    let Some(Value::Mapping(map)) = value else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(word, colour)| {
            let word = scalar_text(word)?;
            let colour = scalar_text(colour)
                .and_then(|c| Color::parse(&c))
                .unwrap_or(Color::Grey);
            Some((word, colour))
        })
        .collect()
}

/// The keys kabelsalat reads; every other key is a field row.
const RESERVED_KEYS: [&str; 7] = [
    "id", "title", "kind", "summary", "status", "parent", "links",
];

/// Parse one file. `Ok(NotANode)` for a file without a frontmatter block or
/// without an `id`; `Err` when a block is present but is not YAML or its `id`
/// is not a valid id (that file is an unreadable-file issue).
pub fn parse_file(file: &Path, text: &str) -> Result<Parsed, String> {
    let Some((yaml, body)) = split_frontmatter(text) else {
        return Ok(Parsed::NotANode);
    };
    let value: Value =
        serde_yaml_ng::from_str(yaml).map_err(|e| format!("frontmatter is not valid YAML: {e}"))?;
    let Value::Mapping(map) = value else {
        return Ok(Parsed::NotANode);
    };
    let Some(id_value) = key(&map, "id") else {
        return Ok(Parsed::NotANode);
    };
    let id = match scalar_text(id_value) {
        Some(id) if is_valid_id(&id) => id,
        Some(id) => return Err(format!("invalid id '{id}': allowed are A-Z a-z 0-9 . _ -")),
        None => return Err("id must be a string".to_string()),
    };
    let kind = key(&map, "kind")
        .and_then(scalar_text)
        .unwrap_or_else(|| "node".to_string());
    let title = key(&map, "title")
        .and_then(scalar_text)
        .or_else(|| first_heading(body))
        .unwrap_or_else(|| id.clone());
    if kind == "kind" {
        let color = key(&map, "color")
            .and_then(scalar_text)
            .and_then(|c| Color::parse(&c))
            .unwrap_or_else(|| Color::derived(&title));
        let statuses = parse_statuses(key(&map, "statuses"));
        return Ok(Parsed::Kind {
            name: title,
            style: KindStyle { color, statuses },
        });
    }
    let fields = map
        .iter()
        .filter_map(|(k, v)| {
            let name = scalar_text(k)?;
            if RESERVED_KEYS.contains(&name.as_str()) {
                return None;
            }
            let value = match v {
                Value::Sequence(_) => FieldValue::List(string_list(v)?),
                other => FieldValue::Scalar(scalar_text(other)?),
            };
            Some((name, value))
        })
        .collect();
    Ok(Parsed::Node(Node {
        id,
        title,
        kind,
        summary: key(&map, "summary").and_then(scalar_text),
        status: key(&map, "status").and_then(scalar_text),
        parents: key(&map, "parent")
            .and_then(string_list)
            .unwrap_or_default(),
        links: parse_links(key(&map, "links")),
        fields,
        body: body.to_string(),
        file: file.to_path_buf(),
    }))
}

/// The kinds of data issue the files alone raise (tags add more, see
/// `overview::issues`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatIssueKind {
    /// A file that could not be read, or whose frontmatter does not parse.
    UnreadableFile,
    /// Two files share an `id`; the first by path wins.
    DuplicateId,
    /// A `parent` or `links` target that does not exist.
    UnknownNode,
    /// Following `parent` returns to the start; the closing entry is ignored.
    ParentCycle,
}

impl FormatIssueKind {
    /// The kebab-case name the CLI prints.
    pub fn label(self) -> &'static str {
        match self {
            FormatIssueKind::UnreadableFile => "unreadable-file",
            FormatIssueKind::DuplicateId => "duplicate-id",
            FormatIssueKind::UnknownNode => "unknown-node",
            FormatIssueKind::ParentCycle => "parent-cycle",
        }
    }
}

/// One data issue: the nodes involved, the file it was found in, a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatIssue {
    pub kind: FormatIssueKind,
    pub nodes: Vec<String>,
    pub file: Option<PathBuf>,
    pub detail: String,
}

/// Raw file contents the graph was built from; `Err` = the file could not be read.
pub type Source = (PathBuf, Result<String, String>);

/// How deep `scan_root` follows directories; a symlink loop ends here.
const MAX_SCAN_DEPTH: usize = 64;

fn scan_dir(dir: &Path, depth: usize, out: &mut Vec<Source>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            out.push((dir.to_path_buf(), Err(e.to_string())));
            return;
        }
    };
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                out.push((dir.to_path_buf(), Err(e.to_string())));
                continue;
            }
        };
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if !name.starts_with('.') && depth < MAX_SCAN_DEPTH {
                scan_dir(&path, depth + 1, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "md") {
            let text = std::fs::read_to_string(&path).map_err(|e| e.to_string());
            out.push((path, text));
        }
    }
}

/// Every `*.md` file below `root`, sorted by path, with its contents (or the
/// read error). Directories whose name starts with `.` are skipped; a
/// directory that cannot be listed (the root included) is one `Err` source.
pub fn scan_root(root: &Path) -> Vec<Source> {
    let mut out = Vec::new();
    scan_dir(root, 0, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The node graph of one overview root. Built once from the file contents
/// and queried by the layout, the canvas and the issue list; `with_source`
/// and `without_source` rebuild it after one file changed.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    nodes: BTreeMap<String, Node>,
    kinds: HashMap<String, KindStyle>,
    /// Surviving parents per node, frontmatter order.
    parents: HashMap<String, Vec<String>>,
    /// Children per node, by id.
    children: HashMap<String, Vec<String>>,
    top_level: Vec<String>,
    depth: HashMap<String, usize>,
    /// Directed `links` edges between existing nodes.
    edges: HashSet<(String, String)>,
    issues: Vec<FormatIssue>,
    sources: Vec<Source>,
}

impl Graph {
    /// Build the graph from file contents. Sources are taken in path order
    /// whatever order they arrive in; a duplicate id keeps the first file.
    pub fn build(mut sources: Vec<Source>) -> Graph {
        sources.sort_by(|a, b| a.0.cmp(&b.0));
        let mut graph = Graph {
            sources,
            ..Graph::default()
        };
        graph.read_sources();
        graph.resolve_parents();
        graph.compute_depths();
        graph.resolve_links();
        graph
    }

    fn report(&mut self, issue: FormatIssue) {
        if !self.issues.contains(&issue) {
            self.issues.push(issue);
        }
    }

    fn read_sources(&mut self) {
        for (file, text) in self.sources.clone() {
            let parsed = match text {
                Err(e) => Err(e),
                Ok(text) => parse_file(&file, &text),
            };
            match parsed {
                Err(detail) => self.report(FormatIssue {
                    kind: FormatIssueKind::UnreadableFile,
                    nodes: Vec::new(),
                    file: Some(file),
                    detail,
                }),
                Ok(Parsed::NotANode) => {}
                Ok(Parsed::Kind { name, style }) => {
                    self.kinds.entry(name).or_insert(style);
                }
                Ok(Parsed::Node(node)) => {
                    if let Some(first) = self.nodes.get(&node.id) {
                        let detail =
                            format!("'{}' is also defined in {}", node.id, first.file.display());
                        self.report(FormatIssue {
                            kind: FormatIssueKind::DuplicateId,
                            nodes: vec![node.id],
                            file: Some(file),
                            detail,
                        });
                    } else {
                        self.nodes.insert(node.id.clone(), node);
                    }
                }
            }
        }
    }

    /// The chain `start → parent → … → target` over surviving parents, when
    /// one exists (`[start]` when start == target).
    fn parent_chain(&self, start: &str, target: &str) -> Option<Vec<String>> {
        let mut previous: HashMap<&str, &str> = HashMap::new();
        let mut queue: VecDeque<&str> = VecDeque::from([start]);
        let mut seen: HashSet<&str> = HashSet::from([start]);
        while let Some(current) = queue.pop_front() {
            if current == target {
                let mut chain = vec![current.to_string()];
                let mut at = current;
                while let Some(&back) = previous.get(at) {
                    chain.push(back.to_string());
                    at = back;
                }
                chain.reverse();
                return Some(chain);
            }
            for parent in self.parents.get(current).into_iter().flatten() {
                if seen.insert(parent) {
                    previous.insert(parent, current);
                    queue.push_back(parent);
                }
            }
        }
        None
    }

    fn resolve_parents(&mut self) {
        let wanted: Vec<(String, Vec<String>, PathBuf)> = self
            .nodes
            .values()
            .map(|n| (n.id.clone(), n.parents.clone(), n.file.clone()))
            .collect();
        for (id, parents, file) in wanted {
            for parent in parents {
                if !self.nodes.contains_key(&parent) {
                    self.report(FormatIssue {
                        kind: FormatIssueKind::UnknownNode,
                        nodes: vec![id.clone(), parent.clone()],
                        file: Some(file.clone()),
                        detail: format!("'{id}' names unknown node '{parent}' as parent"),
                    });
                    continue;
                }
                if self
                    .parents
                    .get(&id)
                    .is_some_and(|existing| existing.contains(&parent))
                {
                    continue;
                }
                // Would `parent` lead back to `id`? Then this entry closes a
                // cycle: `id → parent → … → id`.
                if let Some(chain) = self.parent_chain(&parent, &id) {
                    let mut cycle = vec![id.clone()];
                    cycle.extend(chain.into_iter().filter(|n| *n != id));
                    let detail = format!("following parent from '{id}' leads back to it");
                    self.report(FormatIssue {
                        kind: FormatIssueKind::ParentCycle,
                        nodes: cycle,
                        file: Some(file.clone()),
                        detail,
                    });
                    continue;
                }
                self.parents
                    .entry(id.clone())
                    .or_default()
                    .push(parent.clone());
                self.children.entry(parent).or_default().push(id.clone());
            }
        }
        for children in self.children.values_mut() {
            children.sort();
        }
        self.top_level = self
            .nodes
            .keys()
            .filter(|id| self.parents.get(*id).is_none_or(Vec::is_empty))
            .cloned()
            .collect();
    }

    /// Breadth-first from the top level: the first visit is the shortest
    /// chain. The surviving parent graph is acyclic, so every node is reached.
    fn compute_depths(&mut self) {
        let mut queue: VecDeque<(String, usize)> =
            self.top_level.iter().map(|id| (id.clone(), 0)).collect();
        while let Some((id, depth)) = queue.pop_front() {
            if self.depth.contains_key(&id) {
                continue;
            }
            self.depth.insert(id.clone(), depth);
            for child in self.children.get(&id).into_iter().flatten() {
                if !self.depth.contains_key(child) {
                    queue.push_back((child.clone(), depth + 1));
                }
            }
        }
    }

    fn resolve_links(&mut self) {
        let wanted: Vec<(String, Vec<Link>, PathBuf)> = self
            .nodes
            .values()
            .map(|n| (n.id.clone(), n.links.clone(), n.file.clone()))
            .collect();
        for (id, links, file) in wanted {
            for link in links {
                if self.nodes.contains_key(&link.target) {
                    self.edges.insert((id.clone(), link.target));
                } else {
                    self.report(FormatIssue {
                        kind: FormatIssueKind::UnknownNode,
                        nodes: vec![id.clone(), link.target.clone()],
                        file: Some(file.clone()),
                        detail: format!(
                            "'{id}' links to unknown node '{}' under '{}'",
                            link.target, link.name
                        ),
                    });
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// Every drawn node, by id (kind nodes are not among them).
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    /// Ids without a surviving parent, by id.
    pub fn top_level(&self) -> &[String] {
        &self.top_level
    }

    /// Children by id, after cycle breaking.
    pub fn children(&self, id: &str) -> &[String] {
        self.children.get(id).map_or(&[], Vec::as_slice)
    }

    /// Surviving parents in frontmatter order (unknown targets and the
    /// cycle-closing entry removed).
    pub fn parents(&self, id: &str) -> &[String] {
        self.parents.get(id).map_or(&[], Vec::as_slice)
    }

    /// Length of the shortest surviving parent chain; 0 at top level and for
    /// unknown ids.
    pub fn depth(&self, id: &str) -> usize {
        self.depth.get(id).copied().unwrap_or(0)
    }

    /// Every node below `id` through any copy, each once, by id, `id` excluded.
    pub fn descendants(&self, id: &str) -> Vec<String> {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut queue: VecDeque<&str> = VecDeque::from([id]);
        while let Some(current) = queue.pop_front() {
            for child in self.children(current) {
                if seen.insert(child) {
                    queue.push_back(child);
                }
            }
        }
        seen.remove(id);
        let mut out: Vec<String> = seen.into_iter().map(str::to_string).collect();
        out.sort();
        out
    }

    /// The colour of a kind: its kind node's, else derived from the name.
    pub fn kind_color(&self, kind: &str) -> Color {
        self.kinds
            .get(kind)
            .map_or_else(|| Color::derived(kind), |style| style.color)
    }

    /// The colour of a node's status: `None` without a status, the kind
    /// node's colour for the status's first word, Grey when unlisted (or
    /// when the kind has no kind node).
    pub fn status_color(&self, node: &Node) -> Option<Color> {
        let word = status_word(node.status.as_deref()?);
        let listed = self.kinds.get(&node.kind).and_then(|style| {
            style
                .statuses
                .iter()
                .find(|(status, _)| *status == word)
                .map(|(_, colour)| *colour)
        });
        Some(listed.unwrap_or(Color::Grey))
    }

    /// Is there a `links` edge between the two nodes in either direction,
    /// under any name?
    pub fn has_edge(&self, a: &str, b: &str) -> bool {
        self.edges.contains(&(a.to_string(), b.to_string()))
            || self.edges.contains(&(b.to_string(), a.to_string()))
    }

    /// The node a link in a rendered body points at: `href` is a node id, or
    /// a path to a node's file — absolute, relative to any ancestor directory
    /// (a path suffix), or just the file name — with or without the `.md`
    /// extension, a leading `./`, or a `#fragment`. URLs with a scheme and
    /// unknown targets give `None`.
    pub fn node_for_href(&self, href: &str) -> Option<&Node> {
        let href = href.split(['#', '?']).next().unwrap_or("");
        if href.is_empty() || href.contains("://") {
            return None;
        }
        if let Some(node) = self.nodes.get(href) {
            return Some(node);
        }
        let target = Path::new(href.strip_prefix("./").unwrap_or(href));
        let with_md = target.with_extension("md");
        let wanted = if target.extension().is_some() {
            target
        } else {
            with_md.as_path()
        };
        self.nodes.values().find(|n| n.file.ends_with(wanted))
    }

    pub fn issues(&self) -> &[FormatIssue] {
        &self.issues
    }

    /// What the graph was built from, sorted by path.
    pub fn sources(&self) -> &[Source] {
        &self.sources
    }

    /// The graph with one file's contents replaced (or added) and rebuilt.
    pub fn with_source(&self, source: Source) -> Graph {
        let mut sources: Vec<Source> = self
            .sources
            .iter()
            .filter(|(path, _)| *path != source.0)
            .cloned()
            .collect();
        sources.push(source);
        Graph::build(sources)
    }

    /// The graph without one file, rebuilt.
    pub fn without_source(&self, file: &Path) -> Graph {
        Graph::build(
            self.sources
                .iter()
                .filter(|(path, _)| path != file)
                .cloned()
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    // --- colours, ids, status words ---

    #[test]
    fn colour_names_round_trip_and_unknown_names_are_rejected() {
        for c in [
            Color::Blue,
            Color::Green,
            Color::Amber,
            Color::Red,
            Color::Purple,
            Color::Pink,
            Color::Teal,
            Color::Grey,
        ] {
            assert_eq!(Color::parse(c.name()), Some(c));
        }
        assert_eq!(Color::parse("amber"), Some(Color::Amber));
        assert_eq!(Color::parse(" Amber "), Some(Color::Amber));
        assert_eq!(Color::parse("mauve"), None);
        assert_eq!(Color::parse(""), None);
    }

    #[test]
    fn derived_colour_is_stable_and_never_grey() {
        for name in ["adr", "theme", "epic", "x", "", "a-very-long-kind-name"] {
            let c = Color::derived(name);
            assert_eq!(c, Color::derived(name), "{name}");
            assert_ne!(c, Color::Grey, "{name}");
        }
        // Different names spread over the palette (not all the same colour).
        let distinct: std::collections::HashSet<Color> = (0..40)
            .map(|i| Color::derived(&format!("kind-{i}")))
            .collect();
        assert!(distinct.len() > 3, "{distinct:?}");
    }

    #[test]
    fn rgb_components_are_unit_range() {
        for c in [Color::Blue, Color::Grey, Color::Pink] {
            let (r, g, b) = c.rgb();
            for v in [r, g, b] {
                assert!((0.0..=1.0).contains(&v));
            }
        }
    }

    #[test]
    fn valid_ids_are_non_empty_and_restricted_to_the_spec_alphabet() {
        assert!(is_valid_id("adr-118"));
        assert!(is_valid_id("P-004"));
        assert!(is_valid_id("a.b_c"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("has space"));
        assert!(!is_valid_id("slash/x"));
        assert!(!is_valid_id("ümlaut"));
    }

    #[test]
    fn status_word_is_the_first_word_lower_cased() {
        assert_eq!(status_word("Accepted"), "accepted");
        assert_eq!(status_word("  In Progress since May"), "in");
        assert_eq!(status_word("proposed"), "proposed");
        assert_eq!(status_word(""), "");
        assert_eq!(status_word("   "), "");
    }

    // --- frontmatter ---

    #[test]
    fn split_frontmatter_returns_yaml_and_body() {
        let text = "---\nid: a\nkind: adr\n---\n# A\n\nbody\n";
        assert_eq!(
            split_frontmatter(text),
            Some(("id: a\nkind: adr\n", "# A\n\nbody\n"))
        );
    }

    #[test]
    fn split_frontmatter_tolerates_bom_crlf_and_trailing_spaces() {
        let text = "\u{feff}---  \r\nid: a\r\n--- \r\nbody\r\n";
        assert_eq!(split_frontmatter(text), Some(("id: a\r\n", "body\r\n")));
    }

    #[test]
    fn split_frontmatter_rejects_files_without_a_leading_block() {
        assert_eq!(split_frontmatter("# Title\n---\nid: a\n---\n"), None);
        assert_eq!(split_frontmatter(""), None);
        assert_eq!(split_frontmatter("----\nid: a\n---\n"), None);
        // An unterminated block is not a block.
        assert_eq!(split_frontmatter("---\nid: a\n"), None);
    }

    #[test]
    fn split_frontmatter_handles_an_empty_block_and_a_missing_body() {
        assert_eq!(split_frontmatter("---\n---"), Some(("", "")));
        assert_eq!(split_frontmatter("---\nid: a\n---"), Some(("id: a\n", "")));
    }

    // --- parse_file ---

    fn node(text: &str) -> Node {
        match parse_file(Path::new("/root/x.md"), text) {
            Ok(Parsed::Node(n)) => n,
            other => panic!("expected a node, got {other:?}"),
        }
    }

    #[test]
    fn parse_file_reads_every_key_of_the_spec_example() {
        let text = "---\nid: adr-118\nkind: adr\ntitle: Emergency access account\n\
summary: controlled recovery when all admins are locked out\nstatus: proposed\n\
parent: [theme-access, P-004]\nlinks:\n  builds-on: [adr-064]\n  related: adr-122\n\
owner: ops\ntags: [security, access]\n---\n# ADR-118: Emergency access account\n\n## Context\n";
        let n = node(text);
        assert_eq!(n.id, "adr-118");
        assert_eq!(n.kind, "adr");
        assert_eq!(n.title, "Emergency access account");
        assert_eq!(
            n.summary.as_deref(),
            Some("controlled recovery when all admins are locked out")
        );
        assert_eq!(n.status.as_deref(), Some("proposed"));
        assert_eq!(n.parents, vec!["theme-access", "P-004"]);
        assert_eq!(
            n.links,
            vec![
                Link {
                    name: "builds-on".into(),
                    target: "adr-064".into()
                },
                Link {
                    name: "related".into(),
                    target: "adr-122".into()
                },
            ]
        );
        assert_eq!(
            n.fields,
            vec![
                ("owner".to_string(), FieldValue::Scalar("ops".into())),
                (
                    "tags".to_string(),
                    FieldValue::List(vec!["security".into(), "access".into()])
                ),
            ]
        );
        assert_eq!(
            n.body,
            "# ADR-118: Emergency access account\n\n## Context\n"
        );
        assert_eq!(n.file, p("/root/x.md"));
    }

    #[test]
    fn parse_file_defaults_kind_title_and_optional_keys() {
        let n = node("---\nid: n1\n---\n\n# Heading one\n# Heading two\n");
        assert_eq!(n.kind, "node");
        assert_eq!(n.title, "Heading one");
        assert_eq!(n.summary, None);
        assert_eq!(n.status, None);
        assert!(n.parents.is_empty());
        assert!(n.links.is_empty());
        assert!(n.fields.is_empty());
        // No heading: the id names the node.
        let n = node("---\nid: n2\n---\nplain text\n## not a top heading\n");
        assert_eq!(n.title, "n2");
    }

    #[test]
    fn parse_file_stringifies_non_string_scalars_and_accepts_scalar_or_list() {
        let n = node(
            "---\nid: 42\nparent: 7\nlinks:\n  sees: [1.5, true, x]\n  broken: {a: b}\n  alone: y\nn: 3\ncolor: red\n---\n",
        );
        assert_eq!(n.id, "42");
        assert_eq!(n.parents, vec!["7"]);
        let targets: Vec<&str> = n.links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, vec!["1.5", "true", "x", "y"]);
        // `color` means something only on a kind node; here it is a field.
        assert_eq!(
            n.fields,
            vec![
                ("n".to_string(), FieldValue::Scalar("3".into())),
                ("color".to_string(), FieldValue::Scalar("red".into())),
            ]
        );
    }

    #[test]
    fn parse_file_ignores_a_links_value_that_is_not_a_mapping_and_nested_extra_keys() {
        let n = node(
            "---\nid: a\nlinks: [b, c]\nmeta:\n  deep: 1\nlist: [1, {x: y}, 2]\nempty:\n---\n",
        );
        assert!(n.links.is_empty());
        assert_eq!(
            n.fields,
            vec![(
                "list".to_string(),
                FieldValue::List(vec!["1".into(), "2".into()])
            )]
        );
    }

    #[test]
    fn parse_file_reports_not_a_node_without_block_or_id() {
        assert_eq!(
            parse_file(Path::new("/r/a.md"), "# Just notes\n"),
            Ok(Parsed::NotANode)
        );
        assert_eq!(
            parse_file(Path::new("/r/a.md"), "---\ntitle: no id here\n---\n"),
            Ok(Parsed::NotANode)
        );
        assert_eq!(
            parse_file(Path::new("/r/a.md"), "---\n---\nbody\n"),
            Ok(Parsed::NotANode)
        );
        assert_eq!(
            parse_file(Path::new("/r/a.md"), "---\n- a list\n---\n"),
            Ok(Parsed::NotANode)
        );
    }

    #[test]
    fn parse_file_errors_on_bad_yaml_or_invalid_id() {
        let bad = parse_file(Path::new("/r/a.md"), "---\nid: [unclosed\n---\n");
        assert!(bad.is_err(), "{bad:?}");
        let bad_id = parse_file(Path::new("/r/a.md"), "---\nid: has space\n---\n");
        assert!(
            matches!(bad_id, Err(ref m) if m.contains("has space")),
            "{bad_id:?}"
        );
        let list_id = parse_file(Path::new("/r/a.md"), "---\nid: [a, b]\n---\n");
        assert!(list_id.is_err(), "{list_id:?}");
        let null_id = parse_file(Path::new("/r/a.md"), "---\nid:\n---\n");
        assert!(null_id.is_err(), "{null_id:?}");
    }

    #[test]
    fn parse_file_reads_kind_nodes_with_colour_and_statuses() {
        let text = "---\nid: kind-adr\nkind: kind\ntitle: adr\ncolor: amber\nstatuses:\n  accepted: green\n  proposed: amber\n  odd: mauve\n---\n";
        assert_eq!(
            parse_file(Path::new("/r/k.md"), text),
            Ok(Parsed::Kind {
                name: "adr".into(),
                style: KindStyle {
                    color: Color::Amber,
                    statuses: vec![
                        ("accepted".into(), Color::Green),
                        ("proposed".into(), Color::Amber),
                        ("odd".into(), Color::Grey),
                    ],
                },
            })
        );
    }

    #[test]
    fn kind_node_without_colour_or_statuses_gets_a_derived_colour() {
        let text = "---\nid: kind-x\nkind: kind\ntitle: epic\ncolor: nope\nstatuses: [a, b]\n---\n";
        match parse_file(Path::new("/r/k.md"), text) {
            Ok(Parsed::Kind { name, style }) => {
                assert_eq!(name, "epic");
                assert_eq!(style.color, Color::derived("epic"));
                assert!(style.statuses.is_empty());
            }
            other => panic!("{other:?}"),
        }
        // The kind's name falls back like a node title: heading, then id.
        match parse_file(
            Path::new("/r/k.md"),
            "---\nid: kind-y\nkind: kind\n---\n# story\n",
        ) {
            Ok(Parsed::Kind { name, .. }) => assert_eq!(name, "story"),
            other => panic!("{other:?}"),
        }
    }

    // --- Graph ---

    fn src(path: &str, text: &str) -> Source {
        (p(path), Ok(text.to_string()))
    }

    fn fm(id: &str, extra: &str) -> String {
        format!("---\nid: {id}\n{extra}---\n")
    }

    fn ids(list: &[String]) -> Vec<&str> {
        list.iter().map(String::as_str).collect()
    }

    fn issues_of(graph: &Graph, kind: FormatIssueKind) -> Vec<&FormatIssue> {
        graph.issues().iter().filter(|i| i.kind == kind).collect()
    }

    /// A small tree: root > (a > a1, b), plus an orphan top-level node.
    fn tree() -> Graph {
        Graph::build(vec![
            src("/r/b.md", &fm("b", "parent: root\nlinks:\n  blocks: a1\n")),
            src(
                "/r/a1.md",
                &fm("a1", "parent: a\nkind: adr\nstatus: Accepted now\n"),
            ),
            src("/r/a.md", &fm("a", "parent: [root]\n")),
            src("/r/root.md", &fm("root", "")),
            src("/r/zz.md", &fm("zz", "kind: adr\nstatus: weird\n")),
            src("/r/notes.md", "# just notes\n"),
            src(
                "/r/kind-adr.md",
                &fm(
                    "kind-adr",
                    "kind: kind\ntitle: adr\ncolor: amber\nstatuses:\n  accepted: green\n",
                ),
            ),
        ])
    }

    #[test]
    fn empty_graph_has_nothing() {
        let g = Graph::build(Vec::new());
        assert!(g.is_empty());
        assert_eq!(g.len(), 0);
        assert!(g.top_level().is_empty());
        assert!(g.issues().is_empty());
        assert_eq!(g.children("nope"), &[] as &[String]);
        assert_eq!(g.parents("nope"), &[] as &[String]);
        assert_eq!(g.depth("nope"), 0);
        assert_eq!(g.node("nope"), None);
        assert!(g.descendants("nope").is_empty());
    }

    #[test]
    fn build_orders_nodes_by_id_and_leaves_out_kind_nodes_and_plain_files() {
        let g = tree();
        assert!(!g.is_empty());
        assert_eq!(g.len(), 5);
        let listed: Vec<&str> = g.nodes().map(|n| n.id.as_str()).collect();
        assert_eq!(listed, vec!["a", "a1", "b", "root", "zz"]);
        assert_eq!(g.node("kind-adr"), None);
        assert_eq!(g.node("a1").map(|n| n.kind.as_str()), Some("adr"));
        assert!(g.issues().is_empty(), "{:?}", g.issues());
    }

    #[test]
    fn containment_children_parents_top_level_and_depth() {
        let g = tree();
        assert_eq!(ids(g.top_level()), vec!["root", "zz"]);
        assert_eq!(ids(g.children("root")), vec!["a", "b"]);
        assert_eq!(ids(g.children("a")), vec!["a1"]);
        assert!(g.children("a1").is_empty());
        assert_eq!(ids(g.parents("a1")), vec!["a"]);
        assert!(g.parents("root").is_empty());
        assert_eq!(g.depth("root"), 0);
        assert_eq!(g.depth("zz"), 0);
        assert_eq!(g.depth("a"), 1);
        assert_eq!(g.depth("a1"), 2);
    }

    #[test]
    fn descendants_are_distinct_and_exclude_the_node() {
        let g = tree();
        assert_eq!(g.descendants("root"), vec!["a", "a1", "b"]);
        assert_eq!(g.descendants("a"), vec!["a1"]);
        assert!(g.descendants("a1").is_empty());
    }

    #[test]
    fn duplicate_ids_keep_the_first_file_by_path_and_report_the_rest() {
        let g = Graph::build(vec![
            src("/r/z-second.md", &fm("dup", "title: second\n")),
            src("/r/a-first.md", &fm("dup", "title: first\n")),
            src("/r/m-third.md", &fm("dup", "title: third\n")),
        ]);
        assert_eq!(g.len(), 1);
        assert_eq!(g.node("dup").map(|n| n.title.as_str()), Some("first"));
        let dups = issues_of(&g, FormatIssueKind::DuplicateId);
        assert_eq!(dups.len(), 2, "{dups:?}");
        assert_eq!(dups[0].nodes, vec!["dup"]);
        assert_eq!(dups[0].file.as_deref(), Some(Path::new("/r/m-third.md")));
        assert!(dups[0].detail.contains("a-first.md"), "{}", dups[0].detail);
        assert_eq!(dups[1].file.as_deref(), Some(Path::new("/r/z-second.md")));
    }

    #[test]
    fn unknown_parent_and_link_targets_are_reported_and_have_no_effect() {
        let g = Graph::build(vec![
            src(
                "/r/a.md",
                &fm("a", "parent: ghost\nlinks:\n  uses: [b, phantom]\n"),
            ),
            src("/r/b.md", &fm("b", "")),
        ]);
        // The dangling references stay on the node …
        let a = g.node("a").unwrap();
        assert_eq!(a.parents, vec!["ghost"]);
        assert_eq!(a.links.len(), 2);
        // … but do not shape the graph.
        assert_eq!(ids(g.top_level()), vec!["a", "b"]);
        assert!(g.parents("a").is_empty());
        assert!(g.children("ghost").is_empty());
        assert!(g.has_edge("a", "b"));
        assert!(!g.has_edge("a", "phantom"));
        let unknown = issues_of(&g, FormatIssueKind::UnknownNode);
        assert_eq!(unknown.len(), 2, "{unknown:?}");
        assert_eq!(unknown[0].nodes, vec!["a", "ghost"]);
        assert_eq!(unknown[0].file.as_deref(), Some(Path::new("/r/a.md")));
        assert!(
            unknown[0].detail.contains("parent"),
            "{}",
            unknown[0].detail
        );
        assert_eq!(unknown[1].nodes, vec!["a", "phantom"]);
        assert!(unknown[1].detail.contains("uses"), "{}", unknown[1].detail);
    }

    #[test]
    fn a_parent_cycle_of_two_drops_the_closing_entry_and_is_reported_once() {
        let g = Graph::build(vec![
            src("/r/a.md", &fm("a", "parent: b\n")),
            src("/r/b.md", &fm("b", "parent: a\n")),
        ]);
        // `a` is processed first (id order): a→b survives, b→a closes the cycle.
        assert_eq!(ids(g.parents("a")), vec!["b"]);
        assert!(g.parents("b").is_empty());
        assert_eq!(ids(g.top_level()), vec!["b"]);
        assert_eq!(ids(g.children("b")), vec!["a"]);
        assert_eq!(g.depth("b"), 0);
        assert_eq!(g.depth("a"), 1);
        // The node keeps what the file says.
        assert_eq!(g.node("b").unwrap().parents, vec!["a"]);
        let cycles = issues_of(&g, FormatIssueKind::ParentCycle);
        assert_eq!(cycles.len(), 1, "{cycles:?}");
        assert_eq!(cycles[0].nodes, vec!["b", "a"]);
        assert_eq!(cycles[0].file.as_deref(), Some(Path::new("/r/b.md")));
    }

    #[test]
    fn a_parent_cycle_of_three_keeps_depth_finite_and_is_reported_once() {
        let g = Graph::build(vec![
            src("/r/a.md", &fm("a", "parent: b\n")),
            src("/r/b.md", &fm("b", "parent: c\n")),
            src("/r/c.md", &fm("c", "parent: a\n")),
        ]);
        assert_eq!(ids(g.top_level()), vec!["c"]);
        assert_eq!(g.depth("c"), 0);
        assert_eq!(g.depth("b"), 1);
        assert_eq!(g.depth("a"), 2);
        assert_eq!(g.descendants("c"), vec!["a", "b"]);
        let cycles = issues_of(&g, FormatIssueKind::ParentCycle);
        assert_eq!(cycles.len(), 1, "{cycles:?}");
        assert_eq!(cycles[0].nodes, vec!["c", "a", "b"]);
        assert_eq!(g.issues().len(), 1);
    }

    #[test]
    fn a_node_that_is_its_own_parent_is_a_cycle_of_one() {
        let g = Graph::build(vec![src("/r/a.md", &fm("a", "parent: [a, a]\n"))]);
        assert!(g.parents("a").is_empty());
        assert_eq!(ids(g.top_level()), vec!["a"]);
        let cycles = issues_of(&g, FormatIssueKind::ParentCycle);
        assert_eq!(cycles.len(), 1, "{cycles:?}");
        assert_eq!(cycles[0].nodes, vec!["a"]);
    }

    #[test]
    fn several_parents_list_the_child_under_each_and_depth_is_the_shortest() {
        let g = Graph::build(vec![
            src("/r/top.md", &fm("top", "")),
            src("/r/mid.md", &fm("mid", "parent: top\n")),
            src("/r/deep.md", &fm("deep", "parent: mid\n")),
            src("/r/x.md", &fm("x", "parent: [deep, top]\n")),
            src("/r/leaf.md", &fm("leaf", "parent: x\n")),
        ]);
        assert_eq!(ids(g.parents("x")), vec!["deep", "top"]);
        assert_eq!(ids(g.children("top")), vec!["mid", "x"]);
        assert_eq!(ids(g.children("deep")), vec!["x"]);
        assert_eq!(g.depth("x"), 1);
        assert_eq!(g.depth("leaf"), 2);
        // Through both copies, once each.
        assert_eq!(g.descendants("top"), vec!["deep", "leaf", "mid", "x"]);
        assert_eq!(g.descendants("mid"), vec!["deep", "leaf", "x"]);
        assert!(g.issues().is_empty());
    }

    #[test]
    fn kind_colours_come_from_kind_nodes_or_are_derived() {
        let g = tree();
        assert_eq!(g.kind_color("adr"), Color::Amber);
        assert_eq!(g.kind_color("node"), Color::derived("node"));
        assert_eq!(g.kind_color("theme"), Color::derived("theme"));
        assert_ne!(g.kind_color("theme"), Color::Grey);
    }

    #[test]
    fn status_colours_follow_the_kind_node_and_fall_back_to_grey() {
        let g = tree();
        // "Accepted now" → "accepted" → green.
        assert_eq!(g.status_color(g.node("a1").unwrap()), Some(Color::Green));
        // Unlisted status of a styled kind.
        assert_eq!(g.status_color(g.node("zz").unwrap()), Some(Color::Grey));
        // No status at all.
        assert_eq!(g.status_color(g.node("a").unwrap()), None);
        // A kind without a kind node has no status list: grey.
        let g2 = Graph::build(vec![src("/r/n.md", &fm("n", "status: open\n"))]);
        assert_eq!(g2.status_color(g2.node("n").unwrap()), Some(Color::Grey));
    }

    #[test]
    fn has_edge_is_symmetric_and_ignores_edge_names() {
        let g = tree();
        assert!(g.has_edge("b", "a1"));
        assert!(g.has_edge("a1", "b"));
        assert!(!g.has_edge("a", "b"));
        assert!(!g.has_edge("root", "a"), "containment is not an edge");
        assert!(!g.has_edge("b", "b"));
    }

    #[test]
    fn unreadable_sources_and_unparsable_frontmatter_are_issues_with_the_file() {
        let g = Graph::build(vec![
            (p("/r/locked.md"), Err("permission denied".to_string())),
            src("/r/bad.md", "---\nid: [\n---\n"),
            src("/r/bad-id.md", "---\nid: 'no good'\n---\n"),
            src("/r/ok.md", &fm("ok", "")),
        ]);
        assert_eq!(g.len(), 1);
        let bad = issues_of(&g, FormatIssueKind::UnreadableFile);
        assert_eq!(bad.len(), 3, "{bad:?}");
        assert_eq!(bad[0].file.as_deref(), Some(Path::new("/r/bad-id.md")));
        assert!(bad[0].nodes.is_empty());
        assert!(bad[0].detail.contains("no good"), "{}", bad[0].detail);
        assert_eq!(bad[1].file.as_deref(), Some(Path::new("/r/bad.md")));
        assert_eq!(bad[2].file.as_deref(), Some(Path::new("/r/locked.md")));
        assert_eq!(bad[2].detail, "permission denied");
    }

    #[test]
    fn issue_kind_labels_match_the_cli_vocabulary() {
        assert_eq!(FormatIssueKind::UnreadableFile.label(), "unreadable-file");
        assert_eq!(FormatIssueKind::DuplicateId.label(), "duplicate-id");
        assert_eq!(FormatIssueKind::UnknownNode.label(), "unknown-node");
        assert_eq!(FormatIssueKind::ParentCycle.label(), "parent-cycle");
    }

    #[test]
    fn with_source_replaces_or_adds_one_file_and_rebuilds() {
        let g = tree();
        assert_eq!(g.sources().len(), 7);
        // Replace: a1 moves under b.
        let g2 = g.with_source(src("/r/a1.md", &fm("a1", "parent: b\n")));
        assert_eq!(g2.sources().len(), 7);
        assert_eq!(ids(g2.children("b")), vec!["a1"]);
        assert!(g2.children("a").is_empty());
        // The original is untouched.
        assert_eq!(ids(g.children("a")), vec!["a1"]);
        // Add: a new file, sorted into place and part of the graph.
        let g3 = g2.with_source(src("/r/c.md", &fm("c", "parent: root\n")));
        assert_eq!(g3.sources().len(), 8);
        assert_eq!(ids(g3.children("root")), vec!["a", "b", "c"]);
        let paths: Vec<&Path> = g3.sources().iter().map(|(p, _)| p.as_path()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn without_source_removes_one_file_and_rebuilds() {
        let g = tree();
        let g2 = g.without_source(Path::new("/r/a.md"));
        assert_eq!(g2.sources().len(), 6);
        assert_eq!(g2.node("a"), None);
        // a1 now points at a node that is gone.
        assert_eq!(ids(g2.top_level()), vec!["a1", "root", "zz"]);
        let unknown = issues_of(&g2, FormatIssueKind::UnknownNode);
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].nodes, vec!["a1", "a"]);
        // Removing an unknown file changes nothing.
        assert_eq!(g.without_source(Path::new("/r/none.md")).len(), g.len());
    }

    #[test]
    fn build_copes_with_a_few_hundred_nodes_in_a_deep_chain() {
        let mut sources = Vec::new();
        for i in 0..400 {
            let extra = if i == 0 {
                String::new()
            } else {
                format!("parent: n{}\nlinks:\n  next: n{}\n", i - 1, (i + 1) % 400)
            };
            sources.push(src(
                &format!("/r/n{i:03}.md"),
                &fm(&format!("n{i}"), &extra),
            ));
        }
        let started = std::time::Instant::now();
        let g = Graph::build(sources);
        assert!(started.elapsed().as_secs() < 5);
        assert_eq!(g.len(), 400);
        assert_eq!(g.depth("n399"), 399);
        assert_eq!(g.descendants("n0").len(), 399);
        assert!(g.has_edge("n399", "n0"));
        assert!(g.issues().is_empty());
    }

    // --- scan_root ---

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kabelsalat-model-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_root_walks_md_files_sorted_and_skips_dot_dirs_and_other_files() {
        let dir = tmp_dir("scan");
        std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join(".obsidian/plugins")).unwrap();
        std::fs::write(dir.join("b.md"), "---\nid: b\n---\n").unwrap();
        std::fs::write(dir.join("a.md"), "---\nid: a\n---\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "not markdown").unwrap();
        std::fs::write(dir.join("README"), "no extension").unwrap();
        std::fs::write(dir.join("sub/c.md"), "---\nid: c\n---\n").unwrap();
        std::fs::write(dir.join("sub/deeper/d.md"), "plain\n").unwrap();
        std::fs::write(dir.join(".git/HEAD.md"), "---\nid: hidden\n---\n").unwrap();
        std::fs::write(
            dir.join(".obsidian/plugins/x.md"),
            "---\nid: hidden2\n---\n",
        )
        .unwrap();
        // Not UTF-8: readable as bytes, not as text.
        std::fs::write(dir.join("binary.md"), [0xff, 0xfe, 0x00, 0x80]).unwrap();

        let sources = scan_root(&dir);
        let paths: Vec<PathBuf> = sources
            .iter()
            .map(|(p, _)| p.strip_prefix(&dir).unwrap().to_path_buf())
            .collect();
        assert_eq!(
            paths,
            vec![
                p("a.md"),
                p("b.md"),
                p("binary.md"),
                p("sub/c.md"),
                p("sub/deeper/d.md"),
            ]
        );
        assert_eq!(sources[0].1.as_deref(), Ok("---\nid: a\n---\n"));
        assert_eq!(sources[4].1.as_deref(), Ok("plain\n"));
        assert!(sources[2].1.is_err(), "{:?}", sources[2].1);

        let g = Graph::build(sources);
        let listed: Vec<&str> = g.nodes().map(|n| n.id.as_str()).collect();
        assert_eq!(listed, vec!["a", "b", "c"]);
        let bad = issues_of(&g, FormatIssueKind::UnreadableFile);
        assert_eq!(bad.len(), 1);
        assert_eq!(
            bad[0].file.as_deref(),
            Some(dir.join("binary.md").as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_root_of_a_missing_directory_reports_it_as_one_unreadable_source() {
        let dir = tmp_dir("scan-missing").join("nope");
        let sources = scan_root(&dir);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, dir);
        assert!(sources[0].1.is_err());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    // --- links in rendered bodies ---

    #[test]
    fn body_link_targets_resolve_to_nodes_by_id_or_file() {
        let graph = Graph::build(vec![
            src("/r/docs/a.md", &fm("A-1", "")),
            src("/r/docs/sub/b.md", &fm("B-2", "")),
        ]);
        fn id(n: Option<&Node>) -> Option<&str> {
            n.map(|n| n.id.as_str())
        }
        assert_eq!(id(graph.node_for_href("A-1")), Some("A-1"));
        assert_eq!(id(graph.node_for_href("b.md")), Some("B-2"));
        assert_eq!(id(graph.node_for_href("sub/b.md")), Some("B-2"));
        assert_eq!(id(graph.node_for_href("./sub/b")), Some("B-2"));
        assert_eq!(id(graph.node_for_href("/r/docs/a.md")), Some("A-1"));
        assert_eq!(id(graph.node_for_href("a.md#context")), Some("A-1"));
        assert_eq!(id(graph.node_for_href("https://example.org/a.md")), None);
        assert_eq!(id(graph.node_for_href("c.md")), None);
        assert_eq!(id(graph.node_for_href("")), None);
    }
}
