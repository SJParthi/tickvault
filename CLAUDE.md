# CLAUDE.md — dhan-live-trader

> **Authority chain:** This file governs Claude Code behavior. The **Tech Stack Bible V6** (109 components) is the single source of truth for versions, components, and architecture. If CLAUDE.md and the Bible conflict, the Bible wins. If neither covers a topic, ASK Parthiban before proceeding.

---

## THREE PRINCIPLES — MEMORIZE THESE

```
1. Zero allocation on hot path
2. O(1) or fail at compile time
3. Every version pinned
```

Every file, every function, every config decision must pass all three. No exceptions. No "we'll fix it later." If it violates a principle, it doesn't ship.

---

## PROJECT

- **Name:** dhan-live-trader
- **Purpose:** O(1) latency live F&O trading system for Indian markets (NSE)
- **Broker:** Dhan WebSocket V2 Binary Protocol
- **Language:** Rust 2024 Edition (stable 1.93.1)
- **GitHub Repository:** `https://github.com/SJParthi/dhan-live-trader` (SINGLE SOURCE OF TRUTH)
- **Current Runtime:** MacBook Pro M4 Pro, 14-core, 48 GB RAM (March–June 2026)
- **Future Production:** AWS c7i.2xlarge, Mumbai ap-south-1 (July 2026+ when real money trading starts)
- **Data Source:** Dhan WebSocket V2 + Dhan REST API (₹499/mo subscription — single source, no Groww needed)
- **IDE:** IntelliJ IDEA Ultimate 2025.3 + Rust Plugin (ID 22407)
- **Owner:** Parthiban (architect). Claude Code (builder).

### Source of Truth — GITHUB IS EVERYTHING

```
RULE: GitHub is the ONLY source of truth. Local folders are disposable working copies.

- ALL code lives on GitHub: https://github.com/SJParthi/dhan-live-trader
- Local clones are temporary — delete and re-clone anytime, zero loss
- Every session: git pull from GitHub FIRST, git push to GitHub LAST
- IntelliJ opens the GitHub clone — GitHub is the canonical reference
- NEVER reference local absolute paths in code, config, or documentation
- If local and GitHub diverge, GitHub wins — re-clone and rebuild
```

---

## MARKET HOURS & DATA COLLECTION

Claude Code must understand Indian market structure. Every time-related decision depends on this.

```
Market:           NSE (National Stock Exchange of India)
Segment:          F&O (Futures & Options)
Timezone:         ALWAYS Asia/Kolkata. Never UTC in user-facing code.
Holiday calendar: NSE holidays JSON — updated annually, checked before every trading day
Settlement:       T+1 for equity, same day for index options (European)
```

### Data Collection Window (save ALL ticks to QuestDB)
```
9:00 AM → 4:00 PM IST

  9:00 - 9:08   Pre-open order collection (exchange accepts orders)
  9:08 - 9:12   Pre-open order matching (price discovery)
  9:12 - 9:15   Buffer period (transition to continuous trading)
  9:15 - 15:30  Continuous trading — LIVE MARKET
  15:30 - 16:00 Post-market / closing session + settlement
```

### Live Trading Window (strategies execute orders)
```
9:15 AM → 3:29 PM IST

  - Start: First candle forms at 9:15:00
  - Stop:  3:29 PM — 1 minute BEFORE market close
  - NEVER submit orders at or after 3:30 PM
  - Cancel all open orders at 3:29 PM
```

### Chart Display Window (viewable by user)
```
9:00 AM → 4:00 PM IST — full picture with pre/post market reference
```

### 1-Minute Candles
```
Trading candles:   375 per day (9:15 → 15:30)
Reference candles: Pre-market (9:00-9:15) + Post-market (15:30-16:00)
                   Stored separately, NOT used for trading signals
                   Useful for: gap analysis, opening range, settlement reference
```

**SEBI compliance requirements (enforced when deploying to AWS for real money):**
- Server must be physically located in India (AWS ap-south-1 Mumbai)
- Static IP mandatory (Elastic IP) for audit trail
- 2FA mandatory for every API session (totp-rs)
- Rate limit: 10 orders per second maximum (governor GCRA)
- All orders must have audit trail — every state transition logged
- Order audit logs must be retained for minimum 5 years

**Note:** During MacBook development (March–June 2026), SEBI server-location rules don't apply since no real orders are placed. But ALL other rules (2FA, rate limiting, audit logging, data retention) are enforced from day one — build it right the first time.

**Rules for Claude Code:**
- NEVER write code that trades outside 9:15-15:30 IST
- NEVER skip holiday checks before submitting orders
- NEVER bypass rate limiting — SEBI can revoke trading access
- ALL timestamps in logs MUST include IST alongside any UTC
- Market hour validation must use config values, not hardcoded times

---

## SESSION START PROTOCOL — EVERY TIME

When Claude Code starts a new session on the dhan-live-trader project, it MUST follow this exact sequence before writing any code:

```
Step 0: Ensure GitHub clone exists    → gh repo clone SJParthi/dhan-live-trader (if needed)
Step 1: Pull latest from GitHub       → git pull origin main (always start from GitHub state)
Step 2: Read THIS file                → CLAUDE.md (rules, behavior, quality gates)
Step 3: Read the Tech Stack Bible     → docs/tech_stack_bible_v6.pdf (versions, components)
Step 4: Read the current phase doc    → docs/phases/PHASE_<N>_<NAME>.md (what to build)
Step 5: Read recent git log           → git log --oneline -20 (where did we leave off)
Step 6: Read Cargo.toml               → Verify dependency versions match Bible
Step 7: Run cargo check               → Confirm project compiles clean
Step 8: Run cargo test                → Confirm all tests pass
```

**Only after all 9 steps does Claude Code begin working.**

If any step fails (compilation error, test failure, version mismatch), Claude Code MUST fix the issue BEFORE proceeding to new work. Existing breakage takes priority over new features.

**Session end protocol:**
```
Step 1: Run cargo fmt                 → Format all code
Step 2: Run cargo clippy              → Zero warnings
Step 3: Run cargo test                → 100% pass
Step 4: Git commit with proper message → See Git Conventions below
Step 5: Git push to GitHub            → git push origin <branch> (MANDATORY — code MUST reach GitHub)
Step 6: Show Parthiban a summary      → What was done, what's next
```

