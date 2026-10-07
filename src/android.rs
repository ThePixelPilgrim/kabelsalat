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
