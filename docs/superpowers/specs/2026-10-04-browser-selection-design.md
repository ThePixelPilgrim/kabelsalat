# Per-group browser selection and verified browser downloads

Date: 2026-10-04
Status: draft. Decisions marked **assumed** were taken by the author of this
spec and stand until overruled.

Let the user choose, per group, which browser its pane runs: a browser that is
installed on the machine the group runs on, or one from a catalogue of browser
builds that kabelsalat downloads and verifies against SHA-256 sums baked into
its own binary. From the user's standpoint a local and a remote group offer the
same list and the same behaviour.

## Goals

- The group settings dialog gains a browser drop-down. It lists only browsers
  that are actually installed on the group's host, plus the catalogue entries
  that fit that host, each marked "downloaded" or "<size> download".
- Installed browsers are found through desktop standards first (`.desktop`
  files under `$XDG_DATA_DIRS/applications`) and a `$PATH` heuristic second.
- Catalogue browsers are fetched from a location kabelsalat controls and are
  only ever executed after their SHA-256 sum matched the one compiled into
  kabelsalat. There is no fetched manifest: the catalogue is source code.
- Chromium-family browsers keep today's CDP contract untouched. Firefox is a
  first-class entry with its own agent contract.
- The detection and download machinery is one POSIX shell script each, run
  locally through `sh` and remotely through the group's ssh master, so both
  sides produce byte-identical results and are parsed by the same pure code.

## Background

- `Browser::spawn` (`src/browser.rs`) is the only place a browser starts. It
  hardcodes `BROWSER_CANDIDATES = ["chromium", "chromium-browser",
  "google-chrome"]`, a `$PATH` search, Chromium flags, Chromium profile
  seeding and `DevToolsActivePort` discovery. The browser-pane spec
  (`2026-07-24-browser-pane-design.md`) put "any browser other than Chromium"
  and "configurable browser command" out of scope. This spec supersedes both
  lines.
- Remote groups (`SavedGroup.host`) refuse a browser today, in the dialog, in
  `kabelsalat browser` (exit 3) and in `app.rs`. The remote display that will
  show a remote browser is spec 1 and 2 of
  `2026-09-25-remote-display-design.md`; neither exists yet.
- The deployment rule for remote binaries already exists on paper
  (`2026-09-25-remote-tmux-design.md`, "Follow-up"): versioned files under
  `~/.local/share/kabelsalat/bin`, written to a temporary name, checksum
  verified, `chmod 700`, renamed, never overwritten, never deleted by an older
  kabelsalat. This spec applies that rule to browsers, on both sides.
- There is no HTTP client, no hashing crate, no `XDG_DATA_HOME` use and no
  `.desktop` parsing in the code base today. `deny.toml` restricts licences.

## Decisions

### Scope

- **Per group**, stored on `SavedGroup`. No app-wide default. **assumed:** a
  group with no choice behaves exactly as today (first Chromium candidate on
  `$PATH`), so old state files and new groups need no migration.
- **Chromium family and Firefox.** Chromium, ungoogled-chromium, Brave,
  Chromium-based builds the catalogue carries, and Firefox. Nothing else.
- **Android emulators are deferred** together with Xwayland in klamottenkiste.
  The data model leaves room for a third kind; nothing else is built for it.
- **Remote groups get the same drop-down, detection and download.** Launching
  the chosen browser on a remote host stays refused with today's message
  until the remote display lands. **assumed:** that is acceptable for this
  feature, because selection and deployment are what the display will need on
  day one.
- **Flatpak and Snap browsers are excluded** in v1. Their sandboxes do not see
  the pane's Wayland socket nor the per-group profile directory without
  per-packager overrides. The detector recognises them and reports them as
  `unsupported`, so the dialog can say why they are missing instead of hiding
  them silently.
- **Google Chrome** is listed when installed (it is on `BROWSER_CANDIDATES`
  today) but is never in the catalogue: its terms forbid redistribution.

### The catalogue

A `const` table in `src/catalogue.rs`, one entry per browser build:

```rust
pub struct Entry {
    /// Stable id the state file refers to, e.g. "chromium", "firefox".
    /// A group pins the id, never the version: a new kabelsalat release
    /// moves the group to the newest version it carries.
    pub id: &'static str,
    pub family: Family,            // Chromium | Firefox
    pub version: &'static str,     // upstream version string
    pub arch: Arch,                // X86_64 | Aarch64 (matched to `uname -m`)
    pub format: Format,            // AppImage | TarXz { entry: "firefox/firefox" }
    pub url: &'static str,         // under an account the project owns
    pub sha256: &'static str,      // 64 lowercase hex chars
    pub size: u64,                 // bytes, shown before downloading
}
```

