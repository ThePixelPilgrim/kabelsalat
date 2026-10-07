# Android pane — design

Date: 2026-10-07

## Goal

Let an agent in a kabelsalat group drive Firefox for Android (Fenix) running
in Waydroid, with the Android display shown as a pane in the group next to
the existing Chromium pane. Page access goes through geckodriver and WebDriver
BiDi over adb; input and screenshots outside the page go through adb or the
pane's control socket.

## Spike results (2026-10-07, this machine)

Everything below was proven end to end without root:

- Waydroid (Android 13, LineageOS, x86_64) boots on the host GPU inside
  klamottenkiste's headless nested compositor
  (`monochromatic-nested-wayland-session`). No props, no swiftshader.
  ~18.5 s from `waydroid session start` to Android ready, container already
  running. Live `resize` works; Android follows.
- The compositor advertises wl_compositor v5, wl_subcompositor, wl_shm,
  zwp_linux_dmabuf_v1 v5, xdg_wm_base v6, wl_seat v9 (keyboard+pointer, no
  touch), wl_output, zxdg_output_manager. No Xwayland.
- `adb connect 192.168.240.112:5555` works after one "Allow USB debugging"
  tap (`ro.adb.secure=1`).
- Official Fenix x86_64 APK installs with `adb install`.
- geckodriver 0.37.1 drives the release build. It sets `am set-debug-app`
  itself. `webSocketUrl:true` yields a BiDi socket; `browsingContext.navigate`,
  `script.evaluate` and `browsingContext.captureScreenshot` work.
- Pane control socket `click`, `type`, `key` reach Android.

Constraints found:

1. One Waydroid per machine; its Wayland display is fixed at `session start`.
   A second `session start` fails with "Session is already running".
2. geckodriver runs `pm clear` on Fenix at every new session: no persistent
   logins or history, onboarding reappears each session.
3. geckodriver needs `--android-storage internal` on Waydroid
   (`/storage/emulated` is a btrfs bind mount; `secure_mkdirs` fails). The
   flag is deprecated upstream.
4. BiDi `captureScreenshot` fails ("width: 0 and height: 0") while Fenix's
   onboarding overlay covers the tab.
5. Pane seat has no touch device; Android sees a mouse. The first click after
   pointer-enter misfired once as a top-edge swipe. Multi-char `key` goes
   through the US keymap (klamottenkiste KI-3); `type` is correct.
6. Fenix at 720 px / density 180 uses its tablet layout and defaults to the
   desktop site.
7. BiDi `session.status` reports `ready:false` while a session is open.
   Port 4444 is taken locally by another process.

## Decisions

_Realigned with the shipped branch `android-pane` after the final review; the
items below and the signatures in sections 1–3 describe what was built._

- Page access: geckodriver-managed sessions (Fenix is disposable). Attaching
  to a persistent Fenix is a possible follow-up, not in scope.
- Surface control: a thin CLI over klamottenkiste's control socket, plus adb
  for Android input. No CDP-shaped server.
- Ownership: the Android pane is claimed by one group at a time, like the
  browser pane. Claiming while another group owns it is refused.
- Panes per group: at most one Browser and one Android, shown as tabs in the
  pane area.
- kabelsalat starts the compositor and the Waydroid session only. Fenix,
  geckodriver and adb authorisation are agent-side, done by the skill.

## Out of scope

Moving the pane between groups without restart, a CDP-shaped server,
Xwayland and the AVD emulator, persistent Fenix state, several browser
profiles per group.

## 1. `src/android.rs` (new, mirrors `src/browser.rs`)

```rust
pub struct Android { pane: WaylandPane, session: Child, ... }

pub enum WaydroidStatus { NotInitialised, Stopped, Running { display: String } }
pub fn parse_status(output: &str) -> WaydroidStatus;          // pure
pub enum Precondition { Ok, NoBinary, NotInitialised, ForeignSession(String) }
pub fn check(status: Option<WaydroidStatus>, our_display: &str) -> Precondition; // pure
pub fn session_start_args() -> Vec<String>;   // pure; the display travels in the env
pub fn show_full_ui_args() -> Vec<String>;    // pure
pub fn client_env(display: &str) -> Vec<(String, String)>; // pure
pub fn should_stop_session(child_alive: bool, saw_foreign: bool) -> bool; // pure
pub fn is_dead(child_exited: bool, compositor_running: bool) -> bool;    // pure
pub fn adb_serial(status_output: &str) -> String;             // pure, default 192.168.240.112:5555
```

