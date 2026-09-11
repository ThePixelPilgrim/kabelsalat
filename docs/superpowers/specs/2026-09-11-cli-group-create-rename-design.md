# CLI: create groups on run, rename groups

Date: 2026-09-11. Extends the 2026-07-26 CLI and agent skill design, whose
"creating groups" non-goal is superseded by this document.

## Problem

An agent using the `kabelsalat` skill can only spawn into groups that already
exist. It cannot create a group for a new project or give a group a name; both
are GUI-only actions today (`Msg::NewGroup` makes an unnamed group, the
Ctrl+Shift+R settings dialog renames it). A `run` against a missing name fails
with exit 3 and there is nothing the agent can do about it.

## Goals

- `run` can create a named group holding the first tab when no group matches.
- A `rename` subcommand renames an existing group.
- The skill prose tells agents when and how to use both.

## Non-goals

- Deleting or closing groups from the CLI.
- Setting the per-group default URL from the CLI.
- Standalone empty group creation. Groups are pruned when their last tab
  closes (`prune_empty_groups`), so an empty group has no representation.

## Command grammar

```
kabelsalat run --group <name|uuid> [--create] [--cwd DIR] -- COMMAND [ARGS...]
kabelsalat rename <name|uuid> <new-name>
```

- `--create` is a boolean flag, valid only with `run`.
- `rename` takes exactly two positional arguments. A missing, extra, or empty
  `<new-name>` is a usage error (exit 2). The new name is trimmed.

## Resolution rules

`resolve_group` is unchanged. On top of it:

**run with `--create`**

| resolution | result |
|---|---|
| unique match (uuid or name) | spawn into that group, as today |
| `NotFound` | spawn into a new group named `<selector>` |
| `Ambiguous` | exit 3, same message as today, no spawn |

Without `--create`, `NotFound` stays exit 3 as today. A selector that looks
like a uuid but matches nothing is not special-cased: with `--create` it
becomes the group's name.

**rename**

| condition | result |
|---|---|
| target `NotFound` / `Ambiguous` | exit 3, same messages as `run` |
| another group already has `<new-name>` | exit 3, stderr: `a group named '<new-name>' already exists (<uuid>)` |
| `<new-name>` equals the target's current name | exit 0, no-op |
| otherwise | exit 0, group renamed |

The duplicate check is exact and case-sensitive, matching name resolution.
Unnamed groups can be renamed by uuid. The GUI dialog keeps its permissive
behaviour; only the CLI refuses duplicates.

## Data flow

`src/cli.rs` (pure):

```rust
pub enum GroupTarget { Existing(String /* uuid */), Create { name: String } }
pub enum Action {
    Spawn { group: GroupTarget, cwd: PathBuf, argv: Vec<String> },
    Rename { group_uuid: String, name: String },
}
pub struct Outcome { stdout, stderr, code: u8, action: Option<Action> }
```

`Outcome.action` replaces `Outcome.spawn`. `Cli` gains `Run { create: bool, .. }`
and `Rename { group: String, name: String }`; both `needs_instance()`.

`src/control.rs`: `handle_command_line` matches on the action.
- `Spawn`: mints the tab uuid, prints it, sends
  `Msg::SpawnCommand { group_uuid: Option<String>, new_group: Option<String>, tab_uuid, cwd, argv }`
  where exactly one of `group_uuid` / `new_group` is `Some`. (Concretely the
  message carries the `GroupTarget` enum.)
- `Rename`: sends `Msg::RenameGroup { group_uuid, name }`. Prints nothing on
  success.
- A failed send prints "no window to spawn into" and returns 1, as today.

`src/app.rs`:
- `SpawnCommand` with `GroupTarget::Create { name }`: `create_group()`, set
  `name`, then `add_tab` into it and `rebuild_list()`. Palette and list
  position follow `create_group` (appended at the end). No activate, no window
  raise, no active-group change, browser pane untouched. Same invariant as the
  existing spawn path.
- `RenameGroup`: find the group by uuid, set `name`, `save_state()` (which
  republishes the snapshot), `rebuild_list()`. Unknown uuid logs
  `rename request for unknown group <uuid>` to stderr and returns.

Snapshot contents (`GroupInfo { uuid, name, tabs }`) are unchanged.

## Exit codes

Unchanged: 0 ok, 1 not running, 2 usage, 3 group not found / ambiguous /
name clash.

## Skill and docs

- `skills/kabelsalat/SKILL.md`: add `--create` to the run synopsis; add a
  "Creating a group" paragraph (use `--create` when the user asks for a new
  group, or names one that `groups` does not list; the name is used verbatim
  and is case-sensitive; the new group appears in the sidebar without being
  activated, so tell the user); add a "Renaming a group" section with the
  duplicate rule; extend the exit-code table row for 3.
- `README.md` command line section: both additions and the exit-code note.

## Tests

`src/cli.rs`:
- `run_parses_the_create_flag`
- `create_reuses_a_unique_match`
- `create_makes_a_new_group_when_nothing_matches`
- `create_still_fails_on_an_ambiguous_name`
- `rename_parses_two_arguments`
- `rename_rejects_missing_or_empty_name`
- `rename_refuses_a_name_another_group_uses`
- `rename_to_the_current_name_is_a_noop_success`
- `rename_targets_an_unnamed_group_by_uuid`
- `rename_on_an_unknown_group_fails`

`src/control.rs`: `a_rename_request_without_a_gui_is_refused`.

Manual check: `kabelsalat run -g newproj --create -- claude` from another
terminal, confirm the group appears named and unfocused; `kabelsalat rename
newproj proj2`; `kabelsalat groups` shows the new name.
