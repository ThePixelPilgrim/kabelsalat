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
- The endpoint is visible and copyable from the browser overflow menu, for shells
  that are already running.
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

### Shells that are already running

`tmux set-environment` populates the environment given to *newly created*
processes in a session. A process already running cannot have its environment
changed — Linux offers no such mechanism. An agent already running in a tab
therefore will not see the variable; a newly started one will.

This is the reason the overflow menu shows the endpoint. It is the supported path
for an already-running shell, not a convenience.

## Menu

The browser overflow popover (`src/app.rs:348-366`) currently sets a single
`gtk::Button` as its child, and `Popover::set_child` accepts one widget. It
becomes a vertical `gtk::Box` containing:

- a monospace label showing `127.0.0.1:<port>`, with a copy button that puts the
  full `http://127.0.0.1:<port>` URL on the clipboard;
- the existing "Close browser" button, unchanged.

Before discovery finishes, or after it fails, the first row reads `CDP:
unavailable` and is dimmed. Closing the browser continues to work in every state.

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

- **Port reuse across a crash.** Deleting `DevToolsActivePort` before spawn
  removes the stale-read hazard, but if Chromium dies without the poll noticing,
  the stored endpoint stays in the group's environment until the browser is
  closed. Sessions started in that window get a dead endpoint.
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
