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

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use relm4::adw;
use relm4::gtk::gio;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::app::Msg;
use crate::cli::{self, Action, Cli, GroupInfo, GroupTarget};

/// How long one control-socket round trip may stall the GUI's main thread:
/// a `screenshot` renders a frame and writes a PNG.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Ask the component to create the tab, in an existing group or in one it
/// creates for us. Returns false when there is no component to ask, or when
/// it has already shut down.
pub fn request_spawn(
    group: GroupTarget,
    cwd: Option<std::path::PathBuf>,
    argv: Vec<String>,
    tab_uuid: String,
) -> bool {
    let Some(control) = CONTROL.get() else {
        return false;
    };
    control
        .sender
        .send(Msg::SpawnCommand {
            group,
            tab_uuid,
            cwd,
            argv,
        })
        .is_ok()
}

/// Ask the component to rename a group. Returns false when there is no
/// component to ask, or when it has already shut down.
pub fn request_rename(group_uuid: String, name: String) -> bool {
    let Some(control) = CONTROL.get() else {
        return false;
    };
    control
        .sender
        .send(Msg::RenameGroup { group_uuid, name })
        .is_ok()
}

/// Ask the component to bring up a group's browser. Returns false when there
/// is no component to ask, or when it has already shut down.
pub fn request_open_browser(group_uuid: String) -> bool {
    let Some(control) = CONTROL.get() else {
        return false;
    };
    control.sender.send(Msg::OpenBrowser { group_uuid }).is_ok()
}

/// Ask the component to bring up Android in a group. Returns false when
/// there is no component to ask, or when it has already shut down.
pub fn request_open_android(group_uuid: String) -> bool {
    let Some(control) = CONTROL.get() else {
        return false;
    };
    control.sender.send(Msg::OpenAndroid { group_uuid }).is_ok()
}

/// Ask the component to set (or, with `None`, clear) a group's overview
/// root. Returns false when there is no component to ask, or when it has
/// already shut down.
pub fn request_set_overview_root(group_uuid: String, root: Option<std::path::PathBuf>) -> bool {
    let Some(control) = CONTROL.get() else {
        return false;
    };
    control
        .sender
        .send(Msg::SetOverviewRoot { group_uuid, root })
        .is_ok()
}