- `Android::spawn()`:
  1. `waydroid status` → `parse_status`; `check` must be `Ok`, else return
     `Err` with a user-facing message (no panic; the app degrades like it does
     without tmux). The check runs before the pane exists, so every running
     session counts as foreign. A status call that does not answer is its
     own error (`StatusUnanswered`), not "not installed".
  2. `WaylandPane::new()`, read its socket and control socket path, and
     `chmod 0600` the control socket (klamottenkiste binds it in the system
     temp dir with umask permissions).
  3. Run `waydroid session start` with `WAYLAND_DISPLAY=<socket>` in its own
     process group (reuse `isolate_process_group`), stdout/stderr to
     `<state_dir>/android/session.log`.
  4. A watcher thread polls `waydroid status` every 500 ms until
     `Running{display==ours}`, runs `waydroid show-full-ui` with the same env
     and process group, then waits for `sys.boot_completed=1`. One 60 s
     budget covers all three phases; each command is capped at the remaining
     budget. The watcher reports through a callback tagged with a boot id;
     the app ignores reports whose id is stale.
- `teardown()`: `waydroid session stop` only when our child is still alive
  and no foreign session was seen (`should_stop_session`), then kill the
  process group, then close the pane. A foreign session is never stopped.
- `control_socket_path()`, `capture_frame()`, `has_exited()`, `is_running()`
  (compositor alive). The poll treats the pane as dead when the child exited
  or the compositor stopped (`is_dead`); Waydroid does not exit when its
  display goes away, so both are needed. There is no `set_visible`: panes
  pause by being unmapped.

## 2. `src/app.rs` wiring

- The `browser_paned` end child becomes a pane host: `adw::TabBar` over an
  `adw::TabView`, one page per open pane, titles "Browser" and "Android",
  closable. The host is attached only when the group has at least one pane
  open, so a group without panes looks as it does today.
- Group state: `browser: Option<Browser>`, `android: Option<Android>`,
  `panes_visible: bool` (replaces `browser_visible`), `front_pane: PaneKind`
  where `enum PaneKind { Browser, Android }`. Tab switch sets `front_pane`;
  closing a tab tears that pane down (Browser: as today's close browser,
  profile removed; Android: teardown).
- Shortcuts: Alt+2 / Alt+Shift+2 toggle the pane area (unchanged bindings).
  Alt+3 / Alt+Shift+3 open Android, or bring it to the front if open; either
  way the pane area becomes visible. Menu:
  "Open Android pane", "Stop Android".
- Messages: `ToggleAndroid`, `StopAndroid`, `AndroidReady`, `AndroidDied`,
  `OpenAndroid` (CLI), `PaneSelected(PaneKind)`. `PollAndroid` folds into
  the existing poll timer.
- Visibility: only the front pane of the active group pumps frames. Panes
  pause by being unmapped (a non-selected tab, or a detached host); an
  `adw::TabView` cannot hold a hidden child, so `set_visible` is not used.
- Env published into the owning group's tabs via the existing
  `tmux set-environment` path: `KABELSALAT_ANDROID_CTL=<control socket>`,
  `KABELSALAT_ANDROID_ADB=<serial>`. Both unset on teardown or death.
- CLI-created panes never raise, focus, switch groups or change
  `front_pane`; `OpenAndroid` adds a hidden page when the group isn't active.
- Restart: `state.json` records `android_owner: Option<String>` (group uuids
  are strings throughout `state.rs`). On launch the pane is reopened hidden
  for that group, mirroring `pending_browser_restore`. If another group took
  Android before the idle restore fires, the restore skips silently.
- Closing a pane that had the keyboard hands focus back to the terminal.
- Alt+2 in an Android-only group hidden state spawns a browser (Alt+3 and the
  indicator re-show Android); with the area visible it hides it whichever
  pane is in front.
