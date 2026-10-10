# Overview node format

kabelsalat can show a group's work as a map: boxes for the things the
project tracks — projects, decisions, themes, whatever it calls them —
nested and linked, with the group's tabs drawn on the node each one works on.
The map is read from Markdown files in the repository. You keep those files
current; kabelsalat only draws them and points out gaps in the data. This
page is everything you need to write or fix a node.

## Files

The user names one directory per group as the overview root (`kabelsalat
overview root DIR`). Every `*.md` file below it whose content starts with a
YAML frontmatter block — a `---` line, YAML, a `---` line — containing an
`id` is a node. Files without such a block or without an `id` are ignored, as
is every directory whose name starts with `.`. The Markdown after the
frontmatter is the node's body.

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

One node per file. Where a file sits below the root does not matter: nesting
on the map comes from `parent`, not from directories. The format is meant
for repositories with hundreds of nodes, so keep each file small and its
`summary` to one line.

## Keys

| Key | Required | Value | Meaning to kabelsalat |
|---|---|---|---|
| `id` | yes | string of `A-Z a-z 0-9 . _ -` | Unique within the root. All references use it. |
| `title` | no | string | Display name. Default: the body's first `# ` heading, else `id`. |
| `kind` | no | string | Selects the node's style (see Kinds). Default `node`. |
| `summary` | no | string | One line, shown when the user zooms in. |
| `status` | no | string | Free text. The first word, lower-cased, selects the status colour. |
| `parent` | no | id or list of ids | Containment. Several parents allowed. No parent: top level. |
| `links` | no | map of edge name → id or list of ids | Directed typed edges. The name is free text and is shown on the edge. |
| anything else | no | scalar or list of scalars | Shown as a field row on the node's card. |

Edges exist only under `links`; no other key is read as a reference. A key
kabelsalat does not know (`owner`, `due`, `tags`, …) is kept and shown as a
field, so add whatever the project needs.

kabelsalat attaches no meaning to kinds, statuses or edge names. `project`,
`adr`, `accepted` and `builds-on` are conventions of your repository, not of
the editor: pick names that suit the project and use them consistently.

## Kinds and colours

A node with `kind: kind` is not drawn on the map. It styles the kind named by
its `title`:

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

The kind's colour marks its nodes. `statuses` maps the first word of a
node's `status`, lower-cased, to a colour, so `status: Proposed since
2026-09` is drawn amber. A kind without a kind node gets a colour derived
from its name; a status not listed for its kind is drawn grey. One kind node
per kind, usually all of them in one directory.

## Parents and mirrors

`parent` nests a node inside another. A node without a parent is drawn at
the top level. A node may name several parents and is then drawn inside each
of them, every copy after the first as a mirror with a dashed outline; all
copies are the same node, so there is one file to edit. Level of detail
follows depth — a deeply nested node appears only when the user zooms in —
so nest to the degree the map should hide, not to mirror a directory tree.

## Links

`links` holds directed, typed edges: the key is the edge name, the value an
id or a list of ids. The name is shown on the edge, so write it to be read:
`builds-on`, `supersedes`, `related`, `blocks`. State a link once, on the
side where it reads naturally; `adr-118` listing `builds-on: [adr-064]` is
enough, and `adr-064` need not point back.

## Body and title

The body is rendered when the user zooms in on the node. Write ordinary
Markdown: headings, paragraphs, lists, emphasis, inline code, code blocks,
links. A link to another node's file or to a node id pans the map to that
node; any other link opens in the default handler; an image is shown as its
alt text.

Without a `title` the node is named by the body's first `# ` heading, and
without that by its `id`.

## Data issues

kabelsalat derives these on every load and stores nothing. Each appears in
the overview's "Data issues" tray with a **Resolve** button, which opens a
new tab in the group running `claude` with the issue and the files involved —
that is probably how you got here.

| Issue | Rule | Fix |
|---|---|---|
| `unreadable-file` | the frontmatter does not parse, or its `id` is not a plain value or uses other characters (a block with no `id` is not a node and raises no issue) | fix the YAML; one bad file never hides the others |
| `duplicate-id` | two files share an `id` | rename one; until then the first file by path keeps the id |
| `unknown-node` | a `parent` or `links` target names an id no file has | create the node or correct the id |
| `parent-cycle` | following `parent` leads back to the start | remove one `parent` entry; until then the closing one is ignored |
| `missing-link` | a tab works on two nodes with no direct edge between them (see below) | add an edge, or change nothing if the pairing is wrong |

Check your work without the GUI:

    kabelsalat overview issues               # the group is taken from your KABELSALAT_GROUP
    kabelsalat overview issues -g <name|uuid>

One line per issue, tab-separated: the issue kind as in the table, the node
ids involved joined by `,`, the file as an absolute path (empty when the issue
is not about one file), and a detail sentence for humans — read it, do not
parse it. No output and exit 0 means the data is clean. Exit 1 means
kabelsalat is not running; exit 3 an unknown, ambiguous or remote group —
remote groups have no overview.

    unknown-node	adr-118,adr-999	/home/you/project/docs/adr/adr-118.md	'adr-118' links to unknown node 'adr-999' under 'builds-on'
    missing-link	adr-064,adr-118		tab 'recovery flow' links both; no edge between them

Run it after every batch of edits; a file watcher re-reads changed files, so
the output reflects what is on disk now.

## How tabs are linked to nodes

You never write tab links. kabelsalat reads each tab's recent work — its
`claude` transcript, or only the tab title for other tabs — asks a small
model which nodes it concerns and in what role (`planning`, `implementing`,
`researching`, `related` by default), and keeps the answer in the tab's tmux
session environment. Nothing of this reaches the repository, so a node file
lists no tabs, and a tab shown on the wrong node is not something you fix in
the data. A tag naming an id no file has is reported as `unknown-node` with
the tab's name in the detail; fix it only when a node by that id should
exist.

### What a missing link means

A tab is linked to nodes A and B, and neither A nor B names the other under
`links`, under any edge name. Either the two really are connected and the
data lacks the edge, or the tagging paired them by accident. Decide from the
two files and the tab's work — the Resolve prompt gives you the tab's name,
its roles on each node and its last activity:

- Connected: add one direct edge, on the side where it reads naturally, with
  a name that says what the connection is. Do not invent a vague edge just
  to silence the issue.
- Not connected: change nothing and say that the tag is wrong. The issue
  stays until the tagging moves on; that is expected.

An issue disappears as soon as the data no longer produces it; there is
nothing to acknowledge or clear.
