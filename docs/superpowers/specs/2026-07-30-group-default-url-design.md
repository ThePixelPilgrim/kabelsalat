# Per-group browser default URL

Give each tab group a configured URL that its browser opens on when one is
launched fresh, set from a group settings dialog.

## Goals

- A group can carry a default URL. A freshly launched browser in that group
  opens it instead of Chromium's own start page.
- A launch that resumes an existing Chromium profile is untouched: it restores
  its session, as today.
- The URL is set through the dialog that already names a group, so it can be set
  before the group has ever had a browser.
- A value that would reach Chromium as a flag rather than a URL can never be
  stored, and can never be passed even if it is already in `state.json`.

## Background

The shipped browser-pane design
(`docs/superpowers/specs/2026-07-24-browser-pane-design.md`) put URLs out of
scope on purpose — "kabelsalat-side control of browser navigation, URLs, or
tabs" — and persists none, because "Chromium owns that". The roadmap
(`docs/superpowers/plans/2026-07-23-embedded-browser-groups.md:89-90`) then
files a default URL under Phase 4 as a *global* setting, next to Chromium binary
path and port range.

This design deliberately narrows that opening rather than reversing it. The
default URL is a **launch argument**, not navigation state: kabelsalat writes it
once at spawn and never reads, tracks, or updates where the browser goes
afterwards. The out-of-scope line on navigation control holds.

It is also distinct from the roadmap's other URL mention, "restore browser
visibility/pane position/URL per group across restarts". That would be a
*remembered* URL, which drifts as the user browses. This is a *configured*
default, which is stable. Conflating them produces the obvious bug: set a
default, browse elsewhere, restart, and never see the default again.

Two existing facts carry most of the implementation:

1. **`Browser::spawn` is the only place a browser process starts**
   (`src/browser.rs:144`; the module doc at `src/browser.rs:4` states the
   invariant). It has exactly two callers, so one signature change covers every
   launch path.
2. **Fresh-profile detection already exists.** `spawn` computes
   `profile_existed` before `create_dir_all` (`src/browser.rs:170`) so that
   `seed_profile` only seeds a profile it created. That same value is exactly the
   condition under which a default URL should apply. No new state is needed.

## Which launches use it

Chromium is launched with `--restore-last-session` into a persistent per-group
profile, so "a new browser is launched" covers four situations. The default URL
applies only where there is no session to restore:

| Launch | Argument |
|---|---|
| group's first `Alt-2` | `chromium … <default-url>` |
| `Alt-2` after **Close browser** (profile deleted) | `chromium … <default-url>` |
| relaunch after Chromium exited on its own (profile kept) | `chromium … --restore-last-session` |
| restore at app startup (profile kept) | `chromium … --restore-last-session` |

Passing it on every launch instead would stack another copy of the default tab
onto the restored session after every crash and every restart. Dropping
`--restore-last-session` for groups with a default URL would trade away crash
recovery of open tabs. Gating on a fresh profile keeps both properties.

## Architecture

### `src/state.rs`

`SavedGroup` gains one field beside `browser_split` (`src/state.rs:43-44`):

```rust
/// Where a freshly launched browser for this group starts. `None` = whatever
/// Chromium opens on its own. Only consulted for a launch into a *fresh*
/// profile; a launch that resumes an existing profile restores that session
/// instead, so a crash or a restart never stacks another copy of this tab.
#[serde(default)]
pub default_url: Option<String>,
```

`SavedGroup::new` (`src/state.rs:52-61`) defaults it to `None`.

`#[serde(default)]` *is* the migration mechanism here — there is no version
field and no migration table, matching `browser_open`, `browser_split`, and
`linger_warning_dismissed`. `Option<String>` is preferred over the
empty-string-means-unset convention that `name` uses, because normalization
already yields an `Option` and `None` is what an older `state.json` deserializes
to anyway.

`state.rs` stores the string and nothing else. Validation lives in
`browser.rs`, with the launch code that cares about it; `state.rs` stays pure
persistence and reconciliation logic.

