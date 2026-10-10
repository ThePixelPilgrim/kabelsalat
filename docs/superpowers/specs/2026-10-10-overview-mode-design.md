# Overview mode — design

Date: 2026-10-10

## Goal

Give each group a visual overview of its work, shown in place of the terminal
area and switched to from the header bar. The overview is a zoomable map of
nodes (projects, ADRs, themes — whatever the project calls them) read from
Markdown files in the project's repository. Agents keep those files current;
kabelsalat draws them, shows which tabs are working on which node, and makes
gaps in the data visible so an agent can fix them. The terminal stays the place
for detail: every tab shown on the map is one click away.

The first user is a repository with ~250 ADRs, ~100 sub-ADRs and a growing set
of projects; the format must stay usable at that size.

Mockups (generated concepts, fictional data):
[first exploration](../../mockups/explorations/2026-10-10-overview-mode/README.md),
[many tabs per node](../../mockups/explorations/2026-10-10-overview-mode-tabs/README.md).

## Decisions

- Agents publish **structured data**, kabelsalat owns one native renderer. No
  web view, no agent-authored HTML.
- The data lives **in the repository** as many small Markdown files with YAML
  frontmatter, not in kabelsalat's state and not in one big file.
- kabelsalat attaches **no meaning** to kinds, statuses or edge names. "Project"
  and "ADR" are conventions of the repository, not of the editor.
- **One overview per group**, rooted at a directory. Overviews of different
  groups never link to each other. No multi-repository discovery.
- A node may have **several parents**; it is drawn in each, as a mirror.
- **Level of detail follows nesting depth.** Zoom is continuous; the tier
  switches at scale thresholds.
- **Tab ↔ node links are inferred by kabelsalat**, not written by agents and
  never stored in the repository: a small model classifies each tab's recent
  work, and the result lives in the tab's tmux session environment.
- Several tabs on one node are **shown, never judged**.
- A tab linked to two nodes that have no direct edge between them is a **data
  issue**: shown to the user, resolved by an agent in a new tab.

## 1. Node format

The format is documented for agents in `skills/kabelsalat/overview-format.md`
(shipped with the plugin, linked from `skills/kabelsalat/SKILL.md`), short
enough that an agent in any project can adopt it from that page alone. This
section is the normative version of that page.

### Files

Every `*.md` file below the group's overview root whose content starts with a
YAML frontmatter block (`---` line, YAML, `---` line) containing an `id` is a
node. Other files are ignored, as are directories whose name starts with `.`.
The Markdown after the frontmatter is the node's body.

```markdown
---
id: adr-118
kind: adr
title: Emergency access account
summary: controlled recovery when all admins are locked out
status: proposed
parent: [theme-access, P-004]
links:
  builds-on: [adr-064]
  related: [adr-122]
---
# ADR-118: Emergency access account

## Context
…
```

### Keys

| Key | Required | Value | Meaning to kabelsalat |
|---|---|---|---|
| `id` | yes | string of `A-Z a-z 0-9 . _ -` | Unique within the root. All references use it. |
| `title` | no | string | Display name. Default: the body's first `# ` heading, else `id`. |
| `kind` | no | string | Selects the node's style (see Kinds). Default `node`. |
| `summary` | no | string | One line, shown from the Near tier. |
| `status` | no | string | Free text. The first word, lower-cased, selects the status colour. |
| `parent` | no | id or list of ids | Containment. Several parents allowed. No parent: top level. |
| `links` | no | map of edge name → id or list of ids | Directed typed edges. The name is free text and is shown on the edge. |
| anything else | no | scalar or list of scalars | Shown as a field row at the Close tier. |

Edges exist only under `links`; no other key is read as a reference.

### Kinds

A node with `kind: kind` styles the kind named by its `title` and is not drawn
on the map:

```yaml
---
id: kind-adr
kind: kind
title: adr
color: amber          # one of: blue green amber red purple pink teal grey
statuses:             # status first word → colour
  accepted: green
  implemented: blue
  proposed: amber
  superseded: grey
  rejected: grey
---
```

A kind without a kind node gets a colour derived from its name. A status not
listed for its kind is drawn grey.

### Data issues raised by the format

Derived on every load, never stored (see section 5): a file whose frontmatter
does not parse; a duplicate `id`; a `parent` or `links` target that does not
exist; a parent cycle (the closing `parent` entry is ignored for drawing).

## 2. Group setting and mode