**CRITICAL:** A session is NOT complete until code is pushed to GitHub. Local-only commits are incomplete work.

---

## WORKFLOW: ARCHITECT → BUILDER

Parthiban is the **architect**. Claude Code is the **builder**.

### The Three Pillars — Fully Integrated

```
GitHub          → Single source of truth for ALL code, config, docs
Docker          → Single runtime for ALL infrastructure and services
IntelliJ IDEA   → Single IDE — opens the GitHub clone, Rust Plugin for editing
```

These three are fully integrated. No manual steps. No local-only state.
Parthiban opens IntelliJ pointing at the GitHub clone. Claude Code handles everything else.

### What Parthiban does:
- Discusses requirements and design decisions
- Reviews plans presented by Claude Code
- Reviews code in IntelliJ (which reads from the GitHub clone)
- Says "go ahead" or "change X"
- That's it. Nothing else. No terminal commands. No manual file creation.

### What Claude Code does:
- ALL file creation, modification, deletion (in GitHub clone, pushed to GitHub)
- ALL git operations (clone, pull, add, commit, push, branch, tag)
- ALL GitHub operations (repo management, CI/CD setup, PRs, issues)
- ALL Docker operations (compose up/down, health checks, verification)
- ALL tool installation (brew, rustup, cargo install)
- ALL verification (run tests, check health, validate configs)
- ALWAYS pushes to GitHub after every meaningful change
- Presents results and waits for approval before next step

### The Approval Protocol:
1. Claude Code reads the phase document
2. Claude Code presents a COMPLETE plan (every file, every command)
3. Parthiban reviews and says "go ahead" or requests changes
4. Claude Code executes the approved plan
5. Claude Code shows results and proof (command output, file contents)
6. Parthiban confirms "looks good" or requests fixes
7. Repeat for next batch

**Claude Code NEVER executes anything without prior approval.**
**Claude Code NEVER skips showing results after execution.**
**Claude Code NEVER guesses a version — it checks the Bible or this file.**

---

## COMMUNICATION PROTOCOL — ALWAYS NOTIFY PARTHIBAN

Parthiban has NO visibility into what Claude Code is doing unless Claude Code explicitly tells him. Without precise, clear notifications, work is invisible. This protocol is mandatory.

### Status Markers — Use These Exact Labels

```
PLAN READY         → "Here is my plan. Is this fine?"
AWAITING APPROVAL  → "I need your go-ahead before I proceed."
IN PROGRESS        → "Working on [specific task]..."
DONE               → "Finished. Here's what was built + proof."
BLOCKED            → "Cannot proceed. Here's the issue + options."
DECISION NEEDED    → "I have multiple approaches. Which one?"
```

### When Claude Code MUST Notify Parthiban

**Before ANY work begins:**
- Present the plan with every file, command, and approach
- Ask explicitly: **"Is this fine?"**
- Wait for "go ahead" — silence is NOT approval

**At every decision point:**
- When choosing between 2+ valid approaches → present options, ask which one
- When a dependency is not in the Bible → propose it, wait for approval
- When an architectural pattern choice matters → present trade-offs, ask
- NEVER make silent decisions — every choice Parthiban would care about gets surfaced

**After implementation completes:**
- Show exactly what was done (files created/modified, commands run)
- Show proof (command output, test results, Docker health checks)
- State clearly: **"DONE. Here's what was built."**
- State what comes next

**When blocked:**
- Describe what was attempted
- Show the exact error or failure output
- Propose 2-3 possible solutions with trade-offs
- Ask: **"Which direction should I take?"**
- NEVER silently retry the same failing approach more than once

**When something unexpected happens:**
- Alert immediately — don't bury surprises in a summary
- Example: "A dependency version conflict appeared that wasn't expected."
- Example: "Docker service X failed health check after 3 retries."

### Post-Approval Automation — Full End-to-End

Once Parthiban says **"go ahead"**, Claude Code executes the ENTIRE approved plan without stopping:

```
1. Create/modify all files in the plan
2. Run cargo fmt → cargo clippy → cargo test (fix any failures)
3. Verify Docker services if infrastructure was changed
4. Git add → commit → push to GitHub
5. Report back: "DONE. Here's the summary + proof."
```

**Rules for post-approval execution:**
- Do NOT stop mid-way to ask again (unless genuinely blocked by an error)
- Do NOT ask "should I continue?" after each sub-step — the approval covers the whole plan
- If a genuine blocker appears mid-execution, stop and escalate with BLOCKED status
- Small implementation details within the approved plan are Claude Code's call — don't ask about indentation, variable names within conventions, or obvious patterns

### Blocker Escalation Protocol

When Claude Code cannot proceed, it MUST escalate with this exact format:

```
BLOCKED: [one-line summary]

What I tried:
  1. [First attempt + result]
  2. [Second attempt + result]

Error output:
  [exact error message or relevant output]

Proposed solutions:
  A. [Option A] — [trade-off]
  B. [Option B] — [trade-off]
  C. [Option C if applicable] — [trade-off]

Which direction should I take?
```

**Rules:**
- NEVER silently skip a failing step — escalate it
- NEVER suppress errors — show the full error message
- NEVER guess a fix without telling Parthiban what happened
- Always propose at least 2 solutions — never just say "it's broken"

---

## GIT CONVENTIONS

### Branch Naming
```
main                              → Production-ready code. Always green.
develop                           → Integration branch. All features merge here first.
feature/<phase>/<short-name>      → Feature work. Example: feature/phase1/docker-compose
fix/<issue-description>           → Bug fixes. Example: fix/websocket-reconnect-panic
hotfix/<critical-description>     → Production emergency. Example: hotfix/oms-state-deadlock
```

### Commit Message Format
```
<type>(<scope>): <description>

Types:
  feat     → New feature or capability
  fix      → Bug fix
  refactor → Code restructure, no behavior change
  test     → Adding or updating tests
  docs     → Documentation changes
  chore    → Build, CI, dependency updates
  perf     → Performance improvement
  security → Security fix or hardening

Scope = module or crate name

Examples:
  feat(websocket): implement Dhan V2 binary frame parser
  fix(oms): handle partial fill state transition correctly
  test(binary-parser): add fuzz tests for malformed packets
  chore(deps): pin tokio to version from Tech Stack Bible V6
  perf(pipeline): replace DashMap with papaya on tick hot path
  security(secrets): add zeroize to Dhan API token on drop
```

