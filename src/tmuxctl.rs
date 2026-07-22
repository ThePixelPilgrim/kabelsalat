//! Thin wrapper around the private kabelsalat tmux server.
//!
//! All tmux interaction goes through this module: availability detection,
//! spawn argv construction, session listing, kill and respawn. Every
//! operation returns a `Result`; nothing in here panics.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Minimum tmux version required for `remain-on-exit failed` and
/// `prefix None`.
pub const MIN_VERSION: (u32, u32) = (3, 2);

/// Session name prefix on our private server.
const SESSION_PREFIX: &str = "ks-";

/// A parsed tmux version, e.g. "3.3a" or "next-3.4".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxVersion {
    pub major: u32,
    pub minor: u32,
    /// Trailing patch letter ("a" in "3.3a"), empty if none.
    pub suffix: String,
}

impl TmuxVersion {
    /// Parse the output of `tmux -V`, e.g. "tmux 3.3a" or "tmux next-3.4".
    pub fn parse(output: &str) -> Option<Self> {
        let word = output.trim().rsplit(' ').next()?;
        // Development builds report "next-3.4"; the numbers still tell us
        // whether the features we need exist.
        let word = word.strip_prefix("next-").unwrap_or(word);
        let (major, rest) = word.split_once('.')?;
        let major: u32 = major.parse().ok()?;
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return None;
        }
        let minor: u32 = rest[..digits].parse().ok()?;
        Some(Self {
            major,
            minor,
            suffix: rest[digits..].to_string(),
        })
    }

    pub fn meets_minimum(&self) -> bool {
        (self.major, self.minor) >= MIN_VERSION
    }
}

impl fmt::Display for TmuxVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}{}", self.major, self.minor, self.suffix)
    }
}

/// Result of the one-time startup availability check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxAvailability {
    Available(TmuxVersion),
    TooOld(TmuxVersion),
    Missing,
}

/// Check whether a usable tmux is installed (runnable and >= 3.2).
pub fn detect() -> TmuxAvailability {
    let output = match Command::new("tmux").arg("-V").output() {
        Ok(out) if out.status.success() => out,
        _ => return TmuxAvailability::Missing,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    match TmuxVersion::parse(&stdout) {
        Some(version) if version.meets_minimum() => TmuxAvailability::Available(version),
        Some(version) => TmuxAvailability::TooOld(version),
        None => TmuxAvailability::Missing,
    }
}

/// Errors from tmux wrapper operations.
#[derive(Debug)]
pub enum TmuxError {
    Io(std::io::Error),
    /// tmux exited non-zero; carries its stderr.
    Command(String),
    /// Unexpected output from a tmux query.
    Parse(String),
    /// Neither $XDG_RUNTIME_DIR nor a home directory could be determined.
    NoDirectory,
}

impl fmt::Display for TmuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TmuxError::Io(err) => write!(f, "tmux i/o error: {err}"),
            TmuxError::Command(msg) => write!(f, "tmux failed: {msg}"),
            TmuxError::Parse(msg) => write!(f, "unexpected tmux output: {msg}"),
            TmuxError::NoDirectory => write!(f, "no usable runtime or state directory"),
        }
    }
}

impl std::error::Error for TmuxError {}

impl From<std::io::Error> for TmuxError {
    fn from(err: std::io::Error) -> Self {
        TmuxError::Io(err)
    }
}

/// One live session on our server, as reported by `list-sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// Tab uuid (session name with the "ks-" prefix stripped).
    pub uuid: String,
    /// Whether the pane's shell has died (kept by `remain-on-exit failed`).
    pub pane_dead: bool,
    /// Exit code of the dead shell, if the pane is dead.
    pub dead_status: Option<i32>,
}

/// Invisible-plumbing tmux configuration. `{events_dir}` is replaced with
/// the runtime events directory before writing.
const TMUX_CONF: &str = "\
# Written by kabelsalat at startup; do not edit.
set -g status off
set -g prefix None
set -g prefix2 None
unbind -a
unbind -a -T root
unbind -a -T copy-mode
unbind -a -T copy-mode-vi
set -g mouse off
set -g detach-on-destroy on
set -s exit-empty on
set -g remain-on-exit failed
set -g history-limit 100000
set -g default-terminal xterm-256color
set-hook -g pane-died 'run-shell \"printf \\\"%s %s\\\" \\\"#{session_name}\\\" \\\"#{pane_dead_status}\\\" > \\\"{events_dir}/#{session_name}\\\"\"'
";

