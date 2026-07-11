# Implementation Plan: Feed Control dashboard link in instance-start Telegram

**Status:** APPROVED
**Date:** 2026-06-29
**Approved by:** Parthiban (operator) — grounded directive 2026-06-29

> Operator directive (2026-06-29): when the AWS EC2 box auto-starts each
> morning, the existing "instance start triggered" Telegram must ALSO carry a
> tappable link to tickvault's own Feed Control web page so the operator can
> open it from his phone. He explicitly accepted that this exposes the
> server's public IP + port into his own private Telegram bot.

## Design

The `ping`-mode branch of the start-watchdog Lambda
(`deploy/aws/lambda/start-watchdog/handler.py`) publishes the
"Instance start triggered" SNS message at 08:30 IST. Today that branch only
creates an SNS client and sends fixed text — it never reads the instance's
public IP.

Change: in the `ping` branch, create a boto3 EC2 client, resolve the
instance's CURRENT public IP via `ec2.describe_instances(...) ->
Reservations[0].Instances[0].PublicIpAddress`, build the dashboard URL
`http://<public_ip>:<port>/feeds`, and append ONE line to the existing
message body. Port comes from a new Lambda env var `DASHBOARD_PORT`
(default `3001`, wired in `start-watchdog-lambda.tf`).

**Why `describe_instances.PublicIpAddress` (not a fixed EIP):** the operator
excluded the Elastic IP for the no-orders data-pull phase
(`daily-universe-scope-expansion-2026-05-27.md` §7 — `enable_eip = false`),
so the public IP is auto-assigned and CHANGES on every start. Reading it
live at trigger time via `describe_instances` is the robust source. The
Lambda IAM role already grants `ec2:DescribeInstances` on `*`
(`start-watchdog-lambda.tf` lines 49-52, used by `check`/`stop_check`), so
**no new IAM permission is required**.

**Telegram style:** the existing message text is kept intact; one extra
line `📊 Dashboard: http://<ip>:<port>/feeds` is appended. A bare URL is an
allowed actionable link under the 10 Telegram commandments (no file path,
no library name, no version number in the body).

**Security note:** this intentionally reverses the IP-masking precedent in
`crates/core/src/notification/events.rs` (`mask_ip_for_notification`),
scoped ONLY to this single start ping, per the dated operator directive. No
secret is exposed — the `/feeds` HTML shell is public and every toggle it
performs is bearer-authed.

## Edge Cases

- **Public IP missing/None** (instance not yet assigned an IP, or
  `describe_instances` returns no `PublicIpAddress`): omit the dashboard line
  and append a clear "dashboard link not ready yet" note instead. The start
  message itself always sends.
- **`describe_instances` raises** (transient AWS error / permission gap): the
  IP-resolver helper catches and returns `None` → same graceful "not ready"
  path. The ping NEVER crashes.
- **`DASHBOARD_PORT` unset**: defaults to `3001`.

## Failure Modes

- A watchdog that crashes is worse than one that omits the link. The new EC2
  read is wrapped so any failure degrades to the no-link message — the
  positive "start triggered" signal is never lost.
- `ALERTS_TOPIC_ARN` unset → existing `_publish` already logs + no-ops; the
  IP read is skipped harmlessly.

## Test Plan

`deploy/aws/lambda/start-watchdog/test_handler.py`:
- `test_ping_includes_dashboard_link`: ping with an EC2 client returning a
  public IP → message contains `/feeds` and `3001`.
- `test_ping_missing_public_ip_degrades_gracefully`: ping with no public IP →
  message still sends, contains no `http://...:3001/feeds`, and the start text
  remains.
- `test_ping_describe_error_degrades_gracefully`: ping where
  `describe_instances` raises → still publishes, no link, no crash.
- Existing `test_ping_publishes_positive_signal` updated so its fake `boto3`
  returns an EC2 client for `svc == "ec2"`.

Run: `python -m pytest deploy/aws/lambda/start-watchdog/test_handler.py`
(fallback `python -m py_compile` if pytest unavailable).

## Rollback

`git revert` the single squash commit. The Lambda message reverts to the
prior fixed text; the `DASHBOARD_PORT` env var becomes inert. No state, no
migration, no Rust rebuild.

## Observability

The dashboard link rides on the EXISTING 08:30 IST "Instance start triggered"
Telegram (no new alert class, no new metric). On the IP-missing path the
operator sees an explicit "not ready yet" line rather than a silent omission
(false-OK avoidance). CloudWatch Logs for the Lambda record the resolved IP
at INFO via the existing logger.

## Plan Items

- [x] Add EC2 public-IP resolver + dashboard-link line to the `ping` branch
  - Files: deploy/aws/lambda/start-watchdog/handler.py
- [x] Wire `DASHBOARD_PORT` env var (default 3001)
  - Files: deploy/aws/terraform/start-watchdog-lambda.tf
