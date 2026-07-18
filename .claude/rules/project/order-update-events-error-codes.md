# Full-Fidelity Order/Position Push-Event Capture — Error Codes (ORDER-EVT-01)

> **Authority:** CLAUDE.md > `operator-charter-forever.md` §C/§F >
> `dhan-rest-only-noise-lock-2026-07-14.md` (the Dhan alert surface is
> UNCHANGED — this subsystem adds ZERO Telegram) >
> `groww-order-push-error-codes.md` (the push transport whose decoded
> payloads this captures) > `live-feed-purity.md` (writes ONLY its own two
> tables — never `ticks`/`candles_*`) > this file.
> **Design record (2026-07-18):** the order-events comparison + capture
> design (coordinator dispatch 2026-07-18) — the 11-field neutral
> `BrokerOrderEvent` seam discards most of what the broker push channels
> deliver; these two forensic tables capture EVERY decoded field.
> **Companion code:** `crates/common/src/broker_order_events.rs`
> (`OrderUpdateEventRecord` / `PositionUpdateEventRecord` /
> `next_event_seq`), `crates/storage/src/order_update_events_persistence.rs`
> + `crates/storage/src/position_update_events_persistence.rs` (the two
> ILP-over-HTTP writers), `crates/app/src/order_update_events_boot.rs`
> (config-gated supervised consumer), the Groww producers under
> `crates/trading/src/oms/groww/push/` (feature `groww_orders`) and the
> Dhan producer on the gated `[dhan_order_push]` mapping path,
> `crates/common/src/error_code.rs::ErrorCode::OrderEvt01PersistFailed`.
> **Cross-ref:** `crates/common/tests/error_code_rule_file_crossref.rs`
> requires this file to mention every `OrderEvt01*` variant verbatim —
> `ORDER-EVT-01` and `OrderEvt01PersistFailed` appear below.

---

## §0. Why this code exists (the capture gap)

Both brokers' push channels decode FAR more than survives into a queryable
row today: the Dhan order-update parse carries 46 fields (most DROPPED at
`order_update_to_audit_row`), the Groww order push decodes 20 (the 11-field
`BrokerOrderEvent` mapper keeps a lossy subset), and the Groww position
push decodes 19 fields that persist NOWHERE. Two new DAY-partitioned
forensic tables close that:

1. **`order_update_events`** — one row per received order push event, BOTH
   feeds, full-fidelity typed columns + a bounded `detail_raw` remainder.
   DEDUP UPSERT KEYS `ts, trading_date_ist, feed, order_id, event_seq`.
2. **`position_update_events`** — one row per Groww position push event
   (all SymbolInfo + PositionInfo NSE/BSE credit/debit legs; absent legs
   stay NULL, never fabricated zeros). DEDUP UPSERT KEYS
   `ts, trading_date_ist, feed, symbol_isin, event_seq`.

`event_seq` is a process-global monotonic receipt sequence (AtomicU64) —
same-second bursts for one order all survive as distinct rows, while an
ILP retry of the identical row (same seq) collapses idempotently. Both
keys carry `feed` (the 2026-06-28 every-table rule); neither carries
`security_id`, so the I-P1-11 segment-pairing DEDUP rule does not bind.

The whole subsystem is COLD-PATH, best-effort forensics: producers hand
records to a bounded sink (`try_send`, never blocking the push read
loops); the app-side consumer (config `[order_update_events]`, serde
default OFF, base.toml ON) drains into the two writers. The 11-field
decision seam, the `order_audit` path, the OMS, and the push transports
are UNTOUCHED by construction.

## §1. ORDER-EVT-01 — push-event capture persist degraded

**Severity:** High. **Auto-triage safe:** Yes (the degrade already
happened; DEDUP-idempotent re-appends and the next event self-heal — the
operator inspects, never manually re-persists first).

**Trigger:** `ErrorCode::OrderEvt01PersistFailed`, distinguished by the
`stage` field:

