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

use relm4::adw;
use relm4::gtk::gio;
use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use crate::app::Msg;
use crate::cli::{self, Action, Cli, GroupInfo, GroupTarget};

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
}