- Screenshot-to-terminal captures the front pane.

## 3. CLI (`src/cli.rs` pure, `src/control.rs` glue)

```
kabelsalat android [-g GROUP]
kabelsalat android screenshot PATH | tap X Y | type TEXT | key NAME | resize W H
```

- `android` with no subcommand: if GROUP owns a ready pane, print
  `ctl=<socket>\nadb=<serial>`, exit 0. While it is still booting: nothing on
  stdout, a hint on stderr, exit 0. If nobody owns one, emit
  `Action::OpenAndroid`, nothing on stdout (a hint on stderr; caller polls
  env), exit 0. If another group owns it, exit 3 with a message naming that
  group. Remote group: exit 3. `-h`/`--help` prints the help.
- Subcommands forward one line to the control socket and print the reply
  (`ok <data>` payload on stdout; an `err` reply → exit 4; unreachable or
  silent socket → exit 3, one 10 s deadline for the round trip). `cli.rs`
  returns `Action::Control { socket, line }`; `control.rs` does the socket
  I/O. No owned pane: exit 3. A relative `screenshot` path resolves against
  the caller's cwd; non-UTF-8 or whitespace-padded paths, multi-line `type`
  text and multi-word `key` names are usage errors. Wire format follows
  `vendor/nested-wayland-session/src/protocol.rs` (`click X Y`, `key NAME`,
  `type TEXT`, `resize W H`, `screenshot PATH`).
- Group defaults from `KABELSALAT_GROUP` as for `browser`.
- Group snapshot in `control.rs` gains the android owner, control socket and
  serial.

## 4. Skill (`skills/kabelsalat/SKILL.md`, new "Android" section)

1. Pin `KABELSALAT_GROUP`; `tmux show-environment KABELSALAT_ANDROID_CTL`.
   If unset, run `kabelsalat android` and poll env for up to 90 s.
2. `adb connect $KABELSALAT_ANDROID_ADB`. On `unauthorized`: screenshot via
   `kabelsalat android screenshot`, tap "Always allow" with
   `kabelsalat android tap`, reconnect.
3. Fenix: `adb shell pm path org.mozilla.firefox`; if missing, download the
   x86_64 APK from `https://archive.mozilla.org/pub/fenix/releases/` into a
   fresh directory, `adb install`.
4. geckodriver: download the linux64 release if missing (verify sha256
   against the GitHub asset digest). Run
   `geckodriver --android-storage internal --port <free port>`; never assume
   4444.
5. New session with `browserName:firefox`, `webSocketUrl:true`,
   `moz:firefoxOptions.androidPackage:org.mozilla.firefox`,
   `androidDeviceSerial:$KABELSALAT_ANDROID_ADB`. Connect to `webSocketUrl`
   with a raw websocket client; example Python script included
   (navigate, `script.evaluate`, `captureScreenshot`).
6. Known behaviours: every session wipes Fenix (`pm clear`); dismiss the
   onboarding overlay with a pane tap before `captureScreenshot`;
   `session.status` is not a health check; phone layout via
   `adb shell wm density 420` or a narrower pane.
7. Input rule: prefer `adb shell input tap|text|keyevent` for anything in
   Android. Use `kabelsalat android tap|type|key` only when adb is not yet
   authorised. Note the first-click quirk and KI-3 (use `type`, not multi-char
   `key`).

## 5. Testing (red-green)

- `android.rs`: `parse_status` for the three states and the display line,
  `check` for every precondition, the arg builders, `adb_serial` default and
  parsed IP.
- `cli.rs`: parse of `android` and each subcommand, usage errors (exit 2),
  dispatch outcomes — owned-and-ready prints, unowned emits `OpenAndroid`,
  foreign owner exit 3, remote exit 3, control line construction.
- `state.rs`: `android_owner` round-trips through `state.json`; restore plan
  reopens hidden for the owner and is a no-op when absent.
- `tmuxctl.rs`: set/unset args for the two new env keys.
- `app.rs` remains wiring only.
- Acceptance: the spike's manual run (compositor, Waydroid, adb, Fenix,
  geckodriver BiDi evaluate of `document.title`) repeated through the GUI and
  the skill text.
