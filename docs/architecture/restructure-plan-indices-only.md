# Restructure Plan — Tables + Code + Boot + Config for Indices-Only Scope

> **Status:** DESIGN (no code shipped). Captures full restructure mandated 2026-05-18.
> **Authority:** CLAUDE.md > operator-charter-forever.md > this file.
> **Companion docs:** `aws-indices-only-locked-architecture.md` (locks), `option-chain-z-plus-heart-piece.md` (heart piece), `.claude/plans/aws-lifecycle/deletion-surface-map.md` (what comes out).
> **This doc describes what stays + new shape.** The deletion-surface map describes what comes out.

---

## §0. Auto-driver one-liner

> "Sir, old shop had 8 storerooms + 6 staff + 50 price boards + 17 file cabinets. New shop has 2 storerooms (just QuestDB) + 2 staff (just the worker + the bank watchman = CloudWatch) + 4 price boards (NIFTY/BANKNIFTY/SENSEX/INDIA VIX) + 8 file cabinets (only what we actually use). Everything else thrown out. Same building, much simpler shop."

```
   OLD SHAPE                          NEW SHAPE
   ────────                          ────────
   6 crates × ~30 modules           6 crates × ~12 modules (slimmer)
   ~74K LoC                          ~30-35K LoC (target)
   ~120 QuestDB tables               ~14 QuestDB tables
   17 config TOML sections           8 config TOML sections
   15-step boot                      8-step boot
   8 Docker containers               2 Docker containers
   ~11K instrument universe          4-SID compile-time constant
   ~5 sinks observability           CloudWatch single sink
```

---

## §1. New table catalog (14 tables — was ~120)

Single source of truth for QuestDB schema after restructure.

### §1.1 Live data tables (5)

| Table | Purpose | DEDUP key | Retention (hot/cold) |
|---|---|---|---|
| `ticks` | Raw tick stream from main feed WS | `(security_id, exchange_segment, ts, sequence_number)` | 7d hot / 90d S3 / 5y Glacier |
| `candles_1m` | Sealed 1m bars (4 SIDs) | `(security_id, exchange_segment, ts)` | same |
| `candles_3m` | Sealed 3m | same | same |
| `candles_5m` | Sealed 5m | same | same |
| `candles_15m` | Sealed 15m | same | same |
| `candles_1h` | Sealed 1h | same | same |
| `candles_1d` | Sealed daily | same | same |

(Note: 6 candle TFs = 6 tables. Adding ticks = 7 live tables total. Updating subtotal — actually 7. Adjusting after.)

### §1.2 Option chain tables (3)

| Table | Purpose | DEDUP key |
|---|---|---|
| `option_chain_snapshots` | Parsed snapshots per strike+side from REST every 50s | `(ts, underlying_id, expiry, strike, side)` |
| `option_chain_request_audit` | Per-request audit row (latency, status, retries) | `(ts, underlying_id, expiry)` |
| `dhan_option_chain_raw` | Full JSON body for replay | `(ts, underlying_id, expiry, payload_hash)` |

### §1.3 Cross-verify audit tables (3)

| Table | Purpose | DEDUP key |
|---|---|---|
| `candle_cross_verify_audit` | 15:31 IST run outcome per (instrument, TF) | `(trading_date_ist, instrument_id, exchange_segment, timeframe)` |
| `candle_cross_verify_mismatches` | Per-mismatch detail | `(trading_date_ist, instrument_id, timeframe, candle_ts, field)` |
| `morning_1d_cross_check_audit` | 08:05 IST pre-market 1d match | `(trading_date_ist, instrument_id, exchange_segment)` |

### §1.4 Operational audit tables (4)

