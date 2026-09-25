//! Layout persistence and startup reconciliation (pure logic only).
//!
//! Serializes the presentation model (groups, tabs, active tab, sidebar) to
//! `<state_dir>/state.json` atomically, and computes a reconciliation plan
//! between saved state and live tmux sessions as plain data — no GUI, no tmux
//! calls here.

use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default divider position (px from the left) of the terminal/browser split.
pub const DEFAULT_BROWSER_SPLIT: f64 = 800.0;

/// A tab group as persisted: stable id, display name, palette color index.
///
/// `PartialEq` only, not `Eq`: `browser_split` is a float.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedGroup {
    /// Stable UUID assigned at group creation, in the same hyphenated v4 form
    /// as `SavedTab::uuid`. Group *ids* are reused (the next id is
    /// `max(id) + 1`, so deleting the highest group hands its id to the next
    /// one), therefore anything that owns per-group data on disk — the browser
    /// profile directory — must key off this uuid, never off `id`.
    /// `serde(default)` so state files predating this field load with an empty
    /// string; `load()` backfills a fresh uuid for every such group.
    #[serde(default)]
    pub uuid: String,
    pub id: usize,
    pub name: String, // empty = unnamed, no header shown
    pub palette: usize,
    /// Whether this group had a browser pane when the app last exited. Restored
    /// browsers are spawned fresh; Chromium restores its own session from the
    /// profile directory. `serde(default)` so older state files load with
    /// `false` (no browser).
    #[serde(default)]
    pub browser_open: bool,
    /// Divider position of the terminal/browser split, in pixels.
    /// `serde(default)` so older state files load with the default split.
    #[serde(default = "default_browser_split")]
    pub browser_split: f64,
    /// Where a freshly launched browser for this group starts. `None` = whatever
    /// Chromium opens on its own. Only consulted for a launch into a *fresh*
    /// profile; a launch that resumes an existing profile restores that session
    /// instead, so a crash or a restart never stacks another copy of this tab.
    #[serde(default)]
    pub default_url: Option<String>,
    /// The ssh destination this group's tabs run on, handed to ssh verbatim
    /// (`user@host`, a `~/.ssh/config` alias, `ssh://user@host:port`). `None`
    /// is this computer. `serde(default)` so older state files load every
    /// group as local. Every tab of the group runs on this host; a tab's own
    /// host is always its group's.
    #[serde(default)]
    pub host: Option<String>,
}

fn default_browser_split() -> f64 {
    DEFAULT_BROWSER_SPLIT
}

impl SavedGroup {
    /// A group with a freshly generated uuid and the default split.
    pub fn new(id: usize, name: String, palette: usize) -> Self {
        Self {
            uuid: new_uuid(),
            id,
            name,
            palette,
            browser_open: false,
            browser_split: DEFAULT_BROWSER_SPLIT,
            default_url: None,
            host: None,
        }
    }
}

/// Generate a random UUID string in the same shape as the tab uuids
/// (`xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx`, lowercase hex, RFC 4122 v4).
///
/// Kept pure: no GTK, no processes. Entropy comes from `RandomState`, which the
/// std library seeds per process from the OS, mixed with a monotonically
/// increasing counter and the wall clock so that repeated calls within one
/// process never collide.
pub fn new_uuid() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hash as _, Hasher as _};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let word = || {
        let mut hasher = RandomState::new().build_hasher();
        COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .hash(&mut hasher);
        hasher.finish()
    };
    let (hi, lo) = (word(), word());
    // Set the version (4) and variant (RFC 4122) bits.
    let hi = (hi & 0xffff_ffff_ffff_0fff) | 0x0000_0000_0000_4000;
    let lo = (lo & 0x3fff_ffff_ffff_ffff) | 0x8000_0000_0000_0000;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (hi >> 32) as u32,
        (hi >> 16) as u16,
        hi as u16,
        (lo >> 48) as u16,
        lo & 0xffff_ffff_ffff,
    )
}

/// A tab as persisted. Order within the containing vec is the display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTab {
    pub uuid: String, // stable UUID assigned at tab creation, names the tmux session
    pub group: usize,
    pub title: String,
    /// When this tab last saw *meaningful* activity (strict `Esc`/`Enter` input
    /// or an actual title change), as unix seconds. Stored as `u64` rather than
    /// `SystemTime` because serde encodes the latter as a nested struct, and
    /// this file's on-disk format is meant to stay hand-readable.
    /// `serde(default)` so state files predating the field load as `None`; the
    /// app seeds those with "now", once — tmux has no per-pane last-input or
    /// last-title-change to recover the real value from, so the persisted
    /// timestamp is the only truthful seed there is.
    #[serde(default)]
    pub last_activity: Option<u64>,
    /// The tab's title as of the last stamp, kept as the dedupe anchor across
    /// restarts. VTE's title signal fires on every title *write*, including
    /// writes of an unchanged string, so "did anything change?" is always an
    /// app-side comparison against this value. Persisting it stops the
    /// attach-time title re-emission from stamping every tab fresh on startup.
    #[serde(default)]
    pub last_title: Option<String>,
}

