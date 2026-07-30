# Per-group CDP Endpoint Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A group's embedded Chromium exposes its DevTools (CDP) endpoint, and every tmux session in the group carries it — plus the group's identity — as live environment, per the approved spec `docs/superpowers/specs/2026-07-30-cdp-endpoint-design.md`.

**Architecture:** Chromium is launched with `--remote-debugging-port=0` and writes `DevToolsActivePort` into its per-group profile; a glib timer polls that file and delivers `Msg::CdpReady`; the handler publishes `PLAYWRIGHT_MCP_CDP_ENDPOINT` + `KABELSALAT_CDP` into the group's tmux sessions via new `TmuxCtl::set_environment`/`unset_environment` methods. `KABELSALAT_GROUP` (the group uuid, a fencing token for agents) is written at every session spawn and rewritten on tab moves. The browser overflow menu shows and copies the endpoint.

**Tech Stack:** Rust, relm4/GTK4, tmux (private server), Chromium `--remote-debugging-port`.

## Global Constraints

- **No new crates.** `Cargo.toml` is not modified.
- **`src/tmuxctl.rs` never panics**: every fallible path returns `Result<_, TmuxError>`; no `unwrap`/`expect` on tmux interaction.
- **`src/state.rs` is not modified.** No new persisted field; the endpoint is runtime state only.
- **The app keeps working without tmux** (missing or < 3.2): every new tmux call sits behind `if let Some(tmux) = &self.tmux` (or receives `self.tmux.as_ref()`); when tmux is absent, no injection happens and everything else still works.
- **A `set-environment` failure for one session is logged with `eprintln!` and does not stop the loop** over the remaining sessions, and never prevents the browser from working.
- Environment names and values, verbatim from the spec: `PLAYWRIGHT_MCP_CDP_ENDPOINT` and `KABELSALAT_CDP`, both `http://127.0.0.1:<port>`; `KABELSALAT_GROUP`, the group **uuid** (`Group::uuid`, never `Group::id` — ids are reused).
- Discovery polls `<profile>/DevToolsActivePort` every **100 ms**, gives up after **10 s**.
- Before claiming any task done: `cargo fmt`, `cargo clippy --all-targets` (no new warnings), `cargo test` (all pass).
- Commit messages are plain imperative sentences, matching `git log` style (no `feat:` prefixes).

---

### Task 1: DevToolsActivePort parser (`CdpEndpoint`)

**Files:**
- Modify: `src/browser.rs` (pure-function section near `profile_dir`, `src/browser.rs:736-746`)
- Test: `src/browser.rs` (existing `#[cfg(test)] mod tests`, starts `src/browser.rs:856`)

**Interfaces:**
- Consumes: nothing new.
- Produces (used by Tasks 2, 4):
  - `pub struct CdpEndpoint { pub port: u16, pub browser_ws_path: String }`
  - `impl CdpEndpoint { pub fn url(&self) -> String }` → `"http://127.0.0.1:<port>"`
  - `pub fn parse_devtools_active_port(contents: &str) -> Option<CdpEndpoint>`
  - `pub fn devtools_port_path(profile: &Path) -> PathBuf` → `<profile>/DevToolsActivePort`

- [ ] **Step 1: Write the failing tests**

Append inside the existing `mod tests` in `src/browser.rs` (after `profile_paths_are_not_keyed_by_a_reusable_group_id`, `src/browser.rs:901-907`):

```rust
    #[test]
    fn devtools_port_file_parses_without_trailing_newline() {
        // Chromium writes no trailing newline after the second line.
        let parsed =
            parse_devtools_active_port("40455\n/devtools/browser/66dad126-b59e").unwrap();
        assert_eq!(parsed.port, 40455);
        assert_eq!(parsed.browser_ws_path, "/devtools/browser/66dad126-b59e");
        assert_eq!(parsed.url(), "http://127.0.0.1:40455");
    }

    #[test]
    fn devtools_port_file_parses_with_trailing_newline() {
        let parsed = parse_devtools_active_port("40455\n/devtools/browser/abc\n").unwrap();
        assert_eq!(parsed.port, 40455);
        assert_eq!(parsed.browser_ws_path, "/devtools/browser/abc");
    }

    #[test]
    fn devtools_port_file_rejects_bad_input() {
        // Empty, port-only, non-numeric, port 0, above u16, leading blank line:
        // all mean "no endpoint", never a panic or a garbage endpoint.
        assert!(parse_devtools_active_port("").is_none());
        assert!(parse_devtools_active_port("40455").is_none());
        assert!(parse_devtools_active_port("40455\n").is_none());
        assert!(parse_devtools_active_port("no-port\n/devtools/browser/abc").is_none());
        assert!(parse_devtools_active_port("0\n/devtools/browser/abc").is_none());
        assert!(parse_devtools_active_port("70000\n/devtools/browser/abc").is_none());
        assert!(parse_devtools_active_port("\n40455\n/devtools/browser/abc").is_none());
    }

    #[test]
    fn devtools_port_path_is_inside_the_profile() {
        let path = devtools_port_path(Path::new("/s/browsers/uuid-a"));
        assert_eq!(path, PathBuf::from("/s/browsers/uuid-a/DevToolsActivePort"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib browser::tests::devtools`
