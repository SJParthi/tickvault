# Z+ Defense Doctrine — Every PR, Every Task, Every Function, Every Bug

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > this file > defaults.
> **Scope:** PERMANENT. Every PR, every task, every subtask, every bug, every functionality, every technique, every commit — FOREVER.
> **Status:** Operator-locked 2026-05-12 04:46 IST after live WS dead-air incident.
> **Auto-load trigger:** Always loaded (this path is in `.claude/rules/project/`).

---

## 🛡️ The Auto-Driver Story (60-second read)

> Sir, imagine the Prime Minister of India. He doesn't have ONE bodyguard — he has **Z+ security**: 36 commandos, 6 layers, multiple agencies, redundant escape routes, decoy convoys, signal jammers, medical team on standby. If layer 1 fails, layer 2 catches it. If layer 2 fails, layer 3. Even if the impossible happens, layer 6 still exists.
>
> **Our trading system is the Prime Minister.** Every WebSocket connection, every QuestDB write, every order placement, every indicator calculation — Z+ security. No single point of failure. No "I hope this works". Multiple verified layers, each with audit trail, alarms, and escalation path.

This doctrine is the meta-rule that ALL future tickvault work inherits.

---

## ⚡ The ONE rule (the foundation)

**For every PR / task / subtask / issue / bug / functionality / technique:**

> If the happy path fails, what catches it? If that catches fails, what catches THAT? Until you can answer "what catches the absolute last failure", the design is NOT complete.

This is what I'll call the **Z+ test**: name the 6 layers, name the audit trail, name the alarm, name the operator action.

---

## 🎯 The 6-Layer Z+ Defense Pattern (mandatory for every new design)

| Layer | Role | Example (WS health) | Example (QuestDB persist) | Example (Order placement) |
|---|---|---|---|---|
| **L1 DETECT** | First sensor — fast, cheap, always-on | Frame-rate gauge per conn (5s window) | ILP write error counter | Order ACK timeout |
| **L2 VERIFY** | Cross-check the sensor before alarming | Subscribe-ACK round-trip (require frame in 5s) | Re-query QuestDB for the row | Re-query order book |
| **L3 RECONCILE** | Periodic sync with ground truth | Daily server-state reconcile @ 09:14:30 IST | Daily candle cross-verify | Daily P&L reconcile vs Dhan |
| **L4 PREVENT** | Stop the failure before it happens | Pre-reconnect token-age guard | Pre-flush spill pre-flight | Pre-trade margin check |
| **L5 AUDIT** | Continuous observability | App-level ping audit | Per-flush latency histogram | Order audit table |
| **L6 RECOVER** | Nuclear option — orchestrated escalation | Soft restart after 3-of-3 failures | 3-tier rescue→spill→DLQ | Kill switch + Telegram alert |
| **L7 COOLDOWN** | Don't let recovery itself cause damage | 30s drain before reconnect | Backoff before retry | Rate-limit before re-place |

**Every new design MUST fill in all 7 cells.** If a cell is genuinely N/A, write "N/A — <reason>" — never leave blank.

---

## 📋 The Z+ Checklist (paste this into EVERY new plan item)

```
## Z+ Defense Checklist

| Layer | Mechanism | Catches | Latency | LoC |
|---|---|---|---|---|
| L1 DETECT    | <what>     | <root causes 1-N>     | <ms-s>  | <N> |
| L2 VERIFY    | <what>     | <root causes>         | <ms-s>  | <N> |
| L3 RECONCILE | <what>     | <root causes>         | daily   | <N> |
| L4 PREVENT   | <what>     | <root causes>         | pre-op  | <N> |
| L5 AUDIT     | <what>     | <all>                 | cont.   | <N> |
| L6 RECOVER   | <what>     | <terminal cases>      | <s-min> | <N> |
| L7 COOLDOWN  | <what>     | recovery-induced harm | <s>     | <N> |

## Z+ 15-row "100% Everything" Matrix
(copy from operator-charter-forever.md §C, fill in this item's specifics)

## Z+ 7-row "Resilience" Matrix
(copy from operator-charter-forever.md §C, fill in this item's specifics)

## Z+ Adversarial 3-Agent Review
- [ ] hot-path-reviewer pass BEFORE impl
- [ ] security-reviewer pass BEFORE impl
- [ ] general-purpose hostile pass BEFORE impl
- [ ] all 3 agents repeat AFTER impl on the diff

## Z+ Honest Envelope Claim
(copy from operator-charter-forever.md §F, fill in this item's specific bounded guarantee — no "never fails" without envelope qualifier)

## Z+ Auto-Driver Explanation
(1 paragraph: "Sir, imagine you run a juice shop..." — auto-driver-readable)
```

**No code commit until ALL boxes filled.** Mechanical enforcement via `.claude/hooks/per-item-guarantee-check.sh` (extends to require Z+ checklist presence).

---

## 🚨 What This Doctrine FORBIDS