/// A remote session still to be killed: its tab was closed, and its host has
/// not confirmed the kill yet — because it was unreachable, or because the app
/// quit first. Remote hosts never adopt sessions, so without this entry the
/// session would run forever. Flushed after the next successful connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingKill {
    pub host: String,
    /// The closed tab's uuid; its session is `ks-<uuid>`.
    pub uuid: String,
}

/// The whole persisted presentation model.
///
/// `PartialEq` only, not `Eq`: `SavedGroup::browser_split` is a float.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedState {
    pub groups: Vec<SavedGroup>,
    pub tabs: Vec<SavedTab>,
    pub active: Option<String>, // uuid of the active tab
    pub sidebar_visible: bool,
    /// Whether the user permanently dismissed the linger (logout survival)
    /// warning. `serde(default)` so state files predating this field load
    /// with `false` (warning still shown).
    #[serde(default)]
    pub linger_warning_dismissed: bool,
    /// How the sidebar orders the tabs of a group. `serde(default)` so older
    /// state files load with the activity ordering.
    #[serde(default)]
    pub sidebar_order: SidebarOrder,
    /// Remote sessions whose tabs are closed but whose kill the host has not
    /// confirmed yet. `serde(default)` so older state files load with none.
    #[serde(default)]
    pub pending_kills: Vec<PendingKill>,
}

/// Sidebar ordering of a group's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarOrder {
    /// Most recent activity on top; the persisted vec order is ignored for
    /// display, which makes drag-and-drop within a group meaningless.
    #[default]
    Activity,
    /// The persisted vec order: new tabs on top of their group, drag-and-drop
    /// rearranges freely.
    Manual,
}

impl Default for SavedState {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            tabs: Vec::new(),
            active: None,
            sidebar_visible: true,
            linger_warning_dismissed: false,
            sidebar_order: SidebarOrder::default(),
            pending_kills: Vec::new(),
        }
    }
}

impl SavedState {
    /// Give every group a non-empty, unique uuid. Groups loaded from a state
    /// file written before uuids existed have an empty one; a duplicate can
    /// only come from a hand-edited or externally merged file. Both get a
    /// fresh uuid so the browser profile keys stay one-to-one with groups.
    ///
    /// Returns whether any uuid was actually minted. A `true` here means the
    /// in-memory state no longer matches the file on disk, and the caller must
    /// get it persisted before anything keys destructive work (the browser
    /// profile sweep) off these uuids.
    pub fn ensure_group_uuids(&mut self) -> bool {
        let mut backfilled = false;
        let mut seen: Vec<String> = Vec::with_capacity(self.groups.len());
        for group in &mut self.groups {
            if group.uuid.is_empty() || seen.contains(&group.uuid) {
                group.uuid = new_uuid();
                backfilled = true;
            }
            seen.push(group.uuid.clone());
        }
        backfilled
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

/// What [`load_detailed`] found, beyond the state itself.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadOutcome {
    pub state: SavedState,
    /// At least one group got a freshly minted uuid during this load, so the
    /// returned state differs from the file on disk. Until it is written back
    /// successfully, these uuids are not stable across restarts.
    pub uuids_backfilled: bool,
}

/// Load state from `path`. A missing file yields the empty default. A corrupt
/// file is renamed aside (to `<path>.corrupt`) and also yields the default —
/// running shells must never be lost over a bad JSON file.
pub fn load(path: &Path) -> SavedState {
    load_detailed(path).state
}

/// [`load`], plus whether group uuids had to be backfilled. Callers that key
/// destructive work off the uuids must use this and check `uuids_backfilled`.
pub fn load_detailed(path: &Path) -> LoadOutcome {
    let default = |uuids_backfilled: bool| LoadOutcome {
        state: SavedState::default(),
        uuids_backfilled,
    };
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(_) => return default(false),
    };
    match serde_json::from_slice::<SavedState>(&data) {
        Ok(mut state) => {
            // Backfill uuids for groups written before the field existed.
            let uuids_backfilled = state.ensure_group_uuids();
            LoadOutcome {
                state,
                uuids_backfilled,
            }
        }
        Err(_) => {
            let aside = path.with_extension("json.corrupt");
            let _ = fs::rename(path, &aside);
            default(false)
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

/// Where a freshly created tab belongs so that the sidebar reads newest
/// first: directly before the group's current first tab, or at the end when
/// the group has no tabs yet (group blocks are rendered per group, so the
/// position among other groups' tabs does not matter). `tab_groups` is the
/// group id of every existing tab in display order.
pub fn newest_first_index(tab_groups: &[usize], group: usize) -> usize {
    tab_groups
        .iter()
        .position(|g| *g == group)
        .unwrap_or(tab_groups.len())
}

/// Why a string cannot be used as an ssh destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    Empty,
    /// Whitespace or a control character.
    BadCharacter,
    /// A leading `-`, which ssh would parse as an option.
    LeadingDash,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            HostError::Empty => {
                "Enter a host, e.g. user@example.com or an alias from ~/.ssh/config."
            }
            HostError::BadCharacter => "A host can't contain spaces or control characters.",
            HostError::LeadingDash => {
                "A host can't start with '-': ssh would read it as an option."
            }
        })
    }
}

