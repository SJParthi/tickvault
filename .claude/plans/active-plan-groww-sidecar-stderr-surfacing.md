# Implementation Plan: Surface Groww sidecar stderr/diagnostics to tracing + feed_health + Telegram

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — "fix everything", this session

> **Cross-reference:** the mandatory 15-row + 7-row guarantee matrix lives in
> `.claude/rules/project/per-wave-guarantee-matrix.md`; this plan fills it in
> honestly below (cold-path: DHAT/hot-path rows N/A-with-reason).

## Design

**The verified bug (origin/main 74c368af):** `crates/app/src/groww_sidecar_supervisor.rs`
builds the Groww Python sidecar `Command` (~line 474) and `spawn()`s it WITHOUT
`.stdout(Stdio::piped())`/`.stderr(Stdio::piped())`. `supervise_child` (~line 379)
only `child.wait()`s. So the sidecar INHERITS the app's stdio — every diagnostic
line it prints (the SDK's bare `Error:`, the `SILENT FEED … account lacks live
market-data feed entitlement` watchdog line, `groww sidecar error [auth] …`, the
redacted traceback) lands ONLY in container/journald logs and NEVER reaches
`tracing`, Telegram, `feed_health`, or the `/feeds` dashboard. The operator is
blind to WHY Groww has 0 ticks.

**The fix (cold-path only; no hot-path, no indicator/strategy [§28 frozen]):**

1. Pipe the child's stdout + stderr (`.stdout(Stdio::piped()).stderr(Stdio::piped())`),
   `take()` both after spawn, and in `supervise_child` drain each via a tokio
   `BufReader::new(pipe).lines()` task. Each line is:
   - forwarded to `tracing` at the right level (`info!`/`warn!`/`error!`) so it
     reaches the 5-sink pipeline → CloudWatch; and
   - CLASSIFIED via a PURE O(1) helper `classify_sidecar_line(&str) -> SidecarLineClass`
     (case-insensitive substring match against the REAL strings the sidecar prints,
     verified in `scripts/groww-sidecar/groww_sidecar.py`).
2. On the FIRST `EntitlementRejected` / `AuthRejected` / `Error` classification
   (edge-triggered — once per running-child instance), the supervisor:
   - calls `feed_health.set_auth_rejected(Feed::Groww, true)` so the `/feeds`
     dashboard shows the actionable Down + reason; and
   - fires ONE typed `NotificationEvent::GrowwSidecarRejected { reason }` (Telegram)
     carrying a fixed plain-English reason for the class (10 commandments: plain
     English, no lib names, no file paths, no version numbers, severity emoji).
3. The child's own lines are already secret-redacted by the sidecar
   (`_exception_detail`/`_redact` mask api_key + TOTP). The supervisor forwards
   them verbatim and NEVER echoes env/creds itself.

**Why feed_health is threaded in (not a notifier-only path):** `feed_health` is
already available at the supervisor spawn site (line 360, same place the bridge +
activation watcher get it). The notifier (`fast_notifier`) is built later in the
boot flow, so the supervisor spawn is RELOCATED to immediately after the notifier
is finalized, cloning the two outer `Arc`s in. This keeps the bridge + activation
spawns untouched.

**The classifier strings (from `groww_sidecar.py`, case-insensitive substring):**

| Real sidecar line (verbatim fragment) | Class |
|---|---|
| `groww sidecar error [auth]` | AuthRejected |
| `account lacks a LIVE market-data feed entitlement` | EntitlementRejected |
| `SILENT FEED` / `STILL SILENT` | EntitlementRejected |
| `groww sidecar error [feed-connect]` / `[subscribe]` / `[consume]` | Error |
| `Error:` (the SDK's bare NATS permissions line) | Error |
| `subscribed N stocks` / `subscribed … indices` | Subscribed |
| `groww auth OK` / `appending NDJSON` / `→ appending` | Info (Streaming-ish positive) |
| `DIAGNOSTIC` | Info |
| anything else | Info |

`AuthRejected`/`EntitlementRejected`/`Error` → `feed_health.set_auth_rejected(true)` +
ONE Telegram event. `Subscribed`/`Streaming`/`Info` → tracing only.

## Edge Cases

- **Burst of identical error lines** (a fast-looping `consume()` re-raising):
  the Telegram + feed_health side-effect is EDGE-TRIGGERED per running child
  (a `bool` latch in `supervise_child`), so one running child fires at most one
  Telegram even if it prints the same `Error:` 100×.
- **Pipe closes when child exits:** the `lines()` drain tasks end naturally on
  EOF; `supervise_child` aborts them on the child-exit / disable path so no
  task leaks across restarts.
- **`take()` returns `None`** (pipe not configured): defensively skip that drain
  task; the other side still drains.
- **Empty / whitespace line:** classified `Info`, forwarded at `info!` — no
  side-effect.
- **Disable mid-stream:** the existing disable branch kills the child; the drain
  tasks end on pipe EOF and are aborted.
- **Notifier is `None`** (defensive/test build): the supervisor skips the
  Telegram emit; feed_health + tracing still fire.

## Failure Modes

- **A line embeds a credential** despite the sidecar's redaction: the supervisor
  forwards the child's OWN (already-redacted) text and never interpolates env —
  the typed `NotificationEvent::GrowwSidecarRejected` reason is a FIXED
  per-class `&'static str` mapped to a `String`, NOT the raw line, so no runtime
  child text reaches Telegram (defense-in-depth; same pattern as the fixed
  `auth_rejected` reason in `feed_health`).
- **Drain task panics:** it is a child of the supervise loop; on the next child
  restart a fresh pair is spawned. No silent loss of the supervise loop itself.
- **False-OK avoidance (audit Rule 11):** the supervisor only flips
  `auth_rejected=true` on a genuine reject/error class — never on `Subscribed`
  or `Info`. A healthy sidecar never trips the rejected state. (The bridge's
  existing successful-auth path clears `auth_rejected` on streaming.)

## Test Plan

- `crates/app` unit tests on the PURE classifier `classify_sidecar_line`:
  - `permission` / `authorization` / `account lacks … entitlement` → EntitlementRejected
  - `groww sidecar error [auth]` → AuthRejected
  - `subscribed 765 stocks + 2 indices` → Subscribed
  - a real LTP/NDJSON line / `groww auth OK` → Info
  - bare `Error:` → Error
  - `SILENT FEED` / `STILL SILENT` → EntitlementRejected
  - case-insensitivity proof
  - `class.triggers_alert()` true only for AuthRejected/EntitlementRejected/Error
- `crates/core` render test for the new event:
  `NotificationEvent::GrowwSidecarRejected { reason }.to_message()` contains the
  reason + the severity emoji, `topic()` == "GrowwSidecarRejected",
  `severity()` == High.
- Source-scan ratchet in the supervisor tests: assert the spawn pipes stdout +
  stderr and that `supervise_child` drains them (the supervise loop is a
  TEST-EXEMPT process driver, so pin the wiring by source-scan — mirrors the
  existing `test_supervisor_injects_status_file_env`).
- `cargo test -p tickvault-app` (+ `-p tickvault-core` for the event) — paste
  results in the PR.

## Rollback

- The change is additive + cold-path. To revert: drop the
  `.stdout/.stderr(Stdio::piped())` + the drain tasks + the new event variant +
  the supervisor `feed_health`/`notifier` params, restoring the inherited-stdio
  behaviour. No schema, no DEDUP key, no hot-path, no config flag — a single
  `git revert` of the commit fully restores prior behaviour. Groww default-OFF,
  so prod behaviour is unchanged until Groww is enabled.

## Observability

- **tracing:** every sidecar line now reaches `tracing` (info/warn/error) → the
  5-sink pipeline → CloudWatch. Reject/error classes log at `error!` (the
  existing `code = …` tag-guard does NOT require a code here because the message
  does not mention a known ErrorCode prefix; no new ErrorCode is invented).
- **feed_health:** `set_auth_rejected(Feed::Groww, true)` on a reject/error class
  → the `/feeds` dashboard + `GET /api/feeds/health` show the actionable Down.
- **Telegram:** ONE typed `NotificationEvent::GrowwSidecarRejected { reason }`
  (Severity::High) per running-child reject — the operator now sees WHY Groww
  has 0 ticks, not just that it does.
- No new Prometheus counter (cold-path diagnostic; the existing feed-health
  signals + the Telegram event are the operator surface). N/A — not a hot path.

## Per-Item Guarantee Matrix (cross-ref `.claude/rules/project/per-wave-guarantee-matrix.md`)

### 15-row 100% matrix
| Demand | This item |
|---|---|
| 100% code coverage | classifier + event render fully unit-tested; supervise wiring source-scanned (TEST-EXEMPT process driver) |
| 100% audit coverage | N/A — diagnostic surfacing; no new audit table (uses existing feed_health + Telegram) |
| 100% testing coverage | unit (classifier, event render) + source-scan ratchet |
| 100% code checks | banned-pattern + pub-fn-test + fmt + clippy run pre-PR |
| 100% code performance | N/A — cold path (sidecar diagnostics, not the tick hot path); classifier is O(1) substring |
| 100% monitoring | tracing → CloudWatch + feed_health dashboard + Telegram event |
| 100% logging | every line forwarded at info/warn/error |
| 100% alerting | ONE typed Telegram event on reject/error class (edge-triggered) |
| 100% security | child's own redacted text forwarded; supervisor never echoes env/creds; event reason is fixed `&'static str` |
| 100% security hardening | no new attack surface; reason text is per-class fixed, not raw child input |
| 100% bugs fixing | fixes the verified "operator blind to Groww 0-ticks cause" bug |
| 100% scenarios covering | edge cases enumerated above (burst, pipe-close, disable, None pipe, None notifier) |
| 100% functionalities covering | every new pub fn (classifier, event) has a test; supervise wiring has a call site |
| 100% code review | adversarial review of the diff before opening the PR |
| 100% extreme check | source-scan ratchet fails the build if the pipes/drain are removed |

### 7-row resilience matrix
| Demand | This item |
|---|---|
| Zero ticks lost | No tick path touched — diagnostics only; ring→spill→DLQ unchanged |
| WS never disconnects | N/A — Python sidecar diagnostics, not a WS path |
| Never slow/locked/hanged | drain tasks are non-blocking, end on pipe EOF; no hot-path alloc |
| QuestDB never fails | N/A — no persistence change |
| O(1) latency | classifier is O(1) case-insensitive substring; cold path |
| Uniqueness + dedup | N/A — no DEDUP key change |
| Real-time proof | tracing + feed_health + Telegram fire in real time on each line |

## Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the
sidecar's own (already-redacted) diagnostic lines are forwarded to tracing →
CloudWatch, classified by an O(1) unit-tested pure function, and on a genuine
auth/entitlement/error class the supervisor marks Groww rejected in feed_health
(actionable Down on the `/feeds` dashboard) and fires ONE typed Telegram event.
This does NOT change tick capture, the WAL→ring→spill→DLQ chain, or any DEDUP
key; Groww is default-OFF so prod behaviour is byte-identical until enabled.

## Plan Items

- [ ] Add `SidecarLineClass` + pure `classify_sidecar_line` to the supervisor; pipe stdout/stderr; drain via BufReader lines tasks; edge-triggered feed_health + Telegram on reject/error.
  - Files: crates/app/src/groww_sidecar_supervisor.rs, crates/app/src/main.rs
  - Tests: test_classify_sidecar_line_*, test_sidecar_line_class_triggers_alert, test_supervisor_pipes_and_drains_child_stdio
- [ ] Add typed `NotificationEvent::GrowwSidecarRejected { reason }` (to_message/topic/severity) + render test.
  - Files: crates/core/src/notification/events.rs
  - Tests: test_groww_sidecar_rejected_renders_reason_and_topic_and_severity

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Sidecar prints `SILENT FEED … account lacks … entitlement` | tracing warn + feed_health Down + ONE Telegram "Groww live feed rejected — account lacks live market-data feed entitlement" |
| 2 | Sidecar prints `groww sidecar error [auth] …` | error tracing + feed_health Down + ONE Telegram "Groww live feed rejected — authentication rejected" |
| 3 | Sidecar prints `subscribed 765 stocks + 2 indices` | info tracing only, no Telegram, no feed_health flip |
| 4 | Same `Error:` line printed 100× by one child | exactly ONE Telegram + ONE feed_health flip (edge-triggered) |
| 5 | Groww default-OFF | sidecar never spawned; zero behaviour change |
