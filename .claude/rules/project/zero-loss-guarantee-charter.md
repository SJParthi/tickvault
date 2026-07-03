# Zero-Loss Guarantee Charter — checked EVERY PR / sub-PR / task / sub-task / session (FOREVER)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` > this file > defaults.
> **Scope:** PERMANENT. Applies to **every PR, every sub-PR, every task, every
> sub-task, every current Claude Code session, every new Claude Code session,
> every Claude Cowork task, and every GitHub action** in this repo.
> **Operator demand (2026-05-29, verbatim intent):** *"make it as guarantee
> document check for every pr every sub pr every task every sub task every
> current claude code session and every new claude code session github as
> well."*
> **Auto-load:** this file lives under `.claude/rules/project/`, so it is in
> the auto-load set for every session. **Read it before any work.**
> **Companion enforcement:** `.github/pull_request_template.md` (the per-PR
> checklist that references this charter), `per-wave-guarantee-matrix.md`,
> `operator-charter-forever.md` §C/§F, `z-plus-defense-doctrine.md`.

---

## §0. How this document is USED (the check, not just the wish)

This is a **checklist contract**, not an aspiration. The mechanical reality:

| When | What is checked | By what |
|---|---|---|
| Every new/current session START | this charter is auto-loaded; session runs `run_doctor` + `summary_snapshot` (CloudWatch alarms surfaced via `run_doctor`) | `.claude/settings.json` SessionStart hooks |
| Every PR / sub-PR | the §3 GUARANTEE CHECK block is filled in the PR body (every box ticked **or** marked `N/A — <reason>`) | `.github/pull_request_template.md` + reviewer |
| Every task / sub-task / plan item | the §3 block is carried in the plan item (or cross-referenced) | `per-item-guarantee-check.sh` + `plan-verify.sh` |
| Every commit | banned-pattern + secret + pub-fn + fmt + the 12 pre-push gates | git hooks (see `operator-charter-forever.md` enforcement chain) |
| GitHub (CI) | full 22-test battery + Build & Verify + Security & Audit + Commit Lint + Secret Scan | `.github/workflows/*.yml` |

**Rule:** no box may be silently skipped. A box that genuinely does not apply
is written `N/A — <one-line reason>`. A box with no evidence and no N/A reason
**blocks the PR in review.**

---

## §1. The honest-envelope preamble (NO HALLUCINATION — read FIRST)

The operator's own O(1) and "never" clarifications are binding: literal
"never disconnect / never fail / strict O(1) forever" is physically
impossible for a system bound to a third-party WebSocket, a 24h SEBI JWT, a
remote DB, and an OS scheduler. **Every guarantee in this charter is stated
as a BOUNDED, EVIDENCE-BACKED envelope.** Claiming the unbounded literal is a
hallucination and is REJECTED in review.

| Operator "absolute" | Honest, enforced envelope | Evidence required |
|---|---|---|
| Zero tick loss | Bounded zero loss inside the chaos envelope: ring → spill NDJSON → DLQ; beyond it, DLQ catches every payload as recoverable text | `zero_tick_loss_alert_guard.rs`, chaos tests |
| WS never disconnects | DETECT ≤5s, reconnect with `SubscribeRxGuard`, sleep-until-open post-close; SEBI 24h JWT forces ≥1 reconnect/day by law | pool watchdog + `ws_sleep_resilience` tests |
| Never slow / hanged | Hot path: DHAT ≤ budget alloc, Criterion p99 ≤ budget; tick-gap >30s → Telegram | DHAT tests + Criterion + bench-gate |
| QuestDB never fails | ABSORB via 3-tier rescue→spill→DLQ + schema self-heal; alert if disconnected >30s | chaos_questdb tests |
| Strict O(1) everywhere | **O(1) per-tick + per-lookup (proven)**; inherently-O(N) build steps are flagged honestly, never faked as O(1) | DHAT + Criterion + explicit N flag |
| 100% (every dimension) | 100% **inside the tested envelope with ratcheted regression coverage** | the §2 matrix evidence |

**Mandated wording when a PR/commit/Telegram says "100%" or "guarantee":** use
the qualifier in `operator-charter-forever.md` §F verbatim. Unbounded "never"
without the envelope = REJECT.

---

## §2. The 100% coverage matrix — every dimension, with PROOF

Every PR proves each row with a real artefact (not a claim). `N/A — reason`
allowed where genuinely inapplicable.

| Dimension | Proof artefact (real, no hallucination) |
|---|---|
| Code coverage | `quality/crate-coverage-thresholds.toml` (ratcheted per-crate floors (63.3–99.5, target 100%), floors only move up), `scripts/coverage-gate.sh` post-merge |
| Audit coverage | `<event>_audit` QuestDB table with DEDUP UPSERT KEYS |
| Testing coverage | the 22 test categories in `testing.md`, scoped to changed crate(s) |
| Security coverage | banned-pattern + secret-scan + `Secret<T>` + security-reviewer agent |
| Monitoring coverage | Prom counter + gauge + tracing span (7-layer telemetry) |
| Logging coverage | `error!`/`warn!`/`info!` with `code = ErrorCode::X.code_str()`; hourly `errors.jsonl` |
| Alerting coverage | Prom/CloudWatch alert rule + `resilience_sla_alert_guard.rs` ratchet |
| Scenario coverage | chaos test for any new failure mode |
| Edge-case coverage | property tests (proptest) + boundary tests on financial fns |
| Runtime validation | `make doctor` 7-section + market-open self-test + SLO score @10s |
| Failure detection | typed `ErrorCode` + runbook + triage YAML rule |
| Bug detection | adversarial 3-agent review (hot-path + security + hostile) |
| Performance validation | DHAT zero-alloc + Criterion p99 + bench-gate ≤5% regression |
| Recovery validation | chaos rescue→spill→DLQ + reconnect/sleep-wake tests |
| API validation | request/response schema tests against the Dhan ref rules |
| DB validation | DEDUP-key meta-guard + schema self-heal (`ALTER ADD COLUMN IF NOT EXISTS`) |

