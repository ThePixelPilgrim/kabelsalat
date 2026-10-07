---
name: kabelsalat
description: Use when a command should run in a visible, persistent terminal the user can watch and interact with — a dev server, a long build, or an interactive claude session — rather than as a captured subprocess; not for commands whose output you need to capture or read back. Also use to read, screenshot or drive the page in the user's embedded browser pane (CDP/Playwright), or Firefox for Android in the Android pane (Waydroid, WebDriver BiDi over adb). All of these live in the named groups of the user's running kabelsalat terminal.
---

# Launching terminals in kabelsalat

kabelsalat is the user's terminal app. Its tabs are organised into named
groups and backed by tmux sessions, so a tab survives the GUI restarting.

Use this when a command should keep running and stay visible after you are
done — a dev server, a watcher, a long build, another `claude` session. Do not
use it for commands whose output you need to read: those still belong in your
normal shell tool, because you cannot read a kabelsalat tab's output.

## Listing groups

    kabelsalat groups

One line per group, tab-separated: `uuid`, `name`, `tab count`. The name is
empty for unnamed groups. Run this first — you cannot guess group names.

    a1b2c3d4-...	erhebimus	3
    e5f6a7b8-...	web	1
    c9d0e1f2-...		2

## Running a command

    kabelsalat run --group <name|uuid> [--create] [--cwd DIR] -- COMMAND [ARGS...]

Everything after `--` is the command, taken literally. The `--` is required.

    kabelsalat run -g erhebimus -- claude
    kabelsalat run -g web -- npm run dev
    kabelsalat run -g web --cwd ~/Projects/textimus -- cargo watch -x test

For shell syntax — pipes, `&&`, globbing — invoke a shell explicitly:

    kabelsalat run -g web -- bash -lc 'cd frontend && npm run dev'

`--cwd` defaults to your current working directory. On success the new tab's
uuid is printed.

## Targeting a group

`--group` takes a uuid or an exact, case-sensitive name. A uuid always wins.
If several groups share a name, the command fails and prints their uuids —
retry with one of those. Unnamed groups can only be targeted by uuid.

## Creating a group

Add `--create` when the user asks for a new group, or names a group that
`kabelsalat groups` does not list:

    kabelsalat run -g newproj --create -- claude

If `--group` already resolves to a unique existing group, `--create` is a
no-op and that group is reused. Otherwise a new group is created named
exactly `<selector>` — used verbatim, case-sensitive, no fuzzy matching — and
the command's tab is its first tab. An ambiguous selector still fails with
exit 3; `--create` does not resolve ambiguity.

The new group appears in the sidebar **without being activated** — same
no-steal-focus rule as any new tab. Always tell the user which group you
created, or they will not notice it.

## Renaming a group

    kabelsalat rename <name|uuid> <new-name>

Renames an existing group. `<name|uuid>` resolves exactly like `--group`
above. The command refuses to create a duplicate: if another group already
has `<new-name>`, it fails with exit 3 and does not rename anything. If
`<new-name>` is already the target's current name, it succeeds without doing
anything.

## Exit codes

| Code | Meaning | What to do |
|------|---------|------------|
| 0 | Tab created | Tell the user which group it went to |
| 1 | kabelsalat is not running | Report this to the user and stop. Do not retry, and do not try to start it — that is theirs to do |
| 2 | Usage error | Fix the invocation; check `--` is present |
| 3 | Group not found, ambiguous, (rename) name already in use, (browser, android) a remote group, (android) Android owned by another group or not open, or its pane did not answer | Re-run `kabelsalat groups` and retry with a uuid, or pick a different name; for Android, tell the user which group has it |
| 4 | (android subcommand) the pane refused the request | Read the message; fix the arguments |

## After launching

The new tab is created **without stealing focus** — the window is not raised
and the user's current tab keeps their keystrokes. So always tell the user
which group the tab appeared in, or they will not notice it.

You cannot read the tab's output, send input to it, or close it. If the
command fails (non-zero exit), the tab stays visible showing its exit status
and can be restarted. If it exits successfully (status 0), its tab closes on
its own — do not tell the user to go look for it.

