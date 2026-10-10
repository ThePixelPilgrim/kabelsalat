# Overview Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Each group can show, in place of its terminal area, a zoomable map of the nodes (projects, ADRs, themes — whatever the repository calls them) read from Markdown files with YAML frontmatter below one directory of the project: a "Terminals" | "Overview" toggle in the header bar switches the active group's mode and remembers it per group; `kabelsalat overview root` sets the directory and `kabelsalat overview issues` prints the data issues; a small model tags each tab with the nodes it works on and the tags live in the tab's tmux session environment; tabs appear on the map and open on click; data gaps (unparsable files, duplicate ids, dangling references, parent cycles, a tab that links two unconnected nodes) are listed with a Resolve button that opens a `claude` tab with a prepared prompt — per the approved spec `docs/superpowers/specs/2026-10-10-overview-mode-design.md`.

**Architecture:** A new `src/overview/` module tree. Four pure, unit-tested halves: `model.rs` (frontmatter parsing, the node graph, depth, kind and status colours, format issues, incremental rebuilds), `layout.rs` (inside-out container layout, tiers for a scale, visible boxes, edge lifting and bundling, hit-testing, breadcrumb, viewport maths), `tagger.rs` (watermarks and significance over transcript text, the prompt, response parsing, `overview.json`), `issues.rs` (missing-link and unknown-node issues from graph + tags, the Resolve prompt, the TSV line). `state.rs` gains the two persisted group fields; `tmuxctl.rs` gains a `show-environment` reader; `claude.rs` gains the transcript path; `cli.rs`/`control.rs`/`lib.rs` gain the `overview` command along the existing `Cli` → `Action` → `control::request_*` → `Msg` path. The GTK half is `overview/canvas.rs` (a `gtk::Widget` subclass drawing with `snapshot()`, the trays, the Close-tier `TextView`) plus wiring in `app.rs` (the toggle, a `gtk::Stack` around the terminal area, `gio::FileMonitor`s, tagger scheduling and the tagger process, `tmux set-environment`, Resolve spawning, publishing issues into the group snapshot).

**Tech Stack:** Rust 2024, relm4 0.11 / GTK 4 (`v4_18` via vte4) / libadwaita 0.9.2 (`features = ["v1_5"]`; `adw::ToggleGroup` needs `v1_7` and is **not** available — two linked `gtk::ToggleButton`s instead), gio 0.22 (`v2_80`; `gio::FileMonitor`), pango via gtk (`Widget::create_pango_layout` + `Snapshot::append_layout`; `pangocairo` is not available), serde/serde_json, and two new crates: `serde_yaml_ng = "0.10"` (frontmatter) and `pulldown-cmark = { version = "0.13", default-features = false }` (Markdown events; the default `html`/`getopts` features are not needed and not in the offline cache). Both are MIT OR Apache-2.0, which `deny.toml` allows.

## Global Constraints

From `CLAUDE.md` (binding, verbatim):

- Never expose the user's email address in User-Agent strings or other outgoing request headers; use a neutral identifier instead.
- Investigations / codebase exploration must always be run in subagents using the `opus` or `sonnet` model (pass a `model` override to the Agent tool), never on the default/session model.
- Red-green TDD for every behaviour change: write the failing test first, run it and watch it fail for the expected reason, then write the minimal code that makes it pass, then refactor with the tests green. No implementation before its failing test; a test that passes on first run proves nothing and needs to be made to fail first.
- Logic that is hard to test under this rule belongs in the pure modules (`src/state.rs`, `src/cli.rs`, `src/claude.rs`, `src/tmuxctl.rs`) behind parameters that tests can fabricate (a directory, an output string, a `/proc` root), not in `src/app.rs`.
- The GTK layer in `src/app.rs` is the one untested exception: keep it to wiring, so the behaviour it wires is covered elsewhere.
- `cargo build` needs system dev headers, not just Rust: GTK 4.18+, libadwaita 1.5+, VTE 0.82+ (Fedora: `gtk4-devel libadwaita-devel vte291-gtk4-devel`). A build failure in the `gtk4`/`vte4` sys crates usually means a missing header, not a code bug.
- CI (`.github/workflows/ci.yml`, Fedora container) runs `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test` on every push. Run them yourself before claiming work is done; where the GTK/VTE headers are missing locally, push and check the CI run instead.
- `src/state.rs` is pure logic — serde structs, persistence, and reconciliation planning. No GTK and no tmux calls belong here; it is what keeps the logic testable.
- `src/tmuxctl.rs` never panics: every fallible path returns `Result`. No `unwrap`/`expect` on tmux interaction.
- `src/app.rs` is the relm4 component holding all GUI state and side effects.
- The app must keep working when tmux is missing or older than 3.2 — it degrades to plain shells without session survival rather than erroring out.
- `src/cli.rs` is pure logic — argv parsing, group resolution, and the decision of what an invocation prints and exits with. No GTK, no gio, no tmux, no I/O; it is where the CLI's unit tests live. `src/control.rs` holds the gio glue and the group snapshot the command-line handler reads.
- A CLI-created tab must not steal focus: no activate, no window raise, no active-group change, and no touching the group's panes. `kabelsalat browser` and `kabelsalat android` are the two commands that touch a pane — only the named group's, brought up hidden unless that group is active — under the same no-focus, no-raise, no-switch rules. `android` additionally forwards control-socket commands, to the named group's own Android pane only.
- `src/autostart.rs` is pure over the facts it is handed (unit text, status classification, dialog responses) plus thin `systemctl --user` runners; `src/resume.rs` is the `kabelsalat resume` glue and never writes `state.json` — the GUI stays the state file's single writer.
- State: `$XDG_STATE_HOME/kabelsalat/state.json` (fallback `~/.local/state`), written atomically.

From the spec (binding):

- Agents publish structured data (Markdown + YAML frontmatter in the repository); kabelsalat owns the one native renderer. No web view, no agent-authored HTML. kabelsalat attaches no meaning to kinds, statuses or edge names.
- One overview per group, rooted at `SavedGroup.overview_root`. Remote groups (`host` set) have no overview: the toggle is insensitive with a tooltip, the CLI refuses with exit 3, and remote tabs are never tagged.
- The mode is per group (`SavedGroup.overview_mode`), reflected and set by the header toggle, restored on group switch. Activating a tab — from the sidebar or from a tab chip — switches that group to Terminals.
- Tab ↔ node links are inferred by the tagger, stored only in the tab's tmux session environment (`KABELSALAT_LINKS`, `KABELSALAT_TAG_MARK`), never in the repository and never in `state.json`. Without tmux they live in memory only.
- At most one tagger process at a time; at most one run per tab per two minutes; 60 s timeout; any failure keeps the previous tags and watermark and is logged.
- Data issues are derived on every change and never stored. Resolve opens a new tab the way `kabelsalat run` does: no focus change, no group switch, nothing typed into a running session.
- A CLI `overview` call never changes the mode, the active group or focus.
- One unparsable file never hides the others; a missing or unreadable root shows the empty state and affects nothing else in the group.

Mechanics:

- Every task ends green: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- **This machine has no GTK headers**, so `cargo build`/`cargo test` of the main crate fail locally in the `gtk4-sys` build script. Pure modules are tested through a *harness crate* that compiles the module files by path: copy the template at `/tmp/claude-0/-home-user/6b6dd7a4-392b-5863-9a5c-033116df4356/scratchpad/harness` to `scratchpad/harness-<module>`, keep its `Cargo.toml` (serde, serde_json, serde_yaml_ng 0.10, pulldown-cmark 0.13 without default features), and list in its `src/lib.rs` exactly the modules the task needs via `#[path = "/home/user/kabelsalat/src/<file>"] pub mod <name>;` (always `state`; `cli`, `claude`, `remote`, `tmuxctl` as needed; `pub mod overview { #[path = "/home/user/kabelsalat/src/overview/model.rs"] pub mod model; … }`). Inside the harness, `crate::state::…` and `crate::overview::model::…` resolve exactly as in the real crate, so module code needs no cfg tricks and **no harness-specific code goes into the repository**. `cargo fmt` runs in the real repo. The GTK layer (`control.rs`, `app.rs`, `overview/canvas.rs`) compiles only in CI: push and read the run. The "Run:" lines below say `cargo test …` and mean "in the harness" for pure modules.
- `dead_code`: `src/cli.rs` and `src/control.rs` are private modules, so an item added there that nothing uses yet is an error under `-D warnings` — tasks are cut so every new private item has a caller in the same commit. `src/overview/*` are `pub mod`s under `pub mod overview` in `src/lib.rs` with `pub` items, so they never trip the lint before `app.rs` uses them; keep a helper private only if something in its own module calls it.
- Two new crates and no others: `serde_yaml_ng = "0.10"` and `pulldown-cmark = { version = "0.13", default-features = false }` (Task 1). No `unsafe`.
- Test names are snake_case sentences in `#[cfg(test)] mod tests` at the end of each file, grouped under a `// --- overview ---` banner where the module already has tests. Temp dirs: `std::env::temp_dir().join(format!("kabelsalat-<mod>-test-{name}-{}", std::process::id()))`, removed at the end of the test.
- Commit messages are plain imperative sentences matching `git log` (no prefixes), e.g. "Parse overview node files into a graph". When this plan runs under the orchestrator, the orchestrator performs the Commit step of each task.
- Line numbers below are from `main` at `21b2cfd`; when an earlier task has already shifted them, find the spot by the quoted function or code instead.

---

### Task 1: Cargo and module skeleton

**Files:**
- Modify: `Cargo.toml` (`[dependencies]`, after `serde_json = "1"`)
- Modify: `src/lib.rs:7-18` (module list)
- Create: `src/overview/mod.rs`, `src/overview/model.rs`, `src/overview/layout.rs`, `src/overview/tagger.rs`, `src/overview/issues.rs`, `src/overview/canvas.rs` (each a one-line doc comment)
- Test: none (a skeleton; the first tests arrive in Task 2)

**Interfaces:**
- Consumes: nothing.
- Produces (used by every later task): the module path `crate::overview::{model, layout, tagger, issues, canvas}` and the two crates.

This task is the one place where "no implementation before its failing test" does not bite: it adds no behaviour. Its check is that the harness compiles the empty modules and that `cargo tree` shows the crates without the `html`/`getopts` features.

- [ ] **Step 1: Dependencies**

In `Cargo.toml`, after `serde_json = "1"`:

```toml
# Overview mode: YAML frontmatter of node files and their Markdown bodies.
serde_yaml_ng = "0.10"
pulldown-cmark = { version = "0.13", default-features = false }
```

- [ ] **Step 2: Modules**

In `src/lib.rs`, between `mod control;` and `pub mod remote;`, add `pub mod overview;`.

Create `src/overview/mod.rs`:

```rust
//! Overview mode: a zoomable map of a group's work, read from Markdown node
//! files with YAML frontmatter below the group's overview root. The pure
//! modules (`model`, `layout`, `tagger`, `issues`) are unit-tested; `canvas`
//! is the GTK widget and is wiring only.

pub mod canvas;
pub mod issues;
pub mod layout;
pub mod model;
pub mod tagger;
```

Create each of the five files with one doc line, e.g. `//! Node files, the graph and its format issues.` for `model.rs`.

- [ ] **Step 3: Check the skeleton compiles**

Run: `cd /home/user/kabelsalat && cargo tree -e features -i pulldown-cmark | head -5` — Expected: `pulldown-cmark v0.13.x` with no `html` or `getopts` feature line. Then, in a harness listing `state` and the four pure overview modules: `cargo build` — Expected: clean (empty modules).

- [ ] **Step 4: Format and lint**

