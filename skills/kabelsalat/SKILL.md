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
command exits, the tab stays visible showing its exit status.
