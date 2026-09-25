# Remote tmux groups over ssh

Date: 2026-09-25
Status: approved design (pending written-spec review)

## Goal

Let a kabelsalat group live on a remote host: its tabs are tmux sessions
on a kabelsalat-owned tmux server on that host, reached over ssh, with the
same crash- and restart-survival as local tabs.

kabelsalat never shows a password prompt inside a terminal. Login is
key-based, or goes through a graphical askpass program if one is
installed; otherwise the connection is refused.

## Decisions

- **Managed remote tabs.** kabelsalat runs its own tmux server on the
  host (`tmux -L kabelsalat`) with the usual `ks-<uuid>` sessions.
  Attaching to the user's pre-existing remote tmux sessions is out of
  scope.
- **Host is a group property.** All tabs of a group run on the group's
  host. Tabs cannot move between hosts.
- **Unreachable host → Disconnected, manual Reconnect.** No automatic
  retries, so askpass never pops up unasked.
- **Remote tmux missing or < 3.2 → refuse.** No plain-ssh-shell fallback.
- **ssh must be OpenSSH ≥ 8.4** (needed for `SSH_ASKPASS_REQUIRE`).
  Otherwise the feature is disabled.
- **One shared `tmux -L kabelsalat` server per remote user**, used by
  every kabelsalat installation and version. No automatic adoption of
  sessions on remote hosts (see §1), so installations never see each
  other's sessions.
- **The local-only features stay local.** The browser pane runs locally.
  `KABELSALAT_CDP`, `PLAYWRIGHT_MCP_CDP_ENDPOINT` and `KABELSALAT_GROUP`
  are not exported into remote sessions.

## 1. Data model and restore (`src/state.rs`, pure)

- `SavedGroup` gains `host: Option<String>` with `#[serde(default)]`.
  `None` means local. Old state files load unchanged, and no version
  field is introduced.
- The host string is an ssh destination passed through verbatim
  (`user@host`, a `~/.ssh/config` alias, `ssh://user@host:port`).
  `validate_host` only rejects:
  - an empty string
  - whitespace or control characters
  - a leading `-`, which would be parsed as an option
- A tab's host is always its group's. `SavedTab` gets no host field.
- `SavedGroup` also gains `pending_kills: Vec<Uuid>` with
  `#[serde(default)]`. It lists sessions of tabs that were closed while
  their host was Disconnected (§3). Remote hosts don't auto-adopt, so
  without this list those sessions would leak.
- A group's host may only be changed while the group has no tabs and no
  pending kills.

**Restore is planned per host.** Tabs are split into buckets by their
group's host.

- **Local bucket:** unchanged. The existing synchronous `reconcile`
  runs at startup, including auto-adoption into "Recovered".
- **Remote bucket:** starts as `Disconnected`, with no attach, respawn
  or adopt. When that host's worker delivers a successful
  `list-sessions`, a remote plan runs for the bucket:
  - live `ks-<uuid>` matching a saved tab → attach
  - saved tab with no live session → respawn
  - live `ks-*` sessions no saved tab claims → **ignored**, no
    adoption. They may belong to another installation. A later
    explicit "Adopt sessions from host…" action is a possible
    follow-up.
- **Invariant:** a remote tab is respawned only after a *successful*
  `list-sessions` from its host. An unreachable host, a failed login or
  a refused host never produces a fresh shell.

**Moves.** A pure `drop_allowed(src_host, dest_host)` gates:
- moving a tab to another group (`move_active_tab`)
- dropping a tab on a tab row or group header
- choosing a target in the group picker

A move across hosts is refused. Reordering groups is always allowed.

## 2. Connection and login (`src/remote.rs` pure logic + per-host worker)

### ssh client check

- At startup, run `ssh -V` and pass its stderr to a pure
  `parse_ssh_version(&str) -> Option<(u32, u32)>`, which only recognises
  the `OpenSSH_` prefix.
