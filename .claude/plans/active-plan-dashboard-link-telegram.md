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