Expected: compile error — `parse_devtools_active_port`, `devtools_port_path`, `CdpEndpoint` not found.

- [ ] **Step 3: Write the implementation**

In `src/browser.rs`, directly after `profiles_root` (`src/browser.rs:744-746`), add:

```rust
/// The browser's CDP endpoint, read from `DevToolsActivePort` in its profile.
///
/// Runtime state only — a port is meaningless across a browser restart, so
/// this is never persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpEndpoint {
    pub port: u16,
    pub browser_ws_path: String,
}

impl CdpEndpoint {
    /// The HTTP form consumers need: Playwright resolves the websocket URL by
    /// fetching `/json/version` from here.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// Parse the two-line `DevToolsActivePort` file Chromium writes with
/// `--remote-debugging-port=0`: the chosen port, then the browser websocket
/// path. The second line carries no trailing newline.
pub fn parse_devtools_active_port(contents: &str) -> Option<CdpEndpoint> {
    let mut lines = contents.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    if port == 0 {
        return None;
    }
    let browser_ws_path = lines.next()?.trim();
    if browser_ws_path.is_empty() {
        return None;
    }
    Some(CdpEndpoint {
        port,
        browser_ws_path: browser_ws_path.to_string(),
    })
}

/// `<profile>/DevToolsActivePort` — pure path derivation.
pub fn devtools_port_path(profile: &Path) -> PathBuf {
    profile.join("DevToolsActivePort")
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib browser::tests::devtools`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add src/browser.rs
git commit -m "Parse Chromium's DevToolsActivePort file"
```

---

### Task 2: Launch Chromium with CDP enabled; hold the endpoint on `Browser`

**Files:**
- Modify: `src/browser.rs` — `Browser` struct (`src/browser.rs:229-241`), `Browser::spawn` argv build (`src/browser.rs:298-316`), pre-spawn point (`src/browser.rs:333`, the `let child = match command.spawn()` line)

**Interfaces:**
- Consumes: `CdpEndpoint`, `devtools_port_path` (Task 1).
- Produces (used by Tasks 4-6):
  - field `cdp: Option<CdpEndpoint>` on `Browser` (private)
  - `pub fn set_cdp(&mut self, endpoint: CdpEndpoint)`
  - `pub fn cdp_url(&self) -> Option<String>`

- [ ] **Step 1: Add the field and accessors**

In the `Browser` struct (`src/browser.rs:229-241`), after the `profile: PathBuf,` field, add:

```rust
    /// CDP endpoint, once discovery has read `DevToolsActivePort`. Runtime
    /// state only; `None` until discovery completes, and forever if it fails.
    cdp: Option<CdpEndpoint>,
```

In `impl Browser`, next to the other small methods (anywhere after `spawn`), add:

```rust
    /// Record the discovered CDP endpoint.
    pub fn set_cdp(&mut self, endpoint: CdpEndpoint) {
        self.cdp = Some(endpoint);
    }

    /// The endpoint's HTTP URL, if discovery has completed.
    pub fn cdp_url(&self) -> Option<String> {
        self.cdp.as_ref().map(CdpEndpoint::url)
    }
```

- [ ] **Step 2: Add the flag and the pre-spawn delete**

In the argv build (`src/browser.rs:298-316`), after `.arg("--hide-crash-restore-bubble")`, add:

```rust
            // Port 0: Chromium binds a free port and writes it (plus the
            // browser websocket path) to <profile>/DevToolsActivePort.
            .arg("--remote-debugging-port=0")
