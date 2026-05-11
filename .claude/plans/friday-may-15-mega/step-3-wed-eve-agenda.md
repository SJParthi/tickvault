# Step 3 — Wed Eve Agenda (pre-loaded Mon 21:35 IST)

**Topic:** Go-Live Gate + Adversarial 3-Agent Review + Mega Plan APPROVAL
**When:** Wed 2026-05-13 evening IST
**Status:** PRE-LOADED — operator can read offline before Wed discussion
**Prereq:** Step 1 wrapped Mon eve + Step 2 wrapped Tue eve

---

## Why this step matters

Wed is the FINAL DESIGN MEETING. After Wed night, the mega plan flips Status: DRAFT → APPROVED. Friday morning, 5+ parallel Claude sessions execute the task board. No more discussion until execution begins.

Wed eve covers:
1. Adversarial 3-agent review on Phase 0.5 + Step 2 design
2. Shadow-equivalence mechanism (paper vs live diff)
3. `dry_run` → live flip mechanism
4. Exactly-once order semantics deep-dive
5. Post-trade reconciliation procedure
6. Mega plan FINAL rewrite + APPROVAL

---

## Auto-driver / dumb-kid framing

> "Sir, before you let the new driver take the car out for actual customers, you do THREE final things:
> 1. **Hire 3 inspectors** to find anything you missed — they're paid to be PARANOID (adversarial review)
> 2. **Test-drive with NO passengers first** — same route, same speed, just observe (shadow-equivalence)
> 3. **Sign the final approval document** — once signed, no more changes; tomorrow we start
>
> If inspector finds anything CRITICAL — fix tonight, no exceptions. If shadow-drive shows weird behavior — fix tonight. Only when all 3 inspectors clear + shadow-drive matches paper-drive → sign."

---

## Block A — Adversarial 3-Agent Review (charter §3 mandate)

Spawn THREE specialist subagents IN PARALLEL on the locked Phase 0.5 + Step 2 design:

| Agent | Mandate | Output |
|---|---|---|
| **hot-path-reviewer** | Hot-path violations: no `.clone()`, `Vec::new()`, `.collect()`, `format!()`, `Box`, `dyn` on hot path. Bounded channels + zero-alloc enqueue. | Report ≤ 400 words, written to `research/wed-eve-hotpath.md` |
| **security-reviewer** | Audit: secret exposure in logs/labels, ILP injection, path traversal, unsafe blocks, missing input sanitization, `Secret<T>` usage. | Report ≤ 400 words, written to `research/wed-eve-security.md` |
| **general-purpose (hostile)** | Hostile bug-hunt: race conditions, daily-reset collisions, missing market-hours gate, missing edge-trigger, flush/persist using `warn!` instead of `error!`, counters shown raw without `increase()`, pub fn defined-but-never-called, false-OK class bugs, market-day edge cases (holidays, half-days), Phase 2 emit guard regressions, IST midnight collisions. | Report ≤ 600 words, written to `research/wed-eve-hostile.md` |

### Synthesis after agents return

| Verdict | Action |
|---|---|
| CRITICAL | Fix inline Wed eve. Mega plan NOT signed until cleared. |
| HIGH | Fix inline Wed eve. Mega plan NOT signed until cleared. |
| MEDIUM | Add to Friday task board as priority sub-task. |
| LOW | Note in plan; defer to next wave. |
| FALSE-POSITIVE | Document with grep evidence in `research/wed-eve-triage.md` |

**Charter rule:** if any agent finds 2+ CRITICALs, the Wed-night APPROVAL flip is BLOCKED. Schedule a Thu emergency review.

---

## Block B — Shadow-Equivalence Mechanism

Goal: prove the paper-trading code path produces IDENTICAL decisions to the live code path, given identical tick streams, BEFORE flipping to live orders.

### Architecture (locked design — no code yet)