/// Check a group's host before it is stored. The destination is otherwise
/// passed to ssh verbatim, so this only rejects what cannot be one argument:
/// nothing, whitespace or control characters, and a leading `-`.
pub fn validate_host(host: &str) -> Result<(), HostError> {
    if host.is_empty() {
        return Err(HostError::Empty);
    }
    if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(HostError::BadCharacter);
    }
    if host.starts_with('-') {
        return Err(HostError::LeadingDash);
    }
    Ok(())
}

/// May a tab move from a group on `src_host` into one on `dest_host`? Tabs
/// cannot change hosts — their session lives on one tmux server — so only a
/// move within one host (or within this computer, `None`) is allowed. Gates
/// the move picker, both sidebar drops, and their drag feedback.
pub fn drop_allowed(src_host: Option<&str>, dest_host: Option<&str>) -> bool {
    src_host == dest_host
}

/// The age bucket an elapsed time (seconds) falls in, as the bucket's lower
/// bound: `0` for anything under a minute, then whole minutes, hours and
/// days — exactly the granularity the age prefix labels show. Both the
/// labels and the activity sort derive from this, so two tabs that read the
/// same age never swap places.
pub fn age_bucket(elapsed_secs: u64) -> u64 {
    let unit = match elapsed_secs {
        0..60 => return 0,
        60..3600 => 60,
        3600..86_400 => 3600,
        _ => 86_400,
    };
    elapsed_secs / unit * unit
}

/// Display order for the activity sort: indices into `ages` (each tab's age
/// bucket, `age_bucket`, in vec order), youngest first. Stable, so tabs of
/// equal age keep their vec order — a burst of output in two "now" tabs
/// must not make them flap.
pub fn activity_order(ages: &[u64]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..ages.len()).collect();
    order.sort_by_key(|&i| ages[i]);
    order
}

/// One sidebar group as keyboard navigation sees it: its tabs in display
/// order plus the tab its collapsed row shows (`Group::last_active`, falling
/// back to the first tab).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavGroup {
    pub tabs: Vec<usize>,
    pub representative: usize,
}

/// The tab Ctrl-Page_Up/Down (`step` = ±1) lands on from `active`, following
/// what the sidebar shows: the active group is expanded, so a step inside it
/// moves one row; every other group is a single row showing its
/// representative, so a step past either end of the active group lands on
/// the neighbouring group's representative, wrapping around at both ends.
/// A lone group wraps within itself. `None` when `active` is in no group.
pub fn nav_target(groups: &[NavGroup], active: usize, step: isize) -> Option<usize> {
    let (g, pos) = groups.iter().enumerate().find_map(|(g, group)| {
        group
            .tabs
            .iter()
            .position(|&t| t == active)
            .map(|pos| (g, pos))
    })?;
    let len = groups[g].tabs.len() as isize;
    let next = pos as isize + step;
    if (0..len).contains(&next) {
        return Some(groups[g].tabs[next as usize]);
    }
    let count = groups.len() as isize;
    let mut h = g as isize;
    for _ in 0..count {
        h = (h + step.signum()).rem_euclid(count);
        if h == g as isize {
            // Every other group is empty (or there is only one): wrap inside.
            return Some(groups[g].tabs[next.rem_euclid(len) as usize]);
        }
        if !groups[h as usize].tabs.is_empty() {
            return Some(groups[h as usize].representative);
        }
    }
    None
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

/// The host a saved group's tabs run on; `None` for a local group or an
/// unknown id.
pub fn group_host(saved: &SavedState, group: usize) -> Option<&str> {
    saved
        .groups
        .iter()
        .find(|g| g.id == group)
        .and_then(|g| g.host.as_deref())
}

/// The startup reconciliation of the local bucket: [`reconcile`] over the
/// tabs of local groups only. Remote tabs are neither respawned locally nor
/// counted when deciding what to adopt — their host is reconciled on its own,
/// by [`reconcile_remote`], once it answers.
pub fn reconcile_local(saved: &SavedState, live: &[String], dead: &[DeadPane]) -> ReconcilePlan {
    let local = SavedState {
        tabs: saved
            .tabs
            .iter()
            .filter(|t| group_host(saved, t.group).is_none())
            .cloned()
            .collect(),
        ..saved.clone()
    };
    reconcile(&local, live, dead)
}

/// Every remote host that has at least one saved tab, once, in group order:
/// the hosts to connect at startup. A host known only from its pending kills
/// is not connected unasked; its queue flushes on the next connect.
pub fn remote_hosts(saved: &SavedState) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for group in &saved.groups {
        let Some(host) = &group.host else { continue };
        if saved.tabs.iter().any(|t| t.group == group.id) && !hosts.contains(host) {
            hosts.push(host.clone());
        }
    }
    hosts
}

/// A successful `list-sessions` from a remote host, as plain data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteListing {
    /// Tab uuids of the live `ks-<uuid>` sessions.
    pub live: Vec<String>,
    /// The subset whose pane has died, with the exit code.
    pub dead: Vec<DeadPane>,
}

/// A remote tab whose session is live: attach, crashed if its pane died.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAttach {
    pub uuid: String,
    pub dead_exit: Option<i32>,
}

