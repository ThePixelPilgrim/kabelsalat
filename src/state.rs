//! Layout persistence and startup reconciliation (pure logic only).
//!
//! Serializes the presentation model (groups, tabs, active tab, sidebar) to
//! `<state_dir>/state.json` atomically, and computes a reconciliation plan
//! between saved state and live tmux sessions as plain data — no GUI, no tmux
//! calls here.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A tab group as persisted: stable id, display name, palette color index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedGroup {
    pub id: usize,
    pub name: String, // empty = unnamed, no header shown
    pub palette: usize,
}

/// A tab as persisted. Order within the containing vec is the display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTab {
    pub uuid: String, // stable UUID assigned at tab creation, names the tmux session
    pub group: usize,
    pub title: String,
}

/// The whole persisted presentation model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedState {
    pub groups: Vec<SavedGroup>,
    pub tabs: Vec<SavedTab>,
    pub active: Option<String>, // uuid of the active tab
    pub sidebar_visible: bool,
}

impl Default for SavedState {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            tabs: Vec::new(),
            active: None,
            sidebar_visible: true,
        }
    }
}

/// `$XDG_STATE_HOME/kabelsalat`, falling back to `~/.local/state/kabelsalat`.
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".local/state")
        });
    base.join("kabelsalat")
}

/// Default location of the state file.
pub fn state_file() -> PathBuf {
    state_dir().join("state.json")
}

/// Atomically write `state` to `path`: temp file in the same directory, then
/// rename over the target. Creates parent directories as needed.
pub fn save(state: &SavedState, path: &Path) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = dir.join(".state.json.tmp");
    let json = serde_json::to_vec_pretty(state).expect("state is always serializable");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&json)?;
    file.sync_all()?;
    fs::rename(&tmp, path)
}

/// Load state from `path`. A missing file yields the empty default. A corrupt
/// file is renamed aside (to `<path>.corrupt`) and also yields the default —
/// running shells must never be lost over a bad JSON file.
pub fn load(path: &Path) -> SavedState {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(_) => return SavedState::default(),
    };
    match serde_json::from_slice(&data) {
        Ok(state) => state,
        Err(_) => {
            let aside = path.with_extension("json.corrupt");
            let _ = fs::rename(path, &aside);
            SavedState::default()
        }
    }
}

/// A live session whose pane has died (shell exited non-zero while detached).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadPane {
    pub uuid: String,
    pub exit_code: i32,
}

/// A saved tab with a live backing session: recreate the tab and attach.
/// `dead_exit` is set when the pane is dead — restore in crashed state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachTab {
    pub tab: SavedTab,
    pub dead_exit: Option<i32>,
}

/// A live session with no saved tab: adopt into the "Recovered" group so no
/// live shell is ever invisible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanTab {
    pub uuid: String,
    pub dead_exit: Option<i32>,
}

/// The startup reconciliation plan, as plain data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcilePlan {
    /// Saved tabs with a live session, in saved order.
    pub attach: Vec<AttachTab>,
    /// Saved tabs without a live session: recreate and spawn a fresh shell.
    pub respawn: Vec<SavedTab>,
    /// Live sessions absent from state, in the order given by `live`.
    pub adopt: Vec<OrphanTab>,
}