**Rules:**
- Every commit must compile (`cargo check` passes)
- Every commit must pass all existing tests (`cargo test` passes)
- Every commit MUST be pushed to GitHub — local-only commits are forbidden
- No WIP commits on `main` or `develop` — squash before merging
- Commit message must reference the phase: `[Phase 1]` prefix if relevant
- One logical change per commit — not a dump of unrelated changes

### Tag Strategy
```
v<phase>.<milestone>.<patch>      → Example: v1.0.0, v1.1.0, v1.1.1
Phase completion tag              → Example: phase-1-complete
```

---

## PERMANENT ARCHITECTURE PRINCIPLES

These rules govern EVERY phase, EVERY file, EVERY line of code, EVERY config. They are non-negotiable and override any conflicting instruction.

### Principle 0: GITHUB IS THE SOURCE OF TRUTH
- ALL code, config, docs, and infrastructure definitions live on GitHub.
- GitHub repo: `https://github.com/SJParthi/dhan-live-trader`
- Local folders are disposable working copies of the GitHub repo.
- NEVER create files outside the GitHub clone. NEVER reference local absolute paths.
- IntelliJ, Docker, and Claude Code all operate on the GitHub clone.
- If it's not on GitHub, it doesn't exist.

### Principle 1: DOCKER IS THE RUNTIME
- The app runs on Docker. Mac and AWS are just Docker hosts.
- Same docker-compose.yml everywhere. No exceptions.
- Same Dockerfile everywhere. No exceptions.
- ALL infrastructure in containers. Nothing installed on host except Docker + Git + gh CLI.
- Service discovery via Docker DNS hostnames. Never localhost in app code.

### Principle 2: NO .env FILES. NO SECRETS ON DISK. EVER.
- ALL secrets from **AWS SSM Parameter Store** (SecureString type).
- **LocalStack** for dev, **real AWS SSM** for prod.
- Same SDK, same API calls, same code path in both environments.
- Non-secret config from TOML files checked into git. Zero secrets in TOML.
- The ONLY env-level difference: `AWS_ENDPOINT_URL` for LocalStack vs real AWS.
- Cost: **zero** — SSM Parameter Store standard tier is free.

### Principle 3: COMMON RUNTIME, DYNAMIC, SCALABLE
- Every component works identically on: Mac, AWS EC2, CI runner, any teammate's laptop.
- No platform branching (no if-Mac-then-X, if-Linux-then-Y).
- No host-specific paths. No OS detection.
- Config injected at container startup, not baked in.

**Local-AWS Parity Checklist — Claude Code MUST verify after every infrastructure change:**

```
BEFORE committing any Docker, config, or infrastructure change, verify ALL:

[ ] No hardcoded hostnames — all services use Docker DNS names (dlt-questdb, dlt-valkey, etc.)
[ ] No host-dependent volume mounts — no /Users/..., no /home/..., only named Docker volumes
[ ] No platform-specific code paths — no if-mac/if-linux branching
[ ] No localhost in application code — Docker DNS only
[ ] Docker Compose is identical for both environments — same file, zero changes
[ ] Config TOML uses Docker service names, not localhost or 127.0.0.1
[ ] Secrets use SSM Parameter Store API — same code path for LocalStack and real AWS
[ ] The ONLY env difference is AWS_ENDPOINT_URL — nothing else changes
[ ] No architecture-specific Docker images — use multi-arch images or verify amd64 + arm64
[ ] Health checks work in both environments — no curl/wget dependencies that differ
```

**If ANY item fails, the change does NOT ship. Fix parity BEFORE committing.**

**The Golden Test:** If you deleted the local clone right now, cloned fresh on an AWS EC2 instance, ran `docker compose up -d`, would EVERYTHING work identically? If no → fix it. If yes → ship it.

### Principle 4: ZERO HARDCODED VALUES. ANYWHERE. EVER.
No magic numbers. No inline strings. No embedded values.

**Every value must come from ONE of these sources:**
- Named constant (for values known at compile time)
- Config file (for values that change between environments)
- AWS SSM Parameter Store (for sensitive values)

**Rules:**
```
WRONG: thread::sleep(Duration::from_secs(10));
RIGHT: thread::sleep(Duration::from_secs(PING_INTERVAL_SECS));

WRONG: if depth.len() > 5 {
RIGHT: if depth.len() > MARKET_DEPTH_LEVELS {

WRONG: let url = "wss://api-feed.dhan.co/v2";
RIGHT: let url = config.dhan.websocket_url;  // from config TOML

WRONG: .max_connections(16)
RIGHT: .max_connections(config.valkey.max_connections)

WRONG: "dlt/dhan-credentials"
RIGHT: secrets::DHAN_SECRET_NAME  // named constant

WRONG: vec![0u8; 162]
RIGHT: vec![0u8; FULL_QUOTE_PACKET_SIZE]

WRONG: if hour >= 9 && minute >= 15 {
RIGHT: if current_time >= config.trading.market_open_time {

WRONG: .timeout(Duration::from_millis(500))
RIGHT: .timeout(Duration::from_millis(config.network.request_timeout_ms))
```

**Where constants live:**
- `constants.rs` in the common crate — compile-time known values
  (packet sizes, exchange codes, protocol versions, array bounds)
- `config/*.toml` — runtime values that might differ per environment
  (URLs, ports, timeouts, limits, thresholds)
- AWS SSM Parameter Store — credentials, tokens, API keys

**If you see a raw number or string literal in application code, it's a bug. Fix it immediately.**

### Principle 5: PRECISE, DESCRIPTIVE NAMING
No abbreviations. No ambiguity. Names must be self-documenting.

**Rust naming conventions (enforced):**
```
Types/Structs:   PascalCase    → WebSocketConnection, TickRingBuffer, MarketDepthLevel
Functions:       snake_case    → parse_binary_frame, connect_websocket, fetch_secret
Constants:       SCREAMING_SNAKE → MAX_INSTRUMENTS_PER_CONNECTION, PING_INTERVAL_SECS
Modules:         snake_case    → websocket_client, binary_parser, secret_manager
Crate names:     kebab-case    → dhan-live-trader-common, dhan-live-trader-pipeline
Enum variants:   PascalCase    → Exchange::NationalStockExchange, FeedType::FullMarketDepth
```

