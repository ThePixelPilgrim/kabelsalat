# Android Pane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A group can host Firefox for Android (Fenix) running in Waydroid as a second pane next to its Chromium pane: kabelsalat starts a nested compositor and the Waydroid session in it, shows it as an "Android" tab in the group's pane area, publishes `KABELSALAT_ANDROID_CTL`/`KABELSALAT_ANDROID_ADB` into the group's tmux sessions, offers `kabelsalat android [screenshot|tap|type|key|resize]`, restores the pane after a restart, and teaches agents (skill) to drive Fenix over geckodriver + WebDriver BiDi — per the approved spec `docs/superpowers/specs/2026-10-07-android-pane-design.md`.

**Architecture:** A new `src/android.rs` mirrors `src/browser.rs`: a pure half (Waydroid status parsing, preconditions, argument builders, adb serial, ownership claim) and a process half (`Android` owns a klamottenkiste `WaylandPane` plus the `waydroid session start` child in its own process group; a worker thread watches the boot and reports back through a callback). `src/state.rs` gains the persisted `android_owner` plus pure pane-area decisions (`PaneKind`, which pane is in front after an open or close, what Alt+2 does). `src/cli.rs` gains the environment pair builder and the `android` command (parse, dispatch, control-socket line, reply parsing); `src/control.rs` does the one blocking unix-socket round trip. `src/app.rs` stays wiring: each group gets a `PaneHost` (an `adw::TabBar` over an `adw::TabView`) parented into the existing `browser_paned` end slot only while it is the active group's and holds a pane.

**Tech Stack:** Rust 2024, relm4 0.11 / GTK 4 / libadwaita 0.9.2 (`features = ["v1_5"]`; `adw::TabView`, `adw::TabBar`, `TabView::set_shortcuts` are all available — TabView/TabBar since 1.0 with no feature gate, `set_shortcuts` behind `v1_2`, which `v1_5` implies), klamottenkiste v0.2.1 (`WaylandPane::{new, wayland_socket, control_socket_path, startup_error, capture_frame, is_running, close}`), Waydroid CLI (`status`, `session start|stop`, `show-full-ui`, `prop get`), the nested compositor's control-socket protocol (`vendor/nested-wayland-session/src/protocol.rs`: one request line → one reply line, `ok` / `ok <data>` / `err <message>`).

## Global Constraints

From `CLAUDE.md` (binding, verbatim):

- Red-green TDD for every behaviour change: write the failing test first, run it and watch it fail for the expected reason, then write the minimal code that makes it pass, then refactor with the tests green. No implementation before its failing test; a test that passes on first run proves nothing and needs to be made to fail first.
- Logic that is hard to test under this rule belongs in the pure modules (`src/state.rs`, `src/cli.rs`, `src/claude.rs`, `src/tmuxctl.rs`) behind parameters that tests can fabricate (a directory, an output string, a `/proc` root), not in `src/app.rs`.
- The GTK layer in `src/app.rs` is the one untested exception: keep it to wiring, so the behaviour it wires is covered elsewhere.
- `src/state.rs` is pure logic — serde structs, persistence, and reconciliation planning. No GTK and no tmux calls belong here; it is what keeps the logic testable.
- `src/tmuxctl.rs` never panics: every fallible path returns `Result`. No `unwrap`/`expect` on tmux interaction.
- `src/app.rs` is the relm4 component holding all GUI state and side effects.
- The app must keep working when tmux is missing or older than 3.2 — it degrades to plain shells without session survival rather than erroring out.
- `src/cli.rs` is pure logic — argv parsing, group resolution, and the decision of what an invocation prints and exits with. No GTK, no gio, no tmux, no I/O; it is where the CLI's unit tests live. `src/control.rs` holds the gio glue and the group snapshot the command-line handler reads.
- A CLI-created tab must not steal focus: no activate, no window raise, no active-group change, and no touching the group's browser pane. `kabelsalat browser` is the one command that touches a pane — only the named group's, brought up hidden unless that group is active — under the same no-focus, no-raise, no-switch rules.
- `src/resume.rs` is the `kabelsalat resume` glue and never writes `state.json` — the GUI stays the state file's single writer.
- CI runs `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test` on every push. Run them yourself before claiming work is done.
- Never expose the user's email address in User-Agent strings or other outgoing request headers; use a neutral identifier instead (applies to the skill's download snippets).

From the spec (binding):

- CLI-created panes never raise, focus, switch groups or change `front_pane`; `OpenAndroid` adds a hidden page when the group isn't active. `kabelsalat android` joins `kabelsalat browser` as a command that touches a pane, under the same rules.
- The Android pane is claimed by one group at a time. Claiming while another group owns it is refused (exit 3 from the CLI, a notice in the GUI).
- At most one Browser and one Android per group, shown as tabs in the pane area. A group without panes looks as it does today.
- kabelsalat starts the compositor and the Waydroid session only. Fenix, geckodriver and adb authorisation are agent-side (skill).
- Degrade gracefully when Waydroid is missing, not initialised, or already running elsewhere: `Android::spawn` returns `Err` with a user-facing message, the GUI shows it, nothing panics, and every other feature keeps working.
- Env published into the owning group's tabs via the existing `tmux set-environment` path: `KABELSALAT_ANDROID_CTL=<control socket>`, `KABELSALAT_ANDROID_ADB=<serial>`. Both unset on teardown or death. Local groups only, like the CDP pair.
- `state.json` stays written by the GUI only.

Mechanics:

- Every task ends green: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`. `src/cli.rs` and `src/control.rs` are private modules, so an item added there that nothing uses yet is a `dead_code` error under `-D warnings` — tasks are cut so every new private item has a caller in the same commit. `src/android.rs` and `src/state.rs` are `pub mod`s, so their `pub` items never trip that lint.
- No new crates. `Cargo.toml` is not modified.
- Commit messages are plain imperative sentences matching `git log` (no prefixes), and carry **no attribution lines** (no `Co-Authored-By`, no "Generated with").
- Line numbers below are from `main` at `de787c8`; when a task earlier in this plan has already shifted them, find the spot by the quoted function or code instead.

---

### Task 1: `src/android.rs` — pure Waydroid half

**Files:**
- Create: `src/android.rs`
- Modify: `src/lib.rs:7-17` (module list)
- Test: `src/android.rs` (new `#[cfg(test)] mod tests` at the end of the file)

**Interfaces:**
- Consumes: nothing.
- Produces (used by Tasks 2, 7, 8):
  - `pub const WAYDROID_BINARY: &str = "waydroid"`, `pub const DEFAULT_ADB_SERIAL: &str = "192.168.240.112:5555"`, `pub const ANDROID_SUBDIR: &str = "android"`
  - `pub enum WaydroidStatus { NotInitialised, Stopped, Running { display: String } }`
  - `pub fn parse_status(output: &str) -> WaydroidStatus`
  - `pub enum Precondition { Ok, NoBinary, NotInitialised, ForeignSession(String) }` + `Display`
  - `pub fn check(status: Option<WaydroidStatus>, our_display: &str) -> Precondition`
  - `pub fn status_args() -> &'static [&'static str]`, `pub fn session_start_args() -> &'static [&'static str]`, `pub fn show_full_ui_args() -> &'static [&'static str]`
  - `pub fn client_env(display: &str) -> [(&'static str, String); 1]`
  - `pub fn adb_serial(status_output: &str) -> String`
  - `pub enum BootStep { Wait, Up, Foreign(String), NotInitialised }`, `pub fn boot_step(status: &WaydroidStatus, our_display: &str) -> BootStep`
  - `pub enum Claim { Free, Mine, Taken(String) }`, `pub fn claim(owner: Option<&str>, requester: &str) -> Claim`
  - `pub fn log_path(state_dir: &Path) -> PathBuf` → `<state_dir>/android/session.log`

Real `waydroid status` output (from `/usr/lib/waydroid/tools/actions/status.py`): stopped prints `Session:\tSTOPPED\nVendor type:\t…`; running prints `Session:\tRUNNING`, `Container:\t…`, `Vendor type:\t…`, `IP address:\t<ip or UNKNOWN>`, `Session user:\t…`, `Wayland display:\t<WAYLAND_DISPLAY>`; before `waydroid init` every subcommand prints `Waydroid is not initialized, run "waydroid init"` and exits 0.

- [ ] **Step 1: Write the failing tests**

Create `src/android.rs` containing only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: &str = "Session:\tRUNNING\n\
                           Container:\tRUNNING\n\
                           Vendor type:\tMAINLINE\n\
                           IP address:\t192.168.240.112\n\
                           Session user:\tchristoph(1000)\n\
                           Wayland display:\twayland-7\n";
    const STOPPED: &str = "Session:\tSTOPPED\nVendor type:\tMAINLINE\n";
    const NOT_INITIALISED: &str = "Waydroid is not initialized, run \"waydroid init\"\n";

    fn running(display: &str) -> WaydroidStatus {
        WaydroidStatus::Running {
            display: display.to_string(),
        }
    }

    #[test]
    fn status_parses_the_three_states() {
        assert_eq!(parse_status(STOPPED), WaydroidStatus::Stopped);
        assert_eq!(parse_status(NOT_INITIALISED), WaydroidStatus::NotInitialised);
        assert_eq!(parse_status(RUNNING), running("wayland-7"));
    }

    #[test]
    fn status_reads_the_display_line_wherever_it_is() {
        // An absolute WAYLAND_DISPLAY keeps its own colons-free path intact.
        let reordered = "Wayland display:\t/run/user/1000/wayland-3\nSession:\tRUNNING\n";
        assert_eq!(parse_status(reordered), running("/run/user/1000/wayland-3"));
    }

    #[test]
    fn status_running_without_a_display_line_has_an_empty_display() {
        assert_eq!(parse_status("Session:\tRUNNING\n"), running(""));
    }

    #[test]
    fn empty_or_garbage_status_output_is_stopped() {
        assert_eq!(parse_status(""), WaydroidStatus::Stopped);
        assert_eq!(
            parse_status("[12:00:01] something went sideways\n"),
            WaydroidStatus::Stopped
        );
    }

    #[test]
    fn check_covers_every_precondition() {
        assert_eq!(check(None, "wayland-7"), Precondition::NoBinary);
        assert_eq!(
            check(Some(WaydroidStatus::NotInitialised), "wayland-7"),
            Precondition::NotInitialised
        );
        assert_eq!(
            check(Some(WaydroidStatus::Stopped), "wayland-7"),
            Precondition::Ok
        );
        assert_eq!(
            check(Some(running("wayland-7")), "wayland-7"),
            Precondition::Ok
        );
        assert_eq!(
            check(Some(running("wayland-0")), "wayland-7"),
            Precondition::ForeignSession("wayland-0".into())
        );
        // Before the pane exists there is no display of ours, so any running
        // session — even one whose display is unknown — is someone else's.
        assert_eq!(
            check(Some(running("wayland-0")), ""),
            Precondition::ForeignSession("wayland-0".into())
        );
        assert_eq!(
            check(Some(running("")), ""),
            Precondition::ForeignSession(String::new())
        );
    }

    #[test]
    fn precondition_messages_say_what_to_do() {
        assert!(Precondition::NoBinary.to_string().contains("waydroid"));
        assert!(
            Precondition::NotInitialised
                .to_string()
                .contains("waydroid init")
        );
        let foreign = Precondition::ForeignSession("wayland-0".into()).to_string();
        assert!(foreign.contains("wayland-0"), "{foreign}");
        assert!(foreign.contains("waydroid session stop"), "{foreign}");
        let unknown = Precondition::ForeignSession(String::new()).to_string();
        assert!(!unknown.contains("on display"), "{unknown}");
    }

    #[test]
    fn argument_builders() {
        assert_eq!(status_args(), ["status"]);
        assert_eq!(session_start_args(), ["session", "start"]);
        assert_eq!(show_full_ui_args(), ["show-full-ui"]);
        assert_eq!(
            client_env("wayland-7"),
            [("WAYLAND_DISPLAY", "wayland-7".to_string())]
        );
    }

    #[test]
    fn adb_serial_defaults_and_parses() {
        assert_eq!(adb_serial(RUNNING), "192.168.240.112:5555");
        assert_eq!(
            adb_serial(&RUNNING.replace("192.168.240.112", "10.0.3.9")),
            "10.0.3.9:5555"
        );
        assert_eq!(adb_serial(STOPPED), DEFAULT_ADB_SERIAL);
        assert_eq!(
            adb_serial("Session:\tRUNNING\nIP address:\tUNKNOWN\n"),
            DEFAULT_ADB_SERIAL
        );
    }

    #[test]
    fn boot_step_waits_for_our_display() {
        assert_eq!(
            boot_step(&WaydroidStatus::Stopped, "wayland-7"),
            BootStep::Wait
        );
        assert_eq!(boot_step(&running("wayland-7"), "wayland-7"), BootStep::Up);
        assert_eq!(
            boot_step(&running("wayland-0"), "wayland-7"),
            BootStep::Foreign("wayland-0".into())
        );
        assert_eq!(
            boot_step(&WaydroidStatus::NotInitialised, "wayland-7"),
            BootStep::NotInitialised
        );
    }

    #[test]
    fn claim_is_free_mine_or_taken() {
        assert_eq!(claim(None, "aaa"), Claim::Free);
        assert_eq!(claim(Some("aaa"), "aaa"), Claim::Mine);
        assert_eq!(claim(Some("bbb"), "aaa"), Claim::Taken("bbb".into()));
    }

    #[test]
    fn the_session_log_lives_under_the_state_dir() {
        assert_eq!(
            log_path(Path::new("/s/kabelsalat")),
            PathBuf::from("/s/kabelsalat/android/session.log")
        );
    }
}
```

In `src/lib.rs`, add the module after `mod app;` (keep the list alphabetical):

```rust
mod app;
pub mod android;
pub mod autostart;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib android::tests`
Expected: compile errors — `parse_status`, `WaydroidStatus`, `Precondition`, `check`, `status_args`, `client_env`, `adb_serial`, `boot_step`, `claim`, `log_path` not found in `super`.

- [ ] **Step 3: Write the implementation**

Insert at the top of `src/android.rs`, above the test module:

```rust
//! The Android pane: a klamottenkiste `WaylandPane` hosting a Waydroid
//! session, owned as one unit. Mirrors `browser.rs`.
//!
//! The pure half — status parsing, preconditions, argument builders, the adb
//! serial, the ownership claim — is what the tests cover. The process half
//! spawns `waydroid` and never panics: every failure is a message for the
//! user, the way the app degrades without tmux.

use std::fmt;
use std::path::{Path, PathBuf};

/// Waydroid's command-line tool, resolved against `PATH` at spawn time.
pub const WAYDROID_BINARY: &str = "waydroid";

/// The adb serial when `waydroid status` names no usable address: Waydroid's
/// fixed container address on its `waydroid0` bridge, adbd's TCP port.
pub const DEFAULT_ADB_SERIAL: &str = "192.168.240.112:5555";

/// adbd's TCP port inside the container.
const ADB_PORT: u16 = 5555;

/// Directory under the state dir that holds the session log.
pub const ANDROID_SUBDIR: &str = "android";

/// The session log's file name, truncated at every spawn.
const SESSION_LOG: &str = "session.log";

/// What `waydroid status` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaydroidStatus {
    /// `waydroid init` was never run.
    NotInitialised,
    /// No session is running.
    Stopped,
    /// A session runs on this Wayland display. Empty when the output named
    /// none, which makes it unidentifiable — never ours.
    Running { display: String },
}

/// Parse `waydroid status`. Pure; unknown or garbled output reads as stopped,
/// because a running session always prints its `Session:\tRUNNING` line.
pub fn parse_status(output: &str) -> WaydroidStatus {
    if output.contains("not initialized") {
        return WaydroidStatus::NotInitialised;
    }
    let mut running = false;
    let mut display = String::new();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "Session" => running = value.trim() == "RUNNING",
            "Wayland display" => display = value.trim().to_string(),
            _ => {}
        }
    }
    if running {
        WaydroidStatus::Running { display }
    } else {
        WaydroidStatus::Stopped
    }
}

/// Whether a session may be started for a pane on `our_display`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    Ok,
    /// No `waydroid` on `PATH`, or it could not be run at all.
    NoBinary,
    NotInitialised,
    /// A session already runs on this display. One Waydroid per machine, and
    /// its display is fixed at `session start`.
    ForeignSession(String),
}

impl fmt::Display for Precondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => write!(f, "Waydroid is ready to start."),
            Self::NoBinary => write!(
                f,
                "Waydroid is not installed (no `waydroid` on PATH), or it could not be run. \
                 Install it to use the Android pane."
            ),
            Self::NotInitialised => write!(
                f,
                "Waydroid is installed but not initialised. Run `sudo waydroid init` once, \
                 then try again."
            ),
            Self::ForeignSession(display) => {
                write!(f, "A Waydroid session is already running")?;
                if !display.is_empty() {
                    write!(f, " on display {display}")?;
                }
                write!(
                    f,
                    ". There can be only one per machine; stop it with \
                     `waydroid session stop` and try again."
                )
            }
        }
    }
}

/// Decide whether to start a session. `status` is `None` when `waydroid
/// status` could not be run. `our_display` is empty while no pane of ours
/// exists, so then every running session is foreign. Pure.
pub fn check(status: Option<WaydroidStatus>, our_display: &str) -> Precondition {
    match status {
        None => Precondition::NoBinary,
        Some(WaydroidStatus::NotInitialised) => Precondition::NotInitialised,
        Some(WaydroidStatus::Stopped) => Precondition::Ok,
        Some(WaydroidStatus::Running { display })
            if !our_display.is_empty() && display == our_display =>
        {
            Precondition::Ok
        }
        Some(WaydroidStatus::Running { display }) => Precondition::ForeignSession(display),
    }
}

/// `waydroid status`.
pub fn status_args() -> &'static [&'static str] {
    &["status"]
}

/// `waydroid session start`: runs in the foreground for the session's whole
/// life, which is what makes it the child the pane owns.
pub fn session_start_args() -> &'static [&'static str] {
    &["session", "start"]
}

/// `waydroid show-full-ui`: shows the whole Android screen instead of
/// per-app windows; returns once Android's platform service answered.
pub fn show_full_ui_args() -> &'static [&'static str] {
    &["show-full-ui"]
}

/// The environment every `waydroid` call of a pane runs with: its nested
/// compositor's socket. Waydroid reads `WAYLAND_DISPLAY` once, at
/// `session start`, and keeps that display for the session's life.
pub fn client_env(display: &str) -> [(&'static str, String); 1] {
    [("WAYLAND_DISPLAY", display.to_string())]
}

/// The adb serial for the container: `<IP address line>:5555`, or
/// [`DEFAULT_ADB_SERIAL`] when the line is missing or not an IPv4 address
/// (Waydroid prints `UNKNOWN` before its DHCP lease exists). Pure.
pub fn adb_serial(status_output: &str) -> String {
    status_output
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim() == "IP address")
        .map(|(_, value)| value.trim())
        .filter(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
        .map(|ip| format!("{ip}:{ADB_PORT}"))
        .unwrap_or_else(|| DEFAULT_ADB_SERIAL.to_string())
}

/// Where a booting pane stands, from one `waydroid status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootStep {
    /// Not up yet; poll again.
    Wait,
    /// The session runs on our display.
    Up,
    /// A session runs on someone else's display: ours never will.
    Foreign(String),
    NotInitialised,
}

/// Map a status to the boot watcher's next move. Pure.
pub fn boot_step(status: &WaydroidStatus, our_display: &str) -> BootStep {
    match status {
        WaydroidStatus::Running { display } if display == our_display => BootStep::Up,
        WaydroidStatus::Running { display } => BootStep::Foreign(display.clone()),
        WaydroidStatus::Stopped => BootStep::Wait,
        WaydroidStatus::NotInitialised => BootStep::NotInitialised,
    }
}

/// Whether a group may take the Android pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// Nobody owns it.
    Free,
    /// The requester already owns it.
    Mine,
    /// Another group (this uuid) owns it.
    Taken(String),
}

/// Decide a claim from the current owner's group uuid. Pure.
pub fn claim(owner: Option<&str>, requester: &str) -> Claim {
    match owner {
        None => Claim::Free,
        Some(owner) if owner == requester => Claim::Mine,
        Some(owner) => Claim::Taken(owner.to_string()),
    }
}

