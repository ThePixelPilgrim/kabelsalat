# Tab age prefix

Prefix every tab title with the time since the tab last saw activity — `now`,
`5m`, `2h`, `3d` — so a glance at the sidebar shows which terminals are alive
and which have sat untouched for days.

## Goals

- Every tab title, in the sidebar and the tab bar, carries an age prefix.
- Activity means keystrokes, terminal content changes, and title changes.
- Anything under a minute displays as `now`; the display never shows seconds.
- Ages survive an app restart, without writing anything to `state.json`.
- The app keeps working unchanged when tmux is missing — ages then start
  fresh, which is honest, because the tabs themselves started fresh.

## Background

Nothing tracks activity today. The app connects exactly one VTE signal per
terminal — `connect_window_title_notify` (`src/app.rs:2138-2148`) — and no
key controller sits on any terminal widget. The vte4 crate additionally
offers `connect_contents_changed`, which fires on any change to the visible
terminal content, including keystroke echo.

Two facts shape the design.

**tmux already records last activity.** Every session carries
`#{session_activity}`, a unix timestamp the server updates on pty traffic —
including traffic that happens while the GUI is closed. The app already
fetches a per-session format via `list-sessions`
(`src/tmuxctl.rs:419-426`); extending it by one field gives restart
persistence for free. Writing a timestamp to `state.json` instead would
create a second, staler copy of something tmux knows authoritatively, and
would cost the disk writes this design avoids entirely.

**Wall clock, not monotonic clock.** Rust's `Instant` uses
`CLOCK_MONOTONIC`, which does not advance while the machine is suspended: a
laptop closed overnight would undercount every age by the whole suspension.
`SystemTime` counts real time, and the tmux seed is a unix timestamp anyway.
Backward clock jumps are absorbed by saturating: an age is
`duration_since(last).unwrap_or(ZERO)`.

## Architecture

### `src/app.rs`

`Tab` (`src/app.rs:122-129`) gains two runtime fields, never persisted:

```rust
last_activity: Rc<Cell<SystemTime>>,
age_shown: String, // prefix currently rendered; the tick updates labels only on change
```

`Rc<Cell<…>>` because the signal handlers below fire at output rate — far
too often to send relm4 messages. Each handler does exactly one `Cell::set`
and nothing else; the cost of an update is one `clock_gettime` plus a store.

At terminal construction (the builder block around `src/app.rs:2100`),
connect two new sources, each cloning the `Rc`:

- `terminal.connect_contents_changed(...)` — output and echoed keystrokes.
- A `gtk::EventControllerKey` on the terminal widget, capture phase,
  `connect_key_pressed` returning `glib::Propagation::Proceed` — records
  silent keystrokes (password prompts, keys a full-screen app swallows
  without redrawing) without stealing any input from VTE.

Title changes need no new connection: `update_title` (`src/app.rs`, the
`Msg::TitleChanged` path) additionally sets the tab's `last_activity`.

**Formatting.** Two pure functions beside `crashed_tab_label`
(`src/app.rs:3033`):

```rust
fn age_prefix(elapsed: Duration) -> String
```

`< 60 s → "now"`, `< 60 min → "Xm"`, `< 24 h → "Xh"`, else `"Xd"`,
uncapped — `45d` is honest and needs no fourth unit.

```rust
fn display_title(age: &str, title: &str, crashed: Option<i32>, collapsed: Option<usize>) -> String
```

producing `"now · build"`, `"3d · build [exit 1]"`, `"2h · web (3)"`. It
subsumes `row_label_text` (`src/app.rs:3045`); the tab bar, which today uses
`tab.title` raw (`src/app.rs:2471-2476`), is routed through the same
function — closing that inconsistency is part of this change. The tab-bar
tooltip stays the raw title: it exists to show what the ellipsized label
cannot, and the prefix is already visible on the label.

The window title (`src/app.rs:347`) stays static.

**Refresh tick.** A new always-on
`glib::timeout_add_seconds_local(30, …)` sends `Msg::AgeTick`. The handler
recomputes each tab's prefix, compares it against the tab's cached
`age_shown`, and only on change updates the cache and calls the existing
in-place refreshers (`refresh_sidebar_label`, `refresh_tab_bar_label`). No
always-running timer exists today — the 2 s browser poll
(`src/app.rs:1628-1641`) only runs while a group has a browser — so this is
a new timer, not a piggyback. Worst case a tab shows
`now` for 89 seconds before flipping to `1m`; within tolerance for a
minute-granularity display.