**Naming rules:**
```
WRONG: fn parse(b: &[u8]) -> Res<T>
RIGHT: fn parse_ticker_packet(raw_bytes: &[u8]) -> Result<TickData>

WRONG: let ws = connect();
RIGHT: let websocket_connection = establish_dhan_websocket();

WRONG: struct Cfg { db_h: String }
RIGHT: struct QuestDBConfig { host: String, http_port: u16, pg_port: u16 }

WRONG: const MAX: usize = 5000;
RIGHT: const MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION: usize = 5000;

WRONG: let t = Instant::now();
RIGHT: let tick_received_at = Instant::now();

WRONG: fn handle(msg: Msg) {
RIGHT: fn handle_websocket_message(message: WebSocketMessage) {
```

**File and directory naming:**
```
WRONG: src/ws.rs, src/db.rs, src/cfg.rs
RIGHT: src/websocket_client.rs, src/questdb_writer.rs, src/config_loader.rs

WRONG: deploy/docker/dc.yml
RIGHT: deploy/docker/docker-compose.yml

WRONG: scripts/init.sh
RIGHT: scripts/seed-localstack-secrets.sh
```

**Docker container naming:**
```
WRONG: container_name: db, cache, proxy
RIGHT: container_name: dlt-questdb, dlt-valkey, dlt-traefik
(dlt = dhan-live-trader prefix for containers, logs, docker exec commands.)
```

### Principle 6: O(1) IS NON-NEGOTIABLE
- Hot path = zero heap allocation (zerocopy, arrayvec, stack types)
- All HashMap/Vec MUST have bounded capacity
- papaya for hot-path lookups, DashMap only for cold path
- rtrb for SPSC (wait-free), crossbeam for MPMC
- No .clone() on hot path unless explicitly approved by Parthiban
- Performance targets: <10ns tick parse, <10μs processing, 5-50ms API round-trip

---

## SECRET ROTATION & TOKEN LIFECYCLE

Dhan uses a 24-hour JWT token cycle. This is a critical operational concern — if token rotation fails, the system goes blind (no market data) and deaf (no order placement).

### Dhan Token Lifecycle
```
1. Session init     → reqwest POST to Dhan REST API with credentials
2. JWT issued       → Valid for 24 hours from issue time
3. Token stored     → arc-swap atomic pointer swap (O(1), no lock)
4. Token used       → Every WebSocket frame + REST call reads via arc-swap
5. Token refresh    → Must happen BEFORE expiry, not after
6. Token wiped      → Old token zeroized via zeroize crate on drop
```

### Rotation Rules
```
REFRESH WINDOW:    Refresh token at 23 hours (1 hour before expiry)
RETRY ON FAILURE:  Exponential backoff 100ms → 30s (backon crate)
MAX RETRIES:       5 attempts before circuit breaker trips (failsafe)
ALERT ON FAILURE:  Telegram alert after 2nd consecutive failure
HALT ON FAILURE:   If all 5 retries fail → halt trading, alert via SNS
NEVER:             Never cache token to disk. Never log token value.
```

### SSM Parameter Store Secret Names
```
Secret naming convention:  /dlt/<environment>/<service>/<key>
Examples:
  /dlt/prod/dhan/client-id
  /dlt/prod/dhan/client-secret
  /dlt/prod/dhan/totp-secret
  /dlt/dev/dhan/client-id        (LocalStack)
```

### Security Requirements
- All secret values wrapped in `Secret<String>` (secrecy crate) — Debug prints `[REDACTED]`
- All secret memory wiped on drop via `zeroize` — write_volatile + memory fences
- NEVER log, print, serialize, or transmit raw secret values
- NEVER store secrets in Valkey, QuestDB, or any persistent store
- Secret access failures must fail LOUD (alert + halt), never fail silent

---

## LOGGING STANDARDS

Every log line is a diagnostic tool. Bad logging is worse than no logging — it creates noise that hides real problems.

### Structured JSON Format (enforced by tracing-subscriber)
```json
{
  "timestamp": "2026-03-15T10:30:45.123456789+05:30",
  "level": "INFO",
  "target": "dhan_live_trader::websocket_client",
  "span": "process_tick",
  "message": "Tick processed successfully",
  "fields": {
    "security_id": 12345,
    "exchange": "NSE",
    "latency_ns": 8500,
    "sequence_number": 98765
  }
}
```

### What to Log (with level)
```
ERROR:  System failures requiring immediate attention
        → WebSocket disconnect, QuestDB write failure, OMS invalid state
        → Secret rotation failure, circuit breaker trip
        → MUST trigger Telegram alert

WARN:   Degraded conditions that may need attention
        → Tick latency above threshold, retry attempts, rate limit approaching
        → Duplicate tick received, out-of-order timestamp detected
        → Valkey connection pool exhausted (serving from fallback)

INFO:   Normal operational events (sparse — not every tick)
        → Application startup/shutdown, config loaded, WebSocket connected
        → Token refreshed, deployment completed, phase milestone reached
        → Market open/close detected, holiday detected

DEBUG:  Detailed diagnostic info (disabled in production by default)
        → Individual tick processing steps, cache hit/miss
        → Channel buffer utilization, thread scheduling events

TRACE:  Extremely verbose (never in production)
        → Raw packet bytes, full struct dumps, memory allocation details
```

### What NEVER to Log — VIOLATIONS ARE SECURITY INCIDENTS
```
NEVER LOG:
  - API keys, tokens, passwords, TOTP secrets (use secrecy crate)
  - Raw JWT token values (log token expiry time only)
  - SSM Parameter Store values
  - Full order payloads with credentials
  - Customer/user personal information
  - Raw binary WebSocket frames (log parsed summary only)

If secrecy crate is used correctly, Secret<T> will print [REDACTED] automatically.
Any raw secret in logs = SECURITY INCIDENT → fix immediately + rotate credential.
```

### Log Context Requirements
Every log message in production code MUST include enough context to debug WITHOUT reproducing the issue:
- **What** happened (the event)
- **Where** it happened (module + function via `#[instrument]`)
- **When** it happened (timestamp with IST, automatic via tracing)
- **Which** entity it relates to (security_id, order_id, connection_id)
- **Why** it matters (for errors: the error chain via anyhow context)

### Log Retention
```
Hot logs (Loki):     30 days — queryable via Grafana
Cold logs (S3):      5 years — SEBI audit compliance
Log rotation:        Managed by Alloy + Loki retention policies
```