/// `<state_dir>/android/session.log` — pure path derivation.
pub fn log_path(state_dir: &Path) -> PathBuf {
    state_dir.join(ANDROID_SUBDIR).join(SESSION_LOG)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib android::tests`
Expected: 11 passed.

- [ ] **Step 5: Format and lint**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add src/android.rs src/lib.rs
git commit -m "Parse Waydroid status and decide whether an Android pane may start"
```

---

### Task 2: `src/android.rs` — process half (`Android`)

**Files:**
- Modify: `src/android.rs` (imports at the top; new code between `log_path` and `mod tests`; new tests appended inside `mod tests`)
- Test: `src/android.rs` (`mod tests`)

**Interfaces:**
- Consumes (Task 1): `WAYDROID_BINARY`, `parse_status`, `check`, `Precondition`, `status_args`, `session_start_args`, `show_full_ui_args`, `client_env`, `adb_serial`, `boot_step`, `BootStep`, `log_path`. From `src/browser.rs`: `pub fn resolve_binary_in(candidates: &[&str], path_var: &OsStr) -> Option<PathBuf>`, `pub fn isolate_process_group(command: &mut Command) -> &mut Command`, `pub fn terminate_child(child: &mut Child, grace: Duration)`, `pub use klamottenkiste::{CaptureError, CapturedFrame}`.
- Produces (used by Tasks 7, 8, 9):
  - `pub const BOOT_TIMEOUT: Duration` (60 s), `pub const STOP_GRACE: Duration` (2 s)
  - `pub type BootResult = Result<String, String>` — `Ok(adb serial)` or `Err(user-facing reason)`
  - `pub fn session_stop_args() -> &'static [&'static str]`, `pub fn boot_completed_args() -> &'static [&'static str]`, `pub fn boot_completed(output: &str) -> bool`
  - `pub fn next_boot_id() -> u64`
  - `pub fn run_capture(command: &mut Command, timeout: Duration) -> Option<String>`
  - `pub enum AndroidError { Precondition(Precondition), Compositor(String), Log(std::io::Error), Spawn(std::io::Error) }` + `Display`
  - `pub struct Android` with:
    - `pub fn spawn<F>(state_dir: &Path, on_boot: F) -> Result<Android, AndroidError> where F: FnOnce(u64, BootResult) + Send + 'static` — `on_boot(boot_id, result)` runs once on a worker thread, unless the pane was torn down first
    - `pub fn widget(&self) -> &WaylandPane`, `pub fn is_running(&self) -> bool`, `pub fn capture_frame<F>(&self, callback: F) where F: FnOnce(Result<CapturedFrame, CaptureError>) + 'static`
    - `pub fn has_exited(&mut self) -> std::io::Result<bool>`
    - `pub fn control_socket_path(&self) -> &Path`, `pub fn boot_id(&self) -> u64`, `pub fn adb(&self) -> Option<&str>`, `pub fn set_ready(&mut self, serial: String)`
    - `pub fn teardown(&mut self)` (idempotent; `Drop` calls it)

The boot watcher, per the spec: poll `waydroid status` every 500 ms until the session runs on our display, then run `waydroid show-full-ui` (same env, same process group as the session). The spec's "Running" alone only means the session manager registered — Android is still booting — so the watcher also waits for `show-full-ui` to return (it blocks until Android's platform service answers) and then for `waydroid prop get sys.boot_completed` to print `1`. All three share one 60 s budget; running out is an `Err`, which the app turns into `AndroidDied`.

Note on `show-full-ui`: if no session is running it starts one itself (`maybeLaunchLater` in `/usr/lib/waydroid/tools/actions/app_manager.py`). The watcher only runs it after the status poll saw our session up, so it never does.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/android.rs`:

```rust
    #[test]
    fn boot_completed_reads_the_property_value() {
        assert!(boot_completed("1\n"));
        assert!(boot_completed("1"));
        assert!(!boot_completed(""));
        assert!(!boot_completed("0\n"));
        assert!(!boot_completed("[12:00:01] WayDroid session is stopped\n"));
    }

    #[test]
    fn stop_and_boot_property_arguments() {
        assert_eq!(session_stop_args(), ["session", "stop"]);
        assert_eq!(
            boot_completed_args(),
            ["prop", "get", "sys.boot_completed"]
        );
    }

    #[test]
    fn boot_ids_are_never_reused() {
        let first = next_boot_id();
        let second = next_boot_id();
        assert!(second > first);
    }

    #[cfg(unix)]
    #[test]
    fn run_capture_returns_stdout() {
        let out = run_capture(
            Command::new("sh").args(["-c", "echo hello"]),
            Duration::from_secs(5),
        );
        assert_eq!(out.as_deref(), Some("hello\n"));
    }

    #[cfg(unix)]
    #[test]
    fn run_capture_gives_up_on_a_hung_command() {
        let started = std::time::Instant::now();
        let out = run_capture(
            Command::new("sh").args(["-c", "sleep 5"]),
            Duration::from_millis(100),
        );
        assert_eq!(out, None);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn run_capture_of_a_missing_binary_is_none() {
        let out = run_capture(
            &mut Command::new("/nonexistent/kabelsalat-test/waydroid"),
            Duration::from_secs(1),
        );
        assert_eq!(out, None);
    }

    #[test]
    fn android_errors_read_as_sentences() {
        let err = AndroidError::Precondition(Precondition::NotInitialised);
        assert!(err.to_string().contains("waydroid init"));
        assert!(
            AndroidError::Compositor("no EGL".into())
                .to_string()
                .contains("no EGL")
        );
        let io = || std::io::Error::other("disk full");
        assert!(AndroidError::Log(io()).to_string().contains("disk full"));
        assert!(AndroidError::Spawn(io()).to_string().contains("disk full"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib android::tests`
Expected: compile errors — `boot_completed`, `session_stop_args`, `boot_completed_args`, `next_boot_id`, `run_capture`, `AndroidError`, `Command`, `Duration` not found.

- [ ] **Step 3: Write the implementation**

Replace the imports at the top of `src/android.rs` (`use std::fmt;` / `use std::path::{Path, PathBuf};`) with:

```rust
use std::fmt;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use klamottenkiste::WaylandPane;
use relm4::gtk::prelude::WidgetExt;

use crate::browser::{self, CaptureError, CapturedFrame};
```

Insert between `log_path` and `#[cfg(test)] mod tests`:

```rust
/// How long Android gets from `session start` to `sys.boot_completed=1`.
/// The spike measured ~18.5 s with the container already running.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(60);

/// Pause between two status or property polls while booting.
const BOOT_POLL: Duration = Duration::from_millis(500);

/// Poll interval while waiting for `show-full-ui` to return.
const UI_POLL: Duration = Duration::from_millis(100);

/// One `waydroid status`/`prop get` from the boot thread. Python plus a D-Bus
/// round trip; a hung bus must not stall the watcher past its budget.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// The `waydroid status` before a spawn runs on the GTK main thread, so it is
/// held to a short leash: a hung bus costs at most this much frozen UI.
const PRECHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// `waydroid session stop` at teardown, also on the main thread.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// After `session stop`, how long the session process group gets to exit
/// after `SIGTERM` before it is killed.
pub const STOP_GRACE: Duration = Duration::from_secs(2);

/// Poll interval of [`run_capture`].
const CAPTURE_POLL: Duration = Duration::from_millis(20);

/// `Ok(adb serial)` once Android has booted, or why it did not.
pub type BootResult = Result<String, String>;

/// `waydroid session stop`.
pub fn session_stop_args() -> &'static [&'static str] {
    &["session", "stop"]
}

/// `waydroid prop get sys.boot_completed`: prints `1` once Android is up.
pub fn boot_completed_args() -> &'static [&'static str] {
    &["prop", "get", "sys.boot_completed"]
}

/// Whether `waydroid prop get sys.boot_completed` reported a finished boot.
/// Pure.
pub fn boot_completed(output: &str) -> bool {
    output.lines().any(|line| line.trim() == "1")
}

/// A fresh identity for one spawn. The boot thread reports with it, so a
/// report from a pane that was stopped meanwhile can never be mistaken for
/// one from its successor in the same group.
pub fn next_boot_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Run `command` to completion and return its stdout, or `None` when it could
/// not start, did not finish within `timeout` (it is killed), or wrote
/// something that is not UTF-8. Stdin and stderr are null. Never panics.
///
/// Only for commands with small output: stdout is read after exit, so a
/// command that fills the pipe buffer would stall until the timeout.
pub fn run_capture(command: &mut Command, timeout: Duration) -> Option<String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(CAPTURE_POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    Some(output)
}

/// Everything that can go wrong bringing Android up.
#[derive(Debug)]
pub enum AndroidError {
    /// Waydroid missing, not initialised, or already running elsewhere.
    Precondition(Precondition),
    /// The nested compositor did not come up.
    Compositor(String),
    /// The session log could not be created.
    Log(std::io::Error),
    /// `waydroid session start` or the boot thread could not be started.
    Spawn(std::io::Error),
}

impl fmt::Display for AndroidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Precondition(precondition) => write!(f, "{precondition}"),
            Self::Compositor(msg) => write!(f, "The Android compositor could not start: {msg}"),
            Self::Log(err) => write!(f, "The Android session log could not be created: {err}"),
            Self::Spawn(err) => write!(f, "Waydroid could not be started: {err}"),
        }
    }
}

impl std::error::Error for AndroidError {}

/// A live Android pane: the compositor widget and the `waydroid session
/// start` process rendering into it, plus what the boot thread found out.
pub struct Android {
    pane: WaylandPane,
    session: Child,
    binary: PathBuf,
    control: PathBuf,
    boot: u64,
    /// The adb serial, once the boot thread reported success.
    adb: Option<String>,
    /// Set at teardown; the boot thread stops and reports nothing.
    cancel: Arc<AtomicBool>,
    torn_down: bool,
}

impl Android {
    /// Check Waydroid, create a pane, and start a session on its display.
    ///
    /// The `waydroid status` check runs here, on the caller's (GTK) thread,
    /// bounded by a 3 s timeout. Booting is watched on a worker thread that
    /// calls `on_boot(boot_id, result)` exactly once — unless the pane was
    /// torn down first, in which case it reports nothing. On any error the
    /// pane is closed before returning, so no blank pane is left behind.
    pub fn spawn<F>(state_dir: &Path, on_boot: F) -> Result<Self, AndroidError>
    where
        F: FnOnce(u64, BootResult) + Send + 'static,
    {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let Some(binary) = browser::resolve_binary_in(&[WAYDROID_BINARY], &path) else {
            return Err(AndroidError::Precondition(Precondition::NoBinary));
        };
        // No pane of ours exists yet, so there is no display to call ours:
        // any running session is someone else's.
        let status = run_capture(Command::new(&binary).args(status_args()), PRECHECK_TIMEOUT)
            .map(|output| parse_status(&output));
        match check(status, "") {
            Precondition::Ok => {}
            other => return Err(AndroidError::Precondition(other)),
        }

        let pane = WaylandPane::new();
        let (socket, control) = match (pane.wayland_socket(), pane.control_socket_path()) {
            (Some(socket), Some(control)) => (socket, control),
            _ => {
                let msg = pane
                    .startup_error()
                    .unwrap_or_else(|| "no Wayland socket was advertised".to_string());
                pane.close();
                return Err(AndroidError::Compositor(msg));
            }
        };

        let log = match open_log(state_dir) {
            Ok(log) => log,
            Err(err) => {
                pane.close();
                return Err(AndroidError::Log(err));
            }
        };
        let (stdout, stderr) = log_stdio(&log);
        let mut command = Command::new(&binary);
        command
            .args(session_start_args())
            .envs(client_env(&socket))
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        // `session start` spawns helpers; teardown signals the whole tree.
        browser::isolate_process_group(&mut command);
        let mut session = match command.spawn() {
            Ok(session) => session,
            Err(err) => {
                pane.close();
                return Err(AndroidError::Spawn(err));
            }
        };

        let boot = next_boot_id();
        let cancel = Arc::new(AtomicBool::new(false));
        let watch = BootWatch {
            binary: binary.clone(),
            display: socket,
            session_pid: session.id(),
            log,
            cancel: cancel.clone(),
        };
        let spawned = std::thread::Builder::new()
            .name("kabelsalat-android-boot".to_string())
            .spawn(move || {
                let result = watch.run();
                if !watch.cancelled() {
                    on_boot(boot, result);
                }
            });
        if let Err(err) = spawned {
            browser::terminate_child(&mut session, STOP_GRACE);
            pane.close();
            return Err(AndroidError::Spawn(err));
        }

        pane.set_hexpand(true);
        pane.set_vexpand(true);
        Ok(Self {
            pane,
            session,
            binary,
            control,
            boot,
            adb: None,
            cancel,
            torn_down: false,
        })
    }

    /// The widget to parent into the pane area.
    pub fn widget(&self) -> &WaylandPane {
        &self.pane
    }

    /// Is the pane's compositor still running? Says nothing about Android.
    pub fn is_running(&self) -> bool {
        self.pane.is_running()
    }

    /// A freshly rendered frame of what the pane shows; same contract as
    /// `Browser::capture_frame` (exactly once, on the GTK main context).
    pub fn capture_frame<F>(&self, callback: F)
    where
        F: FnOnce(Result<CapturedFrame, CaptureError>) + 'static,
    {
        self.pane.capture_frame(callback);
    }

    /// Has `waydroid session start` exited? `Ok(true)` means the session is
    /// gone; the caller polls this on the browser poll timer.
    pub fn has_exited(&mut self) -> std::io::Result<bool> {
        if self.torn_down {
            return Ok(true);
        }
        Ok(self.session.try_wait()?.is_some())
    }

    /// The pane's control socket (`screenshot`, `click`, `type`, `key`,
    /// `resize`), what `KABELSALAT_ANDROID_CTL` publishes.
    pub fn control_socket_path(&self) -> &Path {
        &self.control
    }

    /// This spawn's identity, as passed to `on_boot`.
    pub fn boot_id(&self) -> u64 {
        self.boot
    }

    /// The adb serial, once Android has booted.
    pub fn adb(&self) -> Option<&str> {
        self.adb.as_deref()
    }

    /// Record a finished boot.
    pub fn set_ready(&mut self, serial: String) {
        self.adb = Some(serial);
    }

    /// Stop the session and close the pane. Idempotent; never panics.
    ///
    /// Order, per the spec: `waydroid session stop` (bounded), then the
    /// session's process group is terminated (`SIGTERM`, `SIGKILL` after
    /// [`STOP_GRACE`]) and reaped, then the compositor is closed.
    pub fn teardown(&mut self) {
        if !self.torn_down {
            self.torn_down = true;
            self.cancel.store(true, Ordering::SeqCst);
            let mut stop = Command::new(&self.binary);
            stop.args(session_stop_args());
            let _ = run_capture(&mut stop, STOP_TIMEOUT);
            browser::terminate_child(&mut self.session, STOP_GRACE);
        }
        // Idempotent and infallible upstream.
        self.pane.close();
    }
}

impl Drop for Android {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// What the boot thread needs. Everything owned, nothing GTK.
struct BootWatch {
    binary: PathBuf,
    display: String,
    session_pid: u32,
    log: File,
    cancel: Arc<AtomicBool>,
}

impl BootWatch {
    /// A `waydroid` call against this pane's display.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.binary);
        command.args(args).envs(client_env(&self.display));
        command
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Session up on our display → full UI shown → boot completed, all
    /// within one [`BOOT_TIMEOUT`].
    fn run(&self) -> BootResult {
        let deadline = Instant::now() + BOOT_TIMEOUT;
        let serial = self.wait_for_session(deadline)?;
        self.show_full_ui(deadline)?;
        self.wait_for_boot_completed(deadline)?;
        Ok(serial)
    }

    fn check_time(&self, deadline: Instant) -> Result<(), String> {
        if self.cancelled() {
            return Err("Android was stopped while it booted.".to_string());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Android did not finish booting within {} s.",
                BOOT_TIMEOUT.as_secs()
            ));
        }
        Ok(())
    }

    fn wait_for_session(&self, deadline: Instant) -> Result<String, String> {
        loop {
            self.check_time(deadline)?;
            if let Some(output) = run_capture(&mut self.command(status_args()), COMMAND_TIMEOUT) {
                match boot_step(&parse_status(&output), &self.display) {
                    BootStep::Up => return Ok(adb_serial(&output)),
                    BootStep::Wait => {}
                    BootStep::Foreign(display) => {
                        return Err(Precondition::ForeignSession(display).to_string());
                    }
                    BootStep::NotInitialised => {
                        return Err(Precondition::NotInitialised.to_string());
                    }
                }
            }
            std::thread::sleep(BOOT_POLL);
        }
    }

    fn show_full_ui(&self, deadline: Instant) -> Result<(), String> {
        let (stdout, stderr) = log_stdio(&self.log);
        let mut command = self.command(show_full_ui_args());
        command.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
        // Same process group as the session, so teardown reaches it too.
        join_process_group(&mut command, self.session_pid);
        let mut child = command
            .spawn()
            .map_err(|err| format!("`waydroid show-full-ui` could not start: {err}"))?;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {}
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("`waydroid show-full-ui` could not be watched: {err}"));
                }
            }
            if let Err(reason) = self.check_time(deadline) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(reason);
            }
            std::thread::sleep(UI_POLL);
        }
    }

    fn wait_for_boot_completed(&self, deadline: Instant) -> Result<(), String> {
        loop {
            self.check_time(deadline)?;
            if run_capture(&mut self.command(boot_completed_args()), COMMAND_TIMEOUT)
                .is_some_and(|output| boot_completed(&output))
            {
                return Ok(());
            }
            std::thread::sleep(BOOT_POLL);
        }
    }
}

