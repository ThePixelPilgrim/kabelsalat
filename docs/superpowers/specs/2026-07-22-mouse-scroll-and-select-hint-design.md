# Mouse wheel scrollback and the Shift-select hint

Date: 2026-07-22
Status: approved design

## Goal

Make the mouse wheel scroll tmux's scrollback instead of injecting cursor
keys into the shell, and mitigate the one regression that fix causes:
text selection now requires holding Shift.

## Problem

tmux keeps VTE permanently in the alternate screen. With `set -g mouse
off`, tmux never requested mouse tracking, so no application claimed the
wheel and VTE's *fallback scrolling* took over: it translated wheel-up
and wheel-down into `ESC [ A` / `ESC [ B` and wrote them to the pty.

Observed symptoms, both from this single cause:

- At a shell prompt, readline consumed the arrows as history navigation.
- Under a raw-mode program, the sequences were echoed as literal `^[[A`.

The 100k-line tmux history was unreachable throughout, because it is only
reachable via copy-mode and `unbind -a -T root` had removed tmux's stock
`WheelUpPane` binding.

## Part 1: Wheel scrolls tmux scrollback (implemented)

### tmux side (`src/tmuxctl.rs`, `TMUX_CONF`)

```
set -g mouse on
set -g mode-keys emacs
bind -n WheelUpPane if -Ft= '#{?pane_in_mode,1,#{mouse_any_flag}}' 'send -M' 'copy-mode -et='
bind -n WheelDownPane send -M
bind -T copy-mode WheelUpPane   send -X -N 3 scroll-up
bind -T copy-mode WheelDownPane send -X -N 3 scroll-down
bind -T copy-mode Escape send -X cancel
bind -T copy-mode q      send -X cancel
```

`mouse on` alone is insufficient: the `unbind -a` lines above wipe the
root and copy-mode key tables, including the stock wheel bindings, so
they must be restored explicitly and must appear *after* the unbinds.

The root binding is tmux's own default, restored verbatim. It forwards
the wheel to applications that grabbed the mouse (vim, htop, less) and
only otherwise enters copy-mode. The `-e` on `copy-mode -et=` makes tmux
leave copy-mode automatically once the user scrolls back to the bottom,
so the wheel behaves like a normal terminal rather than trapping the user
in a mode.

### VTE side (`src/app.rs`, `add_tab`)

`Terminal::builder().enable_fallback_scrolling(false)` — belt and braces.
VTE can then never synthesize cursor keys again even if the tmux config
regresses, and VTE's own scrollback is empty under tmux regardless.

### Rejected alternatives

- **Disable fallback scrolling only.** Stops the spurious keys but leaves
  the wheel inert and the scrollback unreachable.
- **Stop tmux using the alternate screen** so VTE owns the scrollback.
  Fights tmux's design for no gain.
- **Keep `mouse off` and drive copy-mode from GTK** via
  `tmux copy-mode -e` / `send-keys -X scroll-up` commands on a GTK scroll
  controller. This preserves native selection *and* scrollback, and was
  seriously considered. Rejected for cost: a tmux fork per wheel tick
  needing delta coalescing, plus a "typing while scrolled up is swallowed
  by the empty copy-mode key table" trap whose fix (`bind -T copy-mode
  Any send -X cancel`) still eats the first keystroke. Revisit if the
  Shift requirement proves annoying in practice.

## Part 2: Shift-select hint

### Rationale

X11/xterm mouse reporting is all-or-nothing per terminal: DECSET 1000
routes every button and motion event to the application, so there is no
way to give tmux the wheel while VTE keeps the buttons. Holding Shift is
the terminal emulator's standard override, and gnome-terminal, iTerm and
Kitty all behave this way under tmux. The requirement is conventional but
undiscoverable, so the app should teach it at the moment it bites.

### Detection

A `gtk::GestureDrag` attached to each `Terminal` in `add_tab()`, set to
`PropagationPhase::Capture` and never claiming its sequence, so VTE and
tmux receive the events untouched.

It sends `Msg::BareDragHint` on `drag_update` when all of:

- drag distance exceeds ~8px (a click with a twitchy hand stays silent)
- `SHIFT` is absent from the gesture's modifier state
- the hint has not already fired for this drag (one-shot latch per
  gesture, reset on `drag_end`)

Shift-drags — the ones that actually work — never trigger it.

**Accepted limitation:** when a full-screen app inside tmux has grabbed
the mouse (vim, htop), dragging is meaningful to that app and the hint is
unnecessary. VTE cannot tell us this; from its side tmux always has mouse
tracking on. Detecting it would need a `#{mouse_any_flag}` tmux fork per
drag start. Not worth it — the hint remains *true* in that case, since
Shift-drag still yields a VTE selection.

### UI

A third `pack_end` headerbar button, following the two existing
`tmux-warning` buttons verbatim:

```rust
pack_end = &gtk::Button {
    set_icon_name: "dialog-information-symbolic",
    add_css_class: "select-hint",
    set_tooltip_text: Some("Hold Shift to select text — click for details"),
    #[watch]
    set_visible: model.select_hint_visible,
    connect_clicked => Msg::ShowSelectHelp,
},
```

- `select_hint_visible: bool` is transient model state. Nothing is
  written to `state.rs` and nothing persists across restarts.
- `Msg::BareDragHint` sets it true and arms a
  `glib::timeout_add_seconds_local(7, ...)` sending `Msg::HideSelectHint`.
- A second drag inside the window restarts the timer rather than stacking
  timeouts.
- The hint repeats on every bare drag, forever. No dismissal flag, no
  counter, no "seen it" state — it stays useful to a user who forgets
  months later, and the headerbar is clean whenever it is idle.
- Clicking opens a dialog covering both Shift-drag and the new wheel
  scrollback behaviour. Seven seconds is long enough to be a viable click
  target; at three it would not have been.

CSS joins the existing global rules in `lib.rs`:

```css
.select-hint { color: #3584e4; animation: ks-pulse 1s ease-in-out infinite; }
@keyframes ks-pulse { 0%,100% { opacity: 1; } 50% { opacity: 0.35; } }
```

### Testing

The GTK gesture is not unit-testable without a display server, so the
decision is extracted into a free function tested directly, mirroring how
`tmuxctl.rs` keeps logic in pure functions with process spawning at the
edges:

```rust
fn should_hint(distance: f64, mods: gdk::ModifierType, already_fired: bool) -> bool
```

Cases: past threshold without Shift → true; past threshold with Shift →
false; under threshold → false; already fired this drag → false.

Part 1 is covered by `tmux_conf_pins_mouse_on_with_wheel_bindings`, which
asserts `mouse on`, the four wheel bindings, the copy-mode escape hatch,
and that the binds appear after the `unbind -a` lines that would
otherwise erase them.
