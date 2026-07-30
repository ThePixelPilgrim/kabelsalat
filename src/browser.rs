//! Per-group browser pane: a klamottenkiste `WaylandPane` plus the Chromium
//! process hosted inside it, owned as one unit.
//!
//! Nothing else in the codebase touches `WaylandPane` or spawns a browser.
//! Every fallible path returns `Result`; teardown never panics.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use klamottenkiste::WaylandPane;
use relm4::gtk::prelude::WidgetExt;

/// Chromium binaries tried at spawn time, in order.
pub const BROWSER_CANDIDATES: &[&str] = &["chromium", "chromium-browser", "google-chrome"];

/// Directory under the state dir holding one profile per group *uuid*.
///
/// Group ids are reused (`max(id) + 1`), so a profile must never be keyed by
/// id: a freshly created group could inherit a directory that is still queued
/// for deletion. The per-group uuid from `state::SavedGroup` is stable and
/// never reused, which makes that collision impossible.
pub const PROFILES_SUBDIR: &str = "browsers";

/// Sentinel Chromium looks for in a user-data-dir to decide whether this is a
/// first run. Its presence — not its contents — is what matters.
const FIRST_RUN_SENTINEL: &str = "First Run";

/// Chromium's default profile directory inside a user-data-dir.
const DEFAULT_PROFILE_SUBDIR: &str = "Default";

/// The profile's settings file, read once at startup and rewritten by Chromium.
const PREFERENCES_FILE: &str = "Preferences";

/// Settings a new pane profile starts with. Chromium merges anything absent from
/// its own defaults, so this stays to what an embedded pane actually needs:
///
/// * no default-browser prompt — this browser lives in a terminal group and is
///   nobody's system default;
/// * the welcome page already seen, so a restored group opens on its own page;
/// * notification permission prompts denied by default — a pane cannot usefully
///   grant them, so they are pure interruption.
const DEFAULT_PREFERENCES: &str = r#"{
  "browser": {
    "check_default_browser": false,
    "has_seen_welcome_page": true
  },
  "profile": {
    "default_content_setting_values": {
      "notifications": 2
    }
  }
}
"#;

/// How long Chromium gets to exit after `SIGTERM` before it is `SIGKILL`ed.
///
/// This is spent blocking on the GTK main thread, so it is deliberately short:
/// long enough for Chromium to flush its session file and drop the profile's
/// `SingletonLock`, short enough to be invisible in the UI.
pub const TERM_GRACE: Duration = Duration::from_millis(300);

/// Poll interval while waiting out [`TERM_GRACE`].
const TERM_POLL: Duration = Duration::from_millis(5);

/// Prefix of the temporary name a profile is renamed to before it is deleted
/// in the background. Never equals a group uuid, so the startup sweep treats
/// leftovers as stale and finishes the job.
pub const TRASH_PREFIX: &str = ".trash-";

/// What happens to the profile directory when a [`Browser`] is torn down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileDisposition {
    /// Deliberate close: the profile is garbage, delete it.
    Remove,
    /// Unexpected death or app exit: keep it so the next launch restores the
    /// session, cookies and logins.
    Keep,
}

/// Everything that can go wrong bringing a browser up.
#[derive(Debug)]
pub enum BrowserError {
    /// The nested compositor did not come up (no DRM render node, no EGL, ...).
    Compositor(String),
    /// None of [`BROWSER_CANDIDATES`] was found on `PATH`.
    NoBinary,
    /// The profile directory could not be created.
    Profile(std::io::Error),
    /// `Command::spawn` failed.
    Spawn(std::io::Error),
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compositor(msg) => {
                write!(f, "The browser compositor could not start: {msg}")
            }
            Self::NoBinary => write!(
                f,
                "No Chromium binary found. Tried: {}.",
                BROWSER_CANDIDATES.join(", ")
            ),
            Self::Profile(err) => {
                write!(
                    f,
                    "The browser profile directory could not be created: {err}"
                )
            }
            Self::Spawn(err) => write!(f, "Chromium could not be started: {err}"),
        }
    }
}

impl std::error::Error for BrowserError {}

/// Schemes a group's default URL may use, lowercase. Anything else is refused:
/// a browser pane opens pages, and these are the only forms Chromium is asked
/// to treat as one here.
pub const DEFAULT_URL_SCHEMES: &[&str] = &["http", "https", "file"];

/// Scheme kabelsalat assumes when the user types a bare host, so
/// `localhost:3000` works the way it does in an address bar.
const IMPLIED_SCHEME: &str = "http";

/// Why a group's default URL was refused. Its [`Display`](fmt::Display) text is
/// what the group settings dialog shows under the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultUrlError {
    /// Starts with `-`, so Chromium would read it as a command-line flag.
    LooksLikeFlag,
    /// A `scheme://` that is not one of [`DEFAULT_URL_SCHEMES`].
    UnsupportedScheme(String),
    /// Structurally not a URL: interior whitespace, or nothing where the host
    /// (`http`/`https`) or the path (`file`) has to be.
    NotAUrl,
}

impl fmt::Display for DefaultUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LooksLikeFlag => write!(
                f,
                "A URL cannot start with “-”: the browser would read it as a command-line flag."
            ),
            Self::UnsupportedScheme(scheme) => write!(
                f,
                "“{scheme}://” is not supported. Use {}.",
                DEFAULT_URL_SCHEMES
                    .iter()
                    .map(|s| format!("{s}://"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::NotAUrl => write!(
                f,
                "That is not a URL. Try something like localhost:3000, \
                 https://example.org or file:///home/you/notes.html."
            ),
        }
    }
}

impl std::error::Error for DefaultUrlError {}

