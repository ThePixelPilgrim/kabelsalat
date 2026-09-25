# Remote tmux groups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A kabelsalat group can live on a remote host: its tabs are `ks-<uuid>` sessions on a kabelsalat-owned `tmux -L kabelsalat` server on that host, reached over one shared ssh ControlMaster per host, with the same crash and restart survival as local tabs. This implements the approved spec `docs/superpowers/specs/2026-09-25-remote-tmux-design.md`.

**Architecture:** The pure logic lives in `src/state.rs` and a new `src/remote.rs`. `state.rs` gets the host field, the pending-kill queue, `validate_host`, `drop_allowed` and the per-host restore plan. `remote.rs` gets the ssh version check, the askpass choice, error classification and the ssh argv builders with their quoting. `src/tmuxctl.rs` gains a `Target` (`Local` / `Remote`), so the same tmux argv builders and parsers serve both. A new `src/remote_worker.rs` runs one `std::thread` per host. That thread owns the master, runs the host's remote tmux calls serially, and reports back as `Msg::Remote`. The worker's process runner is behind a trait, so its sequences are unit-tested. `src/app.rs` connects each host at startup and keeps a per-host state (`Connecting` / `Live` / `Disconnected`). A remote tab shows an `adw::StatusPage` in place of its terminal until its host is live. The app also adds the create dialog, the sidebar and drag-and-drop feedback, and remote `kabelsalat run`.

**Tech Stack:** Rust (edition 2024), relm4 0.11 / GTK4 / libadwaita 1.5 / VTE 0.82, tmux ≥ 3.2 on both ends, OpenSSH ≥ 8.4 (`ControlMaster`, `SSH_ASKPASS_REQUIRE`), `libc` (already a dependency) for `setsid`.

## Global Constraints

- `src/state.rs`, `src/cli.rs` and the new `src/remote.rs` stay pure: no GTK, no gio, no tmux or ssh processes, no I/O beyond `state.rs`'s existing load/save.
- `src/tmuxctl.rs` never panics: every fallible path returns `Result<_, TmuxError>`; no `unwrap`/`expect`/unchecked indexing on tmux or ssh interaction. `src/remote_worker.rs` follows the same rule, and a `setsid` failure surfaces as `std::io::Error`.
- The local ssh client must be OpenSSH ≥ 8.4 (`remote::MIN_SSH = (8, 4)`); otherwise remote groups are disabled, never dropped.
- Remote tmux must be ≥ 3.2 (`tmuxctl::MIN_VERSION`); missing or older → the host is refused. There is no plain-ssh fallback.
- The app keeps working with tmux missing or < 3.2, and with ssh missing or too old. Local tabs are unchanged in every case, and a local tmux problem never blocks remote groups.
- A remote tab is spawned only after a **successful** `list-sessions` from its host. Unreachable, failed-login and refused hosts never get a fresh shell.
- kabelsalat never shows a password prompt in a terminal. Every non-master ssh runs with `-o BatchMode=yes`. The master runs with stdin `/dev/null` and under `setsid()`, using either `SSH_ASKPASS_REQUIRE=force` or `BatchMode=yes`.
- `KABELSALAT_CDP`, `PLAYWRIGHT_MCP_CDP_ENDPOINT` and `KABELSALAT_GROUP` are never exported into remote sessions.
- A CLI-created tab never steals focus: no activate, no window raise, no active-group change, and no touching the group's browser pane. This applies to remote groups too.
- Exact values, from the spec:
  - control path `$XDG_RUNTIME_DIR/kabelsalat/ssh/%C`
  - master options `ControlMaster=auto`, `ControlPersist=yes`, `ConnectTimeout=10`, `ServerAliveInterval=15`, `ServerAliveCountMax=3`
  - tab and worker ssh `-S <path> -o ControlMaster=no -o BatchMode=yes -o ProxyCommand=false` (decision 2)
  - remote server `tmux -L kabelsalat`
  - remote cwd lookup waits 300 ms
  - on quit, `ssh -O exit` for each master
- Never put the user's email address in a User-Agent or any other outgoing header or payload. This feature sends none, and must keep it that way.
- No new crates; `Cargo.toml` is not modified.
- Before claiming any task done, run `cargo fmt`, `cargo clippy --all-targets` (no new warnings) and `cargo test` (all pass).
- Commit messages are plain imperative sentences in the `git log` style (`Sort the sidebar by age bucket, not raw stamp`). No `feat:` prefixes and no attribution or `Co-Authored-By` trailers.
- Tasks that touch `src/app.rs` run strictly one after another (see each task's **Depends on:** line).
- Line numbers in the tasks refer to the files as they are on the branch **before any task** (commit `2cf1034`). Earlier tasks shift them (Task 1 adds ~73 lines to `state.rs` and 2 to `app.rs`, Tasks 7 and 8 a few more to `app.rs`), so locate code by the quoted text anchors and treat the numbers as a hint, not an address.

### Decisions this plan takes where the spec is silent or unworkable

These are deliberate. Reviewers should check them against the spec.

1. **Pending kills are top-level, keyed by host** (`SavedState::pending_kills: Vec<PendingKill { host, uuid }>`), not a field of `SavedGroup`.
   - A group is pruned the moment its last tab closes (`prune_empty_groups`). A per-group queue would vanish together with the group it belongs to.
   - The spec itself calls it a "per-host pending-kill queue".
2. **Every non-master ssh carries `-o BatchMode=yes -o ConnectTimeout=10 -o ProxyCommand=false`.**
   - With `-o ControlMaster=no`, ssh whose control socket is missing does **not** exit 255. It falls back to a direct connection (ssh_config(5), ControlMaster: "will fall back to connecting normally if the control socket does not exist, or is not listening"; verified with OpenSSH 10.2), which could prompt on the tab's pty.
   - `BatchMode` makes that fallback non-interactive. `ProxyCommand=false` makes it fail outright: the mux client is tried before any connection is made, so a live master is reused as usual, and without one ssh runs `false` as its transport and exits 255 at once ("Connection closed by UNKNOWN port 65535"), never opening a stray direct connection of its own. That restores the spec's "exits 255 immediately". Verified against a throwaway sshd: remote exit statuses still pass through, `-t` still gets a pty.
3. **Remote tmux runs as `tmux -L kabelsalat -f /dev/null …`.** Without `-f`, a server started by that command would read the user's `~/.tmux.conf` before our config is sourced.
4. **A remote tab with no command passes no shell-command to `new-session`.** The local `$SHELL` means nothing on the host, so tmux starts the remote user's default shell.
5. **`classify` takes the `AuthMode` as a third argument.**
   - ssh's "Permission denied" looks the same whether a password was wrong or merely not askable.
   - The mode is what separates `AuthFailed` from `AuthNeedsAskpass`.
6. **Closing a remote tab always enqueues its kill, and only a confirmed kill dequeues it.**
   - While the host is live, the kill is still sent at once, fire-and-forget.
   - Keeping the queue entry until confirmed means a kill lost to a dropped connection, or to quitting right after closing the last tab, is retried after the next connect instead of leaking the session.
7. **The Host row is read-only for every existing group.** Groups without tabs never exist (they are pruned), so "editable while the group has no tabs" is unreachable. The subtitle says "Close all tabs to change the host", as the spec asks.
8. **"Move to → New group" creates the new group on the tab's own host.** This keeps the move legal.
9. **A remote `list-sessions` is only an empty list on tmux's own exit status 1 with a no-server message.** ssh's 255, or no status at all, is an error. The existing `is_no_server_stderr` matches "No such file", which ssh's own errors can contain, and treating those as empty would close live tabs.
10. **Quick-exit guard: no automatic reattach within 2 s of a spawn.** Some clients exit right after attaching while their session lives on (for example `open terminal failed`). The tab then shows a "Reconnect" page instead of looping.
11. **Crash detection on remote tabs.**
    - A remote tab's crash marker (`[exit N]`) comes from `pane_dead` in the connect and child-exit `list-sessions` replies (spec §3).
    - A pane that dies while attached keeps showing tmux's "Pane is dead" text. The sidebar marker appears at the next list-sessions, which is the next reconnect or client exit.
12. **`Target::Local` also carries the config path and the events directory.** So `TmuxCtl::events_dir()` now returns `Option<&Path>`.
13. **Downgrade is not guarded.** An older kabelsalat ignores `host` and `pending_kills` (serde keeps unknown fields silently; only a parse error moves the file aside), so it respawns every remote tab as a *local* shell under the same uuid and, on its next save, drops `host`: the group is local from then on, and the remote sessions keep running on the host with nothing pointing at them. The spec rules out a version field, and every cheap alternative (a second file, a deliberately unparsable field, a separate `remote_tabs` list) is a version field in disguise or a schema change of its own. So the plan only documents it (Task 16 README) and leaves the fix to the "Adopt sessions from host…" follow-up, which would let a re-upgraded kabelsalat find those sessions again. Recovery by hand: `ssh <host> tmux -L kabelsalat ls`, then `attach -t ks-<uuid>` or `kill-server`. If the user wants fail-safe downgrades in v1, the smallest schema change is to keep remote tabs in a separate top-level `remote_tabs` list (old versions then see no remote group at all, and create nothing), which touches Tasks 1, 2 and 10 — a decision for the user, not this plan.

### Facts checked while writing and reviewing this plan (OpenSSH 10.2, tmux 3.7c)

- `ssh_config(5)`: `ControlMaster=no` "will fall back to connecting normally if the control socket does not exist, or is not listening" — hence decision 2. `BatchMode=yes` disables "password prompts and host key confirmation requests". With `ServerAliveInterval=15` and `ServerAliveCountMax=3`, "ssh will disconnect after approximately 45 seconds". `%C` is the hash of `%l%h%p%r%j` and expands when given through `-S` on the command line too.
- `ssh(1)`: exits "with the exit status of the remote command or with 255 if an error occurred"; `-O check` and `-O exit` need the destination argument and exit 255 with `Control socket connect(…): No such file or directory` when there is no master. `-O exit` ends the master, and with it every mux client that ran through it — on quit that is intended; remote sessions are tmux's and survive.
- `SSH_ASKPASS_REQUIRE=force` is documented for "all passphrase input". Host-key confirmation goes through the same `read_passphrase` path in OpenSSH, so with askpass it is prompted graphically in practice, and a declined or failed confirmation ends in "Host key verification failed" (`classify` → `HostKeyUnknown`). Manual tests 2 and 3 of Task 17 cover both. `NumberOfPasswordPrompts=1` is accepted by 10.2; its effect on askpass prompts is not documented, so it is best-effort.
- A `ControlPersist` master redirects its stdio to `/dev/null` when it detaches, so the worker's pipes close as soon as the foreground ssh exits (`DRAIN_GRACE` is a safety net only). `-t` with a non-tty stdin prints "Pseudo-terminal will not be allocated" and continues; tabs have VTE's pty, the worker never passes `-t`.
- tmux: `source-file -` is listed under "CHANGES FROM 3.0a TO 3.1" (line 1435 of 3.7c's CHANGES, which lists newest first); `new-session -e` and `remain-on-exit failed` are under "3.1c TO 3.2"; `exit-empty` defaults to on since 3.2, so a bare `start-server` leaves no server behind — the bootstrap works only because `remote_conf()` turns `exit-empty` off inside the same client connection (verified with the real config: the server survives, `remain-on-exit failed` and the dead-pane formats work). `-f` only matters for the process that starts the server. `list-sessions` and `kill-session` exit 1 with `no server running on …` when there is no server, and `kill-session -t` exits 1 with `can't find session: …` when the server runs; `display-message -p -t <missing session>` exits 0 with empty output, which is why `pane_path` checks for an empty reply. `new-session -A -c <dir>` never changes the directory of an existing session.
- On this machine `ssh localhost` fails with "Too many authentication failures" because the agent offers many keys: the manual harness needs a `Host localhost` entry with `IdentitiesOnly yes` and the right `IdentityFile`, or a throwaway `sshd -p <port>` with its own key.

### Manual test harness (used by the GUI tasks)

A second kabelsalat instance must not share the running one's D-Bus name, tmux socket or state. Start it in a throwaway environment:

```sh
mkdir -p /tmp/ks-t/state/kabelsalat /tmp/ks-t/run && chmod 700 /tmp/ks-t/run
dbus-run-session -- env \
  WAYLAND_DISPLAY="$XDG_RUNTIME_DIR/${WAYLAND_DISPLAY:-wayland-0}" \
  XDG_STATE_HOME=/tmp/ks-t/state XDG_RUNTIME_DIR=/tmp/ks-t/run \
  cargo run
```

Remote tests need an `sshd` reachable as `localhost` with key login, and tmux ≥ 3.2 on that side. `ssh localhost tmux -V` must work without a prompt. Before Task 13 there is no UI for creating a remote group, so seed `/tmp/ks-t/state/kabelsalat/state.json` with:

```json
{"groups":[{"uuid":"g-remote","id":1,"name":"lo","palette":0,"host":"localhost"}],
 "tabs":[{"uuid":"11111111-1111-4111-8111-111111111111","group":1,"title":"remote"}],
 "active":"11111111-1111-4111-8111-111111111111","sidebar_visible":true}
```

Clean up afterwards: `ssh localhost tmux -L kabelsalat kill-server; rm -rf /tmp/ks-t`.

## File Structure

| File | Status | Responsibility |
|---|---|---|
| `src/remote.rs` | **create** | Pure logic:<br>• ssh version parsing and availability (`SshAvailability`)<br>• askpass choice (`AuthMode`, `auth_mode`, `askpass_env`)<br>• error classification and messages (`RemoteError`, `classify`)<br>• host state (`HostState`)<br>• ssh argv builders (`master_argv`, `mux_argv`, `control_argv`, `start_server_argv`, `upload_conf_argv`, `remote_tmux_prefix`) |
| `src/remote_worker.rs` | **create** | I/O:<br>• `detect_ssh`, `is_executable`, `control_path`<br>• `run_detached` (setsid, null stdin, pipe drains)<br>• the `Runner` trait<br>• the per-host `RemoteWorker` thread and its serial `HostSession` job runner, reporting `RemoteEvent`s |
| `src/state.rs` | modify | `SavedGroup::host`, `SavedState::pending_kills` + `PendingKill`, `validate_host` + `HostError`, `drop_allowed`, `reconcile_local`, `reconcile_remote` + `RemoteListing`/`RemoteAttach`/`RemotePlan`, `remote_hosts`, `group_host` |
| `src/tmuxctl.rs` | modify | `Target { Local, Remote }` in `TmuxCtl`; `command_argv`; public argv builders; `list_sessions_from_output`; remote `spawn_argv`; `remote_conf()` |
| `src/cli.rs` | modify | `GroupInfo::host`; `Action::Spawn::cwd` becomes `Option<PathBuf>` (for a remote group, `--cwd` is passed through verbatim and `None` means the remote home); help text |
| `src/control.rs` | modify | `request_spawn` takes `Option<PathBuf>` |
| `src/app.rs` | modify | Group host; pending-kill outbox; tab view stack + host page; host map and workers; connect/reconnect; remote child exit, disconnect, close, restart and cwd; create dialog + Group Settings host row; sidebar host header; drag-and-drop refusal feedback; picker; CLI spawn into remote groups; `ssh -O exit` on quit |
| `src/lib.rs` | modify | `pub mod remote; pub mod remote_worker;`; CSS for `.host-disconnected`, `.drop-refused` |
| `README.md` | modify | "Remote groups" section, shortcut, CLI note |

---
### Task 1: Persist a group's host and the pending remote kills; host validation and the move gate

**Depends on:** none (touches `src/app.rs` minimally; every later `app.rs` task comes after this one)

**Files:**
- Modify: `src/state.rs`:
  - imports `8-12`
  - `SavedGroup` `17-50`
  - `SavedGroup::new` `56-69`
  - `SavedState` `135-153`
  - `Default for SavedState` `168-179`
  - new free functions after `newest_first_index` (`321-326`)
  - tests: `sample_state` `427-477`, new tests appended to `mod tests`
- Modify: `src/app.rs` `save_state` (`1516-1594`; the `SavedGroup` literal starts at `1523`, the `SavedState` literal ends at `1579`). This is a compile fix only: the struct literals gain the new fields.

**Interfaces:**
- Consumes: nothing new.
- Produces (used by Tasks 2, 10, 13, 14):
  - `pub struct SavedGroup { …, pub host: Option<String> }` (`#[serde(default)]`)
  - `pub struct PendingKill { pub host: String, pub uuid: String }` (`Serialize, Deserialize, Clone, PartialEq, Eq, Debug`)
  - `pub struct SavedState { …, pub pending_kills: Vec<PendingKill> }` (`#[serde(default)]`)
  - `pub enum HostError { Empty, BadCharacter, LeadingDash }` + `impl fmt::Display`
  - `pub fn validate_host(host: &str) -> Result<(), HostError>`
  - `pub fn drop_allowed(src_host: Option<&str>, dest_host: Option<&str>) -> bool`

- [ ] **Step 1: Write the failing tests**

In `src/state.rs` `mod tests`, update `sample_state()`:
- add `host: None,` as the last field of **both** `SavedGroup { … }` literals (after `default_url`)
- add `pending_kills: Vec::new(),` as the last field of the `SavedState { … }` literal (after `sidebar_order`)

Then append these tests at the end of `mod tests`:

```rust
    // --- remote groups ---

    #[test]
    fn old_group_without_host_loads_as_local() {
        let json = r#"{
            "groups": [{"id": 1, "name": "w", "palette": 0}],
            "tabs": [], "active": null, "sidebar_visible": true
        }"#;
        let state: SavedState = serde_json::from_str(json).unwrap();
        assert_eq!(state.groups[0].host, None);
        assert!(state.pending_kills.is_empty());
    }

    #[test]
    fn host_and_pending_kills_survive_save_and_load() {
        let dir = tmp_dir("remote-fields");
        let path = dir.join("state.json");
        let mut state = sample_state();
        state.groups[1].host = Some("me@build-box".into());
        state.pending_kills = vec![PendingKill {
            host: "me@build-box".into(),
            uuid: "dead-beef".into(),
        }];
        save(&state, &path).unwrap();
        let back = load(&path);
        assert_eq!(back.groups[1].host.as_deref(), Some("me@build-box"));
        assert_eq!(back.pending_kills, state.pending_kills);
        assert_eq!(back, state);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_group_new_is_local() {
        assert_eq!(SavedGroup::new(1, "x".into(), 0).host, None);
    }

    #[test]
    fn validate_host_accepts_ssh_destinations_verbatim() {
        for host in [
            "build-box",
            "me@build-box",
            "ssh://me@build-box:2222",
            "me@[::1]",
            "10.0.0.7",
        ] {
            assert_eq!(validate_host(host), Ok(()), "{host}");
        }
    }

    #[test]
    fn validate_host_rejects_empty_whitespace_control_and_dash() {
        assert_eq!(validate_host(""), Err(HostError::Empty));
        assert_eq!(validate_host("me@build box"), Err(HostError::BadCharacter));
        assert_eq!(validate_host(" build-box"), Err(HostError::BadCharacter));
        assert_eq!(validate_host("build-box\n"), Err(HostError::BadCharacter));
        assert_eq!(validate_host("build\u{7}box"), Err(HostError::BadCharacter));
        assert_eq!(
            validate_host("-oProxyCommand=evil"),
            Err(HostError::LeadingDash)
        );
    }

    #[test]
    fn host_errors_read_as_sentences() {
        assert!(HostError::LeadingDash.to_string().contains("'-'"));
        assert!(!HostError::Empty.to_string().is_empty());
    }

    #[test]
    fn drop_allowed_only_within_one_host() {
        assert!(drop_allowed(None, None));
        assert!(drop_allowed(Some("a"), Some("a")));
        assert!(!drop_allowed(None, Some("a")));
        assert!(!drop_allowed(Some("a"), None));
        // Destinations are compared verbatim: two spellings are two hosts.
        assert!(!drop_allowed(Some("a"), Some("me@a")));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib state::tests`
Expected: compile errors. `no field host on type SavedGroup`, `cannot find type PendingKill`, `cannot find function validate_host`, and so on.

- [ ] **Step 3: Write the implementation**

In `src/state.rs`, change the imports (`8-12`) to:

```rust
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
```

In `SavedGroup`, after the `default_url` field (`49`), add:

```rust
    /// The ssh destination this group's tabs run on, handed to ssh verbatim
    /// (`user@host`, a `~/.ssh/config` alias, `ssh://user@host:port`). `None`
    /// is this computer. `serde(default)` so older state files load every
    /// group as local. Every tab of the group runs on this host; a tab's own
    /// host is always its group's.
    #[serde(default)]
    pub host: Option<String>,
```

In `SavedGroup::new`, add `host: None,` after `default_url: None,`.

After `SavedTab` (after line `133`), add:

```rust
/// A remote session still to be killed: its tab was closed, and its host has
/// not confirmed the kill yet — because it was unreachable, or because the app
/// quit first. Remote hosts never adopt sessions, so without this entry the
/// session would run forever. Flushed after the next successful connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingKill {
    pub host: String,
    /// The closed tab's uuid; its session is `ks-<uuid>`.
    pub uuid: String,
}
```

In `SavedState`, after the `sidebar_order` field (`152`), add:

```rust
    /// Remote sessions whose tabs are closed but whose kill the host has not
    /// confirmed yet. `serde(default)` so older state files load with none.
    #[serde(default)]
    pub pending_kills: Vec<PendingKill>,
```

In `impl Default for SavedState`, add `pending_kills: Vec::new(),` after `sidebar_order: SidebarOrder::default(),`.

After `newest_first_index` (after line `326`), add:

```rust
/// Why a string cannot be used as an ssh destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    Empty,
    /// Whitespace or a control character.
    BadCharacter,
    /// A leading `-`, which ssh would parse as an option.
    LeadingDash,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HostError::Empty => {
                "Enter a host, e.g. user@example.com or an alias from ~/.ssh/config."
            }
            HostError::BadCharacter => "A host can't contain spaces or control characters.",
            HostError::LeadingDash => "A host can't start with '-': ssh would read it as an option.",
        })
    }
}

/// Check a group's host before it is stored. The destination is otherwise
/// passed to ssh verbatim, so this only rejects what cannot be one argument:
/// nothing, whitespace or control characters, and a leading `-`.
pub fn validate_host(host: &str) -> Result<(), HostError> {
    if host.is_empty() {
        return Err(HostError::Empty);
    }
    if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(HostError::BadCharacter);
    }
    if host.starts_with('-') {
        return Err(HostError::LeadingDash);
    }
    Ok(())
}

/// May a tab move from a group on `src_host` into one on `dest_host`? Tabs
/// cannot change hosts — their session lives on one tmux server — so only a
/// move within one host (or within this computer, `None`) is allowed. Gates
/// the move picker, both sidebar drops, and their drag feedback.
pub fn drop_allowed(src_host: Option<&str>, dest_host: Option<&str>) -> bool {
    src_host == dest_host
}
```

In `src/app.rs` `save_state`, keep the crate compiling. Task 10 replaces both values with the live model.
- add `host: None,` after `default_url: g.default_url.clone(),` in the `SavedGroup { … }` literal
- add `pending_kills: Vec::new(),` after `sidebar_order: self.sidebar_order,` in the `SavedState { … }` literal

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib state::tests`
Expected: all `state::tests` pass, including the 7 new ones.

Run: `cargo fmt && cargo clippy --all-targets && cargo test`
Expected: no new warnings; all tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/state.rs src/app.rs
git commit -m "Persist a group's host and the pending remote kills"
```

---

### Task 2: Per-host restore plan

**Depends on:** Task 1

**Files:**
- Modify: `src/state.rs`. Add new items after `reconcile` (`399-421` before Task 1; `470-494` after it, with `#[cfg(test)]` at `496`), and append tests to `mod tests`.