/// `<state_dir>/android/session.log`, created (or truncated) for one spawn.
fn open_log(state_dir: &Path) -> std::io::Result<File> {
    let path = log_path(state_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    File::create(path)
}

/// Two handles on the session log for a child's stdout and stderr; null
/// when the log cannot be duplicated, which costs the log, not the session.
fn log_stdio(log: &File) -> (Stdio, Stdio) {
    match (log.try_clone(), log.try_clone()) {
        (Ok(out), Ok(err)) => (out.into(), err.into()),
        _ => (Stdio::null(), Stdio::null()),
    }
}

/// Put `command` into the process group led by `leader` (the session).
/// Spawning then fails with `EPERM` if that group is already gone, which the
/// caller reports like any other spawn failure. A no-op off unix.
fn join_process_group(command: &mut Command, leader: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(leader as i32);
    }
    #[cfg(not(unix))]
    let _ = (command, leader);
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib android::tests`
Expected: 18 passed.

- [ ] **Step 5: Format and lint**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: no warnings. (`Android` is unused until Task 7, but `android` is a `pub mod`, so its `pub` items are not dead code.)

- [ ] **Step 6: Commit**

```bash
git add src/android.rs
git commit -m "Start a Waydroid session in a nested pane and watch it boot"
```

---

### Task 3: `state.rs` — persist the Android owner, plan its restore

**Files:**
- Modify: `src/state.rs:182-226` (`SavedState` and its `Default`), new functions after `remote_hosts` (`src/state.rs:595-605`)
- Modify: `src/app.rs:2102-2110` (`save_state`'s `SavedState { … }` literal)
- Test: `src/state.rs` (`mod tests`; `sample_state` at `src/state.rs:681-737`, new tests appended at the end)

**Interfaces:**
- Consumes: nothing new.
- Produces (used by Task 8):
  - `SavedState::android_owner: Option<String>` (group uuid; `#[serde(default)]`)
  - `pub fn android_restore_target(saved: &SavedState) -> Option<usize>` — the saved group id to reopen Android for: the group whose uuid is `android_owner`, if it exists and is local
  - `pub fn android_owner_desired(live: Option<&str>, pending: Option<&str>) -> Option<String>` — what to persist: the live owner, else a restore still pending

The spec types the field `Option<Uuid>`; group uuids are `String`s throughout this codebase (`SavedGroup::uuid`), so it is `Option<String>`.

- [ ] **Step 1: Write the failing tests**

In `sample_state` (`src/state.rs:681`), add after `pending_kills: Vec::new(),` (line 735):

```rust
            android_owner: Some("g-bbb".into()),
```

Append inside `mod tests`:

```rust
    // --- android pane ---

    #[test]
    fn old_state_without_android_owner_loads_as_none() {
        let json = r#"{"groups": [], "tabs": [], "active": null, "sidebar_visible": true}"#;
        let state: SavedState = serde_json::from_str(json).unwrap();
        assert_eq!(state.android_owner, None);
    }

    #[test]
    fn android_owner_survives_save_and_load() {
        let dir = tmp_dir("androidowner");
        let path = dir.join("state.json");
        save(&sample_state(), &path).unwrap();
        assert_eq!(load(&path).android_owner.as_deref(), Some("g-bbb"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn android_restore_targets_the_owning_local_group() {
        // sample_state: g-bbb is group id 1, local.
        assert_eq!(android_restore_target(&sample_state()), Some(1));
    }

    #[test]
    fn android_restore_is_a_noop_without_a_usable_owner() {
        let mut state = sample_state();
        state.android_owner = None;
        assert_eq!(android_restore_target(&state), None);

        state.android_owner = Some("g-gone".into());
        assert_eq!(android_restore_target(&state), None);

        // A remote group cannot host the (local) pane.
        let mut remote = sample_state();
        remote.groups[1].host = Some("me@box".into());
        assert_eq!(android_restore_target(&remote), None);
    }

    #[test]
    fn the_desired_android_owner_keeps_a_pending_restore() {
        assert_eq!(
            android_owner_desired(Some("g-aaa"), None).as_deref(),
            Some("g-aaa")
        );
        // Restore is idle-driven: a save before it ran must not forget it.
        assert_eq!(
            android_owner_desired(None, Some("g-bbb")).as_deref(),
            Some("g-bbb")
        );
        assert_eq!(
            android_owner_desired(Some("g-aaa"), Some("g-bbb")).as_deref(),
            Some("g-aaa")
        );
        assert_eq!(android_owner_desired(None, None), None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib state::tests`
Expected: compile errors — no field `android_owner` on `SavedState`; `android_restore_target`, `android_owner_desired` not found.

- [ ] **Step 3: Write the implementation**

In `SavedState` (`src/state.rs:182-202`), after `pending_kills`:

```rust
    /// The uuid of the group whose Android pane was open (or still queued
    /// for restore) at the last save. One per machine, so one owner.
    /// `serde(default)` so older state files load with none.
    #[serde(default)]
    pub android_owner: Option<String>,
```

In `impl Default for SavedState` (`src/state.rs:215-226`), after `pending_kills: Vec::new(),`:

```rust
            android_owner: None,
```

After `remote_hosts` (ends `src/state.rs:605`), add:

```rust
/// The saved group id whose Android pane comes back after a restart: the
/// group named by `android_owner`, as long as it still exists and is local
/// (the pane is a local widget). Pure.
pub fn android_restore_target(saved: &SavedState) -> Option<usize> {
    let owner = saved.android_owner.as_deref()?;
    saved
        .groups
        .iter()
        .find(|g| g.uuid == owner && g.host.is_none())
        .map(|g| g.id)
}

/// Which owner to persist: the group that holds a live Android, else the
/// group still queued for restore — restore is idle-driven, and a save in
/// that window must not forget it (same reasoning as `browser_open`). Pure.
pub fn android_owner_desired(live: Option<&str>, pending: Option<&str>) -> Option<String> {
    live.or(pending).map(str::to_string)
}
```

In `src/app.rs` `save_state`, the `SavedState { … }` literal (`src/app.rs:2102-2110`) gains, after `pending_kills: self.pending_kills.clone(),`:

```rust
            // No group can hold an Android pane yet; Task 8 of the Android
            // pane plan persists the real owner here.
            android_owner: None,
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib state::tests`
Expected: all pass (5 new).

- [ ] **Step 5: Format, lint, full test**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean; all pass.

- [ ] **Step 6: Commit**

```bash
git add src/state.rs src/app.rs
git commit -m "Persist the Android pane's owner and plan its restore"
```

---

### Task 4: `state.rs` — pane-area decisions

**Files:**
- Modify: `src/state.rs` (new section after `android_owner_desired` from Task 3)
- Test: `src/state.rs` (`mod tests`, appended)

**Interfaces:**
- Consumes: nothing.
- Produces (used by Tasks 6, 7, 8, 9):
  - `pub enum PaneKind { Browser, Android }` (`Debug, Clone, Copy, PartialEq, Eq`) with `pub fn title(self) -> &'static str`, `pub fn widget_name(self) -> &'static str`, `pub fn from_widget_name(name: &str) -> Option<PaneKind>`, `pub fn other(self) -> PaneKind`
  - `pub enum BrowserKeyStep { Spawn, Front, Hide }`, `pub fn browser_key_step(has_browser: bool, panes_visible: bool, front: PaneKind) -> BrowserKeyStep`
  - `pub fn front_after_open(front: PaneKind, opened: PaneKind, other_open: bool, quiet: bool) -> PaneKind`
  - `pub fn front_after_close(front: PaneKind, closed: PaneKind, other_open: bool) -> PaneKind`
  - `pub fn quiet_open_visible(panes_visible: bool, group_active: bool, other_open: bool) -> bool`

Decisions the spec leaves open, made here so they are tested rather than buried in `app.rs`:

- **Alt+2 with two panes.** The spec keeps Alt+2 as "toggle the pane area" and adds Alt+3 for Android. With a browser and an Android open, a plain show/hide toggle would leave no key that brings the browser to the front. So Alt+2 mirrors Alt+3 for the browser: no browser → spawn it in front; browser behind Android, or area hidden → browser to the front and area visible; browser already in front and visible → hide the area. With only a browser this is exactly today's Alt+2.
- **A quiet open of the only pane.** "CLI-created panes never change `front_pane`" cannot hold when the new pane is the group's only one — there is nothing else to be in front. Then the new pane is in front; otherwise `front_pane` is untouched.
- **Area visibility after a quiet open.** Today a CLI browser comes up visible in the active group, hidden elsewhere. With panes: the area only pops open for the active group's first pane; an existing area keeps its state.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/state.rs`:

```rust
    // --- pane area ---

    #[test]
    fn pane_kinds_round_trip_through_their_widget_names() {
        for kind in [PaneKind::Browser, PaneKind::Android] {
            assert_eq!(PaneKind::from_widget_name(kind.widget_name()), Some(kind));
        }
        assert_eq!(PaneKind::from_widget_name(""), None);
        assert_eq!(PaneKind::from_widget_name("KstWaylandPane"), None);
        assert_eq!(PaneKind::Browser.title(), "Browser");
        assert_eq!(PaneKind::Android.title(), "Android");
        assert_eq!(PaneKind::Browser.other(), PaneKind::Android);
        assert_eq!(PaneKind::Android.other(), PaneKind::Browser);
    }

    #[test]
    fn alt_2_spawns_fronts_or_hides_the_browser() {
        use BrowserKeyStep::*;
        // No browser: spawn one, whatever else is open.
        assert_eq!(browser_key_step(false, false, PaneKind::Browser), Spawn);
        assert_eq!(browser_key_step(false, true, PaneKind::Android), Spawn);
        // Browser in front and showing: hide the area (today's toggle).
        assert_eq!(browser_key_step(true, true, PaneKind::Browser), Hide);
        // Area hidden, or the browser behind Android: bring it to the front.
        assert_eq!(browser_key_step(true, false, PaneKind::Browser), Front);
        assert_eq!(browser_key_step(true, false, PaneKind::Android), Front);
        assert_eq!(browser_key_step(true, true, PaneKind::Android), Front);
    }

    #[test]
    fn an_opened_pane_goes_in_front_unless_it_opened_quietly_next_to_another() {
        // Interactive open: always in front.
        assert_eq!(
            front_after_open(PaneKind::Browser, PaneKind::Android, true, false),
            PaneKind::Android
        );
        // Quiet (CLI, restore) next to another pane: front untouched.
        assert_eq!(
            front_after_open(PaneKind::Browser, PaneKind::Android, true, true),
            PaneKind::Browser
        );
        // Quiet but alone: nothing else can be in front.
        assert_eq!(
            front_after_open(PaneKind::Browser, PaneKind::Android, false, true),
            PaneKind::Android
        );
    }

    #[test]
    fn closing_the_front_pane_fronts_the_other_one() {
        assert_eq!(
            front_after_close(PaneKind::Android, PaneKind::Android, true),
            PaneKind::Browser
        );
        // Closing the pane behind changes nothing.
        assert_eq!(
            front_after_close(PaneKind::Android, PaneKind::Browser, true),
            PaneKind::Android
        );
        // Nothing left: the value no longer matters, and is left alone.
        assert_eq!(
            front_after_close(PaneKind::Android, PaneKind::Android, false),
            PaneKind::Android
        );
    }

    #[test]
    fn a_quiet_open_shows_the_area_only_for_the_active_groups_first_pane() {
        assert!(quiet_open_visible(false, true, false));
        assert!(!quiet_open_visible(false, false, false));
        // An existing area keeps its state either way.
        assert!(!quiet_open_visible(false, true, true));
        assert!(quiet_open_visible(true, false, true));
        assert!(quiet_open_visible(true, true, true));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib state::tests`
Expected: compile errors — `PaneKind`, `BrowserKeyStep`, `browser_key_step`, `front_after_open`, `front_after_close`, `quiet_open_visible` not found.

- [ ] **Step 3: Write the implementation**

After `android_owner_desired` in `src/state.rs`, add:

```rust
// ---- pane area ----------------------------------------------------------

/// One kind of pane a group's pane area can hold; at most one of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    Browser,
    Android,
}

impl PaneKind {
    /// The tab title in the pane area.
    pub fn title(self) -> &'static str {
        match self {
            Self::Browser => "Browser",
            Self::Android => "Android",
        }
    }

    /// The GTK widget name a pane carries, so a tab-view page can be mapped
    /// back to its kind from its child widget.
    pub fn widget_name(self) -> &'static str {
        match self {
            Self::Browser => "pane-browser",
            Self::Android => "pane-android",
        }
    }

    /// Inverse of [`PaneKind::widget_name`]. Pure.
    pub fn from_widget_name(name: &str) -> Option<Self> {
        [Self::Browser, Self::Android]
            .into_iter()
            .find(|kind| kind.widget_name() == name)
    }

    /// The other kind.
    pub fn other(self) -> Self {
        match self {
            Self::Browser => Self::Android,
            Self::Android => Self::Browser,
        }
    }
}

/// What Alt+2 does to the active group's pane area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserKeyStep {
    /// No browser yet: start one, in front, area visible.
    Spawn,
    /// Browser to the front, area visible.
    Front,
    /// Hide the area; every pane keeps running.
    Hide,
}

/// Alt+2: spawn the browser, bring it to the front, or — when it already is
/// in front and showing — hide the area. Pure.
pub fn browser_key_step(has_browser: bool, panes_visible: bool, front: PaneKind) -> BrowserKeyStep {
    if !has_browser {
        BrowserKeyStep::Spawn
    } else if panes_visible && front == PaneKind::Browser {
        BrowserKeyStep::Hide
    } else {
        BrowserKeyStep::Front
    }
}

/// The front pane after `opened` joined the area. An interactive open puts it
/// in front; a quiet one (CLI, restore) leaves the front alone unless there
/// is no other pane to be in front. Pure.
pub fn front_after_open(front: PaneKind, opened: PaneKind, other_open: bool, quiet: bool) -> PaneKind {
    if quiet && other_open { front } else { opened }
}

/// The front pane after `closed` left the area: the remaining pane when the
/// closed one was in front, otherwise unchanged. Pure.
pub fn front_after_close(front: PaneKind, closed: PaneKind, other_open: bool) -> PaneKind {
    if front == closed && other_open {
        closed.other()
    } else {
        front
    }
}

/// Whether the area is visible after a quiet open: an existing area keeps its
/// state; a first pane shows only in the active group, as a CLI-opened
/// browser always has. Pure.
pub fn quiet_open_visible(panes_visible: bool, group_active: bool, other_open: bool) -> bool {
    panes_visible || (group_active && !other_open)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib state::tests`
Expected: all pass (5 new).

- [ ] **Step 5: Format and lint**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add src/state.rs
git commit -m "Decide which pane is in front and what Alt+2 does"
```

---

### Task 5: `cli.rs` — one builder for a session's environment

**Files:**
- Modify: `src/cli.rs:20-26` (env constants), new function after `with_default_group` (`src/cli.rs:79-93`)
- Modify: `src/app.rs:133-141` (env constants), `src/app.rs:1314-1328` (`Msg::ChildExited` reattach env), `src/app.rs:3098-3107` (`group_env_pairs`), `src/app.rs:3789-3802` (`add_tab` spawn env)
- Test: `src/cli.rs` (`mod tests`, appended)

**Interfaces:**
- Consumes: nothing.
- Produces (used by Tasks 7, 9):
  - `pub const ENV_CDP_PLAYWRIGHT: &str = "PLAYWRIGHT_MCP_CDP_ENDPOINT"`, `pub const CDP_ENV_KEYS: [&str; 2]`
  - `pub const ENV_ANDROID_CTL: &str = "KABELSALAT_ANDROID_CTL"`, `pub const ENV_ANDROID_ADB: &str = "KABELSALAT_ANDROID_ADB"`
  - `pub fn android_env_pairs(ctl: &str, adb: &str) -> [(&'static str, String); 2]`
  - `pub fn session_env_pairs(group_uuid: &str, cdp: Option<&str>, android: Option<(&str, &str)>) -> Vec<(&'static str, String)>`
  - `App::group_env_pairs(&self, group_id: usize) -> Option<Vec<(&'static str, String)>>` (app.rs, changed signature)

The spec asks for `tmuxctl.rs` set/unset tests of the two new keys. `TmuxCtl::set_environment_args`/`unset_environment_args` (`src/tmuxctl.rs:884-903`) take any key and are already covered by `set_environment_argv_shape`/`unset_environment_argv_shape` (`src/tmuxctl.rs:1187-1212`); a test with a new key string would pass on its first run and prove nothing. The new behaviour — *which* pairs a session gets — is pure, so it is tested here instead, and `tmuxctl.rs` is not modified.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/cli.rs`:

```rust
    // --- session environment ---

    #[test]
    fn a_session_always_names_its_group() {
        assert_eq!(
            session_env_pairs("aaa-111", None, None),
            vec![(ENV_GROUP, "aaa-111".to_string())]
        );
    }

    #[test]
    fn a_live_browser_adds_the_cdp_pair() {
        assert_eq!(
            session_env_pairs("aaa-111", Some("http://127.0.0.1:40455"), None),
            vec![
                (ENV_GROUP, "aaa-111".to_string()),
                ("KABELSALAT_CDP", "http://127.0.0.1:40455".to_string()),
                (
                    "PLAYWRIGHT_MCP_CDP_ENDPOINT",
                    "http://127.0.0.1:40455".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_booted_android_adds_its_control_socket_and_serial() {
        assert_eq!(
            session_env_pairs(
                "aaa-111",
                None,
                Some(("/tmp/ctl.sock", "192.168.240.112:5555"))
            ),
            vec![
                (ENV_GROUP, "aaa-111".to_string()),
                ("KABELSALAT_ANDROID_CTL", "/tmp/ctl.sock".to_string()),
                ("KABELSALAT_ANDROID_ADB", "192.168.240.112:5555".to_string()),
            ]
        );
        assert_eq!(
            android_env_pairs("/tmp/ctl.sock", "10.0.3.9:5555"),
            [
                (ENV_ANDROID_CTL, "/tmp/ctl.sock".to_string()),
                (ENV_ANDROID_ADB, "10.0.3.9:5555".to_string()),
            ]
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cli::tests::a_`
Expected: compile errors — `session_env_pairs`, `android_env_pairs`, `ENV_ANDROID_CTL`, `ENV_ANDROID_ADB` not found.

- [ ] **Step 3: Write the implementation**

In `src/cli.rs`, after `pub const ENV_CDP: &str = "KABELSALAT_CDP";` (line 26):

```rust
/// The same endpoint under the name Playwright's MCP server reads.
pub const ENV_CDP_PLAYWRIGHT: &str = "PLAYWRIGHT_MCP_CDP_ENDPOINT";
/// The endpoint pair: set on discovery, unset on browser close. `ENV_GROUP`
/// is deliberately not in here — it describes the tab, not the browser, and
/// is never unset.
pub const CDP_ENV_KEYS: [&str; 2] = [ENV_CDP, ENV_CDP_PLAYWRIGHT];
/// The Android pane's control socket, while a booted Android is up in the
/// group (`screenshot`, `click`, `type`, `key`, `resize`; one line each).
pub const ENV_ANDROID_CTL: &str = "KABELSALAT_ANDROID_CTL";
/// The adb serial of that Android (`<ip>:5555`).
pub const ENV_ANDROID_ADB: &str = "KABELSALAT_ANDROID_ADB";
```

After `with_default_group` (ends `src/cli.rs:93`):

```rust
/// The Android pair, in the order `ENV_ANDROID_CTL`, `ENV_ANDROID_ADB`.
pub fn android_env_pairs(ctl: &str, adb: &str) -> [(&'static str, String); 2] {
    [
        (ENV_ANDROID_CTL, ctl.to_string()),
        (ENV_ANDROID_ADB, adb.to_string()),
    ]
}

/// The variables a group's session is created with: the group identity
/// always, the CDP pair while the browser has an endpoint, the Android pair
/// while a booted Android is up. Pure.
pub fn session_env_pairs(
    group_uuid: &str,
    cdp: Option<&str>,
    android: Option<(&str, &str)>,
) -> Vec<(&'static str, String)> {
    let mut env = vec![(ENV_GROUP, group_uuid.to_string())];
    if let Some(url) = cdp {
        for key in CDP_ENV_KEYS {
            env.push((key, url.to_string()));
        }
    }
    if let Some((ctl, adb)) = android {
        env.extend(android_env_pairs(ctl, adb));
    }
    env
}
```

In `src/app.rs`, replace the constants block (`src/app.rs:133-141`, from `/// Environment keys published into each tab's tmux session (see` through `const CDP_ENV_KEYS: [&str; 2] = [ENV_CDP, ENV_CDP_PLAYWRIGHT];`) with:

```rust
/// Environment keys published into each tab's tmux session (see
/// docs/superpowers/specs/2026-07-30-cdp-endpoint-design.md); the pair
/// constants and the builders live in `cli.rs`, where they are tested.
const ENV_GROUP: &str = crate::cli::ENV_GROUP;
```

and add to the import block at the top of `src/app.rs` (after `use crate::claude;`, line 18):

```rust
use crate::cli::CDP_ENV_KEYS;
```

Replace `group_env_pairs` (`src/app.rs:3098-3107`) with:

```rust
    /// The env pairs a session of this group is created with (see
    /// `cli::session_env_pairs`). None only for a group id that no longer
    /// exists.
    fn group_env_pairs(&self, group_id: usize) -> Option<Vec<(&'static str, String)>> {
        self.groups.iter().find(|g| g.id == group_id).map(|g| {
            let cdp = g.browser.as_ref().and_then(|b| b.cdp_url());
            crate::cli::session_env_pairs(&g.uuid, cdp.as_deref(), None)
        })
    }
```

In `Msg::ChildExited` (`src/app.rs:1319-1328`), replace

```rust
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
```

with

```rust
                            let pairs = self.group_env_pairs(group).unwrap_or_default();
                            let env: Vec<(&str, &str)> =
                                pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
```

In `add_tab` (`src/app.rs:3793-3802`), replace the same ten-line block (`let group_info = self.group_env_pairs(group);` … closing `}`, indented three levels less there) with the same three lines at that indentation; `cargo fmt` settles the wrapping.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib cli::tests && cargo build`
Expected: 3 new tests pass; the binary builds. (`app.rs` no longer has its own `ENV_CDP`/`ENV_CDP_PLAYWRIGHT`; nothing else in it used them — `grep -n 'ENV_CDP' src/app.rs` shows only `CDP_ENV_KEYS` uses.)

- [ ] **Step 5: Format, lint, full test**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean; all pass.

- [ ] **Step 6: Commit**

```bash
git add src/cli.rs src/app.rs
git commit -m "Build every session's environment in one tested place"
```

---

### Task 6: `app.rs` — a tab host for the pane area (browser only, behaviour unchanged)

`app.rs` wiring; no unit tests (CLAUDE.md: the GTK layer is the untested exception). Every decision it wires — which pane is in front, what Alt+2 does, whether a quiet open shows the area — was tested in Task 4. Verification is build, lint, the full test suite, and the manual checklist in Step 9.

**Files:**
- Modify: `src/app.rs` — imports (`src/app.rs:22`), `SHORTCUTS` (`src/app.rs:56-59`), `Group` (`src/app.rs:350-375`), new `PaneHost` after `Group`, `App` field `attached_browser` (`src/app.rs:425`), `Msg` (`src/app.rs:597-617`), the header-bar "browser hidden" button (`src/app.rs:820-828`), `update()` (after `Msg::CopyCdpEndpoint`, `src/app.rs:1458-1467`), `create_group` (`src/app.rs:1824-1840`), the group loop in `restore_or_fresh` (`src/app.rs:1904-1920`), and in the browser-pane section: `active_browser_hidden` (`src/app.rs:2704-2708`), `sync_browser_pane` (`src/app.rs:2821-2860`), `focus_browser_or_terminal` (`src/app.rs:2907-2927`), `toggle_browser` (`src/app.rs:2929-2968`), `close_browser` (`src/app.rs:2970-2995`), `restore_browser` (`src/app.rs:3230-3263`), `open_browser` (`src/app.rs:3385-3427`).

**Interfaces:**
- Consumes (Task 4): `state::{PaneKind, BrowserKeyStep, browser_key_step, front_after_open, front_after_close, quiet_open_visible}`.
- Produces (used by Tasks 7, 8, 9):
  - `struct PaneHost { root: gtk::Box, view: adw::TabView, pages: RefCell<Vec<(PaneKind, adw::TabPage)>>, syncing: Rc<Cell<bool>> }` with `fn new(input: &relm4::Sender<Msg>) -> Self`, `fn add(&self, kind: PaneKind, widget: &gtk::Widget)`, `fn remove(&self, kind: PaneKind)`, `fn select(&self, kind: PaneKind)`, `fn page(&self, kind: PaneKind) -> Option<adw::TabPage>`, `fn is_empty(&self) -> bool`
  - `Group` fields `panes_visible: bool` (renamed from `browser_visible`), `front_pane: PaneKind`, `panes: PaneHost`; methods `fn has_panes(&self) -> bool`, `fn front_widget(&self) -> Option<gtk::Widget>`
  - `App::attached_host: Option<usize>` (renamed from `attached_browser`), `App::sync_pane_host(&mut self)` (renamed from `sync_browser_pane`), `App::focus_panes_or_terminal(&self)` (renamed from `focus_browser_or_terminal`), `App::active_panes_hidden(&self) -> bool` (renamed from `active_browser_hidden`)
  - `Msg::PaneSelected(PaneKind)`, `Msg::ShowPanes`

How the host works: every group owns a `PaneHost` from creation — a vertical `gtk::Box` holding an `adw::TabBar` over an `adw::TabView`, one page per open pane, titled by `PaneKind::title`, each page's child the pane's `WaylandPane`, named by `PaneKind::widget_name`. Only the active group's host is ever parented, and only while it has a page and `panes_visible` — exactly where the bare browser widget sits today (`browser_paned`'s end child), so a group without panes looks as it does today. Hiding, a group switch, and a non-selected tab all unmap the pane, which pauses its frame pump (klamottenkiste's map lifecycle) while the hosted app keeps running.

Two deliberate details:
- `TabView::set_shortcuts(TabViewShortcuts::NONE)`: libadwaita's defaults include Alt+1…9 and Ctrl+PgUp/PgDn, which are the window's own shortcuts.
- The spec says non-front panes are paused "via `set_visible(false)`". In a `TabView` that would be wrong: its pages live in an internal stack that switches away from a hidden child on its own. Pausing comes from unmapping instead (non-selected page, detached host), which is what `set_visible(false)` achieves for a bare widget anyway; no pane widget is ever hidden.
- A user closing a tab is refused in the view and turned into the existing close path (`Msg::CloseBrowser` here; Task 7 adds Android), which removes the page itself. Programmatic adds, removes and selects set `syncing`, so the view's signals do not echo them back — the same guard `reselecting` gives the sidebar.

- [ ] **Step 1: Mechanical renames**

Run:

```bash
sed -i \
  -e 's/browser_visible/panes_visible/g' \
  -e 's/attached_browser/attached_host/g' \
  -e 's/sync_browser_pane/sync_pane_host/g' \
  -e 's/focus_browser_or_terminal/focus_panes_or_terminal/g' \
  -e 's/active_browser_hidden/active_panes_hidden/g' \
  src/app.rs
```

Then `cargo build` — expected to succeed unchanged (pure renames).

- [ ] **Step 2: Imports, shortcut wording, `Group`, `PaneHost`**

Replace the `state` import (`src/app.rs:22`) with:

```rust
use crate::state::{
    self, BrowserKeyStep, ClaudeSession, PaneKind, SavedGroup, SavedState, SavedTab,
    SidebarOrder,
};
```

In `SHORTCUTS`, change the four Alt+2 descriptions from `"Toggle browser pane"` to `"Browser pane: open, bring to front, or hide"`.

In `pub struct Group`, replace the `panes_visible` field and its doc comment (formerly `browser_visible`) with:

```rust
    /// Whether the pane area is shown rather than hidden. Only meaningful
    /// while a pane is open; hidden panes keep running.
    panes_visible: bool,
    /// Which pane the area shows on top when it holds more than one. Runtime
    /// only: a restart brings the browser back in front.
    front_pane: PaneKind,
    /// The tab bar and tab view holding this group's panes; parented into
    /// `browser_paned` only while this is the active group and shows a pane.
    panes: PaneHost,
```

Directly after the closing `}` of `pub struct Group`, add:

```rust
impl Group {
    /// Does the pane area hold at least one pane?
    fn has_panes(&self) -> bool {
        !self.panes.is_empty()
    }

    /// The widget of the pane in front, if that pane is open.
    fn front_widget(&self) -> Option<gtk::Widget> {
        self.panes.page(self.front_pane).map(|page| page.child())
    }
}

/// A group's pane area: an `adw::TabBar` over an `adw::TabView`, one page
/// per open pane. Owns no pane: the `Browser`/`Android` on the group do, and
/// their widgets are only parented here.
struct PaneHost {
    root: gtk::Box,
    view: adw::TabView,
    /// The open pages by kind; the view itself has no notion of kinds.
    pages: RefCell<Vec<(PaneKind, adw::TabPage)>>,
    /// Set while the app adds, removes or selects pages itself, so the view's
    /// signals do not echo those changes back as messages.
    syncing: Rc<Cell<bool>>,
}

impl PaneHost {
    fn new(input: &relm4::Sender<Msg>) -> Self {
        let view = adw::TabView::new();
        // Alt+digits and Ctrl+PgUp/PgDn belong to the window's shortcuts.
        view.set_shortcuts(adw::TabViewShortcuts::NONE);
        view.set_hexpand(true);
        view.set_vexpand(true);
        let bar = adw::TabBar::new();
        bar.set_view(Some(&view));
        // One pane still gets its tab: it says what the pane is and carries
        // the close button.
        bar.set_autohide(false);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);
        root.append(&bar);
        root.append(&view);

        let syncing = Rc::new(Cell::new(false));
        view.connect_close_page({
            let syncing = syncing.clone();
            let input = input.clone();
            move |view, page| {
                if syncing.get() {
                    // The app's own removal: let the default handler finish it.
                    return gtk::glib::Propagation::Proceed;
                }
                // A click on a tab's close button. Refuse it here; the close
                // path tears the pane down and removes the page itself.
                view.close_page_finish(page, false);
                if let Some(PaneKind::Browser) =
                    PaneKind::from_widget_name(page.child().widget_name().as_str())
                {
                    let _ = input.send(Msg::CloseBrowser);
                }
                gtk::glib::Propagation::Stop
            }
        });
        view.connect_selected_page_notify({
            let syncing = syncing.clone();
            let input = input.clone();
            move |view| {
                if syncing.get() {
                    return;
                }
                if let Some(kind) = view
                    .selected_page()
                    .and_then(|page| PaneKind::from_widget_name(page.child().widget_name().as_str()))
                {
                    let _ = input.send(Msg::PaneSelected(kind));
                }
            }
        });

        Self {
            root,
            view,
            pages: RefCell::new(Vec::new()),
            syncing,
        }
    }

    /// Add a page for `kind` holding `widget`. A kind that already has a page
    /// is left alone.
    fn add(&self, kind: PaneKind, widget: &gtk::Widget) {
        if self.page(kind).is_some() {
            return;
        }
        widget.set_widget_name(kind.widget_name());
        self.syncing.set(true);
        let page = self.view.append(widget);
        page.set_title(kind.title());
        self.syncing.set(false);
        self.pages.borrow_mut().push((kind, page));
    }

    /// Remove `kind`'s page; its widget is unparented, not destroyed — the
    /// pane that owns it decides that.
    fn remove(&self, kind: PaneKind) {
        let page = {
            let mut pages = self.pages.borrow_mut();
            let Some(index) = pages.iter().position(|(k, _)| *k == kind) else {
                return;
            };
            pages.remove(index).1
        };
        self.syncing.set(true);
        self.view.close_page(&page);
        self.syncing.set(false);
    }

    /// Show `kind`'s page, if there is one.
    fn select(&self, kind: PaneKind) {
        let Some(page) = self.page(kind) else {
            return;
        };
        self.syncing.set(true);
        self.view.set_selected_page(&page);
        self.syncing.set(false);
    }

    fn page(&self, kind: PaneKind) -> Option<adw::TabPage> {
        self.pages
            .borrow()
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, page)| page.clone())
    }

    fn is_empty(&self) -> bool {
        self.pages.borrow().is_empty()
    }
}
```

- [ ] **Step 3: Construct the host with every group**

In `create_group` (`src/app.rs:1827-1838`), the `Group { … }` literal gains, after `panes_visible: false,`:

```rust
            front_pane: PaneKind::Browser,
            panes: PaneHost::new(&self.input),
```

In `restore_or_fresh`'s `for group in &saved.groups` loop (`src/app.rs:1905-1919`), the `Group { … }` literal gains, after `panes_visible: group.browser_open,`:

```rust
                front_pane: PaneKind::Browser,
                panes: PaneHost::new(&self.input),
```

- [ ] **Step 4: Messages**

In `enum Msg`, after `CopyCdpEndpoint,` add:

```rust
    /// The user picked a tab in the active group's pane area.
    PaneSelected(PaneKind),
    /// Show the active group's hidden pane area (the header-bar indicator).
    ShowPanes,
```

In `update()`, directly after the `Msg::CopyCdpEndpoint => { … }` arm, add:

```rust
            // Runtime only, like the CDP messages: the front pane is not
            // persisted, so this skips the save at the bottom.
            Msg::PaneSelected(kind) => {
                if let Some(id) = self.active_group()
                    && let Some(group) = self.groups.iter_mut().find(|g| g.id == id)
                {
                    group.front_pane = kind;
                }
                self.focus_panes_or_terminal();
                return;
            }
            Msg::ShowPanes => {
                if let Some(id) = self.active_group()
                    && let Some(group) = self.groups.iter_mut().find(|g| g.id == id)
                    && group.has_panes()
                {
                    group.panes_visible = true;
                }
                self.sync_pane_host();
                self.focus_panes_or_terminal();
            }
```

In the `view!` header bar, replace the "browser hidden" button (`src/app.rs:820-828`, the one with `add_css_class: "browser-hidden"`) with:

```rust
                pack_end = &gtk::Button {
                    set_icon_name: "web-browser-symbolic",
                    add_css_class: "browser-hidden",
                    set_tooltip_text: Some("Panes hidden — click to show them"),
                    #[watch]
                    set_visible: model.active_panes_hidden(),
                    connect_clicked => Msg::ShowPanes,
                },
```

- [ ] **Step 5: Visibility and focus**

Replace `active_panes_hidden` (formerly `active_browser_hidden`) with:

```rust
    /// Does the active group have panes that are currently hidden? Drives
    /// the header-bar indicator.
    fn active_panes_hidden(&self) -> bool {
        self.active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .is_some_and(|g| g.has_panes() && !g.panes_visible)
    }
```

Replace `sync_pane_host` (formerly `sync_browser_pane`, whole function including its doc comment) with:

```rust
    /// Make the split show exactly the active group's pane area, if it has a
    /// visible one, with its front pane selected. The outgoing host's divider
    /// position is saved and the host is unparented — its panes are unmapped,
    /// which pauses their frame pumps, and keep running, so coming back to
    /// the group is instant with state intact.
    fn sync_pane_host(&mut self) {
        // Before anything else: the active group may have changed even when
        // the attached host does not (e.g. to a group whose panes are hidden),
        // and the CDP menu must reflect it regardless.
        self.refresh_cdp_menu();
        let want = self.active_group().filter(|id| {
            self.groups
                .iter()
                .any(|g| g.id == *id && g.has_panes() && g.panes_visible)
        });
        if self.attached_host != want {
            if let Some(previous) = self.attached_host.take() {
                let split = self.browser_paned.position() as f64;
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == previous) {
                    group.browser_split = split;
                }
                self.browser_paned.set_end_child(gtk::Widget::NONE);
            }
            if let Some(id) = want
                && let Some(group) = self.groups.iter().find(|g| g.id == id)
            {
                let split = group.browser_split as i32;
                let widget: gtk::Widget = group.panes.root.clone().upcast();
                self.browser_paned.set_end_child(Some(&widget));
                self.apply_browser_split(split, widget);
                self.attached_host = Some(id);
            }
        }
        // Every host shows its front pane, attached or not, so coming back to
        // a group lands on the right tab without a flash of the other one.
        for group in &self.groups {
            group.panes.select(group.front_pane);
        }
    }
```

Replace `focus_panes_or_terminal` (formerly `focus_browser_or_terminal`; keep its doc comment, change "the browser" to "the front pane" in it) body with:

```rust
    fn focus_panes_or_terminal(&self) {
        let front = self
            .active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .filter(|g| g.panes_visible)
            .and_then(Group::front_widget);
        if let Some(widget) = front {
            widget.grab_focus();
        } else if let Some(tab) = self
            .active
            .and_then(|id| self.tabs.iter().find(|t| t.id == id))
        {
            tab.terminal.grab_focus();
        }
    }
```

- [ ] **Step 6: Alt+2 and the browser's open/close paths**

Replace `toggle_browser` (whole function with doc comment) with:

```rust
    /// Alt-2: no browser → spawn it in front and show the area; browser
    /// hidden or behind another pane → bring it to the front and show the
    /// area; browser in front and showing → hide the area (see
    /// `state::browser_key_step`). Never panics: a failure to spawn leaves
    /// the group as it was and reports the reason.
    fn toggle_browser(&mut self) {
        let Some(id) = self.active_group() else {
            return;
        };
        let Some(group) = self.groups.iter().find(|g| g.id == id) else {
            return;
        };
        match state::browser_key_step(group.browser.is_some(), group.panes_visible, group.front_pane) {
            BrowserKeyStep::Hide => {
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    group.panes_visible = false;
                }
            }
            BrowserKeyStep::Front => {
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    group.front_pane = PaneKind::Browser;
                    group.panes_visible = true;
                }
            }
            BrowserKeyStep::Spawn => {
                // Keyed by the group's uuid: ids are reused, so an id-keyed
                // profile could be one the startup sweep is still deleting.
                let uuid = group.uuid.clone();
                // Cloned like the uuid, so no borrow of `self.groups` is alive
                // across the spawn.
                let default_url = group.default_url.clone();
                match Browser::spawn(&uuid, &self.state_dir, default_url.as_deref()) {
                    Ok(browser) => {
                        if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                            group.panes.add(PaneKind::Browser, browser.widget().upcast_ref());
                            group.browser = Some(browser);
                            group.front_pane = PaneKind::Browser;
                            group.panes_visible = true;
                        }
                        self.start_browser_poll();
                        self.start_cdp_poll(id);
                    }
                    Err(err) => {
                        self.show_notice(&err.to_string());
                        return;
                    }
                }
            }
        }
        self.sync_pane_host();
        self.focus_panes_or_terminal();
    }
```

Replace `close_browser`'s body (keep its doc comment) with:

```rust
    fn close_browser(&mut self, id: usize, disposition: ProfileDisposition) {
        let Some(group) = self.groups.iter_mut().find(|g| g.id == id) else {
            return;
        };
        let Some(mut browser) = group.browser.take() else {
            return;
        };
        // Unparent first, so the compositor is torn down outside the layout.
        group.panes.remove(PaneKind::Browser);
        let other_open = group.has_panes();
        group.front_pane = state::front_after_close(group.front_pane, PaneKind::Browser, other_open);
        if !other_open {
            group.panes_visible = false;
        }
        // NLL: `group` is done; &self calls are fine from here. Runs only
        // when a browser was actually present, so a duplicate BrowserDied
        // (see the doc comment above) stays a true no-op.
        self.cdp_env_unset(id);
        self.sync_pane_host();
        browser.teardown(disposition);
        drop(browser);
    }
```

In `restore_browser`, replace the `Ok(browser) => { … }` arm and the `Err(err) => { … }` arm with:

```rust
                Ok(browser) => {
                    if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                        let other_open = group.has_panes();
                        group.panes.add(PaneKind::Browser, browser.widget().upcast_ref());
                        group.front_pane = state::front_after_open(
                            group.front_pane,
                            PaneKind::Browser,
                            other_open,
                            true,
                        );
                        group.browser = Some(browser);
                    }
                    self.start_browser_poll();
                    self.start_cdp_poll(id);
                }
                Err(err) => {
                    eprintln!("kabelsalat: restoring browser for group {id}: {err}");
                    if let Some(group) = self.groups.iter_mut().find(|g| g.id == id)
                        && !group.has_panes()
                    {
                        group.panes_visible = false;
                    }
                    // One dialog, however many groups fail for the same reason.
                    if !self.browser_restore_error_shown {
                        self.browser_restore_error_shown = true;
                        self.show_notice(&err.to_string());
                    }
                }
```

In `open_browser`, replace `let visible = self.active_group() == Some(id);` with `let active = self.active_group() == Some(id);`, and the `Ok(browser) => { … }` arm with:

```rust
            Ok(browser) => {
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    let other_open = group.has_panes();
                    group.panes.add(PaneKind::Browser, browser.widget().upcast_ref());
                    // Quiet: the front pane and the area only change when this
                    // is the group's first pane (see the state.rs tests).
                    group.front_pane =
                        state::front_after_open(group.front_pane, PaneKind::Browser, other_open, true);
                    group.panes_visible = state::quiet_open_visible(group.panes_visible, active, other_open);
                    group.browser = Some(browser);
                }
                self.start_browser_poll();
                self.start_cdp_poll(id);
            }
