# CLI interface and agent skill

Date: 2026-07-26

## Problem

kabelsalat has no external control surface. An agent (Claude Code) working in a
project cannot start a long-running, persistent, visible terminal — it can only
run captured subprocesses that die with it. The concrete case: an agent in
`~/Projects/erhebimus` wants to launch `claude` in a new tab inside a specific
kabelsalat group, and see which groups exist in order to pick one.

## Goals

- List the groups of the running instance.
- Launch a command in a new tab inside a named group.
- Ship a skill so an agent knows the interface exists and how to use it.

## Non-goals

Listing tabs, creating groups, closing tabs, focusing tabs, sending input to an
existing tab, reading a tab's output. The surface stays at two working
subcommands plus `help`; more can be added once the transport exists.

## CLI surface

```
kabelsalat                                    # unchanged: start the GUI, or activate the running one
kabelsalat groups                             # list groups
kabelsalat run --group <name|uuid> [--cwd DIR] -- <command> [args...]
kabelsalat help | --help | -h                 # usage
```

A plain `kabelsalat` does not *raise* the existing window — relm4's activate
handler only sets it visible, which is a no-op on a window that already is. That
was true before this work too; it is stated here because it is easy to document
as "raise" and be wrong.

`help` is answered locally and never contacts the bus, so it works with or
without a running instance.

`groups` prints one line per group, tab-separated, no header:

```
<uuid>\t<name>\t<tab-count>
```

The name field is empty for unnamed groups. Output order matches the GUI's
sidebar order.

`run` takes everything after `--` as argv, not as a shell string:

```
kabelsalat run --group erhebimus -- claude --model opus
kabelsalat run --group web -- bash -lc 'cd frontend && npm run dev'
```

Shell syntax is therefore explicit rather than implicit. `--cwd` defaults to the
calling process's working directory, which GApplication already forwards
(`GApplicationCommandLine::cwd()`). `run` prints the new tab's uuid on stdout so
the caller has a confirmation token.

Short flags: `-g` for `--group`. No short flag for `--cwd`.

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | success |
| 1 | kabelsalat is not running |
| 2 | usage error (unknown flag, missing `--group`, missing command) |
| 3 | group not found, or name matched more than one group |

### Group resolution

Resolve `--group <value>` against the live group list:

1. Exact match on `uuid` wins.
2. Otherwise exact, case-sensitive match on `name`.
3. Zero matches → exit 3, `no group matching '<value>'`.
4. More than one name match → exit 3, listing each candidate's uuid so the
   caller can retry unambiguously.

Unnamed groups (`name == ""`) are reachable only by uuid; an empty `--group`
value is a usage error, not a match against them.

Rationale: `SavedGroup.id` is reused after a delete (`next id = max(id)+1`) and
is process-local, so it is never part of the external interface. `uuid` is
stable and persisted; `name` is neither unique nor mandatory, so it is a
convenience with a defined failure mode.

## Transport

The app already owns `de.nereide.kabelsalat` on the session bus for GTK's
single-instance behaviour, and currently discards a second launch's argv. We opt
into `gio::ApplicationFlags::HANDLES_COMMAND_LINE`, which makes GLib forward
argv, cwd and environment to the running instance and route `print()` output and
the exit status back to the caller.

`lib.rs::run()` becomes:

```rust
pub fn run() {
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let cli = cli::parse(&args);          // usage errors exit 2 here

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.connect_command_line(handle_command_line);

    if cli.needs_instance() {
        // Never start a GUI for a subcommand.
        match instance_is_running() {
            Ok(true) => std::process::exit(app.run_with_args(&args).get().into()),
            Ok(false) => eprintln!("kabelsalat: not running"),
            Err(err) => eprintln!("kabelsalat: not running ({err})"),
        }
        std::process::exit(cli::EXIT_NOT_RUNNING.into());
    }

    relm4::gtk::init()?;
    adw::init()?;
    relm4::set_global_css(...);
    RelmApp::from_app(app).with_args(args).run::<app::App>(());
}
```

Two details this depends on, both verified against relm4 0.11.0:

- `RelmApp::run()` discards the `ExitCode` returned by `run_with_args`
  (`relm4-0.11.0/src/app.rs:191`), so the subcommand path must call
  `run_with_args` directly to propagate the remote exit status. It does not need
  `RelmApp`: a remote invocation never builds a window.
- relm4 builds the component in `connect_startup`
  (`relm4-0.11.0/src/app.rs:162`), not `connect_activate`. `startup` fires on
  the primary instance before `command-line`, so the shared snapshot described
  below is always populated when the handler runs.

With `HANDLES_COMMAND_LINE` set, GApplication no longer emits `activate` by
itself, so the no-subcommand branch of the handler must call `app.activate()`
explicitly or the window never becomes visible.

The "is it running?" test is a `NameHasOwner` call for `APP_ID` on the session
bus, made before any GTK contact.

