//! The I/O side of remote groups: the local ssh check, a detached process
//! runner, and one worker thread per remote host.
//!
//! A worker owns its host's ssh master and runs that host's remote tmux calls
//! strictly one after another (so `new-session` always precedes whatever
//! follows it). Results go back to the GUI through a callback that sends a
//! `Msg`, like the linger and profile-sweep threads do. Nothing in here
//! panics; failures become `RemoteEvent`s.

use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::remote::{self, AuthMode, ControlOp, RemoteError, SshAvailability};
use crate::tmuxctl::{self, SessionInfo, TmuxCtl, TmuxVersion};

/// How long to wait for a pipe's end after the process exited. A master
/// forked by `ControlPersist` redirects its stdio to /dev/null when it
/// detaches (verified with OpenSSH 10.2), so this is only a safety net for
/// a client that keeps an inherited pipe open; the output read so far is
/// used then.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Run `ssh -V` once (stdin from /dev/null) and classify the client.
pub fn detect_ssh() -> SshAvailability {
    match Command::new("ssh").arg("-V").stdin(Stdio::null()).output() {
        // ssh prints its version on stderr.
        Ok(out) => remote::ssh_availability(Some(&String::from_utf8_lossy(&out.stderr))),
        Err(_) => remote::ssh_availability(None),
    }
}

/// A regular file with any execute bit set.
pub fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// `$XDG_RUNTIME_DIR/kabelsalat/ssh/%C` (falling back to the state
/// directory, like the local tmux socket). The directory is created 0700;
/// ssh expands `%C` to a hash of the connection, which keeps the socket path
/// under the Unix socket length limit.
pub fn control_path() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join("kabelsalat"))
        .unwrap_or_else(crate::state::state_dir);
    let dir = base.join("ssh");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    Ok(dir.join("%C"))
}

/// A finished process: exit code (`None` if killed by a signal) and output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Captured {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run `argv` with extra `env`, detached from any terminal: stdin is
/// `/dev/null` (or a pipe carrying `stdin`), and the child calls `setsid()`
/// before exec, so it has no controlling terminal and ssh can never prompt
/// on the terminal kabelsalat was started from, whatever its version.
pub fn run_detached(
    argv: &[String],
    env: &[(String, String)],
    stdin: Option<&[u8]>,
) -> io::Result<Captured> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(
            env.iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: the closure runs in the forked child before exec. It only calls
    // setsid(2), which is async-signal-safe, and builds an io::Error from the
    // raw errno, which does not allocate.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn()?;
    let stdout = Drain::start(child.stdout.take());
    let stderr = Drain::start(child.stderr.take());
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A failed write means the child is already gone; its status says why.
        let _ = pipe.write_all(data);
        // Dropping `pipe` closes it: the child sees end of input.
    }
    let status = child.wait()?;
    Ok(Captured {
        code: status.code(),
        stdout: stdout.finish(),
        stderr: stderr.finish(),
    })
}

/// Reads one pipe to its end on a helper thread, so a full stderr can never
/// block the child, and a pipe held open by a backgrounded master cannot
/// block us.
struct Drain {
    bytes: Arc<Mutex<Vec<u8>>>,
    done: mpsc::Receiver<()>,
}

impl Drain {
    fn start<R: Read + Send + 'static>(pipe: Option<R>) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (finished, done) = mpsc::channel();
        match pipe {
            Some(mut pipe) => {
                let sink = bytes.clone();
                // If no reader thread can be started, `finished` is dropped
                // with the closure: nothing is captured and nobody waits.
                let _ = std::thread::Builder::new()
                    .name("kabelsalat-pipe".into())
                    .spawn(move || {
                        let mut chunk = [0u8; 4096];
                        loop {
                            match pipe.read(&mut chunk) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    if let Ok(mut buf) = sink.lock() {
                                        buf.extend_from_slice(&chunk[..n]);
                                    }
                                }
                            }
                        }
                        let _ = finished.send(());
                    });
            }
            None => {
                let _ = finished.send(());
            }
        }
        Self { bytes, done }
    }

    fn finish(self) -> String {
        let _ = self.done.recv_timeout(DRAIN_GRACE);
        let bytes = self.bytes.lock().map(|buf| buf.clone()).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Runs one command for a worker. The real one is [`SshRunner`]; tests
/// script the replies.
pub trait Runner: Send + 'static {
    fn run(
        &mut self,
        argv: &[String],
        env: &[(String, String)],
        stdin: Option<&[u8]>,
    ) -> io::Result<Captured>;
}

