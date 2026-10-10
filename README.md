# kabelsalat

[![CI](https://github.com/ThePixelPilgrim/kabelsalat/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/ThePixelPilgrim/kabelsalat/actions/workflows/ci.yml?query=branch%3Amain)

A crash-safe GTK4/libadwaita terminal emulator with tabs organised into
colour-coded groups.

- **Shells survive everything.** Tabs are backed by an invisible tmux
  server; crash, quit or upgrade the GUI and every shell reattaches, layout
  intact. With systemd lingering they survive logout too.
- **Claude Code sessions are resumed.** A tab in which `claude` runs
  records its session id; after a reboot the tab comes back as
  `claude --resume` in the same directory rather than as a plain shell.
  This applies to local and remote tabs.
- **Failure is survivable.** Crashed shells keep their output and restart in
  one click; orphaned sessions land in a "Recovered" group. Works without
  tmux, minus the survival guarantees.
- **Groups can live on another machine.** A remote group's tabs are tmux
  sessions on that host, reached over one shared ssh connection — they
  survive the GUI, the network dropping and the next login just like local
  ones.
- **Tabs live in groups.** Colour-coded, nameable, drag-and-drop; the
  sidebar collapses to a compact tab bar.
- **One browser per project.** `Alt+2` opens a per-group Chromium pane with
  its own profile and start page.
- **Your AI agent sees the web page you see.** Each group's embedded browser
  hands its CDP endpoint to the group's terminals automatically — an agent
  running there inspects and drives the page in front of you, zero
  configuration. One camera-button click hands it a screenshot of the pane
  instead, pasted straight into the tab you took it from.
- **A map of the work.** A group can show an overview in place of its
  terminals: a zoomable map of projects, decisions and themes read from
  Markdown files in the repository, with each tab drawn on the node it is
  working on and gaps in the data flagged for an agent to fix.
- **Agent-friendly CLI.** `kabelsalat run -g web -- npm run dev` opens a
  command in a visible tab without stealing focus; a Claude Code plugin
  teaches agents the whole interface.

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
- A tab in which Claude Code is running is recreated as
  `claude --resume <id>` when its tmux session did not survive; see
  "Claude Code sessions" below.
- **Resume at boot** (opt-in, from the primary menu): a systemd user unit
  recreates those Claude Code sessions right after boot, before login; see
  "Resume agent sessions at boot" below.
- **Logout survival**: the tmux server is detached from the login session
  (`systemd-run --user --scope`). If lingering is disabled for your user, a
  header-bar icon explains what `loginctl enable-linger` adds and its
  trade-offs, and can enable it for you; the hint can be dismissed
  permanently. The primary menu's *Keep shells running after logout…* entry
  shows whether lingering is on or off, opens the same offer while it is
  off, and turns it off again once it is on.
- **Without tmux** (or tmux < 3.2) everything still works — plain shells,
  no session survival — and a warning icon explains what installing tmux
  enables.

## Keyboard shortcuts

| Shortcut | Action |
| --- | --- |
| `Ctrl+Shift+T` | New tab in the active group |
| `Ctrl+Shift+C` | Copy the selection — or, with nothing selected, open a `claude` tab |
| `Ctrl+Shift+V` | Paste the clipboard |
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
    kabelsalat browser -g web              # bring up "web"'s browser, or print its CDP endpoint
    kabelsalat android -g web              # bring up Android in "web"'s pane, or print its ctl=/adb= lines
    kabelsalat overview root -g web docs   # draw "web"'s overview from the *.md files below docs/
    kabelsalat overview issues -g web      # print the overview's data issues, one per line
    kabelsalat resume                      # recreate saved claude sessions, no GUI (the boot unit)

`--group` takes a group name or uuid; `--cwd` overrides the working directory,
which defaults to the caller's. For a remote group the command runs on its
host: `--cwd` is a path there (default: the remote home) and the caller's
directory is ignored. `--create` always makes a local group. Everything
after `--` is the command. The new tab does not steal focus. `run --create`
reuses a unique existing match, or else creates a new group named exactly
the given selector. `rename` refuses
to create a duplicate name and is a no-op if the name is unchanged. `browser`
takes the group from the caller's `KABELSALAT_GROUP` when `-g` is omitted; it
prints the endpoint if the browser is already up, otherwise brings it up —
hidden unless that group is the active one, never taking focus — and prints
nothing, the endpoint then appearing in the group's sessions as
`KABELSALAT_CDP`. `android` does the same for the group's Android pane
(Waydroid) and, with a subcommand (`screenshot`, `tap`, `type`, `key`,
`resize`), drives it. `overview root` sets the directory a group's overview is
read from — resolved against the caller's directory, and it must exist —
or unsets it with `--clear`; `overview issues` prints the group's current data
issues, tab-separated (kind, node ids, file, detail), nothing when there are
none. Neither switches the group to its overview, changes the active group or
takes focus. Exit codes: 0 success, 1 not running, 2 usage (also an
`overview root` directory that does not exist), 3 no such group, ambiguous,
remote (for `browser`, `android` and `overview`), or (for `rename`) name
already in use, 4 (for `resume`) tmux unavailable or (for `android`) the
pane refused the request.

Claude Code learns this interface through the plugin below.

## Claude Code sessions

`Ctrl+Shift+C` opens a tab in the active group that runs `claude` instead of
a shell, in the active tab's directory. The same tracking applies to a
`claude` started by hand in any tab and to `kabelsalat run -- claude`.

Every 5 seconds, each local tab is matched against the `claude` processes on
this machine. Claude Code registers each interactive process in
`~/.claude/sessions/<pid>.json` (under `$CLAUDE_CONFIG_DIR` if set) with its
session id, working directory and process start time; kabelsalat reads that
registry, checks that the pid still belongs to the registered process, and
follows the process's parent chain up to a tab's pane shell. The session id
and directory of the match are saved with the tab in `state.json`. A new
session id after `/clear` is picked up the same way. A tab with a tracked
session is shown with a green tint in the tab list and the tab bar.

What the saved session is used for:

- When kabelsalat starts and a tab's tmux session is gone (after a reboot,
  or a killed server), the tab is recreated running
  `claude --resume <id>` in the saved directory instead of the shell.
- When a crashed tab is restarted, the same command is used.
- When `claude` exits, the tab forgets the session and is restored as a
  shell again. A crashed pane keeps it until it is restarted or closed.

Remote groups are handled on their host. Every 15 seconds, each connected
host runs a short POSIX `sh` script over the existing ssh connection: it
lists the host's kabelsalat panes, its process tree (`ps -eo pid=,ppid=`),
and the registry files under that host's `~/.claude`. The matching is done
locally on that output. The saved directory is a path on the host, and the
resume runs there. The script needs `sh`, `ps`, `awk`, `tr` and `printf` on
the host.

Limits:

- The registry is Claude Code's own, undocumented format (observed with
  Claude Code 2.1.x). A change to it would stop the tracking, not the app.
