# Implementation Plan: Surface the REAL Groww NATS reject reason in the sidecar

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — 2026-06-29

> Cross-reference: this plan carries the full guarantee matrix per
> `.claude/rules/project/per-wave-guarantee-matrix.md`. All 15 + 7 rows below
> apply to every item in this plan.
> Scope rule: this is a **Python-only** change to the Groww validation sidecar
> (operator lock §32, LOCAL/operator-captured). It touches NO Rust crate, NO
> Dhan path, NO auth, NO watch-list build, NO NDJSON 6-key contract, NO segment
> handling. Per `no-rest-except-live-feed-2026-06-27.md` it adds NO REST call.

## Design

**Problem (proven from the operator's real live run, 2026-06-29):** Groww REST
auth succeeds, the live-socket token is issued (`POST /v1/api/apex/v1/socket/
token/create` → 200), NATS connects, then `growwapi.groww.nats_client` logs a
bare empty `Error: ` every ~4s and ZERO ticks flow. The SDK SWALLOWS the real
NATS reject reason, so we cannot tell Groww support WHY the price subjects are
rejected.

**Root cause (VERIFIED against growwapi-1.5.0 + nats-py 2.15.0 source — quoted):**
- `growwapi/groww/nats_client.py:150-151`:
  `async def _on_error_cb(self, e): logger.error("Error: %s", e)` — the bare line.
- The NATS consume loop runs in a **daemon thread** (`NatsClient._consume` →
  `self._loop.run_forever()`, started at `__init__` line 55). The async reject
  callbacks fire INSIDE that thread's event loop — NOT the sidecar's main
  thread — which is exactly why the sidecar's `except` never catches it and
  `feed.consume()` (`feed.py:629-630` = `self._nats_client.consume_thread.join()`)
  just blocks.
- `nats.aio.client.Client._process_err` (read from the wheel): a server
  `-ERR` carrying **"Authorization Violation"** stores `errors.AuthorizationError()`
  in `Client._err` and **closes the connection — it NEVER calls `_error_cb`**, so
  the real reason reaches NO growwapi callback; it survives ONLY in
  `_socket._err` / `_socket.last_error` (a property returning `_err`). A
  **"Permissions Violation"** `-ERR` stores `errors.Error(msg)` in `_err` AND
  calls `_error_cb(err)` (→ the empty growwapi line).

**Hook point (file:line + attribute, ground-truth):**
- `feed._nats_client` → the `NatsClient` (`feed.py:149`).
- `feed._nats_client._socket` → the underlying `nats.aio.client.Client`
  (`nats_client.py:51`).
- `_socket.last_error` (property → `Client._err`) → the reliable, durable
  holder of the REAL reason for BOTH the authorization AND permissions classes.

**The fix (Python sidecar only):** after the feed is constructed + subscribed,
(1) `install_nats_reason_hooks(feed, secrets)` replaces the SDK NatsClient
instance's `_on_error_cb` / `_on_closed_cb` / `_on_disconnected_cb` with
real-detail versions (print `repr(e)` + the socket's stored `last_error`); and
(2) a daemon `nats_reject_poller(feed, secrets)` polls `_socket.last_error`
every 2s and, on a NEW reject-class reason, prints ONE edge-triggered
`GROWW LIVE FEED REJECTED: <reason>` line + the raw `last_error`, then a 60s
heartbeat — never spamming every 4s. The poller is the GUARANTEED backstop
because nats-py captured the original callback references at connect time. Every
secret value is redacted (`_redact`) and length-capped before printing. Both
paths are best-effort: any future-wheel attribute mismatch is caught and logged
(type only), NEVER breaks the feed.

## Edge Cases

- **Future-wheel attribute rename** (`_nats_client` / `_socket` / `last_error`
  gone): `_nats_socket_from_feed` + `install_nats_reason_hooks` catch any
  exception, log the type once, return/skip — the silent-feed watchdog still
  covers "0 ticks". (Tested: missing-attr feed → `None`, no raise.)
- **`AuthorizationError` (terse str):** `_socket_error_detail` uses `repr()`
  first, then appends `str()` when it differs → `AuthorizationError() (nats:
  authorization failed)` — never empty. (Verified live against nats-py.)
- **Permissions Violation with a subject embedding a secret:** subject is
  surfaced, the secret value is `***REDACTED***`. (Verified.)
- **No stored error yet (transient):** `_socket_error_detail` returns `None`;
  poller prints nothing until a real reason appears.
- **Reject then recovers (streams):** poller returns the moment `EMITTED_TOTAL
  > 0` — a feed that streams is not a reject.
- **Repeated identical reason:** edge-triggered — printed once, then a 60s
  heartbeat carrying live emitted/dropped counts (no 4s flood).

## Failure Modes

- Hook install raises → caught, logged (type only), poller is the backstop.
- Socket unreachable → poller no-ops; silent-feed watchdog unaffected.
- nats-py changes `last_error` semantics → `_err` direct-read fallback; if both
  gone, `None` → degraded-but-safe (no crash).
- The reject reason itself may BE an authorization/permissions violation — that
  is the POINT: surface Groww's exact text for support escalation, not hide it.

## Test Plan

- `python3 -m py_compile scripts/groww-sidecar/groww_sidecar.py` → PY_COMPILE_OK
  (the only CI-relevant gate for a Python file; matches the repo's existing
  sidecar verification used by the prior groww-sidecar PRs).
- Functional unit checks against the REAL `nats.errors` types from the pinned
  venv (growwapi==1.5.0, nats-py==2.15.0): AuthorizationError → non-empty
  reject detail; Permissions Violation → subject surfaced + secret redacted;
  None → None; socket reach + hook install + missing-attr degrade.
- Live poller simulation: stored `AuthorizationError()` on a fake socket →
  poller prints `GROWW LIVE FEED REJECTED: AuthorizationError() (nats:
  authorization failed)`.
- Live end-to-end against Groww is NOT runnable here (no creds in this env);
  relied on SDK-source reasoning + the venv-backed unit checks. Honest:
  Verified = py_compile + unit/poller checks; Assumed-pending-operator =
  the exact live Groww reason text (will appear on the operator's next run).

## Rollback

Single-file, additive change. `git revert <sha>` removes the two new functions +
the 6-line wiring in `main()` and restores the prior sidecar exactly. No schema,
no Rust, no config, no data migration — rollback is instantaneous and lossless.

## Observability

The fix IS observability: it converts the SDK's empty `Error:` into operator-
actionable stderr lines the Rust supervisor already captures:
`GROWW LIVE FEED REJECTED: <reason>` (edge-triggered) + raw `NATS last_error ->
<detail>` + a 60s `STILL REJECTED` heartbeat with live emitted/dropped counts.
No new metric/table (sidecar is operator-local per lock §32); the existing
`groww-status.json` honest-feed counts are untouched. Secrets are redacted in
every printed line.

---

## Per-Item Guarantee Matrix — 15-row "100% everything"

| Demand | Mechanical proof artefact | This item |
|---|---|---|
| 100% code coverage | sidecar is operator-local Python; py_compile + venv-backed unit checks of every new fn | covered |
| 100% audit coverage | N/A — sidecar is local/operator-captured (lock §32), no QuestDB audit table | N/A — by lock |
| 100% testing coverage | py_compile + functional unit checks against real nats-py error types | covered |
| 100% code checks | py_compile clean; no secrets in logs (`_redact`); plan-gate + guarantee hook | covered |
| 100% code performance | poll loop is 2s sleep + attribute read — O(1), off the tick hot path entirely | covered |
| 100% monitoring | stderr lines captured by the Rust supervisor; honest-feed status unchanged | covered |
| 100% logging | edge-triggered loud line + raw last_error + heartbeat, all to stderr | covered |
| 100% alerting | `GROWW LIVE FEED REJECTED:` is the operator-facing alert string | covered |
| 100% security | every secret value redacted + length-capped before any print | covered |
| 100% security hardening | best-effort try/except so a bad wheel can never crash/leak | covered |
| 100% bugs fixing | fixes the empty-`Error:` blindness; reasoning grounded in SDK source | covered |
| 100% scenarios covering | auth-violation / permissions-violation / transient / recover / rename | covered |
| 100% functionalities covering | every new pub fn (`install_nats_reason_hooks`, `nats_reject_poller`, `_socket_error_detail`, `_nats_socket_from_feed`, `_is_reject_reason`) is wired + unit-checked | covered |
| 100% code review | adversarial self-review of redaction + thread-safety + degrade paths | covered |
| 100% extreme check | venv-backed proof against the EXACT pinned wheels (1.5.0 / 2.15.0) | covered |

## Per-Item Guarantee Matrix — 7-row "Resilience"

| Demand | Honest envelope | This item |
|---|---|---|
| Zero ticks lost | No tick path touched; poller is read-only, off the NDJSON write path | no new drop path |
| WS never disconnects | Diagnostic only — surfaces WHY Groww rejects; does not alter reconnect | reconnect untouched |
| Never slow/locked/hanged | 2s-sleep daemon thread; zero hot-path work; exits on first real tick | safe |
| QuestDB never fails | N/A — sidecar does not touch QuestDB | N/A |
| O(1) latency | poll = one attribute read; no per-tick cost | O(1) |
| Uniqueness + dedup | N/A — no new persisted rows | N/A |
| Real-time proof | edge-triggered reject line + 60s heartbeat = real-time operator proof | covered |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage:
py_compile passes; every new helper is proven against the EXACT pinned wheels
(growwapi==1.5.0, nats-py==2.15.0) in a venv — AuthorizationError and
Permissions-Violation both yield a NON-EMPTY, secret-redacted reason and trip
the `GROWW LIVE FEED REJECTED:` edge-trigger. The hook + poller are best-effort:
a future-wheel attribute rename degrades safely (caught, logged type-only,
silent-feed watchdog still covers 0-ticks) and NEVER crashes the feed. NOT
claimed: the exact live Groww reason text — that is precisely what this change
surfaces on the operator's next live run; we make NO claim about what Groww's
reject text WILL say, only that whatever it is will now be printed verbatim
(redacted) instead of an empty `Error:`.

## Plan Items

- [x] Add `install_nats_reason_hooks` + `nats_reject_poller` + helpers
  (`_nats_socket_from_feed`, `_socket_error_detail`, `_is_reject_reason`) +
  the `_REJECT_MARKERS` / `NATS_REASON_*` constants.
  - Files: scripts/groww-sidecar/groww_sidecar.py
- [x] Wire the hooks + poller in `main()` after subscribe (alongside the
  silent-feed watchdog, armed once per process).
  - Files: scripts/groww-sidecar/groww_sidecar.py
- [x] Verify: `python3 -m py_compile scripts/groww-sidecar/groww_sidecar.py`
  → PY_COMPILE_OK + venv-backed unit/poller checks pass.

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Groww rejects with Authorization Violation | `GROWW LIVE FEED REJECTED: AuthorizationError() (nats: authorization failed)` (verified) |
| 2 | Groww rejects with Permissions Violation incl. subject + secret | subject surfaced, secret `***REDACTED***`, REJECTED line fires (verified) |
| 3 | No stored error yet | poller silent until a real reason appears (verified) |
| 4 | Feed recovers and streams | poller exits on first emitted tick (verified) |
| 5 | Future wheel renames `_socket`/`last_error` | degrade safely, no crash (verified) |