## Driving the group's browser (CDP)

Each group's embedded browser exposes a CDP endpoint. The workflow is
co-browsing: the user steers the visible browser; you attach to inspect, and
act only when asked. An attached client has full power over the profile
(cookies, logins, script execution) — treat it as the user's browser, because
it is.

The endpoint is **unauthenticated**: any process running as any user on
this machine can connect to the same browser. Do not treat it as a private
channel.

Fetch the endpoint and group identity once per task (a running shell's
inherited environment may be stale; the tmux table is live):

    tmux show-environment KABELSALAT_CDP     # KABELSALAT_CDP=http://127.0.0.1:<port>
    tmux show-environment KABELSALAT_GROUP   # KABELSALAT_GROUP=<group-uuid>

A leading `-` in the output means the variable is unset (no browser running).
If `show-environment` reports no `KABELSALAT_GROUP` at all, the tab predates
the feature or lost its stamp — say so to the user instead of guessing a
group.

### Bringing the browser up

The browser is transient: it may never have been opened, and a crashed one
is not restarted by anything. When `KABELSALAT_CDP` is unset, ask for it:

    kabelsalat browser            # the group is taken from your KABELSALAT_GROUP
    kabelsalat browser -g <name|uuid>

If the browser is already up, the endpoint is printed and you are done.
Otherwise nothing is printed and the pane is being brought up — hidden
unless the user is looking at that group, and without taking their focus.
Poll `tmux show-environment KABELSALAT_CDP` about once a second for up to
ten seconds until it is set. Exit 1 means kabelsalat's window is not running,
and without it there can be no browser: tell the user and stop. Exit 3
means a remote group, whose browser pane does not exist.

Drive the browser from a script — Python or JavaScript against stock
Playwright, connecting to `KABELSALAT_CDP`. **Do not use a Playwright MCP
server**, even if one is configured and already pointed at this endpoint: a
script runs many steps in one process, where MCP makes each step a separate
round trip. The script is scratch tooling for you, not something to present
to the user.

(`PLAYWRIGHT_MCP_CDP_ENDPOINT` holds the same value. It exists so that a
session which does use an MCP server at least points at the right browser —
not as a recommendation.)

Start every script with the same preamble — re-resolve the environment and
assert the group you pinned on first fetch. Tabs can be moved between groups;
the assert is what turns a silently-wrong browser into a loud, recoverable
failure:

    import subprocess
    from playwright.sync_api import sync_playwright

    PINNED_GROUP = "<uuid from your first fetch>"

    def tmux_env():
        out = subprocess.run(["tmux", "show-environment"],
                             capture_output=True, text=True).stdout
        return dict(line.split("=", 1) for line in out.splitlines()
                    if "=" in line and not line.startswith("-"))

    env = tmux_env()
    assert env.get("KABELSALAT_GROUP") == PINNED_GROUP, \
        f"tab moved (now in {env.get('KABELSALAT_GROUP', 'nowhere')}) — re-orient"
    endpoint = env["KABELSALAT_CDP"]      # KeyError = no browser: also loud

    with sync_playwright() as p:
        browser = p.chromium.connect_over_cdp(endpoint)
        pages = [pg for ctx in browser.contexts for pg in ctx.pages]
        page = next((pg for pg in pages
                     if pg.evaluate("document.visibilityState") == "visible"),
                    pages[0] if pages else None)
        # page is the tab the user is looking at: read, evaluate, screenshot.

A failed `connect_over_cdp` (connection refused) means the browser restarted:
re-run the fetch and retry.

## Driving Firefox for Android (Waydroid)

A group can own the machine's one Android: Waydroid running in a pane next
to the group's browser, shown as an "Android" tab. You drive Firefox for
Android (Fenix) in it with geckodriver and WebDriver BiDi over adb.
kabelsalat only starts Android; adb authorisation, Fenix and geckodriver are
yours to set up, as below. The co-browsing rule is the browser's: the user
watches the pane; act when asked.

### How the Android pane fits with the system

