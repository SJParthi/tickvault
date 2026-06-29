# Implementation Plan: Redact Groww access token + SDK-DEBUG creds in the sidecar before they reach logs

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — grounded directive, this session

> **Cross-ref:** the FULL 15-row + 7-row guarantee matrices live in
> `.claude/rules/project/per-wave-guarantee-matrix.md`. This plan carries them
> inline below (§ Guarantee Matrices) and confirms ALL 15 + 7 rows apply to
> every item here.

---

## Context (why)

The just-merged #1254 patch surfaces the Groww sidecar's NATS reject/error text
to stderr. The Rust supervisor (`crates/app/src/groww_sidecar_supervisor.rs`
~490-505) forwards EVERY child stderr line into `tracing::error!`/`info!` → the
5-sink fan-out → CloudWatch + `errors.jsonl`. Two confirmed secret-leak findings
from the 2026-06-29 adversarial Groww audit (`confirmed[]`, both MEDIUM):

1. **Groww access token NOT in the sidecar redaction set**
   (`scripts/groww-sidecar/groww_sidecar.py:859` `secrets = (api_key,
   totp_secret)`; access token acquired :886, used :900, hooks armed :963/:966;
   `_redact` :445-460). A NATS-over-WS auth/handshake error's repr can embed the
   CONNECT frame / auth payload carrying the bearer token → it reaches CloudWatch
   un-masked, violating "access token never logged" (`rust-code.md` Logging).
2. **SDK (`growwapi`/`nats`) DEBUG logging routes to stderr WITHOUT passing
   through `_redact`** (:59-70 `basicConfig DEBUG` + `setLevel(DEBUG)`). `_redact`
   only runs on the sidecar's own `print()` lines — it cannot intercept records
   the SDK emits through Python's `logging` module. A credential the SDK logs at
   DEBUG (connect URL, exception repr) leaks verbatim.

Same root cause: redaction was per-print-call-site, not structural. I introduced
the surfacing risk with #1254, so this is priority.

## Design

Scope is `scripts/groww-sidecar/groww_sidecar.py` ONLY (Python sidecar). No
change to the subscribe request, watch-list, segment handling, or the
capture-at-receipt NDJSON contract. Three layers:

1. **Access token in the live redaction set.** Change `secrets` from a tuple to
   a MUTABLE list seeded `[api_key, totp_secret]`. Add the bearer token to it the
   moment it is acquired (`_add_secret(secrets, access_token)` right after
   `get_access_token`), and again on every refresh (the reconnect loop re-acquires
   on auth failure → re-adds). The NATS reason hooks + reject poller capture
   `secrets` by reference, so the token masks in them too with no re-arming.
2. **Structural backstop in `_redact`.** After the exact-value pass, mask any
   JWT-shaped substring (`eyJ...\....\....`) and any `bearer <token>` so an
   unanticipated/refreshed token shape is scrubbed even if not in the set.
3. **SDK-DEBUG structural scrub.** A `logging.Filter` (`_RedactingLogFilter`)
   that runs `record.getMessage()` (msg % args) + `exc_text` through `_redact`
   before emit, then clears `record.args`. Attached at the ROOT-handler level
   (catches every record reaching the stderr handler, including arbitrary SDK
   child loggers e.g. `growwapi.feed`, `nats.aio.client` via propagation) AND on
   the `growwapi`/`nats` loggers directly. If no handler can be filtered, FALLBACK
   to demoting those loggers to WARNING (no DEBUG leak). The #1254 reject-reason
   surfacing is OUR `print()` path, not the SDK logger, so it stays intact.

## Edge Cases

- **Token refresh mid-session.** `access_token = None` on auth failure → next
  loop re-acquires → `_add_secret` appends the NEW token; the OLD stale value
  stays in the set (harmless, still masked). Hooks hold the list by reference.
- **Token NOT in the set (race / new shape).** Structural JWT/bearer masks fire.
- **SDK logs on a child logger that does not propagate to root.** Direct filter
  on `growwapi`/`nats` is the second layer; WARNING demotion is the floor.
- **Short token / empty value.** `_add_secret` skips values < 8 chars / empty so
  it never masks incidental substrings; `_redact` skips falsy secrets.