| stage | Meaning |
|---|---|
| `ensure_client_build` | the boot-time ensure-DDL reqwest client could not be built (HTTP-CLIENT-01 class — host fd/TLS/resolver pressure); DDL skipped this boot |
| `ensure_ddl` | the `CREATE TABLE` / `ALTER ADD COLUMN` / `DEDUP ENABLE` `/exec` leg returned non-2xx or was unreachable — NOTE the duplicate-row window: the first ILP write may auto-create the table WITHOUT DEDUP UPSERT KEYS until a later boot's ensure succeeds |
| `append` | an ILP buffer append was rejected — the row is skipped, the loop continues |
| `flush` | the ILP-over-HTTP flush was refused by the per-request server ACK (the 2026-07-05 fire-and-forget lesson) — pending rows DISCARDED (`discard_pending`, the poisoned-buffer defense; `tv_order_update_events_rows_discarded_total` counts) |
| `sink_drop` | a producer's bounded-sink `try_send` was refused (full/closed) — the event's capture row is LOST, counted PER EVENT `tv_order_update_events_dropped_total{reason="full"\|"closed"}`; the coded `error!` is EDGE-LATCHED per channel on BOTH feeds (audit-findings Rule 4 — the episode's first drop is loud, subsequent drops are `debug!`+counter, a successful publish re-arms); the push read loop is never blocked |

**Triage:**
1. `mcp__tickvault-logs__tail_errors` — find `ORDER-EVT-01`; the `stage`
   names the failing leg (+ feed context fields).
2. `tv_order_update_events_persist_errors_total{stage}` rate non-zero →
   QuestDB ILP/HTTP degraded; run `make doctor` (cross-check
   BOOT-01/BOOT-02, WAL-SUSPEND-01 — the sibling audit writers share the
   target).
3. Sustained `sink_drop` with `reason="full"` = the consumer is stalled
   behind a slow QuestDB — the forensic rows for the window are lost
   (best-effort by design); the trading-decision seam is unaffected.
4. Verify recovery:
   `mcp__tickvault-logs__questdb_sql "select count(*) from
   order_update_events where ts > dateadd('h', -1, now())"` — rows land
   again once events flow.
5. Day-1 DEDUP verify (the CHAIN-03 checklist precedent): after the first
   enabled session run
   `select ts, feed, order_id, event_seq, count(*) c from
   order_update_events group by ts, feed, order_id, event_seq order by c
   desc limit 5` — every `c` MUST be 1.

**Counters (static labels only):**
`tv_order_update_events_rows_total{feed}` ·
`tv_position_update_events_rows_total{feed}` ·
`tv_order_update_events_persist_errors_total{stage}` ·
`tv_order_update_events_dropped_total{reason}` ·
`tv_order_update_events_rows_discarded_total` ·
`tv_order_update_events_nonfinite_clamped_total` ·
`tv_order_update_events_task_respawn_total{reason}`.

**Honest envelope:** best-effort forensics — a persist outage loses
capture rows for the outage window ONLY; it never affects the OMS, the
order_audit path, the push transports, or any trading decision. Records
capture what the broker DELIVERED — proto3 zeros are persisted verbatim
(absent submessages degrade to `-1` / `"n/a"` sentinels; absent position
legs stay NULL). Groww push rows carry NO correlation_id (the wire has
none) and `security_id = -1` when the ISIN/contract mapping cannot
resolve — honest sentinels, never fabricated. Release-build panics abort
the process (`panic = "abort"`) — the consumer respawn arms self-heal in
unwind (dev/test) builds only (the TICK-FLUSH-01 honesty note). Rows
arrive only when the gated producers run: Groww needs the non-default
`groww_orders` feature + `[groww_orders] order_push_enabled`; Dhan needs
the `[dhan_order_push]` re-arm (base.toml `false`; the scope-lock §A.1
spawn retirement stands — NO socket is spawned by this subsystem).

**Redaction (2026-07-18, review round 1):** Dhan ClientId is deliberately
NOT persisted (house redaction class; account-constant, zero forensic
value — dated decision 2026-07-18). The builder omits it from
`detail_raw` and the completeness test pins the ABSENCE.

**Best-effort Groww security_id resolution (2026-07-18, review round 1
Fix 5):** the app consumer LAZILY loads the day's Groww watch file
(`data/groww/groww-watch-<date>.json` — the SAME builder product the
Groww lane subscribes from; `parse_watch_file` reused, never a
hand-rolled format) on the FIRST Groww record needing resolution, and
enriches a `security_id = -1` record by ISIN/contract-id then
display/contract symbol, using the SAME id space the Groww lane persists
everywhere else (stocks: numeric `exchange_token`; indices:
`stable_index_security_id` — the ids come from the watch BUILDER, never
re-derived). On a hit `exchange_segment` is set to the house slug
(`IDX_I`/`NSE_EQ`/`BSE_EQ`/`NSE_FNO`/`BSE_FNO`). Honest envelope: the
watch file lands asynchronously after boot, an absent/unparseable file
logs ONE coalesced `warn!` per IST day and rows keep the honest `-1`
(re-resolved on later events once the file lands); an identity outside
the day's watch set (a manually-traded contract) stays `-1` — never
fabricated, never cross-feed-guessed.

**Delivery boundary (honest — no false-OK):** ORDER-EVT-01 is
**log-sink-only**: NO `error_code_alerts` map entry in
`deploy/aws/terraform/error-code-alarms.tf` and NO mention in
`observability-architecture.md`'s paging list (the paging drift guard
sees no drift); ZERO new Telegram events (the Dhan noise-lock posture).
The coded `error!` lines + counters are the operator surface; adding a
CloudWatch log-filter alarm is a flagged follow-up (one map entry + a
cost note — the SCOREBOARD-01 precedent).

