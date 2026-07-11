# Implementation Plan: Groww shared AWS token-minter sync (consumer-only, never mint)

**Status:** VERIFIED
**Date:** 2026-07-02
**Approved by:** Parthiban (operator) — verbatim directive this session: "GROWW TOKEN ARCHITECTURE SYNC — TickVault must adopt the SHARED AWS token system without breaking it… NEVER mint a Groww access token from TickVault code… Read access-token only… re-read on 401 + bounded retry (~60s, up to ~10 min)… rebuild GrowwAPI + GrowwFeed on reconnect… alert, do not mint."

## Design

**Authoritative architecture (bruteX-owned):** one Lambda `groww-token-minter`
(EventBridge ~06:05 IST daily) is the SOLE minter; it reads
`/tickvault/prod/groww/api-key` + `/totp-secret` and writes
`/tickvault/prod/groww/access-token` (SecureString, KMS `alias/tickvault-groww`).
TickVault is a READ-ONLY consumer via IAM role
`groww-token-minter-reader-tickvault` scoped to `ssm:GetParameter` on the
access-token param + `kms:Decrypt`.

**Audit findings (every violation, file:line):**

| # | Violation | Site |
|---|---|---|
| V1 | Full TOTP mint in Rust: `obtain_groww_access_token` POSTs `api.groww.in/v1/token/api/access` with api-key + TOTP | `crates/core/src/feed/groww/auth.rs:168-230` (+ consts :40-49) |
| V2 | Boot smoke-check MINTS a token to validate credentials | `crates/core/src/feed/groww/auth.rs:252-274`, called from `crates/app/src/groww_activation.rs:267` |
| V3 | Rust reads BOTH credentials from SSM (`api-key` + `totp-secret` — Lambda-only params) | `crates/core/src/auth/secret_manager.rs:237-264` (`fetch_groww_credentials`) |
| V4 | Supervisor injects raw credentials into the sidecar env (`GROWW_API_KEY`, `GROWW_TOTP_SECRET`) | `crates/app/src/groww_sidecar_supervisor.rs:944, 981-982` |
| V5 | Python sidecar MINTS in its reconnect loop (`pyotp.TOTP(...).now()` + `GrowwAPI.get_access_token`) — uncoordinated mints can invalidate BruteX's shared token | `scripts/groww-sidecar/groww_sidecar.py:1216-1217` (+ pyotp import :50, env creds :1172-1175) |
| V6 | Smoke script mints the same way | `scripts/groww-sidecar/groww_smoke.py:41-42` |
| V7 | Seed script WRITES `/tickvault/<env>/groww/api-key` + `totp-secret` (writing ANY `/tickvault/prod/groww/*` param is banned) | `scripts/aws-seed-ssm-parameters.sh:231-232` |
| V8 | Credential-param constants used by production paths | `crates/common/src/constants.rs:622,626` |

No violations found for: token caching past auth failure into QuestDB/disk (token
was memory-only), local .env credential files (none tracked), a second
minter/terraform copy (none exists).

**Fix design (Option B — sidecar is the SSM reader; chosen because the
re-read-on-401 + rebuild-GrowwAPI/GrowwFeed loop and the process that talks to
Groww are then the same place, and the Rust supervisor stops touching secrets
entirely):**

1. **Sidecar (`groww_sidecar.py`):** delete pyotp + `get_access_token`. At the
   top of EVERY reconnect cycle, read the token fresh from SSM
   (`boto3.client("ssm").get_parameter(Name=$GROWW_SSM_TOKEN_PARAM,
   WithDecryption=True)`) — trivially satisfies "never cache past an auth
   failure" and the 06:00 rotation case (post-06:00 socket drop → reconnect →
   fresh token). Build `GrowwAPI(access_token)` + `GrowwFeed(groww)` from it —
   token only, never credentials. Auth-class failure pacing: floor the backoff
   at 60s for auth/SSM failures; after ≥600s of consecutive auth-stale cycles,
   print ONE edge-triggered `GROWW LIVE FEED REJECTED: access token stale
   >10min …check the bruteX groww-token-minter Lambda` marker (the existing
   supervisor stderr routing turns that into feed-health Down + Telegram) and
   keep retrying at the ceiling — never mint. Local-dev fallback: if
   `GROWW_ACCESS_TOKEN` env is set, use it directly (memory-only token, no
   credentials — permitted; SSM is the prod path). `requirements.txt`: drop
   `pyotp`, add `boto3` (pinned).