---

## CARGO.TOML & WORKSPACE CONVENTIONS

### Workspace Structure
```
dhan-live-trader/                     ← Project root (top-level has the full name)
├── Cargo.toml                        ← Workspace root — ALL versions pinned here
├── crates/
│   ├── common/                       ← Shared types, constants, config
│   ├── core/                         ← WebSocket, parsing, pipeline
│   ├── trading/                      ← OMS, risk, order execution
│   ├── storage/                      ← QuestDB, Valkey, persistence
│   ├── api/                          ← axum HTTP server, REST endpoints
│   └── app/                          ← Binary entry point, orchestration
├── config/
│   ├── base.toml                     ← Shared config (non-secret)
│   └── local-overrides.toml          ← Dev-only overrides (git-ignored)
├── docs/
│   ├── tech_stack_bible_v6.pdf
│   ├── phases/
│   ├── adr/
│   └── incidents/
├── deploy/
│   └── docker/
│       └── docker-compose.yml
├── scripts/
└── CLAUDE.md                         ← This file
```

**Note:** Directory names under `crates/` are SHORT — no `dhan-live-trader-` prefix on folders. The full prefix lives in each crate's `[package] name` inside Cargo.toml (e.g., `name = "dhan-live-trader-core"`). This keeps the filesystem clean while Rust's module system gets the fully qualified name.

### Version Pinning Rules (Cargo.toml)

**ALL version numbers come from the Tech Stack Bible. Never type a version from memory.**

```toml
# CORRECT — exact version from Bible, no range operators
some-crate = "x.y.z"

# WRONG — range operators allow uncontrolled updates
some-crate = "^x.y"       # BANNED — caret allows minor bumps
some-crate = "~x.y"       # BANNED — tilde allows patch bumps
some-crate = "*"           # BANNED — wildcard = chaos
some-crate = ">=x.y"      # BANNED — open-ended range
```

### Workspace Cargo.toml Pattern
```toml
[workspace]
members = ["crates/*"]
resolver = "2"

[workspace.dependencies]
# ALL shared dependencies pinned here with exact versions from the Bible
# Crates inherit via { workspace = true } — no version in crate-level Cargo.toml
# Example pattern (use ACTUAL versions from Bible, not these):
# some-crate = { version = "x.y.z", features = ["..."] }
```

### Crate-Level Cargo.toml Pattern
```toml
[package]
name = "dhan-live-trader-core"       # Full prefix in package name
version = "0.1.0"
edition = "2024"
rust-version = "1.93.1"

[dependencies]
# Inherit from workspace — no version numbers here
tokio = { workspace = true }
serde = { workspace = true }
```

**Rules:**
- ALL dependency versions defined in workspace root — crates use `{ workspace = true }`
- ONE place to update versions, ONE place to audit
- `edition = "2024"` and `rust-version = "1.93.1"` in every crate
- No `[patch]` section unless explicitly approved by Parthiban
- `cargo update` is BANNED — versions change only via Bible updates

---

## BOOT → SHUTDOWN SEQUENCE

Claude Code must understand the full runtime flow. Every component has a role in this chain:

```
Boot
→ Config            (toml)
→ Auth              (reqwest + JWT + arc-swap)
→ WebSocket         (tokio-tungstenite)
→ Parse             (zerocopy)
→ Route             (rtrb SPSC)
→ Indicators        (yata + blackscholes)
→ IST               (chrono + chrono-tz)
→ State             (papaya + DashMap)
→ OMS               (statig + governor)
→ Persist           (QuestDB)
→ Cache             (Valkey)
→ HTTP              (axum + tower)
→ Metrics           (Prometheus)
→ Logs              (Loki/Alloy)
→ Traces            (Jaeger v2)
→ Dashboards        (Grafana/WireGuard)
→ Alerts            (Telegram → Grafana → SNS)
→ TLS               (Let's Encrypt)
→ Secrets           (SSM + secrecy + zeroize)
→ Recovery          (CloudWatch + memmap2)
→ Shutdown          (signal-hook + sd-notify)
```

When building any component, Claude Code must know where it sits in this chain and what it connects to upstream and downstream.

---

## VERSIONS & INFRASTRUCTURE — SINGLE SOURCE OF TRUTH

**All versions, Docker images, AWS config, and alerting chains live in the Tech Stack Bible V6 ONLY.**

Claude Code must:
1. Open and read `docs/tech_stack_bible_v6.pdf` at the start of every session
2. Use ONLY the versions specified in the Bible — no exceptions
3. If a component is not in the Bible, ASK Parthiban before choosing a version
4. NEVER duplicate Bible content into this file

**The Bible covers:** 109 components across 22 sections — Rust crates, Docker images (SHA256 pinned), AWS production stack, alerting chain, observability, testing tools, and more.

---

## BANNED — NEVER USE THESE

Claude Code must treat these as compilation errors. If any appears in code, config, or documentation, it is a bug that must be fixed immediately.

```
.env files               → Use AWS SSM Parameter Store + LocalStack
AWS Secrets Manager       → Use SSM Parameter Store (zero cost)
bincode (any version)     → TOMBSTONED upstream. Use bitcode (version from Bible)
Promtail                  → DEPRECATED. Use Grafana Alloy (version from Bible)
Jaeger v1                 → EOL Dec 2025. MUST use Jaeger v2 (version from Bible)
:latest tag               → Always pin exact version + SHA256 digest
brew install <infra>      → Docker containers only. No host installs
localhost in app code     → Use Docker DNS hostnames
Hardcoded values          → Named constants or config only
Abbreviated names         → Full descriptive names only
Manual steps              → Claude Code does everything
.clone() on hot path      → Zero allocation. Use references or Copy types
DashMap on hot path       → Use papaya for hot-path maps
unbounded channels        → All channels MUST have bounded capacity
dyn Trait on hot path     → Use enum_dispatch for jump table dispatch
intellij-rust plugin      → ARCHIVED. Use new Rust Plugin (ID 22407)
cargo update              → Versions change ONLY via Bible updates
^ or ~ in Cargo.toml     → Exact versions only (e.g., "x.y.z" not "^x.y")
println! in prod code     → Use tracing macros (info!, warn!, error!)
.unwrap() in prod code    → Propagate errors with ? and anyhow/thiserror
Local absolute paths      → Use repo-relative paths only. Never /Users/... or /home/...
Local-only commits        → Every commit MUST be pushed to GitHub immediately
```