- Without tmux there is no tracking; a tab is then a plain shell.
- A session whose transcript no longer exists cannot be resumed; the tab
  then shows claude's exit code and offers a restart like any crashed tab.

## Resume agent sessions at boot

Optional, off by default. The primary menu (top right) → "Resume agent
sessions at boot…" installs a systemd user unit,
`~/.config/systemd/user/kabelsalat-resume.service`, that runs
`kabelsalat resume` once the user manager is up. With lingering enabled that
is right after boot, before anyone logs in; without it, at first login — the
dialog enables lingering along with the unit, and the menu entry's status
line says which of the two you have. The same dialog removes the unit again,
leaving lingering as it is. Nothing in your shell setup is touched either way.

`kabelsalat resume` starts the private tmux server the way the GUI does, then
recreates every saved tab that had a Claude Code session and whose tmux
session is gone, as `claude --resume <id>` in its directory — behind a login
shell, so `PATH` and the rest of your profile apply. Plain shells are not
resumed. When the GUI starts it finds those sessions live and attaches to
them; a `claude` that could not start shows as a crashed tab like any other.
The GUI also hands the server its own environment on every launch, so shells
opened later see the desktop's variables rather than the boot environment.
Running `kabelsalat resume` while the GUI is up does nothing.

This needs an unencrypted home directory: an encrypted one is not mounted
before login, and the unit then does nothing (its `ConditionPathExists` on
`state.json` fails cleanly). The unit is rewritten on launch when the binary
moves or the template changes. `systemctl --user status
kabelsalat-resume.service` shows the last run, `journalctl --user -u
kabelsalat-resume.service` its output.

