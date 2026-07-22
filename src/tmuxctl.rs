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

/// Whether lingering (logout survival) is enabled for the current user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LingerStatus {
    /// `Linger=yes` — shells already survive logout; no icon.
    Enabled,
    /// `Linger=no` — offer to enable it; show the warning icon.
    Disabled,
    /// loginctl missing, non-systemd system, no user, or any error — no icon.
    NotApplicable,
}

/// Parse the value from `loginctl show-user <user> --property=Linger` output.
/// `Linger=yes` → `Some(true)`, `Linger=no` → `Some(false)`, anything else
/// (garbage, empty, missing property) → `None`.
pub fn parse_linger(output: &str) -> Option<bool> {
    for line in output.lines() {
        if let Some(value) = line.trim().strip_prefix("Linger=") {
            return match value.trim() {
                "yes" => Some(true),
                "no" => Some(false),
                _ => None,
            };
        }
    }
    None
}

/// The current user's login name, from `$USER` or `$LOGNAME`.
pub fn current_user() -> Option<String> {
    std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("LOGNAME").ok())
        .filter(|u| !u.is_empty())
}

/// Whether `systemd-run` is available to start the tmux server in its own
/// transient user scope (so session-scope cleanup at logout cannot kill it).
pub fn has_systemd_run() -> bool {
    Command::new("systemd-run")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check whether lingering is enabled for the current user. A missing
/// `loginctl` (non-systemd system), no resolvable user, or any error all map
/// to `NotApplicable` (no icon) — never surfaced as an error dialog.
pub fn detect_linger() -> LingerStatus {
    let Some(user) = current_user() else {
        return LingerStatus::NotApplicable;
    };
    let output = match Command::new("loginctl")
        .args(["show-user", &user, "--property=Linger"])
        .output()
    {
        Ok(out) if out.status.success() => out,
        _ => return LingerStatus::NotApplicable,
    };
    match parse_linger(&String::from_utf8_lossy(&output.stdout)) {
        Some(true) => LingerStatus::Enabled,
        Some(false) => LingerStatus::Disabled,
        None => LingerStatus::NotApplicable,
    }
}

/// Enable lingering for the current user (`loginctl enable-linger <user>`).
pub fn enable_linger() -> Result<(), TmuxError> {
    let user = current_user().ok_or_else(|| TmuxError::Command("no current user".into()))?;
    let output = Command::new("loginctl")
        .args(["enable-linger", &user])
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(TmuxError::Command(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
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
# Mouse must stay ON: tmux runs in VTE's alternate screen, where VTE's
# fallback scrolling turns the wheel into cursor-up/down keypresses that
# leak into the shell as history navigation or literal ^[[A. Requesting
# mouse tracking makes VTE hand wheel events to tmux instead. The root and
# copy-mode key tables are wiped by `unbind -a` above, so the wheel and
# copy-mode exit have to be re-bound explicitly below.
set -g mouse on
set -g mode-keys emacs
# tmux's stock root binding: forward to apps that grabbed the mouse
# (vim, htop), otherwise enter copy-mode and scroll the history.
bind -n WheelUpPane if -Ft= '#{?pane_in_mode,1,#{mouse_any_flag}}' 'send -M' 'copy-mode -et='
bind -n WheelDownPane send -M
bind -T copy-mode WheelUpPane send -X -N 3 scroll-up
bind -T copy-mode WheelDownPane send -X -N 3 scroll-down
bind -T copy-mode Escape send -X cancel
bind -T copy-mode q send -X cancel
set -g detach-on-destroy on
# exit-empty must stay OFF: ensure_server() pre-claims the socket with a
# detached, out-of-scope `start-server` that holds zero sessions, so later
# VTE clients only attach instead of implicitly starting the server inside
# the GUI's session scope. With exit-empty on, that empty server would exit
# immediately, defeating logout survival. Keeping it off leaves a small
# server running with no sessions, which is the intended tradeoff.
set -s exit-empty off
set -g remain-on-exit failed
# Forward the shell's OSC title to the outer terminal (VTE), which feeds
# the GUI tab titles; without set-titles tmux swallows it into pane_title.
set -g set-titles on
set -g set-titles-string \"#{pane_title}\"
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

    /// Argv that starts the private tmux server. When `use_systemd_run` is
    /// true the server is launched inside its own transient user scope
    /// (`systemd-run --user --scope --collect …`) so that session-scope
    /// cleanup at logout (e.g. GNOME/Wayland) cannot kill it; otherwise a
    /// plain `tmux … start-server`.
    ///
    /// This must run once, before any VTE client's `new-session -A`: a client's
    /// implicit server start would place the server inside the GUI's session
    /// scope. An explicit detached `start-server` claims the socket first, and
    /// later clients merely attach to the already-running server.
    pub fn server_start_argv(&self, use_systemd_run: bool) -> Vec<String> {
        let tmux = vec![
            "tmux".to_string(),
            "-S".into(),
            self.socket.to_string_lossy().into_owned(),
            "-f".into(),
            self.conf.to_string_lossy().into_owned(),
            "start-server".into(),
        ];
        if use_systemd_run {
            let mut argv = vec![
                "systemd-run".to_string(),
                "--user".into(),
                "--scope".into(),
                "--collect".into(),
            ];
            argv.extend(tmux);
            argv
        } else {
            tmux
        }
    }

    /// Start the detached tmux server. Idempotent: `start-server` against an
    /// already-running socket just connects and exits. Best-effort — a failure
    /// only means clients will start the server themselves (attached, so it
    /// won't survive logout), which is the pre-existing fallback behavior.
    pub fn ensure_server(&self, use_systemd_run: bool) -> Result<(), TmuxError> {
        let argv = self.server_start_argv(use_systemd_run);
        let output = Command::new(&argv[0]).args(&argv[1..]).output()?;
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
            .arg(&self.socket)
            .arg("source-file")
            .arg(&self.conf)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(TmuxError::Command(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ))
        }
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
    fn server_start_argv_plain() {
        let dir = temp_dir("srv-plain");
        let ctl = test_ctl(&dir);
        let argv = ctl.server_start_argv(false);
        assert_eq!(argv[0], "tmux");
        assert_eq!(argv[1], "-S");
        assert!(argv[2].ends_with("run/tmux.sock"));
        assert_eq!(argv[3], "-f");
        assert!(argv[4].ends_with("state/tmux.conf"));
        assert_eq!(argv[5], "start-server");
        assert_eq!(argv.len(), 6);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn server_start_argv_via_systemd_run() {
        let dir = temp_dir("srv-systemd");
        let ctl = test_ctl(&dir);
        let argv = ctl.server_start_argv(true);
        assert_eq!(
            &argv[..4],
            ["systemd-run", "--user", "--scope", "--collect"]
        );
        // The full tmux start-server invocation follows the scope wrapper.
        assert_eq!(argv[4], "tmux");
        assert_eq!(argv.last().unwrap(), "start-server");
        assert_eq!(argv[5..], ctl.server_start_argv(false)[1..]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_linger_yes_no() {
        assert_eq!(parse_linger("Linger=yes"), Some(true));
        assert_eq!(parse_linger("Linger=no\n"), Some(false));
        assert_eq!(parse_linger("Linger=yes\n"), Some(true));
    }

    #[test]
    fn parse_linger_garbage_and_empty() {
        assert_eq!(parse_linger(""), None);
        assert_eq!(parse_linger("Linger="), None);
        assert_eq!(parse_linger("Linger=maybe"), None);
        assert_eq!(parse_linger("Something=yes"), None);
        assert_eq!(parse_linger("garbage without equals"), None);
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
    fn tmux_conf_forwards_shell_titles_to_outer_terminal() {
        // Regression (v0.2.1): without set-titles, tmux swallows the shell's
        // OSC title into pane_title and VTE never sees it, so GUI tab titles
        // stopped updating under tmux backing.
        assert!(TMUX_CONF.contains("set -g set-titles on"));
        assert!(TMUX_CONF.contains("set-titles-string \"#{pane_title}\""));
    }

    #[test]
    fn tmux_conf_pins_mouse_on_with_wheel_bindings() {
        // Regression: with `mouse off`, tmux never requested mouse tracking,
        // so VTE's alternate-screen fallback scrolling turned the wheel into
        // ^[[A/^[[B keypresses (shell history walked, ^[[A echoed by raw
        // programs). Mouse on alone is not enough -- `unbind -a -T root`
        // above also drops tmux's stock wheel bindings.
        assert!(TMUX_CONF.contains("set -g mouse on"));
        assert!(TMUX_CONF.contains("bind -n WheelUpPane"));
        assert!(TMUX_CONF.contains("bind -n WheelDownPane"));
        // Copy-mode needs its own wheel bindings and a way out.
        assert!(TMUX_CONF.contains("bind -T copy-mode WheelUpPane"));
        assert!(TMUX_CONF.contains("bind -T copy-mode Escape send -X cancel"));
        // The wheel bindings must come after the unbind -a lines that wipe
        // the tables they live in.
        assert!(
            TMUX_CONF.find("unbind -a -T copy-mode").unwrap()
                < TMUX_CONF.find("bind -n WheelUpPane").unwrap()
        );
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
        // Must be OFF so the detached, out-of-scope server pre-claimed by
        // ensure_server() survives with zero sessions; exit-empty on would
        // make it exit immediately and defeat logout survival.
        assert!(TMUX_CONF.contains("set -s exit-empty off"));
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