| Banned pattern | Why | Mechanical check |
|---|---|---|
| Single-layer "I hope this works" design | No L1-L7 = no Z+ | Plan-verify hook rejects items without Z+ matrix |
| `unwrap()` / `expect()` in prod (already banned) | Single-point panic = no L6 | Existing clippy lints |
| Silent retry without escalation cap | No L6 = infinite loop | Adversarial review catches |
| `warn!` on persist/flush/drain failure | Should be `error!` | Existing `error_level_meta_guard.rs` |
| Counter without alert rule | No L1 detection without L2 verify path | Existing `resilience_sla_alert_guard.rs` |
| Counter without Grafana panel | No visualization layer | Existing `operator_health_dashboard_guard.rs` |
| New table without DEDUP UPSERT KEYS | No uniqueness guarantee | Existing `dedup_segment_meta_guard.rs` |
| New pub fn without call site | Dead code = no L5 audit | Existing `pub-fn-wiring-guard.sh` |
| Promise "100% no disconnect" without envelope | Hallucination | Operator-charter-forever §F + reviewer agent |
| `.clone()` / `Vec::new()` on hot path | Breaks O(1) | Existing banned-pattern scanner |

---

## ⚙️ The 100% Coverage Mapping (15 dimensions × 7 layers)

Every PR must mechanically prove all 15 × 7 cells = 105 cells:

| Dimension | L1 DETECT | L2 VERIFY | L3 RECONCILE | L4 PREVENT | L5 AUDIT | L6 RECOVER | L7 COOLDOWN |
|---|---|---|---|---|---|---|---|
| Code coverage | unit test | property test | mutation test | clippy lint | llvm-cov ≥ ratcheted per-crate floor (target 100%) | re-test on fail | — |
| Audit coverage | counter | gauge | daily verify | pre-op guard | `<event>_audit` table | re-query | — |
| Testing coverage | unit | integration | property | loom | dhat | fuzz | mutation |
| Code checks | banned-pattern | secret-scan | pub-fn-test | pub-fn-wiring | plan-verify | hook block | retry |
| Code performance | DHAT zero-alloc | Criterion bench | bench-gate 5% | budget pin | profile dump | regression revert | — |
| Monitoring | Prom counter | Prom gauge | scrape verify | metric exists | metric panel | alert fires | — |
| Logging | `info!` | `warn!` | reconcile log | `error!` w/ code | hourly JSONL | summary writer | — |
| Alerting | Prom rule | for-duration | edge-trigger | suppression | Alertmanager | Telegram fire | dedup |
| Security | `Secret<T>` | secret-scan | cargo-audit | banned-pattern | audit table | TOTP regen | static IP |
| Security hardening | banned-pattern | secret-scan | `unused_must_use` | clippy deny | tracing redact | revoke token | refresh |
| Bug fixing | unit test | adversarial agent | mutation test | clippy | issue tracker | revert | re-deploy |
| Scenarios | 11+ chaos tests | failure injection | weekly fuzz | banned-pattern | spill audit | rescue ring | spill drain |
| Functionalities | pub-fn test | integration | property | call-site exists | tracing span | feature flag | rollback |
| Code review | hot-path agent | security agent | hostile agent | reviewer human | PR diff | revert | re-review |
| Extreme check | ratchet test | meta-guard | cross-ref test | guard test | retention sweep | restart | retry |

**This is the master matrix.** Every PR mechanically proves all 105 cells via existing ratchets + new ratchets per the changed code.

---

## 🤖 Per-Session Automation (every session inherits)

Every new Claude Code / Cowork session automatically inherits via SessionStart hooks (`.claude/settings.json`):

| Auto-step | Tool | Purpose |
|---|---|---|
| 1 | `mcp__tickvault-logs__run_doctor` | 7-section health snapshot |
| 2 | `mcp__tickvault-logs__summary_snapshot` | Recent ERROR signatures |
| 3 | `mcp__tickvault-logs__run_doctor` (CloudWatch alarms) | Currently-firing alerts |
| 4 | `session-auto-health.sh` | Doctor + validate-automation in background |
| 5 | `session-context-brief.sh` | Active plans + open PRs |
| 6 | `session-sanity.sh` | Branch + uncommitted check |

**If ANY step fails → fix underlying issue. NEVER bypass.**

---

## 📊 The "Common Runtime, Dynamic, Scalable, Incremental" 4-Word Test

For every new design, answer in one sentence each:

| Word | Question | Pass criteria |
|---|---|---|
| **Common** | Does the same code run on Mac dev + AWS prod? | Same `docker-compose.yml`, same config TOML, no `#[cfg(target_os)]` divergence in hot path |
| **Runtime** | Does the system adapt at runtime without redeploys? | Config hot-reload OR feature flag in TOML OR runtime-mutable state in Arc<RwLock> |
| **Dynamic** | Does the system reshape based on observed conditions? | Selector re-ranks on real-time data (e.g., dynamic depth) OR alerts adjust thresholds |
| **Scalable** | Does adding more instruments / connections / data NOT break the design? | O(1) hot path; bounded mpsc; horizontal scaling within Dhan's per-account caps |
| **Incremental** | Can it ship as small PRs without big-bang merges? | Wave A/B/C/D rollout pattern; each wave independently mergeable |

