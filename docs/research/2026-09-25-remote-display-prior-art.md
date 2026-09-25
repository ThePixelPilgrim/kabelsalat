# Remote display: prior-art research

Date: 2026-09-25
Context: kabelsalat remote display pane (see docs/superpowers/specs/2026-09-25-remote-display-design.md). Sources were checked on 2026-09-25; **[I]** marks inference.

I verified most of the prior art from primary sources (repos cloned or read via the GitHub API, man pages, source code). Where a point is my own inference, it is marked **[I]**. Everything else was checked in source or docs on 2026-09-25.

## Findings per candidate

**1. wprs** ([github.com/wayland-transpositor/wprs](https://github.com/wayland-transpositor/wprs))
- **Status:** Apache-2.0, about 600 stars, no tagged releases. Last commits were Aug 2026 (damage and viewport fixes), so it is maintained but slow. The `wprs` crate on crates.io is a yanked 0.1.0-alpha.1.
- **Architecture:** `wprsd` is a Smithay 0.7 compositor that doesn't composite. It serialises surface state (rkyv) to `wprsc`, a smithay-client-toolkit client, which recreates each window as a local toplevel. It is rootless, xpra-style.
- **Persistence:** the client can disconnect and reattach, and even restart. `wprsd` keeps the last committed buffer for every surface. A `wprsd` restart kills the apps.
- **XWayland:** a separate `xwayland-xdg-shell` binary that works like sommelier or xwayland-proxy-virtwl.
- **Encoding:** lossless only. Per frame it does an SoA transpose, then DPCM, then a YUV-like transform, then zstd. There is no lossy mode and no progressive mode.
- **Flow control:** none. The writer channel is `crossbeam_channel::unbounded()` (src/serialization/mod.rs). Frame callbacks come from a fixed local `--framerate` and pause only while no client is attached. Issue #148 ("terrible networks") is open.
- **Transport and protocol:** a unix socket forwarded over ssh. The README says outright that the protocol is "not stable" across versions or builds.
- **Limitations:** core and xdg-shell only, no dmabuf, no touch, XWayland drag-and-drop missing.
- **Reuse:** there is a `lib.rs`, but it is not a published library.

**2. xpra** (GPL-2.0; latest tag v6.5.3 on 2026-08-18; master's changelog already has 6.6 and 7.0 entries)
- **Lossless refresh:** `auto-refresh-delay` (default `0.15`; config comment: "Idle delay in seconds before doing an automatic lossless refresh"). It works together with `quality`/`min-quality` and `speed`/`min-speed`.
- **Bandwidth:** `bandwidth-limit` and `bandwidth-detection` are capabilities plus runtime packets.
- **Flow control:** every `window-draw` must be acked with `window-draw-ack`, which carries the sequence number and the decode time in microseconds. `batch_delay_calculator.py` adapts the update rate from those acks.
- **Wayland server backend:** it shipped in 6.5 as a wlroots/Cython backend (issue #387). The author's own words were "racy, buggy, slow, and very incomplete" (Oct 2025). The mature path is still the X11 backend (Xvfb/Xdummy), which covers Chromium and the emulator.
- **Protocol docs:** `docs/Network/Protocol.md` is now normative for the modern protocol: 8-byte header, rencodeplus, lz4, packet tables. That is enough to write a client.
- **Clients:** the HTML5 client ([xpra-html5](https://github.com/Xpra-org/xpra-html5)) is MPL-2.0. The official [rust-xpra](https://github.com/Xpra-org/rust-xpra) client (v0.4, very active) is **GPL-3.0** and needs a server at 6.6 or later.
- **GPL for an MIT app:** talking to a GPL server over the wire is fine. Linking or copying rust-xpra is not, so a client would have to be clean-room written from Protocol.md **[I]**.
- **Deployment:** a Python server with C extensions plus Xvfb is not a static binary you drop into a home directory **[I]**.

**3. waypipe** (Rust v0.11.2, last commit Aug 2026, GPL-3.0-or-later; the legacy `waypipe-c` is MIT)
- **Rate adaptation:** none. There is a fixed `--compress`, and `--video` is lossy for DMABUFs only, with a fixed `bpf` bits-per-frame target. There is no refinement pass.
- **Reconnection:** the README says it was "dropped in later versions". It survives only in `waypipe-c`, and only as network reconnection (both ends stay alive).
- **X11:** `--xwls` runs X clients through xwayland-satellite.

**4. VNC**
- **TurboVNC** (3.3.1, Aug 2026, GPL-2.0, an Xvnc X server only):
  - `-alr <timeout>` turns on Automatic Lossless Refresh. `-alrqual` and `-alrsamp` make the refresh JPEG instead.
  - By default only `X[Shm]PutImage` regions qualify; `ALRAll` / `TVNC_ALRALL=1` makes every region qualify.
  - Viewers can also request a refresh by hand (Ctrl-Alt-Shift-L).
  - The refresh needs no RFB extension: it is just a lossless Tight update. Flow control uses the Fence and ContinuousUpdates extensions (`flowcontrol.c`).
- **TigerVNC** (1.16.2) and **KasmVNC** (GPL-2.0) do the same thing. TigerVNC has `EncodeManager::writeLosslessRefresh`. KasmVNC has `-DynamicQualityMin/Max` and `-TreatLossless`.
- **wayvnc 0.10.2 / neatvnc 1.0.2** (ISC, very active):
  - neatvnc supports Fence and ContinuousUpdates, estimates bandwidth and RTT, and drops frames when data in flight exceeds bandwidth × (33 ms + RTT). It also uses 32×32 tile hashing ("damage refinery").
  - It has **no automatic lossless refresh**: quality is fixed per client.
  - wayvnc can listen on `unix:` sockets, but it needs a separate wlroots compositor.

**5. RDP**
- **Progressive codec in Linux encoders:**
  - gnome-remote-desktop (51.0) uses RFX Progressive, but only `RFX_PROGRESSIVE_TILE_SIMPLE`, i.e. no upgrade passes. It requires EGFX.
  - FreeRDP (3.32.0) has an encoder (`progressive_compress`), but it too writes only the simple-tile variant.
  - Weston's RDP backend (16.0.90) has no EGFX at all: only RemoteFX surface bits, NSCodec, or raw bitmaps. It listens on `--address`/`--port` over TCP with TLS or RDP security. The alternative is `--external-listener-fd`, which is meant for "local (such as AF_VSOCK)" sockets and **skips TLS and security**. Passing a unix-socket fd there would probably also work **[I]**.
- **IronRDP** (MIT OR Apache-2.0, very active; ironrdp 0.17, ironrdp-server 0.13, ironrdp-egfx 0.3):
  - `ironrdp-egfx` has a server with frame-ack flow control (3 frames in flight by default, QoE tracking, the suspend-ack state) and codecs AVC420, AVC444, **lossless ClearCodec and Planar**.
  - `ironrdp-graphics` has RFX Progressive `encode_first_pass` / `encode_upgrade_pass` primitives and a full progressive decoder, including TILE_UPGRADE.
  - The plain `ironrdp-server` crate supports TLS only.
- **Nobody ships a Linux server that does real progressive upgrades.** The building blocks exist in IronRDP.
- **Caveat:** the finest RFX quality is still not bit-exact. For a truly lossless end state you would finish with ClearCodec or Planar **[I]**.

**6. SPICE and NoMachine**
- **SPICE:** images are sent lossless. Regions detected as video are streamed as MJPEG, and when the stream stops a `RedUpgradeItem` re-sends that area lossless. It is built for VMs, not rootless apps.
- **NoMachine:** proprietary. Its "multi-pass display encoding" refines progressively up to the quality slider's target when the screen is idle.

**7. Android emulator**
- **gRPC:** `streamScreenshot(ImageFormat) returns (stream Image)` delivers raw RGBA or PNG frames. Pacing would come only from gRPC/HTTP2 flow control **[I]**.
- **WebRTC:** [android-emulator-webrtc](https://github.com/google/android-emulator-webrtc) (Apache-2.0, active, Jul 2026 release) uses the emulator's `Rtc` gRPC service. It gets WebRTC's congestion control, which is lossy video that never refines **[I]**.
- **scrcpy** (v4.1, Apache-2.0): H.264, H.265 or AV1 with fixed `--video-bit-rate`, `--max-fps` and `--max-size`, not adaptive **[I, from general knowledge]**.
- **Qt window:** the emulator's own window generally needs xcb, i.e. XWayland **[I]**. None of these routes covers Chromium.

**8. Close relatives**
- [wrdp](https://github.com/rcarmo/wrdp) (MIT): IronRDP in front of a headless GPL labwc session. Useful as a design reference.
- [lamco-rdp-server](https://github.com/lamco-admin/lamco-rdp-server): Rust/IronRDP, EGFX H.264, but **BUSL-1.1**, so not usable.
- [xwayland-satellite](https://github.com/Supreeeme/xwayland-satellite) (MPL-2.0, very active): an option for rootless XWayland beside Smithay's built-in X11Wm.
- [selkies](https://github.com/selkies-project/selkies) (MPL-2.0): a WebRTC desktop, not a fit.
- I found no project that pairs a Smithay remote compositor with its own client and progressive tiles.

## Comparison table

Legend: + meets it, ~ partly, − no.

| | Chromium + emulator (XWayland) | Flow control | Lossy → lossless refinement | Survives disconnect | Static binary in `$HOME`, no root | Rust client for GTK4 | Licence vs. MIT app |
|---|---|---|---|---|---|---|---|
| wprs | + (xwayland-xdg-shell) | − (unbounded queue) | − (lossless only) | + (client can restart) | + [I] (needs Xwayland on host) | ~ (SCTK client; unstable protocol) | + Apache-2.0 |
| xpra (X11 backend) | + | + (draw acks, bandwidth detection) | + (`auto-refresh-delay`) | + | − (Python + Xvfb) | ~ (clean-room client from Protocol.md) | ~ GPL server over the wire; client must not copy GPL code |
| waypipe | + (`--xwls`) | − | − | − in Rust version (waypipe-c only) | + | − (it's a proxy) | ~ GPL-3 |
| TurboVNC | + (X only) | + (Fence/CU) | + (`-alr`) | + | − (X server) | ~ | ~ GPL, over the wire |
| wayvnc/neatvnc | + via wlroots compositor | + (bandwidth estimate, frame drop) | − | + | ~ | ~ | + ISC |
| gnome-remote-desktop / Weston RDP | + / + | + (EGFX acks) / ~ | − (TILE_SIMPLE only) / − | + | − (needs GNOME) / ~ | + (IronRDP) | + over the wire |
| IronRDP building blocks | n/a | + (EGFX frame acks) | + (progressive first/upgrade passes + lossless ClearCodec/Planar) | + [I] | + | + | + MIT/Apache |
| Emulator gRPC/WebRTC, scrcpy | emulator only | ~ / + / − | − | + | + | ~ | + Apache |

## Recommendation [I]

Nothing off the shelf meets all the must-haves together. The two that come closest fail on hard requirements. xpra meets the functional must-haves (refinement, acks, persistence), but it is GPL and a Python stack, not a static binary. wprs has the right architecture and licence, but has neither flow control nor lossy encoding.

**Build your own remote side from Smithay, taking wprs as the main source to borrow from, and use RDP-EGFX via IronRDP as the display protocol:**

1. **Remote side: a static Rust binary ("remote klamottenkiste").** A headless Smithay compositor that composites one output per pane, with XWayland through Smithay's X11Wm or xwayland-satellite. From wprs, reuse or port the buffer storage, the per-surface state kept for resumption, and the local frame-callback scheduling. Its Apache-2.0 licence is compatible with MIT.
2. **Protocol: EGFX on top of `ironrdp-egfx`.** It already gives:
   - ack-based flow control (frames in flight, queue depth, suspend);
   - progressive RFX first and upgrade passes;
   - lossless ClearCodec and Planar for the final refresh.

   The client side (the `ironrdp-egfx` client plus the progressive decoder) lands in klamottenkiste's GTK4 widget. Because it is standard RDP, FreeRDP and mstsc can connect for debugging. Transport would be a unix socket forwarded over the existing ControlMaster (as wprs does), with TLS kept or security stubbed out as Weston does for local listeners.
3. **What still needs building:**
   - the policy that takes each tile from lossy to upgrade passes to lossless when it goes idle (a TurboVNC `-alr` / xpra `auto-refresh-delay` equivalent, driven by per-tile hashes like neatvnc);
   - pull and ack pacing tied to Smithay frame callbacks;
   - a reattach handshake that replays surface state (ResetGraphics plus a full refresh);
   - input mapping, clipboard, and popup handling within a single composited output.
4. **Quick proof of concept on the way:** run an unmodified `wprsc` as a Wayland client of klamottenkiste, with `wprsd` on the remote host. That should show Chromium and the emulator in a pane, with reconnection, today. Its missing flow control is exactly what step 3 fixes. This is untested.

Keep the Android emulator display on the general path. `-no-window` with gRPC/WebRTC or scrcpy is only a fallback, and CDP/adb control is unaffected either way.

The cloned repos are in `/tmp` (wprs, xpra, waypipe, turbovnc, neatvnc, wayvnc, grd, FreeRDP, weston, IronRDP, spice). No project files were changed.