---

## SNIPER-LEVEL QUALITY GATES — NON-NEGOTIABLE

This is a financial system that trades REAL MONEY. A single uncaught bug can lose lakhs in seconds. Build like lives depend on it. No shortcuts. No "good enough." No "we'll add tests later."

### Quick Reference — Before Every Action

```
Before writing code → Read Bible for versions. If not in Bible, ASK Parthiban.
Before committing   → cargo fmt + clippy (zero warnings) + test (100% pass)
Adding dependency   → Bible has it? Use it. Bible doesn't? Propose to Parthiban.
Encountering error  → Read FULL message. Fix root cause. Never suppress.
                      Never add #[allow(...)] without Parthiban's approval.
```

### Gate 1: CODE REVIEW — Every Line, Every Time

Claude Code performs a **self-review** before presenting ANY code to Parthiban:

**Checklist (must pass ALL):**
- [ ] Every function has a clear single responsibility
- [ ] Every error path is explicitly handled — no `.unwrap()` in production code
- [ ] Every `Result` is propagated with context via `anyhow`/`thiserror` — not silently dropped
- [ ] Every `unsafe` block has a `// SAFETY:` comment justifying why it's needed
- [ ] Zero `.clone()` on hot path — verified by inspection
- [ ] Zero heap allocation on hot path — verified by `dhat` in tests
- [ ] All numeric types are explicitly sized (`u32`, `i64`, not platform-dependent)
- [ ] All timeout/retry logic uses values from config, never hardcoded
- [ ] All log messages include enough context to debug without reproducing
- [ ] No `todo!()`, `unimplemented!()`, or placeholder code in production modules
- [ ] No commented-out code — delete it or don't write it
- [ ] Every public function has a doc comment explaining what it does, not how

**If any item fails, fix it before showing code to Parthiban.**

### Gate 2: TEST COVERAGE — Sniper Level

**Minimum coverage: 90%+ line coverage on all production code.**
**Target: 95%+ on critical paths (OMS, risk, order execution, WebSocket parsing).**

**Required test types per module:**

| Test Type | What It Covers | When Required |
|---|---|---|
| Unit tests | Individual functions, pure logic | EVERY function |
| Integration tests | Cross-module interactions | EVERY module boundary |
| Property-based tests | Random input fuzzing | Parsers, serializers, math |
| Boundary tests | Edge cases, min/max, empty, overflow | ALL numeric operations |
| Error path tests | Every `Err` variant is tested | EVERY Result-returning function |
| Concurrency tests (Loom) | Thread safety, data races | ALL shared state |
| Benchmark tests (Criterion) | Performance regression detection | ALL hot-path functions |
| Fuzz tests (cargo-fuzz) | Crash discovery from malformed input | Dhan binary parser, all external input |
| Heap allocation tests (dhat) | Verify zero allocation on hot path | ALL hot-path functions |

**Test naming convention:**
```
#[test]
fn test_<module>_<function>_<scenario>_<expected_outcome>()

Examples:
fn test_binary_parser_parse_ticker_packet_valid_full_quote_returns_tick_data()
fn test_binary_parser_parse_ticker_packet_truncated_buffer_returns_error()
fn test_oms_state_machine_new_order_submitted_transitions_to_submitted()
fn test_oms_state_machine_filled_order_cancel_attempt_returns_invalid_transition()
fn test_websocket_reconnect_after_disconnect_resumes_within_backoff_limit()
```

**What Claude Code must NEVER do:**
- Write a function without writing its tests in the same session
- Write a test that only covers the happy path
- Use `#[ignore]` without Parthiban's explicit approval and a TODO to un-ignore
- Skip testing error paths because "they probably won't happen"
- Write tests that depend on external services without mocking/stubbing

### Gate 3: END-TO-END — ZERO BREAKAGE TOLERANCE

**The system must survive every failure scenario without losing money or data.**

**Failure scenarios Claude Code MUST test:**

```
NETWORK FAILURES:
- WebSocket disconnects mid-tick           → Must reconnect + resume
- Dhan API returns 429 (rate limited)      → Must back off per governor
- Dhan API returns 500/502/503             → Must retry per backon
- DNS resolution fails                     → Must use circuit breaker (failsafe)
- TLS handshake timeout                    → Must retry, not crash
- Internet goes down completely            → Must alert via SMS (SNS), pause trading

DATA FAILURES:
- Malformed binary packet from Dhan        → Must reject + log, not crash
- Zero-length WebSocket frame              → Must handle gracefully
- Duplicate tick data                      → Must deduplicate, not double-count
- Out-of-order timestamps                  → Must detect + handle
- QuestDB write fails                      → Must buffer + retry, not lose data
- Valkey connection drops                  → Must reconnect via deadpool, serve stale if needed

STATE FAILURES:
- Application crash mid-processing         → Must recover state from memmap2
- OMS in invalid state                     → Must reject order + alert, not proceed
- Config file corrupted/missing            → Must fail fast with clear error, not use defaults
- Secrets not available in SSM             → Must fail fast, not start with empty credentials
- Market holiday not in calendar           → Must refuse to trade, not submit orders

INFRASTRUCTURE FAILURES:
- Docker container OOM killed              → Must restart via Docker restart policy
- EBS volume full                          → Must alert before 90%, stop writes at 95%
- EC2 instance hardware failure            → Must auto-recover via CloudWatch
- Process hangs (no heartbeat)             → Must auto-restart via sd-notify watchdog

TRADING SAFEGUARDS:
- Position size exceeds risk limit         → Must reject order + alert
- P&L drawdown exceeds daily limit         → Must halt all trading + alert
- Order rejected by exchange               → Must update OMS state + log reason
- Partial fill received                    → Must track correctly, not treat as full fill
- Market closes while order pending        → Must cancel open orders + reconcile
```

**Claude Code must write tests for EVERY scenario above as they become relevant to the current phase.**

### Gate 4: CI PIPELINE GATES — AUTOMATED ENFORCEMENT

Every push to the repository must pass ALL of these in CI (GitHub Actions):

