//! Discovery of the Claude Code sessions running inside tabs.
//!
//! Claude Code registers every interactive process it starts in
//! `~/.claude/sessions/<pid>.json`: the session id, the directory it runs in,
//! and the process start time. Matching that registry against the process
//! tree under each tab's tmux pane tells which tab hosts which session, with
//! nothing to configure on the user's side — a `claude` typed into a plain
//! shell is found exactly like one opened with Ctrl+Shift+C, and a `/clear`
//! (which starts a new session id) is picked up on the next tick.
//!
//! Pure logic over two directories, the registry and a `/proc` root, so the
//! whole thing is testable with fabricated trees. No GTK, no tmux.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::state::ClaudeSession;

/// Longest parent chain followed from a claude process up to a pane shell.
/// A real chain is two or three deep; the bound only guards against a
/// corrupted `/proc` cycling.
const MAX_ANCESTORS: usize = 64;

/// The registry's per-process file, as far as discovery needs it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    pid: u32,
    session_id: String,
    cwd: PathBuf,
    /// The process start time as `/proc/<pid>/stat` reports it (field 22,
    /// clock ticks since boot). With it a pid reused after a reboot or an
    /// exit cannot pass for the process that registered.
    #[serde(default)]
    proc_start: Option<String>,
    /// Unix milliseconds; the newer registration wins when one pane has two.
    #[serde(default)]
    started_at: u64,
}

/// A registered claude process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub pid: u32,
    pub session: ClaudeSession,
    proc_start: Option<String>,
    started_at: u64,
}

/// Where Claude Code keeps its registry: `$CLAUDE_CONFIG_DIR/sessions`, or
/// `~/.claude/sessions`.
pub fn sessions_dir() -> Option<PathBuf> {
    sessions_dir_from(
        std::env::var_os("CLAUDE_CONFIG_DIR").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// `sessions_dir` over explicit `$CLAUDE_CONFIG_DIR` and `$HOME` values; an
/// empty config dir counts as unset, as it does for claude itself.
pub fn sessions_dir_from(config_dir: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let config = match config_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(home?).join(".claude"),
    };
    Some(config.join("sessions"))
}

/// What a tab knows after a discovery tick: the claude found under its pane
/// if there is one; nothing if the pane is live without one (the tab is a
/// shell again); and, for a dead pane, whatever it knew before — a crashed
/// pane has no live claude by definition, and what it had is exactly what
/// its restart should resume.
pub fn after_tick(
    previous: Option<ClaudeSession>,
    found: Option<ClaudeSession>,
    pane_dead: bool,
) -> Option<ClaudeSession> {
    match found {
        Some(session) => Some(session),
        None if pane_dead => previous,
        None => None,
    }
}

/// Parse one registry file. Anything malformed is `None`: the registry is
/// another program's private format, so a surprise must cost at most that
/// one entry. The id is kept only when it looks like a session id, because
/// it ends up on a command line.
pub fn parse_record(json: &str) -> Option<Record> {
    let entry: Entry = serde_json::from_str(json).ok()?;
    let plausible_id = !entry.session_id.is_empty()
        && entry
            .session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !plausible_id || !entry.cwd.is_absolute() {
        return None;
    }
    Some(Record {
        pid: entry.pid,
        session: ClaudeSession {
            id: entry.session_id,
            cwd: entry.cwd,
        },
        proc_start: entry.proc_start,
        started_at: entry.started_at,
    })
}

/// Every registered process that is still running: the registry outlives a
/// claude that was killed rather than exited, so each entry is checked
/// against `proc_root` (normally `/proc`) before it counts.
pub fn live_records(dir: &Path, proc_root: &Path) -> Vec<Record> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .filter_map(|json| parse_record(&json))
        .filter(|record| is_live(record, proc_root))
        .collect()
}

