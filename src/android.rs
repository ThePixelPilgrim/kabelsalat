//! The Android pane: a klamottenkiste `WaylandPane` hosting a Waydroid
//! session, owned as one unit. Mirrors `browser.rs`.
//!
//! The pure half — status parsing, preconditions, argument builders, the adb
//! serial, the ownership claim — is what the tests cover. The process half
//! spawns `waydroid` and never panics: every failure is a message for the
//! user, the way the app degrades without tmux.

use std::fmt;
use std::fs::File;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use klamottenkiste::WaylandPane;
use relm4::gtk::prelude::WidgetExt;

use crate::browser::{self, CaptureError, CapturedFrame};

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

/// Whether teardown may run the global `waydroid session stop`: only while
/// our `session start` child still runs and the boot watcher never saw a
/// session on someone else's display. Otherwise the running session (if
/// any) is not ours to stop. Pure.
pub fn should_stop_session(child_alive: bool, saw_foreign: bool) -> bool {
    child_alive && !saw_foreign
}

/// Whether a live Android pane must be torn down: its `waydroid session
/// start` child exited, or its compositor stopped (then the session renders
/// nowhere and the published socket is dead). Pure.
pub fn is_dead(child_exited: bool, compositor_running: bool) -> bool {
    child_exited || !compositor_running
}

/// The timeout for one command of the boot watcher: its own cap, but never
/// more than what is left of the boot budget. Pure.
pub fn command_budget(remaining: Duration, per_command: Duration) -> Duration {
    remaining.min(per_command)
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
    /// `waydroid` was found, but `waydroid status` failed or timed out.
    StatusUnanswered,
}

impl fmt::Display for AndroidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Precondition(precondition) => write!(f, "{precondition}"),
            Self::Compositor(msg) => write!(f, "The Android compositor could not start: {msg}"),
            Self::Log(err) => write!(f, "The Android session log could not be created: {err}"),
            Self::Spawn(err) => write!(f, "Waydroid could not be started: {err}"),
            Self::StatusUnanswered => write!(
                f,
                "Waydroid did not answer `waydroid status`. Check that its container \
                 service runs, then try again."
            ),
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
    /// Set at teardown; the boot thread stops and skips its report.
    cancel: Arc<AtomicBool>,
    /// Set by the boot thread when it saw a session on another display:
    /// then the running session is not ours and teardown must not stop it.
    saw_foreign: Arc<AtomicBool>,
    torn_down: bool,
}

impl Android {
    /// Check Waydroid, create a pane, and start a session on its display.
    ///
    /// The `waydroid status` check runs here, on the caller's (GTK) thread,
    /// bounded by a 3 s timeout. Booting is watched on a worker thread that
    /// calls `on_boot(boot_id, result)` at most once; it skips the call when
    /// it notices the pane was torn down, but a report already under way can
    /// still arrive afterwards, so the caller must compare the boot id with
    /// the current pane's before acting on it. On any error the
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
        let Some(output) = run_capture(Command::new(&binary).args(status_args()), PRECHECK_TIMEOUT)
        else {
            return Err(AndroidError::StatusUnanswered);
        };
        match check(Some(parse_status(&output)), "") {
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
        // The compositor binds the control socket under the system temp dir
        // with a predictable name and umask permissions; whoever can connect
        // can click and type into Android. Owner only.
        if let Err(err) = std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o600))
        {
            pane.close();
            return Err(AndroidError::Compositor(format!(
                "the control socket {} could not be restricted to this user: {err}",
                control.display()
            )));
        }

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
        let saw_foreign = Arc::new(AtomicBool::new(false));
        let watch = BootWatch {
            binary: binary.clone(),
            display: socket,
            session_pid: session.id(),
            log,
            cancel: cancel.clone(),
            saw_foreign: saw_foreign.clone(),
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
            saw_foreign,
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
    /// [`STOP_GRACE`]) and reaped, then the compositor is closed. The global
    /// `session stop` is skipped unless [`should_stop_session`] says the
    /// running session is ours.
    pub fn teardown(&mut self) {
        if !self.torn_down {
            self.torn_down = true;
            self.cancel.store(true, Ordering::SeqCst);
            let child_alive = matches!(self.session.try_wait(), Ok(None));
            let saw_foreign = self.saw_foreign.load(Ordering::SeqCst);
            if should_stop_session(child_alive, saw_foreign) {
                let mut stop = Command::new(&self.binary);
                stop.args(session_stop_args());
                let _ = run_capture(&mut stop, STOP_TIMEOUT);
            }
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
    saw_foreign: Arc<AtomicBool>,
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
            let budget = command_budget(
                deadline.saturating_duration_since(Instant::now()),
                COMMAND_TIMEOUT,
            );
            if let Some(output) = run_capture(&mut self.command(status_args()), budget) {
                match boot_step(&parse_status(&output), &self.display) {
                    BootStep::Up => return Ok(adb_serial(&output)),
                    BootStep::Wait => {}
                    BootStep::Foreign(display) => {
                        self.saw_foreign.store(true, Ordering::SeqCst);
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
                    return Err(format!(
                        "`waydroid show-full-ui` could not be watched: {err}"
                    ));
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
            let budget = command_budget(
                deadline.saturating_duration_since(Instant::now()),
                COMMAND_TIMEOUT,
            );
            if run_capture(&mut self.command(boot_completed_args()), budget)
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
        assert_eq!(
            parse_status(NOT_INITIALISED),
            WaydroidStatus::NotInitialised
        );
        assert_eq!(parse_status(RUNNING), running("wayland-7"));
    }

    #[test]
    fn android_is_dead_when_the_session_or_the_compositor_is_gone() {
        assert!(!is_dead(false, true));
        assert!(is_dead(true, true));
        assert!(is_dead(false, false));
        assert!(is_dead(true, false));
    }

    #[test]
    fn status_reads_the_display_line_wherever_it_is() {
        // An absolute WAYLAND_DISPLAY path with slashes survives whole, and
        // the display line is found even when it is not the first line.
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
        assert_eq!(boot_completed_args(), ["prop", "get", "sys.boot_completed"]);
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

    #[test]
    fn an_unanswered_status_check_is_not_a_missing_binary() {
        let msg = AndroidError::StatusUnanswered.to_string();
        assert!(msg.contains("did not answer `waydroid status`"), "{msg}");
        assert!(!msg.contains("not installed"), "{msg}");
    }

    #[test]
    fn only_our_live_session_is_stopped() {
        assert!(should_stop_session(true, false));
        assert!(!should_stop_session(true, true));
        assert!(!should_stop_session(false, false));
        assert!(!should_stop_session(false, true));
    }

    #[test]
    fn a_command_never_outlives_the_remaining_budget() {
        let per = Duration::from_secs(10);
        assert_eq!(
            command_budget(Duration::from_secs(3), per),
            Duration::from_secs(3)
        );
        assert_eq!(command_budget(Duration::from_secs(30), per), per);
        assert_eq!(command_budget(Duration::ZERO, per), Duration::ZERO);
    }
}
