# Topic — Security + Config + Observability Stack Audit

> **Status:** DRAFT (discussion mode 2026-05-12 → 2026-05-14)
> **Authority:** `operator-charter-forever.md` §C 100% security + hardening > this file.
> **Trigger:** Operator demand 2026-05-12 16:30 IST: "option A" — audit security / config / observability stack.

---

## 🚗 Auto-Driver Story

> Sir, our trading system has SECRETS (Dhan JWT, TOTP, AWS keys), CONFIGS (TOML files), and OBSERVABILITY STACK (Grafana, Prometheus, QuestDB). Today I audit ALL THREE for:
>
> 1. **Secret leakage** — anywhere a JWT, TOTP, or password could leak (logs, errors, dashboards)
> 2. **Config integrity** — what if config is corrupted, missing, or maliciously edited?
> 3. **Observability attack surface** — Grafana admin/admin? Prometheus open to internet? QuestDB UI exposed?
>
> Found 14 gaps. Each needs Friday fix.

---

## 📋 Section 1 — SECRET LEAKAGE audit

### Where secrets live

| Secret | Storage | Access pattern |
|---|---|---|
| Dhan JWT (24h) | AWS SSM `/tickvault/<env>/dhan/access-token` | Valkey cache (24h TTL) + arc-swap in-process |
| Dhan client ID | SSM `/tickvault/<env>/dhan/client-id` | Loaded at boot, in-memory |
| Dhan client secret | SSM `/tickvault/<env>/dhan/client-secret` | Used in token generation |
| TOTP secret (RFC 6238) | SSM `/tickvault/<env>/dhan/totp-secret` | Used per-renewal |
| Telegram bot token | SSM `/tickvault/<env>/telegram/bot-token` | Used in notification service |
| Telegram chat ID | SSM `/tickvault/<env>/telegram/chat-id` | Used in notification service |
| QuestDB pg user/password | SSM `/tickvault/<env>/questdb/pg-*` | Used in storage connection |
| Grafana admin user/password | SSM `/tickvault/<env>/grafana/admin-*` | Used in dashboard provisioning |
| API bearer token | SSM `/tickvault/<env>/api/bearer-token` | Used in API middleware |

### Leakage vectors audited

| Vector | Current defense | Gap? |
|---|---|---|
| **stdout logs** | `Secret<T>` wrapper from `secrecy` crate; Debug impl redacts | ✅ |
| **app.log files** | Same; never `{:?}` on token | ✅ |
| **errors.jsonl** | Same; structured fields exclude Secret<T> | ✅ |
| **Telegram messages** | Per `wave-3-error-codes.md`, NotificationEvent variants don't carry secrets | ✅ |
| **Prometheus labels** | Label values are user-input; could leak via metric name | ⚠️ Verify no SID-as-label patterns |
| **tracing spans** | `#[instrument(skip(token))]` required | ⚠️ Verify all auth code paths |
| **Panic stack traces** | Debug impl of Secret<T> redacts | ✅ |
| **Error messages to clients** | Token never in error response | ✅ |
| **Telegram coalescer samples** | 10 samples max; could include error message with token? | ⚠️ Verify error formatting excludes secrets |
| **Handoff bundle** | Bundle generator writes to disk; could include errors with secrets | ⚠️ **GAP 1** |
| **errors.summary.md** | Hourly snapshot; could include token in error message | ⚠️ **GAP 2** |
| **Git commits** | Pre-commit secret-scan hook runs | ✅ |
| **Docker image** | Static binary; no secrets baked in | ✅ |
| **Grafana dashboards** | Dashboards in repo; no secrets | ✅ |
| **Prometheus alerts.yml** | In repo; no secrets | ✅ |
| **AWS CloudTrail logs** | SSM access logged but no value | ✅ |
| **MCP server queries** | `mcp__tickvault-logs__tail_errors` reads `errors.jsonl` — gap if secrets leaked there | ⚠️ Depends on GAP 1+2 |

### GAP 1 — Handoff bundle may contain error messages with token

The handoff bundle includes raw Dhan responses + log samples. If a log line was `ERROR auth failed: token=eyJ...`, it'd land in the bundle.

