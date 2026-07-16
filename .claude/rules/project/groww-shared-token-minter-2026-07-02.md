# Groww Shared Token-Minter — Consumer-Only Lock (Operator 2026-07-02)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §I > `groww-second-feed-scope-2026-06-19.md` > this file > `no-rest-except-live-feed-2026-06-27.md` (Groww-auth row SUPERSEDED by this file) > defaults.
> **Scope:** PERMANENT. Every environment, every PR, every future Claude/Cowork session.
> **Operator-locked:** 2026-07-02 (verbatim directive below).
> **Auto-load trigger:** Always loaded (path is in `.claude/rules/project/`).

---

## §0. The verbatim operator directive (preserve exactly, do not paraphrase)

> "GROWW TOKEN ARCHITECTURE SYNC — TickVault must adopt the SHARED AWS token system without breaking it. … ONE MINTER ONLY: an AWS Lambda `groww-token-minter` is the SOLE component that ever mints a Groww access token. It runs daily via EventBridge cron(35 0 * * ? *) (~06:05 IST, right after Groww's 06:00 IST daily token reset), reads the api-key + totp-secret FRESH from SSM on every invocation, mints via TOTP, and writes the token to SSM. … CONSUMERS ARE READ-ONLY: BruteX and TickVault each have a dedicated IAM reader role (groww-token-minter-reader-tickvault for you) scoped to ssm:GetParameter on the access-token parameter ONLY + kms:Decrypt. … NEVER mint a Groww access token from TickVault code, in any environment, ever. … NEVER write to any /tickvault/prod/groww/* parameter. … NEVER deploy a second minter/Lambda/terraform copy. … NEVER cache the token past an auth failure. On any 401/auth-expired from Groww: re-read /tickvault/prod/groww/access-token from SSM and retry with the fresh value, bounded retry with backoff (~60s interval, up to ~10 min) … If still stale after the bound, alert — do not mint. … on reconnect/re-subscribe, re-read the token from SSM and rebuild GrowwAPI + GrowwFeed. Never pass credentials to the feed — it only takes the token. No local .env/keychain copies of Groww credentials in TickVault production paths. The token may be held in memory only."

---

## §1. The architecture (authoritative, bruteX-owned)

| Component | Owner | Role |
|---|---|---|
| Lambda `groww-token-minter` (ap-south-1, acct 208384284948, `deploy/aws_token_minter/` in bruteX) | bruteX repo (terraform, S3 remote state) | THE SOLE MINTER — daily ~06:05 IST, reads credentials fresh, TOTP-mints, writes the token |
| `/tickvault/prod/groww/api-key` + `/totp-secret` (SecureString, KMS `alias/tickvault-groww`) | Lambda-only read; operator writes on rotation | Credentials — TickVault can NEITHER read NOR write them (IAM) |
| `/tickvault/prod/groww/access-token` | Written by the Lambda; read by consumers | The shared daily token |
| IAM role `groww-token-minter-reader-tickvault` | consumed by TickVault | `ssm:GetParameter` on the access-token param ONLY + `kms:Decrypt` |

Credential rotation is invisible to TickVault: the operator pastes new credentials into the two SSM params once; the next Lambda mint picks them up; consumers notice nothing.

## §2. TickVault's implementation (this lock's landing PR)

| Surface | Behaviour |
|---|---|
| `crates/core/src/auth/secret_manager.rs::fetch_groww_access_token` | READ-ONLY GetParameter(decrypt) of the access-token param via the default AWS credential chain (the reader role on the box); no writes exist in the module (ratcheted) |
| `crates/core/src/feed/groww/auth.rs` | Mint DELETED. `run_groww_auth_smoke_check` = SSM read + pure `validate_groww_token_shape`; empty/placeholder → feed page shows "minter has not populated the token" *(2026-07-15: file DELETED with the Groww live feed — row retained as historical audit; the machinery rows for `secret_manager.rs` stand — the REST legs remain the read-only consumers)* |
| `crates/app/src/groww_sidecar_supervisor.rs` | NO credential fetch, NO secret in the child env — injects only `GROWW_SSM_TOKEN_PARAM` (a path string). Requirements-fingerprint marker re-provisions the venv when deps change *(2026-07-15: file DELETED with the Groww live feed — row retained as historical audit; the machinery rows for `secret_manager.rs` stand — the REST legs remain the read-only consumers)* |
| `scripts/groww-sidecar/groww_sidecar.py` | Reads the token via boto3 GetParameter(WithDecryption) each auth cycle (env `GROWW_ACCESS_TOKEN` override for local dev — token only, never credentials). Cached across pure feed-side reconnects; dropped on ANY auth-class failure (`_is_auth_error`: 401/403/auth-shaped — how the 06:00 reset surfaces on connect/subscribe) so the NEXT cycle re-reads SSM and rebuilds `GrowwAPI(access_token)` + `GrowwFeed(groww)`. Auth-stale retries floored at `AUTH_RETRY_FLOOR_SECS = 60`; after `TOKEN_STALE_ALERT_SECS = 600` of continuous auth failure, ONE edge-triggered `GROWW LIVE FEED REJECTED: access token stale …` line (supervisor routes → feed-health Down + Telegram) and retries continue at the ceiling — NEVER a mint *(2026-07-15: file DELETED with the Groww live feed — row retained as historical audit; the machinery rows for `secret_manager.rs` stand — the REST legs remain the read-only consumers)* |
| `scripts/aws-seed-ssm-parameters.sh` | Seeds NOTHING under `groww/*` (the Lambda owns every groww param) |
| `scripts/groww-sidecar/requirements.txt` | `boto3` pinned; the TOTP dependency removed with the mint path *(2026-07-15: file DELETED with the sidecar — historical)* |

