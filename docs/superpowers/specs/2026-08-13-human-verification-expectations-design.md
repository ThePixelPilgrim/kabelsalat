# Human verification expectations — design

Date: 2026-08-13
Status: approved design, pre-implementation

## Summary

Agent sessions running inside kabelsalat tabs can register "human verification
expectations" — requests for the user to manually verify something. Kabelsalat
tracks them globally (across all sessions, segmented by group), shows them in a
resizable side panel, lets the user write feedback per expectation, jump to the
registering tab, and paste all resolved feedback as one consolidated message
into the active terminal, where the user can amend it and submit it personally.

## Decisions made

- Registration via a new CLI subcommand (not sockets, not OSC sequences).
- Tab identity via the tmux session environment (`tmux set-environment`), not
  process env vars, so pre-existing sessions can be backfilled on re-attach.
- Lifecycle: pending → resolved (feedback written) → member of pasted batches.
  No deletion in this iteration.
- Paste target: the currently active tab, injected without a trailing newline.
- UI: persistent resizable side panel (not a popover), all groups visible under
  group headers, inline expansion where the editor fills remaining panel height.
- No CLI read/list access for agents in this iteration (YAGNI).
- Expectations have a title plus an optional body.

## Data model

New module of pure logic (no GTK, no tmux calls), unit-testable — either inside
`src/state.rs` or a new `src/expectations.rs` (implementer's choice; keep the
purity invariant either way).

```rust
struct Expectation {
    uuid: String,            // minted at registration
    title: String,
    body: Option<String>,    // agent's fuller request text
    group_uuid: String,      // owning group at registration time
    tab_uuid: Option<String>, // registering tab; None => no jump button
    created: SystemTime,     // shown as age in the UI
    feedback: String,        // user's draft/final feedback text
    resolved: bool,          // set when the user clicks Done
}

struct PastedBatch {
    uuid: String,
    pasted_at: SystemTime,   // shown as "Pasted on <datetime>"
    expectation_uuids: Vec<String>,
}
```

- An expectation may appear in multiple batches (re-paste). Nothing is
  auto-deleted; deletion/pruning is explicitly out of scope for this iteration.
- Amending feedback on a resolved expectation edits `feedback` in place and does
  not change `resolved`.

### Persistence

- Own file `expectations.json` next to `state.json` (same directory resolution:
  `$XDG_STATE_HOME/kabelsalat/`, fallback `~/.local/state`).
- Same robustness pattern as `state.json`: atomic write via tmp file +
  `sync_all` + rename; corrupt file renamed aside to `.corrupt` and replaced
  with an empty store (never blocks startup).
- Feedback drafts are saved as-you-type (debounced) so a crash loses nothing.

## Registration path

### CLI (`src/cli.rs` — pure argv logic, unit tests here)

```
kabelsalat expect <title> [--body <text>]
```

- New `Cli` variant + parser branch + `dispatch()` handling.
- Tab identity: when `$TMUX` is set, the CLI queries
  `tmux show-environment KABELSALAT_TAB` against its own session (a live query
  against the tmux server, so it works for processes started before the
  variable existed). The tmux invocation itself lives outside `cli.rs` (no I/O
  there); `cli.rs` receives the resolved identity as input.
- If no tab identity can be determined (no tmux, foreign session, variable
  absent): the CLI errors with a clear message. Rationale: group membership is
  derived from the tab, and a groupless expectation has nowhere to be filed.
- On success the CLI prints the new expectation's uuid.

### tmux side (`src/tmuxctl.rs` — never panics, `Result` everywhere)

- On session creation **and** on every re-attach/adopt, the app runs
  `tmux set-environment -t ks-<uuid> KABELSALAT_TAB <uuid>` (idempotent;
  backfills sessions created before this feature existed).

### Control plumbing (`src/control.rs`, `src/app.rs`)

- `control::request_expect(...)` following the existing `request_spawn`
  pattern → new `Msg::RegisterExpectation { title, body, tab_uuid }` handled in
  the relm4 component. Fire-and-forget, one-way, like spawn.
- The app resolves `tab_uuid` → group; unknown tab uuid → registration is
  dropped with a toast (the CLI has already exited by then; acceptable for this
  trust model).
- Invariant honored: registration never steals focus — no activate, no window
  raise, no active-group change. Only the badge/panel content updates.

## Panel UI (`src/app.rs`)

- **Placement**: a resizable side panel inside a `gtk::Paned`, following the
  existing browser-pane pattern. Toggled by a new header-bar button carrying a
  **pending-count badge**. Split ratio and visibility are persisted (alongside
  the existing layout state in `state.json`, like `browser_split`).
- **Structure**, top to bottom:
  - Entries grouped under **group headers** (group name + palette color dot);
    all groups are always visible, since paste may target a terminal outside
    the expectation's group.
  - **Pending entry (expanded)** — at most one entry expanded at a time:
    title, meta line (agent/tab title, age), the agent's body text, then a
    feedback `TextView` that fills all remaining vertical panel space, and
    `↗ tab` / `✔ Done` buttons.
  - **Pending entry (collapsed)**: `● title` + jump button.
  - **Resolved entry (collapsed)**: `✔ title` + jump button; clicking expands
    it again for amending.
  - **Pasted batches** at the bottom: collapsed rows
    `▸ Pasted on <datetime> (<count>)`, expandable to list members, each batch
    with a re-paste button.
- **Jump button**: resolves `tab_uuid` to the live tab and calls the existing
  `activate()` path (stack switch, focus, sidebar sync — same as
  `JumpToGroup`). Hidden or insensitive when the tab no longer exists.
- **Toasts** (`show_toast`) for: new expectation arrived, paste completed,
  registration errors.

## Paste

- Button label: `Paste N resolved` where N counts resolved expectations not yet
  in any batch.
- Action: build one consolidated message from those expectations (all groups),
  inject into the **active tab's** terminal via `feed_child` using bracketed
  paste (so multi-line text doesn't execute line-by-line), **without** a
  trailing newline — the user amends and submits personally.
- On success, record a new `PastedBatch`. Re-pasting a batch re-injects that
  batch's members in the same format (with their current feedback text).
- Message format (pure formatting function, unit-tested):

  ```
  Human verification feedback (2 items):

  1. [backend / tab "api"] Verify login flow works
     Login works, but the TOTP page flashes white before the dashboard renders.

  2. [frontend] Check dark-mode contrast
     Fine except disabled buttons.
  ```

  The `[group / tab "…"]` tag drops the tab part when `tab_uuid` is `None` or
  the tab is gone.

## Degradation

- tmux missing or older than 3.2: the app keeps working as today; `expect`
  cannot self-identify and errors with a helpful message.
- Registering tab closed later: entry remains, jump button disabled, paste tag
  falls back to group-only.
- Group deleted later: entries remain under the stored group name/uuid
  (rendered with a "gone" marker); they still paste.

## Skill collection

- The `kabelsalat` plugin skill gains a section documenting `kabelsalat
  expect`: when an agent should register a verification expectation (work is
  done but needs human eyes) and how to phrase title/body.

## Testing

- Unit tests (pure code): CLI parsing of `expect`, store transitions
  (pending → resolved → batched, amend keeps resolved), paste-message
  formatting incl. missing-tab fallback, persistence round-trip and
  corrupt-file recovery.
- GUI behavior verified manually (no GUI test harness exists).

## Out of scope (this iteration)

- Deleting/pruning expectations or batches.
- Agent-side read access (`expect --list`).
- Any notification beyond badge + toast.
- Per-expectation paste (paste is batch-only, plus batch re-paste).
