# wprs proof of concept (2026-09-25)

Unmodified wprs does work inside a klamottenkiste pane: remote windows render and take input. But the missing flow control turned out to be a real problem even on the real link. I did not write `.poc/REPORT.md`: the tool guard here blocks subagents from writing report files. The findings are below, so you can save them there.

Tags: **[M]** measured, **[C]** read in wprs source, **[I]** inference. wprs was a clean upstream master `12b864d`, klamottenkiste v0.2.1.

## 1. dev host [M]
- x86_64 Fedora 43 container, glibc 2.42, cargo/rustc 1.98.1 installed.
- **Not present:** Chromium/Chrome, Xwayland, xterm/xeyes, Android SDK/emulator, `/dev/kvm`, libxkbcommon, xkb data, libwayland. `XDG_RUNTIME_DIR` is not set over ssh.
- Link: about 20 ms RTT and about 80 Mbit/s over ssh.

Because of this, the remote tests on dev used `.poc/animbox`, a small Wayland test client I wrote that needs no system libraries. Chrome and Xwayland were tested through a **local** wprsd.

## 2. Build [M]
- Built locally and copied over. The binaries only need libxkbcommon, libgcc_s and libc (glibc 2.39 or newer is enough).
- On dev, wprsd also needed libxkbcommon.so.0, the xkb data, `LD_LIBRARY_PATH`, `XKB_CONFIG_ROOT`, `XDG_RUNTIME_DIR` and `--enable-xwayland=false`. Without them it fails with "could not load the specified keymap".

## 3. Harness
`.poc/harness` is one WaylandPane in a GTK4 window, the same approach as klamottenkiste's `examples/demo.rs`, and it spawns a command into the pane. Three helpers sit alongside it:
- `relay.py` is a rate-limiting relay that also counts bytes.
- `lat.py` measures latency: it clicks through the control channel, then polls screenshots until the marker pixel changes.
- `phase.sh`, `drain.sh` and `matrix.sh` run the measurement matrix.

## 4. What worked and what didn't [M]
- **Worked:**
  - animbox on dev, reaching the pane over my own `ssh -N -L` unix-socket forward.
  - Chrome: typing into an `<input>`, the `<select>` popup and the right-click context menu.
  - xterm via Xwayland, with a server-side titlebar.
  - Screenshots are in `.poc/shots/` (02, 03, 05, 10–16).
- **Gaps:**
  - The pane does not offer `wp_viewporter` or primary selection. wprsc warns and carries on.
  - Several remote windows just stack on top of each other, all maximized, with no way to switch.
  - The control channel has no scroll command, and the Escape key is `esc`, not `Escape`.
  - Android emulator: not testable.

## 5. Reattach [M, C]
- Killing the ssh forward makes wprsc **panic and exit**. It never reconnects by itself, so something has to restart it.
- While detached, the remote app stays alive at 0 fps (wprsd stops frame callbacks) and wprsd memory stays flat.
- A new forward plus a new wprsc restored the view with the app's state intact. One harmless "commit with empty data" error appears on every reattach.
- **Stale frames are replayed:** after 30 s at 1 Mbit, reattaching unthrottled first delivered about 32 MB over 7 s. The first click took 5.3 s to show; after that it was back to about 80 ms. The cause is that the send queue is a single unbounded channel shared across connections and never cleared on disconnect [C].

## 6. Bandwidth and latency [M]
How measured: bytes per second at the relay; wprsd memory (RSS) sampled once a second on dev; latency as click-to-pixel-change; backlog as the bytes still delivered after switching the app to idle and removing the throttle. The throttle sits between the ssh forward and wprsc.

animbox workloads on dev (1280×720):

| Workload | Unthrottled (real link) | 10 Mbit/s | 1 Mbit/s |
|---|---|---|---|
| idle | 0.24 Mbit/s, 67 ms | 81 ms | 205 ms |
| plasma (full-window gradient, compresses very well) | 2.7 Mbit/s, 122 ms | 120 ms | 51 s, then over 60 s; memory 316 MB → 1.49 GB in 203 s |
| scroll (text-like, full window) | 8.1 Mbit/s, 108 ms | 7 Mbit/s, 139 ms | all probes over 60 s; memory 45 MB → 1.54 GB; 216 MB backlog |
| noise (400×400 incompressible patch) | link saturated at 61 Mbit/s; latency 6.3 s, then 26.8 s, then over 60 s; memory 27 MB → 2.24 GB in 105 s; 1.3 GB still draining 121 s after the workload stopped | memory → 3.6 GB; 2.76 GB backlog, 282 s to drain | not run |

