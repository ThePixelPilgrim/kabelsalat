//! Pure command-line surface: argv in, decisions out.
//!
//! Like `state.rs` this module touches no GTK, no gio, no tmux and no
//! filesystem, which is what keeps it unit-testable. The gio plumbing that
//! feeds it lives in `control.rs`.

use std::path::{Path, PathBuf};

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
        /// `--create`: when the selector matches nothing, make a group with
        /// that name instead of failing.
        create: bool,
        /// `--cwd`, still possibly relative to the caller's directory.
        cwd: Option<PathBuf>,
        /// Everything after `--`, verbatim.
        argv: Vec<String>,
    },
    /// Give an existing group a new name.
    Rename {
        /// A group uuid or name, resolved like `run`'s `--group`.
        group: String,
        /// The new name, already trimmed and known to be non-empty.
        name: String,
    },
}

impl Cli {
    /// Whether answering this needs the running GUI. `Gui` and `Help` are
    /// handled entirely in the calling process.
    pub fn needs_instance(&self) -> bool {
        matches!(self, Cli::Groups | Cli::Run { .. } | Cli::Rename { .. })
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
  kabelsalat                                   Start the GUI, or activate the running instance
  kabelsalat groups                            List groups: uuid, name, tab count
  kabelsalat run -g <group> [--create] [--cwd DIR] -- CMD [ARGS...]
                                               Run CMD in a new tab of <group>
  kabelsalat rename <group> <new-name>         Rename an existing group

Options for run:
  -g, --group <name|uuid>   Target group. A uuid always wins; otherwise the
                            name must match exactly and match only one group.
      --create              When nothing matches, create a group named
                            <group> and put the tab in it.
      --cwd <dir>           Working directory (default: the caller's).
      --                    Required. Everything after it is the command.

Exit codes:
  0 success   1 kabelsalat not running   2 usage error
  3 group not found, ambiguous, or the new name is already taken
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
        "rename" => parse_rename(&rest[1..]),
        other => Err(UsageError(format!("unknown command '{other}'"))),
    }
}

/// Flags of `run`, up to the mandatory `--`. The separator is required: without
/// it a command's own flags (`claude --model opus`) would be swallowed here.
fn parse_run(args: &[String]) -> Result<Cli, UsageError> {
    let mut group: Option<String> = None;
    let mut cwd: Option<PathBuf> = None;
    let mut create = false;
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
            // A boolean flag consumes one argument, not two.
            "--create" => {
                create = true;
                i += 1;
                continue;
            }
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
    // With --create the selector becomes the new group's name, so it has to
    // survive `kabelsalat groups`. A plain lookup is left alone: it can only
    // fail to match.
    if create {
        check_name_chars(&group)?;
    }
    Ok(Cli::Run {
        group,
        create,
        cwd,
        argv,
    })
}

/// A group name the CLI sets must stay on one `kabelsalat groups` line and
/// keep its three tab-separated fields, so control characters — tabs and
/// newlines above all — are a usage error. The GUI's single-line entry cannot
/// produce them; only these paths could.
fn check_name_chars(name: &str) -> Result<(), UsageError> {
    if name.chars().any(char::is_control) {
        return Err(UsageError(
            "a group name must not contain control characters".into(),
        ));
    }
    Ok(())
}

/// `rename <group> <new-name>`: exactly two positional arguments, no flags.
/// The new name is trimmed, and must not be empty afterwards.
fn parse_rename(args: &[String]) -> Result<Cli, UsageError> {
    let (Some(group), Some(name)) = (args.first(), args.get(1)) else {
        return Err(UsageError(
            "rename needs a group and a new name (e.g. rename web frontend)".into(),
        ));
    };
    if let Some(extra) = args.get(2) {
        return Err(UsageError(format!(
            "rename takes exactly two arguments (got '{extra}')"
        )));
    }
    if group.is_empty() {
        return Err(UsageError("rename needs a non-empty group".into()));
    }
    let name = name.trim();
    if name.is_empty() {
        return Err(UsageError("the new name must not be empty".into()));
    }
    check_name_chars(name)?;
    Ok(Cli::Rename {
        group: group.clone(),
        name: name.to_string(),
    })
}

/// Where a spawned tab goes: an existing group, or one to be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupTarget {
    /// A group that already exists, by uuid.
    Existing(String),
    /// No group matched and `--create` was given: make one with this name.
    Create { name: String },
}