- [x] Tests for link present + IP-missing + describe-error graceful paths
  - Files: deploy/aws/lambda/start-watchdog/test_handler.py
  - Tests: test_ping_includes_dashboard_link,
    test_ping_missing_public_ip_degrades_gracefully,
    test_ping_describe_error_degrades_gracefully

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 08:30 ping, box has public IP 13.x.x.x | message has `📊 Dashboard: http://13.x.x.x:3001/feeds` |
| 2 | 08:30 ping, no public IP yet | message sends, no link, "not ready yet" line |
| 3 | 08:30 ping, describe_instances errors | message sends, no link, no crash |

## Per-Item / Per-Plan Guarantee Matrix

This plan carries the project-wide guarantee contract by cross-reference to
`.claude/rules/project/per-wave-guarantee-matrix.md` (the 15-row "100% everything"
matrix + the 7-row resilience matrix), mirroring
`.claude/plans/active-plan-groww-log-hardening.md`. This is a **Python AWS-Lambda-only**
change — it touches NO Rust crate, NO hot path, NO tick/persist/order path — so the
hot-path / DHAT / Criterion / coverage-gate / DEDUP / Rust-ratchet rows are honestly
**N/A** with the reason stated. The real, applicable proof is the Lambda pytest suite +
`py_compile` + the graceful IP-missing degrade + Telegram-commandments compliance.

### 15-row "100% everything" matrix (honest, per-row)

| Demand | Proof for THIS change |
|---|---|
| 100% code coverage | N/A for the Rust `scripts/coverage-gate.sh` (no crate touched). Applicable proof: the 4 Lambda pytest cases cover every new branch — link present, IP-missing degrade, describe-error degrade, and the updated positive-signal test (`test_handler.py`). |
| 100% audit coverage | N/A — no new typed event, no `<event>_audit` QuestDB table; the link rides the EXISTING "Instance start triggered" SNS message. |
| 100% testing coverage | `python -m pytest deploy/aws/lambda/start-watchdog/test_handler.py` (fallback `python -m py_compile`). Rust 22-category battery N/A (no crate touched). |
| 100% code checks | Plan-gate + per-item-guarantee + secret-scan apply; Rust banned-pattern / pub-fn guards N/A (Python-only diff). |
| 100% code performance | N/A — cold-path Lambda invoked once/day at 08:30 IST; no hot path, no DHAT/Criterion budget applies. |
| 100% monitoring | Existing CloudWatch Logs for the Lambda record the resolved IP at INFO; no new metric needed. |
| 100% logging | `logger` (Python `logging`) only; no secret logged (the public IP is operator-authorized for the body per the dated directive). |
| 100% alerting | N/A — reuses the EXISTING 08:30 IST start ping; no new alert class. |
| 100% security | The IP exposure is a dated operator-authorized scope reversal of `mask_ip_for_notification`, limited to this one start ping; no secret/token exposed (the `/feeds` shell is public, toggles bearer-authed). No new IAM permission (existing `ec2:DescribeInstances`). |
| 100% security hardening | EC2 read wrapped so any failure degrades to the no-link message; the ping never crashes (fail-soft). |
| 100% bug fixing | 3 graceful-degrade paths (IP `None`, `describe_instances` raises, `DASHBOARD_PORT` unset) each have a dedicated test. |
| 100% scenarios covering | The 3-row Scenarios table above maps 1:1 to the 3 degrade/link tests. |
| 100% functionalities covering | Every new code path (IP resolver helper, link append, "not ready yet" note) is exercised by a test. |
| 100% code review | Diff is plan-gated + reviewer-visible; small Python surface. |
| 100% extreme check | The pytest suite fails CI on regression of any of the link/degrade paths. |

### 7-row "Resilience demand" matrix (honest, per-row)

| Demand | Proof for THIS change |
|---|---|
| Zero ticks lost | N/A — no tick path touched; ring→spill→DLQ untouched. |
| WS never disconnects | N/A — no WebSocket code touched; `SubscribeRxGuard` / pool watchdog untouched. |
| Never slow/locked/hanged | N/A — no hot path; the once-daily Lambda EC2 read is bounded by boto3 defaults and wrapped to degrade. |
| QuestDB never fails | N/A — no QuestDB write touched; schema self-heal untouched. |
| O(1) latency | N/A — no Rust lookup/hot path; the single `describe_instances` call is a cold-path AWS API call, honestly NOT claimed O(1). |
| Uniqueness + dedup | N/A — no DEDUP key / composite key touched. |
| Real-time proof | The dashboard link is verified by the pytest assertions on the published SNS message body. |

### Honest 100% claim

100% inside the tested envelope, with ratcheted regression coverage: the 4 Lambda
pytest cases (link-present + IP-missing-degrade + describe-error-degrade + updated
positive-signal) fail CI on regression, and `py_compile` guards syntax. This is a
Python-Lambda-only, cold-path change that cannot affect ticks, persistence, the live
feed, or any order path; the Rust hot-path / DHAT / Criterion / coverage-gate / DEDUP
rows are honestly N/A because no crate is touched.
