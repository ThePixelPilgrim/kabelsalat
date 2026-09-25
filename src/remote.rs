//! Pure logic for remote groups: which ssh client and login mode to use, how
//! ssh and tmux failures read, and how remote commands are built and quoted.
//!
//! Like `state.rs` and `cli.rs` this module runs no processes and touches no
//! GTK; `remote_worker.rs` does the I/O. That split is what keeps it
//! unit-testable.

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
}