| Table | Purpose | SEBI retention | DEDUP key |
|---|---|---|---|
| `order_audit` | Every order placed/cancelled/filled | **5 years (SEBI mandate)** | `(ts, correlation_id, transition_state)` |
| `auth_renewal_audit` | Token generation + renewal events | 1 year | `(ts, dhan_client_id, outcome)` |
| `boot_audit` | Each boot sequence outcome | 90 days | `(boot_id, step_name)` |
| `selftest_audit` | Market-open self-test outcomes | 90 days | `(trading_date_ist, check_name)` |

### §1.5 Supporting tables (1)

| Table | Purpose | DEDUP key |
|---|---|---|
| `previous_close` | Yesterday's close per instrument (for indicator warm-up + % move calc) | `(trading_date_ist, instrument_id, exchange_segment)` |

### §1.6 Total: 7 live + 3 option chain + 3 cross-verify + 4 audit + 1 supporting = **18 tables**

(Was: ~120 tables across phase2, depth, greeks, movers, bhavcopy, instrument master, etc.)

### §1.7 DELETED tables (~100)

Bucket: `instrument_master_*`, `derivative_contracts`, `instrument_build_metadata`, `phase2_audit`, `depth_rebalance_audit`, `depth_dynamic_diff_audit`, `deep_market_depth`, `market_depth`, `option_greeks`, `pcr_snapshots`, `greeks_verification`, `option_movers`, `movers_1s`, all 25 `movers_*` matviews, all 21 prior candle TF tables beyond the 6 locked, `subscription_audit`, `aggregator_seal_audit`, `bar_correction_audit`, `phase2_emit_audit`, `prev_close_persist_audit`, `ws_reconnect_audit`, `s3_archive_log`, `live_instance_lock`, `orphan_position_audit`, `bhavcopy_*`, etc.

---

## §2. New crate layout (6 crates — slimmer modules)

### §2.1 `crates/common/` (~10 modules — was 10)

| Module | Purpose | Status |
|---|---|---|
| `config.rs` | TOML config, slimmed sections | MODIFY (drop unused sections) |
| `constants.rs` | API URLs, packet offsets, **TICK_BUFFER_CAPACITY=100K** | MODIFY (right-size constants) |
| `error.rs` | DhanErrorCode + DataApiError + new OPTION-CHAIN + CROSS-VERIFY codes | MODIFY |
| `error_code.rs` | ErrorCode enum | MODIFY (delete ~12 variants, add ~12 new) |
| `types.rs` | ExchangeSegment, FeedRequestCode, FeedResponseCode | KEEP |
| `order_types.rs` | OrderStatus, ProductType, OrderType | KEEP |
| `tick_types.rs` | TickerData, QuoteData, FullPacketData | KEEP (Quote/Full slimmed since we only use Ticker for IDX_I) |
| **`locked_universe.rs`** | **NEW — 4-SID compile-time constant slice** | NEW |
| `trading_calendar.rs` | Market hours, holiday checks, IST handling | KEEP |
| `sanitize.rs` | Input sanitization | KEEP |

**Deleted:** `instrument_types.rs` (InstrumentType, ExpiryCode, InstrumentRecord — gone with universe builder), `instrument_registry.rs` (replaced by `locked_universe.rs`).

### §2.2 `crates/core/` (~6 modules — was 9)

| Module | Purpose | Status |
|---|---|---|
| `parser/` | Binary packet parsing (Ticker + Quote + Full + OI + PrevClose + Disconnect) | KEEP (drop market_depth, deep_depth parsers) |
| `websocket/` | Main-feed connection + order-update connection | SHRINK (drop depth WS code, drop pool — only 1 main + 1 order = 2 conns) |
| `auth/` | Token manager, TOTP, SSM | KEEP (drop Valkey path) |
| **`option_chain/`** | **NEW — REST fetcher, 50s scheduler, RAM cache, audit writer** | NEW |
| **`cross_verify/`** | **NEW — 15:31 IST scheduler, morning 1d scheduler, comparator** | NEW |
| `pipeline/` | Tick processor (SPSC 100K buffer), candle aggregator (21 TFs locked) | MODIFY (slim TFs, resize ring) |
| `notification/` | Telegram events, NotificationService | KEEP (drop deleted event variants) |

