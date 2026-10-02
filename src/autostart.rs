//! Resume agent sessions at boot: the systemd user unit behind the primary
//! menu's "Resume agent sessions at boot…" entry, installed and removed from
//! the GUI (see docs/superpowers/specs/2026-10-02-boot-resume-design.md).
//!
//! The decisions — the unit's text, what state the installation is in, which
//! dialog responses that state offers — are pure functions over facts the
//! caller supplies, so they are unit-tested here. The thin `systemctl`
//! runners at the bottom are the only I/O, and like `tmuxctl` they never
//! panic: every fallible path returns a `Result`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::tmuxctl::LingerStatus;

/// The unit's file name under the user manager's unit directory.
pub const UNIT_NAME: &str = "kabelsalat-resume.service";

/// Where the unit lives: `<config home>/systemd/user/kabelsalat-resume.service`.
pub fn unit_path(config_home: &Path) -> PathBuf {
    config_home.join("systemd").join("user").join(UNIT_NAME)
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
}

/// The unit's text for this binary and this state file. Both paths are
/// rendered absolute rather than through `%S`-style specifiers: the user
/// manager need not share the GUI's `XDG_*` variables, and the GUI's view
/// is the one that must match. `%` is a specifier in unit files and is
/// escaped; a quoted `ExecStart` keeps a space in the path from splitting it.
pub fn unit_text(exe: &Path, state_file: &Path) -> String {
    let exe = exe.display().to_string().replace('%', "%%");
    let state_file = state_file.display().to_string().replace('%', "%%");
    format!(
        "# Written by kabelsalat; install or remove it from the app's primary menu.\n\
         [Unit]\n\
         Description=Resume kabelsalat agent sessions after boot\n\
         ConditionPathExists={state_file}\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart=\"{exe}\" resume\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// What the installation is in, computed from the system every time rather
/// than remembered: the file, `systemctl --user is-enabled`, and lingering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No usable tmux or no `systemctl --user`: the menu entry is insensitive.
    Unavailable,
    /// Nothing runs at boot: no unit, or one that is not enabled.
    Off,
    /// Enabled and current. `at_boot` is lingering: without it the unit can
    /// only run once the user logs in.
    On { at_boot: bool },
    /// Enabled, but the file is missing or differs from what this build
    /// writes — the binary moved, or a newer template shipped.
    Stale,
}

/// Classify the installation. `on_disk` is the unit file's text, if any;
/// `expected` what [`unit_text`] renders for this build.
pub fn classify(
    available: bool,
    on_disk: Option<&str>,
    expected: &str,
    enabled: bool,
    linger: LingerStatus,
) -> Status {
    if !available {
        return Status::Unavailable;
    }
    if !enabled {
        return Status::Off;
    }
    match on_disk {
        Some(text) if text == expected => Status::On {
            at_boot: linger == LingerStatus::Enabled,
        },
        _ => Status::Stale,
    }
}

/// The dimmed status line under the menu entry.
pub fn status_label(status: Status) -> &'static str {
    match status {
        Status::Unavailable => "Unavailable",
        Status::Off => "Off",
        Status::On { at_boot: true } => "On",
        Status::On { at_boot: false } => "At login only",
        Status::Stale => "Needs repair",
    }
}

/// How a dialog response is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Default,
    Suggested,
    Destructive,
}

/// A response of the boot-resume dialog. The id is what `adw::AlertDialog`
/// reports back; the label is what the user reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    Cancel,
    Close,
    Install,
    Remove,
    EnableLinger,
    Repair,
}

impl Response {
    pub fn id(self) -> &'static str {
        match self {
            Response::Cancel => "cancel",
            Response::Close => "close",
            Response::Install => "install",
            Response::Remove => "remove",
            Response::EnableLinger => "enable-linger",
            Response::Repair => "repair",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Response::Cancel => "Cancel",
            Response::Close => "Close",
            Response::Install => "Install",
            Response::Remove => "Remove",
            Response::EnableLinger => "Enable lingering",
            Response::Repair => "Repair",
        }
    }

    pub fn appearance(self) -> Appearance {
        match self {
            Response::Install | Response::EnableLinger | Response::Repair => {
                Appearance::Suggested
            }
            Response::Remove => Appearance::Destructive,
            Response::Cancel | Response::Close => Appearance::Default,
        }
    }

    /// The response from its dialog id, for the `connect_response` handler.
    pub fn from_id(id: &str) -> Option<Self> {
        [
            Response::Cancel,
            Response::Close,
            Response::Install,
            Response::Remove,
            Response::EnableLinger,
            Response::Repair,
        ]
        .into_iter()
        .find(|response| response.id() == id)
    }
}

