//! Pure logic for remote groups: which ssh client and login mode to use, how
//! ssh and tmux failures read, and how remote commands are built and quoted.
//!
//! Like `state.rs` and `cli.rs` this module runs no processes and touches no
//! GTK; `remote_worker.rs` does the I/O. That split is what keeps it
//! unit-testable.

use std::path::{Path, PathBuf};

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
            RemoteError::AuthFailed => {
                format!("Logging in to {host} failed. Check your key or password, then Reconnect.")
            }
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
    if stderr.contains("host key is known for") || stderr.contains("Host key verification failed") {
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

/// File name of `dest`'s master socket: a fixed FNV-1a hash of the
/// destination, not ssh's `%C`. `%C` hashes in `%j` (ProxyJump), and the
/// tabs' `-o ProxyCommand=false` clears ProxyJump, so for a host with a
/// ProxyJump in `~/.ssh/config` the tabs would look for a different socket
/// than the master's and never find it. A literal name is the same for every
/// command; `ks-` plus 16 hex digits stays far below the socket path limit.
pub fn control_socket_name(dest: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in dest.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("ks-{hash:016x}")
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
        assert_eq!(
            parse_ssh_version("OpenSSH_9.9p1, OpenSSL 3.2.2"),
            Some((9, 9))
        );
        assert_eq!(
            parse_ssh_version("OpenSSH_10.2p1, OpenSSL 3.5.8 25 Aug 2026"),
            Some((10, 2))
        );
    }

    #[test]
    fn parse_ssh_version_rejects_other_clients_and_garbage() {
        assert_eq!(
            parse_ssh_version("OpenSSH_for_Windows_8.1p1, LibreSSL 3.0.2"),
            None
        );
        assert_eq!(
            parse_ssh_version("Sun_SSH_1.1.8, SSH protocols 1.5/2.0"),
            None
        );
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
        assert_eq!(
            ssh_availability(Some("Dropbear v2022.83")),
            SshAvailability::Missing
        );
        assert_eq!(ssh_availability(None), SshAvailability::Missing);
    }

    #[test]
    fn only_an_available_client_has_no_reason() {
        assert!(SshAvailability::Available((9, 0)).is_available());
        assert_eq!(SshAvailability::Available((9, 0)).reason(), None);
        let too_old = SshAvailability::TooOld((7, 4)).reason().unwrap();
        assert!(
            too_old.contains("8.4") && too_old.contains("7.4"),
            "{too_old}"
        );
        assert!(!SshAvailability::Missing.is_available());
        assert!(SshAvailability::Missing.reason().unwrap().contains("8.4"));
    }

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
            executables(&[
                "/a/ssh-askpass",
                "/b/lxqt-openssh-askpass",
                "/b/ksshaskpass",
            ]),
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
        assert_eq!(
            auth_mode(env_of(&[]), executables(&[])),
            AuthMode::BatchOnly
        );
    }

    #[test]
    fn askpass_env_forces_askpass_and_batch_sets_nothing() {
        assert_eq!(
            askpass_env(&askpass("/usr/bin/ksshaskpass")),
            vec![
                (
                    "SSH_ASKPASS".to_string(),
                    "/usr/bin/ksshaskpass".to_string()
                ),
                ("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()),
            ]
        );
        assert!(askpass_env(&AuthMode::BatchOnly).is_empty());
    }

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
            assert_eq!(
                classify(Some(255), stderr, &BATCH),
                RemoteError::Unreachable,
                "{stderr}"
            );
        }
    }

    #[test]
    fn an_unknown_host_key_is_host_key_unknown() {
        let stderr = "No ED25519 host key is known for [localhost]:2222 and you have \
                      requested strict checking.\r\nHost key verification failed.\r\n";
        assert_eq!(
            classify(Some(255), stderr, &BATCH),
            RemoteError::HostKeyUnknown
        );
        // With askpass, a declined confirmation ends the same way.
        assert_eq!(
            classify(
                Some(255),
                "Host key verification failed.\r\n",
                &with_askpass()
            ),
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
        assert_eq!(
            classify(Some(255), stderr, &BATCH),
            RemoteError::HostKeyChanged
        );
    }

    #[test]
    fn permission_denied_depends_on_whether_a_prompt_was_possible() {
        let stderr = "me@localhost: Permission denied (publickey,gssapi-keyex,gssapi-with-mic,password).\r\n";
        assert_eq!(
            classify(Some(255), stderr, &BATCH),
            RemoteError::AuthNeedsAskpass
        );
        assert_eq!(
            classify(Some(255), stderr, &with_askpass()),
            RemoteError::AuthFailed
        );
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
            assert_eq!(
                classify(Some(127), stderr, &BATCH),
                RemoteError::TmuxMissing,
                "{stderr}"
            );
        }
        // Exit 127 alone is enough, whatever the shell printed.
        assert_eq!(classify(Some(127), "", &BATCH), RemoteError::TmuxMissing);
    }

    #[test]
    fn anything_else_keeps_its_stderr() {
        assert_eq!(
            classify(
                Some(255),
                "  mux_client_request_session: read from master failed  \n",
                &BATCH
            ),
            RemoteError::Other("mux_client_request_session: read from master failed".into())
        );
        assert_eq!(
            classify(None, "", &BATCH),
            RemoteError::Other(String::new())
        );
    }

    #[test]
    fn every_error_names_what_to_do() {
        let host = "me@box";
        assert!(
            RemoteError::HostKeyUnknown
                .message(host)
                .contains("ssh me@box")
        );
        assert!(
            RemoteError::HostKeyChanged
                .message(host)
                .contains("ssh-keygen -R")
        );
        assert!(
            RemoteError::AuthNeedsAskpass
                .message(host)
                .contains("askpass")
        );
        assert!(
            RemoteError::TmuxTooOld("3.1c".into())
                .message(host)
                .contains("3.1c")
        );
        assert!(
            RemoteError::TmuxTooOld("3.1c".into())
                .message(host)
                .contains("3.2")
        );
        assert!(RemoteError::TmuxMissing.message(host).contains("3.2"));
        assert!(RemoteError::SshUnsupported.message(host).contains("8.4"));
        assert!(
            RemoteError::Other("boom".into())
                .message(host)
                .contains("boom")
        );
        for err in [
            RemoteError::Unreachable,
            RemoteError::AuthFailed,
            RemoteError::Other(String::new()),
        ] {
            assert!(err.message(host).contains(host), "{err:?}");
        }
    }

    // --- argv and quoting ---

    /// Run `script` with the local `sh -c` and return its stdout. Stands in
    /// for the remote login shell, which only has to parse POSIX quotes.
    fn run_sh(script: &str) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
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
            let at = argv
                .iter()
                .position(|a| a == option)
                .unwrap_or_else(|| panic!("{option}"));
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
    fn the_control_socket_is_named_by_the_destination_alone() {
        let name = control_socket_name("me@box");
        // Stable across runs and builds: a master left running by the last
        // run must be found again.
        assert_eq!(name, control_socket_name("me@box"));
        assert_eq!(name, "ks-fae6588ce9ae0dd2");
        assert_ne!(name, control_socket_name("me@box2"));
        // No ssh tokens: every command must name the same literal file.
        assert!(!control_socket_name("%C%j@h").contains('%'));
        assert!(name.len() < 32);
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
}