/// [`run_detached`] as a [`Runner`].
pub struct SshRunner;

impl Runner for SshRunner {
    fn run(
        &mut self,
        argv: &[String],
        env: &[(String, String)],
        stdin: Option<&[u8]>,
    ) -> io::Result<Captured> {
        run_detached(argv, env, stdin)
    }
}

/// What a worker reports back to the GUI.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteEvent {
    /// Logged in, tmux checked and configured; these are the live sessions.
    Connected(Vec<SessionInfo>),
    ConnectFailed(RemoteError),
    /// A tab's ssh exited 255 and `ssh -O check` found the master gone.
    MasterDead,
    /// The listing a tab's client exit asked for (`tab` is the tab id).
    /// `Err` means liveness is unknown — never "gone".
    TabSessions {
        tab: usize,
        result: Result<Vec<SessionInfo>, String>,
    },
    /// These queued kills are done: killed, or already gone.
    Killed(Vec<String>),
}

/// One unit of work, run in arrival order.
#[derive(Debug)]
enum Job {
    Connect,
    ChildExited {
        tab: usize,
        ssh_failed: bool,
    },
    Kill(Vec<String>),
    Respawn(String),
    PanePath {
        uuid: String,
        reply: mpsc::Sender<Option<String>>,
    },
}

/// A worker's state and the jobs it runs; lives on the worker thread.
struct HostSession<R: Runner> {
    dest: String,
    control_path: PathBuf,
    auth: AuthMode,
    runner: R,
    ctl: TmuxCtl,
}

impl<R: Runner> HostSession<R> {
    fn new(dest: String, control_path: PathBuf, auth: AuthMode, runner: R) -> Self {
        let ctl = TmuxCtl::remote(&dest, &control_path);
        Self {
            dest,
            control_path,
            auth,
            runner,
            ctl,
        }
    }

    fn run_job(&mut self, job: Job) -> Option<RemoteEvent> {
        match job {
            Job::Connect => Some(self.connect()),
            Job::ChildExited { tab, ssh_failed } => Some(self.child_exited(tab, ssh_failed)),
            Job::Kill(uuids) => Some(self.kill(uuids)),
            Job::Respawn(uuid) => {
                self.respawn(&uuid);
                None
            }
            Job::PanePath { uuid, reply } => {
                // The asker may have stopped waiting; that is fine.
                let _ = reply.send(self.pane_path(&uuid));
                None
            }
        }
    }

    /// Run one command; a process that cannot even start reads as a failure
    /// with no status and the error text as stderr.
    fn run(&mut self, argv: &[String], env: &[(String, String)], stdin: Option<&[u8]>) -> Captured {
        self.runner
            .run(argv, env, stdin)
            .unwrap_or_else(|err| Captured {
                code: None,
                stdout: String::new(),
                stderr: err.to_string(),
            })
    }

    /// The host check over a fresh (or reused) master: `tmux -V`, then start
    /// and configure the server, then list its sessions.
    fn connect(&mut self) -> RemoteEvent {
        let argv = remote::master_argv(
            &self.dest,
            &self.control_path,
            &self.auth,
            &["tmux".to_string(), "-V".to_string()],
        );
        let env = remote::askpass_env(&self.auth);
        let version = self.run(&argv, &env, None);
        if version.code != Some(0) {
            let err = remote::classify(version.code, &version.stderr, &self.auth);
            if err == RemoteError::TmuxMissing {
                self.exit_master();
            }
            return RemoteEvent::ConnectFailed(err);
        }
        match TmuxVersion::parse(&version.stdout) {
            Some(found) if found.meets_minimum() => {}
            Some(found) => {
                self.exit_master();
                return RemoteEvent::ConnectFailed(RemoteError::TmuxTooOld(found.to_string()));
            }
            None => {
                self.exit_master();
                return RemoteEvent::ConnectFailed(RemoteError::TmuxMissing);
            }
        }
        if let Err(err) = self.start_server() {
            return RemoteEvent::ConnectFailed(err);
        }
        match self.list_sessions() {
            Ok(sessions) => RemoteEvent::Connected(sessions),
            Err(detail) => RemoteEvent::ConnectFailed(RemoteError::Other(detail)),
        }
    }