### `src/tmuxctl.rs`

The `list-sessions` format string
(`'#{session_name}\t#{pane_dead}\t#{pane_dead_status}'`,
`src/tmuxctl.rs:419-426`) gains a fourth field, `#{session_activity}`.
`parse_session_line` and the `SessionInfo` it produces carry the value
through as `Option<SystemTime>`: a missing, empty, or non-numeric field
parses as `None` rather than failing the line — an old tmux, or a format
surprise, must not cost the session list. Everything stays `Result`; no
panics, per the file's invariant.

### `src/state.rs`

Untouched. No new field, no schema change, no writes.

## Behaviour

- **Tab created:** `last_activity` starts at `SystemTime::now()` — a fresh
  tab is `now` by definition.
- **Reattach on startup:** each restored tab seeds `last_activity` from its
  session's `#{session_activity}`. `None` seeds with now.
- **Crashed tab:** the age keeps counting from the last activity —
  `3h · build [exit 1]` reads as "died three hours ago", which is exactly
  the useful fact.
- **Collapsed group row:** shows the representative tab's age, consistent
  with it showing that tab's title.
- **No tmux:** tabs are plain shells that never survive a restart, so every
  tab is genuinely new; seeding with now is the truth, not a fallback lie.

## Error handling

- `#{session_activity}` missing or unparsable → `None` → seed with now.
  Never a failed reattach.
- Clock jumps backward past `last_activity` → age saturates to zero →
  displays `now` until real time catches up.
- The tick fires with zero tabs → nothing to compare, nothing to update.

## Testing

Pure functions only, in the existing style (banner comment, direct
`assert_eq!`, no GTK):

- `age_prefix`: 0 s and 59 s → `now`; 60 s → `1m`; 3 599 s → `59m`;
  3 600 s → `1h`; 86 399 s → `23h`; 86 400 s → `1d`; 40 days → `40d`.
- `display_title`: plain, crashed, collapsed, and crashed-while-collapsed
  compositions; prefix always present.
- `parse_session_line`: existing cases extended with the fourth field —
  present and numeric, missing, empty, garbage; foreign sessions still
  ignored.

## Known risks

- **`contents-changed` on reattach.** Reattaching a tmux session repaints
  the whole terminal, which fires `contents-changed` and would stamp every
  restored tab as `now`, defeating the seed. The seed must therefore be
  applied *after* the initial attach settles, or the handler connected only
  once the first repaint is done. Verify the ordering during
  implementation; this is the one place the design touches event timing.
- **Key controller interference.** The controller is capture-phase and
  non-consuming, so VTE sees every key exactly as before — but any
  regression here is a daily-use papercut. Manual check: typing, shortcuts,
  and IME input behave identically with the controller attached.
- **Label churn.** The 30 s tick updates only labels whose prefix changed;
  a pathological case (hundreds of tabs crossing a minute boundary
  together) is still bounded by tab count and happens twice a minute at
  most.

## Out of scope

- Sorting or dimming tabs by age.
- Group-level age aggregation beyond the collapsed row's representative.
- Tracking activity types separately (keystroke vs output); one timestamp
  serves all.

---

# Revision v2 — meaningful activity only (2026-08-13)

Dogfooding falsified the v1 source model. Field observations (sidebar full of
agent tabs): most tabs pinned to `now` and never aging, two-bucket age
distributions after reattach, tabs idle for days showing minutes. Trace
results:

- `contents-changed` fires on any *visible appearance* change, not on
  meaningful output. An idle Claude TUI repaints continuously (spinner,
  status line, caret), so an agent tab can never age. The 750 ms
  post-attach settle window guards only the attach repaint, not this.
- The v1 title hook stamps on every OSC title *event*, including events
  that carry an unchanged title.
- The tmux seed (`#{window_activity}`, and `#{session_activity}` before
  it) is itself output-based: an idle-but-running agent keeps it
  perpetually fresh inside tmux. No kabelsalat-side filtering can fix the
  seed; "4 days ago" is unreachable from any output-derived source.

