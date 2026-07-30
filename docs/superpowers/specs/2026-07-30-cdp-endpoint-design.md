# Per-group CDP endpoint

Publish the group browser's Chrome DevTools Protocol endpoint so an agent running
in one of the group's terminals can drive the browser it can see, and show that
endpoint in the browser overflow menu.

This is the first of two phases, named A and C after the options weighed when it
was chosen. Phase A uses a loopback TCP port, which any local user can reach.
Phase C replaces the transport with a kabelsalat-owned, token-authenticated
broker; see "Out of scope".

## Goals

- A group's browser exposes a CDP endpoint on loopback.
- Every terminal in that group gets the endpoint in its environment, so an agent
  started there attaches without being configured.
- A shell that is already running can fetch the live endpoint from tmux without
  restarting anything.
- The endpoint is visible and copyable from the browser overflow menu, as the
  human-facing fallback.
- The app keeps working unchanged when the endpoint never materialises.

## Background

Today nothing is exposed. `Browser::spawn` (`src/browser.rs:144-231`) launches
Chromium with six flags — `--ozone-platform=wayland`, `--user-data-dir`,
`--restore-last-session`, `--no-first-run`, `--no-default-browser-check`,
`--hide-crash-restore-bubble` — none CDP-related, and the only environment
variable set anywhere is `WAYLAND_DISPLAY`, on Chromium's own process
(`src/browser.rs:198`). No tmux session receives anything.

Three facts, measured against Chromium 150.0.7871.181, shape this design.

**Discovery is a file, and it is late.** `--remote-debugging-port=0` makes
Chromium bind a free port and write `<user-data-dir>/DevToolsActivePort`. In a
local run that file appeared 525 ms after spawn. It holds two lines — the port,
then the browser websocket path — and the second line has no trailing newline:

```
40455\n/devtools/browser/66dad126-b59e-4258-9550-7ea9e13dd248
```

**That file outlives the process.** After the browser was killed the file
remained on disk, unchanged. Reading it without precautions yields a dead port,
or one the kernel has since reassigned to an unrelated process.

**Playwright needs HTTP.** `@playwright/mcp` (0.0.78) accepts `--cdp-endpoint`
and reads the environment variable `PLAYWRIGHT_MCP_CDP_ENDPOINT`. It resolves
that URL by fetching `/json/version` for a `webSocketDebuggerUrl`, so the
endpoint must be `http://127.0.0.1:<port>`. `connectOverCDP` cannot use a unix
socket, and `--remote-debugging-pipe` provides no HTTP server at all — it exposes
only the raw NUL-delimited message stream on inherited descriptors, usable solely
by the parent process. Serving Playwright from a pipe therefore requires
kabelsalat to implement the DevTools HTTP and WebSocket front-end itself, which
is phase C's work, not phase A's.

## Architecture

### `src/browser.rs`

Add `--remote-debugging-port=0` to the argv built at `src/browser.rs:183-201`.
The port needs no allocation logic: `--user-data-dir` is already per-group
(`profile_dir()` = `<state_dir>/browsers/<group-uuid>`), so two groups cannot
collide.

Immediately before `command.spawn()` (`src/browser.rs:208`), delete
`<profile>/DevToolsActivePort`, ignoring a missing file. Only the new process can
recreate it, which turns "is this file current?" — unanswerable by inspecting the
file — into a structural guarantee.

Add a pure parser and its type:

```rust
pub struct CdpEndpoint {
    pub port: u16,
    pub browser_ws_path: String,
}

fn parse_devtools_active_port(contents: &str) -> Option<CdpEndpoint>
```

It returns `None` for empty input, a missing or unparsable port, a port of 0, or
a missing second line. It must not require a trailing newline.

`Browser` (`src/browser.rs:123-135`) gains `cdp: Option<CdpEndpoint>`, set once
discovery completes. It is runtime state and is never persisted.

### `src/app.rs`

Discovery cannot block: the file arrives roughly half a second after spawn, and
`Browser::spawn` is called synchronously on the GTK main thread from
`toggle_browser` (`src/app.rs:1224`) and `restore_browser` (`src/app.rs:1382`).

After a successful spawn, start a `glib::timeout_add_local` polling the profile's
`DevToolsActivePort` every 100 ms, giving up after 10 s, and delivering
`Msg::CdpReady(group_id, Option<CdpEndpoint>)`. The poll also stops early if the
group or its browser is gone.

This differs from the `std::thread::spawn` pattern used by `enable_linger`
(`src/app.rs:1467-1473`) on purpose. Stat-ing a file is not blocking work, and
staying on the main thread avoids `Send` bounds on `Browser`, which owns a
`WaylandPane` — a GTK type. The recurring `BROWSER_POLL_SECS` timer at
`src/app.rs:1290-1296` is the closer precedent.

The `Msg::CdpReady` handler stores the endpoint on the group's browser, injects
the environment into the group's sessions, and refreshes the menu.

### `src/tmuxctl.rs`

