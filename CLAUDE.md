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
- **Tools:** Claude Code (builder) + Docker (runtime) + GitHub (repo)
- **IDE (Parthiban's):** IntelliJ IDEA Ultimate 2025.3 — for code review, debugging, manual edits when Parthiban wants. Claude Code never depends on or automates IntelliJ.
- **Owner:** Parthiban (architect). Claude Code (builder).

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

### Instrument Build Window (daily, BEFORE market opens)
```
8:30 AM IST → Server starts, all Docker containers health-checked
8:30 AM IST → CSV download + universe build (3-10 seconds expected)
8:31 AM IST → System READY with instruments loaded
8:45 AM IST → DEADLINE — if not ready, SEV-1 alert fires
9:00 AM IST → Pre-market data collection begins (instruments MUST be loaded)

FAILURE PROTOCOL:
  CSV download fails (primary + fallback + cache ALL exhausted):
    → INSTANT Telegram alert — no waiting, no silent failure
    → System cannot start instrument-dependent operations
    → Total worst-case retry window: ~30 seconds before alert

  Build/validation fails after CSV obtained:
    → INSTANT Telegram alert with error details
    → Previous day's in-memory universe NOT usable (stale contracts)
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
Step 3: Read the current phase doc    → docs/phases/PHASE_<N>_<NAME>.md (what to build)
Step 4: Read recent git log           → git log --oneline -20 (where did we leave off)
Step 5: Read Cargo.toml               → Verify dependency versions match Bible
Step 6: Run cargo check               → Confirm project compiles clean
Step 7: Run cargo test                → Confirm all tests pass
```

**DO NOT read the Tech Stack Bible (PDF or MD) at startup.** Read it ONLY when adding or updating a dependency.

**Only after all 8 steps does Claude Code begin working.**

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

## TOKEN EFFICIENCY — ZERO WASTE

Tokens cost money and time. Every wasted token is wasted budget. Claude Code must be surgically efficient.

### Forbidden Reads — NEVER read unless triggered:
```
docs/pdf/*                    → NEVER. PDFs are for Parthiban only.
docs/tech_stack_bible_v6.md   → ONLY when adding/updating a dependency
docs/templates/*              → ONLY when an incident or release actually happens
docs/reference/*              → ONLY when implementing the specific topic (logging, tests, data integrity, failures)
```

### Rules:

1. **NEVER re-read files already read in the current session** — remember what you read
2. **NEVER read the Tech Stack Bible unless adding/updating a dependency** — versions don't change between sessions unless Parthiban says so
3. **NEVER read large files (>200 lines) in full when you only need a specific section** — use offset+limit or grep first
4. **Keep responses short** — say what was done, show proof, stop. No essays.
5. **Don't repeat CLAUDE.md rules back** — Parthiban wrote them, he knows them
6. **Don't explain obvious decisions** — if the Bible says tokio 1.44.1, just use it, don't explain why
7. **Parallelize tool calls** — if 3 files need reading, read all 3 in one round, not 3 separate rounds
8. **Skip redundant verification** — if cargo check passed, don't also manually inspect for syntax errors
9. **One commit message, not a paragraph** — follow the format, keep it tight
10. **No filler phrases** — skip "Let me...", "I'll now...", "Great, now I'll..." — just do it

---

## WORKFLOW: ARCHITECT → BUILDER

Parthiban is the **architect**. Claude Code is the **builder**.

### The Three Pillars — That's It

```
Claude Code     → Builder — writes all code, runs all commands, manages git
GitHub          → Single source of truth for ALL code, config, docs
Docker          → Single runtime for ALL infrastructure and services
```

No IDE. No manual steps. No local-only state. Claude Code does everything.

### What Parthiban does:
- Discusses requirements and design decisions
- Reviews plans presented by Claude Code
- Says "go ahead" or "change X"
- Checks dashboards in browser (Grafana, QuestDB Web Console, Jaeger)
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
Every time Claude Code completes a task, milestone, or session, it MUST send a Telegram notification to Parthiban. This is non-negotiable — Parthiban may not be watching the terminal.

**When to notify:**
- Task/subtask completed (e.g., "all 6 quality gates passed")
- PR created or merged
- Session end summary
- Build/test failure that blocks progress
- Any BLOCKED or DECISION NEEDED status

**How:** Use the Telegram Bot API via `curl`:
```
curl -s -X POST "https://api.telegram.org/bot<TOKEN>/sendMessage" \
  -d chat_id="<CHAT_ID>" -d parse_mode="Markdown" \
  -d text="<message>"
```
- Bot token and chat ID are in AWS SSM Parameter Store:
  - `/dlt/<env>/telegram/bot-token`
  - `/dlt/<env>/telegram/chat-id`
- During dev (LocalStack not yet set up), Claude Code fetches these from SSM when available. If SSM is not reachable, skip silently (do NOT block work on Telegram failures).
- NEVER log or print the bot token. Use it only in the curl call.
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
- Docker and Claude Code all operate on the GitHub clone.
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

**Local-AWS Parity — Golden Test:** Clone fresh on AWS EC2, `docker compose up -d` → everything works identically? No hardcoded hostnames, no host volume mounts, no platform branching, no localhost, Docker DNS only, same compose file, only `AWS_ENDPOINT_URL` differs. If parity fails, the change does NOT ship.

### Principle 4: ZERO HARDCODED VALUES. ANYWHERE. EVER.
No magic numbers. No inline strings. No embedded values.

**Every value must come from ONE of these sources:**
- Named constant (for values known at compile time)
- Config file (for values that change between environments)
- AWS SSM Parameter Store (for sensitive values)

**Examples:**
```
WRONG: thread::sleep(Duration::from_secs(10));     RIGHT: Duration::from_secs(PING_INTERVAL_SECS)
WRONG: let url = "wss://api-feed.dhan.co/v2";      RIGHT: config.dhan.websocket_url
WRONG: if hour >= 9 && minute >= 15 {               RIGHT: current_time >= config.trading.market_open_time
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

**Examples:**
```
WRONG: fn parse(b: &[u8])          RIGHT: fn parse_ticker_packet(raw_bytes: &[u8])
WRONG: let ws = connect();         RIGHT: let websocket_connection = establish_dhan_websocket();
WRONG: src/ws.rs                   RIGHT: src/websocket_client.rs
WRONG: container_name: db          RIGHT: container_name: dlt-questdb (dlt = dhan-live-trader prefix)
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

Full reference: `docs/reference/logging_standards.md`
Key rules: Use tracing macros only. NEVER log secrets (secrecy crate enforces `[REDACTED]`). Every log includes What/Where/When/Which/Why context. ERROR triggers Telegram alert. Any raw secret in logs = security incident.

---

## CARGO.TOML & WORKSPACE CONVENTIONS

- Crate folders under `crates/` are SHORT names (common, core, trading, storage, api, app). Full `dhan-live-trader-` prefix is in each crate's `[package] name`.
- ALL dependency versions pinned in workspace root Cargo.toml — crates use `{ workspace = true }`
- Exact versions ONLY from the Bible — `^`, `~`, `*`, `>=` are BANNED
- `edition = "2024"`, `rust-version = "1.93.1"` in every crate
- No `[patch]` without Parthiban's approval. `cargo update` is BANNED.

---

## BOOT → SHUTDOWN SEQUENCE

`Config → Auth → WebSocket → Parse → Route → Indicators → State → OMS → Persist → Cache → HTTP → Metrics → Logs → Traces → Dashboards → Alerts → TLS → Secrets → Recovery → Shutdown`

When building any component, know where it sits in this chain and what connects upstream/downstream.

---

## VERSIONS & INFRASTRUCTURE

All versions live in Tech Stack Bible V6 ONLY. Read `docs/tech_stack_bible_v6.md` ONLY when adding/updating a dependency. If not in Bible, ASK Parthiban.
`cargo update` is BANNED. Versions change only when Parthiban provides an updated Bible → Claude Code diffs → Parthiban approves → update Cargo.toml → full CI + benchmarks → rollback if >5% regression. CVE found → escalate to Parthiban immediately.

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
Claude Code automating IntelliJ → Parthiban uses IntelliJ himself. Claude Code never touches it
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

### Quick Reference
Before writing code: check Bible for versions. Before committing: `cargo fmt` + `clippy` (zero warnings) + `test` (100% pass). Adding dependency: Bible has it? Use it. Not in Bible? Propose to Parthiban. Error: fix root cause, never suppress, never `#[allow(...)]` without approval.

### Gate 1: CODE REVIEW — Every Line, Every Time

Self-review before presenting code: single responsibility, no `.unwrap()`/`.expect()`/`todo!()` in prod, `Result` propagated with context, `// SAFETY:` on every `unsafe`, zero `.clone()` on hot path, zero heap allocation on hot path, explicit numeric types, config-driven timeouts, doc comments on every `pub` item, no commented-out code.

Compile-time lint gates are in every lib.rs — see `docs/reference/quality_gates.md` for the full list.

### Gate 2: TEST COVERAGE — Sniper Level

Coverage: core/trading 95%, common/storage/api 90%, app 80%. Tool: cargo-tarpaulin.
Test naming: `fn test_<module>_<function>_<scenario>_<expected_outcome>()`
NEVER: write code without tests, test only happy path, skip error paths, use `#[ignore]` without approval.
Full test types table: `docs/reference/quality_gates.md`

### Gate 3: END-TO-END — ZERO BREAKAGE TOLERANCE

The system must survive every failure scenario without losing money or data.
Full scenario checklist: `docs/reference/failure_scenarios.md` (read ONLY when writing tests for a specific failure).
Claude Code must write tests for each scenario as it becomes relevant to the current phase.

### Gate 4: CI PIPELINE — 6 stages, any failure = RED

Compile → Lint → Test → Security → Performance → Coverage. Details in `.github/workflows/ci.yml` and `docs/reference/quality_gates.md`.
Main branch: requires all checks + PR review. Develop: compile+lint+test, direct push allowed.

### Gate 5: PRE-DEPLOYMENT

All CI green + Docker image builds (<15MB scratch) + health checks pass + smoke test (1 tick end-to-end) + Parthiban approves.

### Gate 5a: BENCHMARK BASELINES

>5% regression on any benchmark = build FAILS. Key budgets: tick parse <10ns, pipeline routing <100ns, full processing <10μs, OMS transition <100ns.
Full benchmark table and directory structure: `docs/reference/quality_gates.md`

### Gate 5b: RELEASE CHECKLIST — BEFORE EVERY VERSION BUMP

Full checklist: `docs/templates/release_checklist.md` (read ONLY at release time).

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

Full reference: `docs/reference/data_integrity.md`
Key rules: Every write is idempotent. Orders use idempotency keys in Valkey BEFORE submission (duplicate orders = double exposure). Ticks deduplicate by (security_id, exchange_timestamp, sequence_number). Position reconciliation runs after every fill — mismatch = halt trading. Data retained 5 years (SEBI).
Every module must handle ALL scenarios. If it CAN happen, it MUST have a test.

---

## INCIDENT RESPONSE — WHEN PRODUCTION BREAKS

Full protocol, severity levels, rollback steps, and post-mortem template: `docs/templates/incident_response.md`
Read that file ONLY when an actual incident occurs. Key rule: SEV-1/SEV-2 = halt trading first, diagnose second.

---

## DOCUMENTATION STANDARDS

Code without documentation is a liability. In a financial system, undocumented behavior is a risk.

### Doc Comments — Every Public Item
- Every `pub fn/struct/enum/trait` gets a doc comment (WHAT and WHY, not HOW)
- Include `# Arguments`, `# Returns`, `# Errors` for public functions
- Include `# Performance` for hot-path functions (O-notation + benchmark)
- `cargo doc --no-deps` must build with zero warnings

### README Per Crate
Every crate gets a README.md: Purpose, Key Modules, Dependencies, Boot Sequence Position.

### Architecture Decision Records
Non-obvious decisions → `docs/adr/YYYY-MM-DD-<title>.md` (Status, Context, Decision, Consequences, Alternatives).

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

Mar 2026: MacBook (data collection, no orders) → Apr-Jun: backtesting + paper trading → Jul 2026+: AWS Mumbai (real money).
Code works identically in both — only `AWS_ENDPOINT_URL` changes (LocalStack vs real AWS).

---

## PHASE SYSTEM

One phase at a time. Each phase document in `docs/phases/`.
Build ONLY what the current phase specifies. Do not build ahead.

## CURRENT PHASE

**Phase 1 — Environment Readiness**
Read: `docs/phases/PHASE_1_ENVIRONMENT.md`

---

## TECH STACK BIBLE REFERENCE

- **PDF (Parthiban only):** `docs/pdf/tech_stack_bible_v6.pdf` — NEVER read by Claude Code
- **Markdown (Claude Code):** `docs/tech_stack_bible_v6.md` — read ONLY when adding/updating dependencies
- **Authority:** Single source of truth for all 109 components. Only Parthiban updates it.

---

> **Claude Code reminder:** You are the builder, not the architect. Follow the Bible. Follow this file. Ask when unsure. Never guess. Every version pinned. Every allocation justified. O(1) or don't ship it.