The obvious test — `register()` then `is_remote()` — is wrong, and it took a
review to see why: `g_application_register()` emits `startup` as a side effect
when the process becomes the *primary* instance, and `GtkApplication`'s startup
calls `gtk_init()`, which prints `Gtk-WARNING: Failed to open display` and calls
`exit(1)` from inside GTK when there is no display. So `kabelsalat groups` on a
tty, over ssh, or in a systemd unit died inside GTK and never printed our own
message; the exit code was 1 only because GTK happens to choose 1. Querying the
bus for name ownership decides the same question without ever constructing a
GTK anything.

A bus that cannot be reached, a failed call, and an undecodable reply all mean
the same thing operationally — there is no instance to talk to — so all three
exit `EXIT_NOT_RUNNING`, with the underlying error included in the message so a
broken bus stays distinguishable from an absent app.

One race remains and is accepted: if the GUI exits between the ownership check
and `run_with_args`, this process can still become primary. No D-Bus API offers
an atomic connect-only-if-owned, so any check-then-act scheme has this window.

### Answering without deadlocking

The `command-line` handler runs on the primary instance's main thread, the same
thread that drives the relm4 component. A relm4 component can only be reached by
sending it a message and awaiting the result on that same main loop, which would
deadlock. So `App` instead publishes a read-only snapshot:

```rust
pub struct GroupInfo {
    pub uuid: String,
    pub name: String,
    pub tabs: usize,
}

pub struct Control {
    pub groups: Arc<Mutex<Vec<GroupInfo>>>,
    pub sender: relm4::Sender<Msg>,
}

static CONTROL: OnceLock<Control> = OnceLock::new();
```

`App` sets `CONTROL` during `init` and refreshes the snapshot at the existing
state-save choke point — the same place that already runs whenever groups or
tabs change, so no new invalidation paths are introduced.

The handler therefore:

1. Reads the snapshot (synchronous, no deadlock) to answer `groups`.
2. Resolves `--group` against the snapshot for `run`, and exits 3 on failure
   without sending anything.
3. On success, mints the tab uuid, sends a one-way
   `Msg::SpawnCommand { group_uuid, cwd, argv, tab_uuid }`, prints the tab
   uuid, and exits 0.

Validation happens before the message is sent, so the exit code is meaningful
without a reply channel. Be precise about what that does *not* promise: exit 0
means the request was validated against the snapshot and queued, not that the
tab exists. If the group disappears in the moment between the two, the handler
gives up and reports to the GUI's stderr, invisible to a caller that has already
been told 0. The window is sub-millisecond and the human-scale `groups`-then-
`run` sequence is unaffected, because a rename or close republishes the snapshot
and the next `run` then fails cleanly with exit 3. A `run` that resolves but
fails to spawn for other reasons (a tmux error, say) surfaces in the GUI as a
crashed tab, exactly as a GUI-initiated tab would.

## Spawn path

`Msg::SpawnCommand` looks up the group by uuid and calls the existing
`add_tab`, which gains an `Option<&[String]>` command parameter threaded down
into `spawn_backing`:

- **tmux path**: `TmuxCtl::spawn_argv(uuid, cwd, command)` shell-quotes each
  argv element and joins them into the single command string tmux expects,
  replacing `$SHELL`. Quoting is done by us rather than relying on tmux's own
  argv joining, which loses the original word boundaries.
- **no-tmux path**: `spawn_shell(terminal, cwd, command)` passes argv straight
  to VTE, which is exact. The app keeps working without tmux, as required.

The working directory needs escaping of its own, which is not obvious: tmux runs
`format_single()` over the `-c` argument, and tmux's format syntax includes
`#(shell-command)`. An unescaped `#(...)` in a path therefore *executes*, and
does so even when the directory does not exist, because tmux tolerates a bad
`-c`. `spawn_argv` escapes `#` as `##` before handing the path over. This
matters more here than it used to: before the CLI, `cwd` only ever came from
`active_tab_cwd()` — a real path read back from tmux or VTE — whereas now it is
caller-supplied text. The escaping lives in `spawn_argv` so the GUI's own
new-tab path is covered by the same guard.

The initial tab title is the basename of `argv[0]` (e.g. `claude`), until the
program sets its own title via OSC.

Nothing new is persisted. `SavedTab` gains no field: the tmux session survives
restarts and reattaches with its command intact, and `respawn-pane` re-runs the
pane's original command.

What happens when the command finishes depends on how it finished, because the
tmux config sets `remain-on-exit failed` rather than `on`: a **non-zero** exit
keeps the pane, so the tab stays visible showing its status and the existing
`RestartTab` path re-runs it, while a **clean** exit ends the session, and
`Msg::ChildExited` then closes the tab and prunes the group if it emptied. The
skill documents this distinction, since an agent that promises the user a tab
which has already vanished is the failure mode worth preventing.

## GUI side effects: none

Creating a tab through the CLI does exactly one thing — the tab appears in its
group in the sidebar. Specifically it does **not**:

- activate or select the new tab,
- raise, present or focus the window,
- change the active group,
- open, close or otherwise touch the group's browser pane.