```

- [ ] **Step 7: Build, lint, test**

Run: `cargo fmt && cargo build && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean; all tests pass. (`grep -n 'set_visible(' src/app.rs` must no longer show any `browser.set_visible` call; `Browser::set_visible` stays in `browser.rs`, a `pub` item of a `pub mod`.)

- [ ] **Step 8: Commit**

```bash
git add src/app.rs
git commit -m "Host a group's panes in a tab view"
```

- [ ] **Step 9: Manual check (needs the GUI; ask the user to drive if headless)**

kabelsalat is single-instance (`de.nereide.kabelsalat`): quit the running one first — its tmux sessions survive — or `cargo run` only forwards to it. Then run `cargo run` and confirm:
1. A group without a browser looks exactly as before (no tab bar, terminal full width).
2. Alt+2 opens the browser in a pane area with one "Browser" tab; Alt+2 again hides the area; again shows it.
3. The tab's close button closes the browser (as "Close browser" in the overflow menu does), and the area disappears.
4. With the browser hidden, the blue header icon shows; clicking it shows the area.
5. Switching groups swaps pane areas; the divider position per group survives a switch and a restart.
6. `kabelsalat browser -g <other group>` from a tab brings that group's browser up hidden, without focus or a group switch.
7. Alt+1, Alt+2, Ctrl+PgUp/PgDn still do what F1 lists while the browser has focus.

---

### Task 7: `app.rs` — the Android pane (Alt+3, menu, boot, death, env, screenshot)

`app.rs` wiring, no unit tests; what it wires is tested in Tasks 1, 2, 4 and 5. Verification: build, lint, tests, and the manual checklist in Step 11.

**Files:**
- Modify: `src/cli.rs` (one constant after `ENV_ANDROID_ADB`, from Task 5)
- Modify: `src/app.rs` — imports, `SHORTCUTS` and `SHORTCUT_ALIASES` (`src/app.rs:29-81`), `Group` (+ `android`), both `Group { … }` literals (Task 6 Step 3), `PaneHost::new`'s close handler (Task 6 Step 2), `Msg`, `update()`, the primary menu in `view!` (`src/app.rs:778-790`, before the "Keyboard shortcuts (F1)" button), the screenshot header button (`src/app.rs:830-842`), `shutdown` (`src/app.rs:1753-1792`), and in the browser-pane section `active_browser_running`, `capture_browser`, `paste_screenshot`, `session_env_refresh`, `group_env_pairs`, `poll_browsers`; new functions after `open_browser`.

