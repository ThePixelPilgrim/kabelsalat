//! Data issues of an overview: the graph's format issues plus what the tabs'
//! tags add (unknown nodes, missing links), the prompt that resolves one, and
//! the line `kabelsalat overview issues` prints. Pure over a [`Graph`] and the
//! tags the GUI holds.

use std::path::{Path, PathBuf};

use super::model::{FormatIssueKind, Graph};
use super::tagger::Tags;

/// Every kind of data issue the overview shows: the four the files raise
/// (see [`FormatIssueKind`]) and the one the tags add.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueKind {
    UnreadableFile,
    DuplicateId,
    UnknownNode,
    ParentCycle,
    /// A tab links two nodes that have no `links` edge in either direction.
    MissingLink,
}

impl IssueKind {
    /// The kebab-case name the CLI prints and the tray shows.
    pub fn label(self) -> &'static str {
        match self {
            IssueKind::UnreadableFile => "unreadable-file",
            IssueKind::DuplicateId => "duplicate-id",
            IssueKind::UnknownNode => "unknown-node",
            IssueKind::ParentCycle => "parent-cycle",
            IssueKind::MissingLink => "missing-link",
        }
    }

    /// The issue kind of a format issue.
    pub fn from_format(k: FormatIssueKind) -> IssueKind {
        match k {
            FormatIssueKind::UnreadableFile => IssueKind::UnreadableFile,
            FormatIssueKind::DuplicateId => IssueKind::DuplicateId,
            FormatIssueKind::UnknownNode => IssueKind::UnknownNode,
            FormatIssueKind::ParentCycle => IssueKind::ParentCycle,
        }
    }
}

/// One data issue: the nodes involved, the file it was found in (none for
/// an issue the tags raise), a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub kind: IssueKind,
    pub nodes: Vec<String>,
    pub file: Option<PathBuf>,
    pub detail: String,
}

/// A tab's current tags, as the GUI knows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggedTab {
    pub uuid: String,
    pub name: String,
    pub tags: Tags,
}

/// The distinct node ids a tab is tagged with, in tag order.
fn linked_ids(tab: &TaggedTab) -> Vec<&str> {
    let mut ids: Vec<&str> = Vec::new();
    for link in &tab.tags.links {
        if !ids.contains(&link.node.as_str()) {
            ids.push(&link.node);
        }
    }
    ids
}

/// Append `issue` unless one of the same kind over the same nodes is there.
fn push_unique(out: &mut Vec<Issue>, issue: Issue) {
    if !out
        .iter()
        .any(|i| i.kind == issue.kind && i.nodes == issue.nodes)
    {
        out.push(issue);
    }
}

/// Every issue of the overview: the graph's format issues in graph order,
/// then the unknown nodes the tags name, then the missing links, the latter
/// two each ordered by nodes. An issue the tags raise is reported once per
/// `(kind, nodes)`, naming the first tab (in `tabs` order) that raises it.
pub fn derive(graph: &Graph, tabs: &[TaggedTab]) -> Vec<Issue> {
    let mut issues: Vec<Issue> = Vec::new();
    for issue in graph.issues() {
        let issue = Issue {
            kind: IssueKind::from_format(issue.kind),
            nodes: issue.nodes.clone(),
            file: issue.file.clone(),
            detail: issue.detail.clone(),
        };
        if !issues.contains(&issue) {
            issues.push(issue);
        }
    }
    let mut unknown: Vec<Issue> = Vec::new();
    let mut missing: Vec<Issue> = Vec::new();
    for tab in tabs {
        let ids = linked_ids(tab);
        let (known, ghosts): (Vec<&str>, Vec<&str>) =
            ids.iter().partition(|id| graph.node(id).is_some());
        for id in ghosts {
            push_unique(
                &mut unknown,
                Issue {
                    kind: IssueKind::UnknownNode,
                    nodes: vec![id.to_string()],
                    file: None,
                    detail: format!("tab '{}' is tagged with unknown node '{id}'", tab.name),
                },
            );
        }
        for (i, a) in known.iter().enumerate() {
            for b in &known[i + 1..] {
                if graph.has_edge(a, b) {
                    continue;
                }
                let mut nodes = vec![a.to_string(), b.to_string()];
                nodes.sort();
                push_unique(
                    &mut missing,
                    Issue {
                        kind: IssueKind::MissingLink,
                        nodes,
                        file: None,
                        detail: format!("tab '{}' links both; no edge between them", tab.name),
                    },
                );
            }
        }
    }
    let by_nodes =
        |x: &Issue, y: &Issue| x.nodes.cmp(&y.nodes).then_with(|| x.detail.cmp(&y.detail));
    unknown.sort_by(by_nodes);
    missing.sort_by(by_nodes);
    issues.extend(unknown);
    issues.extend(missing);
    issues
}