```
                    ┌─────────────────────┐
                    │  Single tick source │
                    └──────────┬──────────┘
                               │
                  ┌────────────┴────────────┐
                  ▼                         ▼
         ┌────────────────┐      ┌─────────────────┐
         │  PAPER path    │      │  LIVE path      │
         │  dry_run=true  │      │  dry_run=true   │
         │  emits Order   │      │  emits Order    │
         │  to log only   │      │  to Dhan but    │
         │                │      │  dropped at     │
         │                │      │  api_client     │
         └────────┬───────┘      └────────┬────────┘
                  │                       │
                  ▼                       ▼
         ┌────────────────────────────────────────┐
         │  DIFF ENGINE — every minute                │
         │  Per (security_id, side, qty, price):    │
         │    Δ <= 0 → MATCH ✓                      │
         │    Δ > 0  → MISMATCH (Telegram HIGH)     │
         └────────────────────────────────────────┘
```

### Wed eve questions

| Q | Decision needed |
|---|---|
| Shadow run for how many trading days before approving live? | Default: 5 trading days = 1 week |
| Tolerance: zero diff acceptable? | Default: ZERO (no epsilon) |
| What action on diff > 0? | Telegram HIGH + halt for the day |
| Where do paper-vs-live orders log? | `paper_orders` + `live_orders` audit tables |

---

## Block C — dry_run → live Flip Mechanism

Goal: ONE config flag flips between paper and live. No code change, no redeploy.

### Locked design

`config/base.toml::[strategy] dry_run = true` (current default).

Setting `dry_run = false` makes the OMS api_client actually POST to Dhan. Everything else identical.

### Wed eve safety questions

| Q | Decision |
|---|---|
| Who can flip dry_run? | Operator only, via SSH + `sed` + restart |
| Confirmation required? | Telegram CRITICAL on boot if `dry_run = false` |
| Audit trail? | `boot_audit` table records dry_run state per boot |
| Rollback if live blows up? | `make stop && sed -i 's/dry_run = false/dry_run = true/' config/base.toml && make run` |

---

## Block D — Exactly-Once Order Semantics

Per `dhan-ref/07-orders.md` + OMS-GAP-05 (idempotency):

1. Every order carries a `correlationId` (UUID v4, max 30 chars, [a-zA-Z0-9 _-])
2. We store `correlationId → status` in Valkey BEFORE sending
3. If we crash between POST and response → on restart, query Dhan `GET /v2/orders/external/{correlationId}` — recover the order_id
4. If duplicate placement attempt → reject locally (UUID already in Valkey)

### Wed eve questions

| Q | Decision needed |
|---|---|
| UUID v4 collision probability acceptable? | Yes — 2^122 space, never a concern |
| Valkey persistence guarantee? | AOF every-sec; survives restart within 1s window |
| Cross-restart reconciliation interval? | At every boot, sweep Valkey for in-flight orders |

---

## Block E — Post-trade Reconciliation

After every trading day at 16:00 IST:

1. Pull our `order_audit` table (every order we attempted)
2. Pull Dhan `GET /v2/orders` for the day (every order Dhan saw)
3. Pull Dhan `GET /v2/trades` for the day (every fill Dhan reports)
4. Three-way reconcile: every order in our audit MUST appear in Dhan's order book AND every fill MUST appear in our trade log
5. Any mismatch → Telegram HIGH + diff list

### Wed eve questions

| Q | Decision needed |
|---|---|
| Acceptable diff between our log and Dhan's? | ZERO (every order must match) |
| What if Dhan shows an order we didn't place? | Telegram CRITICAL — possible account compromise |
| What if we placed an order Dhan doesn't show? | Telegram HIGH — possible network loss |
| Reconciliation latency target? | ≤ 5 min after 15:30 IST close |

---

## Block F — Mega Plan APPROVAL Flip

**ONLY after Blocks A-E are all GREEN:**

