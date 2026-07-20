---
paths:
  - "crates/common/src/error_code.rs"
  - "crates/app/src/tick_conservation_boot.rs"
  - "crates/storage/src/tick_conservation_audit_persistence.rs"
---

# Daily Tick-Conservation Audit — Error Codes (TICK-CONSERVE-01)

> **⚠ RETIRED 2026-07-18 (tick-conservation retirement — dead-WS sweep
> follow-up):** the daily 15:40 IST audit and this code are DELETED. Every
> audit INPUT died with the dead tick chain (stage-2 sweep #1631, 2026-07-17):
> the WAL frame counter had no live writer (both live feeds retired — Dhan
> 2026-07-13, Groww 2026-07-15), the processor outcome counters died with the
> deleted `tick_processor.rs` chain, and nothing writes the `ticks` table on
> the REST-only runtime — so every run could only record `partial` (an
> honest-but-permanent no-signal state, never a measurement). Deleted:
> `crates/app/src/tick_conservation_boot.rs` (the audit + spawn sites),
> `crates/storage/src/tick_conservation_audit_persistence.rs` (the Rust
> WRITER only), the `ws_frame_spill.rs::count_frames_for_ist_day` scan block,
> the `ErrorCode::TickConserve01DailyResidual` variant, the
> `tick-conserve-01` CloudWatch filter+alarm (dated note in
> `error-code-alarms.tf`), and the triage rule. The
> **`tick_conservation_audit` QuestDB TABLE is RETAINED** (SEBI 5y, never
> dropped — no DDL drop anywhere; historical rows stay queryable).
> Relocated survivors: `ws_wal_dir()` → `crates/app/src/boot_helpers.rs`;
> `parse_questdb_count()` → `crates/app/src/feed_scoreboard_boot.rs`.
> Content below retained as historical audit per house convention.

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C > this file.
> **Operator directive (2026-06-10, verbatim):** *"Go ahead to achieve zero
> tick loss"* — following *"I suspect maybe in some cases we are removing
> ticks … or dropping or … duplicating or … missing or … miss replaying or
> wal or being buffer or spill or dlq or in memory ram or db"*.
> **Companion code:** `crates/app/src/tick_conservation_boot.rs`,
> `crates/storage/src/tick_conservation_audit_persistence.rs`,
> `crates/storage/src/ws_frame_spill.rs::count_frames_for_ist_day`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `TickConserve*` variant.

---

## §0. Why this exists

The in-loop conservation ledger (60 s) proves `processed == persisted +
filtered` INSIDE the process; the 15:31 IST cross-verify proves candle-level
agreement with Dhan's own history. What neither covers: an end-to-end count
reconciliation across the THREE independent record stores — the WAL disk log
(every frame Dhan delivered, captured at the socket), the processor outcome
counters, and the actual queryable `ticks` rows. At **15:40 IST** each
trading day the auditor reconciles all three and writes one forensic row to
`tick_conservation_audit`:

```
delivery_residual = wal_tick_frames − processed
outcome_residual  = processed − (persisted + junk + stale_day
                                 + outside_hours + dedup + storage_errors)
```

Both zero + full-session coverage → `balanced` (info! line, no page).
Either positive → **TICK-CONSERVE-01**.

## §1. TICK-CONSERVE-01 — daily conservation residual found

**Severity:** High. **Auto-triage:** No (data-accounting signal; the WAL
still holds the frames durably, so this is not Critical-class data loss).

**Trigger:** the 15:40 IST audit computed a positive `delivery_residual`
and/or `outcome_residual` for the trading day.

**What each residual means:**

| Residual | Meaning | Recovery |
|---|---|---|
| `delivery_residual > 0` | Frames Dhan delivered (in the WAL) never reached the processor — live-path backpressure drop during a burst | The frames are ON DISK in the WAL → replayed into the pipeline at next boot; the count pins the exact magnitude |
| `outcome_residual > 0` | Ticks entered the pipeline but reached no known outcome — true in-process leak (same class the 60 s ledger pages on) | Investigate immediately; cross-check `tv_tick_conservation_leak_total` and the 60 s ledger logs |

**Triage:**
1. `mcp__tickvault-logs__questdb_sql "select * from tick_conservation_audit
   order by ts desc limit 5"` — the full per-stage numbers.
2. `delivery_residual > 0` → check `tv_ws_frame_spill_drop_critical` and the
   live-channel drop counters for the matching count; confirm the next boot's
   `tv_wal_replay_recovered_total` picks them up.
3. `outcome_residual > 0` → check the 60 s conservation ledger
   (`tickvault_core::pipeline::tick_processor::conservation` target) for the
   window where the leak appeared.
4. `partial_coverage = true` rows are NOT leaks — they mean the process
   booted mid-session (counters reset) or a source (QuestDB / metrics
   endpoint) was unavailable; outcome is `partial`, honestly flagged.
5. Cross-check `db_rows` vs `persisted`: db_rows > persisted after a mid-day
   restart is NORMAL (earlier process's rows); db_rows < persisted by more
   than the in-flight margin suggests QuestDB-side DEDUP collapse or WAL
   apply lag — inspect QuestDB.

**Honest envelope (§F wording):** the audit proves conservation INSIDE the
capture boundary — frames that reached our WAL. Ticks Dhan sent while we
were disconnected never reached the box and are visible ONLY via the
15:31 cross-verify against Dhan's own candles + the gap detector, not via
this count. Dhan's feed is itself sampled (~2–4 updates/sec/SID), so
"every tick Dhan delivered" ≠ "every exchange trade".

**Source:**
- `crates/common/src/error_code.rs::TickConserve01DailyResidual`
- `crates/app/src/tick_conservation_boot.rs::run_tick_conservation_audit`
- `crates/storage/src/tick_conservation_audit_persistence.rs`
- `crates/storage/src/ws_frame_spill.rs::count_frames_for_ist_day`

### 2026-07-14 Update — TICK-CONSERVE-01 now PAGES (delivery boundary closed)

The 2026-07-10 automation audit found this High code emitted `error!` but
was **log-sink-only** — a positive daily residual paged NOBODY. It now
routes via the canonical errcode chain: `error!` → errors.jsonl →
CloudWatch Logs `/tickvault/<env>/app` → the
`tv-<env>-errcode-tick-conserve-01` log metric filter
(`deploy/aws/terraform/error-code-alarms.tf`) → alarm (≤5 min) → SNS →
Telegram. `ok_recovery = false`: the 15:40 IST audit fires its residual
line once per day — a discrete data-accounting finding; the auto-OK
~15 min after the datapoint ages out can never mean the accounting
balanced (Rule-11 false-recovery; the aggregator-drop-01 precedent) —
the real recovery signal is the next trading day's `balanced` row.
Lockstep surfaces: the "Which codes page" list in
`observability-architecture.md` + the bidirectional drift ratchet
`crates/common/tests/error_code_paging_filter_drift_guard.rs`.

## §2. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `TickConserve*` variant)
- `crates/app/src/tick_conservation_boot.rs`
- `crates/storage/src/tick_conservation_audit_persistence.rs`
- Any file containing `TICK-CONSERVE-` or `TickConserve0` or
  `tick_conservation_audit`
