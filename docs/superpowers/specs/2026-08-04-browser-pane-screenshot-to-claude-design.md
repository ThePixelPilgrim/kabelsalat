# Browser-pane screenshot → Claude paste

Design for a one-click (later: one-keyword) way to put what the user sees in
the embedded browser pane in front of the Claude agent running in a terminal
tab — replacing today's two-round-trip flow where the agent writes a
Playwright/CDP script, screenshots, and ingests the file in a second turn.

Date: 2026-08-04. Status: validated by live experiment (see § Validation).

## Goal

The user clicks a screenshot button in kabelsalat. The current composited
frame of the active group's browser pane lands on the Wayland clipboard, and a
paste keystroke is injected into the active terminal tab so a running `claude`
session stages it as `[Image #1]`. The user types their problem description
next to the staged image and sends both together. The screenshot also remains
on the clipboard for the user to paste anywhere else; no file is ever written.

## Decisions taken (with reasons)

- **Capture source is the klamottenkiste pane, not the desktop.** The
  terminal side never needs pixels (the agent reads text); the browser is the
  opaque part. Pane-level capture is compositor-agnostic (no niri/portal
  dependency) and bounded by construction: nothing outside the pane can leak
  to the model.
- **In-memory hand-off (option B), not the control socket's
  `screenshot <path>` command (option A).** B returns RGBA bytes in-process;
  kabelsalat wraps them in a `gdk::MemoryTexture` and sets the clipboard
  directly. No temp file, no PNG encode/decode round-trip. A remains a valid
  fallback and needs no library change, but the file was pure transport
  overhead.
- **Paste is injected via VTE `feed_child`, not tmux `send-keys`.** Writing
  the Ctrl-V byte (`0x16`) to the active tab's pty behaves identically whether
  the pty runs a tmux client or a plain shell. tmux drops out of the feature
  entirely, which satisfies the "must work without tmux" invariant
  structurally instead of via a degraded-mode branch.
- **The library returns pixels; the app owns policy.** klamottenkiste never
  touches the host clipboard or the terminal. Its new API is "give me the
  current frame"; everything opinionated (clipboard, keystroke, UI) stays in
  kabelsalat.

## Component 1: klamottenkiste — `WaylandPane::capture_frame`

New public API on `WaylandPane` (klamottenkiste/src/widget.rs):

```rust
pub struct CapturedFrame {
    pub rgba: Vec<u8>,   // RGBA8, upright rows
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

#[derive(Debug)]
pub enum CaptureError {
    NotRunning,      // compositor thread not started or already shut down
    RenderFailed(String),
}

impl WaylandPane {
    /// Request a freshly rendered frame of the nested output.
    /// `callback` is invoked on the GTK main context.
    pub fn capture_frame<F>(&self, callback: F)
    where
        F: FnOnce(Result<CapturedFrame, CaptureError>) + 'static;
}
```

Implementation notes:

- A one-shot request is sent into the compositor thread's calloop event loop
  (same pattern the control socket's request/reply channel already uses in
  `control.rs`; either reuse that channel with a new request variant or add a
  parallel lightweight channel).
- The compositor-thread handler calls the existing
  `render::read_frame_rgba(&mut Compositor)` (vendor crate, render.rs), which
  renders a fresh frame off the offscreen GLES renderbuffer and maps it to CPU
  memory. This works in **both** present modes — it must not rely on the
  `latest_frame()` cache, which is only populated in `Readback` mode while the
  default is `Dmabuf`.
- The reply crosses back to the GTK thread via a oneshot channel drained on
  the GTK main context (e.g. `glib::MainContext` channel/idle), so the UI
  thread never blocks on GPU readback.
- If the pane is not running, the callback fires with
  `Err(CaptureError::NotRunning)` instead of being dropped silently.

Release: this is the only klamottenkiste change. Ship as a new tag (v0.2.0);
kabelsalat then bumps its pinned tag. No path dependencies on `main`, per the
existing release discipline. The capture primitive lands on the LGPL side,
all app wiring on the MIT side.

## Component 2: kabelsalat — button, clipboard, paste

