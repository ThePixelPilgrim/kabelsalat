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
/// `--group` matched no group, or matched more than one; or the Android pane
/// is remote, owned by another group, not open, or did not answer.
pub const EXIT_GROUP: u8 = 3;
/// `resume` could not reach tmux, start the server, or create a session; or
/// the Android pane answered a control request with an error.
pub const EXIT_FAILED: u8 = 4;

/// The variable every tab's tmux session carries: its group's uuid. Set by
/// the GUI at session creation and refreshed on every move; read back by
/// `browser` as the default group, and by agents (see the skill).
pub const ENV_GROUP: &str = "KABELSALAT_GROUP";
/// The variable that carries a group browser's CDP endpoint while it is up
/// (`http://127.0.0.1:<port>`), alongside `PLAYWRIGHT_MCP_CDP_ENDPOINT`.
pub const ENV_CDP: &str = "KABELSALAT_CDP";
/// The same endpoint under the name Playwright's MCP server reads.
pub const ENV_CDP_PLAYWRIGHT: &str = "PLAYWRIGHT_MCP_CDP_ENDPOINT";
/// The endpoint pair: set on discovery, unset on browser close. `ENV_GROUP`
/// is deliberately not in here — it describes the tab, not the browser, and
/// is never unset.
pub const CDP_ENV_KEYS: [&str; 2] = [ENV_CDP, ENV_CDP_PLAYWRIGHT];
/// The Android pane's control socket, while a booted Android is up in the
/// group (`screenshot`, `click`, `type`, `key`, `resize`; one line each).
pub const ENV_ANDROID_CTL: &str = "KABELSALAT_ANDROID_CTL";
/// The adb serial of that Android (`<ip>:5555`).
pub const ENV_ANDROID_ADB: &str = "KABELSALAT_ANDROID_ADB";
/// The Android pair: set once Android has booted, unset on stop or death.
pub const ANDROID_ENV_KEYS: [&str; 2] = [ENV_ANDROID_CTL, ENV_ANDROID_ADB];

/// One request to the Android pane's control socket, as typed on the command
/// line. See `vendor/nested-wayland-session/src/protocol.rs` in klamottenkiste
/// for the wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AndroidCmd {
    /// Write the pane's current frame to this PNG file.
    Screenshot(PathBuf),
    /// Click at pane coordinates (Android sees a mouse, not a finger).
    Tap(u32, u32),
    /// Type this text, one key tap per character.
    Type(String),
    /// Tap one named key (`enter`, `escape`, `a`, …).
    Key(String),
    /// Resize the nested screen; Android follows.
    Resize(u32, u32),
}

impl AndroidCmd {
    /// The argv tail that parses back to this command.
    pub fn args(&self) -> Vec<String> {
        match self {
            Self::Screenshot(path) => {
                vec!["screenshot".into(), path.to_string_lossy().into_owned()]
            }
            Self::Tap(x, y) => vec!["tap".into(), x.to_string(), y.to_string()],
            Self::Type(text) => vec!["type".into(), text.clone()],
            Self::Key(name) => vec!["key".into(), name.clone()],
            Self::Resize(w, h) => vec!["resize".into(), w.to_string(), h.to_string()],
        }
    }
}

/// What an invocation asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    /// No arguments: start or raise the GUI, as before.
    Gui,
    Help,
    Groups,
    /// Bring up a group's browser pane, or print its live CDP endpoint.
    Browser {
        /// A group uuid or name; `None` until `with_default_group` fills it
        /// from the caller's `KABELSALAT_GROUP`.
        group: Option<String>,
    },
    /// Bring up Android in a group's pane area and print its endpoints, or
    /// send one request to its pane's control socket.
    Android {
        /// A group uuid or name; `None` until `with_default_group` fills it
        /// from the caller's `KABELSALAT_GROUP`.
        group: Option<String>,
        /// `None`: open it, or print `ctl=`/`adb=`. `Some`: one request.
        cmd: Option<AndroidCmd>,
    },
    /// Recreate the claude sessions of saved tabs on the private tmux
    /// server, without a GUI: what the boot unit runs. Handled entirely in
    /// the calling process; against a running GUI it does nothing.
    Resume,
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
    /// Set or clear the directory a group's overview is built from.
    OverviewRoot {
        /// A group uuid or name; `None` until `with_default_group` fills it
        /// from the caller's `KABELSALAT_GROUP`.
        group: Option<String>,
        /// The directory verbatim, still possibly relative to the caller's;
        /// `None` is `--clear`.
        root: Option<PathBuf>,
    },
    /// Print a group's current overview data issues, one per line.
    OverviewIssues {
        /// A group uuid or name; `None` until `with_default_group` fills it
        /// from the caller's `KABELSALAT_GROUP`.
        group: Option<String>,
    },
}

impl Cli {
    /// Whether answering this needs the running GUI. `Gui` and `Help` are
    /// handled entirely in the calling process.
    pub fn needs_instance(&self) -> bool {
        matches!(
            self,
            Cli::Groups
                | Cli::Run { .. }
                | Cli::Rename { .. }
                | Cli::Browser { .. }
                | Cli::Android { .. }
                | Cli::OverviewRoot { .. }
                | Cli::OverviewIssues { .. }
        )
    }
}

/// Fill `browser`'s group from the caller's environment when `--group` was
/// not given: a tab's shell carries its group in `KABELSALAT_GROUP`, so an
/// agent in a tab need not name the group it sits in. Outside a tab there
/// is nothing to default to, and that is a usage error. Every other command
/// passes through untouched.
pub fn with_default_group(cli: Cli, env_group: Option<&str>) -> Result<Cli, UsageError> {
    match cli {
        Cli::Browser { group: None } => match env_group.filter(|g| !g.is_empty()) {
            Some(group) => Ok(Cli::Browser {
                group: Some(group.to_string()),
            }),
            None => Err(UsageError(format!(
                "browser needs --group outside a kabelsalat tab ({ENV_GROUP} is unset)"
            ))),
        },
        Cli::Android { group: None, cmd } => match env_group.filter(|g| !g.is_empty()) {
            Some(group) => Ok(Cli::Android {
                group: Some(group.to_string()),
                cmd,
            }),
            None => Err(UsageError(format!(
                "android needs --group outside a kabelsalat tab ({ENV_GROUP} is unset)"
            ))),
        },
        Cli::OverviewRoot { group: None, root } => match env_group.filter(|g| !g.is_empty()) {
            Some(group) => Ok(Cli::OverviewRoot {
                group: Some(group.to_string()),
                root,
            }),
            None => Err(UsageError(overview_group_missing())),
        },
        Cli::OverviewIssues { group: None } => match env_group.filter(|g| !g.is_empty()) {
            Some(group) => Ok(Cli::OverviewIssues {
                group: Some(group.to_string()),
            }),
            None => Err(UsageError(overview_group_missing())),
        },
        other => Ok(other),
    }
}

/// The one wording for an `overview` command with no group anywhere.
fn overview_group_missing() -> String {
    format!("overview needs --group outside a kabelsalat tab ({ENV_GROUP} is unset)")
}

/// The Android pair, in the order `ENV_ANDROID_CTL`, `ENV_ANDROID_ADB`.
pub fn android_env_pairs(ctl: &str, adb: &str) -> [(&'static str, String); 2] {
    [
        (ENV_ANDROID_CTL, ctl.to_string()),
        (ENV_ANDROID_ADB, adb.to_string()),
    ]
}

/// The variables a group's session is created with: the group identity
/// always, the CDP pair while the browser has an endpoint, the Android pair
/// while a booted Android is up. Pure.
pub fn session_env_pairs(
    group_uuid: &str,
    cdp: Option<&str>,
    android: Option<(&str, &str)>,
) -> Vec<(&'static str, String)> {
    let mut env = vec![(ENV_GROUP, group_uuid.to_string())];
    if let Some(url) = cdp {
        for key in CDP_ENV_KEYS {
            env.push((key, url.to_string()));
        }
    }
    if let Some((ctl, adb)) = android {
        env.extend(android_env_pairs(ctl, adb));
    }
    env
}

/// One group as the CLI sees it: the stable uuid, the (possibly empty,
/// possibly duplicated) name, how many tabs it holds, for a remote group
/// the ssh destination its tabs run on, the live CDP endpoint of its
/// browser when it has one, its Android pane when it owns it, and its
/// overview root and current overview issues.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupInfo {
    pub uuid: String,
    pub name: String,
    pub tabs: usize,
    pub host: Option<String>,
    pub cdp: Option<String>,
    pub android: Option<AndroidInfo>,
    /// The directory the group's overview is built from, absolute.
    pub overview_root: Option<PathBuf>,
    /// The overview's current data issues, as `overview issues` prints them.
    pub issues: Vec<IssueLine>,
}