Run: `cd /home/user/kabelsalat && cargo fmt` and, in the harness, `cargo clippy --all-targets -- -D warnings`. Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/overview
git commit -m "Add the overview module skeleton and its two crates"
```

---

### Task 2: `state.rs` — persist the overview root and mode per group

**Files:**
- Modify: `src/state.rs:22-58` (`SavedGroup`, after `host` at `:56-57`), `src/state.rs:64-79` (`SavedGroup::new`)
- Modify: `src/state.rs:808`, `:818`, `:1714` (exhaustive `SavedGroup` literals in tests)
- Modify: `src/app.rs:2365-2390` (`save_state`'s `SavedGroup { … }` literal: `overview_root: None, overview_mode: false` for now; Task 10 writes the real values)
- Test: `src/state.rs` (`mod tests` at `:801`; `tmp_dir` at `:864`)

**Interfaces:**
- Consumes: nothing new.
- Produces (used by Tasks 5, 10):
  - `SavedGroup::overview_root: Option<PathBuf>` (`#[serde(default)]`)
  - `SavedGroup::overview_mode: bool` (`#[serde(default)]`)

`PathBuf` is already imported at `src/state.rs:11`; `ClaudeSession.cwd` already serializes one, so no custom default function is needed.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests`, after the last test:

```rust
    // --- overview ---

    #[test]
    fn old_group_without_overview_fields_loads_without_an_overview() {
        let json = r#"{"groups": [{"id": 1, "name": "w", "palette": 0}],
                       "tabs": [], "active": null, "sidebar_visible": true}"#;
        let state: SavedState = serde_json::from_str(json).unwrap();
        assert_eq!(state.groups[0].overview_root, None);
        assert!(!state.groups[0].overview_mode);
    }

    #[test]
    fn overview_root_and_mode_round_trip() {
        let mut group = SavedGroup::new(4, "dev".into(), 0);
        group.overview_root = Some(PathBuf::from("/home/me/proj/docs"));
        group.overview_mode = true;
        let json = serde_json::to_string(&group).unwrap();
        assert_eq!(serde_json::from_str::<SavedGroup>(&json).unwrap(), group);
    }

    #[test]
    fn overview_fields_survive_save_and_load() { /* save(sample_state()) with one group's root set, load, assert both fields; tmp_dir("overview") */ }

    #[test]
    fn saved_group_new_has_no_overview() {
        let group = SavedGroup::new(1, "x".into(), 0);
        assert_eq!(group.overview_root, None);
        assert!(!group.overview_mode);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib state::tests` (harness listing `state`)
Expected: compile errors — no field `overview_root` / `overview_mode` on `SavedGroup`.

- [ ] **Step 3: Write the implementation**

In `SavedGroup` after `host` (`src/state.rs:57`):

```rust
    /// Directory the group's overview is read from (spec §2). None: the
    /// Overview page shows the empty state. `serde(default)` so older state
    /// files load without an overview.
    #[serde(default)]
    pub overview_root: Option<PathBuf>,
    /// Whether the group currently shows its overview instead of its
    /// terminals. `serde(default)` so older state files load in Terminals.
    #[serde(default)]
    pub overview_mode: bool,
```

Add `overview_root: None, overview_mode: false,` to `SavedGroup::new` (`:66-78`), to the test literals at `:808`, `:818`, `:1714`, and to the `save_state` literal in `src/app.rs:2365-2390` with the comment `// Task 10 of the overview plan writes the group's real values here.`

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib state::tests`
Expected: all pass (4 new).

- [ ] **Step 5: Format, lint, full test**

Run: `cd /home/user/kabelsalat && cargo fmt`; in the harness `cargo clippy --all-targets -- -D warnings && cargo test`. Expected: clean; all pass. (`src/app.rs` compiles in CI only.)

- [ ] **Step 6: Commit**

```bash
git add src/state.rs src/app.rs
git commit -m "Persist each group's overview root and mode"
```

---

### Task 3: `src/overview/model.rs` — node files, the graph, format issues

**Files:**
- Create/replace: `src/overview/model.rs`
- Test: `src/overview/model.rs` (`#[cfg(test)] mod tests` at the end)

**Interfaces:**
- Consumes: `serde_yaml_ng` (frontmatter → `serde_yaml_ng::Value`), `std::fs` only inside `scan_root`.
- Produces (used by Tasks 7, 8, 9, 10) — exact shapes, binding:

```rust
use std::collections::BTreeMap; use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color { Blue, Green, Amber, Red, Purple, Pink, Teal, Grey }
impl Color {
    pub fn parse(name: &str) -> Option<Color>;          // lower-case names from the spec
    pub fn name(self) -> &'static str;                   // "blue" …
    pub fn derived(kind_name: &str) -> Color;            // deterministic, never Grey, stable hash of the name
    pub fn rgb(self) -> (f32, f32, f32);                 // 0..1 for the canvas
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue { Scalar(String), List(Vec<String>) }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link { pub name: String, pub target: String }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String, pub title: String, pub kind: String, pub summary: Option<String>,
    pub status: Option<String>, pub parents: Vec<String>, pub links: Vec<Link>,
    pub fields: Vec<(String, FieldValue)>,               // extra keys, file order
    pub body: String, pub file: PathBuf,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindStyle { pub color: Color, pub statuses: Vec<(String, Color)> }
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed { Node(Node), Kind { name: String, style: KindStyle }, NotANode }
pub fn split_frontmatter(text: &str) -> Option<(&str, &str)>;   // (yaml, body); None when no leading `---` block
pub fn parse_file(file: &Path, text: &str) -> Result<Parsed, String>;  // Err = frontmatter present but unparsable / id missing or invalid
pub fn is_valid_id(id: &str) -> bool;                            // non-empty, chars in A-Z a-z 0-9 . _ -
pub fn status_word(status: &str) -> String;                     // first whitespace-separated word, lower-cased

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatIssueKind { UnreadableFile, DuplicateId, UnknownNode, ParentCycle }
impl FormatIssueKind { pub fn label(self) -> &'static str }     // "unreadable-file" "duplicate-id" "unknown-node" "parent-cycle"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatIssue { pub kind: FormatIssueKind, pub nodes: Vec<String>, pub file: Option<PathBuf>, pub detail: String }

/// Raw file contents the graph was built from; `Err` = the file could not be read.
pub type Source = (PathBuf, Result<String, String>);
pub fn scan_root(root: &Path) -> Vec<Source>;  // recursive, `*.md` only, skips dirs starting with '.', sorted by path; thin I/O
#[derive(Debug, Clone, Default)]
pub struct Graph { /* private */ }
impl Graph {
    pub fn build(sources: Vec<Source>) -> Graph;         // pure; duplicate id: first by path order wins, others → issue
    pub fn is_empty(&self) -> bool; pub fn len(&self) -> usize;
    pub fn node(&self, id: &str) -> Option<&Node>;
    pub fn nodes(&self) -> impl Iterator<Item = &Node>;  // ordered by id
    pub fn top_level(&self) -> &[String];                // ids with no (surviving) parent, ordered by id
    pub fn children(&self, id: &str) -> &[String];       // ordered by id; after cycle breaking
    pub fn parents(&self, id: &str) -> &[String];        // surviving parents, frontmatter order (the cycle-closing entry removed)
    pub fn depth(&self, id: &str) -> usize;              // shortest surviving parent chain; 0 at top level
    pub fn descendants(&self, id: &str) -> Vec<String>;  // distinct, excluding id, ordered by id
    pub fn kind_color(&self, kind: &str) -> Color;       // kind node colour or Color::derived
    pub fn status_color(&self, node: &Node) -> Option<Color>; // None without status; Grey when unlisted for its kind
    pub fn has_edge(&self, a: &str, b: &str) -> bool;    // a→b or b→a under links, any name
    pub fn issues(&self) -> &[FormatIssue];
    pub fn sources(&self) -> &[Source];                  // what it was built from, for incremental rebuilds
    pub fn with_source(&self, source: Source) -> Graph;  // replace/add one file and rebuild
    pub fn without_source(&self, file: &Path) -> Graph;  // remove one file and rebuild
}
```

Rules (spec §1, §6; contract): cycle breaking walks `parent` entries in frontmatter order and drops an entry whose target is the node itself or whose ancestor chain leads back, reporting it once as `ParentCycle` with `nodes` = the cycle. Unknown `parent`/`links` targets → `UnknownNode` with `nodes = [referrer, missing]`; the dangling reference stays in `Node` but has no effect on `children`/`has_edge`. Kind nodes (`kind: kind`) are not in `nodes()`; their `title` names the kind; `color` goes through `Color::parse` (bad/missing → derived); `statuses` values through `Color::parse` (bad → Grey). Title default: first `# ` heading of the body, else id. `parent`/`links` values accept a scalar or a list; non-string scalars are stringified (`P-004`, `42`). A `Source` with `Err` → `UnreadableFile` with the read error as detail.

- [ ] **Step 1: Write the failing tests**

Create `src/overview/model.rs` with the test module only. Sketch (fixtures are `&str` texts and `Path::new("/r/a.md")`-style paths; `Graph::build` takes `Vec<Source>`, so no disk I/O except in the `scan_root` test):

```rust
    #[test]
    fn split_frontmatter_needs_a_leading_fence_and_a_closing_one() {
        assert_eq!(split_frontmatter("---\nid: a\n---\nbody\n"), Some(("id: a\n", "body\n")));
        assert_eq!(split_frontmatter("# no frontmatter\n"), None);
        assert_eq!(split_frontmatter("---\nid: a\n"), None);
    }
    #[test]
    fn parse_file_reads_every_key_and_keeps_extra_fields_in_order() { /* the spec's ADR-118 example: parents [theme-access, P-004], links builds-on→adr-064, related→adr-122; summary, status; extra `owner: alice` and `tags: [x, y]` land in `fields` as Scalar/List in file order; body starts with "# ADR-118" */ }
    #[test]
    fn a_file_without_id_or_with_bad_yaml_is_an_error_and_plain_markdown_is_not_a_node() {
        assert!(parse_file(Path::new("/r/x.md"), "---\nkind: adr\n---\n").is_err());
        assert!(parse_file(Path::new("/r/x.md"), "---\nid: [\n---\n").is_err());
        assert!(parse_file(Path::new("/r/x.md"), "---\nid: 'bad id'\n---\n").is_err()); // space is not allowed
        assert_eq!(parse_file(Path::new("/r/x.md"), "# README\n"), Ok(Parsed::NotANode));
    }
    #[test]
    fn title_falls_back_to_the_first_heading_then_the_id() { /* "---\nid: a\n---\n# Hello\n" → "Hello"; no heading → "a" */ }
    #[test]
    fn kind_nodes_style_their_kind_and_are_not_drawn() { /* kind-adr file with color amber and statuses; graph.nodes() excludes it; kind_color("adr") == Amber; status_color(proposed node) == Some(Amber); unlisted status → Some(Grey); no status → None; kind without kind node → Color::derived, never Grey, stable across two calls */ }
    #[test]
    fn the_graph_orders_children_breaks_parent_cycles_and_reports_unknown_targets() {
        // a ← b ← c ← a (c's parent a closes the cycle): c has parents [] after breaking, one ParentCycle issue with nodes [a, b, c];
        // d: parent: ghost → UnknownNode [d, ghost]; top_level() == [a, d] (ordered by id); depth(b) == 1, depth(c) == 2;
        // descendants(a) == [b, c]; has_edge works both directions and ignores dangling link targets.
    }
    #[test]
    fn duplicate_ids_keep_the_first_file_by_path_and_report_the_second() { /* two sources with id a; node("a").file is the smaller path; one DuplicateId with file = the later path */ }
    #[test]
    fn with_source_and_without_source_rebuild_incrementally() { /* build 2 files; with_source replacing one changes its title and keeps len 2; with_source of a new path → len 3; without_source → len 2 and sources() shrinks */ }
    #[test]
    fn scan_root_finds_md_files_recursively_and_skips_dot_dirs() { /* temp dir: a.md, sub/b.md, .git/c.md, d.txt → paths [a.md, sub/b.md] sorted; an unreadable entry is Err */ }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib overview::model::tests` (harness listing `state` and `overview::model`)
Expected: compile errors — `split_frontmatter`, `parse_file`, `Graph`, `Color` … not found.

- [ ] **Step 3: Write the implementation**

Implement the signatures above. Frontmatter: `split_frontmatter` requires the text to start with `---\n` (or `---\r\n`) and finds the next line that is exactly `---`. Parse the YAML into `serde_yaml_ng::Value` (a `Mapping`); `id` must be a string passing `is_valid_id`. Build the graph in `Graph::build`: parse every `Ok` source in path order, collect `UnreadableFile`/`DuplicateId`, index nodes in a `BTreeMap<String, Node>`, resolve parents with cycle breaking, compute `children` (`BTreeMap<String, Vec<String>>`), `top_level`, `depth` (BFS over surviving parents, memoised in a map), and a `BTreeMap<String, KindStyle>` for kinds. `Color::derived` hashes the kind name with a small FNV-1a over bytes and picks from the seven non-grey colours. `with_source`/`without_source` edit a copy of `sources` and call `build`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib overview::model::tests`
Expected: all pass (9 new).

- [ ] **Step 5: Format and lint**

Run: `cd /home/user/kabelsalat && cargo fmt`; in the harness `cargo clippy --all-targets -- -D warnings`. Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src/overview/model.rs
git commit -m "Parse overview node files into a graph with format issues"
```

---

### Task 4: `tmuxctl.rs` — read a session variable; `claude.rs` — the transcript path

**Files:**
- Modify: `src/tmuxctl.rs:884-915` (next to `set_environment_args`/`set_environment`)
- Modify: `src/claude.rs:57-73` (next to `sessions_dir`/`sessions_dir_from`)
- Test: `src/tmuxctl.rs` (`mod tests`; argv tests at `:1187-1215`), `src/claude.rs` (`mod tests` at `:280`; `temp_dir` at `:285`)

**Interfaces:**
- Consumes: `tmuxctl::SESSION_PREFIX` (`:18`), `TmuxError` (`:174-203`), `state::ClaudeSession` (`src/state.rs:152-158`).
- Produces (used by Task 10):
  - `fn show_environment_args(uuid: &str, key: &str) -> [String; 4]` — `["show-environment", "-t", "ks-<uuid>", key]`
  - `pub fn show_environment_from_output(code: Option<i32>, stdout: &str, stderr: &str) -> Result<Option<String>, TmuxError>` — `KEY=value` → `Ok(Some(value))` (the value may contain `=`); `-KEY` or an "unknown variable" stderr → `Ok(None)`; any other failure → `Err(TmuxError::Command(stderr.trim()))`
  - `pub fn show_environment(&self, uuid: &str, key: &str) -> Result<Option<String>, TmuxError>` — `self.command(&args)?.output()?` then the parser; never panics
  - `pub fn projects_dir_from(config_dir: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf>` — `<root>/projects`, same root rule as `sessions_dir_from`
  - `pub fn projects_dir() -> Option<PathBuf>`
  - `pub fn escape_cwd(cwd: &Path) -> String` — every char not `[A-Za-z0-9]` → `-`
  - `pub fn transcript_path(projects_dir: &Path, session: &ClaudeSession) -> PathBuf` — `projects_dir/<escape_cwd(cwd)>/<id>.jsonl`

- [ ] **Step 1: Write the failing tests**

In `src/tmuxctl.rs` `mod tests`, after `unset_environment_argv_shape` (`:1203`):

```rust
    // --- overview ---

    #[test]
    fn show_environment_argv_shape() {
        assert_eq!(
            TmuxCtl::show_environment_args("u1", "KABELSALAT_LINKS"),
            ["show-environment", "-t", "ks-u1", "KABELSALAT_LINKS"].map(String::from)
        );
    }
    #[test]
    fn show_environment_output_yields_the_value_even_with_equals_inside() {
        assert_eq!(show_environment_from_output(Some(0), "K={\"a\":\"b=c\"}\n", ""), Ok(Some("{\"a\":\"b=c\"}".into())));
    }
    #[test]
    fn an_unset_variable_is_none_not_an_error() {
        assert_eq!(show_environment_from_output(Some(0), "-K\n", ""), Ok(None));
        assert_eq!(show_environment_from_output(Some(1), "", "unknown variable: K\n"), Ok(None));
    }
    #[test]
    fn a_failed_show_environment_is_a_command_error() {
        assert!(matches!(show_environment_from_output(Some(1), "", "no server running"), Err(TmuxError::Command(_))));
    }
```

(`Result<Option<String>, TmuxError>` has no `PartialEq` because `TmuxError::Io` holds an `io::Error`; write the two `Ok` assertions with `matches!` or `.unwrap()` if the existing tests do.)

In `src/claude.rs` `mod tests`:

```rust
    // --- overview ---

    #[test]
    fn projects_dir_mirrors_the_sessions_dir_root() {
        assert_eq!(projects_dir_from(Some(OsStr::new("/cfg")), None), Some(PathBuf::from("/cfg/projects")));
        assert_eq!(projects_dir_from(Some(OsStr::new("")), Some(OsStr::new("/home/me"))), Some(PathBuf::from("/home/me/.claude/projects")));
        assert_eq!(projects_dir_from(None, None), None);
    }
    #[test]
    fn escape_cwd_replaces_everything_but_alphanumerics() {
        assert_eq!(escape_cwd(Path::new("/home/user/kabelsalat")), "-home-user-kabelsalat");
        assert_eq!(escape_cwd(Path::new("/tmp/a b.c_d")), "-tmp-a-b-c-d");
    }
    #[test]
    fn transcript_path_joins_escaped_cwd_and_session_id() {
        let s = ClaudeSession { id: "abc-123".into(), cwd: PathBuf::from("/home/me/proj") };
        assert_eq!(transcript_path(Path::new("/home/me/.claude/projects"), &s), PathBuf::from("/home/me/.claude/projects/-home-me-proj/abc-123.jsonl"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib tmuxctl::tests::show_environment && cargo test --lib claude::tests` (harness listing `state`, `remote`, `tmuxctl`, `claude`)
Expected: compile errors — `show_environment_args`, `show_environment_from_output`, `projects_dir_from`, `escape_cwd`, `transcript_path` not found.

- [ ] **Step 3: Write the implementation**

`tmuxctl.rs`: add the three functions after `unset_environment` (`:913-915`), following `pane_pids`/`pane_pids_from_output` (`:858`): the parser checks `code == Some(0)` and a first stdout line starting with `format!("{key}=")` → `Some(value)`; a first line equal to `-{key}` → `None`; `stderr.contains("unknown variable")` → `None`; else `Err(TmuxError::Command(stderr.trim().to_string()))`. `show_environment` maps `io::Error` through `From`.

`claude.rs`: `projects_dir_from` reuses the root logic of `sessions_dir_from` (extract a private `fn config_root(config_dir, home) -> Option<PathBuf>` and call it from both). `escape_cwd` maps `cwd.to_string_lossy().chars()`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: the Step 2 commands. Expected: all pass (7 new).

- [ ] **Step 5: Format and lint**

Run: `cd /home/user/kabelsalat && cargo fmt`; in the harness `cargo clippy --all-targets -- -D warnings && cargo test`. Expected: clean; all pass.

- [ ] **Step 6: Commit**

```bash
git add src/tmuxctl.rs src/claude.rs
git commit -m "Read a tmux session variable back and locate claude transcripts"
```

---

### Task 5: `kabelsalat overview root` / `overview issues` — cli, control, lib forwarding

**Files:**
- Modify: `src/cli.rs:76-119` (`Cli`), `:124-133` (`needs_instance`), `:141-162` (`with_default_group`), `:196-204` (`GroupInfo`), `:251-288` (`help_text`), `:291-323` (`parse`), new `parse_overview` after `parse_android_cmd` (`:424`), `:627-658` (`Action`), `:693-905` (`dispatch`), `overview_argv` after `android_argv` (`:457-468`), `check_overview_root` after `with_absolute_path` (`:938-943`)
- Modify: `src/control.rs:68-120` (`request_set_overview_root`), `:212+` (the `match outcome.action` arm)
- Modify: `src/lib.rs:101-115` (argv rewrite after `with_default_group`)
- Modify: `src/app.rs:2451-2470` (`publish_groups`: `overview_root: None, issues: Vec::new()` for now; Task 10 fills them), `src/app.rs:715+` (`Msg::SetOverviewRoot`, handler next to `RenameGroup` at `:1995`)
- Test: `src/cli.rs` (`mod tests` at `:957`; fixtures `sample_groups` `:1053`, `browser_groups` `:1555`), `src/control.rs` (`mod tests` `:263-381`)

**Interfaces:**
- Consumes: Task 2 `SavedGroup::overview_root` (via the app handler).
- Produces (used by Tasks 10, 11):
  - `Cli::OverviewRoot { group: Option<String>, root: Option<PathBuf> }` (`root: None` = `--clear`; the path stays verbatim, possibly relative)
  - `Cli::OverviewIssues { group: Option<String> }`
  - `Action::SetOverviewRoot { group_uuid: String, root: Option<PathBuf> }` (root made absolute against the caller's cwd in `dispatch`)
  - `GroupInfo::overview_root: Option<PathBuf>`, `GroupInfo::issues: Vec<IssueLine>` with
    ```rust
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct IssueLine { pub kind: String, pub nodes: Vec<String>, pub file: Option<PathBuf>, pub detail: String }
    ```
  - `pub fn check_overview_root(action: &Action, is_dir: impl Fn(&Path) -> bool) -> Option<Outcome>` — `Some(Outcome::fail(EXIT_USAGE, "kabelsalat: no such directory: {path}\n"))` for a `SetOverviewRoot` whose `Some(root)` fails `is_dir`; `None` otherwise
  - `pub fn overview_argv(argv0: &str, group: &str, cli: &Cli) -> Vec<String>` — `[argv0, "overview", "root"|"issues", "-g", group, (DIR | "--clear")?]`
  - `control::request_set_overview_root(group_uuid: String, root: Option<PathBuf>) -> bool` → `Msg::SetOverviewRoot { group_uuid: String, root: Option<PathBuf> }`

`IssueLine` is deliberately a `cli.rs`-local type with `String` kind, independent of `overview::issues::Issue`: `cli.rs` stays a leaf that `control.rs` and the harness can compile without the overview tree, and the snapshot stores already-formatted text. `app.rs` converts with `IssueLine { kind: issue.kind.label().into(), … }` in Task 10. `GroupInfo` gets `#[derive(Default)]` so the 15 existing test literals can take `..Default::default()` — or add both fields to each; either keeps `Eq`.

- [ ] **Step 1: Write the failing tests**

In `src/cli.rs` `mod tests`, a new `// --- overview ---` section with a fixture `overview_groups()` (`aaa-111` "web" with `overview_root: Some("/home/me/web/docs")` and two issues, `bbb-222` "api" without a root, `rrr-555` "box" with `host: Some("me@box")`):

```rust
    #[test]
    fn overview_root_parses_a_directory_a_clear_flag_and_rejects_both_or_neither() {
        assert_eq!(parse(&args(&["overview", "root", "-g", "web", "docs"])), Ok(Cli::OverviewRoot { group: Some("web".into()), root: Some(PathBuf::from("docs")) }));
        assert_eq!(parse(&args(&["overview", "root", "--clear"])), Ok(Cli::OverviewRoot { group: None, root: None }));
        assert!(parse(&args(&["overview", "root"])).is_err());
        assert!(parse(&args(&["overview", "root", "docs", "--clear"])).is_err());
        assert!(parse(&args(&["overview", "root", "a", "b"])).is_err());
        assert!(parse(&args(&["overview", "frobnicate"])).is_err());
        assert_eq!(parse(&args(&["overview", "issues", "--group", "web"])), Ok(Cli::OverviewIssues { group: Some("web".into()) }));
    }
    #[test]
    fn overview_commands_need_an_instance_and_take_the_callers_group() {
        assert!(Cli::OverviewIssues { group: None }.needs_instance());
        assert_eq!(with_default_group(Cli::OverviewIssues { group: None }, Some("web")), Ok(Cli::OverviewIssues { group: Some("web".into()) }));
        assert!(with_default_group(Cli::OverviewRoot { group: None, root: None }, None).is_err());
    }
    #[test]
    fn overview_root_resolves_the_directory_against_the_caller_and_asks_the_gui() {
        let out = dispatch(&Cli::OverviewRoot { group: Some("web".into()), root: Some("docs".into()) }, &overview_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.action, Some(Action::SetOverviewRoot { group_uuid: "aaa-111".into(), root: Some(PathBuf::from("/w/docs")) }));
        // --clear carries root: None; an absolute DIR replaces the base.
    }
    #[test]
    fn overview_refuses_a_remote_group() {
        let out = dispatch(&Cli::OverviewIssues { group: Some("box".into()) }, &overview_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.stderr.contains("me@box"), "stderr was: {}", out.stderr);
    }
    #[test]
    fn overview_issues_prints_one_tab_separated_line_per_issue_and_nothing_when_clean() {
        let out = dispatch(&Cli::OverviewIssues { group: Some("web".into()) }, &overview_groups(), Path::new("/w"));
        assert_eq!(out.stdout, "missing-link\tadr-064,adr-118\t\ttab 'auth' links both; no edge between them\nunknown-node\tadr-118,ghost\t/home/me/web/docs/adr-118.md\tparent 'ghost' does not exist\n");
        assert!(out.action.is_none());
        let clean = dispatch(&Cli::OverviewIssues { group: Some("api".into()) }, &overview_groups(), Path::new("/w"));
        assert_eq!((clean.code, clean.stdout.as_str()), (EXIT_OK, ""));
    }
    #[test]
    fn a_missing_overview_directory_is_a_usage_error_before_the_gui_is_asked() {
        let action = Action::SetOverviewRoot { group_uuid: "aaa-111".into(), root: Some("/w/nope".into()) };
        let out = check_overview_root(&action, |_| false).expect("refused");
        assert_eq!((out.code, out.stderr.as_str()), (EXIT_USAGE, "kabelsalat: no such directory: /w/nope\n"));
        assert!(check_overview_root(&action, |_| true).is_none());
        assert!(check_overview_root(&Action::SetOverviewRoot { group_uuid: "a".into(), root: None }, |_| false).is_none());
    }
    #[test]
    fn overview_argv_round_trips_through_parse() { /* for root Some("docs"), root None and issues: parse(&overview_argv("kabelsalat", "web", &cli)) == Ok(cli with group Some("web")) */ }
    #[test]
    fn help_lists_overview() { assert!(help_text().contains("overview root")); assert!(help_text().contains("overview issues")); }
```

In `src/control.rs` `mod tests`: `fn a_set_overview_root_request_without_a_gui_is_refused() { assert!(!request_set_overview_root("aaa-111".into(), None)); }`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cli::tests::overview` (harness listing `state`, `cli`)
Expected: compile errors — no variants `OverviewRoot`/`OverviewIssues`/`SetOverviewRoot`, `check_overview_root`/`overview_argv` not found, no field `issues` on `GroupInfo`.

- [ ] **Step 3: Write the implementation**

`cli.rs`: add the variants; `needs_instance` matches both; `with_default_group` gains two arms copying the Browser arm with the message `"overview needs --group outside a kabelsalat tab ({ENV_GROUP} is unset)"`; `parse` gets `"overview" => parse_overview(&rest[1..])`, where `parse_overview` matches `args.split_first()`: `"root"` loops over `-g/--group VALUE`, `--clear` (boolean) and one positional (errors: `"overview root takes a directory or --clear"`, `"overview root takes one directory (got '{extra}')"`, an empty DIR), `"issues"` is the `parse_browser` loop, `-h|--help` → `Cli::Help`, a missing verb → `"overview needs root or issues"`, anything else → `"unknown overview command '{other}'"`. `dispatch`: `group: None` → `EXIT_USAGE` like `:750-753`; resolve, refuse remote with `"the overview is local, but '{group}' runs on {host}\n"`; `OverviewRoot` → `Outcome { code: EXIT_OK, action: Some(Action::SetOverviewRoot { group_uuid, root: root.as_ref().map(|d| caller_cwd.join(d)) }), .. }`; `OverviewIssues` → `Outcome::ok` of the formatted lines (`kind\tnodes.join(",")\tfile or ""\tdetail\n`, tabs/newlines inside fields replaced by spaces). `help_text`: two Usage lines and "2 usage (or `overview root` DIR missing)" in the exit-code paragraph.

`control.rs`: `request_set_overview_root` like `request_rename`; in `handle_command_line`, `Some(Action::SetOverviewRoot { group_uuid, root }) => { if let Some(refused) = cli::check_overview_root(&action, Path::is_dir) { printerr refused.stderr; return ExitCode::new(refused.code); } if !request_set_overview_root(group_uuid, root) { printerr "kabelsalat: no window to set the overview in\n"; return EXIT_NOT_RUNNING; } }` (match on a reference so `action` is still available for the check).

`lib.rs:101-115`: two arms — `cli::Cli::OverviewRoot { group: Some(group), .. } | cli::Cli::OverviewIssues { group: Some(group) } => cli::overview_argv(&args[0], group, &parsed)`.

`app.rs`: `Msg::SetOverviewRoot { group_uuid: String, root: Option<PathBuf> }` with the doc comment "Set by `kabelsalat overview root`. Changes no mode, no active group and no focus."; the handler finds the group by uuid (`eprintln!("overview root request for unknown group {group_uuid}")` and return if gone), sets `group.overview_root = root` and falls through to the save. Task 10 extends it with reload and rewatch. `publish_groups` gains `overview_root: g.overview_root.clone(), issues: Vec::new()` (Task 10 fills `issues`). `Group` gains `overview_root: Option<PathBuf>` and `overview_mode: bool` here, with `None`/`false` in `create_group` (`:2131-2150`) and copied from `SavedGroup` in `restore_or_fresh` (`:2209-2230`); `save_state` (`:2365-2390`) now writes `g.overview_root.clone()` and `g.overview_mode` instead of Task 2's placeholders.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib cli::tests` — Expected: all pass (8 new). `control.rs`, `lib.rs` and `app.rs` compile in CI (Step 5).

- [ ] **Step 5: Format, lint, full test**

Run: `cd /home/user/kabelsalat && cargo fmt`; in the harness `cargo clippy --all-targets -- -D warnings && cargo test`; push and check the CI run for the GTK-side files. Expected: clean; all pass.

- [ ] **Step 6: Commit**

```bash
git add src/cli.rs src/control.rs src/lib.rs src/app.rs
git commit -m "Add kabelsalat overview root and overview issues"
```

---

### Task 6: `src/overview/tagger.rs` — watermarks, significance, prompt, response, config

**Files:**
- Create/replace: `src/overview/tagger.rs`
- Test: `src/overview/tagger.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `serde`, `serde_json`.
- Produces (used by Tasks 8, 9, 10) — binding:

```rust
use serde::{Deserialize, Serialize};
pub const ENV_LINKS: &str = "KABELSALAT_LINKS"; pub const ENV_TAG_MARK: &str = "KABELSALAT_TAG_MARK";
pub const DEFAULT_ROLES: [&str; 4] = ["planning", "implementing", "researching", "related"];
pub const MAX_TRANSCRIPT_CHARS: usize = 40_000; pub const MIN_RUN_GAP_SECS: u64 = 120; pub const SCAN_SECS: u32 = 60; pub const TIMEOUT_SECS: u64 = 60;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Tags { #[serde(default)] pub links: Vec<TagLink>, #[serde(default)] pub activity: Option<String>, #[serde(default)] pub topic: Option<String> }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagLink { pub node: String, pub role: String }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config { pub tagger_command: Vec<String>, pub roles: Vec<String> }
impl Default for Config { /* ["claude","-p","--model","haiku","--output-format","json"], DEFAULT_ROLES */ }
pub fn config_path(config_home: &Path) -> PathBuf;              // config_home/kabelsalat/overview.json
pub fn parse_config(text: &str) -> Result<Config, String>;      // missing keys → defaults; empty argv → Err
pub fn load_config(path: &Path) -> Config;                      // missing file → default; unreadable/bad → default + eprintln
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Watermark { Transcript { uuid: String, offset: u64 }, Title(String) }
impl Watermark { pub fn parse(s: &str) -> Option<Watermark>; pub fn render(&self) -> String; }  // "<uuid>:<offset>" | "title:<title>"
/// Cheap check: `true` when size and last uuid both match the mark (nothing to do).
pub fn unchanged(mark: Option<&Watermark>, size: u64, last_uuid: Option<&str>) -> bool;
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Delta { pub user_prompts: usize, pub assistant_messages: usize, pub text: String, pub last_uuid: Option<String>, pub end_offset: u64 }
/// Parse the transcript part after `mark` (byte offset; a mark whose uuid is not found → whole file).
pub fn delta_since(transcript: &str, mark: Option<&Watermark>) -> Delta;
pub fn significant(d: &Delta) -> bool;                           // >= 1 user prompt or >= 6 assistant messages
pub fn last_uuid(transcript: &str) -> Option<String>;
pub fn tail_chars(s: &str, n: usize) -> &str;
pub struct NodeSummary<'a> { pub id: &'a str, pub kind: &'a str, pub title: &'a str }
pub fn prompt(roles: &[String], nodes: &[NodeSummary], current: &Tags, tab_title: &str, text: &str) -> String;
pub fn parse_response(stdout: &str) -> Result<Tags, String>;    // bare object, or envelope {"result": "<json string>"}; also tolerate the result string wrapped in ```json fences
pub fn may_run(last_run_secs_ago: Option<u64>, running_elsewhere: bool) -> bool;  // one at a time, >= MIN_RUN_GAP_SECS per tab
pub fn title_input(title: &str) -> (Watermark, String);         // for tabs without a transcript
```

Transcript lines (spec §5; contract): JSON per line with `type` (`"user"`/`"assistant"`, others ignored), `uuid`, `message.content` (a string, or an array of blocks where `type: "text"` contributes `text` and `tool_result`/`tool_use` are dropped). A `user` line whose content is entirely `tool_result` blocks is **not** a user prompt. Lines with `isSidechain: true` and malformed lines are skipped. `end_offset` = byte length of the transcript. `text` = messages joined as `"user: …\n"` / `"assistant: …\n"`. `tail_chars` cuts on a char boundary.

- [ ] **Step 1: Write the failing tests**

```rust
    const T: &str = concat!(
        r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"fix the unseal check"}}"#, "\n",
        r#"{"type":"assistant","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"t","name":"Read","input":{}}]}}"#, "\n",
        r#"{"type":"user","uuid":"u2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"…"}]}}"#, "\n",
        r#"{"type":"assistant","uuid":"a2","isSidechain":true,"message":{"content":"side"}}"#, "\n",
        "not json\n",
        r#"{"type":"assistant","uuid":"a3","message":{"role":"assistant","content":"Done: wired it."}}"#, "\n");

    #[test]
    fn watermarks_render_and_parse_both_forms() {
        let w = Watermark::Transcript { uuid: "a3".into(), offset: 42 };
        assert_eq!(Watermark::parse(&w.render()), Some(w));
        assert_eq!(Watermark::parse("title:claude: foo:bar"), Some(Watermark::Title("claude: foo:bar".into())));
        assert_eq!(Watermark::parse("garbage"), None);
    }
    #[test]
    fn the_cheap_check_needs_both_size_and_uuid_to_match() { /* unchanged(Some(&mark{a3,len}), len, Some("a3")) true; different size or uuid false; None mark false */ }
    #[test]
    fn delta_counts_prompts_and_assistant_messages_and_drops_tool_traffic() {
        let d = delta_since(T, None);
        assert_eq!((d.user_prompts, d.assistant_messages), (1, 2));   // u2 is a tool result, a2 is a sidechain
        assert_eq!(d.text, "user: fix the unseal check\nassistant: Looking.\nassistant: Done: wired it.\n");
        assert_eq!(d.last_uuid.as_deref(), Some("a3"));
        assert_eq!(d.end_offset, T.len() as u64);
    }
    #[test]
    fn delta_since_a_mark_starts_after_its_offset_and_falls_back_to_the_whole_file_for_an_unknown_uuid() { /* mark at the offset after line 1 with uuid u1 → user_prompts 0, assistant 2; mark uuid "zzz" → same as None */ }
    #[test]
    fn significance_is_one_prompt_or_six_assistant_messages() { /* Delta{user_prompts:1} true; {assistant_messages:5} false; 6 true; Default false */ }
    #[test]
    fn responses_parse_bare_enveloped_and_fenced() {
        let bare = r#"{"links":[{"node":"adr-118","role":"implementing"}],"activity":"wiring","topic":null}"#;
        let want = Tags { links: vec![TagLink { node: "adr-118".into(), role: "implementing".into() }], activity: Some("wiring".into()), topic: None };
        assert_eq!(parse_response(bare), Ok(want.clone()));
        let env = serde_json::json!({"type":"result","result": format!("```json\n{bare}\n```")}).to_string();
        assert_eq!(parse_response(&env), Ok(want));
        assert!(parse_response("I cannot").is_err());
    }
    #[test]
    fn the_prompt_names_roles_nodes_current_links_title_and_the_tail_of_the_text() { /* contains each role, "adr-118 (adr): Emergency access account", the tab title, and only the last MAX_TRANSCRIPT_CHARS of a long text */ }
    #[test]
    fn config_defaults_overrides_and_rejects_an_empty_command() {
        assert_eq!(parse_config("{}"), Ok(Config::default()));
        assert_eq!(parse_config(r#"{"roles":["a","b"]}"#).unwrap().roles, vec!["a", "b"]);
        assert!(parse_config(r#"{"tagger_command":[]}"#).is_err());
        assert_eq!(config_path(Path::new("/home/me/.config")), PathBuf::from("/home/me/.config/kabelsalat/overview.json"));
        assert_eq!(load_config(Path::new("/nonexistent/overview.json")), Config::default());
    }
    #[test]
    fn may_run_enforces_one_at_a_time_and_the_per_tab_gap() {
        assert!(may_run(None, false)); assert!(!may_run(None, true));
        assert!(!may_run(Some(119), false)); assert!(may_run(Some(120), false));
    }
    #[test]
    fn a_tab_without_a_transcript_uses_its_title() { assert_eq!(title_input("npm run dev"), (Watermark::Title("npm run dev".into()), "title: npm run dev\n".into())); }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib overview::tagger::tests` (harness listing `state`, `overview::tagger`)
Expected: compile errors — `Watermark`, `delta_since`, `parse_response` … not found.

- [ ] **Step 3: Write the implementation**

Implement the signatures. `delta_since` slices at `offset` only when the uuid in the mark is found by `last_uuid`-style scanning of the part before the offset (else from 0), then iterates `lines()` with `serde_json::from_str::<serde_json::Value>`, skipping errors and sidechains. `parse_response` tries `serde_json::from_str::<Tags>` on the trimmed stdout, else reads a `Value` with a string `result`, strips ```` ``` ```` fences and a leading `json`, and parses that. `prompt` is a plain multi-line template: instructions, `Roles: …`, `Nodes:` one `id (kind): title` per line, `Current links: <json>`, `Tab title: …`, `Transcript:` + `tail_chars(text, MAX_TRANSCRIPT_CHARS)`, and the required output shape. `load_config` reads the file, `parse_config`s it and `eprintln!`s on failure.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib overview::tagger::tests` — Expected: all pass (10 new).

- [ ] **Step 5: Format and lint**

Run: `cd /home/user/kabelsalat && cargo fmt`; harness `cargo clippy --all-targets -- -D warnings`. Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src/overview/tagger.rs
git commit -m "Decide when and how a tab's work is tagged with its nodes"
```

---

### Task 7: `src/overview/layout.rs` — containers, tiers, visibility, edges, hit-testing, viewport

**Files:**
- Create/replace: `src/overview/layout.rs`
- Test: `src/overview/layout.rs` (`#[cfg(test)] mod tests`, with a `fn graph(files: &[(&str, &str)]) -> Graph` helper building from in-memory `Source`s)

**Interfaces:**
- Consumes: Task 3 `model::{Graph, Node}`.
- Produces (used by Tasks 9, 10) — binding:

```rust
use super::model::Graph;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier { Far, Mid, Near, Close }
impl Tier {
    pub fn for_scale(scale: f64) -> Tier;   // thresholds: < 0.35 Far, < 0.7 Mid, < 1.4 Near, else Close
    pub fn scale(self) -> f64;              // representative scale for slider jumps: 0.25, 0.5, 1.0, 2.0
    pub fn label(self) -> &'static str;     // "Far" "Mid" "Near" "Close"
    pub const ALL: [Tier; 4];
}
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect { pub x: f64, pub y: f64, pub w: f64, pub h: f64 }
impl Rect { pub fn contains(&self, x: f64, y: f64) -> bool; pub fn center(&self) -> (f64, f64); pub fn union(&self, o: &Rect) -> Rect; }
/// One drawn copy of a node. `parent` is the copy's container (None at top level).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BoxKey { pub id: String, pub parent: Option<String> }
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedBox { pub key: BoxKey, pub rect: Rect, pub level: usize, pub mirror: bool, pub child_order: usize }
#[derive(Debug, Clone, PartialEq)]
pub struct Metrics { pub card_w: f64, pub card_h: f64, pub header_h: f64, pub pad: f64, pub gap_x: f64, pub gap_y: f64, pub row_h: f64 }
impl Default for Metrics { /* card 220x120, header 44, pad 16, gaps 48/40, row 26 */ }
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout { pub boxes: Vec<PlacedBox>, pub bounds: Rect }
impl Layout {
    pub fn get(&self, key: &BoxKey) -> Option<&PlacedBox>;
    pub fn copies(&self, id: &str) -> Vec<&PlacedBox>;   // all copies of a node (for mirror hover highlight)
}
pub fn layout(graph: &Graph, m: &Metrics) -> Layout;

pub fn visible_levels(tier: Tier) -> usize;   // Far: 1 (level 0 only), Mid: 2, Near: 3, Close: 4 (the spec's "Deeper: as Near": cards' children take the card form, their own children are rows)
pub fn visible<'a>(layout: &'a Layout, tier: Tier) -> Vec<&'a PlacedBox>;
pub fn visible_ancestor<'a>(layout: &'a Layout, graph: &Graph, key: &BoxKey, tier: Tier) -> Option<&'a PlacedBox>; // the box itself when visible, else nearest visible container copy
pub const MID_ROWS: usize = 12;  // rows shown per container at Mid before "+ N more"
#[derive(Debug, Clone, PartialEq)]
pub struct Edge { pub from: BoxKey, pub to: BoxKey, pub labels: Vec<String>, pub count: usize, pub warning: bool }
/// Lifted to visible boxes and bundled per (from,to) pair; `warning_pairs` are missing-link node pairs drawn dashed (Near+).
pub fn edges(graph: &Graph, layout: &Layout, tier: Tier, warning_pairs: &[(String, String)]) -> Vec<Edge>;
pub fn hit_test<'a>(layout: &'a Layout, tier: Tier, x: f64, y: f64) -> Option<&'a PlacedBox>;  // deepest visible box containing the world point
pub fn focused<'a>(layout: &'a Layout, tier: Tier, cx: f64, cy: f64) -> Option<&'a PlacedBox>;  // visible box with level>=1 whose centre is nearest (cx,cy); None when none visible
pub fn breadcrumb(layout: &Layout, graph: &Graph, tier: Tier, cx: f64, cy: f64) -> Vec<String>; // ids: outermost box under the centre, then inner ones, by containment
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport { pub scale: f64, pub ox: f64, pub oy: f64, pub w: f64, pub h: f64 }  // screen = world*scale + o
impl Viewport {
    pub fn new(w: f64, h: f64) -> Viewport;
    pub fn to_world(&self, sx: f64, sy: f64) -> (f64, f64); pub fn to_screen(&self, wx: f64, wy: f64) -> (f64, f64);
    pub fn zoom_at(&self, sx: f64, sy: f64, factor: f64) -> Viewport;  // keeps the world point under (sx,sy) fixed; clamps scale to 0.05..=8
    pub fn set_scale_at_center(&self, scale: f64) -> Viewport;
    pub fn pan(&self, dx: f64, dy: f64) -> Viewport;
    pub fn fit(&self, r: &Rect, margin: f64) -> Viewport;  // scale & offset so r fills the viewport minus margin
    pub fn center_world(&self) -> (f64, f64);
    pub fn tier(&self) -> Tier;
}
```

Algorithm (contract): inside out — `size(id)` = card size without children, else header + pad + layered arrangement of the children + pad. Layered arrangement among one container's children: take the `links` edges whose both endpoints are children of that container; layers by longest path from sources (back-edges found by DFS in id order are dropped); order within a layer by barycenter of predecessors, ties by id; layers are rows stacked vertically, boxes within a row left to right, row width = sum + gaps, container width = max row width + 2 pad. The top level uses the same arrangement over `top_level()`. A node with several parents is placed in each parent's box with its own subtree laid out in each copy; `mirror = true` for every copy after the first in `graph.parents(id)` order. Nesting below level 6 is ignored. Edge lifting: an edge (a→b) is drawn between `visible_ancestor` of the first (non-mirror) copy of a and of b; an edge whose endpoints lift to the same box is dropped; bundled per (from, to) with `labels` = distinct names and `count` = number of edges; the renderer shows the name when `count == 1`, else "N links". Warning pairs are lifted the same way and set `warning: true`, only from Near on.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn tiers_follow_the_scale_thresholds_and_round_trip_their_jump_scales() {
        assert_eq!(Tier::for_scale(0.2), Tier::Far); assert_eq!(Tier::for_scale(0.35), Tier::Mid);
        assert_eq!(Tier::for_scale(0.7), Tier::Near); assert_eq!(Tier::for_scale(1.4), Tier::Close);
        for t in Tier::ALL { assert_eq!(Tier::for_scale(t.scale()), t); }
    }
    #[test]
    fn children_are_laid_out_inside_their_parent_and_the_parent_grows_to_fit() { /* p with children a, b (a →links→ b): both rects inside p.rect; a above b (layers); p.rect.h > 2 * card_h; bounds is the union */ }
    #[test]
    fn a_node_with_two_parents_is_placed_in_each_and_the_second_copy_is_a_mirror() {
        /* m: parent [p, q] → copies(m).len() == 2; copy with parent Some("p") has mirror false; Some("q") mirror true; levels both 1 */
    }
    #[test]
    fn visible_levels_match_the_tier_table_and_edges_lift_to_visible_ancestors() {
        /* p{a}, q{b}, a →related→ b: at Far, edges() has one Edge from p to q with labels ["related"], count 1;
           at Near the edge runs a → b; two edges a→b with names x,y bundle into count 2 with both labels; an edge inside p drops at Far */
    }
    #[test]
    fn warning_pairs_appear_dashed_from_near_only() { /* edges(.., Far, &[(a,b)]) has no warning edge; Near has one with warning: true and count 0 or 1 by design (document) */ }
    #[test]
    fn hit_test_returns_the_deepest_visible_box_and_focused_prefers_level_one_and_up() { /* point inside a at Near → a; at Far → p; focused at Far → None */ }
    #[test]
    fn breadcrumb_lists_containment_from_the_outside_in() { /* centre in a (inside p) at Near → ["p", "a"]; Far → ["p"] */ }
    #[test]
    fn the_viewport_keeps_the_point_under_the_cursor_fixed_when_zooming_and_fits_a_rect() {
        let v = Viewport::new(800.0, 600.0);
        let z = v.zoom_at(100.0, 50.0, 2.0);
        assert_eq!(z.to_world(100.0, 50.0), v.to_world(100.0, 50.0));
        assert!((z.scale - 2.0 * v.scale).abs() < 1e-9);
        assert!(v.zoom_at(0.0, 0.0, 1000.0).scale <= 8.0);
        let f = v.fit(&Rect { x: 0.0, y: 0.0, w: 400.0, h: 200.0 }, 20.0);
        let (cx, cy) = f.center_world(); assert!((cx - 200.0).abs() < 1e-6 && (cy - 100.0).abs() < 1e-6);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib overview::layout::tests` (harness listing `state`, `overview::{model, layout}`)
Expected: compile errors — `Tier`, `layout`, `Viewport` … not found.

- [ ] **Step 3: Write the implementation**

Implement the signatures. Keep the layered arrangement in a private `fn arrange(ids: &[String], edges: &[(usize, usize)], sizes: &[(f64, f64)], m: &Metrics) -> (Vec<Rect>, f64, f64)` (relative rects, width, height) used both for containers and the top level, and a recursive `fn place(graph, id, parent: Option<&str>, level, origin, m, out: &mut Vec<PlacedBox>, first_copy_seen: &mut HashSet<String>)`. `visible_ancestor` walks `key.parent` through `layout.get` with the parent's own container found by the chain of keys recorded during placement (store `parent_key: Option<BoxKey>` privately or recompute from `graph.parents`). `edges` collects `(from_key, to_key, name)` then folds into a `BTreeMap<(BoxKey, BoxKey), Edge>`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib overview::layout::tests` — Expected: all pass (8 new).

- [ ] **Step 5: Format and lint**

Run: `cd /home/user/kabelsalat && cargo fmt`; harness `cargo clippy --all-targets -- -D warnings`. Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src/overview/layout.rs
git commit -m "Lay out the overview graph by nesting and tier"
```

---

### Task 8: `src/overview/issues.rs` — data issues from graph + tags, the Resolve prompt

**Files:**
- Create/replace: `src/overview/issues.rs`
- Test: `src/overview/issues.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: Task 3 `model::{FormatIssue, FormatIssueKind, Graph}`, Task 6 `tagger::Tags`.
- Produces (used by Tasks 9, 10) — binding:

```rust
use super::model::{FormatIssueKind, Graph}; use super::tagger::Tags;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueKind { UnreadableFile, DuplicateId, UnknownNode, ParentCycle, MissingLink }
impl IssueKind { pub fn label(self) -> &'static str; pub fn from_format(k: FormatIssueKind) -> IssueKind; }  // "missing-link"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue { pub kind: IssueKind, pub nodes: Vec<String>, pub file: Option<PathBuf>, pub detail: String }
/// A tab's current tags, as the GUI knows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggedTab { pub uuid: String, pub name: String, pub tags: Tags }
pub fn derive(graph: &Graph, tabs: &[TaggedTab]) -> Vec<Issue>;   // format issues + unknown-node from tags (nodes=[id], detail names the tab) + missing-link (nodes=[a,b] sorted, detail: "tab '<name>' links both; no edge between them"); deduplicated
pub fn missing_link_pairs(issues: &[Issue]) -> Vec<(String, String)>;
pub fn resolve_prompt(issue: &Issue, graph: &Graph, root: &Path, tab: Option<&TaggedTab>) -> String;  // per spec §6; mentions overview-format.md; for MissingLink: add a fitting direct edge or state the tag is wrong and change nothing
pub fn tsv_line(issue: &Issue) -> String;                          // kind \t ids joined "," \t file or "" \t detail, tabs/newlines → spaces, no trailing newline
```

- [ ] **Step 1: Write the failing tests**

```rust
    fn tab(name: &str, nodes: &[&str]) -> TaggedTab { /* uuid = name, links with role "implementing" */ }

    #[test]
    fn format_issues_are_carried_over_with_their_kinds() { /* graph with a dangling parent → derive() contains Issue{UnknownNode, nodes [d, ghost], file Some(d.md)}; label() == "unknown-node"; from_format round-trips all four */ }
    #[test]
    fn a_tab_linking_two_unconnected_nodes_is_a_missing_link_sorted_and_deduplicated() {
        /* nodes a, b, c; a →builds-on→ c. tab "auth" links [b, a] and tab "other" links [a, b]:
           exactly one MissingLink with nodes ["a","b"] and detail "tab 'auth' links both; no edge between them"; a–c yields none (edge exists either direction) */
    }
    #[test]
    fn a_tag_naming_an_unknown_node_is_an_unknown_node_issue_that_names_the_tab() { /* tab links "ghost": Issue{UnknownNode, nodes ["ghost"], file None, detail contains "tab 'auth'"} */ }
    #[test]
    fn missing_link_pairs_extracts_only_missing_links() { /* from a mixed list → [("a","b")] */ }
    #[test]
    fn the_resolve_prompt_names_the_issue_the_files_the_format_page_and_the_two_choices() {
        /* MissingLink with tab: contains both file paths (root-relative), "overview-format.md", the tab name, its role, its activity, "add a direct edge" and "change nothing";
           UnreadableFile: contains the file and the parse error */
    }
    #[test]
    fn tsv_lines_are_single_line_and_tab_separated() {
        let i = Issue { kind: IssueKind::DuplicateId, nodes: vec!["a".into()], file: Some("/r/x.md".into()), detail: "also\tin\n/r/y.md".into() };
        assert_eq!(tsv_line(&i), "duplicate-id\ta\t/r/x.md\talso in /r/y.md");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib overview::issues::tests` (harness listing `state`, `overview::{model, tagger, issues}`)
Expected: compile errors — `IssueKind`, `derive`, `tsv_line` … not found.

- [ ] **Step 3: Write the implementation**

`derive`: map `graph.issues()` through `from_format`; for each tab, for each link whose node is `graph.node(..).is_none()` push an `UnknownNode`; for each unordered pair of distinct linked nodes that both exist and `!graph.has_edge(a, b)` push a `MissingLink` with sorted `nodes` and the first tab's name; deduplicate on `(kind, nodes)` keeping the first. `resolve_prompt` builds a plain-text instruction: the issue line, the node files involved (relative to `root` via `strip_prefix`), for a missing link the tab's name, roles and activity, then "Fix the data per overview-format.md (skills/kabelsalat/overview-format.md in the kabelsalat plugin). For a missing link, add a direct edge under `links` with a name that fits, or state that the tag is wrong and change nothing. Do not type into any running session."

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib overview::issues::tests` — Expected: all pass (6 new).

- [ ] **Step 5: Format and lint**

Run: `cd /home/user/kabelsalat && cargo fmt`; harness `cargo clippy --all-targets -- -D warnings && cargo test`. Expected: clean; all pass.

- [ ] **Step 6: Commit**

```bash
git add src/overview/issues.rs
git commit -m "Derive overview data issues and the prompt that resolves them"
```

---

### Task 9: `src/overview/canvas.rs` — the overview widget (GTK, no tests)

**Files:**
- Create/replace: `src/overview/canvas.rs`
- Test: none (GTK wiring; the behaviour it draws is decided in Tasks 3, 7, 8)

**Interfaces:**
- Consumes: Task 3 `model::{Graph, Node, Color}`, Task 7 `layout::*`, Task 8 `issues::Issue`, `pulldown_cmark::{Parser, Event, Tag, TagEnd}`.
- Produces (used by Task 10) — binding:

```rust
pub struct TabChip { pub uuid: String, pub name: String, pub role: String, pub node: String, pub activity: Option<String>, pub age: String }
pub struct Unmapped { pub uuid: String, pub name: String, pub topic: String }
pub enum EmptyState { NoRoot, Unreadable { root: PathBuf, error: String } }
pub struct Callbacks { pub open_tab: Box<dyn Fn(String /*uuid*/)>, pub resolve: Box<dyn Fn(usize /*issue index*/)>, pub open_uri: Box<dyn Fn(String)> }
pub struct OverviewView { /* root: gtk::Box holding a gtk::Overlay; canvas, trays, zoom panel, breadcrumb, empty page */ }
impl OverviewView {
    pub fn new(on: Callbacks) -> Self;
    pub fn widget(&self) -> &gtk::Widget;
    pub fn set_data(&self, graph: Rc<Graph>, layout: Rc<Layout>, root: Option<PathBuf>);
    pub fn set_tabs(&self, chips: Vec<TabChip>, unmapped: Vec<Unmapped>, tagging_available: bool);
    pub fn set_issues(&self, issues: Vec<Issue>);
    pub fn set_selected_tab(&self, uuid: Option<String>);
    pub fn set_empty(&self, state: Option<EmptyState>, group_name: &str);
    pub fn pan_to(&self, id: &str);
}
```

GTK facts (gtk-apis report in understanding.md, verified against the registry): `adw::ToggleGroup` and `pangocairo` are unavailable; `gtk::render_layout` is deprecated under `v4_10`; text is drawn with `widget.create_pango_layout(Some(text))` + `snapshot.append_layout(&layout, &rgba)`; shapes with `snapshot.append_cairo(&graphene::Rect)` (cairo `Result`s need `let _ =`) or `gsk::PathBuilder` + `append_stroke` (`v4_14` is on). The repository has no `glib::wrapper!` subclass yet; use `relm4::gtk::{self, gdk, glib, graphene, gsk, pango}; use gtk::prelude::*; use gtk::subclass::prelude::*;`.

- [ ] **Step 1: The canvas subclass**

`mod imp { pub struct OverviewCanvas { state: RefCell<CanvasState> } }` with `#[glib::object_subclass] impl ObjectSubclass` (`NAME = "KabelsalatOverviewCanvas"`, `ParentType = gtk::Widget`) and `impl WidgetImpl { fn snapshot(&self, snapshot: &gtk::Snapshot) }`. `CanvasState` holds `Rc<Graph>`, `Rc<Layout>`, `Viewport`, `Vec<Edge>`, tabs per node (`HashMap<String, Vec<TabChip>>`), `issues`, `warning_pairs`, `hover: Option<BoxKey>`, `selected_tab`. The wrapper `glib::wrapper! { pub struct OverviewCanvas(ObjectSubclass<imp::OverviewCanvas>) @extends gtk::Widget; }`.

- [ ] **Step 2: Drawing per tier**

In `snapshot`: translate/scale by the viewport; for every `visible(layout, tier)` box draw the rounded rect (dashed outline when `mirror`, with a "◇" mark), the kind stripe in `graph.kind_color(kind).rgb()`, the title, and per tier: Far — descendant count, status bar (share of `status_color` among `descendants`), "● N tabs" (distinct tab uuids on it or its descendants), issue count; Mid — rows (status dot, title, tab count) up to `MID_ROWS` then "+ N more"; Near — cards (title, summary, status chip, tab chips, highlighted when `selected_tab`; mirrors collapse chips to "● N tabs"); edges as curves between box sides with a label pill from Near (`labels[0]` when `count == 1`, else "N links"), dashed when `warning`. Hovering a mirror highlights `layout.copies(id)`.

- [ ] **Step 3: Input**

`EventControllerScroll::new(VERTICAL)` → `viewport.zoom_at(pointer, 1.1^(-dy))`; `GestureZoom` → `zoom_at(bbox centre, scale_delta)`; `GestureDrag` → `pan`; `GestureClick` (primary) — single click on a tab chip → `on.open_tab(uuid)`, double click on a box → `fit(rect, margin)`; `connect_motion` for hover. Every change calls `queue_draw()` and a `tier_changed` callback so the slider, breadcrumb and Close overlay follow.

- [ ] **Step 4: Zoom panel, breadcrumb, trays**

Zoom panel (bottom-right): a vertical `gtk::Scale::with_range(VERTICAL, ln(0.05), ln(8), 0.01)` with `add_mark(ln(t.scale()), PositionType::Right, Some(t.label()))` for `Tier::ALL`, set from the viewport and setting it with `set_scale_at_center`; "−", "+", "Fit" buttons. Breadcrumb (top-left): a `gtk::Label` rebuilt from `breadcrumb(..)` as `"<group> overview › a › b"` with the last crumb bold. "Unmapped work" and "Data issues" trays (bottom-left): `gtk::ListBox`es in dashed frames; an issue row has the kind, nodes, detail and a "Resolve" button calling `on.resolve(index)`.

- [ ] **Step 5: Close tier**

An overlay child positioned over `focused(..)` through `connect_get_child_position`: a `gtk::TextView` fed from `pulldown_cmark::Parser::new(&node.body)` events into `TextBuffer` tags (`h1`…`h3`, `para`, `list`, `em`, `strong`, `code`, `codeblock`, `link:<n>` with a `HashMap<String, String>` to the href; images become their alt text); a `GestureClick` on the view resolves the `link:` tag under the pointer — a target that is a node id or a node file path → `pan_to(id)`, else `on.open_uri(href)`. Below the body: field rows (`node.fields`), "in: <parent>" pills, a "Links:" footer, and the panel "Tabs on this node · N" grouped by role, each row with role icon (`document-edit-symbolic` planning, `applications-engineering-symbolic` implementing, `system-search-symbolic` researching, `insert-link-symbolic` related), tab name, activity, age, opening the tab on click. When `tagging_available` is false the empty panel says "Tagging is unavailable".

- [ ] **Step 6: Empty states**

`set_empty(Some(NoRoot), name)` shows an `adw::StatusPage` naming `kabelsalat overview root -g <name> DIR` and linking `overview-format.md`; `Unreadable { root, error }` shows the path and the error. `None` shows the canvas.

- [ ] **Step 7: Build, lint**

Run: `cd /home/user/kabelsalat && cargo fmt --check`; push and check CI's `cargo clippy --all-targets -- -D warnings` (the module is `pub mod`, so unused `pub` items are fine). Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add src/overview/canvas.rs
git commit -m "Draw the overview canvas with its tiers, trays and Close view"
```

---

### Task 10: `app.rs` — toggle, mode stack, monitors, tagger, tmux env, Resolve, issues

**Files:**
- Modify: `src/app.rs:389-423` (`Group`: runtime overview state), `:581-708` (`App`: `mode_stack`, `overview_view`, `overview_monitors`, `tagger_running`, `tag_runs`, `tags`), `:715+` (`Msg`), `:917-1262` (`view!`: title widget, `mode_stack`), `:1502-1529` (timers), `:2103-2119` (helpers), `:2209-2230` and `:2131-2150` (`Group` literals), `:2361-2470` (`save_state`, `publish_groups`), `:3168-3202` (`sync_pane_host` → `sync_overview`), `:4709-4735` (`activate`), `:1542-1546` (`Select`), `:1918-1993` (`SpawnCommand`, reused), `:5224-5253` (`update_title` → tagger trigger), `:1468-1491` (file-monitor pattern)
- Test: none (wiring; every decision it wires is tested in Tasks 2–8)

**Interfaces:**
- Consumes: Task 2 fields; Task 3 `model::{scan_root, Graph}`; Task 4 `TmuxCtl::show_environment`, `claude::{projects_dir, transcript_path}`; Task 5 `Msg::SetOverviewRoot`, `IssueLine`; Task 6 `tagger::*`; Task 7 `layout::{layout, Metrics}`; Task 8 `issues::{derive, missing_link_pairs, resolve_prompt, Issue, TaggedTab}`; Task 9 `OverviewView`.
- Produces: `Msg::SetOverviewMode(bool)`, `Msg::OverviewFileChanged { group: usize, path: PathBuf, event: gio::FileMonitorEvent }`, `Msg::TagScan`, `Msg::TaggerDone { tab_uuid: String, mark: String, result: Result<String, String> }`, `Msg::ResolveIssue { group: usize, index: usize }`, `Msg::ShowTab(String /*uuid*/)`.

- [ ] **Step 1: Runtime state**

`Group` gains `overview: Option<OverviewState>` where `struct OverviewState { graph: Rc<Graph>, layout: Rc<Layout>, issues: Vec<Issue>, error: Option<String> }` (plus `overview_root`/`overview_mode` from Task 5). `App` gains `mode_stack: gtk::Stack`, `overview_view: OverviewView` (one shared view, re-fed per group; `attached_overview: Option<usize>`), `overview_monitors: HashMap<PathBuf, gio::FileMonitor>` (dropping one stops it, so they must be stored), `tags: HashMap<String /*tab uuid*/, (Tags, Option<Watermark>)>`, `tag_runs: HashMap<String, Instant>`, `tagger_running: bool`, `tagger_config: tagger::Config` (loaded once from `autostart::config_home().map(|h| tagger::config_path(&h))`).

- [ ] **Step 2: Header toggle and mode stack**

In `view!` (`:923`), add `#[wrap(Some)] set_title_widget = &gtk::Box { add_css_class: "linked", gtk::ToggleButton { set_label: "Terminals", #[watch] set_active: !model.active_overview_mode(), connect_toggled[sender] => move |b| if b.is_active() { sender.input(Msg::SetOverviewMode(false)) } }, gtk::ToggleButton { set_label: "Overview", set_group: Some(&terminals_button), #[watch] set_active: model.active_overview_mode(), #[watch] set_sensitive: !model.active_group_is_remote(), #[watch] set_tooltip_text: model.active_group_is_remote().then_some("No overview for remote groups"), connect_toggled[sender] => … SetOverviewMode(true) } }`. Wrap the start child of `browser_paned` (`:1235`): `set_start_child = &mode_stack.clone() { add_named[Some("terminals")] = &gtk::Box { …the existing tab_scroller + stack… }, add_named[Some("overview")] = model.overview_view.widget() }`. Helpers next to `active_group()` (`:2108`): `fn active_overview_mode(&self) -> bool`, `fn active_group_is_remote(&self) -> bool { self.active_group().and_then(|g| self.group_host(g)).is_some() }`.

- [ ] **Step 3: Messages**

`Msg::SetOverviewMode(bool)`: no-op (`return`) when the active group is remote or already has that mode (the `#[watch] set_active` echo); otherwise set `group.overview_mode`, call `sync_overview()`, fall through to the save. `fn sync_overview(&mut self)` — called at the end of `sync_pane_host()` (`:3168`) so every group-switch path reaches it: shows `mode_stack` page `"overview"` or `"terminals"`, refeeds `overview_view` from the active group's `OverviewState` (or `set_empty(Some(NoRoot))` / `Unreadable`), `set_selected_tab(active tab uuid)`, and grabs the active terminal's focus when returning to Terminals. `Msg::ShowTab(uuid)` (from a chip): map uuid → tab id through `self.tabs`, set that tab's group `overview_mode = false`, then `activate(id)` and `sync_overview()` even when `activate` returns false (already active). The `Select` arm (`:1542`) also clears the target group's `overview_mode` before `activate`.

- [ ] **Step 4: Loading and watching the root**

`fn load_overview(&mut self, group: usize)`: `scan_root(&root)` → `Graph::build` → `layout(&graph, &Metrics::default())` → `issues::derive(&graph, &self.tagged_tabs(group))`; an unreadable root (`!root.is_dir()`, or a directory `read_dir` cannot list) sets `error`. Called for every local rooted group when the state is restored and again on group activation whenever the group has no data or an error state — whatever its mode, so `kabelsalat overview issues` and the tagger see the data without the map being shown — from `SetOverviewRoot` (Task 5's handler now also reloads and rewatches when the group is active, and clears monitors on `--clear`), and from `Msg::OverviewFileChanged`: a `.md` `Created`/`Changed`/`ChangesDoneHint` → `graph.with_source((path, fs::read_to_string(..).map_err(|e| e.to_string())))`; `Deleted` → `without_source`; a created directory → new monitor; then re-layout, re-derive issues, refeed the view, `publish_groups()`, `return` (no save). Monitors follow the pattern at `:1468-1491`: `gio::File::for_path(dir).monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)` (so renamed or moved directories are followed), `connect_changed(move |_, file, _, event| …send(Msg::OverviewFileChanged{..}))`, one per directory below the root (skipping dot dirs), stored in `overview_monitors`.

- [ ] **Step 5: Tagger scheduling**

A 60 s timer copying `:1522-1528` sends `Msg::TagScan`; `update_title` (`:5249`, after `tab.title = title`) calls `self.consider_tagging(id)`. `fn consider_tagging(&mut self, id)`: skip remote tabs, tabs of groups without an overview, and when `!tagger::may_run(last_run_secs_ago, self.tagger_running)`; with `tab.claude`: `transcript_path(projects_dir, session)`, `fs::metadata` size and `tagger::last_uuid` of the file tail → `unchanged(mark, size, uuid)` → stop; else `delta_since(&text, mark)` and `significant(&d)` → build `prompt(roles, nodes, current, title, &d.text)` and spawn; without a transcript: `title_input(&tab.title)`, run only when the watermark differs. The spawn copies `set_linger` (`:4136-4145`): `std::thread::spawn` running `std::process::Command` from `config.tagger_command` with stdin piped (write the prompt, drop stdin), waiting at most `TIMEOUT_SECS` by polling `try_wait` (as `android::run_capture` does), then `input.send(Msg::TaggerDone { tab_uuid, mark: new_mark.render(), result })`. `tagger_running = true` until `TaggerDone`.

- [ ] **Step 6: Tagger result and tmux env**

`Msg::TaggerDone`: `tagger_running = false`; `Err(e)` → `eprintln!` and `return` (old tags and watermark kept); `Ok(stdout)` → `parse_response` (Err → log, return) → store `(tags, mark)` in `self.tags`; when `self.tmux` is `Some` and the group is local, `tmux.set_environment(uuid, ENV_LINKS, &serde_json::to_string(&tags))` and `set_environment(uuid, ENV_TAG_MARK, &mark)` (errors logged, as in `cdp_env_set` `:3400-3415`); re-derive the group's issues, refeed chips (`fn tab_chips(&self, group) -> (Vec<TabChip>, Vec<Unmapped>)` from `self.tags`, with `age_prefix`'s output as the age), `publish_groups()`, `return`. On restore (`restore_or_fresh`, after `:2310`'s `session_env_refresh`): for every reattached local tab, `tmux.show_environment(uuid, ENV_LINKS)` and `ENV_TAG_MARK` → seed `self.tags` (parse errors ignored). `tagging_available` = the command's binary resolves on `PATH`, checked once.

- [ ] **Step 7: Resolve and publishing**

`Msg::ResolveIssue { group, index }` (from the view's `resolve` callback, which the app wires with the active group id): look up the issue, the tab for a missing link, and send `Msg::SpawnCommand { group: GroupTarget::Existing(uuid), tab_uuid: state::new_uuid(), cwd: Some(root), argv: vec!["claude".into(), issues::resolve_prompt(&issue, &graph, &root, tab)] }` — the exact no-focus path of `kabelsalat run` (`:1918-1993`). `publish_groups` (`:2451`) maps `g.overview.issues` to `IssueLine { kind: i.kind.label().into(), nodes: i.nodes.clone(), file: i.file.clone(), detail: i.detail.clone() }`. The view's `open_tab` callback sends `Msg::ShowTab`, `open_uri` uses `gtk::UriLauncher::new(&uri).launch(..)`.

- [ ] **Step 8: Build, lint, test**

Run: `cd /home/user/kabelsalat && cargo fmt --check`; push and check CI (`cargo clippy --all-targets -- -D warnings && cargo test`). Expected: clean; all pass.

- [ ] **Step 9: Commit**

```bash
git add src/app.rs
git commit -m "Wire overview mode into the window, the tagger and the CLI snapshot"
```

- [ ] **Step 10: Manual check (needs the GUI; ask the user to drive if headless)**

Set a root with `kabelsalat overview root -g <group> docs`, toggle to Overview, zoom through the four tiers, edit a node file and watch it update, click a tab chip (group switches to Terminals, tab active), start `claude` in a tab and wait for its chip to appear, break a link and press Resolve (a new `claude` tab appears without focus). Switch groups: each keeps its mode. A remote group's toggle is insensitive.

---

### Task 11: Docs — README, SKILL.md, overview-format.md

**Files:**
- Modify: `README.md:8-32` (feature bullet), `:87-115` (Command line block and paragraph), `:186-201` (plugin paragraph), `:285-344` (new `### Overview mode` under Behaviour notes)
- Modify: `skills/kabelsalat/SKILL.md:3` (description), `:84` (exit-code row 2/3), end of file (new section)
- Create: `skills/kabelsalat/overview-format.md`
- Test: none (prose; checked by grep)

**Interfaces:**
- Consumes: Task 5's command forms and exit codes; spec §1 (normative for the format page).
- Produces: the agent-facing format page the Resolve prompt (Task 8) and the empty state (Task 9) name.

No plugin version bump: `.claude-plugin/plugin.json` moves only in release commits.

- [ ] **Step 1: README**

Feature bullet: `- **A map of the work.** Each group can switch its terminal area to an overview: nodes read from Markdown files in the project, drawn by nesting and zoom level, with the tabs working on each node one click away and data gaps listed for an agent to fix.` Command line block: two lines `kabelsalat overview root -g web docs      # read "web"'s overview from ./docs` and `kabelsalat overview issues -g web          # one data issue per line, nothing when clean`; extend the exit-code sentence with "2 usage (or `overview root` DIR missing), 3 … remote (for `browser`, `overview`)". Plugin paragraph: "… over CDP, and to keep a group's overview data in the format `skills/kabelsalat/overview-format.md` describes". New `### Overview mode` subsection: the toggle and per-group mode, the empty state, remote groups insensitive, tags in the tmux env (`KABELSALAT_LINKS`, `KABELSALAT_TAG_MARK`), `$XDG_CONFIG_HOME/kabelsalat/overview.json` (`tagger_command`, `roles`), degradation without tmux or the tagger command.

- [ ] **Step 2: SKILL.md**

Line 3: append "Also use to set a group's overview root or check its overview data for issues (`kabelsalat overview`)." Row 2 of the exit-code table gains "(overview root) DIR does not exist"; row 3 gains `overview` next to `browser, android` for a remote group. Append after the last line:

```
## Keeping the group's overview current

Each group can show an overview: a map of nodes read from Markdown files
with YAML frontmatter below one directory of the project. The format is
documented in [overview-format.md](overview-format.md); read it before
creating or editing node files.

    kabelsalat overview root [-g <name|uuid>] DIR    # set the overview root
    kabelsalat overview root [-g <name|uuid>] --clear
    kabelsalat overview issues [-g <name|uuid>]

`issues` prints one issue per line, tab-separated: `kind`, `node ids`
(comma-separated), `file`, `detail`. No output and exit 0 means the data is
clean. Run it after every change to node files. kabelsalat infers which
tabs work on which node by itself — never write tab names into the files.
```

- [ ] **Step 3: overview-format.md**

Plain Markdown, no frontmatter, H1 "Overview node files", sections "Files", "Keys" (the spec's table), "Kinds" (the kind-node example), "Data issues" (the five kinds and what `kabelsalat overview issues` prints), "Checking your work". Content is spec §1 verbatim in substance; examples as fenced ```` ```yaml ```` blocks where the language label helps, otherwise 4-space indented like SKILL.md. Second person, under 120 lines.

- [ ] **Step 4: Check the wording**

Run: `grep -n "overview" README.md skills/kabelsalat/SKILL.md | wc -l` (≥ 8) and `grep -c '^| `' skills/kabelsalat/overview-format.md` (the keys table has 8 rows). Expected: both hold; every command form in the docs parses per Task 5's tests.

- [ ] **Step 5: Commit**

```bash
git add README.md skills/kabelsalat/SKILL.md skills/kabelsalat/overview-format.md
git commit -m "Document overview mode and the node file format"
```

---

### Task 12: Full verification and acceptance

**Files:** none modified (fixes found here go back to the task that owns them, with a failing test first where the fix is in a pure module).

- [ ] **Step 1: The CI gate**

Run: `cd /home/user/kabelsalat && cargo fmt --check`; in a harness listing every pure module (`state`, `cli`, `claude`, `remote`, `tmuxctl`, `autostart`, `overview::{model, layout, tagger, issues}`): `cargo clippy --all-targets -- -D warnings && cargo test`. Then push and confirm `.github/workflows/ci.yml` is green (`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on Fedora). Expected: clean; all tests pass.

- [ ] **Step 2: Degradation**

1. `PATH=/nonexistent ./target/debug/kabelsalat` (no tmux): Overview works; tags stay in memory; restart loses them; nothing panics.
2. `overview.json` with `"tagger_command": ["/nonexistent"]`: the overview shows nodes, no chips, "Tagging is unavailable" in the Close panel; the log shows the spawn error once per significant change, not per tick.
3. `kabelsalat overview root -g web /nonexistent` → exit 2 and `kabelsalat: no such directory: /nonexistent`; `overview root` on a remote group → exit 3.
4. Delete the root directory while in Overview: the empty state names the path and the error; the group's terminals keep working.
5. Break one node file's YAML: it appears as `unreadable-file` in the tray and in `kabelsalat overview issues`; every other node still draws.

- [ ] **Step 3: Acceptance (needs the GUI and a repository in the format)**

1. Convert a small docs tree with `overview-format.md` only (two projects, six ADRs, one node with two parents, one kind node), `overview root`, toggle: Far shows two boxes with counts and status bars; Mid rows; Near cards with a dashed mirror; Close renders the body and the links footer; the breadcrumb and slider follow the zoom.
2. Start `claude` in two tabs working on different ADRs; within two minutes both chips appear on the right cards; `KABELSALAT_LINKS` is set in their tmux sessions (`tmux -S $XDG_RUNTIME_DIR/kabelsalat/tmux.sock show-environment -t ks-<uuid> KABELSALAT_LINKS`); quit and restart the GUI: the chips are back.
3. Tag a tab with two unconnected nodes (edit the transcript scenario or add a link in a file and remove it): the dashed warning edge appears from Near, the tray lists `missing-link`, `kabelsalat overview issues` prints it, Resolve opens a `claude` tab in the root without focus, and the issue disappears once the edge is added.
4. Switch groups and back: each group keeps its mode; a remote group's toggle is insensitive with the tooltip.

- [ ] **Step 4: Report**

Summarise in the PR description which manual checks ran, the tagger command used, and anything that behaved differently from this plan.

---

## Self-review

Spec coverage, section by section:

- **Goal / Decisions** — structured data in the repository, one native renderer, no meaning attached to kinds/statuses/edges (Task 3); one overview per group rooted at a directory (Tasks 2, 5, 10); mirrors for several parents (Tasks 3, 7, 9); level of detail by nesting with continuous zoom (Tasks 7, 9); inferred tab links stored in the tmux env, never in the repository (Tasks 4, 6, 10); overlaps shown, never judged (Task 9); missing links as data issues resolved by an agent in a new tab (Tasks 8, 10).
- **§1 Node format** — files, keys, kinds, title default, scalar-or-list values, format issues (Task 3); the agent-facing page `overview-format.md` and its link from SKILL.md (Task 11).
- **§2 Group setting and mode** — `overview_root`/`overview_mode` with `serde(default)` (Task 2); the two-segment toggle, per-group restore on switch, the mode stack over the terminal area, tab activation switching to Terminals, the empty state naming the CLI command and the format page, remote groups insensitive with a tooltip (Tasks 9, 10).
- **§3 CLI** — `overview root DIR | --clear`, `overview issues` output, `KABELSALAT_GROUP` default, snapshot-based answer, no mode/group/focus change (Task 5; `publish_groups` fill in Task 10).
- **§4 Rendering** — inside-out layout, mirrors dashed with a mark and hover highlight, depth, edge lifting and bundling (Task 7); tiers and their contents, focused card, status bar, edge labels from Near, breadcrumb, zoom controls (Tasks 7, 9); tabs on the map per tier and the Unmapped tray (Tasks 9, 10); Markdown via pulldown-cmark into a TextView with tags, node links pan, other links open externally, images as alt text (Task 9); one `gio::FileMonitor` per directory with incremental re-parse and new-directory monitors (Task 10).
- **§5 Tagging** — output shape, env keys, watermark forms, triggers (title change, 60 s scan), the cheap check, significance, one process at a time and the two-minute gap, title input for transcript-less tabs, `overview.json`, prompt contents, bare/envelope response, 60 s timeout, failures keep the old tags, remote groups untagged (Tasks 4, 6, 10).
- **§6 Data issues** — the five kinds and their rules, the tray, the Far count, the dashed warning edge, Resolve through the `kabelsalat run` path with the prepared prompt (Tasks 3, 7, 8, 9, 10).
- **§7 Code layout** — exactly the module split named (Tasks 1, 3, 5, 6, 7, 8, 9, 10); the two dependencies (Task 1).
- **§8 Degradation** — without tmux, without the tagger command, missing/unreadable root, one unparsable file (Tasks 3, 9, 10; verified in Task 12).
- **Out of scope** — nothing in this plan converts a repository, links overviews, edits nodes from the canvas, flags overlaps or tags remote groups.

Where this plan departs from the spec's letter, and why (each is also stated in its task):

1. **Tier visibility uses the placed copy's `level`, not the semantic depth.** The spec says level of detail follows nesting depth and a node's depth is its shortest parent chain; a node with two parents at different depths would then be visible in one container and hidden in the other at the same tier. Visibility by the copy's own nesting (`PlacedBox.level`) is what the eye expects; `Graph::depth` stays the semantic depth for everything else (Task 7, contract decision 8).
2. **`cli::IssueLine` is independent of `overview::issues::Issue`.** The snapshot type uses a `String` kind so `cli.rs` stays a leaf module that compiles without the overview tree; `app.rs` converts with `IssueKind::label()` (Tasks 5, 10).
3. **Exit code 2 for a missing DIR.** The spec says `DIR … must exist` but names no code; the check is a usage-level refusal before the GUI is asked, so it shares `EXIT_USAGE` with the other argument errors and is documented in `help_text`, README and SKILL.md (Tasks 5, 11).
4. **`Color` is a fixed palette with `derived` never Grey.** The spec names the eight colours and says a kind without a kind node gets a colour derived from its name; Grey is reserved for unlisted statuses so a derived kind colour cannot be mistaken for "no status" (Task 3).
5. **A parent cycle drops the closing entry rather than all of them.** The spec says "the closing `parent` entry is ignored for drawing"; this plan fixes which one closes it (frontmatter order, first entry that would lead back) so the result is deterministic (Task 3).
6. **The toggle is two linked `gtk::ToggleButton`s.** `adw::ToggleGroup` needs libadwaita 1.7; CLAUDE.md guarantees 1.5 (Task 10).
7. **The terminal area is wrapped in a `gtk::Stack` instead of swapping `browser_paned`'s start child.** Re-parenting the VTE box on every toggle would disturb focus and size (Task 10).
8. **One shared `OverviewView` re-fed per group** rather than one widget per group; the state per group lives in `Group.overview` (Task 10).
9. **Tags are seeded from tmux on restore, not read on every tick.** `show_environment` runs once per reattached tab at startup; afterwards the in-memory map is authoritative and tmux is written, not read (Task 10).
10. **The tagger's "transcript" significance also applies when the watermark's uuid is gone** (compacted or rotated transcript): the whole file counts as new, which errs towards one extra run rather than a stale tag (Task 6).
11. **No plugin version bump** in Task 11, following the Android plan's rule.
12. **The Resolve tab's title is "claude".** `command_title` takes the basename of `argv[0]`; a dedicated title would need a new path through `add_tab`, which is not worth a variant (Task 10).

Mechanical check: Task 1's module skeleton and Cargo lines were applied to the working tree and compiled in the harness; Tasks 2–8 were written against the harness template (the main crate does not build on this machine, so `control.rs`, `app.rs` and `canvas.rs` are verified in CI per the Mechanics). Every signature in the Interfaces blocks is copied from `contract.md`, which is binding for the implementing agents, so modules written in parallel fit without renames. Line numbers were taken from the six reports in `understanding.md`, all from `main` at `21b2cfd`.

Placeholder scan: every task shows its tests by name with the key assertions; the deliberately interim values (`overview_root: None, overview_mode: false` in `save_state`, Task 2; `issues: Vec::new()` in `publish_groups`, Task 5) are true at those points and are replaced in Tasks 5 and 10, which say so. Types used across tasks are defined in the task listed under their "Produces". No step says "TODO", "TBD" or "similar to above".
