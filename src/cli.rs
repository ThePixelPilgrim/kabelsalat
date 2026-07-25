//! Pure command-line surface: argv in, decisions out.
//!
//! Like `state.rs` this module touches no GTK, no gio, no tmux and no
//! filesystem, which is what keeps it unit-testable. The gio plumbing that
//! feeds it lives in `control.rs`.

use std::path::PathBuf;

/// Success.
pub const EXIT_OK: u8 = 0;
/// No kabelsalat instance is running, so there is nothing to talk to.
pub const EXIT_NOT_RUNNING: u8 = 1;
/// Bad invocation: unknown flag, missing value, missing command.
pub const EXIT_USAGE: u8 = 2;
/// `--group` matched no group, or matched more than one.
pub const EXIT_GROUP: u8 = 3;

/// What an invocation asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    /// No arguments: start or raise the GUI, as before.
    Gui,
    Help,
    Groups,
    Run {
        /// A group uuid or name, resolved later against the live instance.
        group: String,
        /// `--cwd`, still possibly relative to the caller's directory.
        cwd: Option<PathBuf>,
        /// Everything after `--`, verbatim.
        argv: Vec<String>,
    },
}

impl Cli {
    /// Whether answering this needs the running GUI. `Gui` and `Help` are
    /// handled entirely in the calling process.
    pub fn needs_instance(&self) -> bool {
        matches!(self, Cli::Groups | Cli::Run { .. })
    }
}

/// One group as the CLI sees it: the stable uuid, the (possibly empty,
/// possibly duplicated) name, and how many tabs it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInfo {
    pub uuid: String,
    pub name: String,
    pub tabs: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NotFound,
    /// The name matched several groups; these are their uuids, in list order.
    Ambiguous(Vec<String>),
}