- **OpenSSH ≥ 8.4:** the feature is enabled.
- **Anything else** (older OpenSSH, a non-OpenSSH client, output that
  doesn't parse, `ssh` missing): the feature is disabled.
  - "New remote group" is insensitive, with the reason in its tooltip.
  - Existing remote groups load as Disconnected with that reason. They
    are never dropped.
- The result uses the same `Available` / `TooOld` / `Missing` shape as
  the tmux `detect()`.

### The shared connection (ControlMaster)

- One control master per host, at
  `$XDG_RUNTIME_DIR/kabelsalat/ssh/%C`. `%C` keeps the path under the
  Unix socket length limit.
- Only the host's worker authenticates. It uses:
  - `ControlMaster=auto`
  - `ControlPersist=yes`
  - `ConnectTimeout=10`
  - `ServerAliveInterval=15`
  - `ServerAliveCountMax=3`
- **Tabs never authenticate.** Their ssh runs with
  `-S <path> -o ControlMaster=no`, so without a live master it exits 255
  immediately instead of prompting.
- **On quit**, kabelsalat runs `ssh -O exit` for each master. The remote
  sessions survive, and the next start logs in again.

### Login

A pure `auth_mode(env, is_executable) -> Askpass(path) | BatchOnly`
chooses the mode. It looks for an askpass program in this order:

1. `$SSH_ASKPASS`, if it is executable
2. `/usr/libexec/openssh/gnome-ssh-askpass`,
   `/usr/libexec/openssh/ssh-askpass` (Fedora)
3. `/usr/lib/ssh/ssh-askpass` (Debian, Arch)
4. `ksshaskpass`, `lxqt-openssh-askpass`, `ssh-askpass` on `PATH`

The two modes:

- **`Askpass(path)`:** a single attempt with `SSH_ASKPASS=<path>` and
  `SSH_ASKPASS_REQUIRE=force`. Key-based login succeeds silently.
  Passwords, key passphrases and host-key confirmations go to askpass.
- **`BatchOnly`:** `-o BatchMode=yes`. Anything interactive fails, and
  the connection is refused.

In both modes, the worker's ssh:
- has stdin set to `/dev/null`
- runs in a new session (`setsid()` via `CommandExt::pre_exec`), so it
  has no controlling terminal and can never prompt on the terminal
  kabelsalat was started from, whatever the ssh version

### Host check (first commands over a fresh master)

1. `tmux -V`. Missing or older than 3.2 → refused (`TmuxMissing`,
   `TmuxTooOld`).
2. `tmux -L kabelsalat start-server \; source-file -`, with the remote
   variant of `TMUX_CONF` on stdin. Nothing is written to the remote
   disk. The remote config is `TMUX_CONF` without the local `pane-died`
   file hook.
   - **Verify during implementation:** that `source-file -` works on
     tmux 3.2. It is confirmed on 3.7c. If 3.2 lacks it, fall back to
     uploading the file to
     `${XDG_STATE_HOME:-$HOME/.local/state}/kabelsalat/tmux.conf`.
3. `list-sessions` in the existing format. The result goes to the main
   thread, which runs the remote restore plan (§1).

**Config compatibility rule.** Every kabelsalat version shares the one
remote server, and the last one to connect applies its config to the
whole server. So changes to `TMUX_CONF` must stay backward compatible
with older kabelsalat versions.

### Remote quoting

Remote tmux commands are built as argv by the existing builders. They
are then quoted once more, into one string for the remote login shell
(`shell_quote_argv`). The only assumption about the remote login shell
is that it parses POSIX single quotes, which bash, zsh and fish do.

### Error classification

A pure `classify(exit_code, stderr) -> RemoteError` returns one of:

| Error | Covers |
|---|---|
| `Unreachable` | DNS failure, connection refused, timeout |
| `AuthFailed` | wrong credentials |
| `AuthNeedsAskpass` | login needs a prompt, no askpass available |
| `HostKeyUnknown` | host key not in `known_hosts` |
| `HostKeyChanged` | host key differs from `known_hosts` |
| `TmuxMissing` | no tmux on the host |
| `TmuxTooOld(version)` | tmux older than 3.2 |
| `SshUnsupported` | local ssh fails the version check |
| `Other(stderr)` | anything else |

Each error has a specific, actionable message. For example,
`HostKeyUnknown` without askpass says: run `ssh <host>` once in a
terminal to accept the key.

### Worker

- Each remote host gets one `std::thread` that owns its master and runs
  that host's remote tmux calls **serially**. Serial order preserves
  dependencies such as `new-session` before `set-environment`.
- Results come back as `Msg`s, the same pattern the linger and profile
  sweep code already use.
- `TmuxCtl` gains a target, `Local { socket }` or
  `Remote { dest, control_path }`. The remote form puts
  `ssh -S … <dest> --` in front of the same commands and shares the
  argv builders and parsers.
- Local calls stay synchronous and unchanged. `tmuxctl.rs` keeps its
  no-panic rule, and `setsid` errors surface as `io::Error`.

## 3. Tab lifecycle

**States.** Remote hosts have a state: `Connecting`, `Live` or
`Disconnected(RemoteError)`. It lives on the host, so all of a host's
tabs change state together. Tabs keep the existing `Crashed` state.

**Spawn.** VTE runs:

```
ssh -S <path> -o ControlMaster=no -t <dest> -- \
  tmux -L kabelsalat new-session -A [-e K=V]… [-c <cwd>] -s ks-<uuid> <cmd>
```

The part after `--` is quoted for the remote shell. Tabs are spawned
only while the host is `Live`. While it is `Connecting`, the tab is
registered but its terminal waits.

**Child exit (`ChildExited`) for a remote tab:**
- **Exit status 255** (ssh failed): the worker runs `ssh -O check`. If
  the master is dead, all of the host's tabs become Disconnected. There
  is never an automatic reattach loop.
- **Any other status:** the worker runs `list-sessions`
  **asynchronously**, never the synchronous call at `app.rs:940`. The
  reply is fed into the existing `session_definitively_gone` decision,
  which closes the tab or reattaches it.

**Crash detection.** The `pane-died` file hook is local-only. For
remote tabs, crashed panes are detected from `pane_dead` and
`pane_dead_status` in that `list-sessions` reply. `remain-on-exit
failed` keeps the client attached to a dead pane, so no crash goes
unnoticed.

**Disconnected page.** An `adw::StatusPage` replaces the terminal. It
shows:
- the host
- the classified error message
- a **Reconnect** button, which reruns §2 for the host and then
  reattaches all of its tabs

**Close.**
- **Live host:** the tab is removed immediately, and `kill-session`
  goes to the worker without waiting (fire-and-forget).
- **Disconnected host:** the tab is removed from state, and its uuid
  goes into a per-host pending-kill queue. The queue is persisted in the
  group, so it survives restarts, and it is flushed after the next
  successful connect.

**Other per-tab calls** (`respawn-pane`, `set-environment`,
`pane_current_path`) go through the worker.

**New tab in the active remote tab's directory.** The directory lookup
waits at most about 300 ms, then falls back to the remote home
directory (no `-c`). The local `is_dir()` check is never applied to
remote paths.

**`kabelsalat run -g <remote group>`:**
- runs the command on the remote host
- uses `--cwd` if given, otherwise the remote home directory; the
  caller's local cwd is ignored, and there is no local `is_dir()` check
- quotes the command for the remote shell
- keeps the no-focus-steal rule
- `--create` still creates local groups only

## 4. UI

- **Creating a remote group:** a "New remote group…" button in the
  sidebar header, plus the shortcut Ctrl+Shift+H. It opens the Group
  Settings dialog in create mode:
  - **Host** (`adw::EntryRow`, live `validate_host`), then Name and
    Browser URL
  - an empty name defaults to the host
  - the group appears immediately as `Connecting`. On success its first
    tab opens. On failure it stays, with a Disconnected tab showing the
    reason.
- **Group Settings on an existing group:**
  - **Host row:** shown for every group. It is editable only while the
    group has no tabs; otherwise it is read-only with the subtitle
    "Close all tabs to change the host".
  - **Browser URL row:** in remote groups it notes that the browser runs
    locally and isn't exposed to remote tabs.
- **Sidebar group header:** remote groups show a `network-server-symbolic`
  icon and the host. The host state shows as normal (`Live`), a spinner
  (`Connecting`) or a warning colour with the error as tooltip
  (`Disconnected`). Rows of a Disconnected host are dimmed but can
  still be selected.
- **Drag-and-drop feedback:**
  - The sidebar `DropTarget`s use `set_preload(true)`.
  - `connect_enter` and `connect_motion` return `DragAction::empty()`
    when `drop_allowed` is false, which makes the compositor show its
    no-drop cursor.
  - A `drop-refused` CSS class tints the row while the pointer is over
    it.
  - Payloads become `tab:<id>:<host-key>`. The host key is `local` or
    an index into a per-render host table, so `:` in a destination
    can't break parsing.
  - The drop-time check in `dispatch_sidebar_drop` stays as the
    authority, with a toast "Can't move a tab between hosts".
- **Group picker (Move mode):** groups on other hosts are shown but
  can't be selected.

## 5. Testing

**Unit tests** (pure modules):
- Per-host restore:
  - an unreachable host never respawns
  - no adoption on remote hosts
  - a local-only state behaves exactly as before
- `drop_allowed`, `validate_host`
- `auth_mode`: each candidate and the priority order, using a fake env
  and a fake executable check
- `parse_ssh_version`: OpenSSH 7.4, 8.3, 8.4, 9.9 and 10.x, a
  non-OpenSSH string, and garbage
- `classify`: captured real ssh and tmux error texts
- Remote argv quoting: arguments containing `'`, `$`, `#`, spaces and
  newlines are round-tripped through a local `sh -c`, and the argv that
  comes out must equal the argv that went in
- CLI: a remote group target, `--cwd` passed through without a local
  check, `--create` still local
- State: a file without `host` loads, and the host and pending-kill
  queue survive a save and reload

**Manual verification** (against `sshd` on localhost or a container):
1. Key login: create the group, open tabs, restart kabelsalat; the
   sessions reattach.
2. Password login via gnome-ssh-askpass: exactly one prompt per host.
3. No askpass available: refused, with the message.
4. Stop `sshd` or drop the network: every tab of the host goes
   Disconnected within about 45 s with no loop, then Reconnect works.
5. Remote tmux < 3.2 or missing: refused.
6. Drag a tab onto a group on another host: no-drop cursor and tint.
7. kabelsalat started from a terminal: no prompt ever appears in that
   terminal.
8. Two local installations (separate `XDG_STATE_HOME`) on the same host
   don't see or disturb each other's tabs.

Finally, `cargo fmt`, `cargo clippy` and `cargo test` must pass.

## Out of scope (v1)

- Attaching to or adopting pre-existing remote sessions (possible later
  as an explicit "Adopt sessions from host…" action, which also enables
  continuing work across machines)
- tmux control mode (`-C`)
- automatic reconnect
- creating remote groups from the CLI
- logout survival on the remote host, which depends on that host's
  linger or `KillUserProcesses` setting (to be documented in the README)

## Follow-up: remote display pane (separate spec)

Graphical programs started in remote tabs, and the screenshot paste into
a remote `claude`.

**Rejected: waypipe or `ssh -X`.** The forwarded display socket lives
only as long as the ssh connection, while the tmux shells outlive it.
After a reconnect the sessions would hold a stale `WAYLAND_DISPLAY` or
`DISPLAY`.

**Chosen direction: a headless Wayland compositor on the host.** It is
long-lived like the tmux server and exports RDP. Its `WAYLAND_DISPLAY`
inside tmux is stable, and a dropped network only drops the viewer.

**Viewer:**
- a local RDP viewer reached through a runtime forward on the shared
  master (`ssh -O forward -L`)
- a per-group pane beside the browser pane
- an external viewer first; embedded later (there is no mature GTK4
  RDP/VNC widget for Rust)

**Sink binary:** a static musl binary per architecture (`uname -m`) at
`~/.local/share/kabelsalat/bin/kabelsalat-sink-<version>`.
- **Upload:** a kabelsalat uploads only its own version, only if it is
  missing. It writes to a temporary name, verifies the checksum, runs
  `chmod 700`, then renames. It never overwrites or deletes another
  version.
- **Selection:** the newest *compatible* binary is used. Every sink
  reports its supported launch-interface range via `--launch-abi`. An
  older kabelsalat uses a newer sink only if its own launch-interface
  version is in that range; otherwise it uses its own.
- **Pinning:** a per-group "use this kabelsalat's sink" setting forces
  the kabelsalat's own sink version.
- **Running sinks** are never restarted automatically. At most, the UI
  shows a hint that a newer sink is available.
- **Session name:** the sink runs in the kabelsalat tmux server under a
  name outside the `ks-` prefix (e.g. `ksd-…`).

**Also in that spec:**
- **Browser forwarding:** `ssh -O forward -R`, with the CDP variables
  re-published into the sessions on every reconnect.
- **Shared or per-installation sink:** whether one sink per host is
  shared by all installations (several RDP viewers) or each
  installation runs its own.
- **Image clipboard over RDP:** whether it can carry the screenshot
  paste.

## Amendments (2026-09-25)

Recorded by the plan's pre-verification review; the plan
(`docs/superpowers/plans/2026-09-25-remote-tmux.md`) is authoritative where
the two differ.

- **§2 "Tabs never authenticate".** `-o ControlMaster=no` alone does not
  exit 255 without a master: ssh falls back to a direct connection
  (ssh_config(5)). Tabs and the worker's tmux calls therefore run with
  `-S <path> -o ControlMaster=no -o BatchMode=yes -o ConnectTimeout=10
  -o ProxyCommand=false`. `BatchMode` stops any prompt; `ProxyCommand=false`
  makes the fallback fail at once with 255 and no network, while a live
  master is still reused (verified against a throwaway sshd).
- **§1 pending kills** are kept per host at the top level of the state file
  (`pending_kills: [{host, uuid}]`), not in the group: an empty group is
  pruned, and its queue must outlive it. Closing a remote tab always enqueues
  the kill; only the host's confirmation dequeues it.
- **§2 remote tmux** is invoked as `tmux -L kabelsalat -f /dev/null …`, so a
  server started by kabelsalat never reads the host user's `~/.tmux.conf`;
  the remote config arrives through `source-file -` (tmux 3.1+, confirmed in
  CHANGES; the upload fallback stays). The bootstrap works because the
  config sets `exit-empty off` inside the same client connection — a bare
  `start-server` on tmux ≥ 3.2 leaves no server behind.
- **§2 `classify`** takes the auth mode: "Permission denied" is
  `AuthNeedsAskpass` without askpass and `AuthFailed` with it.
- **§3 remote tab without a command** passes no shell-command to
  `new-session`; the remote user's default shell starts. Remote tabs get no
  `-e` pairs (local-only variables).
- **§3 quick-exit guard.** A remote client that exits within 2 s of its
  spawn while its session lives is not reattached automatically; the tab
  shows Reconnect instead of looping.
- **§4 Host row** is read-only on every existing group (an empty group does
  not exist); "Move to → New group" creates the new group on the tab's host.
- **§1 downgrade.** No version field, so an older kabelsalat respawns remote
  tabs as local shells and drops `host` on save; the remote sessions keep
  running on the host. Documented in the README; the "Adopt sessions from
  host…" follow-up is the real fix. A fail-safe schema (separate
  `remote_tabs` list) is possible but is a decision for the user.
- **`SSH_ASKPASS_REQUIRE=force`** is documented for passphrase input only;
  host-key confirmation uses the same OpenSSH code path and reaches askpass
  in practice, which manual tests 2 and 3 verify.