/// One overview data issue as the CLI sees it: already reduced to text, so
/// this module stays independent of the overview's own types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueLine {
    /// `unreadable-file`, `duplicate-id`, `unknown-node`, `parent-cycle`,
    /// `missing-link`.
    pub kind: String,
    /// The node ids involved.
    pub nodes: Vec<String>,
    /// The file at fault, when there is one.
    pub file: Option<PathBuf>,
    pub detail: String,
}

/// A group's Android pane as the CLI sees it: its control socket from the
/// moment it exists, its adb serial once Android has booted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidInfo {
    pub ctl: PathBuf,
    pub adb: Option<String>,
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
  kabelsalat browser [-g <group>]              Open <group>'s browser pane, or print its live
                                               CDP endpoint; <group> defaults to the caller's
                                               KABELSALAT_GROUP
  kabelsalat android [-g <group>]              Open Android (Waydroid) in <group>'s pane area, or
                                               print its control socket and adb serial as
                                               ctl=/adb= lines; <group> defaults to the
                                               caller's KABELSALAT_GROUP
  kabelsalat android [-g <group>] screenshot PATH | tap X Y | type TEXT | key NAME | resize W H
                                               Drive <group>'s Android pane
  kabelsalat overview root [-g <group>] DIR    Build <group>'s overview from the *.md files
                                               below DIR (resolved against the caller's
                                               directory; must exist); <group> defaults to
                                               the caller's KABELSALAT_GROUP
  kabelsalat overview root [-g <group>] --clear
                                               Unset <group>'s overview root
  kabelsalat overview issues [-g <group>]      Print <group>'s overview data issues, one per
                                               line: kind, node ids, file, detail (tab-
                                               separated); nothing and exit 0 when there are none
  kabelsalat resume                            Recreate saved claude sessions on the tmux
                                               server without a GUI (what the boot unit runs)

Options for run:
  -g, --group <name|uuid>   Target group. A uuid always wins; otherwise the
                            name must match exactly and match only one group.
      --create              When nothing matches, create a group named
                            <group> and put the tab in it.
      --cwd <dir>           Working directory (default: the caller's). For a
                            remote group, a path on its host (default: the
                            remote home).
      --                    Required. Everything after it is the command.

Exit codes:
  0 success   1 kabelsalat not running   2 usage error (also an overview DIR that does not exist)
  3 group not found, ambiguous, remote (browser, android, overview), the new name is
    already taken, Android owned by another group or not open, or its pane did not answer
  4 resume could not reach tmux, or the Android pane refused a request
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
        "browser" => parse_browser(&rest[1..]),
        "android" => parse_android(&rest[1..]),
        "overview" => parse_overview(&rest[1..]),
        "resume" => {
            if rest.len() > 1 {
                return Err(UsageError(format!(
                    "resume takes no arguments (got '{}')",
                    rest[1]
                )));
            }
            Ok(Cli::Resume)
        }
        other => Err(UsageError(format!("unknown command '{other}'"))),
    }
}

/// `browser [-g <group>]`: at most the group flag, nothing positional.
fn parse_browser(args: &[String]) -> Result<Cli, UsageError> {
    let mut group: Option<String> = None;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            "--group" | "-g" => {
                let value = args
                    .get(i + 1)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| UsageError(format!("{arg} needs a value")))?;
                group = Some(value.clone());
                i += 2;
            }
            other => return Err(UsageError(format!("unknown argument '{other}'"))),
        }
    }
    Ok(Cli::Browser { group })
}

/// `android [-g <group>] [SUBCOMMAND ARGS...]`. The group flag may only come
/// before the subcommand, so text after `type` is never read as a flag.
fn parse_android(args: &[String]) -> Result<Cli, UsageError> {
    let mut group: Option<String> = None;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if arg != "--group" && arg != "-g" {
            break;
        }
        let value = args
            .get(i + 1)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| UsageError(format!("{arg} needs a value")))?;
        group = Some(value.clone());
        i += 2;
    }
    // Only in the verb's place: after it, `--help` is an argument.
    if matches!(args.get(i).map(String::as_str), Some("-h" | "--help")) {
        return Ok(Cli::Help);
    }
    let cmd = parse_android_cmd(&args[i..])?;
    Ok(Cli::Android { group, cmd })
}

/// The subcommand of `android`, if any.
fn parse_android_cmd(args: &[String]) -> Result<Option<AndroidCmd>, UsageError> {
    let Some((verb, rest)) = args.split_first() else {
        return Ok(None);
    };
    let cmd = match (verb.as_str(), rest) {
        ("screenshot", [path]) => {
            if path.is_empty() {
                return Err(UsageError("screenshot needs a file path".into()));
            }
            check_one_line("screenshot path", path)?;
            // The pane trims its command line, and an argument that was not
            // UTF-8 arrives with U+FFFD in it: either way the pane would
            // write a file other than the one named.
            if path.trim() != path {
                return Err(UsageError(
                    "the screenshot path cannot start or end with whitespace".into(),
                ));
            }
            if path.contains('\u{FFFD}') {
                return Err(UsageError(
                    "the screenshot path is not valid UTF-8; choose another name".into(),
                ));
            }
            AndroidCmd::Screenshot(PathBuf::from(path))
        }
        ("tap", [x, y]) => AndroidCmd::Tap(coordinate("tap x", x)?, coordinate("tap y", y)?),
        ("type", [text]) => {
            if text.is_empty() {
                return Err(UsageError("type needs some text".into()));
            }
            check_one_line("text to type", text)?;
            AndroidCmd::Type(text.clone())
        }
        ("key", [name]) => {
            if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(UsageError(format!(
                    "'{name}' is not a key name (one word, e.g. enter)"
                )));
            }
            AndroidCmd::Key(name.clone())
        }
        ("resize", [w, h]) => AndroidCmd::Resize(
            dimension("resize width", w)?,
            dimension("resize height", h)?,
        ),
        ("screenshot" | "type" | "key", _) => {
            return Err(UsageError(format!("{verb} takes exactly one argument")));
        }
        ("tap" | "resize", _) => {
            return Err(UsageError(format!("{verb} takes exactly two numbers")));
        }
        (other, _) => return Err(UsageError(format!("unknown android command '{other}'"))),
    };
    Ok(Some(cmd))
}

/// The control protocol is one request per line, so a value that would end
/// the line early is a usage error.
fn check_one_line(what: &str, value: &str) -> Result<(), UsageError> {
    if value.contains(['\n', '\r']) {
        return Err(UsageError(format!("the {what} must be a single line")));
    }
    Ok(())
}

fn coordinate(what: &str, value: &str) -> Result<u32, UsageError> {
    value.parse().map_err(|_| {
        UsageError(format!(
            "{what} must be a whole number of pixels (got '{value}')"
        ))
    })
}

/// A screen dimension: the protocol takes a positive `i32`.
fn dimension(what: &str, value: &str) -> Result<u32, UsageError> {
    let n = coordinate(what, value)?;
    if n == 0 || n > i32::MAX as u32 {
        return Err(UsageError(format!(
            "{what} must be between 1 and {}",
            i32::MAX
        )));
    }
    Ok(n)
}

/// The argv a caller forwards to the running GUI once its group is known:
/// `android -g <group>` plus the subcommand's own arguments.
pub fn android_argv(argv0: &str, group: &str, cmd: Option<&AndroidCmd>) -> Vec<String> {
    let mut argv = vec![
        argv0.to_string(),
        "android".to_string(),
        "-g".to_string(),
        group.to_string(),
    ];
    if let Some(cmd) = cmd {
        argv.extend(cmd.args());
    }
    argv
}

/// `overview root [-g <group>] DIR | --clear` and `overview issues [-g <group>]`.
fn parse_overview(args: &[String]) -> Result<Cli, UsageError> {
    let Some((verb, rest)) = args.split_first() else {
        return Err(UsageError("overview needs root or issues".into()));
    };
    match verb.as_str() {
        "-h" | "--help" => Ok(Cli::Help),
        "root" => parse_overview_root(rest),
        "issues" => {
            let mut group: Option<String> = None;
            let mut i = 0;
            while let Some(arg) = rest.get(i) {
                match arg.as_str() {
                    "--group" | "-g" => {
                        group = Some(group_value(rest, i)?);
                        i += 2;
                    }
                    other => return Err(UsageError(format!("unknown argument '{other}'"))),
                }
            }
            Ok(Cli::OverviewIssues { group })
        }
        other => Err(UsageError(format!("unknown overview command '{other}'"))),
    }
}