/// Validate and canonicalize a group's default URL.
///
/// `Ok(None)` means "no default URL" — the empty input is how the setting is
/// cleared, so an empty string is a valid answer rather than an error. The
/// returned string has its scheme lowercased and the rest kept exactly as
/// typed; a bare host gains an implied `http://`, the way an address bar does
/// it.
///
/// Pure: no filesystem, no network, no reachability check. Anything a browser
/// cannot load is Chromium's error page, not this function's business. The one
/// thing it exists to make impossible is a value that reaches Chromium as a
/// *flag* instead of a URL — which is why it also runs again at spawn time,
/// against whatever `state.json` happens to contain.
///
/// Deliberate limits of a hand-rolled parser: IPv6 literals and userinfo pass
/// through untouched, and no percent-encoding, punycode or path normalization
/// happens.
pub fn normalize_default_url(input: &str) -> Result<Option<String>, DefaultUrlError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    // Before anything else: this is the case the whole helper exists for.
    if trimmed.starts_with('-') {
        return Err(DefaultUrlError::LooksLikeFlag);
    }
    // Whitespace inside would split into more than one Chromium argument.
    if trimmed.chars().any(char::is_whitespace) {
        return Err(DefaultUrlError::NotAUrl);
    }

    // No `scheme://` at all: assume one, then apply the same rules.
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return normalize_default_url(&format!("{IMPLIED_SCHEME}://{trimmed}"));
    };
    let scheme = scheme.to_ascii_lowercase();
    if !DEFAULT_URL_SCHEMES.contains(&scheme.as_str()) {
        return Err(DefaultUrlError::UnsupportedScheme(scheme));
    }

    // The requirement is per scheme: `file:///home/c/notes.html` has no host by
    // construction — everything after `://` is its path, and that is what has
    // to be there. For http/https it is the host that must not be empty.
    if scheme == "file" {
        if rest.is_empty() {
            return Err(DefaultUrlError::NotAUrl);
        }
    } else {
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        if host.is_empty() {
            return Err(DefaultUrlError::NotAUrl);
        }
    }

    Ok(Some(format!("{scheme}://{rest}")))
}

/// A live browser: the pane widget, the Chromium process, and its profile dir.
pub struct Browser {
    pane: WaylandPane,
    child: Child,
    profile: PathBuf,
    /// Set once the child has been killed and reaped, so `Drop` does not retry.
    torn_down: bool,
    /// Decided by the first [`Browser::teardown`] call; `Drop` obeys it.
    /// Defaults to `Remove` for a browser that is dropped without one.
    disposition: ProfileDisposition,
    /// Set once the profile has been handed to the removal worker, so `Drop`
    /// does not queue it a second time.
    profile_handled: bool,
}

impl Browser {
    /// Create a pane, then launch Chromium into its Wayland socket.
    ///
    /// `state_dir` is the kabelsalat state directory; the profile lives at
    /// `<state_dir>/browsers/<group_uuid>`. On any failure the pane is torn
    /// down before returning, so no blank pane is ever left behind, and a
    /// profile directory this call created is rolled back.
    ///
    /// `default_url` is the group's configured start page. It is used only for a
    /// launch into a profile this call creates, and an unusable value is dropped
    /// rather than turned into an error: a bad default URL costs a start page,
    /// never a browser.
    pub fn spawn(
        group_uuid: &str,
        state_dir: &Path,
        default_url: Option<&str>,
    ) -> Result<Self, BrowserError> {
        let pane = WaylandPane::new();
        // `wayland_socket() == None` is the startup-failure signal; the message
        // (if any) lives in `startup_error()`.
        let socket = match pane.wayland_socket() {
            Some(socket) => socket,
            None => {
                let msg = pane
                    .startup_error()
                    .unwrap_or_else(|| "no Wayland socket was advertised".to_string());
                pane.close();
                return Err(BrowserError::Compositor(msg));
            }
        };

        let binary = match resolve_binary() {
            Some(binary) => binary,
            None => {
                pane.close();
                return Err(BrowserError::NoBinary);
            }
        };

        let profile = profile_dir(state_dir, group_uuid);
        // Remember whether the directory was already there, so a later failure
        // only rolls back a directory *this* call created.
        let profile_existed = profile.is_dir();
        if let Err(err) = std::fs::create_dir_all(&profile) {
            pane.close();
            return Err(BrowserError::Profile(err));
        }
        // Only a profile this call created: seeding an existing one would throw
        // away settings the user made inside the pane.
        if !profile_existed && let Err(err) = seed_profile(&profile) {
            pane.close();
            let _ = remove_profile(&profile);
            return Err(BrowserError::Profile(err));
        }

        let mut command = Command::new(&binary);
        command
            .arg("--ozone-platform=wayland")
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("--restore-last-session")
            // The first-run wizard is for someone setting up a browser, not for a
            // pane in a terminal: the group's browser should come up on the page
            // the user wanted, not on a welcome flow.
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            // Teardown gives Chromium `TERM_GRACE` and then kills it, so it usually
            // records an unclean exit and offers to restore pages on the next start.
            // That prompt is an artefact of how this app stops it, not something the
            // user did — and `--restore-last-session` already brings the tabs back.
            .arg("--hide-crash-restore-bubble")
            .env("WAYLAND_DISPLAY", &socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Only a profile this call created: an existing one has a session that
        // `--restore-last-session` brings back, and the group's default URL would
        // pile a duplicate tab on top of it every crash and every restart.
        // Re-validated here rather than trusted: `state.json` is user-editable, and
        // a value beginning with `-` would reach Chromium as a *flag*, not a URL.
        if !profile_existed
            && let Ok(Some(url)) = normalize_default_url(default_url.unwrap_or_default())
        {
            command.arg(url);
        }
        // Chromium is a process *tree* (zygote, GPU process, one renderer per
        // tab). Signalling the direct pid alone leaves those behind, so the
        // whole tree gets its own process group and is signalled with
        // `killpg` at teardown.
        isolate_process_group(&mut command);

        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                pane.close();
                if !profile_existed {
                    // Nothing ever ran in it: leave no empty profile behind.
                    let _ = remove_profile(&profile);
                }
                return Err(BrowserError::Spawn(err));
            }
        };

        pane.set_hexpand(true);
        pane.set_vexpand(true);