- **Hashes live in the binary.** A fetched manifest, signed or not, would add
  a second trust root and a live supply-chain surface. The host serving the
  files is untrusted storage: a swapped file fails verification and is
  deleted. The price is one kabelsalat release per browser security update,
  which is accepted and documented in the README.
- **Hosting.** GitHub Releases assets of a repository the project owns. Git
  refuses files over 100 MB and GitHub Pages serves no LFS, so Pages cannot
  carry a Chromium-family AppImage (150 to 200 MB). Releases allow 2 GB per
  asset under the same account, which is the same trust model.
- **Formats.** AppImage for the Chromium family. Firefox ships no AppImage;
  its official portable form is Mozilla's `tar.xz`, so the catalogue carries a
  `TarXz` format with the entry point's relative path. An AppImage is run with
  `--appimage-extract-and-run` when `/dev/fuse` is unavailable (containers,
  many servers).
- **Several versions may coexist** in the table (same id, different version)
  during a transition; the newest version for the host's arch wins. A table
  with duplicate `(id, version, arch)` or a malformed hash fails a unit test.
- **Filling the table.** `scripts/catalogue-entry.sh <url>` downloads a file,
  prints its size and SHA-256 and emits the Rust literal to paste. Hashes are
  never typed by hand.

### Installed-browser detection

`scripts/detect-browsers.sh`, embedded with `include_str!` and fed over stdin
to `sh` locally and to `ssh <host> sh` remotely (the mechanism
`upload_conf_argv` already uses). It prints one tab-separated line per finding:

```
family<TAB>source<TAB>exec<TAB>name<TAB>version
```

- `source` is `desktop:<desktop-id>`, `path:<binary>`, `flatpak:<app-id>` or
  `snap:<name>`.
- Desktop standard: for each `$XDG_DATA_DIRS` (default
  `/usr/local/share:/usr/share`) and `$XDG_DATA_HOME/applications`, the known
  desktop ids (`chromium.desktop`, `org.chromium.Chromium.desktop`,
  `google-chrome.desktop`, `brave-browser.desktop`, `firefox.desktop`,
  `org.mozilla.firefox.desktop` and their distro variants) are read for
  `Exec=`; the first word is resolved on `$PATH`. `TryExec=` is honoured.
- Heuristic: the known binary names on `$PATH`, for hosts without any
  `applications` directory.
- Flatpak and Snap: `flatpak list --app --columns=application` and
  `/snap/bin/<name>`, reported with their own `source` so the parser marks
  them `unsupported`.
- `version` comes from `<exec> --version`, run with a 5 s timeout and a
  stripped environment, or is empty.
- The script also prints one header line: `host<TAB><uname -m><TAB>fuse=<0|1>`,
  which selects catalogue entries and the AppImage launch mode.

The parser, `apps::parse_detection(&str) -> Detected`, is pure and
table-tested in `src/apps.rs`. The dialog's list is
`apps::choices(&Detected, &[Entry], &InstalledCatalogue) -> Vec<Choice>`, also
pure: installed browsers first, catalogue entries for the host's arch second,
each with a display name, a stable `AppChoice` value and a state
(`Ready`, `Download(size)`, `Unsupported(reason)`).

### Download and verification

`scripts/fetch-browser.sh <url> <sha256> <dest>`, run the same two ways. It:

1. refuses to run if `<dest>` exists (never overwrite);
2. downloads with `curl -fL --proto =https` (or `wget` as fallback) into
   `<dest>.part-<pid>`;
3. verifies with `sha256sum -c` against the given sum; on mismatch deletes the
   part file and exits 3;
4. `chmod 700`, renames to `<dest>`, exits 0.

A file at `<dest>` therefore exists only if it was verified. `<dest>` is
`${XDG_DATA_HOME:-$HOME/.local/share}/kabelsalat/bin/<id>-<version>-<arch>.<ext>`
on both sides. Tarballs are extracted next to the file into
`<id>-<version>-<arch>/` after verification, also by the script.

- **assumed:** the host downloads for itself. Streaming the file through the
  ssh master would share the terminals' connection with 200 MB of traffic. A
  host without internet is reported as a plain download failure in v1.
- Progress is the part file's size, polled once a second and shown in the
  dialog row; the dialog stays usable meanwhile. Cancel kills the script's
  process group and removes the part file.
- Nothing under `bin/` is ever deleted by kabelsalat. The profile sweep in
  `browser.rs` is unaffected: it only touches `<state_dir>/browsers/`.
- The User-Agent is `kabelsalat/<version>` and nothing else, per CLAUDE.md.
- kabelsalat itself speaks no HTTP: `curl` is required on the side that
  downloads, as `ssh` and `tmux` already are, and its absence is reported.

### Persistence

`SavedGroup` gains one field:

