# Full Project Security Audit Report

**Date:** 2026-03-07
**Scope:** Entire tickvault repository — code, dependencies, CI/CD, Docker, docs, hooks
**Method:** 6 parallel deep-scan agents + manual cargo check/clippy/test verification
**Build Status:** CLEAN (0 clippy warnings, 163 tests passed, 0 failures)

---

## EXECUTIVE SUMMARY

| Area | Status | Critical | High | Medium | Low |
|------|--------|----------|------|--------|-----|
| Rust Source Code Security | EXCELLENT | 0 | 1 | 0 | 2 |
| Dependency Management | NEEDS FIX | 1 | 1 | 1 | 0 |
| CI/CD & Hooks | NEEDS REVIEW | 2 | 2 | 3 | 0 |
| Docker & Deployment | NEEDS FIX | 2 | 4 | 3 | 1 |
| Hot-Path Compliance | EXCELLENT | 0 | 0 | 0 | 0 |
| Documentation Consistency | NEEDS FIX | 0 | 1 | 2 | 2 |
| **TOTAL** | | **5** | **9** | **9** | **5** |

**Overall Security Posture:** The Rust application code is production-grade with mature security practices. The primary risk surface is infrastructure configuration (Docker/Traefik/Grafana exposed without auth) and dependency version drift.

---

## SECTION 1: RUST SOURCE CODE SECURITY

### 1.1 Authentication & Secrets — SECURE

- All secrets fetched from AWS SSM Parameter Store only (no hardcoded secrets anywhere)
- `SecretString` wrapping enforces `[REDACTED]` in Debug output
- Environment variable `ENVIRONMENT` validated against `[a-zA-Z0-9-]` only (path traversal prevented)
- TOTP implementation uses `totp_rs` (RFC 6238 compliant)
- Token management uses `arc-swap` for O(1) atomic reads
- Zeroize-on-drop for all credential types

**Files verified:** `crates/core/src/auth/secret_manager.rs`, `token_manager.rs`, `totp_generator.rs`, `types.rs`

### 1.2 WebSocket Security — SECURE

- TLS via `aws_lc_rs` CryptoProvider (FIPS-eligible)
- Native root CA certificates loaded; empty root store rejected
- HTTP/1.1 ALPN explicitly set (prevents proxy issues)
- No custom certificate validation bypass
- Reconnection with exponential backoff
- All channels bounded (capacity 100 or 1)

**Files verified:** `crates/core/src/websocket/tls.rs`, `connection.rs`, `connection_pool.rs`

### 1.3 Parser Input Validation — SECURE

- All binary parsers length-check before indexing
- NaN/Infinity guards on all float fields
- `from_le_bytes` used safely with pre-validated slices
- CSV parser auto-detects columns from header, strips UTF-8 BOM
- No panic paths from malformed input

**Files verified:** All 12 files in `crates/core/src/parser/`

### 1.4 API Security

#### HIGH: Overly Permissive CORS Configuration
**File:** `crates/api/src/lib.rs:23-26`
```rust
let cors = CorsLayer::new()
    .allow_origin(Any)
    .allow_methods(Any)
    .allow_headers(Any);
```
**Risk:** Allows requests from ANY origin. Acceptable for internal-only service behind Traefik, but if API port is exposed directly, any website can make authenticated requests.
**Recommendation:** Restrict to specific origins in production.

### 1.5 Unsafe Code — NONE

Zero `unsafe` blocks in the entire codebase. All performance achieved through safe Rust patterns.

### 1.6 Error Handling — COMPLIANT

- All `unwrap()`/`expect()` in production code are marked with `// APPROVED:` comments
- 3 approved `expect()` locations: lock poison (architecturally unrecoverable), TLS bootstrap (mandatory), stream reunite (same stream guarantee)
- Compile-time deny lints enforced across all crates

### 1.7 Concurrency — SECURE

- No data races detected
- All channels bounded (no unbounded channels anywhere)
- Lock poison handling properly approved
- `papaya` used instead of `DashMap` (lock-free)

