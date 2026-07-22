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
- `set -g remain-on-exit failed` (tmux ≥ 3.2) — clean shell exit
  destroys the session; non-zero exit keeps the pane dead with its
  final screen intact and the real exit code in `#{pane_dead_status}`
- `set-hook -g pane-died 'run-shell "..."'` — writes
  `<uuid> <exit-code>` to `<runtime_dir>/events/` (see Crash detection)
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
   - per live session, query `#{pane_dead}` / `#{pane_dead_status}`:
     a dead pane restores the tab in crashed state with its final
     output visible and the real exit code shown.
4. Empty state and no sessions → current fresh-start behavior
   (one group, one tab).

## 4. Lifecycle semantics

- **Explicit tab close** kills the backing session
  (`tmux kill-session -t ks-<uuid>`). Only explicit close kills.
- **App quit, crash, upgrade** merely detach; the tmux server keeps all
  shells running.
- `ChildExited` semantics change: the child VTE observes is the tmux
  *client*, not the shell, and its exit code does not reflect the
  shell's. Client exit while the session is still alive (e.g. external
  detach) offers reattach instead of marking the tab crashed. Session
  actually gone → clean shell exit → tab closes as today.

### Crash detection (non-zero shell exit)

- With `remain-on-exit failed`, a shell exiting non-zero leaves a dead
  pane: the session survives, the final screen stays visible, and the
  VTE client stays attached — so `ChildExited` never fires for it.
- The `pane-died` hook writes a file `<uuid> <exit-code>` into
  `<runtime_dir>/events/`; the GUI watches that directory with a
  `GFileMonitor` (no polling) and marks the tab crashed, displaying
  the real exit code from `#{pane_dead_status}`.
- Because the dead session survives GUI restarts, crashed tabs —
  including their final output — are restored by reconciliation.
- Closing a crashed tab kills its session. A tab "restart" action may
  use `tmux respawn-pane` to rerun the shell in place.

## 5. Fallback without tmux

- Detected once at startup: tmux binary present, runnable, and
  `tmux -V` reports version ≥ 3.2 (required for `remain-on-exit
  failed` and `prefix None`). An older tmux is treated the same as a
  missing one.
- Absent or too old → direct `$SHELL` spawn exactly as today. All UI
  features and layout persistence still work; only session survival is
  missing.
- The header bar shows a yellow warning icon
  (`dialog-warning-symbolic`). Clicking it opens a dialog explaining
  that installing **tmux ≥ 3.2** enables crash-safe, upgrade-safe
  sessions, with install hints for the major distro families:
  Debian/Ubuntu (`apt install tmux`), Fedora/Red Hat
  (`dnf install tmux`), and openSUSE/SLE (`zypper install tmux`). If an
  older tmux was found, the dialog states the detected version and
  the required minimum instead of a plain install hint.

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
- Manual exit-code test: run `exit 3` in a tab → tab stays open,
  marked crashed, shows exit code 3 and final output; run `exit 0` →
  tab closes. Repeat `exit 3` then restart GUI → crashed tab restored.

## 8. Logout survival (linger)

Sessions survive GUI crash/quit/upgrade but not logout by default:
systemd stops `user@<uid>.service` when the last login session ends,
killing the tmux server, and `/run/user/<uid>` (socket dir) is wiped.
Two additions make logout survival possible and informed:

- **Server detachment**: when `systemd-run` is available, the tmux
  server is started via `systemd-run --user --scope --collect` so it
  lives outside the GUI's session scope and session-scope cleanup
  (e.g. GNOME/Wayland logout) cannot kill it. Fallback: plain spawn
  as before.
- **Linger warning icon**: when tmux is usable but linger is disabled
  (`loginctl show-user <user> --property=Linger` → `Linger=no`), a
  second header-bar warning icon appears (distinct tooltip from the
  tmux icon). Its dialog explains, in this order and without
  dramatizing: (1) what enabling linger adds — shells survive
  logout/re-login, not just GUI crashes; (2) the honest downsides —
  a small permanent background footprint (user manager + enabled user
  services), "logged out" no longer meaning nothing of yours is
  running (runaway processes and agents keep going unattended, a
  consideration on shared machines), and stale state persisting where
  re-login used to be a clean reset. Reversible via
  `loginctl disable-linger` at any time.
- Dialog buttons: **Enable** (runs `loginctl enable-linger <user>`,
  re-checks, hides the icon on success), **Not now** (icon stays,
  re-shown next launch), **Don't show again** (icon hidden
  permanently via a `linger_warning_dismissed` flag in state.json;
  `#[serde(default)]` for backward compatibility with existing state
  files). Dismissal never enables linger.
- No linger check when tmux is unavailable (logout survival is moot)
  or when `loginctl` is missing (non-systemd system: no icon).

## Out of scope

- Scrollback beyond tmux's visible-screen replay and internal history.
- tmux panes/splits, exposing tmux features to the user.
- Live migration of tabs opened before this feature (pre-tmux tabs).