/// `root`'s arguments: the group flag anywhere, then exactly one of a
/// directory and `--clear`.
fn parse_overview_root(args: &[String]) -> Result<Cli, UsageError> {
    let mut group: Option<String> = None;
    let mut dir: Option<PathBuf> = None;
    let mut clear = false;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            "--group" | "-g" => {
                group = Some(group_value(args, i)?);
                i += 2;
                continue;
            }
            "--clear" => {
                if clear {
                    return Err(UsageError("--clear was given twice".into()));
                }
                clear = true;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(UsageError(format!("unknown option '{other}'")));
            }
            "" => {
                return Err(UsageError(
                    "overview root needs a non-empty directory".into(),
                ));
            }
            path => {
                if let Some(first) = &dir {
                    return Err(UsageError(format!(
                        "overview root takes one directory (got '{}' and '{path}')",
                        first.display()
                    )));
                }
                dir = Some(PathBuf::from(path));
            }
        }
        i += 1;
    }
    match (dir, clear) {
        (Some(root), false) => Ok(Cli::OverviewRoot {
            group,
            root: Some(root),
        }),
        (None, true) => Ok(Cli::OverviewRoot { group, root: None }),
        _ => Err(UsageError(
            "overview root takes a directory or --clear".into(),
        )),
    }
}

/// The value of the group flag at `args[i]`: the next argument, non-empty.
fn group_value(args: &[String], i: usize) -> Result<String, UsageError> {
    args.get(i + 1)
        .filter(|v| !v.is_empty())
        .cloned()
        .ok_or_else(|| UsageError(format!("{} needs a value", args[i])))
}

/// The argv a caller forwards to the running GUI once its group is known:
/// `overview root|issues -g <group>` plus the directory or `--clear`. For
/// any other `cli` it is `argv0` alone, which parses as the GUI.
pub fn overview_argv(argv0: &str, group: &str, cli: &Cli) -> Vec<String> {
    let mut argv = vec![argv0.to_string(), "overview".to_string()];
    match cli {
        Cli::OverviewRoot { root, .. } => {
            argv.extend(["root".to_string(), "-g".to_string(), group.to_string()]);
            match root {
                Some(dir) => argv.push(dir.to_string_lossy().into_owned()),
                None => argv.push("--clear".to_string()),
            }
        }
        Cli::OverviewIssues { .. } => {
            argv.extend(["issues".to_string(), "-g".to_string(), group.to_string()]);
        }
        _ => argv.truncate(1),
    }
    argv
}

/// The control-socket request line for `cmd`, without the newline. Pure.
pub fn control_line(cmd: &AndroidCmd) -> String {
    match cmd {
        AndroidCmd::Screenshot(path) => format!("screenshot {}", path.display()),
        AndroidCmd::Tap(x, y) => format!("click {x} {y}"),
        AndroidCmd::Type(text) => format!("type {text}"),
        AndroidCmd::Key(name) => format!("key {name}"),
        AndroidCmd::Resize(w, h) => format!("resize {w} {h}"),
    }
}

/// What a reply line from the pane prints and exits with: `ok` → nothing,
/// `ok <data>` → the data, `err <message>` → the message and exit 4. Pure.
pub fn control_reply(reply: &str) -> Outcome {
    let line = reply.trim_end_matches(['\r', '\n']);
    if line == "ok" {
        return Outcome::ok(String::new());
    }
    if let Some(data) = line.strip_prefix("ok ") {
        return Outcome::ok(format!("{data}\n"));
    }
    if let Some(message) = line.strip_prefix("err ") {
        return Outcome::fail(EXIT_FAILED, format!("kabelsalat: android: {message}\n"));
    }
    Outcome::fail(
        EXIT_FAILED,
        format!("kabelsalat: android: unexpected reply from the pane: '{line}'\n"),
    )
}

/// The stderr line for a control-socket round trip that failed with an I/O
/// error of `kind` (`detail` is its text): a timeout is said as one, with the
/// bound `timeout_secs`, whatever the platform calls it. Pure.
pub fn control_error(
    socket: &str,
    kind: std::io::ErrorKind,
    detail: &str,
    timeout_secs: u64,
) -> String {
    let why = match kind {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            format!("timed out after {timeout_secs} s")
        }
        _ => detail.to_string(),
    };
    format!("kabelsalat: the Android pane did not answer on {socket}: {why}\n")
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
        /// Where the tab starts. Always `Some` for a local group (the
        /// caller's directory, or `--cwd` resolved against it). For a remote
        /// group it is `--cwd` verbatim — a path on that host, never checked
        /// here — and `None` (the remote home) without it.
        cwd: Option<PathBuf>,
        argv: Vec<String>,
    },
    Rename {
        group_uuid: String,
        name: String,
    },
    /// Bring up this group's browser. Like `Spawn`, inert otherwise: no
    /// focus, no raise, no active-group change; a non-active group's
    /// browser comes up hidden, as a restored one does.
    OpenBrowser {
        group_uuid: String,
    },
    /// Bring up Android in this group. Inert like `OpenBrowser`: no focus,
    /// no raise, no active-group change, no front-pane change.
    OpenAndroid {
        group_uuid: String,
    },
    /// Send `line` to the Android pane's control socket and print the reply.
    Control {
        socket: PathBuf,
        line: String,
    },
    /// Set this group's overview root, or clear it with `None`. Inert like
    /// the others: no mode change, no focus, no raise, no active-group
    /// change.
    SetOverviewRoot {
        group_uuid: String,
        /// Already absolute: resolved against the caller's directory.
        root: Option<PathBuf>,
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
            let (target, host) = match resolve_group(groups, group) {
                Ok(found) => (
                    GroupTarget::Existing(found.uuid.clone()),
                    found.host.as_deref(),
                ),
                // A selector that looks like a uuid is not special-cased: with
                // --create it simply becomes the new group's name. `--create`
                // only ever makes local groups.
                Err(ResolveError::NotFound) if *create => (
                    GroupTarget::Create {
                        name: group.clone(),
                    },
                    None,
                ),
                Err(err) => return resolve_failure(group, err),
            };
            let cwd = match host {
                // A remote tab runs on its host, where the caller's directory
                // means nothing: --cwd passes through verbatim, and without it
                // the shell starts in the remote home.
                Some(_) => cwd.clone(),
                // `join` with an absolute path replaces the base, so this
                // handles both absolute and relative --cwd values.
                None => Some(match cwd {
                    Some(dir) => caller_cwd.join(dir),
                    None => caller_cwd.to_path_buf(),
                }),
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
        // control.rs fills the default before dispatching; this is only
        // reached by a caller that skipped that step.
        Cli::Browser { group: None } => Outcome::fail(
            EXIT_USAGE,
            format!("browser needs --group outside a kabelsalat tab ({ENV_GROUP} is unset)\n"),
        ),
        Cli::Browser { group: Some(group) } => {
            let target = match resolve_group(groups, group) {
                Ok(found) => found,
                Err(err) => return resolve_failure(group, err),
            };
            // The pane is a local widget; a loopback endpoint means nothing
            // on the host a remote group's tabs run on.
            if let Some(host) = &target.host {
                return Outcome::fail(
                    EXIT_GROUP,
                    format!("the browser pane is local, but '{group}' runs on {host}\n"),
                );
            }
            match &target.cdp {
                // Already up: the endpoint is the answer, nothing to ask for.
                Some(url) => Outcome::ok(format!("{url}\n")),
                None => Outcome {
                    stdout: String::new(),
                    stderr: format!(
                        "kabelsalat: bringing up the browser of group '{}'; read {ENV_CDP} \
                         from `tmux show-environment` once it is there\n",
                        if target.name.is_empty() {
                            &target.uuid
                        } else {
                            &target.name
                        }
                    ),
                    code: EXIT_OK,
                    action: Some(Action::OpenBrowser {
                        group_uuid: target.uuid.clone(),
                    }),
                },
            }
        }
        // control.rs fills the default before dispatching, as for browser.
        Cli::Android { group: None, .. } => Outcome::fail(
            EXIT_USAGE,
            format!("android needs --group outside a kabelsalat tab ({ENV_GROUP} is unset)\n"),
        ),
        Cli::Android {
            group: Some(group),
            cmd,
        } => {
            let target = match resolve_group(groups, group) {
                Ok(found) => found,
                Err(err) => return resolve_failure(group, err),
            };
            if let Some(host) = &target.host {
                return Outcome::fail(
                    EXIT_GROUP,
                    format!("the Android pane is local, but '{group}' runs on {host}\n"),
                );
            }
            // One Waydroid per machine, so at most one owner.
            let owner = groups.iter().find(|g| g.android.is_some());
            if let Some(other) = owner.filter(|o| o.uuid != target.uuid) {
                return Outcome::fail(
                    EXIT_GROUP,
                    format!(
                        "Android is open in group '{}'; there is one per machine, \
                         so stop it there first\n",
                        display_name(other)
                    ),
                );
            }
            match (owner.and_then(|o| o.android.as_ref()), cmd) {
                (Some(info), None) => match &info.adb {
                    Some(adb) => Outcome::ok(format!("ctl={}\nadb={adb}\n", info.ctl.display())),
                    None => Outcome {
                        stdout: String::new(),
                        stderr: format!(
                            "kabelsalat: Android in group '{}' is still booting; read \
                             {ENV_ANDROID_CTL} from `tmux show-environment` once it is there\n",
                            display_name(target)
                        ),
                        code: EXIT_OK,
                        action: None,
                    },
                },
                (Some(info), Some(cmd)) => Outcome {
                    stdout: String::new(),
                    stderr: String::new(),
                    code: EXIT_OK,
                    action: Some(Action::Control {
                        socket: info.ctl.clone(),
                        line: control_line(&with_absolute_path(cmd, caller_cwd)),
                    }),
                },
                (None, None) => Outcome {
                    stdout: String::new(),
                    stderr: format!(
                        "kabelsalat: bringing up Android in group '{}' (about 20 s); read \
                         {ENV_ANDROID_CTL} from `tmux show-environment` once it is there\n",
                        display_name(target)
                    ),
                    code: EXIT_OK,
                    action: Some(Action::OpenAndroid {
                        group_uuid: target.uuid.clone(),
                    }),
                },
                (None, Some(_)) => Outcome::fail(
                    EXIT_GROUP,
                    format!(
                        "group '{group}' has no Android pane; run `kabelsalat android` first\n"
                    ),
                ),
            }
        }
        // Reaching the GUI means it is running, and then the sessions are
        // its to manage: nothing to do, and nothing wrong.
        Cli::Resume => Outcome {
            stdout: String::new(),
            stderr: "kabelsalat: the GUI is running and manages the sessions itself; \
                     nothing to resume\n"
                .into(),
            code: EXIT_OK,
            action: None,
        },
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
        // lib.rs fills the default before forwarding, as for browser.
        Cli::OverviewRoot { group: None, .. } | Cli::OverviewIssues { group: None } => {
            Outcome::fail(EXIT_USAGE, format!("{}\n", overview_group_missing()))
        }
        Cli::OverviewRoot {
            group: Some(group),
            root,
        } => {
            let target = match resolve_group(groups, group) {
                Ok(found) => found,
                Err(err) => return resolve_failure(group, err),
            };
            if let Some(refused) = overview_remote_refusal(group, target) {
                return refused;
            }
            Outcome {
                stdout: String::new(),
                stderr: String::new(),
                code: EXIT_OK,
                action: Some(Action::SetOverviewRoot {
                    group_uuid: target.uuid.clone(),
                    // `join` with an absolute path replaces the base, so this
                    // handles both absolute and relative directories. The
                    // lexical cleanup makes the stored root the path gio
                    // later reports events under.
                    root: root
                        .as_ref()
                        .map(|dir| normalize_lexically(&caller_cwd.join(dir))),
                }),
            }
        }
        // Answered from the snapshot: the GUI publishes its issues with the
        // rest of the group, so there is nothing to ask for.
        Cli::OverviewIssues { group: Some(group) } => {
            let target = match resolve_group(groups, group) {
                Ok(found) => found,
                Err(err) => return resolve_failure(group, err),
            };
            if let Some(refused) = overview_remote_refusal(group, target) {
                return refused;
            }
            Outcome::ok(target.issues.iter().map(issue_line).collect())
        }
    }
}