2. **Smoke script (`groww_smoke.py`):** same conversion (env token or SSM read;
   no mint).
3. **Rust secret_manager:** replace `fetch_groww_credentials` with read-only
   `fetch_groww_access_token()` on `/tickvault/<env>/groww/access-token`
   (existing `fetch_secret` = `GetParameter` with decryption; no writes exist in
   this module). Delete `GrowwCredentials`.
4. **Rust groww/auth.rs:** delete the mint (endpoint consts, request/response
   types, `obtain_groww_access_token`, `classify_groww_auth_failure`).
   `run_groww_auth_smoke_check` becomes a read-only SSM token presence/shape
   check: SSM read failure → `Credentials` (transient, don't blame);
   empty/placeholder token → `Rejected` (actionable: "minter has not populated
   the token"); plausible token → OK. Pure `validate_groww_token_shape` is
   unit-tested. `GrowwAuthSmokeError` keeps its 3 variants (activation-watcher
   API unchanged; `Transport` retained for enum compat).
5. **Supervisor:** remove the SSM credential fetch + credential env injection;
   inject `GROWW_SSM_TOKEN_PARAM=/tickvault/<env>/groww/access-token` +
   `AWS_REGION` (default chain delivers the reader role on the box).
6. **Seed script:** remove the two groww put_params (TickVault never writes any
   `/tickvault/*/groww/*` param); leave a pointer comment to the bruteX minter.
7. **Constants:** `GROWW_API_KEY_SECRET`/`GROWW_TOTP_SECRET` → removed;
   `GROWW_ACCESS_TOKEN_SECRET = "access-token"` added.
8. **Rule file:** new
   `.claude/rules/project/groww-shared-token-minter-2026-07-02.md` (verbatim
   directive + hard rules + ratchet list); update the
   `no-rest-except-live-feed-2026-06-27.md` §3 Groww-auth KEEP row (superseded —
   the auth REST call is deleted; the "live-feed AUTH prerequisite" is now the
   SSM read).
9. **Ratchet:** `crates/common/tests/groww_no_mint_guard.rs` — source-scans
   crates/ + scripts/groww-sidecar/ + the seed script and fails the build on:
   any `get_access_token`/`pyotp`/`/v1/token/api/access` reference in production
   paths, any `groww/api-key`|`groww/totp-secret` SSM reference, any seed-script
   groww put_param, missing `WithDecryption` read of the access-token param in
   the sidecar, or missing credential-free feed construction. Updates
   `groww_ssm_seed_script_wiring.rs` to pin the REMOVAL.

**Ambiguities reported, not guessed:** (a) whether the EC2 instance profile IS
`groww-token-minter-reader-tickvault` or TickVault must `sts:AssumeRole` — code
uses the default AWS credential chain (how an instance-profile-delivered reader
role arrives); if an explicit AssumeRole is required, say so and a
`GROWW_READER_ROLE_ARN` env hop gets added. (b) dev-Mac runs without AWS use the
`GROWW_ACCESS_TOKEN` env fallback (token-only). (c) `docs/groww-ref/01…auth.md`
still documents the wire mint for reference — retained as API documentation of
the Lambda's flow, marked not-for-TickVault.

## Edge Cases
- 06:00 IST daily reset with socket open → socket drops → reconnect cycle
  re-reads SSM → pre-06:05 the old token 401s → 60s-paced retries ride the gap →
  post-mint cycle succeeds. No mint, no alert if within 10 min.
- SSM unreachable (IAM broken, region wrong, network) → auth-stale pacing +
  10-min alert marker; loop never exits, never falls back to minting.
- Credential rotation in Groww console → operator updates the two credential
  params (bruteX side) → next Lambda mint → TickVault's next read gets the fresh
  token — zero TickVault changes (nothing here reads credentials).
- Runtime Groww OFF→ON toggle → supervisor relaunch → fresh SSM read (unchanged
  flow).
- Token param exists but empty/placeholder → sidecar treats as auth-stale (never
  passes an empty token to GrowwAPI); Rust smoke check → `Rejected` → feed page
  shows actionable Down.
- Mid-stream NATS drop with token still valid → same-cycle rebuild of
  GrowwAPI+GrowwFeed with the freshly-read token (SSM read is cheap, no Groww
  rate limit involved).

## Failure Modes
- Minter Lambda dead >10 min → edge-triggered REJECTED marker → feed-health Down
  + Telegram page names the Lambda; retries continue at ceiling.
- boto3 missing/venv stale → sidecar exits at import → supervisor relaunch +
  `ensure_python_env` re-provision path; ratchet pins boto3 in requirements.
- A future PR re-introducing a mint → `groww_no_mint_guard.rs` fails the build.
- Seed script run with old TV_GROWW_* env vars → groww section gone; no write
  can occur.

## Test Plan
- Rust unit: `validate_groww_token_shape` (empty / whitespace / placeholder /
  plausible JWT / short garbage), smoke-check classification mapping, SSM path
  construction (`build_ssm_path` for access-token).
- Ratchet: `groww_no_mint_guard.rs` (≥6 assertions listed above);
  `groww_ssm_seed_script_wiring.rs` updated to pin removal + token-param
  non-write.
- Python: `groww_sidecar.py --selftest` extended: token-source resolution order
  (env override → SSM), stale-pacing floor (pure `_backoff_secs` auth floor),
  marker-line emission threshold logic (pure helper `_token_stale_alert_due`).
- Scenario proofs (paste verbatim): rotation simulation + SSM-unavailable +
  reconnect rebuild are covered by the pure-function tests + selftest (no live
  AWS in CI); live-box behavior is the documented envelope.

## Rollback
`git revert` of the single PR restores the mint path intact (auth.rs,
supervisor env, sidecar, seed script all revert together; no schema/config
migration involved).

## Observability
- Sidecar prints `groww auth OK: token read from SSM (len=N)` per cycle (length
  only), the edge-triggered stale marker, and the existing REJECTED routing
  (feed_health + Telegram) covers the alert path.
- Rust smoke check logs the token param path (never the value) on failure.
- No new ErrorCode: the existing `GrowwSidecarRejected` notification event +
  FEED-STALL/FEED-SUPERVISOR chains carry the operator signal.

## Plan Items
- [x] Rust: `fetch_groww_access_token` (read-only) + delete `fetch_groww_credentials` + `GrowwCredentials`; constants swap
  - Files: crates/core/src/auth/secret_manager.rs, crates/core/src/auth/types.rs, crates/common/src/constants.rs
  - Tests: test_build_ssm_path_groww_access_token
- [x] Rust: rewrite groww auth.rs as read-only token presence/shape check (mint deleted)
  - Files: crates/core/src/feed/groww/auth.rs
  - Tests: test_validate_groww_token_shape_rejects_empty, test_validate_groww_token_shape_accepts_jwt
- [x] Supervisor: token-param env injection, credential plumbing removed
  - Files: crates/app/src/groww_sidecar_supervisor.rs
  - Tests: test_supervisor_injects_token_param_env
- [x] Sidecar + smoke: SSM token read, no mint, stale pacing + 10-min alert marker
  - Files: scripts/groww-sidecar/groww_sidecar.py, scripts/groww-sidecar/groww_smoke.py, scripts/groww-sidecar/requirements.txt
  - Tests: test_sidecar_reads_token_from_ssm_never_mints
- [x] Seed script: groww credential writes removed
  - Files: scripts/aws-seed-ssm-parameters.sh
  - Tests: seed_script_never_writes_groww_params
- [x] Ratchet + rule files
  - Files: crates/common/tests/groww_no_mint_guard.rs, crates/common/tests/groww_ssm_seed_script_wiring.rs, .claude/rules/project/groww-shared-token-minter-2026-07-02.md, .claude/rules/project/no-rest-except-live-feed-2026-06-27.md
  - Tests: no_mint_call_sites_in_production_paths, no_credential_param_references_in_production_paths

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | 06:00 rotation, socket drops | reconnect → SSM re-read → fresh token → streaming resumes; zero mints |
| 2 | 06:00–06:05 gap | 60s-paced auth retries ride the gap silently |
| 3 | Minter dead > 10 min | one REJECTED marker → Telegram + feed page Down; retries continue; zero mints |
| 4 | SSM unreachable | same bounded-pace + alert path; no crash-loop; no mint fallback |
| 5 | Credential rotation (bruteX side) | TickVault unchanged — nothing here reads credentials |
| 6 | grep proof | zero production references to api-key/totp-secret params or any mint call |
