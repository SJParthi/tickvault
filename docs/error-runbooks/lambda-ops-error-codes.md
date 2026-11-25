# LAMBDA-* — AWS Lambda operations error codes

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > this file.
> **Added:** 2026-09-05.
> **Scope:** every `error!` in `crates/aws-lambdas/src/**`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs` requires
> every code string below to appear here verbatim; `runbook_path()` for all
> nine variants points at this file.

---

## Why this family exists

`crates/aws-lambdas` was **never scanned** by `error_code_tag_guard`, which
walked a hardcoded six-name crate list against an eight-crate workspace. The
guard reported success having never looked at it — which is worse than
failing, because the output asserted completeness.

**MEASURED 2026-09-05: 44 production `error!` sites, exactly ONE carrying a
`code =` field.** They sit in the components that page the operator:

| file | uncoded sites | what it does |
|---|---:|---|
| `start_watchdog.rs` | 11 | starts the prod box every weekday morning, retries, verifies the stop |
| `operator_control.rs` | 10 | the operator control portal |
| `deploy_watchdog.rs` | 5 | detects a box booted on a stale binary |
| `budget_killswitch.rs` | 4 | can **stop** the box on a budget breach |
| `hard_stop_guard.rs` | 3 | can **stop** the box outside its window |
| `telegram_webhook.rs` | 3 | how the operator is actually reached |
| `market_open_readiness.rs` | 3 | pre-open readiness check |
| `market_hours_gate.rs` | 2 | arms/disarms the market-hours alarm set |
| `alarm_gate.rs` | 1 | arms/disarms the boot alarm set |
| `dhan_token_minter.rs` | 1 | mints the shared Dhan access token daily |

Every CloudWatch metric filter in this repository matches on `$.code`. So
**43 failures in the paging machinery could not themselves page anyone**, be
counted, or be found by code in triage.

## Nine codes, not forty-three

Grouped by **operator action**, not by call site: every site inside a group
has the same remedy, and the detail that differs (which API, which action,
which alarm) already rides as a structured field on the existing `error!`.

## ⚠ All nine are `Severity::Medium`, and that is FORCED

`error_code_alarm_coverage_guard` fails the build for a new High or Critical
code that has neither a CloudWatch alarm nor an entry on a shrink-only
exemption ratchet. A new alarm costs money against a measured margin to the
automatic `STOP_EC2_INSTANCES` line and needs a dated operator quote; padding
a shrink-only ratchet is exactly what that ratchet exists to prevent. Medium
touches neither guard.

**HONEST CONSEQUENCE:** Medium makes these 43 failures **countable and
greppable by code**. It does **not** make them page anyone. Promoting any of
them to a paging severity is a separate, operator-gated change.

---

## LAMBDA-START-01 — the box is not in the state the schedule expects

`ErrorCode::LambdaStart01BoxNotRunning`. Severity **Medium**.

**Fires when:** the box is not running when it should be, is still running
past the 17:30 IST stop, launched late, or is running with an unreadable
`launch_time`.

This is a statement about the **box**, not about our automation.

**Triage**
1. `aws ec2 describe-instances --filters Name=tag:Name,Values=tv-prod-app` —
   what state is it actually in?
2. Not running in the window → check the EventBridge start rule fired, and
   check for an `InsufficientInstanceCapacity` refusal (the 2026-08-06 class:
   AZ capacity, which no retry can satisfy).
3. Still running past the stop → `tv_hard_stop_guard` runs hourly and should
   force it; check whether **LAMBDA-START-02** or **LAMBDA-AWS-02** also fired.
4. A day with no start at all costs a whole trading session and shows as 0h
   CPU. Cross-check `tv-<env>-start-watchdog-not-invoked`.

## LAMBDA-START-02 — our own start/stop self-heal failed

`ErrorCode::LambdaStart02SelfHealFailed`. Severity **Medium**.

**Fires when:** `StartInstances` or `StopInstances` returned an error —
including an availability-zone capacity refusal.

Distinct from LAMBDA-START-01 because the remedy differs: there the **box** is
wrong; here the thing that **fixes** the box is broken.

**Triage**
1. Read the `reason` field. `InsufficientInstanceCapacity` is the AZ-capacity
   class — retrying in the same zone cannot succeed. The instance is
   multi-AZ-capable; the zone is selected by a terraform variable.
2. Any other API error → check IAM on the Lambda's role.
3. If the box is down and this is firing, the trading session is at risk and
   a manual start is the immediate remedy.

## LAMBDA-AWS-01 — a read-only AWS call failed

`ErrorCode::LambdaAws01ReadCallFailed`. Severity **Medium**.

**Fires when:** `DescribeInstances`, `GetMetricData`, or `GetParameter`
errored. The check that depended on it is **degraded**; nothing was changed.

**Triage:** transient AWS errors self-heal on the next invocation. A
persistent one is IAM or a throttle — read the `reason` field. A blinded
check is not a failed check: confirm what it was checking is still healthy by
another route before standing down.

## LAMBDA-AWS-02 — a mutating AWS call failed

`ErrorCode::LambdaAws02WriteCallFailed`. Severity **Medium**.

**Fires when:** `StopInstances`, `DisableRule`, `PutMetricData`, an alarm
state or action change, or a workflow dispatch failed.

Split from LAMBDA-AWS-01 because a failed **write** can leave the box or an
alarm in an unintended state, while a failed read only blinds a check.

**Triage**
1. A failed `DisableRule` in `hard_stop_guard` means the morning auto-start
   is **still armed** after a budget stop — the box will come back up.
2. A failed alarm state/action change in `market_hours_gate` or `alarm_gate`
   leaves an alarm armed overnight (false page) or disarmed all session
   (silent). Check the alarm's `ActionsEnabled` directly.
3. A failed `StopInstances` means the box **may still be billing**.

## LAMBDA-CONFIG-01 — a required environment variable is empty

`ErrorCode::LambdaConfig01RequiredEnvMissing`. Severity **Medium**.

**Fires when:** `EC2_INSTANCE_ID`, `INSTANCE_ID` or `ALERTS_TOPIC_ARN` is
empty, so the Lambda cannot do its job at all.

This is a **deploy/terraform defect**, not a runtime one: the same input fails
on every invocation until the environment is fixed. A kill-switch that cannot
read the instance id cannot stop the box; one that cannot read the topic ARN
cannot tell you it failed.

**Triage:** `aws lambda get-function-configuration --function-name <fn>` and
compare `Environment.Variables` against the terraform that defines it.

## LAMBDA-NOTIFY-01 — the operator could not be reached

`ErrorCode::LambdaNotify01OperatorUnreachable`. Severity **Medium**.

**Fires when:** an SNS publish, a Telegram POST, a message relay, or the
credential read that precedes them failed.

**The condition that prompted the notification still stands and is now
unreported.** This code is the one that says the alerting path itself broke.

**Triage**
1. Telegram credential read failing → the SSM parameter is unreadable; check
   the Lambda role's `ssm:GetParameter` and `kms:Decrypt`.
2. Telegram POST returning an error status → read the status and body in the
   log line; a 429 is rate limiting, a 401 is a bad token.
3. SNS publish failing → check the topic ARN exists and the role can publish.
4. **Whatever the original alert was, it did not arrive.** Look for the
   preceding error in the same invocation.

## LAMBDA-PORTAL-01 — an operator-portal action failed

`ErrorCode::LambdaPortal01ActionFailed`. Severity **Medium**.

**Fires when:** an operator-control-portal action failed. The operator asked
for something and did not get it; the response carries which action.

**Triage:** read the action name in the log line, then the underlying error.
Most portal actions dispatch SSM commands to the box — a box that is stopped,
or whose SSM agent has not registered (which happens on a 100%-full root
filesystem), fails every one of them.

## LAMBDA-PROV-01 — the deploy-provenance comparison was skipped

`ErrorCode::LambdaProv01ShaUnknown`. Severity **Medium**.

**Fires when:** the deploy watchdog could not resolve one of the two shas it
compares, so `tv-<env>-binary-sha-stale` **cannot fire**.

This is the blindness recorded in `dhan-rest-only-noise-lock-2026-07-14.md`
§2.3q, made greppable by code: that alarm sat OK for its entire life while
its producer declined to produce, because `notBreaching` treats no-data as
health.

**Triage**
1. Which sha is unknown? The **binary** sha comes from SSM
   (`/tickvault/<env>/deploy/binary-git-sha`, written by the deploy workflow);
   the **desired** sha comes from GitHub, falling back to an SSM mirror
   written every 30 minutes by the post-merge catch-up workflow.
2. If the desired sha is unknown, the mirror is stale or the GitHub token is
   absent — the token is a known, recorded gap.
3. While this fires, **the box could be running old code and nothing would
   say so.**

## LAMBDA-MINT-01 — the Dhan token mint failed

`ErrorCode::LambdaMint01TokenMintFailed`. Severity **Medium**.

**Fires when:** the daily Dhan access-token mint failed, so every consumer
keeps serving the previously published token until the next run.

Deliberately **not** `DH-901`: that code is the in-app token **consumer's**
failure and carries its own alarm. Conflating producer with consumer would
make an existing alarm fire for a different component.

**Triage**
1. Dhan permits **one active token per account** — a second minter anywhere
   invalidates ours. Confirm nothing else is minting.
2. Check the SSM credential parameters are readable and current; a TOTP
   secret rotated in the vendor UI without updating SSM fails every attempt.
3. Verify with
   `aws ssm get-parameter --name /tickvault/prod/dhan/access-token --query Parameter.LastModifiedDate`
   — a date that is not today means the mint has not succeeded today.
4. A stale token means the REST legs 401 until it is fixed.

---

## What a PR that violates this file looks like (REJECT)

- Promotes any LAMBDA-* code to High or Critical without a dated operator
  quote **and** the alarm decision that severity forces.
- Adds a tenth LAMBDA-* code for a site an existing one already covers —
  the grouping is by remedy, and a code per call site defeats it.
- Adds a `code =` field to an `aws-lambdas` `error!` without lowering
  `UNCODED_ERROR_BUDGET_BY_CRATE` in the same change. That assertion is
  `assert_eq!` — exact, not a ceiling — precisely so partial progress cannot
  be left sitting above the truth.
- Reuses `DH-901` for the minter, or `TELEGRAM-01` for the webhook: both are
  in-app codes whose runbooks send triage to machinery the Lambda does not
  contain.
