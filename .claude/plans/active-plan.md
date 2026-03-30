# Implementation Plan: Sandbox Mode + Complete Order Trading Implementation

**Status:** IN_PROGRESS
**Date:** 2026-03-30
**Approved by:** Parthiban

## Scope

26 missing endpoints + 6 missing WS fields across 11 Dhan order docs.
Hybrid sandbox mode: real market data + sandbox order execution.

## Plan Items

### Phase 1: Sandbox Mode Infrastructure (Day 1)

- [ ] 1.1: Add TradingMode enum (Paper/Sandbox/Live) to config.rs
  - Files: crates/common/src/config.rs, config/base.toml
  - Tests: test_trading_mode_paper, test_trading_mode_sandbox, test_trading_mode_live

- [ ] 1.2: Add sandbox SSM credential fetch (sandbox-client-id + sandbox-token)
  - Files: crates/core/src/auth/secret_manager.rs
  - Tests: test_fetch_sandbox_credentials

- [ ] 1.3: Add DHAN_SANDBOX_BASE_URL constant + URL switching in OMS engine
  - Files: crates/common/src/constants.rs, crates/trading/src/oms/engine.rs
  - Tests: test_sandbox_url_selected, test_live_url_selected, test_paper_no_http

- [ ] 1.4: Skip IP verification + TOTP auth chain for sandbox mode in boot
  - Files: crates/app/src/main.rs
  - Tests: test_sandbox_skips_ip_check

### Phase 2: Kill Switch + P&L Exit API Methods (Day 2)

- [ ] 2.1: Add 6 API client methods for kill switch + P&L exit
  - Files: crates/trading/src/oms/api_client.rs
  - Methods: activate_kill_switch, deactivate_kill_switch, get_kill_switch_status, configure_pnl_exit, stop_pnl_exit, get_pnl_exit_status
  - Tests: test_kill_switch_activate, test_pnl_exit_configure, test_pnl_exit_string_values

- [ ] 2.2: Add emergency halt module wired to kill switch
  - Files: crates/trading/src/risk/kill_switch.rs (NEW)
  - Tests: test_emergency_halt_activates_kill_switch

### Phase 3: Missing Standard Order Endpoints (Day 3)

- [ ] 3.1: Add order slicing endpoint (POST /orders/slicing)
  - Files: crates/trading/src/oms/api_client.rs, types.rs
  - Tests: test_order_slicing_request

- [ ] 3.2: Add trade book endpoints (GET /trades, GET /trades/{order-id})
  - Files: crates/trading/src/oms/api_client.rs, types.rs (TradeEntry struct)
  - Tests: test_get_trades, test_get_trades_for_order

- [ ] 3.3: Add get order by correlation ID (GET /orders/external/{corr-id})
  - Files: crates/trading/src/oms/api_client.rs
  - Tests: test_get_order_by_correlation

- [ ] 3.4: Fix cancel order to return response body (not discard)
  - Files: crates/trading/src/oms/api_client.rs
  - Tests: test_cancel_returns_status

- [ ] 3.5: Add missing PlaceOrderRequest fields (amoTime, boProfitValue, boStopLossValue, disclosedQuantity, correlationId)
  - Files: crates/trading/src/oms/types.rs
  - Tests: test_amo_fields, test_bracket_fields

### Phase 4: Super Orders (Day 4-5)

- [ ] 4.1: Add PlaceSuperOrderRequest + SuperOrderLeg types + OrderLeg enum
  - Files: crates/trading/src/oms/types.rs
  - Tests: test_super_order_request_serialization, test_leg_enum

- [ ] 4.2: Add 4 super order API client methods (place/modify/cancel/list)
  - Files: crates/trading/src/oms/api_client.rs
  - Tests: test_place_super_order, test_modify_entry_leg, test_modify_target_leg_only_price, test_modify_sl_leg_only_sl_and_trail, test_cancel_entry_cancels_all

- [ ] 4.3: Add trailing SL logic (trailingJump=0 cancels trailing)
  - Files: crates/trading/src/oms/super_order.rs (NEW)
  - Tests: test_trailing_jump_zero_cancels, test_trailing_jump_positive

- [ ] 4.4: Add leg-specific modification restrictions validation
  - Files: crates/trading/src/oms/super_order.rs
  - Tests: test_entry_leg_all_fields_modifiable, test_target_leg_only_price, test_sl_leg_only_sl_and_trail

### Phase 5: Forever Orders / GTT (Day 6)

- [ ] 5.1: Add ForeverOrderRequest + OrderFlag enum (SINGLE/OCO) + OCO second-leg fields
  - Files: crates/trading/src/oms/types.rs
  - Tests: test_single_order_flag, test_oco_requires_second_leg, test_single_rejects_second_leg

- [ ] 5.2: Add 4 forever order API client methods (create/modify/delete/list)
  - Files: crates/trading/src/oms/api_client.rs
  - Tests: test_create_forever_single, test_create_forever_oco, test_modify_with_leg_name

- [ ] 5.3: Add CNC/MTF-only product type validation
  - Files: crates/trading/src/oms/forever_order.rs (NEW)
  - Tests: test_intraday_rejected, test_margin_rejected, test_cnc_accepted, test_mtf_accepted

- [ ] 5.4: Add CONFIRM status handling in state machine
  - Files: crates/trading/src/oms/state_machine.rs
  - Tests: test_confirm_status_parsed, test_confirm_to_traded_valid

### Phase 6: Conditional Triggers (Day 7)