## §3. What a PR that violates this lock looks like (REJECT)

- Re-introduces any call to Groww's token-mint REST endpoint, any Groww TOTP computation, or the deleted `obtain_groww_access_token`/`fetch_groww_credentials`/`GrowwCredentials` surface.
- Reads `groww/api-key` or `groww/totp-secret` from anywhere in TickVault.
- Writes (put_param / put-parameter) ANY `/tickvault/*/groww/*` parameter from this repo.
- Adds a second minter Lambda / terraform copy / EventBridge cron for Groww tokens.
- Caches the token past an auth failure (removes the `_is_auth_error` token-drop) or removes the 60s auth pacing / 10-min stale alert.
- Passes credentials (not the token) to `GrowwAPI`/`GrowwFeed`.
- Adds a local `.env`/keychain copy of Groww credentials to a production path.

Any such PR MUST be rejected in review even if the operator approves verbally — the operator must update this rule file FIRST with a dated quote.

## §4. Mechanical ratchets (build-failing)

| Ratchet | Pins |
|---|---|
| `crates/common/tests/groww_no_mint_guard.rs` (6 tests) | No mint endpoint / mint fn / credential fetch / credential type in any `crates/*/src`; no credential param paths; sidecar reads `GROWW_SSM_TOKEN_PARAM` + `WithDecryption=True`, never mints, carries the 60s floor + 600s alert marker; smoke script mint-free; requirements pins boto3 not the TOTP lib; `fetch_groww_access_token` exists and its module has no `put_parameter` |
| `crates/common/tests/groww_ssm_seed_script_wiring.rs` (5 tests) | Seed script writes NO groww param, documents the minter ownership, carries no Groww credential env vars; the Rust reader targets the access-token param only |
| `crates/app/src/groww_sidecar_supervisor.rs` unit tests | Supervisor injects `GROWW_SSM_TOKEN_PARAM`, never `GROWW_API_KEY`/`GROWW_TOTP_SECRET`; requirements-fingerprint re-provisioning gate |
| `crates/core/src/feed/groww/auth.rs` unit tests | `validate_groww_token_shape` (empty/placeholder/short/JWT + no-value-echo) |

## §5. Honest envelope (mandatory per operator-charter §F)

> "100% inside the tested envelope: TickVault holds the Groww token in memory only, read read-only from the minter's SSM parameter; on auth failure it re-reads (never mints) at ≥60s pacing and alerts once after ~10 min while continuing to retry. TickVault does NOT control the minter — if the bruteX Lambda stops minting, Groww stays down until the Lambda runs (the alert names it); that is the designed division of ownership, not a TickVault defect. Whether the box's instance profile IS the reader role (vs an explicit sts:AssumeRole hop) is AWS-side wiring verified on the live box, not provable from this repo."

## §6. Auto-driver / Insta-reel explanation

> Sir, the juice shop's fridge has ONE key, and only the head office (bruteX) cuts new keys — every morning at 6:05 they put the fresh key in the shared locker. Our shop boy just TAKES the key from the locker; he is physically unable to cut keys and unable to open the credentials drawer. If the key doesn't work (6:00 changeover), he calmly checks the locker again every minute; after ten minutes he phones you ONCE ("head office hasn't cut today's key") and keeps checking — he never tries to cut his own key, because two shops cutting keys at once breaks BOTH shops' locks.

## §7. Trigger / auto-load

Always loaded. Reinforced on any session editing `crates/core/src/feed/groww/auth.rs`, `crates/core/src/auth/secret_manager.rs`, `crates/app/src/groww_sidecar_supervisor.rs`, `scripts/groww-sidecar/*`, `scripts/aws-seed-ssm-parameters.sh`, or any file containing `groww-token-minter`, `GROWW_SSM_TOKEN_PARAM`, `GROWW_ACCESS_TOKEN_SECRET`, or `fetch_groww_access_token`.
