# Implementation Plan: Dhan token minter Lambda (box-independent daily mint)

**Status:** APPROVED
**Date:** 2026-08-08
**Approved by:** Parthiban (operator) — 2026-08-08, verbatim: *"build the lambda"*, reconfirmed *"yes yes yes"*
**Authority:** `.claude/rules/project/groww-shared-token-minter-2026-07-02.md` §10 (this PR adds it)

---

## Design

### The problem in one line

Dhan permits **one active access token per account**, and the only thing that minted one was the tickvault app **on the prod EC2 box**. The box has been unstartable since ~2026-08-06 (`InsufficientInstanceCapacity`, ap-south-1a), so nothing minted, `/tickvault/prod/dhan/access-token` froze at its 25 July value, and consumers got `HTTP 401 / DH-901` on **785 of 785** attempts.

### Why Groww was immune

Groww's minter is a **Lambda**, which runs whether or not the box does. That is why the Groww parameter refreshed at 00:35 GMT on 2026-08-08 while the Dhan one sat two weeks stale. The asymmetry is the whole finding.

### The fix

Mirror it: a `tv-<env>-dhan-token-minter` Lambda reads the three Dhan credentials from SSM, generates a TOTP code, POSTs `generateAccessToken`, and writes the JWT to `/tickvault/<env>/dhan/access-token`. Consumers read. **One minter per broker, neither tied to EC2.**

### Why this supersedes PR #1729's placement

#1729 (merged, `6b7c93ea`) fixed a real defect — the token was never published at all — but left the **minter** on the box. This PR moves the minter off the box. #1729's publisher is retained; see the coexistence obligation below.

### Placement

`crates/aws-lambdas/` — the existing Rust Lambda fleet. Logic in `src/dhan_token_minter.rs`, thin bin in `src/bin/dhan_token_minter.rs` (thin-bin coverage rule). `cargo lambda build -p tickvault-aws-lambdas` already builds every `[[bin]]`, so CI picks it up with **no workflow change**.

### Dependencies

`totp-rs` and `secrecy` — both EXISTING workspace pins reused verbatim. **No new dependency root**, so no new-dep approval is needed (`totp-rs` is the same crate `crates/core` uses for the identical SHA-1/6-digit/30s Dhan TOTP).

### Coexistence (the one real hazard)

Today only one minter can run, because the box is down. **Once the box starts, two minters against a one-token account is the re-mint war from the other direction.** The required follow-up — switching tickvault to READ this parameter, the posture it already has for Groww — is flagged in §10.3 and deliberately NOT bundled here.

---

## Plan Items

- [x] Minter module: SSM reads → TOTP → mint → shape-gate → SSM write, fail-loud
  - Files: `crates/aws-lambdas/src/dhan_token_minter.rs`, `src/lib.rs`
  - Tests: 24 unit tests (path/URL/JWT-shape/Dhan-error-envelope/truncation/TOTP/no-secret-leak)

- [x] Thin bin + Cargo wiring
  - Files: `crates/aws-lambdas/src/bin/dhan_token_minter.rs`, `crates/aws-lambdas/Cargo.toml`
  - Tests: covered by the lib tests (thin-bin rule)

- [x] Terraform: Lambda + least-privilege IAM + daily EventBridge + 2 alarms
  - Files: `deploy/aws/terraform/dhan-token-minter-lambda.tf`
  - Tests: `crates/aws-lambdas/tests/dhan_token_minter_wiring_guard.rs`

- [x] Rule-file §10 — the authority record + coexistence obligation + honest envelope
  - Files: `.claude/rules/project/groww-shared-token-minter-2026-07-02.md`
  - Tests: pinned by the wiring guard

- [x] Archive the merged `active-plan-dhan-token-publish.md` (rule 7; keeps the plan-gate at its cap)
  - Files: `.claude/plans/archive/2026-08-08-dhan-token-publish.md`
  - Tests: n/a

---

## Edge Cases

| # | Case | Handling |
|---|---|---|
| 1 | Dhan answers HTTP 200 with `{"status":"error","message":"Invalid Pin"}` | Classified `DhanError` (checked BEFORE the token field) so a wrong PIN reads as a wrong PIN |
| 2 | Response body is HTML (502 page) | `NoToken("body is not JSON")` — never written |
| 3 | `accessToken` present but not JWT-shaped | `MalformedToken` — **refused before the SSM write**, so a good token is never overwritten with junk |
| 4 | `accessToken` empty string | Same as #3 |
| 5 | Both an error envelope and a token field | Error wins (authoritative) |
| 6 | A credential parameter is missing or empty | `CredentialRead` naming the path; no mint attempted |
| 7 | TOTP secret is not valid base32 | `TotpGeneration`; no mint attempted |
| 8 | Mint succeeds, SSM write fails | `Publish` — loud: the previous token is already dead, so the parameter is now stale |
| 9 | Huge/hostile response body | `capture_body` truncates at 300 bytes on a **char boundary** (no mid-codepoint panic) |
| 10 | `TV_ENVIRONMENT` unset | Defaults to `prod` with a `warn!` (the only real environment) |
| 11 | Weekend | Runs anyway — cron is `* * ?`, not MON-FRI; consumers read on weekends |

---

## Failure Modes

| Mode | Blast radius | Detection | Mitigation |
|---|---|---|---|
| Mint fails | Consumers keep a stale token until the next run | `tv-<env>-dhan-token-minter-errors` (every path returns `Err`) | Next scheduled run retries; alarm pages |
| **Schedule dropped** (2026-07-02 class) | Silent staleness — Errors alarm is blind (no invocation = no error) | `tv-<env>-dhan-token-minter-not-invoked` (`Invocations < 1`/24h, `treat_missing_data=breaching`) | Distinct alarm exists precisely for this |
| Writes the Groww token by mistake | Groww REST legs 401 | Write scoped to one ARN; unit test asserts the paths differ | IAM makes it impossible |
| Secret leaks to CloudWatch | Credential disclosure | Unit test asserts no error message carries `pin=`/`totp=`; transport errors reported by KIND (reqwest `Display` embeds the URL) | Length + shape verdict only |
| Two minters race once the box returns | Re-mint war returns | §10.3 flagged obligation | Switch tickvault to READ before the box runs |

---

## Test Plan

- 24 unit tests on the pure logic — all green locally.
- Wiring guard: terraform resources exist, IAM is scoped, cron is daily, both alarms present, rule file carries §10.
- **NOT claimed:** live behaviour. CI has no AWS credentials, so the SSM reads/write, the real `generateAccessToken` call, and the EventBridge trigger are UNVERIFIED-LIVE until the first 06:05 IST invocation.
- Real proof: `aws ssm get-parameter --name /tickvault/prod/dhan/access-token --query Parameter.LastModifiedDate` reads today.

---

## Rollback

Disable the EventBridge rule (`state = "DISABLED"`) or delete the terraform file. Nothing else regresses: the Lambda is additive, holds no EC2 permission, and #1729's publisher is untouched. No data migration, no schema change.

---

## Observability

- Two CloudWatch alarms → SNS → Telegram: mint FAILED, and mint DID NOT RUN.
- Structured logs in `/aws/lambda/tv-<env>-dhan-token-minter` (30-day retention) carrying the failing `stage` label and the parameter written — never the token.
- The published parameter's `LastModifiedDate` is itself the operator-visible health signal.
- Cost: 1 invocation/day (free tier) + 2 alarms ≈ **$0.20/mo**.
