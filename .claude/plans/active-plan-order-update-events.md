# Implementation Plan: Full-Fidelity Order/Position Push-Event Capture

**Status:** APPROVED
**Date:** 2026-07-18
**Approved by:** Parthiban (operator standing pre-authorization — the order/position push-capture design, coordinator dispatch 2026-07-18; design record: scratchpad `order-events-comparison-and-design.md` §5)

> **Guarantee matrices:** every item below is bound by the 15-row + 7-row
> guarantee matrices in `.claude/rules/project/per-wave-guarantee-matrix.md`
> (cross-referenced per that file's mandate — the matrices apply verbatim to
> each item; no item may weaken a row).

## Design

Two new forensic QuestDB tables capture EVERY field the broker push channels
deliver, closing the loss point where the 11-field neutral `BrokerOrderEvent`
seam discards the rest of the decoded payloads:

1. `order_update_events` — one row per received order push event, BOTH feeds
   (`feed='dhan'` from the gated `[dhan_order_push]` channel's full
   `OrderUpdate` struct; `feed='groww'` from the decoded
   `OrderDetailsBroadCastDto` at the push decode site, BEFORE the lossy
   `map_order_broadcast` mapper). DEDUP UPSERT KEYS
   `ts, trading_date_ist, feed, order_id, event_seq` — `event_seq` is a
   process-global monotonic receipt sequence (AtomicU64 in
   `crates/common/src/broker_order_events.rs`) so a burst of same-second
   updates for one order all survive, while an ILP retry of the same row
   collapses idempotently.
2. `position_update_events` — one row per Groww position push event (all 19
   decoded `PositionDetailProto` fields — SymbolInfo + PositionInfo NSE/BSE
   credit/debit legs), replacing today's decode-and-drop in
   `crates/trading/src/oms/groww/push/position.rs`. DEDUP UPSERT KEYS
   `ts, trading_date_ist, feed, symbol_isin, event_seq`.

Records are defined in `crates/common/src/broker_order_events.rs`
(`OrderUpdateEventRecord`, `PositionUpdateEventRecord`) — the existing
11-field `BrokerOrderEvent` seam is UNTOUCHED. Producers hand records to an
injectable bounded sink (mpsc ~256, `try_send`, never blocking the push read
loops); the app-side consumer (`crates/app/src/order_update_events_boot.rs`,
config `[order_update_events]`, serde default OFF, base.toml ON) drains into
two ILP-over-HTTP writers (`crates/storage/src/order_update_events_persistence.rs`
+ `position_update_events_persistence.rs`) following the
`order_audit_persistence.rs` template: ensure-DDL self-heal, per-flush server
ACK, `discard_pending` on failed flush, non-finite clamps, sanitized bounded
strings, static-label counters. Groww paise → rupees at record construction.
Both tables register in `partition_manager.rs` `DAY_PARTITIONED_TABLES`
(Standard 90d class). New `ErrorCode::OrderEvt01PersistFailed`
("ORDER-EVT-01", High, log-sink-only) + runbook rule file. Crates touched:
common, storage, trading, app.

## Edge Cases

- Same-second burst of order updates for one order: `event_seq` in DEDUP key
  keeps every row; ILP retry of the identical row reuses the same seq →
  collapses (idempotent).
- proto3 absent-vs-zero: Groww numeric zeros persisted verbatim (audit table
  records the wire, never invents Options); absent submessages → `-1`
  numeric sentinels / empty-string → `"n/a"` SYMBOL sentinel (ILP rejects
  empty symbols).
- Non-finite prices (NaN/inf from f64 math): clamped to 0.0 + counted
  (`tv_order_update_events_nonfinite_clamped_total`).
- Oversized strings: `reject_reason` ≤300 chars, `detail_raw` ≤2000 chars via
  the house sanitize choke point (control/BiDi strip) — a hostile broker
  string can never bloat a row or inject ILP.
- Sink full/closed (slow QuestDB, shutdown): `try_send` refused → drop counted
  `tv_order_update_events_dropped_total{reason="full"|"closed"}` + ONE coded
  `error!` (stage `sink_drop`) — best-effort forensics; the push read loop and
  the trading-decision seam are NEVER blocked.