/// Handle to the private kabelsalat tmux server: socket, config and
/// event-file paths. Construction writes the config and creates the
/// runtime directories.
#[derive(Debug, Clone)]
pub struct TmuxCtl {
    socket: PathBuf,
    conf: PathBuf,
    events_dir: PathBuf,
}

impl TmuxCtl {
    /// Set up paths under $XDG_RUNTIME_DIR/kabelsalat (socket, events) and
    /// the state dir (tmux.conf), writing the embedded config.
    pub fn new() -> Result<Self, TmuxError> {
        let state_dir = state_dir().ok_or(TmuxError::NoDirectory)?;
        let runtime_dir = runtime_dir().unwrap_or_else(|| state_dir.clone());
        Self::with_dirs(&runtime_dir, &state_dir)
    }

    /// Like `new()` but with explicit directories (used by tests).
    pub fn with_dirs(runtime_dir: &Path, state_dir: &Path) -> Result<Self, TmuxError> {
        let events_dir = runtime_dir.join("events");
        std::fs::create_dir_all(&events_dir)?;
        std::fs::create_dir_all(state_dir)?;
        let conf = state_dir.join("tmux.conf");
        let body = TMUX_CONF.replace("{events_dir}", &events_dir.to_string_lossy());
        std::fs::write(&conf, body)?;
        Ok(Self {
            socket: runtime_dir.join("tmux.sock"),
            conf,
            events_dir,
        })
    }

    /// Directory the pane-died hook writes "<uuid> <exit-code>" files into.
    pub fn events_dir(&self) -> &Path {
        &self.events_dir
    }

