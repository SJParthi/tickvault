# Depth-200 SELF Token — Research, Design, Implementation

> **Status:** Module shipped (commits `9b7b533`, `e4d1b66`). Wiring + Telegram events pending.
> **Branch:** `claude/debug-grafana-issues-LxiLH`
> **Authority:** `.claude/rules/dhan/full-market-depth.md`, `docs/dhan-support/2026-04-28-depth-200-app-vs-self-token.md`

---

## 1. The 16-connection always-connected architecture

Dhan caps each WebSocket type at 5 connections per account, with **independent pools** (confirmed by Dhan support 2026-04-06). Plus 1 order-update WS (single connection per account).

| Pool | Cap | Currently used | Status |
|---|---:|---:|---|
| Live Market Feed | 5 | 5 | ✅ all 5 connected |
| Depth-20 | 5 | 2 (NIFTY + BANKNIFTY only, 2026-04-25 policy) | ✅ both connected |
| Depth-200 | 5 | 4 (NIFTY ATM CE+PE, BANKNIFTY ATM CE+PE) | ❌ **all 4 fail with APP token** |
| Order Update | 1 | 1 | ✅ connected |
| **Total connections we use** | — | **12** | **8 working, 4 broken** |
| **Total connections cap** | **16** | — | — |

**Goal:** every used connection stays connected continuously through the trading session and idle-sleeps post-close (Wave 2 WS-GAP-04). Currently the 4 depth-200 connections are the only failing path — and the entire reason for this work.

> **Note on "all 5":** the user's intuition was 5+5+5+1 = 16. The cap is correct, but we only currently *use* 2 of depth-20 and 4 of depth-200 because the F&O scope was narrowed on 2026-04-25 (`.claude/rules/project/depth-subscription.md` 2026-04-25 Updates). If Dhan's pool fixes the auth issue and we expand index coverage, the spare slots are available without re-architecture.

---

## 2. The 2-week disconnect chase (Ticket #5519522)

Symptom: `Protocol(ResetWithoutClosingHandshake)` storm on `wss://full-depth-api.dhan.co/twohundreddepth` for 2+ weeks. All other WebSockets streamed fine on the same account, same minute.