**Fix:** bundle generator MUST scrub all Dhan-token patterns (`eyJ...`) and TOTP secrets before write.

**Mechanical check:** add regex scan to bundle generator + ratchet test.

### GAP 2 — errors.summary.md may contain raw error messages with tokens

Same risk. Summary writer (per `observability-architecture.md`) groups by signature hash + truncates message. Need to verify truncation strips token patterns.

**Fix:** summary writer scrubs token patterns before write.

### Tracing skip audit

Need to verify EVERY auth code path uses `#[instrument(skip(token))]`:
- `crates/core/src/auth/token_manager.rs`
- `crates/core/src/auth/secret_manager.rs`
- `crates/core/src/auth/totp_generator.rs`
- `crates/trading/src/oms/api_client.rs`

**Friday action:** grep for `#[instrument` in auth paths; ensure `skip` on every secret arg.

---

## 📋 Section 2 — CONFIG INTEGRITY audit

### Config files

| File | Format | Source |
|---|---|---|
| `config/base.toml` | TOML | Git-tracked |
| `config/{env}.toml` | TOML | Git-tracked, overrides base |
| `config/indicators.toml` | TOML | Git-tracked |
| `config/strategies.toml` | TOML | Git-tracked, hot-reload via notify |
| `Cargo.toml` (workspace) | TOML | Git-tracked, exact pins |
| `deny.toml` | TOML | Git-tracked, supply chain |

### Integrity checks needed

| Concern | Current | Gap? |
|---|---|---|
| **TOML parse fail at boot** | Figment crate returns error; boot halts | ✅ |
| **Missing required keys** | Figment validates against struct | ✅ |
| **Schema drift after upgrade** | Existing fields with new defaults work; missing fields fail | ⚠️ Need migration path |
| **Banned values** (e.g., `dry_run = false` on dev) | No env-aware validation | ⚠️ **GAP 3** |
| **Hot-reload corruption** | notify watcher reads file; if corrupt → reload error | ⚠️ Verify fallback to last-good config |
| **Concurrent edit during reload** | notify debounces 100ms | ✅ |
| **Comment-only edits trigger reload** | Yes; minor inefficiency | LOW |
| **File deleted during runtime** | notify fires; reload fails; keep last-good | ⚠️ Verify fallback |
| **TOML injection (untrusted input)** | Configs are git-tracked, no external input | ✅ |
| **Env variable override unsafe** | figment uses env-var prefix; can override secrets via env | ⚠️ **GAP 4** |
| **Config drift Mac vs AWS** | Parity check (Section 3 of mac-aws-parity plan) | ✅ |
| **Config secrets in git** | Pre-commit secret-scan hook | ✅ |
| **Cargo.toml drift** | banned-pattern scanner + cargo deny | ✅ |
| **Cargo.lock not committed** | Should be committed for reproducibility | ⚠️ Verify .gitignore |

### GAP 3 — Env-aware validation missing

Today, `dry_run = false` is valid for both dev and prod TOML. If dev TOML accidentally has `dry_run = false`, a live trade could execute on dev.

**Fix:** boot-time validation checks env:
- IF `RUST_ENV=dev` AND `[strategy] dry_run = false` → REFUSE TO BOOT
- IF `RUST_ENV=prod` AND `[strategy] dry_run = true` → INFO warning (operator might be intentional)

### GAP 4 — Env-var override could leak secrets

If figment reads `TICKVAULT_DHAN_ACCESS_TOKEN` from environment, anyone with shell access could set it. But also: shell history could capture it.

**Fix:** disable env-var override for secret fields; secrets ONLY from SSM. Document in config TOML.

---

## 📋 Section 3 — OBSERVABILITY STACK ATTACK SURFACE

### Services + ports