/// The overview is drawn locally from a local directory, and a remote
/// group has none (spec §2): the refusal both `overview` commands share.
fn overview_remote_refusal(group: &str, target: &GroupInfo) -> Option<Outcome> {
    let host = target.host.as_ref()?;
    Some(Outcome::fail(
        EXIT_GROUP,
        format!("the overview is local, but '{group}' runs on {host}\n"),
    ))
}

/// One `overview issues` line: kind, node ids joined by `,`, file or
/// nothing, detail — tab-separated, newline-terminated. A tab or newline
/// inside a field would break the format, so they become spaces.
fn issue_line(issue: &IssueLine) -> String {
    let field = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    format!(
        "{}\t{}\t{}\t{}\n",
        field(&issue.kind),
        field(&issue.nodes.join(",")),
        issue
            .file
            .as_ref()
            .map(|f| field(&f.to_string_lossy()))
            .unwrap_or_default(),
        field(&issue.detail)
    )
}

/// The one check `dispatch` cannot make: the overview root must exist. The
/// caller passes the filesystem (`Path::is_dir`); tests pass a closure.
/// `Some` is the outcome to print and exit with instead of acting; `None`
/// means go ahead. Only a `SetOverviewRoot` with a directory is checked.
pub fn check_overview_root(action: &Action, is_dir: impl Fn(&Path) -> bool) -> Option<Outcome> {
    match action {
        Action::SetOverviewRoot {
            root: Some(root), ..
        } if !is_dir(root) => Some(Outcome::fail(
            EXIT_USAGE,
            format!("kabelsalat: no such directory: {}\n", root.display()),
        )),
        _ => None,
    }
}

/// `path` with `.` dropped and `..` resolved against the preceding component,
/// lexically, the way glib's `g_canonicalize_filename` does it: gio reports
/// monitor events under that form, so a stored root must match it or later
/// events never find their source. Nothing is read from the filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // `/..` is `/`.
                Some(Component::RootDir) => {}
                // A leading `..` of a relative path has nothing to cancel.
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    out
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

/// A group's name for messages, or its uuid when it has none.
fn display_name(group: &GroupInfo) -> &str {
    if group.name.is_empty() {
        &group.uuid
    } else {
        &group.name
    }
}