Chrome through the local wprsd:
- Unthrottled: an idle page costs 0.38 Mbit/s, **one 120 px CSS spinner costs 19.2 Mbit/s**, and autoscrolling costs 23.4 Mbit/s. The reason is that wprsd compresses and sends the **whole buffer on every commit**; damage is only passed along as metadata [C].
- Spinner at 10 Mbit/s: latency 28 s and 54 s, memory 35 MB → 1.17 GB in 112 s.
- Spinner at 1 Mbit/s: latency over 60 s, memory → 1.68 GB.
- The app never slows down: 40–60 fps at every throttle, because frame callbacks run on a timer that ignores delivery [C].

**Missing flow control is confirmed as a practical problem.** Memory and latency grow without bound whenever the app produces more than the link carries. That happened even on the unthrottled 80 Mbit link.

- **Did ssh itself stall?** No: running a command over the same ssh connection took 0.12–0.38 s on average (max 0.86 s) in every phase, including while the real link was saturated.
  - Caveat: because my throttle sits after ssh, the ssh TCP connection itself was never congested in the throttled phases.
  - On a real 1 Mbit link, interactive channels would likely queue behind up to about 2 MB of screen data, around 16 s [I].
- **Memory grows much faster than the backlog** [M]: at 1 Mbit plasma, wprsd grew by 1.18 GB while only 49 MB was later delivered. It is only partly released after draining.
- **wprsd is CPU-bound on compression** [I]: at 10 Mbit, scroll fps fell from about 45 to 37 while bandwidth stayed under the cap.
- **Version mismatch only logs a warning** [C].

## 7. Implications [I unless noted]
**(1) Extending wprs**
- Frames are full buffers, not deltas. So a bounded queue should keep only the newest pending buffer per surface, replacing older ones, while keeping protocol messages in order. wprsc already tolerates a commit whose buffer is missing.
- Ack pacing is simplest done by holding back frame callbacks until wprsc acks. The detached case already shows apps throttle themselves to 0 fps that way [M].
- Before any lossy encoding, send only the damaged regions: the spinner changes about 1.6 % of the pixels but costs 19 Mbit/s.
- Clear the queue on disconnect and send a fresh full snapshot on reattach.
- Make a version mismatch a hard failure. kabelsalat will be deploying wprsd itself.
- Plan to bundle or statically link libxkbcommon plus xkb data, and to set `XDG_RUNTIME_DIR`.

**(2) Moving wprsc's role into klamottenkiste**
- Running unmodified wprsc as a separate process per pane is viable today, provided kabelsalat restarts it after each disconnect [M].
- Doing it inside klamottenkiste would remove one Wayland hop and one full-frame copy. It would let the pane ack only frames it actually presented, and let a hidden pane stop acking so the remote app idles. That fits the pane's existing pause-on-hide behaviour.
- klamottenkiste would need viewporter and a way to handle several remote windows.
- Latency floor for an idle app: 46 ms locally and about 70 ms over the 20 ms link [M]. Both include 10–25 ms of screenshot-polling granularity.

## Cleanup
- All PoC processes are stopped, locally and on dev, and `~/.local/share/kabelsalat-poc` is deleted on dev.
- `tmux -L kabelsalat` and the kabelsalat ssh ControlMaster sockets were not touched.
- The build target dirs, the Chrome profile and the stale sockets are deleted.
- `.poc/` is about 4.6 MB and keeps the sources, logs and shots.
- Side effect: rustup installed toolchain 1.88.0 locally, because wprs pins it. Remove it with `rustup toolchain uninstall 1.88.0` if you don't want it.
- My own mistake during the run: one `pkill -f` pattern also killed the dev test harness. I restarted it; it cost one planned screenshot (shot 04).

Files are in /home/christoph/Projects/kabelsalat/.poc/:
- harness/src/main.rs
- animbox/src/main.rs
- relay.py
- lat.py
- phase.sh
- drain.sh
- matrix.sh
- test.html
- logs/phase-*.txt
- logs/relay-dev.csv
- shots/*.png