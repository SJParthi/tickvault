# CLAUDE.md — dhan-live-trader

> **Authority chain:** This file governs Claude Code behavior. The **Tech Stack Bible V6** (109 components) is the single source of truth for versions, components, and architecture. If CLAUDE.md and the Bible conflict, the Bible wins. If neither covers a topic, ASK Parthiban before proceeding.

---

## THREE PRINCIPLES — MEMORIZE THESE

```
1. Zero allocation on hot path
2. O(1) or fail at compile time
3. Every version pinned
```

Every file, every function, every config decision must pass all three. No exceptions.

---

## PROJECT

- **Name:** dhan-live-trader
- **Purpose:** O(1) latency live F&O trading system for Indian markets (NSE)
- **Broker:** Dhan WebSocket V2 Binary Protocol
- **Language:** Rust 2024 Edition (stable 1.93.1)
- **GitHub Repository:** `https://github.com/SJParthi/dhan-live-trader` (SINGLE SOURCE OF TRUTH — if it's not on GitHub, it doesn't exist)
- **Runtime:** MacBook Pro M4 Pro (Mar–Jun 2026) → AWS c7i.2xlarge Mumbai ap-south-1 (Jul 2026+)
- **Data Source:** Dhan WebSocket V2 + Dhan REST API (₹499/mo subscription)
- **Tools:** Claude Code (builder) + Docker (runtime) + GitHub (repo)
- **IDE:** IntelliJ IDEA Ultimate 2025.3 (Parthiban's — Claude Code never touches it)
- **Owner:** Parthiban (architect). Claude Code (builder).

---

## MARKET HOURS

Full reference: `docs/reference/market_hours.md` — read when implementing time-dependent logic.

Key facts: Timezone ALWAYS Asia/Kolkata. Data collection 9:00-16:00 IST. Trading 9:15-15:29 IST. NEVER order at/after 15:30. 375 candles/day (9:15-15:30). Pre/post market stored separately. Holiday calendar checked before every trading day. ALL timestamps include IST alongside any UTC. Market hour validation uses config values, not hardcoded times.

---

## SESSION START PROTOCOL — EVERY TIME

```
Step 0: Ensure GitHub clone exists    → gh repo clone SJParthi/dhan-live-trader (if needed)
Step 1: Pull latest from GitHub       → git pull origin main (always start from GitHub state)
Step 2: Read THIS file                → CLAUDE.md (rules, behavior, quality gates)
Step 3: Read the current phase doc    → docs/phases/PHASE_<N>_<NAME>.md (what to build)
Step 4: Read recent git log           → git log --oneline -20 (where did we leave off)
Step 5: Skim codebase map (if returning) → docs/codebase_map.md (architecture overview)
Step 6: Read Cargo.toml               → Verify dependency versions match Bible
Step 7: Run cargo check               → Confirm project compiles clean
Step 8: Run cargo test                → Confirm all tests pass
```

**DO NOT read the Tech Stack Bible at startup.** Read it ONLY when adding or updating a dependency.

Only after all steps pass does Claude Code begin working. Existing breakage takes priority over new features.

**Session end:** `cargo fmt` → `cargo clippy` (zero warnings) → `cargo test` (100% pass) → git commit → git push to GitHub (MANDATORY) → show Parthiban summary.

---

## TOKEN EFFICIENCY — ZERO WASTE

### Forbidden Reads — NEVER read unless triggered:
```
docs/pdf/*                    → NEVER. PDFs are for Parthiban only.
docs/tech_stack_bible_v6.md   → ONLY when adding/updating a dependency
docs/templates/*              → ONLY when an incident or release actually happens
docs/reference/*              → ONLY when implementing the specific topic
```

### Rules:
1. **NEVER re-read files already read in the current session**
2. **NEVER read large files (>200 lines) in full when you only need a specific section** — use offset+limit or grep
3. **Keep responses short** — say what was done, show proof, stop. No essays.
4. **Don't repeat CLAUDE.md rules back** — Parthiban wrote them, he knows them
5. **Don't explain obvious decisions** — if the Bible says tokio 1.44.1, just use it
6. **Parallelize tool calls** — if 3 files need reading, read all 3 in one round
7. **Skip redundant verification** — if cargo check passed, don't manually inspect for syntax errors
8. **One commit message, not a paragraph** — follow the format, keep it tight
9. **No filler phrases** — skip "Let me...", "I'll now...", "Great, now I'll..." — just do it

---

## WORKFLOW: ARCHITECT → BUILDER

Parthiban: architect — reviews plans, says "go ahead" or "change X", checks dashboards. Nothing else.

Claude Code: builder — ALL code, git, Docker, GitHub, verification, installation. Always pushes to GitHub. Always waits for approval before executing.

**Approval Protocol:**
1. Read phase doc → present COMPLETE plan → Parthiban approves or requests changes
2. Execute approved plan → show results and proof
3. Parthiban confirms or requests fixes → repeat

NEVER execute without approval. NEVER skip showing results. NEVER guess a version — check the Bible.

---

## COMMUNICATION PROTOCOL

### Status Markers
```
PLAN READY / AWAITING APPROVAL / IN PROGRESS / DONE / BLOCKED / DECISION NEEDED
```

### Core Rules:
- **Before work:** Present plan → wait for "go ahead" (silence ≠ approval)
- **At decisions:** Present options with trade-offs → ask which one
- **After work:** Show what was done + proof → state what's next
- **When blocked:** Show error + 2-3 solutions → ask direction
- **Post-approval:** Execute entire plan without stopping. Only stop if genuinely blocked.
- NEVER make silent decisions. NEVER suppress errors. NEVER silently retry failures.

### Telegram Notifications — MANDATORY on Task Completion
Notify Parthiban via Telegram Bot API on: task completion, PR created/merged, session end, build failure, BLOCKED status.
- Bot token/chat ID from SSM: `/dlt/<env>/telegram/bot-token`, `/dlt/<env>/telegram/chat-id`
- If SSM unreachable, skip silently (don't block work). NEVER log the bot token.
- Keep messages short: status + what was done + what's next.

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

Types: feat, fix, refactor, test, docs, chore, perf, security
Scope = module or crate name

Examples:
  feat(websocket): implement Dhan V2 binary frame parser
  fix(oms): handle partial fill state transition correctly
  chore(deps): pin tokio to version from Tech Stack Bible V6
```

### Rules:
- Every commit must compile (`cargo check`) and pass tests (`cargo test`)
- No WIP commits on `main` or `develop` — squash before merging
- Commit message must reference the phase: `[Phase 1]` prefix if relevant
- One logical change per commit

### Tag Strategy
```
v<phase>.<milestone>.<patch>      → Example: v1.0.0, v1.1.0, v1.1.1
Phase completion tag              → Example: phase-1-complete
```

---

## PERMANENT ARCHITECTURE PRINCIPLES

### Principle 0: GITHUB IS THE SOURCE OF TRUTH
All code, config, docs, and infrastructure definitions live on GitHub. Local folders are disposable working copies. NEVER create files outside the clone. NEVER reference local absolute paths.

### Principle 1: DOCKER IS THE RUNTIME
Docker is the single runtime. Mac and AWS are just hosts. Same docker-compose.yml and Dockerfile everywhere. All infrastructure in containers (host only has Docker + Git + gh CLI). Service discovery via Docker DNS — never localhost in app code.

**Parity test:** Fresh clone on any host + `docker compose up -d` = identical behavior. Only `AWS_ENDPOINT_URL` differs.

### Principle 2: NO .env FILES. NO SECRETS ON DISK.
All secrets from AWS SSM Parameter Store (SecureString). LocalStack for dev, real AWS for prod. Same SDK, same code path. Non-secret config from TOML files in git. The ONLY env-level difference: `AWS_ENDPOINT_URL`.

### Principle 3: COMMON RUNTIME
Every component works identically on Mac, AWS, CI, any laptop. No platform branching. No host-specific paths. Config injected at container startup, not baked in.

### Principle 4: ZERO HARDCODED VALUES
Every value from: named constant (compile time), config file (environment), or SSM (secrets).
```
WRONG: Duration::from_secs(10)         RIGHT: Duration::from_secs(PING_INTERVAL_SECS)
WRONG: "wss://api-feed.dhan.co/v2"     RIGHT: config.dhan.websocket_url
WRONG: if hour >= 9 && minute >= 15     RIGHT: current_time >= config.trading.market_open_time
```
Raw number or string literal in app code = bug. Fix immediately.

### Principle 5: PRECISE, DESCRIPTIVE NAMING
```
Types/Structs:   PascalCase       Functions:       snake_case
Constants:       SCREAMING_SNAKE   Modules:         snake_case
Crate names:     kebab-case        Enum variants:   PascalCase
```
No abbreviations. Names must be self-documenting. `fn parse_ticker_packet(raw_bytes: &[u8])` not `fn parse(b: &[u8])`.

### Principle 6: O(1) IS NON-NEGOTIABLE
- Hot path = zero heap allocation (zerocopy, arrayvec, stack types)
- All HashMap/Vec MUST have bounded capacity
- papaya for hot-path lookups, DashMap only for cold path
- rtrb for SPSC (wait-free), crossbeam for MPMC
- No .clone() on hot path unless explicitly approved by Parthiban
- Performance targets: <10ns tick parse, <10μs processing, 5-50ms API round-trip

---

## SECRET ROTATION

Full reference: `docs/reference/secret_rotation.md` — read when building auth/token modules.

Key rules: Dhan JWT 24h cycle, refresh at 23h, arc-swap atomic swap, zeroize on drop. `Secret<String>` wraps all secrets. SSM naming: `/dlt/<env>/<service>/<key>`. Failures = halt + alert, never fail silent.

---

## LOGGING STANDARDS

Full reference: `docs/reference/logging_standards.md`

Key rules: tracing macros only. NEVER log secrets (secrecy crate enforces `[REDACTED]`). Every log includes What/Where/When/Which/Why. ERROR triggers Telegram alert.

---

## CARGO & VERSIONS

- Crate folders under `crates/` use short names; full `dhan-live-trader-` prefix in `[package] name`
- ALL dependency versions pinned in workspace root Cargo.toml — crates use `{ workspace = true }`
- Exact versions ONLY from the Bible — `^`, `~`, `*`, `>=` are BANNED
- `edition = "2024"`, `rust-version = "1.93.1"` in every crate
- `cargo update` is BANNED. Versions change ONLY when Parthiban provides an updated Bible.
- CVE found → escalate to Parthiban immediately

---

## BOOT SEQUENCE

`Config → Auth → WebSocket → Parse → Route → Indicators → State → OMS → Persist → Cache → HTTP → Metrics → Logs → Traces → Dashboards → Alerts → TLS → Secrets → Recovery → Shutdown`

See `docs/codebase_map.md` for detailed component positions.

---

## BANNED — NEVER USE THESE

```
.env files / AWS Secrets Manager   → SSM Parameter Store + LocalStack
bincode / Promtail / Jaeger v1     → bitcode / Grafana Alloy / Jaeger v2 (Bible versions)
:latest tag / ^ ~ in Cargo.toml   → Pin exact versions + SHA256 digest
brew install <infra>               → Docker containers only
localhost in app code              → Docker DNS hostnames
Hardcoded values                   → Named constants or config only
Abbreviated names                  → Full descriptive names only
Manual steps                       → Claude Code does everything
.clone() / DashMap / dyn Trait on hot path → papaya, enum_dispatch, references/Copy
unbounded channels                 → All channels MUST have bounded capacity
println! / .unwrap() in prod       → tracing macros, ? with anyhow/thiserror
Local absolute paths               → Repo-relative paths only
cargo update                       → Bible updates only
Claude Code automating IntelliJ    → Parthiban uses IntelliJ himself
```

---

## QUALITY GATES

Full reference: `docs/reference/quality_gates.md` — read when setting up CI, writing tests, or benchmarking.

**Before commit:** `cargo fmt` + `clippy` (zero warnings) + `test` (100% pass).
**Adding dependency:** Bible has it? Use exact version. Not in Bible? Propose to Parthiban.
**Error handling:** Fix root cause, never suppress, never `#[allow(...)]` without approval.

**Coverage thresholds:** core/trading 95%, common/storage/api 90%, app 80%.
**Test naming:** `fn test_<module>_<function>_<scenario>_<expected_outcome>()`
**CI pipeline:** Compile → Lint → Test → Security → Performance → Coverage. Any failure = RED.
**Benchmarks:** >5% regression = build FAILS. <10ns tick parse, <10μs processing, <100ns OMS transition.

---

## DATA INTEGRITY

Full reference: `docs/reference/data_integrity.md`

Key rules: Every write idempotent. Orders use idempotency keys in Valkey BEFORE submission. Ticks deduplicate by (security_id, exchange_timestamp, sequence_number). Position reconciliation after every fill — mismatch = halt trading. Data retained 5 years (SEBI).

---

## INCIDENT RESPONSE

Full protocol: `docs/templates/incident_response.md` — read ONLY when an actual incident occurs.

Key rule: SEV-1/SEV-2 = halt trading first, diagnose second.

---

## DOCUMENTATION STANDARDS

- Every `pub fn/struct/enum/trait` gets a doc comment (WHAT and WHY). Include `# Arguments`, `# Returns`, `# Errors`, `# Performance` for hot-path.
- `cargo doc --no-deps` must build with zero warnings.
- Every crate gets README.md: Purpose, Key Modules, Dependencies, Boot Sequence Position.
- Non-obvious decisions → `docs/adr/YYYY-MM-DD-<title>.md`.

---

## FUNCTIONAL CRATES — ADDED DURING DEVELOPMENT

```
# FORMAT: crate_name = "version"  — purpose  — approved by Parthiban (date)
(none yet — development not started)
```

Rules: Parthiban approval required. Prefer Bible crate if it serves the purpose.

---

## CURRENT CONTEXT

**Phase:** Phase 1 — Environment Readiness → `docs/phases/PHASE_1_ENVIRONMENT.md`
**Bible:** `docs/tech_stack_bible_v6.md` — read ONLY when adding/updating dependencies. PDF is for Parthiban only.
**Timeline:** Mar 2026 MacBook (data collection) → Apr-Jun backtesting/paper → Jul 2026+ AWS Mumbai (real money). Only `AWS_ENDPOINT_URL` changes.

---

> **Claude Code reminder:** Builder, not architect. Follow the Bible. Follow this file. Ask when unsure. Never guess.
