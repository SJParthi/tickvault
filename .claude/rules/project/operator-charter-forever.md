# Operator Charter — FOREVER (auto-loaded every session)

> **Authority:** CLAUDE.md > this file > defaults.
> **Scope:** Every Claude Code session, every Claude Cowork task, every git branch, every PR, every plan item, every task file, every commit — FOREVER.
> **Status:** PERMANENT. Cannot be superseded. Operator's verbatim demand 2026-05-11.
> **Auto-load trigger:** Always loaded. (This file's path puts it in the auto-load set for `.claude/rules/project/`.)

---

## The verbatim operator charter (preserved exactly, do not paraphrase)

> "I need extreme complete comprehensive extensive automation 100 percentage especially where my every new or existing Claude code session or every new Claude Cowork task or existing should automatically do everything as an automated process always. Especially it should access logs queries dbs project product everything should be entirely accessible as an automated process either in local or AWS as a common runtime dynamic scalable automated extreme automation approach.
>
> See always achieve guaranteed 100 percentage code coverage 100 percentage audit coverage 100 percentage testing coverage 100 percentage code checks 100 percentage code performance 100 percentage monitoring 100 percentage logging 100 percentage alerting 100 percentage security 100 percentage security hardening 100 percentage bugs fixing 100 percentage scenarios covering 100 percentage functionalities covering 100 percentage code review 100 percentage extreme check as well.
>
> I need 100 percentage guarantee (extensive, comprehensive, intensive, deep thorough code coverage, code scanning, code duplication, functionality missing, scenario missing, bug fixing, issues finding, testing types, testing coverage, extensive comprehensive real time testing, code performance, uniqueness, deduplication, O(1) latency always) — inside these brackets whatever is mentioned I need 100 percentage.
>
> Zero ticks loss and nowhere WebSocket should get disconnected or reconnect, not even a single tick should be missed. Meanwhile our entire WebSockets and connections should never ever become slow or locked or hanged or stuck. QuestDB should never ever fail or always disconnect. I need guarantee for both of these always.
>
> Always extreme comprehensive automation monitoring alerting notifying on Telegram capturing tracking auditing debugging detailing visualizing checking everything.
>
> Always activate and use all the agents and all subagents as well to do a deep thorough research.
>
> Always it should be common runtime dynamic scalable incremental approach.
>
> Always ensure to maintain O(1) latency and uniqueness guarantee and deduplication. Provide all these with real-time checks and guarantee and assurance and proof.
>
> Always provide the explanation in a table comparison easy viewable readable understandable format where anyone can easily understand.
>
> Explain everything like explaining to a lazy dumb non-technical kid where the kid or auto-driver or some technical or non-technical person can't understand anything even if he is lazy to read — so explain using diagrams + animations + easily-understood format, looking like an Insta reel where if 1 million people see it they all understand."

---

## What this means MECHANICALLY (the binding contract)

### A. Every new Claude Code session at START

Auto-fired by `.claude/settings.json` SessionStart hooks (already wired):

| Step | Tool | What it does |
|---|---|---|
| 1 | `mcp__tickvault-logs__run_doctor` | 7-section health snapshot |
| 2 | `mcp__tickvault-logs__summary_snapshot` | Last-hour error signatures |
| 3 | `mcp__tickvault-logs__run_doctor` (CloudWatch alarms) | Currently-firing alerts |
| 4 | `bash .claude/hooks/session-auto-health.sh` | doctor + validate-automation in bg |
| 5 | `bash .claude/hooks/session-context-brief.sh` | Active plans + open PR + errors-last-hour |
| 6 | `bash .claude/hooks/session-sanity.sh` | Branch + auto-save check |

**If ANY step fails:** fix the underlying issue. Do NOT bypass with skip-flags.

### B. Every new Claude Cowork task

The `.mcp.json` `tickvault-logs` entry is loaded automatically — same MCP tools surface as `mcp__tickvault-logs__*`. Therefore:

- Logs ✅ accessible via `mcp__tickvault-logs__app_log_tail` / `tail_errors`
- Queries ✅ accessible via `mcp__tickvault-logs__questdb_sql` (live metrics via CloudWatch / the `/metrics` exporter)
- DBs ✅ accessible (QuestDB via above)
- Project / product ✅ accessible (entire repo + grep + read)
- Local / AWS ✅ both supported (same MCP server)
- Common runtime ✅ same docker-compose Mac dev = AWS prod
- Dynamic / scalable ✅ same tool surface regardless of env
- Automated ✅ no manual setup needed

### C. Every plan item / task file MUST carry the 15-row + 7-row guarantee matrix

(Mechanically enforced by `.claude/hooks/per-item-guarantee-check.sh` + `make wave-guarantee-check`.)

#### 15-row "100% everything" matrix

| Demand | Mechanical proof artefact | Real-time check tool |
|---|---|---|
| 100% code coverage | `quality/crate-coverage-thresholds.toml` ratcheted per-crate floors (63.3–99.5, target 100%), floors only move up | `scripts/coverage-gate.sh` in CI |
| 100% audit coverage | `<event>_audit` table with DEDUP UPSERT KEYS | `mcp__tickvault-logs__questdb_sql` |
| 100% testing coverage | 22 test categories per `testing.md` (unit/integration/property/loom/dhat/fuzz/mutation/sanitizer/coverage/etc.) | `cargo test --workspace` green |
| 100% code checks | banned-pattern + pub-fn-test + pub-fn-wiring + plan-verify + secret-scan + 8 pre-commit gates | pre-push mandatory |
| 100% code performance | DHAT zero-alloc + Criterion p99 budgets + bench-gate ≤5% regression | `cargo bench` + `scripts/bench-gate.sh` |
| 100% monitoring | 7-layer telemetry (Prom counter + gauge + tracing span + Loki log + Telegram event + Grafana panel + audit table) | `mcp__tickvault-logs__run_doctor` |
| 100% logging | tracing macros mandatory; ERROR → Telegram via Loki/Alertmanager | hourly `errors.jsonl` rotation |
| 100% alerting | `alerts.yml` Prom rule + `resilience_sla_alert_guard.rs` ratchet | `mcp__tickvault-logs__run_doctor` (CloudWatch alarms) |
| 100% security | banned-pattern + secret-scan + `Secret<T>` enforcement + security-reviewer agent | `cargo audit` post-deploy |
| 100% security hardening | static IP enforcement + secret scan + `unused_must_use` lint | post-deploy IP verify |
| 100% bug fixing | adversarial 3-agent review (proven 4-bug catch rate per 30-commit PR) | pre-PR + post-impl agent pass |
| 100% scenarios covering | 9-box + chaos test for every new failure mode | `crates/storage/tests/chaos_*.rs` (16 today) |
| 100% functionalities covering | every pub fn has call site + matching test | pre-push gates 6+11 |
| 100% code review | adversarial 3-agent on diff BEFORE and AFTER impl | per-PR |
| 100% extreme check | all of above + ratchet test fails build on regression | every commit |

#### 7-row "Resilience demand" matrix

| Demand | Honest envelope (the truth) | Per-item proof |
|---|---|---|
| Zero ticks lost | Bounded zero loss inside chaos envelope: 100K-tick rescue ring → NDJSON spill → DLQ NDJSON catches every payload | item must not introduce new tick-drop path |
| WS never disconnects | SEBI 24h JWT forces ≥1 reconnect/day BY LAW. DETECT ≤5s, reconnect with `SubscribeRxGuard`, sleep-until-open post-close. | item must not break `SubscribeRxGuard` or pool watchdog |
| Never slow/locked/hanged | DHAT ≤4 alloc blocks/8KB across 10K calls; Criterion p99 ≤100ns enqueue; tick-gap >30s coalesced Telegram; core_affinity Core 0 | item must not add hot-path allocation |
| QuestDB never fails | ABSORB via 3-tier rescue→spill→DLQ + schema self-heal via `ALTER ADD COLUMN IF NOT EXISTS` | item must not break self-heal |
| O(1) latency | `from_le_bytes` + `papaya` + `Arc<HashMap>` + SPSC bounded; bench-gate ≤5% regression on hot path | item adds Criterion bench if hot path |
| Uniqueness + dedup | Composite `(security_id, exchange_segment)` per I-P1-11 + DEDUP UPSERT KEYS on every storage table | item DEDUP key includes segment |
| Real-time proof | 7-layer telemetry + SLO score every 10s + market-open self-test at 09:16:30 IST | item ratchet pins all 7 layers |

---

## D. Every Telegram message MUST follow the 10 commandments

(See `.claude/plans/friday-may-15-mega/topic-telegram-message-style-rules.md` for full detail.)

| # | Rule |
|---|---|
| 1 | Plain English ONLY — auto-driver level |
| 2 | NO library names (rkyv / papaya / DEDUP / arc-swap / mpsc / GCRA banned) |
| 3 | NO file paths |
| 4 | NO version numbers in body |
| 5 | YES emoji for status (✅ ⚠️ 🆘 🚀 🔔 🟢) |
| 6 | YES specific numbers ("11,034 instruments", "5/5 connections") |
| 7 | YES action verbs in degraded ("What you need to do RIGHT NOW: 1...2...3...") |
| 8 | One Telegram = one decision |
| 9 | Time stamps IST 12-hour ("9:13 AM", not "0913" or "09:13:00.000Z") |
| 10 | Severity emoji at START of subject |

**Litmus test for every NEW NotificationEvent:** Could a 60-year-old auto-driver read it on a phone in 5 seconds and know "OK or not OK + what to do"? If no → REWRITE.

---

## E. Every NEW design / PR / task MUST run adversarial 3-agent review

For any change touching > 3 crates or > 1,000 LoC: spawn THREE specialist agents IN PARALLEL:

| Agent | Mandate | Output |
|---|---|---|
| **hot-path-reviewer** | No `.clone()` / `Vec::new()` / `.collect()` / `format!()` / `Box` / `dyn` on hot path | Report ≤ 400 words |
| **security-reviewer** | Secret exposure, ILP injection, path traversal, unsafe blocks, missing sanitization | Report ≤ 400 words |
| **general-purpose (hostile)** | Race conditions, daily-reset collisions, missing market-hours gate, missing edge-trigger, false-OK class bugs | Report ≤ 600 words |

Wait for ALL THREE. Synthesize into CRITICAL/HIGH/MEDIUM/LOW table. Fix every CRITICAL/HIGH inline BEFORE opening the PR. After implementation lands green → run same 3 agents on the diff (proven pattern: 4-bug catch rate per 30-commit PR).

---

## F. The HONEST 100% claim (mandated PR-body wording)

When ANY PR body / commit message / Telegram message / docs writes "100% guarantee", it MUST be qualified exactly:

> "100% inside the tested envelope, with ratcheted regression coverage:
> ≤60s QuestDB outage absorbed by rescue→spill→DLQ;
> ≤100,000-tick ring buffer capacity (constant `TICK_BUFFER_CAPACITY`, ratcheted by `zero_tick_loss_alert_guard.rs`);
> bench-gated O(1) hot path;
> composite-key uniqueness;
> chaos-tested 65h Fri 16:00 IST → Mon 09:00 IST weekend sleep/wake.
> Beyond the envelope, DLQ NDJSON catches every payload as recoverable text."

**Anything stronger ("WebSocket never disconnects" / "QuestDB never fails" without envelope) = REJECT IN REVIEW.**

---

## G. Every artefact MUST include kid-friendly + table explanation

For every plan item / new feature / Telegram message / docs:

- Table-comparison format ✅ (operator-mandated)
- Auto-driver / dumb-kid analogy ✅ ("Sir, imagine your juice shop register breaks...")
- Diagrams / ASCII visualizations ✅
- Tight bullet lists, not paragraphs
- If a 1-million-view Insta reel must explain it → simplify until it can

---

## The 11 ALWAYS-ON rules (cheat sheet for any session)

| # | Rule | Source |
|---|---|---|
| 1 | Common-runtime: same docker-compose Mac dev = AWS prod | `aws-budget.md` rule 10 |
| 2 | RAM-first hot path — no DB queries from indicator/strategy/risk paths | `aws-budget.md` rule 12 |
| 3 | Composite-key uniqueness `(security_id, exchange_segment)` everywhere | `security-id-uniqueness.md` |
| 4 | DEDUP UPSERT KEYS on every QuestDB table + `ALTER ADD COLUMN IF NOT EXISTS` | `stream-resilience.md` B10 |
| 5 | Every `error!` carries `code = ErrorCode::X.code_str()` field | `error_code_tag_guard.rs` |
| 6 | flush/persist/drain failures use `error!`, never `warn!` | `error_level_meta_guard.rs` |
| 7 | Every counter panel wraps in `increase()` or `rate()` (per audit-findings Rule 12) | `audit-findings-2026-04-17.md` |
| 8 | Every new pub fn has a call site | `pub-fn-wiring-guard.sh` |
| 9 | Market-hours-aware tokio tasks (audit-findings Rule 3) | `audit-findings-2026-04-17.md` |
| 10 | Edge-triggered alerts only (audit-findings Rule 4) | `audit-findings-2026-04-17.md` |
| 11 | NO false-OK signals — gate success notifications on positive progress (audit-findings Rule 11) | `audit-findings-2026-04-17.md` |
| 12 | One PR open at a time — finish + merge current before starting next (operator lock 2026-05-15) | `.claude/rules/project/pr-completion-protocol.md` |
| 13 | Only 2 WebSocket connections EVER — 1 main-feed + 1 order-update; never depth-20, depth-200, or any 2nd main-feed conn (operator lock 2026-05-15) | `.claude/rules/project/websocket-connection-scope-lock.md` |

---

## H. PR Completion Protocol (MANDATORY, every PR — operator lock 2026-05-15)

> Operator verbatim 2026-05-15:
> "always ensure to make it ready for merge once branch pr is created and finished and even always monitor the current PR until it gets merged successfully to main after doing this alone only go ahead with the remaining process always dude okay? always save this bro okay?"

**The serial-completion rule:** Claude MUST finish + merge the current PR to `main` BEFORE starting the next item. No parallel branches. No "stack 3 PRs in flight". One PR at a time, monitored to merge.

### The 10-step loop (every PR)

| Step | Mandatory action | Tool |
|---|---|---|
| 1 | Code complete + local verify (`cargo test`, banned-pattern, pub-fn guards, plan-verify) | Bash |
| 2 | Push branch (`git push -u origin <branch>` with retry on network err) | Bash |
| 3 | Open PR as **draft**; body contains 9-box + 15-row + 7-row matrices + honest 100% claim | `mcp__github__create_pull_request` |
| 4 | Tick the plan checkbox `[ ]` → `[x]` for every item this PR completes — INSIDE this PR's diff, same commit | Edit |
| 5 | Mark PR **ready for review** once CI starts green | `mcp__github__update_pull_request` |
| 6 | Enable **auto-merge** (squash) so merge fires the moment branch protection signals pass | `mcp__github__enable_pr_auto_merge` |
| 7 | **Subscribe** to PR activity (CI status + review comments) | `mcp__github__subscribe_pr_activity` |
| 8 | On CI red → diagnose → push fix → re-monitor. On reviewer comment → fix or reply with grep evidence. | event-driven |
| 9 | Verify merge to `main`: `pull_request_read` returns `state=closed, merged=true` | `mcp__github__pull_request_read` |
| 10 | Unsubscribe; ONLY THEN start the next item (next PR) | `mcp__github__unsubscribe_pr_activity` |

### What this rule FORBIDS

| Banned pattern | Why |
|---|---|
| Opening PR #2 before PR #1 is merged to main | Violates serial completion |
| Marking plan checkbox `[x]` BEFORE PR merges | The merge IS the completion signal |
| Closing the session while a PR is mid-flight without subscribing | Operator loses visibility |
| Manual merge skipping CI | Branch protection blocks; auto-merge respects gates |
| Bypassing `--no-verify` on push or `--no-gpg-sign` | Operator never authorized this |

### Auto-driver one-liner

> "Sir, one juice at a time. Make the orange juice → squeeze it → pour it → hand it to customer → wait until they pay → only then start the next mango juice. No making three juices at once."

### Mechanical ratchet (to be wired)

- New hook `.claude/hooks/serial-pr-guard.sh` runs at session start; refuses code commits if an open PR authored by this branch exists on the operator's repo without merge.
- Tag-guard meta-test `crates/storage/tests/serial_pr_protocol_guard.rs` greps for the canonical phrases in this section so the rule cannot be silently deleted.

---

## I. WebSocket connection scope lock (MANDATORY, FOREVER — operator lock 2026-05-15)

> **⚠ DHAN LIVE WS RETIRED 2026-07-13 — new locked state (full amendment: `websocket-connection-scope-lock.md` "2026-07-13 Amendment" §-section):** operator verbatim: *"now remove this entire Dhan live websocket feed instruments subscription even entire live websocket feed itself... As of now only Groww and Dhan historical api pull as we discussed last night along with option chain."* The locked state is now: **(1) Groww = the SOLE live market-data feed** (1 native connection, full WAL/ring/spill/DLQ chain, `feed='groww'`); **(2) Dhan main-feed live WS = RETIRED, deletion authorized** — with it the `SubscriptionScope` enum, the instrument CSV download/parse chain, and the tick-gap detector/WS-GAP-06 (operator "agreed dude" rulings, 2026-07-13); **(3) Dhan order-update WS = KEPT functional-dormant, rewired into `dhan_rest_stack`** (JWT + client-id only; no market data); **(4) Dhan REST surface = the §8 per-minute spot-1m + option-chain pulls on HARDCODED SIDs (13/25/51) + historical**, lock semantics per `dual-instance-lock-2026-07-04.md` §3.5; **(5) GDF feed #3 = a separate default-OFF lock** (`gdf-third-feed-scope-2026-07-13.md`) — the pluggable seam (FeedsConfig, feed-in-key tables, WAL/aggregator) must stay clean for it. Re-introducing any Dhan market-data WS requires a fresh dated quote in the scope-lock file first. The two tables below ("only WebSocket connections this product will EVER open" + "Forbidden FOREVER") are retained as historical audit; the scope-lock amendment's §A table is the effective contract.
>
> **2026-07-15:** the GROWW live WS is ALSO retired (operator Q1, received directly in this session: *"remove the whole Groww live feed; keep only spot 1m and option chain for both brokers; go."*) — the market-data runtime is REST-only for both brokers, live market-data WS count = 0; see `websocket-connection-scope-lock.md` "2026-07-15 Amendment".
>
> **⚠ 2026-07-14 — Groww ORDER-SIDE build authorized, live fire LOCKED (pointer):** the operator authorized BUILDING + INTEGRATING the Groww order-side (orders / smart orders / portfolio / margin / user) **entirely behind a 4-gate live-fire lattice** — `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §39 + `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md` §10 (each gate build-failure-ratcheted by `crates/common/tests/groww_order_lattice_guard.rs`). NO Groww order fires until a separate future dated live-orders enable; `dry_run` stays `true` and the §28 strategy/indicator boundary stays frozen. This is a market-data-WS-neutral change — it adds NO WebSocket connection.
>
> **⚠ ALLOWED-INSTRUMENTS SUPERSEDED 2026-05-27 by [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md):** main-feed expanded from 4 IDX_I SIDs to ~250 daily-fetched SIDs (all NSE indices + 1 BSE SENSEX + unique F&O underlyings, all Quote mode). The 2-WebSocket lock (1 main-feed + 1 order-update FOREVER) is UNCHANGED — only the instrument set on the single main-feed conn expanded per operator approval 2026-05-27. The 2026-05-15 verbatim quote below still binds the 2-WS architecture.
>
> Operator verbatim 2026-05-15:
> "except [for] this 1 connection main feed websocket and order update websocket we will never ever use anything else"
>
> **⚠ SECOND-FEED EXTENSION 2026-06-19 by [`groww-second-feed-scope-2026-06-19.md`](./groww-second-feed-scope-2026-06-19.md):** the Dhan 2-WS lock (1 main-feed + 1 order-update) below is UNCHANGED. The operator authorized **GROWW** as an independent, **default-OFF**, per-feed-toggleable second market-data feed (feed #2), implemented in **native tickvault Rust** (brutex = reference only, no code pulled), reusing the same WAL/ring/spill/DLQ/aggregator chain. It adds NO Dhan connection. See that file for the verbatim authorization + contract.
> **⚠ FUTIDX-4 EXTENSION 2026-07-08 by [`daily-universe-scope-expansion-2026-05-27.md`](./daily-universe-scope-expansion-2026-05-27.md) §36:** the single Dhan main-feed conn additionally subscribes ALL available monthly-expiry index-futures contracts of the 4 underlyings (§36.7, 2026-07-10; nearest first; NIFTY/BANKNIFTY/MIDCPNIFTY = NSE_FNO, SENSEX = BSE_FNO; Quote mode; NEVER rolls intraday), and the Groww watch set gains the SAME logical contracts on its existing single connection. The 2-WS lock is UNCHANGED. The "NSE_EQ / NSE_FNO / BSE_FNO / MCX / currency subscriptions" forbidden row below is carved down to "…except the §36/§36.7 FUTIDX contracts (NSE_FNO ×3 + BSE_FNO SENSEX futures)". Operator verbatim 2026-07-08: *"for both dhan and groww we need to add futures and those also should be subscribed along with this, especially only for nifty banknifty and sensex nifty midcap."* Operator verbatim 2026-07-10 (relayed via the coordinator session): *"instead of only one current month futures contracts just take all the futures of these indices — I mean take all available applicable months futures."*

**The only WebSocket connections this product will EVER open:** (Dhan; the Groww second feed is governed separately per the 2026-06-19 extension above)

| WS type | Count | Allowed instruments | Mode |
|---|---|---|---|
| **Main feed** (`wss://api-feed.dhan.co`) | **1 conn** | 4 IDX_I SIDs ONLY: NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21 (per `LOCKED_UNIVERSE` in `crates/common/src/locked_universe.rs`) | Ticker for IDX_I (16-byte packets) |
| **Order update** (`wss://api-order-update.dhan.co`) | **1 conn** | n/a (receives order events for orders we place) | JSON, MsgCode 42, filter `Source=P` |

**AWS-lifecycle PR #7 (2026-05-19) update:** the 218 NSE_EQ F&O underlying stocks dropped from the live universe. `SubscriptionScope` collapsed to a single-variant enum (`Indices4Only`). Compile-time prevention of any scope expansion. See `.claude/rules/project/websocket-connection-scope-lock.md` for the full LOCKED contract.

**Forbidden FOREVER (until operator explicitly re-approves):**

| ❌ Banned WS type | Why |
|---|---|
| Depth-20 (`depth-api-feed.dhan.co/twentydepth`) | Operator lock — not in scope for any phase |
| Depth-200 (`full-depth-api.dhan.co/?token=...`) | Same |
| Any 2nd/3rd/4th/5th main-feed conn | 4 SIDs fit on 1 conn (Dhan cap = 5,000/conn). More conns = wasted token + IP budget |
| Any new WS endpoint Dhan ships in future | Not in scope without operator re-approval |
| NSE_EQ / NSE_FNO / BSE_FNO / MCX / currency subscriptions — **except the §36/§36.7 FUTIDX contracts** (all monthly serials; NSE_FNO ×3 underlyings: NIFTY/BANKNIFTY/MIDCPNIFTY + BSE_FNO: SENSEX futures; operator quotes 2026-07-08 + 2026-07-10 in the banner above) | `SubscriptionScope` stays a single-variant enum; the ONLY runtime path that can emit an NSE_FNO/BSE_FNO subscription is the `IndexFuture` role (underlying-arity-4 + per-underlying serial envelope, ratcheted by `daily_universe_scope_guard.rs`) — everything else in this row remains impossible (hostile-review round 2, 2026-07-08: the pre-§36 "compile-time impossible" blanket rationale was stale for those two segments and is corrected here inline, matching the banner) |

**Reconnect parity (both allowed WS types):**

- First reconnect attempt: **0 ms (instant)** on both. Main feed via `compute_reconnect_base_delay_ms(0, _, _) → 0` (`connection.rs:1666`); order-update via `compute_reconnect_backoff_ms(1) → 0` (`order_update_connection.rs:639`, Phase 0 Item 4 fix 2026-05-15).
- Subsequent attempts: exponential backoff capped at `*_MAX_MS`.

**Mechanical guards (post-PR #7b):**

- `SubscriptionScope` is a single-variant enum (`Indices4Only`) in `crates/common/src/config.rs`. Adding a new variant requires a rule-file edit + ratchet update.
- `effective_main_feed_pool_size(_, _) → PHASE_0_MAIN_FEED_CONNECTION_COUNT = 1` (constant) in `crates/common/src/config.rs`. No path increases this.
- Source-scan ratchet `crates/core/tests/indices4only_scope_lock_guard.rs` (3 tests) blocks reappearance of the retired `FullUniverse` / `IndicesOnlyAllExpiries` / `IndicesUnderlyingsOnly` variants and the 3 retired `subscribe_*` flags anywhere in `crates/`.
- Depth-20 / depth-200 / Phase 2 / movers / greeks pipelines: all deleted from the codebase in earlier AWS-lifecycle PRs (#2, #3, #4, #5, #6a, #6b).

**Auto-driver one-liner:**

> "Sir, only 2 phone lines to Dhan EVER. Line 1: live price feed for 4 fruits (NIFTY, BANKNIFTY, SENSEX, INDIA VIX). Line 2: order confirmation notifications. NOTHING else. No phone for vegetable prices. No phone for 200 individual fruit stalls. EVER. If anyone tries to add a 3rd phone, this rule file + the compiler both reject the PR — the enum has only 1 variant; there is no place to put a 3rd phone line."

---

## Mechanical enforcement chain (the gates that fail-the-build)

| Gate | Where | What it catches |
|---|---|---|
| `pre-commit-fast-gate.sh` | every `git commit` | fmt + banned-pattern + secret-scan + version pin + 9 invariants |
| `pre-push-gate.sh` | every `git push` | 12 fast gates (~35s total) |
| `per-item-guarantee-check.sh` | pre-PR + `make wave-guarantee-check` | 15-row + 7-row matrix presence |
| `banned-pattern-scanner.sh` | pre-commit | `HashSet<u32>` / `.clone()` on hot path / etc. |
| `pub-fn-test-guard.sh` | pre-push | every pub fn has test or TEST-EXEMPT comment |
| `pub-fn-wiring-guard.sh` | pre-push | every pub fn has a call site |
| `plan-verify.sh` | pre-push | active plan items checked |
| `error_code_tag_guard.rs` | `cargo test` | every error code mentioned in a log has `code =` field |
| `error_code_rule_file_crossref.rs` | `cargo test` | every ErrorCode variant has rule-file mention |
| `dedup_segment_meta_guard.rs` | `cargo test` | every DEDUP key includes segment |
| `operator_health_dashboard_guard.rs` | `cargo test` | every counter has a Grafana panel |
| `resilience_sla_alert_guard.rs` | `cargo test` | every counter has an alert rule |
| `error_level_meta_guard.rs` | `cargo test` | flush/persist failures use `error!` not `warn!` |
| `scripts/bench-gate.sh` | post-merge | 5% regression budget on hot-path benches |
| `scripts/coverage-gate.sh` | post-merge | ratcheted per-crate floors (63.3–99.5, target 100%) per crate |
| `block-bypass.sh` (wired via `pre-tool-dispatch.sh`) | pre-bash | `--no-verify`, `git stash push/pop/apply/save`, `--dangerously-skip-permissions`, `gh pr merge --admin`, force-push to main/master |
| GitHub Actions CI | every PR | full 22-test battery + mutation + fuzz |

**Anything that needs to be true must be machine-checked. Hand-wave = REJECT.**

## Advisory automation (NOT gates — opt-in helpers)

Distinct from the build-failing chain above. These do NOT block commits/pushes/merges — they are slash-command tools or model-invoked skills that augment reasoning. Listed here so future sessions know they exist.

| Tool | Invocation | What it does |
|---|---|---|
| `/nse-fact-checker` skill (`.claude/skills/nse-fact-checker/`) | manual slash-command | Verifies NSE/Dhan/SEBI/Income-Tax rates against authoritative sources; recommends 5-line citation metadata for NEW rate/cost/regulatory constants. Trigger surface limited to external regulatory facts; pure protocol constants are covered by the gates above. |
| `/quality` skill | manual slash-command | Runs the 8-check quality pipeline locally (fmt/clippy/test/docs/banned-patterns/test-count/audit/deny) |
| `/health` skill | manual slash-command | Runs `make doctor` + `make validate-automation` for the 30-check observability sweep |

---

## Read-this-first protocol for ANY new session

Any new Claude Code session (CLI or web Cowork) opening this repo MUST:

```bash
# 1. Sync
git fetch origin
git pull <current-branch>

# 2. Mandatory reads (in order)
cat CLAUDE.md                                                            # project charter
cat .claude/rules/project/operator-charter-forever.md                    # this file (the forever rules)
cat .claude/rules/project/zero-loss-guarantee-charter.md                 # the per-PR/task/session GUARANTEE CHECK (2026-05-29)
cat .claude/rules/project/wave-4-shared-preamble.md                      # wave-4 specifics (still applies)
cat .claude/plans/friday-may-15-mega/INDEX.md                            # current state
cat .claude/plans/friday-may-15-mega/step-1-honest-envelope.md           # canonical reference
tail -50 .claude/plans/friday-may-15-mega/00-decisions-log.md            # recent locks

# 3. Auto-fire (handled by SessionStart hook — already wired)
# - run_doctor / summary_snapshot (CloudWatch alarms surfaced via run_doctor)

# 4. Ask operator: "I've read the charter. Current state is <X>. Continuing with <Y>?"
```

---

## Templates that MUST exist (so every artefact carries the charter)

| Template | Path | Purpose |
|---|---|---|
| PR template | `.github/pull_request_template.md` | every PR auto-asks for 15-row + 7-row matrix check |
| Task template | `.claude/plans/friday-may-15-mega/tasks/T00-example-template.md` | every Friday task carries the matrix |
| Plan-item template | (inline in plan files) | every plan item carries 9-box minimum |

---

## Summary (auto-driver one-liner)

> "Sir, this is the FOREVER contract. Every new chat I open, every PR I write, every task I claim — this file is the FIRST thing I read. Same rules: plain Telegram messages, 100% in 15 dimensions, honest envelope (no 'never disconnects' lies), kid-friendly explanations, 3-agent adversarial review on every PR. Mac dev = AWS prod identical. NEVER paraphrase the 100% claim without the envelope qualifier. NEVER use library jargon in operator-facing text. Mechanical gates catch every shortcut. This is the standard for tickvault forever."
