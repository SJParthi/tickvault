# Implementation Plan: Phase C1 — Dhan retirement rewires (order-update → dhan_rest_stack; scoreboard feed_off; intraday-parser relocation; Groww FUTIDX de-gate)

**Status:** APPROVED
**Date:** 2026-07-13
**Approved by:** Parthiban (operator) — 2026-07-13 "agreed" rulings relayed via the coordinator session (Q4-i order-update rewire; the Phase C sequencing rulings), on top of the 2026-07-13 retirement directive recorded in `websocket-connection-scope-lock.md` "2026-07-13 Amendment".
**Authority:** `websocket-connection-scope-lock.md` "2026-07-13 Amendment" §A (order-update WS KEPT functional-dormant inside `dhan_rest_stack`, Q4-i) + §B (the KEEP/REWIRE seam: `index_futures.rs` de-gate obligation; `parse_intraday_1m_candles` relocation obligation) + `dual-feed-scoreboard-error-codes.md` §0 supersession banner ("Honest obligation — Phase C acceptance criterion": exclude `ws_type='order_update'` rows from the feed_off detection) + `cross-verify-1m-error-codes.md` retirement banner (parser relocation obligation) + `daily-universe-scope-expansion-2026-05-27.md` 2026-07-13 banner §(d) (the §36.7 Groww futures leg STANDS; the selector must be de-gated from the `daily_universe_fetcher` cargo feature or the Groww futures silently drop — a scope violation).
**Guarantee matrices:** 15-row + 7-row per `.claude/rules/project/per-wave-guarantee-matrix.md` — cross-referenced (this line is the cross-reference per the per-item enforcement rule). Item-specific deltas: all four rewires are COLD-PATH (boot/spawn/parse/daily-aggregation); no hot-path change, no new tick-drop path, no DEDUP-key change, no new QuestDB table, no new ErrorCode (no new failure class — the order-update WS keeps its self-contained WS-GAP-04/10 machinery; the scoreboard keeps SCOREBOARD-01).

## Design

PR-C1 is the REWIRES-ONLY precursor of the Phase C deletion PRs (ZERO deletions; crates touched: **tickvault-app** + **tickvault-core**):

