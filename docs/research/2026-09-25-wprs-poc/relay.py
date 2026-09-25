#!/usr/bin/env python3
"""Rate-limited, byte-counting unix-socket relay (PoC).

relay.py LISTEN_SOCK UPSTREAM_SOCK LOG_CSV RATE_FILE

Every accepted connection on LISTEN_SOCK is paired with a fresh connection to
UPSTREAM_SOCK. Downstream (upstream -> client, i.e. wprsd -> wprsc) is paced by a
token bucket whose rate in bit/s is re-read from RATE_FILE once per second
("0" = unlimited). The relay reads from upstream only when it may forward, so
backpressure propagates to the sender exactly as a slow link would (plus a single
64 KiB read buffer). Upstream direction is unthrottled. Once per second a CSV row
`t,down_bytes,up_bytes,rate_bps` is appended to LOG_CSV.
"""
import os, socket, sys, threading, time

LISTEN, UPSTREAM, LOG, RATE_FILE = sys.argv[1:5]
CHUNK = 64 * 1024
counters = {"down": 0, "up": 0}
lock = threading.Lock()
rate = [0.0]


def read_rate():
    try:
        with open(RATE_FILE) as f:
            return float(f.read().strip() or 0)
    except Exception:
        return 0.0


def logger():
    with open(LOG, "a", buffering=1) as f:
        while True:
            time.sleep(1)
            rate[0] = read_rate()
            with lock:
                d, u = counters["down"], counters["up"]
                counters["down"] = counters["up"] = 0
            f.write(f"{time.time():.3f},{d},{u},{rate[0]:.0f}\n")


def pump(src, dst, key, paced):
    tokens = 0.0
    last = time.monotonic()
    try:
        while True:
            if paced and rate[0] > 0:
                bps = rate[0] / 8.0
                burst = max(bps / 20.0, 4096)  # 50 ms burst
                while True:
                    now = time.monotonic()
                    tokens = min(burst, tokens + (now - last) * bps)
                    last = now
                    if tokens >= 1024:
                        break
                    time.sleep((1024 - tokens) / bps)
                n = int(min(CHUNK, tokens))
            else:
                n = CHUNK
                last = time.monotonic()
            data = src.recv(n)
            if not data:
                break
            dst.sendall(data)
            if paced and rate[0] > 0:
                tokens -= len(data)
            with lock:
                counters[key] += len(data)
    except OSError:
        pass
    finally:
        for s in (src, dst):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


def main():
    try:
        os.unlink(LISTEN)
    except FileNotFoundError:
        pass
    rate[0] = read_rate()
    threading.Thread(target=logger, daemon=True).start()
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(LISTEN)
    srv.listen(8)
    while True:
        c, _ = srv.accept()
        u = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            u.connect(UPSTREAM)
        except OSError as e:
            print(f"upstream connect failed: {e}", flush=True)
            c.close()
            continue
        print("connection paired", flush=True)
        threading.Thread(target=pump, args=(u, c, "down", True), daemon=True).start()
        threading.Thread(target=pump, args=(c, u, "up", False), daemon=True).start()


main()
