# CLI Interface and Agent Skill Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give kabelsalat a two-command CLI (`groups`, `run`) so an external agent can list groups and launch a command in a new tab of a named group, plus a Claude skill documenting it.

**Architecture:** The app already owns `de.nereide.kabelsalat` on the session bus for GTK single-instance behaviour but discards a second launch's argv. We opt into `ApplicationFlags::HANDLES_COMMAND_LINE` so GLib forwards argv/cwd to the running instance and routes output and exit status back. All decision logic lives in a pure `src/cli.rs` (parse → resolve → dispatch), so everything except the D-Bus plumbing is unit-testable. `src/control.rs` holds the GApplication glue and a read-only group snapshot the handler can consult synchronously without deadlocking against the relm4 component.

**Tech Stack:** Rust 2024, relm4 0.11, gtk4 / libadwaita 0.9, gio 0.22, glib 0.22, vte4 0.10. No new dependencies.

## Global Constraints

- **No new crates.** Argument parsing is hand-rolled; `clap` is explicitly rejected.
- `src/cli.rs` is **pure**: no GTK, no gio, no tmux, no file I/O, no process spawning. Same rule as `src/state.rs`. It is where the unit tests live.
- `src/tmuxctl.rs` never panics: no `unwrap`/`expect` on tmux interaction, every fallible path returns `Result`.
- The app must keep working when tmux is missing — the no-tmux spawn path gets the same command support as the tmux path.
- Exit codes are fixed: `0` success, `1` kabelsalat not running, `2` usage error, `3` group not found or ambiguous.
- Group targeting uses `uuid` (stable, persisted) or `name`. **Never `SavedGroup.id`** — it is process-local and reused after a delete.
- A CLI-created tab must not activate itself, must not raise or focus the window, must not change the active group, and must not open, close or otherwise touch the group's browser pane.
- `cargo fmt`, `cargo clippy` and `cargo test` must all pass before the work is called done. There is no CI.

---

### Task 0: Branch

**Files:** none

- [ ] **Step 1: Create the working branch**

Branch from the current `browser-pane` HEAD (that is where the spec was committed and where the browser-pane code the app compiles against lives). Do not branch from `main`, and do not merge or push anything.

```bash
cd /home/christoph/Projects/kabelsalat
git switch -c cli-interface
git log --oneline -1
```

Expected: the `cli-interface` branch at commit `af1d905 Spec: CLI interface and agent skill`.

---

### Task 1: Pure argument parsing

**Files:**
- Create: `src/cli.rs`
- Modify: `src/lib.rs:3-6` (add `mod cli;`)
- Test: inside `src/cli.rs` (`#[cfg(test)] mod tests`, matching the `state.rs` / `tmuxctl.rs` convention — this codebase has no `tests/` directory)

**Interfaces:**
- Produces: `pub enum Cli { Gui, Help, Groups, Run { group: String, cwd: Option<PathBuf>, argv: Vec<String> } }`, `pub struct UsageError(pub String)`, `pub fn parse(args: &[String]) -> Result<Cli, UsageError>`, `pub fn help_text() -> &'static str`, `pub const EXIT_OK/EXIT_NOT_RUNNING/EXIT_USAGE/EXIT_GROUP: u8`.

- [ ] **Step 1: Write the failing tests**

Create `src/cli.rs` with only this test module plus the imports it needs (the implementation comes in Step 3):

```rust
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
            parse(&args(&["run", "-g", "web", "--", "claude", "--cwd", "-g", "--help"])),
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
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cli::`
Expected: compile failure — `cannot find function 'parse' in this scope`, `cannot find type 'Cli'`.

- [ ] **Step 3: Write the implementation**

Put this **above** the test module in `src/cli.rs`:

```rust
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
```

Then register the module in `src/lib.rs`, keeping the existing `mod app;` private-module style:

```rust
mod app;
pub mod browser;
mod cli;
pub mod state;
pub mod tmuxctl;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib cli::`
Expected: PASS, 8 tests. (`mod cli;` being unused so far may produce dead-code warnings; those clear in Task 7.)

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs src/lib.rs
git commit -m "Add pure argv parsing for the CLI"
```

---

### Task 2: Group resolution

**Files:**
- Modify: `src/cli.rs`
- Test: `src/cli.rs` test module

**Interfaces:**
- Consumes: nothing from Task 1 beyond living in the same file.
- Produces: `pub struct GroupInfo { pub uuid: String, pub name: String, pub tabs: usize }`, `pub enum ResolveError { NotFound, Ambiguous(Vec<String>) }`, `pub fn resolve_group<'a>(groups: &'a [GroupInfo], selector: &str) -> Result<&'a GroupInfo, ResolveError>`.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `src/cli.rs`:

```rust
    fn sample_groups() -> Vec<GroupInfo> {
        vec![
            GroupInfo { uuid: "aaa-111".into(), name: "web".into(), tabs: 2 },
            GroupInfo { uuid: "bbb-222".into(), name: "api".into(), tabs: 1 },
            GroupInfo { uuid: "ccc-333".into(), name: "api".into(), tabs: 3 },
            GroupInfo { uuid: "ddd-444".into(), name: String::new(), tabs: 1 },
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
            GroupInfo { uuid: "xyz".into(), name: "other".into(), tabs: 1 },
            GroupInfo { uuid: "qqq".into(), name: "xyz".into(), tabs: 1 },
        ];
        assert_eq!(resolve_group(&groups, "xyz").unwrap().uuid, "xyz");
    }

    #[test]
    fn name_matching_is_case_sensitive() {
        let groups = sample_groups();
        assert_eq!(resolve_group(&groups, "Web"), Err(ResolveError::NotFound));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cli::`