/// Pure reconciliation of saved state against the live session list.
/// `live` holds the tab UUIDs of running `ks-<uuid>` sessions; `dead` the
/// subset whose pane has died, with the shell's real exit code.
pub fn reconcile(saved: &SavedState, live: &[String], dead: &[DeadPane]) -> ReconcilePlan {
    let dead_exit = |uuid: &str| dead.iter().find(|d| d.uuid == uuid).map(|d| d.exit_code);
    let mut plan = ReconcilePlan::default();
    for tab in &saved.tabs {
        if live.contains(&tab.uuid) {
            plan.attach.push(AttachTab {
                tab: tab.clone(),
                dead_exit: dead_exit(&tab.uuid),
            });
        } else {
            plan.respawn.push(tab.clone());
        }
    }
    for uuid in live {
        if !saved.tabs.iter().any(|t| t.uuid == *uuid) {
            plan.adopt.push(OrphanTab {
                uuid: uuid.clone(),
                dead_exit: dead_exit(uuid),
            });
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> SavedState {
        SavedState {
            groups: vec![
                SavedGroup {
                    id: 0,
                    name: String::new(),
                    palette: 0,
                },
                SavedGroup {
                    id: 1,
                    name: "work".into(),
                    palette: 2,
                },
            ],
            tabs: vec![
                SavedTab {
                    uuid: "aaa".into(),
                    group: 0,
                    title: "bash".into(),
                },
                SavedTab {
                    uuid: "bbb".into(),
                    group: 1,
                    title: "vim".into(),
                },
                SavedTab {
                    uuid: "ccc".into(),
                    group: 1,
                    title: "logs".into(),
                },
            ],
            active: Some("bbb".into()),
            sidebar_visible: false,
        }
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kabelsalat-state-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip_via_json() {
        let state = sample_state();
        let json = serde_json::to_string(&state).unwrap();
        let back: SavedState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, state);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tmp_dir("roundtrip");
        let path = dir.join("state.json");
        let state = sample_state();
        save(&state, &path).unwrap();
        assert_eq!(load(&path), state);
        // No temp file left behind.
        assert!(!dir.join(".state.json.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_missing_directories() {
        let dir = tmp_dir("mkdirs");
        let path = dir.join("nested/deeper/state.json");
        save(&sample_state(), &path).unwrap();
        assert_eq!(load(&path), sample_state());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_yields_default() {
        let dir = tmp_dir("missing");
        let state = load(&dir.join("state.json"));
        assert_eq!(state, SavedState::default());
        assert!(state.sidebar_visible);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_corrupt_file_renames_aside_and_yields_default() {
        let dir = tmp_dir("corrupt");
        let path = dir.join("state.json");
        fs::write(&path, b"{ this is not json").unwrap();
        assert_eq!(load(&path), SavedState::default());
        // The corrupt file was moved aside, not deleted.
        assert!(!path.exists());
        let aside = dir.join("state.json.corrupt");
        assert_eq!(fs::read(&aside).unwrap(), b"{ this is not json");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_empty_everything() {
        let plan = reconcile(&SavedState::default(), &[], &[]);
        assert_eq!(plan, ReconcilePlan::default());
    }

    #[test]
    fn reconcile_all_live_attaches_in_saved_order() {
        let state = sample_state();
        let live = vec!["ccc".to_string(), "aaa".to_string(), "bbb".to_string()];
        let plan = reconcile(&state, &live, &[]);
        let uuids: Vec<_> = plan.attach.iter().map(|a| a.tab.uuid.as_str()).collect();
        assert_eq!(uuids, ["aaa", "bbb", "ccc"]);
        assert!(plan.attach.iter().all(|a| a.dead_exit.is_none()));
        assert!(plan.respawn.is_empty());
        assert!(plan.adopt.is_empty());
    }

    #[test]
    fn reconcile_no_live_sessions_respawns_all() {
        let state = sample_state();
        let plan = reconcile(&state, &[], &[]);
        assert!(plan.attach.is_empty());
        assert_eq!(plan.respawn, state.tabs);
        assert!(plan.adopt.is_empty());
    }

    #[test]
    fn reconcile_mixed_live_and_gone() {
        let state = sample_state();
        let live = vec!["bbb".to_string()];
        let plan = reconcile(&state, &live, &[]);
        assert_eq!(plan.attach.len(), 1);
        assert_eq!(plan.attach[0].tab.uuid, "bbb");
        let respawned: Vec<_> = plan.respawn.iter().map(|t| t.uuid.as_str()).collect();
        assert_eq!(respawned, ["aaa", "ccc"]);
        assert!(plan.adopt.is_empty());
    }

    #[test]
    fn reconcile_adopts_orphans_in_live_order() {
        let state = sample_state();
        let live = vec!["zzz".to_string(), "aaa".to_string(), "yyy".to_string()];
        let plan = reconcile(&state, &live, &[]);
        let adopted: Vec<_> = plan.adopt.iter().map(|o| o.uuid.as_str()).collect();
        assert_eq!(adopted, ["zzz", "yyy"]);
        assert_eq!(plan.attach.len(), 1);
        assert_eq!(plan.respawn.len(), 2);
    }

    #[test]
    fn reconcile_marks_dead_panes_on_attach_and_orphan() {
        let state = sample_state();
        let live = vec!["aaa".to_string(), "bbb".to_string(), "zzz".to_string()];
        let dead = vec![
            DeadPane {
                uuid: "bbb".into(),
                exit_code: 3,
            },
            DeadPane {
                uuid: "zzz".into(),
                exit_code: 127,
            },
        ];
        let plan = reconcile(&state, &live, &dead);
        assert_eq!(plan.attach[0].dead_exit, None); // aaa alive
        assert_eq!(plan.attach[1].dead_exit, Some(3)); // bbb crashed
        assert_eq!(plan.adopt[0].dead_exit, Some(127)); // orphan crashed
    }
}