An agent spawning a tab must not steal keystrokes from whatever the user is
typing into another tab.

## Code layout

| File | Change |
|------|--------|
| `src/cli.rs` | **new**, pure: argv → `Cli` enum, group resolution over `&[GroupInfo]`, and `dispatch` deciding what to print and exit with. No GTK, no gio, no tmux, no I/O. Unit-tested like `state.rs`. |
| `src/control.rs` | **new**, the gio glue: the `CONTROL` snapshot, the one-way spawn request, and the `command-line` handler that feeds `cli::dispatch` and prints its outcome. |
| `src/lib.rs` | Build the `adw::Application` with the flag, register the `command-line` handler, implement the not-running check and the remote exit-status path. |
| `src/app.rs` | Publish the `CONTROL` snapshot; handle `Msg::SpawnCommand`; thread the command parameter through `add_tab`/`spawn_backing`/`spawn_shell`. |
| `src/tmuxctl.rs` | `spawn_argv` accepts an optional command; add shell quoting. Still never panics, still returns `Result`. |
| `skills/kabelsalat/SKILL.md` | **new**, the agent skill. |
| `scripts/install-skill.sh` | **new**, symlinks the skill into `~/.claude/skills/`. |
| `README.md`, `CLAUDE.md` | Document the CLI and the new invariant that `cli.rs` stays pure. |

Argument parsing is hand-rolled. The surface is two subcommands and three flags;
`clap` would add a dependency tree for roughly sixty lines of code, and the
parser is pure and directly unit-testable either way.

One dependency line did change, against the "no new crates" rule: `Cargo.toml`
gains `gio = { version = "0.22", features = ["v2_80"] }`. No crate is added —
`gio` was already in the tree via gtk4, `Cargo.lock` gains only a dependency
edge, and the version range is the one gtk4 itself declares, so unification is
guaranteed rather than lucky. The entry exists solely to enable the feature
gating `ApplicationCommandLine::print_literal`/`printerr_literal`. It raises no
floor: `v2_80` means glib ≥ 2.80, which GTK 4.18 already requires.

## Skill

`skills/kabelsalat/SKILL.md`, versioned in the repo alongside the CLI it
documents, installed by `scripts/install-skill.sh`:

```sh
ln -sfn "$PWD/skills/kabelsalat" ~/.claude/skills/kabelsalat
```

User-level rather than project-level, because an agent working in erhebimus,
textimus or surveillance is exactly who needs it.

Content:

- **Trigger**: when a command should run in a visible, persistent terminal the
  user can watch and interact with — a dev server, a long build, an interactive
  `claude` session — rather than as a captured subprocess.
- The two commands, with the `--` convention and worked examples.
- Group resolution rules, and that `groups` must be run first to learn valid
  names.
- Exit codes, with explicit handling guidance: **1 means the GUI is not running
  — report that to the user and stop; do not retry and do not try to start it.**
  3 means re-read the `groups` output and retry with a uuid.
- That the spawned tab is not focused, so the user must be told where it went.

## Testing

`cargo test` unit tests:

- `cli.rs` parsing: no args; `groups`; `run` with and without `--`; missing
  `--group`; empty `--group`; unknown flag; `--cwd` present and absent; flags
  appearing after `--` treated as part of the command.
- `cli.rs` resolution: uuid hit; unique name hit; duplicate names; no match;
  unnamed group not matched by an empty selector; uuid preferred over a name
  that happens to equal a uuid.
- `tmuxctl.rs`: `spawn_argv` without a command is byte-identical to today's
  output (regression guard); with a command, quoting survives spaces, single
  quotes and `$`.

Manual verification, since none of the D-Bus path is unit-testable:

1. `cargo build`, launch the GUI, create two groups, name one.
2. `kabelsalat groups` prints both, with the unnamed one showing an empty name.
3. `kabelsalat run -g <name> -- claude` creates a tab in that group, running
   claude, in the shell's cwd; the window does not raise and focus does not move.
4. `kabelsalat run -g <duplicate-name> -- true` exits 3 and lists uuids.
5. Quit the GUI; `kabelsalat groups` exits 1 with a clear message and no window.
6. Plain `kabelsalat` still starts the GUI, and a second plain `kabelsalat`
   still raises the existing window.

`cargo fmt` and `cargo clippy` before the work is called done.

## Risks

- `RelmApp::from_app` does not call relm4's `init()` the way `RelmApp::new`
  does, and that function is private, so the GUI path must call `gtk::init()`
  and `adw::init()` itself before `set_global_css` builds a `CssProvider`.
  Only on that path: constructing the `adw::Application` needs no display, so
  a subcommand still works without one.
- Setting `HANDLES_COMMAND_LINE` changes activation semantics for the existing
  plain-launch path. Manual check 6 exists specifically to catch a regression
  there.
- A `run` issued while the GUI is still starting up, before it has claimed the
  bus name, will exit 1. Acceptable; the window is only unregistered for a
  fraction of a second at startup.