/// What the GUI is asked to do once the invocation validated. The tab's own
/// uuid is minted by the caller, not here, because that needs glib.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Spawn {
        group: GroupTarget,
        cwd: PathBuf,
        argv: Vec<String>,
    },
    Rename {
        group_uuid: String,
        name: String,
    },
}

/// The complete result of an invocation: what to print, what to exit with,
/// and — only when everything validated — what to ask the GUI to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub code: u8,
    pub action: Option<Action>,
}

impl Outcome {
    fn ok(stdout: String) -> Self {
        Self {
            stdout,
            stderr: String::new(),
            code: EXIT_OK,
            action: None,
        }
    }

    fn fail(code: u8, stderr: String) -> Self {
        Self {
            stdout: String::new(),
            stderr,
            code,
            action: None,
        }
    }
}

/// Decide what an invocation does, given the live group list and the calling
/// process's working directory. Pure: the caller performs the printing and
/// the spawning.
pub fn dispatch(cli: &Cli, groups: &[GroupInfo], caller_cwd: &Path) -> Outcome {
    match cli {
        Cli::Gui => Outcome::ok(String::new()),
        Cli::Help => Outcome::ok(help_text().to_string()),
        Cli::Groups => Outcome::ok(
            groups
                .iter()
                .map(|g| format!("{}\t{}\t{}\n", g.uuid, g.name, g.tabs))
                .collect(),
        ),
        Cli::Run {
            group,
            create,
            cwd,
            argv,
        } => {
            // `join` with an absolute path replaces the base, so this handles
            // both absolute and relative --cwd values.
            let cwd = match cwd {
                Some(dir) => caller_cwd.join(dir),
                None => caller_cwd.to_path_buf(),
            };
            let target = match resolve_group(groups, group) {
                Ok(found) => GroupTarget::Existing(found.uuid.clone()),
                // A selector that looks like a uuid is not special-cased: with
                // --create it simply becomes the new group's name.
                Err(ResolveError::NotFound) if *create => GroupTarget::Create {
                    name: group.clone(),
                },
                Err(err) => return resolve_failure(group, err),
            };
            Outcome {
                stdout: String::new(),
                stderr: String::new(),
                code: EXIT_OK,
                action: Some(Action::Spawn {
                    group: target,
                    cwd,
                    argv: argv.clone(),
                }),
            }
        }
        Cli::Rename { group, name } => {
            let target = match resolve_group(groups, group) {
                Ok(found) => found,
                Err(err) => return resolve_failure(group, err),
            };
            // A rename that changes nothing succeeds even when the GUI has
            // left a second group with that same name around: there is
            // nothing to refuse, so this is checked before the clash.
            if target.name == *name {
                return Outcome::ok(String::new());
            }
            // Exact and case-sensitive, like name resolution. The target
            // itself does not count as a clash.
            if let Some(clash) = groups
                .iter()
                .find(|g| g.uuid != target.uuid && g.name == *name)
            {
                return Outcome::fail(
                    EXIT_GROUP,
                    format!("a group named '{name}' already exists ({})\n", clash.uuid),
                );
            }
            Outcome {
                stdout: String::new(),
                stderr: String::new(),
                code: EXIT_OK,
                action: Some(Action::Rename {
                    group_uuid: target.uuid.clone(),
                    name: name.clone(),
                }),
            }
        }
    }
}

/// The shared `run`/`rename` wording for a selector that matched nothing or
/// matched too much.
fn resolve_failure(selector: &str, err: ResolveError) -> Outcome {
    match err {
        ResolveError::NotFound => Outcome::fail(
            EXIT_GROUP,
            format!("no group matching '{selector}'; try `kabelsalat groups`\n"),
        ),
        ResolveError::Ambiguous(uuids) => Outcome::fail(
            EXIT_GROUP,
            format!(
                "'{selector}' matches {} groups; use one of these uuids instead:\n{}",
                uuids.len(),
                uuids.iter().map(|u| format!("  {u}\n")).collect::<String>()
            ),
        ),
    }
}

