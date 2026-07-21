# Implementation Plan: TF Reshape — Second-Scale Frame Set + Broker-Pulled 1d

**Status:** IN_PROGRESS
**Date:** 2026-07-21
**Approved by:** Parthiban (operator) — standing zero-touch pre-authorization (operator-charter governance: a plan for operator-ordered work is APPROVED on existence); frame set per the operator's 2026-07-21 directive quoted below.

## Operator frame-set directive (2026-07-21 — verbatim, typos preserved)

> "even our tiemframe will be liek thsi ddue whcu is 1 seocnd till 15 seocnd sequantially one by oen and 30 second and 1m, 3m, 5m, 15m alone dude okay?"

## Frame contract (locked for PR #1696)

- **IN — 21 frames:** 1s, 2s, 3s, 4s, 5s, 6s, 7s, 8s, 9s, 10s, 11s, 12s, 13s, 14s, 15s (15 second-frames) + 30s + 1m, 3m, 5m, 15m + 1d (broker-pulled per the 2026-07-20 1d-direct spec).
- **OUT — write-stop only:** 2m, 4m, 6m–14m, 30m, 1h, 2h, 3h, 4h. Their QuestDB tables and stored history are KEPT: they are NEVER added to `RETIRED_QUESTDB_TABLES` or any boot-DROP union (the standing write-stop invariant).
- **COORDINATOR ASSUMPTION 1 (pending operator veto — isolated in commit C2):** the replace-reading of "alone": 2m/4m/30m/1h/2h/3h/4h live aggregation retires. A veto costs one revert of C2.
- **COORDINATOR ASSUMPTION 2 (pending operator veto — isolated in commit C4 + the single 1d membership entry):** 1d is retained as broker-pulled (the operator's quote does not name 1d). A veto costs one revert of C4 plus dropping the 1d membership entry.
- The 6m–14m retirement (both specs agree) already landed in commit 320be7c4 (the preserved snapshot).

## Execution reality (honest envelope)

- Second-frames source from the **GDF 1s feed**, which is being designed/built in a parallel session (that session specs the interface). They land STRUCTURALLY in this PR but are FEED-GATED: **zero rows flow until the GDF 1s feed goes live.** The PR body states this plainly.
- Minute-frames 1m/3m/5m/15m keep working TODAY from the REST cadence (the 1m source is unchanged by this PR).
- 1d fills from the broker daily pull, not from live aggregation.

## Plan Items (commit ladder C1–C5; push per commit; drive PR #1696 to READY, never merge — the operator merges post-freeze)

- [x] C1 — re-scope this plan to the new frame contract (docs-only)
  - Files: .claude/plans/active-plan-tf-diet-11-broker-1d.md
  - Tests: n/a (docs-only)

- [x] C2 — retirement widening: write-stop 2m, 4m, 30m, 1h, 2h, 3h, 4h; minute-side membership becomes 1m, 3m, 5m, 15m, 1d (TF_COUNT 12 → 5 at this commit)
  - Files: crates/trading/src/candles/tf_index.rs, crates/trading/src/in_mem/spot_bar_store.rs, crates/app/src/tf_consistency_boot.rs, crates/storage/src/partition_manager.rs, crates/storage/src/shadow_persistence.rs, crates/storage/src/shadow_seal_columns.rs, crates/storage/tests/partition_retention_coverage_guard.rs
  - Tests: partition_retention_coverage_guard bare-literal count re-pin (12 → 5), tf_index membership unit tests, scoped trading/storage/app suites green

- [ ] C3 — second-scale structural: TfIndex gains 1s–15s + 30s variants; bucket math generalized to second-scale; candle DDL + seal/spill format extended; every second-frame FEED-GATED (zero rows until the GDF 1s feed is live) (TF_COUNT 5 → 21)
  - Files: crates/trading/src/candles/tf_index.rs, the candle aggregator/seal path under crates/trading/src/candles/, crates/storage persistence + DDL modules, crates/app/src/candle_ddl_boot.rs
  - Tests: second-scale bucket-boundary unit tests + proptest, TF_COUNT/membership ratchets, DDL guard updates, a feed-gate zero-row test

- [ ] C4 — broker-pulled 1d per the 2026-07-20 spec (assumption-isolated)
  - Files: the 1d pull modules in crates/app (cadence lane) + crates/storage persistence
  - Tests: 1d pull unit/integration tests + dedup-key guard coverage

- [ ] C5 — ratchet/guard/string sweep + PR body: residual "21 TF" prose (rest_candle_fold.rs, metrics_catalog.rs, subsystem_memory.rs, lib.rs, main.rs "all 21 timeframes" string, candle_ddl_boot.rs, d2_stage2_hoist_guard.rs, ensure_ddl_boot_wiring_guard.rs); PR body rewritten with the full gate evidence + the feed-gated zero-rows statement; flip #1696 to READY
  - Files: the prose/guard files above; the PR body
  - Tests: full scoped suites re-run with pasted evidence; guard self-tests

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | GDF 1s feed not yet live (today) | Second-frame tables exist with ZERO rows; minute-frames 1m/3m/5m/15m fill from REST; no errors, no pages |
| 2 | Operator vetoes the replace-reading | Revert C2 alone; retired minute frames resume aggregation |
| 3 | Operator vetoes 1d retention | Revert C4 + drop the 1d membership entry |
| 4 | Boot on the existing prod DB | Retired tables untouched (write-stop); new second-frame DDL created via the CREATE/ALTER IF NOT EXISTS self-heal |

## Design

TfIndex is the single frame-membership authority (TF_COUNT + the frame enum + bucket math). C2 shrinks live minute membership to the surviving four + 1d using the exact retirement machinery proven in commit 320be7c4 (write-stop only: aggregation and writes cease; tables, history, and reads are untouched). C3 generalizes the bucket index from minute-scale to second-scale (bucket width expressed in seconds; 1m stays a 60-second bucket), adds the 16 second-frames to the enum, extends the candle DDL and seal/spill column sets, and wraps every second-frame write behind a feed-gate keyed on the GDF 1s feed's presence so the structure ships dark. C4 wires the broker daily pull as the sole 1d source. C5 sweeps the numeric ratchets and prose so no guard still pins the old 21/12 counts.

## Edge Cases

- Second-scale bucket boundaries: bucket(ts) for 1s..15s/30s must floor on second boundaries; the 09:15:00 IST open lands in the first bucket of each frame; partial buckets at open/close seal per the existing seal law.
- 30s vs 30m: 30m retires while 30s arrives — membership tables and prose must never conflate them.
- The bare-literal count pin in partition_retention_coverage_guard.rs (~lines 102/119) is invisible to the `; N]` sweep — every count re-cut includes it (12 → 5 at C2, 5 → 21 at C3).
- Retired-table reads (dashboards, backfills) keep working — write-stop only.
- Feed-gate OFF is a NORMAL state (zero rows, no error, no page), not a degrade.

## Failure Modes

- The GDF 1s feed slips or never launches → second-frames stay structurally present with zero rows; harmless by design; no alarm (stated in the PR body).
- The operator vetoes assumption 1 or 2 → single-commit reverts (C2 / C4) by construction.
- DDL drift on an existing DB → the boot DDL self-heal (CREATE/ALTER IF NOT EXISTS) creates the new second-frame tables; retired tables are never dropped.
- A missed count pin (a guard still expecting the old TF_COUNT) → the build-failing ratchet catches it at the C2/C3 gates — that is the ratchet working; the pin is fixed in the same commit.

## Test Plan

Per-crate scoped suites (trading, storage, app) at each commit; second-scale bucket-boundary unit tests plus a proptest over arbitrary in-session timestamps at C3; ratchet re-pins (TF_COUNT, membership, the bare-literal guard) in the same commit as each count change; a feed-gate test asserting zero second-frame writes while the gate is off; full evidence (test counts, fmt, clippy) pasted into the PR body at C5.

## Rollback

Each commit is independently revertable (the assumption isolation is designed for exactly that). Write-stop-only retirement means no data is lost in either direction — re-enabling a frame is a membership re-add, and retired history was never dropped. PR #1696 stays draft until READY; nothing merges before the operator's own post-freeze merge.

## Observability

Existing tv_* candle metrics keep reporting for surviving frames; the feed-gate state (GDF 1s live or not) is logged once at boot; no new Telegram surface (the noise-lock is unchanged); retired frames simply stop appearing in write-path logs while their tables remain queryable.

## Per-item guarantee matrix

Cross-reference: every C-item above carries the 15-row + 7-row guarantee matrices of `.claude/rules/project/per-wave-guarantee-matrix.md` by reference, enforced mechanically by `per-item-guarantee-check.sh`.