Expected: compile failure — `cannot find type 'GroupInfo'`, `cannot find function 'resolve_group'`.

- [ ] **Step 3: Write the implementation**

Add to `src/cli.rs`, after the `Cli` definitions:

```rust
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib cli::`
Expected: PASS, 15 tests.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs
git commit -m "Resolve a CLI group selector to a group"
```

---

### Task 3: Dispatch — turning a parsed command into an outcome

**Files:**
- Modify: `src/cli.rs`
- Test: `src/cli.rs` test module

**Interfaces:**
- Consumes: `Cli`, `GroupInfo`, `resolve_group`, the `EXIT_*` constants from Tasks 1-2.
- Produces: `pub struct SpawnRequest { pub group_uuid: String, pub cwd: PathBuf, pub argv: Vec<String> }`, `pub struct Outcome { pub stdout: String, pub stderr: String, pub code: u8, pub spawn: Option<SpawnRequest> }`, `pub fn dispatch(cli: &Cli, groups: &[GroupInfo], caller_cwd: &Path) -> Outcome`, `pub fn command_title(argv: &[String]) -> String`.

This is the whole decision layer. `control.rs` in Task 7 only moves bytes between gio and this function, which is why almost nothing is left untested at the end.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `src/cli.rs`:

```rust
    fn run(group: &str, cwd: Option<&str>, argv: &[&str]) -> Cli {
        Cli::Run {
            group: group.into(),
            cwd: cwd.map(PathBuf::from),
            argv: argv.iter().map(|s| s.to_string()).collect(),
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
        assert!(out.spawn.is_none());
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
        let spawn = out.spawn.expect("spawn request");
        assert_eq!(spawn.group_uuid, "aaa-111");
        assert_eq!(spawn.cwd, PathBuf::from("/home/u/proj"));
        assert_eq!(spawn.argv, vec!["claude".to_string()]);
        assert_eq!(out.code, EXIT_OK);
    }

    #[test]
    fn an_absolute_cwd_flag_replaces_the_callers_cwd() {
        let cli = run("web", Some("/srv/app"), &["ls"]);
        let out = dispatch(&cli, &sample_groups(), Path::new("/home/u/proj"));
        assert_eq!(out.spawn.unwrap().cwd, PathBuf::from("/srv/app"));
    }

    #[test]
    fn a_relative_cwd_flag_is_resolved_against_the_caller() {
        let cli = run("web", Some("sub/dir"), &["ls"]);
        let out = dispatch(&cli, &sample_groups(), Path::new("/home/u/proj"));
        assert_eq!(out.spawn.unwrap().cwd, PathBuf::from("/home/u/proj/sub/dir"));
    }

    #[test]
    fn run_on_an_unknown_group_fails_without_spawning() {
        let cli = run("nope", None, &["ls"]);
        let out = dispatch(&cli, &sample_groups(), Path::new("/home/u"));
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.spawn.is_none());
        assert!(out.stderr.contains("nope"));
    }

    #[test]
    fn run_on_an_ambiguous_group_names_the_candidate_uuids() {
        let cli = run("api", None, &["ls"]);
        let out = dispatch(&cli, &sample_groups(), Path::new("/home/u"));
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.spawn.is_none());
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
    fn a_title_is_the_basename_of_the_command() {
        assert_eq!(command_title(&["claude".to_string()]), "claude");
        assert_eq!(command_title(&["/usr/bin/htop".to_string()]), "htop");
        assert_eq!(command_title(&[]), "Terminal");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cli::`
Expected: compile failure — `cannot find function 'dispatch'`, `cannot find function 'command_title'`.

- [ ] **Step 3: Write the implementation**

Change the import at the top of `src/cli.rs` to `use std::path::{Path, PathBuf};`, then add:

```rust
/// A validated `run`: everything `app.rs` needs to create the tab. The tab's
/// own uuid is minted by the caller, not here, because that needs glib.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    pub group_uuid: String,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
}

/// The complete result of an invocation: what to print, what to exit with,
/// and — only when everything validated — what to ask the GUI to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub code: u8,
    pub spawn: Option<SpawnRequest>,
}

impl Outcome {
    fn ok(stdout: String) -> Self {
        Self { stdout, stderr: String::new(), code: EXIT_OK, spawn: None }
    }