**Interfaces:**
- Consumes (Task 1): `SavedGroup::host`.
- Produces (used by Tasks 10, 11):
  - `pub fn group_host(saved: &SavedState, group: usize) -> Option<&str>`
  - `pub fn reconcile_local(saved: &SavedState, live: &[String], dead: &[DeadPane]) -> ReconcilePlan`
  - `pub fn remote_hosts(saved: &SavedState) -> Vec<String>`
  - `pub struct RemoteListing { pub live: Vec<String>, pub dead: Vec<DeadPane> }` (`Debug, Clone, Default, PartialEq, Eq`)
  - `pub struct RemoteAttach { pub uuid: String, pub dead_exit: Option<i32> }`
  - `pub struct RemotePlan { pub attach: Vec<RemoteAttach>, pub respawn: Vec<String>, pub wait: Vec<String>, pub kill: Vec<String> }`
  - `pub fn reconcile_remote(tabs: &[String], listing: Option<&RemoteListing>, pending_kills: &[String]) -> RemotePlan`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/state.rs`:

```rust
    // --- per-host restore ---

    fn remote_state() -> SavedState {
        let mut state = sample_state();
        // Group 1 ("work", tabs bbb and ccc) lives on a remote host.
        state.groups[1].host = Some("me@box".into());
        state
    }

    #[test]
    fn a_local_only_state_reconciles_exactly_as_before() {
        let state = sample_state();
        for live in [
            vec![],
            vec!["aaa".to_string()],
            vec!["zzz".to_string(), "bbb".to_string()],
        ] {
            let dead = vec![DeadPane {
                uuid: "bbb".into(),
                exit_code: 3,
            }];
            assert_eq!(
                reconcile_local(&state, &live, &dead),
                reconcile(&state, &live, &dead)
            );
        }
    }

    #[test]
    fn local_reconcile_never_respawns_remote_tabs_locally() {
        let plan = reconcile_local(&remote_state(), &[], &[]);
        let respawned: Vec<_> = plan.respawn.iter().map(|t| t.uuid.as_str()).collect();
        assert_eq!(respawned, ["aaa"]);
        assert!(plan.attach.is_empty());
        assert!(plan.adopt.is_empty());
    }

    #[test]
    fn local_reconcile_still_adopts_local_orphans() {
        let plan = reconcile_local(&remote_state(), &["zzz".to_string()], &[]);
        let adopted: Vec<_> = plan.adopt.iter().map(|o| o.uuid.as_str()).collect();
        assert_eq!(adopted, ["zzz"]);
    }

    #[test]
    fn group_host_names_the_host_or_none() {
        let state = remote_state();
        assert_eq!(group_host(&state, 0), None);
        assert_eq!(group_host(&state, 1), Some("me@box"));
        assert_eq!(group_host(&state, 99), None);
    }

    #[test]
    fn an_unreachable_host_never_respawns() {
        // No successful list-sessions: every tab waits, nothing is spawned,
        // attached or killed — whatever is queued.
        let tabs = vec!["x".to_string(), "y".to_string()];
        let plan = reconcile_remote(&tabs, None, &["k".to_string()]);
        assert!(plan.attach.is_empty());
        assert!(plan.respawn.is_empty());
        assert!(plan.kill.is_empty());
        assert_eq!(plan.wait, tabs);
    }

    #[test]
    fn a_listed_host_attaches_and_respawns_but_never_adopts() {
        let tabs = vec!["x".to_string(), "y".to_string()];
        let listing = RemoteListing {
            // "stranger" may belong to another installation: ignored.
            live: vec!["stranger".to_string(), "x".to_string()],
            dead: vec![DeadPane {
                uuid: "x".into(),
                exit_code: 3,
            }],
        };
        let plan = reconcile_remote(&tabs, Some(&listing), &[]);
        assert_eq!(
            plan.attach,
            vec![RemoteAttach {
                uuid: "x".into(),
                dead_exit: Some(3)
            }]
        );
        assert_eq!(plan.respawn, vec!["y".to_string()]);
        assert!(plan.wait.is_empty());
        assert!(plan.kill.is_empty());
    }

    #[test]
    fn pending_kills_flush_only_sessions_that_still_live() {
        let listing = RemoteListing {
            live: vec!["k1".to_string()],
            dead: Vec::new(),
        };
        let plan = reconcile_remote(&[], Some(&listing), &["k1".to_string(), "k2".to_string()]);
        assert_eq!(plan.kill, vec!["k1".to_string()]);
    }

    #[test]
    fn remote_hosts_lists_each_host_with_tabs_once_in_group_order() {
        let mut state = remote_state();
        let mut second = SavedGroup::new(2, "again".into(), 0);
        second.host = Some("me@box".into());
        let mut empty = SavedGroup::new(3, "idle".into(), 0);
        empty.host = Some("other".into());
        let mut third = SavedGroup::new(4, "b".into(), 0);
        third.host = Some("b-host".into());
        state.groups.extend([second, empty, third]);
        for (uuid, group) in [("d", 2), ("e", 4)] {
            state.tabs.push(SavedTab {
                uuid: uuid.into(),
                group,
                title: String::new(),
                last_activity: None,
                last_title: None,
            });
        }
        assert_eq!(remote_hosts(&state), ["me@box", "b-host"]);
        assert!(remote_hosts(&sample_state()).is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib state::tests`
Expected: compile errors. `cannot find function reconcile_local`, `RemoteListing`, and so on.

- [ ] **Step 3: Write the implementation**

In `src/state.rs`, directly after `reconcile` (after its closing brace, line `421` before Task 1 / `494` after it), add:

```rust
/// The host a saved group's tabs run on; `None` for a local group or an
/// unknown id.
pub fn group_host(saved: &SavedState, group: usize) -> Option<&str> {
    saved
        .groups
        .iter()
        .find(|g| g.id == group)
        .and_then(|g| g.host.as_deref())
}

/// The startup reconciliation of the local bucket: [`reconcile`] over the
/// tabs of local groups only. Remote tabs are neither respawned locally nor
/// counted when deciding what to adopt — their host is reconciled on its own,
/// by [`reconcile_remote`], once it answers.
pub fn reconcile_local(saved: &SavedState, live: &[String], dead: &[DeadPane]) -> ReconcilePlan {
    let local = SavedState {
        tabs: saved
            .tabs
            .iter()
            .filter(|t| group_host(saved, t.group).is_none())
            .cloned()
            .collect(),
        ..saved.clone()
    };
    reconcile(&local, live, dead)
}

/// Every remote host that has at least one saved tab, once, in group order:
/// the hosts to connect at startup. A host known only from its pending kills
/// is not connected unasked; its queue flushes on the next connect.
pub fn remote_hosts(saved: &SavedState) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for group in &saved.groups {
        let Some(host) = &group.host else { continue };
        if saved.tabs.iter().any(|t| t.group == group.id) && !hosts.contains(host) {
            hosts.push(host.clone());
        }
    }
    hosts
}

/// A successful `list-sessions` from a remote host, as plain data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteListing {
    /// Tab uuids of the live `ks-<uuid>` sessions.
    pub live: Vec<String>,
    /// The subset whose pane has died, with the exit code.
    pub dead: Vec<DeadPane>,
}

/// A remote tab whose session is live: attach, crashed if its pane died.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAttach {
    pub uuid: String,
    pub dead_exit: Option<i32>,
}

/// What to do with one host's tabs, as plain data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemotePlan {
    /// Tabs with a live session, in `tabs` order.
    pub attach: Vec<RemoteAttach>,
    /// Tabs without one: `new-session -A` creates a fresh session.
    pub respawn: Vec<String>,
    /// Tabs that must stay unspawned: there is no successful listing.
    pub wait: Vec<String>,
    /// Queued kills whose session still lives.
    pub kill: Vec<String>,
}