/// Initial tab title for a command: its basename, until the program sets its
/// own title via OSC.
pub fn command_title(argv: &[String]) -> String {
    let Some(first) = argv.first() else {
        return "Terminal".to_string();
    };
    Path::new(first)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| first.clone())
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
                create: false,
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
                create: false,
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
                create: false,
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

    fn run(group: &str, cwd: Option<&str>, argv: &[&str]) -> Cli {
        Cli::Run {
            group: group.into(),
            create: false,
            cwd: cwd.map(PathBuf::from),
            argv: argv.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn run_create(group: &str, argv: &[&str]) -> Cli {
        Cli::Run {
            group: group.into(),
            create: true,
            cwd: None,
            argv: argv.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn rename(group: &str, name: &str) -> Cli {
        Cli::Rename {
            group: group.into(),
            name: name.into(),
        }
    }

    fn spawn_of(out: &Outcome) -> (&GroupTarget, &PathBuf, &Vec<String>) {
        match out.action.as_ref().expect("an action") {
            Action::Spawn { group, cwd, argv } => (group, cwd, argv),
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn groups_prints_tab_separated_lines() {
        let out = dispatch(&Cli::Groups, &sample_groups(), Path::new("/home/u"));
        assert_eq!(
            out.stdout,
            "aaa-111\tweb\t2\nbbb-222\tapi\t1\nccc-333\tapi\t3\nddd-444\t\t1\n"
        );
        assert_eq!(out.code, EXIT_OK);
        assert!(out.action.is_none());
    }

    #[test]
    fn groups_with_no_groups_prints_nothing_and_succeeds() {
        let out = dispatch(&Cli::Groups, &[], Path::new("/home/u"));
        assert_eq!(out.stdout, "");
        assert_eq!(out.code, EXIT_OK);
    }

    #[test]
    fn run_produces_a_spawn_request_with_the_callers_cwd() {
        let cli = run("web", None, &["claude"]);
        let out = dispatch(&cli, &sample_groups(), Path::new("/home/u/proj"));
        let (group, cwd, argv) = spawn_of(&out);
        assert_eq!(*group, GroupTarget::Existing("aaa-111".into()));
        assert_eq!(*cwd, PathBuf::from("/home/u/proj"));
        assert_eq!(*argv, vec!["claude".to_string()]);
        assert_eq!(out.code, EXIT_OK);
    }

    #[test]
    fn an_absolute_cwd_flag_replaces_the_callers_cwd() {
        let cli = run("web", Some("/srv/app"), &["ls"]);
        let out = dispatch(&cli, &sample_groups(), Path::new("/home/u/proj"));
        assert_eq!(*spawn_of(&out).1, PathBuf::from("/srv/app"));
    }

    #[test]
    fn a_relative_cwd_flag_is_resolved_against_the_caller() {
        let cli = run("web", Some("sub/dir"), &["ls"]);
        let out = dispatch(&cli, &sample_groups(), Path::new("/home/u/proj"));
        assert_eq!(*spawn_of(&out).1, PathBuf::from("/home/u/proj/sub/dir"));
    }

    #[test]
    fn run_on_an_unknown_group_fails_without_spawning() {
        let cli = run("nope", None, &["ls"]);
        let out = dispatch(&cli, &sample_groups(), Path::new("/home/u"));
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.action.is_none());
        assert!(out.stderr.contains("nope"));
    }

    #[test]
    fn run_on_an_ambiguous_group_names_the_candidate_uuids() {
        let cli = run("api", None, &["ls"]);
        let out = dispatch(&cli, &sample_groups(), Path::new("/home/u"));
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.action.is_none());
        assert!(out.stderr.contains("bbb-222"), "stderr was: {}", out.stderr);
        assert!(out.stderr.contains("ccc-333"), "stderr was: {}", out.stderr);
    }

    #[test]
    fn help_goes_to_stdout_and_succeeds() {
        let out = dispatch(&Cli::Help, &[], Path::new("/home/u"));
        assert!(out.stdout.starts_with("kabelsalat"));
        assert_eq!(out.code, EXIT_OK);
    }

    #[test]
    fn run_parses_the_create_flag() {
        assert_eq!(
            parse(&args(&["run", "-g", "newproj", "--create", "--", "claude"])),
            Ok(Cli::Run {
                group: "newproj".into(),
                create: true,
                cwd: None,
                argv: vec!["claude".into()],
            })
        );
    }

    #[test]
    fn create_reuses_a_unique_match() {
        let out = dispatch(
            &run_create("web", &["ls"]),
            &sample_groups(),
            Path::new("/w"),
        );
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(*spawn_of(&out).0, GroupTarget::Existing("aaa-111".into()));
    }

    #[test]
    fn create_makes_a_new_group_when_nothing_matches() {
        let out = dispatch(
            &run_create("newproj", &["claude"]),
            &sample_groups(),
            Path::new("/w"),
        );
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(
            *spawn_of(&out).0,
            GroupTarget::Create {
                name: "newproj".into()
            }
        );
    }

    #[test]
    fn create_still_fails_on_an_ambiguous_name() {
        let out = dispatch(
            &run_create("api", &["ls"]),
            &sample_groups(),
            Path::new("/w"),
        );
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.action.is_none());
        assert!(out.stderr.contains("bbb-222"), "stderr was: {}", out.stderr);
    }

    #[test]
    fn rename_parses_two_arguments() {
        assert_eq!(
            parse(&args(&["rename", "web", "  frontend  "])),
            Ok(Cli::Rename {
                group: "web".into(),
                name: "frontend".into(),
            })
        );
    }

    #[test]
    fn rename_rejects_missing_or_empty_name() {
        assert!(parse(&args(&["rename"])).is_err());
        assert!(parse(&args(&["rename", "web"])).is_err());
        assert!(parse(&args(&["rename", "web", ""])).is_err());
        assert!(parse(&args(&["rename", "web", "   "])).is_err());
        assert!(parse(&args(&["rename", "web", "a", "b"])).is_err());
        assert!(parse(&args(&["rename", "", "a"])).is_err());
    }

    #[test]
    fn rename_refuses_a_name_another_group_uses() {
        let out = dispatch(&rename("web", "api"), &sample_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.action.is_none());
        assert_eq!(out.stderr, "a group named 'api' already exists (bbb-222)\n");
    }

    #[test]
    fn rename_to_the_current_name_is_a_noop_success() {
        let out = dispatch(&rename("web", "web"), &sample_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stdout, "");
        assert_eq!(out.stderr, "");
        assert!(out.action.is_none());
    }

    #[test]
    fn renaming_a_group_to_its_own_name_is_a_noop_even_with_a_twin() {
        // bbb-222 and ccc-333 are both called "api" (the GUI allows that).
        // Renaming one to "api" changes nothing, so there is nothing to
        // refuse — the clash rule must not fire on the target's twin.
        let out = dispatch(&rename("bbb-222", "api"), &sample_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stderr, "");
        assert!(out.action.is_none());
    }

    #[test]
    fn a_name_with_control_characters_is_a_usage_error() {
        // These would break the one-line, tab-separated `groups` output.
        assert!(parse(&args(&["rename", "web", "a\nb"])).is_err());
        assert!(parse(&args(&["rename", "web", "a\tb"])).is_err());
        assert!(parse(&args(&["run", "-g", "a\nb", "--create", "--", "ls"])).is_err());
        // Without --create the selector is only ever looked up, never stored.
        assert!(parse(&args(&["run", "-g", "a\nb", "--", "ls"])).is_ok());
    }

    #[test]
    fn rename_targets_an_unnamed_group_by_uuid() {
        let out = dispatch(
            &rename("ddd-444", "scratch"),
            &sample_groups(),
            Path::new("/w"),
        );
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stdout, "");
        assert_eq!(
            out.action,
            Some(Action::Rename {
                group_uuid: "ddd-444".into(),
                name: "scratch".into(),
            })
        );
    }

    #[test]
    fn rename_on_an_unknown_group_fails() {
        let out = dispatch(&rename("nope", "x"), &sample_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.action.is_none());
        assert!(out.stderr.contains("nope"));

        let ambiguous = dispatch(&rename("api", "x"), &sample_groups(), Path::new("/w"));
        assert_eq!(ambiguous.code, EXIT_GROUP);
        assert!(ambiguous.action.is_none());
        assert!(ambiguous.stderr.contains("ccc-333"));
    }

    #[test]
    fn a_title_is_the_basename_of_the_command() {
        assert_eq!(command_title(&["claude".to_string()]), "claude");
        assert_eq!(command_title(&["/usr/bin/htop".to_string()]), "htop");
        assert_eq!(command_title(&[]), "Terminal");
    }
}