        Ok(Self {
            pane,
            child,
            profile,
            torn_down: false,
            disposition: ProfileDisposition::Remove,
            profile_handled: false,
        })
    }

    /// The widget to parent into the layout.
    pub fn widget(&self) -> &WaylandPane {
        &self.pane
    }

    /// Show or hide the pane. Hiding only pauses the frame pump — the
    /// compositor and Chromium keep running, so page state survives.
    pub fn set_visible(&self, visible: bool) {
        self.pane.set_visible(visible);
    }

    /// Has the hosted Chromium exited? `Ok(true)` means it is gone.
    ///
    /// klamottenkiste cannot report this (`is_running()` describes the
    /// compositor only), so the caller polls this on a timer.
    pub fn has_exited(&mut self) -> Result<bool, std::io::Error> {
        if self.torn_down {
            return Ok(true);
        }
        Ok(self.child.try_wait()?.is_some())
    }

    /// The profile directory backing this browser.
    pub fn profile(&self) -> &Path {
        &self.profile
    }

    /// Terminate Chromium, close the pane, and dispose of the profile as asked.
    /// Idempotent, and the first call decides the disposition `Drop` obeys.
    ///
    /// Order matters and is guaranteed: the child is asked to exit (`SIGTERM`,
    /// then `SIGKILL` after [`TERM_GRACE`]) and reaped *before* the pane — i.e.
    /// the compositor it renders into — is closed.
    ///
    /// With [`ProfileDisposition::Remove`] the profile directory is renamed out
    /// of the way immediately and deleted on a worker thread, so no multi-
    /// hundred-megabyte `remove_dir_all` runs on the GTK main thread.
    pub fn teardown(&mut self, disposition: ProfileDisposition) {
        if !self.torn_down {
            self.torn_down = true;
            self.disposition = disposition;
            terminate_child(&mut self.child, TERM_GRACE);
        }
        // `close()` is idempotent and infallible upstream.
        self.pane.close();
        if self.disposition == ProfileDisposition::Remove && !self.profile_handled {
            self.profile_handled = true;
            remove_profile_in_background(&self.profile);
        }
    }

    /// Terminate Chromium and close the pane, but keep the profile directory so
    /// the next start can restore the session. For app exit, where the group
    /// still exists and its browser is meant to come back, and for an
    /// unexpected Chromium death, where wiping the profile would lose the
    /// user's cookies, logins and tabs.
    pub fn detach(&mut self) {
        self.teardown(ProfileDisposition::Keep);
    }
}

/// Tear down MANY browsers against ONE shared grace period.
///
/// Serial teardown costs `n * grace` of frozen UI at app exit. This signals
/// every child with `SIGTERM` first, then polls all of them against a single
/// overall deadline, and only `SIGKILL`s whatever is still alive when it
/// expires — so the whole shutdown costs at most `grace`, no matter how many
/// browsers there are. Every child is always reaped.
///
/// The teardown order is preserved: all children are dead and reaped before
/// any pane is closed. Browsers already torn down are skipped (and their pane
/// close / profile disposal re-run idempotently).
pub fn shutdown_all<'a, I>(browsers: I, grace: Duration, disposition: ProfileDisposition)
where
    I: IntoIterator<Item = &'a mut Browser>,
{
    let mut all: Vec<&mut Browser> = browsers.into_iter().collect();

    // Phase 1: mark, then kill every still-running child against one deadline.
    let mut children: Vec<&mut Child> = Vec::new();
    for browser in &mut all {
        if browser.torn_down {
            continue;
        }
        browser.torn_down = true;
        browser.disposition = disposition;
        children.push(&mut browser.child);
    }
    terminate_children(&mut children, grace);
    drop(children);

    // Phase 2: only now the compositors go away, and the profiles are disposed.
    for browser in &mut all {
        browser.pane.close();
        if browser.disposition == ProfileDisposition::Remove && !browser.profile_handled {
            browser.profile_handled = true;
            remove_profile_in_background(&browser.profile);
        }
    }
}

/// Put `command` into its own process group, so its whole process tree can be
/// signalled as a unit with `killpg(2)`.
///
/// Used for every browser spawn, and available to callers that want the same
/// guarantee. A no-op off unix, where there are no process groups.
pub fn isolate_process_group(command: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // `0` means "a new group whose id is the child's pid", i.e. the child
        // becomes the group leader, so `pgid == child.id()`.
        command.process_group(0);
    }
    command
}