- **A redaction bug inside the filter.** `filter()` catches all exceptions and
  emits a suppressed-line placeholder rather than dropping or leaking the record.

## Failure Modes

- **Filter-install failure** → logged (type only) + WARNING-demotion fallback;
  feed never breaks (best-effort, mirrors the existing hook-install pattern).
- **Regex catastrophic backtracking** → the two regexes are linear (bounded
  char classes, no nested quantifiers) — no ReDoS.
- **Over-redaction** masks a benign JWT-shaped string in a log → acceptable: a
  false-positive mask never loses a tick or breaks triage (cause text remains).
- **Under-redaction** is the failure we are removing; the self-test proves both
  the exact-value and structural paths fire.

### Adversarial security-review findings (2026-06-29) — disposition

- **exc_info raw tuple not cleared (MEDIUM) — FIXED.** `_RedactingLogFilter` now
  sets `record.exc_info = None` (and a secret-free `exc_text` marker) so a
  formatter that renders `exc_info` fresh cannot re-leak an unredacted traceback.
  Asserted by the self-test (`rec2.exc_info is None`).
- **6-digit TOTP code not masked (MEDIUM) — ACCEPTED, documented.** The generated
  TOTP *code* (not the seed) is ephemeral (30s) and only 6 digits. `_add_secret`'s
  8-char floor and the regex masks deliberately exclude it: masking the literal
  `"123456"` everywhere would corrupt legitimate numeric log output (prices,
  counts) in a tick sidecar — over-redaction is a worse failure here than a 30s
  exposure of an already-rotated code. The TOTP *seed* (`totp_secret`) IS masked.
- **SDK logging during `import growwapi` is pre-filter (MEDIUM) — ACCEPTED.** The
  filter installs in `main()` after import. At import time NO credential exists
  yet (api_key/totp/token are read inside `main()`), so there is nothing to leak;
  the structural masks cannot apply earlier but have nothing to scrub.
- **Poller polls the first feed's socket after reconnect (LOW) — OUT OF SCOPE.**
  Pre-existing #1254 behaviour; this PR must keep #1254 intact and not touch the
  reconnect/subscribe path. Tracked as a follow-up.

## Test Plan

- `python3 -m py_compile scripts/groww-sidecar/groww_sidecar.py` → PY_COMPILE_OK.
- Inline `_selftest_redaction()` guarded behind `--selftest` (NEVER runs in prod;
  prod = no args → `main()`). Asserts: (a) the access token is added to the live
  set and masked by exact value; (b) the api_key value is masked; (c) an
  unanticipated JWT-shaped token NOT in the set is masked structurally; (d) the
  `_RedactingLogFilter` scrubs a synthetic `growwapi.feed` DEBUG record (token
  gone, `args` cleared). Run via stubbed-deps harness (pyotp/growwapi absent in
  this env). Result: `groww sidecar redaction self-test: PASS`.
- 22-test taxonomy applicable here: **unit** (the self-test asserts), **security**
  (secret-leak closure is the change), **edge/boundary** (short/empty/new-shape
  token). Others (loom/dhat/fuzz/mutation/property) N/A — pure Python string
  redaction, no Rust hot path, no concurrency primitive added.

## Rollback