1. **Order-update WS → `dhan_rest_stack` (functional-dormant, Q4-i).** `crates/app/src/dhan_rest_stack.rs` spawns `run_order_update_connection` after TokenManager init + client-id fetch + the post-market family claim, with: the stack's `TokenHandle`; a STACK-LOCAL `tokio::sync::broadcast::channel::<OrderUpdate>(256)` (mirrors the legacy channel size; the receiver is intentionally dropped — no OMS consumer exists until live trading returns; the send-drop arm in `order_update_connection.rs` logs + counts per audit Rule 5); `wal_spill = None` (Verified: the order-update WAL replay staging is a `main()` STAGE-C boot concern — main.rs:1142-1160 stages, the drain sites at main.rs:2621/8707 are both Dhan-lane/fast-arm-gated and dead on a dhan-off boot — the stack neither drains nor appends WAL; the dormant phase places no orders, so there is no order-event stream to durably capture); the OrderUpdateAuthenticated signal/latch + listener (fast-arm mirror); `Some(notifier)`; a ws_event_audit sender from the relocated shared consumer helper; `dhan_feed_flag = None` (the stack exists only when the raw TOML retires the lane — always-on within the stack; lane/stack mutual exclusion by construction + the family claim tripwire). The consumer helper (`spawn_ws_event_audit_consumer` + `run_ws_event_audit_consumer` + capacity const) MOVES from the main.rs binary into the new lib module `crates/app/src/ws_audit_consumer.rs` so the stack (lib) can call it; main.rs re-imports. Legacy spawn sites (main.rs fast arm + lane) UNTOUCHED — dead under `dhan_enabled=false`, deleted in C2; a dated comment at the stack spawn notes it becomes the sole call site after C2.
2. **Scoreboard feed_off acceptance criterion (the Phase B "Honest obligation").** `crates/app/src/feed_scoreboard_boot.rs`: `is_session_up_row` + `is_pre_session_up_row` gain the existing `is_market_data_ws_type()` filter, and the `any_up` fold in the 15:45 aggregation gains the same gate — a rewired order-update in-session Connected/Reconnected row can never defeat the round-6 `feed_off` classification for the permanently-off Dhan feed; genuine `main_feed`/`groww_bridge` up rows still do.
3. **Intraday-parser relocation (pure move, zero behavior change).** `MinuteCandle`, `intraday_request_body`, `intraday_utc_secs_to_ist_minute_nanos` (the parser's own IST-bucket helper) and `parse_intraday_1m_candles` (+ their 5 unit tests) move from `crates/app/src/cross_verify_1m_boot.rs` to the new `crates/app/src/dhan_intraday_parse.rs`; `cross_verify_1m_boot.rs`, `spot_1m_rest_boot.rs` AND `groww_spot_1m_boot.rs` (the third consumer, added by PR #1507) re-import from the new home — so Phase C can delete `cross_verify_1m_boot.rs` without orphaning the §8/§9 spot legs.
4. **Groww FUTIDX de-gate (the §36.7 mandate must not depend on a build feature).** Remove the `daily_universe_fetcher` cfg gates from `crates/core/src/instrument/index_futures.rs` (+ its two compile-time deps `csv_parser.rs` + `index_extractor.rs` — the minimal transitive closure; both are pure parse/canonicalize modules with no feature-only deps) and from the Groww extraction/audit/emission sites in `crates/core/src/feed/groww/instruments.rs` (extract_index_future_entries + GrowwIndexFuture + collect_fut_underlying_symbols_seen + groww_indices_absent_vs_dhan + the index-coverage audit block + the §36 selection/emission blocks + their tests); delete the now-dead `#[cfg(not(feature))]` empty-futures fallbacks. `shared_master_writer.rs` gates + the `daily_universe_fetcher` feature itself stay INTACT (SEBI-table gating — ruling Q3 scope). New ratchet pins: `index_futures.rs` carries no feature gate; the Groww extraction call is unconditional.

## Edge Cases

- Off-hours boot: the stack's order-update spawn parks on `defer_until_market_open_ist` (order_update_connection.rs:115) exactly as the lane spawn did — no flap, no reconnect storm.
- Broadcast receiver absent: `order_sender.send()` failures hit the existing error-logged drop arm + `tv_order_update_broadcast_drops_total` (audit-findings 2026-04-24 finding #4) — never silent.
- Family-claim tripwire: the order-update spawn sits AFTER `claim_post_market_task_family_once()` — if the lane/stack mutual-exclusion invariant ever broke (lane ran first), the stack returns before spawning a SECOND order-update WS.
- Scoreboard backfill/rerun days: the ws_type filter changes only up-row classification; the pre-session-toggle disable markers, the parked-wake exclusion, and the backfill inference are untouched.
- Feature matrix: `cargo check -p tickvault-core` AND `cargo check -p tickvault-core --features daily_universe_fetcher` both compile (the de-gated modules must build in BOTH modes; the app crate keeps the feature in `default`).
- WAL residual: on a dhan-off boot, boot-staged order-update WAL segments remain un-drained in `replaying/` (pre-existing Phase A behavior, unchanged by this PR — bounded, idempotent re-stage; C2 settles the replay topology when it deletes the legacy arms). Stated honestly at the spawn site.

## Failure Modes

- Double order-update spawn: impossible — the stack spawns only on the raw-TOML-retired branch (`is_dhan_config_enabled() == false`), behind the stack once-guard AND the family claim; the lane (the other spawner) refuses to start on the same raw gate. Ratchet pins the stack call site.
- Order-update auth failure in the stack: the connection's own WS-GAP-10 in-market outage paging + bounded reconnect ladder are self-contained — a dead Dhan token degrades the dormant WS loudly without touching the stack's REST legs or the Groww feed (the WS runs as its own spawned task; it can never block or kill the stack bring-up).
- Parser relocation regression: the 5 moved unit tests run in the new home; the consumers re-import — a signature/behavior drift fails compile or the moved tests.
- De-gate regression (re-gating): the new ratchet test fails the build if `index_futures.rs` regains a `daily_universe_fetcher` gate or the Groww extraction call goes conditional again.

## Test Plan

- NEW unit tests (feed_scoreboard_boot.rs): in-session order-update up row rejected by `is_session_up_row`; pre-session order-update up row rejected by `is_pre_session_up_row`; `main_feed`/`groww_bridge` rows still accepted (both arms).
- NEW ratchet (dhan_rest_stack.rs tests): production region spawns `run_order_update_connection(` with a stack-local broadcast channel + audit sender + `None` wal/feed-flag (source-scan, production-region split per the house pattern).
- NEW ratchet (core): `crates/core/tests/` or in-module — `index_futures.rs` carries no `cfg(feature = "daily_universe_fetcher")`; `instruments.rs`'s `extract_index_future_entries` call chain is unconditional.
- MOVED tests: the 5 intraday-parser unit tests run in `dhan_intraday_parse.rs`.
- ADJUSTED guard: `crates/app/tests/ws_event_audit_boot_guard.rs` — the consumer-internals pins re-point to the relocated module; a new assertion pins the stack's order-update audit-sender wiring.
- Suites: `cargo test -p tickvault-core -p tickvault-app` (+ `-p tickvault-common` if touched); `cargo check -p tickvault-core` both feature modes; fmt + clippy (`-D warnings -W clippy::perf`); banned-pattern scanner; plan-gate; the dhan_live_off_phase_a_guard + tick_conservation_wiring_guard + feed-scoreboard suites explicitly.

## Rollback

Single revert of this PR restores: the order-update WS spawned only from the (dead under `dhan_enabled=false`) lane/fast arms — zero prod impact on a dhan-off boot; the parser back in `cross_verify_1m_boot.rs`; the cfg gates back (Groww futures again feature-dependent — the pre-C1 state); the scoreboard predicates unfiltered. No schema change, no data change, no config change anywhere → rollback is a pure code revert.

## Observability

- The stack-spawned order-update WS stamps the SAME `ws_event_audit` rows (`WsType::OrderUpdate` — Connected/Reconnected/Sleep/Disconnected) through the relocated consumer; AUDIT-WS-01 semantics unchanged. `tv_order_update_reconnections_total` + the WS-GAP-10 outage page + the OrderUpdateAuthenticated Telegram continue unchanged. No new ErrorCode (no new failure class).
- Scoreboard: the feed_off classification change is pinned by unit tests; the daily card, SCOREBOARD-01 semantics, and all counters unchanged (episode ROWS for order_update are still persisted for forensics — only the up-signal classification filters).
- FUTIDX: the §36 emissions (FUTIDX-01 errors, `tv_index_futures_*` counters/gauge, parity recording) now compile unconditionally — identical emissions, wider build coverage.

## Plan Items

- [x] Item 1 — order-update WS rewired into dhan_rest_stack (functional-dormant) + ws-audit consumer helper relocated to lib
  - Files: crates/app/src/dhan_rest_stack.rs, crates/app/src/ws_audit_consumer.rs, crates/app/src/lib.rs, crates/app/src/main.rs, crates/app/tests/ws_event_audit_boot_guard.rs, .claude/rules/project/websocket-connection-scope-lock.md
  - Tests: test_rest_stack_spawns_order_update_ws_functional_dormant, test_order_update_connection_is_audit_wired (extended)
- [x] Item 2 — scoreboard feed_off up-row ws_type filter (Phase C acceptance criterion CLOSED)
  - Files: crates/app/src/feed_scoreboard_boot.rs, .claude/rules/project/dual-feed-scoreboard-error-codes.md
  - Tests: test_is_session_up_row_ignores_order_update_ws, test_is_pre_session_up_row_ignores_order_update_ws
- [x] Item 3 — intraday-parser relocation (pure move)
  - Files: crates/app/src/dhan_intraday_parse.rs, crates/app/src/cross_verify_1m_boot.rs, crates/app/src/spot_1m_rest_boot.rs, crates/app/src/groww_spot_1m_boot.rs, crates/app/src/lib.rs
  - Tests: intraday_request_body_has_interval_1_and_string_sid, intraday_utc_secs_to_ist_minute_nanos_floors_to_minute, parse_intraday_1m_candles_happy_path, parse_intraday_1m_candles_rejects_length_mismatch_and_malformed, parse_intraday_1m_candles_volume_as_float_truncates (moved)
- [x] Item 4 — Groww FUTIDX de-gate (selector + minimal dep closure + Groww sites)
  - Files: crates/core/src/instrument/index_futures.rs, crates/core/src/instrument/csv_parser.rs, crates/core/src/instrument/index_extractor.rs, crates/core/src/instrument/mod.rs, crates/core/src/feed/groww/instruments.rs
  - Tests: futidx_selector_is_not_feature_gated (new ratchet), existing index_futures + groww instruments suites now running in BOTH feature modes

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | dhan-off boot, market hours | stack brings up lock→token→REST legs AND the order-update WS (connected, dormant); ws_event_audit rows stamped |
| 2 | dhan-off boot, off-hours | order-update spawn parks until 09:00 IST (market-hours gate) — no flap |
| 3 | scoreboard day with an in-session order-update Reconnected row + zero Dhan ticks + disable-at-open | day still classifies `feed_off` (the row no longer defeats it) |
| 4 | scoreboard day with a genuine main_feed session up row | NOT feed_off (unchanged) |
| 5 | `cargo check -p tickvault-core` without the feature | index_futures + csv_parser + index_extractor + Groww futures extraction all compile |
| 6 | spot_1m_rest per-minute fire | identical parse behavior via the relocated module (moved tests green) |
