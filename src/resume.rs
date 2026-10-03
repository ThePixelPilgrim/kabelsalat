//! `kabelsalat resume`: what the boot unit runs (see
//! docs/superpowers/specs/2026-10-02-boot-resume-design.md).
//!
//! Recreates the claude sessions of saved tabs on the private tmux server,
//! without a GUI, so they are already running when the user opens kabelsalat
//! after a reboot. Glue only: which tabs is `state::boot_resume_plan`, how is
//! `tmuxctl`'s detached creation. This never writes `state.json` — the GUI
//! stays its single writer and finds these sessions live when it starts.

use crate::cli::{ENV_GROUP, EXIT_FAILED, EXIT_OK};
use crate::state;
use crate::tmuxctl::{self, Created, TmuxAvailability, TmuxCtl};

/// Run the resume and return the process exit code. `gui_running` is the
/// session-bus check from `lib.rs`: a running GUI owns the sessions, so
/// there is nothing to do — and nothing wrong.
pub fn run(gui_running: bool) -> u8 {
    if gui_running {
        eprintln!(
            "kabelsalat: the GUI is running and manages the sessions itself; nothing to resume"
        );
        return EXIT_OK;
    }
    let ctl = match tmuxctl::detect() {
        TmuxAvailability::Available(_) => match TmuxCtl::new() {
            Ok(ctl) => ctl,
            Err(err) => {
                eprintln!("kabelsalat: tmux setup failed: {err}");
                return EXIT_FAILED;
            }
        },
        TmuxAvailability::TooOld(found) => {
            let (major, minor) = tmuxctl::MIN_VERSION;
            eprintln!("kabelsalat: tmux {found} is older than the required {major}.{minor}");
            return EXIT_FAILED;
        }
        TmuxAvailability::Missing => {
            eprintln!("kabelsalat: tmux is not installed");
            return EXIT_FAILED;
        }
    };
    // Same start as the GUI's: out of this unit's cgroup, so the server
    // outlives this oneshot.
    if let Err(err) = ctl.ensure_server(tmuxctl::has_systemd_run()) {
        eprintln!("kabelsalat: starting the tmux server: {err}");
        return EXIT_FAILED;
    }
    let loaded = state::load_detailed(&state::state_file());
    let live: Vec<String> = match ctl.list_sessions() {
        Ok(sessions) => sessions.into_iter().map(|s| s.uuid).collect(),
        Err(err) => {
            eprintln!("kabelsalat: tmux list-sessions failed: {err}");
            return EXIT_FAILED;
        }
    };
    let plan = state::boot_resume_plan(&loaded.state, &live);
    if plan.is_empty() {
        println!("nothing to resume");
        return EXIT_OK;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let mut failed = 0usize;
    for tab in &plan {
        let Some(claude) = &tab.claude else { continue };
        // A group uuid minted by this load is not on disk, so it is not
        // what the GUI will know the group by; leave the stamp to the GUI,
        // which refreshes every attached session's group anyway.
        let group_uuid = if loaded.uuids_backfilled {
            None
        } else {
            loaded
                .state
                .groups
                .iter()
                .find(|g| g.id == tab.group)
                .map(|g| g.uuid.as_str())
        };
        let env: Vec<(&str, &str)> = group_uuid
            .map(|uuid| (ENV_GROUP, uuid))
            .into_iter()
            .collect();
        let argv = state::boot_resume_argv(&shell, claude);
        match ctl.create_detached_session(&tab.uuid, Some(&claude.cwd), &argv, &env) {
            Ok(Created::Created) => {
                println!("resumed claude {} in {}", claude.id, claude.cwd.display());
            }
            Ok(Created::AlreadyExists) => {
                println!("claude {} already has its session", claude.id);
            }
            Err(err) => {
                eprintln!("kabelsalat: resuming claude {}: {err}", claude.id);
                failed += 1;
            }
        }
    }
    if failed > 0 { EXIT_FAILED } else { EXIT_OK }
}