- [ ] 6.1: Add ComparisonType, Operator, IndicatorName, TimeFrame enums
  - Files: crates/trading/src/oms/types.rs
  - Tests: test_all_comparison_types, test_all_operators, test_all_indicators, test_all_timeframes

- [ ] 6.2: Add 5 conditional trigger API client methods
  - Files: crates/trading/src/oms/api_client.rs
  - Tests: test_create_trigger, test_modify_trigger, test_delete_trigger

- [ ] 6.3: Add equities/indices-only validation (reject F&O/commodity)
  - Files: crates/trading/src/oms/conditional_trigger.rs (NEW)
  - Tests: test_fno_rejected, test_commodity_rejected, test_equity_accepted, test_index_accepted

- [ ] 6.4: Add required-field validation per comparison type
  - Files: crates/trading/src/oms/conditional_trigger.rs
  - Tests: test_technical_with_value_requires_indicator, test_price_with_value_no_indicator

### Phase 7: OrderUpdate WS Fields + Position Reconciliation (Day 8)

- [ ] 7.1: Add 6 missing fields to OrderUpdate struct (AlgoOrdNo, Series, GoodTillDaysDate, AlgoId, Multiplier, MktType)
  - Files: crates/common/src/order_types.rs
  - Tests: test_all_ws_fields_parsed, test_missing_fields_default

- [ ] 7.2: Update handle_order_update to sync all fields (not just traded_qty/avg_price)
  - Files: crates/trading/src/oms/engine.rs
  - Tests: test_update_syncs_all_fields

- [ ] 7.3: Add position-level reconciliation (aggregate fills per security)
  - Files: crates/trading/src/oms/reconciliation.rs
  - Tests: test_position_level_recon, test_multi_order_position_netting

### Phase 8: EDIS + Statements + Margin Fixes (Day 9-10)

- [ ] 8.1: Add EDIS types + constants + 3 API methods (tpin/form/inquire)
  - Files: crates/trading/src/oms/api_client.rs, types.rs, crates/common/src/constants.rs
  - Tests: test_generate_tpin, test_edis_form, test_edis_inquire_all

- [ ] 8.2: Add ledger endpoint (GET /ledger) with STRING debit/credit
  - Files: crates/trading/src/oms/api_client.rs, types.rs
  - Tests: test_ledger_string_debit_credit, test_ledger_date_format

- [ ] 8.3: Add trade history endpoint (GET /trades/{from}/{to}/{page}) with 0-indexed pagination
  - Files: crates/trading/src/oms/api_client.rs, types.rs
  - Tests: test_trade_history_page_zero, test_trade_history_path_params

- [ ] 8.4: Fix margin calculator type issues (quantity i32, trigger_price Option<f64>)
  - Files: crates/trading/src/oms/types.rs
  - Tests: test_margin_optional_trigger_price, test_margin_quantity_i32

### Phase 9: Indicator Persistence + Warmup from Historical (Day 11-12)

- [ ] 9.1: Create `indicator_snapshots` QuestDB table DDL
  - Files: crates/storage/src/indicator_persistence.rs (NEW), crates/storage/src/lib.rs
  - Columns: ts, security_id, segment, timeframe, sma_5, sma_10, sma_20, sma_50, ema_9, ema_12, ema_26, rsi_14, macd_line, macd_signal, macd_hist, bb_upper, bb_lower, bb_middle, atr_14, obv, vwap, adx
  - Tests: test_indicator_snapshot_ddl, test_append_indicator_snapshot

- [ ] 9.2: Persist indicator values alongside 1-minute candles
  - Files: crates/core/src/pipeline/tick_processor.rs, crates/trading/src/indicator/engine.rs
  - Every 60s: snapshot current indicator state for all tracked securities
  - Tests: test_indicator_snapshot_persisted, test_indicator_outside_hours_skipped

- [ ] 9.3: Load indicator state from QuestDB on restart (warmup)
  - Files: crates/trading/src/indicator/engine.rs, crates/app/src/trading_pipeline.rs
  - At boot: query last indicator snapshot per security from DB
  - Initialize EMA/SMA/RSI/MACD with historical state (no cold start)
  - Tests: test_warmup_from_db, test_warmup_skips_if_no_data, test_indicators_warm_after_load

- [ ] 9.4: Store indicator values for multiple timeframes (1m, 5m, 15m, 1h, 1d)
  - Files: crates/trading/src/indicator/engine.rs
  - Compute indicators from materialized view candles (not just ticks)
  - Tests: test_multi_timeframe_indicators, test_5m_sma_from_candles

## Scenarios

| # | Scenario | Expected |
|---|----------|----------|
| 1 | mode=paper, place order | PAPER-0001 ID, zero HTTP calls |
| 2 | mode=sandbox, place order | HTTP to sandbox.dhan.co, fills at ₹100 |
| 3 | mode=live, place order | HTTP to api.dhan.co, real exchange fill |
| 4 | mode=sandbox, kill switch | HTTP to sandbox.dhan.co/killswitch |
| 5 | mode=sandbox, no static IP | IP check skipped, boot succeeds |
| 6 | Super order cancel ENTRY_LEG | All 3 legs cancelled |
| 7 | Super order cancel TARGET_LEG | Only target removed, cannot re-add |
| 8 | Forever order OCO missing price1 | Rejected at build time |
| 9 | Conditional trigger on F&O | Rejected (equities/indices only) |
| 10 | OrderUpdate with AlgoOrdNo field | Parsed into OrderUpdate struct |