**Mechanical truth:** anything weaker than a ratchet test that **fails the
build on regression** is not "100%" — it is a wish. Wishes are rejected.

---

## §3. THE GUARANTEE CHECK — paste into every PR / task / plan item

Copy this block into the PR body (it is already embedded in the PR template)
and every plan item. Tick each box or write `N/A — <reason>`:

```
## Zero-Loss Guarantee Charter check (.claude/rules/project/zero-loss-guarantee-charter.md)

### Coverage (§2) — proof, not claim
- [ ] Code coverage: tests added; coverage delta ≥ 0
- [ ] Audit coverage: <event>_audit table + DEDUP keys (or N/A)
- [ ] Testing coverage: which of the 22 categories apply, named
- [ ] Security: secret-scan + Secret<T> + security-reviewer pass (or N/A)
- [ ] Monitoring: Prom counter/gauge + tracing span (or N/A)
- [ ] Logging: error! carries code = ErrorCode::X.code_str()
- [ ] Alerting: alert rule added for new failure mode (or N/A)
- [ ] Scenarios/edge: chaos + property test for new failure mode (or N/A)
- [ ] Performance: DHAT + Criterion if hot path; ≤5% regression (or N/A — not hot path)
- [ ] Recovery: rescue→spill→DLQ / reconnect path intact
- [ ] Functionalities: every new pub fn has a test AND a call site
- [ ] Extreme check: a ratchet test fails the build on regression

### Zero-loss / resilience (§1 envelope) — honest, bounded
- [ ] No new tick-drop path; ring→spill→DLQ unchanged
- [ ] WS SubscribeRxGuard + pool watchdog unchanged
- [ ] No new hot-path allocation (DHAT clean) — O(1) per-tick/per-lookup
- [ ] Any inherently-O(N) step is FLAGGED as O(N), not claimed O(1)
- [ ] QuestDB schema self-heal preserved
- [ ] Composite (security_id, exchange_segment) uniqueness + DEDUP keys

### Evidence (§4 — NO HALLUCINATION)
- [ ] Real test output pasted (counts), not "should pass"
- [ ] Real bench/DHAT numbers if hot path
- [ ] "100%"/"guarantee" wording carries the §1 envelope qualifier

### Review
- [ ] Adversarial 3-agent review (hot-path + security + hostile) BEFORE and AFTER
```

For a tiny/docs-only change, most boxes are `N/A — docs only`; the review +
evidence boxes still apply (per `z-plus-defense-doctrine.md` "when Z+
conflicts with speed").

---

## §4. Evidence discipline (the no-hallucination law)

Every claim in a PR/commit/Telegram MUST be backed by a **real** artefact —
the operator's "no hallucination" rule made mechanical:

- "tests pass" → paste the `test result: ok. N passed` line, with N.
- "fast / O(1)" → paste Criterion / DHAT numbers, name the budget.
- "alerts fire" → name the Prom rule + the CloudWatch alarm state (via `run_doctor`).
- "audited" → name the `<event>_audit` table + a `questdb_sql` count.
- "recovers" → name the chaos test that exercises the path.

If the evidence does not exist yet, say so plainly ("not yet measured") — do
NOT assert the outcome. A stated-but-unmeasured outcome is a hallucination and
is rejected.

---

## §5. Real-time pipeline + observability (the always-on stack)

```
Market feed → WebSocket → tick validation → dedup → sequence check →
bounded queue → processing → QuestDB persist → metrics → Telegram alert →
audit table → auto recovery / replay
```

Every stage has: a Prom counter, a tracing span, an `error!`-with-code on
failure, a Telegram event for operator-actionable conditions, and an audit
row where the event is SEBI-relevant. Telegram alert classes (bounded,
edge-triggered, market-hours-gated): WS disconnect, tick gap/delay, queue
buildup, high latency, DB slow/disconnect, memory/CPU spike, retry storm,
packet/tick loss, duplicate, deadlock, failover, security anomaly.

---

## §6. Auto-driver explanation (Insta-reel)

> Sir, imagine a super-fast airport where millions of vehicles arrive every
> second. The rule: **not one vehicle lost, no jam, no signal failure, every
> camera recording, every problem instantly rings an alarm, and a backup
> system switches on the moment anything wobbles.** This document is the
> airport's safety manual — and before every single change to the airport,
> the engineer must tick the safety checklist and SHOW the camera footage
> (real evidence), not just say "trust me, it's fine." If he can't show the
> footage, the change does not open.

---

## §7. Priority order (when two demands collide)

**Reliability > Correctness > Observability > Recoverability > Performance >
Scalability.** A change that makes things faster but less reliable is
rejected. A change that adds a feature but removes an audit row is rejected.

---

## §8. Trigger (auto-loaded paths)

Always loaded. Reinforced on any session editing `.claude/rules/`,
`.claude/plans/`, `.github/`, `crates/`, or any file containing `error!`,
`Severity::`, `NotificationEvent`, or `DEDUP`.