**Deleted:** `instrument/` (entire dir — bhavcopy, csv_downloader, universe_builder, depth_strike_selector, depth_rebalancer, live_tick_atm_resolver, prev_oi loader, phase2_scheduler, market_open_self_test stock checks, depth_20_dynamic_subscriber, depth_200_dynamic_subscriber, depth_dynamic_top_volume_selector), `historical/` (cross_verify lives in new `cross_verify/`; candle_fetcher kept but moved inside new dir; backfill already deleted), `index_constituency/`, `network/` (IP monitor + verifier — keep ONLY the boot-time IP check).

### §2.3 `crates/trading/` (~5 modules — was 4)

| Module | Purpose | Status |
|---|---|---|
| `oms/` | Engine, API client, state machine, rate limiter, idempotency, **<1ms decision-to-wire latency target** | KEEP + tighten |
| `risk/` | Pre-trade checks, P&L tracker, kill switch | KEEP |
| `indicator/` | O(1) yata-based engine (RSI/MACD/BB/SMA/EMA × 21 TFs) | KEEP + wire to bar_cache (Wave-7A4 scaffold) |
| `strategy/` | FSM evaluator, **strategy fail-closed gate** on option chain staleness | KEEP + add gate |
| **`in_mem/`** | **bar_cache (today + yesterday sealed bars in RAM), option_chain_cache (3 underlyings × strikes), indicator_state** | NEW (consolidates Wave-7A4 scaffold) |

**Deleted:** `greeks/` (entire dir — inline_computer, aggregator; keep `black_scholes.rs` as a leaf library), all movers code.

### §2.4 `crates/storage/` (~8 modules — was 12)

| Module | Purpose | Status |
|---|---|---|
| `tick_persistence.rs` | ILP writer for `ticks` table | KEEP (rescue ring resized to 100K) |
| `candle_persistence.rs` | ILP writer for 6 candle tables | KEEP (drop other TF writers) |
| **`option_chain_persistence.rs`** | **NEW — writes 3 option chain tables** | NEW |
| **`cross_verify_persistence.rs`** | **NEW — writes 3 cross-verify audit tables** | NEW |
| `order_audit_persistence.rs` | SEBI 5-year `order_audit` writer | KEEP |
| `auth_renewal_audit_persistence.rs` | Auth audit writer | KEEP |
| `boot_audit_persistence.rs` | Boot audit writer | KEEP |
| `selftest_audit_persistence.rs` | Self-test audit writer | KEEP |
| `previous_close_persistence.rs` | Previous-close writer | KEEP |
| **`cloudwatch_metrics.rs`** | **NEW — PutMetricData wrapper for the 10 locked metrics** | NEW |
| **`cloudwatch_logs.rs`** | **NEW — JSON log writer that the CW Logs agent tails** | NEW |

**Deleted:** `valkey_cache.rs` (cache replaced by in-memory + SSM), `materialized_views.rs` (no movers / no depth matviews), `deep_depth_persistence.rs`, `movers_persistence.rs`, `indicator_snapshot_persistence.rs`, all phase2/depth audit persistence files, `instrument_persistence.rs`, `calendar_persistence.rs` (calendar is a `const` slice).

### §2.5 `crates/api/` (~3 modules — was 7)

| Module | Purpose | Status |
|---|---|---|
| `handlers/health.rs` | `GET /health` for CloudWatch alarm | KEEP |
| `handlers/quote.rs` | `GET /api/quote/{security_id}` for 4 SIDs | KEEP (slim) |
| `state.rs` | Shared app state | KEEP |
| `middleware.rs` | Auth + request tracing | KEEP |

