# Depth-200 SELF Token Error Codes

> **Authority:** This file is the runbook target for the `Depth200Auth01..03`
> ErrorCode variants added 2026-04-28. The cross-ref test
> `crates/common/tests/error_code_rule_file_crossref.rs` requires every
> variant in `crates/common/src/error_code.rs::ErrorCode` to be mentioned
> in at least one rule file under `.claude/rules/`.
>
> **Background:** `docs/architecture/depth-200-self-token-design.md`
> covers the full architecture, investigation, and Telegram event matrix.
> **Module:** `crates/core/src/auth/depth_200_self_token_manager.rs`.

## DEPTH200-AUTH-01 — SELF token failed validation at boot

**Trigger:** `Depth200SelfTokenManager::boot_from_ssm` fetched a JWT
from `/tickvault/<env>/dhan/depth_200_self_token`, parsed it
successfully, but `validate_self_claims()` rejected it because:

- `tokenConsumerType` was not `"SELF"` (most commonly `"APP"` — the
  operator pasted the wrong token type), OR
- `dhanClientId` did not match the expected client ID (cross-account
  leak — paranoia check), OR
- `exp` was less than `min_remaining_secs_at_boot` away (stale token),
  OR
- `iat` was more than 60s in the future (clock skew or forged token).

**Severity:** Critical. Boot does NOT continue with a broken depth-200
auth path.

**Triage:**

1. Decode the SSM value — paste the JWT body into any decoder, or run:
   ```bash
   aws ssm get-parameter --name /tickvault/dev/dhan/depth_200_self_token \
     --with-decryption --region ap-south-1 --query Parameter.Value \
     --output text | cut -d. -f2 | base64 -d 2>/dev/null | jq .
   ```
2. Confirm `tokenConsumerType` field. If `"APP"`, the operator pasted
   a token from `/app/generateAccessToken` instead of `web.dhan.co`.
3. Generate a fresh SELF token at `web.dhan.co > Generate Access Token`
   (revoke + generate new), paste into SSM:
   ```bash
   aws ssm put-parameter --name /tickvault/dev/dhan/depth_200_self_token \
     --value "<paste-fresh-jwt>" --type SecureString --overwrite \
     --region ap-south-1
   ```
4. Restart the app.

**Auto-triage safe:** NO (Critical — operator paste required).

## DEPTH200-AUTH-02 — RenewToken cycle failed

**Trigger:** `Depth200SelfTokenManager::renew_once` failed during the
23h renewal cycle. One of:

- HTTP request to `GET /v2/RenewToken` returned non-2xx,
- The renewed JWT failed `validate_self_claims` (Dhan-side change to
  `RenewToken` semantics — `tokenConsumerType` no longer preserved?),
- `aws_sdk_ssm::PutParameter` returned an error.

**Severity:** Critical. The arc-swap is NOT updated; the existing
SELF token continues to be used. If this fires repeatedly, the SSM
token will eventually expire and depth-200 will go dark.

**Triage:**

1. Check the error payload in `errors.jsonl` for the underlying cause:
   - `HTTP 401 Unauthorized` → token may have been revoked by operator
     or by Dhan. Generate fresh, paste into SSM.
   - `HTTP 429 Rate limit` → unusual on the auth tier (1 req per 23h).
     Investigate.
   - `RenewToken response JSON parse failed` → Dhan changed schema.
     Escalate to apihelp@dhan.co.
   - `renewed token failed SELF validation` → Dhan changed
     `tokenConsumerType` semantics. URGENT escalate.
   - `PutParameter failed` → check IAM permissions
     (`ssm:PutParameter` + `kms:Encrypt`).
2. Stopgap: paste a fresh SELF token into SSM manually (see
   DEPTH200-AUTH-01 step 3-4) to restart the renewal clock.

**Auto-triage safe:** NO (Critical — root cause may be Dhan-side).

## DEPTH200-AUTH-03 — AWS SSM unreachable

**Trigger:** `aws_sdk_ssm::Client::get_parameter` returned an error
during boot OR renewal write-back. Causes:

- IAM role lacks `ssm:GetParameter` / `ssm:PutParameter` permission,
- IAM role lacks `kms:Decrypt` / `kms:Encrypt` on the SecureString's
  KMS key,
- Network egress to `ssm.ap-south-1.amazonaws.com` blocked,
- AWS region misconfigured.

**Severity:** Critical. At boot, the manager fails to construct and
the app halts on the depth-200 path. At renewal, the existing token
keeps running until it expires.

**Triage:**

1. From the host where tickvault runs:
   ```bash
   aws ssm get-parameter --name /tickvault/dev/dhan/depth_200_self_token \
     --with-decryption --region ap-south-1
   ```
   If this CLI command also fails, the issue is environmental
   (IAM / network / KMS), not tickvault.
2. Check IAM policy attached to the EC2 role / IAM user running
   tickvault. Required actions:
   - `ssm:GetParameter`, `ssm:PutParameter` on the parameter ARN
   - `kms:Decrypt`, `kms:Encrypt` on the KMS key (typically
     `alias/aws/ssm` if you used the AWS-managed default).
3. Check network reachability:
   ```bash
   curl -sS https://ssm.ap-south-1.amazonaws.com/ -o /dev/null -w '%{http_code}\n'
   ```
   Expect HTTP 404 or similar (endpoint replies); a connection
   timeout means egress is blocked.

**Auto-triage safe:** NO (Critical — environmental fix required).

## Cross-references

- `docs/architecture/depth-200-self-token-design.md` — full design
- `.claude/rules/dhan/full-market-depth.md` — root-path URL invariant
  + APP-vs-SELF discovery
- `.claude/rules/dhan/authentication.md` rule 5 — `RenewToken`
  semantics
- `docs/dhan-support/2026-04-28-depth-200-app-vs-self-token.md` —
  reproducer sent to apihelp@dhan.co
- `crates/core/src/auth/depth_200_self_token_manager.rs` — module
  source
- `crates/common/src/error_code.rs` — `Depth200Auth01..03` variants