- Groww security_id resolution best-effort: unresolved → `-1` sentinel, never
  fabricated; `exchange_segment` from the wire enum with raw-preserving
  fallback labels.
- Feed disabled at boot / feature absent: zero producers exist; the consumer
  parks idle on an empty channel (config OFF = byte-identical behavior).
- IST midnight crossing: `trading_date_ist` derived from the record's own ts
  at persist time, never a cached boot date.

## Failure Modes

- QuestDB unreachable at boot (ensure-DDL fails): coded `error!`
  (ORDER-EVT-01, stage `ensure_client_build`/`ensure_ddl`) — HTTP-CLIENT-01
  class duplicate-row window documented (first ILP write may auto-create the
  table WITHOUT DEDUP until a later ensure succeeds); boot never blocks.
- ILP flush refused by the per-request server ACK: `error!` (stage `flush`) +
  `discard_pending()` (poisoned-buffer defense) +
  `tv_order_update_events_rows_discarded_total`; the next event re-appends.
- Append rejected: `error!` (stage `append`), row skipped, loop continues.
- Consumer task dies: supervised respawn via `classify_join_exit` house
  pattern + `tv_order_update_events_task_respawn_total{reason}`; release
  builds abort on panic (`panic = "abort"` — respawn arms are unwind-build
  self-heal only, stated honestly).
- Sustained sink backpressure: bounded channel (256) drops loudly (counted +
  coded), never OOM, never blocks the push transport.
- All failures are log-sink-only (NO error_code_alerts entry, NO Telegram —
  the Dhan noise-lock posture; forensic capture, not a pager).

## Test Plan

- `crates/common` (`cargo test -p tickvault-common`): record constructors,
  event_seq monotonicity + uniqueness across threads, ErrorCode catalogue
  tests (all()/unique/roundtrip/runbook-path-exists/severity),
  `error_code_rule_file_crossref` (both directions), tag guard.
- `crates/storage` (`cargo test -p tickvault-storage`): DDL contains every §5
  column (both tables), DEDUP key consts EXACT
  (`ts, trading_date_ist, feed, order_id, event_seq` /
  `ts, trading_date_ist, feed, symbol_isin, event_seq`), ILP conf targets
  http port not tcp (ratchet), non-finite clamp, sanitize caps (300/2000),
  paise→rupees conversion, mock-HTTP flush ACK tests (200/500/unreachable →
  discard_pending), `dedup_segment_meta_guard` (feed-in-key) green,
  `partition_retention_coverage_guard` green with both tables registered.
- `crates/trading` (`cargo test -p tickvault-trading --features groww_orders`):
  Groww order-record builder maps every `OrderDetailUpdate` field; position
  handler builds `PositionUpdateEventRecord` from a full
  `PositionDetailProto` (all 19 fields asserted); absent submessages degrade
  to sentinels; sink-refused delivery counted not panicked.
- `crates/app` (`cargo test -p tickvault-app`): config default-OFF +
  base.toml opt-in, consumer wiring guard, Dhan completeness test asserting
  every `OrderUpdate` field lands in a typed column or `detail_raw`.
- `bash .claude/hooks/plan-verify.sh` + `banned-pattern-scanner.sh` clean;
  `cargo fmt --check` + `cargo clippy --workspace --no-deps -- -D warnings`.

## Rollback

- Config kill switch: `[order_update_events] enabled = false` (serde default
  is ALREADY off — an absent section is disabled) stops the consumer; the
  bounded sinks drop-count harmlessly; zero behavior change elsewhere.
- Full revert: `git revert` of the feature commits — the two new tables are
  additive (no existing table's schema or DEDUP key is touched, no existing
  writer modified); orphaned tables are inert data, droppable at leisure.
- The 11-field `BrokerOrderEvent` seam, the order_audit path, the OMS, and
  the push transports are unmodified — reverting cannot regress them.

## Observability

- New ErrorCode `ORDER-EVT-01` (`OrderEvt01PersistFailed`, Severity::High,
  auto-triage-safe, runbook
  `.claude/rules/project/order-update-events-error-codes.md`) on every
  persist-leg failure with a `stage` field
  (`ensure_client_build`/`ensure_ddl`/`append`/`flush`/`sink_drop`).