**Every PR description includes this 5-row table answered.** Mechanically checkable by the PR template.

---

## 🎬 The Auto-Driver / Insta-Reel Explanation Test

Every PR / plan / Telegram message must pass:

| Question | Pass if... |
|---|---|
| Can a 60-year-old auto-driver read it on a phone in 5 seconds? | Yes |
| Could 1 million Instagram viewers all understand the diagram? | Yes |
| Does it use a juice-shop / phone-line / bodyguard analogy? | Yes |
| Does it have ASCII boxes + arrows + tables, NOT walls of prose? | Yes |
| Does it say "Sir, imagine ..." at least once? | Yes |
| Does it use library names (rkyv/papaya/etc.) in operator-facing text? | **NO — auto-fail** |
| Does it use file paths in operator-facing text? | **NO — auto-fail** |

Failing this test = reject in review.

---

## 🔬 The Honest 100% Claim (mandatory wording)

When any PR / Telegram / commit body says "100% guarantee", it MUST be qualified per operator-charter-forever §F:

> "100% inside the tested envelope, with ratcheted regression coverage: <list specific envelope bounds with constants + ratchet test files>. Beyond the envelope, <fallback mechanism> catches every payload as recoverable text."

**Examples (good):**
- "100% inside the tested envelope: ≤80s soft-restart absorbed by 100K-tick rescue ring (constant `TICK_BUFFER_CAPACITY`, ratcheted by `zero_tick_loss_alert_guard.rs`)"
- "100% inside the tested envelope: 60s QuestDB outage absorbed by 3-tier rescue→spill→DLQ"

**Examples (rejected):**
- ❌ "100% zero ticks lost forever" — no envelope
- ❌ "WebSocket never disconnects" — physical impossibility (SEBI 24h JWT)
- ❌ "QuestDB never fails" — third-party process

---

## ⚖️ When Z+ Conflicts with Speed

The operator's charter is non-negotiable. If a PR seems "too much work for a small fix":

| Situation | What to do |
|---|---|
| 1-line bug fix in pure-function code | Skip L3/L4/L7; still do L1 (unit test) + L5 (audit via existing) + L6 (revert PR) |
| Documentation-only change | Skip all layers; still get 3-agent review pass |
| New feature touching hot path | FULL Z+ — no shortcuts |
| New WS / token / order / persist path | FULL Z+ — no shortcuts |
| Cosmetic refactor with no behavior change | Skip L2-L4; still do L1 + L5 |

**Litmus test:** if this code fails at 2 AM on the day of a circuit breaker, what catches it? If the answer is "the operator wakes up and runs `make stop && make run`" → NOT acceptable. Build the layers.

---

## 📈 Mechanical Enforcement (the gates)

| Gate | Where | What it catches |
|---|---|---|
| `per-item-guarantee-check.sh` | pre-PR + `make wave-guarantee-check` | Z+ matrix presence in plan |
| `banned-pattern-scanner.sh` | pre-commit | hot-path violations |
| `pub-fn-test-guard.sh` | pre-push | dead code |
| `pub-fn-wiring-guard.sh` | pre-push | unused exports |
| `plan-verify.sh` | pre-push | unchecked plan items |
| `error_code_tag_guard.rs` | `cargo test` | missing `code =` field |
| `error_code_rule_file_crossref.rs` | `cargo test` | rule-file mention required |
| `dedup_segment_meta_guard.rs` | `cargo test` | DEDUP key must include segment |
| `operator_health_dashboard_guard.rs` | `cargo test` | counter must have Grafana panel |
| `resilience_sla_alert_guard.rs` | `cargo test` | counter must have alert rule |
| `error_level_meta_guard.rs` | `cargo test` | flush/persist uses `error!` |
| `scripts/bench-gate.sh` | post-merge | 5% regression budget |
| `scripts/coverage-gate.sh` | post-merge | ratcheted per-crate floors (63.3–99.5, target 100%) per crate |
| **NEW:** `z-plus-checklist-guard.sh` | pre-PR | Z+ checklist filled in for every plan item |
| **NEW:** `auto-driver-test-guard.sh` | pre-PR | operator-facing text passes auto-driver test |

The two NEW gates ship in the next maintenance commit.

---

## 🎤 SUMMARY (auto-driver one-liner)

> "Sir, every line of code in tickvault is a VVIP. We give it Z+ security — 7 layers, no single point of failure, audit trail, alarm, Telegram alert, dashboard, recovery plan, and a written promise that says EXACTLY what we can guarantee and EXACTLY what we can't. No lies. No hallucination. If the impossible happens, layer 6 still catches it. This is the standard. Every PR. Every task. Forever."

---

## Trigger (auto-loaded paths)

Always loaded. Activates on any session editing:
- Any file under `crates/`
- Any file under `.claude/plans/`
- Any file under `.claude/rules/project/`
- Any file under `deploy/`
- Any file containing `pub fn` or `error!` or `warn!` or `Severity::`