/// Whether the registered process is the one still running under its pid.
fn is_live(record: &Record, proc_root: &Path) -> bool {
    match (stat_fields(proc_root, record.pid), &record.proc_start) {
        (Some(fields), Some(start)) => fields.start_time == *start,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// The session running in each pane: `panes` maps a tab uuid to the pid of
/// its pane's shell, and a record belongs to the pane whose shell is one of
/// its ancestors (or the process itself, when claude *is* the pane command).
pub fn sessions_by_pane(
    records: &[Record],
    panes: &[(String, u32)],
    proc_root: &Path,
) -> HashMap<String, ClaudeSession> {
    let mut found: HashMap<String, (u64, ClaudeSession)> = HashMap::new();
    for record in records {
        let chain = ancestors(proc_root, record.pid);
        let Some((uuid, _)) = panes.iter().find(|(_, pid)| chain.contains(pid)) else {
            continue;
        };
        let newer = found
            .get(uuid)
            .is_none_or(|(started, _)| record.started_at > *started);
        if newer {
            found.insert(uuid.clone(), (record.started_at, record.session.clone()));
        }
    }
    found
        .into_iter()
        .map(|(uuid, (_, session))| (uuid, session))
        .collect()
}

/// `pid` and its parents up to init, nearest first.
fn ancestors(proc_root: &Path, pid: u32) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut current = pid;
    while chain.len() < MAX_ANCESTORS {
        let Some(fields) = stat_fields(proc_root, current) else {
            break;
        };
        if fields.ppid <= 1 || chain.contains(&fields.ppid) {
            break;
        }
        chain.push(fields.ppid);
        current = fields.ppid;
    }
    chain
}

struct StatFields {
    ppid: u32,
    start_time: String,
}

/// The parent pid and start time from `<proc_root>/<pid>/stat`.
fn stat_fields(proc_root: &Path, pid: u32) -> Option<StatFields> {
    let stat = fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    parse_stat(&stat)
}

/// The `comm` field is parenthesised and may itself contain spaces or
/// parentheses, so the fixed-width part starts after the *last* `)`:
/// state, ppid, … with the start time as the 22nd field of the whole line.
fn parse_stat(stat: &str) -> Option<StatFields> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let ppid = fields.nth(1)?.parse().ok()?;
    let start_time = fields.nth(17)?.to_string();
    Some(StatFields { ppid, start_time })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kabelsalat-claude-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake `/proc` entry: `comm` with a space in it, like a real one can.
    fn write_stat(proc_root: &Path, pid: u32, ppid: u32, start: &str) {
        let dir = proc_root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        let tail: Vec<String> = (5..=52).map(|i| i.to_string()).collect();
        let mut tail = tail;
        tail[22 - 5] = start.to_string();
        fs::write(
            dir.join("stat"),
            format!("{pid} (claude (x)) S {ppid} {}\n", tail.join(" ")),
        )
        .unwrap();
    }

    fn registry(dir: &Path, pid: u32, id: &str, cwd: &str, start: &str, started_at: u64) {
        fs::write(
            dir.join(format!("{pid}.json")),
            format!(
                r#"{{"pid":{pid},"sessionId":"{id}","cwd":"{cwd}","startedAt":{started_at},"procStart":"{start}","version":"2.1.287","status":"busy"}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn sessions_dir_prefers_claude_config_dir_over_home() {
        let cfg = OsStr::new("/cfg");
        let home = OsStr::new("/home/me");
        assert_eq!(
            sessions_dir_from(Some(cfg), Some(home)),
            Some(PathBuf::from("/cfg/sessions"))
        );
        assert_eq!(
            sessions_dir_from(None, Some(home)),
            Some(PathBuf::from("/home/me/.claude/sessions"))
        );
        // An empty CLAUDE_CONFIG_DIR is as good as unset.
        assert_eq!(
            sessions_dir_from(Some(OsStr::new("")), Some(home)),
            Some(PathBuf::from("/home/me/.claude/sessions"))
        );
        assert_eq!(sessions_dir_from(None, None), None);
    }

    #[test]
    fn after_tick_takes_what_is_found_and_keeps_a_crashed_tabs_session() {
        let old = ClaudeSession {
            id: "old".into(),
            cwd: PathBuf::from("/a"),
        };
        let new = ClaudeSession {
            id: "new".into(),
            cwd: PathBuf::from("/b"),
        };
        // A claude seen in the pane is the truth, whatever was known before.
        assert_eq!(
            after_tick(None, Some(new.clone()), false),
            Some(new.clone())
        );
        assert_eq!(
            after_tick(Some(old.clone()), Some(new.clone()), false),
            Some(new.clone())
        );
        assert_eq!(
            after_tick(Some(old.clone()), Some(new.clone()), true),
            Some(new)
        );
        // No claude under a live pane: the tab is a shell again.
        assert_eq!(after_tick(Some(old.clone()), None, false), None);
        assert_eq!(after_tick(None, None, false), None);
        // No claude under a dead pane means nothing: what it had is what a
        // restart resumes.
        assert_eq!(after_tick(Some(old.clone()), None, true), Some(old));
        assert_eq!(after_tick(None, None, true), None);
    }

    #[test]
    fn parse_record_reads_the_registry_shape() {
        let json = r#"{"pid":213,"sessionId":"7b776c86-7bbd-5685-bafa-ec6d54623792","cwd":"/home/user","startedAt":1790952027265,"procStart":"796","version":"2.1.287","kind":"interactive"}"#;
        let record = parse_record(json).unwrap();
        assert_eq!(record.pid, 213);
        assert_eq!(record.session.id, "7b776c86-7bbd-5685-bafa-ec6d54623792");
        assert_eq!(record.session.cwd, PathBuf::from("/home/user"));
        assert_eq!(record.proc_start.as_deref(), Some("796"));
        assert_eq!(record.started_at, 1790952027265);
    }

    #[test]
    fn parse_record_rejects_what_must_not_reach_a_command_line() {
        assert!(parse_record("not json").is_none());
        assert!(parse_record(r#"{"pid":1,"sessionId":"","cwd":"/x"}"#).is_none());
        assert!(parse_record(r#"{"pid":1,"sessionId":"a b","cwd":"/x"}"#).is_none());
        assert!(parse_record(r#"{"pid":1,"sessionId":"$(id)","cwd":"/x"}"#).is_none());
        assert!(parse_record(r#"{"pid":1,"sessionId":"ok","cwd":"rel"}"#).is_none());
        // Missing optional fields are fine.
        let record = parse_record(r#"{"pid":1,"sessionId":"ok-1_2","cwd":"/x"}"#).unwrap();
        assert_eq!(record.proc_start, None);
        assert_eq!(record.started_at, 0);
    }

    #[test]
    fn parse_stat_survives_a_comm_with_spaces_and_parens() {
        let fields = parse_stat(
            "42 (a (b) c) S 7 42 42 0 -1 4194560 1 2 3 4 5 6 7 8 20 0 1 0 796 1000 2 3\n",
        )
        .unwrap();
        assert_eq!(fields.ppid, 7);
        assert_eq!(fields.start_time, "796");
        assert!(parse_stat("garbage").is_none());
        assert!(parse_stat("1 (x) S 0").is_none());
    }

    #[test]
    fn live_records_drop_exited_and_reused_pids() {
        let dir = temp_dir("live");
        let registry_dir = dir.join("sessions");
        let proc_root = dir.join("proc");
        fs::create_dir_all(&registry_dir).unwrap();
        fs::create_dir_all(&proc_root).unwrap();
        registry(&registry_dir, 100, "alive", "/a", "500", 1);
        registry(&registry_dir, 200, "exited", "/b", "600", 2);
        registry(&registry_dir, 300, "reused", "/c", "700", 3);
        registry(&registry_dir, 400, "legacy", "/d", "", 4);
        write_stat(&proc_root, 100, 10, "500");
        write_stat(&proc_root, 300, 10, "9999"); // same pid, a different process
        write_stat(&proc_root, 400, 10, "800");
        fs::write(registry_dir.join("100.key"), "secret").unwrap(); // not a record
        fs::write(registry_dir.join("broken.json"), "{").unwrap();
        let mut ids: Vec<String> = live_records(&registry_dir, &proc_root)
            .into_iter()
            .map(|r| r.session.id)
            .collect();
        ids.sort();
        // "legacy" has a procStart of "" which cannot match; only a missing
        // procStart is trusted on pid existence alone.
        assert_eq!(ids, ["alive"]);
        assert!(live_records(&dir.join("missing"), &proc_root).is_empty());
    }

    #[test]
    fn live_record_without_proc_start_counts_while_its_pid_exists() {
        let dir = temp_dir("legacy");
        let registry_dir = dir.join("sessions");
        let proc_root = dir.join("proc");
        fs::create_dir_all(&registry_dir).unwrap();
        fs::write(
            registry_dir.join("5.json"),
            r#"{"pid":5,"sessionId":"old","cwd":"/x"}"#,
        )
        .unwrap();
        assert!(live_records(&registry_dir, &proc_root).is_empty());
        write_stat(&proc_root, 5, 1, "1");
        assert_eq!(live_records(&registry_dir, &proc_root).len(), 1);
    }

    #[test]
    fn sessions_by_pane_follows_the_parent_chain_to_the_pane_shell() {
        let proc_root = temp_dir("panes").join("proc");
        // tmux server 10 → shells 20 (tab a) and 30 (tab b).
        // Tab a: shell → claude 21. Tab b: shell → wrapper 31 → claude 32.
        // Tab c: the pane command *is* claude (40, under the server directly).
        // 50 is a claude outside every pane.
        write_stat(&proc_root, 10, 1, "1");
        write_stat(&proc_root, 20, 10, "2");
        write_stat(&proc_root, 21, 20, "3");
        write_stat(&proc_root, 30, 10, "4");
        write_stat(&proc_root, 31, 30, "5");
        write_stat(&proc_root, 32, 31, "6");
        write_stat(&proc_root, 40, 10, "7");
        write_stat(&proc_root, 50, 1, "8");
        let record = |pid: u32, id: &str, started_at: u64| Record {
            pid,
            session: ClaudeSession {
                id: id.into(),
                cwd: PathBuf::from("/p"),
            },
            proc_start: None,
            started_at,
        };
        let records = [
            record(21, "a-session", 1),
            record(32, "b-session", 2),
            record(40, "c-session", 3),
            record(50, "stray", 4),
        ];
        let panes = [
            ("a".to_string(), 20),
            ("b".to_string(), 30),
            ("c".to_string(), 40),
            ("d".to_string(), 60),
        ];
        let found = sessions_by_pane(&records, &panes, &ProcFs(&proc_root));
        let mut pairs: Vec<(String, String)> = found.into_iter().map(|(u, s)| (u, s.id)).collect();
        pairs.sort();
        assert_eq!(
            pairs,
            [
                ("a".to_string(), "a-session".to_string()),
                ("b".to_string(), "b-session".to_string()),
                ("c".to_string(), "c-session".to_string()),
            ]
        );
    }

    #[test]
    fn sessions_by_pane_keeps_the_newest_of_two_in_one_pane() {
        let proc_root = temp_dir("newest").join("proc");
        write_stat(&proc_root, 20, 1, "1");
        write_stat(&proc_root, 21, 20, "2");
        write_stat(&proc_root, 22, 20, "3");
        let record = |pid: u32, id: &str, started_at: u64| Record {
            pid,
            session: ClaudeSession {
                id: id.into(),
                cwd: PathBuf::from("/p"),
            },
            proc_start: None,
            started_at,
        };
        let panes = [("a".to_string(), 20)];
        let found = sessions_by_pane(
            &[record(22, "later", 9), record(21, "earlier", 1)],
            &panes,
            &ProcFs(&proc_root),
        );
        assert_eq!(found["a"].id, "later");
    }

    #[test]
    fn ancestors_stop_at_init_and_on_a_cycle() {
        let proc_root = temp_dir("cycle").join("proc");
        write_stat(&proc_root, 2, 1, "1");
        write_stat(&proc_root, 3, 2, "1");
        let tree = ProcFs(&proc_root);
        assert_eq!(ancestors(&tree, 3), [3, 2]);
        write_stat(&proc_root, 7, 8, "1");
        write_stat(&proc_root, 8, 7, "1");
        assert_eq!(ancestors(&tree, 7), [7, 8]);
        assert_eq!(ancestors(&tree, 99), [99]); // unknown pid: itself only
    }

    #[test]
    fn a_parent_map_is_a_process_tree_too() {
        // What a remote host prints: pid → ppid pairs, no /proc in sight.
        let tree: HashMap<u32, u32> = [(2, 1), (3, 2), (4, 3)].into_iter().collect();
        assert_eq!(ancestors(&tree, 4), [4, 3, 2]);
        assert_eq!(ancestors(&tree, 9), [9]);
        let record = Record {
            pid: 4,
            session: ClaudeSession {
                id: "s".into(),
                cwd: PathBuf::from("/p"),
            },
            proc_start: None,
            started_at: 0,
        };
        let found = sessions_by_pane(&[record], &[("t".to_string(), 2)], &tree);
        assert_eq!(found["t"].id, "s");
    }

    // --- remote hosts ---

    #[test]
    fn host_script_lists_panes_processes_and_the_registry() {
        let script = host_script();
        assert!(script.contains("list-panes -a -F"));
        assert!(script.contains("#{session_name}"));
        assert!(script.contains("#{pane_pid}"));
        assert!(script.contains("ps -eo pid=,ppid="));
        assert!(script.contains("${CLAUDE_CONFIG_DIR:-$HOME/.claude}/sessions/*.json"));
        // One line per record whatever the file's formatting.
        assert!(script.contains("tr -d '\\n'"));
    }

    #[test]
    fn sessions_from_host_listing_matches_like_the_local_path() {
        // tmux server 10 → shells 20 (tab a) and 30 (tab b); claude 21 under
        // a, claude 32 under a wrapper under b. 40 is a registered claude
        // whose process is gone (not in the ps listing). Pretty-printed and
        // compact records alike; unknown lines are ignored.
        let listing = "\
pane\tks-a\t20
pane\tks-b\t30
pane\tforeign\t99
proc\t10\t1
proc\t20\t10
proc\t21\t20
proc\t30\t10
proc\t31\t30
proc\t32\t31
record\t{\"pid\":21,\"sessionId\":\"a-session\",\"cwd\":\"/home/me/a\",\"startedAt\":5}
record\t{ \"pid\": 32, \"sessionId\": \"b-session\", \"cwd\": \"/home/me/b\" }
record\t{\"pid\":40,\"sessionId\":\"gone\",\"cwd\":\"/x\"}
record\tnot json
warning: something ssh printed
";
        let found = sessions_from_host_listing(listing);
        let mut pairs: Vec<(String, String, PathBuf)> =
            found.into_iter().map(|(u, s)| (u, s.id, s.cwd)).collect();
        pairs.sort();
        assert_eq!(
            pairs,
            [
                (
                    "a".to_string(),
                    "a-session".to_string(),
                    PathBuf::from("/home/me/a")
                ),
                (
                    "b".to_string(),
                    "b-session".to_string(),
                    PathBuf::from("/home/me/b")
                ),
            ]
        );
        assert!(sessions_from_host_listing("").is_empty());
    }
}