## v2 activity definition

A tab's age is the time since the last **meaningful** event, which is
exactly one of:

1. **User input, strict (global default):** a press of `Esc` or `Enter`
   (`GDK_KEY_Escape`, `GDK_KEY_Return`, `GDK_KEY_KP_Enter`,
   `GDK_KEY_ISO_Enter`) on the tab's key controller. In an agent tab these
   are the moments of intent — submitting work, interrupting — while
   composing and scrolling stay silent. The controller sees GTK keyvals,
   so `Esc` is cleanly distinguishable from ESC-prefixed escape sequences;
   a pty-byte tap could not make this distinction.
2. **Agent activity: a title *change*.** Stamp only when the new title
   string differs from the tab's previous one. Agents update the terminal
   title while they work; when the title stops changing, the tab ages.
   The comparison also absorbs the attach-time title re-emission for free.
   The signal is not the change: VTE's `window-title-notify` fires on
   every title *write*, including writes that set the same string — an
   "update" and an "update that changed something" are indistinguishable
   at the signal level. Change detection is therefore always an app-side
   comparison against the stored `last_title`; the signal only prompts
   the comparison, and is never itself evidence of activity.

Dropped from v1: `contents-changed` as a source (entirely), the
unchanged-title stamps, the `#{window_activity}`/`#{session_activity}`
seed, and with them the `activity_armed` / `ACTIVITY_SETTLE_MS` machinery —
neither remaining source needs an attach-settle window.

Deferred (explicitly wanted later, not now): per-tab/per-group activity
modes — e.g. a lenient mode counting any keystroke — and assignment
policies (by command, by group). v2 ships one global behavior: strict
input + title changes.

## Persistence (reverses v1's "state.rs untouched")

v1 avoided `state.json` because tmux was assumed authoritative. With
output-derived sources rejected, tmux has nothing per-tab to offer: there
is no per-pane "last input" or "last title change" in tmux. The persisted
state is now the only truthful seed, so `SavedTab` gains:

```rust
last_activity: Option<SystemTime>,  // serde-friendly encoding at impl time
last_title: Option<String>,         // dedupe anchor across restarts
```

- Written only on the existing save events (tab create/close/move, title
  change already triggers `save_state` today — verify at impl time;
  otherwise piggyback on the events that do). The 30 s `AgeTick` still
  never writes — v1's no-disk-churn guarantee stands.
- `Esc`/`Enter` stamps mark state dirty but must not fsync per keystroke;
  they ride the next regular save. A stamp lost to a crash costs at most
  the age looking slightly older — the honest direction to err.
- Reattach seeds `last_activity` from the persisted value; missing field
  (older state files) seeds with now, once. `last_title` prevents the
  attach-time title re-emission from stamping.
- No-tmux mode is unchanged: tabs are fresh shells, seeded now — still
  the truth.

## v2 behaviour deltas

- An idle agent tab ages honestly: its TUI may repaint forever, but with
  no title change and no Esc/Enter, `4d` is reachable and correct.
- A *working* agent tab shows `now` while the agent is actually doing
  things (title churn), which is the desired signal.
- Plain shells under the strict default only stamp on Enter — which in a
  shell is precisely "a command was run". Acceptable; lenient mode is the
  deferred refinement.

## v2 testing additions

- Title dedupe: same title twice → one stamp; A→B→B→A → three stamps.
- Strict filter: Esc/Enter keyvals stamp; printable keys, arrows (ESC-
  prefixed on the wire, distinct keyvals in GTK) do not.
- `SavedTab` round-trip with and without the new fields (old state files
  must load; `state.rs` reconciliation carries the values through).
- Age seeding: persisted value wins; absent → now.

## Amendment 2026-09-14: contents-changed is back

`contents-changed` is an activity source again, alongside Enter/Escape and
title changes, so that output alone (a finished build, a log line) counts as
activity and drives the activity-sorted sidebar. The v1 attach-settle window
returns with it (`ACTIVITY_SETTLE_MS`, 750 ms after spawn) to protect the
persisted seed from the reattach repaint. The v2 finding still holds and is
accepted as the trade-off: a TUI that repaints while idle never ages.