**Interfaces:**
- Consumes: Task 2 `android::{Android, BootResult, log_path}` and `Android::{spawn, widget, is_running, capture_frame, has_exited, control_socket_path, boot_id, adb, set_ready, teardown}`; Task 1 `android::{claim, Claim}`; Task 4 `state::{front_after_close, PaneKind}`; Task 5 `cli::{android_env_pairs, session_env_pairs}`; Task 6 `PaneHost`, `Group::{has_panes, front_pane, panes_visible}`, `App::{sync_pane_host, focus_panes_or_terminal}`.
- Produces (used by Tasks 8, 9):
  - `pub const ANDROID_ENV_KEYS: [&str; 2]` in `src/cli.rs`
  - `Group::android: Option<Android>`
  - `Msg::ToggleAndroid`, `Msg::StopAndroid`, `Msg::AndroidReady(usize, u64, String)` (group id, boot id, adb serial), `Msg::AndroidDied(usize, u64, String)` (group id, boot id, reason)
  - `App::spawn_android(&mut self, id: usize) -> Result<(), String>` (claims, spawns, adds the page; touches neither `front_pane` nor `panes_visible`)
  - `App::close_android(&mut self, id: usize)`, `App::android_env_set(&self, group_id: usize)`, `App::android_env_unset(&self, group_id: usize)`, `fn android_env(android: &Android) -> Option<[(&'static str, String); 2]>`

Decisions:
- **Menu.** "Open Android pane (Alt+3)" and "Stop Android" go into the primary (hamburger) menu: the browser's overflow menu is only visible while the group has a browser, and Stop Android must be reachable from any group (one Waydroid per machine).
- **Boot reports carry a boot id** (Task 2's `next_boot_id`): a report from a pane that was stopped and replaced within its 60 s budget is ignored instead of tearing down the replacement.
- **Death** (boot failure, or `waydroid session start` exiting — found by the existing 2 s browser poll, which is what "`PollAndroid` folds into the existing poll timer" means) tears the pane down like Stop and shows a notice with the reason and the session-log path.
- **App exit** stops the session (the compositor dies with the process, so Android would lose its display) but keeps the `Android` on its group, torn down, the way `browser::shutdown_all` keeps torn-down browsers — so the final save still records the owner for restore (Task 8).
- **One owner.** `spawn_android` checks `android::claim` against the live groups, so a stale CLI snapshot cannot open a second session.

- [ ] **Step 1: Constant and imports**

In `src/cli.rs`, after `pub const ENV_ANDROID_ADB …;`:

```rust
/// The Android pair: set once Android has booted, unset on stop or death.
pub const ANDROID_ENV_KEYS: [&str; 2] = [ENV_ANDROID_CTL, ENV_ANDROID_ADB];
```

In `src/app.rs`, add to the imports (after `use crate::autostart;`):

```rust
use crate::android::{self, Android};
```

and replace `use crate::cli::CDP_ENV_KEYS;` (Task 5) with:

```rust
use crate::cli::{ANDROID_ENV_KEYS, CDP_ENV_KEYS};
```

- [ ] **Step 2: Shortcuts**

In `SHORTCUTS`, after the last Alt+2 entry (`("<Alt>quotedbl", …)`), add:

```rust
    ("<Alt>3", "Android pane: open or bring to front", Msg::ToggleAndroid),
    ("<Alt><Shift>3", "Android pane: open or bring to front", Msg::ToggleAndroid),
    // Shift+3 is `#` on US layouts and `§` on German ones.
    ("<Alt>numbersign", "Android pane: open or bring to front", Msg::ToggleAndroid),
    ("<Alt>section", "Android pane: open or bring to front", Msg::ToggleAndroid),
```

In `SHORTCUT_ALIASES`, after `"<Alt>quotedbl",` add:

```rust
    "<Alt><Shift>3",
    "<Alt>numbersign",
    "<Alt>section",
```

- [ ] **Step 3: `Group::android`**

In `pub struct Group`, after the `browser: Option<Browser>,` field:

```rust
    /// The Android pane, in at most one group at a time (one Waydroid per
    /// machine). Dropping it stops the session and closes the pane.
    android: Option<Android>,
```

In both `Group { … }` literals (`create_group` and the `restore_or_fresh` loop), after `browser: None,` add:

```rust
            android: None,
```

- [ ] **Step 4: Messages and their arms**

In `enum Msg`, after `ShowPanes,` (Task 6) add:

```rust
    /// Alt+3 / primary menu: open Android in the active group, or bring it to
    /// the front; either way the pane area becomes visible.
    ToggleAndroid,
    /// Stop Android wherever it runs: stop the session, close the pane.
    StopAndroid,
    /// Android finished booting: group id, boot id, adb serial.
    AndroidReady(usize, u64, String),
    /// Android failed to boot or its session exited: group id, boot id,
    /// reason.
    AndroidDied(usize, u64, String),
```

In `update()`, after the `Msg::ShowPanes => { … }` arm (Task 6) add:

```rust
            Msg::ToggleAndroid => self.toggle_android(),
            Msg::StopAndroid => self.stop_android(),
            // Runtime state only (the owner was persisted at spawn), like
            // CdpReady.
            Msg::AndroidReady(group_id, boot, serial) => {
                self.android_ready(group_id, boot, serial);
                return;
            }
            Msg::AndroidDied(group_id, boot, reason) => self.android_died(group_id, boot, &reason),
```

Replace the `Msg::Screenshot` and `Msg::ScreenshotCaptured` arms with:

```rust
            Msg::Screenshot => {
                self.capture_front_pane(&sender);
                return;
            }
            Msg::ScreenshotCaptured(tab, result) => {
                match result {
                    Ok(frame) => self.paste_screenshot(tab, frame),
                    Err(detail) => {
                        eprintln!("kabelsalat: pane screenshot failed: {detail}");
                        self.show_toast("The screenshot failed.");
                    }
                }
                return;
            }
```

In `paste_screenshot`, change the toast `"The browser screenshot failed."` to `"The screenshot failed."`.

In `PaneHost::new`'s `connect_close_page` closure (Task 6), replace

```rust
                if let Some(PaneKind::Browser) =
                    PaneKind::from_widget_name(page.child().widget_name().as_str())
                {
                    let _ = input.send(Msg::CloseBrowser);
                }
```

with

```rust
                match PaneKind::from_widget_name(page.child().widget_name().as_str()) {
                    Some(PaneKind::Browser) => {
                        let _ = input.send(Msg::CloseBrowser);
                    }
                    Some(PaneKind::Android) => {
                        let _ = input.send(Msg::StopAndroid);
                    }
                    None => {}
                }
```

- [ ] **Step 5: Menu entries and the screenshot button**

In the primary menu's `gtk::Box` (`view!`), directly before the `gtk::Button` whose label is `"Keyboard shortcuts (F1)"`, add:

```rust
                            gtk::Button {
                                add_css_class: "flat",
                                connect_clicked[sender, primary_menu] => move |_| {
                                    primary_menu.popdown();
                                    sender.input(Msg::ToggleAndroid);
                                },

                                #[wrap(Some)]
                                set_child = &gtk::Label {
                                    set_label: "Open Android pane (Alt+3)",
                                    set_halign: gtk::Align::Start,
                                },
                            },

                            gtk::Button {
                                add_css_class: "flat",
                                #[watch]
                                set_sensitive: model.android_running(),
                                connect_clicked[sender, primary_menu] => move |_| {
                                    primary_menu.popdown();
                                    sender.input(Msg::StopAndroid);
                                },

                                #[wrap(Some)]
                                set_child = &gtk::Label {
                                    set_label: "Stop Android",
                                    set_halign: gtk::Align::Start,
                                },
                            },
```

In the screenshot header button, change `set_tooltip_text: Some("Screenshot browser → paste into terminal"),` to `set_tooltip_text: Some("Screenshot the front pane → paste into terminal"),` and `set_sensitive: model.active_browser_running(),` to `set_sensitive: model.active_front_running(),`.

- [ ] **Step 6: Front-pane capture**

Replace `active_browser_running` (with its doc comment) with:

```rust
    /// Can the active group's front pane be captured right now? Drives the
    /// screenshot button. The pane's compositor renders the frame, so a pane
    /// whose compositor died has nothing left to show — and a hidden one has,
    /// which is why this asks "running", not "visible".
    fn active_front_running(&self) -> bool {
        self.active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .is_some_and(|g| match g.front_pane {
                PaneKind::Browser => g.browser.as_ref().is_some_and(Browser::is_running),
                PaneKind::Android => g.android.as_ref().is_some_and(Android::is_running),
            })
    }

    /// Is Android up anywhere? Drives "Stop Android".
    fn android_running(&self) -> bool {
        self.groups.iter().any(|g| g.android.is_some())
    }
```

Replace `capture_browser` (with its doc comment) with:

```rust
    /// Ask the active group's front pane for a fresh frame. The pane answers
    /// on the GTK main context, so the reply comes back as an ordinary
    /// message — carrying the tab the user asked from, because the answer
    /// may take seconds and the user is free to switch tabs meanwhile.
    fn capture_front_pane(&self, sender: &ComponentSender<Self>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let tab_id = tab.id;
        let Some(group) = self.groups.iter().find(|g| g.id == tab.group) else {
            return;
        };
        let sender = sender.clone();
        let callback = move |result: Result<CapturedFrame, browser::CaptureError>| {
            sender.input(Msg::ScreenshotCaptured(
                tab_id,
                result.map_err(|err| err.to_string()),
            ));
        };
        match (group.front_pane, &group.browser, &group.android) {
            (PaneKind::Browser, Some(browser), _) => browser.capture_frame(callback),
            (PaneKind::Android, _, Some(android)) => android.capture_frame(callback),
            _ => {}
        }
    }
```

- [ ] **Step 7: Environment**

Add, as a free function next to `browser_open_desired` (`src/app.rs:5210`):

```rust
/// The Android pair for a booted Android; `None` while it boots.
fn android_env(android: &Android) -> Option<[(&'static str, String); 2]> {
    let adb = android.adb()?;
    Some(crate::cli::android_env_pairs(
        &android.control_socket_path().to_string_lossy(),
        adb,
    ))
}
```

Replace `group_env_pairs` (Task 5) with:

```rust
    /// The env pairs a session of this group is created with (see
    /// `cli::session_env_pairs`). None only for a group id that no longer
    /// exists.
    fn group_env_pairs(&self, group_id: usize) -> Option<Vec<(&'static str, String)>> {
        self.groups.iter().find(|g| g.id == group_id).map(|g| {
            let cdp = g.browser.as_ref().and_then(|b| b.cdp_url());
            let android = g.android.as_ref().and_then(android_env);
            crate::cli::session_env_pairs(
                &g.uuid,
                cdp.as_deref(),
                android
                    .as_ref()
                    .map(|[(_, ctl), (_, adb)]| (ctl.as_str(), adb.as_str())),
            )
        })
    }
```

In `session_env_refresh`, change the doc comment's "the endpoint pair set or unset by whether the group's browser has a live endpoint" to "the CDP pair and the Android pair each set or unset by whether the group has a live endpoint / a booted Android", and append after the closing `}` of its `for key in CDP_ENV_KEYS` loop:

```rust
        match group.android.as_ref().and_then(android_env) {
            Some(pairs) => {
                for (key, value) in &pairs {
                    if let Err(err) = tmux.set_environment(tab_uuid, key, value) {
                        eprintln!("kabelsalat: refresh {key} on {tab_uuid}: {err}");
                    }
                }
            }
            // As for CDP: a reattached session may carry a stale pair from a
            // previous run, and this is where it is cleared.
            None => {
                for key in ANDROID_ENV_KEYS {
                    if let Err(err) = tmux.unset_environment(tab_uuid, key) {
                        eprintln!("kabelsalat: refresh {key} on {tab_uuid}: {err}");
                    }
                }
            }
        }
```

Add after `cdp_env_unset`:

```rust
    /// Publish the Android pair to every session in the group, once Android
    /// has booted. Local groups only; one failing session never stops the
    /// others, and never Android.
    fn android_env_set(&self, group_id: usize) {
        let Some(tmux) = &self.tmux else { return };
        if self.group_host(group_id).is_some() {
            return;
        }
        let Some(pairs) = self
            .groups
            .iter()
            .find(|g| g.id == group_id)
            .and_then(|g| g.android.as_ref())
            .and_then(android_env)
        else {
            return;
        };
        for tab in self.tabs.iter().filter(|t| t.group == group_id) {
            for (key, value) in &pairs {
                if let Err(err) = tmux.set_environment(&tab.uuid, key, value) {
                    eprintln!("kabelsalat: set {key} on {}: {err}", tab.uuid);
                }
            }
        }
    }

    /// Remove the Android pair from every session in the group, so nothing
    /// started later inherits a dead control socket.
    fn android_env_unset(&self, group_id: usize) {
        let Some(tmux) = &self.tmux else { return };
        if self.group_host(group_id).is_some() {
            return;
        }
        for tab in self.tabs.iter().filter(|t| t.group == group_id) {
            for key in ANDROID_ENV_KEYS {
                if let Err(err) = tmux.unset_environment(&tab.uuid, key) {
                    eprintln!("kabelsalat: unset {key} on {}: {err}", tab.uuid);
                }
            }
        }
    }
```

- [ ] **Step 8: Spawn, toggle, stop, ready, died**

Add after `open_browser`:

```rust
    /// A group's name for messages, or its uuid when it has none.
    fn group_label(&self, uuid: &str) -> String {
        self.groups
            .iter()
            .find(|g| g.uuid == uuid && !g.name.is_empty())
            .map_or_else(|| uuid.to_string(), |g| g.name.clone())
    }

    /// Bring Android up in group `id` and add its page. Touches neither the
    /// front pane nor the area's visibility: the caller decides those. Already
    /// up here is success; up in another group, or a remote group, is refused.
    fn spawn_android(&mut self, id: usize) -> Result<(), String> {
        if self.group_host(id).is_some() {
            return Err("Android runs on this computer; a remote group cannot host it.".into());
        }
        let Some(requester) = self.groups.iter().find(|g| g.id == id).map(|g| g.uuid.clone())
        else {
            return Err("That group no longer exists.".into());
        };
        let owner = self
            .groups
            .iter()
            .find(|g| g.android.is_some())
            .map(|g| g.uuid.clone());
        match android::claim(owner.as_deref(), &requester) {
            android::Claim::Mine => return Ok(()),
            android::Claim::Taken(uuid) => {
                return Err(format!(
                    "Android is already open in group “{}”. There is one per machine; \
                     stop it there first (main menu → Stop Android).",
                    self.group_label(&uuid)
                ));
            }
            android::Claim::Free => {}
        }
        let input = self.input.clone();
        let android = Android::spawn(&self.state_dir, move |boot, result| {
            let msg = match result {
                Ok(serial) => Msg::AndroidReady(id, boot, serial),
                Err(reason) => Msg::AndroidDied(id, boot, reason),
            };
            let _ = input.send(msg);
        })
        .map_err(|err| err.to_string())?;
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
            group.panes.add(PaneKind::Android, android.widget().upcast_ref());
            group.android = Some(android);
        }
        // The exit poll reaps Android's session too.
        self.start_browser_poll();
        Ok(())
    }

    /// Alt+3: open Android in the active group, or bring it to the front;
    /// either way the area becomes visible and gets the keyboard.
    fn toggle_android(&mut self) {
        let Some(id) = self.active_group() else {
            return;
        };
        if let Err(message) = self.spawn_android(id) {
            self.show_notice(&message);
            return;
        }
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
            group.front_pane = PaneKind::Android;
            group.panes_visible = true;
        }
        self.sync_pane_host();
        self.focus_panes_or_terminal();
    }

    /// "Stop Android" / the tab's close button: wherever it runs.
    fn stop_android(&mut self) {
        if let Some(id) = self.groups.iter().find(|g| g.android.is_some()).map(|g| g.id) {
            self.close_android(id);
        }
    }

    /// Tear group `id`'s Android down: unset its env, remove its page, stop
    /// the session, close the pane. A group without one is a no-op.
    fn close_android(&mut self, id: usize) {
        if !self.groups.iter().any(|g| g.id == id && g.android.is_some()) {
            return;
        }
        self.android_env_unset(id);
        let Some(group) = self.groups.iter_mut().find(|g| g.id == id) else {
            return;
        };
        let Some(mut android) = group.android.take() else {
            return;
        };
        group.panes.remove(PaneKind::Android);
        let other_open = group.has_panes();
        group.front_pane = state::front_after_close(group.front_pane, PaneKind::Android, other_open);
        if !other_open {
            group.panes_visible = false;
        }
        self.sync_pane_host();
        android.teardown();
        drop(android);
        self.publish_groups();
    }

    /// The boot thread's success: record the serial, publish the pair. A
    /// report from a pane that was stopped meanwhile is ignored.
    fn android_ready(&mut self, group_id: usize, boot: u64, serial: String) {
        let Some(android) = self
            .groups
            .iter_mut()
            .find(|g| g.id == group_id)
            .and_then(|g| g.android.as_mut())
            .filter(|a| a.boot_id() == boot)
        else {
            return;
        };
        android.set_ready(serial);
        self.android_env_set(group_id);
        self.publish_groups();
    }

    /// Boot failure or session exit: tear down like Stop, and say why.
    fn android_died(&mut self, group_id: usize, boot: u64, reason: &str) {
        let current = self
            .groups
            .iter()
            .find(|g| g.id == group_id)
            .and_then(|g| g.android.as_ref())
            .is_some_and(|a| a.boot_id() == boot);
        if !current {
            return;
        }
        self.close_android(group_id);
        eprintln!("kabelsalat: Android stopped: {reason}");
        self.show_notice(&format!(
            "Android stopped: {reason}\n\nThe Waydroid session log is at {}.",
            android::log_path(&self.state_dir).display()
        ));
    }
```

- [ ] **Step 9: Exit poll and app exit**

Replace `poll_browsers` (with its doc comment) with:

```rust
    /// Reap exited Chromium processes and Waydroid sessions; each comes back
    /// as `BrowserDied` / `AndroidDied`.
    fn poll_browsers(&mut self) {
        let mut dead = Vec::new();
        let mut android_dead = Vec::new();
        for group in self.groups.iter_mut() {
            let id = group.id;
            if let Some(browser) = &mut group.browser {
                match browser.has_exited() {
                    Ok(true) => dead.push(id),
                    Ok(false) => {}
                    Err(err) => eprintln!("kabelsalat: browser {id} could not be polled: {err}"),
                }
            }
            if let Some(android) = &mut group.android {
                match android.has_exited() {
                    Ok(true) => android_dead.push((id, android.boot_id())),
                    Ok(false) => {}
                    Err(err) => eprintln!("kabelsalat: Android in group {id} could not be polled: {err}"),
                }
            }
        }
        for id in dead {
            let _ = self.input.send(Msg::BrowserDied(id));
        }
        for (id, boot) in android_dead {
            let _ = self.input.send(Msg::AndroidDied(
                id,
                boot,
                "the Waydroid session exited.".to_string(),
            ));
        }
    }
```

In `shutdown`, directly after the `browser::shutdown_all( … );` call and before `self.save_state();`, add:

```rust
        // Android's display dies with this process, so its session is
        // stopped too. The `Android` stays on its group, torn down, so the
        // save below still records the owner and the next start restores it.
        let android_groups: Vec<usize> = self
            .groups
            .iter()
            .filter(|g| g.android.is_some())
            .map(|g| g.id)
            .collect();
        for id in android_groups {
            self.android_env_unset(id);
        }
        for group in &mut self.groups {
            if let Some(android) = &mut group.android {
                android.teardown();
            }
        }
```

(`prune_empty_groups` needs nothing new: a pruned group's `Android` is dropped with it, and `Drop` tears it down; its tabs, and with them the sessions carrying the pair, are already gone.)

- [ ] **Step 10: Build, lint, test, commit**

Run: `cargo fmt && cargo build && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean; all pass.

```bash
git add src/cli.rs src/app.rs
git commit -m "Open Android in a group's pane area with Alt+3"
```

- [ ] **Step 11: Manual check (needs the GUI and an initialised Waydroid; ask the user to drive if headless; quit the running kabelsalat first, see Task 6 Step 9)**

1. Without `waydroid` on `PATH` — `cargo build`, then `PATH=/nonexistent ./target/debug/kabelsalat` (tmux is then missing too, so the app runs on plain shells): Alt+3 shows "Waydroid is not installed…" and nothing else changes.
2. With `waydroid session start` already running in a terminal: Alt+3 shows the "already running … waydroid session stop" notice.
3. Normal case: Alt+3 opens an "Android" tab, front and visible; Android boots within ~20 s.
4. Once booted, in a tab of that group: `tmux show-environment KABELSALAT_ANDROID_CTL` and `… KABELSALAT_ANDROID_ADB` are set; in a tab of another group they are unset (`-KABELSALAT_…` or absent).
5. Alt+2 opens the browser as a second tab in front; Alt+3 brings Android back to the front; clicking the tabs switches; Alt+2 with the browser in front hides the area.
6. The screenshot button pastes the front pane's frame (Android when Android is in front).
7. Alt+3 in a second group: notice naming the first group; nothing opens.
8. "Stop Android" (main menu) and the tab's close button both stop it: tab gone, env unset, `waydroid status` says STOPPED.
9. `waydroid session stop` from a terminal while it runs: within ~2 s the tab disappears and a notice names the session log.