/// `cmd` with a relative screenshot path resolved against the caller's
/// directory: the compositor writes the file from inside the GUI process,
/// whose directory is not the caller's.
fn with_absolute_path(cmd: &AndroidCmd, caller_cwd: &Path) -> AndroidCmd {
    match cmd {
        AndroidCmd::Screenshot(path) => AndroidCmd::Screenshot(caller_cwd.join(path)),
        other => other.clone(),
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
                host: None,
                cdp: None,
                android: None,
                ..Default::default()
            },
            GroupInfo {
                uuid: "bbb-222".into(),
                name: "api".into(),
                tabs: 1,
                host: None,
                cdp: None,
                android: None,
                ..Default::default()
            },
            GroupInfo {
                uuid: "ccc-333".into(),
                name: "api".into(),
                tabs: 3,
                host: None,
                cdp: None,
                android: None,
                ..Default::default()
            },
            GroupInfo {
                uuid: "ddd-444".into(),
                name: String::new(),
                tabs: 1,
                host: None,
                cdp: None,
                android: None,
                ..Default::default()
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
                host: None,
                cdp: None,
                android: None,
                ..Default::default()
            },
            GroupInfo {
                uuid: "qqq".into(),
                name: "xyz".into(),
                tabs: 1,
                host: None,
                cdp: None,
                android: None,
                ..Default::default()
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

    fn spawn_of(out: &Outcome) -> (&GroupTarget, &Option<PathBuf>, &Vec<String>) {
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
        assert_eq!(*cwd, Some(PathBuf::from("/home/u/proj")));
        assert_eq!(*argv, vec!["claude".to_string()]);
        assert_eq!(out.code, EXIT_OK);
    }

    #[test]
    fn an_absolute_cwd_flag_replaces_the_callers_cwd() {
        let cli = run("web", Some("/srv/app"), &["ls"]);
        let out = dispatch(&cli, &sample_groups(), Path::new("/home/u/proj"));
        assert_eq!(*spawn_of(&out).1, Some(PathBuf::from("/srv/app")));
    }

    #[test]
    fn a_relative_cwd_flag_is_resolved_against_the_caller() {
        let cli = run("web", Some("sub/dir"), &["ls"]);
        let out = dispatch(&cli, &sample_groups(), Path::new("/home/u/proj"));
        assert_eq!(
            *spawn_of(&out).1,
            Some(PathBuf::from("/home/u/proj/sub/dir"))
        );
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

    // --- remote groups ---

    fn remote_groups() -> Vec<GroupInfo> {
        vec![
            GroupInfo {
                uuid: "rrr-555".into(),
                name: "box".into(),
                tabs: 1,
                host: Some("me@box".into()),
                cdp: None,
                android: None,
                ..Default::default()
            },
            GroupInfo {
                uuid: "lll-666".into(),
                name: "here".into(),
                tabs: 1,
                host: None,
                cdp: None,
                android: None,
                ..Default::default()
            },
        ]
    }

    #[test]
    fn run_on_a_remote_group_ignores_the_callers_cwd() {
        let out = dispatch(
            &run("box", None, &["ls"]),
            &remote_groups(),
            Path::new("/home/u/proj"),
        );
        let (group, cwd, argv) = spawn_of(&out);
        assert_eq!(*group, GroupTarget::Existing("rrr-555".into()));
        // None = the remote home; the local directory means nothing there.
        assert_eq!(*cwd, None);
        assert_eq!(*argv, vec!["ls".to_string()]);
    }

    #[test]
    fn run_on_a_remote_group_passes_cwd_through_unchecked() {
        // Relative stays relative (to the remote home), absolute stays as
        // typed; neither is joined onto the caller's directory.
        for dir in ["src/app", "/srv/app"] {
            let out = dispatch(
                &run("box", Some(dir), &["ls"]),
                &remote_groups(),
                Path::new("/home/u/proj"),
            );
            assert_eq!(*spawn_of(&out).1, Some(PathBuf::from(dir)));
        }
    }

    #[test]
    fn run_on_a_local_group_next_to_a_remote_one_is_unchanged() {
        let out = dispatch(
            &run("here", None, &["ls"]),
            &remote_groups(),
            Path::new("/home/u/proj"),
        );
        assert_eq!(*spawn_of(&out).1, Some(PathBuf::from("/home/u/proj")));
    }

    #[test]
    fn create_still_makes_a_local_group() {
        let out = dispatch(
            &run_create("fresh", &["ls"]),
            &remote_groups(),
            Path::new("/w"),
        );
        let (group, cwd, _) = spawn_of(&out);
        assert_eq!(
            *group,
            GroupTarget::Create {
                name: "fresh".into()
            }
        );
        assert_eq!(*cwd, Some(PathBuf::from("/w")));
    }

    #[test]
    fn help_explains_cwd_on_remote_groups() {
        assert!(help_text().contains("remote"));
    }

    // --- browser and resume ---

    #[test]
    fn browser_parses_with_and_without_a_group() {
        assert_eq!(parse(&args(&["browser"])), Ok(Cli::Browser { group: None }));
        assert_eq!(
            parse(&args(&["browser", "-g", "web"])),
            Ok(Cli::Browser {
                group: Some("web".into())
            })
        );
        assert_eq!(
            parse(&args(&["browser", "--group", "aaa-111"])),
            Ok(Cli::Browser {
                group: Some("aaa-111".into())
            })
        );
    }

    #[test]
    fn browser_rejects_stray_arguments() {
        assert!(parse(&args(&["browser", "--group"])).is_err());
        assert!(parse(&args(&["browser", "-g", ""])).is_err());
        assert!(parse(&args(&["browser", "extra"])).is_err());
        assert!(parse(&args(&["browser", "-g", "web", "extra"])).is_err());
    }

    #[test]
    fn resume_takes_no_arguments_and_needs_no_instance() {
        assert_eq!(parse(&args(&["resume"])), Ok(Cli::Resume));
        assert!(parse(&args(&["resume", "now"])).is_err());
        assert!(!Cli::Resume.needs_instance());
        assert!(Cli::Browser { group: None }.needs_instance());
    }

    #[test]
    fn the_callers_group_variable_fills_a_missing_group() {
        let filled = with_default_group(Cli::Browser { group: None }, Some("aaa-111"));
        assert_eq!(
            filled,
            Ok(Cli::Browser {
                group: Some("aaa-111".into())
            })
        );
        // An explicit --group wins over the environment.
        let explicit = with_default_group(
            Cli::Browser {
                group: Some("web".into()),
            },
            Some("aaa-111"),
        );
        assert_eq!(
            explicit,
            Ok(Cli::Browser {
                group: Some("web".into())
            })
        );
        // Outside a kabelsalat tab there is nothing to default to.
        assert!(with_default_group(Cli::Browser { group: None }, None).is_err());
        assert!(with_default_group(Cli::Browser { group: None }, Some("")).is_err());
        // Other commands pass through untouched.
        assert_eq!(with_default_group(Cli::Groups, None), Ok(Cli::Groups));
    }

    fn browser_groups() -> Vec<GroupInfo> {
        vec![
            GroupInfo {
                uuid: "aaa-111".into(),
                name: "web".into(),
                tabs: 2,
                host: None,
                cdp: Some("http://127.0.0.1:40455".into()),
                android: None,
                ..Default::default()
            },
            GroupInfo {
                uuid: "bbb-222".into(),
                name: "api".into(),
                tabs: 1,
                host: None,
                cdp: None,
                android: None,
                ..Default::default()
            },
            GroupInfo {
                uuid: "rrr-555".into(),
                name: "box".into(),
                tabs: 1,
                host: Some("me@box".into()),
                cdp: None,
                android: None,
                ..Default::default()
            },
        ]
    }

    fn browser(group: &str) -> Cli {
        Cli::Browser {
            group: Some(group.into()),
        }
    }

    #[test]
    fn browser_prints_a_live_endpoint_without_asking_the_gui() {
        let out = dispatch(&browser("web"), &browser_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stdout, "http://127.0.0.1:40455\n");
        assert!(out.action.is_none());
    }

    #[test]
    fn browser_asks_the_gui_to_open_a_missing_one() {
        let out = dispatch(&browser("bbb-222"), &browser_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stdout, "");
        assert_eq!(
            out.action,
            Some(Action::OpenBrowser {
                group_uuid: "bbb-222".into()
            })
        );
    }

    #[test]
    fn browser_refuses_a_remote_group() {
        let out = dispatch(&browser("box"), &browser_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.action.is_none());
        assert!(out.stderr.contains("me@box"), "stderr was: {}", out.stderr);
    }

    #[test]
    fn browser_on_an_unknown_group_fails_like_run() {
        let out = dispatch(&browser("nope"), &browser_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.action.is_none());
    }

    #[test]
    fn a_browser_without_any_group_is_a_usage_error_at_dispatch_too() {
        // control.rs fills the default first; this is the belt to that brace.
        let out = dispatch(
            &Cli::Browser { group: None },
            &browser_groups(),
            Path::new("/w"),
        );
        assert_eq!(out.code, EXIT_USAGE);
        assert!(out.action.is_none());
    }

    #[test]
    fn resume_against_a_running_gui_is_a_noop_success() {
        let out = dispatch(&Cli::Resume, &browser_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert!(out.action.is_none());
        assert!(!out.stderr.is_empty());
    }

    #[test]
    fn help_lists_browser_and_resume() {
        assert!(help_text().contains("kabelsalat browser"));
        assert!(help_text().contains("kabelsalat resume"));
        assert!(help_text().contains(&format!("{EXIT_FAILED} ")));
    }

    // --- session environment ---

    #[test]
    fn a_session_always_names_its_group() {
        assert_eq!(
            session_env_pairs("aaa-111", None, None),
            vec![(ENV_GROUP, "aaa-111".to_string())]
        );
    }

    #[test]
    fn a_live_browser_adds_the_cdp_pair() {
        assert_eq!(
            session_env_pairs("aaa-111", Some("http://127.0.0.1:40455"), None),
            vec![
                (ENV_GROUP, "aaa-111".to_string()),
                ("KABELSALAT_CDP", "http://127.0.0.1:40455".to_string()),
                (
                    "PLAYWRIGHT_MCP_CDP_ENDPOINT",
                    "http://127.0.0.1:40455".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_booted_android_adds_its_control_socket_and_serial() {
        assert_eq!(
            session_env_pairs(
                "aaa-111",
                None,
                Some(("/tmp/ctl.sock", "192.168.240.112:5555"))
            ),
            vec![
                (ENV_GROUP, "aaa-111".to_string()),
                ("KABELSALAT_ANDROID_CTL", "/tmp/ctl.sock".to_string()),
                ("KABELSALAT_ANDROID_ADB", "192.168.240.112:5555".to_string()),
            ]
        );
        assert_eq!(
            android_env_pairs("/tmp/ctl.sock", "10.0.3.9:5555"),
            [
                (ENV_ANDROID_CTL, "/tmp/ctl.sock".to_string()),
                (ENV_ANDROID_ADB, "10.0.3.9:5555".to_string()),
            ]
        );
    }

    #[test]
    fn group_comes_first_then_the_cdp_pair_then_the_android_pair() {
        assert_eq!(
            session_env_pairs(
                "aaa-111",
                Some("http://127.0.0.1:40455"),
                Some(("/tmp/ctl.sock", "10.0.3.9:5555"))
            ),
            vec![
                (ENV_GROUP, "aaa-111".to_string()),
                (ENV_CDP, "http://127.0.0.1:40455".to_string()),
                (ENV_CDP_PLAYWRIGHT, "http://127.0.0.1:40455".to_string()),
                (ENV_ANDROID_CTL, "/tmp/ctl.sock".to_string()),
                (ENV_ANDROID_ADB, "10.0.3.9:5555".to_string()),
            ]
        );
    }

    // --- android ---

    fn android(group: &str, cmd: Option<AndroidCmd>) -> Cli {
        Cli::Android {
            group: Some(group.into()),
            cmd,
        }
    }

    #[test]
    fn android_parses_with_and_without_a_group() {
        assert_eq!(
            parse(&args(&["android"])),
            Ok(Cli::Android {
                group: None,
                cmd: None
            })
        );
        assert_eq!(
            parse(&args(&["android", "-g", "web"])),
            Ok(android("web", None))
        );
        assert_eq!(
            parse(&args(&["android", "--group", "aaa-111", "tap", "10", "20"])),
            Ok(android("aaa-111", Some(AndroidCmd::Tap(10, 20))))
        );
    }

    #[test]
    fn android_parses_every_subcommand() {
        let cases = [
            (
                vec!["screenshot", "/tmp/a b.png"],
                AndroidCmd::Screenshot(PathBuf::from("/tmp/a b.png")),
            ),
            (vec!["tap", "0", "1080"], AndroidCmd::Tap(0, 1080)),
            (
                vec!["type", "hello world"],
                AndroidCmd::Type("hello world".into()),
            ),
            (vec!["key", "enter"], AndroidCmd::Key("enter".into())),
            (vec!["resize", "720", "1280"], AndroidCmd::Resize(720, 1280)),
        ];
        for (tail, cmd) in cases {
            let mut argv = vec!["android"];
            argv.extend(tail);
            assert_eq!(
                parse(&args(&argv)),
                Ok(Cli::Android {
                    group: None,
                    cmd: Some(cmd)
                })
            );
        }
    }

    #[test]
    fn the_group_flag_only_counts_before_the_subcommand() {
        // Text to type is never read as a flag.
        assert_eq!(
            parse(&args(&["android", "type", "-g"])),
            Ok(Cli::Android {
                group: None,
                cmd: Some(AndroidCmd::Type("-g".into()))
            })
        );
    }

    #[test]
    fn a_control_timeout_names_the_bound() {
        let timed_out = control_error(
            "/tmp/ctl.sock",
            std::io::ErrorKind::TimedOut,
            "connection timed out",
            10,
        );
        assert_eq!(
            timed_out,
            "kabelsalat: the Android pane did not answer on /tmp/ctl.sock: timed out after 10 s\n"
        );
        let would_block = control_error(
            "/tmp/ctl.sock",
            std::io::ErrorKind::WouldBlock,
            "Resource temporarily unavailable (os error 11)",
            10,
        );
        assert_eq!(would_block, timed_out);
        assert_eq!(
            control_error(
                "/tmp/ctl.sock",
                std::io::ErrorKind::NotFound,
                "No such file or directory (os error 2)",
                10,
            ),
            "kabelsalat: the Android pane did not answer on /tmp/ctl.sock: \
             No such file or directory (os error 2)\n"
        );
    }

    #[test]
    fn android_help_is_help() {
        for argv in [
            vec!["android", "-h"],
            vec!["android", "--help"],
            vec!["android", "-g", "web", "--help"],
        ] {
            assert_eq!(parse(&args(&argv)), Ok(Cli::Help), "{argv:?}");
        }
        // Past the verb it is an argument: `type --help` types it.
        assert_eq!(
            parse(&args(&["android", "type", "--help"])),
            Ok(Cli::Android {
                group: None,
                cmd: Some(AndroidCmd::Type("--help".into())),
            })
        );
    }

    #[test]
    fn android_screenshot_paths_the_pane_would_mangle_are_usage_errors() {
        // The pane trims the line, and a path that was not UTF-8 reaches us
        // with replacement characters: neither names the file asked for.
        for path in [" shot.png", "shot.png ", "\tshot.png", "sh\u{FFFD}t.png"] {
            let err = parse(&args(&["android", "screenshot", path]));
            assert!(err.is_err(), "{path:?} should be a usage error");
        }
        assert!(parse(&args(&["android", "screenshot", "my shot.png"])).is_ok());
    }

    #[test]
    fn android_usage_errors() {
        let bad = [
            vec!["android", "-g"],
            vec!["android", "-g", ""],
            vec!["android", "swipe", "1", "2"],
            vec!["android", "tap", "1"],
            vec!["android", "tap", "1", "2", "3"],
            vec!["android", "tap", "x", "2"],
            vec!["android", "tap", "-1", "2"],
            vec!["android", "tap", "1", "2", "-g", "web"],
            vec!["android", "resize", "0", "10"],
            vec!["android", "resize", "10"],
            vec!["android", "resize", "3000000000", "10"],
            vec!["android", "type"],
            vec!["android", "type", ""],
            vec!["android", "type", "a\nb"],
            vec!["android", "key", ""],
            vec!["android", "key", "two words"],
            vec!["android", "screenshot"],
            vec!["android", "screenshot", ""],
            vec!["android", "screenshot", "a\nb.png"],
        ];
        for argv in bad {
            assert!(
                parse(&args(&argv)).is_err(),
                "{argv:?} should be a usage error"
            );
        }
    }

    #[test]
    fn android_argv_round_trips_through_parse() {
        let cmds = [
            None,
            Some(AndroidCmd::Screenshot(PathBuf::from("/tmp/a b.png"))),
            Some(AndroidCmd::Tap(3, 4)),
            Some(AndroidCmd::Type("-g x".into())),
            Some(AndroidCmd::Key("enter".into())),
            Some(AndroidCmd::Resize(720, 1280)),
        ];
        for cmd in cmds {
            let argv = android_argv("kabelsalat", "aaa-111", cmd.as_ref());
            assert_eq!(parse(&argv), Ok(android("aaa-111", cmd)));
        }
    }

    #[test]
    fn control_lines_follow_the_pane_protocol() {
        assert_eq!(
            control_line(&AndroidCmd::Screenshot("/tmp/a b.png".into())),
            "screenshot /tmp/a b.png"
        );
        assert_eq!(control_line(&AndroidCmd::Tap(10, 20)), "click 10 20");
        assert_eq!(
            control_line(&AndroidCmd::Type("hello world".into())),
            "type hello world"
        );
        assert_eq!(control_line(&AndroidCmd::Key("enter".into())), "key enter");
        assert_eq!(
            control_line(&AndroidCmd::Resize(720, 1280)),
            "resize 720 1280"
        );
    }

    #[test]
    fn control_replies_become_outcomes() {
        let ok = control_reply("ok\n");
        assert_eq!(
            (ok.code, ok.stdout.as_str(), ok.stderr.as_str()),
            (EXIT_OK, "", "")
        );
        let data = control_reply("ok /tmp/a.png\n");
        assert_eq!((data.code, data.stdout.as_str()), (EXIT_OK, "/tmp/a.png\n"));
        let err = control_reply("err no frame yet\n");
        assert_eq!(err.code, EXIT_FAILED);
        assert_eq!(err.stderr, "kabelsalat: android: no frame yet\n");
        let garbage = control_reply("what\n");
        assert_eq!(garbage.code, EXIT_FAILED);
        assert!(garbage.stderr.contains("what"), "{}", garbage.stderr);
        assert!(ok.action.is_none() && err.action.is_none());
    }

    #[test]
    fn the_callers_group_variable_fills_android_too() {
        let filled = with_default_group(
            Cli::Android {
                group: None,
                cmd: Some(AndroidCmd::Key("enter".into())),
            },
            Some("aaa-111"),
        );
        assert_eq!(
            filled,
            Ok(android("aaa-111", Some(AndroidCmd::Key("enter".into()))))
        );
        assert!(
            with_default_group(
                Cli::Android {
                    group: None,
                    cmd: None
                },
                None
            )
            .is_err()
        );
        assert!(
            Cli::Android {
                group: None,
                cmd: None
            }
            .needs_instance()
        );
    }

    fn android_groups() -> Vec<GroupInfo> {
        vec![
            GroupInfo {
                uuid: "aaa-111".into(),
                name: "web".into(),
                tabs: 2,
                host: None,
                cdp: None,
                android: Some(AndroidInfo {
                    ctl: PathBuf::from("/tmp/ctl.sock"),
                    adb: Some("192.168.240.112:5555".into()),
                }),
                ..Default::default()
            },
            GroupInfo {
                uuid: "bbb-222".into(),
                name: "api".into(),
                tabs: 1,
                host: None,
                cdp: None,
                android: None,
                ..Default::default()
            },
            GroupInfo {
                uuid: "rrr-555".into(),
                name: "box".into(),
                tabs: 1,
                host: Some("me@box".into()),
                cdp: None,
                android: None,
                ..Default::default()
            },
        ]
    }

    #[test]
    fn android_prints_its_endpoints_when_owned_and_booted() {
        let out = dispatch(&android("web", None), &android_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stdout, "ctl=/tmp/ctl.sock\nadb=192.168.240.112:5555\n");
        assert!(out.action.is_none());
    }

    #[test]
    fn android_still_booting_prints_nothing_and_asks_nothing() {
        let mut groups = android_groups();
        if let Some(info) = groups[0].android.as_mut() {
            info.adb = None;
        }
        let out = dispatch(&android("web", None), &groups, Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stdout, "");
        assert!(out.stderr.contains("booting"), "{}", out.stderr);
        assert!(out.action.is_none());
    }

    #[test]
    fn android_asks_the_gui_when_nobody_owns_it() {
        // browser_groups(): nobody has an Android.
        let out = dispatch(&android("api", None), &browser_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stdout, "");
        assert_eq!(
            out.action,
            Some(Action::OpenAndroid {
                group_uuid: "bbb-222".into()
            })
        );
    }

    #[test]
    fn android_refuses_while_another_group_owns_it() {
        for cmd in [None, Some(AndroidCmd::Tap(1, 2))] {
            let out = dispatch(&android("api", cmd), &android_groups(), Path::new("/w"));
            assert_eq!(out.code, EXIT_GROUP);
            assert!(out.action.is_none());
            assert!(out.stderr.contains("web"), "{}", out.stderr);
        }
    }

    #[test]
    fn android_refuses_a_remote_group() {
        let out = dispatch(&android("box", None), &browser_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.action.is_none());
        assert!(out.stderr.contains("me@box"), "{}", out.stderr);
    }

    #[test]
    fn android_on_an_unknown_group_or_without_one_fails() {
        let out = dispatch(&android("nope", None), &android_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_GROUP);
        let out = dispatch(
            &Cli::Android {
                group: None,
                cmd: None,
            },
            &android_groups(),
            Path::new("/w"),
        );
        assert_eq!(out.code, EXIT_USAGE);
        assert!(out.action.is_none());
    }

    #[test]
    fn a_subcommand_without_an_owned_pane_is_exit_3() {
        let out = dispatch(
            &android("api", Some(AndroidCmd::Tap(1, 2))),
            &browser_groups(),
            Path::new("/w"),
        );
        assert_eq!(out.code, EXIT_GROUP);
        assert!(out.action.is_none());
    }

    #[test]
    fn a_subcommand_sends_one_line_to_the_owned_pane() {
        let out = dispatch(
            &android("web", Some(AndroidCmd::Tap(10, 20))),
            &android_groups(),
            Path::new("/w"),
        );
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(
            out.action,
            Some(Action::Control {
                socket: PathBuf::from("/tmp/ctl.sock"),
                line: "click 10 20".into(),
            })
        );
    }

    #[test]
    fn a_relative_screenshot_path_is_the_callers() {
        let out = dispatch(
            &android("web", Some(AndroidCmd::Screenshot("shots/a.png".into()))),
            &android_groups(),
            Path::new("/home/u/proj"),
        );
        assert_eq!(
            out.action,
            Some(Action::Control {
                socket: PathBuf::from("/tmp/ctl.sock"),
                line: "screenshot /home/u/proj/shots/a.png".into(),
            })
        );
    }

    #[test]
    fn help_lists_android() {
        assert!(help_text().contains("kabelsalat android [-g <group>]"));
        assert!(help_text().contains("screenshot PATH"));
    }

    // --- overview ---

    fn overview_root(group: &str, root: Option<&str>) -> Cli {
        Cli::OverviewRoot {
            group: Some(group.into()),
            root: root.map(PathBuf::from),
        }
    }

    fn overview_issues(group: &str) -> Cli {
        Cli::OverviewIssues {
            group: Some(group.into()),
        }
    }

    #[test]
    fn overview_root_parses_a_directory_with_or_without_a_group() {
        assert_eq!(
            parse(&args(&["overview", "root", "docs/plan"])),
            Ok(Cli::OverviewRoot {
                group: None,
                root: Some(PathBuf::from("docs/plan")),
            })
        );
        assert_eq!(
            parse(&args(&["overview", "root", "-g", "web", "/abs"])),
            Ok(overview_root("web", Some("/abs")))
        );
        // The group flag may follow the directory too.
        assert_eq!(
            parse(&args(&["overview", "root", "/abs", "--group", "web"])),
            Ok(overview_root("web", Some("/abs")))
        );
    }

    #[test]
    fn overview_root_clear_unsets_the_root() {
        assert_eq!(
            parse(&args(&["overview", "root", "--clear"])),
            Ok(Cli::OverviewRoot {
                group: None,
                root: None,
            })
        );
        assert_eq!(
            parse(&args(&["overview", "root", "-g", "web", "--clear"])),
            Ok(overview_root("web", None))
        );
    }

    #[test]
    fn overview_issues_parses_with_or_without_a_group() {
        assert_eq!(
            parse(&args(&["overview", "issues"])),
            Ok(Cli::OverviewIssues { group: None })
        );
        assert_eq!(
            parse(&args(&["overview", "issues", "-g", "web"])),
            Ok(overview_issues("web"))
        );
    }

    #[test]
    fn overview_help_is_help() {
        assert_eq!(parse(&args(&["overview", "-h"])), Ok(Cli::Help));
        assert_eq!(parse(&args(&["overview", "--help"])), Ok(Cli::Help));
    }

    #[test]
    fn overview_rejects_bad_forms() {
        let bad: Vec<Vec<&str>> = vec![
            vec!["overview"],
            vec!["overview", "frobnicate"],
            vec!["overview", "root"],
            vec!["overview", "root", ""],
            vec!["overview", "root", "a", "b"],
            vec!["overview", "root", "a", "--clear"],
            vec!["overview", "root", "--clear", "--clear"],
            vec!["overview", "root", "-g"],
            vec!["overview", "root", "-g", "", "a"],
            vec!["overview", "root", "--bogus", "a"],
            vec!["overview", "issues", "extra"],
            vec!["overview", "issues", "-g"],
            vec!["overview", "issues", "-g", ""],
            vec!["overview", "issues", "--clear"],
        ];
        for argv in bad {
            assert!(
                parse(&args(&argv)).is_err(),
                "{argv:?} should be a usage error"
            );
        }
    }

    #[test]
    fn overview_needs_an_instance() {
        assert!(
            Cli::OverviewRoot {
                group: None,
                root: None
            }
            .needs_instance()
        );
        assert!(Cli::OverviewIssues { group: None }.needs_instance());
    }

    #[test]
    fn the_callers_group_variable_fills_overview_too() {
        assert_eq!(
            with_default_group(
                Cli::OverviewRoot {
                    group: None,
                    root: Some(PathBuf::from("docs")),
                },
                Some("aaa-111"),
            ),
            Ok(overview_root("aaa-111", Some("docs")))
        );
        assert_eq!(
            with_default_group(Cli::OverviewIssues { group: None }, Some("aaa-111")),
            Ok(overview_issues("aaa-111"))
        );
        // An explicit --group wins over the environment.
        assert_eq!(
            with_default_group(overview_issues("web"), Some("aaa-111")),
            Ok(overview_issues("web"))
        );
        // Outside a kabelsalat tab there is nothing to default to.
        let unset = with_default_group(Cli::OverviewIssues { group: None }, None);
        assert_eq!(
            unset,
            Err(UsageError(format!(
                "overview needs --group outside a kabelsalat tab ({ENV_GROUP} is unset)"
            )))
        );
        assert!(
            with_default_group(
                Cli::OverviewRoot {
                    group: None,
                    root: None
                },
                Some("")
            )
            .is_err()
        );
    }

    #[test]
    fn overview_argv_round_trips_through_parse() {
        let clis = [
            overview_root("aaa-111", Some("docs/plan")),
            overview_root("aaa-111", Some("/abs/with space")),
            overview_root("aaa-111", None),
            overview_issues("aaa-111"),
        ];
        for cli in clis {
            let argv = overview_argv("kabelsalat", "aaa-111", &cli);
            assert_eq!(argv[0], "kabelsalat");
            assert_eq!(parse(&argv), Ok(cli));
        }
    }

    fn overview_groups() -> Vec<GroupInfo> {
        vec![
            GroupInfo {
                uuid: "aaa-111".into(),
                name: "web".into(),
                tabs: 2,
                overview_root: Some(PathBuf::from("/home/u/web/docs")),
                issues: vec![
                    IssueLine {
                        kind: "unknown-node".into(),
                        nodes: vec!["P-001".into(), "P-404".into()],
                        file: Some(PathBuf::from("/home/u/web/docs/p1.md")),
                        detail: "parent 'P-404' does not exist".into(),
                    },
                    IssueLine {
                        kind: "missing-link".into(),
                        nodes: vec!["A".into(), "B".into()],
                        file: None,
                        detail: "tab 'claude' links both;\tno edge\nbetween them".into(),
                    },
                ],
                ..Default::default()
            },
            GroupInfo {
                uuid: "bbb-222".into(),
                name: "api".into(),
                tabs: 1,
                ..Default::default()
            },
            GroupInfo {
                uuid: "rrr-555".into(),
                name: "box".into(),
                tabs: 1,
                host: Some("me@box".into()),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn an_overview_command_without_any_group_is_a_usage_error_at_dispatch_too() {
        let groups = overview_groups();
        let root = dispatch(
            &Cli::OverviewRoot {
                group: None,
                root: Some(PathBuf::from("docs")),
            },
            &groups,
            Path::new("/w"),
        );
        assert_eq!(root.code, EXIT_USAGE);
        assert!(root.action.is_none());
        assert!(
            root.stderr.contains(ENV_GROUP),
            "stderr was: {}",
            root.stderr
        );
        let issues = dispatch(
            &Cli::OverviewIssues { group: None },
            &groups,
            Path::new("/w"),
        );
        assert_eq!(issues.code, EXIT_USAGE);
        assert!(issues.action.is_none());
    }

    #[test]
    fn overview_root_resolves_a_relative_directory_against_the_caller() {
        let out = dispatch(
            &overview_root("web", Some("docs/plan")),
            &overview_groups(),
            Path::new("/home/u/proj"),
        );
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stdout, "");
        assert_eq!(out.stderr, "");
        assert_eq!(
            out.action,
            Some(Action::SetOverviewRoot {
                group_uuid: "aaa-111".into(),
                root: Some(PathBuf::from("/home/u/proj/docs/plan")),
            })
        );
    }

    #[test]
    fn overview_root_keeps_an_absolute_directory() {
        let out = dispatch(
            &overview_root("bbb-222", Some("/srv/notes")),
            &overview_groups(),
            Path::new("/home/u/proj"),
        );
        assert_eq!(
            out.action,
            Some(Action::SetOverviewRoot {
                group_uuid: "bbb-222".into(),
                root: Some(PathBuf::from("/srv/notes")),
            })
        );
    }

    #[test]
    fn overview_root_normalises_dot_and_dotdot_lexically() {
        // gio reports monitor events under the canonical path, so the stored
        // root must already be the clean one or later events never match.
        let out = dispatch(
            &overview_root("web", Some("../y/./z")),
            &overview_groups(),
            Path::new("/w/x"),
        );
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(
            out.action,
            Some(Action::SetOverviewRoot {
                group_uuid: "aaa-111".into(),
                root: Some(PathBuf::from("/w/y/z")),
            })
        );
        let absolute = dispatch(
            &overview_root("web", Some("/r/sub/../docs/.")),
            &overview_groups(),
            Path::new("/w/x"),
        );
        assert_eq!(
            absolute.action,
            Some(Action::SetOverviewRoot {
                group_uuid: "aaa-111".into(),
                root: Some(PathBuf::from("/r/docs")),
            })
        );
    }

    #[test]
    fn normalize_lexically_stops_at_the_root_and_keeps_plain_paths() {
        assert_eq!(
            normalize_lexically(Path::new("/a/../../b")),
            PathBuf::from("/b")
        );
        assert_eq!(normalize_lexically(Path::new("/")), PathBuf::from("/"));
        assert_eq!(
            normalize_lexically(Path::new("/srv/notes")),
            PathBuf::from("/srv/notes")
        );
        assert_eq!(
            normalize_lexically(Path::new("./a/../b")),
            PathBuf::from("b")
        );
        assert_eq!(
            normalize_lexically(Path::new("../a")),
            PathBuf::from("../a")
        );
    }

    #[test]
    fn overview_root_clear_asks_the_gui_to_unset() {
        let out = dispatch(
            &overview_root("web", None),
            &overview_groups(),
            Path::new("/w"),
        );
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(
            out.action,
            Some(Action::SetOverviewRoot {
                group_uuid: "aaa-111".into(),
                root: None,
            })
        );
    }

    #[test]
    fn overview_refuses_a_remote_group() {
        for cli in [overview_root("box", Some("docs")), overview_issues("box")] {
            let out = dispatch(&cli, &overview_groups(), Path::new("/w"));
            assert_eq!(out.code, EXIT_GROUP);
            assert!(out.action.is_none());
            assert_eq!(
                out.stderr,
                "the overview is local, but 'box' runs on me@box\n"
            );
        }
    }

    #[test]
    fn overview_on_an_unknown_group_fails_like_run() {
        for cli in [overview_root("nope", Some("docs")), overview_issues("nope")] {
            let out = dispatch(&cli, &overview_groups(), Path::new("/w"));
            assert_eq!(out.code, EXIT_GROUP);
            assert!(out.action.is_none());
            assert!(out.stderr.contains("nope"), "stderr was: {}", out.stderr);
        }
    }

    #[test]
    fn overview_issues_prints_tab_separated_lines_from_the_snapshot() {
        let out = dispatch(&overview_issues("web"), &overview_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert!(out.action.is_none());
        assert_eq!(out.stderr, "");
        assert_eq!(
            out.stdout,
            "unknown-node\tP-001,P-404\t/home/u/web/docs/p1.md\tparent 'P-404' does not exist\n\
             missing-link\tA,B\t\ttab 'claude' links both; no edge between them\n"
        );
    }

    #[test]
    fn overview_issues_with_none_prints_nothing_and_succeeds() {
        let out = dispatch(&overview_issues("api"), &overview_groups(), Path::new("/w"));
        assert_eq!(out.code, EXIT_OK);
        assert_eq!(out.stdout, "");
        assert_eq!(out.stderr, "");
        assert!(out.action.is_none());
    }

    #[test]
    fn check_overview_root_rejects_a_missing_directory() {
        let action = Action::SetOverviewRoot {
            group_uuid: "aaa-111".into(),
            root: Some(PathBuf::from("/nope/docs")),
        };
        let refused = check_overview_root(&action, |_| false).expect("an outcome");
        assert_eq!(refused.code, EXIT_USAGE);
        assert_eq!(
            refused.stderr,
            "kabelsalat: no such directory: /nope/docs\n"
        );
        assert_eq!(refused.stdout, "");
        assert!(refused.action.is_none());
    }

    #[test]
    fn check_overview_root_passes_an_existing_directory_and_other_actions() {
        let action = Action::SetOverviewRoot {
            group_uuid: "aaa-111".into(),
            root: Some(PathBuf::from("/home/u/docs")),
        };
        assert_eq!(
            check_overview_root(&action, |p| p == Path::new("/home/u/docs")),
            None
        );
        // --clear names no directory, so there is nothing to check.
        let clear = Action::SetOverviewRoot {
            group_uuid: "aaa-111".into(),
            root: None,
        };
        assert_eq!(check_overview_root(&clear, |_| false), None);
        let rename = Action::Rename {
            group_uuid: "aaa-111".into(),
            name: "x".into(),
        };
        assert_eq!(check_overview_root(&rename, |_| false), None);
    }

    #[test]
    fn help_lists_overview() {
        assert!(help_text().contains("kabelsalat overview root [-g <group>]"));
        assert!(help_text().contains("--clear"));
        assert!(help_text().contains("kabelsalat overview issues [-g <group>]"));
        assert!(help_text().contains("overview"));
    }
}