| Service | Default port | Public? | Auth? |
|---|---|---|---|
| QuestDB HTTP UI | 9000 | bind 127.0.0.1 ✅ | basic auth via SSM ✅ |
| QuestDB ILP | 9009 | bind 127.0.0.1 ✅ | none (TCP, internal) ✅ |
| QuestDB PG-wire | 8812 | bind 127.0.0.1 ✅ | pg user/pass from SSM ✅ |
| Prometheus | 9090 | bind 127.0.0.1 ✅ | none — local Docker network |
| Grafana | 3000 | bind 127.0.0.1 ✅ | admin user/pass from SSM |
| Valkey | 6379 | bind 127.0.0.1 ✅ | none |
| Tickvault API | 3001 | bind 0.0.0.0 ⚠️ | bearer token via SSM |
| Tickvault metrics | 9091 | bind 127.0.0.1 ✅ | none |
| Alertmanager | 9093 | bind 127.0.0.1 ✅ | none |

### Attack surface analysis

| Service | Threat | Defense | Gap? |
|---|---|---|---|
| **API server :3001** | Internet-exposed (0.0.0.0) for AWS ALB | Bearer token required | ⚠️ **GAP 5** — verify token rotation policy |
| **API /health** | Open endpoint (no auth typical) | Returns minimal info | ✅ |
| **API /metrics** | Bound to localhost | Scraped by Prometheus locally | ✅ |
| **API /portal** | UI endpoint | Bearer required? | ⚠️ **GAP 6** verify |
| **API /api/option-chain** | Data endpoint | Bearer required | ✅ |
| **QuestDB UI :9000** | localhost binding | If operator binds to 0.0.0.0 mistake | ⚠️ **GAP 7** docker-compose verify |
| **Grafana :3000** | localhost binding | Admin password from SSM | ⚠️ **GAP 8** verify default not admin/admin |
| **Prometheus :9090** | No auth! | Localhost only | ✅ but ⚠️ if exposed |
| **Valkey :6379** | No auth! | Localhost only | ⚠️ if exposed = full data access |

### GAP 5 — API bearer token rotation policy missing

The API bearer token is loaded from SSM at boot but never rotated. If leaked, valid until SSM updated + app restarted.

**Fix proposal:**
- Daily rotation: SSM stores `current` + `previous` tokens
- Boot loads both; rejects requests using `previous` after 24h
- Rotation script runs nightly (off-AWS or via Lambda)

**My vote:** SKIP for v1. Bearer token in single-tenant trading app is lower risk than multi-tenant. Document as known limit.

### GAP 6 — `/portal` (DLT Control Panel + options-chain) bearer auth

Per CLAUDE.md API routes table:
- `GET /portal` — DLT Control Panel
- `GET /portal/options-chain` — live options chain

**Question:** are these auth-protected? Operator wants to access from phone browser.

**Action:** verify in `crates/api/src/middleware.rs` that `/portal/*` uses bearer auth.

### GAP 7 — QuestDB UI bind address verification

Docker compose currently uses `127.0.0.1:9000:9000`. If operator edits to `0.0.0.0:9000:9000` (e.g., wants remote access), QuestDB UI becomes internet-accessible with default admin/admin.

**Fix:** add banned-pattern in docker-compose.yml: reject `0.0.0.0` binding for QuestDB UI port.

### GAP 8 — Grafana admin/admin default

Grafana ships with default admin/admin if not provisioned otherwise. We provision from SSM. But if SSM fetch fails at boot, Grafana might fall back to default.

**Fix:** boot-fail if Grafana admin SSM fetch fails. No silent default.

---

## 📋 Section 4 — AUDIT TRAIL INTEGRITY

### Audit tables

15+ audit tables in QuestDB (per Wave 2-D + plans):
- `boot_audit`
- `phase2_audit`
- `ws_reconnect_audit`
- `selftest_audit`
- `depth_rebalance_audit`
- `order_audit` (SEBI 5y)
- `aggregator_seal_audit`
- `prev_oi_load_audit`
- `bhavcopy_load_audit`
- `cross_verify_mismatches`
- `cross_verify_summary`
- `claude_handoff_audit`
- `aws_budget_audit`
- `parity_check_audit`
- `telegram_event_audit` (NEW from Telegram audit plan)
- + more

### Threats