    /// Argv for spawning (or reattaching) the backing session of a tab:
    /// `tmux -S <sock> -f <conf> new-session -A -s ks-<uuid> $SHELL`.
    pub fn spawn_argv(&self, uuid: &str) -> Vec<String> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        vec![
            "tmux".into(),
            "-S".into(),
            self.socket.to_string_lossy().into_owned(),
            "-f".into(),
            self.conf.to_string_lossy().into_owned(),
            "new-session".into(),
            "-A".into(),
            "-s".into(),
            format!("{SESSION_PREFIX}{uuid}"),
            shell,
        ]
    }

    /// List live `ks-*` sessions with their pane-dead state.
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, TmuxError> {
        let output = Command::new("tmux")
            .args(["-S", &self.socket.to_string_lossy()])
            .args([
                "list-sessions",
                "-F",
                "#{session_name}\t#{pane_dead}\t#{pane_dead_status}",
            ])
            .output()?;
        if !output.status.success() {
            // A missing socket / stopped server simply means no sessions.
            let stderr = String::from_utf8_lossy(&output.stderr);
            if is_no_server_stderr(&stderr) {
                return Ok(Vec::new());
            }
            return Err(TmuxError::Command(stderr.trim().to_string()));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut sessions = Vec::new();
        for line in stdout.lines() {
            if let Some(session) = parse_session_line(line)? {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    /// Kill the backing session of a tab (explicit tab close).
    pub fn kill_session(&self, uuid: &str) -> Result<(), TmuxError> {
        self.run(&["kill-session", "-t", &format!("{SESSION_PREFIX}{uuid}")])
    }

    /// Rerun the shell in a crashed tab's dead pane (tab restart).
    pub fn respawn_pane(&self, uuid: &str) -> Result<(), TmuxError> {
        self.run(&["respawn-pane", "-t", &format!("{SESSION_PREFIX}{uuid}")])
    }

    fn run(&self, args: &[&str]) -> Result<(), TmuxError> {
        let output = Command::new("tmux")
            .args(["-S", &self.socket.to_string_lossy()])
            .args(args)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(TmuxError::Command(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
    }
}

/// Parse one `list-sessions -F` line into a `SessionInfo`, or `None` if it
/// names a foreign (non-`ks-`) session on our socket to be ignored.
fn parse_session_line(line: &str) -> Result<Option<SessionInfo>, TmuxError> {
    let mut parts = line.split('\t');
    let (Some(name), Some(dead), status) = (parts.next(), parts.next(), parts.next()) else {
        return Err(TmuxError::Parse(line.to_string()));
    };
    let Some(uuid) = name.strip_prefix(SESSION_PREFIX) else {
        return Ok(None); // foreign session on our socket; ignore
    };
    let pane_dead = dead == "1";
    let dead_status = if pane_dead {
        status.and_then(|s| s.parse().ok())
    } else {
        None
    };
    Ok(Some(SessionInfo {
        uuid: uuid.to_string(),
        pane_dead,
        dead_status,
    }))
}

/// Whether `list-sessions` stderr indicates "no server / socket running",
/// which is not an error condition but simply an empty session list.
fn is_no_server_stderr(stderr: &str) -> bool {
    stderr.contains("no server running") || stderr.contains("No such file")
}

fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join("kabelsalat"))
}

fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(dir).join("kabelsalat"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state/kabelsalat"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_version() {
        let v = TmuxVersion::parse("tmux 3.2").unwrap();
        assert_eq!((v.major, v.minor, v.suffix.as_str()), (3, 2, ""));
        assert!(v.meets_minimum());
    }

    #[test]
    fn parses_letter_suffix() {
        let v = TmuxVersion::parse("tmux 3.3a\n").unwrap();
        assert_eq!((v.major, v.minor, v.suffix.as_str()), (3, 3, "a"));
        assert_eq!(v.to_string(), "3.3a");
    }

    #[test]
    fn parses_next_prefix() {
        let v = TmuxVersion::parse("tmux next-3.4").unwrap();
        assert_eq!((v.major, v.minor), (3, 4));
        assert!(v.meets_minimum());
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(TmuxVersion::parse("tmux"), None);
        assert_eq!(TmuxVersion::parse(""), None);
        assert_eq!(TmuxVersion::parse("tmux x.y"), None);
    }

    #[test]
    fn old_versions_below_minimum() {
        assert!(!TmuxVersion::parse("tmux 3.1c").unwrap().meets_minimum());
        assert!(!TmuxVersion::parse("tmux 2.9a").unwrap().meets_minimum());
        assert!(TmuxVersion::parse("tmux 4.0").unwrap().meets_minimum());
    }

    fn test_ctl(dir: &Path) -> TmuxCtl {
        TmuxCtl::with_dirs(&dir.join("run"), &dir.join("state")).unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kabelsalat-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn spawn_argv_shape() {
        let dir = temp_dir("argv");
        let ctl = test_ctl(&dir);
        let argv = ctl.spawn_argv("1234-abcd");
        assert_eq!(argv[0], "tmux");
        assert_eq!(argv[1], "-S");
        assert!(argv[2].ends_with("run/tmux.sock"));
        assert_eq!(argv[3], "-f");
        assert!(argv[4].ends_with("state/tmux.conf"));
        assert_eq!(&argv[5..9], ["new-session", "-A", "-s", "ks-1234-abcd"]);
        assert!(!argv[9].is_empty()); // $SHELL or /bin/bash
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn writes_config_with_events_dir() {
        let dir = temp_dir("conf");
        let ctl = test_ctl(&dir);
        let conf = std::fs::read_to_string(dir.join("state/tmux.conf")).unwrap();
        assert!(conf.contains("set -g status off"));
        assert!(conf.contains("remain-on-exit failed"));
        assert!(conf.contains(&*ctl.events_dir().to_string_lossy()));
        assert!(!conf.contains("{events_dir}"));
        assert!(ctl.events_dir().is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }

    // -- finding 3: TMUX_CONF pins the config lines that make crash
    // detection and rendering safe (kill-server, prefix, TERM, hook). --

    #[test]
    fn tmux_conf_pins_remain_on_exit_failed() {
        assert!(TMUX_CONF.contains("remain-on-exit failed"));
    }

    #[test]
    fn tmux_conf_pins_default_terminal_xterm_256color() {
        // finding 3: refuted-but-proven-correct — VTE's default TERM is
        // xterm-256color and the app never overrides it, so this line must
        // stay pinned; a future TERM override should force revisiting it.
        assert!(TMUX_CONF.contains("default-terminal xterm-256color"));
    }

    #[test]
    fn tmux_conf_pins_status_off() {
        assert!(TMUX_CONF.contains("set -g status off"));
    }

    #[test]
    fn tmux_conf_pins_prefix_disabled() {
        assert!(TMUX_CONF.contains("set -g prefix None"));
        assert!(TMUX_CONF.contains("set -g prefix2 None"));
    }

    #[test]
    fn tmux_conf_pins_exit_empty() {
        assert!(TMUX_CONF.contains("set -s exit-empty on"));
    }

    #[test]
    fn tmux_conf_pins_pane_died_hook_writes_session_and_code() {
        assert!(TMUX_CONF.contains("set-hook -g pane-died"));
        assert!(TMUX_CONF.contains("#{session_name}"));
        assert!(TMUX_CONF.contains("#{pane_dead_status}"));
        assert!(TMUX_CONF.contains("{events_dir}/#{session_name}"));
    }

    // -- finding 2: dead_status None on a dead pane is the unknown-exit
    // sentinel that must render as "[exit ?]", never "[exit -1]". --

    #[test]
    fn parse_session_line_dead_with_empty_status_yields_none() {
        let session = parse_session_line("ks-abcd\t1\t")
            .unwrap()
            .expect("ks- prefixed session");
        assert_eq!(session.uuid, "abcd");
        assert!(session.pane_dead);
        assert_eq!(session.dead_status, None);
    }

    #[test]
    fn parse_session_line_dead_with_nonzero_status_yields_some() {
        let session = parse_session_line("ks-abcd\t1\t137")
            .unwrap()
            .expect("ks- prefixed session");
        assert!(session.pane_dead);
        assert_eq!(session.dead_status, Some(137));
    }

    #[test]
    fn parse_session_line_dead_with_zero_status_yields_some_zero() {
        let session = parse_session_line("ks-abcd\t1\t0")
            .unwrap()
            .expect("ks- prefixed session");
        assert!(session.pane_dead);
        assert_eq!(session.dead_status, Some(0));
    }

    #[test]
    fn parse_session_line_alive_ignores_status_field() {
        let session = parse_session_line("ks-abcd\t0\t42")
            .unwrap()
            .expect("ks- prefixed session");
        assert!(!session.pane_dead);
        assert_eq!(session.dead_status, None);
    }

    #[test]
    fn parse_session_line_ignores_foreign_session() {
        assert_eq!(parse_session_line("other-session\t0\t").unwrap(), None);
    }

    #[test]
    fn parse_session_line_rejects_malformed_line() {
        // Missing the `dead` field entirely (no tab at all) is malformed.
        assert!(parse_session_line("ks-abcd").is_err());
    }

    #[test]
    fn parse_session_line_missing_status_field_defaults_dead_status_none() {
        // Dead field present, status field absent: still parseable, and an
        // absent status is treated the same as an empty one (finding 2).
        let session = parse_session_line("ks-abcd\t1")
            .unwrap()
            .expect("ks- prefixed session");
        assert!(session.pane_dead);
        assert_eq!(session.dead_status, None);
    }

    // -- finding 1: "no server"/"no such file" stderr must be classified as
    // "empty list", not an error, so the ChildExited handler never mistakes
    // a transient failure for a definitively gone session. --

    #[test]
    fn no_server_running_stderr_is_empty_list_not_error() {
        assert!(is_no_server_stderr("no server running on ...\n"));
    }

    #[test]
    fn no_such_file_stderr_is_empty_list_not_error() {
        assert!(is_no_server_stderr(
            "error connecting to /run/kabelsalat/tmux.sock (No such file or directory)\n"
        ));
    }

    #[test]
    fn other_stderr_is_not_classified_as_no_server() {
        // Any other failure (permission denied, corrupted socket, etc.)
        // must surface as Err, never be silently treated as "gone".
        assert!(!is_no_server_stderr("permission denied\n"));
        assert!(!is_no_server_stderr(""));
    }

    /// End-to-end against a real tmux; skipped when tmux is unavailable
    /// so `cargo test` stays hermetic.
    #[test]
    fn live_session_roundtrip() {
        if !matches!(detect(), TmuxAvailability::Available(_)) {
            return;
        }
        let dir = temp_dir("live");
        let ctl = test_ctl(&dir);
        let argv = ctl.spawn_argv("itest");
        // Run detached (-d) instead of attaching a client.
        let status = Command::new(&argv[0])
            .args(&argv[1..6])
            .arg("-d")
            .args(&argv[6..])
            .status()
            .unwrap();
        assert!(status.success());
        let sessions = ctl.list_sessions().unwrap();
        assert!(sessions.iter().any(|s| s.uuid == "itest" && !s.pane_dead));
        ctl.kill_session("itest").unwrap();
        assert!(ctl.list_sessions().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