```

Immediately before `let child = match command.spawn() {` (`src/browser.rs:333`), add:

```rust
        // The file outlives the process that wrote it, so a stale copy cannot
        // be told apart from a fresh one by inspection. Deleting it here means
        // only the process spawned below can recreate it, which turns "is this
        // file current?" into a structural guarantee.
        let _ = std::fs::remove_file(devtools_port_path(&profile));
```

In the `Ok(Self { ... })` at `src/browser.rs:348-355`, add `cdp: None,` after `profile,`.

- [ ] **Step 3: Build and run all browser tests**

Run: `cargo build && cargo test --lib browser`
Expected: builds clean; all browser tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/browser.rs
git commit -m "Launch the group browser with a CDP port and track its endpoint"
```

---

### Task 3: `TmuxCtl::set_environment` / `unset_environment`

**Files:**
- Modify: `src/tmuxctl.rs` — new methods next to `kill_session`/`respawn_pane` (`src/tmuxctl.rs:445-453`)
- Test: `src/tmuxctl.rs` existing `mod tests` (argv-shape tests live near `spawn_argv_shape`, `src/tmuxctl.rs:635-670`)

**Interfaces:**
- Consumes: existing private `run` helper (`src/tmuxctl.rs:455-467`), `SESSION_PREFIX` (`src/tmuxctl.rs:16`).
- Produces (used by Tasks 4-6):
  - `pub fn set_environment(&self, uuid: &str, key: &str, value: &str) -> Result<(), TmuxError>`
  - `pub fn unset_environment(&self, uuid: &str, key: &str) -> Result<(), TmuxError>`
  - (test-only surface) associated fns `set_environment_args`, `unset_environment_args`

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/tmuxctl.rs`, after `spawn_argv_with_cwd_inserts_c_flag` (`src/tmuxctl.rs:650-670`):

```rust
    #[test]
    fn set_environment_argv_shape() {
        let args =
            TmuxCtl::set_environment_args("1234-abcd", "KABELSALAT_CDP", "http://127.0.0.1:4567");
        assert_eq!(
            args,
            [
                "set-environment",
                "-t",
                "ks-1234-abcd",
                "KABELSALAT_CDP",
                "http://127.0.0.1:4567"
            ]
        );
    }

    #[test]
    fn unset_environment_argv_shape() {
        let args = TmuxCtl::unset_environment_args("1234-abcd", "KABELSALAT_CDP");
        assert_eq!(
            args,
            ["set-environment", "-u", "-t", "ks-1234-abcd", "KABELSALAT_CDP"]
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib tmuxctl::tests::set_environment_argv_shape`
Expected: compile error — no `set_environment_args` on `TmuxCtl`.

- [ ] **Step 3: Write the implementation**

In `impl TmuxCtl`, directly after `respawn_pane` (`src/tmuxctl.rs:450-453`), add:

```rust
    /// Argv tail for publishing one variable into a tab's session environment.
    /// Pure, so the shape is testable like `spawn_argv`.
    fn set_environment_args(uuid: &str, key: &str, value: &str) -> [String; 5] {
        [
            "set-environment".into(),
            "-t".into(),
            format!("{SESSION_PREFIX}{uuid}"),
            key.into(),
            value.into(),
        ]
    }

    /// Argv tail for removing one variable from a tab's session environment.
    fn unset_environment_args(uuid: &str, key: &str) -> [String; 5] {
        [
            "set-environment".into(),
            "-u".into(),
            "-t".into(),
            format!("{SESSION_PREFIX}{uuid}"),
            key.into(),
        ]
    }

    /// Publish `key=value` into a tab session's environment table. Inherited
    /// by processes created in the session afterwards; a process already
    /// running sees it only by querying `show-environment`.
    pub fn set_environment(&self, uuid: &str, key: &str, value: &str) -> Result<(), TmuxError> {
        let args = Self::set_environment_args(uuid, key, value);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&refs)
    }

    /// Remove `key` from a tab session's environment table.
    pub fn unset_environment(&self, uuid: &str, key: &str) -> Result<(), TmuxError> {
        let args = Self::unset_environment_args(uuid, key);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&refs)
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib tmuxctl`
Expected: all tmuxctl tests pass, including the two new ones.

- [ ] **Step 5: Commit**

```bash
git add src/tmuxctl.rs
git commit -m "Add tmux session environment set/unset to TmuxCtl"
```

---

### Task 4: Discovery poll and `Msg::CdpReady`

**Files:**
- Modify: `src/app.rs` — constants near `BROWSER_POLL_SECS` (`src/app.rs:63`), `Msg` enum (`src/app.rs:197-291`), `update()` (`src/app.rs:635-827`), helpers near `start_browser_poll` (`src/app.rs:1305-1321`), `toggle_browser` (`src/app.rs:1226-1264`), `restore_browser` (`src/app.rs:1396-1428`)

**Interfaces:**
- Consumes: `browser::{parse_devtools_active_port, devtools_port_path, profile_dir, CdpEndpoint}` (Tasks 1-2), `Browser::set_cdp` (Task 2), `TmuxCtl::{set_environment, unset_environment}` (Task 3).
- Produces (used by Tasks 5-6):
  - `Msg::CdpReady(usize, Option<browser::CdpEndpoint>)`
  - `const ENV_CDP`, `const ENV_CDP_PLAYWRIGHT`, `const ENV_GROUP`, `const CDP_ENV_KEYS`
  - `fn cdp_env_set(&self, group_id: usize, url: &str)`
  - `fn cdp_env_unset(&self, group_id: usize)`
  - `fn start_cdp_poll(&self, group_id: usize)`

- [ ] **Step 1: Add constants**

After `const BROWSER_POLL_SECS: u32 = 2;` (`src/app.rs:63`), add:

```rust
/// CDP discovery: poll the profile's DevToolsActivePort file this often…
const CDP_POLL_MS: u64 = 100;
/// …and give up after this many attempts (10 s total).
const CDP_POLL_TRIES: u32 = 100;

/// Environment keys published into each tab's tmux session (see
/// docs/superpowers/specs/2026-07-30-cdp-endpoint-design.md).
const ENV_CDP: &str = "KABELSALAT_CDP";
const ENV_CDP_PLAYWRIGHT: &str = "PLAYWRIGHT_MCP_CDP_ENDPOINT";
const ENV_GROUP: &str = "KABELSALAT_GROUP";
/// The endpoint pair: set on discovery, unset on browser close. `ENV_GROUP`
/// is deliberately not in here — it describes the tab, not the browser, and
/// is never unset.
const CDP_ENV_KEYS: [&str; 2] = [ENV_CDP, ENV_CDP_PLAYWRIGHT];
```

- [ ] **Step 2: Add the `Msg` variant**

In the `Msg` enum, after `Msg::RestoreBrowser(usize)` (`src/app.rs:264`), add:

```rust
    /// CDP discovery finished for this group's browser: the endpoint, or
    /// `None` when `DevToolsActivePort` never appeared or never parsed.
    CdpReady(usize, Option<browser::CdpEndpoint>),
```

(`browser` is already imported for `Browser`; if the module path is not in scope as `browser::`, use the same path the `Browser` import at the top of `src/app.rs` uses.)

- [ ] **Step 3: Add the env helpers and the poll**

Next to `start_browser_poll` (`src/app.rs:1305-1321`), add:

```rust
    /// Publish the endpoint pair to every session in the group. One session
    /// failing must not stop the others, and a tmux failure must never keep
    /// the browser from working: log and continue.
    fn cdp_env_set(&self, group_id: usize, url: &str) {
        let Some(tmux) = &self.tmux else { return };
        for tab in self.tabs.iter().filter(|t| t.group == group_id) {
            for key in CDP_ENV_KEYS {
                if let Err(err) = tmux.set_environment(&tab.uuid, key, url) {
                    eprintln!("kabelsalat: set {key} on {}: {err}", tab.uuid);
                }
            }
        }
    }

    /// Remove the endpoint pair from every session in the group, so a shell
    /// started later does not inherit a dead endpoint.
    fn cdp_env_unset(&self, group_id: usize) {
        let Some(tmux) = &self.tmux else { return };
        for tab in self.tabs.iter().filter(|t| t.group == group_id) {
            for key in CDP_ENV_KEYS {
                if let Err(err) = tmux.unset_environment(&tab.uuid, key) {
                    eprintln!("kabelsalat: unset {key} on {}: {err}", tab.uuid);
                }
            }
        }
    }

    /// Watch for `DevToolsActivePort` in the group's profile. The file shows
    /// up roughly half a second after spawn, so this cannot block the main
    /// thread; stat-ing a file is cheap enough for a 100 ms local timeout
    /// (same reasoning as the BROWSER_POLL_SECS timer, not the linger thread).
    fn start_cdp_poll(&self, group_id: usize) {
        let Some(group) = self.groups.iter().find(|g| g.id == group_id) else {
            return;
        };
        if group.browser.is_none() {
            return;
        }
        let profile = browser::profile_dir(&self.state_dir, &group.uuid);
        let port_file = browser::devtools_port_path(&profile);
        let input = self.input.clone();
        let mut tries = 0u32;
        gtk::glib::timeout_add_local(std::time::Duration::from_millis(CDP_POLL_MS), move || {
            tries += 1;
            if let Ok(contents) = std::fs::read_to_string(&port_file)
                && let Some(endpoint) = browser::parse_devtools_active_port(&contents)
            {
                let _ = input.send(Msg::CdpReady(group_id, Some(endpoint)));
                return gtk::glib::ControlFlow::Break;
            }
            // Profile gone = browser closed mid-poll; stop quietly. The
            // CdpReady(None) on timeout is still delivered so the failure is
            // logged in exactly one place, the handler.
            if !profile.is_dir() {
                return gtk::glib::ControlFlow::Break;
            }
            if tries >= CDP_POLL_TRIES {
                let _ = input.send(Msg::CdpReady(group_id, None));
                return gtk::glib::ControlFlow::Break;
            }
            gtk::glib::ControlFlow::Continue
        });
    }
```

- [ ] **Step 4: Handle `Msg::CdpReady` in `update()`**

In the `update()` match (`src/app.rs:635-827`), after the `Msg::RestoreBrowser(group) => self.restore_browser(group),` arm, add:

```rust
            // Runtime state only: nothing here is persisted, so return early
            // to skip the save_state() at the bottom (like PollBrowsers).
            Msg::CdpReady(group_id, endpoint) => {
                let Some(group) = self.groups.iter_mut().find(|g| g.id == group_id) else {
                    return;
                };
                let Some(browser) = group.browser.as_mut() else {
                    return;
                };
                match endpoint {
                    Some(endpoint) => {
                        let url = endpoint.url();
                        browser.set_cdp(endpoint);
                        self.cdp_env_set(group_id, &url);
                    }
                    None => {
                        eprintln!(
                            "kabelsalat: no CDP endpoint for group {group_id} within 10 s"
                        );
                    }
                }
                return;
            }
```

- [ ] **Step 5: Start the poll after both spawn paths**

In `toggle_browser` (`src/app.rs:1226-1264`), in the `Ok(browser)` arm, after `self.start_browser_poll();`, add:

```rust
                    self.start_cdp_poll(id);
```

In `restore_browser` (`src/app.rs:1396-1428`), in the `Ok(browser)` arm, after `self.start_browser_poll();`, add:

```rust
                    self.start_cdp_poll(id);
```

- [ ] **Step 6: Build, clippy, test**

Run: `cargo build && cargo clippy --all-targets && cargo test`
Expected: clean build, no new clippy warnings, all tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/app.rs
git commit -m "Discover the browser's CDP endpoint and publish it to the group"
```

---

### Task 5: Environment lifecycle — session spawn, browser close, tab move

**Files:**
- Modify: `src/app.rs` — `add_tab` (`src/app.rs:1657-1724`), `close_browser` (find with `fn close_browser`; called from the `Msg::CloseBrowser` and `Msg::BrowserDied` arms at `src/app.rs:719-727`), `move_active_tab` (`src/app.rs:1522-1541`), `drop_tab` (`src/app.rs:1858-1883`), `drop_tab_on_group` (`src/app.rs:1896-1921`)

**Interfaces:**
- Consumes: `ENV_GROUP`, `CDP_ENV_KEYS`, `cdp_env_unset` (Task 4), `TmuxCtl::{set_environment, unset_environment}` (Task 3), `Browser::cdp_url` (Task 2).
- Produces: `fn session_env_refresh(&self, tab_uuid: &str, group_id: usize)` (also used by Task 5's three move sites; nothing later consumes it).

- [ ] **Step 1: Add the per-session refresh helper**

Next to `cdp_env_set`/`cdp_env_unset` (added in Task 4), add:

```rust
    /// Make one session's environment describe its group: `KABELSALAT_GROUP`
    /// always (a tab always belongs to some group — never unset), the
    /// endpoint pair set or unset by whether the group's browser has a live
    /// endpoint. Used when a session is (re)created and when a tab moves.
    fn session_env_refresh(&self, tab_uuid: &str, group_id: usize) {
        let Some(tmux) = &self.tmux else { return };
        let Some(group) = self.groups.iter().find(|g| g.id == group_id) else {
            return;
        };
        if let Err(err) = tmux.set_environment(tab_uuid, ENV_GROUP, &group.uuid) {
            eprintln!("kabelsalat: set {ENV_GROUP} on {tab_uuid}: {err}");
        }
        let url = group.browser.as_ref().and_then(|b| b.cdp_url());
        for key in CDP_ENV_KEYS {
            let result = match &url {
                Some(url) => tmux.set_environment(tab_uuid, key, url),
                // Also the startup path: a reattached session may carry a
                // stale endpoint written by a previous run (or version), and
                // this unset is the only point where it can be cleared.
                None => tmux.unset_environment(tab_uuid, key),
            };
            if let Err(err) = result {
                eprintln!("kabelsalat: refresh {key} on {tab_uuid}: {err}");
            }
        }
    }
```

- [ ] **Step 2: Call it wherever a session is (re)created**

In `add_tab` (`src/app.rs:1657-1724`), directly after the `spawn_backing(&terminal, &uuid, self.tmux.as_ref(), cwd, command);` line, add:

```rust
        // Every path that creates or reattaches a session funnels through
        // here: fresh tabs, restored tabs, adopted orphans, CLI tabs. The
        // refresh both stamps the group identity and clears any stale
        // endpoint a reattached session inherited from a previous run.
        self.session_env_refresh(&uuid, group);
```

(`uuid` is still owned at this point — it moves into the `Tab` struct only at the `self.tabs.push` further down — and `group` is the `usize` parameter.)

- [ ] **Step 3: Unset the endpoint pair when the browser goes away**

In `close_browser` (find `fn close_browser(`; it takes the group id and a `ProfileDisposition`), add as its first line — for **both** dispositions, because a kept profile still loses its browser process and with it the endpoint:

```rust
        self.cdp_env_unset(group_id);
```

(Use the function's actual parameter name for the group id; keep the call before any teardown so it runs even if teardown logs errors. If the parameter is named `id`, the call is `self.cdp_env_unset(id);`.)

- [ ] **Step 4: Rewrite the environment on all three tab-move paths**

In `move_active_tab` (`src/app.rs:1522-1541`), after `tab.group = target;`, capture the uuid and refresh. The borrow of `self.tabs` must end first, so restructure the tail of the function:

```rust
        tab.group = target;
        let moved_uuid = tab.uuid.clone();
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == target) {
            group.last_active = active;
        }
        // A moved tab's session must describe its new home; the old group's
        // endpoint may still point at a live browser — just not one visible
        // from here (see the spec's "Tab moved between groups").
        self.session_env_refresh(&moved_uuid, target);
        self.prune_empty_groups();
        self.rebuild_list();
```

In `drop_tab` (`src/app.rs:1858-1883`), the tab may or may not change groups. After `tab.group = self.tabs[di].group;` the original code continues; restructure so the old group is known and the refresh only fires on an actual move:

```rust
        let old_group = tab.group;
        tab.group = self.tabs[di].group;
        let moved_group = tab.group;
        let moved_uuid = tab.uuid.clone();
        self.tabs.insert(di, tab);
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == moved_group)
            && self.active == Some(src)
        {
            group.last_active = src;
        }
        if moved_group != old_group {
            self.session_env_refresh(&moved_uuid, moved_group);
        }
        self.prune_empty_groups();
        self.rebuild_list();
```

In `drop_tab_on_group` (`src/app.rs:1896-1921`), same pattern — after `let mut tab = self.tabs.remove(si);`:

```rust
        let old_group = tab.group;
        tab.group = group;
        let moved_uuid = tab.uuid.clone();
```

and before `self.prune_empty_groups();`:

```rust
        if group != old_group {
            self.session_env_refresh(&moved_uuid, group);
        }
```

- [ ] **Step 5: Build, clippy, test**

Run: `cargo build && cargo clippy --all-targets && cargo test`
Expected: clean; all tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/app.rs
git commit -m "Keep session environment truthful across spawn, close and move"
```

---

### Task 6: Overflow menu shows and copies the endpoint

**Files:**
- Modify: `src/app.rs` — view! popover block (`src/app.rs:360-378`), `App` struct fields (`src/app.rs:132-195`, near `browser_menu`), `Msg` enum (`src/app.rs:197-291`), `update()` match, `sync_browser_pane` (find with `fn sync_browser_pane`), the `Msg::CdpReady` handler (Task 4)

**Interfaces:**
- Consumes: `Browser::cdp_url` (Task 2), `active_group` (`src/app.rs:858-861`).
- Produces: `Msg::CopyCdpEndpoint`, `fn refresh_cdp_menu(&self)`, `App` fields `cdp_label: gtk::Label`, `cdp_copy: gtk::Button`.

- [ ] **Step 1: Create the widgets alongside `browser_menu`**

`browser_menu` is a `gtk::Popover` created in `init` before the view! macro and stored on `App` (field at `src/app.rs:152`). Find where it is constructed (search `browser_menu` in the `init` fn) and create two more widgets next to it, then store them in the model the same way `browser_menu` is stored:

```rust
        let cdp_label = gtk::Label::builder()
            .label("CDP: unavailable")
            .css_classes(["monospace", "dim-label"])
            .build();
        let cdp_copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text("Copy CDP endpoint URL")
            .css_classes(["flat"])
            .sensitive(false)
            .build();
```

Add the `App` fields after `browser_menu: gtk::Popover,` (`src/app.rs:152`):

```rust
    /// Endpoint row in the browser overflow menu: dimmed "CDP: unavailable"
    /// until discovery succeeds, then `127.0.0.1:<port>` plus a live copy
    /// button.
    cdp_label: gtk::Label,
    cdp_copy: gtk::Button,
```

- [ ] **Step 2: Restructure the popover child in view!**

Replace the popover's single-button child (`src/app.rs:366-377`) with a vertical box holding the endpoint row and the existing close button, keeping the close button's behavior identical:

```rust
                    #[wrap(Some)]
                    set_popover = &browser_menu.clone() {
                        #[wrap(Some)]
                        set_child = &gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_spacing: 6,

                            gtk::Box {
                                set_orientation: gtk::Orientation::Horizontal,
                                set_spacing: 6,
                                append: &cdp_label.clone(),
                                append: &cdp_copy.clone(),
                            },

                            gtk::Button {
                                set_label: "Close browser",
                                add_css_class: "flat",
                                connect_clicked[sender, browser_menu] => move |_| {
                                    browser_menu.popdown();
                                    sender.input(Msg::CloseBrowser);
                                },
                            },
                        },
                    },
```

Wire the copy button (next to where other `connect_clicked` handlers are set up in `init`, or inline via builder pattern if the view! macro placement fights the `append:` form — match whichever compiles cleanly with the file's existing idioms):

```rust
        cdp_copy.connect_clicked({
            let sender = sender.clone();
            let browser_menu = browser_menu.clone();
            move |_| {
                browser_menu.popdown();
                sender.input(Msg::CopyCdpEndpoint);
            }
        });
```

- [ ] **Step 3: Add `Msg::CopyCdpEndpoint` and its handler**

In the `Msg` enum, after the `CdpReady` variant (Task 4):

```rust
    /// Copy the active group's CDP endpoint URL to the clipboard.
    CopyCdpEndpoint,
```

In `update()`, after the `Msg::CdpReady` arm:

```rust
            Msg::CopyCdpEndpoint => {
                let url = self
                    .active_group()
                    .and_then(|id| self.groups.iter().find(|g| g.id == id))
                    .and_then(|g| g.browser.as_ref())
                    .and_then(|b| b.cdp_url());
                if let (Some(url), Some(display)) = (url, gtk::gdk::Display::default()) {
                    display.clipboard().set_text(&url);
                }
                return;
            }
```

- [ ] **Step 4: Add `refresh_cdp_menu` and call it from both change points**

Next to `active_group_has_browser` (`src/app.rs:1109-1113`), add:

```rust
    /// Reflect the active group's CDP state in the overflow menu. The label
    /// shows host:port; the copy button carries the full http:// URL.
    fn refresh_cdp_menu(&self) {
        let url = self
            .active_group()
            .and_then(|id| self.groups.iter().find(|g| g.id == id))
            .and_then(|g| g.browser.as_ref())
            .and_then(|b| b.cdp_url());
        match url {
            Some(url) => {
                self.cdp_label
                    .set_label(url.trim_start_matches("http://"));
                self.cdp_label.remove_css_class("dim-label");
                self.cdp_copy.set_sensitive(true);
            }
            None => {
                self.cdp_label.set_label("CDP: unavailable");
                self.cdp_label.add_css_class("dim-label");
                self.cdp_copy.set_sensitive(false);
            }
        }
    }
```

Call it in two places:
1. At the end of the `Msg::CdpReady` arm (Task 4), replace the bare `return;` at the arm's end with `self.refresh_cdp_menu(); return;` — discovery finishing must update an already-open menu.
2. At the end of `sync_browser_pane` (find `fn sync_browser_pane`) — this runs on every activation/browser change, which is what keeps the menu correct when the user switches to a tab in another group.

- [ ] **Step 5: Build, clippy, test**

Run: `cargo build && cargo clippy --all-targets && cargo test`
Expected: clean; all tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/app.rs
git commit -m "Show the CDP endpoint in the browser overflow menu"
```

---

### Task 7: Documentation — README security statement and agent skill

**Files:**
- Modify: `README.md` (browser/feature section — find the section describing the embedded browser)
- Modify: `skills/kabelsalat/SKILL.md` (the agent skill; add a browser/CDP section)

**Interfaces:** none — prose only, but the spec makes both mandatory ("It must be stated plainly in the README and in the agent skill, not buried").

- [ ] **Step 1: README security statement**

In the README's embedded-browser section, add:

```markdown
### Browser automation (CDP)

Each group's browser exposes an **unauthenticated** Chrome DevTools Protocol
endpoint on loopback. CDP grants full control of that browser profile —
cookies, sessions, arbitrary navigation and script execution — and any process
running as any user on this machine can connect to it. This is a deliberate,
documented interim state; a token-authenticated broker is planned.

Terminals in the group receive the endpoint as `KABELSALAT_CDP` and
`PLAYWRIGHT_MCP_CDP_ENDPOINT` (both `http://127.0.0.1:<port>`), plus
`KABELSALAT_GROUP` (the group's uuid). Because a running shell's environment
is frozen at spawn, the live values are always available from tmux:

    tmux show-environment KABELSALAT_CDP
```

- [ ] **Step 2: Agent skill — co-browsing workflow**

In `skills/kabelsalat/SKILL.md`, add a section (adapt heading level to the file's structure):

```markdown
## Driving the group's browser (CDP)

Each group's embedded browser exposes a CDP endpoint. The workflow is
co-browsing: the user steers the visible browser; you attach to inspect, and
act only when asked. An attached client has full power over the profile
(cookies, logins, script execution) — treat it as the user's browser, because
it is.

Fetch the endpoint and group identity once per task (a running shell's
inherited environment may be stale; the tmux table is live):

    tmux show-environment KABELSALAT_CDP     # KABELSALAT_CDP=http://127.0.0.1:<port>
    tmux show-environment KABELSALAT_GROUP   # KABELSALAT_GROUP=<group-uuid>

A leading `-` in the output means the variable is unset (no browser running).

Author scripts against stock Playwright, and start every script with the same
preamble — re-resolve the environment and assert the group you pinned on
first fetch. Tabs can be moved between groups; the assert is what turns a
silently-wrong browser into a loud, recoverable failure:

    import subprocess
    from playwright.sync_api import sync_playwright

    PINNED_GROUP = "<uuid from your first fetch>"

    def tmux_env():
        out = subprocess.run(["tmux", "show-environment"],
                             capture_output=True, text=True).stdout
        return dict(line.split("=", 1) for line in out.splitlines()
                    if "=" in line and not line.startswith("-"))

    env = tmux_env()
    assert env.get("KABELSALAT_GROUP") == PINNED_GROUP, \
        f"tab moved (now in {env.get('KABELSALAT_GROUP', 'nowhere')}) — re-orient"
    endpoint = env["KABELSALAT_CDP"]      # KeyError = no browser: also loud

    with sync_playwright() as p:
        browser = p.chromium.connect_over_cdp(endpoint)
        pages = [pg for ctx in browser.contexts for pg in ctx.pages]
        page = next((pg for pg in pages
                     if pg.evaluate("document.visibilityState") == "visible"),
                    pages[0] if pages else None)
        # page is the tab the user is looking at: read, evaluate, screenshot.

A failed `connect_over_cdp` (connection refused) means the browser restarted:
re-run the fetch and retry. Prefer one script that does many steps over many
single-step invocations.
```

- [ ] **Step 3: Commit**

```bash
git add README.md skills/kabelsalat/SKILL.md
git commit -m "Document the CDP endpoint's security posture and agent workflow"
```

---

### Task 8: Full verification

**Files:** none new.

- [ ] **Step 1: Full gate**

Run: `cargo fmt && cargo clippy --all-targets && cargo test`
Expected: fmt makes no changes (or only changes that are then committed), clippy reports no warnings, all tests pass.

- [ ] **Step 2: Manual smoke test (needs the GUI; ask the user to drive if headless)**

1. `cargo run`, press Alt-2 in a group → browser appears.
2. In a terminal of that group: `tmux show-environment KABELSALAT_CDP` → `KABELSALAT_CDP=http://127.0.0.1:<port>` within ~1 s of the browser appearing; `KABELSALAT_GROUP` shows the group uuid.
3. `curl -s http://127.0.0.1:<port>/json/version` → JSON with `webSocketDebuggerUrl`.
4. Overflow menu (⋮ next to the browser) → shows `127.0.0.1:<port>`; copy button puts the `http://` URL on the clipboard.
5. Open a *new* tab in the group → its shell has both variables in `env` directly.
6. Move a tab to another group (drag or Move-to picker) → `tmux show-environment` in it now shows the new group's uuid, and the endpoint pair matches that group's browser (or `-KABELSALAT_CDP` if it has none).
7. Close the browser → `tmux show-environment KABELSALAT_CDP` shows `-KABELSALAT_CDP`.
8. Restart the app with a browser open → variables reappear after restore; no stale port survives (compare before/after).

- [ ] **Step 3: Commit any leftovers and stop**

```bash
git status   # expect: clean (or only intended changes)
```

Report results honestly, including any smoke-test step that could not be run.