---

## SECTION 2: DEPENDENCY MANAGEMENT

### CRITICAL: 10 Version Pinning Violations (Cargo.toml vs Cargo.lock)

The root Cargo.toml declares exact versions that do NOT match what is resolved in Cargo.lock:

| Dependency | Declared | Resolved | Location |
|---|---|---|---|
| tokio | 1.49.0 | 1.50.0 | Cargo.toml:15 |
| anyhow | 1.0.99 | 1.0.102 | Cargo.toml:92 |
| arc-swap | 1.7.1 | 1.8.2 | Cargo.toml:57 |
| chrono | 0.4.41 | 0.4.44 | Cargo.toml:82 |
| clap | 4.5.32 | 4.5.60 | Cargo.toml:122 |
| reqwest | 0.12.15 | 0.12.28 | Cargo.toml:70 |
| tower | 0.5.2 | 0.5.3 | Cargo.toml:74 |
| tower-http | 0.6.5 | 0.6.8 | Cargo.toml:75 |
| uuid | 1.16.0 | 1.21.0 | Cargo.toml:125 |
| zerocopy | 0.8.39 | 0.8.40 | Cargo.toml:22 |

**Impact:** Violates Principle #3 ("Every version pinned"). Declared versions imply exact pinning but Cargo.lock has drifted to newer versions via transitive resolution.

### HIGH: Fuzz Crate Dependencies Not in Workspace

**File:** `fuzz/Cargo.toml:12-13`
- `libfuzzer-sys = "0.4.9"` and `toml = "1.0.4"` are pinned locally, not through workspace
- Acceptable since fuzz crate is excluded from workspace, but creates maintenance burden

### MEDIUM: Competing Dependency Automation Tools

Both `renovate.json` and `.github/dependabot.yml` are ACTIVE and manage the same Cargo.toml:
- Renovate automerges Cargo PRs with squash
- Dependabot creates daily PRs without auto-merge
- Creates duplicate PRs and race conditions

**Recommendation:** Disable one (preferably Dependabot, since Renovate has more granular config).

### POSITIVE: Supply Chain Configuration

- `deny.toml` properly configured with advisories, license whitelist, and banned crates (bincode, openssl)
- `supply-chain/config.toml` imports Mozilla, Google, Bytecode Alliance audits
- All duplicate version skips (rustls, rustls-webpki) documented with justification
- Docker images use SHA256 digest pinning (not `:latest`)

---

## SECTION 3: CI/CD & HOOKS

### CRITICAL: Auto-Merge Workflow Race Condition
**File:** `.github/workflows/auto-merge.yml:33-39`
- Triggers on `pull_request` event BEFORE CI checks complete
- Does NOT wait for build-and-verify, security-and-audit, or mutation-testing
- Could merge broken/vulnerable code

**Recommendation:** Add `needs: [build-and-verify, security-and-audit]` or use `check_suite` event.

### CRITICAL: Banned Pattern Scanner Self-Approval
**File:** `.claude/hooks/banned-pattern-scanner.sh:44-60`
- Any engineer can add `// APPROVED:` comment to exempt ANY banned pattern
- No co-sign or second-human approval mechanism
- Malicious commit could bypass all hot-path checks with a single comment

**Recommendation:** Require CODEOWNERS approval for any file containing APPROVED exemptions.

### HIGH: Pre-Push Gate State File Race Condition
**File:** `.claude/hooks/pre-push-gate.sh:56-74`
- Checks parent commit hash as valid (HEAD~1), allowing push of unverified current HEAD
- 300-second TTL is too long — code could change between verify and push
- State file can be trivially deleted to reset

**Recommendation:** Only match current HEAD, reduce TTL to 60 seconds.

### HIGH: Secret-Scan Workflow Incomplete Coverage
**File:** `.github/workflows/secret-scan.yml:61-101`
- `grep -v 'secret-scan.yml'` can cause false negatives (removes entire match line, not just self-reference)
- Missing patterns: Google Cloud API keys, Azure keys, Anthropic API keys (sk-ant-...)