- **The pane is a nested compositor.** kabelsalat hosts a nested Wayland
  compositor (klamottenkiste) per pane; its Wayland socket lives under
  `$XDG_RUNTIME_DIR`. Each pane also has a control socket, a per-pane path
  under the system temp dir (not `$XDG_RUNTIME_DIR`), restricted to your
  user. It accepts one-line commands (`screenshot`, `click`, `type`, `key`,
  `resize`) that reach only the client hosted in the pane, never the host
  seat. `kabelsalat android …` subcommands forward to that socket. Read its
  path from `KABELSALAT_ANDROID_CTL` only; never guess it.
- **Waydroid is split in two.** A root-owned container service
  (`waydroid-container.service`, started once by the admin after
  `waydroid init`) and a user session (`waydroid session start`) that binds to
  exactly one Wayland display when it starts. kabelsalat starts and stops the
  session with `WAYLAND_DISPLAY` pointed at the pane's socket; the container
  keeps running. Android renders on the host GPU via dmabuf; no software
  rendering props are needed.
- **Hence one Android pane per machine**, owned by one group at a time.
  `kabelsalat android` from another group exits 3 naming the owner. Moving it
  means Stop, then open it elsewhere (about 20 s to boot). A Waydroid session
  started outside kabelsalat (e.g. from a terminal) blocks the pane with a
  notice; it is stopped with `waydroid session stop`.
- **adb goes over the container network**, to the serial in
  `KABELSALAT_ANDROID_ADB` (`192.168.240.112:5555` by default); the first
  connect needs the "Allow USB debugging" tap. Fenix, geckodriver and the BiDi
  session are entirely yours; kabelsalat never installs or launches them.
- **Lifecycle.** The two variables appear in the group's tmux sessions once
  Android has booted and disappear on Stop, tab close, death or app exit. The
  owner is remembered in `state.json`: after a kabelsalat restart the pane
  comes back hidden for that group, and the variables reappear once it has
  booted. On teardown the pane's compositor and session are killed as one
  process group, so nothing of kabelsalat's survives a Stop; a crash of
  kabelsalat can leave the session running, and the "already running" notice
  then appears at the next start.
- **Input.** The pane's seat has keyboard and pointer only, no touch: Android
  sees a mouse. Prefer `adb shell input` for input inside Android (step 7).

The blocks below assume one shell: variables set in one step (`CTL`, `ADB`,
`D`, `PORT`, `V`) are used in later ones. In a fresh shell, re-run the
`CTL=`/`ADB=` lines of step 1 first.

### 1. Bring Android up

Pin `KABELSALAT_GROUP` as for the browser, then:

    tmux show-environment KABELSALAT_ANDROID_CTL   # the pane's control socket
    tmux show-environment KABELSALAT_ANDROID_ADB   # adb serial, e.g. 192.168.240.112:5555

A leading `-`, or "unknown variable", means no booted Android in your group.
Ask for one:

    kabelsalat android               # the group is taken from your KABELSALAT_GROUP
    kabelsalat android -g <name|uuid>

If Android is already up, it prints `ctl=<socket>` and `adb=<serial>` and you
are done. Otherwise stdout stays empty and Android boots — hidden unless the
user is looking at that group, without taking their focus. Poll
`tmux show-environment KABELSALAT_ANDROID_CTL` every two seconds for up to
90 seconds. Exit 3 means another group owns Android (the message names it;
there is one per machine — tell the user, do not stop it), a remote group,
or an unknown group. Exit 1: kabelsalat's window is not running; stop.

If the variables never appear, the user got a notice in the window
(Waydroid missing, not initialised, or a session already running elsewhere).
Ask them what it said.

Once polling succeeds, load both values from the live tmux table. Do not
read `$KABELSALAT_ANDROID_*` from your own environment: your shell started
before Android booted, so `tmux set-environment` never reached it and the
variables there are empty or stale.

    CTL=$(tmux show-environment KABELSALAT_ANDROID_CTL | cut -d= -f2-)
    ADB=$(tmux show-environment KABELSALAT_ANDROID_ADB | cut -d= -f2-)

