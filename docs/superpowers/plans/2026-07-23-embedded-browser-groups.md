# Per-Group Embedded Browser — Phased Roadmap

> **For agentic workers:** This is a roadmap with gates, not a task-level implementation plan. When a phase is started, write a detailed implementation plan for that phase using superpowers:writing-plans, then execute via superpowers:subagent-driven-development or superpowers:executing-plans.

**Goal:** Each tab group can show/hide an embedded Chromium pane, split-screen with the terminal stack. Switching tabs within a group swaps only the terminal; the browser pane persists. A Claude Code instance running in any terminal of the group can attach to the browser via a per-group CDP endpoint published in an env var, and (later) drive any embedded Wayland app via a compositor control socket.

**Architecture:** Kabelsalat (Rust, GTK4/libadwaita/relm4, VTE, tmux control mode) gains a per-group nested Wayland compositor widget (Smithay-based, forked from `monochromatic-nested-wayland-session`) that hosts one Chromium instance launched with `--remote-debugging-port=<per-group port>`. The compositor renders the client's dmabuf buffers into a GTK pane and translates GTK input events to its own seat. Port/socket paths are injected into the group's tmux sessions as env vars.

**Tech stack:** Rust edition 2024, GTK4 + relm4, Smithay, `zwp_linux_dmabuf_v1` → GLES → `GdkTexture`/`GLArea`, Chromium (CDP), tmux.

## Global constraints