UI: one header-bar button (`camera-photo-symbolic`, tooltip "Screenshot
browser → paste into terminal"), declared in the `view!` macro next to the
browser toggle. Sensitive only while the active group's browser is running
(same condition that makes a capture possible).

relm4 flow in `src/app.rs`:

1. `Msg::Screenshot` (button click) → resolve the active group's
   `WaylandPane`, call `capture_frame`, forwarding the result as
   `Msg::ScreenshotCaptured(Result<CapturedFrame, CaptureError>)` through the
   component sender.
2. `Msg::ScreenshotCaptured(Ok(frame))`:
   - Build `gdk::MemoryTexture` from the frame
     (`gdk::MemoryFormat::R8g8b8a8`, respecting `stride`).
   - `display.clipboard().set_texture(&texture)`. The claim is valid because
     kabelsalat holds the focused surface (the user just clicked its button).
   - After the clipboard claim has been flushed (defer to the next main-loop
     iteration, e.g. an idle callback — no arbitrary sleeps), write `0x16`
     to the **active tab's** VTE via `feed_child`. If there is no active tab,
     skip the paste but keep the clipboard set — the screenshot is still
     usable manually.
3. `Msg::ScreenshotCaptured(Err(e))`: show an `adw::Toast` with a short
   message; log the detail. No keystroke is sent.

GDK serializes the texture to `image/png` lazily per requester, so Claude
Code's `wl-paste` read and any later manual paste by the user are independent
reads of the same owned selection.

## Data flow

```
click → Msg::Screenshot
      → WaylandPane::capture_frame ──(calloop request)──► compositor thread
                                                          read_frame_rgba
      ◄──(oneshot, GTK main context)── CapturedFrame ◄────┘
      → MemoryTexture → clipboard.set_texture
      → (next main-loop iteration) active tab VTE feed_child(0x16)
      → claude catches Ctrl-V, reads clipboard via wl-paste → "[Image #1]"
```

## Edge cases and accepted behavior

- **Active tab isn't running claude**: the shell receives `0x16` (in
  readline that's literal-next — harmless). Same outcome as the user pressing
  Ctrl-V by hand; acceptable for a user-initiated action.
- **tmux client in copy-mode / prefix pending**: tmux eats the byte, same as
  a manual Ctrl-V would be. Accepted; no special handling.
- **Clipboard clobbering**: the button replaces the current clipboard
  content, like any copy action. Accepted.
- **Clipboard lifetime**: the selection lives as long as kabelsalat runs
  (standard Wayland semantics). Fine for a long-running terminal app.
- **Browser hidden but running**: capture still works (offscreen renderbuffer
  is independent of widget visibility). The button keys off "browser
  running", not "browser visible".
- **No browser / pane crashed**: button insensitive; if a race slips through,
  `CaptureError` → toast.
- **Focus invariants**: unaffected. The action is user-initiated inside the
  focused window; no tab is created, raised, or activated.

## Validation already performed (2026-08-04, live system)

- `WAYLAND_DISPLAY` is present in kabelsalat's tmux panes and `wl-paste`
  connects — Claude Code can read the clipboard from inside a pane.
- End-to-end paste chain proven: image on clipboard → Ctrl-V into a freshly
  spawned `claude` tab → `[Image #1]` staged in its input box.
- Wayland refuses selection claims from clients without a focused surface
  (verified with a headless GTK script). Kabelsalat is focused at click time,
  so this does not affect the design — but it is why the clipboard set must
  happen in the app, not in a helper process.
- klamottenkiste v0.1.0 already contains the render/readback plumbing
  (`read_frame_rgba`, used by the control socket's `screenshot <path>`
  command); the new API only adds an in-process request/reply wrapper.

Remaining assumption to confirm during implementation: GDK's lazy `image/png`
serialization of a `MemoryTexture` is accepted by Claude Code's `wl-paste`
read (proven with `wl-copy`-owned PNG; the GTK-owned variant is checked the
first time the real button is clicked).

## Testing

- **klamottenkiste**: an integration test alongside the existing compositor
  tests — start the headless session, call `capture_frame`, assert a frame
  with plausible dimensions and stride arrives on the main context; assert
  `NotRunning` after shutdown.
- **kabelsalat**: the feature is GUI + side effects, deliberately kept out of
  `state.rs` (no persistence impact, nothing to reconcile). Pure-logic pieces
  that emerge (e.g. button-sensitivity predicate) get unit tests next to the
  existing ones. End-to-end verification is manual, mirroring the validation
  steps above; `cargo fmt`, `cargo clippy`, `cargo test` before completion.

## Phase 2 (recorded, out of scope)

Keyword trigger: detect the user *typing* `SCREENSHOT` in a terminal tab and
run the same capture→clipboard→paste pipeline without a button click.
Watch the keystroke stream at the VTE layer (per-tab rolling buffer) — not
tmux output scraping, which suffers redraw duplication, escape-sequence
splitting, and false triggers when the agent merely prints the word. The
typed word remains in the prompt as harmless self-documentation, and the
image pastes at the cursor. Builds on this feature's pipeline unchanged.