**Deleted:** `handlers/stats.rs`, `handlers/movers.rs` (entire endpoint family), `handlers/instruments.rs` (no instrument rebuild), `handlers/index_constituency.rs`, `handlers/option_chain.rs` (no HTTP — strategy reads RAM cache directly), `handlers/static_file.rs` (no portal — Grafana gone).

### §2.6 `crates/app/` (~3 modules — was 4)

| Module | Purpose | Status |
|---|---|---|
| `main.rs` | 8-step boot (was 15), shutdown handler | REWRITE (slim) |
| `observability.rs` | CloudWatch agent integration + tracing subscriber | MODIFY (drop OpenTelemetry / Jaeger / Prometheus exporters) |
| **`schedulers.rs`** | **NEW — consolidates 15:31 IST cross-verify + 08:05 IST morning 1d + 50s option chain loops** | NEW |
| `infra.rs` | Docker health checks, QuestDB readiness | KEEP |

**Deleted:** `trading_pipeline.rs` (subsumed into new schedulers.rs), `phase2_recovery.rs`, `depth_dynamic_pipeline_v2.rs`, `depth_20_conn_spawner.rs`, `boot_smoke_test.rs` (subsumed into self-test), `movers_pipeline.rs`, `greeks_pipeline.rs`, `prev_day_cache_loader.rs` (replaced by simpler in_mem loader), `subsystem_memory.rs` (CloudWatch reports memory).

---

## §3. New boot sequence (8 steps — was 15)

```
   T+0   instance state=running (from EventBridge cron 08:00 IST)
         systemd starts tickvault.service
         │
         ▼
   T+5s  Step 1: Config + CryptoProvider + Tracing subscriber
   T+8s  Step 2: CloudWatch Logs agent confirmed running on host
   T+10s Step 3: Auth (Valkey gone → in-memory cache + SSM fetch + TOTP fallback)
   T+12s Step 4: QuestDB DDL (idempotent CREATE TABLE IF NOT EXISTS + ALTER ADD COLUMN IF NOT EXISTS for the 14 tables)
   T+15s Step 5: Main feed WS connect (4 SIDs subscribe in Ticker mode)
   T+20s Step 6: Order update WS connect (auth via MsgCode 42)
   T+25s Step 7: Spawn schedulers (option chain 50s loop, 15:31 cross-verify, 08:05 morning 1d, market-open self-test 09:16:30)
   T+30s Step 8: HTTP /health endpoint up; emit BootReadyConfirmation Telegram
         │
         ▼
   T+30s Boot complete; tickvault enters steady-state
```

**Removed steps:** universe build (CSV download/parse), historical 90-day fetch (cold-path; runs separately via scheduler now), instrument master refresh, network IP monitor (boot-time check kept; ongoing monitor dropped — CloudWatch handles).

---

## §4. New `config/base.toml` (8 sections — was 17)

```toml
# config/base.toml — POST-RESTRUCTURE (2026-05-18)

[app]
log_level = "info"
shutdown_grace_secs = 30

[dhan]
client_id = "..."  # from SSM
api_base = "https://api.dhan.co/v2"
ws_main_feed = "wss://api-feed.dhan.co"
ws_order_update = "wss://api-order-update.dhan.co"

[questdb]
ilp_addr = "127.0.0.1:9009"
pg_wire = "127.0.0.1:8812"

[option_chain]                    # NEW heart-piece block
cadence_secs = 50
underlyings = [
  { security_id = 13, name = "NIFTY",     segment = "IDX_I" },
  { security_id = 25, name = "BANKNIFTY", segment = "IDX_I" },
  { security_id = 51, name = "SENSEX",    segment = "IDX_I" },
]
max_cache_age_secs = 60
fetch_timeout_secs = 5
retry_count = 1
backoff_ladder_secs = [10, 20, 40, 80]

[cross_verify]                    # NEW
intraday_run_ist = "15:31:00"
morning_run_ist = "08:05:00"
timeframes_intraday = ["1m", "5m", "15m", "1h"]
tolerance_price = 0.0             # zero per rule
tolerance_volume = 0.0

[telemetry]                       # replaces prom/grafana/loki blocks
cloudwatch_namespace = "tickvault/prod"
cloudwatch_log_group = "/tickvault/prod/app"
sns_topic_arn = "arn:aws:sns:ap-south-1:...:tv-prod-alerts"
sns_dlq_arn = "arn:aws:sqs:ap-south-1:...:tv-prod-sns-dlq"

[strategy]
dry_run = true                    # always Phase 0; flip to false post-Phase-1
fail_closed_on_stale_chain = true

[risk]
daily_loss_threshold_inr = 5000
max_open_positions = 10
```