### MEDIUM: Pre-PR Gate Merge-Base Fallback
**File:** `.claude/hooks/pre-pr-gate.sh:200-206`
- If `origin/main` doesn't exist locally, falls back to only last 10 commits
- Commits beyond the 10th are not validated

### MEDIUM: Test-Count Guard Writable Baseline
**File:** `.claude/hooks/test-count-guard.sh:25-29`
- Baseline file can be deleted to reset
- Should be version-controlled or read-only

### MEDIUM: Auto-Save Remote Missing Mutex
**File:** `.claude/hooks/auto-save-remote.sh:137-197`
- Two concurrent sessions can both detect collisions and overwrite each other's warnings
- No mutual exclusion mechanism

---

## SECTION 4: DOCKER & DEPLOYMENT

### CRITICAL: Grafana Anonymous Admin Access
**File:** `deploy/docker/docker-compose.yml:221-222`
```yaml
GF_AUTH_ANONYMOUS_ENABLED: "true"
GF_AUTH_ANONYMOUS_ORG_ROLE: "Admin"
```
- Anonymous users get full Admin privileges
- On AWS with public IP, exposes all dashboards, datasources, and alert configs

**Recommendation:** Set `GF_AUTH_ANONYMOUS_ORG_ROLE: "Viewer"` or disable anonymous access entirely.

### CRITICAL: Traefik Dashboard Unauthenticated
**File:** `deploy/docker/traefik/traefik.yml:9-10`
```yaml
api:
  dashboard: true
  insecure: true
```
- Dashboard exposed on :8080 without authentication
- Reveals all routes, services, middleware, internal network topology

**Recommendation:** Set `insecure: false`, add OAuth2/BasicAuth middleware.

### HIGH: QuestDB HTTP Console Exposed (port 9000)
**File:** `deploy/docker/docker-compose.yml:37-40`
- Web console allows unauthenticated SQL queries
- Full database access: tick data, instruments, audit trails

### HIGH: Prometheus UI Exposed (port 9090)
**File:** `deploy/docker/docker-compose.yml:100-101`
- No authentication on metrics API
- Exposes all trading system metrics

### HIGH: Loki Without Authentication
**File:** `deploy/docker/loki/loki-config.yml:5`
- `auth_enabled: false` — any service can inject/read logs

### HIGH: Valkey (Redis) Without Password
**File:** `deploy/docker/docker-compose.yml:70`
- No `--requirepass` flag
- Unauthenticated access to cached state

### MEDIUM: Missing TLS/HTTPS in Traefik
**File:** `deploy/docker/traefik/traefik.yml:22-26`
- Port 443 defined but no certificate provider configured
- Traffic could be plaintext despite HTTPS entrypoint

### MEDIUM: Missing Docker Resource Limits
- No `mem_limit`, `cpus`, or ulimits on any service
- Single service can starve others (e.g., Prometheus TSDB compaction)

### MEDIUM: Jaeger OTLP Endpoints Open
**File:** `deploy/docker/docker-compose.yml:185-190`
- Ports 4317/4318 accept traces from any source
- Trace injection and metrics exfiltration possible

### LOW: Docker Socket Mount
- Alloy mounts `/var/run/docker.sock` (with `:ro`) — acceptable but worth noting

---

## SECTION 5: HOT-PATH COMPLIANCE

### Status: FULLY COMPLIANT

| Check | Result |
|-------|--------|
| `.clone()` on hot path | CLEAN (1 instance in constructor, exempted) |
| `DashMap` usage | CLEAN (imported but unused; papaya preferred) |
| `dyn` trait objects on hot path | CLEAN (enum_dispatch used) |
| `println!`/`eprintln!` | CLEAN (tracing only) |
| `.unwrap()` in production | CLEAN (denied at compile time) |
| Unbounded channels | CLEAN (all bounded: 1, 100, or 200) |
| `.env` file usage | CLEAN (TOML + AWS SSM) |
| `localhost` references | CLEAN (Docker DNS only in production) |
| Allocations in parsers | CLEAN (zero-copy throughout) |
| O(n) operations | CLEAN (O(1) verified) |