1. Rewrite strawman → `99-mega-plan-final.md`
2. Status: DRAFT → APPROVED
3. Operator signs in `00-decisions-log.md` with explicit "[APPROVED]" entry
4. Task Board frozen — no more design changes
5. Commit + push final plan
6. Friday morning, parallel Claude sessions claim tasks via commit-push race

### What APPROVAL means

- Plan content is FROZEN. Changes require a SUPERSEDED entry in decisions log.
- Task assignments via Task Board (`tasks/_board.md`).
- Each task gets its own branch `claude/task-T<NN>-*`.
- PR per task. CI gates per PR.

---

## Block G — Friday Task Board Fan-Out

Wed night ends with `tasks/_board.md` populated:

| Task | Branch | Estimated LoC | Status |
|---|---|---|---|
| T05-01 NET-01 IP drift detector | `claude/task-T05-01-net-01-ip-drift` | ~150 | AVAILABLE |
| T05-02 NET-02 DNS resolution failure | `claude/task-T05-02-net-02-dns-failure` | ~100 | AVAILABLE |
| T05-03 PROC-01 OOM kill detector | `claude/task-T05-03-proc-01-oom-kill` | ~120 | AVAILABLE |
| T05-04 DH-911 silent black-hole | `claude/task-T05-04-dh-911-black-hole` | ~80 | AVAILABLE |
| T05-05 WS-BACKPRESSURE-01 gauge | `claude/task-T05-05-ws-backpressure-01` | ~60 | AVAILABLE |
| T05-06 TCP SO_KEEPALIVE | `claude/task-T05-06-tcp-keepalive` | ~40 | AVAILABLE |
| T05-07 WS-BACKPRESSURE-02 SO_RCVBUF | `claude/task-T05-07-ws-backpressure-02` | ~40 | AVAILABLE |
| T05-08 RESOURCE-03 spill-size guard | `claude/task-T05-08-resource-03-spill` | ~80 | AVAILABLE |
| T05-09 core_pinning wiring verify | `claude/task-T05-09-core-pinning-verify` | ~30 (verification) | AVAILABLE |
| T05-10 PreMarketReady consolidator | `claude/task-T05-10-premarket-ready-telegram` | ~200 | AVAILABLE |

Plus Step 2 tasks (T06-NN-*) — to be populated after Tue eve decisions.

Stale-claim recovery rule: 30 min CLAIMED with no commits → STALE, 60 min total → AVAILABLE.

---

## Charter check (15-row + 7-row matrix per item)

Every task on the board MUST carry the matrix in its task file (`tasks/T<NN>-*.md`). Wed eve verifies:

| Demand | Verified by |
|---|---|
| 100% audit coverage | Every task adds/extends an audit table |
| 100% testing | Each task file declares which of 22 test categories it covers |
| 100% monitoring | Each task adds at least 1 Prom counter + 1 Grafana panel |
| 100% alerting | Each task adds 1 alert rule in `alerts.yml` |
| 100% bug fixing | Each task ran 3-agent adversarial review |
| 100% code review | Each task PR runs 3-agent review again on the diff |
| Zero ticks lost | Task doesn't add new tick-drop path |
| O(1) latency | If hot path, task adds DHAT + Criterion |

---

## Expected Wed eve output

1. 3 adversarial agent reports written to `research/wed-eve-*.md`
2. Block A-E decisions locked in `00-decisions-log.md`
3. `99-mega-plan-final.md` written (Status: APPROVED)
4. `tasks/_board.md` fully populated with T05 + T06 series
5. New file `step-3-discussion-log-wed-eve.md` written
6. Mega plan APPROVED — Friday execution can begin

**Estimated Wed eve duration:** 2-3 hours (FAANG-style final design review + signing meeting)

---

## What this file is for

Wed eve session opens cold → reads INDEX → reads this file → already knows the 7 blocks (A-G) to work through → no warm-up overhead.

**Token budget Wed eve:** ≤ 70% of 5-hour limit (the 3 adversarial agents will burn most of it).
