#!/usr/bin/env python3
"""RECONSTRUCTED probe script (committed 2026-07-08, closure pass).

The original repro_backlambda.py ran in the session-ephemeral 2026-07-06
repro sandbox and was never committed. This file is a RECONSTRUCTION built
from repro-evidence.md §9's frozen transcripts and §11 conclusion 5: the
byte-for-byte urllib relay of deploy/aws/lambda/questdb-console-proxy/
handler.py's PRE-FIX (r2) request shape — a
`urllib.request.Request(..., method="GET")` carrying the r2 headers
(`Accept-Encoding: identity` + `Connection: close`), `urlopen(timeout=N)`,
and a capped read() loop. It deliberately uses urllib's DEFAULT opener (no
_NoFollowRedirect), so against QuestDB 9.3.5 it reproduces the §9a hang:
the default HTTPRedirectHandler drains the unframed keep-alive 301 body
before following, blocking until the socket timeout. The frozen §9 OUTPUT
blocks are the raw evidence; this script exists so a reader of HEAD can
re-run the probe against a local QuestDB on 127.0.0.1:9000.

Usage: python3 repro_backlambda.py <timeout-secs> <path>
       e.g.  python3 repro_backlambda.py 12 /
"""

from __future__ import annotations

import sys
import time
import urllib.request


def main() -> None:
    timeout = float(sys.argv[1])
    path = sys.argv[2]
    url = f"http://127.0.0.1:9000{path}"
    t0 = time.monotonic()

    def say(msg: str) -> None:
        print(f"[t+{time.monotonic() - t0:7.3f}s] {msg}", flush=True)

    req = urllib.request.Request(url, method="GET")
    req.add_header("Accept", "text/html")
    req.add_header("Accept-Encoding", "identity")
    req.add_header("Connection", "close")
    say(f"urlopen({url!r}, timeout={timeout}) ...")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:  # noqa: S310 — local probe
            say(f"urlopen returned: status={resp.status} headers={dict(resp.headers)}")
            total = b""
            while True:
                t1 = time.monotonic()
                chunk = resp.read(262_144)
                say(f"read(262144) -> {len(chunk)} bytes in {time.monotonic() - t1:.3f}s")
                if not chunk:
                    break
                total += chunk
            say(f"DONE: total body={len(total)} bytes; first 80: {total[:80]!r}")
    except Exception as exc:  # noqa: BLE001 — probe script: show the raise verbatim
        say(f"RAISED {type(exc).__name__}: {exc}")
    say("script exit")


if __name__ == "__main__":
    main()