### `src/browser.rs`

`spawn` grows a parameter:

```rust
pub fn spawn(
    group_uuid: &str,
    state_dir: &Path,
    default_url: Option<&str>,
) -> Result<Self, BrowserError>
```

After the existing flags (`src/browser.rs:183-206`):

```rust
// Only a profile this call created: an existing one has a session that
// `--restore-last-session` brings back, and the group's default URL would
// pile a duplicate tab on top of it every crash and every restart.
// Re-validated here rather than trusted: `state.json` is user-editable, and
// a value beginning with `-` would reach Chromium as a *flag*, not a URL.
if !profile_existed
    && let Ok(Some(url)) = normalize_default_url(default_url.unwrap_or_default())
{
    command.arg(url);
}
```

`--restore-last-session` stays unconditional. On a fresh profile there is
nothing to restore, so it cannot conflict with the URL.

The gate lives inside `spawn`, not at its call sites, because `app.rs` cannot
know whether a profile exists without recomputing `profile_dir()` and stat-ing
it — duplicating private path logic that could then drift, and re-opening the
boundary the module doc closes. Keeping it here puts the rule on the same lines
as the fact it depends on.

#### Validation

A new pure helper joins `resolve_binary` and `profile_dir` as logic testable
without a display:

```rust
pub fn normalize_default_url(input: &str) -> Result<Option<String>, DefaultUrlError>
```

| Input | Result |
|---|---|
| `""` or whitespace only | `Ok(None)` — this is how the URL is cleared |
| begins with `-` | `Err(LooksLikeFlag)` |
| `scheme://…` where scheme is `http`, `https`, `file` | `Ok(Some(…))`, scheme lowercased |
| `scheme://…` with any other scheme | `Err(UnsupportedScheme)` |
| no scheme | `http://` prepended, then re-checked |
| interior whitespace | `Err(NotAUrl)` |
| `http`/`https` with an empty host | `Err(NotAUrl)` |
| `file://` with an empty path | `Err(NotAUrl)` |

The host requirement is per scheme, because `file:///home/c/notes.html` has no
host by construction — for `file://` the path carries the meaning, so that is
what must be non-empty.

So `localhost:3000` becomes `http://localhost:3000` and
`file:///home/c/notes.html` is kept as typed, while `--disable-web-security`,
`ftp://host`, and `hello world` are refused.

The scheme prefix is matched case-insensitively and stored lowercased. IPv6
literals and userinfo are accepted as typed with no special handling — a
deliberate limit of a hand-rolled parser, documented rather than silently
mangled.

This is hand-rolled rather than delegated to the `url` crate: the accepted
grammar is small enough to cover exhaustively with table-driven tests, and it
keeps the dependency list at seven.

`DefaultUrlError` implements `Display`; that text is what the dialog shows.

### `src/app.rs`

`Group` (`src/app.rs:108-125`) gains `default_url: Option<String>`, threaded
through the same three places `browser_split` already flows:

- `create_group()` (`src/app.rs:853-867`) — `None`
- the restore loop in `restore_or_fresh()` (`src/app.rs:925-940`) — from `SavedGroup`
- `save_state()` (`src/app.rs:1006-1026`) — back into `SavedGroup`

Both spawn sites pass a cloned `default_url`, mirroring the existing `uuid`
clone so no borrow of `self.groups` is held across the call:

- `toggle_browser()` (`src/app.rs:1224`)
- `restore_browser()` (`src/app.rs:1382`)

#### Group settings dialog

`show_rename_dialog()` (`src/app.rs:2295-2322`) becomes
`show_group_settings_dialog()`, titled **Group Settings**. Its `extra_child` is
an `adw::PreferencesGroup` holding two `adw::EntryRow`s — Name, and Browser
default URL — plus a `gtk::Label` with the `error` CSS class, hidden while the
input is valid.

