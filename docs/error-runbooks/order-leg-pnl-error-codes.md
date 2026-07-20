---
paths:
  - "crates/storage/src/order_leg_pnl_persistence.rs"
  - "crates/app/src/order_leg_pnl_boot.rs"
  - "crates/app/src/order_runtime.rs"
  - "crates/app/src/groww_cadence_executor.rs"
  - "crates/common/src/error_code.rs"
  - "config/base.toml"
---

# 🔷 Per-Leg Option P&L Capture — Error Codes (ORDER-PNL-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `dhan-rest-only-noise-lock-2026-07-14.md` (the Telegram posture this
> subsystem complies with — ZERO new pages) >
> `order-update-events-error-codes.md` (the sibling forensic-capture
> pattern this subsystem mirrors) > this file.
> **Design record (2026-07-19):** the order-leg-pnl plan
> (`.claude/plans/active-plan-order-leg-pnl.md`) — per-leg
> realized/unrealized P&L for OPTION CONTRACT legs in the dry-run order
> runtime, built on the PR #1649 option-contract mark tap.
> **Companion code:** `crates/trading/src/risk/types.rs`
> (`PositionInfo::unrealized_at` — the ONE unrealized formula; the
> risk-engine total delegates to it), `crates/app/src/order_runtime.rs`
> (the two emission seams: post-fill + post-mark),
> `crates/app/src/groww_cadence_executor.rs` (the once-per-day option-leg
> identity index), `crates/storage/src/order_leg_pnl_persistence.rs` (the
> ILP-over-HTTP writer), `crates/app/src/order_leg_pnl_boot.rs` (the
> supervised consumer),
> `crates/common/src/error_code.rs::ErrorCode::OrderPnl01PersistFailed`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `OrderPnl01*` variant verbatim —
> `ORDER-PNL-01` and `OrderPnl01PersistFailed` appear below; the
> variant's `runbook_path()` points at THIS file (test
> `every_runbook_path_exists_on_disk`).

## §0. Why this code exists (the capture gap)

