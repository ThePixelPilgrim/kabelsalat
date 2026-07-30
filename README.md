# kabelsalat

A crash-safe GTK4/libadwaita terminal emulator with tabs organised into
colour-coded groups.

Tabs belong to a group; groups are colour-coded and can be named. Navigation
happens per tab or per group, and tabs can be moved between groups. The tab
pane on the left can be hidden, leaving a compact tab bar.

Shells are backed by tmux sessions on a private, invisible tmux server, so
they survive the GUI crashing, quitting, or being upgraded — relaunch and
every tab reattaches to its still-running shell, with grouping, colours,
order, and titles restored. With systemd lingering enabled they even survive
logging out and back in.

Written in Rust using [relm4](https://relm4.org/), [libadwaita] and
[VTE](https://gitlab.gnome.org/GNOME/vte).

## Crash-safe sessions

- The layout (groups, tabs, order, active tab) is persisted on every change
  to `$XDG_STATE_HOME/kabelsalat/state.json`.
- Each tab's shell runs in a tmux session on a kabelsalat-owned server
  (private socket, no status bar, no prefix key — tmux is invisible
  plumbing, not a UI). Killing the app never kills the shells; only closing
  a tab does.
- A shell exiting non-zero keeps its final output visible, marks the tab
  with the exit code, and offers a one-click restart — this survives a GUI
  restart too.
- Sessions found without a matching saved tab are adopted into a
  "Recovered" group rather than lost.
- **Logout survival**: the tmux server is detached from the login session
  (`systemd-run --user --scope`). If lingering is disabled for your user, a
  header-bar icon explains what `loginctl enable-linger` adds and its
  trade-offs, and can enable it for you; the hint can be dismissed
  permanently.
- **Without tmux** (or tmux < 3.2) everything still works — plain shells,
  no session survival — and a warning icon explains what installing tmux
  enables.

## Keyboard shortcuts

| Shortcut | Action |
| --- | --- |
| `Ctrl+Shift+T` | New tab in the active group |
| `Ctrl+Shift+N` | New group |
| `Ctrl+Shift+W` | Close the active tab |
| `Ctrl+Shift+M` | Move the tab to another group |
| `Ctrl+Shift+G` | Jump to a group |
| `Ctrl+Shift+R` | Group settings (name, browser default URL) |
| `Ctrl+Page Down` / `Ctrl+Page Up` | Next / previous tab |
| `Alt+Page Down` / `Alt+Page Up` | Next / previous group |
| `Alt+1` | Toggle the tab pane |
| `Alt+2` | Toggle the browser pane |
| `F1` | Show the shortcut list |

## Command line

With kabelsalat running, a second invocation talks to it instead of opening a
second window:

    kabelsalat groups                      # uuid, name and tab count per group
    kabelsalat run -g web -- npm run dev   # new tab in the "web" group

`--group` takes a group name or uuid; `--cwd` overrides the working directory,
which defaults to the caller's. Everything after `--` is the command. The new
tab does not steal focus. Exit codes: 0 success, 1 not running, 2 usage,
3 no such group.

`scripts/install-skill.sh` links `skills/kabelsalat` into `~/.claude/skills/`
so Claude Code knows how to use this.

## Requirements

- Rust 1.85 or newer (edition 2024)
- GTK 4.18, libadwaita 1.5 and VTE 0.82 or newer, including development headers
- Optional: tmux 3.2 or newer for crash-safe sessions (fully usable without)

On Fedora:

```sh
sudo dnf install gtk4-devel libadwaita-devel vte291-gtk4-devel tmux
```

On Debian/Ubuntu:

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev tmux
```

## Installation

This is not published on crates.io. Install it straight from the repository:

```sh
cargo install --git https://github.com/ThePixelPilgrim/kabelsalat
```

The binary lands in `~/.cargo/bin/kabelsalat`.

Or build from a checkout:

```sh
git clone https://github.com/ThePixelPilgrim/kabelsalat
cd kabelsalat
cargo build --release
./target/release/kabelsalat
```

## Behaviour notes

Each tab runs the shell from `$SHELL`, falling back to `/bin/bash`. When a
shell exits with a non-zero status the tab is marked with the exit code
instead of closed, so the output stays readable; a restart button reruns the
shell in place.

`Ctrl+Shift+R` opens the active group's settings: its name, and the URL a
freshly launched browser pane in that group starts on. Leave the URL empty for
whatever Chromium opens by itself. A bare host gets `http://` filled in, so
`localhost:3000` is enough; `https://…` and `file:///…` are taken as typed, and
anything else — another scheme, or something that would reach the browser as a
command-line flag — is refused with the reason under the entry, with Apply
disabled until it is fixed.

Editing that URL never touches a browser that is already running: it is a launch
argument, not navigation, and a relaunched pane restores its own session instead
of stacking another copy of the default tab on top of it. So a group that
already has a browser picks up a new default only after **Close browser** —
which deletes that pane's profile — and a fresh `Alt+2`.

### Browser automation (CDP)

Each group's browser exposes an **unauthenticated** Chrome DevTools Protocol
endpoint on loopback. CDP grants full control of that browser profile —
cookies, sessions, arbitrary navigation and script execution — and any process
running as any user on this machine can connect to it. This is a deliberate,
documented interim state; a token-authenticated broker is planned.

Terminals in the group receive the endpoint as `KABELSALAT_CDP` and
`PLAYWRIGHT_MCP_CDP_ENDPOINT` (both `http://127.0.0.1:<port>`), plus
`KABELSALAT_GROUP` (the group's uuid). Because a running shell's environment
is frozen at spawn, the live values are always available from tmux:

    tmux show-environment KABELSALAT_CDP

The tmux server keeps running after the last tab closes (this is what makes
the crash and logout guarantees work). To stop it entirely:
`tmux -S "$XDG_RUNTIME_DIR/kabelsalat/tmux.sock" kill-server`.

The design is documented in
[docs/superpowers/specs/2026-07-22-tmux-persistence-design.md](docs/superpowers/specs/2026-07-22-tmux-persistence-design.md).

## License

MIT — see [LICENSE](LICENSE).

[libadwaita]: https://gnome.pages.gitlab.gnome.org/libadwaita/