/// The responses the dialog offers in each state, in display order. The
/// first one is also the close response (Escape).
pub fn responses(status: Status) -> Vec<Response> {
    use Response::*;
    match status {
        Status::Unavailable => vec![Close],
        Status::Off => vec![Cancel, Install],
        Status::On { at_boot: true } => vec![Close, Remove],
        Status::On { at_boot: false } => vec![Close, Remove, EnableLinger],
        Status::Stale => vec![Close, Repair],
    }
}

pub fn dialog_heading() -> &'static str {
    "Resume agent sessions at boot"
}

/// The dialog's body for a state; `path` is where the unit is or would be.
pub fn dialog_body(status: Status, path: &Path) -> String {
    let path = path.display();
    match status {
        Status::Unavailable => "Resuming sessions at boot needs tmux and a systemd user manager \
             (`systemctl --user`), which this system does not provide."
            .to_string(),
        Status::Off => format!(
            "kabelsalat can bring the Claude sessions of your tabs back right after the \
             machine boots, before you log in, so they are already running when you open \
             it. Plain shells are not resumed this way.\n\n\
             Install writes a systemd user unit, {path}, enables it, and enables lingering \
             for your user if it is off — without lingering the unit can only run once you \
             log in. Nothing in your shell setup is touched.\n\n\
             This needs an unencrypted home directory: an encrypted one is not mounted \
             before login, and the unit then simply does nothing."
        ),
        Status::On { at_boot: true } => format!(
            "Claude sessions are resumed right after boot by {UNIT_NAME}.\n\n\
             Remove disables the unit and deletes {path}. Running sessions are not \
             affected, and lingering stays as it is."
        ),
        Status::On { at_boot: false } => format!(
            "{UNIT_NAME} is installed, but lingering is off for your user, so your Claude \
             sessions come back only once you log in rather than right after boot. Enable \
             lingering to have them resume at boot.\n\n\
             Remove disables the unit and deletes {path}. Running sessions are not affected."
        ),
        Status::Stale => format!(
            "{UNIT_NAME} is enabled but does not match this version of kabelsalat — the \
             binary may have moved. Repair rewrites {path} so the next boot runs the \
             current binary."
        ),
    }
}

// --- I/O: the system's answers, and the two changes ---------------------

/// Whether `systemctl` exists at all. Whether the *user* manager answers is
/// found out by the calls themselves, which report their error.
pub fn has_systemctl() -> bool {
    Command::new("systemctl")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// `is-enabled` prints one word; only the exact `enabled` counts. A runtime
/// enablement (`enabled-runtime`) vanishes at reboot, which is precisely
/// when the unit must run, so it is treated as not enabled.
pub fn parse_is_enabled(stdout: &str) -> bool {
    stdout.trim() == "enabled"
}

fn systemctl(args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|err| format!("systemctl --user {}: {err}", args.join(" ")))
}

/// Run a `systemctl --user` change and turn a non-zero exit into its stderr.
fn systemctl_ok(args: &[&str]) -> Result<(), String> {
    let output = systemctl(args)?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!(
            "systemctl --user {}: {}",
            args.join(" "),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        ))
    }
}

