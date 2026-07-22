# Crash-safe sessions and persistent layout via tmux

Date: 2026-07-22
Status: approved design

## Goal

Make kabelsalat (1) GUI-crash safe and (2) GUI-upgradeable: killing or
restarting the GUI process must not terminate running shells, and the
tab/group presentation must be restored on next launch.

## Approach

Two independent mechanisms:

1. **Session backing via tmux.** Shells run inside tmux sessions on a
   private kabelsalat-owned tmux server. The tmux server (a separate
   process) owns the PTYs, so the GUI can die freely.
2. **Layout persistence.** The presentation model (tabs, groups, colors,
   order, active tab) is serialized to a state file on every mutation.

tmux is an optional runtime dependency: without it, kabelsalat behaves
exactly as today (direct `$SHELL` spawn) — fully usable, just not
crash-safe — and shows a warning indicator (see Fallback).

## 1. Session backing

- Each tab gets a stable UUID at creation time, stored in the state file.
- `spawn_shell()` spawns:
  `tmux -S <runtime_dir>/tmux.sock -f <state_dir>/tmux.conf new-session -A -s ks-<uuid> $SHELL`
  where `runtime_dir` is `$XDG_RUNTIME_DIR/kabelsalat` and `state_dir`
  is `$XDG_STATE_HOME/kabelsalat` (fallback `~/.local/state/kabelsalat`).
- `-A` attaches if the session exists, creates otherwise — one code path
  for fresh tabs and reattach after restart.
- One tmux session per tab (not one session with many windows):
  per-tab attach/detach, failure isolation, trivial orphan detection.

### Invisible-plumbing tmux config

Embedded in the binary, written to `<state_dir>/tmux.conf` at startup:

- `set -g status off` — no status bar
- prefix disabled (`set -g prefix None`, unbind defaults) — no tmux
  keybindings visible to the user
- `set -g mouse off` — mouse events pass through to applications
- `set -g detach-on-destroy on`
- `set -s exit-empty on` — server exits when the last session ends;
  no lingering daemon
- generous `history-limit`
- `default-terminal` matching VTE's TERM

## 2. Layout persistence

New module `src/state.rs`:

- serde structs mirroring the model: groups (id, name, palette index),
  tabs (uuid, group id, title, order = vector position), active tab id,
  sidebar visibility.
- Written to `<state_dir>/state.json` atomically (write temp file in
  the same directory, then rename).
- Saved on every mutation: tab/group create, close, move, rename,
  regroup, active change, sidebar toggle. Mutations are human-paced;
  no debouncing.

## 3. Startup reconciliation

1. Load `state.json` (missing/corrupt file → empty state; a corrupt
   file is renamed aside, not deleted).
2. If tmux is available: `tmux -S <sock> list-sessions` on our socket.
3. Reconcile:
   - state entry with live `ks-<uuid>` session → recreate tab, attach.
   - state entry without live session → recreate tab, spawn fresh shell
     (shell exited while GUI was gone; not marked "crashed").
   - live `ks-*` session absent from state → adopt as a tab in a
     special **"Recovered"** group so no live shell is ever invisible.
4. Empty state and no sessions → current fresh-start behavior
   (one group, one tab).

## 4. Lifecycle semantics

- **Explicit tab close** kills the backing session
  (`tmux kill-session -t ks-<uuid>`). Only explicit close kills.
- **App quit, crash, upgrade** merely detach; the tmux server keeps all
  shells running.
- `ChildExited` semantics change: the child VTE observes is the tmux
  *client*, not the shell. Client exit while the session is still alive
  (e.g. external detach) offers reattach instead of marking the tab
  crashed. Session actually gone → existing crashed/close behavior.

## 5. Fallback without tmux

- Detected once at startup (tmux binary present and runnable).
- Absent → direct `$SHELL` spawn exactly as today. All UI features and
  layout persistence still work; only session survival is missing.
- The header bar shows a yellow warning icon
  (`dialog-warning-symbolic`). Clicking it opens a dialog explaining
  that installing tmux enables crash-safe, upgrade-safe sessions, with
  an install hint (e.g. `dnf install tmux`).

## 6. Error handling

- All tmux interaction goes through one small wrapper (availability
  check, session list, kill-session, spawn argv construction).
- Wrapper failures surface as a UI notice, never a panic; worst case
  degrades to the no-tmux fallback path.

## 7. Testing

- Unit tests for `state.rs`: serialization round-trip, atomic write,
  corrupt-file handling.
- Unit tests for reconciliation as a pure function:
  (state, live-session list) → (tabs to attach, tabs to respawn,
  orphans to adopt).
- Manual crash test: open several tabs with running processes,
  `kill -9` the GUI, relaunch, verify layout restored and processes
  still running.

## Out of scope

- Scrollback beyond tmux's visible-screen replay and internal history.
- tmux panes/splits, exposing tmux features to the user.
- Live migration of tabs opened before this feature (pre-tmux tabs).