/// Send `signal` to `pid`'s whole process group, falling back to the single
/// process if there is no such group.
///
/// `pid` must still be an unreaped child of ours: it is a group leader (see
/// [`isolate_process_group`]), so its pgid equals its pid. A child that was
/// *not* spawned into its own group has no group of that id, `killpg` fails
/// with `ESRCH`, and the fallback `kill` reaches it directly. Every error —
/// `ESRCH` above all — is deliberately ignored: nothing to signal is success.
#[cfg(unix)]
fn signal_group(pid: libc::pid_t, signal: libc::c_int) {
    if pid <= 0 {
        return;
    }
    // SAFETY: `killpg`/`kill` on the group of our own not-yet-reaped child.
    // The pid cannot have been recycled because we have not waited on it.
    // Both return `-1`/`ESRCH` rather than doing anything when the target is
    // already gone, which is why the return value is discarded.
    let sent = unsafe { libc::killpg(pid, signal) };
    if sent != 0 {
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

/// `SIGKILL` whatever is left of a reaped child's process group.
///
/// Chromium's helper processes survive their parent, so a *gracefully* exited
/// browser can still leave a zygote and a GPU process behind. They are not our
/// children, so they cannot be reaped here — killing them hands them to init,
/// which does. Only `killpg` is used: the leader's pid is already reaped, so
/// signalling it directly could in principle reach a recycled pid, whereas a
/// process *group* with that id only exists while a member of the original
/// group is still alive.
#[cfg(unix)]
fn kill_group_remnants(pid: libc::pid_t) {
    if pid <= 0 {
        return;
    }
    // SAFETY: see above — a `killpg` whose only failure mode (`ESRCH`, the
    // group is empty) is exactly the expected case and is ignored.
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
}

/// `SIGTERM` every child's process group, wait out a single shared `grace`,
/// then `SIGKILL` the stragglers. Always reaps every child, and never leaves a
/// grandchild behind. Never panics.
///
/// On non-unix there is no graceful signal, so this is a plain kill-and-reap.
pub fn terminate_children(children: &mut [&mut Child], grace: Duration) {
    // `true` once the child has been reaped and needs no further attention.
    let mut done: Vec<bool> = Vec::with_capacity(children.len());
    // Pids are captured before anything is reaped, so the process groups can
    // still be swept afterwards.
    #[cfg(unix)]
    let pids: Vec<libc::pid_t> = children.iter().map(|c| c.id() as libc::pid_t).collect();
    for child in children.iter_mut() {
        done.push(matches!(child.try_wait(), Ok(Some(_))));
    }

    #[cfg(unix)]
    {
        // Children still worth polling. Distinct from `done`: a child whose
        // `try_wait` *errors* stops being polled at once — it can never be
        // observed to exit, so keeping it in the set would make every other
        // child pay the full shared grace for nothing. It is handled by the
        // trailing kill-and-reap instead.
        let mut polling: Vec<bool> = done.iter().map(|done| !*done).collect();

        for (pid, done) in pids.iter().zip(done.iter()) {
            if *done {
                continue;
            }
            signal_group(*pid, libc::SIGTERM);
        }

        let deadline = Instant::now() + grace;
        loop {
            let mut waiting = false;
            for ((child, done), polling) in children
                .iter_mut()
                .zip(done.iter_mut())
                .zip(polling.iter_mut())
            {
                if !*polling {
                    continue;
                }
                match child.try_wait() {
                    Ok(Some(_)) => {
                        *done = true;
                        *polling = false;
                    }
                    // Waiting failed; stop polling and let the kill-and-wait
                    // below deal with it.
                    Err(_) => *polling = false,
                    Ok(None) => waiting = true,
                }
            }
            if !waiting || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(TERM_POLL);
        }
    }

    // Whatever survived the shared grace period (or every child, off unix).
    for (index, (child, done)) in children.iter_mut().zip(done.iter()).enumerate() {
        if !*done {
            #[cfg(unix)]
            signal_group(pids[index], libc::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
        }
        // Even a child that exited on its own can have left helpers running.
        #[cfg(unix)]
        kill_group_remnants(pids[index]);
        #[cfg(not(unix))]
        let _ = index;
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // A browser dropped without an explicit teardown belongs to a group
        // that is gone for good, so its profile is garbage.
        let disposition = self.disposition;
        self.teardown(disposition);
    }
}

/// Ask `child`'s process group to exit with `SIGTERM`, wait up to `grace` for
/// the child itself, then escalate to `SIGKILL` on the whole group. Always
/// reaps the direct child, so no zombie is left behind, and never leaves a
/// grandchild running. Never panics.
///
/// On non-unix there is no graceful signal, so this is a plain kill-and-reap.
pub fn terminate_child(child: &mut Child, grace: Duration) {
    #[cfg(unix)]
    let pid = child.id() as libc::pid_t;

    // Already dead (and reaped by `try_wait`)? Only its leftovers matter.
    if matches!(child.try_wait(), Ok(Some(_))) {
        #[cfg(unix)]
        kill_group_remnants(pid);
        return;
    }

    #[cfg(unix)]
    {
        signal_group(pid, libc::SIGTERM);

        let deadline = Instant::now() + grace;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    // Reaped, but its helpers may still be running.
                    kill_group_remnants(pid);
                    return;
                }
                Ok(None) => {}
                // Waiting failed; fall through to kill and one blocking wait.
                Err(_) => break,
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(TERM_POLL);
        }

        // Still alive after the grace period: no more patience, and the whole
        // tree goes, not just the process we can wait on.
        signal_group(pid, libc::SIGKILL);
    }

    let _ = child.kill();
    let _ = child.wait();
    #[cfg(unix)]
    kill_group_remnants(pid);
}

/// Get `profile` out of the way now and delete it on a worker thread.
///
/// The rename is a cheap directory operation, so the main thread never walks
/// the profile's thousands of files. It is also the *only* thing that makes
/// background deletion safe: once renamed, the worker owns a path nobody else
/// can ever ask for again.
///
/// If the profile cannot be moved aside, nothing is deleted here. Deleting it
/// under its original name would race a browser that re-occupies exactly that
/// path (the caller has already released the group's browser slot, so an
/// immediate re-spawn is legal). Leaving it costs one stale directory until
/// the next [`sweep_profiles`], which removes it because no live group uuid
/// claims it — strictly better than deleting a live profile underneath a
/// running Chromium.
pub fn remove_profile_in_background(profile: &Path) {
    let Some(target) = move_profile_aside(profile) else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("kabelsalat-profile-rm".to_string())
        .spawn(move || {
            if let Err(err) = remove_profile(&target) {
                eprintln!(
                    "kabelsalat: could not remove browser profile {}: {err}",
                    target.display()
                );
            }
        });
    if spawned.is_err() {
        // Out of threads: the startup sweep will get it next time.
        eprintln!("kabelsalat: could not start the browser profile removal thread");
    }
}

/// Rename `profile` to a sibling trash name so it can be deleted safely in the
/// background. `None` means the profile is still sitting at its original path
/// and MUST NOT be deleted by anyone but the startup sweep.
///
/// A profile that does not exist at all is nothing to move and nothing to
/// delete, so that is `None` too.
pub fn move_profile_aside(profile: &Path) -> Option<PathBuf> {
    let trash = trash_path(profile)?;
    match std::fs::rename(profile, &trash) {
        Ok(()) => Some(trash),
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "kabelsalat: could not move browser profile {} aside ({err}); \
                     leaving it for the next startup sweep",
                    profile.display()
                );
            }
            None
        }
    }
}

/// Sibling path a profile is renamed to before background deletion. `None` for
/// a path with no file name. Pure — the counter only keeps names unique.
pub fn trash_path(profile: &Path) -> Option<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = profile.file_name()?.to_string_lossy().into_owned();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let parent = profile.parent().unwrap_or(Path::new("."));
    Some(parent.join(format!(
        "{TRASH_PREFIX}{name}-{}-{unique}",
        std::process::id()
    )))
}

