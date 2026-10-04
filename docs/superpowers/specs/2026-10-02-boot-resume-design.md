# Resume agent sessions at boot

Date: 2026-10-02
Status: approved design

## Goal

A Claude Code session that was running in a tab comes back right after the
machine boots, before anyone logs in, so it is already there when the user
opens kabelsalat. The same path will later carry Codex sessions.

Everything stays "sidewise": nothing is inserted into the user's shell or
terminal path, no rc file is touched, and the feature ships switched off.
The user installs and removes it from the GUI.

## Decisions

- **One oneshot user unit, `kabelsalat-resume.service`,** `WantedBy=default.target`,
  running `kabelsalat resume`. With lingering enabled the user manager
  starts at boot, so the unit runs before login; without lingering it runs
  at first login, which is a harmless fallback the dialog names.
- **`kabelsalat resume` recreates only tabs that carry a claude session.**
  It starts the private tmux server the way the GUI does (`ensure_server`),
  runs `reconcile_local` against it, and creates a detached `ks-<uuid>`
  session for every `respawn` entry with a `claude`, in the saved
  directory, running the resume command. Plain shells are left to the GUI:
  a shell started at boot would lack the desktop environment for nothing.
- **`resume` never writes `state.json`.** The GUI stays the single writer.
  When it starts, `reconcile` finds the sessions live and attaches to them
  instead of respawning; nothing in the state model changes.
- **Boot-time commands run through a login shell**: `$SHELL -lc 'exec claude
  --resume <id>'`. The user manager's environment has no `~/.cargo/bin`, no
  npm global bin, nothing from `.profile`; a login shell brings that back
  without touching any rc file.
- **The GUI syncs the server's global environment on every launch.** A
  tmux server started at boot would otherwise hand its minimal environment
  (`PATH`, no `XDG_CURRENT_DESKTOP`, …) to every shell created later, until
  the next reboot. `ensure_server` is followed by one `set-environment -g`
  per exportable variable of the GUI's own environment, so sessions created
  afterwards see the desktop's environment. Variables that describe a
  process rather than a session (`TMUX`, `PWD`, `SHLVL`, …) are skipped.
- **The GUI running makes `resume` a no-op**, exit 0 with a note. The GUI
  owns the sessions then; this also covers an auto-login racing the unit.
  A session that already exists when `resume` creates it is skipped, not
  an error, so both orders of that race are safe.
- **Failures surface through the existing crash path.** A `claude` that is
  missing or a session id that is gone leaves a dead pane (`remain-on-exit
  failed`), and the GUI shows the tab as crashed on attach.
- **The browser is transient.** Nothing restarts a crashed browser. An
  agent in the group asks for it with `kabelsalat browser`; the endpoint it
  then reads live from the tmux session table, as the skill already says.

## Unit file

Written to `$XDG_CONFIG_HOME/systemd/user/kabelsalat-resume.service`
(fallback `~/.config`). Absolute paths are rendered at install time from the
GUI's own view, not from specifiers, because the user manager may not share
the GUI's `XDG_*` variables.

```ini
[Unit]
Description=Resume kabelsalat agent sessions after boot
ConditionPathExists=/home/me/.local/state/kabelsalat/state.json

[Service]
Type=oneshot
ExecStart="/home/me/.cargo/bin/kabelsalat" resume

[Install]
WantedBy=default.target
```

The condition makes an encrypted or unmounted home a clean skip, not a
failure.

## GUI: primary menu and dialog

The header bar gains a primary menu (`open-menu-symbolic`, rightmost). It is
a `gtk::MenuButton` with a popover of flat buttons, like the browser overflow
menu, since the app has no `gio::Menu` and three entries do not justify a
second idiom:

- **Resume agent sessions at boot…** with a dimmed status line: *Off*, *On*,
  *At login only* (installed, lingering off), *Needs repair*. Insensitive with
  a tooltip when tmux or `systemctl --user` is unavailable.