/// Plan one remote host's tabs. `listing` is `None` until the host answered
/// `list-sessions` successfully — and then every tab waits: an unreachable
/// host, a failed login or a refused host never produces a fresh shell.
/// Live `ks-*` sessions no tab claims are ignored, never adopted; they may
/// belong to another installation sharing the host's server.
pub fn reconcile_remote(
    tabs: &[String],
    listing: Option<&RemoteListing>,
    pending_kills: &[String],
) -> RemotePlan {
    let Some(listing) = listing else {
        return RemotePlan {
            wait: tabs.to_vec(),
            ..RemotePlan::default()
        };
    };
    let dead_exit = |uuid: &str| {
        listing
            .dead
            .iter()
            .find(|d| d.uuid == uuid)
            .map(|d| d.exit_code)
    };
    let mut plan = RemotePlan::default();
    for uuid in tabs {
        if listing.live.contains(uuid) {
            plan.attach.push(RemoteAttach {
                uuid: uuid.clone(),
                dead_exit: dead_exit(uuid),
            });
        } else {
            plan.respawn.push(uuid.clone());
        }
    }
    plan.kill = pending_kills
        .iter()
        .filter(|uuid| listing.live.contains(uuid) && !tabs.contains(uuid))
        .cloned()
        .collect();
    plan
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib state::tests`
Expected: all pass (8 new).

Run: `cargo fmt && cargo clippy --all-targets && cargo test`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/state.rs
git commit -m "Plan startup restore per host; remote hosts never adopt"
```

---

### Task 3: `remote.rs` — ssh client version check

**Depends on:** none (can run in parallel with Tasks 1–2)

**Files:**
- Create: `src/remote.rs`
- Modify: `src/lib.rs:7-12` (module list)

**Interfaces:**
- Consumes: nothing.
- Produces (used by Tasks 9, 11, 13):
  - `pub const MIN_SSH: (u32, u32) = (8, 4);`
  - `pub enum SshAvailability { Available((u32, u32)), TooOld((u32, u32)), Missing }` (`Debug, Clone, Copy, PartialEq, Eq`)
  - `impl SshAvailability { pub fn is_available(&self) -> bool; pub fn reason(&self) -> Option<String> }`
  - `pub fn parse_ssh_version(output: &str) -> Option<(u32, u32)>`
  - `pub fn ssh_availability(version_output: Option<&str>) -> SshAvailability`

- [ ] **Step 1: Write the failing tests**

Create `src/remote.rs` with only the module doc and the tests:

```rust
//! Pure logic for remote groups: which ssh client and login mode to use, how
//! ssh and tmux failures read, and how remote commands are built and quoted.
//!
//! Like `state.rs` and `cli.rs` this module runs no processes and touches no
//! GTK; `remote_worker.rs` does the I/O. That split is what keeps it
//! unit-testable.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ssh_version_reads_openssh_releases() {
        assert_eq!(
            parse_ssh_version("OpenSSH_7.4p1, OpenSSL 1.0.2k-fips  26 Jan 2017\n"),
            Some((7, 4))
        );
        assert_eq!(parse_ssh_version("OpenSSH_8.3p1 Ubuntu-1"), Some((8, 3)));
        assert_eq!(
            parse_ssh_version("OpenSSH_8.4p1 Debian-5+deb11u3, OpenSSL 1.1.1w  11 Sep 2023"),
            Some((8, 4))
        );
        assert_eq!(parse_ssh_version("OpenSSH_9.9p1, OpenSSL 3.2.2"), Some((9, 9)));
        assert_eq!(
            parse_ssh_version("OpenSSH_10.2p1, OpenSSL 3.5.8 25 Aug 2026"),
            Some((10, 2))
        );
    }

    #[test]
    fn parse_ssh_version_rejects_other_clients_and_garbage() {
        assert_eq!(parse_ssh_version("OpenSSH_for_Windows_8.1p1, LibreSSL 3.0.2"), None);
        assert_eq!(parse_ssh_version("Sun_SSH_1.1.8, SSH protocols 1.5/2.0"), None);
        assert_eq!(parse_ssh_version("Dropbear v2022.83"), None);
        assert_eq!(parse_ssh_version("OpenSSH_"), None);
        assert_eq!(parse_ssh_version("OpenSSH_x.y"), None);
        assert_eq!(parse_ssh_version("OpenSSH_9"), None);
        assert_eq!(parse_ssh_version(""), None);
    }

    #[test]
    fn availability_needs_openssh_8_4() {
        assert_eq!(
            ssh_availability(Some("OpenSSH_8.4p1")),
            SshAvailability::Available((8, 4))
        );
        assert_eq!(
            ssh_availability(Some("OpenSSH_10.0p2")),
            SshAvailability::Available((10, 0))
        );
        assert_eq!(
            ssh_availability(Some("OpenSSH_8.3p1")),
            SshAvailability::TooOld((8, 3))
        );
        assert_eq!(ssh_availability(Some("Dropbear v2022.83")), SshAvailability::Missing);
        assert_eq!(ssh_availability(None), SshAvailability::Missing);
    }

    #[test]
    fn only_an_available_client_has_no_reason() {
        assert!(SshAvailability::Available((9, 0)).is_available());
        assert_eq!(SshAvailability::Available((9, 0)).reason(), None);
        let too_old = SshAvailability::TooOld((7, 4)).reason().unwrap();
        assert!(too_old.contains("8.4") && too_old.contains("7.4"), "{too_old}");
        assert!(!SshAvailability::Missing.is_available());
        assert!(SshAvailability::Missing.reason().unwrap().contains("8.4"));
    }
}
```

In `src/lib.rs`, change the module list (`7-12`, starting at `mod app;`) to:

```rust
mod app;
pub mod browser;
mod cli;
mod control;
pub mod remote;
pub mod state;
pub mod tmuxctl;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib remote::tests`
Expected: compile errors. `cannot find function parse_ssh_version`, `SshAvailability`, and so on.

- [ ] **Step 3: Write the implementation**

In `src/remote.rs`, between the module doc and `#[cfg(test)]`, add:

```rust
/// Oldest OpenSSH that honours `SSH_ASKPASS_REQUIRE`, without which a login
/// could end up prompting on a terminal.
pub const MIN_SSH: (u32, u32) = (8, 4);

/// Result of the one-time startup check of the local ssh client. Same shape
/// as `tmuxctl::TmuxAvailability`. Anything but `Available` disables remote
/// groups: "New remote group" is insensitive, and existing remote groups load
/// as disconnected (never dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshAvailability {
    Available((u32, u32)),
    TooOld((u32, u32)),
    /// No ssh, a non-OpenSSH client, or `ssh -V` output that does not parse.
    Missing,
}

impl SshAvailability {
    pub fn is_available(&self) -> bool {
        matches!(self, SshAvailability::Available(_))
    }

    /// Why remote groups are off, for the tooltip and the notice. `None` when
    /// they are on.
    pub fn reason(&self) -> Option<String> {
        let (major, minor) = MIN_SSH;
        match self {
            SshAvailability::Available(_) => None,
            SshAvailability::TooOld((found_major, found_minor)) => Some(format!(
                "Remote groups need OpenSSH {major}.{minor} or newer; this computer \
                 has OpenSSH {found_major}.{found_minor}."
            )),
            SshAvailability::Missing => Some(format!(
                "Remote groups need OpenSSH {major}.{minor} or newer; no OpenSSH \
                 client was found on this computer."
            )),
        }
    }
}

/// Parse `ssh -V` output (printed on stderr), e.g. `OpenSSH_9.9p1, OpenSSL …`,
/// into `(major, minor)`. Only the `OpenSSH_` prefix is recognised: other
/// clients (and OpenSSH for Windows, whose flags differ) yield `None`.
pub fn parse_ssh_version(output: &str) -> Option<(u32, u32)> {
    let rest = output.trim_start().strip_prefix("OpenSSH_")?;
    let (major, rest) = rest.split_once('.')?;
    let major: u32 = major.parse().ok()?;
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let minor: u32 = rest[..digits].parse().ok()?;
    Some((major, minor))
}

/// Classify the `ssh -V` output; `None` means ssh could not be run at all.
pub fn ssh_availability(version_output: Option<&str>) -> SshAvailability {
    match version_output.and_then(parse_ssh_version) {
        Some(version) if version >= MIN_SSH => SshAvailability::Available(version),
        Some(version) => SshAvailability::TooOld(version),
        None => SshAvailability::Missing,
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib remote::tests`
Expected: 4 passed.

Run: `cargo fmt && cargo clippy --all-targets && cargo test`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/remote.rs src/lib.rs
git commit -m "Add remote.rs with the OpenSSH version check"
```

---

### Task 4: `remote.rs` — login mode (askpass or batch only)

**Depends on:** Task 3 (same file)

**Files:**
- Modify: `src/remote.rs`. Add items after `ssh_availability`; add tests to `mod tests`.

**Interfaces:**
- Consumes: nothing new.
- Produces (used by Tasks 5, 6, 9, 11):
  - `pub const ASKPASS_PATHS: [&str; 3]`, `pub const ASKPASS_NAMES: [&str; 3]`
  - `pub enum AuthMode { Askpass(PathBuf), BatchOnly }` (`Debug, Clone, PartialEq, Eq`)
  - `pub fn auth_mode(env: impl Fn(&str) -> Option<String>, is_executable: impl Fn(&Path) -> bool) -> AuthMode`
  - `pub fn askpass_env(mode: &AuthMode) -> Vec<(String, String)>`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/remote.rs`:

```rust
    // --- auth mode ---

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn executables(paths: &[&str]) -> impl Fn(&Path) -> bool {
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        move |path| paths.iter().any(|p| p == path)
    }

    fn askpass(path: &str) -> AuthMode {
        AuthMode::Askpass(PathBuf::from(path))
    }

    #[test]
    fn ssh_askpass_wins_when_executable() {
        let mode = auth_mode(
            env_of(&[("SSH_ASKPASS", "/opt/my-askpass"), ("PATH", "/usr/bin")]),
            executables(&["/opt/my-askpass", "/usr/libexec/openssh/gnome-ssh-askpass"]),
        );
        assert_eq!(mode, askpass("/opt/my-askpass"));
    }

    #[test]
    fn a_non_executable_ssh_askpass_is_skipped() {
        let mode = auth_mode(
            env_of(&[("SSH_ASKPASS", "/opt/broken")]),
            executables(&["/usr/libexec/openssh/gnome-ssh-askpass"]),
        );
        assert_eq!(mode, askpass("/usr/libexec/openssh/gnome-ssh-askpass"));
    }

    #[test]
    fn fixed_paths_are_tried_in_order() {
        let all = [
            "/usr/libexec/openssh/gnome-ssh-askpass",
            "/usr/libexec/openssh/ssh-askpass",
            "/usr/lib/ssh/ssh-askpass",
        ];
        for skip in 0..all.len() {
            let mode = auth_mode(env_of(&[]), executables(&all[skip..]));
            assert_eq!(mode, askpass(all[skip]), "with {:?}", &all[skip..]);
        }
    }

    #[test]
    fn fixed_paths_beat_path_lookups() {
        let mode = auth_mode(
            env_of(&[("PATH", "/usr/bin")]),
            executables(&["/usr/bin/ksshaskpass", "/usr/lib/ssh/ssh-askpass"]),
        );
        assert_eq!(mode, askpass("/usr/lib/ssh/ssh-askpass"));
    }

    #[test]
    fn path_names_are_tried_in_order_whatever_the_directory_order() {
        let mode = auth_mode(
            env_of(&[("PATH", "/a:/b")]),
            executables(&["/a/ssh-askpass", "/b/lxqt-openssh-askpass", "/b/ksshaskpass"]),
        );
        assert_eq!(mode, askpass("/b/ksshaskpass"));
        let mode = auth_mode(
            env_of(&[("PATH", "/a:/b")]),
            executables(&["/a/ssh-askpass", "/b/lxqt-openssh-askpass"]),
        );
        assert_eq!(mode, askpass("/b/lxqt-openssh-askpass"));
    }

    #[test]
    fn empty_path_entries_are_ignored() {
        // An empty PATH entry means "the current directory" to a shell; a
        // relative askpass is never what we want.
        let mode = auth_mode(env_of(&[("PATH", "::/x")]), executables(&["ssh-askpass"]));
        assert_eq!(mode, AuthMode::BatchOnly);
    }

    #[test]
    fn nothing_found_means_batch_only() {
        assert_eq!(auth_mode(env_of(&[]), executables(&[])), AuthMode::BatchOnly);
    }

    #[test]
    fn askpass_env_forces_askpass_and_batch_sets_nothing() {
        assert_eq!(
            askpass_env(&askpass("/usr/bin/ksshaskpass")),
            vec![
                ("SSH_ASKPASS".to_string(), "/usr/bin/ksshaskpass".to_string()),
                ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
            ]
        );
        assert!(askpass_env(&AuthMode::BatchOnly).is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib remote::tests`
Expected: compile errors. `cannot find type AuthMode`, `auth_mode`, `askpass_env`, `Path`.

- [ ] **Step 3: Write the implementation**

At the top of `src/remote.rs`, after the module doc, add:

```rust
use std::path::{Path, PathBuf};
```

After `ssh_availability`, add:

```rust
/// Askpass programs at fixed paths, in priority order: Fedora's two, then
/// Debian's and Arch's.
pub const ASKPASS_PATHS: [&str; 3] = [
    "/usr/libexec/openssh/gnome-ssh-askpass",
    "/usr/libexec/openssh/ssh-askpass",
    "/usr/lib/ssh/ssh-askpass",
];

/// Askpass programs looked up on `PATH`, in priority order.
pub const ASKPASS_NAMES: [&str; 3] = ["ksshaskpass", "lxqt-openssh-askpass", "ssh-askpass"];

/// How a host's master logs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    /// One attempt with `SSH_ASKPASS=<path>` and `SSH_ASKPASS_REQUIRE=force`:
    /// key logins stay silent; passwords, passphrases and host-key
    /// confirmations go to the graphical program.
    Askpass(PathBuf),
    /// `BatchMode=yes`: anything interactive fails, and the host is refused.
    BatchOnly,
}

/// Pick the login mode. Candidates, first executable wins:
/// `$SSH_ASKPASS`, then [`ASKPASS_PATHS`], then [`ASKPASS_NAMES`] on `PATH`
/// (by name first, then by directory). `env` and `is_executable` are
/// injected so the choice is testable without touching the system.
pub fn auth_mode(
    env: impl Fn(&str) -> Option<String>,
    is_executable: impl Fn(&Path) -> bool,
) -> AuthMode {
    if let Some(path) = env("SSH_ASKPASS").filter(|p| !p.is_empty()) {
        let path = PathBuf::from(path);
        if is_executable(&path) {
            return AuthMode::Askpass(path);
        }
    }
    for path in ASKPASS_PATHS {
        let path = Path::new(path);
        if is_executable(path) {
            return AuthMode::Askpass(path.to_path_buf());
        }
    }
    if let Some(search) = env("PATH") {
        for name in ASKPASS_NAMES {
            for dir in search.split(':').filter(|dir| !dir.is_empty()) {
                let candidate = Path::new(dir).join(name);
                if is_executable(&candidate) {
                    return AuthMode::Askpass(candidate);
                }
            }
        }
    }
    AuthMode::BatchOnly
}

/// Environment for the master's ssh: the askpass pair in askpass mode,
/// nothing in batch mode (`BatchMode=yes` goes on the command line).
pub fn askpass_env(mode: &AuthMode) -> Vec<(String, String)> {
    match mode {
        AuthMode::Askpass(path) => vec![
            (
                "SSH_ASKPASS".to_string(),
                path.to_string_lossy().into_owned(),
            ),
            ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
        ],
        AuthMode::BatchOnly => Vec::new(),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib remote::tests`
Expected: 12 passed.

Run: `cargo fmt && cargo clippy --all-targets && cargo test`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/remote.rs
git commit -m "Choose between askpass and batch-only ssh logins"
```

---

### Task 5: `remote.rs` — error classification, messages and host state

**Depends on:** Task 4 (same file; uses `AuthMode`)

**Files:**
- Modify: `src/remote.rs`. Add items after `askpass_env`; add tests to `mod tests`.

**Interfaces:**
- Consumes (Task 4): `AuthMode`.
- Produces (used by Tasks 9, 11, 12, 13):
  - `pub enum RemoteError { Unreachable, AuthFailed, AuthNeedsAskpass, HostKeyUnknown, HostKeyChanged, TmuxMissing, TmuxTooOld(String), SshUnsupported, Other(String) }` (`Debug, Clone, PartialEq, Eq`)
  - `impl RemoteError { pub fn message(&self, host: &str) -> String }`
  - `pub fn classify(exit_code: Option<i32>, stderr: &str, mode: &AuthMode) -> RemoteError`
  - `pub enum HostState { Connecting, Live, Disconnected(RemoteError) }` (`Debug, Clone, PartialEq, Eq`)

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/remote.rs`. The stderr texts are captured from OpenSSH 9.x/10.x and from bash, dash and fish:

```rust
    // --- classify ---

    const BATCH: AuthMode = AuthMode::BatchOnly;

    fn with_askpass() -> AuthMode {
        AuthMode::Askpass(PathBuf::from("/usr/libexec/openssh/gnome-ssh-askpass"))
    }

    #[test]
    fn network_failures_are_unreachable() {
        for stderr in [
            "ssh: Could not resolve hostname nohost.invalid: Name or service not known\r\n",
            "ssh: connect to host 127.0.0.1 port 2222: Connection refused\r\n",
            "ssh: connect to host 10.255.255.1 port 22: Connection timed out\r\n",
            "ssh: connect to host 10.0.0.9 port 22: No route to host\r\n",
            "ssh: connect to host 10.0.0.9 port 22: Network is unreachable\r\n",
            "kex_exchange_identification: read: Connection reset by peer\r\n",
        ] {
            assert_eq!(classify(Some(255), stderr, &BATCH), RemoteError::Unreachable, "{stderr}");
        }
    }

    #[test]
    fn an_unknown_host_key_is_host_key_unknown() {
        let stderr = "No ED25519 host key is known for [localhost]:2222 and you have \
                      requested strict checking.\r\nHost key verification failed.\r\n";
        assert_eq!(classify(Some(255), stderr, &BATCH), RemoteError::HostKeyUnknown);
        // With askpass, a declined confirmation ends the same way.
        assert_eq!(
            classify(Some(255), "Host key verification failed.\r\n", &with_askpass()),
            RemoteError::HostKeyUnknown
        );
    }

    #[test]
    fn a_changed_host_key_is_host_key_changed() {
        let stderr = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
                      @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\r\n\
                      @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
                      IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\r\n\
                      Host key verification failed.\r\n";
        assert_eq!(classify(Some(255), stderr, &BATCH), RemoteError::HostKeyChanged);
    }

    #[test]
    fn permission_denied_depends_on_whether_a_prompt_was_possible() {
        let stderr = "me@localhost: Permission denied (publickey,gssapi-keyex,gssapi-with-mic,password).\r\n";
        assert_eq!(classify(Some(255), stderr, &BATCH), RemoteError::AuthNeedsAskpass);
        assert_eq!(classify(Some(255), stderr, &with_askpass()), RemoteError::AuthFailed);
        assert_eq!(
            classify(
                Some(255),
                "Received disconnect from 10.0.0.9 port 22:2: Too many authentication failures\r\n",
                &with_askpass()
            ),
            RemoteError::AuthFailed
        );
    }

    #[test]
    fn a_missing_remote_tmux_is_tmux_missing() {
        for stderr in [
            "bash: line 1: tmux: command not found\n",
            "sh: 1: tmux: not found\n",
            "fish: Unknown command: tmux\nfish: \ntmux -V\n^~~^\n",
        ] {
            assert_eq!(classify(Some(127), stderr, &BATCH), RemoteError::TmuxMissing, "{stderr}");
        }
        // Exit 127 alone is enough, whatever the shell printed.
        assert_eq!(classify(Some(127), "", &BATCH), RemoteError::TmuxMissing);
    }

    #[test]
    fn anything_else_keeps_its_stderr() {
        assert_eq!(
            classify(Some(255), "  mux_client_request_session: read from master failed  \n", &BATCH),
            RemoteError::Other("mux_client_request_session: read from master failed".into())
        );
        assert_eq!(classify(None, "", &BATCH), RemoteError::Other(String::new()));
    }

    #[test]
    fn every_error_names_what_to_do() {
        let host = "me@box";
        assert!(RemoteError::HostKeyUnknown.message(host).contains("ssh me@box"));
        assert!(RemoteError::HostKeyChanged.message(host).contains("ssh-keygen -R"));
        assert!(RemoteError::AuthNeedsAskpass.message(host).contains("askpass"));
        assert!(RemoteError::TmuxTooOld("3.1c".into()).message(host).contains("3.1c"));
        assert!(RemoteError::TmuxTooOld("3.1c".into()).message(host).contains("3.2"));
        assert!(RemoteError::TmuxMissing.message(host).contains("3.2"));
        assert!(RemoteError::SshUnsupported.message(host).contains("8.4"));
        assert!(RemoteError::Other("boom".into()).message(host).contains("boom"));
        for err in [
            RemoteError::Unreachable,
            RemoteError::AuthFailed,
            RemoteError::Other(String::new()),
        ] {
            assert!(err.message(host).contains(host), "{err:?}");
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib remote::tests`
Expected: compile errors. `cannot find type RemoteError`, `classify`.

- [ ] **Step 3: Write the implementation**

After `askpass_env` in `src/remote.rs`, add:

```rust
/// Why a host is not usable. Each variant carries a specific, actionable
/// message (`message`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteError {
    /// DNS failure, connection refused, timeout, lost connection.
    Unreachable,
    /// Wrong credentials (or a declined askpass prompt).
    AuthFailed,
    /// The login needs a prompt and no askpass program is available.
    AuthNeedsAskpass,
    /// The host key is not in `known_hosts`.
    HostKeyUnknown,
    /// The host key differs from `known_hosts`.
    HostKeyChanged,
    /// No tmux on the host.
    TmuxMissing,
    /// The host's tmux is older than 3.2; carries its version.
    TmuxTooOld(String),
    /// The local ssh client fails the version check.
    SshUnsupported,
    /// Anything else; carries the trimmed stderr.
    Other(String),
}

impl RemoteError {
    /// The text the disconnected page and the sidebar tooltip show.
    pub fn message(&self, host: &str) -> String {
        let (major, minor) = crate::tmuxctl::MIN_VERSION;
        let (ssh_major, ssh_minor) = MIN_SSH;
        match self {
            RemoteError::Unreachable => format!(
                "Can't reach {host}: the name did not resolve, the connection was \
                 refused or lost, or it timed out. Check the network and the host, \
                 then Reconnect."
            ),
            RemoteError::AuthFailed => format!(
                "Logging in to {host} failed. Check your key or password, then Reconnect."
            ),
            RemoteError::AuthNeedsAskpass => format!(
                "{host} asks for a password or passphrase, and no graphical askpass \
                 program is installed. Add your key to ssh-agent, or install one \
                 (Fedora: openssh-askpass, Debian: ssh-askpass-gnome), then Reconnect."
            ),
            RemoteError::HostKeyUnknown => format!(
                "The host key of {host} is not known yet. Run `ssh {host}` once in a \
                 terminal to check and accept it, then Reconnect."
            ),
            RemoteError::HostKeyChanged => format!(
                "The host key of {host} differs from the one in ~/.ssh/known_hosts. \
                 This can mean an attack. Verify the new key, remove the old one with \
                 `ssh-keygen -R`, then Reconnect."
            ),
            RemoteError::TmuxMissing => format!(
                "tmux is not installed on {host}. Remote groups need tmux \
                 {major}.{minor} or newer on the host."
            ),
            RemoteError::TmuxTooOld(found) => format!(
                "{host} has tmux {found}; remote groups need tmux {major}.{minor} or newer."
            ),
            RemoteError::SshUnsupported => format!(
                "Remote groups need OpenSSH {ssh_major}.{ssh_minor} or newer on this computer."
            ),
            RemoteError::Other(detail) if detail.is_empty() => {
                format!("Connecting to {host} failed.")
            }
            RemoteError::Other(detail) => format!("Connecting to {host} failed: {detail}"),
        }
    }
}

/// Map a failed ssh (or remote command) to a [`RemoteError`]. Order matters:
/// a changed host key also ends in "Host key verification failed", so it is
/// checked first. `mode` decides what "Permission denied" means — with no
/// askpass, a password host fails exactly like a wrong password.
pub fn classify(exit_code: Option<i32>, stderr: &str, mode: &AuthMode) -> RemoteError {
    const UNREACHABLE: [&str; 7] = [
        "Could not resolve hostname",
        "Name or service not known",
        "Connection refused",
        "timed out",
        "No route to host",
        "Network is unreachable",
        "Connection reset",
    ];
    const AUTH: [&str; 3] = [
        "Permission denied",
        "Too many authentication failures",
        "Authentication failed",
    ];
    const NO_TMUX: [&str; 3] = [
        "tmux: command not found",
        "tmux: not found",
        "Unknown command: tmux",
    ];
    if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return RemoteError::HostKeyChanged;
    }
    if stderr.contains("host key is known for") || stderr.contains("Host key verification failed")
    {
        return RemoteError::HostKeyUnknown;
    }
    if UNREACHABLE.iter().any(|needle| stderr.contains(needle)) {
        return RemoteError::Unreachable;
    }
    if AUTH.iter().any(|needle| stderr.contains(needle)) {
        return match mode {
            AuthMode::BatchOnly => RemoteError::AuthNeedsAskpass,
            AuthMode::Askpass(_) => RemoteError::AuthFailed,
        };
    }
    if exit_code == Some(127) || NO_TMUX.iter().any(|needle| stderr.contains(needle)) {
        return RemoteError::TmuxMissing;
    }
    RemoteError::Other(stderr.trim().to_string())
}

/// A remote host's state. It lives on the host, so all of a host's tabs
/// change state together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostState {
    /// The worker is logging in and checking the host.
    Connecting,
    /// The master is up and the host listed its sessions.
    Live,
    /// Not usable; the error says why. Only an explicit Reconnect leaves this
    /// state — there are no automatic retries, so askpass never pops up unasked.
    Disconnected(RemoteError),
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib remote::tests`
Expected: 19 passed.

Run: `cargo fmt && cargo clippy --all-targets && cargo test`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/remote.rs
git commit -m "Classify ssh and remote tmux failures into actionable errors"
```

---

### Task 6: `remote.rs` — ssh argv builders and remote quoting

**Depends on:** Task 5 (same file; uses `AuthMode`)

**Files:**
- Modify: `src/remote.rs`. Add items after `HostState`; add tests to `mod tests`.

**Interfaces:**
- Consumes: `crate::tmuxctl::shell_quote_argv(&[String]) -> String` (existing, `pub(crate)`, `src/tmuxctl.rs:609`), and `AuthMode` (Task 4).
- Produces (used by Tasks 7, 9):
  - `pub const REMOTE_SERVER: &str = "kabelsalat";`
  - `pub const SSH_FAILED: i32 = 255;`
  - `pub const CONNECT_TIMEOUT_SECS: u32 = 10;`
  - `pub fn remote_tmux_prefix() -> Vec<String>` → `["tmux", "-L", "kabelsalat", "-f", "/dev/null"]`
  - `pub fn mux_argv(dest: &str, control_path: &Path, tty: bool, remote_argv: &[String]) -> Vec<String>`
  - `pub fn master_argv(dest: &str, control_path: &Path, mode: &AuthMode, remote_argv: &[String]) -> Vec<String>`
  - `pub enum ControlOp { Check, Exit }` (`Debug, Clone, Copy, PartialEq, Eq`)
  - `pub fn control_argv(dest: &str, control_path: &Path, op: ControlOp) -> Vec<String>`
  - `pub fn start_server_argv() -> Vec<String>`
  - `pub fn upload_conf_argv() -> Vec<String>`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/remote.rs`:

```rust
    // --- argv and quoting ---

    /// Run `script` with the local `sh -c` and return its stdout. Stands in
    /// for the remote login shell, which only has to parse POSIX quotes.
    fn run_sh(script: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    /// The words `sh` makes of `command` (a string of shell words), each
    /// printed with a 0x1f terminator so empty words and newlines survive.
    fn sh_words(command: &str) -> Vec<String> {
        let out = run_sh(&format!("printf '%s\\037' {command}"));
        let mut words: Vec<String> = out.split('\u{1f}').map(String::from).collect();
        words.pop(); // after the last terminator
        words
    }

    fn nasty() -> Vec<String> {
        [
            "it's",
            "$HOME",
            "#not a comment",
            "two  spaces",
            "line1\nline2",
            "",
            "a;b|c&d",
            "*",
            "~",
            "back\\slash",
            "\"dq\"",
            "$(touch /tmp/ks-pwned)",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn remote_quoting_round_trips_through_sh() {
        let argv = nasty();
        let ssh = mux_argv("me@box", Path::new("/run/ks/ssh/%C"), false, &argv);
        // What ssh hands the remote login shell is exactly the last word.
        assert_eq!(sh_words(ssh.last().unwrap()), argv);
    }

    #[test]
    fn nested_quoting_survives_two_shells() {
        // A tab command is quoted once for tmux's `sh -c` and once more for
        // the login shell; both layers must give back the original argv.
        let inner = crate::tmuxctl::shell_quote_argv(&nasty());
        let outer = crate::tmuxctl::shell_quote_argv(&[
            "sh".to_string(),
            "-c".to_string(),
            format!("printf '%s\\037' {inner}"),
        ]);
        let mut words: Vec<String> = run_sh(&outer).split('\u{1f}').map(String::from).collect();
        words.pop();
        assert_eq!(words, nasty());
    }

    #[test]
    fn mux_argv_never_authenticates_or_prompts() {
        let argv = mux_argv(
            "me@box",
            Path::new("/run/ks/ssh/%C"),
            true,
            &["tmux".to_string(), "-V".to_string()],
        );
        assert_eq!(
            argv,
            [
                "ssh",
                "-S",
                "/run/ks/ssh/%C",
                "-o",
                "ControlMaster=no",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ProxyCommand=false",
                "-t",
                "me@box",
                "--",
                "tmux -V",
            ]
        );
        let no_tty = mux_argv("me@box", Path::new("/c"), false, &["true".to_string()]);
        assert!(!no_tty.contains(&"-t".to_string()));
    }

    #[test]
    fn master_argv_carries_the_spec_options() {
        let argv = master_argv(
            "me@box",
            Path::new("/run/ks/ssh/%C"),
            &AuthMode::BatchOnly,
            &["tmux".to_string(), "-V".to_string()],
        );
        for option in [
            "ControlMaster=auto",
            "ControlPersist=yes",
            "ConnectTimeout=10",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
            "BatchMode=yes",
        ] {
            let at = argv.iter().position(|a| a == option).unwrap_or_else(|| panic!("{option}"));
            assert_eq!(argv[at - 1], "-o");
        }
        assert_eq!(&argv[..3], ["ssh", "-S", "/run/ks/ssh/%C"]);
        assert_eq!(&argv[argv.len() - 3..], ["me@box", "--", "tmux -V"]);

        let askpass = master_argv(
            "me@box",
            Path::new("/c"),
            &AuthMode::Askpass(PathBuf::from("/usr/bin/ksshaskpass")),
            &["true".to_string()],
        );
        assert!(!askpass.contains(&"BatchMode=yes".to_string()));
        // A single attempt, as the spec asks.
        assert!(askpass.contains(&"NumberOfPasswordPrompts=1".to_string()));
    }

    #[test]
    fn control_argv_targets_the_master() {
        assert_eq!(
            control_argv("me@box", Path::new("/c/%C"), ControlOp::Check),
            ["ssh", "-S", "/c/%C", "-O", "check", "me@box"]
        );
        assert_eq!(
            control_argv("me@box", Path::new("/c/%C"), ControlOp::Exit),
            ["ssh", "-S", "/c/%C", "-O", "exit", "me@box"]
        );
    }

    #[test]
    fn the_server_is_started_and_configured_in_one_command() {
        assert_eq!(
            start_server_argv(),
            [
                "tmux",
                "-L",
                "kabelsalat",
                "-f",
                "/dev/null",
                "start-server",
                ";",
                "source-file",
                "-"
            ]
        );
        // The `;` must reach tmux as its own word.
        let quoted = crate::tmuxctl::shell_quote_argv(&start_server_argv());
        assert!(sh_words(&quoted).contains(&";".to_string()));
    }

    #[test]
    fn the_upload_fallback_writes_under_the_remote_state_dir() {
        let argv = upload_conf_argv();
        assert_eq!(&argv[..2], ["sh", "-c"]);
        assert!(argv[2].contains("${XDG_STATE_HOME:-$HOME/.local/state}/kabelsalat"));
        assert!(argv[2].contains("cat >"));
        assert!(argv[2].contains("source-file"));
        assert!(argv[2].contains("-L kabelsalat -f /dev/null"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib remote::tests`
Expected: compile errors. `cannot find function mux_argv`, and so on.

- [ ] **Step 3: Write the implementation**

After `HostState` in `src/remote.rs`, add:

```rust
/// Socket name of the shared kabelsalat tmux server on every remote host
/// (`tmux -L kabelsalat`), used by every installation and version. The last
/// one to connect applies its config to the whole server, so `TMUX_CONF`
/// changes must stay backward compatible.
pub const REMOTE_SERVER: &str = "kabelsalat";

/// ssh's own exit status for "ssh failed" (as opposed to the remote
/// command's status, which ssh passes through).
pub const SSH_FAILED: i32 = 255;

/// `ConnectTimeout` for every ssh this app runs.
pub const CONNECT_TIMEOUT_SECS: u32 = 10;

/// `tmux -L kabelsalat -f /dev/null`: our server, and — should this command
/// be the one that starts it — none of the user's own `~/.tmux.conf`. Our
/// config arrives through `source-file -` right after the start.
pub fn remote_tmux_prefix() -> Vec<String> {
    ["tmux", "-L", REMOTE_SERVER, "-f", "/dev/null"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// ssh through the host's master for tabs and the worker's tmux calls. They
/// never authenticate: `ControlMaster=no` uses the master. ssh would fall
/// back to a direct connection when the master's socket is gone, so
/// `BatchMode=yes` keeps that fallback from ever prompting, and
/// `ProxyCommand=false` makes it fail at once (exit 255, no network): the
/// mux client is tried before any connection, so a live master is reused as
/// usual. `remote_argv` is quoted once more into the single string the
/// remote login shell parses.
pub fn mux_argv(dest: &str, control_path: &Path, tty: bool, remote_argv: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "ssh".into(),
        "-S".into(),
        control_path.to_string_lossy().into_owned(),
        "-o".into(),
        "ControlMaster=no".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
        "-o".into(),
        "ProxyCommand=false".into(),
    ];
    if tty {
        argv.push("-t".into());
    }
    argv.push(dest.into());
    argv.push("--".into());
    argv.push(crate::tmuxctl::shell_quote_argv(remote_argv));
    argv
}

/// The worker's login: becomes (or reuses) the host's master and runs
/// `remote_argv` over it. Only this command ever authenticates.
pub fn master_argv(
    dest: &str,
    control_path: &Path,
    mode: &AuthMode,
    remote_argv: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "ssh".into(),
        "-S".into(),
        control_path.to_string_lossy().into_owned(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        "ControlPersist=yes".into(),
        "-o".into(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-o".into(),
    ];
    argv.push(match mode {
        AuthMode::BatchOnly => "BatchMode=yes".into(),
        // One attempt: a wrong password is reported, not re-asked.
        AuthMode::Askpass(_) => "NumberOfPasswordPrompts=1".into(),
    });
    argv.push(dest.into());
    argv.push("--".into());
    argv.push(crate::tmuxctl::shell_quote_argv(remote_argv));
    argv
}

/// A control request to the host's master (`ssh -O …`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlOp {
    /// Is the master alive? Exit 0 when it is.
    Check,
    /// Stop the master. Remote sessions survive.
    Exit,
}

pub fn control_argv(dest: &str, control_path: &Path, op: ControlOp) -> Vec<String> {
    let op = match op {
        ControlOp::Check => "check",
        ControlOp::Exit => "exit",
    };
    vec![
        "ssh".into(),
        "-S".into(),
        control_path.to_string_lossy().into_owned(),
        "-O".into(),
        op.into(),
        dest.into(),
    ]
}

/// `tmux -L kabelsalat -f /dev/null start-server ; source-file -`, with the
/// remote config on stdin: nothing is written to the remote disk. tmux reads
/// `source-file -` from stdin since 3.1 (CHANGES, "3.0a to 3.1").
pub fn start_server_argv() -> Vec<String> {
    let mut argv = remote_tmux_prefix();
    argv.extend(["start-server", ";", "source-file", "-"].map(String::from));
    argv
}

/// Fallback for a tmux that refuses `source-file -`: store the config (from
/// stdin) at `${XDG_STATE_HOME:-$HOME/.local/state}/kabelsalat/tmux.conf` on
/// the host and source it from there. Runs under `sh -c` so the remote login
/// shell's syntax does not matter.
pub fn upload_conf_argv() -> Vec<String> {
    const SCRIPT: &str = r#"d="${XDG_STATE_HOME:-$HOME/.local/state}/kabelsalat" && mkdir -p "$d" && cat > "$d/tmux.conf" && exec tmux -L kabelsalat -f /dev/null start-server \; source-file "$d/tmux.conf""#;
    vec!["sh".into(), "-c".into(), SCRIPT.into()]
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib remote::tests`
Expected: 26 passed. The quoting tests shell out to the local `sh`, which is always present.

Run: `cargo fmt && cargo clippy --all-targets && cargo test`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/remote.rs
git commit -m "Build the ssh argv for masters, tabs and control requests"
```

---
### Task 7: `tmuxctl.rs` — `Target::Local` / `Target::Remote`

**Depends on:** Tasks 1 and 6. It changes one line of `src/app.rs` `init`, so it runs after Task 1 and before every later `app.rs` task.

**Files:**
- Modify `src/tmuxctl.rs`:
  - imports (`7-9`)
  - `TMUX_CONF` users
  - `TmuxCtl` struct and impl (`249-524`)
  - tests: `writes_config_with_events_dir` (`990-1000`), plus new tests at the end of `mod tests`
- Modify `src/app.rs` `init`, the events-dir watch (`847-881`, first two lines).

**Interfaces:**
- Consumes (Task 6): `remote::{remote_tmux_prefix, mux_argv, SSH_FAILED}`.
- Produces (used by Tasks 9, 11):
  - `pub enum Target { Local { socket: PathBuf, conf: PathBuf, events_dir: PathBuf }, Remote { dest: String, control_path: PathBuf } }` (`Debug, Clone, PartialEq, Eq`)
  - `impl TmuxCtl`:
    - `pub fn remote(dest: &str, control_path: &Path) -> Self`
    - `pub fn target(&self) -> &Target`
    - `pub fn is_remote(&self) -> bool`
    - `pub fn events_dir(&self) -> Option<&Path>` (**signature change**, was `&Path`)
    - `pub fn command_argv(&self, args: &[String]) -> Vec<String>`
    - `pub fn list_sessions_args() -> Vec<String>`
    - `pub fn kill_session_args(uuid: &str) -> Vec<String>`
    - `pub fn respawn_pane_args(uuid: &str) -> Vec<String>`
    - `pub fn pane_current_path_args(uuid: &str) -> Vec<String>`
    - `pub fn list_sessions_from_output(&self, code: Option<i32>, stdout: &str, stderr: &str) -> Result<Vec<SessionInfo>, TmuxError>`
  - `spawn_argv` for a `Remote` target returns `ssh -S <cp> -o ControlMaster=no -o BatchMode=yes -o ConnectTimeout=10 -o ProxyCommand=false -t <dest> -- '<tmux -L kabelsalat -f /dev/null new-session -A … -s ks-<uuid> [cmd]>'` (no `$SHELL`).
  - `pub fn remote_conf() -> String`: `TMUX_CONF` without the `pane-died` hook line.

- [ ] **Step 1: Write the failing tests**

In `src/tmuxctl.rs` `mod tests`, change `writes_config_with_events_dir` (`990-1000`) to unwrap the new `Option`:

```rust
    #[test]
    fn writes_config_with_events_dir() {
        let dir = temp_dir("conf");
        let ctl = test_ctl(&dir);
        let events_dir = ctl.events_dir().unwrap().to_path_buf();
        let conf = std::fs::read_to_string(dir.join("state/tmux.conf")).unwrap();
        assert!(conf.contains("set -g status off"));
        assert!(conf.contains("remain-on-exit failed"));
        assert!(conf.contains(&*events_dir.to_string_lossy()));
        assert!(!conf.contains("{events_dir}"));
        assert!(events_dir.is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }
```

Append to the end of `mod tests`:

```rust
    // --- remote target ---

    fn remote_ctl() -> TmuxCtl {
        TmuxCtl::remote("me@box", Path::new("/run/ks/ssh/%C"))
    }

    /// The words `sh` makes of a string of shell words (see remote.rs).
    fn sh_words(command: &str) -> Vec<String> {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\037' {command}"))
            .output()
            .unwrap();
        assert!(out.status.success());
        let mut words: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .split('\u{1f}')
            .map(String::from)
            .collect();
        words.pop();
        words
    }

    #[test]
    fn a_local_ctl_targets_its_socket() {
        let dir = temp_dir("target-local");
        let ctl = test_ctl(&dir);
        assert!(!ctl.is_remote());
        assert!(matches!(ctl.target(), Target::Local { .. }));
        let argv = ctl.command_argv(&TmuxCtl::kill_session_args("abc"));
        assert_eq!(argv[0], "tmux");
        assert_eq!(argv[1], "-S");
        assert!(argv[2].ends_with("run/tmux.sock"));
        assert_eq!(&argv[3..], ["kill-session", "-t", "ks-abc"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_remote_ctl_wraps_the_same_command_in_ssh() {
        let ctl = remote_ctl();
        assert!(ctl.is_remote());
        assert_eq!(ctl.events_dir(), None);
        let argv = ctl.command_argv(&TmuxCtl::kill_session_args("abc"));
        assert_eq!(&argv[..3], ["ssh", "-S", "/run/ks/ssh/%C"]);
        assert!(argv.contains(&"ControlMaster=no".to_string()));
        assert!(argv.contains(&"BatchMode=yes".to_string()));
        // No pty for a query.
        assert!(!argv[..argv.len() - 1].contains(&"-t".to_string()));
        assert_eq!(
            sh_words(argv.last().unwrap()),
            [
                "tmux",
                "-L",
                "kabelsalat",
                "-f",
                "/dev/null",
                "kill-session",
                "-t",
                "ks-abc"
            ]
        );
    }

    #[test]
    fn remote_spawn_argv_attaches_through_the_master_with_a_tty() {
        let argv = remote_ctl().spawn_argv("1234", Some(Path::new("/srv/#x")), None, &[]);
        assert_eq!(&argv[..3], ["ssh", "-S", "/run/ks/ssh/%C"]);
        let dest = argv.iter().position(|a| a == "me@box").unwrap();
        assert_eq!(argv[dest - 1], "-t");
        assert_eq!(argv[dest + 1], "--");
        assert_eq!(dest + 3, argv.len());
        // No command: no shell-command word at all, so tmux starts the remote
        // user's default shell — the local $SHELL means nothing there.
        assert_eq!(
            sh_words(&argv[dest + 2]),
            [
                "tmux",
                "-L",
                "kabelsalat",
                "-f",
                "/dev/null",
                "new-session",
                "-A",
                "-c",
                "/srv/##x",
                "-s",
                "ks-1234"
            ]
        );
    }

    #[test]
    fn remote_spawn_argv_quotes_the_command_for_both_shells() {
        let command = vec!["echo".to_string(), "it's $HOME".to_string()];
        let argv = remote_ctl().spawn_argv("1234", None, Some(&command), &[]);
        let words = sh_words(argv.last().unwrap());
        assert_eq!(&words[5..9], ["new-session", "-A", "-s", "ks-1234"]);
        assert_eq!(words.len(), 10);
        // tmux hands its one shell-command word to `sh -c`, which must see
        // the original argv again.
        assert_eq!(sh_words(&words[9]), command);
    }

    #[test]
    fn a_remote_ssh_failure_is_never_an_empty_session_list() {
        let ctl = remote_ctl();
        // ssh's own failure, even with a "No such file" in its stderr.
        assert!(
            ctl.list_sessions_from_output(
                Some(255),
                "",
                "Control socket connect(/run/ks/ssh/x): No such file or directory"
            )
            .is_err()
        );
        // ssh never ran (spawn error): no status at all.
        assert!(
            ctl.list_sessions_from_output(None, "", "No such file or directory (os error 2)")
                .is_err()
        );
        // The remote shell has no tmux.
        assert!(
            ctl.list_sessions_from_output(Some(127), "", "tmux: command not found")
                .is_err()
        );
    }

    #[test]
    fn a_remote_server_that_is_not_running_is_an_empty_list() {
        let ctl = remote_ctl();
        assert_eq!(
            ctl.list_sessions_from_output(
                Some(1),
                "",
                "no server running on /tmp/tmux-1000/kabelsalat\n"
            )
            .unwrap(),
            Vec::new()
        );
        assert_eq!(
            ctl.list_sessions_from_output(
                Some(1),
                "",
                "error connecting to /tmp/tmux-1000/kabelsalat (No such file or directory)\n"
            )
            .unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn list_sessions_from_output_parses_both_targets_alike() {
        let dir = temp_dir("target-parse");
        let local = test_ctl(&dir);
        for ctl in [&local, &remote_ctl()] {
            let sessions = ctl
                .list_sessions_from_output(Some(0), "ks-a\t0\t\nother\t0\t\nks-b\t1\t2\n", "")
                .unwrap();
            assert_eq!(
                sessions,
                vec![
                    SessionInfo {
                        uuid: "a".into(),
                        pane_dead: false,
                        dead_status: None
                    },
                    SessionInfo {
                        uuid: "b".into(),
                        pane_dead: true,
                        dead_status: Some(2)
                    },
                ]
            );
        }
        // Local keeps its old rule: any no-server stderr is an empty list.
        assert!(local
            .list_sessions_from_output(Some(1), "", "no server running on /x")
            .unwrap()
            .is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_remote_ctl_does_not_start_a_local_server() {
        let ctl = remote_ctl();
        assert!(ctl.server_start_argv(false).is_empty());
        assert!(ctl.ensure_server(false).is_err());
    }

    #[test]
    fn the_remote_config_is_the_local_one_without_the_file_hook() {
        let remote = remote_conf();
        assert!(!remote.contains("pane-died"));
        assert!(!remote.contains("{events_dir}"));
        assert!(remote.contains("remain-on-exit failed"));
        assert!(remote.contains("set -s exit-empty off"));
        assert!(remote.contains("set -g prefix None"));
        assert_eq!(remote.lines().count(), TMUX_CONF.lines().count() - 1);
    }

    #[test]
    fn query_argv_builders_keep_their_shapes() {
        assert_eq!(
            TmuxCtl::list_sessions_args(),
            [
                "list-sessions",
                "-F",
                "#{session_name}\t#{pane_dead}\t#{pane_dead_status}"
            ]
        );
        assert_eq!(TmuxCtl::respawn_pane_args("u"), ["respawn-pane", "-t", "ks-u"]);
        assert_eq!(
            TmuxCtl::pane_current_path_args("u"),
            ["display-message", "-p", "-t", "ks-u", "#{pane_current_path}"]
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib tmuxctl::tests`
Expected: compile errors. `no function or associated item named remote`, `Target`, `command_argv`, `remote_conf`, and so on.

- [ ] **Step 3: Write the implementation**

In `src/tmuxctl.rs`, change the imports (`7-9`) to:

```rust
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::remote;
```

Replace the `TmuxCtl` struct (`249-257`) with:

```rust
/// Where a [`TmuxCtl`] sends its commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The private local server: its socket, the config file it is started
    /// with, and the directory the pane-died hook writes into.
    Local {
        socket: PathBuf,
        conf: PathBuf,
        events_dir: PathBuf,
    },
    /// The shared `tmux -L kabelsalat` server on a remote host, reached
    /// through that host's ssh control master. The same argv builders and
    /// parsers apply; the argv is wrapped in ssh and quoted for the remote
    /// login shell.
    Remote { dest: String, control_path: PathBuf },
}

/// Handle to a kabelsalat tmux server: the private local one, or a remote
/// host's. Local construction writes the config and creates the runtime
/// directories; a remote handle is plain data.
#[derive(Debug, Clone)]
pub struct TmuxCtl {
    target: Target,
}
```

In `impl TmuxCtl`:

1. In `with_dirs`, replace the final `Ok(Self { socket: …, conf, events_dir, })` with:

```rust
        Ok(Self {
            target: Target::Local {
                socket: runtime_dir.join("tmux.sock"),
                conf,
                events_dir,
            },
        })
```

2. Replace `events_dir` (`283-286`) with:

```rust
    /// A handle for `dest`'s shared server, through the master at
    /// `control_path` (`…/ssh/%C`, expanded by ssh itself).
    pub fn remote(dest: &str, control_path: &Path) -> Self {
        Self {
            target: Target::Remote {
                dest: dest.to_string(),
                control_path: control_path.to_path_buf(),
            },
        }
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.target, Target::Remote { .. })
    }

    /// Directory the pane-died hook writes "<uuid> <exit-code>" files into.
    /// Local only: the hook is not installed on remote servers.
    pub fn events_dir(&self) -> Option<&Path> {
        match &self.target {
            Target::Local { events_dir, .. } => Some(events_dir),
            Target::Remote { .. } => None,
        }
    }

    /// Full argv for one tmux subcommand against this target: `tmux -S <sock>
    /// <args>` locally, or the same tmux words behind `ssh … <dest> --`
    /// (quoted once more for the login shell, no tty) on a remote host.
    pub fn command_argv(&self, args: &[String]) -> Vec<String> {
        match &self.target {
            Target::Local { socket, .. } => {
                let mut argv = vec![
                    "tmux".to_string(),
                    "-S".into(),
                    socket.to_string_lossy().into_owned(),
                ];
                argv.extend(args.iter().cloned());
                argv
            }
            Target::Remote { dest, control_path } => {
                let mut remote_argv = remote::remote_tmux_prefix();
                remote_argv.extend(args.iter().cloned());
                remote::mux_argv(dest, control_path, false, &remote_argv)
            }
        }
    }

    /// A ready-to-run `Command` for `args`. Remote commands get stdin from
    /// `/dev/null`: ssh would otherwise read the app's own stdin.
    fn command(&self, args: &[String]) -> Result<Command, TmuxError> {
        let argv = self.command_argv(args);
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| TmuxError::Command("empty tmux argv".into()))?;
        let mut command = Command::new(program);
        command.args(rest);
        if self.is_remote() {
            command.stdin(Stdio::null());
        }
        Ok(command)
    }
```

3. Replace `spawn_argv` (`307-343`: signature and body; keep its existing doc comment and append the paragraph below to it, after a blank `///` line) with:

```rust
    /// On a remote target the argv is `ssh … -t <dest> -- '<tmux words>'`,
    /// and without `command` no shell-command word is passed at all, so tmux
    /// starts the remote user's default shell.
    pub fn spawn_argv(
        &self,
        uuid: &str,
        cwd: Option<&Path>,
        command: Option<&[String]>,
        env: &[(&str, &str)],
    ) -> Vec<String> {
        let mut session: Vec<String> = vec!["new-session".into(), "-A".into()];
        // -e stamps the variable into the new session's environment table
        // atomically with its creation, so the very first shell already
        // inherits it. Like -c, it takes effect only when the session is
        // created; a -A reattach ignores it (reattached sessions get an
        // explicit refresh instead).
        for (key, value) in env {
            session.push("-e".into());
            session.push(format!("{key}={value}"));
        }
        if let Some(dir) = cwd {
            session.push("-c".into());
            session.push(escape_tmux_format(&dir.to_string_lossy()));
        }
        session.push("-s".into());
        session.push(format!("{SESSION_PREFIX}{uuid}"));
        let command = command
            .filter(|command| !command.is_empty())
            .map(shell_quote_argv);
        match &self.target {
            Target::Local { socket, conf, .. } => {
                let mut argv = vec![
                    "tmux".to_string(),
                    "-S".into(),
                    socket.to_string_lossy().into_owned(),
                    "-f".into(),
                    conf.to_string_lossy().into_owned(),
                ];
                argv.extend(session);
                argv.push(
                    command.unwrap_or_else(|| {
                        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
                    }),
                );
                argv
            }
            Target::Remote { dest, control_path } => {
                let mut remote_argv = remote::remote_tmux_prefix();
                remote_argv.extend(session);
                remote_argv.extend(command);
                remote::mux_argv(dest, control_path, true, &remote_argv)
            }
        }
    }
```

4. Replace `pane_current_path` (`345-369`) with:

```rust
    /// Argv tail of the `pane_current_path` query.
    pub fn pane_current_path_args(uuid: &str) -> Vec<String> {
        vec![
            "display-message".into(),
            "-p".into(),
            "-t".into(),
            format!("{SESSION_PREFIX}{uuid}"),
            "#{pane_current_path}".into(),
        ]
    }

    /// The current working directory of a tab's tmux pane, via
    /// `display-message -p -t ks-<uuid> '#{pane_current_path}'`. An empty
    /// reply (no such pane) is a `Parse` error rather than a bogus path.
    pub fn pane_current_path(&self, uuid: &str) -> Result<PathBuf, TmuxError> {
        let output = self.command(&Self::pane_current_path_args(uuid))?.output()?;
        if !output.status.success() {
            return Err(TmuxError::Command(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            return Err(TmuxError::Parse("empty pane_current_path".into()));
        }
        Ok(PathBuf::from(path))
    }
```

5. In `server_start_argv`, replace `let tmux = vec![ … "tmux".to_string(), "-S".into(), self.socket…, "-f".into(), self.conf…, "start-server".into(), ];` with:

```rust
        // A remote server is started by its host's connect sequence
        // (remote::start_server_argv), never from here.
        let Target::Local { socket, conf, .. } = &self.target else {
            return Vec::new();
        };
        let tmux = vec![
            "tmux".to_string(),
            "-S".into(),
            socket.to_string_lossy().into_owned(),
            "-f".into(),
            conf.to_string_lossy().into_owned(),
            "start-server".into(),
        ];
```

6. Replace `ensure_server` (`408-432`) with:

```rust
    pub fn ensure_server(&self, use_systemd_run: bool) -> Result<(), TmuxError> {
        let Target::Local { socket, conf, .. } = &self.target else {
            return Err(TmuxError::Command(
                "a remote server is started by its host's connect sequence".into(),
            ));
        };
        let argv = self.server_start_argv(use_systemd_run);
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| TmuxError::Command("empty tmux argv".into()))?;
        let output = Command::new(program).args(rest).output()?;
        if !output.status.success() {
            return Err(TmuxError::Command(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        // The server survives app restarts and upgrades and only reads its
        // config at start, so re-apply it to pick up config shipped by a
        // newer binary. Live sessions are unaffected.
        let output = Command::new("tmux")
            .arg("-S")
            .arg(socket)
            .arg("source-file")
            .arg(conf)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(TmuxError::Command(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }
```

7. Replace `list_sessions`, `kill_session`, `respawn_pane`, `set_environment`, `unset_environment` and `run` (`434-523`; keep `set_environment_args` / `unset_environment_args` as they are) with:

```rust
    /// Argv tail of the session listing: name, pane-dead flag, dead status.
    pub fn list_sessions_args() -> Vec<String> {
        vec![
            "list-sessions".into(),
            "-F".into(),
            "#{session_name}\t#{pane_dead}\t#{pane_dead_status}".into(),
        ]
    }

    /// List live `ks-*` sessions with their pane-dead state.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, TmuxError> {
        let output = self.command(&Self::list_sessions_args())?.output()?;
        self.list_sessions_from_output(
            output.status.code(),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    }

    /// Interpret a finished `list-sessions`. A stopped server is an empty
    /// list, not an error. On a remote target only tmux's own exit status 1
    /// can say that: ssh's 255 (or no status at all) is the transport
    /// failing, which says nothing about the sessions — and ssh's errors can
    /// contain "No such file" too. Mistaking one for "empty" would close
    /// live tabs.
    pub fn list_sessions_from_output(
        &self,
        code: Option<i32>,
        stdout: &str,
        stderr: &str,
    ) -> Result<Vec<SessionInfo>, TmuxError> {
        if code != Some(0) {
            let from_tmux = !self.is_remote() || code == Some(1);
            if from_tmux && is_no_server_stderr(stderr) {
                return Ok(Vec::new());
            }
            if self.is_remote() && code == Some(remote::SSH_FAILED) {
                return Err(TmuxError::Command(format!("ssh: {}", stderr.trim())));
            }
            return Err(TmuxError::Command(stderr.trim().to_string()));
        }
        let mut sessions = Vec::new();
        for line in stdout.lines() {
            if let Some(session) = parse_session_line(line)? {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    /// Argv tail that kills a tab's session.
    pub fn kill_session_args(uuid: &str) -> Vec<String> {
        vec![
            "kill-session".into(),
            "-t".into(),
            format!("{SESSION_PREFIX}{uuid}"),
        ]
    }

    /// Kill the backing session of a tab (explicit tab close).
    pub fn kill_session(&self, uuid: &str) -> Result<(), TmuxError> {
        self.run(&Self::kill_session_args(uuid))
    }

    /// Argv tail that reruns the shell in a tab's dead pane.
    pub fn respawn_pane_args(uuid: &str) -> Vec<String> {
        vec![
            "respawn-pane".into(),
            "-t".into(),
            format!("{SESSION_PREFIX}{uuid}"),
        ]
    }

    /// Rerun the shell in a crashed tab's dead pane (tab restart).
    pub fn respawn_pane(&self, uuid: &str) -> Result<(), TmuxError> {
        self.run(&Self::respawn_pane_args(uuid))
    }
```

After the unchanged `set_environment_args` / `unset_environment_args`, add:

```rust
    /// Publish `key=value` into a tab session's environment table. Inherited
    /// by processes created in the session afterwards; a process already
    /// running sees it only by querying `show-environment`.
    pub fn set_environment(&self, uuid: &str, key: &str, value: &str) -> Result<(), TmuxError> {
        self.run(&Self::set_environment_args(uuid, key, value))
    }

    /// Remove `key` from a tab session's environment table.
    pub fn unset_environment(&self, uuid: &str, key: &str) -> Result<(), TmuxError> {
        self.run(&Self::unset_environment_args(uuid, key))
    }

    fn run(&self, args: &[String]) -> Result<(), TmuxError> {
        let output = self.command(args)?.output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(TmuxError::Command(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }
```

After `TMUX_CONF` (after line `247`), add:

```rust
/// The config every remote server is given (`source-file -`): `TMUX_CONF`
/// without the pane-died file hook, which writes into a *local* directory.
/// Remote crashes are read from `pane_dead` in `list-sessions` instead.
/// Shared by every kabelsalat version on the host, so changes to
/// `TMUX_CONF` must stay backward compatible.
pub fn remote_conf() -> String {
    TMUX_CONF
        .lines()
        .filter(|line| !line.starts_with("set-hook -g pane-died"))
        .map(|line| format!("{line}\n"))
        .collect()
}
```

In `src/app.rs` `init`, change the first two lines of the events-dir watch (`849-850`):

```rust
        if let Some(ctl) = &model.tmux {
            let events_dir = ctl.events_dir().to_path_buf();
```

to:

```rust
        if let Some(events_dir) = model.tmux.as_ref().and_then(|ctl| ctl.events_dir()) {
            let events_dir = events_dir.to_path_buf();
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib tmuxctl::tests`
Expected: all pass. That covers the unchanged local shape tests (`spawn_argv_*`, `server_start_*`, `live_session_roundtrip`) and the 10 new ones.

Run: `cargo fmt && cargo clippy --all-targets && cargo test`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/tmuxctl.rs src/app.rs
git commit -m "Give TmuxCtl a local or remote target sharing one set of argv builders"
```

---

### Task 8: CLI — remote group targets

**Depends on:** Task 7. It touches `src/app.rs` (the `SpawnCommand` cwd type and the published `GroupInfo`), so it runs after Task 7.

**Files:**
- Modify `src/cli.rs`:
  - `GroupInfo` (`53-60`)
  - `Action` (`256-267`)
  - `help_text` (`99-121`)
  - `dispatch` `Cli::Run` arm (`312-343`)
  - tests: `sample_groups` (`506-529`), `a_uuid_beats_a_name_that_collides_with_it` (`568-582`), `spawn_of` (`615-620`), `run_produces_a_spawn_request_with_the_callers_cwd` (`641-649`), `an_absolute_cwd_flag_replaces_the_callers_cwd` (`652-656`), `a_relative_cwd_flag_is_resolved_against_the_caller` (`659-663`), plus new tests
- Modify `src/control.rs`: `request_spawn` (`62-80`), test `a_spawn_request_without_a_gui_is_refused` (`175-184`).
- Modify `src/app.rs`:
  - `Msg::SpawnCommand` (`438-443`)
  - its handler's `let cwd = cwd.is_dir().then_some(cwd);` (`1216`)
  - `save_state`'s `GroupInfo` literal (`1587-1591`; `1589-1593` after Task 1)

**Interfaces:**
- Consumes (Task 1): `SavedGroup::host`.
- Produces (used by Task 15):
  - `pub struct GroupInfo { pub uuid: String, pub name: String, pub tabs: usize, pub host: Option<String> }`
  - `Action::Spawn { group: GroupTarget, cwd: Option<PathBuf>, argv: Vec<String> }`. `cwd` is `None` only for a remote group without `--cwd` (the remote home).
  - `control::request_spawn(group: GroupTarget, cwd: Option<PathBuf>, argv: Vec<String>, tab_uuid: String) -> bool`
  - `Msg::SpawnCommand { group, tab_uuid, cwd: Option<PathBuf>, argv }`

- [ ] **Step 1: Write the failing tests**

In `src/cli.rs` `mod tests`:
- add `host: None,` as the last field of every `GroupInfo { … }` literal: four in `sample_groups`, two in `a_uuid_beats_a_name_that_collides_with_it`
- change `spawn_of` to return the optional cwd:

```rust
    fn spawn_of(out: &Outcome) -> (&GroupTarget, &Option<PathBuf>, &Vec<String>) {
        match out.action.as_ref().expect("an action") {
            Action::Spawn { group, cwd, argv } => (group, cwd, argv),
            other => panic!("expected Spawn, got {other:?}"),
        }
    }
```

Then update the three local-cwd assertions to `Some(…)`:
- in `run_produces_a_spawn_request_with_the_callers_cwd`: `assert_eq!(*cwd, Some(PathBuf::from("/home/u/proj")));`
- in `an_absolute_cwd_flag_replaces_the_callers_cwd`: `assert_eq!(*spawn_of(&out).1, Some(PathBuf::from("/srv/app")));`
- in `a_relative_cwd_flag_is_resolved_against_the_caller`: `assert_eq!(*spawn_of(&out).1, Some(PathBuf::from("/home/u/proj/sub/dir")));`

Append:

```rust
    // --- remote groups ---

    fn remote_groups() -> Vec<GroupInfo> {
        vec![
            GroupInfo {
                uuid: "rrr-555".into(),
                name: "box".into(),
                tabs: 1,
                host: Some("me@box".into()),
            },
            GroupInfo {
                uuid: "lll-666".into(),
                name: "here".into(),
                tabs: 1,
                host: None,
            },
        ]
    }

    #[test]
    fn run_on_a_remote_group_ignores_the_callers_cwd() {
        let out = dispatch(
            &run("box", None, &["ls"]),
            &remote_groups(),
            Path::new("/home/u/proj"),
        );
        let (group, cwd, argv) = spawn_of(&out);
        assert_eq!(*group, GroupTarget::Existing("rrr-555".into()));
        // None = the remote home; the local directory means nothing there.
        assert_eq!(*cwd, None);
        assert_eq!(*argv, vec!["ls".to_string()]);
    }

    #[test]
    fn run_on_a_remote_group_passes_cwd_through_unchecked() {
        // Relative stays relative (to the remote home), absolute stays as
        // typed; neither is joined onto the caller's directory.
        for dir in ["src/app", "/srv/app"] {
            let out = dispatch(
                &run("box", Some(dir), &["ls"]),
                &remote_groups(),
                Path::new("/home/u/proj"),
            );
            assert_eq!(*spawn_of(&out).1, Some(PathBuf::from(dir)));
        }
    }

    #[test]
    fn run_on_a_local_group_next_to_a_remote_one_is_unchanged() {
        let out = dispatch(
            &run("here", None, &["ls"]),
            &remote_groups(),
            Path::new("/home/u/proj"),
        );
        assert_eq!(*spawn_of(&out).1, Some(PathBuf::from("/home/u/proj")));
    }

    #[test]
    fn create_still_makes_a_local_group() {
        let out = dispatch(
            &run_create("fresh", &["ls"]),
            &remote_groups(),
            Path::new("/w"),
        );
        let (group, cwd, _) = spawn_of(&out);
        assert_eq!(
            *group,
            GroupTarget::Create {
                name: "fresh".into()
            }
        );
        assert_eq!(*cwd, Some(PathBuf::from("/w")));
    }

    #[test]
    fn help_explains_cwd_on_remote_groups() {
        assert!(help_text().contains("remote"));
    }
```

In `src/control.rs` test `a_spawn_request_without_a_gui_is_refused`, change `std::path::PathBuf::from("/tmp"),` to `Some(std::path::PathBuf::from("/tmp")),`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cli::tests`
Expected: compile errors. `struct GroupInfo has no field named host`; mismatched types `Option<PathBuf>` vs `PathBuf`.

- [ ] **Step 3: Write the implementation**

In `src/cli.rs`, replace `GroupInfo` (`53-60`) with:

```rust
/// One group as the CLI sees it: the stable uuid, the (possibly empty,
/// possibly duplicated) name, how many tabs it holds, and — for a remote
/// group — the ssh destination its tabs run on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInfo {
    pub uuid: String,
    pub name: String,
    pub tabs: usize,
    pub host: Option<String>,
}
```

In `Action`, change the `Spawn` variant to:

```rust
    Spawn {
        group: GroupTarget,
        /// Where the tab starts. Always `Some` for a local group (the
        /// caller's directory, or `--cwd` resolved against it). For a remote
        /// group it is `--cwd` verbatim — a path on that host, never checked
        /// here — and `None` (the remote home) without it.
        cwd: Option<PathBuf>,
        argv: Vec<String>,
    },
```

In `help_text`, replace the `--cwd` line:

```
      --cwd <dir>           Working directory (default: the caller's).
```

with:

```
      --cwd <dir>           Working directory (default: the caller's). For a
                            remote group, a path on its host (default: the
                            remote home).
```

In `dispatch`, replace the whole `Cli::Run { … } => { … }` arm (`312-343`) with:

```rust
        Cli::Run {
            group,
            create,
            cwd,
            argv,
        } => {
            let (target, host) = match resolve_group(groups, group) {
                Ok(found) => (
                    GroupTarget::Existing(found.uuid.clone()),
                    found.host.as_deref(),
                ),
                // A selector that looks like a uuid is not special-cased: with
                // --create it simply becomes the new group's name. `--create`
                // only ever makes local groups.
                Err(ResolveError::NotFound) if *create => (
                    GroupTarget::Create {
                        name: group.clone(),
                    },
                    None,
                ),
                Err(err) => return resolve_failure(group, err),
            };
            let cwd = match host {
                // A remote tab runs on its host, where the caller's directory
                // means nothing: --cwd passes through verbatim, and without it
                // the shell starts in the remote home.
                Some(_) => cwd.clone(),
                // `join` with an absolute path replaces the base, so this
                // handles both absolute and relative --cwd values.
                None => Some(match cwd {
                    Some(dir) => caller_cwd.join(dir),
                    None => caller_cwd.to_path_buf(),
                }),
            };
            Outcome {
                stdout: String::new(),
                stderr: String::new(),
                code: EXIT_OK,
                action: Some(Action::Spawn {
                    group: target,
                    cwd,
                    argv: argv.clone(),
                }),
            }
        }
```

In `src/control.rs`, change `request_spawn`'s `cwd: std::path::PathBuf,` parameter to `cwd: Option<std::path::PathBuf>,`. Nothing else in it changes.

In `src/app.rs`:
- in `Msg::SpawnCommand`, change `cwd: PathBuf,` to `cwd: Option<PathBuf>,`
- in its handler, replace `let cwd = cwd.is_dir().then_some(cwd);` with `let cwd = cwd.filter(|dir| dir.is_dir());`. Task 15 exempts remote groups from this check.
- in `save_state`'s `crate::cli::GroupInfo { … }` literal, add `host: g.host.clone(),` after the `tabs:` line (`g` is the `SavedGroup`, whose `host` Task 10 fills from the model).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib cli::tests && cargo test --lib control::tests` (cargo takes one filter per run)
Expected: all pass (42 in `cli::tests`, 5 of them new; 3 in `control::tests`).

Run: `cargo fmt && cargo clippy --all-targets && cargo test`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs src/control.rs src/app.rs
git commit -m "Let kabelsalat run target remote groups without a local cwd"
```

---
### Task 9: `remote_worker.rs` — detached ssh runner and the per-host worker thread

**Depends on:** Task 7 (uses `TmuxCtl::remote`, `command_argv`, `list_sessions_from_output`, `remote_conf`). Can run in parallel with Task 8: it touches only the new file and `src/lib.rs`'s module list.

**Files:**
- Create: `src/remote_worker.rs`
- Modify: `src/lib.rs` (module list, after `pub mod remote;`)

**Interfaces:**
- Consumes:
  - Tasks 3–6: `remote::{SshAvailability, ssh_availability, AuthMode, askpass_env, classify, RemoteError, master_argv, mux_argv, control_argv, ControlOp, start_server_argv, upload_conf_argv, SSH_FAILED}`
  - Task 7: `tmuxctl::{TmuxCtl, SessionInfo, TmuxVersion, remote_conf}`
  - existing: `state::state_dir() -> PathBuf` (`src/state.rs`)
- Produces (used by Tasks 11, 12):
  - `pub fn detect_ssh() -> SshAvailability`
  - `pub fn is_executable(path: &Path) -> bool`
  - `pub fn control_path() -> std::io::Result<PathBuf>` (creates `…/kabelsalat/ssh` with mode 0700, returns `…/ssh/%C`)
  - `pub struct Captured { pub code: Option<i32>, pub stdout: String, pub stderr: String }` (`Debug, Clone, Default, PartialEq, Eq`)
  - `pub fn run_detached(argv: &[String], env: &[(String, String)], stdin: Option<&[u8]>) -> std::io::Result<Captured>`
  - `pub trait Runner: Send + 'static { fn run(&mut self, argv: &[String], env: &[(String, String)], stdin: Option<&[u8]>) -> std::io::Result<Captured>; }`
  - `pub struct SshRunner;` (`impl Runner` via `run_detached`)
  - `pub enum RemoteEvent { Connected(Vec<SessionInfo>), ConnectFailed(RemoteError), MasterDead, TabSessions { tab: usize, result: Result<Vec<SessionInfo>, String> }, Killed(Vec<String>) }` (`Debug, Clone, PartialEq`)
  - `pub struct RemoteWorker`, with:
    - `pub fn spawn<R: Runner>(dest: String, control_path: PathBuf, auth: AuthMode, runner: R, notify: impl Fn(RemoteEvent) + Send + 'static) -> std::io::Result<Self>`
    - `pub fn connect(&self)`
    - `pub fn child_exited(&self, tab: usize, ssh_failed: bool)`
    - `pub fn kill(&self, uuids: Vec<String>)`
    - `pub fn respawn(&self, uuid: String)`
    - `pub fn pane_current_path(&self, uuid: &str, wait: Duration) -> Option<String>`
    - `pub fn exit_master(&self)`

- [ ] **Step 1: Verify `source-file -` on tmux 3.2 (spec's open check)**

Run: `grep -n 'support "-" for standard input' /usr/share/doc/tmux/CHANGES && grep -n '^CHANGES FROM 3.0a TO 3.1\|^CHANGES FROM 3.0 TO 3.0a' /usr/share/doc/tmux/CHANGES`
Expected: the file lists releases newest first, so the "support "-" for standard input" line number falls between the `3.0a TO 3.1` header and the `3.0 TO 3.0a` header below it: the feature shipped in **tmux 3.1**. It is in every tmux ≥ 3.2. (Checked while writing this plan against tmux 3.7c's CHANGES: line 1434–1435, inside "3.0a to 3.1", which spans lines 1380–1513.)

Optional live check against a real 3.2 (Ubuntu 22.04 ships 3.2a):

```sh
podman run --rm docker.io/library/ubuntu:22.04 sh -c '
  apt-get update -qq && apt-get install -y -qq tmux >/dev/null && tmux -V &&
  printf "set -s exit-empty off\nset -g status off\n" |
    tmux -L t -f /dev/null start-server \; source-file - &&
  tmux -L t show -g status'
```

Expected: `tmux 3.2a`, then `status off`.

The code in Step 4 still includes the upload fallback (`remote::upload_conf_argv`, stdin to `~/.local/state/kabelsalat/tmux.conf`). It covers a patched or broken tmux that refuses `source-file -`, and `connect_uploads_the_config_when_stdin_sourcing_fails` below tests it.

- [ ] **Step 2: Write the failing tests**

Create `src/remote_worker.rs` containing only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// One recorded command.
    #[derive(Debug, Clone)]
    struct Call {
        argv: Vec<String>,
        env: Vec<(String, String)>,
        stdin: Option<Vec<u8>>,
    }

    /// Scripted runner: replies in order (then success with no output) and
    /// records every call.
    struct Fake {
        replies: VecDeque<Captured>,
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl Runner for Fake {
        fn run(
            &mut self,
            argv: &[String],
            env: &[(String, String)],
            stdin: Option<&[u8]>,
        ) -> std::io::Result<Captured> {
            self.calls.lock().unwrap().push(Call {
                argv: argv.to_vec(),
                env: env.to_vec(),
                stdin: stdin.map(<[u8]>::to_vec),
            });
            Ok(self.replies.pop_front().unwrap_or(Captured {
                code: Some(0),
                ..Captured::default()
            }))
        }
    }

    fn reply(code: i32, stdout: &str, stderr: &str) -> Captured {
        Captured {
            code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    fn host(auth: AuthMode, replies: Vec<Captured>) -> (HostSession<Fake>, Arc<Mutex<Vec<Call>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = Fake {
            replies: replies.into(),
            calls: calls.clone(),
        };
        (
            HostSession::new(
                "me@box".into(),
                Path::new("/run/ks/ssh/%C").to_path_buf(),
                auth,
                runner,
            ),
            calls,
        )
    }

    fn last_word(call: &Call) -> &str {
        call.argv.last().map(String::as_str).unwrap_or_default()
    }

    fn sessions() -> Vec<SessionInfo> {
        vec![
            SessionInfo {
                uuid: "a".into(),
                pane_dead: false,
                dead_status: None,
            },
            SessionInfo {
                uuid: "b".into(),
                pane_dead: true,
                dead_status: Some(2),
            },
        ]
    }

    #[test]
    fn connect_logs_in_checks_tmux_configures_and_lists() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![
                reply(0, "tmux 3.4\n", ""),
                reply(0, "", ""),
                reply(0, "ks-a\t0\t\nks-b\t1\t2\n", ""),
            ],
        );
        assert_eq!(host.run_job(Job::Connect), Some(RemoteEvent::Connected(sessions())));
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        // 1: the only authenticating command, the master, running tmux -V.
        assert!(calls[0].argv.contains(&"ControlMaster=auto".to_string()));
        assert!(calls[0].argv.contains(&"BatchMode=yes".to_string()));
        assert_eq!(last_word(&calls[0]), "tmux -V");
        assert!(calls[0].stdin.is_none());
        // 2: start + configure over the master, config on stdin.
        assert!(calls[1].argv.contains(&"ControlMaster=no".to_string()));
        assert!(last_word(&calls[1]).contains("source-file -"));
        assert_eq!(calls[1].stdin.as_deref(), Some(tmuxctl::remote_conf().as_bytes()));
        // 3: list-sessions over the master.
        assert!(last_word(&calls[2]).contains("list-sessions"));
    }

    #[test]
    fn askpass_env_goes_to_the_master_only() {
        let askpass = AuthMode::Askpass("/usr/bin/ksshaskpass".into());
        let (mut host, calls) = host(
            askpass,
            vec![reply(0, "tmux 3.2\n", ""), reply(0, "", ""), reply(0, "", "")],
        );
        assert_eq!(host.run_job(Job::Connect), Some(RemoteEvent::Connected(Vec::new())));
        let calls = calls.lock().unwrap();
        assert!(calls[0]
            .env
            .contains(&("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string())));
        assert!(calls[1].env.is_empty());
        assert!(calls[2].env.is_empty());
    }

    #[test]
    fn connect_refuses_tmux_older_than_3_2_and_drops_the_master() {
        let (mut host, calls) = host(AuthMode::BatchOnly, vec![reply(0, "tmux 3.1c\n", "")]);
        assert_eq!(
            host.run_job(Job::Connect),
            Some(RemoteEvent::ConnectFailed(RemoteError::TmuxTooOld("3.1c".into())))
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(&calls[1].argv[3..5], ["-O", "exit"]);
    }

    #[test]
    fn connect_refuses_a_host_without_tmux() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![reply(127, "", "bash: line 1: tmux: command not found\n")],
        );
        assert_eq!(
            host.run_job(Job::Connect),
            Some(RemoteEvent::ConnectFailed(RemoteError::TmuxMissing))
        );
        assert_eq!(&calls.lock().unwrap()[1].argv[3..5], ["-O", "exit"]);
    }

    #[test]
    fn a_failed_login_stops_before_any_tmux_call() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![reply(255, "", "me@box: Permission denied (publickey,password).\r\n")],
        );
        assert_eq!(
            host.run_job(Job::Connect),
            Some(RemoteEvent::ConnectFailed(RemoteError::AuthNeedsAskpass))
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn connect_uploads_the_config_when_stdin_sourcing_fails() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![
                reply(0, "tmux 3.2a\n", ""),
                reply(1, "", "usage: source-file [-Fnqv] [-t target-pane] path ...\n"),
                reply(0, "", ""),
                reply(0, "", ""),
            ],
        );
        assert_eq!(host.run_job(Job::Connect), Some(RemoteEvent::Connected(Vec::new())));
        let calls = calls.lock().unwrap();
        assert!(last_word(&calls[2]).contains("cat >"));
        assert_eq!(calls[2].stdin.as_deref(), Some(tmuxctl::remote_conf().as_bytes()));
        assert!(last_word(&calls[3]).contains("list-sessions"));
    }

    #[test]
    fn an_ssh_failure_while_configuring_is_not_retried_as_an_upload() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![
                reply(0, "tmux 3.3a\n", ""),
                reply(255, "", "ssh: connect to host box port 22: Connection refused\r\n"),
            ],
        );
        assert_eq!(
            host.run_job(Job::Connect),
            Some(RemoteEvent::ConnectFailed(RemoteError::Unreachable))
        );
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_255_exit_with_a_dead_master_reports_master_dead() {
        let (mut host, calls) = host(AuthMode::BatchOnly, vec![reply(255, "", "")]);
        assert_eq!(
            host.run_job(Job::ChildExited {
                tab: 7,
                ssh_failed: true
            }),
            Some(RemoteEvent::MasterDead)
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(&calls[0].argv[3..5], ["-O", "check"]);
    }

    #[test]
    fn a_255_exit_with_a_live_master_lists_sessions() {
        let (mut host, _) = host(
            AuthMode::BatchOnly,
            vec![reply(0, "", ""), reply(0, "ks-a\t0\t\n", "")],
        );
        assert_eq!(
            host.run_job(Job::ChildExited {
                tab: 7,
                ssh_failed: true
            }),
            Some(RemoteEvent::TabSessions {
                tab: 7,
                result: Ok(vec![sessions()[0].clone()])
            })
        );
    }

    #[test]
    fn any_other_exit_lists_sessions_without_a_check() {
        let (mut host, calls) = host(AuthMode::BatchOnly, vec![reply(1, "", "no server running on /tmp/tmux-1000/kabelsalat\n")]);
        assert_eq!(
            host.run_job(Job::ChildExited {
                tab: 3,
                ssh_failed: false
            }),
            Some(RemoteEvent::TabSessions {
                tab: 3,
                result: Ok(Vec::new())
            })
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_ssh_failure_during_a_listing_is_never_an_empty_list() {
        let (mut host, _) = host(
            AuthMode::BatchOnly,
            vec![reply(255, "", "Control socket connect(/x): No such file or directory\r\n")],
        );
        let Some(RemoteEvent::TabSessions { result, .. }) = host.run_job(Job::ChildExited {
            tab: 1,
            ssh_failed: false,
        }) else {
            panic!("expected TabSessions");
        };
        assert!(result.is_err());
    }

    #[test]
    fn kill_reports_killed_and_already_gone_but_not_ssh_failures() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![
                reply(0, "", ""),
                reply(1, "", "can't find session: ks-gone\n"),
                reply(255, "", "mux_client_request_session: read from master failed\r\n"),
            ],
        );
        assert_eq!(
            host.run_job(Job::Kill(vec!["live".into(), "gone".into(), "lost".into()])),
            Some(RemoteEvent::Killed(vec!["live".into(), "gone".into()]))
        );
        assert!(last_word(&calls.lock().unwrap()[0]).contains("kill-session -t ks-live"));
    }

    #[test]
    fn the_pane_path_is_answered_on_the_reply_channel() {
        let (mut host, _) = host(
            AuthMode::BatchOnly,
            vec![reply(0, "/home/me/proj\n", ""), reply(1, "", "can't find pane\n")],
        );
        let (reply_to, answer) = std::sync::mpsc::channel();
        assert_eq!(
            host.run_job(Job::PanePath {
                uuid: "a".into(),
                reply: reply_to.clone()
            }),
            None
        );
        assert_eq!(answer.recv().unwrap(), Some("/home/me/proj".to_string()));
        host.run_job(Job::PanePath {
            uuid: "a".into(),
            reply: reply_to,
        });
        assert_eq!(answer.recv().unwrap(), None);
    }

    #[test]
    fn run_detached_captures_output_status_and_stdin() {
        let out = run_detached(
            &["sh".to_string(), "-c".to_string(), "cat; echo err >&2; exit 3".to_string()],
            &[("KS_TEST".to_string(), "1".to_string())],
            Some(b"hello"),
        )
        .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.stderr, "err\n");
    }

    #[test]
    fn run_detached_has_no_controlling_terminal() {
        // setsid(): the child leads its own session, so /dev/tty cannot open.
        let out = run_detached(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "if sh -c ': </dev/tty' 2>/dev/null; then echo tty; else echo none; fi".to_string(),
            ],
            &[],
            None,
        )
        .unwrap();
        assert_eq!(out.stdout.trim(), "none");
    }

    #[test]
    fn run_detached_rejects_an_empty_argv() {
        assert!(run_detached(&[], &[], None).is_err());
    }
}
```

In `src/lib.rs`, add `pub mod remote_worker;` directly after `pub mod remote;`.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib remote_worker::tests`
Expected: compile errors. `cannot find type Captured`, `HostSession`, `Job`, `Runner`, `run_detached`, and so on.

- [ ] **Step 4: Write the implementation**

Put this above the test module in `src/remote_worker.rs`:

```rust
//! The I/O side of remote groups: the local ssh check, a detached process
//! runner, and one worker thread per remote host.
//!
//! A worker owns its host's ssh master and runs that host's remote tmux calls
//! strictly one after another (so `new-session` always precedes whatever
//! follows it). Results go back to the GUI through a callback that sends a
//! `Msg`, like the linger and profile-sweep threads do. Nothing in here
//! panics; failures become `RemoteEvent`s.

use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::remote::{self, AuthMode, ControlOp, RemoteError, SshAvailability};
use crate::tmuxctl::{self, SessionInfo, TmuxCtl, TmuxVersion};

/// How long to wait for a pipe's end after the process exited. A master
/// forked by `ControlPersist` redirects its stdio to /dev/null when it
/// detaches (verified with OpenSSH 10.2), so this is only a safety net for
/// a client that keeps an inherited pipe open; the output read so far is
/// used then.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Run `ssh -V` once (stdin from /dev/null) and classify the client.
pub fn detect_ssh() -> SshAvailability {
    match Command::new("ssh").arg("-V").stdin(Stdio::null()).output() {
        // ssh prints its version on stderr.
        Ok(out) => remote::ssh_availability(Some(&String::from_utf8_lossy(&out.stderr))),
        Err(_) => remote::ssh_availability(None),
    }
}

/// A regular file with any execute bit set.
pub fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// `$XDG_RUNTIME_DIR/kabelsalat/ssh/%C` (falling back to the state
/// directory, like the local tmux socket). The directory is created 0700;
/// ssh expands `%C` to a hash of the connection, which keeps the socket path
/// under the Unix socket length limit.
pub fn control_path() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join("kabelsalat"))
        .unwrap_or_else(crate::state::state_dir);
    let dir = base.join("ssh");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    Ok(dir.join("%C"))
}

/// A finished process: exit code (`None` if killed by a signal) and output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Captured {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run `argv` with extra `env`, detached from any terminal: stdin is
/// `/dev/null` (or a pipe carrying `stdin`), and the child calls `setsid()`
/// before exec, so it has no controlling terminal and ssh can never prompt
/// on the terminal kabelsalat was started from, whatever its version.
pub fn run_detached(
    argv: &[String],
    env: &[(String, String)],
    stdin: Option<&[u8]>,
) -> io::Result<Captured> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().map(|(key, value)| (key.as_str(), value.as_str())))
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: the closure runs in the forked child before exec. It only calls
    // setsid(2), which is async-signal-safe, and builds an io::Error from the
    // raw errno, which does not allocate.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let stdout = Drain::start(child.stdout.take());
    let stderr = Drain::start(child.stderr.take());
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A failed write means the child is already gone; its status says why.
        let _ = pipe.write_all(data);
        // Dropping `pipe` closes it: the child sees end of input.
    }
    let status = child.wait()?;
    Ok(Captured {
        code: status.code(),
        stdout: stdout.finish(),
        stderr: stderr.finish(),
    })
}

/// Reads one pipe to its end on a helper thread, so a full stderr can never
/// block the child, and a pipe held open by a backgrounded master cannot
/// block us.
struct Drain {
    bytes: Arc<Mutex<Vec<u8>>>,
    done: mpsc::Receiver<()>,
}

impl Drain {
    fn start<R: Read + Send + 'static>(pipe: Option<R>) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (finished, done) = mpsc::channel();
        match pipe {
            Some(mut pipe) => {
                let sink = bytes.clone();
                // If no reader thread can be started, `finished` is dropped
                // with the closure: nothing is captured and nobody waits.
                let _ = std::thread::Builder::new()
                    .name("kabelsalat-pipe".into())
                    .spawn(move || {
                        let mut chunk = [0u8; 4096];
                        loop {
                            match pipe.read(&mut chunk) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if let Ok(mut buf) = sink.lock() {
                                        buf.extend_from_slice(&chunk[..n]);
                                    }
                                }
                            }
                        }
                        let _ = finished.send(());
                    });
            }
            None => {
                let _ = finished.send(());
            }
        }
        Self { bytes, done }
    }

    fn finish(self) -> String {
        let _ = self.done.recv_timeout(DRAIN_GRACE);
        let bytes = self.bytes.lock().map(|buf| buf.clone()).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Runs one command for a worker. The real one is [`SshRunner`]; tests
/// script the replies.
pub trait Runner: Send + 'static {
    fn run(
        &mut self,
        argv: &[String],
        env: &[(String, String)],
        stdin: Option<&[u8]>,
    ) -> io::Result<Captured>;
}

/// [`run_detached`] as a [`Runner`].
pub struct SshRunner;

impl Runner for SshRunner {
    fn run(
        &mut self,
        argv: &[String],
        env: &[(String, String)],
        stdin: Option<&[u8]>,
    ) -> io::Result<Captured> {
        run_detached(argv, env, stdin)
    }
}

/// What a worker reports back to the GUI.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteEvent {
    /// Logged in, tmux checked and configured; these are the live sessions.
    Connected(Vec<SessionInfo>),
    ConnectFailed(RemoteError),
    /// A tab's ssh exited 255 and `ssh -O check` found the master gone.
    MasterDead,
    /// The listing a tab's client exit asked for (`tab` is the tab id).
    /// `Err` means liveness is unknown — never "gone".
    TabSessions {
        tab: usize,
        result: Result<Vec<SessionInfo>, String>,
    },
    /// These queued kills are done: killed, or already gone.
    Killed(Vec<String>),
}

/// One unit of work, run in arrival order.
#[derive(Debug)]
enum Job {
    Connect,
    ChildExited {
        tab: usize,
        ssh_failed: bool,
    },
    Kill(Vec<String>),
    Respawn(String),
    PanePath {
        uuid: String,
        reply: mpsc::Sender<Option<String>>,
    },
}

/// A worker's state and the jobs it runs; lives on the worker thread.
struct HostSession<R: Runner> {
    dest: String,
    control_path: PathBuf,
    auth: AuthMode,
    runner: R,
    ctl: TmuxCtl,
}

impl<R: Runner> HostSession<R> {
    fn new(dest: String, control_path: PathBuf, auth: AuthMode, runner: R) -> Self {
        let ctl = TmuxCtl::remote(&dest, &control_path);
        Self {
            dest,
            control_path,
            auth,
            runner,
            ctl,
        }
    }

    fn run_job(&mut self, job: Job) -> Option<RemoteEvent> {
        match job {
            Job::Connect => Some(self.connect()),
            Job::ChildExited { tab, ssh_failed } => Some(self.child_exited(tab, ssh_failed)),
            Job::Kill(uuids) => Some(self.kill(uuids)),
            Job::Respawn(uuid) => {
                self.respawn(&uuid);
                None
            }
            Job::PanePath { uuid, reply } => {
                // The asker may have stopped waiting; that is fine.
                let _ = reply.send(self.pane_path(&uuid));
                None
            }
        }
    }

    /// Run one command; a process that cannot even start reads as a failure
    /// with no status and the error text as stderr.
    fn run(&mut self, argv: &[String], env: &[(String, String)], stdin: Option<&[u8]>) -> Captured {
        self.runner
            .run(argv, env, stdin)
            .unwrap_or_else(|err| Captured {
                code: None,
                stdout: String::new(),
                stderr: err.to_string(),
            })
    }

    /// The host check over a fresh (or reused) master: `tmux -V`, then start
    /// and configure the server, then list its sessions.
    fn connect(&mut self) -> RemoteEvent {
        let argv = remote::master_argv(
            &self.dest,
            &self.control_path,
            &self.auth,
            &["tmux".to_string(), "-V".to_string()],
        );
        let env = remote::askpass_env(&self.auth);
        let version = self.run(&argv, &env, None);
        if version.code != Some(0) {
            let err = remote::classify(version.code, &version.stderr, &self.auth);
            if err == RemoteError::TmuxMissing {
                self.exit_master();
            }
            return RemoteEvent::ConnectFailed(err);
        }
        match TmuxVersion::parse(&version.stdout) {
            Some(found) if found.meets_minimum() => {}
            Some(found) => {
                self.exit_master();
                return RemoteEvent::ConnectFailed(RemoteError::TmuxTooOld(found.to_string()));
            }
            None => {
                self.exit_master();
                return RemoteEvent::ConnectFailed(RemoteError::TmuxMissing);
            }
        }
        if let Err(err) = self.start_server() {
            return RemoteEvent::ConnectFailed(err);
        }
        match self.list_sessions() {
            Ok(sessions) => RemoteEvent::Connected(sessions),
            Err(detail) => RemoteEvent::ConnectFailed(RemoteError::Other(detail)),
        }
    }

    /// `start-server ; source-file -` with the remote config on stdin. If
    /// tmux itself refuses (every tmux since 3.1 reads `-` as stdin, but a
    /// patched build might not), upload the file and source it from disk.
    fn start_server(&mut self) -> Result<(), RemoteError> {
        let conf = tmuxctl::remote_conf();
        let argv = remote::mux_argv(
            &self.dest,
            &self.control_path,
            false,
            &remote::start_server_argv(),
        );
        let direct = self.run(&argv, &[], Some(conf.as_bytes()));
        if direct.code == Some(0) {
            return Ok(());
        }
        if direct.code == Some(remote::SSH_FAILED) || direct.code.is_none() {
            return Err(remote::classify(direct.code, &direct.stderr, &self.auth));
        }
        let argv = remote::mux_argv(
            &self.dest,
            &self.control_path,
            false,
            &remote::upload_conf_argv(),
        );
        let upload = self.run(&argv, &[], Some(conf.as_bytes()));
        if upload.code == Some(0) {
            Ok(())
        } else {
            Err(RemoteError::Other(format!(
                "could not configure tmux: {}",
                upload.stderr.trim()
            )))
        }
    }

    fn list_sessions(&mut self) -> Result<Vec<SessionInfo>, String> {
        let argv = self.ctl.command_argv(&TmuxCtl::list_sessions_args());
        let out = self.run(&argv, &[], None);
        self.ctl
            .list_sessions_from_output(out.code, &out.stdout, &out.stderr)
            .map_err(|err| err.to_string())
    }

    /// A tab's client exited. 255 is ssh failing: if the master is gone too,
    /// the whole host is down. Otherwise the listing decides (close or
    /// reattach) — asynchronously, never on the GUI thread.
    fn child_exited(&mut self, tab: usize, ssh_failed: bool) -> RemoteEvent {
        if ssh_failed {
            let argv = remote::control_argv(&self.dest, &self.control_path, ControlOp::Check);
            if self.run(&argv, &[], None).code != Some(0) {
                return RemoteEvent::MasterDead;
            }
        }
        RemoteEvent::TabSessions {
            tab,
            result: self.list_sessions(),
        }
    }

    /// Kill each session. Done means tmux answered: 0 (killed) or any other
    /// tmux status (no such session, no server — already gone). ssh's 255 or
    /// no status leaves the uuid queued for the next connect.
    fn kill(&mut self, uuids: Vec<String>) -> RemoteEvent {
        let mut done = Vec::new();
        for uuid in uuids {
            let argv = self.ctl.command_argv(&TmuxCtl::kill_session_args(&uuid));
            let out = self.run(&argv, &[], None);
            if out.code.is_some() && out.code != Some(remote::SSH_FAILED) {
                done.push(uuid);
            }
        }
        RemoteEvent::Killed(done)
    }

    fn respawn(&mut self, uuid: &str) {
        let argv = self.ctl.command_argv(&TmuxCtl::respawn_pane_args(uuid));
        let out = self.run(&argv, &[], None);
        if out.code != Some(0) {
            eprintln!(
                "kabelsalat: respawn-pane for {uuid} on {}: {}",
                self.dest,
                out.stderr.trim()
            );
        }
    }

    /// The empty check is load-bearing: on tmux 3.7c `display-message -p -t
    /// <missing session>` exits 0 with empty output rather than failing.
    fn pane_path(&mut self, uuid: &str) -> Option<String> {
        let argv = self.ctl.command_argv(&TmuxCtl::pane_current_path_args(uuid));
        let out = self.run(&argv, &[], None);
        let path = out.stdout.trim();
        (out.code == Some(0) && !path.is_empty()).then(|| path.to_string())
    }

    fn exit_master(&mut self) {
        let argv = remote::control_argv(&self.dest, &self.control_path, ControlOp::Exit);
        let _ = self.run(&argv, &[], None);
    }
}

/// Handle to one host's worker thread. Dropping it ends the thread once its
/// queue is empty.
pub struct RemoteWorker {
    jobs: mpsc::Sender<Job>,
    dest: String,
    control_path: PathBuf,
}

impl RemoteWorker {
    /// Start the worker for `dest`. `notify` runs on the worker thread for
    /// every event; it should only forward (e.g. send a `Msg`).
    pub fn spawn<R: Runner>(
        dest: String,
        control_path: PathBuf,
        auth: AuthMode,
        runner: R,
        notify: impl Fn(RemoteEvent) + Send + 'static,
    ) -> io::Result<Self> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let mut session = HostSession::new(dest.clone(), control_path.clone(), auth, runner);
        std::thread::Builder::new()
            // Not named after `dest`: a thread name must not contain NUL,
            // and a hand-edited state file is not validated.
            .name("kabelsalat-ssh".into())
            .spawn(move || {
                while let Ok(job) = queue.recv() {
                    if let Some(event) = session.run_job(job) {
                        notify(event);
                    }
                }
            })?;
        Ok(Self {
            jobs,
            dest,
            control_path,
        })
    }

    /// Log in (once per host: this is the only authenticating command),
    /// check tmux, configure the server and list its sessions.
    pub fn connect(&self) {
        let _ = self.jobs.send(Job::Connect);
    }

    /// A tab's client exited; `ssh_failed` when its status was ssh's 255.
    pub fn child_exited(&self, tab: usize, ssh_failed: bool) {
        let _ = self.jobs.send(Job::ChildExited { tab, ssh_failed });
    }

    pub fn kill(&self, uuids: Vec<String>) {
        let _ = self.jobs.send(Job::Kill(uuids));
    }

    pub fn respawn(&self, uuid: String) {
        let _ = self.jobs.send(Job::Respawn(uuid));
    }

    /// The pane's directory on the host, waiting at most `wait` (the queue
    /// is serial, so a busy worker simply times out). Blocks the caller.
    pub fn pane_current_path(&self, uuid: &str, wait: Duration) -> Option<String> {
        let (reply, answer) = mpsc::channel();
        self.jobs
            .send(Job::PanePath {
                uuid: uuid.to_string(),
                reply,
            })
            .ok()?;
        answer.recv_timeout(wait).ok().flatten()
    }

    /// `ssh -O exit` for this host's master, synchronously on the caller's
    /// thread (quit must not wait behind a queued login). Remote sessions
    /// survive; the next start logs in again.
    pub fn exit_master(&self) {
        let argv = remote::control_argv(&self.dest, &self.control_path, ControlOp::Exit);
        if let Err(err) = run_detached(&argv, &[], None) {
            eprintln!("kabelsalat: ssh -O exit for {}: {err}", self.dest);
        }
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib remote_worker::tests`
Expected: 16 passed. `run_detached_has_no_controlling_terminal` passes whether or not `cargo test` itself runs in a terminal.

Run: `cargo fmt && cargo clippy --all-targets && cargo test`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src/remote_worker.rs src/lib.rs
git commit -m "Add the per-host ssh worker that runs remote tmux calls serially"
```

---
### Task 10: App — groups carry their host, the kill queue persists, restore skips remote tabs locally, env stays local

**Depends on:** Tasks 2 and 8 (`app.rs` sequence: Task 8 → Task 10)

**Files:**
- Modify `src/app.rs`:
  - `Group` struct (`191-213`)
  - `App` struct (`215-312`)
  - `init` model literal (`764-815`)
  - `create_group` (`1325-1340`)
  - `restore_or_fresh` (`1349-1513`)
  - `save_state` (`1516-1594`)
  - `cdp_env_set` / `cdp_env_unset` / `session_env_refresh` (`1958-2007`)
  - new helper `group_host`

This task makes no remote connection yet. Remote groups cannot be created before Task 13, so this is only reachable through a hand-edited state file. In that case, `add_tab` still spawns a restored remote tab like a local one until Task 11 routes remote tabs to their host.

**Interfaces:**
- Consumes:
  - Task 1: `SavedGroup::host`, `SavedState::pending_kills`, `state::PendingKill`
  - Task 2: `state::reconcile_local`
- Produces (used by Tasks 11–15):
  - `Group { …, host: Option<String> }`
  - `App { …, pending_kills: Vec<state::PendingKill> }`
  - `fn group_host(&self, group: usize) -> Option<String>` on `App`

- [ ] **Step 1: Add the fields**

In `Group`, after `default_url` (`212`), add:

```rust
    /// The ssh destination this group's tabs run on; `None` = this computer.
    /// Fixed for the group's lifetime: tabs cannot move between hosts.
    host: Option<String>,
```

In `App`, after `save_error_shown` (`301`), add:

```rust
    /// Remote sessions whose tab is closed but whose kill the host has not
    /// confirmed yet — an outbox, persisted in the state file and flushed
    /// after the next successful connect to each host.
    pending_kills: Vec<state::PendingKill>,
```

In `init`'s `App { … }` literal, add `pending_kills: Vec::new(),` after `save_error_shown: Cell::new(false),`.

In `create_group`, add `host: None,` after `default_url: None,`.

- [ ] **Step 2: Persist and restore them**

In `restore_or_fresh`, after `self.sidebar_order = saved.sidebar_order;` (`1362`), add:

```rust
        // Kept whether or not any tab comes back: a queued kill belongs to a
        // host, not to a tab, and must survive until that host confirms it.
        self.pending_kills = saved.pending_kills.clone();
```

Replace:

```rust
        let plan = state::reconcile(&saved, &live, &dead);
        if plan.attach.is_empty() && plan.respawn.is_empty() && plan.adopt.is_empty() {
```

with:

```rust
        // The local bucket only: remote tabs are planned per host once that
        // host answers (on_host_connected), and never respawned here.
        let plan = state::reconcile_local(&saved, &live, &dead);
        if saved.tabs.is_empty() && plan.adopt.is_empty() {
```

In the `self.groups.push(Group { … })` of the saved-groups loop, add `host: group.host.clone(),` after `default_url: group.default_url.clone(),`.

In `save_state` only (both strings also occur in `init` and `create_group`, which stay as they are): replace the `host: None,` that follows `default_url: g.default_url.clone(),` with `host: g.host.clone(),`, and the `pending_kills: Vec::new(),` that follows `sidebar_order: self.sidebar_order,` with `pending_kills: self.pending_kills.clone(),`.

- [ ] **Step 3: Add `group_host` and keep the CDP/group variables local**

After `active_group` (`1311-1313`), add:

```rust
    /// The host a group's tabs run on; `None` for a local group (or an
    /// unknown id). Owned, so callers can hold it across `&mut self` calls.
    fn group_host(&self, group: usize) -> Option<String> {
        self.groups
            .iter()
            .find(|g| g.id == group)
            .and_then(|g| g.host.clone())
    }
```

In `cdp_env_set` and in `cdp_env_unset`, directly after `let Some(tmux) = &self.tmux else { return };`, add:

```rust
        // The browser is local-only: its endpoint is never exported into a
        // remote group's sessions (they live on another server anyway).
        if self.group_host(group_id).is_some() {
            return;
        }
```

In `session_env_refresh`, directly after `let Some(tmux) = &self.tmux else { return };`, add:

```rust
        // KABELSALAT_GROUP and the endpoint pair stay local (spec).
        if self.group_host(group_id).is_some() {
            return;
        }
```

- [ ] **Step 4: Verify**

Run: `cargo fmt && cargo build && cargo clippy --all-targets && cargo test`
Expected: builds; no new warnings; all tests pass.

Manual check, using the harness from the top of this plan without a seeded state:
1. Start it and open two tabs.
2. Quit, then run `jq '.groups[].host, .pending_kills' /tmp/ks-t/state/kabelsalat/state.json`.
   Expected: `null` per group and `[]`.
3. Restart. Both tabs reattach exactly as before.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "Keep each group's host and the pending remote kills in the model"
```

---

### Task 11: App — host connection lifecycle, host page, attach after list-sessions

**Depends on:** Tasks 9 and 10

**Files:**
- Modify `src/app.rs`:
  - imports (`16-19`)
  - new types after `enum PickerMode` (`159-163`)
  - `Tab` (`165-189`)
  - `App` (`215-312`)
  - `Msg` (`324-450`)
  - `init` (`718-815`)
  - `update` (new arms)
  - `shutdown` (`1271-1302`)
  - `restore_or_fresh` (end, `1506-1512`)
  - `add_tab` (`2422-2544`)
  - `activate` (`2625`)
  - `close_tab` (`2654`)
  - `spawn_backing` (`3940-3972`, with its doc comment)
  - new `impl App` section "remote hosts"

**Interfaces:**
- Consumes:
  - Task 5: `remote::{HostState, RemoteError}`
  - Task 3: `remote::SshAvailability`
  - Task 4: `remote::{AuthMode, auth_mode}`
  - Task 9: `remote_worker::{detect_ssh, is_executable, control_path, RemoteEvent, RemoteWorker, SshRunner}`
  - Task 7: `TmuxCtl::remote`, `TmuxCtl::spawn_argv`
  - Task 2: `state::{remote_hosts, reconcile_remote, RemoteListing, DeadPane}`
- Produces (used by Tasks 12–15):
  - `Msg::Remote { host: String, event: RemoteEvent }`, `Msg::Reconnect(String)`
  - `Tab { …, view: gtk::Stack, status: Option<HostPage>, attached: bool, pending: Option<PendingSpawn> }`
  - `App { …, ssh: remote::SshAvailability, auth: remote::AuthMode, hosts: HashMap<String, RemoteHost> }`
  - `App` methods:
    - `fn tab_host(&self, id: usize) -> Option<String>`
    - `fn host_state(&self, host: &str) -> HostState`
    - `fn worker(&self, host: &str) -> Option<&RemoteWorker>`
    - `fn set_host_state(&mut self, host: &str, state: HostState)`
    - `fn connect_host(&mut self, host: &str)`
    - `fn attach_host_tabs(&mut self, host: &str)`
    - `fn spawn_remote_tab(&mut self, id: usize)`
    - `fn refresh_tab_page(&self, tab: &Tab)`
    - `fn refresh_host_pages(&self, host: &str)`
  - free fn `spawn_client(terminal: &Terminal, argv: &[String])`

- [ ] **Step 1: Imports, types, fields and messages**

Change the imports (`16-19`) to:

```rust
use crate::browser::{self, Browser, CapturedFrame, ProfileDisposition};
use crate::control;
use crate::remote::{self, HostState, RemoteError};
use crate::remote_worker::{self, RemoteEvent, RemoteWorker, SshRunner};
use crate::state::{self, SavedGroup, SavedState, SavedTab, SidebarOrder};
use crate::tmuxctl::{self, LingerStatus, SessionInfo, TmuxAvailability, TmuxCtl, TmuxError};
```

After `enum PickerMode { … }` (`163`), add:

```rust
/// What a remote tab spawns once its host is live: the directory and command
/// it was created with. Consumed by the first spawn; a later reattach needs
/// neither (`new-session -A` ignores `-c` for an existing session).
#[derive(Debug, Default)]
struct PendingSpawn {
    cwd: Option<PathBuf>,
    command: Option<Vec<String>>,
}

/// The page a remote tab shows instead of its terminal while its host is
/// connecting or disconnected, or while the tab itself is detached.
struct HostPage {
    page: adw::StatusPage,
    spinner: gtk::Spinner,
    reconnect: gtk::Button,
}

impl HostPage {
    fn new(host: &str, input: &relm4::Sender<Msg>) -> Self {
        let spinner = gtk::Spinner::builder()
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .build();
        let reconnect = gtk::Button::builder()
            .label("Reconnect")
            .halign(gtk::Align::Center)
            .build();
        reconnect.add_css_class("pill");
        reconnect.add_css_class("suggested-action");
        reconnect.connect_clicked({
            let input = input.clone();
            let host = host.to_string();
            move |_| {
                let _ = input.send(Msg::Reconnect(host.clone()));
            }
        });
        let child = gtk::Box::new(gtk::Orientation::Vertical, 12);
        child.append(&spinner);
        child.append(&reconnect);
        let page = adw::StatusPage::builder()
            .icon_name("network-server-symbolic")
            .child(&child)
            .build();
        Self {
            page,
            spinner,
            reconnect,
        }
    }

    fn show(&self, title: &str, description: &str, connecting: bool, reconnect: bool) {
        self.page.set_title(title);
        // The description is Pango markup, and ssh's stderr can hold '<'/'&'.
        let markup = gtk::glib::markup_escape_text(description);
        self.page
            .set_description(Some(markup.as_str()).filter(|text| !text.is_empty()));
        self.spinner.set_visible(connecting);
        self.spinner.set_spinning(connecting);
        self.reconnect.set_visible(reconnect);
    }
}

/// One remote host as the app sees it.
struct RemoteHost {
    state: HostState,
    /// Present once a connect was attempted with a usable ssh.
    link: Option<HostLink>,
}

struct HostLink {
    /// Builds the tabs' `ssh … tmux new-session -A …` argv.
    ctl: TmuxCtl,
    worker: RemoteWorker,
}
```

In `Tab`, after `age_shown` (`188`), add:

```rust
    /// What the stack shows for this tab: its terminal ("terminal") or —
    /// remote tabs only — the host page ("status").
    view: gtk::Stack,
    /// Remote tabs only; `None` for local tabs, which always show the
    /// terminal.
    status: Option<HostPage>,
    /// Whether the terminal runs this tab's tmux client. Always true for a
    /// local tab. A remote tab is detached until its host is live, and again
    /// after its ssh client exits.
    attached: bool,
    /// Remote tabs only: the first spawn's directory and command, kept until
    /// the host is live.
    pending: Option<PendingSpawn>,
```

In `App`, after `linger_dismissed` (`280`), add:

```rust
    /// Startup check of the local ssh client: remote groups need OpenSSH
    /// >= 8.4, and are disabled (never dropped) otherwise.
    ssh: remote::SshAvailability,
    /// How the workers log in: through an askpass program, or keys only.
    auth: remote::AuthMode,
    /// Remote hosts by their destination string, as the groups name them.
    hosts: HashMap<String, RemoteHost>,
```

In `Msg`, after `RenameGroup { … }` (`446-449`), add:

```rust
    /// A remote host's worker reported back.
    Remote {
        host: String,
        event: RemoteEvent,
    },
    /// Rerun the connect sequence for a host — or, when it is live, reattach
    /// its detached tabs.
    Reconnect(String),
```

- [ ] **Step 2: Detect ssh at startup, connect restored hosts, drop masters on quit**

In `init`, directly before `let mut model = App {` (`764`), add:

```rust
        // Checked once, like tmux. Remote groups need OpenSSH >= 8.4 for
        // SSH_ASKPASS_REQUIRE; anything else disables them.
        let ssh = remote_worker::detect_ssh();
        let auth = remote::auth_mode(|key| std::env::var(key).ok(), remote_worker::is_executable);
```

In the `App { … }` literal, after `linger_dismissed,` add:

```rust
            ssh,
            auth,
            hosts: HashMap::new(),
```

In `update`, after the `Msg::RenameGroup { … } => { … }` arm, add:

```rust
            Msg::Remote { host, event } => self.on_remote_event(host, event),
            Msg::Reconnect(host) => self.connect_host(&host),
```

In `shutdown`, directly after the `for id in browser_group_ids { self.cdp_env_unset(id); }` loop, add:

```rust
        // Remote sessions outlive the app like local ones; only the ssh
        // masters go. The next start logs in again.
        for host in self.hosts.values() {
            if let Some(link) = &host.link {
                link.worker.exit_master();
            }
        }
```

In `restore_or_fresh`, directly before the comment `// Before the save, not after: this fills `pending_browser_restore`…` that precedes the final `self.start_browser_maintenance();` (`1506-1511`), so the comment stays attached to that call, add:

```rust
        // Remote tabs came back detached, behind their host page. Each host
        // logs in once now; its tabs spawn only after its list-sessions
        // succeeded (on_host_connected).
        for host in state::remote_hosts(&saved) {
            self.connect_host(&host);
        }
```

- [ ] **Step 3: Tabs get a view stack; remote tabs wait for their host**

In `add_tab`, replace the block from `// Stamped at session creation via new-session -e:` through `spawn_backing(&terminal, &uuid, self.tmux.as_ref(), cwd, command, &env);` (`2442-2456`) with:

```rust
        let host = self.group_host(group);
        let pending = match &host {
            // Never spawned here: a remote shell may only be created after a
            // successful list-sessions from its host, and a host that is
            // already live gets the spawn right below, once the tab exists.
            Some(_) => Some(PendingSpawn {
                cwd: cwd.map(Path::to_path_buf),
                command: command.map(<[String]>::to_vec),
            }),
            None => {
                // Stamped at session creation via new-session -e: the group
                // identity always, the endpoint pair when the group's browser
                // already has one. A -A reattach ignores -e; reattached
                // sessions are refreshed explicitly in restore_or_fresh.
                let group_info = self.group_env_pairs(group);
                let mut env: Vec<(&str, &str)> = Vec::new();
                if let Some((group_uuid, cdp_url)) = &group_info {
                    env.push((ENV_GROUP, group_uuid.as_str()));
                    if let Some(url) = cdp_url {
                        for key in CDP_ENV_KEYS {
                            env.push((key, url.as_str()));
                        }
                    }
                }
                spawn_backing(&terminal, &uuid, self.tmux.as_ref(), cwd, command, &env);
                None
            }
        };
```

Replace `self.stack.add_child(&terminal);` (`2531`) with:

```rust
        let view = gtk::Stack::new();
        view.add_named(&terminal, Some("terminal"));
        let status = host.as_deref().map(|host| {
            let page = HostPage::new(host, &self.input);
            view.add_named(&page.page, Some("status"));
            page
        });
        self.stack.add_child(&view);
```

In the `self.tabs.push(Tab { … })` literal, add after `age_shown: age_prefix(Duration::ZERO),`:

```rust
            view,
            status,
            attached: host.is_none(),
            pending,
```

Replace the final `id` of `add_tab` (`2543`) with:

```rust
        if let Some(host) = &host {
            if let Some(tab) = self.tabs.last() {
                self.refresh_tab_page(tab);
            }
            if self.host_state(host) == HostState::Live {
                self.spawn_remote_tab(id);
            }
        }
        id
```

In `activate`, replace `self.stack.set_visible_child(&tab.terminal);` with `self.stack.set_visible_child(&tab.view);`.

In `close_tab`, replace `self.stack.remove(&tab.terminal);` with `self.stack.remove(&tab.view);`.

Replace `spawn_backing` (`3940-3972`, doc comment included; the `fn` line is `3943`) with:

```rust
/// Spawn a tab's backing process: the tmux client for its session when tmux is
/// available (`new-session -A` attaches or creates), else a direct $SHELL.
/// `command`, when set, replaces the shell in either path.
fn spawn_backing(
    terminal: &Terminal,
    uuid: &str,
    tmux: Option<&TmuxCtl>,
    cwd: Option<&Path>,
    command: Option<&[String]>,
    env: &[(&str, &str)],
) {
    let Some(ctl) = tmux else {
        spawn_shell(terminal, cwd, command);
        return;
    };
    spawn_client(terminal, &ctl.spawn_argv(uuid, cwd, command, env));
}

/// Run a tmux client argv in the terminal: `tmux …` locally, or a remote
/// tab's `ssh … -- 'tmux …'`.
fn spawn_client(terminal: &Terminal, argv: &[String]) {
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    terminal.spawn_async(
        PtyFlags::DEFAULT,
        None,
        &refs,
        &[],
        gtk::glib::SpawnFlags::DEFAULT,
        || {},
        -1,
        gtk::gio::Cancellable::NONE,
        |result| {
            if let Err(err) = result {
                eprintln!("failed to attach tmux session: {err}");
            }
        },
    );
}
```

- [ ] **Step 4: The host lifecycle**

Add a new section to `impl App`, directly before `// ---- browser pane ---` (`1617`):

```rust
    // ---- remote hosts ---------------------------------------------------

    fn tab_host(&self, id: usize) -> Option<String> {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| self.group_host(t.group))
    }

    /// A host nobody has connected yet reads as connecting: its tabs are
    /// about to be, and must not look broken in the meantime.
    fn host_state(&self, host: &str) -> HostState {
        self.hosts
            .get(host)
            .map_or(HostState::Connecting, |h| h.state.clone())
    }

    fn worker(&self, host: &str) -> Option<&RemoteWorker> {
        self.hosts
            .get(host)
            .and_then(|h| h.link.as_ref())
            .map(|link| &link.worker)
    }

    fn set_host_state(&mut self, host: &str, state: HostState) {
        self.hosts
            .entry(host.to_string())
            .or_insert_with(|| RemoteHost {
                state: HostState::Connecting,
                link: None,
            })
            .state = state;
        self.refresh_host_pages(host);
        self.rebuild_list();
    }

    /// Log in to `host` (once; the worker runs the host check and lists the
    /// sessions), or, when it is already live, reattach its detached tabs.
    /// Never retried automatically, so askpass never pops up unasked.
    fn connect_host(&mut self, host: &str) {
        match self.hosts.get(host).map(|h| h.state.clone()) {
            // One login at a time; the running attempt reports back.
            Some(HostState::Connecting) if self.worker(host).is_some() => return,
            Some(HostState::Live) => {
                self.attach_host_tabs(host);
                return;
            }
            _ => {}
        }
        if !self.ssh.is_available() {
            self.set_host_state(host, HostState::Disconnected(RemoteError::SshUnsupported));
            return;
        }
        // The dialog validates, but a hand-edited state file does not: a
        // destination starting with '-' would reach ssh as an option.
        if let Err(err) = state::validate_host(host) {
            self.set_host_state(host, HostState::Disconnected(RemoteError::Other(err.to_string())));
            return;
        }
        if self.worker(host).is_none() {
            match self.spawn_worker(host) {
                Ok(link) => {
                    self.hosts
                        .entry(host.to_string())
                        .or_insert_with(|| RemoteHost {
                            state: HostState::Connecting,
                            link: None,
                        })
                        .link = Some(link);
                }
                Err(detail) => {
                    self.set_host_state(host, HostState::Disconnected(RemoteError::Other(detail)));
                    return;
                }
            }
        }
        self.set_host_state(host, HostState::Connecting);
        if let Some(worker) = self.worker(host) {
            worker.connect();
        }
    }

    fn spawn_worker(&self, host: &str) -> Result<HostLink, String> {
        let control_path = remote_worker::control_path()
            .map_err(|err| format!("no directory for the ssh control socket: {err}"))?;
        let input = self.input.clone();
        let reply_host = host.to_string();
        let worker = RemoteWorker::spawn(
            host.to_string(),
            control_path.clone(),
            self.auth.clone(),
            SshRunner,
            move |event| {
                let _ = input.send(Msg::Remote {
                    host: reply_host.clone(),
                    event,
                });
            },
        )
        .map_err(|err| format!("could not start the connection thread: {err}"))?;
        Ok(HostLink {
            ctl: TmuxCtl::remote(host, &control_path),
            worker,
        })
    }

    fn on_remote_event(&mut self, host: String, event: RemoteEvent) {
        match event {
            RemoteEvent::Connected(sessions) => self.on_host_connected(&host, &sessions),
            RemoteEvent::ConnectFailed(err) => {
                eprintln!("kabelsalat: {}", err.message(&host));
                self.set_host_state(&host, HostState::Disconnected(err));
            }
            RemoteEvent::Killed(uuids) => self
                .pending_kills
                .retain(|kill| kill.host != host || !uuids.contains(&kill.uuid)),
            // Nothing asks the worker about a tab's child exit yet, so
            // neither of these can arrive.
            RemoteEvent::MasterDead | RemoteEvent::TabSessions { .. } => {}
        }
    }

    /// The host passed its check and listed its sessions: plan its tabs
    /// (state::reconcile_remote), flush the queued kills, mark crashed panes,
    /// and only now spawn its tabs.
    fn on_host_connected(&mut self, host: &str, sessions: &[SessionInfo]) {
        let listing = state::RemoteListing {
            live: sessions.iter().map(|s| s.uuid.clone()).collect(),
            dead: sessions
                .iter()
                .filter(|s| s.pane_dead)
                .map(|s| state::DeadPane {
                    uuid: s.uuid.clone(),
                    exit_code: s.dead_status.unwrap_or(-1),
                })
                .collect(),
        };
        let tabs: Vec<String> = self
            .tabs
            .iter()
            .filter(|t| self.group_host(t.group).as_deref() == Some(host))
            .map(|t| t.uuid.clone())
            .collect();
        let queued: Vec<String> = self
            .pending_kills
            .iter()
            .filter(|kill| kill.host == host)
            .map(|kill| kill.uuid.clone())
            .collect();
        let plan = state::reconcile_remote(&tabs, Some(&listing), &queued);
        // A queued kill whose session is already gone is done; the live ones
        // leave the queue when the host confirms them (RemoteEvent::Killed).
        self.pending_kills
            .retain(|kill| kill.host != host || plan.kill.contains(&kill.uuid));
        if !plan.kill.is_empty()
            && let Some(worker) = self.worker(host)
        {
            worker.kill(plan.kill.clone());
        }
        for attach in &plan.attach {
            if let Some(code) = attach.dead_exit
                && let Some(tab) = self.tabs.iter_mut().find(|t| t.uuid == attach.uuid)
            {
                tab.crashed = Some(code);
            }
        }
        if let Some(entry) = self.hosts.get_mut(host) {
            entry.state = HostState::Live;
        }
        self.attach_host_tabs(host);
        self.refresh_host_pages(host);
        self.rebuild_list();
    }

    /// Spawn the client of every detached tab on a live host: restored and
    /// fresh tabs alike (`new-session -A` attaches or creates).
    fn attach_host_tabs(&mut self, host: &str) {
        let detached: Vec<usize> = self
            .tabs
            .iter()
            .filter(|t| !t.attached && self.group_host(t.group).as_deref() == Some(host))
            .map(|t| t.id)
            .collect();
        for id in detached {
            self.spawn_remote_tab(id);
        }
    }

    /// Run a remote tab's `ssh -S … -t <dest> -- tmux … new-session -A …`.
    /// Only ever called while its host is live. KABELSALAT_* stay local, so
    /// no `-e` pairs.
    fn spawn_remote_tab(&mut self, id: usize) {
        let Some(host) = self.tab_host(id) else {
            return;
        };
        let Some(ctl) = self
            .hosts
            .get(&host)
            .and_then(|h| h.link.as_ref())
            .map(|link| link.ctl.clone())
        else {
            return;
        };
        let active = self.active == Some(id);
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return;
        };
        let pending = tab.pending.take().unwrap_or_default();
        let argv = ctl.spawn_argv(
            &tab.uuid,
            pending.cwd.as_deref(),
            pending.command.as_deref(),
            &[],
        );
        spawn_client(&tab.terminal, &argv);
        tab.attached = true;
        tab.view.set_visible_child_name("terminal");
        if active {
            tab.terminal.grab_focus();
        }
    }

    fn refresh_host_pages(&self, host: &str) {
        for tab in self
            .tabs
            .iter()
            .filter(|t| self.group_host(t.group).as_deref() == Some(host))
        {
            self.refresh_tab_page(tab);
        }
    }

    /// Show a remote tab's terminal or its host page, whichever its host's
    /// state and its own attachment call for. Local tabs: no-op.
    fn refresh_tab_page(&self, tab: &Tab) {
        let Some(page) = &tab.status else {
            return;
        };
        let Some(host) = self.group_host(tab.group) else {
            return;
        };
        match self.host_state(&host) {
            HostState::Live if tab.attached => {
                tab.view.set_visible_child_name("terminal");
                return;
            }
            HostState::Live => page.show(
                &host,
                "This tab is not attached to its session. Reconnect to attach it again.",
                false,
                true,
            ),
            HostState::Connecting => page.show(&format!("Connecting to {host}…"), "", true, false),
            HostState::Disconnected(err) => page.show(
                &format!("{host} is disconnected"),
                &err.message(&host),
                false,
                true,
            ),
        }
        tab.view.set_visible_child_name("status");
    }
```

- [ ] **Step 5: Verify**

Run: `cargo fmt && cargo build && cargo clippy --all-targets && cargo test`
Expected: builds; no new warnings; all tests pass.

Manual check with the harness from the top, seeded with the `localhost` state:
1. Start it. The tab shows "Connecting to localhost…" with a spinner, then a shell on localhost.
2. Run `ssh localhost tmux -L kabelsalat ls` in another terminal. It shows `ks-11111111-…`.
3. Quit and restart. The tab reattaches to the same session (scrollback intact), and `ls /tmp/ks-t/run/kabelsalat/ssh/` is empty after the quit.
4. Stop `sshd` (`sudo systemctl stop sshd`), then quit and restart. The page reads "localhost is disconnected" with the Unreachable message and a Reconnect button, and there is no retry loop.
5. Start `sshd` and press Reconnect. The tab attaches.
6. Unset `SSH_AUTH_SOCK` for a password-only host with no askpass installed. The page shows the AuthNeedsAskpass text.

- [ ] **Step 6: Commit**

```bash
git add src/app.rs
git commit -m "Connect remote hosts through a worker and attach their tabs once live"
```

---
### Task 12: App — remote child exit, disconnect, close outbox, restart and remote cwd

**Depends on:** Task 11

**Files:**
- Modify `src/app.rs`:
  - imports (`5`)
  - constants (after `TAB_MIN_CHARS`, `157`)
  - `Tab` (spawn stamp)
  - `update`'s `Msg::ChildExited` arm (`923-972`)
  - `on_remote_event` (Task 11)
  - `spawn_remote_tab` (Task 11)
  - `add_tab`'s `Tab` literal
  - `open_tab` (`2374-2380`)
  - `restart_tab` (`2549-2568`)
  - `close_tab` (`2642-2656`)
  - new methods `host_tab_ids`, `remote_child_exited`, `on_tab_sessions`, `new_tab_cwd`
  - tests in `mod tests`

**Interfaces:**
- Consumes:
  - Task 9: `RemoteWorker::{child_exited, kill, respawn, pane_current_path}`, `RemoteEvent::{MasterDead, TabSessions}`
  - Task 6: `remote::SSH_FAILED`
  - existing `session_definitively_gone`, `decode_exit`
- Produces:
  - `const REMOTE_CWD_WAIT: Duration` (300 ms)
  - `const REATTACH_GUARD: Duration` (2 s)
  - `Tab { …, spawned_at: Option<Instant> }`
  - `fn reattach_allowed(since_spawn: Option<Duration>) -> bool` (pure, tested)

- [ ] **Step 1: Write the failing test for the reattach guard**

Append to `mod tests` in `src/app.rs`:

```rust
    // --- remote reattach guard -------------------------------------------

    #[test]
    fn a_client_that_exits_right_after_attaching_is_not_reattached() {
        // Some clients exit immediately while their session lives on
        // ("open terminal failed"); reattaching those would loop.
        assert!(!reattach_allowed(Some(Duration::from_millis(300))));
        assert!(!reattach_allowed(Some(Duration::from_millis(1999))));
    }

    #[test]
    fn a_client_that_ran_for_a_while_is_reattached() {
        assert!(reattach_allowed(Some(Duration::from_secs(2))));
        assert!(reattach_allowed(Some(Duration::from_secs(3600))));
        // Never spawned: nothing to guard against.
        assert!(reattach_allowed(None));
    }

    #[test]
    fn only_ssh_s_own_status_counts_as_an_ssh_failure() {
        // Raw wait statuses: exit(255) is ssh failing; a signal or another
        // code is the remote side's business.
        assert_eq!(decode_exit(255 << 8), remote::SSH_FAILED);
        assert_ne!(decode_exit(1 << 8), remote::SSH_FAILED);
        assert_ne!(decode_exit(9), remote::SSH_FAILED);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib app::tests::a_client`
Expected: compile error, `cannot find function reattach_allowed`.

- [ ] **Step 3: Write the implementation**

Change `use std::time::{Duration, SystemTime, UNIX_EPOCH};` (`5`) to `use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};`.

After `const TAB_MIN_CHARS: i32 = 3;` (`157`), add:

```rust
/// How long a new remote tab waits for the active tab's remote directory
/// before starting in the remote home instead.
const REMOTE_CWD_WAIT: Duration = Duration::from_millis(300);

/// A remote client that exits sooner than this after its spawn, while its
/// session lives on, is not reattached automatically — that would loop.
const REATTACH_GUARD: Duration = Duration::from_secs(2);
```

In `Tab`, after `pending` (Task 11), add:

```rust
    /// When a remote tab's client was last spawned; the reattach guard
    /// compares against it. `None` for local tabs.
    spawned_at: Option<Instant>,
```

In `add_tab`'s `Tab { … }` literal, add `spawned_at: None,` after `pending,`.

In `spawn_remote_tab`, after `tab.attached = true;`, add `tab.spawned_at = Some(Instant::now());`.

In `decode_exit`'s doc comment, replace the sentence that says it is used only in the no-tmux fallback with: `/// Used in the no-tmux fallback, and by remote tabs to tell ssh's own 255 from the remote side's status.`

Next to `session_definitively_gone` (`3631-3633`), add:

```rust
/// May a remote tab whose client just exited (while its session lives on)
/// be reattached automatically? Not when the client barely ran: that is a
/// client that cannot attach, and reattaching would loop. Pure.
fn reattach_allowed(since_spawn: Option<Duration>) -> bool {
    since_spawn.is_none_or(|elapsed| elapsed >= REATTACH_GUARD)
}
```

In `update`, change the start of the `Msg::ChildExited(id, status) => {` arm. Replace `if let Some(tmux) = &self.tmux {` (its first line, `924`) with:

```rust
                if self.tab_host(id).is_some() {
                    // Remote: the worker asks the host, never this thread.
                    self.remote_child_exited(id, status);
                } else if let Some(tmux) = &self.tmux {
```

The rest of the arm stays as it is. The former `if` now reads `else if`, so its `else if gtk::glib::spawn_check_wait_status(…)` branches keep working.

In `on_remote_event`, replace the arm

```rust
            // Nothing asks the worker about a tab's child exit yet, so
            // neither of these can arrive.
            RemoteEvent::MasterDead | RemoteEvent::TabSessions { .. } => {}
```

with:

```rust
            RemoteEvent::MasterDead => {
                // Only a live host can die; a Reconnect already underway
                // (Connecting) must not be overridden by a stale report.
                if self.host_state(&host) != HostState::Live {
                    return;
                }
                eprintln!("kabelsalat: lost the connection to {host}");
                // Every client of the master went down with it. No automatic
                // reattach: Reconnect reruns the login, then attaches them.
                for id in self.host_tab_ids(&host) {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                        tab.attached = false;
                    }
                }
                self.set_host_state(&host, HostState::Disconnected(RemoteError::Unreachable));
            }
            RemoteEvent::TabSessions { tab, result } => self.on_tab_sessions(tab, result),
```

Add these methods to the "remote hosts" section of `impl App`:

```rust
    fn host_tab_ids(&self, host: &str) -> Vec<usize> {
        self.tabs
            .iter()
            .filter(|t| self.group_host(t.group).as_deref() == Some(host))
            .map(|t| t.id)
            .collect()
    }

    /// A remote tab's ssh client exited. Status 255 is ssh itself failing:
    /// the worker checks the master (`ssh -O check`), and a dead master takes
    /// the whole host to Disconnected. Any other status: the worker lists the
    /// host's sessions — asynchronously, never on this thread — and
    /// `on_tab_sessions` closes or reattaches.
    fn remote_child_exited(&mut self, id: usize, status: i32) {
        let Some(host) = self.tab_host(id) else {
            return;
        };
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return;
        };
        tab.attached = false;
        if self.host_state(&host) != HostState::Live {
            // Already down or reconnecting; the next connect reattaches.
            if let Some(tab) = self.tabs.iter().find(|t| t.id == id) {
                self.refresh_tab_page(tab);
            }
            return;
        }
        let ssh_failed = decode_exit(status) == remote::SSH_FAILED;
        if let Some(worker) = self.worker(&host) {
            worker.child_exited(id, ssh_failed);
        }
    }

    /// The listing a remote child exit asked for. Only a successful listing
    /// without the session closes the tab (`session_definitively_gone`); an
    /// error means liveness is unknown and the tab stays. A dead pane marks
    /// the tab crashed (remote servers have no pane-died file hook).
    fn on_tab_sessions(&mut self, id: usize, result: Result<Vec<SessionInfo>, String>) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return;
        };
        let uuid = tab.uuid.clone();
        let since_spawn = tab.spawned_at.map(|at| at.elapsed());
        let Some(host) = self.group_host(tab.group) else {
            return;
        };
        let result = result.map_err(TmuxError::Command);
        if session_definitively_gone(&result, &uuid) {
            self.close_tab(id);
            return;
        }
        let dead_exit = result
            .ok()
            .and_then(|sessions| sessions.into_iter().find(|s| s.uuid == uuid))
            .filter(|s| s.pane_dead)
            .map(|s| s.dead_status.unwrap_or(-1));
        if let Some(code) = dead_exit
            && let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id)
        {
            tab.crashed = Some(code);
        }
        if self.host_state(&host) == HostState::Live && reattach_allowed(since_spawn) {
            self.spawn_remote_tab(id);
        } else if let Some(tab) = self.tabs.iter().find(|t| t.id == id) {
            if self.host_state(&host) == HostState::Live {
                eprintln!(
                    "kabelsalat: {uuid} on {host} exited right after attaching; \
                     not reattaching automatically"
                );
            }
            // Shows the Reconnect page; its button reattaches this tab.
            self.refresh_tab_page(tab);
        }
        self.rebuild_list();
    }

    /// Directory for a new tab in `group`, taken from the active tab when
    /// both run on the same machine. Local: `active_tab_cwd` as before.
    /// Remote: the host's `pane_current_path`, waited on for at most
    /// `REMOTE_CWD_WAIT` and never checked against the local filesystem;
    /// `None` starts the shell in the remote home.
    fn new_tab_cwd(&self, group: usize) -> Option<PathBuf> {
        let tab = self.active_tab()?;
        let host = self.group_host(group);
        if self.group_host(tab.group) != host {
            return None;
        }
        match host {
            None => self.active_tab_cwd(),
            Some(host) => {
                if self.host_state(&host) != HostState::Live {
                    return None;
                }
                self.worker(&host)?
                    .pane_current_path(&tab.uuid, REMOTE_CWD_WAIT)
                    .map(PathBuf::from)
            }
        }
    }
```

In `open_tab`, replace `let cwd = self.active_tab_cwd();` with `let cwd = self.new_tab_cwd(group);`.

In `close_tab`, replace:

```rust
        let tab = self.tabs.remove(index);
        // Explicit close is the only thing that kills the backing session.
        if let Some(tmux) = &self.tmux
            && let Err(err) = tmux.kill_session(&tab.uuid)
        {
            eprintln!("failed to kill session {}: {err}", tab.uuid);
        }
```

with:

```rust
        let tab = self.tabs.remove(index);
        // Explicit close is the only thing that kills the backing session.
        match self.group_host(tab.group) {
            None => {
                if let Some(tmux) = &self.tmux
                    && let Err(err) = tmux.kill_session(&tab.uuid)
                {
                    eprintln!("failed to kill session {}: {err}", tab.uuid);
                }
            }
            // Remote: the queue is an outbox. The entry leaves it only when
            // the host confirms the kill (RemoteEvent::Killed), so a kill
            // lost to a dropped connection — or to quitting right after
            // closing the last tab — is retried after the next successful
            // connect instead of leaking the session. A live host gets the
            // kill at once, fire-and-forget.
            Some(host) => {
                self.pending_kills.push(state::PendingKill {
                    host: host.clone(),
                    uuid: tab.uuid.clone(),
                });
                if self.host_state(&host) == HostState::Live
                    && let Some(worker) = self.worker(&host)
                {
                    worker.kill(vec![tab.uuid.clone()]);
                }
            }
        }
```

Replace `restart_tab` (`2549-2568`) with:

```rust
    /// Rerun the shell in a crashed tab, clearing its crashed marker. With
    /// tmux the dead pane is respawned in place (client stays attached) —
    /// through the host's worker for a remote tab; in the fallback path a
    /// fresh $SHELL is spawned into the same terminal.
    fn restart_tab(&mut self, id: usize) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return;
        };
        let uuid = tab.uuid.clone();
        let terminal = tab.terminal.clone();
        match (self.group_host(tab.group), &self.tmux) {
            (Some(host), _) => {
                // A host that is not live has nothing to respawn into.
                if self.host_state(&host) != HostState::Live {
                    return;
                }
                if let Some(worker) = self.worker(&host) {
                    worker.respawn(uuid);
                }
            }
            (None, Some(tmux)) => {
                if let Err(err) = tmux.respawn_pane(&uuid) {
                    eprintln!("failed to respawn pane: {err}");
                    return;
                }
            }
            (None, None) => spawn_shell(&terminal, None, None),
        }
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
            tab.crashed = None;
        }
        self.rebuild_list();
    }
```

- [ ] **Step 4: Run the tests and verify**

Run: `cargo test --lib app::tests`
Expected: all pass (3 new).

Run: `cargo fmt && cargo build && cargo clippy --all-targets && cargo test`
Expected: clean.

Manual check with the harness from the top and the `localhost` seed:
1. `exit` in the remote shell. The tab closes; `ssh localhost tmux -L kabelsalat ls` no longer lists it.
2. Open a second remote tab (`Ctrl+Shift+T`), `cd /tmp` in the first, then `Ctrl+Shift+T` again. The new tab starts in `/tmp` on localhost.
3. Run `sh -c 'exit 3'` as the shell's last command (`exec sh -c 'exit 3'`). The pane shows tmux's dead-pane text. Quit and restart: the tab carries `[exit 3]`, and the restart button respawns it.
4. `sudo systemctl stop sshd && sudo pkill -f 'sshd: .*@notty'` (or drop the network). Within about 45 s every tab of the host shows "localhost is disconnected", and no reconnect loop appears in `journalctl --user -f`. Start `sshd` again and press Reconnect: all tabs reattach.
5. While disconnected, close a tab. `jq .pending_kills /tmp/ks-t/state/kabelsalat/state.json` lists it. Reconnect: the entry disappears and `ssh localhost tmux -L kabelsalat ls` no longer shows the session.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "Handle remote client exits, disconnects, closes and restarts through the worker"
```

---
### Task 13: App — "New remote group…", Group Settings host row, sidebar host header

**Depends on:** Task 12

**Files:**
- Modify `src/app.rs`:
  - `SHORTCUTS` (`25-60`)
  - `enum PickerMode` area (new `SettingsMode`)
  - `Msg` (new variants)
  - `view!` sidebar header box (`585-622`)
  - `update` (`Msg::GroupSettingsDialog` arm `1135` and new arms)
  - `show_group_settings_dialog` (`3382-3475`), which becomes `show_group_settings`
  - `make_group_header` (`3253-3302`)
  - `make_row` (`3352-3357`)
  - new `remote_button_tooltip`
- Modify `src/lib.rs`: global CSS (`115-145`).

**Interfaces:**
- Consumes:
  - Task 1: `state::validate_host`
  - Task 3: `SshAvailability::{is_available, reason}`
  - Tasks 11/12: `connect_host`, `host_state`, `open_tab`
- Produces:
  - `Msg::NewRemoteGroupDialog`
  - `Msg::CreateRemoteGroup { host: String, name: String, default_url: Option<String> }`
  - `enum SettingsMode { Create, Edit }`
  - `fn show_group_settings(&self, mode: SettingsMode)` (replaces `show_group_settings_dialog`)
  - CSS class `host-disconnected`

- [ ] **Step 1: Shortcut, messages, mode**

In `SHORTCUTS`, after the `("<Control><Shift>n", "New group", Msg::NewGroup),` entry, add:

```rust
    (
        "<Control><Shift>h",
        "New remote group",
        Msg::NewRemoteGroupDialog,
    ),
```

After `enum PickerMode { … }`, add:

```rust
/// Which form the group settings dialog shows: the one that creates a remote
/// group, or the active group's settings.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SettingsMode {
    Create,
    Edit,
}
```

In `Msg`, after `Reconnect(String),` (Task 11), add:

```rust
    /// Ctrl+Shift+H / the sidebar button: open the create form for a remote
    /// group, or explain why remote groups are unavailable.
    NewRemoteGroupDialog,
    /// Apply the create form: a new group on `host`, its first tab, and the
    /// host's connect. `default_url` is already normalized.
    CreateRemoteGroup {
        host: String,
        name: String,
        default_url: Option<String>,
    },
```

- [ ] **Step 2: The sidebar button**

In `view!`, in the sidebar header box, directly after the `folder-new-symbolic` button (`611-615`), add:

```rust
                                gtk::Button {
                                    set_icon_name: "network-server-symbolic",
                                    #[watch]
                                    set_sensitive: model.ssh.is_available(),
                                    #[watch]
                                    set_tooltip_text: Some(model.remote_button_tooltip().as_str()),
                                    connect_clicked => Msg::NewRemoteGroupDialog,
                                },
```

In `impl App`, next to `active_group_has_browser`, add:

```rust
    /// Tooltip of the "New remote group" button: its shortcut, or why it is
    /// insensitive.
    fn remote_button_tooltip(&self) -> String {
        self.ssh
            .reason()
            .unwrap_or_else(|| "New remote group (Ctrl+Shift+H)".to_string())
    }
```

- [ ] **Step 3: The dialog in both modes**

In `update`, replace `Msg::GroupSettingsDialog => self.show_group_settings_dialog(),` with:

```rust
            Msg::GroupSettingsDialog => self.show_group_settings(SettingsMode::Edit),
            Msg::NewRemoteGroupDialog => {
                match self.ssh.reason() {
                    Some(reason) => self.show_notice(&reason),
                    None => self.show_group_settings(SettingsMode::Create),
                }
                return;
            }
            Msg::CreateRemoteGroup {
                host,
                name,
                default_url,
            } => {
                // The form only enables Create for a valid host; this guards
                // the message itself.
                if state::validate_host(&host).is_err() {
                    return;
                }
                let id = self.create_group();
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    let name = name.trim();
                    // An empty name defaults to the host.
                    group.name = if name.is_empty() {
                        host.clone()
                    } else {
                        name.to_string()
                    };
                    group.host = Some(host.clone());
                    group.default_url = default_url;
                }
                // The tab appears at once, behind "Connecting…"; it spawns when
                // the host is live, or shows why it is not.
                self.open_tab(id, &sender);
                self.connect_host(&host);
            }
```

Replace the whole `show_group_settings_dialog` (doc comment included, `3382-3475`) with:

```rust
    /// The group settings dialog. `Edit`: the active group's name and browser
    /// default URL, with its host shown read-only. `Create`: the same form
    /// headed by a Host entry, creating a remote group. Enter applies, Esc
    /// cancels, an empty name removes the header (or, in create mode,
    /// defaults to the host) and an empty URL clears the default.
    ///
    /// Host and URL are validated as they are typed and Apply is disabled
    /// while either is unusable: `adw::AlertDialog` closes on any response,
    /// so an error raised at submit time would have nowhere left to live.
    fn show_group_settings(&self, mode: SettingsMode) {
        let group = match mode {
            SettingsMode::Create => None,
            SettingsMode::Edit => {
                let Some(group) = self
                    .active_group()
                    .and_then(|id| self.groups.iter().find(|g| g.id == id))
                else {
                    return;
                };
                Some(group)
            }
        };
        let remote = group.is_none_or(|g| g.host.is_some());

        let rows = adw::PreferencesGroup::new();
        // Host first. Read-only on an existing group: its tabs live on that
        // host, and a group without tabs does not exist (it is pruned).
        let host_entry = match group {
            None => {
                let entry = adw::EntryRow::builder()
                    .title("Host")
                    .activates_default(true)
                    .build();
                rows.add(&entry);
                Some(entry)
            }
            Some(group) => {
                // A subtitle is Pango markup by default, and a host may
                // legally contain '&' or '<'.
                let subtitle = match group.host.as_deref() {
                    Some(host) => format!("{host}\nClose all tabs to change the host"),
                    None => "This computer".to_string(),
                };
                rows.add(
                    &adw::ActionRow::builder()
                        .title("Host")
                        .subtitle(subtitle)
                        .use_markup(false)
                        .build(),
                );
                None
            }
        };
        let name_row = adw::EntryRow::builder()
            .title("Name")
            .activates_default(true)
            .build();
        name_row.set_text(group.map_or("", |g| g.name.as_str()));
        let url_row = adw::EntryRow::builder()
            .title("Browser default URL")
            .activates_default(true)
            .build();
        url_row.set_text(group.and_then(|g| g.default_url.as_deref()).unwrap_or_default());
        rows.add(&name_row);
        rows.add(&url_row);

        // Stock Adwaita style class, so the CSS provider needs nothing added.
        let error = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .build();
        error.add_css_class("error");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        content.append(&rows);
        if remote {
            let note = gtk::Label::builder()
                .label("The browser runs on this computer and isn't exposed to remote tabs.")
                .xalign(0.0)
                .wrap(true)
                .build();
            note.add_css_class("dim-label");
            content.append(&note);
        }
        content.append(&error);

        let (title, apply) = match mode {
            SettingsMode::Create => ("New Remote Group", "Create"),
            SettingsMode::Edit => ("Group Settings", "Apply"),
        };
        let dialog = adw::AlertDialog::new(Some(title), None);
        dialog.set_extra_child(Some(&content));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("apply", apply);
        dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("apply"));
        dialog.set_close_response("cancel");

        // Live validation: the only gate on Apply. Also runs once up front, so
        // a hand-edited `state.json` opens the dialog already showing why.
        // Everything inside the dialog is held weakly: this closure lives on
        // widgets *inside* it, so strong references would be a cycle.
        let validate = Rc::new({
            let dialog = dialog.downgrade();
            let error = error.clone();
            let url_row = url_row.downgrade();
            let host_entry = host_entry.as_ref().map(|entry| entry.downgrade());
            move || {
                let (Some(dialog), Some(url_row)) = (dialog.upgrade(), url_row.upgrade()) else {
                    return;
                };
                let host = host_entry
                    .as_ref()
                    .and_then(|weak| weak.upgrade())
                    .map(|entry| entry.text().to_string());
                // An empty host just keeps Create disabled: "enter a host"
                // shouted at a fresh, empty form would be noise.
                let host_blank = host.as_deref() == Some("");
                let problem = host
                    .as_deref()
                    .filter(|host| !host.is_empty())
                    .and_then(|host| state::validate_host(host).err())
                    .map(|err| err.to_string())
                    .or_else(|| {
                        browser::normalize_default_url(&url_row.text())
                            .err()
                            .map(|err| err.to_string())
                    });
                match problem {
                    Some(message) => {
                        error.set_text(&message);
                        error.set_visible(true);
                        dialog.set_response_enabled("apply", false);
                    }
                    None => {
                        error.set_visible(false);
                        dialog.set_response_enabled("apply", !host_blank);
                    }
                }
            }
        });
        validate();
        url_row.connect_changed({
            let validate = validate.clone();
            move |_| validate()
        });
        if let Some(entry) = &host_entry {
            entry.connect_changed({
                let validate = validate.clone();
                move |_| validate()
            });
        }

        let id = group.map(|g| g.id);
        let input = self.input.clone();
        dialog.connect_response(Some("apply"), move |_, _| {
            // Apply is only pressable while these parse, and what the URL
            // normalizes to — scheme lowercased, implied `http://` filled in —
            // is what gets stored. `None` covers both "cleared" and the
            // unreachable error.
            let default_url = browser::normalize_default_url(&url_row.text())
                .ok()
                .flatten();
            let name = name_row.text().to_string();
            let msg = match (&host_entry, id) {
                (Some(entry), _) => Msg::CreateRemoteGroup {
                    host: entry.text().to_string(),
                    name,
                    default_url,
                },
                (None, Some(id)) => Msg::ApplyGroupSettings {
                    id,
                    name,
                    default_url,
                },
                (None, None) => return,
            };
            let _ = input.send(msg);
        });
        dialog.present(Some(&self.window));
    }
```

- [ ] **Step 4: Sidebar: host and state in the header, dimmed rows**

In `make_group_header`, directly after the title label is appended to `row_box` (after `3271`), add:

```rust
        // Remote groups name their host (unless the name already is the
        // host) and show its state: nothing when live, a spinner while
        // connecting, a warning with the reason when disconnected.
        if let Some(host) = &group.host {
            row_box.append(&gtk::Image::from_icon_name("network-server-symbolic"));
            if group.name != *host {
                let label = gtk::Label::builder().label(host.as_str()).build();
                label.add_css_class("dim-label");
                row_box.append(&label);
            }
            match self.host_state(host) {
                HostState::Live => {}
                HostState::Connecting => {
                    row_box.append(&gtk::Spinner::builder().spinning(true).build());
                }
                HostState::Disconnected(err) => {
                    let warning = gtk::Image::from_icon_name("dialog-warning-symbolic");
                    warning.add_css_class("tmux-warning");
                    warning.set_tooltip_text(Some(&err.message(host)));
                    row_box.append(&warning);
                }
            }
        }
```

In `make_row` (not the picker, which has the same line), after `row.add_css_class(group.css);` (`3354`), add:

```rust
        // Rows of a disconnected host stay selectable (their page explains
        // and offers Reconnect) but read as unavailable.
        if let Some(host) = &group.host
            && matches!(self.host_state(host), HostState::Disconnected(_))
        {
            row.add_css_class("host-disconnected");
        }
```

In `src/lib.rs`, extend the `set_global_css` string: after the line `row.group-header-placeholder label { … }`, add:

```css
         row.host-disconnected { opacity: 0.55; }
```

- [ ] **Step 5: Verify**

Run: `cargo fmt && cargo build && cargo clippy --all-targets && cargo test`
Expected: clean.

Manual check with the harness from the top (no seed needed):
1. `Ctrl+Shift+H` opens "New Remote Group". Create stays disabled while Host is empty. Typing `-x` shows "A host can't start with '-'…" and keeps it disabled. `me@some host` shows the space error.
2. Enter `localhost` with an empty name and press Create.
   - The group appears named `localhost` with a server icon and a spinner.
   - Its tab shows "Connecting to localhost…", then a remote shell.
   - The spinner disappears.
3. Enter an unresolvable host (`nohost.invalid`) and press Create. The group stays, with a warning icon whose tooltip is the Unreachable message. Its tab shows the same message and a Reconnect button, and its sidebar row is dimmed but selectable.
4. `Ctrl+Shift+R` on the remote group:
   - The read-only Host row reads `localhost` / "Close all tabs to change the host".
   - The browser note is shown.
   - Name and URL still apply.
5. `Ctrl+Shift+R` on a local group: the Host row reads "This computer" and there is no browser note.
6. Start the harness with `PATH` holding a fake `ssh` that prints `OpenSSH_8.3p1` to stderr, for example `mkdir -p /tmp/ks-t/bin && printf '#!/bin/sh\necho OpenSSH_8.3p1 >&2\n' > /tmp/ks-t/bin/ssh && chmod +x /tmp/ks-t/bin/ssh`, then prefix `PATH=/tmp/ks-t/bin:$PATH`.
   - The server button is insensitive, with the 8.4 reason as tooltip.
   - `Ctrl+Shift+H` shows the same reason.
   - A seeded remote group loads disconnected with the SshUnsupported message.

- [ ] **Step 6: Commit**

```bash
git add src/app.rs src/lib.rs
git commit -m "Create remote groups from the sidebar and show each host's state"
```

---

### Task 14: App — cross-host drag-and-drop feedback, move refusal, group picker

**Depends on:** Task 13

**Files:**
- Modify `src/app.rs`:
  - `App` struct and `init` literal (new `host_keys`)
  - `move_active_tab` (`2256-2283`)
  - `show_group_picker` (`2288-2369`)
  - `drop_tab` (`2768-2797`)
  - `drop_tab_on_group` (`2813-2842`)
  - `rebuild_list` (`2864-2867`)
  - `make_group_header` drop target (`3295-3299`)
  - `make_row` drag source and drop target (`3359-3377`)
  - `dispatch_sidebar_drop` (`3521-3545`)
  - new pure fns `parse_drop_payload`, `drop_refused`, method `sidebar_drop_target`, `host_key`
  - tests
- Modify `src/lib.rs`: CSS `.drop-refused`.

**Interfaces:**
- Consumes (Task 1): `state::drop_allowed`.
- Produces:
  - `fn parse_drop_payload(payload: &str) -> Option<(&str, usize, Option<&str>)>` (pure, tested)
  - `fn drop_refused(payload: &str, target_key: &str) -> bool` (pure, tested)
  - `const CROSS_HOST_TOAST: &str = "Can't move a tab between hosts";`
  - payloads `tab:<id>:<host-key>` / `group:<id>`, where `<host-key>` is `local` or an index into `App::host_keys`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/app.rs`:

```rust
    // --- cross-host drag and drop ----------------------------------------

    #[test]
    fn payloads_carry_kind_id_and_host_key() {
        assert_eq!(parse_drop_payload("tab:7:local"), Some(("tab", 7, Some("local"))));
        assert_eq!(parse_drop_payload("tab:7:2"), Some(("tab", 7, Some("2"))));
        assert_eq!(parse_drop_payload("group:3"), Some(("group", 3, None)));
        assert_eq!(parse_drop_payload("tab:x:local"), None);
        assert_eq!(parse_drop_payload("nonsense"), None);
    }

    #[test]
    fn a_tab_from_another_host_is_refused() {
        assert!(drop_refused("tab:7:0", "local"));
        assert!(drop_refused("tab:7:local", "1"));
        assert!(drop_refused("tab:7:0", "1"));
        assert!(!drop_refused("tab:7:1", "1"));
        assert!(!drop_refused("tab:7:local", "local"));
    }

    #[test]
    fn groups_may_always_be_reordered_and_unknown_payloads_are_left_to_the_drop() {
        assert!(!drop_refused("group:3", "1"));
        assert!(!drop_refused("tab:7", "1"));
        assert!(!drop_refused("garbage", "local"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib -- app::tests::payloads app::tests::a_tab_from app::tests::groups_may`
Expected: compile error, `cannot find function parse_drop_payload`.

- [ ] **Step 3: Write the implementation**

Next to `CTRL_V` (`145`), add:

```rust
/// Toast for a tab move across hosts, the drop-time authority's refusal.
const CROSS_HOST_TOAST: &str = "Can't move a tab between hosts";
```

In `App`, after `drag_handle_icon` (`311`), add:

```rust
    /// Per-render host table behind the `tab:<id>:<host-key>` drag payloads:
    /// the key is `local` or an index into this list, so a `:` in a
    /// destination cannot break the payload. Rebuilt by `rebuild_list`.
    host_keys: RefCell<Vec<String>>,
```

and in `init`'s literal add `host_keys: RefCell::new(Vec::new()),` after `drag_handle_icon,`.

In `rebuild_list`, directly after the `while let Some(row) = … { self.tab_list.remove(&row); }` loop (`2865-2867`), add:

```rust
        {
            let mut keys = self.host_keys.borrow_mut();
            keys.clear();
            for host in self.groups.iter().filter_map(|g| g.host.as_ref()) {
                if !keys.contains(host) {
                    keys.push(host.clone());
                }
            }
        }
```

Add to `impl App` (next to `make_row`):

```rust
    /// The drag-payload key of a host in the current render.
    fn host_key(&self, host: Option<&str>) -> String {
        let Some(host) = host else {
            return "local".to_string();
        };
        self.host_keys
            .borrow()
            .iter()
            .position(|known| known == host)
            .map_or_else(|| "unknown".to_string(), |index| index.to_string())
    }

    /// A sidebar drop target for `row`, whose group runs on the host keyed
    /// `host_key`. The payload is preloaded, so hovering can already tell a
    /// cross-host tab: `enter`/`motion` answer no action (the compositor's
    /// no-drop cursor) and tint the row. The drop itself still goes through
    /// `dispatch_sidebar_drop` and the drop handlers, which stay the
    /// authority.
    fn sidebar_drop_target(
        &self,
        row: &gtk::ListBoxRow,
        target: SidebarDropTarget,
        host_key: String,
    ) -> gtk::DropTarget {
        let drop = gtk::DropTarget::new(gtk::glib::Type::STRING, gtk::gdk::DragAction::MOVE);
        drop.set_preload(true);
        let judge = Rc::new({
            let row = row.downgrade();
            move |drop: &gtk::DropTarget| -> gtk::gdk::DragAction {
                let refused = drop
                    .value()
                    .and_then(|value| value.get::<String>().ok())
                    .is_some_and(|payload| drop_refused(&payload, &host_key));
                if let Some(row) = row.upgrade() {
                    if refused {
                        row.add_css_class("drop-refused");
                    } else {
                        row.remove_css_class("drop-refused");
                    }
                }
                if refused {
                    gtk::gdk::DragAction::empty()
                } else {
                    gtk::gdk::DragAction::MOVE
                }
            }
        });
        drop.connect_enter({
            let judge = judge.clone();
            move |drop, _, _| judge(drop)
        });
        drop.connect_motion({
            let judge = judge.clone();
            move |drop, _, _| judge(drop)
        });
        drop.connect_leave({
            let row = row.downgrade();
            move |_| {
                if let Some(row) = row.upgrade() {
                    row.remove_css_class("drop-refused");
                }
            }
        });
        let input = self.input.clone();
        drop.connect_drop(move |_, value, _, _| dispatch_sidebar_drop(&input, value, target));
        drop
    }
```

In `make_group_header`, replace:

```rust
        let drop = gtk::DropTarget::new(gtk::glib::Type::STRING, gtk::gdk::DragAction::MOVE);
        let target = SidebarDropTarget::Header { group: group.id };
        let input = self.input.clone();
        drop.connect_drop(move |_, value, _, _| dispatch_sidebar_drop(&input, value, target));
        header.add_controller(drop);
```

with:

```rust
        let target = SidebarDropTarget::Header { group: group.id };
        let key = self.host_key(group.host.as_deref());
        header.add_controller(self.sidebar_drop_target(&header, target, key));
```

In `make_row`, replace the drag source's payload and the drop target (`3359-3377`):

```rust
        let drag = gtk::DragSource::builder()
            .actions(gtk::gdk::DragAction::MOVE)
            .build();
        let src_id = tab.id;
        drag.connect_prepare(move |_, _, _| {
            Some(gtk::gdk::ContentProvider::for_value(
                &format!("tab:{src_id}").to_value(),
            ))
        });
        row.add_controller(drag);

        let drop = gtk::DropTarget::new(gtk::glib::Type::STRING, gtk::gdk::DragAction::MOVE);
        let target = SidebarDropTarget::Tab {
            tab: tab.id,
            group: group.id,
        };
        let input = self.input.clone();
        drop.connect_drop(move |_, value, _, _| dispatch_sidebar_drop(&input, value, target));
        row.add_controller(drop);
```

with:

```rust
        // The host key rides along so every drop target can judge the drag
        // while it hovers, without looking the tab up.
        let key = self.host_key(group.host.as_deref());
        let drag = gtk::DragSource::builder()
            .actions(gtk::gdk::DragAction::MOVE)
            .build();
        let src_id = tab.id;
        let payload_key = key.clone();
        drag.connect_prepare(move |_, _, _| {
            Some(gtk::gdk::ContentProvider::for_value(
                &format!("tab:{src_id}:{payload_key}").to_value(),
            ))
        });
        row.add_controller(drag);

        let target = SidebarDropTarget::Tab {
            tab: tab.id,
            group: group.id,
        };
        row.add_controller(self.sidebar_drop_target(&row, target, key));
```

Replace `dispatch_sidebar_drop`'s parsing (`3526-3534`):

```rust
    let Ok(payload) = value.get::<String>() else {
        return false;
    };
    let Some((kind, id)) = payload.split_once(':') else {
        return false;
    };
    let Ok(src) = id.parse::<usize>() else {
        return false;
    };
```

with:

```rust
    let Ok(payload) = value.get::<String>() else {
        return false;
    };
    let Some((kind, src, _)) = parse_drop_payload(&payload) else {
        return false;
    };
```

and update its doc comment's first line to `/// Parse a namespaced sidebar DnD payload ("tab:<id>:<host-key>" / "group:<id>") and send`.

After `dispatch_sidebar_drop`, add:

```rust
/// Split a sidebar payload into kind, id and (for tabs) host key. Pure.
fn parse_drop_payload(payload: &str) -> Option<(&str, usize, Option<&str>)> {
    let mut parts = payload.splitn(3, ':');
    let kind = parts.next()?;
    let id = parts.next()?.parse().ok()?;
    Some((kind, id, parts.next()))
}

/// Would dropping `payload` on a row whose group is keyed `target_key` move
/// a tab between hosts? Group payloads never are (reordering groups is always
/// allowed), and a payload without a key is left to the drop-time check.
/// Pure.
fn drop_refused(payload: &str, target_key: &str) -> bool {
    matches!(parse_drop_payload(payload), Some(("tab", _, Some(key))) if key != target_key)
}
```

In `drop_tab`, directly after `if src == dest { return; }`, add:

```rust
        let group_of = |id: usize| self.tabs.iter().find(|t| t.id == id).map(|t| t.group);
        let (Some(src_group), Some(dest_group)) = (group_of(src), group_of(dest)) else {
            return;
        };
        if !state::drop_allowed(
            self.group_host(src_group).as_deref(),
            self.group_host(dest_group).as_deref(),
        ) {
            self.show_toast(CROSS_HOST_TOAST);
            return;
        }
```

In `drop_tab_on_group`, directly after `if !self.groups.iter().any(|g| g.id == group) { return; }`, add:

```rust
        let src_group = self.tabs[si].group;
        if !state::drop_allowed(
            self.group_host(src_group).as_deref(),
            self.group_host(group).as_deref(),
        ) {
            self.show_toast(CROSS_HOST_TOAST);
            return;
        }
```

(`si` comes from the `position` lookup just above, so the index is valid.)

Two things to know, not to change: because `enter`/`motion` answer no action for a cross-host tab, GTK never emits `drop` for it, so the toast in the three handlers above is only reached through a stale payload (a `rebuild_list` renumbering `host_keys` mid-drag) or the picker — the drop-time check is the authority, the cursor is the feedback. And if a compositor does not emit `leave` after a refused hover, the `drop-refused` tint lasts until the next `rebuild_list`; manual check 1 covers this.

In `move_active_tab`, replace:

```rust
        let Some(active) = self.active else { return };
        let target = match target {
            Some(id) if self.groups.iter().any(|g| g.id == id) => id,
            Some(_) => return, // group vanished while the picker was open
            None => self.create_group(),
        };
```

with:

```rust
        let Some(active) = self.active else { return };
        let Some(src_group) = self.tabs.iter().find(|t| t.id == active).map(|t| t.group) else {
            return;
        };
        let src_host = self.group_host(src_group);
        let target = match target {
            Some(id) if self.groups.iter().any(|g| g.id == id) => {
                if !state::drop_allowed(src_host.as_deref(), self.group_host(id).as_deref()) {
                    self.show_toast(CROSS_HOST_TOAST);
                    return;
                }
                id
            }
            Some(_) => return, // group vanished while the picker was open
            // A new group on the tab's own host: a move never changes hosts.
            None => {
                let id = self.create_group();
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    group.host = src_host;
                }
                id
            }
        };
```

In `show_group_picker`, replace the group-row loop and the "New group" row (`2296-2330`) with:

```rust
        let active_host = self.group_host(current_group);
        let mut first_row: Option<gtk::ListBoxRow> = None;
        for group in self.groups.iter().filter(|g| g.id != current_group) {
            let members: Vec<&Tab> = self.tabs.iter().filter(|t| t.group == group.id).collect();
            let name = if group.name.is_empty() {
                // unnamed group: fall back to its last-active tab's title
                &members
                    .iter()
                    .find(|t| t.id == group.last_active)
                    .unwrap_or(&members[0])
                    .title
            } else {
                &group.name
            };
            let text = match &group.host {
                Some(host) if host != name => format!("{name} ({}) · {host}", members.len()),
                _ => format!("{} ({})", name, members.len()),
            };
            let label = gtk::Label::builder()
                .label(text)
                .halign(gtk::Align::Start)
                .margin_start(6)
                .build();
            let row = gtk::ListBoxRow::builder().child(&label).build();
            row.set_widget_name(&group.id.to_string());
            row.add_css_class(group.css);
            let reachable = mode == PickerMode::Jump
                || state::drop_allowed(active_host.as_deref(), group.host.as_deref());
            if reachable {
                first_row.get_or_insert(row.clone());
            } else {
                // Shown, so the list still reads as "all groups", but a tab
                // cannot move to another host.
                row.set_sensitive(false);
                row.set_activatable(false);
                row.set_selectable(false);
                row.set_tooltip_text(Some("On another host: tabs can't move between hosts"));
            }
            list.append(&row);
        }
        if mode == PickerMode::Move {
            let new_label = gtk::Label::builder()
                .label(match &active_host {
                    Some(host) => format!("New group on {host}"),
                    None => "New group".to_string(),
                })
                .halign(gtk::Align::Start)
                .margin_start(6)
                .build();
            let new_row = gtk::ListBoxRow::builder().child(&new_label).build();
            new_row.set_widget_name("new");
            list.append(&new_row);
            first_row.get_or_insert(new_row);
        }
```

In `src/lib.rs`'s CSS, after the `row.host-disconnected` line (Task 13), add:

```css
         row.drop-refused { background: alpha(#e01b24, 0.18); }
```

- [ ] **Step 4: Run the tests and verify**

Run: `cargo test --lib app::tests`
Expected: all pass (3 new).

Run: `cargo fmt && cargo build && cargo clippy --all-targets && cargo test`
Expected: clean.

Manual check with the harness from the top, with one local group and one `localhost` group:
1. Drag a local tab over a row or the header of the `localhost` group. The cursor shows no-drop and the row turns red-tinted. Moving away clears the tint. Dropping anyway does nothing.
2. Drag within the same host, and reorder groups by dragging headers. Both still work, with no tint.
3. `Ctrl+Shift+M` on a local tab: the `localhost` group row is greyed out with a tooltip and cannot be chosen by arrows or click. "New group" creates a local group.
4. `Ctrl+Shift+M` on the remote tab offers "New group on localhost". Choosing it creates a remote group, and the tab keeps running.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs src/lib.rs
git commit -m "Refuse cross-host tab moves with no-drop feedback in the sidebar and picker"
```

---

### Task 15: App — `kabelsalat run` into remote groups

**Depends on:** Task 14

**Files:**
- Modify `src/app.rs`: the `Msg::SpawnCommand` handler (`1161-1232`; the `cwd` line changed in Task 8).

**Interfaces:**
- Consumes:
  - Task 8: `Msg::SpawnCommand { cwd: Option<PathBuf>, .. }`
  - Task 11: `add_tab`, which keeps a remote tab pending until its host is live and spawns it at once when the host is live
- Produces: nothing new.

- [ ] **Step 1: Write the change**

In the `Msg::SpawnCommand` handler, replace the comment block and line starting `// Without tmux, VTE's spawn just fails for a nonexistent` through `let cwd = cwd.filter(|dir| dir.is_dir());` with:

```rust
                // A remote group's directory is a path on its host (or None,
                // the remote home): no local is_dir() check applies to it.
                // Locally: without tmux, VTE's spawn just fails for a
                // nonexistent directory and the callback only logs to stderr,
                // leaving a permanently empty tab even though the CLI already
                // printed a uuid and exited 0. Drop the cwd so the tab starts
                // in the default location instead (tmux itself tolerates a
                // missing -c directory, so this only matters for the no-tmux
                // path).
                let cwd = if self.group_host(group_id).is_some() {
                    cwd
                } else {
                    cwd.filter(|dir| dir.is_dir())
                };
```

Nothing else changes, and the no-focus-steal rule holds for remote tabs too:
- `add_tab` stays inert for them.
- `spawn_remote_tab` grabs focus only for the already-active tab, and a CLI tab never is.
- The command reaches the host quoted twice, once in `TmuxCtl::spawn_argv` and once in `mux_argv`, both covered by tests.

- [ ] **Step 2: Verify**

Run: `cargo fmt && cargo build && cargo clippy --all-targets && cargo test`
Expected: clean.

Manual check. Start the harness in a `dbus-run-session -- bash` shell (CLI and GUI must share the bus), with a `localhost` group named `lo`, and focus a local tab:
1. `cargo run -- run -g lo -- sh -c 'echo "it'\''s $HOME"; sleep 30'` prints a uuid.
   - A new tab appears in `lo` without focus moving or the window raising.
   - It shows `it's /home/<you>`: expanded remotely, and the quote intact.
2. `cargo run -- run -g lo --cwd /tmp -- pwd; sleep 5` prints `/tmp`, run on the host.
3. `cargo run -- run -g lo -- pwd` prints the remote home, even when called from a directory that does not exist on the host.
4. With the host disconnected, `run -g lo -- ls` still prints a uuid. The tab shows the disconnected page, and runs `ls` after Reconnect.
5. `cargo run -- run -g brandnew --create -- pwd` creates a **local** group.

- [ ] **Step 3: Commit**

```bash
git add src/app.rs
git commit -m "Run CLI commands in remote groups without a local cwd check"
```

---

### Task 16: README — remote groups

**Depends on:** Task 15

**Files:**
- Modify `README.md`:
  - feature bullets (`5-20`)
  - `## Keyboard shortcuts` table (`50-64`)
  - `## Command line` paragraph (`77-83`)
  - new `## Remote groups` section before `## Requirements` (`103`)
  - `## Requirements` list (`105-107`)

**Interfaces:** none.

- [ ] **Step 1: Write the documentation**

In the feature bullets, after the "**Tabs live in groups.**" bullet, add:

```markdown
- **Groups can live on another machine.** A remote group's tabs are tmux
  sessions on that host, reached over one shared ssh connection — they
  survive the GUI, the network dropping and the next login just like local
  ones.
```

In the shortcuts table, after the `Ctrl+Shift+N` row, add:

```markdown
| `Ctrl+Shift+H` | New remote group |
```

In `## Command line`, after the sentence ending "which defaults to the caller's.", add:

```markdown
For a remote group the command runs on its host: `--cwd` is a path there
(default: the remote home) and the caller's directory is ignored. `--create`
always makes a local group.
```

Before `## Requirements`, add:

````markdown
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
````

In `## Requirements`, after the tmux bullet, add:

```markdown
- Optional: OpenSSH 8.4 or newer for remote groups (tmux 3.2 or newer on
  each remote host)
```

- [ ] **Step 2: Verify**

Run: `grep -n "Remote groups\|Ctrl+Shift+H\|remote home" README.md`
Expected: the new section, the shortcut row, and the CLI note are all present.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "README: document remote groups"
```

---

### Task 17: Full verification

**Depends on:** Tasks 1–16

**Files:** none (fix-ups only if a check fails, committed with a message that names the fix).

- [ ] **Step 1: Format, lint, test**

Run: `cargo fmt --check`
Expected: no output.

Run: `cargo clippy --all-targets`
Expected: no warnings introduced by this branch. Compare with `git stash; cargo clippy --all-targets; git stash pop` on `main` if unsure.

Run: `cargo test`
Expected: all tests pass, including:
- `state::tests` (per-host restore, `validate_host`, `drop_allowed`, host/queue round-trip)
- `remote::tests` (26)
- `tmuxctl::tests` (remote target)
- `remote_worker::tests` (16)
- `cli::tests` (remote targets)
- `app::tests` (reattach guard, payloads)

- [ ] **Step 2: Spec coverage walk-through**

Check each spec section against the code (`git diff main --stat`, then read the diff):
- §1:
  - `SavedGroup::host` with `serde(default)`
  - `validate_host`
  - pending kills (top-level, see decision 1)
  - `reconcile_local` / `reconcile_remote` / `remote_hosts`
  - the `drop_allowed` gates in `move_active_tab`, both drops and the picker
- §2:
  - `detect_ssh` + `parse_ssh_version`
  - control path and master options
  - tabs on `ControlMaster=no` + `BatchMode=yes`
  - `ssh -O exit` on quit
  - `auth_mode` order
  - setsid + null stdin
  - host check order (`tmux -V`, start + `source-file -`, `list-sessions`)
  - `classify`
  - one serial worker per host
  - `Target`
- §3:
  - spawn only while Live
  - 255 → `-O check` → Disconnected
  - other statuses → async list → gone/reattach
  - crash from `pane_dead`
  - the disconnected page with Reconnect
  - the close outbox
  - respawn and cwd through the worker (300 ms)
  - CLI run
- §4:
  - create dialog + `Ctrl+Shift+H`
  - read-only host row
  - browser note
  - header icon, host and state
  - dimmed rows
  - `set_preload`, the `enter`/`motion` refusal, `drop-refused`, `tab:<id>:<host-key>`, the drop-time toast
  - the picker
- §5: every unit test listed in the spec exists (search the test names above).

- [ ] **Step 3: Manual verification (for the human; spec §5)**

Run each against `sshd` on localhost or a container, using the harness from the top of this plan:

1. **Key login.** Create a `localhost` group and open tabs, then restart kabelsalat. The sessions reattach with their scrollback.
2. **Password login via gnome-ssh-askpass.** Use a host that needs a password, with `openssh-askpass` installed: exactly one askpass prompt per host, and none for further tabs.
3. **No askpass available.**
   - Start with `SSH_ASKPASS=` unset.
   - Hide `/usr/libexec/openssh/*askpass` by running in a container or with `PATH` stripped. Make sure the fixed paths are absent too.
   - The host is refused with the AuthNeedsAskpass message.
4. **Drop the connection.** Stop `sshd` (and kill the `sshd: user@notty` process) or drop the network.
   - Within about 45 s every tab of the host shows Disconnected, with no loop.
   - Reconnect works once sshd is back.
5. **Remote tmux < 3.2 or missing.** Use a container with tmux 3.1c (Debian bullseye) or none. The host is refused with TmuxTooOld / TmuxMissing, and no tab is spawned.
6. **Cross-host drag.** Drag a tab onto a group on another host: no-drop cursor and red tint; the drop does nothing.
7. **No terminal prompts.** Start kabelsalat from a terminal (`cargo run` in a shell) and connect to a host needing a password without askpass. No prompt ever appears in that terminal.
8. **Two installations, one host.** Run two local installations with separate `XDG_STATE_HOME` **and** separate `XDG_RUNTIME_DIR`, because the runtime dir also holds the local tmux socket and the ssh control sockets. Point both at the same host. Neither sees nor disturbs the other's tabs, and `tmux -L kabelsalat ls` on the host lists both sets.

- [ ] **Step 4: Commit any fix-ups**

If Steps 1–3 required changes:

```bash
git add -A src README.md
git commit -m "Fix <what the verification found>"
```

Otherwise there is nothing to commit.
