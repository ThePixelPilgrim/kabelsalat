# New tabs inherit the active shell's CWD

Date: 2026-07-23

## Goal

When the user creates a new tab (Ctrl+Shift+T, the + button, or the first
tab of a new group via Ctrl+Shift+N), the new shell starts in the current
working directory of the shell that was active at that moment. Restored,
reattached, and respawned tabs are unchanged.

## Getting the CWD

New helper on the app model: `active_tab_cwd(&self) -> Option<PathBuf>`.

- tmux path: new `TmuxCtl::pane_current_path(uuid) -> Result<PathBuf, TmuxError>`
  in `src/tmuxctl.rs`, running
  `display-message -p -t kabelsalat-<uuid> '#{pane_current_path}'`
  via the existing command plumbing.
- No-tmux fallback: read the active tab's VTE `current-directory-uri`
  property (a `file://` URI). Strip scheme and hostname, percent-decode
  the path.
- Any failure, missing active tab, or non-existent directory → `None`.

## Using it

- `TmuxCtl::spawn_argv` gains an `Option<&Path>` cwd parameter; when set,
  insert `-c <dir>` into the `new-session` portion of the argv. `-A`
  ignores `-c` when the session already exists, so restore paths simply
  pass `None`.
- `spawn_shell` (plain fallback in `src/app.rs`) takes an optional working
  directory forwarded to `Terminal::spawn_async`.
- All three creation paths capture the CWD from the active tab *before*
  the new tab becomes active, then pass it through `spawn_backing`.
- Restore/reattach/adopt/respawn paths pass `None` — behavior unchanged.

## Error handling

Never user-facing. Check `is_dir()` before passing `-c` (tmux errors on a
nonexistent directory); otherwise spawn without a cwd, landing in `$HOME`
as today.

## Testing

Unit tests in the existing style in `src/tmuxctl.rs`:
- `spawn_argv` with and without a cwd (argv shape).
- file-URI → path parsing (plain, percent-encoded, with hostname, bad
  scheme).