- **Keep shells running after logout…** has a status line of its own, *On*
  or *Off*, re-read from `loginctl show-user` each time the dialog opens.
  While lingering is off it opens the existing linger offer (Not now /
  Don't show again / Enable); once it is on, a dialog stating the fact with
  Close / Disable, which runs `loginctl disable-linger`. Either change is
  confirmed with a toast. The warning icon stays as the first-time hint;
  the menu is the permanent way back once the icon is dismissed.
- **Keyboard shortcuts**, moved here from the `help-about-symbolic` button,
  which is the About icon.

The dialog is an `adw::AlertDialog` whose responses follow the state:

| State | Responses |
|---|---|
| Off | Cancel, **Install** (suggested) |
| On | Close, Remove (destructive) |
| At login only | Close, Remove, **Enable lingering** (suggested) |
| Needs repair | Close, **Repair** (suggested) |

Install writes the unit, runs `daemon-reload` and `enable`, and when
lingering is off also runs the existing `enable_linger`: a unit without
lingering only gives "at login", which is rarely what someone opening this
dialog wants. Remove runs `disable`, deletes the file and reloads; it leaves
lingering alone, because the linger dialog enables that for tmux survival
independently. Neither touches running sessions.

The state is computed from the system on every launch and after every
change: whether the file exists and matches what this build would write,
`systemctl --user is-enabled`, and `loginctl show-user`. Nothing new is
persisted in `state.json`. A unit whose text differs from the current
template (a moved binary, a newer template) while enabled is *Needs repair*
and is rewritten silently at launch, as `tmux.conf` is; the dialog shows
Repair only if that rewrite failed.

Outcomes go through the toast overlay; failures through `show_notice` with
the `systemctl` error text.

## CLI

```
kabelsalat browser [-g <group>]
kabelsalat resume
```

`browser` asks for the group's browser pane. `-g` defaults to the caller's
`KABELSALAT_GROUP`, forwarded with the command line; without either it is a
usage error. Semantics against the published snapshot, which gains the
group's live CDP endpoint:

| condition | result |
|---|---|
| group has a live endpoint | print it, exit 0, no action |
| group has no browser | ask the GUI to open it, exit 0, empty stdout |
| remote group | exit 3: the pane is local |
| GUI not running | exit 1, as for every command |

The GUI handles the request like a restore, not like Alt+2: spawn, mark the
pane visible only if the group is the active one, never focus, never raise,
never switch groups. The CLAUDE.md invariant "no touching the group's
browser pane" is refined: it holds for `run`; `browser` touches only the
named group's pane under the same rules. The agent polls
`tmux show-environment KABELSALAT_CDP` until the endpoint appears, within
the GUI's own 10 s discovery window.

`resume` is the unit's command. It is handled in the calling process with no
GTK: exit 0 after creating the sessions (or when there is nothing to do, the
GUI running included), exit 4 when tmux is unavailable or the server could
not be started.

## Modules and tests

- `src/autostart.rs` (new, pure plus thin `systemctl` runners): unit text and
  path, status classification from the three facts, status labels, the
  response table. Tests per state, including *Needs repair*.
- `src/state.rs`: the boot plan (the `respawn` entries with a claude, local
  groups only) and the login-shell resume argv.
- `src/tmuxctl.rs`: detached `new-session` argv, the "duplicate session"
  stderr test, the exportable-variable filter and `set-environment -g` argv.
- `src/cli.rs`: `browser` and `resume` parsing, the `KABELSALAT_GROUP`
  default, `browser` dispatch, `GroupInfo::cdp`.
- `src/resume.rs` (new, std only): the `resume` glue over the pure pieces.
- `src/control.rs`, `src/lib.rs`: forward `browser`, run `resume`.
- `src/app.rs`: menu, dialog, `OpenBrowser`, snapshot republish on
  `CdpReady`, environment sync after `ensure_server`, launch repair. Wiring
  only.

## Out of scope

- Restarting a crashed browser.
- Resuming plain shells or arbitrary commands at boot.
- A `--wait` flag on `browser`: it needs the command line held open across
  an asynchronous reply, which `control.rs` does not do.
- Codex: a second variant of the per-tab agent record, same path.