/// Remove a profile directory. A missing directory is success.
pub fn remove_profile(profile: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(profile) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// `<state_dir>/browsers/<group_uuid>` — pure path derivation.
///
/// Keyed by the group's uuid, never by its id: ids are reused.
pub fn profile_dir(state_dir: &Path, group_uuid: &str) -> PathBuf {
    state_dir.join(PROFILES_SUBDIR).join(group_uuid)
}

/// `<state_dir>/browsers` — pure path derivation.
pub fn profiles_root(state_dir: &Path) -> PathBuf {
    state_dir.join(PROFILES_SUBDIR)
}

/// Give a freshly created profile the state Chromium would otherwise ask the user
/// for on its first run.
///
/// `--no-first-run` suppresses the welcome flow for the run it is passed to, but
/// Chromium still treats a user-data-dir without the `First Run` sentinel as new and
/// keeps offering the setup prompts. Writing the sentinel plus a small `Preferences`
/// file settles it once, at the only moment it is safe to: a directory this process
/// just created, before Chromium has ever opened it.
///
/// The seeded values are deliberately few — the defaults a browser embedded in a
/// terminal wants, not a general opinion about how the user should browse.
fn seed_profile(profile: &Path) -> std::io::Result<()> {
    std::fs::write(profile.join(FIRST_RUN_SENTINEL), b"")?;
    let default = profile.join(DEFAULT_PROFILE_SUBDIR);
    std::fs::create_dir_all(&default)?;
    std::fs::write(default.join(PREFERENCES_FILE), DEFAULT_PREFERENCES)
}

/// Resolve the Chromium binary from [`BROWSER_CANDIDATES`] against `PATH`.
pub fn resolve_binary() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    resolve_binary_in(BROWSER_CANDIDATES, &path)
}