- Static-label counters: `tv_order_update_events_rows_total{feed,kind}`,
  `tv_order_update_events_persist_errors_total{stage}`,
  `tv_order_update_events_dropped_total{reason}`,
  `tv_order_update_events_rows_discarded_total`,
  `tv_order_update_events_nonfinite_clamped_total`,
  `tv_order_update_events_task_respawn_total{reason}`.
- Forensic ground truth: the two QuestDB tables themselves
  (`mcp__tickvault-logs__questdb_sql` count checks in the runbook triage).
- Delivery boundary (honest): log-sink-only — no CloudWatch filter, no
  Telegram event; adding one is a flagged follow-up per the SCOREBOARD-01
  precedent.

## Plan Items

- [x] Records + receipt sequence in the neutral seam (crate: common)
  - Files: crates/common/src/broker_order_events.rs
  - Tests: test_order_update_event_record_construction, test_position_update_event_record_construction, test_next_event_seq_is_monotonic_and_unique

- [x] ErrorCode variant ORDER-EVT-01 (crate: common)
  - Files: crates/common/src/error_code.rs
  - Tests: test_all_variants_have_unique_code_str, test_every_variant_has_non_empty_runbook_path, every_error_code_variant_appears_in_a_rule_file

- [x] Runbook rule file (docs)
  - Files: .claude/rules/project/order-update-events-error-codes.md
  - Tests: every_runbook_path_exists_on_disk, every_rule_file_code_has_an_enum_variant

- [x] order_update_events persistence writer (crate: storage)
  - Files: crates/storage/src/order_update_events_persistence.rs, crates/storage/src/lib.rs
  - Tests: test_order_update_events_ddl_contains_expected_columns, test_dedup_key_order_update_events_exact, test_order_update_events_ilp_conf_targets_http_port, test_order_update_events_flush_failure_discards_pending

- [x] position_update_events persistence writer (crate: storage)
  - Files: crates/storage/src/position_update_events_persistence.rs, crates/storage/src/lib.rs
  - Tests: test_position_update_events_ddl_contains_expected_columns, test_dedup_key_position_update_events_exact, test_position_update_events_ilp_conf_targets_http_port

- [x] Partition-manager registration (crate: storage)
  - Files: crates/storage/src/partition_manager.rs
  - Tests: partition_retention_coverage_guard (existing, must stay green with the two new tables)

- [x] Groww producers at the push decode sites (crate: trading, feature groww_orders)
  - Files: crates/trading/src/oms/groww/push/runner.rs, crates/trading/src/oms/groww/push/position.rs, crates/trading/src/oms/groww/push/proto.rs, crates/trading/src/oms/groww/push/order_events.rs, crates/trading/src/oms/groww/push/mod.rs
  - Tests: test_build_groww_order_event_record_maps_every_detail_field, test_build_groww_position_event_record_maps_all_symbol_and_position_fields, test_absent_submessages_degrade_to_sentinels

- [x] Dhan producer at the gated order-push mapping path (crate: app)
  - Files: crates/app/src/dhan_order_push_observability.rs
  - Tests: test_dhan_order_update_record_covers_every_field

- [x] App-side consumer + config + wiring (crate: app, common, config)
  - Files: crates/app/src/order_update_events_boot.rs, crates/app/src/main.rs, crates/common/src/config.rs, config/base.toml
  - Tests: test_order_update_events_config_defaults_off, test_base_toml_enables_order_update_events, test_spawn_capture_disabled_returns_none_pair, test_consumer_exits_clean_when_both_channels_close

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | Groww order push burst (3 updates, same order, same second) | 3 rows in order_update_events (event_seq distinct) |
| 2 | Groww position frame with only NSE leg | Row with NSE doubles populated, BSE legs 0.0, symbol_isin keyed |
| 3 | Dhan order update (paper channel) | Row feed='dhan' with every OrderUpdate field typed or in detail_raw |
| 4 | QuestDB down at flush | ORDER-EVT-01 stage=flush, pending discarded, counters rise, loop continues |
| 5 | Sink full (256 backlog) | Drop counted reason="full" + coded error; push loop unblocked |
| 6 | Config OFF | No consumer task, producers' sends drop-counted closed/absent, zero rows |