fn is_enabled() -> bool {
    systemctl(&["is-enabled", UNIT_NAME])
        .map(|output| parse_is_enabled(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or(false)
}

/// The text this build writes: its own path and the state file it reads.
fn expected_unit_text() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(unit_text(&exe, &crate::state::state_file()))
}

fn installed_unit_path() -> Option<PathBuf> {
    config_home().map(|home| unit_path(&home))
}

/// Read the system and classify. `available` is the caller's "tmux is usable
/// and systemctl exists"; without it nothing is queried.
pub fn detect(available: bool, linger: LingerStatus) -> Status {
    if !available {
        return Status::Unavailable;
    }
    let Some(expected) = expected_unit_text() else {
        return Status::Unavailable;
    };
    let on_disk = installed_unit_path().and_then(|path| std::fs::read_to_string(path).ok());
    classify(true, on_disk.as_deref(), &expected, is_enabled(), linger)
}

/// Write the unit for this build, reload the user manager and enable it.
/// Also the repair: a stale unit is simply rewritten.
pub fn install() -> Result<(), String> {
    let text = expected_unit_text().ok_or("cannot determine this binary's path")?;
    let path = installed_unit_path().ok_or("no home directory to install into")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("creating {}: {err}", dir.display()))?;
    }
    std::fs::write(&path, text).map_err(|err| format!("writing {}: {err}", path.display()))?;
    systemctl_ok(&["daemon-reload"])?;
    systemctl_ok(&["enable", UNIT_NAME])
}

