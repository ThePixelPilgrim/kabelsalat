//! Discovery of the Codex sessions running inside tabs.
//!
//! Codex has no registry of running processes the way Claude Code does, but a
//! running session keeps its rollout file open for appending:
//! `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl`. A process
//! holding such a file is a codex session, the uuid in the name is the id
//! `codex resume` takes, and `/proc/<pid>/cwd` is the directory to resume in.
//! The records produced here feed the same pane matching as claude's
//! (`claude::sessions_by_pane`).
//!
//! Pure over a `/proc` root, or over the lines a remote host prints
//! ([`HOST_SNIPPET`]), so it is testable with fabricated trees.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::claude::Record;
use crate::state::AgentSession;

const UUID_LEN: usize = 36;

/// The session id of a rollout file, if `path` is one: a `rollout-*-<uuid>.jsonl`
/// below a `sessions` directory. The uuid is checked for shape because it
/// ends up on a command line.
pub fn session_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(UUID_LEN)?..)?;
    let below_sessions = path
        .parent()?
        .components()
        .any(|part| part.as_os_str() == "sessions");
    (below_sessions && is_uuid(id)).then(|| id.to_string())
}

/// The canonical 8-4-4-4-12 hex shape.
fn is_uuid(text: &str) -> bool {
    text.len() == UUID_LEN
        && text.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// The codex sessions running under `proc_root` (normally `/proc`): every
/// process with a rollout file open, started-at being that file's last
/// write so the busier of two sessions in one pane wins.
pub fn local_records(proc_root: &Path) -> Vec<Record> {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let (id, modified) = newest_rollout(&entry.path().join("fd"))?;
            let cwd = fs::read_link(entry.path().join("cwd")).ok()?;
            cwd.is_absolute()
                .then(|| Record::new(pid, AgentSession::codex(id, cwd), modified))
        })
        .collect()
}

/// The id and last-write time (unix ms) of the most recently written rollout
/// among a process's open files.
fn newest_rollout(fd_dir: &Path) -> Option<(String, u64)> {
    fs::read_dir(fd_dir)
        .ok()?
        .filter_map(|fd| fs::read_link(fd.ok()?.path()).ok())
        .filter_map(|target| {
            let id = session_id(&target)?;
            let modified = fs::metadata(&target).ok()?.modified().ok()?;
            let millis = modified.duration_since(UNIX_EPOCH).ok()?.as_millis();
            Some((id, millis as u64))
        })
        .max_by_key(|(_, modified)| *modified)
}

/// The POSIX `sh` fragment of the remote host script: one
/// `codex\t<pid>\t<rollout path>\t<cwd>` line per process with a rollout
/// file open. Needs a Linux `/proc`; elsewhere it prints nothing.
pub const HOST_SNIPPET: &str = r#"ls -l /proc/[0-9]*/fd 2>/dev/null | awk '/^\/proc\/[0-9]+\/fd:$/ { split($0, a, "/"); pid = a[3] } /-> .*\/rollout-.*\.jsonl$/ { print pid "\t" substr($0, index($0, "-> ") + 3) }' | sort -u | while IFS=$(printf '\t') read -r pid path; do printf 'codex\t%s\t%s\t%s\n' "$pid" "$path" "$(readlink /proc/$pid/cwd 2>/dev/null)"; done"#;