**Deleted sections:** `[valkey]`, `[prometheus]`, `[grafana]`, `[loki]`, `[alertmanager]`, `[depth_20]`, `[depth_200]`, `[greeks]`, `[movers]`, `[instrument]` (CSV-related), `[subscription]` (replaced by hard-coded `LOCKED_UNIVERSE`), `[network]` (IP monitor reduced to boot-time), `[notification]` (sub-fields moved into `[telemetry]`).

---

## §5. New ErrorCode catalog (slim)

| Prefix | Count | Notes |
|---|---|---|
| `DH-*` (Dhan trading) | 10 | All kept (DH-901..910) |
| `DATA-*` (Dhan data API) | 13 | Subset kept (807/810/813/814 are the active ones; rare ones can stay) |
| `AUTH-*` | 4 | AUTH-GAP-01..04 kept |
| `WS-*` | 6 | WS-GAP-01..06 kept (minus depth-specific) |
| `STORAGE-*` | 3 | STORAGE-GAP-01..03 kept |
| `RISK-*` | 3 | RISK-GAP-01..03 kept |
| `BOOT-*` | 3 | BOOT-01..03 kept |
| `OMS-*` | 6 | OMS-GAP-01..06 kept |
| `SELFTEST-*` | 3 | Kept |
| `AUDIT-*` | 3 | Slim subset (orderaudit/bootaudit/selftestaudit failures) |
| **`OPTION-CHAIN-*`** | **8** | **NEW per heart-piece §8** |
| **`CROSS-VERIFY-*`** | **4** | **NEW per cross-verify §14.4** |
| **`TELEGRAM-*`** | 2 | kept |
| **Total** | **~68 variants** | (was ~135 — 50% reduction) |

**Deleted prefixes:** `I-P0-*`, `I-P1-*`, `I-P2-*` (instrument issues — universe is now compile-time), `PHASE2-*`, `DEPTH-*`, `DEPTH-DYN-*`, `DEPTH-20-DYN-*`, `DEPTH-200-DYN-*`, `DEPTH200-SMOKE-*`, `GAP-FILL-*`, `MOVERS-*` (already mostly gone), `PHASE2-READY-*`, `VOLUME-MONO-*` (NSE bhavcopy cross-check gone), `BAR-MISMATCH-*` (this is INTRADAY cross-verify replacement), `PREVCLOSE-*` (slim to 1 variant), `PREVOI-*`, `SLO-*` (composite score not needed at 4 SIDs — direct alarms are enough), `ORPHAN-POSITION-*` (operator decides; can keep), `CORE-PIN-*` (1-core pin works on 2 vCPU), `HOT-PATH-*` (lints catch), `AGGREGATOR-*` (slim).

---

## §6. NotificationEvent catalog (slim)