Add one method, shaped exactly like `kill_session` (`src/tmuxctl.rs:447`) and
routed through the existing `run` helper (`src/tmuxctl.rs:455-467`):

```rust
pub fn set_environment(&self, uuid: &str, key: &str, value: &str) -> Result<(), TmuxError>
pub fn unset_environment(&self, uuid: &str, key: &str) -> Result<(), TmuxError>
```

They invoke `set-environment -t ks-<uuid> KEY VALUE` and `set-environment -u -t
ks-<uuid> KEY`. Like every other path in this file, they return `Result` and never
panic.

### `src/state.rs`

Unchanged. `SavedGroup` persists `browser_open` and `browser_split`
(`src/state.rs:20-44`) and gains nothing. A persisted port would be false after
any restart; `browser_open: true` already drives re-launch, which rediscovers it.

## Behaviour

Two variables are published per session, with identical values:

- `PLAYWRIGHT_MCP_CDP_ENDPOINT` — read directly by `@playwright/mcp`, giving
  zero-configuration attachment.
- `KABELSALAT_CDP` — a neutral name for other tooling and user scripts, so the
  feature is not tied to one client.

Both are `http://127.0.0.1:<port>`.

### Browser start

`Msg::CdpReady` with an endpoint sets both variables on every tmux session in the
group. Sessions are per *tab*, named `ks-<tab-uuid>` (`src/tmuxctl.rs:16,320`), so
the group's sessions are `tabs.iter().filter(|t| t.group == group.id)` mapped to
their uuids — the filter already used throughout `src/app.rs`.

### Tab added to a group

A tab created in a group that already has a live endpoint gets both variables set
on its session immediately after `spawn_backing` (`src/app.rs:2628-2656`).
Injecting only at browser start would silently miss every tab created later.

### Browser closed

Both variables are unset on the group's sessions, so a later shell does not
inherit a stale endpoint.

### Startup

Before any injection, both variables are unset on every session kabelsalat
reattaches to. A session's environment can carry values written by a previous run
— or by a previous *version* — and this is the only point at which they can be
cleared. See "Compatibility".

### Shells that are already running

`tmux set-environment` populates the environment given to *newly created*
processes in a session. A process already running cannot have its environment
changed — Linux offers no such mechanism. An agent already running in a tab
therefore will not see the variable in its inherited environment; a newly
started one will.

The inherited copy is not the only way to read it. The variables live in the
tmux server's per-session environment table, which `show-environment` queries
live — and every process in a pane inherits `$TMUX`, which points at
kabelsalat's private socket and names the pane's session. The supported path for
a running shell is therefore to ask tmux:

```
tmux show-environment KABELSALAT_CDP                 # KABELSALAT_CDP=http://127.0.0.1:<port>
eval "$(tmux show-environment -s KABELSALAT_CDP)"    # same, as a real env var
```

When the variable has been unset, `show-environment` prints `-KABELSALAT_CDP`,
so "browser closed" is distinguishable from "never set". Because the table is
read at query time, a value fetched this way is current even when an inherited
copy is stale — which makes per-use fetching the right default for agents (see
"Consumption model").

The overflow menu shows the endpoint for the same reason, as the human-facing
fallback: visible and copyable with no shell involved.

## Consumption model

The primary consumer is an agent in one of the group's terminals driving the
browser the user is looking at — co-browsing: the user steers, the agent
attaches to inspect, and acts only when asked. Measured against Chromium 150
with Playwright: `connect_over_cdp` on the HTTP endpoint attaches in ~60 ms,
the user-visible tab is discoverable from inside the pages themselves
(`document.visibilityState === "visible"`) with no window-system integration,
and attaching, evaluating, and screenshotting leave scroll position and focus
untouched.

The agent skill documents this as authored scripts against stock Playwright
(Python sync API or Node), not a wrapper library and not an MCP server:

- Scripts compose. Many CDP actions run in one shell invocation with data
  flowing between steps, instead of one agent round-trip per action.
- Each script run resolves the endpoint fresh via `show-environment`, so a
  browser restart between invocations is harmless.
- Stock Playwright is an API agents already know deeply; a kabelsalat-specific
  wrapper would trade that familiarity away. The skill's only bespoke content
  is the two things stock Playwright cannot know: fetch the endpoint from
  tmux, and pick the visible page.

`PLAYWRIGHT_MCP_CDP_ENDPOINT` still gives `@playwright/mcp` zero-configuration
attachment in shells started after the browser. That path is published but
secondary: the MCP server freezes the endpoint string at process start, and
Claude Code respawns a stdio server only on a manual `/mcp` reconnect, so every
browser restart costs a manual step that the script path does not have. Nothing
here blocks MCP use; the skill simply does not lead with it.

## Menu

The browser overflow popover (`src/app.rs:348-366`) currently sets a single
`gtk::Button` as its child, and `Popover::set_child` accepts one widget. It
becomes a vertical `gtk::Box` containing:

- a monospace label showing `127.0.0.1:<port>`, with a copy button that puts the
  full `http://127.0.0.1:<port>` URL on the clipboard;