| Threat | Defense | Gap? |
|---|---|---|
| **Operator manually DROPs a table** | QuestDB admin permission separate from app | ⚠️ **GAP 9** verify app user lacks DROP |
| **Operator runs DELETE on audit table** | App user has DELETE? | ⚠️ **GAP 10** verify app user lacks DELETE |
| **Audit table partition dropped wrong** | Partition manager has hard floor (refuses today - 30d) | ✅ |
| **Audit row never persisted** | Rescue ring + spill catches | ✅ |
| **Audit table schema drift** | Self-heal via `ALTER ADD COLUMN IF NOT EXISTS` | ✅ |
| **S3 archive deleted by AWS misconfig** | S3 versioning + MFA-delete | ⚠️ **GAP 11** verify versioning enabled |
| **SEBI 5y retention violated** | Lifecycle policy → Glacier → never delete | ⚠️ **GAP 12** verify policy |

### GAP 9 + 10 — App user permissions audit

App should connect to QuestDB with a USER that has:
- ✅ INSERT
- ✅ SELECT
- ✅ ALTER ADD COLUMN
- ❌ DROP TABLE
- ❌ DROP PARTITION
- ❌ DELETE FROM audit tables

Admin user (separate) has full permission, used only for migrations.

**Action:** verify QuestDB user permissions in deploy config.

### GAP 11 — S3 versioning + MFA-delete

If S3 archive bucket allows non-MFA delete, an attacker (or AWS misconfig) could wipe audit history.

**Fix:** enable S3 versioning + MFA-delete on archive bucket. Document in `aws-budget.md`.

### GAP 12 — Lifecycle retention verification

S3 lifecycle policy: 0-90d hot → 90-365d IT → ≥365d Glacier Deep Archive. SEBI 5y compliance.

**Mechanical check:** automated daily test that archive bucket has correct lifecycle policy attached.

---

## 📋 Section 5 — SUPPLY CHAIN