| Variant | Severity | Status |
|---|---|---|
| `BootReadyConfirmation` | Info | KEEP |
| `MarketOpenStreamingConfirmation` | Info | KEEP |
| `SelfTestPassed` / `Failed` | Info / Critical | KEEP |
| `WebSocketDisconnected` / `OffHours` | High / Low | KEEP |
| `TokenForceRenewal` | Low | KEEP |
| `QuestDBDisconnected` | High | KEEP |
| `OptionChainFetched` | Info | NEW |
| `OptionChainStaleHaltStrategy` | Critical | NEW |
| `OptionChainBackoffExhausted` | High | NEW |
| `OptionChainExpiryRollover` | Info | NEW |
| `CandleCrossMatchPassed` (15:31 IST) | Info | KEEP |
| `CandleCrossMatchFailed` (15:31 IST) | Critical | KEEP |
| `Morning1dCheckPassed` | Info | NEW |
| `Morning1dCheckFailed` | Critical | NEW |
| `OrderPlaced` / `Filled` / `Rejected` | Info / High | KEEP (when live) |
| `KillSwitchEngaged` | Critical | KEEP |
| `DailyLossThresholdHit` | Critical | KEEP |
| `EodDigest` (15:45 IST daily) | Info | KEEP |
| Plus ~5 boot-gate variants | various | KEEP |
| **Total** | | **~25 variants** (was ~80) |

**Deleted:** `Phase2*`, `Depth*`, `Greeks*`, `Movers*`, `MidMarketBootComplete` (boot is uniform now), `Depth20DynamicSwap*`, `RealtimeGuaranteeDegraded` etc.

---

## §7. New Prometheus → CloudWatch metric mapping

Per `aws-indices-only-locked-architecture.md` §4 we have 10 CloudWatch metrics (free tier). Mapping:

| Old Prometheus metric (deleted with Prom) | New CloudWatch metric | Notes |
|---|---|---|
| `tv_tick_processing_duration_ns` | dropped — sampled via log INFO line | |
| `tv_websocket_connections_active` | `tv_websocket_connections_active` | gauge |
| `tv_orders_placed_total` | `tv_orders_placed_total` | counter |
| `tv_daily_pnl` | `tv_daily_pnl_inr` | gauge |
| `tv_questdb_disconnected_seconds` | `tv_questdb_disconnected_seconds` | gauge |
| `tv_token_remaining_seconds` | `tv_token_remaining_hours` | gauge |
| `tv_main_feed_subscribed_total` | dropped — fixed at 4 | |
| `tv_aggregator_seals_emitted_total` | dropped — log line at seal boundary | |
| 100+ other Prom metrics | dropped — CW free tier has 10 | |

Plus 4 new ones for the heart piece + cross-verify:
- `tv_option_chain_cache_age_seconds`
- `tv_option_chain_fetch_failure_total`
- `tv_cross_verify_mismatches_total`
- `tv_strategy_skipped_total`

---

## §8. Restructure PR sequencing (12 PRs)

Building on the deletion-surface map's 11-PR sequence, the final 12-PR plan is:

| # | PR | Direction | Notes |
|---|---|---|---|
| 0 | Add `[option_chain]` + `[cross_verify]` + `[telemetry]` config blocks (NEW); skeleton ErrorCode variants + stub ratchet tests | ADD | Cement new contracts BEFORE deleting old |
| 1 | Delete movers pipeline + tables | DELETE | Lowest risk; already partly retired |
| 2 | Delete Greeks inline pipeline (keep black_scholes.rs lib) | DELETE | Decoupled |
| 3 | Delete depth-20 / depth-200 dynamic + supervisor + WS depth conns | DELETE | Big chunk |
| 4 | Delete Phase 2 dispatcher + scheduler + emit guard | DELETE | |
| 5 | Delete bhavcopy + prev_OI + live_tick_atm_resolver | DELETE | Heavy loaders out |
| 6 | Replace universe builder + CSV + rkyv cache → 4-line `LOCKED_UNIVERSE` const | REPLACE | Largest single LoC chunk |
| 7 | Tighten SubscriptionScope to `Indices4Only`; drop 218 NSE_EQ + 25 display indices from planner | TIGHTEN | Final universe lock |
| 8 | Build new `option_chain/` module (REST fetcher + 50s scheduler + RAM cache + audit writer) | ADD | Heart piece live |
| 9 | Build new `cross_verify/` module (15:31 IST + 08:05 IST schedulers + comparator + 3 audit tables) | ADD | Z+ L3 reconcile |
| 10 | Cutover observability: delete Grafana/Prom/Alertmanager/Loki/Alloy/Traefik/Valkey from docker-compose; add CloudWatch agent + alarms Terraform | CUTOVER | Big visible change |
| 11 | Rescue ring resize 5M → 100K; right-size remaining buffers | RESIZE | |
| 12 | Terraform: switch instance type to t4g.medium ARM; schedule every-day 08:00/17:00; new alarms; new IAM (drop S3 backup of instrument master) | TERRAFORM | AWS-side cutover |
| 13 | Strategy fail-closed gate + bar_cache wire | LAST | RAM-first invariant enforced |