    fn fail(code: u8, stderr: String) -> Self {
        Self { stdout: String::new(), stderr, code, spawn: None }
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
        Cli::Run { group, cwd, argv } => match resolve_group(groups, group) {
            Ok(found) => Outcome {
                stdout: String::new(),
                stderr: String::new(),
                code: EXIT_OK,
                spawn: Some(SpawnRequest {
                    group_uuid: found.uuid.clone(),
                    // `join` with an absolute path replaces the base, so this
                    // handles both absolute and relative --cwd values.
                    cwd: match cwd {
                        Some(dir) => caller_cwd.join(dir),
                        None => caller_cwd.to_path_buf(),
                    },
                    argv: argv.clone(),
                }),
            },
            Err(ResolveError::NotFound) => Outcome::fail(
                EXIT_GROUP,
                format!("no group matching '{group}'; try `kabelsalat groups`\n"),
            ),
            Err(ResolveError::Ambiguous(uuids)) => Outcome::fail(
                EXIT_GROUP,
                format!(
                    "'{group}' matches {} groups; use one of these uuids instead:\n{}",
                    uuids.len(),
                    uuids
                        .iter()
                        .map(|u| format!("  {u}\n"))
                        .collect::<String>()
                ),
            ),
        },
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib cli::`
Expected: PASS, 24 tests.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs
git commit -m "Turn a parsed CLI command into a printable outcome"
```

---

### Task 4: tmux can run an arbitrary command

**Files:**
- Modify: `src/tmuxctl.rs:288-313` (`spawn_argv`), `src/app.rs:2480` (the one call site)
- Test: `src/tmuxctl.rs` test module

**Interfaces:**
- Produces: `pub fn spawn_argv(&self, uuid: &str, cwd: Option<&Path>, command: Option<&[String]>) -> Vec<String>` (signature change), `pub(crate) fn shell_quote_argv(argv: &[String]) -> String`.

`tmux new-session` takes its command as a *string* and re-joins multiple arguments with spaces, losing word boundaries. So we do the quoting ourselves and hand tmux exactly one already-quoted string.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `src/tmuxctl.rs`:

```rust
    #[test]
    fn spawn_argv_with_a_command_replaces_the_shell() {
        let dir = temp_dir("argv-cmd");
        let ctl = test_ctl(&dir);
        let command = vec!["claude".to_string(), "--model".to_string(), "opus".to_string()];
        let argv = ctl.spawn_argv("1234-abcd", None, Some(&command));
        assert_eq!(&argv[5..9], ["new-session", "-A", "-s", "ks-1234-abcd"]);
        assert_eq!(argv[9], "claude --model opus");
        assert_eq!(argv.len(), 10);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn shell_quoting_survives_spaces_quotes_and_expansion() {
        // tmux hands the string to sh, so anything the shell would interpret
        // has to be quoted here or the command silently changes meaning.
        assert_eq!(shell_quote_argv(&["ls".to_string()]), "ls");
        assert_eq!(
            shell_quote_argv(&["echo".to_string(), "two words".to_string()]),
            "echo 'two words'"
        );
        assert_eq!(
            shell_quote_argv(&["echo".to_string(), "$HOME".to_string()]),
            "echo '$HOME'"
        );
        assert_eq!(
            shell_quote_argv(&["echo".to_string(), "it's".to_string()]),
            r#"echo 'it'\''s'"#
        );
        assert_eq!(
            shell_quote_argv(&["echo".to_string(), "a;rm -rf /".to_string()]),
            "echo 'a;rm -rf /'"
        );
        assert_eq!(shell_quote_argv(&["".to_string()]), "''");
        assert_eq!(
            shell_quote_argv(&["/usr/bin/env".to_string(), "PATH=/x:/y".to_string()]),
            "/usr/bin/env PATH=/x:/y"
        );
    }

    #[test]
    fn spawn_argv_without_a_command_is_unchanged() {
        // Regression guard: the GUI's own new-tab path must keep spawning $SHELL.
        let dir = temp_dir("argv-nocmd");
        let ctl = test_ctl(&dir);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let argv = ctl.spawn_argv("1234-abcd", None, None);
        assert_eq!(argv[9], shell);
        assert_eq!(ctl.spawn_argv("1234-abcd", None, Some(&[])), argv);
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib tmuxctl::`
Expected: compile failure — `this method takes 2 arguments but 3 arguments were supplied`, `cannot find function 'shell_quote_argv'`.

- [ ] **Step 3: Write the implementation**

Replace `spawn_argv` in `src/tmuxctl.rs` (lines 288-313) with:

```rust
    /// Argv for spawning (or reattaching) the backing session of a tab:
    /// `tmux -S <sock> -f <conf> new-session -A [-c <cwd>] -s ks-<uuid> <cmd>`,
    /// where `<cmd>` is `$SHELL` unless `command` overrides it.
    ///
    /// When `cwd` is set, `-c <dir>` sets the new session's working directory so
    /// a fresh shell starts there. `new-session -A` ignores `-c` when the
    /// session already exists, so reattach/restore paths pass `None`.
    ///
    /// `command` is what `kabelsalat run` passes. It arrives as argv but tmux
    /// wants a single shell-command string, so it is quoted and joined here
    /// rather than handed to tmux as separate words (tmux would re-join them
    /// on spaces and lose the original boundaries).
    pub fn spawn_argv(
        &self,
        uuid: &str,
        cwd: Option<&Path>,
        command: Option<&[String]>,
    ) -> Vec<String> {
        let mut argv = vec![
            "tmux".into(),
            "-S".into(),
            self.socket.to_string_lossy().into_owned(),
            "-f".into(),
            self.conf.to_string_lossy().into_owned(),
            "new-session".into(),
            "-A".into(),
        ];
        if let Some(dir) = cwd {
            argv.push("-c".into());
            argv.push(dir.to_string_lossy().into_owned());
        }
        argv.push("-s".into());
        argv.push(format!("{SESSION_PREFIX}{uuid}"));
        argv.push(match command {
            Some(command) if !command.is_empty() => shell_quote_argv(command),
            _ => std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()),
        });
        argv
    }
```

Add these free functions at module level in `src/tmuxctl.rs` (next to the other free helpers such as `parse_file_uri`, i.e. outside the `impl TmuxCtl` block):

```rust
/// Join argv into the single shell-command string tmux expects, quoting every
/// word so spaces, quotes, `$` and `;` reach the program instead of the shell.
pub(crate) fn shell_quote_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// POSIX single-quote a word: wrap it, and end/escape/reopen for each `'`.
/// Words made only of characters no shell treats specially are left bare so
/// the common case stays readable in `tmux list-panes` output.
fn shell_quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%_+=:,./-".contains(&b));
    if safe {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}
```

Update the single production call site, `src/app.rs:2480`, to keep today's behaviour for now:

```rust
    let argv = ctl.spawn_argv(uuid, cwd, None);
```

And the two existing tests in `src/tmuxctl.rs` that call `spawn_argv` (around lines 593 and 608) plus the integration-style one around line 887 — add the third argument `None` to each.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib tmuxctl::`
Expected: PASS. `spawn_argv_shape` and `spawn_argv_with_cwd_inserts_c_flag` still pass unchanged apart from the extra argument.

- [ ] **Step 5: Commit**

```bash
git add src/tmuxctl.rs src/app.rs
git commit -m "Let a tmux-backed tab run an arbitrary command"
```

---

### Task 5: Thread the command through the tab spawn path

**Files:**
- Modify: `src/app.rs:1491-1548` (`add_tab`), `src/app.rs:2475-2517` (`spawn_backing`, `spawn_shell`), and the four `add_tab` call sites at `src/app.rs:885`, `:904`, `:1466`, plus `spawn_backing` at `:641` and `spawn_shell` at `:1566`

**Interfaces:**
- Consumes: `TmuxCtl::spawn_argv(uuid, cwd, command)` from Task 4.
- Produces: `fn add_tab(&mut self, uuid: String, group: usize, title: Option<String>, crashed: Option<i32>, cwd: Option<&Path>, command: Option<&[String]>, sender: &ComponentSender<Self>) -> usize`, `fn spawn_backing(terminal: &Terminal, uuid: &str, tmux: Option<&TmuxCtl>, cwd: Option<&Path>, command: Option<&[String]>)`, `fn spawn_shell(terminal: &Terminal, cwd: Option<&Path>, command: Option<&[String]>)`.

Pure plumbing: every existing caller passes `None`, so behaviour is unchanged and the existing test suite is the regression check. The no-tmux path gets the same support, because the app must keep working without tmux.

- [ ] **Step 1: Change the signatures and all call sites**

In `src/app.rs`, `add_tab` gains a parameter before `sender`:

```rust
    /// Create a tab backed by `uuid` (spawning its tmux session, or a direct
    /// $SHELL in the fallback path) without changing the active tab. Returns
    /// the new tab id. `title = None` uses the default "Terminal N".
    /// `command = None` starts an interactive shell; `Some(argv)` runs that
    /// instead, which is what `kabelsalat run` uses.
    fn add_tab(
        &mut self,
        uuid: String,
        group: usize,
        title: Option<String>,
        crashed: Option<i32>,
        cwd: Option<&Path>,
        command: Option<&[String]>,
        sender: &ComponentSender<Self>,
    ) -> usize {
```

and its body line 1510 becomes:

```rust
        spawn_backing(&terminal, &uuid, self.tmux.as_ref(), cwd, command);
```

`spawn_backing` and `spawn_shell` become:

```rust
/// Spawn a tab's backing process: the tmux client for its session when tmux is
/// available (`new-session -A` attaches or creates), else a direct $SHELL.
/// `command`, when set, replaces the shell in either path.
fn spawn_backing(
    terminal: &Terminal,
    uuid: &str,
    tmux: Option<&TmuxCtl>,
    cwd: Option<&Path>,
    command: Option<&[String]>,
) {
    let Some(ctl) = tmux else {
        spawn_shell(terminal, cwd, command);
        return;
    };
    let argv = ctl.spawn_argv(uuid, cwd, command);
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    terminal.spawn_async(
        PtyFlags::DEFAULT,
        None,
        &refs,
        &[],
        gtk::glib::SpawnFlags::DEFAULT,
        || {},
        -1,
        gtk::gio::Cancellable::NONE,
        |result| {
            if let Err(err) = result {
                eprintln!("failed to attach tmux session: {err}");
            }
        },
    );
}

fn spawn_shell(terminal: &Terminal, cwd: Option<&Path>, command: Option<&[String]>) {
    // Without tmux there is no re-joining to worry about: VTE takes argv
    // directly, so the command's word boundaries survive exactly.
    let argv: Vec<String> = match command {
        Some(command) if !command.is_empty() => command.to_vec(),
        _ => vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())],
    };
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let working_dir = cwd.map(|p| p.to_string_lossy().into_owned());
    terminal.spawn_async(
        PtyFlags::DEFAULT,
        working_dir.as_deref(),
        &refs,
        &[],
        gtk::glib::SpawnFlags::DEFAULT,
        || {},
        -1,
        gtk::gio::Cancellable::NONE,
        |result| {
            if let Err(err) = result {
                eprintln!("failed to spawn shell: {err}");
            }
        },
    );
}
```

Then update every existing caller to pass `None` for the new parameter:

- `src/app.rs:641` → `spawn_backing(&tab.terminal, &uuid, Some(tmux), None, None);`
- `src/app.rs:885` and `src/app.rs:904` → add `None,` as the second-to-last argument of the `self.add_tab(...)` calls (immediately before the `sender`/`&sender` argument).
- `src/app.rs:1466` → `let id = self.add_tab(uuid, group, None, None, cwd.as_deref(), None, sender);`
- `src/app.rs:1566` → `None => spawn_shell(&terminal, None, None),`

- [ ] **Step 2: Verify nothing else calls them**

Run: `grep -n "add_tab(\|spawn_backing(\|spawn_shell(" src/app.rs`
Expected: only the definitions and the five call sites listed above.

- [ ] **Step 3: Build and run the full suite**

Run: `cargo test`
Expected: PASS, with no behaviour change — every caller still passes `None`.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs
git commit -m "Thread an optional command through the tab spawn path"
```

---

### Task 6: The control snapshot and the spawn message

**Files:**
- Create: `src/control.rs`
- Modify: `src/lib.rs` (add `mod control;`), `src/app.rs` (`Msg` enum around `:258`, `update` around `:601`, `init` around `:597`, `save_state` at `:935-985`)
- Test: `src/control.rs` test module

**Interfaces:**
- Consumes: `cli::{GroupInfo, SpawnRequest, command_title}`, `TmuxCtl::spawn_argv`, `add_tab` from Task 5.
- Produces: `pub fn register(sender: relm4::Sender<crate::app::Msg>)`, `pub fn publish(groups: Vec<GroupInfo>)`, `pub fn snapshot() -> Vec<GroupInfo>`, `pub fn request_spawn(request: SpawnRequest, tab_uuid: String) -> bool`, and `app::Msg::SpawnCommand { group_uuid, tab_uuid, cwd, argv }`.

The `command-line` handler runs on the primary instance's main thread — the same thread that drives the relm4 component. Asking the component a question and waiting for the answer would deadlock, so `App` instead *pushes* a read-only snapshot the handler can read synchronously.

- [ ] **Step 1: Write the failing tests**

Create `src/control.rs` containing only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_empty_before_any_gui_publishes() {
        // No App has run in a unit-test process, so nothing was ever published.
        assert!(snapshot().is_empty());
    }

    #[test]
    fn a_spawn_request_without_a_gui_is_refused() {
        // register() is never called in tests, so there is no sender to use.
        let refused = request_spawn(
            SpawnRequest {
                group_uuid: "aaa-111".into(),
                cwd: std::path::PathBuf::from("/tmp"),
                argv: vec!["ls".into()],
            },
            "tab-uuid".into(),
        );
        assert!(!refused);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib control::`
Expected: compile failure — `cannot find function 'snapshot'`, `cannot find function 'request_spawn'`.

- [ ] **Step 3: Write the control module**

Put this above the test module in `src/control.rs`:

```rust
//! Bridge between the GApplication command line and the running relm4
//! component.
//!
//! The `command-line` handler runs on the primary instance's main thread, the
//! same thread that drives the component. It therefore cannot ask the
//! component a question and wait for the reply — that would block the loop
//! that has to produce it. So `App` pushes a read-only snapshot of its groups
//! here whenever it saves state, and the handler reads that snapshot
//! synchronously. Spawning is one-way: everything is validated against the
//! snapshot *before* the message is sent, so the exit code is meaningful
//! without a reply channel.

use std::sync::{Arc, Mutex, OnceLock};

use crate::app::Msg;
use crate::cli::{GroupInfo, SpawnRequest};

struct Control {
    groups: Arc<Mutex<Vec<GroupInfo>>>,
    sender: relm4::Sender<Msg>,
}

static CONTROL: OnceLock<Control> = OnceLock::new();

/// Called once from `App::init`. A second call is ignored: there is only ever
/// one component in a process.
pub fn register(sender: relm4::Sender<Msg>) {
    let _ = CONTROL.set(Control {
        groups: Arc::new(Mutex::new(Vec::new())),
        sender,
    });
}

/// Called from `App::save_state`, i.e. on every change to groups or tabs.
pub fn publish(groups: Vec<GroupInfo>) {
    let Some(control) = CONTROL.get() else {
        return;
    };
    // A poisoned lock would mean a panic while holding it; there is nothing to
    // do but leave the previous snapshot in place.
    if let Ok(mut slot) = control.groups.lock() {
        *slot = groups;
    }
}

/// The groups of the running instance. Empty when no GUI has published yet.
pub fn snapshot() -> Vec<GroupInfo> {
    CONTROL
        .get()
        .and_then(|control| control.groups.lock().ok().map(|slot| slot.clone()))
        .unwrap_or_default()
}

/// Ask the component to create the tab. Returns false when there is no
/// component to ask, or when it has already shut down.
pub fn request_spawn(request: SpawnRequest, tab_uuid: String) -> bool {
    let Some(control) = CONTROL.get() else {
        return false;
    };
    control
        .sender
        .send(Msg::SpawnCommand {
            group_uuid: request.group_uuid,
            tab_uuid,
            cwd: request.cwd,
            argv: request.argv,
        })
        .is_ok()
}
```

Register it in `src/lib.rs`:

```rust
mod app;
pub mod browser;
mod cli;
mod control;
pub mod state;
pub mod tmuxctl;
```

- [ ] **Step 4: Add the message variant**

In `src/app.rs`, add to the end of the `Msg` enum (after `RestoreBrowser(usize)`, before the closing brace at line 258):

```rust
    /// A `kabelsalat run` invocation: create a tab in the group with this
    /// uuid, running `argv` in `cwd`. Deliberately inert otherwise — it does
    /// not activate the tab, raise the window, change the active group, or
    /// touch the group's browser pane, because the user may be typing
    /// somewhere else when an agent fires this.
    SpawnCommand {
        group_uuid: String,
        tab_uuid: String,
        cwd: PathBuf,
        argv: Vec<String>,
    },
```

- [ ] **Step 5: Handle it**

In `src/app.rs`, add a new arm to the `update` match (Task 5's signature is already in place):

```rust
            Msg::SpawnCommand {
                group_uuid,
                tab_uuid,
                cwd,
                argv,
            } => {
                // The snapshot the CLI validated against is a copy, so the
                // group can in principle be gone by the time this arrives.
                let Some(group_id) = self.groups.iter().find(|g| g.uuid == group_uuid).map(|g| g.id)
                else {
                    eprintln!("spawn request for unknown group {group_uuid}");
                    return;
                };
                let title = crate::cli::command_title(&argv);
                self.add_tab(
                    tab_uuid,
                    group_id,
                    Some(title),
                    None,
                    Some(&cwd),
                    Some(&argv),
                    &sender,
                );
                // add_tab alone leaves the sidebar stale; the usual funnel for
                // that is activate(), which we deliberately do not call.
                self.rebuild_list();
                self.save_state();
            }
```

- [ ] **Step 6: Publish the snapshot**

In `src/app.rs`, at the end of `init` (immediately before `model.restore_or_fresh(&sender);` at line 597):

```rust
        control::register(sender.input_sender().clone());
```

and at the end of `save_state` (after `self.persist(&state);` at line 984), publish from the `SavedState` that was just built, so the snapshot and the file can never disagree:

```rust
        // Same choke point as the save, so the CLI always sees what was last
        // written rather than a separately maintained copy.
        control::publish(
            state
                .groups
                .iter()
                .map(|g| crate::cli::GroupInfo {
                    uuid: g.uuid.clone(),
                    name: g.name.clone(),
                    tabs: state.tabs.iter().filter(|t| t.group == g.id).count(),
                })
                .collect(),
        );
```

Note this needs `self.persist(&state);` to borrow rather than move, which it already does. Add `use crate::control;` to the imports at the top of `src/app.rs` if the module is not already in scope, and make sure `std::path::PathBuf` is imported there (it is, via the existing `active_tab_cwd` code).

- [ ] **Step 7: Run the tests**

Run: `cargo test`
Expected: PASS, including the two new `control::` tests. Nothing in the GUI has changed yet — no path sends `SpawnCommand`.

- [ ] **Step 8: Commit**

```bash
git add src/control.rs src/lib.rs src/app.rs
git commit -m "Publish a group snapshot and accept spawn requests"
```

---

### Task 7: Wire up the command line

**Files:**
- Modify: `src/control.rs` (add the handler), `src/lib.rs:10-44` (`run`)

**Interfaces:**
- Consumes: `cli::{parse, dispatch, help_text, Cli, EXIT_*}`, `control::{snapshot, request_spawn}`.
- Produces: `pub fn handle_command_line(app: &adw::Application, cl: &gio::ApplicationCommandLine) -> glib::ExitCode`.

Verified against the vendored crates, because the whole task hinges on these:
- `connect_command_line` takes `Fn(&Self, &ApplicationCommandLine) -> ExitCode` (gio-0.22.8 `src/auto/application.rs:66`).
- `ApplicationCommandLine` gives `arguments() -> Vec<OsString>`, `cwd() -> Option<PathBuf>`, `print_literal(&str)`, `printerr_literal(&str)`.
- `glib::ExitCode::new(u8)` / `.get() -> u8` (glib-0.22.8 `src/exit_code.rs`).
- `RelmApp::run` **discards** the `ExitCode` from `run_with_args` (relm4-0.11.0 `src/app.rs:191`), so the subcommand path must call `run_with_args` itself.
- relm4 builds the component in `connect_startup`, not `connect_activate` (relm4-0.11.0 `src/app.rs:162`), so `control::register` has run before any `command-line` fires on the primary instance.

- [ ] **Step 1: Write the handler**

Append to `src/control.rs`, above the test module:

```rust
use relm4::adw;
use relm4::gtk::gio;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::cli::{self, Cli};

/// Handle one invocation — the local one on a plain GUI start, or a remote
/// one forwarded over the session bus by a second launch of the binary.
///
/// With `HANDLES_COMMAND_LINE` set, GApplication stops emitting `activate` on
/// its own, so the no-arguments path has to do it explicitly or no window ever
/// appears.
pub fn handle_command_line(
    app: &adw::Application,
    command_line: &gio::ApplicationCommandLine,
) -> glib::ExitCode {
    let args: Vec<String> = command_line
        .arguments()
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    let parsed = match cli::parse(&args) {
        Ok(parsed) => parsed,
        Err(err) => {
            command_line.printerr_literal(&format!("kabelsalat: {}\n", err.0));
            command_line.printerr_literal(cli::help_text());
            return glib::ExitCode::new(cli::EXIT_USAGE);
        }
    };

    if parsed == Cli::Gui {
        app.activate();
        return glib::ExitCode::SUCCESS;
    }

    // The caller's directory, forwarded by GApplication. Falling back to "/"
    // only matters if the caller's cwd was deleted underneath it.
    let cwd = command_line
        .cwd()
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let outcome = cli::dispatch(&parsed, &snapshot(), &cwd);

    if !outcome.stdout.is_empty() {
        command_line.print_literal(&outcome.stdout);
    }
    if !outcome.stderr.is_empty() {
        command_line.printerr_literal(&outcome.stderr);
    }

    if let Some(request) = outcome.spawn {
        // The tab's uuid is minted here, where glib is available, and echoed
        // so the caller has a token proving the tab was created.
        let tab_uuid = glib::uuid_string_random().to_string();
        if !request_spawn(request, tab_uuid.clone()) {
            command_line.printerr_literal("kabelsalat: no window to spawn into\n");
            return glib::ExitCode::new(cli::EXIT_NOT_RUNNING);
        }
        command_line.print_literal(&format!("{tab_uuid}\n"));
    }

    glib::ExitCode::new(outcome.code)
}
```

- [ ] **Step 2: Rewrite `run`**

In `src/lib.rs`, replace the `use` line and the body of `run` (keeping the CSS string exactly as it is):

```rust
use relm4::RelmApp;
use relm4::adw;
use relm4::gtk::gio;
use relm4::gtk::prelude::*;

// ... mod declarations, APP_ID ...

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    // Usage errors are decided here, before anything touches the bus, so a
    // typo reports as a typo whether or not a GUI is running.
    let parsed = match cli::parse(&args) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("kabelsalat: {}", err.0);
            eprint!("{}", cli::help_text());
            std::process::exit(cli::EXIT_USAGE.into());
        }
    };
    if parsed == cli::Cli::Help {
        print!("{}", cli::help_text());
        return;
    }

    // Constructing a GtkApplication is safe before GTK is initialised (the
    // gtk4 builder has no init assertion), so the subcommand path below never
    // needs a display.
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    app.connect_command_line(control::handle_command_line);

    if parsed.needs_instance() {
        // A subcommand must never start a GUI. Registering tells us whether
        // somebody else already owns the name: if not, we are alone and there
        // is nothing to talk to, so bail out before `startup` builds a window.
        if app.register(gio::Cancellable::NONE).is_err() || !app.is_remote() {
            eprintln!("kabelsalat: not running");
            std::process::exit(cli::EXIT_NOT_RUNNING.into());
        }
        // RelmApp::run drops this exit code, so drive the application directly.
        let code = app.run_with_args(&args);
        std::process::exit(code.get().into());
    }

    // RelmApp::new calls relm4's private init(); from_app does not, and
    // set_global_css builds a CssProvider, which needs GTK up. So do here
    // exactly what relm4::init() would have done — but only on this path,
    // where a display is genuinely required.
    relm4::gtk::init().expect("failed to initialise GTK");
    adw::init().expect("failed to initialise libadwaita");

    relm4::set_global_css(
        // ... unchanged CSS string ...
    );
    RelmApp::from_app(app).with_args(args).run::<app::App>(());
}
```

Why the explicit init, since this is easy to get wrong: `relm4::init()` is
**private** (relm4-0.11.0 `src/lib.rs:116`) and is called only by
`RelmApp::new`, not by `RelmApp::from_app`. All it does is
`gtk::init().unwrap()` plus `adw::init().unwrap()`, which is what the two lines
above reproduce. Both return `Result<(), glib::BoolError>`.

- [ ] **Step 3: Build and run the full suite**

Run: `cargo build && cargo test`
Expected: builds clean, all tests pass.

- [ ] **Step 4: Verify the GUI still starts and the CLI answers**

This is the first point where the feature is observable. Run it and confirm each line:

```bash
cargo run &            # a window appears, as before
sleep 3
cargo run -- groups    # one tab-separated line per group
cargo run -- run -g "$(cargo run -q -- groups | head -1 | cut -f1)" -- htop
```

Expected: `groups` prints `uuid<TAB>name<TAB>count`; `run` prints a fresh uuid, a new tab appears in that group running htop, and **focus does not move and the window does not raise**.

- [ ] **Step 5: Commit**

```bash
git add src/control.rs src/lib.rs
git commit -m "Forward CLI invocations to the running instance"
```

---

### Task 8: The agent skill

**Files:**
- Create: `skills/kabelsalat/SKILL.md`, `scripts/install-skill.sh`
- Modify: `README.md`, `CLAUDE.md`

- [ ] **Step 1: Write the skill**

Create `skills/kabelsalat/SKILL.md`:

```markdown
---
name: kabelsalat
description: Use when a command should run in a visible, persistent terminal the user can watch and interact with — a dev server, a long build, or an interactive claude session — rather than as a captured subprocess. Launches it in a named group of the user's running kabelsalat terminal.
---

# Launching terminals in kabelsalat

kabelsalat is the user's terminal app. Its tabs are organised into named
groups and backed by tmux sessions, so a tab survives the GUI restarting.

Use this when a command should keep running and stay visible after you are
done — a dev server, a watcher, a long build, another `claude` session. Do not
use it for commands whose output you need to read: those still belong in your
normal shell tool, because you cannot read a kabelsalat tab's output.

## Listing groups

    kabelsalat groups

One line per group, tab-separated: `uuid`, `name`, `tab count`. The name is
empty for unnamed groups. Run this first — you cannot guess group names.

    a1b2c3d4-...	erhebimus	3
    e5f6a7b8-...	web	1
    c9d0e1f2-...		2

## Running a command

    kabelsalat run --group <name|uuid> [--cwd DIR] -- COMMAND [ARGS...]

Everything after `--` is the command, taken literally. The `--` is required.

    kabelsalat run -g erhebimus -- claude
    kabelsalat run -g web -- npm run dev
    kabelsalat run -g web --cwd ~/Projects/textimus -- cargo watch -x test

For shell syntax — pipes, `&&`, globbing — invoke a shell explicitly:

    kabelsalat run -g web -- bash -lc 'cd frontend && npm run dev'

`--cwd` defaults to your current working directory. On success the new tab's
uuid is printed.

## Targeting a group

`--group` takes a uuid or an exact, case-sensitive name. A uuid always wins.
If several groups share a name, the command fails and prints their uuids —
retry with one of those. Unnamed groups can only be targeted by uuid.

## Exit codes

| Code | Meaning | What to do |
|------|---------|------------|
| 0 | Tab created | Tell the user which group it went to |
| 1 | kabelsalat is not running | Report this to the user and stop. Do not retry, and do not try to start it — that is theirs to do |
| 2 | Usage error | Fix the invocation; check `--` is present |
| 3 | Group not found or ambiguous | Re-run `kabelsalat groups` and retry with a uuid |

## After launching

The new tab is created **without stealing focus** — the window is not raised
and the user's current tab keeps their keystrokes. So always tell the user
which group the tab appeared in, or they will not notice it.

You cannot read the tab's output, send input to it, or close it. If the
command exits, the tab stays visible showing its exit status.
```

- [ ] **Step 2: Write the installer**

Create `scripts/install-skill.sh`:

```sh
#!/bin/sh
# Link the kabelsalat agent skill into the user's Claude skills directory, so
# an agent working in any project can find it. A symlink rather than a copy:
# the skill then tracks whatever this checkout has.
set -eu

repo="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
src="$repo/skills/kabelsalat"
dest="${HOME}/.claude/skills/kabelsalat"

[ -d "$src" ] || { echo "no skill at $src" >&2; exit 1; }
mkdir -p "$(dirname "$dest")"
ln -sfn "$src" "$dest"
echo "linked $dest -> $src"
```

Make it executable:

```bash
chmod +x scripts/install-skill.sh
```

- [ ] **Step 3: Run the installer and verify**

```bash
./scripts/install-skill.sh
ls -l ~/.claude/skills/kabelsalat
```

Expected: a symlink pointing at `/home/christoph/Projects/kabelsalat/skills/kabelsalat`.

- [ ] **Step 4: Document it**

Add to `README.md`, after the keyboard-shortcut section:

```markdown
## Command line

With kabelsalat running, a second invocation talks to it instead of opening a
second window:

    kabelsalat groups                      # uuid, name and tab count per group
    kabelsalat run -g web -- npm run dev   # new tab in the "web" group

`--group` takes a group name or uuid; `--cwd` overrides the working directory,
which defaults to the caller's. Everything after `--` is the command. The new
tab does not steal focus. Exit codes: 0 success, 1 not running, 2 usage,
3 no such group.

`scripts/install-skill.sh` links `skills/kabelsalat` into `~/.claude/skills/`
so Claude Code knows how to use this.
```

Add to `CLAUDE.md` under "Architecture invariants":

```markdown
- `src/cli.rs` is pure logic — argv parsing, group resolution, and the decision
  of what an invocation prints and exits with. No GTK, no gio, no tmux, no I/O;
  it is where the CLI's unit tests live. `src/control.rs` holds the gio glue and
  the group snapshot the command-line handler reads.
- A CLI-created tab must not steal focus: no activate, no window raise, no
  active-group change, and no touching the group's browser pane.
```

- [ ] **Step 5: Commit**

```bash
git add skills/kabelsalat/SKILL.md scripts/install-skill.sh README.md CLAUDE.md
git commit -m "Add the kabelsalat agent skill and document the CLI"
```

---

### Task 9: Verification

**Files:** none — this is the gate before calling the work done.

- [ ] **Step 1: Format, lint, test**

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Expected: `cargo fmt` produces no diff on a second run, clippy is clean, all tests pass. Fix anything that is not.

- [ ] **Step 2: Manual verification**

Run each of these against a real GUI and confirm the stated result. None of the D-Bus path is unit-testable, so this is the only check on it.

1. **Groups list.** Start the GUI, create two groups, name one `web` and leave the other unnamed. `kabelsalat groups` prints two tab-separated lines, the unnamed one with an empty middle field, and the counts match the sidebar.
2. **Run.** `cd ~/Projects/erhebimus && kabelsalat run -g web -- htop` prints a uuid, and a tab named `htop` appears in `web`, running htop, with `~/Projects/erhebimus` as its directory (check with `kabelsalat run -g web -- bash -lc 'pwd; sleep 30'`).
3. **No side effects.** While typing in a tab of *another* group, run command 2 again. Your keystrokes keep going where they were, the window does not raise, the active group does not change, and if the target group has a browser pane open or closed it stays exactly as it was.
4. **Ambiguity.** Name both groups `web`. `kabelsalat run -g web -- true` exits 3 and prints both uuids. Targeting one of those uuids works.
5. **Command exit.** `kabelsalat run -g web -- false` creates a tab that stays visible showing its exit status, and the existing restart action re-runs it.
6. **Not running.** Quit the GUI entirely. `kabelsalat groups` prints `kabelsalat: not running`, exits 1, and **no window appears**. Same for `run`.
7. **Usage.** `kabelsalat run -g web claude` (no `--`) exits 2 with the help text, both with the GUI running and with it closed.
8. **Regression — plain launch.** With no GUI running, `kabelsalat` opens a window as before, with the previous session restored. With one running, a second `kabelsalat` raises the existing window instead of opening a second one. This is the check on the `HANDLES_COMMAND_LINE` change.
9. **Regression — new tab.** The in-GUI new-tab shortcut still opens an interactive shell in the active tab's directory.
10. **Regression — restart.** Kill a CLI-created tab's process; the tab shows as crashed and restarting it re-runs the original command, not a shell.

- [ ] **Step 3: Report**

Summarise which of the ten checks passed, quoting the actual output for any that did not. Do not claim the feature works on the strength of `cargo test` alone — the transport has no unit tests.