---

### Task 8: `app.rs` — restore Android after a restart

`app.rs` wiring, no unit tests; the decisions (`android_restore_target`, `android_owner_desired`, `front_after_open`) were tested in Tasks 3 and 4.

**Files:**
- Modify: `src/app.rs` — `App` struct (after `pending_browser_restore`, `src/app.rs:426-428`), `App` literal in `init` (after `pending_browser_restore: Vec::new(),`, `src/app.rs:1130`), `Msg` (after `RestoreBrowser(usize),`), `update()` (after `Msg::RestoreBrowser(group) => …`), the end of `restore_or_fresh` (`src/app.rs:2034-2041`), `save_state` (`src/app.rs:2044-2115`), new functions after `restore_browser`.

**Interfaces:**
- Consumes: Task 3 `state::{android_restore_target, android_owner_desired}`, `SavedState::android_owner`; Task 4 `state::front_after_open`; Task 7 `App::spawn_android`.
- Produces: `App::pending_android_restore: Option<usize>`, `Msg::RestoreAndroid(usize)`.

Spec: "On launch the pane is reopened hidden for that group, mirroring `pending_browser_restore`." It reuses neither `OpenAndroid` (Task 9) nor `ToggleAndroid`: a CLI open in the active group shows a first pane (Task 4's `quiet_open_visible`), but a restore stays hidden. So restore has its own message, like `RestoreBrowser`, and never touches `panes_visible`; the front pane only changes when Android is the group's only pane (`front_after_open`, quiet). A group restored with only Android therefore comes back with the area hidden and the blue indicator showing.

- [ ] **Step 1: State and message**

In `pub struct App`, after `pending_browser_restore: Vec<usize>,`:

```rust
    /// The group whose Android is waiting to be restored after a restart.
    /// One per machine, so at most one. Persisted as `android_owner` while
    /// it waits (see `state::android_owner_desired`).
    pending_android_restore: Option<usize>,
```

In `init`'s `App { … }` literal, after `pending_browser_restore: Vec::new(),`:

```rust
            pending_android_restore: None,
```

In `enum Msg`, after `RestoreBrowser(usize),`:

```rust
    /// Restart restore: bring this group's Android back, hidden.
    RestoreAndroid(usize),
```

In `update()`, after `Msg::RestoreBrowser(group) => self.restore_browser(group),`:

```rust
            Msg::RestoreAndroid(group) => self.restore_android(group),
```

- [ ] **Step 2: Queue and restore**

Add after `restore_browser`:

```rust
    /// Ask for the pending Android restore on an idle callback, so the window
    /// is interactive first.
    fn queue_android_restore(&self) {
        let Some(id) = self.pending_android_restore else {
            return;
        };
        let input = self.input.clone();
        gtk::glib::idle_add_local_once(move || {
            let _ = input.send(Msg::RestoreAndroid(id));
        });
    }

    /// Bring a restored group's Android back, hidden: the area's visibility
    /// is left as restored, and the front pane only changes when Android is
    /// the group's only pane. A group that vanished or got an Android
    /// meanwhile is skipped. A failure is reported once and forgets the
    /// owner, so a broken Waydroid does not nag at every start.
    fn restore_android(&mut self, id: usize) {
        if self.pending_android_restore != Some(id) {
            return;
        }
        self.pending_android_restore = None;
        let Some(other_open) = self
            .groups
            .iter()
            .find(|g| g.id == id && g.android.is_none())
            .map(Group::has_panes)
        else {
            return;
        };
        match self.spawn_android(id) {
            Ok(()) => {
                if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
                    group.front_pane =
                        state::front_after_open(group.front_pane, PaneKind::Android, other_open, true);
                }
            }
            Err(message) => {
                eprintln!("kabelsalat: restoring Android for group {id}: {message}");
                self.show_notice(&message);
            }
        }
        self.sync_pane_host();
    }
```

At the end of `restore_or_fresh`, replace

```rust
        self.start_browser_maintenance();
        self.save_state();
    }
```

(the second exit, `src/app.rs:2039-2041`; the first exit at `src/app.rs:1898` starts fresh and has no owner to restore) with

```rust
        // Same reasoning as the browsers: filled before the save, so the
        // state written below still names the owner while the restore waits.
        self.pending_android_restore = state::android_restore_target(&saved);
        self.start_browser_maintenance();
        self.queue_android_restore();
        self.save_state();
    }
```

- [ ] **Step 3: Persist the owner**

In `save_state`, before `let state = SavedState {`, add:

```rust
        // The DESIRED owner, like `browser_open`: a restore still queued has
        // no `Android` yet but must still be written.
        let live_owner = self
            .groups
            .iter()
            .find(|g| g.android.is_some())
            .map(|g| g.uuid.as_str());
        let pending_owner = self
            .pending_android_restore
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .map(|g| g.uuid.as_str());
```

and replace the Task 3 lines

```rust
            // No group can hold an Android pane yet; Task 8 of the Android
            // pane plan persists the real owner here.
            android_owner: None,
```

with

```rust
            android_owner: state::android_owner_desired(live_owner, pending_owner),
```

- [ ] **Step 4: Build, lint, test, commit**

Run: `cargo fmt && cargo build && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean; all pass.

```bash
git add src/app.rs
git commit -m "Restore the Android pane after a restart"
```

- [ ] **Step 5: Manual check (GUI + Waydroid; quit the running kabelsalat first)**

1. Open Android in group A (Alt+3), quit kabelsalat: `waydroid status` says STOPPED; `state.json` (`$XDG_STATE_HOME/kabelsalat/state.json`) has `"android_owner": "<A's uuid>"`.
2. Start kabelsalat: A gets its Android tab back, booting, with the area hidden (blue indicator) if A had no browser; with a browser, the browser stays in front.
3. Once booted, A's reattached tabs have `KABELSALAT_ANDROID_CTL`/`_ADB` again.
4. Stop Android, quit, start: no Android, and `android_owner` is gone from `state.json`.
5. With Android owned by A, quit, run `waydroid session start` in a plain terminal, start kabelsalat: one notice ("already running … waydroid session stop"), no Android, and after the next save `android_owner` is gone.

---

### Task 9: `kabelsalat android` — CLI, control socket, GUI hook

One deliverable, because the pieces only compile together: a new `Cli` variant needs its `dispatch` arm, a new `Action` variant needs its arm in `control.rs`, and `cli.rs`/`control.rs` are private modules where an item without a caller fails `clippy -D warnings`.

**Files:**
- Modify: `src/cli.rs` — `EXIT_FAILED` doc (`src/cli.rs:17-18`), `Cli` (`src/cli.rs:28-62`), `needs_instance` (`src/cli.rs:64-73`), `with_default_group` (`src/cli.rs:79-93`), `GroupInfo` (`src/cli.rs:95-107`), `help_text` (`src/cli.rs:141-174`), `parse` (`src/cli.rs:177-206`), new parsers after `parse_browser` (`src/cli.rs:212-229`), `Action` (`src/cli.rs:338-365`), `dispatch` (`src/cli.rs:400-537`), new helpers after `resolve_failure` (`src/cli.rs:541-556`)
- Modify: `src/control.rs` — imports, new functions after `request_open_browser` (`src/control.rs:94-102`), `handle_command_line`'s action match (`src/control.rs:142-176`)
- Modify: `src/lib.rs:111-128` (argument forwarding)
- Modify: `src/app.rs` — `Msg` (after `OpenBrowser { … }`), `update()` (after `Msg::OpenBrowser { group_uuid } => …`), `publish_groups` (`src/app.rs:2117-2133`), new `open_android` after `toggle_android` (Task 7)
- Test: `src/cli.rs` (`mod tests`), `src/control.rs` (`mod tests`)

**Interfaces:**
- Consumes: Task 5/7 `ENV_ANDROID_CTL`; Task 7 `App::spawn_android`, `Group::android`, `Android::{control_socket_path, adb}`; Task 4 `state::{front_after_open, quiet_open_visible}`.
- Produces:
  - `pub enum AndroidCmd { Screenshot(PathBuf), Tap(u32, u32), Type(String), Key(String), Resize(u32, u32) }` with `pub fn args(&self) -> Vec<String>`
  - `Cli::Android { group: Option<String>, cmd: Option<AndroidCmd> }`
  - `pub struct AndroidInfo { pub ctl: PathBuf, pub adb: Option<String> }`; `GroupInfo::android: Option<AndroidInfo>`
  - `Action::OpenAndroid { group_uuid: String }`, `Action::Control { socket: PathBuf, line: String }`
  - `pub fn control_line(cmd: &AndroidCmd) -> String`, `pub fn control_reply(reply: &str) -> Outcome`, `pub fn android_argv(argv0: &str, group: &str, cmd: Option<&AndroidCmd>) -> Vec<String>`
  - `control::send_control(socket: &Path, line: &str, timeout: Duration) -> std::io::Result<String>`, `control::request_open_android(group_uuid: String) -> bool`
  - `Msg::OpenAndroid { group_uuid: String }`

Behaviour (spec §3, plus what it leaves open):

| Invocation | Outcome |
|---|---|
| `android` (group owns a booted Android) | stdout `ctl=<socket>\nadb=<serial>\n`, exit 0, no action |
| `android` (group owns an Android still booting) | stdout empty, stderr "still booting; read `KABELSALAT_ANDROID_CTL` …", exit 0, no action — the spec does not cover this case; asking the GUI again would be a no-op |
| `android` (nobody owns one) | `Action::OpenAndroid`, stdout empty, exit 0. stderr carries a one-line hint, as `browser` does — the spec says "print nothing"; stdout is empty, the hint is the deviation |
| `android` (another group owns it) | exit 3, stderr names that group |
| `android` (remote group) | exit 3, before anything else |
| `android SUB …` (group owns it, booted or not) | `Action::Control { socket, line }`; `control.rs` sends `line`, prints the reply via `control_reply`: `ok` → exit 0, `ok <data>` → `<data>` on stdout, exit 0, `err <msg>` → stderr, exit 4 (`EXIT_FAILED`; the spec does not say) |
| `android SUB …` (no owned pane) | exit 3 |
| control socket unreachable or silent | exit 3 (spec) |
| bad subcommand / arguments | exit 2 |

Parsing rules: `-g/--group` only before the subcommand, so `type -g` types "-g". `tap`/`resize` take non-negative integers; `resize` 1…`i32::MAX` (the protocol's `i32`, positive). `type` text and the `screenshot` path must be one line — a newline would end the protocol's request line. `key` is one word. A relative `screenshot` path is resolved against the caller's directory (the compositor runs inside the GUI process, whose cwd is not the caller's). The spec's wire verbs: `tap` → `click X Y`; the others keep their names.

The socket round trip runs on the GUI's main thread (the gio command-line handler). That is safe: the control socket is served by the compositor's own `nws-control` thread and event loop, never by the GTK thread, and the server closes one connection per client after EOF, so the client writes its line, shuts down its write half and reads one reply line, bounded by a 10 s timeout.

- [ ] **Step 1: Write the failing tests**

First give every existing `GroupInfo` literal in `src/cli.rs`'s tests the new field (11 literals; each has a single-line `cdp: None,` or `cdp: Some(…),` field — the pattern deliberately skips `session_env_pairs`'s `cdp: Option<&str>,` parameter from Task 5):

```bash
sed -i 's/^\(\s*\)cdp: \(None\|Some(.*)\),$/&\n\1android: None,/' src/cli.rs
grep -c '^\s*android: None,' src/cli.rs   # expect 11
```

Then append inside `mod tests` in `src/cli.rs`:

```rust
    // --- android ---

    fn android(group: &str, cmd: Option<AndroidCmd>) -> Cli {
        Cli::Android {
            group: Some(group.into()),
            cmd,
        }
    }

    #[test]
    fn android_parses_with_and_without_a_group() {
        assert_eq!(
            parse(&args(&["android"])),
            Ok(Cli::Android {
                group: None,
                cmd: None
            })
        );
        assert_eq!(parse(&args(&["android", "-g", "web"])), Ok(android("web", None)));
        assert_eq!(
            parse(&args(&["android", "--group", "aaa-111", "tap", "10", "20"])),
            Ok(android("aaa-111", Some(AndroidCmd::Tap(10, 20))))
        );
    }

    #[test]
    fn android_parses_every_subcommand() {
        let cases = [
            (
                vec!["screenshot", "/tmp/a b.png"],
                AndroidCmd::Screenshot(PathBuf::from("/tmp/a b.png")),
            ),
            (vec!["tap", "0", "1080"], AndroidCmd::Tap(0, 1080)),
            (
                vec!["type", "hello world"],
                AndroidCmd::Type("hello world".into()),
            ),
            (vec!["key", "enter"], AndroidCmd::Key("enter".into())),
            (vec!["resize", "720", "1280"], AndroidCmd::Resize(720, 1280)),
        ];
        for (tail, cmd) in cases {
            let mut argv = vec!["android"];
            argv.extend(tail);
            assert_eq!(
                parse(&args(&argv)),
                Ok(Cli::Android {
                    group: None,
                    cmd: Some(cmd)
                })
            );
        }
    }

    #[test]
    fn the_group_flag_only_counts_before_the_subcommand() {
        // Text to type is never read as a flag.
        assert_eq!(
            parse(&args(&["android", "type", "-g"])),
            Ok(Cli::Android {
                group: None,
                cmd: Some(AndroidCmd::Type("-g".into()))
            })
        );
    }

    #[test]
    fn android_usage_errors() {
        let bad = [
            vec!["android", "-g"],
            vec!["android", "-g", ""],
            vec!["android", "swipe", "1", "2"],
            vec!["android", "tap", "1"],
            vec!["android", "tap", "1", "2", "3"],
            vec!["android", "tap", "x", "2"],
            vec!["android", "tap", "-1", "2"],
            vec!["android", "tap", "1", "2", "-g", "web"],
            vec!["android", "resize", "0", "10"],
            vec!["android", "resize", "10"],
            vec!["android", "resize", "3000000000", "10"],
            vec!["android", "type"],
            vec!["android", "type", ""],
            vec!["android", "type", "a\nb"],
            vec!["android", "key", ""],
            vec!["android", "key", "two words"],
            vec!["android", "screenshot"],
            vec!["android", "screenshot", ""],
            vec!["android", "screenshot", "a\nb.png"],
        ];
        for argv in bad {
            assert!(parse(&args(&argv)).is_err(), "{argv:?} should be a usage error");
        }
    }

    #[test]
    fn android_argv_round_trips_through_parse() {
        let cmds = [
            None,
            Some(AndroidCmd::Screenshot(PathBuf::from("/tmp/a b.png"))),
            Some(AndroidCmd::Tap(3, 4)),
            Some(AndroidCmd::Type("-g x".into())),
            Some(AndroidCmd::Key("enter".into())),
            Some(AndroidCmd::Resize(720, 1280)),
        ];
        for cmd in cmds {
            let argv = android_argv("kabelsalat", "aaa-111", cmd.as_ref());
            assert_eq!(parse(&argv), Ok(android("aaa-111", cmd)));
        }
    }

    #[test]
    fn control_lines_follow_the_pane_protocol() {
        assert_eq!(
            control_line(&AndroidCmd::Screenshot("/tmp/a b.png".into())),
            "screenshot /tmp/a b.png"
        );
        assert_eq!(control_line(&AndroidCmd::Tap(10, 20)), "click 10 20");
        assert_eq!(
            control_line(&AndroidCmd::Type("hello world".into())),
            "type hello world"
        );
        assert_eq!(control_line(&AndroidCmd::Key("enter".into())), "key enter");
        assert_eq!(
            control_line(&AndroidCmd::Resize(720, 1280)),
            "resize 720 1280"
        );
    }

    #[test]
    fn control_replies_become_outcomes() {
        let ok = control_reply("ok\n");
        assert_eq!((ok.code, ok.stdout.as_str(), ok.stderr.as_str()), (EXIT_OK, "", ""));
        let data = control_reply("ok /tmp/a.png\n");
        assert_eq!((data.code, data.stdout.as_str()), (EXIT_OK, "/tmp/a.png\n"));
        let err = control_reply("err no frame yet\n");
        assert_eq!(err.code, EXIT_FAILED);
        assert_eq!(err.stderr, "kabelsalat: android: no frame yet\n");
        let garbage = control_reply("what\n");
        assert_eq!(garbage.code, EXIT_FAILED);
        assert!(garbage.stderr.contains("what"), "{}", garbage.stderr);
        assert!(ok.action.is_none() && err.action.is_none());
    }

    #[test]
    fn the_callers_group_variable_fills_android_too() {
        let filled = with_default_group(
            Cli::Android {
                group: None,
                cmd: Some(AndroidCmd::Key("enter".into())),
            },
            Some("aaa-111"),
        );
        assert_eq!(
            filled,
            Ok(android("aaa-111", Some(AndroidCmd::Key("enter".into()))))
        );
        assert!(
            with_default_group(
                Cli::Android {
                    group: None,
                    cmd: None
                },
                None
            )
            .is_err()
        );
        assert!(
            Cli::Android {
                group: None,
                cmd: None
            }
            .needs_instance()
        );
    }

    fn android_groups() -> Vec<GroupInfo> {
        vec![
            GroupInfo {
                uuid: "aaa-111".into(),
                name: "web".into(),
                tabs: 2,
                host: None,
                cdp: None,
                android: Some(AndroidInfo {
                    ctl: PathBuf::from("/tmp/ctl.sock"),
                    adb: Some("192.168.240.112:5555".into()),
                }),
            },
            GroupInfo {
                uuid: "bbb-222".into(),
                name: "api".into(),
                tabs: 1,
                host: None,
                cdp: None,
                android: None,
            },
            GroupInfo {
                uuid: "rrr-555".into(),
                name: "box".into(),
                tabs: 1,
                host: Some("me@box".into()),
                cdp: None,
                android: None,
            },
        ]
    }

    #[test]
    fn android_prints_its_endpoints_when_owned_and_booted() {
        let out = dispatch(&android("web", None), &android_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stdout, "ctl=/tmp/ctl.sock\nadb=192.168.240.112:5555\n");
        assert!(out.action.is_none());
    }

    #[test]
    fn android_still_booting_prints_nothing_and_asks_nothing() {
        let mut groups = android_groups();
        if let Some(info) = groups[0].android.as_mut() {
            info.adb = None;
        }
        let out = dispatch(&android("web", None), &groups, Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stdout, "");
        assert!(out.stderr.contains("booting"), "{}", out.stderr);
        assert!(out.action.is_none());
    }

    #[test]
    fn android_asks_the_gui_when_nobody_owns_it() {
        // browser_groups(): nobody has an Android.
        let out = dispatch(&android("api", None), &browser_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stdout, "");
        assert_eq!(
            out.action,
            Some(Action::OpenAndroid {
                group_uuid: "bbb-222".into()
            })
        );
    }

    #[test]
    fn android_refuses_while_another_group_owns_it() {
        for cmd in [None, Some(AndroidCmd::Tap(1, 2))] {
            let out = dispatch(&android("api", cmd), &android_groups(), Path::new("/w"));
            assert_eq!(out.code, EXIT_GROUP);
            assert!(out.action.is_none());
            assert!(out.stderr.contains("web"), "{}", out.stderr);
        }
    }

    #[test]
    fn android_refuses_a_remote_group() {
        let out = dispatch(&android("box", None), &browser_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.action.is_none());
        assert!(out.stderr.contains("me@box"), "{}", out.stderr);
    }

    #[test]
    fn android_on_an_unknown_group_or_without_one_fails() {
        let out = dispatch(&android("nope", None), &android_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_GROUP);
        let out = dispatch(
            &Cli::Android {
                group: None,
                cmd: None,
            },
            &android_groups(),
            Path::new("/w"),
        );
        assert_eq!(out.code, EXIT_USAGE);
        assert!(out.action.is_none());
    }

    #[test]
    fn a_subcommand_without_an_owned_pane_is_exit_3() {
        let out = dispatch(
            &android("api", Some(AndroidCmd::Tap(1, 2))),
            &browser_groups(),
            Path::new("/w"),
        );
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.action.is_none());
    }

    #[test]
    fn a_subcommand_sends_one_line_to_the_owned_pane() {
        let out = dispatch(
            &android("web", Some(AndroidCmd::Tap(10, 20))),
            &android_groups(),
            Path::new("/w"),
        );
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(
            out.action,
            Some(Action::Control {
                socket: PathBuf::from("/tmp/ctl.sock"),
                line: "click 10 20".into(),
            })
        );
    }

    #[test]
    fn a_relative_screenshot_path_is_the_callers() {
        let out = dispatch(
            &android("web", Some(AndroidCmd::Screenshot("shots/a.png".into()))),
            &android_groups(),
            Path::new("/home/u/proj"),
        );
        assert_eq!(
            out.action,
            Some(Action::Control {
                socket: PathBuf::from("/tmp/ctl.sock"),
                line: "screenshot /home/u/proj/shots/a.png".into(),
            })
        );
    }

    #[test]
    fn help_lists_android() {
        assert!(help_text().contains("kabelsalat android [-g <group>]"));
        assert!(help_text().contains("screenshot PATH"));
    }
