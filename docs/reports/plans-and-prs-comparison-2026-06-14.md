# Tickvault — Finished / Current / Future Plans & PRs (Comparison View)

> **Generated:** 2026-06-14 by Claude Code session on branch `claude/upbeat-hypatia-6trzk3`.
> **Evidence basis:** live `git log`, `.claude/plans/` files, GitHub open-PR query. No assumptions — every row traces to a real artefact.
> **Audience test:** readable by a non-technical stakeholder in one pass (auto-driver / Insta-reel standard per operator charter §G).

---

## 0. One-glance status (the honest snapshot)

| Question | Real answer (evidence) |
|---|---|
| Any open PR to merge right now? | **NO.** GitHub `list_pull_requests state=open` → `[]`. Last merged = **#1127**. |
| Is the working branch clean? | **YES.** `git status` empty; `claude/upbeat-hypatia-6trzk3` == `origin`. |
| Does the workspace compile? | **YES — `cargo check --workspace --all-targets` → exit 0** in 1m37s (1 harmless `dead_code` warning, no errors). Real evidence, this session. |
| What is the active plan? | `active-plan-deletion-audit.md` — **Phase A** (docs/rules cleanup), Status APPROVED. |
| Total PRs merged to date | **#1127** is the highest PR number in history. |

**Plain words:** The shop is tidy. Nothing is half-finished waiting to be merged. There is one approved cleanup job in progress (deleting dead code/docs), and a stack of bigger future jobs designed but not yet built.

---

## 1. How the product evolved (why the plans look layered)

The plans contradict each other across time because the scope was **deliberately shrunk, then re-grown**. Read top-to-bottom = newest wins.