```
┌─ Group Settings ─────────────────┐
│  Name                            │
│  ┌────────────────────────────┐  │
│  │ dev                        │  │
│  └────────────────────────────┘  │
│  Browser default URL             │
│  ┌────────────────────────────┐  │
│  │ http://localhost:3000      │  │
│  └────────────────────────────┘  │
│           [ Cancel ] [ Apply ]   │
└──────────────────────────────────┘
```

`connect_changed` on the URL row runs `normalize_default_url` and calls
`set_response_enabled("apply", …)`, so **Apply is not pressable while the URL is
invalid**. This is why validation is live rather than on-submit:
`adw::AlertDialog` dismisses on any response, so an error raised at submit time
would have nowhere to live. Gating the response keeps the failure inside the
dialog.

Apply sends one message, so name and URL land atomically — one `save_state()`,
one sidebar rebuild:

```rust
Msg::ApplyGroupSettings { id: usize, name: String, default_url: Option<String> }
```

This replaces `Msg::RenameGroup` (`src/app.rs:748-753`), and
`Msg::RenameDialog` (`src/app.rs:219`, `src/app.rs:747`) becomes
`Msg::GroupSettingsDialog`. The URL carried is already normalized, so `app.rs`
stores what the validator produced. The handler keeps `Msg::RenameGroup`'s
existing `name.trim()` (`src/app.rs:750`) — an empty name still means an unnamed
group with no header.

The `error` style class is stock Adwaita, so the app's CSS provider needs no
addition for it, unlike the custom `tmux-warning` / `browser-hidden` classes.

The `SHORTCUTS` entry for `Ctrl+Shift+R` (`src/app.rs:33-37`) changes its label
from "Name the active group" to "Group settings". That table also generates the
F1 help, so the help text follows from the same edit.

## Behaviour

Editing the URL never touches a **running** browser — no navigation, no reload.
A group that already has a browser therefore picks up a new default only after
**Close browser** (which deletes the profile) and a fresh `Alt-2`.

This is the feature's one "why didn't that apply?" moment. It follows directly
from the fresh-profile rule and from navigation control staying out of scope, so
it is documented in the README rather than worked around.

A group with no default URL behaves exactly as it does today.

## Error handling

| Failure | Response |
|---|---|
| invalid text in the dialog | Apply disabled, inline error, nothing saved |
| stored value invalid (hand-edited `state.json`, or written by another tool) | dropped at spawn; the browser launches without it |
| URL valid but unreachable | Chromium's own error page; kabelsalat does nothing |
| any existing browser launch failure | unchanged (`Compositor`, `NoBinary`, `Spawn`, `Profile`) |

A bad stored URL costs a start page, never a browser: it is dropped, not turned
into a launch error. `BrowserError` gains no variant.

## Testing

`browser.rs` unit tests — table-driven over `normalize_default_url`, covering
every row of the validation table: a leading dash, interior whitespace, an
uppercase scheme, an unsupported scheme, a bare host with a port, empty input,
and both sides of the per-scheme rule (`http://` with an empty host rejected,
`file:///…` with its empty host accepted, `file://` with an empty path
rejected). All pure; no display required.

`state.rs` unit tests — `old_group_without_default_url_loads`, mirroring
`old_group_without_browser_fields_loads` (`src/state.rs:530-546`), and a
round-trip of a `SavedGroup` carrying `Some(…)`.

The Chromium argument itself needs a display and a DRM render node, so that the
URL actually opens is left to manual verification, consistent with how the rest
of the browser pane is tested.

## Out of scope

- A **global** default URL. That stays roadmap Phase 4, alongside Chromium
  binary path and port range.
- Navigating or reloading a live browser from kabelsalat.
- Per-tab URLs, or more than one URL per group.
- Any CLI surface for group configuration. `cli.rs` exposes read-only `groups`
  and tab-spawning `run`; nothing reads or writes group config from the CLI, and
  this feature does not need to invent that plumbing.
- Remembering the last-visited URL per group. That is a different feature with
  different semantics, noted in Background.
