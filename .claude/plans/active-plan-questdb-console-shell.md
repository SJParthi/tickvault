# Implementation Plan: B4 QuestDB console shell (GET /) 503 fix

**Status:** VERIFIED
**Date:** 2026-07-03
**Approved by:** Parthiban (operator) — B4 shell-load-503 fix directive, this session

> Guarantee matrix: this plan cross-references the canonical 15-row + 7-row
> matrices in `.claude/rules/project/per-wave-guarantee-matrix.md` (deploy-only
> change — no Rust crate / hot-path surface; the 100% envelope below is stated
> honestly per operator-charter §F).

## Design

The B4 QuestDB console is served through two Lambdas: a non-VPC FRONT
(`questdb-console-front`) that does auth + the read-only SQL gate, and a
VPC-attached BACK relay (`questdb-console-proxy`) that forwards the request to
the box's private `:9000` over stdlib `urllib`. CloudWatch shows `GET /`
(the console HTML shell) REPORTs pinned at ~12016ms = `_TIMEOUT_SECS` (12s)
every time, while `GET /exec` returns 200 in ~4ms interleaved in the SAME
second through the SAME back-Lambda→box:9000 hop. The interleaving proves the
box is UP, reachable, and QuestDB is fast — so this is NOT box-offline and NOT
the #1368 bind-refused issue. The failure is specific to `/`: its response
carries no length delimiter (no Content-Length, not chunked — most plausibly
on-the-fly gzip triggered by the forwarded browser `Accept-Encoding`), so
`urllib`'s `HTTPResponse.read()` blocks with no EOF until the 12s socket
timeout fires. `/exec` returns a Content-Length'd JSON body and EOFs in ~4ms.
The back's single blanket `except Exception -> {"err":"box_unreachable"}` then
converts that read-timeout into a false 503 "offline".

Fix (all in the BACK relay's upstream request, framing-agnostic):
1. PRIMARY — add `Connection: close` to the upstream `urllib.request.Request`
   so QuestDB closes the socket after the body, giving `read()` a clean EOF
   that returns immediately regardless of gzip / Content-Length.
2. Stop forwarding the browser's `Accept-Encoding`; force `identity` so the
   shell comes back Content-Length'd (uncompressed, still well under the
   `MAX_BODY_BYTES` relay cap).
3. Diagnostic honesty — split the blanket `except` so `socket.timeout` /
   `TimeoutError` (and a `URLError` wrapping one) map to a distinct
   `upstream_timeout`; the FRONT returns 504 for it, reserving the 503
   "offline" for a genuine connection refusal.

The query path (`/exec`, `/exp`, static assets, auth, the read-only SQL gate,
path-traversal guard, marker parity) is UNCHANGED. Marker bumped
`b4-qdb-console-2026-07-03-r1` → `-r2` in BOTH handlers (parity ratchet).

## Edge Cases

- Browser sends `Accept-Encoding: gzip, br` → NOT forwarded; upstream gets
  `identity`. Ratcheted (`test_shell_get_forces_identity_and_connection_close`).
- Real QuestDB `/exec` 400 (bad column) → still relayed with its body (HTTPError
  arm untouched).
- Response larger than the cap → `too_large` → front 502 (unchanged).
- `socket.timeout` == `TimeoutError` on Python 3.10+ — both caught.
- `URLError` wrapping a socket timeout → `upstream_timeout`; a refused/DNS/reset
  `URLError` → `box_unreachable` (unchanged 503).

## Failure Modes

- Slow box / unframed response → `upstream_timeout` → 504 (was silent 12s hang
  → false 503). Honest 504 now.
- Genuine connection refusal / stopped box → `box_unreachable` → 503 offline
  (preserved).
- `identity` shell larger on the wire but bounded by `MAX_BODY_BYTES`
  (4.1MB) → over-cap → 502, never 503.

## Test Plan

Back (`questdb-console-proxy/test_handler.py`):
- `test_shell_get_forces_identity_and_connection_close` — upstream request
  carries `Accept-Encoding: identity` + `Connection: close`, browser gzip not
  forwarded, browser `accept` still relayed.
- `test_socket_timeout_maps_to_upstream_timeout` (renamed) — timeout →
  `upstream_timeout`.
- `test_urlerror_wrapping_timeout_maps_to_upstream_timeout` (new).
- `test_urlerror_refused_maps_to_box_unreachable` (renamed) — refusal stays 503.

Front (`questdb-console-front/test_handler.py`):
- `test_shell_get_authed_forwards_to_back_root` — GET / authed → path "/".
- `test_upstream_timeout_maps_to_504_not_503`.

Result: back 21 tests OK, front 60 tests OK; `test_build_marker_present`
(parity) green in both.

## Rollback

Pure deploy/lambda change. Revert the 2 handler edits (marker back to r1,
restore the blanket except + `accept-encoding` forwarding). No schema, no
terraform, no Rust. The box QuestDB needs no change. Redeploy the two Lambdas.

## Observability

The distinct `upstream_timeout` → front 504 (vs 503 offline) makes the
CloudWatch access log + status code honest about "reachable-but-slow" vs
"box offline". The `QDB_CONSOLE_BUILD = b4-qdb-console-2026-07-03-r2` marker is
the deployed-bytes proof so the fix is distinguishable in the running Lambda.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | GET / , box up, gzip-capable browser | 200 shell (identity, Connection:close → immediate EOF), no 12s hang |
| 2 | GET /exec select 1 | 200 ~4ms (query path unchanged) |
| 3 | Slow / unframed upstream | 504 upstream_timeout (not false 503) |
| 4 | Box stopped (connection refused) | 503 offline (preserved) |
| 5 | Response > 4.1MB | 502 too_large (preserved) |

## Honest 100% claim

100% inside the tested envelope: the two framing-agnostic guards
(`Connection: close` + forced `identity`) make `read()` EOF immediately
regardless of whether the missing delimiter was gzip-without-Content-Length or
keep-alive-without-Content-Length. Residual [Unknown]: the precise reason `/`
lacks a length delimiter is not provable from the repo (port 9000 is
private/unlogged) — but the fix works regardless of which it is; verify the
live console loads after redeploy.
