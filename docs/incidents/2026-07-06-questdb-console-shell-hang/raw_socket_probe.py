#!/usr/bin/env python3
"""RECONSTRUCTED probe script (committed 2026-07-08, closure pass).

The original raw_socket_probe.py ran in the session-ephemeral 2026-07-06
repro sandbox and was never committed. This file is a RECONSTRUCTION from
repro-evidence.md §10's frozen transcript: the exact request bytes are the
ones printed on §10's `sent:` line, followed by a timed recv loop with a
20s timeout. Against QuestDB 9.3.5 it answers the question "does the server
ever close the socket after the unframed / 301?" — the frozen §10 OUTPUT
block (20.017s recv gap, zero close) is the raw evidence; this script
exists so a reader of HEAD can re-run the probe against a local QuestDB on
127.0.0.1:9000.

Usage: python3 raw_socket_probe.py
"""

from __future__ import annotations

import socket
import time

REQ = b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAccept-Encoding: identity\r\n\r\n"
RECV_TIMEOUT_SECS = 20.0


def main() -> None:
    s = socket.create_connection(("127.0.0.1", 9000), timeout=RECV_TIMEOUT_SECS)
    s.settimeout(RECV_TIMEOUT_SECS)
    print(f"sent: {REQ!r}", flush=True)
    s.sendall(REQ)
    t0 = time.monotonic()
    last = t0
    raw = b""
    while True:
        try:
            chunk = s.recv(65536)
        except socket.timeout:
            now = time.monotonic()
            print(
                f"[t+{now - t0:7.3f}s] recv TIMED OUT after {now - last:.3f}s gap"
                " — server NEVER closed the socket",
                flush=True,
            )
            break
        now = time.monotonic()
        if not chunk:
            print(f"[t+{now - t0:7.3f}s] recv -> EOF (server closed)", flush=True)
            break
        print(f"[t+{now - t0:7.3f}s] recv -> {len(chunk)} bytes (gap {now - last:.3f}s)", flush=True)
        last = now
        raw += chunk
    print("--- full raw bytes received ---")
    print(repr(raw))


if __name__ == "__main__":
    main()