/// The `(a, b)` node pairs of every missing link, for the dashed edges.
pub fn missing_link_pairs(issues: &[Issue]) -> Vec<(String, String)> {
    issues
        .iter()
        .filter(|i| i.kind == IssueKind::MissingLink)
        .filter_map(|i| match i.nodes.as_slice() {
            [a, b] => Some((a.clone(), b.clone())),
            _ => None,
        })
        .collect()
}

/// A path as the prompt shows it: relative to the root where it is below it.
fn shown_path(file: &Path, root: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .display()
        .to_string()
}

/// The files an issue is about: its own, then the file of every node in
/// `nodes` that the graph knows, without repeats.
fn involved_files(issue: &Issue, graph: &Graph, root: &Path) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    if let Some(file) = &issue.file {
        files.push(shown_path(file, root));
    }
    for id in &issue.nodes {
        if let Some(node) = graph.node(id) {
            let shown = format!("{} (node '{id}')", shown_path(&node.file, root));
            if !files.iter().any(|f| f == &shown) {
                files.push(shown);
            }
        }
    }
    files
}

/// The prompt a Resolve tab runs `claude` with (spec section 6): the issue,
/// the files involved, for a missing link the tab's name, roles and
/// activity, and the instruction to fix the data per overview-format.md.
pub fn resolve_prompt(
    issue: &Issue,
    graph: &Graph,
    root: &Path,
    tab: Option<&TaggedTab>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Resolve one data issue of the kabelsalat overview rooted at {}.\n\n",
        root.display()
    ));
    out.push_str(&format!(
        "Issue ({}): {}\n",
        issue.kind.label(),
        issue.detail
    ));
    if !issue.nodes.is_empty() {
        out.push_str(&format!("Nodes: {}\n", issue.nodes.join(", ")));
    }
    let files = involved_files(issue, graph, root);
    if files.is_empty() {
        out.push_str("Files involved: none of the node files; the nodes named do not exist.\n");
    } else {
        out.push_str("Files involved (paths relative to the root where possible):\n");
        for file in &files {
            out.push_str(&format!("- {file}\n"));
        }
    }
    if let (IssueKind::MissingLink, Some(tab)) = (issue.kind, tab) {
        out.push_str(&format!("\nThe tab '{}' links both nodes", tab.name));
        match &tab.tags.activity {
            Some(activity) if !activity.trim().is_empty() => {
                out.push_str(&format!(" while \"{activity}\":\n"));
            }
            _ => out.push_str(":\n"),
        }
        for link in tab
            .tags
            .links
            .iter()
            .filter(|l| issue.nodes.contains(&l.node))
        {
            out.push_str(&format!("- '{}' with role '{}'\n", link.node, link.role));
        }
    }
    out.push_str(
        "\nFix the data per the kabelsalat skill's overview-format.md (the node file format: \
         YAML frontmatter with id, kind, parent and links).",
    );
    if issue.kind == IssueKind::MissingLink {
        out.push_str(
            " For this missing link, either add a direct edge under `links` with a name that \
             fits the relationship in one of the two files, or state that the tab's tag is wrong \
             and change nothing.",
        );
    }
    out.push_str(
        " Do not touch anything else. Do not type into other sessions. Finish by running \
         `kabelsalat overview issues` and reporting what it prints.\n",
    );
    out
}