| Check | Status |
|---|---|
| `cargo audit` for CVE | ✅ in CI |
| `cargo deny` for licenses + bans | ✅ in CI |
| Workspace deps pinned exact (no ^/~/*) | ✅ banned-pattern hook |
| Docker images pinned SHA256 | ✅ deploy/docker/docker-compose.yml |
| `cargo update` BANNED | ✅ hook |
| Git commit signing | ⚠️ **GAP 13** verify enabled |
| Pre-commit hooks bypass via `--no-verify` | ✅ banned by operator |

### GAP 13 — Git commit signing

CLAUDE.md says "NEVER skip --no-gpg-sign". Verify GPG signing is configured + ratcheted via CI test.

**Action:** add CI step that verifies last 10 commits are signed.

---

## 📋 Section 6 — NETWORK SECURITY

| Concern | Status |
|---|---|
| TLS to Dhan API (REST) | ✅ reqwest with rustls + aws-lc-rs |
| TLS to Dhan WS | ✅ tokio-tungstenite with rustls |
| TLS to AWS SSM | ✅ aws-sdk-ssm default |
| TLS to Telegram | ✅ |
| Cert pinning | ❌ **GAP 14** not implemented; rely on system trust store |
| DNS over HTTPS | ❌ system DNS; could be MITM'd if rogue WiFi |

### GAP 14 — Certificate pinning

For Dhan-API connections specifically, cert pinning would catch:
- MITM attacks via rogue WiFi
- DNS hijacking
- CA compromise

**Trade-off:** cert pinning requires updates when Dhan rotates certs (annually). High maintenance cost.

**My vote:** SKIP for v1. Document as known limit. Add post-deploy if MITM threat materializes.

---

## 📊 GAPS SUMMARY (14 found)

| # | Gap | Severity | Fix complexity |
|---|---|---|---|
| 1 | Handoff bundle may contain token in error messages | High | LOW (regex scrub) |
| 2 | errors.summary.md may contain raw token | High | LOW (regex scrub) |
| 3 | Env-aware config validation (dry_run check) | Medium | LOW (boot validation) |
| 4 | Env-var override could leak secrets | Medium | LOW (figment policy) |
| 5 | API bearer token rotation | Low | Medium (defer v1) |
| 6 | /portal/* bearer auth verification | Medium | Verify-only |
| 7 | QuestDB UI bind 0.0.0.0 risk | High | LOW (banned-pattern) |
| 8 | Grafana admin/admin default fallback | High | LOW (boot fail) |
| 9 | App user DROP permission | High | Verify-only |
| 10 | App user DELETE on audit tables | High | Verify-only |
| 11 | S3 versioning + MFA-delete | High | LOW (AWS config) |
| 12 | Lifecycle policy verification | Medium | LOW (CI test) |
| 13 | Git commit signing CI verify | Medium | LOW (CI step) |
| 14 | Cert pinning to Dhan | Low | High (DEFER) |

**HIGH severity gaps (8):** all FIXABLE Friday with <30 LoC each.

---

## 🛡️ Z+ 7-Layer for security stack

| Layer | Mechanism |
|---|---|
| L1 DETECT | `tv_secret_redaction_failures_total` counter; `tv_auth_failures_total` per endpoint |
| L2 VERIFY | Sample 1% of log lines + handoff bundles; assert no token patterns |
| L3 RECONCILE | Weekly: cargo audit + cargo deny + secret-scan history |
| L4 PREVENT | Banned-pattern hook + secret-scan pre-commit + Secret<T> wrapper |
| L5 AUDIT | `auth_audit` table (NEW — every auth attempt, success/fail) |
| L6 RECOVER | On secret leak detected: rotate immediately + audit + Telegram CRITICAL |
| L7 COOLDOWN | Between rotations: 5 min (avoid token storm) |

### NEW Prometheus metrics for security

| Metric | Purpose |
|---|---|
| `tv_secret_redaction_failures_total` | Times redaction missed a secret |
| `tv_auth_failures_total{endpoint}` | API auth failures |
| `tv_config_validation_failures_total{reason}` | Boot config issues |
| `tv_audit_table_drop_attempts_total` | (Should always be 0) |
| `tv_grafana_admin_login_failures_total` | Dashboard attacks |

---

## 🚨 5 NEW worst-cases (W196-W200)

| # | Scenario | Defense |
|---|---|---|
| W196 | Token leaks into handoff bundle | GAP 1 fix — regex scrub bundle generator |
| W197 | Operator pushes dry_run=false to dev | GAP 3 fix — env-aware validation |
| W198 | QuestDB UI accidentally exposed to internet | GAP 7 fix — banned-pattern in compose |
| W199 | App user accidentally has DROP permission | GAP 9 fix — verify via CI |
| W200 | S3 archive deleted (audit history wiped) | GAP 11 fix — MFA-delete + versioning |

---

## 📊 Total worst-case coverage: 411 paths

| Plan | Count |
|---|---|
| Prior plans (W1-W195) | 195 + 211 cells |
| NEW Security/Config/Observability (W196-W200) | 5 |
| **GRAND TOTAL** | **~411** |

---

## 🎯 Discussion items

### D1 — Implement GAPS 1-4, 7-12 in Friday work?

8 HIGH-severity gaps fixable with <300 LoC total. My vote: YES.

### D2 — Defer GAPS 5, 13, 14 (low value or high complexity)?

API bearer rotation + cert pinning + commit signing CI. My vote: defer to post-deploy.

### D3 — Add `auth_audit` table?

NEW table tracking every auth attempt. SEBI-relevant. My vote: YES.

### D4 — Pre-deploy security scan in CI?

CI step that scans final binary for embedded secrets. My vote: YES.

---

## 🎤 Final summary

| Area | Gaps | Status |
|---|---|---|
| Secret leakage | 2 (handoff bundle, summary writer) | Fixable Friday |
| Config integrity | 2 (env validation, env-var override) | Fixable Friday |
| Observability attack surface | 4 (port bindings, auth defaults) | Fixable Friday |
| Audit integrity | 4 (DB permissions, S3 retention) | Mostly verify-only |
| Supply chain | 1 (commit signing) | Defer |
| Network | 1 (cert pinning) | Defer |

**8 HIGH severity gaps → all Friday-fixable.**
**6 Medium/Low → defer or verify.**

**Net: ~300 LoC of security hardening across the 14 gaps. Friday work bumped to ~7,800 LoC total (from 7,500).**

Discussion mode continues. NO IMPLEMENTATION.
EOF
echo "security/config/observability audit locked"