## §2. Table schemas (contract)

**`order_update_events`** (PARTITION BY DAY, Standard 90d retention class,
registered in `partition_manager.rs` `DAY_PARTITIONED_TABLES`): ts
(designated), trading_date_ist, feed SYMBOL, event_seq LONG, order_id
SYMBOL, exch_order_id SYMBOL, correlation_id SYMBOL, status SYMBOL,
raw_status SYMBOL, leg_no INT, product SYMBOL, order_type SYMBOL,
transaction_type SYMBOL, validity SYMBOL, exchange SYMBOL,
exchange_segment SYMBOL, security_id LONG, symbol SYMBOL, quantity LONG,
disclosed_qty LONG, remaining_qty LONG, traded_qty LONG, price DOUBLE,
trigger_price DOUBLE, avg_traded_price DOUBLE, last_traded_price DOUBLE,
reject_reason STRING (sanitized ≤300), remarks STRING, source SYMBOL,
off_mkt_flag SYMBOL, opt_type SYMBOL, algo_ord_no SYMBOL, algo_id SYMBOL,
mkt_type SYMBOL, series SYMBOL, good_till_days_date SYMBOL, ref_ltp
DOUBLE, tick_size DOUBLE, multiplier INT, instrument SYMBOL,
broker_create_time STRING, broker_update_time STRING, exchange_time
STRING, exchange_ts_ms LONG, contract_id SYMBOL, gui_order_id SYMBOL,
stage_trail STRING (ILP-sanitized ≤1024), detail_raw STRING
(ILP-sanitized ≤1024 = `MAX_AUDIT_STR_LEN`, the `sanitize_ilp_string`
internal bound — the originally-declared 2000 was dead, review round 1
LOW-1).
`const DEDUP_KEY_ORDER_UPDATE_EVENTS = "ts, trading_date_ist, feed, order_id, event_seq"`.

**`position_update_events`** (PARTITION BY DAY, same class): ts
(designated), trading_date_ist, feed SYMBOL, event_seq LONG, symbol_isin
SYMBOL, security_id LONG, exchange_segment SYMBOL, symbol SYMBOL,
exchange SYMBOL, tr_time_stamp LONG, search_id SYMBOL, product SYMBOL,
contract_id SYMBOL, equity_type SYMBOL, display_name SYMBOL,
underlying_id SYMBOL, nse_market_lot LONG, bse_market_lot LONG,
underlying_asset_type SYMBOL, freeze_qty LONG, nse_credit_qty DOUBLE,
nse_credit_price DOUBLE, nse_debit_qty DOUBLE, nse_debit_price DOUBLE,
bse_credit_qty DOUBLE, bse_credit_price DOUBLE, bse_debit_qty DOUBLE,
bse_debit_price DOUBLE, detail_raw STRING (ILP-sanitized ≤1024 =
`MAX_AUDIT_STR_LEN`).
`const DEDUP_KEY_POSITION_UPDATE_EVENTS = "ts, trading_date_ist, feed, symbol_isin, event_seq"`.

## §3. Trigger / auto-load

This rule activates when editing:
- `crates/common/src/error_code.rs` (any `OrderEvt01*` variant)
- `crates/common/src/broker_order_events.rs` (the capture records)
- `crates/storage/src/order_update_events_persistence.rs`
- `crates/storage/src/position_update_events_persistence.rs`
- `crates/app/src/order_update_events_boot.rs`
- `crates/common/src/config.rs` (`OrderUpdateEventsConfig`) or
  `config/base.toml` `[order_update_events]`
- Any file containing `ORDER-EVT-01`, `OrderEvt01PersistFailed`,
  `OrderUpdateEventRecord`, `PositionUpdateEventRecord`, `next_event_seq`,
  `order_update_events`, `position_update_events`, or
  `tv_order_update_events_`
