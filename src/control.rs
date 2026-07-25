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