```

Append inside `mod tests` in `src/control.rs` (`Path` and `Duration` reach the tests through `use super::*;` once Step 4 imports them at the top of the file):

```rust
    fn socket_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kabelsalat-control-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_control_line_gets_its_reply() {
        use std::io::{BufRead as _, Write as _};
        let dir = socket_dir("reply");
        let socket = dir.join("ctl.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut writer = stream;
            writer.write_all(b"ok\n").unwrap();
            line
        });
        let reply = send_control(&socket, "click 10 20", Duration::from_secs(5)).unwrap();
        assert_eq!(reply, "ok\n");
        assert_eq!(server.join().unwrap(), "click 10 20\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pane_that_hangs_up_without_a_reply_is_an_error() {
        let dir = socket_dir("silent");
        let socket = dir.join("ctl.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(stream);
        });
        assert!(send_control(&socket, "ping", Duration::from_secs(5)).is_err());
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_control_socket_is_an_error() {
        let missing = Path::new("/nonexistent/kabelsalat-test/ctl.sock");
        assert!(send_control(missing, "ping", Duration::from_secs(1)).is_err());
    }

    #[test]
    fn an_open_android_request_without_a_gui_is_refused() {
        assert!(!request_open_android("aaa-111".into()));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cli::tests`
Expected: compile errors — `GroupInfo` has no field `android`; `AndroidCmd`, `AndroidInfo`, `Cli::Android`, `Action::OpenAndroid`, `Action::Control`, `android_argv`, `control_line`, `control_reply`, `send_control`, `request_open_android`, `Path`, `Duration` not found. (The crate's tests compile as one unit, so this one run shows the `control.rs` errors too.)

- [ ] **Step 3: Implement `cli.rs`**

Change the `EXIT_FAILED` doc (`src/cli.rs:17`) to:

```rust
/// `resume` could not reach tmux, start the server, or create a session; or
/// the Android pane answered a control request with an error.
```

`ENV_ANDROID_CTL` (Task 5) is what the hints below name. Add the `AndroidCmd` type after `pub const ANDROID_ENV_KEYS …;` (Task 7):

```rust
/// One request to the Android pane's control socket, as typed on the command
/// line. See `vendor/nested-wayland-session/src/protocol.rs` in klamottenkiste
/// for the wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AndroidCmd {
    /// Write the pane's current frame to this PNG file.
    Screenshot(PathBuf),
    /// Click at pane coordinates (Android sees a mouse, not a finger).
    Tap(u32, u32),
    /// Type this text, one key tap per character.
    Type(String),
    /// Tap one named key (`enter`, `escape`, `a`, …).
    Key(String),
    /// Resize the nested screen; Android follows.
    Resize(u32, u32),
}

impl AndroidCmd {
    /// The argv tail that parses back to this command.
    pub fn args(&self) -> Vec<String> {
        match self {
            Self::Screenshot(path) => {
                vec!["screenshot".into(), path.to_string_lossy().into_owned()]
            }
            Self::Tap(x, y) => vec!["tap".into(), x.to_string(), y.to_string()],
            Self::Type(text) => vec!["type".into(), text.clone()],
            Self::Key(name) => vec!["key".into(), name.clone()],
            Self::Resize(w, h) => vec!["resize".into(), w.to_string(), h.to_string()],
        }
    }
}
```

In `enum Cli`, after the `Browser { … }` variant:

```rust
    /// Bring up Android in a group's pane area and print its endpoints, or
    /// send one request to its pane's control socket.
    Android {
        /// A group uuid or name; `None` until `with_default_group` fills it
        /// from the caller's `KABELSALAT_GROUP`.
        group: Option<String>,
        /// `None`: open it, or print `ctl=`/`adb=`. `Some`: one request.
        cmd: Option<AndroidCmd>,
    },
```

In `needs_instance`, extend the `matches!` to:

```rust
        matches!(
            self,
            Cli::Groups
                | Cli::Run { .. }
                | Cli::Rename { .. }
                | Cli::Browser { .. }
                | Cli::Android { .. }
        )
```

In `with_default_group`, add before `other => Ok(other),`:

```rust
        Cli::Android { group: None, cmd } => match env_group.filter(|g| !g.is_empty()) {
            Some(group) => Ok(Cli::Android {
                group: Some(group.to_string()),
                cmd,
            }),
            None => Err(UsageError(format!(
                "android needs --group outside a kabelsalat tab ({ENV_GROUP} is unset)"
            ))),
        },
```

Replace `GroupInfo` (with its doc comment) with:

```rust
/// One group as the CLI sees it: the stable uuid, the (possibly empty,
/// possibly duplicated) name, how many tabs it holds, for a remote group
/// the ssh destination its tabs run on, the live CDP endpoint of its
/// browser when it has one, and its Android pane when it owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInfo {
    pub uuid: String,
    pub name: String,
    pub tabs: usize,
    pub host: Option<String>,
    pub cdp: Option<String>,
    pub android: Option<AndroidInfo>,
}

/// A group's Android pane as the CLI sees it: its control socket from the
/// moment it exists, its adb serial once Android has booted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidInfo {
    pub ctl: PathBuf,
    pub adb: Option<String>,
}
```

In `help_text`, after the `kabelsalat browser …` entry (three lines), add:

```
  kabelsalat android [-g <group>]              Open Android (Waydroid) in <group>'s pane area, or
                                               print its control socket and adb serial as
                                               ctl=/adb= lines; <group> defaults to the
                                               caller's KABELSALAT_GROUP
  kabelsalat android [-g <group>] screenshot PATH | tap X Y | type TEXT | key NAME | resize W H
                                               Drive <group>'s Android pane
```

and replace the two exit-code lines after `0 success   1 kabelsalat not running   2 usage error` with:

```
  3 group not found, ambiguous, remote (browser, android), the new name is already
    taken, Android owned by another group or not open, or its pane did not answer
  4 resume could not reach tmux, or the Android pane refused a request
```

In `parse`, after `"browser" => parse_browser(&rest[1..]),`:

```rust
        "android" => parse_android(&rest[1..]),
```

After `parse_browser`, add:

```rust
/// `android [-g <group>] [SUBCOMMAND ARGS...]`. The group flag may only come
/// before the subcommand, so text after `type` is never read as a flag.
fn parse_android(args: &[String]) -> Result<Cli, UsageError> {
    let mut group: Option<String> = None;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if arg != "--group" && arg != "-g" {
            break;
        }
        let value = args
            .get(i + 1)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| UsageError(format!("{arg} needs a value")))?;
        group = Some(value.clone());
        i += 2;
    }
    let cmd = parse_android_cmd(&args[i..])?;
    Ok(Cli::Android { group, cmd })
}

/// The subcommand of `android`, if any.
fn parse_android_cmd(args: &[String]) -> Result<Option<AndroidCmd>, UsageError> {
    let Some((verb, rest)) = args.split_first() else {
        return Ok(None);
    };
    let cmd = match (verb.as_str(), rest) {
        ("screenshot", [path]) => {
            if path.is_empty() {
                return Err(UsageError("screenshot needs a file path".into()));
            }
            check_one_line("screenshot path", path)?;
            AndroidCmd::Screenshot(PathBuf::from(path))
        }
        ("tap", [x, y]) => AndroidCmd::Tap(coordinate("tap x", x)?, coordinate("tap y", y)?),
        ("type", [text]) => {
            if text.is_empty() {
                return Err(UsageError("type needs some text".into()));
            }
            check_one_line("text to type", text)?;
            AndroidCmd::Type(text.clone())
        }
        ("key", [name]) => {
            if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(UsageError(format!(
                    "'{name}' is not a key name (one word, e.g. enter)"
                )));
            }
            AndroidCmd::Key(name.clone())
        }
        ("resize", [w, h]) => AndroidCmd::Resize(
            dimension("resize width", w)?,
            dimension("resize height", h)?,
        ),
        ("screenshot" | "type" | "key", _) => {
            return Err(UsageError(format!("{verb} takes exactly one argument")));
        }
        ("tap" | "resize", _) => {
            return Err(UsageError(format!("{verb} takes exactly two numbers")));
        }
        (other, _) => return Err(UsageError(format!("unknown android command '{other}'"))),
    };
    Ok(Some(cmd))
}

/// The control protocol is one request per line, so a value that would end
/// the line early is a usage error.
fn check_one_line(what: &str, value: &str) -> Result<(), UsageError> {
    if value.contains(['\n', '\r']) {
        return Err(UsageError(format!("the {what} must be a single line")));
    }
    Ok(())
}

fn coordinate(what: &str, value: &str) -> Result<u32, UsageError> {
    value
        .parse()
        .map_err(|_| UsageError(format!("{what} must be a whole number of pixels (got '{value}')")))
}

/// A screen dimension: the protocol takes a positive `i32`.
fn dimension(what: &str, value: &str) -> Result<u32, UsageError> {
    let n = coordinate(what, value)?;
    if n == 0 || n > i32::MAX as u32 {
        return Err(UsageError(format!(
            "{what} must be between 1 and {}",
            i32::MAX
        )));
    }
    Ok(n)
}

/// The argv a caller forwards to the running GUI once its group is known:
/// `android -g <group>` plus the subcommand's own arguments.
pub fn android_argv(argv0: &str, group: &str, cmd: Option<&AndroidCmd>) -> Vec<String> {
    let mut argv = vec![
        argv0.to_string(),
        "android".to_string(),
        "-g".to_string(),
        group.to_string(),
    ];
    if let Some(cmd) = cmd {
        argv.extend(cmd.args());
    }
    argv
}

/// The control-socket request line for `cmd`, without the newline. Pure.
pub fn control_line(cmd: &AndroidCmd) -> String {
    match cmd {
        AndroidCmd::Screenshot(path) => format!("screenshot {}", path.display()),
        AndroidCmd::Tap(x, y) => format!("click {x} {y}"),
        AndroidCmd::Type(text) => format!("type {text}"),
        AndroidCmd::Key(name) => format!("key {name}"),
        AndroidCmd::Resize(w, h) => format!("resize {w} {h}"),
    }
}

/// What a reply line from the pane prints and exits with: `ok` → nothing,
/// `ok <data>` → the data, `err <message>` → the message and exit 4. Pure.
pub fn control_reply(reply: &str) -> Outcome {
    let line = reply.trim_end_matches(['\r', '\n']);
    if line == "ok" {
        return Outcome::ok(String::new());
    }
    if let Some(data) = line.strip_prefix("ok ") {
        return Outcome::ok(format!("{data}\n"));
    }
    if let Some(message) = line.strip_prefix("err ") {
        return Outcome::fail(EXIT_FAILED, format!("kabelsalat: android: {message}\n"));
    }
    Outcome::fail(
        EXIT_FAILED,
        format!("kabelsalat: android: unexpected reply from the pane: '{line}'\n"),
    )
}
```

In `enum Action`, after `OpenBrowser { … }`:

```rust
    /// Bring up Android in this group. Inert like `OpenBrowser`: no focus,
    /// no raise, no active-group change, no front-pane change.
    OpenAndroid {
        group_uuid: String,
    },
    /// Send `line` to the Android pane's control socket and print the reply.
    Control {
        socket: PathBuf,
        line: String,
    },
```

In `dispatch`, after the `Cli::Browser { group: Some(group) } => { … }` arm:

```rust
        // control.rs fills the default before dispatching, as for browser.
        Cli::Android { group: None, .. } => Outcome::fail(
            EXIT_USAGE,
            format!("android needs --group outside a kabelsalat tab ({ENV_GROUP} is unset)\n"),
        ),
        Cli::Android {
            group: Some(group),
            cmd,
        } => {
            let target = match resolve_group(groups, group) {
                Ok(found) => found,
                Err(err) => return resolve_failure(group, err),
            };
            if let Some(host) = &target.host {
                return Outcome::fail(
                    EXIT_GROUP,
                    format!("the Android pane is local, but '{group}' runs on {host}\n"),
                );
            }
            // One Waydroid per machine, so at most one owner.
            let owner = groups.iter().find(|g| g.android.is_some());
            if let Some(other) = owner.filter(|o| o.uuid != target.uuid) {
                return Outcome::fail(
                    EXIT_GROUP,
                    format!(
                        "Android is open in group '{}'; there is one per machine, \
                         so stop it there first\n",
                        display_name(other)
                    ),
                );
            }
            match (owner.and_then(|o| o.android.as_ref()), cmd) {
                (Some(info), None) => match &info.adb {
                    Some(adb) => Outcome::ok(format!("ctl={}\nadb={adb}\n", info.ctl.display())),
                    None => Outcome {
                        stdout: String::new(),
                        stderr: format!(
                            "kabelsalat: Android in group '{}' is still booting; read \
                             {ENV_ANDROID_CTL} from `tmux show-environment` once it is there\n",
                            display_name(target)
                        ),
                        code: EXIT_OK,
                        action: None,
                    },
                },
                (Some(info), Some(cmd)) => Outcome {
                    stdout: String::new(),
                    stderr: String::new(),
                    code: EXIT_OK,
                    action: Some(Action::Control {
                        socket: info.ctl.clone(),
                        line: control_line(&with_absolute_path(cmd, caller_cwd)),
                    }),
                },
                (None, None) => Outcome {
                    stdout: String::new(),
                    stderr: format!(
                        "kabelsalat: bringing up Android in group '{}' (about 20 s); read \
                         {ENV_ANDROID_CTL} from `tmux show-environment` once it is there\n",
                        display_name(target)
                    ),
                    code: EXIT_OK,
                    action: Some(Action::OpenAndroid {
                        group_uuid: target.uuid.clone(),
                    }),
                },
                (None, Some(_)) => Outcome::fail(
                    EXIT_GROUP,
                    format!("group '{group}' has no Android pane; run `kabelsalat android` first\n"),
                ),
            }
        }
```

After `resolve_failure`, add:

```rust
/// A group's name for messages, or its uuid when it has none.
fn display_name(group: &GroupInfo) -> &str {
    if group.name.is_empty() {
        &group.uuid
    } else {
        &group.name
    }
}

/// `cmd` with a relative screenshot path resolved against the caller's
/// directory: the compositor writes the file from inside the GUI process,
/// whose directory is not the caller's.
fn with_absolute_path(cmd: &AndroidCmd, caller_cwd: &Path) -> AndroidCmd {
    match cmd {
        AndroidCmd::Screenshot(path) => AndroidCmd::Screenshot(caller_cwd.join(path)),
        other => other.clone(),
    }
}
```

- [ ] **Step 4: Implement `control.rs`**

Add to the imports at the top of `src/control.rs`:

```rust
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;
```

and after the existing `use` lines:

```rust
/// How long one control-socket round trip may stall the GUI's main thread:
/// a `screenshot` renders a frame and writes a PNG.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
```

After `request_open_browser`, add:

```rust
/// Ask the component to bring up Android in a group. Returns false when
/// there is no component to ask, or when it has already shut down.
pub fn request_open_android(group_uuid: String) -> bool {
    let Some(control) = CONTROL.get() else {
        return false;
    };
    control.sender.send(Msg::OpenAndroid { group_uuid }).is_ok()
}

/// One request to a pane's control socket: write `line` and a newline, shut
/// the write half (the server serves a connection until EOF), read one reply
/// line. Bounded by `timeout` per read and write. A pane that closes without
/// a reply is an error.
pub fn send_control(socket: &Path, line: &str, timeout: Duration) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    if reply.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the pane closed the connection without a reply",
        ));
    }
    Ok(reply)
}
```

In `handle_command_line`, add two arms to `match outcome.action` before `None => {}`:

```rust
        // As for the browser: the endpoints arrive in the group's tmux
        // sessions once Android has booted, so nothing is printed.
        Some(Action::OpenAndroid { group_uuid }) => {
            if !request_open_android(group_uuid) {
                command_line.printerr_literal("kabelsalat: no window to open Android in\n");
                return glib::ExitCode::new(cli::EXIT_NOT_RUNNING);
            }
        }
        // One blocking round trip on this (the GTK) thread. Safe: the socket
        // is served by the compositor's own threads, never by this one, and
        // CONTROL_TIMEOUT bounds the stall.
        Some(Action::Control { socket, line }) => {
            let reply = match send_control(&socket, &line, CONTROL_TIMEOUT) {
                Ok(reply) => reply,
                Err(err) => {
                    command_line.printerr_literal(&format!(
                        "kabelsalat: the Android pane did not answer on {}: {err}\n",
                        socket.display()
                    ));
                    return glib::ExitCode::new(cli::EXIT_GROUP);
                }
            };
            let answer = cli::control_reply(&reply);
            if !answer.stdout.is_empty() {
                command_line.print_literal(&answer.stdout);
            }
            if !answer.stderr.is_empty() {
                command_line.printerr_literal(&answer.stderr);
            }
            return glib::ExitCode::new(answer.code);
        }
```

- [ ] **Step 5: GUI hook and snapshot (`app.rs`)**

In `enum Msg`, after the `OpenBrowser { group_uuid: String },` variant:

```rust
    /// A `kabelsalat android` invocation: bring up Android in the group if
    /// nobody has it. Like `OpenBrowser` it takes no focus, raises no window,
    /// changes no active group and no front pane; the area only appears for
    /// the active group's first pane.
    OpenAndroid {
        group_uuid: String,
    },
```

In `update()`, after `Msg::OpenBrowser { group_uuid } => self.open_browser(&group_uuid),`:

```rust
            Msg::OpenAndroid { group_uuid } => self.open_android(&group_uuid),
```

After `toggle_android` (Task 7), add:

```rust
    /// `kabelsalat android`: bring Android up quietly — never focus, never
    /// switch groups, never change the front pane when another pane is
    /// open; show the area only for the active group's first pane. Refusals
    /// (owned elsewhere since the CLI's snapshot, Waydroid missing) are
    /// reported like `open_browser`'s.
    fn open_android(&mut self, group_uuid: &str) {
        let Some((id, other_open, has_android)) = self
            .groups
            .iter()
            .find(|g| g.uuid == group_uuid)
            .map(|g| (g.id, g.has_panes(), g.android.is_some()))
        else {
            eprintln!("android request for unknown group {group_uuid}");
            return;
        };
        if has_android {
            return;
        }
        let active = self.active_group() == Some(id);
        if let Err(message) = self.spawn_android(id) {
            eprintln!("kabelsalat: opening Android in group {id}: {message}");
            self.show_notice(&message);
            return;
        }
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == id) {
            group.front_pane =
                state::front_after_open(group.front_pane, PaneKind::Android, other_open, true);
            group.panes_visible = state::quiet_open_visible(group.panes_visible, active, other_open);
        }
        self.sync_pane_host();
    }
```

In `publish_groups`, the `GroupInfo { … }` literal gains after `cdp: …,`:

```rust
                    android: g.android.as_ref().map(|a| crate::cli::AndroidInfo {
                        ctl: a.control_socket_path().to_path_buf(),
                        adb: a.adb().map(str::to_string),
                    }),
```

and its doc comment becomes "Publish the groups for the CLI: on every save, and on `CdpReady`/`AndroidReady`, which change nothing the state file records but are exactly what `kabelsalat browser`/`kabelsalat android` ask about."

- [ ] **Step 6: Forward `android` from the caller (`lib.rs`)**

In `src/lib.rs`, change the comment above `with_default_group` to start "`browser` and `android` without --group take the caller's KABELSALAT_GROUP." and replace the `let args = match &parsed { … };` block with:

```rust
    let args = match &parsed {
        cli::Cli::Browser { group: Some(group) } => {
            vec![
                args[0].clone(),
                "browser".into(),
                "-g".into(),
                group.clone(),
            ]
        }
        cli::Cli::Android {
            group: Some(group),
            cmd,
        } => cli::android_argv(&args[0], group, cmd.as_ref()),
        _ => args,
    };
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib cli::tests && cargo test --lib control::tests`
Expected: all pass (18 new in `cli`, 4 new in `control`).

- [ ] **Step 8: Format, lint, full test**

Run: `cargo fmt && cargo build && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean; all pass.