/// Find the group a `--group` value refers to.
///
/// A uuid always wins, so a group whose *name* happens to equal another
/// group's uuid can never shadow it. Names are matched exactly and
/// case-sensitively — no prefix or fuzzy matching, so `web` and `web-2` can
/// never be confused. Unnamed groups are reachable only by uuid: an empty
/// selector matches nothing rather than matching all of them.
pub fn resolve_group<'a>(
    groups: &'a [GroupInfo],
    selector: &str,
) -> Result<&'a GroupInfo, ResolveError> {
    if let Some(group) = groups.iter().find(|g| g.uuid == selector) {
        return Ok(group);
    }
    if selector.is_empty() {
        return Err(ResolveError::NotFound);
    }
    let matches: Vec<&GroupInfo> = groups.iter().filter(|g| g.name == selector).collect();
    match matches.as_slice() {
        [] => Err(ResolveError::NotFound),
        [only] => Ok(only),
        many => Err(ResolveError::Ambiguous(
            many.iter().map(|g| g.uuid.clone()).collect(),
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

pub fn help_text() -> &'static str {
    "kabelsalat — terminal groups with crash-safe tmux sessions

Usage:
  kabelsalat                                   Start or raise the GUI
  kabelsalat groups                            List groups: uuid, name, tab count
  kabelsalat run -g <group> [--cwd DIR] -- CMD [ARGS...]
                                               Run CMD in a new tab of <group>

Options for run:
  -g, --group <name|uuid>   Target group. A uuid always wins; otherwise the
                            name must match exactly and match only one group.
      --cwd <dir>           Working directory (default: the caller's).
      --                    Required. Everything after it is the command.

Exit codes:
  0 success   1 kabelsalat not running   2 usage error   3 group not found
"
}

/// Parse a full argv (including argv[0]).
pub fn parse(args: &[String]) -> Result<Cli, UsageError> {
    let rest = &args[1.min(args.len())..];
    let Some(command) = rest.first() else {
        return Ok(Cli::Gui);
    };

    match command.as_str() {
        "help" | "--help" | "-h" => Ok(Cli::Help),
        "groups" => {
            if rest.len() > 1 {
                return Err(UsageError(format!(
                    "groups takes no arguments (got '{}')",
                    rest[1]
                )));
            }
            Ok(Cli::Groups)
        }
        "run" => parse_run(&rest[1..]),
        other => Err(UsageError(format!("unknown command '{other}'"))),
    }
}

/// Flags of `run`, up to the mandatory `--`. The separator is required: without
/// it a command's own flags (`claude --model opus`) would be swallowed here.
fn parse_run(args: &[String]) -> Result<Cli, UsageError> {
    let mut group: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut i = 0;

    let argv = loop {
        let Some(arg) = args.get(i) else {
            return Err(UsageError(
                "missing '--' before the command (e.g. run -g web -- claude)".into(),
            ));
        };
        if arg == "--" {
            break args[i + 1..].to_vec();
        }
        let value = |i: usize| -> Result<String, UsageError> {
            match args.get(i + 1) {
                Some(v) if v != "--" => Ok(v.clone()),
                _ => Err(UsageError(format!("{} needs a value", args[i]))),
            }
        };
        match arg.as_str() {
            "--group" | "-g" => group = Some(value(i)?),
            "--cwd" => cwd = Some(PathBuf::from(value(i)?)),
            other => return Err(UsageError(format!("unknown option '{other}'"))),
        }
        i += 2;
    };

    let group = group.ok_or_else(|| UsageError("run needs --group".into()))?;
    if group.is_empty() {
        return Err(UsageError("--group must not be empty".into()));
    }
    if argv.is_empty() {
        return Err(UsageError("no command given after '--'".into()));
    }
    Ok(Cli::Run { group, cwd, argv })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        std::iter::once("kabelsalat")
            .chain(items.iter().copied())
            .map(String::from)
            .collect()
    }

    #[test]
    fn no_arguments_is_the_gui() {
        assert_eq!(parse(&args(&[])), Ok(Cli::Gui));
    }

    #[test]
    fn help_is_recognised_in_every_spelling() {
        for spelling in ["--help", "-h", "help"] {
            assert_eq!(parse(&args(&[spelling])), Ok(Cli::Help));
        }
    }

    #[test]
    fn groups_takes_no_arguments() {
        assert_eq!(parse(&args(&["groups"])), Ok(Cli::Groups));
        assert!(parse(&args(&["groups", "extra"])).is_err());
    }

    #[test]
    fn run_parses_group_and_command() {
        assert_eq!(
            parse(&args(&["run", "--group", "web", "--", "claude"])),
            Ok(Cli::Run {
                group: "web".into(),
                cwd: None,
                argv: vec!["claude".into()],
            })
        );
    }

    #[test]
    fn run_accepts_the_short_group_flag_and_cwd() {
        assert_eq!(
            parse(&args(&["run", "-g", "web", "--cwd", "/tmp", "--", "ls"])),
            Ok(Cli::Run {
                group: "web".into(),
                cwd: Some(PathBuf::from("/tmp")),
                argv: vec!["ls".into()],
            })
        );
    }

    #[test]
    fn flags_after_the_separator_belong_to_the_command() {
        assert_eq!(
            parse(&args(&[
                "run", "-g", "web", "--", "claude", "--cwd", "-g", "--help"
            ])),
            Ok(Cli::Run {
                group: "web".into(),
                cwd: None,
                argv: vec![
                    "claude".into(),
                    "--cwd".into(),
                    "-g".into(),
                    "--help".into()
                ],
            })
        );
    }

    #[test]
    fn run_requires_the_separator() {
        // Without `--`, a command's own flags would be eaten by our parser.
        assert!(parse(&args(&["run", "-g", "web", "claude"])).is_err());
    }

    #[test]
    fn run_rejects_missing_or_empty_pieces() {
        assert!(parse(&args(&["run", "--"])).is_err()); // no --group
        assert!(parse(&args(&["run", "-g", "web", "--"])).is_err()); // empty command
        assert!(parse(&args(&["run", "-g", "", "--", "ls"])).is_err()); // empty group
        assert!(parse(&args(&["run", "-g"])).is_err()); // flag without value
        assert!(parse(&args(&["run", "-g", "web", "--cwd", "--", "ls"])).is_err()); // ditto
    }

    #[test]
    fn unknown_commands_and_flags_are_usage_errors() {
        assert!(parse(&args(&["frobnicate"])).is_err());
        assert!(parse(&args(&["run", "--verbose", "-g", "web", "--", "ls"])).is_err());
    }

    fn sample_groups() -> Vec<GroupInfo> {
        vec![
            GroupInfo {
                uuid: "aaa-111".into(),
                name: "web".into(),
                tabs: 2,
            },
            GroupInfo {
                uuid: "bbb-222".into(),
                name: "api".into(),
                tabs: 1,
            },
            GroupInfo {
                uuid: "ccc-333".into(),
                name: "api".into(),
                tabs: 3,
            },
            GroupInfo {
                uuid: "ddd-444".into(),
                name: String::new(),
                tabs: 1,
            },
        ]
    }

    #[test]
    fn resolves_a_uuid() {
        let groups = sample_groups();
        assert_eq!(resolve_group(&groups, "ccc-333").unwrap().uuid, "ccc-333");
    }

    #[test]
    fn resolves_a_unique_name() {
        let groups = sample_groups();
        assert_eq!(resolve_group(&groups, "web").unwrap().uuid, "aaa-111");
    }

    #[test]
    fn a_duplicate_name_is_ambiguous_and_lists_candidates() {
        let groups = sample_groups();
        match resolve_group(&groups, "api") {
            Err(ResolveError::Ambiguous(uuids)) => {
                assert_eq!(uuids, vec!["bbb-222".to_string(), "ccc-333".to_string()]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_selector_is_not_found() {
        let groups = sample_groups();
        assert_eq!(resolve_group(&groups, "nope"), Err(ResolveError::NotFound));
    }

    #[test]
    fn an_unnamed_group_is_reachable_only_by_uuid() {
        let groups = sample_groups();
        assert_eq!(resolve_group(&groups, "ddd-444").unwrap().tabs, 1);
        assert_eq!(resolve_group(&groups, ""), Err(ResolveError::NotFound));
    }

    #[test]
    fn a_uuid_beats_a_name_that_collides_with_it() {
        let groups = vec![
            GroupInfo {
                uuid: "xyz".into(),
                name: "other".into(),
                tabs: 1,
            },
            GroupInfo {
                uuid: "qqq".into(),
                name: "xyz".into(),
                tabs: 1,
            },
        ];
        assert_eq!(resolve_group(&groups, "xyz").unwrap().uuid, "xyz");
    }

    #[test]
    fn name_matching_is_case_sensitive() {
        let groups = sample_groups();
        assert_eq!(resolve_group(&groups, "Web"), Err(ResolveError::NotFound));
    }
}