/// One request to a pane's control socket: write `line` and a newline, shut
/// the write half (the server serves a connection until EOF), read one reply
/// line. The write and the read share one deadline, `timeout` from the
/// connect, so the whole exchange is bounded by `timeout` (plus the connect
/// itself, immediate on a local socket). A pane that closes without a reply
/// is an error; running out of time is `TimedOut` or `WouldBlock`.
pub fn send_control(socket: &Path, line: &str, timeout: Duration) -> std::io::Result<String> {
    let deadline = Instant::now() + timeout;
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    stream.shutdown(std::net::Shutdown::Write)?;
    // A zero timeout is rejected by the socket API; a spent deadline is a
    // timeout already.
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(std::io::ErrorKind::TimedOut.into());
    }
    stream.set_read_timeout(Some(left))?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    if reply.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the pane closed the connection without a reply",
        ));
    }
    Ok(reply)
}

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

    match outcome.action {
        Some(Action::Spawn { group, cwd, argv }) => {
            // The tab's uuid is minted here, where glib is available, and
            // echoed so the caller has the identity of the requested tab to
            // correlate with — not a guarantee it was created; request_spawn
            // only queues the message on the relm4 channel.
            let tab_uuid = glib::uuid_string_random().to_string();
            if !request_spawn(group, cwd, argv, tab_uuid.clone()) {
                command_line.printerr_literal("kabelsalat: no window to spawn into\n");
                return glib::ExitCode::new(cli::EXIT_NOT_RUNNING);
            }
            command_line.print_literal(&format!("{tab_uuid}\n"));
        }
        // Renaming prints nothing on success.
        Some(Action::Rename { group_uuid, name }) => {
            if !request_rename(group_uuid, name) {
                command_line.printerr_literal("kabelsalat: no window to spawn into\n");
                return glib::ExitCode::new(cli::EXIT_NOT_RUNNING);
            }
        }
        // The endpoint arrives in the group's tmux sessions once the browser
        // is up; the caller reads it from there, so nothing is printed.
        Some(Action::OpenBrowser { group_uuid }) => {
            if !request_open_browser(group_uuid) {
                command_line.printerr_literal("kabelsalat: no window to open a browser in\n");
                return glib::ExitCode::new(cli::EXIT_NOT_RUNNING);
            }
        }
        // As for the browser: the endpoints arrive in the group's tmux
        // sessions once Android has booted, so nothing is printed.
        Some(Action::OpenAndroid { group_uuid }) => {
            if !request_open_android(group_uuid) {
                command_line.printerr_literal("kabelsalat: no window to open Android in\n");
                return glib::ExitCode::new(cli::EXIT_NOT_RUNNING);
            }
        }
        // One blocking round trip on this (the GTK) thread. Safe: the socket
        // is served by the compositor's own threads, never by this one, and
        // CONTROL_TIMEOUT bounds the stall.
        Some(Action::Control { socket, line }) => {
            let reply = match send_control(&socket, &line, CONTROL_TIMEOUT) {
                Ok(reply) => reply,
                Err(err) => {
                    command_line.printerr_literal(&cli::control_error(
                        &socket.display().to_string(),
                        err.kind(),
                        &err.to_string(),
                        CONTROL_TIMEOUT.as_secs(),
                    ));
                    return glib::ExitCode::new(cli::EXIT_GROUP);
                }
            };
            let answer = cli::control_reply(&reply);
            if !answer.stdout.is_empty() {
                command_line.print_literal(&answer.stdout);
            }
            if !answer.stderr.is_empty() {
                command_line.printerr_literal(&answer.stderr);
            }
            return glib::ExitCode::new(answer.code);
        }
        // The directory must exist, and `dispatch` has no filesystem: the
        // check happens here, with the real one. Setting prints nothing on
        // success, like Rename.
        Some(action @ Action::SetOverviewRoot { .. }) => {
            if let Some(refused) = cli::check_overview_root(&action, Path::is_dir) {
                command_line.printerr_literal(&refused.stderr);
                return glib::ExitCode::new(refused.code);
            }
            if let Action::SetOverviewRoot { group_uuid, root } = action
                && !request_set_overview_root(group_uuid, root)
            {
                command_line.printerr_literal("kabelsalat: no window to set the overview in\n");
                return glib::ExitCode::new(cli::EXIT_NOT_RUNNING);
            }
        }
        None => {}
    }

    glib::ExitCode::new(outcome.code)
}

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
            GroupTarget::Existing("aaa-111".into()),
            Some(std::path::PathBuf::from("/tmp")),
            vec!["ls".into()],
            "tab-uuid".into(),
        );
        assert!(!refused);
    }

    #[test]
    fn a_rename_request_without_a_gui_is_refused() {
        assert!(!request_rename("aaa-111".into(), "frontend".into()));
    }

    #[test]
    fn an_open_browser_request_without_a_gui_is_refused() {
        assert!(!request_open_browser("aaa-111".into()));
    }

    fn socket_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kabelsalat-control-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_control_line_gets_its_reply() {
        use std::io::{BufRead as _, Write as _};
        let dir = socket_dir("reply");
        let socket = dir.join("ctl.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut writer = stream;
            writer.write_all(b"ok\n").unwrap();
            line
        });
        let reply = send_control(&socket, "click 10 20", Duration::from_secs(5)).unwrap();
        assert_eq!(reply, "ok\n");
        assert_eq!(server.join().unwrap(), "click 10 20\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pane_that_hangs_up_without_a_reply_is_an_error() {
        let dir = socket_dir("silent");
        let socket = dir.join("ctl.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(stream);
        });
        assert!(send_control(&socket, "ping", Duration::from_secs(5)).is_err());
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pane_that_never_answers_times_out() {
        let dir = socket_dir("hang");
        let socket = dir.join("ctl.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        // Accept, then hold the connection open without ever replying.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = held.recv();
            drop(stream);
        });
        let (done, result) = std::sync::mpsc::channel();
        let client_socket = socket.clone();
        std::thread::spawn(move || {
            let _ = done.send(send_control(
                &client_socket,
                "ping",
                Duration::from_millis(200),
            ));
        });
        let reply = result.recv_timeout(Duration::from_secs(5));
        let _ = release.send(());
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            matches!(reply, Ok(Err(_))),
            "a silent pane must fail within the timeout: {reply:?}"
        );
    }

    #[test]
    fn a_missing_control_socket_is_an_error() {
        let missing = Path::new("/nonexistent/kabelsalat-test/ctl.sock");
        assert!(send_control(missing, "ping", Duration::from_secs(1)).is_err());
    }

    #[test]
    fn an_open_android_request_without_a_gui_is_refused() {
        assert!(!request_open_android("aaa-111".into()));
    }

    #[test]
    fn a_set_overview_root_request_without_a_gui_is_refused() {
        assert!(!request_set_overview_root(
            "aaa-111".into(),
            Some(std::path::PathBuf::from("/tmp/docs"))
        ));
        assert!(!request_set_overview_root("aaa-111".into(), None));
    }
}