/// One line of [`HOST_SNIPPET`] output, without its `codex\t` tag. As in the
/// local path, a session counts only with a plausible id and an absolute cwd.
pub fn parse_host_line(rest: &str) -> Option<Record> {
    let mut fields = rest.splitn(3, '\t');
    let pid = fields.next()?.trim().parse().ok()?;
    let id = session_id(Path::new(fields.next()?))?;
    let cwd = PathBuf::from(fields.next()?.trim_end());
    cwd.is_absolute()
        .then(|| Record::new(pid, AgentSession::codex(id, cwd), 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const ID: &str = "0199a213-81c0-7800-8aa1-bbab2a035a53";

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ks-codex-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rollout(root: &Path, id: &str) -> PathBuf {
        let dir = root.join(".codex/sessions/2026/10/21");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("rollout-2026-10-21T09-30-00-{id}.jsonl"));
        fs::write(&path, "{}\n").unwrap();
        path
    }

    /// `<proc>/<pid>/fd/<n>` -> target, `<proc>/<pid>/cwd` -> cwd.
    fn process(proc_root: &Path, pid: u32, fds: &[&Path], cwd: &Path) {
        let dir = proc_root.join(pid.to_string());
        fs::create_dir_all(dir.join("fd")).unwrap();
        for (n, target) in fds.iter().enumerate() {
            symlink(target, dir.join("fd").join(n.to_string())).unwrap();
        }
        symlink(cwd, dir.join("cwd")).unwrap();
    }

    #[test]
    fn session_id_is_the_uuid_of_a_rollout_below_sessions() {
        let path =
            format!("/home/me/.codex/sessions/2026/10/21/rollout-2026-10-21T09-30-00-{ID}.jsonl");
        assert_eq!(session_id(Path::new(&path)).as_deref(), Some(ID));
    }

    #[test]
    fn session_id_rejects_everything_else() {
        for path in [
            // not below sessions
            format!("/tmp/rollout-2026-10-21T09-30-00-{ID}.jsonl"),
            // not a rollout
            format!("/home/me/.codex/sessions/2026/10/21/other-{ID}.jsonl"),
            format!("/home/me/.codex/sessions/2026/10/21/rollout-x-{ID}.txt"),
            // not a uuid: must never reach a command line
            "/home/me/.codex/sessions/rollout-x-$(id)aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.jsonl".into(),
            "/home/me/.codex/sessions/rollout-short.jsonl".into(),
        ] {
            assert_eq!(session_id(Path::new(&path)), None, "{path}");
        }
    }

    #[test]
    fn local_records_find_processes_holding_a_rollout_open() {
        let dir = temp_dir("local");
        let proc_root = dir.join("proc");
        let work = dir.join("work");
        fs::create_dir_all(&work).unwrap();
        let file = rollout(&dir, ID);
        let other = dir.join("notes.txt");
        fs::write(&other, "x").unwrap();
        process(&proc_root, 100, &[&other, &file], &work);
        process(&proc_root, 200, &[&other], &work); // no rollout
        fs::create_dir_all(proc_root.join("self")).unwrap(); // not a pid
        let records = local_records(&proc_root);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].pid, 100);
        assert_eq!(
            records[0].session,
            AgentSession::codex(ID.into(), work.clone())
        );
    }

    #[test]
    fn local_records_ignore_a_process_without_a_readable_cwd_or_fds() {
        let dir = temp_dir("unreadable");
        let proc_root = dir.join("proc");
        let file = rollout(&dir, ID);
        let pid_dir = proc_root.join("7");
        fs::create_dir_all(pid_dir.join("fd")).unwrap();
        symlink(&file, pid_dir.join("fd/0")).unwrap(); // no cwd link
        fs::create_dir_all(proc_root.join("8")).unwrap(); // no fd dir
        assert!(local_records(&proc_root).is_empty());
    }

    #[test]
    fn the_newest_rollout_of_one_process_wins() {
        let dir = temp_dir("two");
        let proc_root = dir.join("proc");
        let work = dir.join("work");
        fs::create_dir_all(&work).unwrap();
        let old_id = "11111111-2222-3333-4444-555555555555";
        let old = rollout(&dir, old_id);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let new = rollout(&dir, ID);
        process(&proc_root, 5, &[&new, &old], &work);
        let records = local_records(&proc_root);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session.id, ID);
    }

    #[test]
    fn host_line_is_pid_path_and_cwd() {
        let line = format!("21\t/h/.codex/sessions/2026/10/21/rollout-t-{ID}.jsonl\t/home/me/a");
        let record = parse_host_line(&line).unwrap();
        assert_eq!(record.pid, 21);
        assert_eq!(
            record.session,
            AgentSession::codex(ID.into(), PathBuf::from("/home/me/a"))
        );
        assert!(parse_host_line("21\t/tmp/x.jsonl\t/a").is_none());
        assert!(parse_host_line(&format!("x\t/s/sessions/rollout-t-{ID}.jsonl\t/a")).is_none());
        assert!(parse_host_line(&format!("1\t/s/sessions/rollout-t-{ID}.jsonl\trel")).is_none());
        assert!(parse_host_line("garbage").is_none());
    }

    #[test]
    fn host_snippet_lists_open_rollouts_with_their_cwd() {
        assert!(HOST_SNIPPET.contains("/proc/"));
        assert!(HOST_SNIPPET.contains("rollout-"));
        assert!(HOST_SNIPPET.contains("codex\\t"));
        assert!(HOST_SNIPPET.contains("cwd"));
    }
}
