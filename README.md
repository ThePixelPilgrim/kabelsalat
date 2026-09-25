# kabelsalat

A crash-safe GTK4/libadwaita terminal emulator with tabs organised into
colour-coded groups.

- **Shells survive everything.** Tabs are backed by an invisible tmux
  server; crash, quit or upgrade the GUI and every shell reattaches, layout
  intact. With systemd lingering they survive logout too.
- **Your AI agent sees the web page you see.** Each group's embedded browser
  hands its CDP endpoint to the group's terminals automatically — an agent
  running there inspects and drives the page in front of you, zero
  configuration. One camera-button click hands it a screenshot of the pane
  instead, pasted straight into the tab you took it from.
- **One browser per project.** `Alt+2` opens a per-group Chromium pane with
  its own profile and start page.
- **Tabs live in groups.** Colour-coded, nameable, drag-and-drop; the
  sidebar collapses to a compact tab bar.
- **Groups can live on another machine.** A remote group's tabs are tmux
  sessions on that host, reached over one shared ssh connection — they
  survive the GUI, the network dropping and the next login just like local
  ones.
- **Agent-friendly CLI.** `kabelsalat run -g web -- npm run dev` opens a
  command in a visible tab without stealing focus; a Claude Code plugin
  teaches agents the whole interface.
- **Failure is survivable.** Crashed shells keep their output and restart in
  one click; orphaned sessions land in a "Recovered" group. Works without
  tmux, minus the survival guarantees.

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
| `Ctrl+Shift+H` | New remote group |
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
    kabelsalat run -g newproj --create -- claude   # create "newproj" if missing
    kabelsalat rename newproj proj2        # rename an existing group

`--group` takes a group name or uuid; `--cwd` overrides the working directory,
which defaults to the caller's. For a remote group the command runs on its
host: `--cwd` is a path there (default: the remote home) and the caller's
directory is ignored. `--create` always makes a local group. Everything
after `--` is the command. The new tab does not steal focus. `run --create`
reuses a unique existing match, or else creates a new group named exactly
the given selector. `rename` refuses
to create a duplicate name and is a no-op if the name is unchanged. Exit
codes: 0 success, 1 not running, 2 usage, 3 no such group, ambiguous, or (for
`rename`) name already in use.

Claude Code learns this interface through the plugin below.

## Claude Code plugin

The agent skill ships as a Claude Code plugin, and this repository is its own
plugin marketplace. Inside Claude Code:

    /plugin marketplace add ThePixelPilgrim/kabelsalat
    /plugin install kabelsalat@kabelsalat

The skill teaches an agent to launch commands into groups via `kabelsalat run`
and to drive the group's embedded browser over CDP (see "Browser automation"
below, including its security note). Plugin versions follow tagged releases —
`/plugin update` picks up a release, not every commit.

For hacking on the skill itself, `scripts/install-skill.sh` symlinks
`skills/kabelsalat` into `~/.claude/skills/` — a dev-mode shortcut, not the
supported install path.

## Remote groups

`Ctrl+Shift+H` (or the server button above the tab list) creates a group on
another machine. Enter any ssh destination — `user@host`, an alias from
`~/.ssh/config`, or `ssh://user@host:port`. The group's tabs are tmux
sessions on a kabelsalat-owned server on that host (`tmux -L kabelsalat`),
reached over one ssh connection per host, so they survive the GUI exactly like
local tabs. A group's host is fixed; tabs cannot be moved between hosts.

- **Login:** key-based, or through a graphical askpass program if one is
  installed (`$SSH_ASKPASS`, `gnome-ssh-askpass`/`ssh-askpass` from
  `openssh-askpass` or `ssh-askpass-gnome`, `ksshaskpass`,
  `lxqt-openssh-askpass`) — at most one prompt per host and start. kabelsalat
  never prompts in a terminal; without askpass, a host that wants a password
  or an unknown host key is refused with instructions (e.g. run `ssh <host>`
  once to accept its key).
- **Disconnects:** when the connection drops, every tab of that host shows a
  page with the reason and a **Reconnect** button (within about 45 s of the
  network going away). There are no automatic retries. Tabs closed while
  disconnected are killed on the host after the next successful connect.
- **Requirements:** OpenSSH 8.4 or newer here, tmux 3.2 or newer on the host.
- **Local-only features:** the browser pane runs on this computer, and its
  CDP variables (`KABELSALAT_CDP`, `PLAYWRIGHT_MCP_CDP_ENDPOINT`) and
  `KABELSALAT_GROUP` are not exported into remote tabs.
- **Sharing a host:** every kabelsalat installation and version shares the one
  `tmux -L kabelsalat` server per remote user, but only ever attaches its own
  sessions; unknown sessions there are ignored, never adopted.
- **Logout survival on the host** depends on that host: if its logind kills a
  user's processes when the last session ends (`KillUserProcesses=yes`), run
  `loginctl enable-linger` there, as for local sessions.
- **Downgrading** to a kabelsalat without remote groups turns them into local
  groups (their tabs are respawned as local shells) and leaves the remote
  sessions running on the host. Find them with
  `ssh <host> tmux -L kabelsalat ls`; attach with `tmux -L kabelsalat attach
  -t ks-<uuid>` or remove them with `tmux -L kabelsalat kill-server`.

The ssh control sockets live in `$XDG_RUNTIME_DIR/kabelsalat/ssh/`; quitting
kabelsalat closes the connections but leaves the remote sessions running.

## Requirements

- Rust 1.85 or newer (edition 2024)
- GTK 4.18, libadwaita 1.5 and VTE 0.82 or newer, including development headers
- Optional: tmux 3.2 or newer for crash-safe sessions (fully usable without)
- Optional: OpenSSH 8.4 or newer for remote groups (tmux 3.2 or newer on
  each remote host)

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

### Screenshot into the terminal

The camera button in the header bar captures the active group's browser pane
and puts the image on the clipboard, then presses `Ctrl+V` in the tab it was
started from — so a `claude` running there stages the screenshot and you type
your question next to it. It works while the pane is hidden, and is greyed out
when the group has no running browser.

Two consequences worth knowing: it **replaces the clipboard contents**, like
any other copy action, and the keystroke goes to whatever is running in that
tab — a shell rather than an agent sees a plain `Ctrl+V`, exactly as if you had
pressed it yourself. If you switch tabs while the capture is still in flight,
the paste is skipped and the screenshot only lands on the clipboard.

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
