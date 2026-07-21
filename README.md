# kabelsalat

A GTK4/libadwaita terminal emulator with tabs that are organised into groups.

Tabs belong to a group; groups are colour-coded and can be named. Navigation
happens per tab or per group, and tabs can be moved between groups. The tab
pane on the left can be hidden, leaving a compact tab bar.

Written in Rust using [relm4](https://relm4.org/), [libadwaita] and
[VTE](https://gitlab.gnome.org/GNOME/vte).

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

On Fedora:

```sh
sudo dnf install gtk4-devel libadwaita-devel vte291-gtk4-devel
```

On Debian/Ubuntu:

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev libvte-2.91-gtk4-dev
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
shell exits with a non-zero status the tab is marked instead of closed, so the
output stays readable.

## License

MIT — see [LICENSE](LICENSE).

[libadwaita]: https://gnome.pages.gitlab.gnome.org/libadwaita/