`kabelsalat android` prints the same two values as its `ctl=` and `adb=`
lines once Android is up.

### 2. Authorise adb

    adb connect "$ADB"
    adb -s "$ADB" get-state      # "device" once authorised

The first connection comes up `unauthorized`, and Android shows an "Allow USB
debugging?" dialog in the pane. adb cannot answer it, so use the pane:

    kabelsalat android screenshot /tmp/android.png   # look at the dialog
    kabelsalat android tap X Y                        # tick "Always allow", then tap "Allow"

Coordinates are pixels of that screenshot. The first click after the pointer
enters the pane can open the notification shade instead (step 7): take a
screenshot after each tap, and if the shade is open, press
`kabelsalat android key escape` and send the tap again. Then
`adb disconnect "$ADB"`, connect again, and re-check `get-state`.
"Always allow" makes this a one-time step.

### 3. Install Fenix (once)

    adb -s "$ADB" shell pm path org.mozilla.firefox

prints `package:…` when Fenix is installed. If it is not, download the
official x86_64 APK over HTTPS from Mozilla's archive (archive.mozilla.org is
the provenance) into a fresh, empty directory and install it. `adb install`
only verifies that the APK's signature is valid, not who signed it:

    V=$(curl -s https://archive.mozilla.org/pub/fenix/releases/ \
        | grep -o 'releases/[0-9][0-9.]*/"' | sed 's#releases/##; s#/"##' | sort -V | tail -1)
    D=$(mktemp -d)
    curl -fL -o "$D/fenix.apk" \
      "https://archive.mozilla.org/pub/fenix/releases/$V/android/fenix-$V-android-x86_64/fenix-$V.multi.android-x86_64.apk"
    adb -s "$ADB" install "$D/fenix.apk"

### 4. Start geckodriver

Use `geckodriver` from `PATH` if there is one. Otherwise download the linux64
release into a fresh, empty directory and check its sha256 against the
digest GitHub publishes for the asset before running anything from it:

    TAG=$(curl -s https://api.github.com/repos/mozilla/geckodriver/releases/latest \
          | python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])')
    ASSET="geckodriver-$TAG-linux64.tar.gz"
    WANT=$(curl -s "https://api.github.com/repos/mozilla/geckodriver/releases/tags/$TAG" \
          | python3 -c "import json,sys; print(next(a['digest'] for a in json.load(sys.stdin)['assets'] if a['name'] == '$ASSET').removeprefix('sha256:'))")
    D=$(mktemp -d)
    curl -fL -o "$D/$ASSET" "https://github.com/mozilla/geckodriver/releases/download/$TAG/$ASSET"
    echo "$WANT  $D/$ASSET" | sha256sum -c - && tar -xzf "$D/$ASSET" -C "$D"

If the check fails, stop and tell the user. Run geckodriver on a free port —
never assume 4444, it is often taken — as a background process of yours, not
in a kabelsalat tab (you need its port, not its output):

    PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
    "$D/geckodriver" --android-storage internal --port "$PORT" &

`--android-storage internal` is required on Waydroid: its `/storage/emulated`
is a bind mount geckodriver cannot create its directories on. The flag is
deprecated upstream and still works.

### 5. A BiDi session from Python