The dry-run order runtime prices OPTION CONTRACT legs off the PR #1649
mark tap, but persisted NO per-leg P&L row — realized/unrealized per leg
existed only as in-RAM aggregates. The NEW DAY-partitioned QuestDB table
**`order_leg_pnl`** closes that: one row per applied paper FILL
(`event_kind='fill'`; its `mark_price` is the fill's own average price)
and one row per accepted option MARK against a held or pending leg
(`event_kind='mark'`), each carrying net_lots / lot_size /
avg_entry_price / mark_price / realized_pnl (a cumulative-DAY snapshot) /
unrealized_pnl (computed at the emit seam via the ONE
`PositionInfo::unrealized_at` formula — never the O(N) engine total).

**DEDUP UPSERT KEYS `ts, trading_date_ist, feed, security_id, segment,
event_seq`** — `feed` IS in the key (the 2026-06-28 every-table rule) and
`segment` sits beside `security_id` per I-P1-11. `event_seq` is a
process-global monotonic receipt sequence stamped ONCE, CONSUMER-side,
via the shared `broker_order_events::next_event_seq` — the event struct
carries NO seq and producer-side stamping is FORBIDDEN (binding pin;
same-second bursts survive as distinct rows while an ILP retry of the
identical row collapses idempotently).

The whole subsystem is COLD-PATH, best-effort forensics: the emit seams
hand a ~72-byte `Copy` `LegPnlEvent` to a bounded `try_send` (never
blocking the runtime); the RiskEngine, the OMS, the `order_audit` path,
and the PR #1649 MarkForwarder tap are UNTOUCHED by construction
(`dhat_mark_forward.rs` byte-identical). Only FNO segments emit
(NSE_FNO=2 / BSE_FNO=8) — IDX_I never. Dry-run paper mode only; no
live-order behavior change.

## §1. ORDER-PNL-01 — per-leg P&L persistence degraded

`ErrorCode::OrderPnl01PersistFailed` (`code_str() == "ORDER-PNL-01"`).
**Severity:** High. **Auto-triage safe:** YES (the degrade already
happened; DEDUP-idempotent re-appends and the next event self-heal — the
operator inspects, nothing is auto-re-emitted).
**Delivery:** log-sink-only.

**Trigger:** distinguished by the `stage` field:

| stage | Meaning |
|---|---|
| `ensure_client_build` | the boot-time ensure-DDL reqwest client could not be built (host fd/TLS/resolver pressure); DDL skipped this boot |
| `ensure_ddl` | the `CREATE TABLE` / `ALTER ADD COLUMN` / `DEDUP ENABLE` `/exec` leg returned non-2xx or was unreachable — NOTE the duplicate-row window: the first ILP write may auto-create the table WITHOUT DEDUP UPSERT KEYS until a later boot's ensure succeeds |
| `append` | an ILP buffer append was rejected — the row is skipped, the loop continues |
| `flush` | the once-per-drain-batch ILP-over-HTTP flush was refused by the per-request server ACK — pending rows DISCARDED (`discard_pending`, the poisoned-buffer defense; counted) |
| `sink_drop` | the producer's bounded-sink `try_send` was refused (full/closed) — the event's row is LOST, counted per event; the coded `error!` is EDGE-LATCHED per episode (the first drop is loud, subsequent drops are counter-only, a successful publish re-arms); the runtime is never blocked |

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `ORDER-PNL-01`; the `stage`
   field names the failing leg.
2. `make doctor` — cross-check QuestDB health (BOOT-01/BOOT-02 class);
   the sibling audit writers share the target.
3. Counter rates: `tv_order_leg_pnl_persist_errors_total{stage}` non-zero
   → QuestDB ILP/HTTP degraded; sustained
   `tv_order_leg_pnl_dropped_total{reason="full"}` → the consumer is
   stalled behind a slow QuestDB (forensic rows lost by design — the
   trading-decision path is unaffected).
4. Verify recovery: `mcp__tickvault-logs__questdb_sql "select count(*)
   from order_leg_pnl where ts > dateadd('h', -1, now())"` — rows land
   again once marks/fills flow.

**Counters (static labels only):**
`tv_order_leg_pnl_rows_total{event_kind}` ·
`tv_order_leg_pnl_persist_errors_total{stage}` ·
`tv_order_leg_pnl_dropped_total{reason}` ·
`tv_order_leg_pnl_rows_discarded_total` ·
`tv_order_leg_pnl_nonfinite_clamped_total` ·
`tv_order_leg_pnl_identity_unresolved_total` ·
`tv_order_leg_pnl_task_respawn_total{reason}`.

**Honest envelope:** best-effort forensics — a persist outage loses
capture rows for the outage window ONLY; it never affects the risk
engine, the paper OMS, the order_audit path, or any trading decision.
`realized_pnl` is the risk engine's cumulative-DAY snapshot (its 16:00
IST daily reset is the row-stream discontinuity; `trading_date_ist`
separates days — no synthetic 15:30 close row is fabricated). A fill
row's `mark_price` is the fill's own average price, not a market mark.
Identity columns resolve from the once-per-day option-leg index built
beside the cadence executor's contract-mark index — rows emitted BEFORE
the day's first index publish persist honest sentinels
(`underlying="n/a"`, `strike_paise=-1`, `option_type="n/a"`), counted via
`tv_order_leg_pnl_identity_unresolved_total`; later rows self-heal.
Emission requires BOTH gates (`[order_runtime] enabled` AND
`[order_leg_pnl] enabled`), resolved ONCE at spawn. Release-build panics
abort the process (`panic = "abort"`) — the consumer respawn arms
self-heal in unwind (dev/test) builds only. Paper mode; `dry_run` stays
hard-true.

**Delivery boundary (honest — no false-OK):** ORDER-PNL-01 is
**log-sink-only**: NO `error_code_alerts` map entry in
`deploy/aws/terraform/error-code-alarms.tf`, NO terraform alarm, and NO
mention in `observability-architecture.md`'s paging list (the paging
drift guard sees no drift); ZERO new Telegram events (the Dhan noise-lock
posture). The coded `error!` lines + counters are the operator surface;
adding a CloudWatch log-filter alarm is a flagged follow-up (one map
entry + a cost note — the SCOREBOARD-01 precedent).

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/storage/src/order_leg_pnl_persistence.rs`
- `crates/app/src/order_leg_pnl_boot.rs`
- `crates/app/src/order_runtime.rs` (the emit seams)
- `crates/app/src/groww_cadence_executor.rs` (the identity index)
- `crates/common/src/error_code.rs` (any `OrderPnl01*` variant)
- `crates/common/src/config.rs` (`OrderLegPnlConfig`) or
  `config/base.toml` `[order_leg_pnl]`
- Any file containing `ORDER-PNL-01`, `OrderPnl01PersistFailed`,
  `LegPnlEvent`, `order_leg_pnl`, or `tv_order_leg_pnl_`