- `SavedGroup` gains `overview_root: Option<PathBuf>` and
  `overview_mode: bool` (both `serde(default)`).
- The header bar gets a two-segment toggle "Terminals" | "Overview". It reflects
  and sets the active group's `overview_mode`. Switching groups restores that
  group's mode.
- Overview mode replaces the terminal area (the start child of
  `browser_paned`); the sidebar and the browser/Android pane host stay.
- Activating a tab — in the sidebar, or from a tab chip on the map — switches
  the group to Terminals and shows that tab.
- A group without `overview_root` shows an empty state in Overview mode that
  names the CLI command below and links the format page.
- Remote groups (`host` set) have no overview in this version; the toggle is
  insensitive with a tooltip saying so.

## 3. CLI

Following the existing `Cli` → `Action` → `control::request_*` → `Msg` pattern:

- `kabelsalat overview root [-g GROUP] DIR` — set the group's overview root.
  `DIR` is resolved against the caller's working directory and must exist.
- `kabelsalat overview root [-g GROUP] --clear` — unset it.
- `kabelsalat overview issues [-g GROUP]` — print the group's current data
  issues, one per line, tab-separated: issue kind, node ids, file, detail.
  Exit 0 with no output when there are none. This is how an agent checks its
  work without the GUI.

The group defaults to the caller's `KABELSALAT_GROUP`. The issue list reaches
the command-line handler through the published group snapshot, like the rest
of `GroupInfo`. A CLI call never changes the mode, the active group or focus.

## 4. Rendering

### Layout

- Containers are laid out inside out: a node's children are placed in its box
  by a layered layout of the edges among them, the box sized to fit; the top
  level is laid out the same way over the top-level nodes.
- A node with several parents is placed in each; every copy after the first is
  drawn with a dashed outline and a mirror mark, and hovering one highlights
  the others.
- A node's depth is the length of its shortest parent chain; top-level nodes
  have depth 0.
- An edge whose endpoints are hidden at the current tier is drawn between the
  nearest visible ancestors. Edges between the same pair of visible boxes are
  bundled into one line labelled with the count ("14 links").

### Tiers

Continuous zoom by scroll or pinch; drag pans; double-click a box zooms to fit
it; a vertical slider with marks "Far", "Mid", "Near", "Close" jumps to the
tier's scale; "−", "+", "Fit" buttons. The tier follows the scale:

| Tier | Depth-0 boxes | Their children | Deeper |
|---|---|---|---|
| Far | title, descendant count, status bar, tab count, issue count | hidden | hidden |
| Mid | as Far | one-line rows: status dot, title, tab count | hidden (`+ N more` row when cut) |
| Near | as Far | cards: title, summary, status chip, tab chips | as Mid |
| Close | as Far | the focused card shows the rendered body, field rows, links and the tabs panel | as Near |

The focused card is the visible card nearest the viewport centre. The status
bar shows the share of each status colour among the box's
descendants. Edge labels appear from Near.

A breadcrumb above the canvas names the box under the viewport centre and its
ancestors.

### Tabs on the map

- Far: "● N tabs" on a box (distinct tabs linked to it or its descendants).
- Mid: a dot and count per row.
- Near: a chip per tab with its role icon and tab name; the tab selected in the
  sidebar is highlighted. Mirrors collapse their chips to "● N tabs".
- Close: a panel "Tabs on this node · N", grouped by role, each row with role
  icon, tab name, activity line and age, opening the tab on click.
- An "Unmapped work" tray lists tabs whose tagging found no node, with their
  topic.

### Markdown

The Close tier renders the body with `pulldown-cmark` into a `gtk::TextView`
with tags for headings, paragraphs, lists, emphasis, inline code, code blocks
and links. Links to other node files or ids pan to that node; other links open
in the default handler. Images are shown as their alt text.

### Updates

The root is read on group activation and watched with one `gio::FileMonitor`
per directory. A change re-parses the changed file and recomputes layout;
created directories gain a monitor.

## 5. Tagging: which tab works on what

### Output

Per tab, a JSON object:

```json
{"links": [{"node": "adr-118", "role": "implementing"},
           {"node": "adr-064", "role": "related"}],
 "activity": "wiring the unseal check",
 "topic": null}
```

`topic` is set (and `links` empty) when the work matches no node. Roles come
from the configuration; default `planning`, `implementing`, `researching`,
`related`.

### Storage

On the tab's tmux session, via `tmux set-environment`:

