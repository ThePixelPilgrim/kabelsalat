# Remote display pane — decision record

Date: 2026-09-25
Status: in design. The requirements and architecture below are agreed.
Spec 1 is being written section by section in the klamottenkiste repo.

This supersedes the "Follow-up: remote display pane" section of
`2026-09-25-remote-tmux-design.md`, whose RDP-sink idea was rejected (see
"Rejected" below).

## Requirements

1. **Use case.**
   - A remote agent session (claude, or any model driven through the pi
     harness) works with graphical apps on the remote host: Chromium,
     steered through CDP, and the Android emulator, steered through adb.
   - The user watches and interacts with those apps from kabelsalat.
2. **Apps run with no one watching.** A missing viewer, a slow viewer or a
   disconnected viewer must never throttle or pause the app.
3. **Slow links.** The viewer stream must never saturate the link. Pixels are
   lossy first and refined to exact once content is static ("eventual
   fidelity").
4. **Capture on the host.** The remote session gets screenshots (PNG),
   synthetic input and **video** directly from the display server, without a
   round trip through the viewer.
   - Video must be a real video file, because other models (e.g. through the
     pi harness) ingest video. A PNG frame sequence covers Claude for now.
   - Recording is tied to a handle: a recording exists only while its handle
     (a connection) stays open, and closing or crashing the handle stops it.
   - Encoding must use little CPU. Lossy is fine.
5. **Deployment without root.** The remote side is a static binary in the
   user's home, following the version rules below.
6. **Works with the Android emulator**, whose Qt UI needs Xwayland.

## Architecture

```
remote host: klamottenkiste-display (static, versioned; one process per display)
  ├ headless Smithay compositor (software/pixman), apps paced at 60 Hz always
  ├ renders only on demand (a consumer exists and something changed), with real damage
  ├ control.sock: screenshot, input, record (handle = connection), status, stop
  ├ Xwayland (host dependency, opt-in) for the emulator
  └ viewer stream (optional consumer): damaged tiles, newest state wins,
    lossy → exact when idle
local: klamottenkiste presenter in the kabelsalat pane (display only;
       input, resize and clipboard go back)
```

- A display runs inside kabelsalat's remote tmux server as session
  `ksd-<group-uuid>`, so it survives disconnects like the tabs do.
- kabelsalat exports `KLAMOTTENKISTE_DISPLAY` (and the CDP variables for a
  remote browser) into the group's remote sessions.

## Decisions

- **One display = one process = one app.** This matches klamottenkiste's
  current model: one compositor per client.
- **Recording.**
  - The default is VP8 in WebM (libvpx realtime, royalty-free, statically
    linked). `--codec vp9` is an option.
  - MP4/H.264 is produced only via an `ffmpeg` already installed on the host,
    so we never compile H.264 ourselves (patent reasons).
  - Encoding is damage-driven with a variable frame rate, capped at 10 fps by
    default, with optional downscaling.
  - WebM is written in live mode, so a killed recorder still leaves a
    playable file.
  - The defaults are to be confirmed by CPU measurements on a real host.
- **Versioned binaries.** They live at
  `~/.local/share/kabelsalat/bin/<name>-<version>`.
  - They are never overwritten, and an older kabelsalat never deletes a newer
    binary.
  - The newest *compatible* binary wins, via a launch-interface version range
    each binary reports.
  - A group can pin a version.
  - A running display is never restarted automatically.
- **Licensing.**
  - klamottenkiste is LGPL-3.0-or-later and kabelsalat stays MIT.
  - Code taken from wprs (Apache-2.0) is compatible with LGPLv3.
  - xpra's client code (GPL) must not be linked or copied.
- **Out of scope for v1:** a replay buffer (it would be just another
  consumer, so it can be added later), audio, and GPU acceleration.

## Rejected

- **CDP screencast of a headless Chromium.** It is browser-only and does not
  work for the emulator.
- **waypipe.** It has no rate adaptation and no refinement; its Rust version
  dropped reconnection; and the display dies with the ssh connection.
- **RDP sink with a stock compositor.**
  - No Linux server implements RDP progressive upgrade passes.
  - weston has no EGFX (RDP's graphics pipeline).
  - IronRDP stays an option as a codec or protocol library for spec 2.
- **Unmodified wprs.** The proof of concept (`docs/research/2026-09-25-wprs-poc/`)
  measured the following problems:
  - Its send queue is unbounded: wprsd grew to 1–3.6 GB and latency to more
    than 60 s whenever the app produced more than the link carried.
  - It sends the whole buffer on every commit: a single CSS spinner cost
    19 Mbit/s.
  - Apps freeze at 0 fps while no viewer is attached, which violates
    requirement 2.
  - wprsd does not render, so it cannot capture on the host
    (requirement 4).
  - It stays a reference for reattach handling and Xwayland.
- **xpra.** It matches the viewer behaviour (it acks every draw and runs an
  `auto-refresh-delay`), but it is GPL, its server is Python plus Xvfb, and it
  cannot ship as a static binary. It is a design reference only.

## Decomposition

1. **klamottenkiste: headless display.** The static binary, pixman rendering,
   on-demand damage-driven rendering, the control API, recording and
   Xwayland. This is spec 1 in the klamottenkiste repo, in progress.
2. **klamottenkiste: viewer stream and remote presenter.** The protocol, flow
   control (newest state wins), tile refinement and reattach. Either an own
   tile protocol or RDP through IronRDP.
3. **kabelsalat: integration.** Deploying and starting displays in remote
   groups, the remote pane, the session environment variables, and the
   screenshot handover by file path.

## Research

- `docs/research/2026-09-25-remote-display-prior-art.md`
- `docs/research/2026-09-25-wprs-poc/REPORT.md`