/// What to do with one host's tabs, as plain data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemotePlan {
    /// Tabs with a live session, in `tabs` order.
    pub attach: Vec<RemoteAttach>,
    /// Tabs without one: `new-session -A` creates a fresh session.
    pub respawn: Vec<String>,
    /// Tabs that must stay unspawned: there is no successful listing.
    pub wait: Vec<String>,
    /// Queued kills whose session still lives.
    pub kill: Vec<String>,
}

/// Plan one remote host's tabs. `listing` is `None` until the host answered
/// `list-sessions` successfully — and then every tab waits: an unreachable
/// host, a failed login or a refused host never produces a fresh shell.
/// Live `ks-*` sessions no tab claims are ignored, never adopted; they may
/// belong to another installation sharing the host's server.
pub fn reconcile_remote(
    tabs: &[String],
    listing: Option<&RemoteListing>,
    pending_kills: &[String],
) -> RemotePlan {
    let Some(listing) = listing else {
        return RemotePlan {
            wait: tabs.to_vec(),
            ..RemotePlan::default()
        };
    };
    let dead_exit = |uuid: &str| {
        listing
            .dead
            .iter()
            .find(|d| d.uuid == uuid)
            .map(|d| d.exit_code)
    };
    let mut plan = RemotePlan::default();
    for uuid in tabs {
        if listing.live.contains(uuid) {
            plan.attach.push(RemoteAttach {
                uuid: uuid.clone(),
                dead_exit: dead_exit(uuid),
            });
        } else {
            plan.respawn.push(uuid.clone());
        }
    }
    plan.kill = pending_kills
        .iter()
        .filter(|uuid| listing.live.contains(uuid) && !tabs.contains(uuid))
        .cloned()
        .collect();
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> SavedState {
        SavedState {
            groups: vec![
                SavedGroup {
                    uuid: "g-aaa".into(),
                    id: 0,
                    name: String::new(),
                    palette: 0,
                    browser_open: false,
                    browser_split: DEFAULT_BROWSER_SPLIT,
                    default_url: None,
                    host: None,
                },
                SavedGroup {
                    uuid: "g-bbb".into(),
                    id: 1,
                    name: "work".into(),
                    palette: 2,
                    browser_open: true,
                    browser_split: 640.0,
                    default_url: Some("http://localhost:3000".into()),
                    host: None,
                },
            ],
            tabs: vec![
                SavedTab {
                    uuid: "aaa".into(),
                    group: 0,
                    title: "bash".into(),
                    last_activity: Some(1_700_000_000),
                    last_title: Some("bash".into()),
                },
                SavedTab {
                    uuid: "bbb".into(),
                    group: 1,
                    title: "vim".into(),
                    last_activity: Some(1_700_000_500),
                    last_title: Some("vim".into()),
                },
                SavedTab {
                    uuid: "ccc".into(),
                    group: 1,
                    title: "logs".into(),
                    last_activity: None,
                    last_title: None,
                },
            ],
            active: Some("bbb".into()),
            sidebar_visible: false,
            linger_warning_dismissed: true,
            sidebar_order: SidebarOrder::default(),
            pending_kills: Vec::new(),
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
    fn legacy_state_without_group_uuids_gets_fresh_unique_ones() {
        // Written before SavedGroup carried a uuid: must still load, and every
        // group must come out with a non-empty, unique uuid.
        let json = r#"{
            "groups": [
                {"id": 0, "name": "", "palette": 0},
                {"id": 1, "name": "work", "palette": 2, "browser_open": true},
                {"id": 2, "name": "play", "palette": 3}
            ],
            "tabs": [],
            "active": null,
            "sidebar_visible": true
        }"#;
        let dir = tmp_dir("legacy-group-uuid");
        let path = dir.join("state.json");
        fs::write(&path, json).unwrap();

        let state = load(&path);
        assert_eq!(state.groups.len(), 3);
        let uuids: Vec<&str> = state.groups.iter().map(|g| g.uuid.as_str()).collect();
        assert!(uuids.iter().all(|u| !u.is_empty()));
        let mut sorted = uuids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "group uuids must be unique: {uuids:?}");
        // Non-uuid fields survive untouched.
        assert_eq!(state.groups[1].name, "work");
        assert!(state.groups[1].browser_open);
        assert_eq!(state.groups[0].browser_split, DEFAULT_BROWSER_SPLIT);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_detailed_reports_a_backfill() {
        let json = r#"{
            "groups": [{"id": 0, "name": "", "palette": 0}],
            "tabs": [], "active": null, "sidebar_visible": true
        }"#;
        let dir = tmp_dir("load-detailed-backfill");
        let path = dir.join("state.json");
        fs::write(&path, json).unwrap();

        let outcome = load_detailed(&path);
        assert!(outcome.uuids_backfilled);
        assert!(!outcome.state.groups[0].uuid.is_empty());

        // Writing the backfilled state back makes the next load clean.
        save(&outcome.state, &path).unwrap();
        let again = load_detailed(&path);
        assert!(!again.uuids_backfilled);
        assert_eq!(again.state, outcome.state);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_detailed_reports_no_backfill_for_missing_or_corrupt_files() {
        let dir = tmp_dir("load-detailed-nofile");
        // Missing: nothing was minted, so nothing needs persisting.
        assert!(!load_detailed(&dir.join("state.json")).uuids_backfilled);
        // Corrupt: the default state has no groups, so likewise.
        let path = dir.join("bad.json");
        fs::write(&path, b"{ nope").unwrap();
        let outcome = load_detailed(&path);
        assert!(!outcome.uuids_backfilled);
        assert_eq!(outcome.state, SavedState::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_group_uuids_is_idempotent_and_reports_it() {
        let mut state = sample_state();
        assert!(!state.ensure_group_uuids(), "already-valid uuids stay put");
        state.groups[1].uuid = state.groups[0].uuid.clone();
        assert!(state.ensure_group_uuids(), "a duplicate is a backfill");
        assert!(!state.ensure_group_uuids());
    }

    #[test]
    fn group_uuids_survive_save_load_roundtrip() {
        let dir = tmp_dir("group-uuid-roundtrip");
        let path = dir.join("state.json");
        let state = sample_state();
        save(&state, &path).unwrap();

        let back = load(&path);
        assert_eq!(back, state);
        let uuids: Vec<&str> = back.groups.iter().map(|g| g.uuid.as_str()).collect();
        assert_eq!(uuids, ["g-aaa", "g-bbb"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_group_uuids_are_regenerated_on_load() {
        // Only reachable through a hand-edited or merged file, but a duplicate
        // would make two groups share one browser profile directory.
        let mut state = SavedState {
            groups: vec![
                SavedGroup::new(0, String::new(), 0),
                SavedGroup::new(1, "work".into(), 1),
            ],
            ..SavedState::default()
        };
        state.groups[1].uuid = state.groups[0].uuid.clone();
        state.ensure_group_uuids();
        assert_ne!(state.groups[0].uuid, state.groups[1].uuid);
        assert!(!state.groups[1].uuid.is_empty());
    }

    #[test]
    fn new_uuid_is_unique_and_well_formed() {
        let a = new_uuid();
        let b = new_uuid();
        assert_ne!(a, b);
        for u in [&a, &b] {
            assert_eq!(u.len(), 36, "{u}");
            let parts: Vec<&str> = u.split('-').collect();
            assert_eq!(
                parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
                vec![8, 4, 4, 4, 12],
                "{u}"
            );
            assert!(u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{u}");
        }
    }

    #[test]
    fn saved_group_new_generates_a_uuid() {
        let g = SavedGroup::new(7, "x".into(), 3);
        assert!(!g.uuid.is_empty());
        assert_eq!(g.id, 7);
        assert_eq!(g.palette, 3);
        assert!(!g.browser_open);
        assert_eq!(g.browser_split, DEFAULT_BROWSER_SPLIT);
        assert_eq!(g.default_url, None);
    }

    #[test]
    fn old_state_without_linger_flag_defaults_to_not_dismissed() {
        // A state file written before the linger flag existed must still load,
        // with the warning not dismissed.
        let json = r#"{
            "groups": [],
            "tabs": [],
            "active": null,
            "sidebar_visible": true
        }"#;
        let state: SavedState = serde_json::from_str(json).unwrap();
        assert!(!state.linger_warning_dismissed);
        assert_eq!(state, SavedState::default());
    }

    #[test]
    fn linger_flag_round_trips() {
        let mut state = sample_state();
        state.linger_warning_dismissed = true;
        let json = serde_json::to_string(&state).unwrap();
        let back: SavedState = serde_json::from_str(&json).unwrap();
        assert!(back.linger_warning_dismissed);
        assert_eq!(back, state);
    }

    #[test]
    fn old_group_without_browser_fields_loads() {
        // A state.json written before the browser pane existed.
        let json = r#"{
            "groups": [{ "id": 3, "name": "work", "palette": 1 }],
            "tabs": [],
            "active": null,
            "sidebar_visible": true
        }"#;
        let state: SavedState = serde_json::from_str(json).unwrap();
        assert_eq!(state.groups.len(), 1);
        assert_eq!(state.groups[0].id, 3);
        assert_eq!(state.groups[0].name, "work");
        assert_eq!(state.groups[0].palette, 1);
        assert!(!state.groups[0].browser_open);
        assert_eq!(state.groups[0].browser_split, DEFAULT_BROWSER_SPLIT);
    }

    #[test]
    fn old_group_without_default_url_loads() {
        // A state.json written before the per-group default URL existed. The
        // absent field is the whole migration: it loads as "no default URL".
        let json = r#"{
            "groups": [{
                "id": 3,
                "name": "work",
                "palette": 1,
                "browser_open": true,
                "browser_split": 640.0
            }],
            "tabs": [],
            "active": null,
            "sidebar_visible": true
        }"#;
        let state: SavedState = serde_json::from_str(json).unwrap();
        assert_eq!(state.groups.len(), 1);
        assert_eq!(state.groups[0].default_url, None);
        assert!(state.groups[0].browser_open);
        assert_eq!(state.groups[0].browser_split, 640.0);
    }

    #[test]
    fn default_url_round_trips() {
        let mut group = SavedGroup::new(4, "dev".into(), 0);
        group.default_url = Some("http://localhost:3000".into());
        let json = serde_json::to_string(&group).unwrap();
        let back: SavedGroup = serde_json::from_str(&json).unwrap();
        assert_eq!(back.default_url.as_deref(), Some("http://localhost:3000"));
        assert_eq!(back, group);
    }

    #[test]
    fn browser_fields_round_trip() {
        let state = sample_state();
        let json = serde_json::to_string(&state).unwrap();
        let back: SavedState = serde_json::from_str(&json).unwrap();
        assert!(!back.groups[0].browser_open);
        assert!(back.groups[1].browser_open);
        assert_eq!(back.groups[1].browser_split, 640.0);
        assert_eq!(back, state);
    }

    #[test]
    fn browser_fields_survive_save_and_load() {
        let dir = tmp_dir("browserfields");
        let path = dir.join("state.json");
        let state = sample_state();
        save(&state, &path).unwrap();
        let back = load(&path);
        assert!(back.groups[1].browser_open);
        assert_eq!(back.groups[1].browser_split, 640.0);
        let _ = fs::remove_dir_all(&dir);
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

    // --- tab age fields ---

    #[test]
    fn tab_age_fields_round_trip() {
        let tab = SavedTab {
            uuid: "aaa".into(),
            group: 0,
            title: "claude".into(),
            last_activity: Some(1_755_000_000),
            last_title: Some("claude — writing tests".into()),
        };
        let json = serde_json::to_string(&tab).unwrap();
        let back: SavedTab = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_activity, Some(1_755_000_000));
        assert_eq!(back.last_title.as_deref(), Some("claude — writing tests"));
        assert_eq!(back, tab);
    }

    #[test]
    fn old_tab_without_age_fields_loads_as_none() {
        // A state.json written before tab ages existed. Absent fields are the
        // whole migration: the app seeds such tabs with "now", once.
        let json = r#"{
            "groups": [],
            "tabs": [{ "uuid": "aaa", "group": 0, "title": "bash" }],
            "active": null,
            "sidebar_visible": true
        }"#;
        let state: SavedState = serde_json::from_str(json).unwrap();
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].title, "bash");
        assert_eq!(state.tabs[0].last_activity, None);
        assert_eq!(state.tabs[0].last_title, None);
    }

    #[test]
    fn reconcile_carries_age_fields_through_attach_and_respawn() {
        // Neither path may silently reset a tab's age: an attached tab keeps the
        // persisted stamp, and a respawned one keeps it too — the shell is new,
        // but "when did the user last do something here" is unchanged by the
        // fact that we had to recreate the session.
        let state = sample_state();
        let plan = reconcile(&state, &["aaa".to_string()], &[]);
        assert_eq!(plan.attach[0].tab.last_activity, Some(1_700_000_000));
        assert_eq!(plan.attach[0].tab.last_title.as_deref(), Some("bash"));
        assert_eq!(plan.respawn[0].last_activity, Some(1_700_000_500));
        assert_eq!(plan.respawn[0].last_title.as_deref(), Some("vim"));
        assert_eq!(plan.respawn[1].last_activity, None);
        assert_eq!(plan.respawn[1].last_title, None);
    }

    #[test]
    fn newest_first_index_lands_before_the_groups_first_tab() {
        // Tabs of groups 0,1,1,2 already exist. A new group-1 tab goes in
        // front of the first group-1 tab; a new group-2 tab in front of the
        // only group-2 tab; a group with no tabs yet appends at the end.
        let groups = [0, 1, 1, 2];
        assert_eq!(newest_first_index(&groups, 1), 1);
        assert_eq!(newest_first_index(&groups, 2), 3);
        assert_eq!(newest_first_index(&groups, 0), 0);
        assert_eq!(newest_first_index(&groups, 7), 4);
        assert_eq!(newest_first_index(&[], 0), 0);
    }

    #[test]
    fn activity_order_is_youngest_first_and_stable() {
        assert_eq!(activity_order(&[3600, 0, 60]), vec![1, 2, 0]);
        // Equal ages keep their vec order.
        assert_eq!(activity_order(&[60, 0, 60, 0]), vec![1, 3, 0, 2]);
        assert_eq!(activity_order(&[]), Vec::<usize>::new());
    }

    #[test]
    fn age_bucket_matches_the_label_granularity() {
        // Everything under a minute is one bucket, so two tabs that both
        // read "now" compare equal however their seconds differ.
        assert_eq!(age_bucket(0), 0);
        assert_eq!(age_bucket(59), 0);
        assert_eq!(age_bucket(60), 60);
        assert_eq!(age_bucket(119), 60);
        assert_eq!(age_bucket(120), 120);
        assert_eq!(age_bucket(3_599), 59 * 60);
        assert_eq!(age_bucket(3_600), 3_600);
        assert_eq!(age_bucket(7_199), 3_600);
        assert_eq!(age_bucket(86_399), 23 * 3_600);
        assert_eq!(age_bucket(86_400), 86_400);
        assert_eq!(age_bucket(40 * 86_400 + 5), 40 * 86_400);
        // Monotonic: never orders an older tab above a younger one.
        let samples = [0, 1, 59, 60, 61, 3_599, 3_600, 86_399, 86_400, 900_000];
        assert!(
            samples
                .windows(2)
                .all(|w| age_bucket(w[0]) <= age_bucket(w[1]))
        );
    }

    fn nav(tabs: &[usize], representative: usize) -> NavGroup {
        NavGroup {
            tabs: tabs.to_vec(),
            representative,
        }
    }

    #[test]
    fn nav_target_steps_inside_the_active_group() {
        let groups = [nav(&[1, 2, 3], 2), nav(&[4, 5], 5)];
        assert_eq!(nav_target(&groups, 1, 1), Some(2));
        assert_eq!(nav_target(&groups, 2, 1), Some(3));
        assert_eq!(nav_target(&groups, 3, -1), Some(2));
    }

    #[test]
    fn nav_target_enters_the_neighbouring_group_on_its_representative() {
        // Group 1's collapsed row shows tab 5, so that is where a step past
        // the end of group 0 lands — in either direction, not on the
        // neighbouring group's first or last tab.
        let groups = [nav(&[1, 2, 3], 2), nav(&[4, 5, 6], 5)];
        assert_eq!(nav_target(&groups, 3, 1), Some(5));
        assert_eq!(nav_target(&groups, 4, -1), Some(2));
        // Wraps around at both ends.
        assert_eq!(nav_target(&groups, 6, 1), Some(2));
        assert_eq!(nav_target(&groups, 1, -1), Some(5));
    }

    #[test]
    fn nav_target_skips_empty_groups_and_wraps_a_lone_group() {
        let groups = [nav(&[], 0), nav(&[1, 2], 1), nav(&[], 0), nav(&[3], 3)];
        assert_eq!(nav_target(&groups, 2, 1), Some(3));
        assert_eq!(nav_target(&groups, 1, -1), Some(3));
        assert_eq!(nav_target(&groups, 3, 1), Some(1));

        let lone = [nav(&[1, 2, 3], 1)];
        assert_eq!(nav_target(&lone, 3, 1), Some(1));
        assert_eq!(nav_target(&lone, 1, -1), Some(3));
        assert_eq!(nav_target(&[nav(&[7], 7)], 7, 1), Some(7));
    }

    #[test]
    fn nav_target_is_none_for_an_unknown_tab() {
        assert_eq!(nav_target(&[nav(&[1, 2], 1)], 9, 1), None);
        assert_eq!(nav_target(&[], 1, 1), None);
    }

    #[test]
    fn sidebar_order_defaults_to_activity_and_round_trips() {
        let json = r#"{"groups": [], "tabs": [], "active": null, "sidebar_visible": true}"#;
        let state: SavedState = serde_json::from_str(json).unwrap();
        assert_eq!(state.sidebar_order, SidebarOrder::Activity);

        let manual = SavedState {
            sidebar_order: SidebarOrder::Manual,
            ..state
        };
        let json = serde_json::to_string(&manual).unwrap();
        assert!(json.contains(r#""sidebar_order":"manual""#));
        let back: SavedState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sidebar_order, SidebarOrder::Manual);
    }

    // --- remote groups ---

    #[test]
    fn old_group_without_host_loads_as_local() {
        let json = r#"{
            "groups": [{"id": 1, "name": "w", "palette": 0}],
            "tabs": [], "active": null, "sidebar_visible": true
        }"#;
        let state: SavedState = serde_json::from_str(json).unwrap();
        assert_eq!(state.groups[0].host, None);
        assert!(state.pending_kills.is_empty());
    }

    #[test]
    fn host_and_pending_kills_survive_save_and_load() {
        let dir = tmp_dir("remote-fields");
        let path = dir.join("state.json");
        let mut state = sample_state();
        state.groups[1].host = Some("me@build-box".into());
        state.pending_kills = vec![PendingKill {
            host: "me@build-box".into(),
            uuid: "dead-beef".into(),
        }];
        save(&state, &path).unwrap();
        let back = load(&path);
        assert_eq!(back.groups[1].host.as_deref(), Some("me@build-box"));
        assert_eq!(back.pending_kills, state.pending_kills);
        assert_eq!(back, state);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_group_new_is_local() {
        assert_eq!(SavedGroup::new(1, "x".into(), 0).host, None);
    }

    #[test]
    fn validate_host_accepts_ssh_destinations_verbatim() {
        for host in [
            "build-box",
            "me@build-box",
            "ssh://me@build-box:2222",
            "me@[::1]",
            "10.0.0.7",
        ] {
            assert_eq!(validate_host(host), Ok(()), "{host}");
        }
    }

    #[test]
    fn validate_host_rejects_empty_whitespace_control_and_dash() {
        assert_eq!(validate_host(""), Err(HostError::Empty));
        assert_eq!(validate_host("me@build box"), Err(HostError::BadCharacter));
        assert_eq!(validate_host(" build-box"), Err(HostError::BadCharacter));
        assert_eq!(validate_host("build-box\n"), Err(HostError::BadCharacter));
        assert_eq!(validate_host("build\u{7}box"), Err(HostError::BadCharacter));
        assert_eq!(
            validate_host("-oProxyCommand=evil"),
            Err(HostError::LeadingDash)
        );
    }

    #[test]
    fn host_errors_read_as_sentences() {
        assert!(HostError::LeadingDash.to_string().contains("'-'"));
        assert!(!HostError::Empty.to_string().is_empty());
    }

    #[test]
    fn drop_allowed_only_within_one_host() {
        assert!(drop_allowed(None, None));
        assert!(drop_allowed(Some("a"), Some("a")));
        assert!(!drop_allowed(None, Some("a")));
        assert!(!drop_allowed(Some("a"), None));
        // Destinations are compared verbatim: two spellings are two hosts.
        assert!(!drop_allowed(Some("a"), Some("me@a")));
    }

    // --- per-host restore ---

    fn remote_state() -> SavedState {
        let mut state = sample_state();
        // Group 1 ("work", tabs bbb and ccc) lives on a remote host.
        state.groups[1].host = Some("me@box".into());
        state
    }

    #[test]
    fn a_local_only_state_reconciles_exactly_as_before() {
        let state = sample_state();
        for live in [
            vec![],
            vec!["aaa".to_string()],
            vec!["zzz".to_string(), "bbb".to_string()],
        ] {
            let dead = vec![DeadPane {
                uuid: "bbb".into(),
                exit_code: 3,
            }];
            assert_eq!(
                reconcile_local(&state, &live, &dead),
                reconcile(&state, &live, &dead)
            );
        }
    }

    #[test]
    fn local_reconcile_never_respawns_remote_tabs_locally() {
        let plan = reconcile_local(&remote_state(), &[], &[]);
        let respawned: Vec<_> = plan.respawn.iter().map(|t| t.uuid.as_str()).collect();
        assert_eq!(respawned, ["aaa"]);
        assert!(plan.attach.is_empty());
        assert!(plan.adopt.is_empty());
    }

    #[test]
    fn local_reconcile_still_adopts_local_orphans() {
        let plan = reconcile_local(&remote_state(), &["zzz".to_string()], &[]);
        let adopted: Vec<_> = plan.adopt.iter().map(|o| o.uuid.as_str()).collect();
        assert_eq!(adopted, ["zzz"]);
    }

    #[test]
    fn group_host_names_the_host_or_none() {
        let state = remote_state();
        assert_eq!(group_host(&state, 0), None);
        assert_eq!(group_host(&state, 1), Some("me@box"));
        assert_eq!(group_host(&state, 99), None);
    }

    #[test]
    fn an_unreachable_host_never_respawns() {
        // No successful list-sessions: every tab waits, nothing is spawned,
        // attached or killed — whatever is queued.
        let tabs = vec!["x".to_string(), "y".to_string()];
        let plan = reconcile_remote(&tabs, None, &["k".to_string()]);
        assert!(plan.attach.is_empty());
        assert!(plan.respawn.is_empty());
        assert!(plan.kill.is_empty());
        assert_eq!(plan.wait, tabs);
    }

    #[test]
    fn a_listed_host_attaches_and_respawns_but_never_adopts() {
        let tabs = vec!["x".to_string(), "y".to_string()];
        let listing = RemoteListing {
            // "stranger" may belong to another installation: ignored.
            live: vec!["stranger".to_string(), "x".to_string()],
            dead: vec![DeadPane {
                uuid: "x".into(),
                exit_code: 3,
            }],
        };
        let plan = reconcile_remote(&tabs, Some(&listing), &[]);
        assert_eq!(
            plan.attach,
            vec![RemoteAttach {
                uuid: "x".into(),
                dead_exit: Some(3)
            }]
        );
        assert_eq!(plan.respawn, vec!["y".to_string()]);
        assert!(plan.wait.is_empty());
        assert!(plan.kill.is_empty());
    }

    #[test]
    fn pending_kills_flush_only_sessions_that_still_live() {
        let listing = RemoteListing {
            live: vec!["k1".to_string()],
            dead: Vec::new(),
        };
        let plan = reconcile_remote(&[], Some(&listing), &["k1".to_string(), "k2".to_string()]);
        assert_eq!(plan.kill, vec!["k1".to_string()]);
    }

    #[test]
    fn remote_hosts_lists_each_host_with_tabs_once_in_group_order() {
        let mut state = remote_state();
        let mut second = SavedGroup::new(2, "again".into(), 0);
        second.host = Some("me@box".into());
        let mut empty = SavedGroup::new(3, "idle".into(), 0);
        empty.host = Some("other".into());
        let mut third = SavedGroup::new(4, "b".into(), 0);
        third.host = Some("b-host".into());
        state.groups.extend([second, empty, third]);
        for (uuid, group) in [("d", 2), ("e", 4)] {
            state.tabs.push(SavedTab {
                uuid: uuid.into(),
                group,
                title: String::new(),
                last_activity: None,
                last_title: None,
            });
        }
        assert_eq!(remote_hosts(&state), ["me@box", "b-host"]);
        assert!(remote_hosts(&sample_state()).is_empty());
    }
}
