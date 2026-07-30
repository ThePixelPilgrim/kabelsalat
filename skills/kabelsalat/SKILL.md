---
name: kabelsalat
description: Use when a command should run in a visible, persistent terminal the user can watch and interact with — a dev server, a long build, or an interactive claude session — rather than as a captured subprocess; not for commands whose output you need to capture or read back. Launches it in a named group of the user's running kabelsalat terminal.
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

    kabelsalat run --group <name|uuid> [--cwd DIR] -- COMMAND [ARGS...]

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

## Exit codes

| Code | Meaning | What to do |
|------|---------|------------|
| 0 | Tab created | Tell the user which group it went to |
| 1 | kabelsalat is not running | Report this to the user and stop. Do not retry, and do not try to start it — that is theirs to do |
| 2 | Usage error | Fix the invocation; check `--` is present |
| 3 | Group not found or ambiguous | Re-run `kabelsalat groups` and retry with a uuid |

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

Fetch the endpoint and group identity once per task (a running shell's
inherited environment may be stale; the tmux table is live):

    tmux show-environment KABELSALAT_CDP     # KABELSALAT_CDP=http://127.0.0.1:<port>
    tmux show-environment KABELSALAT_GROUP   # KABELSALAT_GROUP=<group-uuid>

A leading `-` in the output means the variable is unset (no browser running).

Author scripts against stock Playwright, and start every script with the same
preamble — re-resolve the environment and assert the group you pinned on
first fetch. Tabs can be moved between groups; the assert is what turns a
silently-wrong browser into a loud, recoverable failure:

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
re-run the fetch and retry. Prefer one script that does many steps over many
single-step invocations.