    /// `start-server ; source-file -` with the remote config on stdin. If
    /// tmux itself refuses (every tmux since 3.1 reads `-` as stdin, but a
    /// patched build might not), upload the file and source it from disk.
    fn start_server(&mut self) -> Result<(), RemoteError> {
        let conf = tmuxctl::remote_conf();
        let argv = remote::mux_argv(
            &self.dest,
            &self.control_path,
            false,
            &remote::start_server_argv(),
        );
        let direct = self.run(&argv, &[], Some(conf.as_bytes()));
        if direct.code == Some(0) {
            return Ok(());
        }
        if direct.code == Some(remote::SSH_FAILED) || direct.code.is_none() {
            return Err(remote::classify(direct.code, &direct.stderr, &self.auth));
        }
        let argv = remote::mux_argv(
            &self.dest,
            &self.control_path,
            false,
            &remote::upload_conf_argv(),
        );
        let upload = self.run(&argv, &[], Some(conf.as_bytes()));
        if upload.code == Some(0) {
            Ok(())
        } else {
            Err(RemoteError::Other(format!(
                "could not configure tmux: {}",
                upload.stderr.trim()
            )))
        }
    }

    fn list_sessions(&mut self) -> Result<Vec<SessionInfo>, String> {
        let argv = self.ctl.command_argv(&TmuxCtl::list_sessions_args());
        let out = self.run(&argv, &[], None);
        self.ctl
            .list_sessions_from_output(out.code, &out.stdout, &out.stderr)
            .map_err(|err| err.to_string())
    }

    /// A tab's client exited. 255 is ssh failing: if the master is gone too,
    /// the whole host is down. Otherwise the listing decides (close or
    /// reattach) — asynchronously, never on the GUI thread.
    fn child_exited(&mut self, tab: usize, ssh_failed: bool) -> RemoteEvent {
        if ssh_failed {
            let argv = remote::control_argv(&self.dest, &self.control_path, ControlOp::Check);
            if self.run(&argv, &[], None).code != Some(0) {
                return RemoteEvent::MasterDead;
            }
        }
        RemoteEvent::TabSessions {
            tab,
            result: self.list_sessions(),
        }
    }

    /// Kill each session. Done means tmux answered: 0 (killed) or any other
    /// tmux status (no such session, no server — already gone). ssh's 255 or
    /// no status leaves the uuid queued for the next connect.
    fn kill(&mut self, uuids: Vec<String>) -> RemoteEvent {
        let mut done = Vec::new();
        for uuid in uuids {
            let argv = self.ctl.command_argv(&TmuxCtl::kill_session_args(&uuid));
            let out = self.run(&argv, &[], None);
            if out.code.is_some() && out.code != Some(remote::SSH_FAILED) {
                done.push(uuid);
            }
        }
        RemoteEvent::Killed(done)
    }

    fn respawn(&mut self, uuid: &str) {
        let argv = self.ctl.command_argv(&TmuxCtl::respawn_pane_args(uuid));
        let out = self.run(&argv, &[], None);
        if out.code != Some(0) {
            eprintln!(
                "kabelsalat: respawn-pane for {uuid} on {}: {}",
                self.dest,
                out.stderr.trim()
            );
        }
    }

    /// The empty check is load-bearing: on tmux 3.7c `display-message -p -t
    /// <missing session>` exits 0 with empty output rather than failing.
    fn pane_path(&mut self, uuid: &str) -> Option<String> {
        let argv = self
            .ctl
            .command_argv(&TmuxCtl::pane_current_path_args(uuid));
        let out = self.run(&argv, &[], None);
        let path = out.stdout.trim();
        (out.code == Some(0) && !path.is_empty()).then(|| path.to_string())
    }

    fn exit_master(&mut self) {
        let argv = remote::control_argv(&self.dest, &self.control_path, ControlOp::Exit);
        let _ = self.run(&argv, &[], None);
    }
}

/// Handle to one host's worker thread. Dropping it ends the thread once its
/// queue is empty.
pub struct RemoteWorker {
    jobs: mpsc::Sender<Job>,
    dest: String,
    control_path: PathBuf,
}

