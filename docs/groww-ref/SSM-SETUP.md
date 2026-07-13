# Groww feed — AWS SSM credential setup

> **⚠ SUPERSEDED (2026-07-02 operator lock, banner added 2026-07-13):** this file's Groww-credential guidance predates and conflicts with the shared token-minter lock — [`.claude/rules/project/groww-shared-token-minter-2026-07-02.md`](../../.claude/rules/project/groww-shared-token-minter-2026-07-02.md) is authoritative. TickVault now only ever READS the bruteX-Lambda-minted `/tickvault/<env>/groww/access-token` (the `api-key`/`totp-secret` params below are Lambda-only by IAM; TickVault NEVER mints — the mint path described below was deleted). Token lifecycle facts: [`17-token-lifecycle.md`](./17-token-lifecycle.md). Content below retained as the 2026-06-19 historical record.

> **Scope:** the two secrets the native Rust Groww auth (`crate::feed::groww::auth`)
> reads. Mirrors the Dhan SSM convention `/tickvault/<env>/<service>/<key>`.
> **Locked 2026-06-19** — the Rust code reads exactly these names; don't rename
> without updating `crates/common/src/constants.rs` (`SSM_GROWW_SERVICE`,
> `GROWW_API_KEY_SECRET`, `GROWW_TOTP_SECRET`).

## The two parameters (per environment: `dev`, `staging`, `prod`)

| SSM parameter name | Type | Value to store |
|---|---|---|
| `/tickvault/<env>/groww/api-key` | SecureString | Groww **API key / "TOTP token"** (groww.in/trade-api keys page → *Generate TOTP token*) |
| `/tickvault/<env>/groww/totp-secret` | SecureString | Groww **TOTP secret** (the seed for the rotating 6-digit code; SHA1/6/30) |

The TOTP flow is used because it has **no expiry** (best for a long-running
service). The API-key+secret flow needs daily approval — not used.

## Console
**Create parameter** → Name `/tickvault/staging/groww/api-key` → Tier **Standard**
→ Type **SecureString** → KMS key: **same default as the Dhan rows** → paste the
value → **Create**. Repeat for `…/groww/totp-secret`, then the `dev`/`prod` pairs.

## CLI (ap-south-1)
```bash
aws ssm put-parameter --region ap-south-1 --type SecureString \
  --name "/tickvault/staging/groww/api-key"     --value "PASTE_GROWW_TOTP_TOKEN"
aws ssm put-parameter --region ap-south-1 --type SecureString \
  --name "/tickvault/staging/groww/totp-secret" --value "PASTE_GROWW_TOTP_SECRET"
# repeat with /tickvault/dev/groww/...  and  /tickvault/prod/groww/...
```

## Notes
- **IAM:** the app's SSM read policy must cover `/tickvault/<env>/groww/*` —
  already covered if it uses a `/tickvault/<env>/*` wildcard (same as it reads
  `…/dhan/*`).
- **Safe to add now:** `feeds.groww_enabled` defaults **false**, so even with
  these present nothing runs until the operator flips Groww on (e.g. a local
  `config/<env>.toml` override) — prod stays Dhan-only.
- **What reads them:** at boot, when `feeds.groww_enabled = true`, the app runs a
  background **auth smoke-check** (`run_groww_auth_smoke_check`) that fetches
  these two secrets, generates a TOTP, and obtains a Groww access token —
  logging `Groww access-token auth OK` or a failure naming these paths.