**Each PR is independently reversible. Total: ~36K LoC removed + ~6K LoC added (net -30K LoC).** Calendar: 6-8 weeks at 1 PR/session per operator-charter §H serial completion.

---

## §9. The slim 7-layer Z+ defense (per subsystem)

Maintained at the slimmer footprint:

| Subsystem | L1 DETECT | L2 VERIFY | L3 RECONCILE | L4 PREVENT | L5 AUDIT | L6 RECOVER | L7 COOLDOWN |
|---|---|---|---|---|---|---|---|
| Main feed WS | CW gauge `tv_ws_connections_active` | first tick within 5s of subscribe | daily streaming confirmation 09:15:30 IST | static IP gate | `boot_audit` ws step | reconnect with SubscribeRxGuard | exponential backoff |
| Option chain REST | CW gauge `tv_option_chain_cache_age` | LTP vs WS LTP within 0.5% | 15:31 reconciliation | token age check | `option_chain_request_audit` | DH-904 backoff ladder | skip-next-cycle |
| Tick persistence | CW counter `tv_tick_persist_errors_total` | row-count match | daily cross-verify | ring buffer 100K | `ticks` table | spill NDJSON → DLQ | 60s drain |
| Candle aggregation | CW gauge `tv_candle_seal_lag_secs` | candle count per session | 15:31 cross-verify (the L3 for THIS subsystem) | boundary timer | `candles_*` tables | systemd restart | — |
| Order placement | <1ms decision-to-wire | ACK from order update WS | daily P&L vs Dhan portfolio | static IP + rate limiter | `order_audit` (SEBI 5y) | OMS retry + circuit breaker | GCRA |

---

## §10. Honest envelope (operator-charter §F)

> "100% inside the restructured envelope: 4 SIDs (NIFTY/BANKNIFTY/SENSEX/INDIA VIX), 6 candle TFs + ticks, option chain 50s cadence, 15:31 IST + 08:05 IST cross-verify gates, t4g.medium ARM ap-south-1, CloudWatch observability, 2-container Docker (tickvault + QuestDB). Hot path: <1ms decision-to-wire; tick persistence bounded zero loss via 100K ring → NDJSON spill → DLQ; option chain cache freshness ≤60s with strategy fail-closed beyond; cross-verify zero-tolerance OHLCV match daily.
>
> **NOT promised:** literal "WebSocket never disconnects" (SEBI 24h JWT forces reconnect daily); literal "<1ms order ACK from Dhan" (their server processing 5-30ms typical, beyond our control); region-wide AWS ap-south-1 outage (accepted envelope; cross-region SNS deferred per LOCK-7); strategy survival on a corrupted historical day (we fail-closed instead — operator must manually resume after morning 1d cross-check passes)."

---

## §11. Trigger / auto-load

This rule activates when editing:
- Any file under `crates/`
- `config/base.toml`
- `deploy/docker/docker-compose.yml`
- `deploy/aws/terraform/*.tf`
- Any plan file under `.claude/plans/aws-lifecycle/`