impl RemoteWorker {
    /// Start the worker for `dest`. `notify` runs on the worker thread for
    /// every event; it should only forward (e.g. send a `Msg`).
    pub fn spawn<R: Runner>(
        dest: String,
        control_path: PathBuf,
        auth: AuthMode,
        runner: R,
        notify: impl Fn(RemoteEvent) + Send + 'static,
    ) -> io::Result<Self> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let mut session = HostSession::new(dest.clone(), control_path.clone(), auth, runner);
        std::thread::Builder::new()
            // Not named after `dest`: a thread name must not contain NUL,
            // and a hand-edited state file is not validated.
            .name("kabelsalat-ssh".into())
            .spawn(move || {
                while let Ok(job) = queue.recv() {
                    if let Some(event) = session.run_job(job) {
                        notify(event);
                    }
                }
            })?;
        Ok(Self {
            jobs,
            dest,
            control_path,
        })
    }

    /// Log in (once per host: this is the only authenticating command),
    /// check tmux, configure the server and list its sessions.
    pub fn connect(&self) {
        let _ = self.jobs.send(Job::Connect);
    }

    /// A tab's client exited; `ssh_failed` when its status was ssh's 255.
    pub fn child_exited(&self, tab: usize, ssh_failed: bool) {
        let _ = self.jobs.send(Job::ChildExited { tab, ssh_failed });
    }

    pub fn kill(&self, uuids: Vec<String>) {
        let _ = self.jobs.send(Job::Kill(uuids));
    }

    pub fn respawn(&self, uuid: String) {
        let _ = self.jobs.send(Job::Respawn(uuid));
    }

    /// The pane's directory on the host, waiting at most `wait` (the queue
    /// is serial, so a busy worker simply times out). Blocks the caller.
    pub fn pane_current_path(&self, uuid: &str, wait: Duration) -> Option<String> {
        let (reply, answer) = mpsc::channel();
        self.jobs
            .send(Job::PanePath {
                uuid: uuid.to_string(),
                reply,
            })
            .ok()?;
        answer.recv_timeout(wait).ok().flatten()
    }

    /// `ssh -O exit` for this host's master, synchronously on the caller's
    /// thread (quit must not wait behind a queued login). Remote sessions
    /// survive; the next start logs in again.
    pub fn exit_master(&self) {
        let argv = remote::control_argv(&self.dest, &self.control_path, ControlOp::Exit);
        if let Err(err) = run_detached(&argv, &[], None) {
            eprintln!("kabelsalat: ssh -O exit for {}: {err}", self.dest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    /// One recorded command.
    #[derive(Debug, Clone)]
    struct Call {
        argv: Vec<String>,
        env: Vec<(String, String)>,
        stdin: Option<Vec<u8>>,
    }

    /// Scripted runner: replies in order (then success with no output) and
    /// records every call.
    struct Fake {
        replies: VecDeque<Captured>,
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl Runner for Fake {
        fn run(
            &mut self,
            argv: &[String],
            env: &[(String, String)],
            stdin: Option<&[u8]>,
        ) -> std::io::Result<Captured> {
            self.calls.lock().unwrap().push(Call {
                argv: argv.to_vec(),
                env: env.to_vec(),
                stdin: stdin.map(<[u8]>::to_vec),
            });
            Ok(self.replies.pop_front().unwrap_or(Captured {
                code: Some(0),
                ..Captured::default()
            }))
        }
    }

    fn reply(code: i32, stdout: &str, stderr: &str) -> Captured {
        Captured {
            code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    fn host(auth: AuthMode, replies: Vec<Captured>) -> (HostSession<Fake>, Arc<Mutex<Vec<Call>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = Fake {
            replies: replies.into(),
            calls: calls.clone(),
        };
        (
            HostSession::new(
                "me@box".into(),
                Path::new("/run/ks/ssh/%C").to_path_buf(),
                auth,
                runner,
            ),
            calls,
        )
    }

    fn last_word(call: &Call) -> &str {
        call.argv.last().map(String::as_str).unwrap_or_default()
    }

    fn sessions() -> Vec<SessionInfo> {
        vec![
            SessionInfo {
                uuid: "a".into(),
                pane_dead: false,
                dead_status: None,
            },
            SessionInfo {
                uuid: "b".into(),
                pane_dead: true,
                dead_status: Some(2),
            },
        ]
    }

    #[test]
    fn connect_logs_in_checks_tmux_configures_and_lists() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![
                reply(0, "tmux 3.4\n", ""),
                reply(0, "", ""),
                reply(0, "ks-a\t0\t\nks-b\t1\t2\n", ""),
            ],
        );
        assert_eq!(
            host.run_job(Job::Connect),
            Some(RemoteEvent::Connected(sessions()))
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        // 1: the only authenticating command, the master, running tmux -V.
        assert!(calls[0].argv.contains(&"ControlMaster=auto".to_string()));
        assert!(calls[0].argv.contains(&"BatchMode=yes".to_string()));
        assert_eq!(last_word(&calls[0]), "tmux -V");
        assert!(calls[0].stdin.is_none());
        // 2: start + configure over the master, config on stdin.
        assert!(calls[1].argv.contains(&"ControlMaster=no".to_string()));
        assert!(last_word(&calls[1]).contains("source-file -"));
        assert_eq!(
            calls[1].stdin.as_deref(),
            Some(tmuxctl::remote_conf().as_bytes())
        );
        // 3: list-sessions over the master.
        assert!(last_word(&calls[2]).contains("list-sessions"));
    }

    #[test]
    fn askpass_env_goes_to_the_master_only() {
        let askpass = AuthMode::Askpass("/usr/bin/ksshaskpass".into());
        let (mut host, calls) = host(
            askpass,
            vec![
                reply(0, "tmux 3.2\n", ""),
                reply(0, "", ""),
                reply(0, "", ""),
            ],
        );
        assert_eq!(
            host.run_job(Job::Connect),
            Some(RemoteEvent::Connected(Vec::new()))
        );
        let calls = calls.lock().unwrap();
        assert!(
            calls[0]
                .env
                .contains(&("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()))
        );
        assert!(calls[1].env.is_empty());
        assert!(calls[2].env.is_empty());
    }

    #[test]
    fn connect_refuses_tmux_older_than_3_2_and_drops_the_master() {
        let (mut host, calls) = host(AuthMode::BatchOnly, vec![reply(0, "tmux 3.1c\n", "")]);
        assert_eq!(
            host.run_job(Job::Connect),
            Some(RemoteEvent::ConnectFailed(RemoteError::TmuxTooOld(
                "3.1c".into()
            )))
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(&calls[1].argv[3..5], ["-O", "exit"]);
    }

    #[test]
    fn connect_refuses_a_host_without_tmux() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![reply(127, "", "bash: line 1: tmux: command not found\n")],
        );
        assert_eq!(
            host.run_job(Job::Connect),
            Some(RemoteEvent::ConnectFailed(RemoteError::TmuxMissing))
        );
        assert_eq!(&calls.lock().unwrap()[1].argv[3..5], ["-O", "exit"]);
    }

    #[test]
    fn a_failed_login_stops_before_any_tmux_call() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![reply(
                255,
                "",
                "me@box: Permission denied (publickey,password).\r\n",
            )],
        );
        assert_eq!(
            host.run_job(Job::Connect),
            Some(RemoteEvent::ConnectFailed(RemoteError::AuthNeedsAskpass))
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn connect_uploads_the_config_when_stdin_sourcing_fails() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![
                reply(0, "tmux 3.2a\n", ""),
                reply(
                    1,
                    "",
                    "usage: source-file [-Fnqv] [-t target-pane] path ...\n",
                ),
                reply(0, "", ""),
                reply(0, "", ""),
            ],
        );
        assert_eq!(
            host.run_job(Job::Connect),
            Some(RemoteEvent::Connected(Vec::new()))
        );
        let calls = calls.lock().unwrap();
        assert!(last_word(&calls[2]).contains("cat >"));
        assert_eq!(
            calls[2].stdin.as_deref(),
            Some(tmuxctl::remote_conf().as_bytes())
        );
        assert!(last_word(&calls[3]).contains("list-sessions"));
    }

    #[test]
    fn an_ssh_failure_while_configuring_is_not_retried_as_an_upload() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![
                reply(0, "tmux 3.3a\n", ""),
                reply(
                    255,
                    "",
                    "ssh: connect to host box port 22: Connection refused\r\n",
                ),
            ],
        );
        assert_eq!(
            host.run_job(Job::Connect),
            Some(RemoteEvent::ConnectFailed(RemoteError::Unreachable))
        );
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_255_exit_with_a_dead_master_reports_master_dead() {
        let (mut host, calls) = host(AuthMode::BatchOnly, vec![reply(255, "", "")]);
        assert_eq!(
            host.run_job(Job::ChildExited {
                tab: 7,
                ssh_failed: true
            }),
            Some(RemoteEvent::MasterDead)
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(&calls[0].argv[3..5], ["-O", "check"]);
    }

    #[test]
    fn a_255_exit_with_a_live_master_lists_sessions() {
        let (mut host, _) = host(
            AuthMode::BatchOnly,
            vec![reply(0, "", ""), reply(0, "ks-a\t0\t\n", "")],
        );
        assert_eq!(
            host.run_job(Job::ChildExited {
                tab: 7,
                ssh_failed: true
            }),
            Some(RemoteEvent::TabSessions {
                tab: 7,
                result: Ok(vec![sessions()[0].clone()])
            })
        );
    }

    #[test]
    fn any_other_exit_lists_sessions_without_a_check() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![reply(
                1,
                "",
                "no server running on /tmp/tmux-1000/kabelsalat\n",
            )],
        );
        assert_eq!(
            host.run_job(Job::ChildExited {
                tab: 3,
                ssh_failed: false
            }),
            Some(RemoteEvent::TabSessions {
                tab: 3,
                result: Ok(Vec::new())
            })
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_ssh_failure_during_a_listing_is_never_an_empty_list() {
        let (mut host, _) = host(
            AuthMode::BatchOnly,
            vec![reply(
                255,
                "",
                "Control socket connect(/x): No such file or directory\r\n",
            )],
        );
        let Some(RemoteEvent::TabSessions { result, .. }) = host.run_job(Job::ChildExited {
            tab: 1,
            ssh_failed: false,
        }) else {
            panic!("expected TabSessions");
        };
        assert!(result.is_err());
    }

    #[test]
    fn kill_reports_killed_and_already_gone_but_not_ssh_failures() {
        let (mut host, calls) = host(
            AuthMode::BatchOnly,
            vec![
                reply(0, "", ""),
                reply(1, "", "can't find session: ks-gone\n"),
                reply(
                    255,
                    "",
                    "mux_client_request_session: read from master failed\r\n",
                ),
            ],
        );
        assert_eq!(
            host.run_job(Job::Kill(vec!["live".into(), "gone".into(), "lost".into()])),
            Some(RemoteEvent::Killed(vec!["live".into(), "gone".into()]))
        );
        assert!(last_word(&calls.lock().unwrap()[0]).contains("kill-session -t ks-live"));
    }

    #[test]
    fn the_pane_path_is_answered_on_the_reply_channel() {
        let (mut host, _) = host(
            AuthMode::BatchOnly,
            vec![
                reply(0, "/home/me/proj\n", ""),
                reply(1, "", "can't find pane\n"),
            ],
        );
        let (reply_to, answer) = std::sync::mpsc::channel();
        assert_eq!(
            host.run_job(Job::PanePath {
                uuid: "a".into(),
                reply: reply_to.clone()
            }),
            None
        );
        assert_eq!(answer.recv().unwrap(), Some("/home/me/proj".to_string()));
        host.run_job(Job::PanePath {
            uuid: "a".into(),
            reply: reply_to,
        });
        assert_eq!(answer.recv().unwrap(), None);
    }

    #[test]
    fn run_detached_captures_output_status_and_stdin() {
        let out = run_detached(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "cat; echo err >&2; exit 3".to_string(),
            ],
            &[("KS_TEST".to_string(), "1".to_string())],
            Some(b"hello"),
        )
        .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.stderr, "err\n");
    }

    #[test]
    fn run_detached_has_no_controlling_terminal() {
        // setsid(): the child leads its own session, so /dev/tty cannot open.
        let out = run_detached(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "if sh -c ': </dev/tty' 2>/dev/null; then echo tty; else echo none; fi".to_string(),
            ],
            &[],
            None,
        )
        .unwrap();
        assert_eq!(out.stdout.trim(), "none");
    }

    #[test]
    fn run_detached_rejects_an_empty_argv() {
        assert!(run_detached(&[], &[], None).is_err());
    }
}