- `KABELSALAT_LINKS` — the JSON object above.
- `KABELSALAT_TAG_MARK` — the watermark, `<last message uuid>:<byte offset>`
  of the transcript, or `title:<title>` for tabs without one.

Nothing is written to the repository or to `state.json`. Tags survive a GUI
restart with the tmux server. Without tmux, tags live in memory only.

### When the model runs

1. Triggers: a tab's title changes; a periodic scan every 60 s.
2. Cheap check: stat the tab's claude transcript
   (`~/.claude/projects/<escaped cwd>/<session id>.jsonl`, from the tab's
   `ClaudeSession`). If size and last message uuid match the watermark, stop.
3. Significance: the new part since the watermark must contain at least one
   new user prompt (a `user` message that is not a tool result), or at least
   six new assistant messages. Otherwise stop and keep the old watermark.
4. At most one tagger process runs at a time; at most one run per tab per two
   minutes.
5. Tabs without a transcript (plain shells, other agents) use the tab title
   as input; the watermark is the title.

### Model call

- Command from `$XDG_CONFIG_HOME/kabelsalat/overview.json`, key
  `tagger_command` (argv array), default
  `["claude", "-p", "--model", "haiku", "--output-format", "json"]`; key
  `roles` overrides the role list. A missing file means defaults.
- The prompt goes to stdin and contains: the role list; every node's `id`,
  `kind` and `title`; the tab's current links; the tab title; the transcript
  text since the watermark (message text only, tool output dropped, last
  40 000 characters).
- stdout is either the JSON object or the `claude --output-format json`
  envelope whose `result` string contains it. Links naming an id outside the
  node list are kept and become "unknown node" issues.
- Timeout 60 s. Any failure keeps the previous tags and watermark and is
  logged; the next significant change retries.
- Remote groups are not tagged.

## 6. Data issues

Derived from the parsed nodes and the current tags on every change:

| Issue | Rule |
|---|---|
| unreadable file | frontmatter does not parse |
| duplicate id | two files share an `id` |
| unknown node | a `parent`, `links` target or tag names an id that does not exist |
| parent cycle | following `parent` returns to the start |
| missing link | a tab links nodes A and B, and neither A→B nor B→A exists under `links` (any edge name) |

Shown as: a "Data issues" tray listing every issue; an issue count on Far
boxes; a dashed warning edge between the two nodes of a missing link from the
Near tier.

Each issue has a **Resolve** button. It opens a new tab in the group, the way
`kabelsalat run` does (no focus change, no group switch), in the overview root,
running `claude` with a prepared prompt: the issue, the files involved, for a
missing link the tab name, its roles and activity, and the instruction to fix
the data per `overview-format.md` — for a missing link, add a direct edge with
a name that fits, or state that the tag is wrong and change nothing. Nothing is
ever typed into a running session. An issue disappears when the data no longer
produces it.

## 7. Code layout

Pure modules, unit-tested:

- `src/overview/model.rs` — frontmatter parsing, the node graph, depth,
  status colour lookup, format issues.
- `src/overview/layout.rs` — container layout, tier for a scale, visible-node
  set, edge lifting and bundling, hit-testing.
- `src/overview/tagger.rs` — watermark and significance rules over transcript
  text, the prompt, response parsing (bare and envelope), config parsing.
- `src/overview/issues.rs` — missing-link and unknown-node issues from graph +
  tags, the Resolve prompt.
- `src/cli.rs` — `overview root` / `overview issues` parsing and output.

GTK wiring, untested per the repository rule:

- `src/overview/canvas.rs` — the canvas widget, drawing and input, card
  widgets, the Markdown text view.
- `src/app.rs` — toggle, mode per group, file monitors, tagger scheduling and
  process spawning, `tmux set-environment`, Resolve spawning.

New dependencies: `serde_yaml_ng` (frontmatter), `pulldown-cmark` (Markdown).

## 8. Error handling and degradation

- Without tmux or with tmux older than 3.2: the overview works; tags are kept
  in memory and lost on restart.
- Without the tagger command: the overview works without tab links; the empty
  tab panels say tagging is unavailable.
- A missing or unreadable root shows the empty state with the path and the
  error; nothing else in the group is affected.
- One unparsable file never hides the others; it becomes an issue.

## Out of scope

- Converting any existing repository to the format. For the first user this is
  a separate task in that repository, done by its agents from
  `overview-format.md`.
- Several repositories per overview, links between overviews, discovery.
- Editing nodes from the canvas; manual positions.
- Flagging overlaps (several tabs on one node).
- Remote groups.