Single-file Python change, no schema/contract change. Revert the commit (or this
file's diff) → behaviour returns to pre-fix exactly. The 3 layers are additive
and independently removable; the `secrets` tuple→list change is the only
structural edit and is internal to `main()`.

## Observability

The fix REMOVES a leak path (no new metric needed). It keeps every existing
diagnostic: the NATS reject surfacing (#1254) still prints the permission-
violation reason (now scrubbed), the SDK DEBUG diagnostics still flow (scrubbed),
the auth-OK/len logs are unchanged. One new stderr breadcrumb on boot
("SDK-log redaction filter attached") so the operator can confirm the scrub is
live. No counter/gauge/audit-table change — this is a redaction hardening of an
existing log path, not a new event.

---

## Guarantee Matrices (per `per-wave-guarantee-matrix.md` — all rows apply)

### 15-row "100% everything" matrix

| Demand | Proof for this item |
|---|---|
| 100% code coverage | `_selftest_redaction()` exercises both `_redact` paths + the filter; no new uncovered Rust branch |
| 100% audit coverage | N/A — redaction of an existing log path; no new typed event/audit table |
| 100% testing coverage | unit (self-test) + security + edge; loom/dhat/fuzz/mutation N/A (pure Python string fn) |
| 100% code checks | `py_compile` clean; banned-pattern/secret-scan run on push |
| 100% code performance | N/A — sidecar I/O path, not the Rust hot path; two linear regexes, no ReDoS |
| 100% monitoring | existing diagnostics preserved; one boot breadcrumb confirms scrub live |
| 100% logging | the change IS the logging hardening — secrets masked before emit |
| 100% alerting | N/A — no new failure mode; #1254 reject alert preserved (scrubbed) |
| 100% security | closes a confirmed secret-leak (access token + SDK-DEBUG creds) |
| 100% security hardening | structural JWT/bearer masks + filter = defense-in-depth beyond known values |
| 100% bugs fixing | both audit-confirmed MEDIUM findings remediated at the source |
| 100% scenarios covering | refresh, race/new-shape, child-logger, short/empty token all handled |
| 100% functionalities covering | new fns (`_add_secret`, `_RedactingLogFilter`, `_install_sdk_log_redaction`) all called + asserted |
| 100% code review | adversarial audit synthesis is the review input; self-test proves the fix |
| 100% extreme check | self-test fails loudly if either redaction path regresses |

### 7-row "Resilience demand" matrix

| Demand | Honest envelope for this item |
|---|---|
| Zero ticks lost | UNCHANGED — no edit to the capture-at-receipt NDJSON path; redaction only touches log strings |
| WS never disconnects | UNCHANGED — no edit to the NATS connect/subscribe/consume path |
| Never slow/locked/hanged | filter is per-log-record string work, off the tick path; two linear regexes |
| QuestDB never fails | N/A — no persistence path touched |
| O(1) latency | UNCHANGED tick path; redaction is per-log-line, not per-tick |
| Uniqueness + dedup | UNCHANGED — no DEDUP key / composite-key touched |
| Real-time proof | `--selftest` is the real-time proof the scrub fires |

---

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the
`--selftest` asserts that (a) the access token, once acquired, is masked by exact
value, (b) the api_key is masked, (c) an unanticipated JWT-shaped token is masked
structurally, and (d) an SDK DEBUG record is scrubbed before emit with `args`
cleared — so a NATS auth error or SDK DEBUG line can no longer write the Groww
bearer token to CloudWatch. Beyond the envelope (a credential in a shape neither
in the `secrets` set nor JWT/bearer-shaped, logged on a non-propagating child
logger we could not filter), the WARNING-level fallback demotes the SDK loggers
so unredacted DEBUG protocol/auth detail is simply not emitted. This is a
redaction hardening — it does not, and does not claim to, make any third-party
SDK "never log a secret"; it scrubs every path that reaches the supervisor.

## Plan Items

- [x] Item 1 — Add access token to the live (mutable) redaction set on
  acquire/refresh; pass the live set to the NATS hooks + poller (by reference).
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: `_selftest_redaction` (exact-value mask of token + api_key)
- [x] Item 2 — Belt-and-suspenders structural masks in `_redact` (JWT + bearer).
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: `_selftest_redaction` (structural JWT mask, token not in set)
- [x] Item 3 — `_RedactingLogFilter` on root handler + `growwapi`/`nats` loggers,
  WARNING-demotion fallback; #1254 reject surfacing preserved.
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: `_selftest_redaction` (SDK DEBUG record scrubbed, args cleared)
- [x] Item 4 — `--selftest` entry path; prod (no args) unchanged.
  - Files: scripts/groww-sidecar/groww_sidecar.py
  - Tests: py_compile + stubbed-deps self-test run → PASS

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | NATS auth-reject repr embeds the bearer token | token masked (in set + JWT shape) |
| 2 | SDK logs connect URL with token at DEBUG | record scrubbed before emit |
| 3 | Token refreshes mid-session | new token added to live set; both masked |
| 4 | Token shape unanticipated, not in set | structural JWT/bearer mask fires |
| 5 | Filter cannot attach to a handler | SDK loggers demoted to WARNING |
| 6 | #1254 reject reason line | still printed, secrets scrubbed |