```
Stage 1 — Compile:
  cargo build --release                    → Zero errors
  cargo build --release --target x86_64    → Cross-compile check for AWS

Stage 2 — Lint:
  cargo fmt --check                        → Zero formatting issues
  cargo clippy -- -D warnings              → Zero warnings (warnings = errors)
  cargo clippy -- -W clippy::perf          → Performance lint group enforced

Stage 3 — Test:
  cargo test                               → 100% pass
  cargo test -- --ignored                  → Explicitly ignored tests tracked

Stage 4 — Security:
  cargo audit                              → Zero known CVEs
  git-secrets --scan                       → Zero leaked secrets

Stage 5 — Performance:
  cargo bench (Criterion)                  → >5% regression = FAIL the build
  dhat heap check                          → >0 allocations on hot path = FAIL

Stage 6 — Coverage:
  cargo-tarpaulin or llvm-cov              → <90% coverage = FAIL the build
                                           → <95% on critical modules = WARNING
```

**If ANY stage fails, the build is RED. No merging. No deploying. No exceptions.**

### Gate 5: PRE-DEPLOYMENT VERIFICATION

Before ANY deployment to AWS production:

1. **All CI gates pass** (Stage 1-6 above)
2. **Docker image builds successfully** with multi-stage (scratch base, <15MB)
3. **Docker Compose health checks pass** for ALL services
4. **Smoke test** — connect to Dhan WebSocket, receive 1 tick, parse it, store it
5. **Rollback plan confirmed** — Traefik blue-green ready, previous version tagged
6. **Parthiban gives explicit "deploy" approval**

### Gate 6: RUNTIME MONITORING — POST-DEPLOYMENT

After deployment, Claude Code must verify:

1. **All Grafana dashboards showing data** — no gaps, no stale panels
2. **Prometheus scrape targets all UP** — zero down targets
3. **Jaeger traces flowing** — spans visible for tick processing
4. **Loki logs streaming** — application logs arriving via Alloy
5. **Telegram test alert fires** — confirm alerting pipeline works
6. **Tick latency within budget** — <10ns parse, <10μs processing confirmed in metrics

---

## DATA INTEGRITY — ZERO DATA LOSS TOLERANCE

This system processes financial data. Every tick, every order, every state transition is a financial record. Losing or corrupting data can mean incorrect P&L, missed trades, or regulatory violations.

### Idempotency Rules
```
RULE: Every write operation must be idempotent — running it twice produces the same result.

QuestDB writes:
  - Use designated timestamp + security_id as natural dedup key
  - QuestDB's out-of-order tolerance handles late-arriving ticks
  - NEVER rely on insert order — always use explicit timestamps

Valkey writes:
  - Use SET with explicit keys — naturally idempotent
  - For counters, use atomic INCR, never GET + SET

OMS state transitions:
  - Use statig state machine — invalid transitions rejected at type level
  - Every transition logged with before/after state + timestamp
  - State changes are append-only in QuestDB — full audit trail

Order submission:
  - Generate idempotency key BEFORE sending to Dhan API
  - Store key in Valkey BEFORE submission
  - On retry, check Valkey for existing key — prevent duplicate orders
  - This is CRITICAL — duplicate orders = double the financial exposure
```

### Deduplication
```
Tick data:
  - Dhan may send duplicate ticks on reconnect
  - Deduplicate by: (security_id, exchange_timestamp, sequence_number)
  - Keep a bounded ring buffer of recent tick hashes (rtrb) for O(1) lookup
  - Log duplicates at WARN level — they indicate reconnection overlap

Orders:
  - Deduplicate by idempotency key (generated client-side)
  - NEVER submit an order without checking for existing idempotency key
  - On Dhan API timeout: CHECK order status before retrying
  - Assume the order WENT THROUGH until confirmed otherwise
```

### Reconciliation
```
End-of-day reconciliation (automated, runs at 15:35 IST):
  1. Fetch all orders from Dhan API for the day
  2. Compare with OMS internal state
  3. Flag any mismatches:
     - Orders in Dhan but not in OMS → CRITICAL alert
     - Orders in OMS but not in Dhan → investigate timeout/rejection
     - Fill quantity mismatches → CRITICAL alert
     - Price mismatches → log for review
  4. Generate reconciliation report → store in QuestDB
  5. Alert if ANY mismatches found

Position reconciliation:
  - Current positions MUST match Dhan's reported positions
  - Run after every fill, not just end-of-day
  - Mismatch = halt trading + alert immediately
```

### Data Retention
```
Hot data (QuestDB):  90 days of tick data, all orders forever
Cold data (S3):      All tick data archived, 5-year minimum retention
Audit logs (Loki):   30 days hot, 5 years cold (SEBI requirement)
Backups (EBS):       Daily automated snapshots, 30-day retention
```

---

## INCIDENT RESPONSE — WHEN PRODUCTION BREAKS

When something goes wrong in production, speed and clarity matter. Claude Code must follow this exact protocol.

### Severity Levels
```
SEV-1 (CRITICAL): Money at risk. Trading halted. Data loss.
  → Response time: IMMEDIATE
  → Examples: OMS stuck, duplicate orders, position mismatch, data corruption
  → Action: Halt trading → diagnose → fix → verify → resume

SEV-2 (HIGH): System degraded but trading safe.
  → Response time: Within 15 minutes
  → Examples: WebSocket reconnecting, elevated latency, Valkey down (stale cache)
  → Action: Diagnose → fix → verify monitoring

SEV-3 (MEDIUM): Non-critical component down.
  → Response time: Within 1 hour
  → Examples: Grafana dashboard stale, Jaeger not receiving traces, log gap
  → Action: Diagnose → fix → verify in next session

SEV-4 (LOW): Cosmetic or non-urgent.
  → Response time: Next session
  → Examples: Dashboard formatting, log verbosity too high, unused metric
```

### Rollback Protocol
```
Step 1: Confirm the issue (check Grafana, Loki, Telegram alerts)
Step 2: Decide: rollback or hotfix?
  - If uncertain → ROLLBACK (safe default)
  - If root cause is clear and fix is <10 lines → hotfix

ROLLBACK:
  1. Traefik blue-green switch to previous version (tagged in Docker registry)
  2. Verify previous version is healthy (health checks, smoke test)
  3. Notify Parthiban with: what broke, what was rolled back, next steps

HOTFIX:
  1. Branch: hotfix/<description> from main
  2. Fix + test (ALL quality gates still apply — no shortcuts)
  3. Deploy via Traefik blue-green
  4. Verify fix in production
  5. Merge hotfix to main AND develop
```

