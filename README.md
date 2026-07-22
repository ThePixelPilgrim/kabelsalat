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
| `Ctrl+Shift+R` | Name the active group |
| `Ctrl+Page Down` / `Ctrl+Page Up` | Next / previous tab |
| `Alt+Page Down` / `Alt+Page Up` | Next / previous group |
| `Alt+1` | Toggle the tab pane |
| `F1` | Show the shortcut list |

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

The tmux server keeps running after the last tab closes (this is what makes
the crash and logout guarantees work). To stop it entirely:
`tmux -S "$XDG_RUNTIME_DIR/kabelsalat/tmux.sock" kill-server`.

The design is documented in
[docs/superpowers/specs/2026-07-22-tmux-persistence-design.md](docs/superpowers/specs/2026-07-22-tmux-persistence-design.md).

## License

MIT — see [LICENSE](LICENSE).

[libadwaita]: https://gnome.pages.gitlab.gnome.org/libadwaita/