- Frictionless bar: the feature ships only if daily use has no chronic papercuts ("low friction or no feature at all"). Every gate evaluates against this.
- Wayland-first (Fedora/GNOME). No X11-only mechanisms (no XEmbed/XReparentWindow paths).
- Playwright/CDP compatibility is mandatory → the browser must be real Chromium; WebKitGTK is ruled out (BiDi immature, Playwright can't attach).
- Hidden browser = UI identical to today.

---

## Phase 0 — Spike: embedding go/no-go (~1–2 days)

The whole roadmap hangs on this gate. Nothing lands in kabelsalat's main branch.

**S1. Fork and strip**
- Fork the crate (repo: `Aquaticat/Monochromatic`, `packages/cli/nested-wayland-session`, ~2 kLOC).
- Keep: Smithay bootstrap, single-client hosting, dmabuf v4/v3 import → GLES, seat input synthesis, the Unix-socket control API (`screenshot`, `click`, `key`, `type`, `resize` — becomes a feature in Phase 3).
- Remove: recording/screencast timer, winit presentation window.

**S2. GTK presentation path**
- Toy GTK4 app (not kabelsalat): render the composited client surface into a `GLArea`/`GdkTexture`. This is the main unproven rework — the crate presents via its own winit window today.

**S3. Real input path**
- GTK event controllers (pointer, scroll, keyboard, focus) → compositor seat events, alongside the existing socket injection.

**S4. Papercut checklist with Chromium as the hosted client**
Launch `chromium --ozone-platform=wayland --remote-debugging-port=9333` against the nested socket and test, in order of expected pain:
1. `<select>` dropdown and right-click context menu (xdg_popup surfaces — biggest known risk; the crate documents no popup support).
2. Clipboard both directions (browser ↔ host terminals).
3. Typing in a form; keyboard focus handoff GTK ↔ pane.
4. Cursor shape changes.
5. HiDPI / fractional scaling.
6. Resize behavior (pane drag).
7. Playwright MCP attach over the CDP port while clicking around manually.

**GATE G0:** every checklist item is solid or has a clearly bounded fix. If popups/clipboard feel janky with no bounded path → **stop; no feature** (explicitly acceptable per requirements). Do not fall back to the external-window variant — it fails the friction bar by prior decision.

---

## Phase 1 — Group model & agent plumbing in kabelsalat (~1–1.5 days, after G0 pass)

Browser-agnostic core; almost all of it survives any Phase 2 surprises.

- Extend `Group` (src/app.rs:78) and `SavedGroup` (src/state.rs) with `browser_visible: bool`, `cdp_port: u16`, and profile-dir path; `#[serde(default)]` like `linger_warning_dismissed`.
- Port allocator: stable per-group port from a configurable base range, persisted; collision check at bind time.
- Env injection: set `KABELSALAT_CDP=http://127.0.0.1:<port>` (and later `KABELSALAT_GUI_SOCKET`) per group — at spawn in `add_tab` (src/app.rs:983) and via `tmux set-environment` for the group's existing sessions.
- Chromium lifecycle manager: launch (`--remote-debugging-port`, `--user-data-dir=<per-group profile>`), health check, crash detection/relaunch, kill on group close and app exit.
- UI: per-group show/hide toggle (sidebar group header + keybinding), state persisted.
- Docs: README snippet for users' CLAUDE.md files: "if `KABELSALAT_CDP` is set, connect Playwright MCP there."

**GATE G1:** toggle a group's browser on/off (rendered in a plain external window for now — scaffolding only, not shipped), Claude Code in a group terminal attaches via `$KABELSALAT_CDP` and inspects the page the user sees.

---

## Phase 2 — Embedded pane integration (~3–5 days)

- Integrate the forked compositor as a GTK widget in kabelsalat: horizontal `gtk::Paned` inside the content box (src/app.rs:315) — terminal `Stack` on one side, browser pane on the other; pane position persisted per group.
- Wire to group lifecycle: `activate()` (src/app.rs:1066) and `navigate_group()` (src/app.rs:1197) swap the browser pane only when `active_group()` changes; tab switches within a group never touch it.
- Hidden state: pane fully removed from layout — UI identical to pre-feature kabelsalat.
- One compositor+Chromium per group, lazily launched on first "show".
- Failure UX: Chromium crash → placeholder in pane with relaunch button; compositor panic isolated from terminals.

**GATE G2:** a week of daily self-use ("dogfood gate"). Any chronic papercut → fix or revert to no-feature. Friction bar is the acceptance test.

---

## Phase 3 — Universal app embedding + automation socket (~2–3 days, optional layer)

- Generalize "show browser" to "embed command": a group can host any Wayland app in the pane (Chromium remains the default/one-key case).
- Keep and expose the fork's control socket per group: `KABELSALAT_GUI_SOCKET=<path>` env var; commands `screenshot`, `click`, `key`, `type`, `resize`. Compositor-level injection reaches only the hosted client — never the host seat.
- Deliberately **not** BiDi: a minimal honest API beats a mostly-NotImplemented standard protocol. Browsers keep CDP as the deep channel; the socket is the universal shallow (vision/computer-use) channel.
- Optional: tiny MCP server wrapping the socket so Claude Code gets `screenshot`/`click`/`type` tools automatically; else document raw socket usage in CLAUDE.md.

**GATE G3:** Claude Code, from a group terminal, screenshots and clicks an embedded non-browser GTK app via the socket.

---

## Phase 4 — Polish & release

- Persistence: restore browser visibility/pane position/URL per group across restarts (extend `save_state()` src/app.rs:717 / `restore_or_fresh()` src/app.rs:622).
- Settings: Chromium binary path, port range, default URL.
- Upstream reusable fixes (esp. xdg_popup support) to the monochromatic author.
- Version bump + changelog.

---

## Risk register

| Risk | Phase | Mitigation |
|---|---|---|
| xdg_popup (dropdowns/menus) unsupported in fork base | 0 | Top of spike checklist; bounded-fix-or-stop gate |
| Clipboard integration complexity | 0/2 | Spike item 2; Smithay has data-device support to build on |
| Fork abandonment upstream (2-week-old, 1 author) | all | Treat as fork, not dependency; vendored in-tree |
| GNOME/driver-specific dmabuf issues | 0/2 | Spike runs on the actual target machine |
| Chromium flag/behavior drift | 2+ | Lifecycle manager owns flags in one place; health check |