### Post-Mortem Format
After every SEV-1 or SEV-2 incident, Claude Code creates a post-mortem document:

```markdown
# Incident Post-Mortem: <Title>
**Date:** YYYY-MM-DD
**Severity:** SEV-X
**Duration:** HH:MM (from detection to resolution)
**Impact:** What was affected (trading, data, monitoring)

## Timeline
- HH:MM IST — First alert received
- HH:MM IST — Investigation started
- HH:MM IST — Root cause identified
- HH:MM IST — Fix deployed
- HH:MM IST — Verified resolved

## Root Cause
What actually broke and why.

## Fix Applied
What was changed to resolve it.

## What Went Well
Detection, response, tooling that helped.

## What Went Wrong
Gaps in testing, monitoring, or process.

## Action Items
- [ ] Concrete fix to prevent recurrence (with owner and deadline)
- [ ] Test added for this failure scenario
- [ ] Monitoring gap closed
```

Store in `docs/incidents/YYYY-MM-DD-<title>.md` and commit to git.

---

## DOCUMENTATION STANDARDS

Code without documentation is a liability. In a financial system, undocumented behavior is a risk.

### Doc Comments — Every Public Item
```rust
/// Parses a raw binary frame from the Dhan WebSocket V2 protocol into a structured TickData.
///
/// # Arguments
/// * `raw_bytes` - The raw binary frame received from the WebSocket connection.
///   Must be at least `FULL_QUOTE_PACKET_SIZE` bytes for full quote packets.
///
/// # Returns
/// * `Ok(TickData)` - Successfully parsed tick data with all fields populated.
/// * `Err(ParseError::InsufficientBytes)` - Frame shorter than minimum required size.
/// * `Err(ParseError::InvalidExchangeCode)` - Unrecognized exchange byte value.
///
/// # Performance
/// O(1) — uses zerocopy for zero-allocation struct cast from buffer.
/// Benchmark: ~8ns on x86-64 (Criterion, c7i.2xlarge).
pub fn parse_ticker_packet(raw_bytes: &[u8]) -> Result<TickData, ParseError> {
```

**Rules:**
- Every `pub fn`, `pub struct`, `pub enum`, `pub trait` gets a doc comment
- Doc comments describe WHAT and WHY, not HOW (the code shows how)
- Include `# Arguments`, `# Returns`, `# Errors` sections for public functions
- Include `# Performance` for hot-path functions (O-notation + benchmark result)
- Include `# Panics` section if the function can panic (should be rare)
- `cargo doc --no-deps` must build with zero warnings

### README Per Crate
Every crate in the workspace gets a `README.md`:
```markdown
# dhan-live-trader-core

## Purpose
WebSocket connection management, binary frame parsing, and tick data pipeline.

## Key Modules
- `websocket_client` — Dhan WebSocket V2 connection lifecycle
- `binary_parser` — Zero-copy tick data parsing via zerocopy
- `tick_pipeline` — SPSC ring buffer routing to downstream consumers

## Dependencies on Other Crates
- `dhan-live-trader-common` — shared types, constants, config

## Boot Sequence Position
Auth → **WebSocket → Parse → Route** → Indicators
```

### Architecture Decision Records (ADRs)
When Claude Code makes a non-obvious technical decision, document it:
```
Location: docs/adr/YYYY-MM-DD-<title>.md
Format:
  # ADR: <Title>
  **Status:** Accepted / Superseded / Deprecated
  **Context:** What situation required a decision
  **Decision:** What we chose and why
  **Consequences:** Trade-offs, what we gain, what we lose
  **Alternatives considered:** What else we evaluated and why we rejected it
```

---

## FUNCTIONAL CRATES — ADDED DURING DEVELOPMENT

Crates added by Claude Code during development that are NOT in the Tech Stack Bible. Each entry must include justification and Parthiban's approval status.

```
# FORMAT: crate_name = "version"  — purpose  — approved by Parthiban (date)
# Example:
# uuid = "1.11.0"  — unique order IDs for OMS  — approved 2026-03-15

(none yet — development not started)
```

**Rules for this section:**
- Only Claude Code adds entries here, never manually
- Every entry requires Parthiban's explicit "go ahead"
- If a Bible crate can serve the purpose, use the Bible crate instead
- Review this section monthly for promotion to the Bible or removal

---

## DEPLOYMENT TIMELINE

```
March 2026     → MacBook (Docker host): Build live system (monitoring + debug, NO orders)
                  Docker stack runs on MacBook. Dhan WebSocket connected.
                  Data collection 9:00-16:00 IST into Docker QuestDB.
                  Code always on GitHub — MacBook is just a Docker host.

April-June 2026 → MacBook (Docker host): Backtesting engine development
                   Historical data from Dhan API + collected tick data.
                   Strategy optimization, walk-forward validation.
                   Paper trading (simulated orders, no real money).

July 2026+      → AWS c7i.2xlarge Mumbai: Real money trading begins
                   Migrate Docker stack to EC2.
                   SEBI compliance activated (static IP, server in India).
                   All quality gates, monitoring, alerting MUST be production-ready.
```

**Key rule:** The code is written ONCE to work in BOTH environments. Docker + config TOML + SSM/LocalStack ensures zero code changes between MacBook and AWS. The ONLY difference is `AWS_ENDPOINT_URL` pointing to LocalStack vs real AWS. Code always lives on GitHub — MacBook and AWS are just Docker hosts that clone from GitHub.

---

## PHASE SYSTEM

One phase at a time. Each phase document in `docs/phases/`.
Build ONLY what the current phase specifies. Do not build ahead.

## CURRENT PHASE

**Phase 1 — Environment Readiness**
Read: `docs/phases/PHASE_1_ENVIRONMENT.md`

---

## TECH STACK BIBLE REFERENCE

- **Document:** Tech Stack Bible V6
- **Components:** 109 total across 22 sections
- **Location:** `docs/tech_stack_bible_v6.pdf` (in GitHub repo)
- **Authority:** This is the SINGLE SOURCE OF TRUTH for all versions and components
- **Update process:** Only Parthiban updates the Bible. Claude Code proposes changes, Parthiban approves and updates.

---

> **Claude Code reminder:** You are the builder, not the architect. Follow the Bible. Follow this file. Ask when unsure. Never guess. Every version pinned. Every allocation justified. O(1) or don't ship it.
