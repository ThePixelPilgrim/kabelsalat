# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

# Instructions for Claude

- Never expose the user's email address in User-Agent strings or other outgoing request headers; use a neutral identifier instead.
- Investigations / codebase exploration must always be run in subagents using the `opus` or `sonnet` model (pass a `model` override to the Agent tool), never on the default/session model.

## Build & verify

- `cargo build` needs system dev headers, not just Rust: GTK 4.18+, libadwaita 1.5+, VTE 0.82+ (Fedora: `gtk4-devel libadwaita-devel vte291-gtk4-devel`). A build failure in the `gtk4`/`vte4` sys crates usually means a missing header, not a code bug.
- There is no CI. Run `cargo fmt`, `cargo clippy` and `cargo test` yourself before claiming work is done.
- Tests are unit tests inside `src/state.rs` and `src/tmuxctl.rs`; single test: `cargo test <name>`.

## Architecture invariants

- `src/state.rs` is pure logic — serde structs, persistence, and reconciliation planning. No GTK and no tmux calls belong here; it is what keeps the logic testable.
- `src/tmuxctl.rs` never panics: every fallible path returns `Result`. No `unwrap`/`expect` on tmux interaction.
- `src/app.rs` is the relm4 component holding all GUI state and side effects.
- The app must keep working when tmux is missing or older than 3.2 — it degrades to plain shells without session survival rather than erroring out.

## Runtime facts

- State: `$XDG_STATE_HOME/kabelsalat/state.json` (fallback `~/.local/state`), written atomically.
- Private tmux server socket: `$XDG_RUNTIME_DIR/kabelsalat/tmux.sock`, launched under `systemd-run --user --scope --collect`. Full logout survival additionally needs `loginctl enable-linger`.

## Licensing

- The app on `main` is MIT.