- [ ] **Step 9: Commit**

```bash
git add src/cli.rs src/control.rs src/lib.rs src/app.rs
git commit -m "Add kabelsalat android: open, print endpoints, drive the pane"
```

- [ ] **Step 10: Manual check (GUI + Waydroid; quit the running kabelsalat first)**

From a tab of group A, with nothing running:
1. `kabelsalat android; echo $?` → nothing on stdout, a hint on stderr, `0`; Android comes up in A without focus change (hidden if A is not the active group). Within ~20 s `tmux show-environment KABELSALAT_ANDROID_CTL` is set.
2. `kabelsalat android` again → `ctl=/tmp/kabelsalat-spike-control-….sock` and `adb=192.168.240.112:5555`, exit 0.
3. `kabelsalat android screenshot shot.png` → exit 0, `./shot.png` is a PNG of the Android screen.
4. `kabelsalat android tap 100 100`, `type hello`, `key enter`, `resize 720 1280` → exit 0 each, visible in the pane.
5. From a tab of group B: `kabelsalat android; echo $?` → `3`, message names A. `kabelsalat android tap 1 1` → `3`.
6. `kabelsalat android tap 1` → `2` with usage. From a remote group's tab → `3`.
7. After "Stop Android": `kabelsalat android tap 1 1` in A → `3` ("has no Android pane").

---

### Task 10: Skill — the "Android" section

**Files:**
- Modify: `skills/kabelsalat/SKILL.md` — frontmatter `description` (line 3), exit-code table rows (lines 83-84), new section appended at the end of the file (after line 177)

**Interfaces:**
- Consumes: the CLI and environment from Tasks 7–9: `kabelsalat android [-g G] [screenshot PATH | tap X Y | type TEXT | key NAME | resize W H]`, `KABELSALAT_ANDROID_CTL`, `KABELSALAT_ANDROID_ADB`, exit codes 0–4.
- Produces: agent-facing documentation, in plain Markdown and standard tools (shell, Python, `adb`, `curl`) so it serves any agent, not only Claude.

Plugin version: `.claude-plugin/plugin.json`'s history shows the version moving only in standalone release commits ("Bump version to 0.7.0", together with `Cargo.toml`/`Cargo.lock`), never with a skill change (e.g. `cd81a6b` changed the skill without a bump). So this plan does not bump it; the next release does.

The download snippets send no custom headers — curl's default User-Agent, never the user's email (CLAUDE.md).

- [ ] **Step 1: Frontmatter and exit codes**

Replace line 3 of `skills/kabelsalat/SKILL.md` with:

```markdown
description: Use when a command should run in a visible, persistent terminal the user can watch and interact with — a dev server, a long build, or an interactive claude session — rather than as a captured subprocess; not for commands whose output you need to capture or read back. Also use to read, screenshot or drive the page in the user's embedded browser pane (CDP/Playwright), or Firefox for Android in the Android pane (Waydroid, WebDriver BiDi over adb). All of these live in the named groups of the user's running kabelsalat terminal.
```

Replace the table row for code 3 (line 84) with these two rows:

```markdown
| 3 | Group not found, ambiguous, (rename) name already in use, (browser, android) a remote group, (android) Android owned by another group or not open, or its pane did not answer | Re-run `kabelsalat groups` and retry with a uuid, or pick a different name; for Android, tell the user which group has it |
| 4 | (android subcommand) the pane refused the request | Read the message; fix the arguments |
```

- [ ] **Step 2: Append the section**

Append to the end of `skills/kabelsalat/SKILL.md`:

```markdown

## Driving Firefox for Android (Waydroid)

A group can own the machine's one Android: Waydroid running in a pane next
to the group's browser, shown as an "Android" tab. You drive Firefox for
Android (Fenix) in it with geckodriver and WebDriver BiDi over adb.
kabelsalat only starts Android; adb authorisation, Fenix and geckodriver are
yours to set up, as below. The co-browsing rule is the browser's: the user
watches the pane; act when asked.

### 1. Bring Android up

Pin `KABELSALAT_GROUP` as for the browser, then:

    tmux show-environment KABELSALAT_ANDROID_CTL   # the pane's control socket
    tmux show-environment KABELSALAT_ANDROID_ADB   # adb serial, e.g. 192.168.240.112:5555

A leading `-`, or "unknown variable", means no booted Android in your group.
Ask for one:

    kabelsalat android               # the group is taken from your KABELSALAT_GROUP
    kabelsalat android -g <name|uuid>

If Android is already up, it prints `ctl=<socket>` and `adb=<serial>` and you
are done. Otherwise stdout stays empty and Android boots — hidden unless the
user is looking at that group, without taking their focus. Poll
`tmux show-environment KABELSALAT_ANDROID_CTL` every two seconds for up to
90 seconds. Exit 3 means another group owns Android (the message names it;
there is one per machine — tell the user, do not stop it), a remote group,
or an unknown group. Exit 1: kabelsalat's window is not running; stop.

If the variables never appear, the user got a notice in the window
(Waydroid missing, not initialised, or a session already running elsewhere).
Ask them what it said.

### 2. Authorise adb

    adb connect "$KABELSALAT_ANDROID_ADB"
    adb -s "$KABELSALAT_ANDROID_ADB" get-state      # "device" once authorised

The first connection comes up `unauthorized`, and Android shows an "Allow USB
debugging?" dialog in the pane. adb cannot answer it, so use the pane:

    kabelsalat android screenshot /tmp/android.png   # look at the dialog
    kabelsalat android tap X Y                        # tick "Always allow", then tap "Allow"

Coordinates are pixels of that screenshot. Then
`adb disconnect "$KABELSALAT_ANDROID_ADB"`, connect again, and re-check
`get-state`. "Always allow" makes this a one-time step.

### 3. Install Fenix (once)

    adb -s "$KABELSALAT_ANDROID_ADB" shell pm path org.mozilla.firefox

prints `package:…` when Fenix is installed. If it is not, download the
official x86_64 APK from Mozilla's archive into a fresh, empty directory and
install it (`adb install` checks the APK's signature):

    V=$(curl -s https://archive.mozilla.org/pub/fenix/releases/ \
        | grep -o 'releases/[0-9][0-9.]*/"' | sed 's#releases/##; s#/"##' | sort -V | tail -1)
    D=$(mktemp -d)
    curl -fL -o "$D/fenix.apk" \
      "https://archive.mozilla.org/pub/fenix/releases/$V/android/fenix-$V-android-x86_64/fenix-$V.multi.android-x86_64.apk"
    adb -s "$KABELSALAT_ANDROID_ADB" install "$D/fenix.apk"

### 4. Start geckodriver

Use `geckodriver` from `PATH` if there is one. Otherwise download the linux64
release into a fresh, empty directory and check its sha256 against the
digest GitHub publishes for the asset before running anything from it:

    TAG=$(curl -s https://api.github.com/repos/mozilla/geckodriver/releases/latest \
          | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')
    ASSET="geckodriver-$TAG-linux64.tar.gz"
    WANT=$(curl -s "https://api.github.com/repos/mozilla/geckodriver/releases/tags/$TAG" \
          | python3 -c "import json,sys; print(next(a['digest'] for a in json.load(sys.stdin)['assets'] if a['name'] == '$ASSET').removeprefix('sha256:'))")
    D=$(mktemp -d)
    curl -fL -o "$D/$ASSET" "https://github.com/mozilla/geckodriver/releases/download/$TAG/$ASSET"
    echo "$WANT  $D/$ASSET" | sha256sum -c - && tar -xzf "$D/$ASSET" -C "$D"

If the check fails, stop and tell the user. Run geckodriver on a free port —
never assume 4444, it is often taken — as a background process of yours, not
in a kabelsalat tab (you need its port, not its output):

    PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
    "$D/geckodriver" --android-storage internal --port "$PORT" &

`--android-storage internal` is required on Waydroid: its `/storage/emulated`
is a bind mount geckodriver cannot create its directories on. The flag is
deprecated upstream and still works.

### 5. A BiDi session from Python

A raw websocket client keeps this to one small dependency
(`pip install websockets`). Same preamble as for the browser — re-resolve
the environment and assert the group you pinned:

    import base64, json, subprocess, urllib.request
    from websockets.sync.client import connect

    PINNED_GROUP = "<uuid from your first fetch>"
    GECKO = "http://127.0.0.1:<PORT geckodriver listens on>"

    def tmux_env():
        out = subprocess.run(["tmux", "show-environment"],
                             capture_output=True, text=True).stdout
        return dict(line.split("=", 1) for line in out.splitlines()
                    if "=" in line and not line.startswith("-"))

    env = tmux_env()
    assert env.get("KABELSALAT_GROUP") == PINNED_GROUP, \
        f"tab moved (now in {env.get('KABELSALAT_GROUP', 'nowhere')}) — re-orient"
    serial = env["KABELSALAT_ANDROID_ADB"]     # KeyError = no Android: also loud

    caps = {"capabilities": {"alwaysMatch": {
        "browserName": "firefox",
        "webSocketUrl": True,
        "moz:firefoxOptions": {
            "androidPackage": "org.mozilla.firefox",
            "androidDeviceSerial": serial,
        },
    }}}
    request = urllib.request.Request(
        f"{GECKO}/session", data=json.dumps(caps).encode(),
        headers={"Content-Type": "application/json"})
    # Starting Fenix on the device takes a while.
    session = json.load(urllib.request.urlopen(request, timeout=180))["value"]
    session_id = session["sessionId"]
    ws_url = session["capabilities"]["webSocketUrl"]

    next_id = 0

    def bidi(ws, method, **params):
        global next_id
        next_id += 1
        ws.send(json.dumps({"id": next_id, "method": method, "params": params}))
        while True:
            msg = json.loads(ws.recv(timeout=120))
            if msg.get("id") != next_id:
                continue                       # an event, not our answer
            if msg.get("type") == "error":
                raise RuntimeError(f"{method}: {msg['error']}: {msg.get('message')}")
            return msg["result"]

    try:
        with connect(ws_url, max_size=None) as ws:
            tree = bidi(ws, "browsingContext.getTree")
            context = tree["contexts"][0]["context"]
            bidi(ws, "browsingContext.navigate", context=context,
                 url="https://example.org", wait="complete")
            title = bidi(ws, "script.evaluate", expression="document.title",
                         target={"context": context}, awaitPromise=False)
            print(title["result"]["value"])
            shot = bidi(ws, "browsingContext.captureScreenshot", context=context)
            with open("/tmp/fenix.png", "wb") as f:
                f.write(base64.b64decode(shot["data"]))
    finally:
        urllib.request.urlopen(urllib.request.Request(
            f"{GECKO}/session/{session_id}", method="DELETE"), timeout=60)

Keep one session per task and end it (the `DELETE`): every new session wipes
Fenix, see below. A refused connection to `GECKO` means geckodriver is gone;
start it again.

### 6. Known behaviours

- **Every session wipes Fenix.** geckodriver runs `pm clear` on it at each
  new session: no logins, history or settings survive, and the onboarding
  screens come back every time.
- **Onboarding blocks screenshots.** While Fenix's first-run overlay covers
  the tab, `browsingContext.captureScreenshot` fails with "width: 0 and
  height: 0". Dismiss it first: `kabelsalat android screenshot`, find the
  button, tap it (with adb, step 7).
- **`session.status` is not a health check.** It reports `ready: false`
  whenever a session is open.
- **Tablet layout by default.** At the pane's size Fenix uses its tablet
  layout and requests desktop sites. For a phone: `adb -s
  "$KABELSALAT_ANDROID_ADB" shell wm density 420` (undo with
  `wm density reset`), or a narrower screen with
  `kabelsalat android resize 540 1080`.
- **One Android per machine**, owned by one group; it is not moved between
  groups.

### 7. Input

Prefer adb for anything inside Android:

    adb -s "$KABELSALAT_ANDROID_ADB" shell input tap X Y
    adb -s "$KABELSALAT_ANDROID_ADB" shell input text 'hello%sworld'   # %s is a space
    adb -s "$KABELSALAT_ANDROID_ADB" shell input keyevent KEYCODE_BACK

Use `kabelsalat android tap|type|key` only while adb is not authorised yet
(step 2). The pane has no touch device — Android sees a mouse — and the
first click after the pointer enters the pane can register as a swipe from
the top edge, opening the notification shade: press `kabelsalat android key
escape` and tap again. For text, use `kabelsalat android type '…'`, never a
multi-character `key`: key names go through a US keymap.
```

- [ ] **Step 3: Check the example and the wording**

Extract the Python example and compile it (syntax only; it needs a live geckodriver to run):

```bash
python3 - <<'EOF'
import re, pathlib, py_compile, tempfile
text = pathlib.Path("skills/kabelsalat/SKILL.md").read_text()
section = text.split("### 5. A BiDi session from Python", 1)[1].split("Keep one session per task", 1)[0]
code = "\n".join(line[4:] for line in section.splitlines() if line.startswith("    ") or not line.strip())
path = pathlib.Path(tempfile.mkdtemp()) / "bidi_example.py"
path.write_text(code)
py_compile.compile(str(path), doraise=True)
print("ok", path)
EOF
grep -c 'KABELSALAT_ANDROID_CTL' skills/kabelsalat/SKILL.md   # expect at least 2
grep -n 'android-storage internal' skills/kabelsalat/SKILL.md
```

Expected: `ok …/bidi_example.py`; the two greps find the variable and the flag.

- [ ] **Step 4: Commit**

```bash
git add skills/kabelsalat/SKILL.md
git commit -m "Teach the skill to drive Firefox for Android in the Android pane"
```

---

### Task 11: Full verification and acceptance

**Files:** none modified (fixes found here go back to the task that owns them, with a failing test first where the fix is in a pure module).

- [ ] **Step 1: The CI gate, locally**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean; all tests pass. Where GTK/VTE headers are missing locally, push the branch and check the CI run (`.github/workflows/ci.yml`) instead.

- [ ] **Step 2: Degradation**

Quit the running kabelsalat (sessions survive), then:
1. `PATH=/nonexistent ./target/debug/kabelsalat`: the app starts on plain shells (no tmux on that PATH); Alt+3 shows the "Waydroid is not installed" notice; Alt+2 shows the "No Chromium binary found" notice; nothing panics, and tabs keep working.
2. Normal start with an uninitialised Waydroid (only if available, e.g. another machine): Alt+3 shows the `waydroid init` notice.

- [ ] **Step 3: The spike's acceptance run, through the GUI and the skill**

With an initialised Waydroid and the container service running:
1. Start kabelsalat, open a tab in group A, start `claude` there, and ask it to "open Firefox on Android and tell me the title of https://example.org".
2. The agent follows the skill: `kabelsalat android`, polls the env, `adb connect`, handles the authorisation tap via `kabelsalat android screenshot`/`tap`, installs Fenix if missing, starts geckodriver with `--android-storage internal` on a free port, opens a BiDi session, navigates, evaluates `document.title` ("Example Domain"), and saves a `captureScreenshot`.
3. Throughout: no focus steal, no window raise, no group switch; the Android tab is hidden unless A was the active group.
4. Quit kabelsalat while Android runs, start it again: A's Android comes back hidden and boots; `KABELSALAT_ANDROID_*` reappear in A's tabs.
5. "Stop Android": env unset in A's tabs, `waydroid status` STOPPED, `state.json` without `android_owner` after the next save.

- [ ] **Step 4: Report**

Summarise in the PR/merge description which manual checks ran (and on which Waydroid/Android version), and anything that behaved differently from this plan.

---

## Self-review

Spec coverage, section by section:

- **Goal / Decisions** — geckodriver-managed Fenix, CLI over the control socket plus adb, single owner, ≤1 Browser + ≤1 Android per group as tabs, kabelsalat starts compositor + session only: Tasks 2, 6, 7, 9, 10.
- **§1 `src/android.rs`** — `parse_status`, `check`/`Precondition`, arg builders, `adb_serial` (Task 1); `Android::spawn` with the four steps (precondition → pane → `session start` in its own process group with `<state_dir>/android/session.log` → status poll every 500 ms, then `show-full-ui` in the same group, 60 s budget), `teardown` order, `control_socket_path`/`capture_frame`/`has_exited` (Task 2); `set_visible` is deliberately absent, see deviation 4.
- **§2 `app.rs` wiring** — TabBar over TabView in `browser_paned`'s end slot, attached only with ≥1 pane, closable "Browser"/"Android" tabs (Task 6); `browser`/`android`/`panes_visible`/`front_pane` and `PaneKind` (Tasks 4, 6, 7); Alt+2/Alt+Shift+2 unchanged bindings, Alt+3/Alt+Shift+3, menu entries (Tasks 6, 7); `ToggleAndroid`, `StopAndroid`, `AndroidReady`, `AndroidDied` (Task 7), `OpenAndroid` (Task 9), `PaneSelected` (Task 6), poll folded into the existing timer (Task 7); front-pane-only pumping (Task 6, via unmapping); env publish/unset (Tasks 5, 7); CLI panes never raise/focus/switch/change front (Tasks 4, 9); restart via `android_owner` (Tasks 3, 8); screenshot of the front pane (Task 7).
- **§3 CLI** — both command forms, all dispatch outcomes, `Action::Control` + socket I/O in `control.rs`, exit codes, protocol wire format, `KABELSALAT_GROUP` default, snapshot gains owner/socket/serial (Task 9).
- **§4 Skill** — steps 1–7 including the Python example, adb-auth tap flow, Fenix download/install, geckodriver with `--android-storage internal` on a free port and sha256 check, gotchas, input rule (Task 10).
- **§5 Testing** — `android.rs` (Tasks 1, 2), `cli.rs` parse/usage/dispatch/control line (Tasks 5, 9), `state.rs` round trip and restore plan (Task 3), `tmuxctl.rs` → replaced by `session_env_pairs` tests (Task 5, see below), `app.rs` wiring only (Tasks 6–9), acceptance (Task 11).

Where this plan departs from the spec's letter, and why (each is also stated in its task):

1. `session_start_args(display)` / `show_full_ui_args(display)`: the display is an environment variable, not an argument, so the builders take none and `client_env(display)` carries it (Task 1).
2. `check(…, our_display)` runs before the pane exists, so it is called with `""`, which makes every running session foreign (Task 1/2).
3. `android_owner: Option<Uuid>` is `Option<String>`: group uuids are strings here (Task 3).
4. Non-front panes pause by being unmapped (non-selected `TabView` page or detached host), not by `set_visible(false)`, which would make the `TabView`'s internal stack switch pages (Task 6).
5. No new `tmuxctl.rs` tests: its set/unset builders are key-agnostic and already tested, so new-key tests would pass on first run; the new behaviour is tested in `cli::session_env_pairs` (Task 5).
6. Boot readiness waits beyond "status says Running on our display" for `show-full-ui` to return and `sys.boot_completed=1`, inside the same 60 s (Task 2).
7. `Action::Control { line }` also carries the socket path (Task 9).
8. Alt+2 with two panes brings the browser to the front before it hides the area (Task 4).
9. Restore uses its own `Msg::RestoreAndroid` so it stays hidden even in the active group (Task 8).
10. A quiet open of a group's only pane puts it in front (Task 4).
11. `android` with no subcommand, owned but still booting: exit 0, stderr note, no action; an unowned `android` prints a stderr hint (stdout empty) like `browser`; an `err` reply exits 4 (Task 9).
12. The menu entries live in the primary menu (Task 7).
13. No plugin version bump (Task 10).

Mechanical check: Tasks 1–10 were applied verbatim from this document to a scratch worktree of `de787c8`; after each of Tasks 5–9, `cargo build`, `cargo clippy --all-targets -- -D warnings` and `cargo test` were clean (435 lib tests at the end), `cargo fmt --check` passed after `cargo fmt`, and Task 10's example compiled with `py_compile`. That check found and fixed one bug in this plan (Task 9's `sed` also matched Task 5's `cdp: Option<&str>` parameter). The manual GUI and Waydroid checks (Tasks 6–9, 11) were not run.

Placeholder scan: every code step shows its code; the one deliberately interim value (`android_owner: None` in `save_state`, Task 3) is true at that point — no group can own an Android before Task 7 — and Task 8 replaces it. Types used across tasks are defined in the task listed under their "Produces".