**Benchmark budgets enforced:** tick_binary_parse <10ns, tick_pipeline_routing <100ns, full_tick_processing <10us

---

## SECTION 6: DOCUMENTATION CONSISTENCY

### HIGH: Boot Sequence Incomplete in CLAUDE.md
**File:** `CLAUDE.md:70`
- Documents: `CryptoProvider -> Config -> Logging -> Auth -> QuestDB -> Universe -> WebSocket -> TickProcessor -> API -> TokenRenewal -> Shutdown`
- Actual (main.rs): 12 steps including Observability (Step 2) and Notification (Step 4) which are missing

### MEDIUM: Workspace Cargo.toml Version Drift (formerly Bible)
- `deadpool-redis`: Bible says 0.22.1, Cargo.toml has 0.23.0
- `statrs`, `mockall`, `once_cell`, `parking_lot`: Listed in Bible but not in Cargo.toml or code

### MEDIUM: Bible Reconciliation Date Stale
- Bible last reconciled 2026-02-26 but dependencies have drifted since

### LOW: Fuzz Crate Edition Duplication
- `fuzz/Cargo.toml` duplicates `edition = "2024"` and `rust-version = "1.95.0"` instead of workspace inheritance (acceptable since excluded from workspace)

### LOW: DashMap in Dependencies
- `dashmap` appears in root Cargo.toml but is imported nowhere — dead dependency

---

## PRIORITIZED ACTION ITEMS

### P0 — Fix Before Production

| # | Finding | Fix |
|---|---------|-----|
| 1 | Grafana anonymous Admin access | Set `GF_AUTH_ANONYMOUS_ORG_ROLE: "Viewer"` or disable |
| 2 | Traefik dashboard unauthenticated | Set `insecure: false`, add auth middleware |
| 3 | QuestDB/Prometheus/Loki/Valkey exposed without auth | Restrict ports to Docker network, add auth |
| 4 | 10 Cargo.toml version mismatches | Update declared versions to match Cargo.lock |
| 5 | Auto-merge workflow race condition | Require CI status checks before merge |

### P1 — Fix Soon

| # | Finding | Fix |
|---|---------|-----|
| 6 | CORS allow-all in API | Restrict origins for production |
| 7 | Pre-push gate race condition (300s TTL) | Reduce TTL to 60s, match current HEAD only |
| 8 | Secret-scan incomplete patterns | Add Google/Azure/Anthropic key patterns |
| 9 | Banned pattern self-approval | Require CODEOWNERS for APPROVED comments |
| 10 | Traefik missing TLS certificates | Configure Let's Encrypt or self-signed |

### P2 — Fix When Convenient

| # | Finding | Fix |
|---|---------|-----|
| 11 | Boot sequence incomplete in CLAUDE.md | Add Observability and Notification steps |
| 12 | Bible version mismatches | Update deadpool-redis, clarify future deps |
| 13 | Competing automation (Renovate + Dependabot) | Disable one |
| 14 | Docker resource limits missing | Add mem_limit/cpus to all services |
| 15 | Test-count baseline writable | Version-control or make read-only |

---

## WHAT'S WORKING WELL

1. **Zero unsafe code** in entire codebase
2. **Compile-time lint enforcement** (deny unwrap/expect/println in production)
3. **SecretString + AWS SSM** for all credentials — no hardcoded secrets
4. **Zero-copy parsers** with O(1) performance guarantees
5. **Bounded channels everywhere** — no memory leak risk
6. **163 tests passing** with comprehensive coverage
7. **cargo-deny** + **cargo-vet** supply chain auditing
8. **Docker image SHA256 pinning** (no `:latest` tags)
9. **NaN/Infinity guards** on all financial data
10. **Exponential backoff** on all retry paths

---

*Report generated by 6 parallel deep-scan agents covering: Cargo dependencies, CI/CD+hooks, Rust security, Docker+deployment, hot-path compliance, documentation consistency.*
