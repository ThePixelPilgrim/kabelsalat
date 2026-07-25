# Per-group browser pane

Integrate the `klamottenkiste` widget into kabelsalat as a browser pane owned by a
tab group, toggled with `Alt-2`.

## Goals

- Each tab group can have at most one browser. Switching groups switches browsers.
- A group starts with no browser. Nothing spawns one automatically; the user
  presses `Alt-2`.
- `Alt-2` toggles the browser between shown and hidden, and spawns one if the
  group has none.
- A hidden browser is indicated by an icon in the header bar. A group with no
  browser shows no icon.
- Browsers survive an app restart, including their Chromium session.

## Background

`klamottenkiste` (`../klamottenkiste/klamottenkiste`, LGPL-3.0-or-later) exposes
`WaylandPane`, a plain GTK4 `gtk::Widget` subclass — not a relm4 component, not
WebKit. It spawns a nested Smithay compositor in `constructed()` and composites
the hosted client's output into a `gtk::Picture`.

Two facts shape this design:

1. **The widget never spawns a client.** `spawn_headless(width, height)` starts
   only the compositor; the embedder reads `pane.wayland_socket()` and launches
   the client itself with `WAYLAND_DISPLAY` set to that socket
   (`klamottenkiste/docs/widget.md:35-41`). kabelsalat therefore owns the
   Chromium process, and no change to klamottenkiste is required.
2. **Visibility and lifetime are decoupled.** `set_visible(false)` pauses only
   the frame pump and resize poll; the compositor and hosted client are
   untouched, so page and scroll state survive hiding. Teardown happens in
   `dispose()` or via the idempotent `pane.close()`.

kabelsalat links klamottenkiste as an unmodified path dependency. This is
dynamic linking against an LGPL library and does not affect the MIT licence of
the app.

## Architecture

### `src/browser.rs` (new)

Owns the pane, the Chromium process, and the profile directory as one unit:

```rust
pub struct Browser {
    pane: WaylandPane,
    child: std::process::Child,
    profile: PathBuf,
}
```

Responsibilities: construct a pane, launch Chromium into its socket, poll the
child for exit, and tear both down in the correct order (kill and reap the
child, then `pane.close()`). Nothing else in the codebase touches
`WaylandPane` or `Command` for browsers.

### `src/app.rs`

`Group` gains:

```rust
browser: Option<Browser>,
browser_visible: bool,
browser_split: f64,
```

This is the first widget a group has ever owned; groups were previously pure
metadata over a flat `Vec<Tab>`.

New `Msg` variants: `ToggleBrowser`, `CloseBrowser`, `BrowserDied(usize)`.

### `src/state.rs`

`SavedGroup` gains three fields, all `#[serde(default)]` for backward
compatibility, following the `linger_warning_dismissed` precedent:

```rust
pub uuid: String,
pub browser_open: bool,
pub browser_split: f64,
```

`uuid` is the stable per-group identity, exactly as tabs already carry one. A
`state.json` written by an older build has no `uuid`; a group loading without one
gets a fresh uuid generated on load and keeps it from then on.

No URLs or tab state are persisted — Chromium owns that. `state.rs` remains
pure logic: no GTK, no process handling.

## Layout

The existing `gtk::Paned`'s `end_child` becomes a second horizontal
`gtk::Paned`:

```
┌──────────────────────────────────┐
│ HeaderBar              [🌐][⋮]   │
├──────┬───────────────┬───────────┤
│ side │ tab bar       │           │
│ bar  ├───────────────┤  browser  │
│      │               │           │
│ Alt1 │  terminal     │   Alt-2   │
└──────┴───────────────┴───────────┘
```

- start child: the existing content box (tab bar + terminal `gtk::Stack`)
- end child: the active group's `WaylandPane`

Only the active group's pane is parented at any time. On group switch the
outgoing pane is unparented and the incoming one attached. The divider position
is read back into the active group on change and persisted per group.

## Behaviour

### `Alt-2`

| Group state | Action |
|---|---|
| no browser | spawn one, show it |
| browser hidden | show it |
| browser shown | hide it |

`Alt-1` is already toggle-sidebar, so `Alt-2` extends the existing convention.
The shortcut is added to the `SHORTCUTS` table in app.rs, which also feeds the
F1 help dialog.

### Group switch

Hide, keep alive: `set_visible(false)` and unparent. The compositor and Chromium
keep running, so returning to the group is instant with state intact.

### Header bar

Two `pack_end` controls, following the existing tmux-warning / linger-warning
pattern (icon-only button, CSS class, `#[watch] set_visible`,
`connect_clicked => Msg`):

- **Indicator button** — visible when the active group has a browser and it is
  hidden. Clicking sends `ToggleBrowser`, i.e. shows it.
- **Overflow `gtk::MenuButton`** — visible when the active group has a browser
  at all. Contains a **Close browser** entry sending `CloseBrowser`, which kills
  the child, closes the pane, drops the `Browser`, removes the profile
  directory, and clears `browser_open`.

### Group pruned

Groups are pruned when their last tab closes. The group's `Browser` is dropped
and its profile directory removed as part of that.

### Chromium exit

The Chromium process can exit on its own — a crash, or the user quitting from
inside the browser. klamottenkiste cannot detect this: `startup_error()` and
`is_running()` describe the compositor only.

kabelsalat polls `child.try_wait()` on a timer. On exit it sends
`BrowserDied(group)`, which closes the pane, drops the `Browser`, and clears
`browser_open`. The group returns to having no browser and the icon disappears.