A raw websocket client keeps this to one small dependency
(`pip install websockets`). Same preamble as for the browser — re-resolve
the environment and assert the group you pinned:

    import base64, json, subprocess, urllib.error, urllib.request
    from websockets.sync.client import connect

    PINNED_GROUP = "<uuid from your first fetch>"
    GECKO = "http://127.0.0.1:<PORT geckodriver listens on>"

    def tmux_env():
        out = subprocess.run(["tmux", "show-environment"],
                             capture_output=True, text=True).stdout
        return dict(line.split("=", 1) for line in out.splitlines()
                    if "=" in line and not line.startswith("-"))

    env = tmux_env()
    assert env.get("KABELSALAT_GROUP") == PINNED_GROUP, \
        f"tab moved (now in {env.get('KABELSALAT_GROUP', 'nowhere')}) — re-orient"
    serial = env["KABELSALAT_ANDROID_ADB"]     # KeyError = no Android: also loud

    caps = {"capabilities": {"alwaysMatch": {
        "browserName": "firefox",
        "webSocketUrl": True,
        "moz:firefoxOptions": {
            "androidPackage": "org.mozilla.firefox",
            "androidDeviceSerial": serial,
        },
    }}}
    request = urllib.request.Request(
        f"{GECKO}/session", data=json.dumps(caps).encode(),
        headers={"Content-Type": "application/json"})
    # Starting Fenix on the device takes a while.
    try:
        session = json.load(urllib.request.urlopen(request, timeout=180))["value"]
    except urllib.error.HTTPError as e:
        # geckodriver explains a refused session in its JSON body.
        raise SystemExit(f"new session failed: {e.code} {e.read().decode()}")
    session_id = session["sessionId"]
    ws_url = session["capabilities"]["webSocketUrl"]

    next_id = 0

    def bidi(ws, method, **params):
        global next_id
        next_id += 1
        ws.send(json.dumps({"id": next_id, "method": method, "params": params}))
        while True:
            msg = json.loads(ws.recv(timeout=120))
            if msg.get("id") != next_id:
                continue                       # an event, not our answer
            if msg.get("type") == "error":
                raise RuntimeError(f"{method}: {msg['error']}: {msg.get('message')}")
            return msg["result"]

    try:
        with connect(ws_url, max_size=None) as ws:
            tree = bidi(ws, "browsingContext.getTree")
            context = tree["contexts"][0]["context"]
            bidi(ws, "browsingContext.navigate", context=context,
                 url="https://example.org", wait="complete")
            title = bidi(ws, "script.evaluate", expression="document.title",
                         target={"context": context}, awaitPromise=False)
            if title["type"] == "exception":       # the page's JS threw
                raise RuntimeError(f"script.evaluate: {title['exceptionDetails']['text']}")
            print(title["result"]["value"])
            shot = bidi(ws, "browsingContext.captureScreenshot", context=context)
            with open("/tmp/fenix.png", "wb") as f:
                f.write(base64.b64decode(shot["data"]))
    finally:
        urllib.request.urlopen(urllib.request.Request(
            f"{GECKO}/session/{session_id}", method="DELETE"), timeout=60)

Keep one session per task and end it (the `DELETE`): every new session wipes
Fenix, see below. A refused connection to `GECKO` means geckodriver is gone;
start it again.

### 6. Known behaviours

- **Every session wipes Fenix.** geckodriver runs `pm clear` on it at each
  new session: no logins, history or settings survive, and the onboarding
  screens come back every time.
- **Onboarding blocks screenshots.** While Fenix's first-run overlay covers
  the tab, `browsingContext.captureScreenshot` fails with "width: 0 and
  height: 0". Dismiss it first: `kabelsalat android screenshot`, find the
  button, tap it (with adb, step 7).
- **`session.status` is not a health check.** It reports `ready: false`
  whenever a session is open.
- **Tablet layout by default.** At the pane's size Fenix uses its tablet
  layout and requests desktop sites. For a phone:
  `adb -s "$ADB" shell wm density 420` (undo with
  `adb -s "$ADB" shell wm density reset`), or a narrower screen with
  `kabelsalat android resize 540 1080`.
- **One Android per machine**, owned by one group; it is not moved between
  groups.

### 7. Input

Prefer adb for anything inside Android:

    adb -s "$ADB" shell input tap X Y
    adb -s "$ADB" shell input text 'hello%sworld'   # %s is a space
    adb -s "$ADB" shell input keyevent KEYCODE_BACK

Use `kabelsalat android tap|type|key` only while adb is not authorised yet
(step 2). The pane has no touch device — Android sees a mouse — and the
first click after the pointer enters the pane can register as a swipe from
the top edge, opening the notification shade: press `kabelsalat android key
escape` and tap again. For text, use `kabelsalat android type '…'`, never a
multi-character `key`: key names go through a US keymap.