/// Disable the unit, delete its file and reload. Lingering is left alone:
/// the linger dialog enables that for tmux survival in its own right.
pub fn remove() -> Result<(), String> {
    let path = installed_unit_path().ok_or("no home directory to remove from")?;
    // `disable` on a unit the manager has never seen fails; the file is what
    // matters, so that failure only counts when the file is there to disable.
    let disable = systemctl_ok(&["disable", UNIT_NAME]);
    match std::fs::remove_file(&path) {
        Ok(()) => disable?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(format!("removing {}: {err}", path.display())),
    }
    systemctl_ok(&["daemon-reload"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn text() -> String {
        unit_text(
            Path::new("/home/me/.cargo/bin/kabelsalat"),
            Path::new("/home/me/.local/state/kabelsalat/state.json"),
        )
    }

    #[test]
    fn unit_lives_under_the_user_manager_directory() {
        assert_eq!(
            unit_path(Path::new("/home/me/.config")),
            PathBuf::from("/home/me/.config/systemd/user/kabelsalat-resume.service")
        );
    }

    #[test]
    fn unit_runs_resume_once_after_the_user_manager_is_up() {
        let text = text();
        assert!(text.contains("[Unit]\n"), "{text}");
        assert!(text.contains("[Service]\nType=oneshot\n"), "{text}");
        assert!(
            text.contains("ExecStart=\"/home/me/.cargo/bin/kabelsalat\" resume\n"),
            "{text}"
        );
        assert!(text.contains("[Install]\nWantedBy=default.target\n"), "{text}");
    }

    #[test]
    fn unit_is_skipped_cleanly_while_the_state_file_is_unreachable() {
        // An encrypted or unmounted home has no state file yet; the condition
        // turns that into a skip rather than a failed unit.
        assert!(
            text().contains(
                "ConditionPathExists=/home/me/.local/state/kabelsalat/state.json\n"
            ),
            "{}",
            text()
        );
    }

    #[test]
    fn percent_signs_in_paths_are_escaped_from_specifier_expansion() {
        let text = unit_text(Path::new("/opt/100%/kabelsalat"), Path::new("/s/state.json"));
        assert!(text.contains("ExecStart=\"/opt/100%%/kabelsalat\" resume\n"), "{text}");
    }

    #[test]
    fn unavailable_wins_over_everything() {
        let expected = text();
        assert_eq!(
            classify(false, Some(&expected), &expected, true, LingerStatus::Enabled),
            Status::Unavailable
        );
    }

    #[test]
    fn no_unit_is_off() {
        let expected = text();
        assert_eq!(
            classify(true, None, &expected, false, LingerStatus::Enabled),
            Status::Off
        );
    }

    #[test]
    fn a_current_unit_that_is_not_enabled_is_off() {
        // `systemctl --user disable` by hand, or a copy left behind: either
        // way nothing runs at boot, which is what the label must say.
        let expected = text();
        assert_eq!(
            classify(true, Some(&expected), &expected, false, LingerStatus::Enabled),
            Status::Off
        );
    }

    #[test]
    fn a_current_enabled_unit_is_on_at_boot_only_with_lingering() {
        let expected = text();
        assert_eq!(
            classify(true, Some(&expected), &expected, true, LingerStatus::Enabled),
            Status::On { at_boot: true }
        );
        assert_eq!(
            classify(true, Some(&expected), &expected, true, LingerStatus::Disabled),
            Status::On { at_boot: false }
        );
        // Unknown lingering is not promised as "at boot".
        assert_eq!(
            classify(
                true,
                Some(&expected),
                &expected,
                true,
                LingerStatus::NotApplicable
            ),
            Status::On { at_boot: false }
        );
    }

    #[test]
    fn an_enabled_unit_with_other_text_needs_repair() {
        // The binary moved, or a newer build ships a new template.
        let expected = text();
        let old = expected.replace("/home/me/.cargo/bin", "/usr/local/bin");
        assert_eq!(
            classify(true, Some(&old), &expected, true, LingerStatus::Enabled),
            Status::Stale
        );
        // Enabled but the file is gone: the symlink dangles.
        assert_eq!(
            classify(true, None, &expected, true, LingerStatus::Enabled),
            Status::Stale
        );
    }

    #[test]
    fn a_disabled_unit_with_other_text_is_simply_off() {
        let expected = text();
        let old = expected.replace("/home/me/.cargo/bin", "/usr/local/bin");
        assert_eq!(
            classify(true, Some(&old), &expected, false, LingerStatus::Enabled),
            Status::Off
        );
    }

    #[test]
    fn labels_name_the_four_states_the_menu_shows() {
        assert_eq!(status_label(Status::Off), "Off");
        assert_eq!(status_label(Status::On { at_boot: true }), "On");
        assert_eq!(status_label(Status::On { at_boot: false }), "At login only");
        assert_eq!(status_label(Status::Stale), "Needs repair");
        assert_eq!(status_label(Status::Unavailable), "Unavailable");
    }

    #[test]
    fn responses_follow_the_state_table() {
        use Response::*;
        assert_eq!(responses(Status::Off), vec![Cancel, Install]);
        assert_eq!(responses(Status::On { at_boot: true }), vec![Close, Remove]);
        assert_eq!(
            responses(Status::On { at_boot: false }),
            vec![Close, Remove, EnableLinger]
        );
        assert_eq!(responses(Status::Stale), vec![Close, Repair]);
        assert_eq!(responses(Status::Unavailable), vec![Close]);
    }

    #[test]
    fn exactly_one_response_per_dialog_is_suggested_and_none_but_remove_destructive() {
        for status in [
            Status::Off,
            Status::On { at_boot: true },
            Status::On { at_boot: false },
            Status::Stale,
        ] {
            let suggested = responses(status)
                .iter()
                .filter(|r| r.appearance() == Appearance::Suggested)
                .count();
            assert!(suggested <= 1, "{status:?}");
            for response in responses(status) {
                assert_eq!(
                    response.appearance() == Appearance::Destructive,
                    response == Response::Remove,
                    "{response:?}"
                );
            }
        }
    }

    #[test]
    fn response_ids_are_distinct_and_round_trip() {
        use Response::*;
        let all = [Cancel, Close, Install, Remove, EnableLinger, Repair];
        for (i, a) in all.iter().enumerate() {
            assert_eq!(Response::from_id(a.id()), Some(*a));
            for b in &all[i + 1..] {
                assert_ne!(a.id(), b.id());
            }
        }
        assert_eq!(Response::from_id("nope"), None);
    }

    #[test]
    fn the_body_says_where_the_unit_goes_and_what_lingering_adds() {
        let path = Path::new("/home/me/.config/systemd/user/kabelsalat-resume.service");
        let off = dialog_body(Status::Off, path);
        assert!(off.contains("kabelsalat-resume.service"), "{off}");
        assert!(off.contains("lingering"), "{off}");
        let login_only = dialog_body(Status::On { at_boot: false }, path);
        assert!(login_only.contains("log in"), "{login_only}");
        let stale = dialog_body(Status::Stale, path);
        assert!(stale.contains("repair") || stale.contains("Repair"), "{stale}");
    }

    #[test]
    fn is_enabled_means_exactly_enabled() {
        assert!(parse_is_enabled("enabled\n"));
        assert!(!parse_is_enabled("disabled\n"));
        assert!(!parse_is_enabled("not-found\n"));
        assert!(!parse_is_enabled("enabled-runtime\n"));
        assert!(!parse_is_enabled(""));
    }
}