| Phase | Date | What changed | Net effect |
|---|---|---|---|
| Full F&O system | Apr 2026 | ~24,000 instruments, depth-20/200, movers, greeks, Grafana+Prometheus+Loki+Jaeger+Valkey | Big, complex |
| AWS-lifecycle narrowing | 19 May (#2–#7b) | Collapsed to **4 index SIDs only**, deleted depth/movers/greeks/Phase-2 | Tiny, simple |
| CloudWatch-only | 20 May+ (#O1–#O4) | Removed Grafana, Prometheus, Alertmanager, Valkey | Runtime = QuestDB + app + CloudWatch |
| Daily-universe expansion | 27 May | Re-grow main feed to **~250 SIDs** (all NSE indices + 1 BSE SENSEX + F&O underlyings), Quote mode, `instrument_lifecycle` table, infinite-retry fetch, host → m8g.large 8 GiB | Mid-size, durable |
| NTM expansion | 06 Jun | Add NIFTY Total Market index + **~750 constituent stocks** → **~1,000 live SIDs**, cap 400→1200 | Broad, still 2 WebSockets |
| Current focus | Jun | Observability hardening + dead-code deletion | Polish + safety |

**Unchanged forever:** exactly **2 WebSocket connections** (1 main-feed + 1 order-update); **Quote mode**; composite `(security_id, exchange_segment)` uniqueness; `instrument_lifecycle` never deleted (SEBI).

---

## 2. FINISHED — recent merged PRs (the last ~50, grouped)

| Theme | Representative PRs | What it delivered |
|---|---|---|
| **CloudWatch log/metric access** | #1124, #1120, #1114, #1097-area, #1100s | Read prod logs from any Claude session (SigV4 reader, operator portal, no aws CLI); fix log shipping for `/tickvault/prod/app` |
| **WebSocket audit & classification** | #1111, #1112, #1113, classifier PR | Full disconnect/reconnect CloudWatch logging + JWT redaction; `ws_event_audit` table (future-proof to 16 conns); disconnect-cause classifier (AWS/network/Dhan) |
| **Alert wiring (fill the gaps)** | #1115, #1116, #1117, #1118, QuestDbReconnected, InstrumentBuildFailed | Wire missing safety-ping/recovery/boot-failure alerts; retire redundant alerts |
| **Zero-tick-loss / conservation** | #1090, #1091, TICK-CONSERVE-01 | Per-tick latency hunt, windowed p50/p99 dashboards, daily end-to-end tick-conservation audit |
| **Cross-verify visibility** | #1097 cross-verify card, 15:31 Telegram summary + API endpoint | Make the daily 1-minute cross-verify operator-visible |
| **REST health** | DHAN-REST-400 (#f3bf668), REST canary | Capture REST error body/URL, BLIND guard, 09:05/12:00/15:25 canary |
| **Orphan-position safety** | #1123 | 15:25 IST watchdog — refuses to carry overnight option positions |
| **Deletion audit (cleanup)** | #1122 (Phase C deps), Phase B batches (#4087b49, #9e0f457, #a8fb92f), #1121/#1127 (archive) | Drop 9 dead deps; delete depth-era telemetry + NotificationEvent variants; archive merged plans |
| **AWS ops self-heal** | #bff2514 | 16:45 stop-check self-heals a failed 16:30 auto-stop |

**Plain words:** The last two months of merged work = "make every failure visible, prove no tick is lost, and delete the code we no longer use."

---

## 3. CURRENT — the one active plan

| Item | Detail |
|---|---|
| File | `.claude/plans/active-plan-deletion-audit.md` |
| Status | **APPROVED** (Phase A — docs/rules cleanup only) |
| Goal | Remove dead code/deps/tests/rules/docs left over from the scope-shrink |
| Size of prize | ~35–55K of ~214K Rust LoC deletable (~16–26%); ~17 dead deps; ~80 test files; ~40–60 docs |
| Phase A (now) | Whole-file deletions of dead Dhan rules + superseded docs + plan archiving. **No `crates/` edits this session.** |
| Phase B (future) | In-crate module deletes (collapse 21→2 timeframes, drop trading/strategy/indicator/risk ~25K LoC, slim observability) — each its own PR + 3-agent review |
| Phase C (partly done) | Drop dead workspace deps (#1122 already removed 9) + workspace-member consolidation |
| Risk control | One mergeable PR at a time; `git revert` restores any deleted module verbatim |

---

## 4. FUTURE — designed but not yet built (with honesty flags)

| Plan | File | Status | Honest note |
|---|---|---|---|
| **Daily-universe + NTM build-out** | `daily-universe-scope-expansion-2026-05-27.md` (rule), §31/§31.1 | Contract LOCKED; sub-PRs partly landed (#0,#1,#1.5,#2 deps + stubs) | The **biggest live direction**: fetch CSV → ~1,000-SID universe → ISIN-mapped NTM constituents. Several sub-PRs (#3 downloader, #4 parser/resolver, #5 assembly, #10 orchestrator) still to ship behind the `daily_universe_fetcher` feature flag |
| **AWS indices-only production deploy** | `aws-lifecycle/THE-FINAL-PLAN.md` | LOCKED 2026-05-18, 14 numbered PRs | **Partly superseded** by the daily-universe expansion (its "4 SIDs only" universe grew to ~1,000). Cross-verify/CloudWatch/schedule parts still valid |
| **Autonomous operations 100%** | `autonomous-operations-100pct.md` | M1–M3 done/closed; **M4 + M5 not built** | M4 = post-fix verify + auto-rollback; M5 = AWS autoscale + nightly chaos. Deferred until AWS instance is provisioned |
| **Phase 0 → Phase 1 (go live)** | `friday-may-15-mega/topic-PHASE-0-LEAN-LOCKED.md` | Phase 0 mostly done; **dry_run flip = future** | Strategy/risk/OMS wiring + `dry_run=false` is the real "start trading" milestone. Operator boundary: indicators/strategies OFF-LIMITS until re-approved |
| **Movers 22-TF v3** | `v2-architecture.md`, `v2-phases.md`, `v2-ratchets.md`, `v2-risks.md` | DRAFT | **Effectively DEAD** — the movers + depth subsystems these plans target were deleted in the AWS-lifecycle narrowing. Kept only as historical design |
| **CloudWatch-only migration tail** | `aws-lifecycle/` + observability rule | In-progress | Loki/Alloy re-add (Phase 3) + AWS CloudWatch parity (Phase 4) deferred until the prod instance is provisioned |

---

## 5. Comparison: BEFORE vs NOW (stakeholder view)

| Dimension | Early (Apr) | Now (Jun) | Direction |
|---|---|---|---|
| Live instruments | ~24,000 | 4 → re-growing to ~1,000 (NTM) | Right-sized to the strategy |
| WebSocket connections | many (5 feed + depth pools) | **2** (locked forever) | Far simpler, cheaper |
| Observability stack | Grafana+Prometheus+Loki+Jaeger+Valkey | **QuestDB + app + CloudWatch** | One sink, lower cost |
| Monthly AWS cost | (large, unprovisioned) | **~₹2,058/mo all-in** (m8g.large, 270 hrs) | Predictable, ~target |
| Code size | ~214K Rust LoC | shrinking (~16–26% deletable) | Leaner, faster builds |
| Audit/observability | partial | every WS event + tick conservation + cross-verify + REST canary tracked | Production-grade visibility |
| Trading | dry-run, 0 strategies wired | still dry-run | Go-live is a deliberate future gate |

---

## 6. Honest envelope (no hallucination)

- "Zero tick loss" = **bounded** zero loss inside the rescue-ring → spill → DLQ envelope, plus the 15:31 cross-verify + tick-conservation audit that *prove* it daily. Not an unbounded literal.
- "Never disconnects" is impossible (SEBI 24h JWT forces ≥1 reconnect/day). Real guarantee: detect ≤5s, reconnect preserving subscriptions, sleep-until-open after close.
- The "future" plans above are **designs**, not shipped code. Where a plan is superseded or dead, it is flagged as such above rather than presented as live.

---

## 7. Recommended next action

There is **no open PR to merge** (the explicit "monitor until merged" instruction has no current target). The cleanest forward path is one of:
1. Execute **deletion-audit Phase A** (the approved active plan) as one PR, monitored to merge — lowest risk, immediate value.
2. Continue the **daily-universe / NTM** sub-PR sequence (#3 downloader → #4 parser/resolver) — the biggest live direction.
3. Build **autonomous-ops M4/M5** — only sensible once the AWS prod instance is provisioned.

Pick one; this session will run it through the full 10-step PR-completion loop, monitored to merge.