```rust
/// The browser this group's pane runs. `None` = today's behaviour (first
/// Chromium candidate on $PATH). Stored as text so that an id this version
/// does not know (a downgrade, a hand edit) degrades to `None` at use
/// instead of making the whole state file unreadable.
#[serde(default)]
pub app: Option<String>,
```

The text is `catalogue:<id>` or `installed:<family>:<exec>`; `apps::parse_choice`
turns it into `Option<AppChoice>`. A missing catalogue id or a vanished
executable is reported in the dialog and falls back to today's behaviour at
launch; it never becomes a launch error. `#[serde(default)]` is the
migration, as for `default_url`.

The last detection result per host is cached in `SavedState` under
`detected_apps: BTreeMap<String, String>` (host key `""` for local, the raw
script output as value), so a remote group's dialog can show its list while
the host is offline, marked "last seen". It is overwritten on every
successful scan and is never required to be present.

### Launch

`Browser::spawn` takes a resolved `Launch { family, exec, extract_and_run }`
instead of searching `$PATH`. The launch table is pure
(`browser::launch_argv`) and unit-tested:

| family | argv | profile | agent endpoint |
|---|---|---|---|
| Chromium | today's flags, `--user-data-dir=<profile>` | `<state_dir>/browsers/<uuid>` as today | `DevToolsActivePort` → `KABELSALAT_CDP`, `PLAYWRIGHT_MCP_CDP_ENDPOINT` |
| Firefox | `--new-instance --no-remote --profile <profile> --remote-debugging-port <port>` with `MOZ_ENABLE_WAYLAND=1` | `<state_dir>/browsers/<uuid>/firefox` | WebDriver BiDi on `<port>` → `KABELSALAT_BIDI` |

- Firefox has no `DevToolsActivePort`; the port is chosen by binding and
  releasing a loopback socket. Stock Firefox has no CDP, so the two CDP
  variables are not exported for Firefox groups and the README says so.
- A profile directory belongs to one family. Switching a group's family
  deletes its profile after a confirmation; switching between Chromium
  builds keeps it (newer Chromium opens older profiles; the reverse is
  refused in the dialog when the catalogue version is older than the
  installed one). A running browser is never restarted by a settings change.
- `default_url` applies to both families on a fresh profile, as today.

### Dialog

`show_group_settings` gains an `adw::ComboRow` "Browser" above "Browser
default URL". Rows read, for example, `Chromium 141 (installed)`,
`Firefox 144 (installed)`, `Chromium 141 — 182 MB download`, `Brave —
unsupported: Flatpak`. Choosing a download entry starts the download on
Apply, not on selection. The row shows the cached list immediately and
refreshes when the scan returns.

## Error handling

| Failure | Response |
|---|---|
| no detection output (no `sh`, host offline) | cached list or empty list, note "could not scan"; Apply still allowed |
| `curl` missing on the downloading side | row disabled with that reason |
| hash mismatch | part file deleted, toast "verification failed", entry stays `Download` |
| disk full, network error | part file deleted, toast with the script's stderr tail |
| stored `app` unknown or executable gone | dialog note, launch falls back to today's `$PATH` search |
| catalogue arch does not match the host | entry not listed |

## Testing

- `src/catalogue.rs`: unique `(id, version, arch)`, every `sha256` is 64 hex
  chars, every `url` is `https://` under the project's account, `newest_for`
  picks the highest version per id and arch.
- `src/apps.rs`: table-driven `parse_detection` over fabricated script
  output (desktop, path, flatpak, snap, missing version, garbage lines, header
  only), `choices` ordering and states, `parse_choice` round trips and
  unknown values.
- `src/browser.rs`: `launch_argv` per family, extract-and-run selection from
  the `fuse` flag, profile path per family.
- `src/state.rs`: old group without `app` loads; round trip with `Some`;
  `detected_apps` absent loads.
- The scripts: a shell test under `scripts/tests/` runs `detect-browsers.sh`
  against a fabricated `XDG_DATA_DIRS` tree and `fetch-browser.sh` against a
  local `file://` URL with a right and a wrong hash, in CI.
- Launching real browsers and the remote path need a display and a host and
  stay manual, as for the rest of the pane.

## Out of scope

- Running the chosen browser on a remote host (remote display, spec 2).
- Android emulators and anything needing Xwayland.
- Flatpak and Snap launch support.
- A fetched or signed manifest; automatic browser updates independent of
  kabelsalat releases.
- Streaming a download through the ssh connection.
- A CLI surface for choosing a browser.

## Open

- Which builds to re-host first. Candidates with redistributable licences:
  Chromium (BSD), ungoogled-chromium's AppImage, Brave (MPL-2.0) and
  Mozilla's Firefox tarball. The table starts empty and grows as files are
  published and hashed.
