//! Resume agent sessions at boot: the systemd user unit behind the primary
//! menu's "Resume agent sessions at boot…" entry, installed and removed from
//! the GUI (see docs/superpowers/specs/2026-10-02-boot-resume-design.md).
//!
//! The decisions — the unit's text, what state the installation is in, which
//! dialog responses that state offers — are pure functions over facts the
//! caller supplies, so they are unit-tested here. The thin `systemctl`
//! runners at the bottom are the only I/O, and like `tmuxctl` they never
//! panic: every fallible path returns a `Result`.

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
    fn response_ids_are_distinct() {
        use Response::*;
        let all = [Cancel, Close, Install, Remove, EnableLinger, Repair];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.id(), b.id());
            }
        }
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
