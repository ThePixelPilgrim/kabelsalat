#!/usr/bin/env python3
"""Input-to-screen latency probe through the klamottenkiste control channel.

lat.py CONTROL_SOCK N [TIMEOUT_S] [X Y]

Reads the marker pixel (default 60,60) from a pane screenshot, sends `click X Y`
(default 60 60, inside animbox's marker), then polls screenshots until the marker
pixel changes. Prints one latency line per probe and a summary. The poll interval
is bounded by screenshot cost (printed as `shot_ms`).
"""
import socket, sys, time
from PIL import Image

ctl = sys.argv[1]
n = int(sys.argv[2])
timeout = float(sys.argv[3]) if len(sys.argv) > 3 else 30
mx, my = (int(sys.argv[4]), int(sys.argv[5])) if len(sys.argv) > 5 else (60, 60)
path = "/tmp/kst-lat-probe.png"


def req(line):
    s = socket.socket(socket.AF_UNIX)
    s.connect(ctl)
    s.sendall((line + "\n").encode())
    r = b""
    while not r.endswith(b"\n"):
        c = s.recv(4096)
        if not c:
            break
        r += c
    s.close()
    return r.decode().strip()


def pixel():
    t = time.monotonic()
    r = req(f"screenshot {path}")
    if not r.startswith("ok"):
        raise RuntimeError(r)
    with Image.open(path) as im:
        p = im.convert("RGB").getpixel((mx, my))
    return p, (time.monotonic() - t) * 1000


res = []
for i in range(n):
    before, shot = pixel()
    t0 = time.monotonic()
    req(f"click {mx} {my}")
    lat = None
    while time.monotonic() - t0 < timeout:
        p, shot = pixel()
        if p != before:
            lat = (time.monotonic() - t0) * 1000
            break
    print(f"probe {i}: before={before} latency_ms={'TIMEOUT' if lat is None else f'{lat:.0f}'} shot_ms={shot:.0f}", flush=True)
    res.append(lat)
    time.sleep(0.3)
ok = sorted(x for x in res if x is not None)
if ok:
    print(f"SUMMARY n={len(ok)}/{n} min={ok[0]:.0f} median={ok[len(ok)//2]:.0f} max={ok[-1]:.0f} ms")
else:
    print(f"SUMMARY n=0/{n} all timed out")