## Claude Code plugin

The agent skill ships as a Claude Code plugin, and this repository is its own
plugin marketplace. Inside Claude Code:

    /plugin marketplace add ThePixelPilgrim/kabelsalat
    /plugin install kabelsalat@kabelsalat

The skill teaches an agent to launch commands into groups via `kabelsalat run`,
to drive the group's embedded browser over CDP (see "Browser automation"
below, including its security note) and Firefox in its Android pane, and to
keep the group's overview current — `kabelsalat overview` plus the node file
format in `skills/kabelsalat/overview-format.md`, which an agent in any
repository can adopt from that page alone. Plugin versions follow tagged
releases — `/plugin update` picks up a release, not every commit.

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
  network going away). kabelsalat retries in the background and reconnects
  on its own once the host is back, as long as logging in needs no password
  or passphrase — it never pops up a prompt unasked. Tabs closed while
  disconnected are killed on the host after the next successful connect.
- **Requirements:** OpenSSH 8.4 or newer here, tmux 3.2 or newer on the host.
- **Local-only features:** the browser pane runs on this computer, and its
  CDP variables (`KABELSALAT_CDP`, `PLAYWRIGHT_MCP_CDP_ENDPOINT`) and
  `KABELSALAT_GROUP` are not exported into remote tabs. Claude Code
  sessions in remote tabs are tracked and resumed on the host (see "Claude
  Code sessions").
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

### Overview mode

The "Terminals | Overview" toggle in the header bar swaps the active group's
terminal area for a map of its work; the sidebar and the browser or Android
pane stay. The mode is remembered per group, so switching groups brings back
whichever view each one was in, and activating a tab — in the sidebar, or by
clicking it on the map — switches back to Terminals and shows it. The map is
read from Markdown files with YAML frontmatter below one directory of the
project, set with `kabelsalat overview root`; the format is documented for
agents in [skills/kabelsalat/overview-format.md](skills/kabelsalat/overview-format.md).
A group without a root shows an empty state naming that command and linking
the format page; a root that is missing or unreadable shows the path and the
error, and nothing else in the group is affected. Remote groups have no
overview in this version — their toggle is insensitive, with a tooltip saying
so.

Which tab works on which node is inferred, not written by anyone: when a
tab's recent work changes, kabelsalat hands the new part of its `claude`
transcript (or just the tab title, for other tabs) to a small model and keeps
the answer in the tab's tmux session as `KABELSALAT_LINKS` (a JSON object with
the linked nodes, their roles and an activity line) and `KABELSALAT_TAG_MARK`
(how far the transcript was read). Nothing is written to the repository or
to `state.json`, and the tags survive a GUI restart with the tmux server. The
model command comes from `$XDG_CONFIG_HOME/kabelsalat/overview.json`, key
`tagger_command` (an argv array; default
`["claude", "-p", "--model", "haiku", "--output-format", "json"]`), and key
`roles` overrides the default role list `planning`, `implementing`,
`researching`, `related`; a missing file means defaults. Data issues — an
unparsable file, a duplicate id, a reference to a node that does not exist, a
parent cycle, or a tab working on two nodes with no edge between them — are
listed in a tray on the map, each with a **Resolve** button that opens a
`claude` tab in the group with the issue and the files involved, under the
same no-focus rules as `kabelsalat run`.

Without tmux (or tmux < 3.2) the overview works, but the tags live in memory
and are lost on restart. Without a working tagger command the overview works
without tab links, and the empty tab panels say that tagging is unavailable.

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

An agent whose group has no browser yet asks for one with `kabelsalat
browser`. The browser is transient — nothing restarts a crashed one — so the
same command brings it back.

The tmux server keeps running after the last tab closes (this is what makes
the crash and logout guarantees work). To stop it entirely:
`tmux -S "$XDG_RUNTIME_DIR/kabelsalat/tmux.sock" kill-server`.

The design is documented in
[docs/superpowers/specs/2026-07-22-tmux-persistence-design.md](docs/superpowers/specs/2026-07-22-tmux-persistence-design.md).

## License

MIT — see [LICENSE](LICENSE).

[libadwaita]: https://gnome.pages.gitlab.gnome.org/libadwaita/
