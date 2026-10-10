# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

# Instructions for Claude

- Never expose the user's email address in User-Agent strings or other outgoing request headers; use a neutral identifier instead.
- Investigations / codebase exploration must always be run in subagents using the `opus` or `sonnet` model (pass a `model` override to the Agent tool), never on the default/session model.

## Methodology

- Red-green TDD for every behaviour change: write the failing test first, run
  it and watch it fail for the expected reason, then write the minimal code
  that makes it pass, then refactor with the tests green. No implementation
  before its failing test; a test that passes on first run proves nothing and
  needs to be made to fail first.
- Logic that is hard to test under this rule belongs in the pure modules
  (`src/state.rs`, `src/cli.rs`, `src/claude.rs`, `src/tmuxctl.rs`) behind
  parameters that tests can fabricate (a directory, an output string, a
  `/proc` root), not in `src/app.rs`.
- The GTK layer in `src/app.rs` is the one untested exception: keep it to
  wiring, so the behaviour it wires is covered elsewhere.

## Build & verify

- `cargo build` needs system dev headers, not just Rust: GTK 4.18+, libadwaita 1.5+, VTE 0.82+ (Fedora: `gtk4-devel libadwaita-devel vte291-gtk4-devel`). A build failure in the `gtk4`/`vte4` sys crates usually means a missing header, not a code bug.
- CI (`.github/workflows/ci.yml`, Fedora container) runs `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test` on every push. Run them yourself before claiming work is done; where the GTK/VTE headers are missing locally, push and check the CI run instead.
- Tests are unit tests in a `mod tests` at the end of each module: the pure modules (`src/state.rs`, `src/cli.rs`, `src/claude.rs`, `src/tmuxctl.rs`, `src/remote.rs`, `src/remote_worker.rs`, `src/autostart.rs`, `src/android.rs`, `src/browser.rs`, `src/control.rs`, `src/overview/{model,layout,tagger,issues}.rs`) and the pure helpers of `src/app.rs`; `src/overview/canvas.rs` has none. Single test: `cargo test <name>`.

## Architecture invariants

- `src/state.rs` is pure logic — serde structs, persistence, and reconciliation planning. No GTK and no tmux calls belong here; it is what keeps the logic testable.
- `src/tmuxctl.rs` never panics: every fallible path returns `Result`. No `unwrap`/`expect` on tmux interaction.
- `src/app.rs` is the relm4 component holding all GUI state and side effects.
- The app must keep working when tmux is missing or older than 3.2 — it degrades to plain shells without session survival rather than erroring out.
- `src/cli.rs` is pure logic — argv parsing, group resolution, and the decision
  of what an invocation prints and exits with. No GTK, no gio, no tmux, no I/O;
  it is where the CLI's unit tests live. `src/control.rs` holds the gio glue and
  the group snapshot the command-line handler reads.
- A CLI-created tab must not steal focus: no activate, no window raise, no
  active-group change, and no touching the group's panes. `kabelsalat
  browser` and `kabelsalat android` are the two commands that touch a pane —
  only the named group's, brought up hidden unless that group is active —
  under the same no-focus, no-raise, no-switch rules. `android` additionally
  forwards control-socket commands, to the named group's own Android pane
  only. `kabelsalat overview root` and `kabelsalat overview issues` touch no
  pane and never change a group's overview mode, the active group or focus;
  the user flips "Terminals | Overview" in the header bar themselves.
- `src/overview/{model,layout,tagger,issues}.rs` are pure and unit-tested —
  frontmatter parsing and the node graph, layout and tiers, the tagger's
  watermark/significance/prompt/response rules and config, issue derivation
  and the Resolve prompt — over data a test can fabricate (file texts,
  transcript text, a config string). `src/overview/canvas.rs` is GTK wiring
  like `src/app.rs`, which owns the toggle, the per-group mode, the file
  monitors, tagger scheduling and `tmux set-environment`. Tab ↔ node links
  are inferred by the tagger and live in the tab's tmux environment only:
  never in the repository, never in `state.json`.
- `src/autostart.rs` is pure over the facts it is handed (unit text, status
  classification, dialog responses) plus thin `systemctl --user` runners;
  `src/resume.rs` is the `kabelsalat resume` glue and never writes
  `state.json` — the GUI stays the state file's single writer.

## Runtime facts

- State: `$XDG_STATE_HOME/kabelsalat/state.json` (fallback `~/.local/state`), written atomically.
- Private tmux server socket: `$XDG_RUNTIME_DIR/kabelsalat/tmux.sock`, launched under `systemd-run --user --scope --collect`. Full logout survival additionally needs `loginctl enable-linger`.
- Boot resume (opt-in, from the primary menu): `$XDG_CONFIG_HOME/systemd/user/kabelsalat-resume.service`, a oneshot running `kabelsalat resume`, which recreates the claude sessions of saved tabs on the tmux server without a GUI. Its state is read from the system (file, `is-enabled`, lingering), never persisted.
- Overview: a group's root and mode are in `state.json` (`overview_root`, `overview_mode`); the node files are the repository's own, documented in `skills/kabelsalat/overview-format.md`. Tagger config: `$XDG_CONFIG_HOME/kabelsalat/overview.json` (`tagger_command` argv, `roles`), missing file = defaults. Per-tab tags live on the tab's tmux session as `KABELSALAT_LINKS` (JSON: links, activity, topic) and `KABELSALAT_TAG_MARK` (`<uuid>:<offset>` or `title:<title>`); without tmux they are in memory only.

## Licensing

- The app on `main` is MIT.