/// One field of the TSV line: tabs, newlines and carriage returns become spaces.
fn tsv_field(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// The line `kabelsalat overview issues` prints for one issue:
/// `kind \t ids joined "," \t file or "" \t detail`, without a trailing newline.
pub fn tsv_line(issue: &Issue) -> String {
    let file = issue
        .file
        .as_ref()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!(
        "{}\t{}\t{}\t{}",
        issue.kind.label(),
        tsv_field(&issue.nodes.join(",")),
        tsv_field(&file),
        tsv_field(&issue.detail)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::model::Source;
    use crate::overview::tagger::TagLink;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn src(path: &str, text: &str) -> Source {
        (p(path), Ok(text.to_string()))
    }

    fn fm(id: &str, extra: &str) -> String {
        format!("---\nid: {id}\n{extra}---\n")
    }

    /// Nodes a, b, c with the edge a →builds-on→ c, plus whatever `extra`
    /// sources add.
    fn graph(extra: Vec<Source>) -> Graph {
        let mut sources = vec![
            src("/r/a.md", &fm("a", "links:\n  builds-on: c\n")),
            src("/r/b.md", &fm("b", "")),
            src("/r/sub/c.md", &fm("c", "")),
        ];
        sources.extend(extra);
        Graph::build(sources)
    }

    fn tab(name: &str, nodes: &[&str]) -> TaggedTab {
        TaggedTab {
            uuid: name.to_string(),
            name: name.to_string(),
            tags: Tags {
                links: nodes
                    .iter()
                    .map(|n| TagLink {
                        node: n.to_string(),
                        role: "implementing".to_string(),
                    })
                    .collect(),
                activity: Some("wiring the unseal check".to_string()),
                topic: None,
            },
        }
    }

    // --- kinds ---

    #[test]
    fn issue_kind_labels_are_kebab_case_and_format_kinds_convert() {
        assert_eq!(IssueKind::UnreadableFile.label(), "unreadable-file");
        assert_eq!(IssueKind::DuplicateId.label(), "duplicate-id");
        assert_eq!(IssueKind::UnknownNode.label(), "unknown-node");
        assert_eq!(IssueKind::ParentCycle.label(), "parent-cycle");
        assert_eq!(IssueKind::MissingLink.label(), "missing-link");
        for k in [
            FormatIssueKind::UnreadableFile,
            FormatIssueKind::DuplicateId,
            FormatIssueKind::UnknownNode,
            FormatIssueKind::ParentCycle,
        ] {
            assert_eq!(IssueKind::from_format(k).label(), k.label());
        }
    }

    // --- derive ---

    #[test]
    fn format_issues_are_carried_over_with_their_kinds() {
        let g = graph(vec![src("/r/d.md", &fm("d", "parent: ghost\n"))]);
        let issues = derive(&g, &[]);
        assert_eq!(
            issues,
            vec![Issue {
                kind: IssueKind::UnknownNode,
                nodes: vec!["d".into(), "ghost".into()],
                file: Some(p("/r/d.md")),
                detail: "'d' names unknown node 'ghost' as parent".into(),
            }]
        );
    }

    #[test]
    fn a_tag_naming_an_unknown_node_is_an_unknown_node_issue_that_names_the_tab() {
        let g = graph(Vec::new());
        let issues = derive(&g, &[tab("auth", &["ghost", "a"])]);
        assert_eq!(
            issues,
            vec![Issue {
                kind: IssueKind::UnknownNode,
                nodes: vec!["ghost".into()],
                file: None,
                detail: "tab 'auth' is tagged with unknown node 'ghost'".into(),
            }]
        );
    }

    #[test]
    fn a_tab_linking_two_nodes_with_an_edge_between_them_raises_nothing() {
        let g = graph(Vec::new());
        // a → c exists.
        assert!(derive(&g, &[tab("auth", &["a", "c"])]).is_empty());
        // The same pair named the other way round: b → a direction of the edge.
        assert!(derive(&g, &[tab("auth", &["c", "a"])]).is_empty());
    }

    #[test]
    fn a_tab_linking_two_unconnected_nodes_is_a_missing_link_with_sorted_nodes() {
        let g = graph(Vec::new());
        let issues = derive(&g, &[tab("auth", &["b", "a"])]);
        assert_eq!(
            issues,
            vec![Issue {
                kind: IssueKind::MissingLink,
                nodes: vec!["a".into(), "b".into()],
                file: None,
                detail: "tab 'auth' links both; no edge between them".into(),
            }]
        );
    }

    #[test]
    fn two_tabs_with_the_same_missing_pair_deduplicate_to_one_naming_the_first_tab() {
        let g = graph(Vec::new());
        let issues = derive(&g, &[tab("auth", &["b", "a"]), tab("other", &["a", "b"])]);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].nodes, vec!["a", "b"]);
        assert_eq!(
            issues[0].detail,
            "tab 'auth' links both; no edge between them"
        );
        // The same for an unknown node named by two tabs.
        let issues = derive(&g, &[tab("auth", &["ghost"]), tab("other", &["ghost"])]);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].detail.contains("'auth'"), "{issues:?}");
        // A tab naming the same node twice is one node, not a pair.
        let issues = derive(&g, &[tab("auth", &["a", "a"])]);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn a_tab_with_three_unconnected_nodes_yields_three_pairs_in_order() {
        let g = graph(vec![src("/r/d.md", &fm("d", ""))]);
        let issues = derive(&g, &[tab("auth", &["d", "b", "a"])]);
        assert_eq!(issues.len(), 3, "{issues:?}");
        let pairs: Vec<Vec<String>> = issues.iter().map(|i| i.nodes.clone()).collect();
        assert_eq!(
            pairs,
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["a".to_string(), "d".to_string()],
                vec!["b".to_string(), "d".to_string()],
            ]
        );
    }

    #[test]
    fn derived_issues_are_ordered_format_then_unknown_then_missing() {
        let g = graph(vec![src("/r/d.md", &fm("d", "parent: ghost\n"))]);
        let issues = derive(&g, &[tab("auth", &["zzz", "b", "a"])]);
        let kinds: Vec<IssueKind> = issues.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![
                IssueKind::UnknownNode, // format: d → ghost
                IssueKind::UnknownNode, // tag: zzz
                IssueKind::MissingLink, // a–b
            ]
        );
        assert_eq!(issues[1].nodes, vec!["zzz"]);
    }

    // --- missing_link_pairs ---

    #[test]
    fn missing_link_pairs_extracts_only_missing_links() {
        let issues = vec![
            Issue {
                kind: IssueKind::UnknownNode,
                nodes: vec!["x".into(), "y".into()],
                file: None,
                detail: String::new(),
            },
            Issue {
                kind: IssueKind::MissingLink,
                nodes: vec!["a".into(), "b".into()],
                file: None,
                detail: String::new(),
            },
            Issue {
                kind: IssueKind::DuplicateId,
                nodes: vec!["a".into()],
                file: Some(p("/r/a.md")),
                detail: String::new(),
            },
        ];
        assert_eq!(
            missing_link_pairs(&issues),
            vec![("a".to_string(), "b".to_string())]
        );
    }

    // --- resolve_prompt ---

    #[test]
    fn the_missing_link_prompt_names_the_files_the_tab_its_roles_activity_and_the_choices() {
        let g = graph(Vec::new());
        let mut t = tab("auth", &["a", "b"]);
        t.tags.links[1].role = "planning".into();
        let issues = derive(&g, &[t.clone()]);
        let issue = &issues[0];
        let prompt = resolve_prompt(issue, &g, &p("/r"), Some(&t));
        assert!(prompt.contains("missing-link"), "{prompt}");
        assert!(prompt.contains(&issue.detail), "{prompt}");
        assert!(prompt.contains("a.md"), "{prompt}");
        assert!(prompt.contains("b.md"), "{prompt}");
        assert!(
            !prompt.contains("/r/a.md"),
            "paths are root-relative: {prompt}"
        );
        assert!(prompt.contains("'auth'"), "{prompt}");
        assert!(prompt.contains("implementing"), "{prompt}");
        assert!(prompt.contains("planning"), "{prompt}");
        assert!(prompt.contains("wiring the unseal check"), "{prompt}");
        assert!(
            prompt.contains("the kabelsalat skill's overview-format.md"),
            "{prompt}"
        );
        assert!(prompt.contains("add a direct edge"), "{prompt}");
        assert!(prompt.contains("`links`"), "{prompt}");
        assert!(prompt.contains("change nothing"), "{prompt}");
        assert!(prompt.contains("kabelsalat overview issues"), "{prompt}");
        assert!(prompt.contains("Do not type into"), "{prompt}");
    }

    #[test]
    fn the_unreadable_file_prompt_names_the_file_and_the_error() {
        let g = graph(vec![src("/r/bad.md", "---\nid: [\n---\n")]);
        let issues = derive(&g, &[]);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].kind, IssueKind::UnreadableFile);
        let prompt = resolve_prompt(&issues[0], &g, &p("/r"), None);
        assert!(prompt.contains("unreadable-file"), "{prompt}");
        assert!(prompt.contains("bad.md"), "{prompt}");
        assert!(prompt.contains(&issues[0].detail), "{prompt}");
        assert!(prompt.contains("overview-format.md"), "{prompt}");
        assert!(!prompt.contains("add a direct edge"), "{prompt}");
        // A file outside the root is shown as its full path.
        let g = graph(vec![src("/elsewhere/bad.md", "---\nid: [\n---\n")]);
        let issues = derive(&g, &[]);
        let prompt = resolve_prompt(&issues[0], &g, &p("/r"), None);
        assert!(prompt.contains("/elsewhere/bad.md"), "{prompt}");
    }

    #[test]
    fn the_missing_link_prompt_without_the_tab_still_states_the_issue() {
        let g = graph(Vec::new());
        let issues = derive(&g, &[tab("auth", &["a", "b"])]);
        let prompt = resolve_prompt(&issues[0], &g, &p("/r"), None);
        assert!(
            prompt.contains("a.md") && prompt.contains("b.md"),
            "{prompt}"
        );
        assert!(prompt.contains("add a direct edge"), "{prompt}");
    }

    // --- tsv_line ---

    #[test]
    fn tsv_lines_are_single_line_and_tab_separated() {
        let i = Issue {
            kind: IssueKind::DuplicateId,
            nodes: vec!["a".into()],
            file: Some("/r/x.md".into()),
            detail: "also\tin\n/r/y.md\r\nend".into(),
        };
        assert_eq!(
            tsv_line(&i),
            "duplicate-id\ta\t/r/x.md\talso in /r/y.md  end"
        );
    }

    #[test]
    fn tsv_line_joins_ids_with_commas_and_leaves_a_missing_file_empty() {
        let i = Issue {
            kind: IssueKind::MissingLink,
            nodes: vec!["a".into(), "b".into()],
            file: None,
            detail: "tab 'x' links both; no edge between them".into(),
        };
        assert_eq!(
            tsv_line(&i),
            "missing-link\ta,b\t\ttab 'x' links both; no edge between them"
        );
        assert!(!tsv_line(&i).ends_with('\n'));
    }
}