- the existing "Close browser" button, unchanged.

Before discovery finishes, or after it fails, the first row reads `CDP:
unavailable` and is dimmed. Closing the browser continues to work in every state.

## Compatibility

Two stores outlive a single run, and they need different treatment.

### `state.json`

Unchanged by this phase. No field is added, so no schema change happens and
nothing here needs new handling.

The existing posture, for reference: every field added over time carries a
default — `uuid` (`src/state.rs:29`), `browser_open` (`:38`), `browser_split`
(`:42`, via `default_browser_split`), `linger_warning_dismissed` (`:123`) — each
covered by a test. `active: Option<String>` deserializes to `None` when the key is
absent. No struct declares `deny_unknown_fields`, so an older binary silently
ignores keys it does not recognise. There is no schema version marker. A corrupt
file is renamed to `state.json.corrupt` and the app starts from defaults
(`src/state.rs:227-236`), so a bad parse never orphans running shells.

One property constrains later phases: `save()` (`src/state.rs:184-193`) serializes
the in-memory struct wholesale, with no read-merge of the file on disk. An older
binary that loads a newer file and then saves drops every field it does not know
about. Reading forward-compatibly is not the same as round-tripping
forward-compatibly. Nothing in this phase writes such a field; anything that does
later must accept that a downgrade erases it, or introduce a version marker.

### The tmux server

This is the store this phase actually writes to, and it outlives not just the app
but the app's *version*: sessions survive restart, and with `loginctl
enable-linger`, logout. Kabelsalat does not currently read or clear a tmux
session's environment anywhere — there is no `set-environment`,
`show-environment`, or `update-environment` call in `src/` today.

A published variable can therefore outlive the browser that justified it:

- **Downgrade.** A version without this feature reattaches to sessions that
  already have the variables set, and never clears them. Shells started there
  inherit an endpoint whose port died with the previous browser, or that the
  kernel has since reassigned to an unrelated process.
- **Crash.** The unset on browser close does not run if the app dies with the
  browser still open.

The remedy belongs on the read side, the half a new version controls: **at
startup, unset both variables on every session kabelsalat reattaches to, before
any injection.** That makes one invariant true regardless of which version wrote
the session earlier — a variable present in a session always describes a live
browser.

Measured tmux semantics this relies on: `set-environment` is inherited by panes
created afterwards; `set-environment -u` removes the entry, and panes created
after it see nothing. Neither affects a process that is already running.

## Error handling

- The file never appears within 10 s: no endpoint, no environment injection, menu
  shows unavailable. The pane and terminals are unaffected.
- The file appears but does not parse: treated identically to absent.
- tmux is missing or too old: no injection happens; the menu still shows the
  endpoint, and copy still works.
- A `set-environment` call fails for one session: log it and continue with the
  remaining sessions. A tmux failure must not prevent the browser from working.

## Security

The endpoint is unauthenticated. CDP grants full control of the browser profile —
cookies, sessions, arbitrary navigation and script execution — and Chromium binds
it to loopback with no credential of any kind. Any process running as any user on
this machine can connect.

This is a deliberate, documented interim state, accepted to unlock the workflow
while phase C is built. It must be stated plainly in the README and in the agent
skill, not buried.

## Testing

Pure functions only, matching the existing style (`src/browser.rs:732`,
`src/tmuxctl.rs:590-621`):

- `parse_devtools_active_port`: valid two-line input without a trailing newline;
  trailing newline present; empty; port only, no second line; non-numeric port;
  port `0`; port above `u16`; leading blank line.
- Endpoint URL construction from a port.
- `set_environment` and `unset_environment` argv shape, alongside the existing
  `spawn_argv` tests.

No GTK-level tests, consistent with the rest of the repository.

## Known risks

- **Port reuse within a run.** Deleting `DevToolsActivePort` before spawn removes
  the stale-read hazard, and the startup unset covers stale variables inherited
  across restarts and downgrades. Neither covers Chromium dying *while the app
  keeps running*: the endpoint stays in the group's environment until the browser
  is closed, so sessions started in that window get a dead endpoint. Clearing it
  from the existing `PollBrowsers` timer (`src/app.rs:1290`) would close this, at
  the cost of scope; it is deliberately left out of phase A.
- **Environment drift.** A tab moved between groups, if that is possible, would
  keep the old group's endpoint. Verify against the group-reassignment paths
  during implementation.
- **Silent Playwright attachment.** An agent picks the variable up automatically,
  which is the point, but it also means an agent may drive the user's visible
  browser without being asked to. Documented in the skill.

## Out of scope

- Any change to the pipe/broker question. Phase C introduces
  `--remote-debugging-pipe` plus a kabelsalat-owned DevTools HTTP and WebSocket
  front-end with a token in the URL path, published through the same two
  variables. Nothing in phase A should make that harder — in particular, consumers
  read an opaque URL from an environment variable, so the transport can change
  underneath them.
- Restricting what an attached client may do. The broker in phase C is the first
  point at which policy can be applied.
- Driving embedded Wayland apps other than the browser.