/// Pure form of [`resolve_binary`]: first candidate present as an executable
/// file in one of the `path_var` entries wins. Candidate order beats PATH order.
pub fn resolve_binary_in(candidates: &[&str], path_var: &OsStr) -> Option<PathBuf> {
    for candidate in candidates {
        for dir in std::env::split_paths(path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let full = dir.join(candidate);
            if is_executable_file(&full) {
                return Some(full);
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        match std::fs::metadata(path) {
            Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Should a directory named `name` under `browsers/` be deleted, given the
/// live group uuids? Pure; anything that is not a live uuid is garbage —
/// including [`TRASH_PREFIX`] leftovers and old id-keyed profile directories.
pub fn is_stale_profile(name: &str, live: &HashSet<String>) -> bool {
    !live.contains(name)
}

/// Delete every profile directory under `<state_dir>/browsers` that does not
/// belong to a live group. Pure filesystem IO — safe to call from a worker
/// thread, touches no GTK.
///
/// A missing `browsers/` directory is success. Individual removal failures are
/// collected and reported, but never abort the sweep.
///
/// One exception: failing to remove a [`TRASH_PREFIX`] directory is *benign*
/// and never reported. A background remover may be walking exactly that tree
/// right now, which makes a mid-walk `ENOENT`/`ENOTEMPTY` expected rather than
/// a problem — and the loser of that race would otherwise raise a spurious
/// "could not remove browser profile" error for work that is being done anyway.
/// The directory is still attempted, so trash left by a crashed run is cleaned.
pub fn sweep_profiles(state_dir: &Path, live: &HashSet<String>) -> std::io::Result<()> {
    let root = profiles_root(state_dir);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let mut failures = 0usize;
    for entry in entries {
        let Ok(entry) = entry else {
            failures += 1;
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue; // not a name we ever wrote; leave it alone
        };
        if !is_stale_profile(name, live) {
            continue;
        }
        if remove_profile(&entry.path()).is_err() && !name.starts_with(TRASH_PREFIX) {
            failures += 1;
        }
    }
    if failures > 0 {
        return Err(std::io::Error::other(format!(
            "{failures} stale browser profile(s) could not be removed"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kabelsalat-browser-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn touch_exec(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    const UUID_A: &str = "11111111-1111-4111-8111-111111111111";
    const UUID_B: &str = "22222222-2222-4222-8222-222222222222";

    fn live_set(uuids: &[&str]) -> HashSet<String> {
        uuids.iter().map(|u| (*u).to_string()).collect()
    }

    #[test]
    fn profile_path_is_state_dir_browsers_group_uuid() {
        let path = profile_dir(Path::new("/var/state/kabelsalat"), UUID_A);
        assert_eq!(
            path,
            PathBuf::from(format!("/var/state/kabelsalat/browsers/{UUID_A}"))
        );
    }

    #[test]
    fn profile_paths_differ_per_group() {
        let base = Path::new("/s");
        assert_ne!(profile_dir(base, UUID_A), profile_dir(base, UUID_B));
        assert_eq!(profiles_root(base), PathBuf::from("/s/browsers"));
    }

    #[test]
    fn profile_paths_are_not_keyed_by_a_reusable_group_id() {
        // Two groups that reuse id 3 must never share a profile directory.
        let base = Path::new("/s");
        assert_ne!(profile_dir(base, UUID_A), profile_dir(base, UUID_B));
        assert!(!profile_dir(base, UUID_A).ends_with("3"));
    }

    #[test]
    fn resolve_binary_finds_nothing_in_empty_path() {
        assert_eq!(
            resolve_binary_in(BROWSER_CANDIDATES, &OsString::from("")),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_binary_prefers_candidate_order_over_path_order() {
        let dir = tmp_dir("resolve");
        let first = dir.join("a");
        let second = dir.join("b");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        // "google-chrome" comes earlier on PATH, but "chromium" wins by order.
        touch_exec(&first.join("google-chrome"));
        touch_exec(&second.join("chromium"));
        let path = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(
            resolve_binary_in(BROWSER_CANDIDATES, &path),
            Some(second.join("chromium"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_binary_ignores_non_executable_files() {
        let dir = tmp_dir("nonexec");
        std::fs::write(dir.join("chromium"), b"not executable").unwrap();
        let path = std::env::join_paths([&dir]).unwrap();
        assert_eq!(resolve_binary_in(BROWSER_CANDIDATES, &path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_binary_falls_through_to_last_candidate() {
        let dir = tmp_dir("fallthrough");
        touch_exec(&dir.join("google-chrome"));
        let path = std::env::join_paths([&dir]).unwrap();
        assert_eq!(
            resolve_binary_in(BROWSER_CANDIDATES, &path),
            Some(dir.join("google-chrome"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_url_accepts_only_browsable_urls() {
        use DefaultUrlError::*;
        let cases: &[(&str, Result<Option<String>, DefaultUrlError>)] = &[
            // Cleared: an empty (or blank) setting is a valid "no default URL".
            ("", Ok(None)),
            ("   ", Ok(None)),
            ("\t\n ", Ok(None)),
            // The case this whole helper exists for: a flag, not a URL.
            ("--disable-web-security", Err(LooksLikeFlag)),
            ("-", Err(LooksLikeFlag)),
            ("  --headless", Err(LooksLikeFlag)),
            // Whitespace inside would become a second Chromium argument.
            ("hello world", Err(NotAUrl)),
            ("http://example.org /etc/passwd", Err(NotAUrl)),
            // Supported schemes, scheme lowercased, the rest kept as typed.
            (
                "http://example.org/a?b=1#c",
                Ok(Some("http://example.org/a?b=1#c".into())),
            ),
            (
                "HTTPS://Example.ORG/Path",
                Ok(Some("https://Example.ORG/Path".into())),
            ),
            ("FiLe:///tmp/x.html", Ok(Some("file:///tmp/x.html".into()))),
            // A bare host gains the implied scheme, address-bar style.
            ("localhost:3000", Ok(Some("http://localhost:3000".into()))),
            (
                "example.org/path",
                Ok(Some("http://example.org/path".into())),
            ),
            // Anything else is not a page this pane opens.
            ("ftp://host/file", Err(UnsupportedScheme("ftp".into()))),
            (
                "JavaScript://alert(1)",
                Err(UnsupportedScheme("javascript".into())),
            ),
            // Both sides of the per-scheme rule: http needs a host, file needs
            // a path (and has no host at all, by construction).
            ("http://", Err(NotAUrl)),
            ("https:///just/a/path", Err(NotAUrl)),
            ("file://", Err(NotAUrl)),
            ("file:///", Ok(Some("file:///".into()))),
            (
                "file:///home/c/notes.html",
                Ok(Some("file:///home/c/notes.html".into())),
            ),
            // Surrounding whitespace is trimmed, not rejected.
            (
                "  https://example.org  ",
                Ok(Some("https://example.org".into())),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(&normalize_default_url(input), expected, "input: {input:?}");
        }
    }

    #[test]
    fn default_url_errors_all_say_something() {
        for err in [
            DefaultUrlError::LooksLikeFlag,
            DefaultUrlError::UnsupportedScheme("ftp".into()),
            DefaultUrlError::NotAUrl,
        ] {
            assert!(!err.to_string().is_empty(), "{err:?}");
        }
    }

    #[test]
    fn stale_profile_detection() {
        let live = live_set(&[UUID_A, UUID_B]);
        assert!(!is_stale_profile(UUID_A, &live));
        assert!(!is_stale_profile(UUID_B, &live));
        assert!(is_stale_profile(
            "33333333-3333-4333-8333-333333333333",
            &live
        ));
        assert!(is_stale_profile("not-a-uuid", &live));
        assert!(is_stale_profile("", &live));
        // Legacy id-keyed profile directories are garbage now.
        assert!(is_stale_profile("1", &live));
    }

    #[test]
    fn sweep_removes_only_dead_groups() {
        let dir = tmp_dir("sweep");
        let dead = "33333333-3333-4333-8333-333333333333";
        for name in [UUID_A, UUID_B, dead, "junk", "2"] {
            std::fs::create_dir_all(profiles_root(&dir).join(name)).unwrap();
            std::fs::write(profiles_root(&dir).join(name).join("f"), b"x").unwrap();
        }
        let live = live_set(&[UUID_A, UUID_B]);
        sweep_profiles(&dir, &live).unwrap();
        assert!(profile_dir(&dir, UUID_A).exists());
        assert!(profile_dir(&dir, UUID_B).exists());
        assert!(!profile_dir(&dir, dead).exists());
        assert!(!profiles_root(&dir).join("junk").exists());
        assert!(!profiles_root(&dir).join("2").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_without_browsers_dir_is_ok() {
        let dir = tmp_dir("sweep-missing");
        assert!(sweep_profiles(&dir, &HashSet::new()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trash_path_is_a_sibling_that_never_looks_like_a_group() {
        let profile = profile_dir(Path::new("/s"), UUID_A);
        let trash = trash_path(&profile).unwrap();
        assert_eq!(
            trash.parent(),
            Some(profiles_root(Path::new("/s")).as_path())
        );
        let name = trash.file_name().unwrap().to_str().unwrap().to_string();
        assert!(name.starts_with(TRASH_PREFIX), "{name}");
        assert!(is_stale_profile(&name, &live_set(&[UUID_A])));
    }

    #[test]
    fn trash_paths_are_unique() {
        let profile = profile_dir(Path::new("/s"), UUID_A);
        assert_ne!(trash_path(&profile), trash_path(&profile));
        assert_eq!(trash_path(Path::new("/")), None);
    }

    #[test]
    fn background_removal_frees_the_path_immediately() {
        let dir = tmp_dir("bg-remove");
        let profile = profile_dir(&dir, UUID_A);
        std::fs::create_dir_all(profile.join("Default")).unwrap();
        std::fs::write(profile.join("Default").join("Cookies"), b"x").unwrap();
        remove_profile_in_background(&profile);
        // The rename happens synchronously, so the group's path is free at once.
        assert!(!profile.exists());
        // And the trash is emptied shortly after, by the worker.
        for _ in 0..200 {
            let empty = std::fs::read_dir(profiles_root(&dir))
                .map(|mut e| e.next().is_none())
                .unwrap_or(false);
            if empty {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            std::fs::read_dir(profiles_root(&dir))
                .unwrap()
                .next()
                .is_none(),
            "background worker did not empty the trash"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn terminate_child_ends_a_well_behaved_child_gracefully() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let start = Instant::now();
        terminate_child(&mut child, TERM_GRACE);
        // SIGTERM was enough, so we did not sit out the whole grace period.
        assert!(start.elapsed() < TERM_GRACE, "{:?}", start.elapsed());
        // Reaped: a second wait cannot block on a live process.
        assert!(child.try_wait().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn terminate_child_escalates_when_sigterm_is_ignored() {
        let mut child = Command::new("sh")
            .arg("-c")
            // A self-contained busy loop: a `sleep` would be `exec`ed by some
            // shells (losing the trap), and waiting on a child makes others
            // exit on SIGTERM despite it. It is SIGKILLed a moment later.
            // "ready" is printed once the trap is installed, so the signal
            // cannot race the shell's startup.
            .arg("trap '' TERM; echo ready; while :; do :; done")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        {
            use std::io::Read as _;
            let mut out = child.stdout.take().unwrap();
            let mut buf = [0u8; 6];
            out.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"ready\n");
        }
        let grace = Duration::from_millis(120);
        let start = Instant::now();
        terminate_child(&mut child, grace);
        let elapsed = start.elapsed();
        assert!(elapsed >= grace, "escalated too early: {elapsed:?}");
        assert!(elapsed < grace + Duration::from_secs(5), "{elapsed:?}");
        assert!(child.try_wait().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn terminate_child_on_an_already_dead_child_is_a_no_op() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let _ = child.wait();
        let start = Instant::now();
        terminate_child(&mut child, TERM_GRACE);
        terminate_child(&mut child, TERM_GRACE); // idempotent
        assert!(start.elapsed() < TERM_GRACE);
    }

    #[test]
    fn background_removal_of_a_missing_profile_does_nothing() {
        let dir = tmp_dir("bg-missing");
        std::fs::create_dir_all(profiles_root(&dir)).unwrap();
        let profile = profile_dir(&dir, UUID_A);
        assert_eq!(move_profile_aside(&profile), None);
        remove_profile_in_background(&profile);
        assert!(
            std::fs::read_dir(profiles_root(&dir))
                .unwrap()
                .next()
                .is_none(),
            "a missing profile must not create trash"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_profile_that_cannot_be_moved_aside_is_never_deleted() {
        use std::os::unix::fs::PermissionsExt as _;
        // SAFETY: `geteuid` is a pure read of the calling process's identity.
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores the directory permissions this relies on
        }
        let dir = tmp_dir("bg-rename-fails");
        let root = profiles_root(&dir);
        let profile = profile_dir(&dir, UUID_A);
        std::fs::create_dir_all(profile.join("Default")).unwrap();
        std::fs::write(profile.join("Default").join("Cookies"), b"x").unwrap();
        // Read-only parent: the rename out of the way cannot succeed.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500)).unwrap();

        assert_eq!(move_profile_aside(&profile), None);
        remove_profile_in_background(&profile);
        // Give any (wrongly) spawned worker a chance to do damage.
        std::thread::sleep(Duration::from_millis(50));

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            profile.join("Default").join("Cookies").exists(),
            "a profile that could not be moved aside was deleted under its live name"
        );
        // It is still garbage, so the startup sweep must claim it.
        assert!(is_stale_profile(UUID_A, &live_set(&[UUID_B])));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn terminate_children_shares_one_deadline_across_all_children() {
        let mut children: Vec<Child> = (0..4)
            .map(|_| {
                let mut child = Command::new("sh")
                    .arg("-c")
                    .arg("trap '' TERM; echo ready; while :; do :; done")
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                {
                    use std::io::Read as _;
                    let mut out = child.stdout.take().unwrap();
                    let mut buf = [0u8; 6];
                    out.read_exact(&mut buf).unwrap();
                    assert_eq!(&buf, b"ready\n");
                }
                child
            })
            .collect();

        let grace = Duration::from_millis(150);
        let start = Instant::now();
        {
            let mut refs: Vec<&mut Child> = children.iter_mut().collect();
            terminate_children(&mut refs, grace);
        }
        let elapsed = start.elapsed();
        assert!(elapsed >= grace, "escalated too early: {elapsed:?}");
        // The whole point: one grace, not one per child.
        assert!(
            elapsed < grace * 2,
            "grace was paid per child: {elapsed:?} for 4 children"
        );
        for child in &mut children {
            assert!(child.try_wait().is_ok(), "child was not reaped");
        }
    }

    #[cfg(unix)]
    #[test]
    fn terminate_children_reaps_a_mix_of_live_and_dead_children() {
        let mut dead = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut live = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let start = Instant::now();
        {
            let mut refs: Vec<&mut Child> = vec![&mut dead, &mut live];
            terminate_children(&mut refs, TERM_GRACE);
        }
        // Both were well behaved, so the shared grace was not sat out.
        assert!(start.elapsed() < TERM_GRACE, "{:?}", start.elapsed());
        assert!(matches!(dead.try_wait(), Ok(Some(_))));
        assert!(matches!(live.try_wait(), Ok(Some(_))));
    }

    #[cfg(unix)]
    #[test]
    fn terminate_children_on_an_empty_slice_is_a_no_op() {
        let start = Instant::now();
        terminate_children(&mut [], TERM_GRACE);
        assert!(start.elapsed() < TERM_GRACE);
    }

    /// Spawn `sh -c script` into its own process group, exactly the way
    /// [`Browser::spawn`] launches Chromium, and read the single line the
    /// script prints (the grandchild's pid).
    #[cfg(unix)]
    fn spawn_group_leader_printing_pid(script: &str) -> (Child, libc::pid_t) {
        use std::io::Read as _;
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        isolate_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        let mut out = child.stdout.take().unwrap();
        let mut line = String::new();
        let mut byte = [0u8; 1];
        loop {
            let read = out.read(&mut byte).unwrap();
            if read == 0 || byte[0] == b'\n' {
                break;
            }
            line.push(byte[0] as char);
        }
        let pid: libc::pid_t = line.trim().parse().unwrap();
        (child, pid)
    }

    /// Is `pid` still a signallable process?
    #[cfg(unix)]
    fn alive(pid: libc::pid_t) -> bool {
        // SAFETY: `kill` with signal 0 only probes; it changes nothing.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(unix)]
    fn wait_until_gone(pid: libc::pid_t) -> bool {
        for _ in 0..400 {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        !alive(pid)
    }

    #[cfg(unix)]
    #[test]
    fn a_child_is_spawned_into_its_own_process_group() {
        let (mut child, _) = spawn_group_leader_printing_pid("echo $$; sleep 30");
        let pid = child.id() as libc::pid_t;
        // SAFETY: a pure read of our own live child's process group id.
        let pgid = unsafe { libc::getpgid(pid) };
        assert_eq!(pgid, pid, "child is not its own process group leader");
        // SAFETY: same read for ourselves.
        let ours = unsafe { libc::getpgid(0) };
        assert_ne!(pgid, ours, "child shares our process group");
        terminate_child(&mut child, TERM_GRACE);
    }

    #[cfg(unix)]
    #[test]
    fn terminating_a_group_leader_takes_its_grandchildren_with_it() {
        // A shell that forks a long sleeper (the stand-in for Chromium's
        // zygote/GPU/renderer processes) and then exits *gracefully* on
        // SIGTERM — the case where signalling the direct pid alone leaks.
        let (mut child, grandchild) =
            spawn_group_leader_printing_pid("sleep 300 & echo $!; trap 'exit 0' TERM; wait");
        assert!(alive(grandchild), "the sleeper never started");

        terminate_child(&mut child, TERM_GRACE);

        assert!(matches!(child.try_wait(), Err(_) | Ok(Some(_))));
        assert!(
            wait_until_gone(grandchild),
            "grandchild {grandchild} survived termination"
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminate_children_takes_grandchildren_with_it_too() {
        let mut spawned: Vec<(Child, libc::pid_t)> = (0..3)
            .map(|_| {
                spawn_group_leader_printing_pid("sleep 300 & echo $!; trap 'exit 0' TERM; wait")
            })
            .collect();
        let grandchildren: Vec<libc::pid_t> = spawned.iter().map(|(_, pid)| *pid).collect();
        for pid in &grandchildren {
            assert!(alive(*pid));
        }
        {
            let mut refs: Vec<&mut Child> = spawned.iter_mut().map(|(c, _)| c).collect();
            terminate_children(&mut refs, TERM_GRACE);
        }
        for pid in &grandchildren {
            assert!(wait_until_gone(*pid), "grandchild {pid} survived shutdown");
        }
    }

    /// A child reaped behind `Child`'s back: every later `try_wait` fails with
    /// `ECHILD`, which is the only way this error is reachable in practice.
    #[cfg(unix)]
    fn externally_reaped_child() -> Child {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        let mut status: libc::c_int = 0;
        // SAFETY: a blocking `waitpid` on our own child, deliberately reaping
        // it out from under `Child` to make `try_wait` fail afterwards.
        unsafe {
            libc::waitpid(pid, &mut status, 0);
        }
        assert!(child.try_wait().is_err(), "try_wait did not start failing");
        child
    }

    #[cfg(unix)]
    #[test]
    fn a_child_whose_try_wait_errors_does_not_consume_the_shared_grace() {
        let mut broken = externally_reaped_child();
        let grace = Duration::from_millis(400);
        let start = Instant::now();
        {
            let mut refs: Vec<&mut Child> = vec![&mut broken];
            terminate_children(&mut refs, grace);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < grace,
            "an unwaitable child sat out the whole grace: {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn one_unwaitable_child_does_not_delay_its_well_behaved_siblings() {
        let mut broken = externally_reaped_child();
        let mut good = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let grace = Duration::from_millis(400);
        let start = Instant::now();
        {
            let mut refs: Vec<&mut Child> = vec![&mut broken, &mut good];
            terminate_children(&mut refs, grace);
        }
        let elapsed = start.elapsed();
        assert!(elapsed < grace, "grace was sat out: {elapsed:?}");
        assert!(matches!(good.try_wait(), Ok(Some(_))), "sibling not reaped");
    }

    #[cfg(unix)]
    #[test]
    fn sweep_does_not_report_trash_it_lost_a_race_for() {
        use std::os::unix::fs::PermissionsExt as _;
        // SAFETY: `geteuid` is a pure read of the calling process's identity.
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores the directory permissions this relies on
        }
        let dir = tmp_dir("sweep-trash");
        let root = profiles_root(&dir);
        let trash = root.join(format!("{TRASH_PREFIX}{UUID_A}-1-0"));
        std::fs::create_dir_all(trash.join("Default")).unwrap();
        std::fs::write(trash.join("Default").join("Cookies"), b"x").unwrap();
        // Undeletable, like a tree the background remover is halfway through.
        std::fs::set_permissions(&trash, std::fs::Permissions::from_mode(0o500)).unwrap();

        assert!(
            sweep_profiles(&dir, &live_set(&[UUID_B])).is_ok(),
            "a trash directory the sweep could not remove was reported as an error"
        );

        // A *real* stale profile that cannot be removed is still an error.
        let stale = root.join(UUID_A);
        std::fs::create_dir_all(stale.join("Default")).unwrap();
        std::fs::write(stale.join("Default").join("Cookies"), b"x").unwrap();
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert!(sweep_profiles(&dir, &live_set(&[UUID_B])).is_err());

        std::fs::set_permissions(&trash, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_profile_is_idempotent() {
        let dir = tmp_dir("remove");
        let profile = profile_dir(&dir, UUID_B);
        std::fs::create_dir_all(&profile).unwrap();
        remove_profile(&profile).unwrap();
        remove_profile(&profile).unwrap(); // missing dir is success
        assert!(!profile.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