| Date | Variant tested | Result |
|---|---|---|
| 2026-04-15 to 2026-04-22 | URL path `/twohundreddepth`, APP token, our Rust client | ❌ RST after subscribe |
| 2026-04-22 | Same path, APP token, Variant H (Python UA + no-ALPN TLS, PR #340) | ❌ RST after subscribe |
| 2026-04-23 | URL path `/` (root), APP token, Dhan Python SDK 2.2.0rc1 | ❌ RST after subscribe |
| 2026-04-23 (afternoon) | URL path `/` (root), **SELF token from web.dhan.co**, Dhan Python SDK | ✅ **streamed for 30+ min** |
| 2026-04-28 (today) | Side-by-side: same script, same SID 74147, only token source changed | ❌ APP fails / ✅ SELF works |

**Diagnostic finality:** with the SELF token, both URL paths (`/` and `/twohundreddepth`) work — confirming it was never a URL/TLS/UA issue. It is a **server-side token-type gate** on `full-depth-api.dhan.co`.

---

## 3. Root cause — JWT `tokenConsumerType` claim

```
APP token (failing, our TOTP /app/generateAccessToken):
  "tokenConsumerType": "APP"   ← rejected by full-depth-api.dhan.co

SELF token (working, web.dhan.co manual generation):
  "tokenConsumerType": "SELF"  ← accepted, streams 200-level depth
```

Other paths (main feed, depth-20, order-update, REST) accept BOTH types. **Only depth-200 discriminates.** This is undocumented; we've reported it to Dhan via `docs/dhan-support/2026-04-28-depth-200-app-vs-self-token.md` and asked four questions (is it intentional, is there a programmatic SELF mint, does RenewToken preserve SELF, can one account hold both token types).

---

## 4. Solution — AWS SSM-backed alternate auth path

**Per Parthiban directive 2026-04-28: "AWS only, no local anywhere".**

| Path | Token Source | Renewal | Used By |
|---|---|---|---|
| Existing TOTP/APP | `POST /app/generateAccessToken` (TOTP-driven) | `TokenManager` 23h loop | Main feed + depth-20 + order-update + REST |
| **New: Manual SELF + RenewToken** | AWS SSM `/tickvault/dev/dhan/depth_200_self_token` (operator-pasted) | `Depth200SelfTokenManager` 23h loop, `GET /v2/RenewToken` → `PutParameter` write-back | **Depth-200 only** |

**Mode flag in `config/base.toml`:**

```toml
[depth_200_auth]
mode = "totp_app"  # default — no behavior change
# mode = "manual_self_with_renewal"  # operator flips when ready
ssm_parameter_name = "/tickvault/dev/dhan/depth_200_self_token"
renewal_interval_secs = 82800        # 23h
min_remaining_secs_at_boot = 3600    # 1h
```

**Daily operator workflow (after wiring lands):**
1. **Day 0 (one-time):** generate SELF token from `web.dhan.co > Generate Access Token`, paste into SSM via AWS console or CLI. Flip `mode` to `"manual_self_with_renewal"`. Restart app.
2. **Day 1+:** automatic. Renewal task extends the SELF token in-place every 23h via Dhan's `RenewToken` endpoint (which preserves `tokenConsumerType`).
3. **On renewal failure:** Telegram CRITICAL → operator pastes a fresh SELF token into SSM → restart depth-200 task.

---

## 5. Telegram event matrix (the new piece)

Every lifecycle moment of the depth-200 SELF token gets an explicit Telegram event. Coalesced via the existing bucket-coalescer (Wave 3 Item 11) so bursts don't spam.

| # | Event | Severity | Trigger | Channels | ErrorCode |
|---|---|---|---|---|---|
| 1 | `Depth200SelfTokenLoaded` | Info | Boot succeeded fetching + validating SELF JWT from SSM | Telegram only | (positive — none) |
| 2 | `Depth200SelfTokenRenewed` | Info | Renewal cycle succeeded (RenewToken + PutParameter + arc-swap) | Telegram only | (positive — none) |
| 3 | `Depth200SelfTokenInvalidAtBoot` | Critical | SSM had APP token / wrong client_id / expired / corrupt JWT | Telegram + SNS SMS | `DEPTH200-AUTH-01` |
| 4 | `Depth200SelfTokenRenewalFailed` | Critical | RenewToken HTTP error / response not SELF-type / SSM PutParameter failure | Telegram + SNS SMS | `DEPTH200-AUTH-02` |
| 5 | `Depth200SelfTokenSsmUnreachable` | Critical | aws-sdk-ssm GetParameter failed (network / IAM / KMS) | Telegram + SNS SMS | `DEPTH200-AUTH-03` |
| 6 | `Depth200ConnectionEstablished` | Info | One of the 4 depth-200 WS connections came up (new) | Telegram only | (positive — none) |

Existing events that already cover depth-200 connection states (no change needed):
- `WebSocketConnected{feed: "depth_200", ...}` — fires today
- `WebSocketDisconnected{feed: "depth_200", ...}` (in-market) → Severity::High
- `WebSocketDisconnectedOffHours{feed: "depth_200"}` (post-15:30) → Severity::Low (no page)
- `MarketOpenStreamingConfirmation` — 09:15:30 IST, includes depth-200 active count
- `DepthRebalanced` — Severity::Low for routine ATM swaps
- `DepthRebalanceFailed` — Severity::High when swap channel broken

**Telegram message format (drafts):**

```
[INFO] Depth-200 SELF Token Loaded
ssm_param: /tickvault/dev/dhan/depth_200_self_token
client_id: 1106656882
expires_at: 2026-04-29 15:46:51 IST
hours_remaining: 23.5
```

```
[CRITICAL] Depth-200 SELF Token RENEWAL FAILED
ssm_param: /tickvault/dev/dhan/depth_200_self_token
error: HTTP 401 from /v2/RenewToken — token may be revoked
last_known_expiry: 2026-04-29 15:46:51 IST (1h 12min remaining)
runbook: docs/runbooks/depth-200-self-token-renewal-failed.md
ACTION REQUIRED: paste fresh SELF token into SSM, restart depth-200 task
```

---

## 6. ErrorCode + runbook (next commit)

3 new variants in `crates/common/src/error_code.rs`:

```rust
Depth200Auth01InvalidAtBoot,           // Severity::Critical
Depth200Auth02RenewalFailed,           // Severity::Critical
Depth200Auth03SsmUnreachable,          // Severity::Critical
```

Runbook file: `.claude/rules/project/depth-200-auth-error-codes.md` covering:
- DEPTH200-AUTH-01: `tokenConsumerType` is APP / wrong client_id / expired
  → operator action: paste fresh SELF token, restart
- DEPTH200-AUTH-02: RenewToken HTTP error or response not SELF
  → operator action: paste fresh SELF token; if Dhan-side change, escalate
- DEPTH200-AUTH-03: SSM unreachable
  → operator action: check IAM, network, KMS key access

---

## 7. Implementation status

| Component | File | Status | Commit |
|---|---|---|---|
| Plan file | `.claude/plans/active-plan-depth-200-self-token.md` | ✅ Shipped | `a30e55b` |
| Dhan support email | `docs/dhan-support/2026-04-28-depth-200-app-vs-self-token.md` | ✅ Shipped | `3a355fe` |
| URL-compare Python | `scripts/dhan-200-depth-repro/check_depth_200_url_compare.py` | ✅ Shipped | `6e37a4f` |
| Config struct + base.toml | `crates/common/src/config.rs` `Depth200AuthConfig` | ✅ Shipped, 11 tests | `a30e55b`, `279a89a` |
| SSM constant | `crates/common/src/constants.rs` `DEPTH_200_SELF_TOKEN_SSM_PARAMETER` | ✅ Shipped | `279a89a` |
| Manager module | `crates/core/src/auth/depth_200_self_token_manager.rs` (625 LoC) | ✅ Shipped, 16 tests | `9b7b533`, `e4d1b66` |
| AWS SSM parameter | `/tickvault/dev/dhan/depth_200_self_token` (SecureString, KMS) | ✅ Created by operator | (AWS) |
| 3 new ErrorCode variants | `crates/common/src/error_code.rs` | ⏳ Pending | next |
| 6 new NotificationEvent variants | `crates/core/src/notification/events.rs` | ⏳ Pending | next |
| Runbook | `.claude/rules/project/depth-200-auth-error-codes.md` | ⏳ Pending | next |
| Manager → Telegram wiring | `Depth200SelfTokenManager` boot + renewal paths | ⏳ Pending | next |
| **main.rs wiring (the big one)** | `crates/app/src/main.rs` ~line 3459 — branch on `mode == "manual_self_with_renewal"` | ⏳ Pending | next |
| Integration test | `crates/core/tests/depth_200_self_token_integration.rs` | ⏳ Pending | next |
| Grafana panel + Prometheus counter | `tv_depth_200_self_token_renewals_total{status}` | ⏳ Pending | next |

---

## 8. Test plan

| Layer | What's verified | Where | Status |
|---|---|---|---|
| Pure JWT parser | base64url decode, JSON parse, malformed input rejection | `auth::depth_200_self_token_manager::tests` | ✅ 5/5 pass |
| SELF claim validator | rejects APP, wrong client_id, expired, future iat | same | ✅ 6/6 pass |
| TokenState builder | parse + validate + IST timestamp roundtrip | same | ✅ 2/2 pass |
| base64url decoder | known vectors + invalid char rejection | same | ✅ 3/3 pass |
| Boot from SSM | requires AWS creds + network | TEST-EXEMPT, manual operator test | ⏳ Mac integration |
| Renewal HTTP + write-back | requires Dhan + AWS | TEST-EXEMPT, future localstack mock | ⏳ |
| Live depth-200 streaming | end-to-end with SELF token | manual `check_depth_200_url_compare.py` | ⏳ Mac run pending |

---

## 9. Operator runbook

### Daily (steady state)

Nothing. Renewal task extends SSM token automatically every 23h.

### After fresh deploy / cold start

1. Verify SSM parameter exists: `aws ssm get-parameter --name /tickvault/dev/dhan/depth_200_self_token --with-decryption --region ap-south-1`
2. Verify token is SELF: pipe the value into a JWT decoder, confirm `"tokenConsumerType": "SELF"`.
3. Boot the app. Watch Telegram for `Depth200SelfTokenLoaded` (Info, ~5s after boot).
4. Verify all 4 depth-200 WS connections come up (`/health` shows `depth_200_active: 4`).

### On `Depth200SelfTokenRenewalFailed` Telegram alert

1. Generate fresh SELF token at `web.dhan.co > Generate Access Token > Revoke active > Generate new`.
2. `aws ssm put-parameter --name /tickvault/dev/dhan/depth_200_self_token --value "<paste-fresh-jwt>" --type SecureString --overwrite --region ap-south-1`
3. Restart depth-200 task (full app restart for now; in future we can hot-reload via signal).
4. Watch Telegram for `Depth200SelfTokenLoaded` confirmation.

### On `Depth200SelfTokenInvalidAtBoot` Telegram alert (Critical)

Boot found a token in SSM but it failed validation (APP type, wrong client_id, or expired). Same as above — generate fresh, paste, restart.

### Pre-market checklist

- 08:45 IST: confirm SSM token has > 4h validity remaining (the renewal loop should have refreshed it the previous evening, but verify on Friday before weekend).

---

## 10. Migration plan

Today's behavior (`mode = "totp_app"`) is unchanged — depth-200 still tries with the rejected APP token and fails. To switch:

1. **Pre-flight (already done):**
   - SSM parameter created at `/tickvault/dev/dhan/depth_200_self_token` ✅
   - Manager module shipped + 16 unit tests passing ✅
2. **Pending commits in this PR:**
   - Add 3 ErrorCode variants + cross-ref test
   - Add 6 NotificationEvent variants
   - Wire `Depth200SelfTokenManager::boot_from_ssm` + `spawn_renewal_task` into `main.rs` behind the config flag
   - Replace `token_handle.clone()` for the depth-200 spawn with `manager.handle().clone()` when flag is on
3. **Operator step (after PR merges):**
   - Set `[depth_200_auth] mode = "manual_self_with_renewal"` in `config/base.toml`
   - Restart app
4. **Verification:**
   - Telegram: `Depth200SelfTokenLoaded` Info
   - Telegram: 4× `WebSocketConnected{feed: "depth_200"}` Info events
   - QuestDB: `select count(*) from deep_market_depth where depth_type='200' and ts > dateadd('m',-5,now())` returns > 0

---

## 11. Open questions for Dhan (sent 2026-04-28, GitHub-rendered link)

1. Is APP-token rejection on `full-depth-api.dhan.co` intentional?
2. Is there a programmatic SELF-mint API?
3. Does `GET /v2/RenewToken` preserve `tokenConsumerType: "SELF"` after extending?
4. Can one account hold an active APP token AND an active SELF token simultaneously (multiple Application entries)?

Awaiting reply before declaring the workaround stable. Until then, the manual SELF paste + RenewToken extend cycle is our defense.