An unexpected exit is **not** the same intent as **Close browser**: the profile
directory is kept. The next `Alt-2` relaunches Chromium into the same profile,
so it restores the session that was open when it died. Only a deliberate
**Close browser** from the overflow menu discards the profile.

## Chromium launch

```
chromium --ozone-platform=wayland \
         --user-data-dir=<profile> \
         --restore-last-session
```

with `WAYLAND_DISPLAY` set to `pane.wayland_socket()`.

The binary is resolved at spawn time from the candidates `chromium`,
`chromium-browser`, `google-chrome`, in that order. If none is found, or the
spawn fails, the pane is torn down immediately and the failure is surfaced in a
dialog rather than leaving a blank pane.

Session restore is entirely Chromium's own mechanism, driven by the persistent
profile directory.

### Termination

Chromium only writes its session state on a clean shutdown, so it is never
killed outright. Teardown sends `SIGTERM` first and waits a short, bounded time
for the child to exit. `SIGKILL` follows only as escalation when that wait
expires. The child is reaped in every case, including after `SIGKILL`, so no
zombie is left behind. The pane is closed after the child is gone.

At app shutdown this is not repeated per browser. All browsers are `SIGTERM`'d
first, then awaited against **one** shared deadline; whoever has not exited when
it expires is `SIGKILL`'d. Quit cost is therefore the grace period once, not once
per browser.

## Profile directories

Location: `$XDG_STATE_HOME/kabelsalat/browsers/<group-uuid>`.

The key is the group's stable uuid, not its id: ids are recycled — deleting the
highest-numbered group makes the next new group reuse that id — so an id-keyed
profile can be deleted out from under a new group that happens to reuse the id.
The directories are disposable cache, not user data — they persist across
restarts only because Chromium session restore requires it.

They are removed when the browser is closed via the overflow menu, when the
group is pruned or deleted, and by the startup sweep below. They are **not**
removed when Chromium exits on its own — that profile is what the next `Alt-2`
restores from. Such a kept profile is still reclaimed as soon as its group is
deleted; it does not have to wait for the next startup sweep.

Removal runs off the main thread: a Chromium profile is large enough that
deleting it synchronously stalls the UI.

## Restart

1. Groups load from `state.json` as today.
2. A worker thread sweeps `browsers/`, deleting every directory whose name does
   not match a live group uuid. This is pure filesystem IO with no GTK
   involvement and cannot race the UI, since it only touches directories no
   group refers to.
3. Groups with `browser_open: true` are restored **sequentially**, the active
   group first, so the browser the user can actually see becomes usable first.
   Remaining groups follow one at a time in unspecified order, spread across
   idle callbacks so the window is interactive immediately. Restored browsers
   for non-active groups are hidden, showing the header icon.

Sequential restore is a deliberate choice, not a limitation to work around:
`WaylandPane::new()` blocks on the compositor advertising its Wayland socket and
GTK widgets must be constructed on the main thread, so pane construction cannot
be parallelised regardless. Hidden panes have their frame pump paused, so
racing them to be ready buys nothing visible.

## Error handling

| Failure | Response |
|---|---|
| `pane.startup_error()` is set (compositor failed) | drop the pane, report in a dialog, group keeps no browser |
| Chromium binary not found | drop the pane, report in a dialog |
| Chromium spawn fails | drop the pane, report in a dialog |
| Chromium exits later | tear the pane down, keep the profile (see above), silently |
| profile dir cannot be created | treat as spawn failure |
| profile dir cannot be removed | log, continue — the startup sweep retries |

Profile removal is dispatched to a worker thread rather than done inline; a
failed removal is logged and left to the startup sweep.

The app must keep working when klamottenkiste cannot start at all (no DRM render
node, no EGL). This degrades to "Alt-2 reports an error and does nothing",
mirroring how the app degrades when tmux is missing.

## Testing

`state.rs` unit tests: round-tripping `SavedGroup` with and without the new
fields, confirming that a `state.json` written by an older build still loads.

`browser.rs` unit tests: the binary-candidate resolution and the profile-path
derivation, both of which are pure functions and need no display.

The pane itself cannot be tested headlessly here — it needs a DRM render node
and a display. klamottenkiste already gates multi-instance coexistence
(`examples/multi_headless.rs`, N=3) upstream; on-screen behaviour is left to
manual verification, as that project also does.

## Known risks

- **Scale.** klamottenkiste has verified three coexisting panes. A user with
  many groups each holding a browser is untested, and each pane costs a
  compositor thread, an EGL context, and a Chromium process. Known issue KI-4
  upstream: each pane does a full-frame CPU readback per frame unless the
  zero-copy dmabuf path (`KLAMOTTENKISTE_PRESENT`) is active, so idle cost grows
  with pane count. No cap is imposed for now; if this bites, measure before
  designing around it.
- **Startup latency.** Each restored pane adds a socket wait on the main thread.
  Spreading construction across idle callbacks keeps the window responsive but
  does not reduce total work. If it dominates, the fix belongs upstream in
  klamottenkiste as a deferred or async `spawn_headless`, not in kabelsalat.

## Out of scope

- More than one browser per group.
- Any browser other than Chromium.
- kabelsalat-side control of browser navigation, URLs, or tabs.
- Configurable browser command. The command is fixed; a setting can be added
  later if a machine needs it.
