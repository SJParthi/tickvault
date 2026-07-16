//! Configuration structs deserialized from `config/base.toml`.
//!
//! Every runtime value that might differ between environments lives here.
//! Secrets are NEVER in config — they come from AWS SSM Parameter Store.

use anyhow::{Result, bail};
use chrono::NaiveTime;
use serde::Deserialize;

use crate::constants::{
    CADENCE_CHAIN_MIN_SPACING_FLOOR_MS, CADENCE_GROWW_WAVE_STEP_MS,
    CADENCE_SPOT_WINDOW_CAP_CEILING, CADENCE_SPOT_WINDOW_MS, DHAN_DATA_API_DEFAULT_TARGET_RPS,
    DHAN_DATA_API_RPS_CEILING, DHAN_DATA_API_RPS_FLOOR, SEBI_MAX_ORDERS_PER_SECOND,
};
use crate::trading_calendar::TradingCalendar;

/// Root application configuration.
// D2b: `Clone` so the runtime Dhan-lane cold-start can capture an owned
// `Arc<ApplicationConfig>` snapshot at boot (the supervisor outlives `main()`'s
// owned `config` borrow). Config is loaded ONCE from figment; the clone is a
// deep copy of immutable boot config — no env re-parse, no drift.
#[derive(Debug, Clone, Deserialize)]
pub struct ApplicationConfig {
    pub trading: TradingConfig,
    pub dhan: DhanConfig,
    pub websocket: WebSocketConfig,
    pub questdb: QuestDbConfig,
    // `valkey: ValkeyConfig` field DELETED in #O4 (2026-05-24).
    // `prometheus: PrometheusConfig` field DELETED in #O5 (2026-05-30) — Prometheus container removed in #O3; the /metrics exporter (observability.rs, port 9091) is independent and stays for CloudWatch.
    pub network: NetworkConfig,
    pub token: TokenConfig,
    pub risk: RiskConfig,
    pub logging: LoggingConfig,
    pub instrument: InstrumentConfig,
    pub api: ApiConfig,
    // PR-C3 (2026-07-14): the `subscription` field (SubscriptionConfig /
    // SubscriptionScope) was DELETED with the Dhan instrument-download +
    // subscription chain (operator retirement directive 2026-07-13,
    // scope-lock amendment §B item 2). Groww's watch set is built from its
    // own master; there is no Dhan WS subscription to configure.
    #[serde(default)]
    pub notification: NotificationConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
    #[serde(default)]
    pub historical: HistoricalDataConfig,
    #[serde(default)]
    pub strategy: StrategyConfig,
    #[serde(default)]
    pub index_constituency: IndexConstituencyConfig,
    #[serde(default)]
    pub greeks: GreeksConfig,
    #[serde(default)]
    pub infrastructure: InfrastructureConfig,
    #[serde(default)]
    pub partition_retention: PartitionRetentionConfig,
    // PR #2 (2026-05-18): `movers: MoversConfig` retired alongside the
    // deleted movers pipeline. The [movers] section in base.toml was
    // also removed.
    /// Wave 1 C9 feature flags — operator-flippable rollback toggles.
    /// 14 flags spanning Wave 1, Wave 2 and Wave 3 items.
    #[serde(default)]
    pub features: FeaturesConfig,
    /// Wave-5 in-memory store §K-L8 (PR #504c) — runtime-tunable
    /// timeframe list driving the in-memory `CascadeFanout`. Default
    /// is the 21-TF set per L6 (drops 1s/3s/5s/10s/15s/30s seconds
    /// engines per L7).
    #[serde(default)]
    pub engine: EngineConfig,
    /// Wave-5 in-memory store §K-L10 (PR #504d) — runtime-tunable
    /// per-instrument tick capacity for `TickStorage`. Default 5_000
    /// covers the busiest contract's daily tick count without
    /// triggering Vec realloc.
    #[serde(default)]
    pub in_mem: InMemConfig,
    /// Pluggable market-data feed selection (Groww second-feed scope,
    /// operator lock 2026-06-19 — see
    /// `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`).
    /// `[feeds]` toggles which feed providers spawn at boot: Dhan
    /// (feed #1, default ON, unchanged) and Groww (feed #2, default
    /// OFF). A missing `[feeds]` section keeps today's Dhan-only
    /// behaviour byte-identical.
    #[serde(default)]
    pub feeds: FeedsConfig,
    /// `[scoreboard]` — dual-feed daily scoreboard (operator directive
    /// 2026-07-10: run Dhan + Groww live for a month, everything tracked,
    /// captured, blame-attributed). Aggregation-only in PR-A (reads
    /// existing tables), so `enabled` defaults ON — safe-on; flipping it
    /// off is the whole-subsystem rollback switch (B12).
    #[serde(default)]
    pub scoreboard: ScoreboardConfig,
    /// `[brutex_crossverify]` — daily BruteX↔TickVault Groww 1-minute
    /// cross-verification (operator authorization tracked in
    /// `.claude/rules/project/brutex-crossverify-error-codes.md`). Reads
    /// BruteX-produced OHLCV CSVs from S3 at 15:50 IST and compares them
    /// against live `candles_1m` rows tagged `feed='groww'`. Default OFF —
    /// a missing section keeps today's behaviour byte-identical; flipping
    /// `enabled = false` is the whole-subsystem rollback switch (B12).
    #[serde(default)]
    pub brutex_crossverify: BrutexCrossverifyConfig,
    /// `[spot_1m_rest]` — per-minute spot 1m REST pipeline (operator grant
    /// 2026-07-12, `no-rest-except-live-feed-2026-06-27.md` §8): every
    /// trading-day minute close in session, fetch that just-closed minute's
    /// official 1m OHLCV for the 3 IDX_I spot indices via Dhan
    /// `POST /v2/charts/intraday` and persist to `spot_1m_rest`. Absent
    /// section ⇒ DISABLED (fail-safe default off).
    #[serde(default)]
    pub spot_1m_rest: Spot1mRestConfig,
    /// `[oms_reconcile]` — scheduled OMS order-book reconciliation loop
    /// (2026-07-14): a timer arm in the trading pipeline periodically calls
    /// `reconcile()` so OMS state converges with the Dhan order book.
    /// Absent section ⇒ DISABLED (fail-safe default off).
    #[serde(default)]
    pub oms_reconcile: OmsReconcileConfig,
    /// `[dhan_data_api]` — shared Dhan Data-API REST pacing (operator
    /// pacing directive 2026-07-14): the process-wide token-bucket limiter
    /// every per-minute Dhan Data-API REST fire passes through (spot-1m
    /// fires + ladder re-polls + the 15:33:30 sweep + the #1524 diagnostic
    /// probes + the option-chain fires). Absent section ⇒ the directed
    /// 3 rps default. Dhan-ONLY — Groww untouched.
    #[serde(default)]
    pub dhan_data_api: DhanDataApiConfig,
    /// `[option_chain_1m]` — per-minute option-chain REST pipeline (operator
    /// grant 2026-07-12, `no-rest-except-live-feed-2026-06-27.md` §8; PR-3,
    /// the OPTION-CHAIN half). Config-gated DEFAULT-OFF pending the
    /// first-live-boot entitlement probe (the account had no Option Chain
    /// Data-API entitlement in June 2026 — DH-902/806 class — and the
    /// entitlement is unprobeable from the dev sandbox). Absent section ⇒
    /// pipeline disabled + probe-and-report ON.
    #[serde(default)]
    pub option_chain_1m: OptionChain1mConfig,
    /// `[groww_spot_1m]` — Groww per-minute spot 1m REST leg (operator grant
    /// 2026-07-13, `.claude/plans/active-plan-groww-rest-1m.md` PR-2): every
    /// trading-day minute close in session, fetch that just-closed minute's
    /// official 1m OHLCV for the 3 spot indices via Groww
    /// `GET /v1/historical/candles` and persist to `spot_1m_rest` tagged
    /// `feed='groww'`. Independent of the Dhan lane (spawned process-global;
    /// a Dhan-off session still runs it). Absent section ⇒ DISABLED
    /// (fail-safe default off).
    #[serde(default)]
    pub groww_spot_1m: GrowwSpot1mConfig,
    /// `[groww_option_chain_1m]` — Groww per-minute option-chain REST leg
    /// (operator grant 2026-07-13, `.claude/plans/active-plan-groww-rest-1m.md`
    /// PR-3): every trading-day minute close in session — sequenced after
    /// the Groww spot leg — fetch the CURRENT-expiry option chain for the
    /// 3 underlyings via Groww `GET /v1/option-chain/...` and persist to
    /// the EXISTING `option_chain_1m` table tagged `feed='groww'`. Shipped
    /// DEFAULT-OFF pending the first live probe (the endpoint is
    /// documented-available — unlike Dhan's entitlement question — but the
    /// live shape/latency are UNVERIFIED). Absent section ⇒ pipeline
    /// disabled + probe-and-report ON.
    #[serde(default)]
    pub groww_option_chain_1m: GrowwOptionChain1mConfig,
    /// `[tf_consistency]` — daily timeframe-consistency verifier (operator
    /// directive 2026-07-13: *"how will you guarantee that all our defined
    /// timeframes internally are correct"*). At 15:40 IST every trading day,
    /// recompute every sealed higher-TF candle (2m..4h, both feeds) from its
    /// stored `candles_1m` constituents and compare exactly; findings land
    /// in `tf_consistency_audit` + one Telegram summary. Cold path only.
    /// Absent section ⇒ DISABLED (fail-safe default off).
    #[serde(default)]
    pub tf_consistency: TfConsistencyConfig,
    /// `[rest_candle_fold]` — REST-era multi-TF candle derivation (operator
    /// directive 2026-07-16: *"why the fuck remaining candles 1m till 1day
    /// is not yet generated and populated — resolve these"*). Folds
    /// persist-confirmed `spot_1m_rest` 1m bars into all 21 `candles_*`
    /// timeframes via the shared seal-writer channel, with a boot catch-up
    /// over the last `catchup_days` of stored bars. Cold path only. Absent
    /// section ⇒ DISABLED (fail-safe default off); `config/base.toml` opts
    /// in.
    #[serde(default)]
    pub rest_candle_fold: RestCandleFoldConfig,
    /// `[market_ram_store]` — RAM residency stores (operator directive
    /// 2026-07-16: *"how can i believe you that you have all these already
    /// available in our in-memory app RAM — especially for the current day
    /// and even in the future last one month data should be entirely in
    /// memory app RAM"*): the month-deep spot bar rings (fed by the
    /// rest_candle_fold emit path — zero new QuestDB reads) + the
    /// current-day chain minute ring (fed by the chain legs' publish path,
    /// rehydrated at boot from `option_chain_1m`). Cold path only. Absent
    /// section ⇒ DISABLED (fail-safe default off); `config/base.toml` opts
    /// in. RAMSTORE-01 runbook:
    /// `.claude/rules/project/ram-store-error-codes.md`.
    #[serde(default)]
    pub market_ram_store: MarketRamStoreConfig,
    /// `[groww_contract_1m]` — Groww per-minute PER-CONTRACT 1m candle REST
    /// leg (operator grant 2026-07-13,
    /// `.claude/plans/active-plan-groww-rest-1m.md` PR-4 — the fill-model
    /// leg): every trading-day minute close in session — sequenced after
    /// the Groww CHAIN leg (its per-minute `underlying_ltp` is the ATM
    /// anchor) — fetch the just-closed minute's 1m candle for a BOUNDED
    /// ATM-window selection of option contracts via Groww
    /// `GET /v1/historical/candles` (`segment=FNO`) and persist to the NEW
    /// `option_contract_1m_rest` table tagged `feed='groww'`. Requires the
    /// chain leg (`[groww_option_chain_1m] enabled = true`) — without it
    /// there is no anchor and the leg is refused loudly at spawn. Absent
    /// section ⇒ DISABLED (fail-safe default off).
    #[serde(default)]
    pub groww_contract_1m: GrowwContract1mConfig,
    /// `[groww_rest_burst]` — the 2026-07-14 Groww REST burst auto-ladder
    /// (operator approval "approved and go ahead with the recommendation";
    /// `no-rest-except-live-feed-2026-06-27.md` §9.7): which burst tier the
    /// per-minute Groww REST legs fire in (`two_wave` default /
    /// `seven_concurrent` probe-gated) + the pre-boundary TLS warm-up
    /// toggle. Absent section ⇒ `two_wave` + warm-up off — rate-safe
    /// because the wave instants are computed from the millisecond clock
    /// (`wave_sleep_from_now_ms`), so the > 1 s two_wave separation holds
    /// with or without the warm-up recompute (LOW-1 wording fix
    /// 2026-07-14; the pre-CRITICAL-1 whole-second else-branch could
    /// collapse it).
    #[serde(default)]
    pub groww_rest_burst: GrowwRestBurstConfig,
    /// `[groww_universe]` — process-global daily Groww watch-set +
    /// shared-master rider (2026-07-15 Groww live-feed retirement re-home of
    /// the activation watcher's daily build loop): once per IST day, build +
    /// write `data/groww/groww-watch-<date>.json` (the spot leg's VIX
    /// resolver reads it) and fire-and-forget `persist_groww_instruments`
    /// (SEBI `feed='groww'` master continuity). Absent section ⇒ DISABLED
    /// (fail-safe default off); `config/base.toml` opts in.
    #[serde(default)]
    pub groww_rest_burst: GrowwRestBurstConfig,
    /// `[groww_orders]` — Groww ORDER-SIDE build gate (operator authorization
    /// 2026-07-14, `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`
    /// §39). GATE 1 of the 4-gate live-fire lattice: every key default-OFF, so
    /// an absent section leaves the entire Groww order-side dark. Read-only
    /// order/portfolio/margin/user GETs are per-area config-gated + market-hours
    /// -only when enabled; live order placement is hard-locked behind Gates
    /// 2 (cargo feature) + 3 (the `GROWW_ORDER_LIVE_FIRE` const) regardless of
    /// this config. Absent section ⇒ fully DISABLED (fail-safe default off).
    #[serde(default)]
    pub groww_orders: GrowwOrdersConfig,
    /// `[dhan_margin_gate]` — 🔷 DHAN pre-trade margin gate (operator
    /// directive 2026-07-14, relayed via the coordinator session — the
    /// Funds & Margin surface runs as its own dedicated build; umbrella
    /// plan cluster E2). Code-ready DEFAULT-OFF: even with
    /// `enabled = true` the REST legs stay dark until the code-change
    /// master lock `DHAN_MARGIN_GATE_REST_ALLOWED` (constants.rs) flips
    /// with a fresh dated operator quote. Absent section ⇒ DISABLED
    /// (fail-safe default off).
    #[serde(default)]
    pub dhan_margin_gate: DhanMarginGateConfig,
    /// `[exit_orders]` — 🔷 DHAN exit-order execution layer (Cluster B,
    /// 2026-07-14; `.claude/rules/project/dhan-exit-order-lockout-2026-07-14.md`).
    /// LOCK #1 of the 4-lock OFF switch: default OFF; absent section =
    /// disabled (fail-safe). The app-crate dispatcher drops every
    /// `ExitCommand` while disabled; enabling activates DRY-RUN PAPER
    /// behavior only (the engine's hardcoded `dry_run` blocks live POSTs).
    #[serde(default)]
    pub exit_orders: ExitOrdersConfig,
    /// `[cadence]` — broker-agnostic fetch-cadence + decision-timing
    /// scheduler (operator cadence directive 2026-07-14, reshaped by the
    /// 2026-07-16 post-close burst directive — `crates/core/src/cadence/`;
    /// supersedes the rev-8 pre-close schedule — 2026-07-16). Dry-run
    /// decision-timing skeleton: per minute close T, BOTH lanes fire
    /// POST-CLOSE — primary (rung 0) = ALL 7 requests concurrent in the
    /// burst second (3 chains + 4 spots; Dhan at T +
    /// `dhan_burst_offset_ms`, Groww at T+0); fallback (rung 1) = chains
    /// in second 1, all 4 spots in second 2. Demotion is
    /// RateLimited-ONLY after 2 consecutive dirty cycles (operator
    /// Correction 2); structural zero-429 gates + event-driven per-lane
    /// decisions throughout. This PR ships NO REST caller — the dry-run
    /// executors log fires and return Empty. Absent section ⇒ DISABLED
    /// (fail-safe default off).
    #[serde(default)]
    pub cadence: CadenceConfig,
}

/// `[feeds]` — pluggable market-data feed selection (operator lock
/// 2026-06-19, Groww second feed). Each feed provider is independently
/// enable/disable-able so the operator can run Dhan-only (default),
/// Groww-only, or both in parallel. Mirrors the `NotificationConfig`
/// simple-boolean-with-`Default`-impl convention.
///
/// `dhan_enabled` defaults to `true` (the existing system is UNCHANGED);
/// `groww_enabled` defaults to `false` so a fresh deployment behaves
/// exactly like today until Groww is explicitly switched on. Groww is
/// native tickvault Rust reusing the same WAL/ring/spill/DLQ/aggregator
/// chain — brutex is a design reference only, no code pulled.
#[derive(Debug, Clone, Deserialize)]
pub struct FeedsConfig {
    /// Dhan live feed (feed #1). Default ON — disabling it is only for
    /// isolated Groww-only testing.
    pub dhan_enabled: bool,
    /// Groww feed identity gate. The Groww LIVE feed was RETIRED
    /// 2026-07-15 (operator directive: "remove the whole Groww live feed;
    /// keep only spot 1m and option chain for both brokers"). This flag does
    /// NOT gate the Groww REST legs (each is section-gated by its own
    /// `enabled` key); it feeds the boot-completed feed gate, the scoreboard
    /// `feed_off` day classification, and the /feeds page display only, and
    /// rides alongside the `[groww_universe]` daily watch-set rider (which
    /// has its OWN gate). Default OFF.
    pub groww_enabled: bool,
}

impl Default for FeedsConfig {
    fn default() -> Self {
        Self {
            dhan_enabled: true,
            groww_enabled: false,
        }
    }
}

/// `[scoreboard]` — dual-feed daily scoreboard (operator directive
/// 2026-07-10). All fields serde-defaulted so a missing section is the
/// sensible day-1 shape (zero manual setup). `enabled = true` is SAFE-ON:
/// the PR-A subsystem only READS existing tables (`ws_event_audit`,
/// `ticks`) plus its own additive forensic tables — it touches no hot
/// path, no order path, no feed lifecycle. Flipping `enabled = false` is
/// the whole-subsystem rollback (nothing spawns; B12 rollback test
/// `scoreboard_flag_rollback` pins the default + the off shape).
#[derive(Debug, Clone, Deserialize)]
pub struct ScoreboardConfig {
    /// Master switch for the whole subsystem (boot reconciler + 15:45 IST
    /// daily aggregation + Telegram scorecard).
    #[serde(default = "default_scoreboard_enabled")]
    pub enabled: bool,
    /// Send the daily Telegram scorecard (the aggregation + tables still
    /// run when this is off — forensic record without the ping).
    #[serde(default = "default_scoreboard_enabled")]
    pub telegram_enabled: bool,
    /// Populate the per-instrument `feed_coverage_daily` detail rows
    /// (~1.5K/day). Consumed by PR-4 (presence registry); the table +
    /// flag ship in PR-A so the config surface is stable.
    #[serde(default = "default_scoreboard_enabled")]
    pub coverage_detail_rows: bool,
    /// Hot-path per-tick presence fold (PR-4). `false` ⇒ coverage via SQL
    /// totals only; per-instrument unique-wins "unavailable".
    #[serde(default = "default_scoreboard_enabled")]
    pub presence_fold_enabled: bool,
    /// Groww exchange→receipt lag histogram fold (PR-3). Until it ships,
    /// lag columns carry the −1 "not measured" sentinel.
    #[serde(default = "default_scoreboard_enabled")]
    pub groww_lag_enabled: bool,
    /// IST seconds-of-day for the daily aggregation trigger. Default
    /// 56_700 = 15:45:00 IST (after the 15:31 cross-verify + the 15:40
    /// tick-conservation audit).
    #[serde(default = "default_scoreboard_trigger_secs")]
    pub trigger_secs_of_day_ist: u32,
}

fn default_scoreboard_enabled() -> bool {
    true
}
fn default_scoreboard_trigger_secs() -> u32 {
    15 * 3600 + 45 * 60 // 56_700 = 15:45:00 IST
}

impl Default for ScoreboardConfig {
    fn default() -> Self {
        Self {
            enabled: default_scoreboard_enabled(),
            telegram_enabled: default_scoreboard_enabled(),
            coverage_detail_rows: default_scoreboard_enabled(),
            presence_fold_enabled: default_scoreboard_enabled(),
            groww_lag_enabled: default_scoreboard_enabled(),
            trigger_secs_of_day_ist: default_scoreboard_trigger_secs(),
        }
    }
}

/// `[brutex_crossverify]` — daily BruteX↔TickVault Groww 1-minute
/// cross-verification. At 15:50 IST the runner lists + downloads the
/// BruteX-produced OHLCV CSVs from S3 (`s3://<bucket>/<prefix>/<date>/…`)
/// and compares them cell-by-cell (paise-integer, tolerance INCLUSIVE)
/// against live `candles_1m` rows tagged `feed='groww'`. All fields
/// serde-defaulted so a missing section is safe. `enabled = false` is
/// SAFE-OFF: nothing spawns until the operator flips it — the flip back
/// to `false` is the whole-subsystem rollback switch (B12; pinned by
/// `brutex_crossverify_flag_rollback`).
#[derive(Debug, Clone, Deserialize)]
pub struct BrutexCrossverifyConfig {
    /// Master switch for the whole subsystem (15:50 IST daily runner +
    /// forensic tables + Telegram summary + `/crossverify` page data).
    /// Default OFF — cold-path S3 reads only begin once enabled.
    #[serde(default = "default_brutex_xverify_enabled")]
    pub enabled: bool,
    /// Send the daily Telegram summary (the comparison + tables still run
    /// when this is off — forensic record without the ping).
    #[serde(default = "default_brutex_xverify_telegram_enabled")]
    pub telegram_enabled: bool,
    /// S3 bucket carrying the BruteX-produced CSVs. Reuses the existing
    /// cold-archive bucket (instance role already has read access —
    /// zero IAM change).
    #[serde(default = "default_brutex_xverify_bucket")]
    pub bucket: String,
    /// Key prefix under the bucket; the runner lists
    /// `<prefix>/<YYYY-MM-DD>/` for the trading day.
    #[serde(default = "default_brutex_xverify_prefix")]
    pub prefix: String,
    /// IST seconds-of-day for the daily trigger. Default 57_000 =
    /// 15:50:00 IST (after the 15:31 cross-verify, the 15:40
    /// tick-conservation audit and the 15:45 scoreboard).
    #[serde(default = "default_brutex_xverify_trigger_secs")]
    pub trigger_secs_of_day_ist: u32,
    /// IST seconds-of-day wall-clock cap. An empty S3 listing re-polls
    /// every `repoll_interval_secs` until this cap (default 57_900 =
    /// 16:05:00 IST — well before the AWS 16:30 IST auto-stop), then the
    /// day records NO_DATA / degraded honestly (stage `wall_clock_cap`).
    #[serde(default = "default_brutex_xverify_deadline_secs")]
    pub deadline_secs_of_day_ist: u32,
    /// Seconds between re-polls while the day's S3 listing is empty.
    #[serde(default = "default_brutex_xverify_repoll_secs")]
    pub repoll_interval_secs: u64,
    /// Per-object size cap (bytes). A CSV larger than this is refused
    /// (degraded, loud) — bounds memory on a corrupt/hostile object.
    #[serde(default = "default_brutex_xverify_max_object_bytes")]
    pub max_object_bytes: u64,
    /// Cap on the number of listed keys per day (bounds a runaway
    /// producer; beyond it the run degrades loudly).
    #[serde(default = "default_brutex_xverify_max_keys")]
    pub max_keys: u32,
    /// Bounded download attempts per S3 object (with backoff) before
    /// that object is counted failed for the day.
    #[serde(default = "default_brutex_xverify_fetch_attempts")]
    pub fetch_attempts_per_object: u32,
    /// INCLUSIVE per-cell price tolerance in paise (integer compare —
    /// `|live - brutex| <= tolerance` matches). Default 0 = exact match.
    #[serde(default = "default_brutex_xverify_price_tolerance_paise")]
    pub price_tolerance_paise: i64,
    /// Classify volume divergences. Default OFF — Groww live volume is
    /// always 0 (LTP-only feed), so volume classification for the groww
    /// feed is hard-refused regardless of this flag; both sides are
    /// still STORED for forensics.
    #[serde(default = "default_brutex_xverify_compare_volume")]
    pub compare_volume: bool,
}

fn default_brutex_xverify_enabled() -> bool {
    false
}
fn default_brutex_xverify_telegram_enabled() -> bool {
    true
}
fn default_brutex_xverify_bucket() -> String {
    String::from("tv-prod-cold")
}
fn default_brutex_xverify_prefix() -> String {
    String::from("crossverify/groww")
}
fn default_brutex_xverify_trigger_secs() -> u32 {
    15 * 3600 + 50 * 60 // 57_000 = 15:50:00 IST
}
fn default_brutex_xverify_deadline_secs() -> u32 {
    16 * 3600 + 5 * 60 // 57_900 = 16:05:00 IST
}
fn default_brutex_xverify_repoll_secs() -> u64 {
    120
}
fn default_brutex_xverify_max_object_bytes() -> u64 {
    5 * 1024 * 1024 // 5 MiB per CSV object
}
fn default_brutex_xverify_max_keys() -> u32 {
    2_000
}
fn default_brutex_xverify_fetch_attempts() -> u32 {
    3
}
fn default_brutex_xverify_price_tolerance_paise() -> i64 {
    0
}
fn default_brutex_xverify_compare_volume() -> bool {
    false
}

impl Default for BrutexCrossverifyConfig {
    fn default() -> Self {
        Self {
            enabled: default_brutex_xverify_enabled(),
            telegram_enabled: default_brutex_xverify_telegram_enabled(),
            bucket: default_brutex_xverify_bucket(),
            prefix: default_brutex_xverify_prefix(),
            trigger_secs_of_day_ist: default_brutex_xverify_trigger_secs(),
            deadline_secs_of_day_ist: default_brutex_xverify_deadline_secs(),
            repoll_interval_secs: default_brutex_xverify_repoll_secs(),
            max_object_bytes: default_brutex_xverify_max_object_bytes(),
            max_keys: default_brutex_xverify_max_keys(),
            fetch_attempts_per_object: default_brutex_xverify_fetch_attempts(),
            price_tolerance_paise: default_brutex_xverify_price_tolerance_paise(),
            compare_volume: default_brutex_xverify_compare_volume(),
        }
    }
}

/// `[spot_1m_rest]` — per-minute spot 1m REST pipeline (operator grant
/// 2026-07-12; PR-2, the SPOT half). Cold path only — the WS candle
/// pipeline is untouched.
///
/// Fail-safe shape: `enabled` is `#[serde(default)]` = `false`, so an
/// absent `[spot_1m_rest]` section (or a TOML written before this PR)
/// disables the fetcher entirely. `config/base.toml` explicitly sets
/// `enabled = true`.
///
/// Extension point (PR-3, option chain): every FUTURE field on this
/// struct MUST also be `#[serde(default)]` so older TOMLs keep
/// deserializing byte-identically — nothing chain-specific ships in
/// PR-2; the chain PR adds its own knobs here without a config-surface
/// break (the nested-sub-table precedent).
#[derive(Debug, Clone, Deserialize)]
pub struct Spot1mRestConfig {
    /// Master switch for the per-minute spot 1m REST fetcher. Default
    /// OFF (fail-safe) — `config/base.toml` turns it on explicitly.
    #[serde(default)]
    pub enabled: bool,
    /// 2026-07-14 serving-delay diagnostics rider: one-shot LOG-ONLY
    /// side-by-side + alternate-window probes (≤6 bounded extra requests
    /// per day) that discriminate "Dhan serves same-day intraday candles
    /// with a DELAY" from "our request shape is wrong". Default OFF
    /// (fail-safe); `config/base.toml` opts in while the 2026-07-14
    /// all-morning `empty` signature is under investigation. Never touches
    /// the fetch/persist/edge behaviour.
    #[serde(default)]
    pub diagnostics: bool,
    /// IST seconds-of-day of the SECOND one-shot diagnostics probe (the
    /// first fires at the first session fire after boot). Default 11:00
    /// IST — mid-session, far from both the open and the 15:31 cross-verify
    /// burst. Inert while `diagnostics = false`.
    #[serde(default = "default_spot1m_diagnostics_second_probe_secs")]
    pub diagnostics_second_probe_secs_of_day_ist: u32,
    /// 2026-07-14 architecture optionality (pending the ~15:40 IST
    /// sweep-discriminator operator ruling): `per_minute` (default —
    /// today's behaviour) fires each minute close; `batch_catchup`
    /// replaces the per-minute fires with a sweep-style catch-up every
    /// [`Self::batch_interval_minutes`] — one day-window fetch per SID
    /// through the shared Dhan Data-API limiter, persisting everything
    /// new above the per-SID persisted watermark. A CONFIG MODE, not a
    /// rewrite (reuses the existing sweep machinery).
    #[serde(default)]
    pub fetch_mode: SpotFetchMode,
    /// Batch catch-up cadence (minutes) — inert while
    /// `fetch_mode = "per_minute"`. Default 5; validated to 1..=60.
    #[serde(default = "default_spot1m_batch_interval_minutes")]
    pub batch_interval_minutes: u32,
}

/// `[spot_1m_rest] fetch_mode` values (2026-07-14 — see the field doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpotFetchMode {
    /// Fire each in-session minute close (the shipped PR-2 behaviour).
    #[default]
    PerMinute,
    /// Sweep-style catch-up every `batch_interval_minutes` instead of
    /// per-minute fires (the batch-architecture option the ~15:40 IST
    /// sweep-discriminator verdict may select).
    BatchCatchup,
}

/// Serde default for [`Spot1mRestConfig::batch_interval_minutes`] — 5.
fn default_spot1m_batch_interval_minutes() -> u32 {
    5
}

impl Spot1mRestConfig {
    /// Boot-time validation — the batch cadence must be a sane in-session
    /// interval (1..=60 minutes).
    ///
    /// # Errors
    /// Returns a descriptive error when `batch_interval_minutes` is
    /// outside `1..=60`.
    pub fn validate(&self) -> Result<()> {
        if !(1..=60).contains(&self.batch_interval_minutes) {
            bail!(
                "spot_1m_rest.batch_interval_minutes ({}) must be within 1..=60",
                self.batch_interval_minutes
            );
        }
        Ok(())
    }
}

/// `[oms_reconcile]` — scheduled OMS order-book reconciliation
/// (2026-07-14): a `tokio::select!` timer arm in the trading pipeline
/// periodically calls `reconcile()` so OMS state converges with the Dhan
/// order book (the OMS-GAP-02 machinery gains its first scheduled caller).
///
/// Triple-gated fail-safe shape: this config defaults OFF (an absent
/// `[oms_reconcile]` section keeps today's behaviour byte-identical), the
/// OMS dry-run flag short-circuits `reconcile()` before any HTTP, and the
/// trading-pipeline spawn itself is dhan-gated.
#[derive(Debug, Clone, Deserialize)]
pub struct OmsReconcileConfig {
    /// Master switch. Default OFF (fail-safe) — an absent section or an
    /// empty `[oms_reconcile]` section keeps the loop disabled.
    #[serde(default)]
    pub enabled: bool,
    /// Seconds between scheduled reconcile runs. Validated 60..=3600 at
    /// boot — the 60s floor bounds any future live-mode order-book GET
    /// rate to ≤1/min (≤375/session), trivially inside the Data-API budget.
    #[serde(default = "default_oms_reconcile_interval_secs")]
    pub interval_secs: u64,
    /// Gate runs to [09:15, 15:30) IST on NSE trading days (Rule 3
    /// market-hours-aware; holiday-correct via the trading calendar).
    /// Default true.
    #[serde(default = "default_oms_reconcile_trading_hours_only")]
    pub trading_hours_only: bool,
}

/// Serde default for [`OmsReconcileConfig::interval_secs`] — 300 (5 min).
fn default_oms_reconcile_interval_secs() -> u64 {
    300
}

/// Serde default for [`OmsReconcileConfig::trading_hours_only`] — true.
fn default_oms_reconcile_trading_hours_only() -> bool {
    true
}

impl Default for OmsReconcileConfig {
    /// Manual impl so `Default` matches the serde field defaults exactly
    /// (a derived `Default` would zero `interval_secs` while an empty
    /// `[oms_reconcile]` section deserializes it to 300).
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: default_oms_reconcile_interval_secs(),
            trading_hours_only: default_oms_reconcile_trading_hours_only(),
        }
    }
}

impl OmsReconcileConfig {
    /// Boot-time validation — the reconcile cadence must be a sane
    /// in-session interval (60..=3600 seconds).
    ///
    /// # Errors
    /// Returns a descriptive error when `interval_secs` is outside
    /// `60..=3600`.
    pub fn validate(&self) -> Result<()> {
        if !(60..=3600).contains(&self.interval_secs) {
            bail!(
                "oms_reconcile.interval_secs ({}) must be within 60..=3600",
                self.interval_secs
            );
        }
        Ok(())
    }
}

/// `[dhan_data_api]` — shared Dhan Data-API REST pacing (operator pacing
/// directive 2026-07-14, relayed via the coordinator session: pace Dhan to
/// 3 requests/sec — tunable DOWN to 2 — spread overflow into the next
/// second(s), route the option-chain API through the SAME limiter, with
/// incremental/decremental self-tuning: "if it accepts max 3 or 2, stick
/// to that and split it up").
///
/// `target_rps` is the tuning CAP: the shared limiter starts here and the
/// self-tuner steps down to the [`DHAN_DATA_API_RPS_FLOOR`] on observed
/// 429 bursts / back up toward this cap on clean streaks. Legal range
/// [`DHAN_DATA_API_RPS_FLOOR`]..=[`DHAN_DATA_API_RPS_CEILING`] (2..=4),
/// rejected at boot by [`Self::validate`]. Absent section ⇒ the directed
/// 3 rps default. Dhan-ONLY — the Groww legs keep their own budgets.
#[derive(Debug, Clone, Deserialize)]
pub struct DhanDataApiConfig {
    /// Target (cap) requests/second for the shared Dhan Data-API limiter.
    #[serde(default = "default_dhan_data_api_target_rps")]
    pub target_rps: u32,
}

/// Serde default for [`DhanDataApiConfig::target_rps`] — the directed 3.
fn default_dhan_data_api_target_rps() -> u32 {
    DHAN_DATA_API_DEFAULT_TARGET_RPS
}

impl Default for DhanDataApiConfig {
    fn default() -> Self {
        Self {
            target_rps: default_dhan_data_api_target_rps(),
        }
    }
}

impl DhanDataApiConfig {
    /// Boot-time validation — the pacing cap must stay inside the
    /// operator-directed ladder (2..=4; 5/sec is the ACCOUNT budget and is
    /// deliberately never claimable by this process alone).
    ///
    /// # Errors
    /// Returns a descriptive error when `target_rps` is outside the legal
    /// range.
    pub fn validate(&self) -> Result<()> {
        if !(DHAN_DATA_API_RPS_FLOOR..=DHAN_DATA_API_RPS_CEILING).contains(&self.target_rps) {
            bail!(
                "dhan_data_api.target_rps ({}) must be within {}..={}",
                self.target_rps,
                DHAN_DATA_API_RPS_FLOOR,
                DHAN_DATA_API_RPS_CEILING
            );
        }
        Ok(())
    }
}

/// Serde default for [`Spot1mRestConfig::diagnostics_second_probe_secs_of_day_ist`]
/// — 11:00 IST as seconds-of-day.
fn default_spot1m_diagnostics_second_probe_secs() -> u32 {
    11 * 3600
}

impl Default for Spot1mRestConfig {
    /// Manual impl so `Default` matches the serde field defaults exactly
    /// (a derived `Default` would zero the second-probe instant while an
    /// empty `[spot_1m_rest]` section deserializes it to 11:00 IST).
    fn default() -> Self {
        Self {
            enabled: false,
            diagnostics: false,
            diagnostics_second_probe_secs_of_day_ist: default_spot1m_diagnostics_second_probe_secs(
            ),
            fetch_mode: SpotFetchMode::default(),
            batch_interval_minutes: default_spot1m_batch_interval_minutes(),
        }
    }
}

/// Days after which the operator's freeze-limit review is considered
/// stale (>90 days ⇒ one boot-time WARN at trading-pipeline init —
/// design Ruling 6 amendment, 2026-07-14).
const FREEZE_REVIEW_STALE_DAYS: i64 = 90;

/// Serde default for [`ExitOrdersConfig::mpp_verify_deadline_secs`] — 30.
fn default_mpp_verify_deadline_secs() -> u64 {
    30
}

/// Serde default for [`ExitOrdersConfig::mpp_verify_max_attempts`] — 5
/// (the 1, 2, 4, 8, 10 ladder: 25s cumulative inside the 30s deadline).
fn default_mpp_verify_max_attempts() -> u32 {
    5
}

/// `[exit_orders]` — 🔷 DHAN exit-order execution layer (Cluster B,
/// 2026-07-14 — LOCK #1 of the 4-lock OFF switch;
/// `.claude/rules/project/dhan-exit-order-lockout-2026-07-14.md`).
///
/// Fail-safe: absent section = disabled (every field is
/// `#[serde(default)]`-safe). The app-crate dispatcher
/// (`crates/app/src/exit_execution.rs`) drops every `ExitCommand` while
/// `enabled = false`; flipping to `true` activates DRY-RUN PAPER behavior
/// ONLY (the engine's hardcoded `dry_run: true` blocks every live POST) —
/// a flip is never a silent no-op: one boot log line names the mode.
/// The ENGINE stays config-free (per-call parameters only — policy lives
/// in the app layer, design Ruling 8).
#[derive(Debug, Clone, Deserialize)]
pub struct ExitOrdersConfig {
    /// Master switch for the exit-order dispatcher. Default OFF
    /// (fail-safe) — `config/base.toml` carries the section with
    /// `enabled = false` as the ratchet's non-vacuous scan surface.
    #[serde(default)]
    pub enabled: bool,
    /// Exchange freeze quantity (operator-supplied; NO Dhan-side constant
    /// exists in-repo — Verified V10). `0` = unset; [`Self::validate`]
    /// requires `>= 1` when `enabled`. The per-underlying MAP is deferred
    /// to Cluster A (Ruling 6 amendment); a per-call `freeze_limit`
    /// parameter on `place_order_sliced` always wins over this scalar.
    #[serde(default)]
    pub default_freeze_limit_qty: i64,
    /// `"YYYY-MM-DD"` the operator last verified the freeze limit against
    /// the NSE qtyfreeze file. `>90` days stale ⇒ one boot-time WARN
    /// (via [`freeze_review_is_stale`], logged at trading-pipeline init).
    #[serde(default)]
    pub freeze_limits_reviewed_on: String,
    /// MPP verify-after-place deadline (seconds) — past it a still-PENDING
    /// order classifies `PendingAtLimit` (orders.md rule 18: a MARKET
    /// order auto-converted to LIMIT is NEVER assumed filled).
    #[serde(default = "default_mpp_verify_deadline_secs")]
    pub mpp_verify_deadline_secs: u64,
    /// Verify-ladder probe budget (1-indexed rungs of
    /// `exit_rules::next_verify_backoff_secs`). Default 5 → the
    /// 1, 2, 4, 8, 10 ladder (25s cumulative inside the 30s deadline).
    #[serde(default = "default_mpp_verify_max_attempts")]
    pub mpp_verify_max_attempts: u32,
    /// Default trailing jump for brackets (`0.0` = no trailing).
    #[serde(default)]
    pub default_trailing_jump: f64,
}

impl Default for ExitOrdersConfig {
    /// Manual impl so `Default` matches the serde field defaults exactly
    /// (a derived `Default` would zero the verify deadline/attempts while
    /// an empty `[exit_orders]` section deserializes them to 30/5).
    fn default() -> Self {
        Self {
            enabled: false,
            default_freeze_limit_qty: 0,
            freeze_limits_reviewed_on: String::new(),
            mpp_verify_deadline_secs: default_mpp_verify_deadline_secs(),
            mpp_verify_max_attempts: default_mpp_verify_max_attempts(),
            default_trailing_jump: 0.0,
        }
    }
}

impl ExitOrdersConfig {
    /// Boot-time validation (design §3.6).
    ///
    /// Always: `mpp_verify_deadline_secs` in `1..=300`;
    /// `mpp_verify_max_attempts` in `1..=8`; `default_trailing_jump`
    /// finite and `>= 0.0`. When `enabled`: `default_freeze_limit_qty >= 1`
    /// and `freeze_limits_reviewed_on` parses as `%Y-%m-%d`.
    ///
    /// # Errors
    /// Returns a descriptive error on the first violated bound.
    pub fn validate(&self) -> Result<()> {
        self.validate_with_today(ist_date_from_utc(chrono::Utc::now()))
    }

    /// Deterministic core of [`Self::validate`] — `today_ist` is INJECTED
    /// so tests never read the wall clock (flake root-cause hardening,
    /// refuter round 2 2026-07-14: the L2 future-review-date check
    /// compared hardcoded test dates against `Utc::now()`, so a run on a
    /// host clock before 2026-07-14 IST — skew, or a session straddling
    /// IST midnight — could flip an enabled-config `validate()` verdict).
    /// Production goes through [`Self::validate`], which supplies the
    /// real IST calendar day.
    ///
    /// # Errors
    /// Returns a descriptive error on the first violated bound.
    pub fn validate_with_today(&self, today_ist: chrono::NaiveDate) -> Result<()> {
        if !(1..=300).contains(&self.mpp_verify_deadline_secs) {
            bail!(
                "exit_orders.mpp_verify_deadline_secs ({}) must be within 1..=300",
                self.mpp_verify_deadline_secs
            );
        }
        if !(1..=8).contains(&self.mpp_verify_max_attempts) {
            bail!(
                "exit_orders.mpp_verify_max_attempts ({}) must be within 1..=8",
                self.mpp_verify_max_attempts
            );
        }
        if !(self.default_trailing_jump.is_finite() && self.default_trailing_jump >= 0.0) {
            bail!(
                "exit_orders.default_trailing_jump ({}) must be finite and >= 0.0",
                self.default_trailing_jump
            );
        }
        if self.enabled {
            if self.default_freeze_limit_qty < 1 {
                bail!(
                    "exit_orders.default_freeze_limit_qty ({}) must be >= 1 when \
                     exit_orders.enabled = true (operator-supplied exchange freeze quantity)",
                    self.default_freeze_limit_qty
                );
            }
            match chrono::NaiveDate::parse_from_str(&self.freeze_limits_reviewed_on, "%Y-%m-%d") {
                Err(_) => {
                    bail!(
                        "exit_orders.freeze_limits_reviewed_on ('{}') must be a YYYY-MM-DD date \
                         when exit_orders.enabled = true",
                        self.freeze_limits_reviewed_on
                    );
                }
                Ok(reviewed) => {
                    // L2 (2026-07-14 hostile review): a FUTURE review date is
                    // a typo/backdating error — it would silence the >90-day
                    // staleness WARN forever. IST calendar day (market-hours
                    // rule) — injected by the caller; `validate()` supplies
                    // `ist_date_from_utc(Utc::now())`.
                    if reviewed > today_ist {
                        bail!(
                            "exit_orders.freeze_limits_reviewed_on ('{}') is in the future \
                             (IST today is {today_ist}) — the review date must be today or past",
                            self.freeze_limits_reviewed_on
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

/// Pure staleness check for the operator's freeze-limit review date
/// (>[`FREEZE_REVIEW_STALE_DAYS`] days ⇒ stale). Empty or unparsable
/// `reviewed_on` is STALE (fail-safe — the WARN fires rather than a
/// silent pass). The WARN itself is logged at trading-pipeline init,
/// not here (this function is pure — zero I/O).
pub fn freeze_review_is_stale(reviewed_on: &str, today: chrono::NaiveDate) -> bool {
    match chrono::NaiveDate::parse_from_str(reviewed_on, "%Y-%m-%d") {
        Ok(reviewed) => (today - reviewed).num_days() > FREEZE_REVIEW_STALE_DAYS,
        Err(_) => true,
    }
}

/// `[tf_consistency]` — daily timeframe-consistency verifier (operator
/// directive 2026-07-13). Cold path only — the live candle pipeline, tick
/// capture and trading are untouched; the verifier READS `candles_*` and
/// writes ONLY its own `tf_consistency_audit` table.
///
/// Fail-safe shape: `enabled` is `#[serde(default)]` = `false`, so an
/// absent `[tf_consistency]` section (or a TOML written before this PR)
/// disables the verifier entirely. `config/base.toml` explicitly sets
/// `enabled = true`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TfConsistencyConfig {
    /// Master switch for the daily 15:40 IST timeframe-consistency
    /// verifier. Default OFF (fail-safe) — `config/base.toml` turns it on
    /// explicitly.
    #[serde(default)]
    pub enabled: bool,
}

/// `[rest_candle_fold]` — REST-era multi-TF candle derivation (operator
/// directive 2026-07-16). Cold path only — folds persist-confirmed
/// `spot_1m_rest` 1m bars into the 21 `candles_*` tables through the shared
/// seal-writer channel; NEVER touches `ticks` (live-feed-purity rules 1-6
/// stand; rule 10 carries the dated 2026-07-16 candles_1d edit).
///
/// Fail-safe shape: `enabled` is `#[serde(default)]` = `false`, so an
/// absent `[rest_candle_fold]` section (or a TOML written before this PR)
/// disables the fold entirely. `config/base.toml` explicitly sets
/// `enabled = true` + `catchup_days = 35` (the operator's one-month spot
/// window demand, 2026-07-16).
#[derive(Debug, Clone, Deserialize)]
pub struct RestCandleFoldConfig {
    /// Master switch for the REST-era bar-fold candle derivation.
    /// Default OFF (fail-safe) — `config/base.toml` turns it on explicitly.
    #[serde(default)]
    pub enabled: bool,
    /// Boot catch-up window in IST days: the fold re-derives all 21 TFs
    /// from the last `catchup_days` calendar days of `spot_1m_rest` rows
    /// per feed — today plus `catchup_days - 1` past days, EXACTLY
    /// `catchup_days` days total (round-2 LOW-3: the code matches this
    /// wording; the old range folded one extra day). Default 35 (one month
    /// of spot history + weekend slack — the operator's 2026-07-16
    /// minimum-one-month demand).
    #[serde(default = "default_rest_candle_fold_catchup_days")]
    pub catchup_days: u32,
}

impl Default for RestCandleFoldConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            catchup_days: default_rest_candle_fold_catchup_days(),
        }
    }
}

/// serde default for [`RestCandleFoldConfig::catchup_days`] — 35 days.
fn default_rest_candle_fold_catchup_days() -> u32 {
    35
}

impl RestCandleFoldConfig {
    /// Boot-time sanity validation — rejected BEFORE the fold task spawns.
    /// The window must be ≥1 day and ≤370 (~one year — the envelope bound;
    /// a larger window is a config typo, not a legitimate ask).
    ///
    /// # Errors
    /// Returns a descriptive error when `catchup_days` is outside `1..=370`.
    pub fn validate(&self) -> Result<()> {
        if !(1..=370).contains(&self.catchup_days) {
            bail!(
                "rest_candle_fold.catchup_days ({}) must be within 1..=370",
                self.catchup_days
            );
        }
        Ok(())
    }
}

/// `[market_ram_store]` — RAM residency stores (operator directive
/// 2026-07-16; PR-2 of the data-completeness build). Two process-RAM
/// stores populated by EXISTING data flows:
///
/// - the SPOT month-deep bar rings
///   (`tickvault_trading::in_mem::spot_bar_store`) — per (feed, sid, tf)
///   rings of sealed bars, capacity `spot_days` × session bars/day,
///   written at the rest_candle_fold emit choke points (live seals +
///   refold re-emits + the boot catch-up — so pre-market rehydration is
///   PR-1's existing catch-up, ZERO new QuestDB reads for spots);
/// - the CHAIN current-day minute ring
///   (`tickvault_core::pipeline::chain_day_store`) — per (feed,
///   underlying) minute → published moneyness snapshot, current IST day
///   only, boot-rehydrated from today's `option_chain_1m` rows via
///   bounded hardened `/exec` windows.
///
/// Fail-safe shape: `enabled` is `#[serde(default)]` = `false`, so an
/// absent `[market_ram_store]` section disables both stores entirely
/// (every hook is a checked no-op). `config/base.toml` explicitly sets
/// `enabled = true`, `spot_days = 35`, `chain_row_cap = 1000`.
#[derive(Debug, Clone, Deserialize)]
pub struct MarketRamStoreConfig {
    /// Master switch for BOTH RAM residency stores.
    /// Default OFF (fail-safe) — `config/base.toml` turns it on explicitly.
    #[serde(default)]
    pub enabled: bool,
    /// Spot ring depth in trading days per (feed, sid, tf) ring. Default 35
    /// (the operator's minimum-one-month spot demand + weekend slack —
    /// matches `rest_candle_fold.catchup_days`). A value BELOW
    /// `rest_candle_fold.catchup_days` is legal (the ring simply retains
    /// less than the catch-up offers — noted with a boot log line, never a
    /// hard error).
    #[serde(default = "default_market_ram_store_spot_days")]
    pub spot_days: u32,
    /// Hard per-minute row cap for the chain day store (rows = strike-leg
    /// snapshot rows per published minute per (feed, underlying)). Default
    /// 1_000 — above the structural 800-row publish bound
    /// (`MAX_STRIKES_PER_CHAIN` 400 × 2 legs), so truncation fires only on
    /// a hostile/runaway snapshot, LOUDLY (counted + coded warn).
    #[serde(default = "default_market_ram_store_chain_row_cap")]
    pub chain_row_cap: u32,
}

impl Default for MarketRamStoreConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            spot_days: default_market_ram_store_spot_days(),
            chain_row_cap: default_market_ram_store_chain_row_cap(),
        }
    }
}

/// serde default for [`MarketRamStoreConfig::spot_days`] — 35 days.
fn default_market_ram_store_spot_days() -> u32 {
    35
}

/// serde default for [`MarketRamStoreConfig::chain_row_cap`] — 1_000 rows.
fn default_market_ram_store_chain_row_cap() -> u32 {
    1_000
}

impl MarketRamStoreConfig {
    /// Boot-time sanity validation — rejected BEFORE any store installs.
    ///
    /// # Errors
    /// Returns a descriptive error when `spot_days` is outside `1..=370`
    /// (the same ~one-year envelope bound as `rest_candle_fold.catchup_days`)
    /// or `chain_row_cap` is outside `1..=10_000`.
    pub fn validate(&self) -> Result<()> {
        if !(1..=370).contains(&self.spot_days) {
            bail!(
                "market_ram_store.spot_days ({}) must be within 1..=370",
                self.spot_days
            );
        }
        if !(1..=10_000).contains(&self.chain_row_cap) {
            bail!(
                "market_ram_store.chain_row_cap ({}) must be within 1..=10000",
                self.chain_row_cap
            );
        }
        Ok(())
    }
}

/// `[option_chain_1m]` — per-minute option-chain REST pipeline (operator
/// grant 2026-07-12; PR-3, the OPTION-CHAIN half). Cold path only — the
/// WS candle pipeline, tick capture and trading are untouched.
///
/// Semantics (the honest reading of the operator's "auto-enables/reports"
/// intent WITHOUT a silent behaviour change — documented in the module doc
/// of `crates/app/src/option_chain_1m_boot.rs`):
/// - `enabled = true` → run the per-minute pipeline. The boot-time
///   entitlement probe still runs FIRST; an entitlement-class reject
///   (DH-902 / DATA 806) fires ONE edge-triggered page and keeps the
///   pipeline down for the day.
/// - `enabled = false` + `probe_and_report = true` (the DEFAULT) → at boot
///   (Dhan lane up, trading day) run ONE expirylist probe, report the
///   verdict via Telegram (entitled → "flip the setting on"; not entitled
///   → names the DH-902/806 class), then exit. The full pipeline NEVER
///   auto-runs while `enabled = false` — the operator flips the config.
///
/// Fail-safe shape: both fields `#[serde(default)]`-covered, so an absent
/// `[option_chain_1m]` section (or a TOML written before this PR)
/// deserializes to `{ enabled: false, probe_and_report: true }`.
#[derive(Debug, Clone, Deserialize)]
pub struct OptionChain1mConfig {
    /// Master switch for the per-minute option-chain fetcher. Default OFF
    /// (pending the live entitlement probe) — flipping the DEFAULT needs a
    /// fresh dated operator quote.
    #[serde(default)]
    pub enabled: bool,
    /// When the pipeline is disabled, still run the ONE boot-time
    /// expirylist entitlement probe and report the verdict via Telegram.
    /// Default ON so the operator learns the entitlement state on the
    /// first live boot without enabling the pipeline.
    #[serde(default = "default_chain_1m_probe_and_report")]
    pub probe_and_report: bool,
}

/// serde default for [`OptionChain1mConfig::probe_and_report`] — ON.
fn default_chain_1m_probe_and_report() -> bool {
    true
}

/// `[groww_spot_1m]` — Groww per-minute spot 1m REST leg (operator grant
/// 2026-07-13; PR-2 of the Groww per-minute REST plan). Cold path only —
/// the WS pipelines, tick capture and trading are untouched.
///
/// Fail-safe shape: `enabled` is `#[serde(default)]` = `false`, so an
/// absent `[groww_spot_1m]` section (or a TOML written before this PR)
/// disables the fetcher entirely. `config/base.toml` explicitly sets
/// `enabled = true` (the Dhan spot-leg precedent: spot on, chain gated).
///
/// Extension point (PR-3/PR-4, chain + contract legs): every FUTURE field
/// on this struct MUST also be `#[serde(default)]` so older TOMLs keep
/// deserializing byte-identically (the `Spot1mRestConfig` precedent).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GrowwSpot1mConfig {
    /// Master switch for the Groww per-minute spot 1m REST fetcher.
    /// Default OFF (fail-safe) — `config/base.toml` turns it on explicitly.
    #[serde(default)]
    pub enabled: bool,
}

impl Default for OptionChain1mConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            probe_and_report: default_chain_1m_probe_and_report(),
        }
    }
}

/// `[groww_option_chain_1m]` — Groww per-minute option-chain REST leg
/// (operator grant 2026-07-13; PR-3 of the Groww per-minute REST plan).
/// Cold path only — the WS pipelines, tick capture and trading are
/// untouched.
///
/// Config semantics mirror the Dhan `[option_chain_1m]` gate:
/// - `enabled = true` → run the per-minute chain pipeline (sequenced after
///   the Groww spot leg via the watch signal + fallback timer).
/// - `enabled = false` + `probe_and_report = true` (the default) → run ONE
///   bounded boot-time chain probe per underlying, report the measured
///   verdict (shape / strikes / latency / reject class) via an Info
///   Telegram + coded log, persist NOTHING, then exit. The pipeline NEVER
///   auto-runs while `enabled = false` — the operator flips the config
///   after the probe verdict.
///
/// DEFAULT-OFF rationale (dated 2026-07-13): the Groww chain endpoint is
/// documented-available (no Dhan-style entitlement question), but the live
/// response shape / strike-key format / latency / rate-limit family are
/// UNVERIFIED-LIVE (`docs/groww-ref/99-UNKNOWNS.md` U-4/U-11/U-12/U-13) —
/// the probe is the first live measurement. Flipping the DEFAULT needs a
/// fresh dated operator quote.
#[derive(Debug, Clone, Deserialize)]
pub struct GrowwOptionChain1mConfig {
    /// Master switch for the Groww per-minute chain fetcher. Default OFF
    /// (pending the first live probe).
    #[serde(default)]
    pub enabled: bool,
    /// When the pipeline is disabled, still run the ONE boot-time chain
    /// probe and report the measured verdict via Telegram. Default ON.
    #[serde(default = "default_chain_1m_probe_and_report")]
    pub probe_and_report: bool,
}

impl Default for GrowwOptionChain1mConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            probe_and_report: default_chain_1m_probe_and_report(),
        }
    }
}

/// `[groww_universe]` — process-global daily Groww watch-set + shared-master
/// rider (2026-07-15 Groww live-feed retirement, re-home of the retired
/// activation watcher's daily `build_and_write_groww_watch` loop + the sole
/// `persist_groww_instruments` caller). Cold path only — one build per IST
/// day; never the tick hot path, never a WebSocket.
///
/// Fail-safe shape: `enabled` is `#[serde(default)]` = `false`, so an absent
/// `[groww_universe]` section (or a TOML written before this PR) disables the
/// rider entirely. `config/base.toml` ships the section with `enabled = true`
/// (base opts in; the serde default stays OFF — the house fail-safe pattern).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GrowwUniverseConfig {
    /// Master switch for the daily watch-set build + shared-master persist
    /// rider. Default OFF (fail-safe).
    #[serde(default)]
    pub enabled: bool,
}

/// serde default for [`GrowwContract1mConfig::strikes_each_side`] — the
/// pinned [`crate::constants::GROWW_CONTRACT_1M_DEFAULT_STRIKES_EACH_SIDE`].
fn default_groww_contract_1m_strikes_each_side() -> u32 {
    crate::constants::GROWW_CONTRACT_1M_DEFAULT_STRIKES_EACH_SIDE
}

/// `[groww_contract_1m]` — Groww per-minute per-contract 1m candle REST
/// leg (operator grant 2026-07-13; PR-4 of the Groww per-minute REST plan
/// — the fill-model leg). Cold path only — the WS pipelines, tick capture
/// and trading are untouched.
///
/// Fail-safe shape: `enabled` is `#[serde(default)]` = `false`, so an
/// absent `[groww_contract_1m]` section (or a TOML written before this PR)
/// disables the fetcher entirely. `config/base.toml` ships the section
/// with `enabled = false` — the leg DEPENDS on the chain leg's per-minute
/// anchors, so it stays OFF until `[groww_option_chain_1m]` is live and
/// the operator flips this with a dated note.
#[derive(Debug, Clone, Deserialize)]
pub struct GrowwContract1mConfig {
    /// Master switch for the Groww per-minute contract candle fetcher.
    /// Default OFF (fail-safe; depends on the chain leg's anchors).
    #[serde(default)]
    pub enabled: bool,
    /// ATM window half-width: strikes selected EACH SIDE of the ATM strike
    /// per underlying (× CE+PE × 3 underlyings = the per-minute contract
    /// count). Default 2 → 30 contracts/minute = exactly the
    /// `GROWW_CONTRACT_1M_MAX_PER_MINUTE` envelope cap; a wider value is
    /// truncated deterministically nearest-ATM-first at the cap (counted +
    /// one coded warn, never fetched past it).
    #[serde(default = "default_groww_contract_1m_strikes_each_side")]
    pub strikes_each_side: u32,
}

impl Default for GrowwContract1mConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            strikes_each_side: default_groww_contract_1m_strikes_each_side(),
        }
    }
}

/// The Groww REST burst tier (2026-07-14 auto-ladder — operator approval
/// "approved and go ahead with the recommendation", relayed via the
/// coordinator session; contract `no-rest-except-live-feed-2026-06-27.md`
/// §9.7). Selects how the per-minute Groww spot + chain waves fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GrowwRestBurstTier {
    /// The SHIPPED default: 3 chain requests concurrently at minute close
    /// + 300 ms, 4 spot requests concurrently at close + 1,350 ms — the
    /// > 1 s wave separation keeps every rolling second single-wave
    /// (boundary burst ≤ 4 req/s).
    #[default]
    TwoWave,
    /// The operator-preferred burst — all 7 requests concurrently at
    /// close + 300 ms. PROBE-GATED: promotion requires the off-hours rate
    /// probe verdict + a fresh dated note in the §9.7 rule file; a live
    /// 429 auto-demotes the session back to `two_wave`.
    SevenConcurrent,
}

impl GrowwRestBurstTier {
    /// Static metric-label value (`tv_groww_rest_burst_tier_total{tier}`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TwoWave => "two_wave",
            Self::SevenConcurrent => "seven_concurrent",
        }
    }
}

/// `[groww_orders]` — Groww ORDER-SIDE build gate (operator authorization
/// 2026-07-14; `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`
/// §39, `.claude/rules/project/no-rest-except-live-feed-2026-06-27.md` §10).
///
/// This is GATE 1 of the 4-gate live-fire lattice (§39.2). Every field is
/// `#[serde(default)]` = `false`, so an absent `[groww_orders]` section (or a
/// TOML written before this build) leaves the ENTIRE Groww order-side dark —
/// no read-only order GET, no margin/portfolio poll, and (independently)
/// NO mutating order request. `config/base.toml` ships the section with every
/// key `false`.
///
/// Two independent classes of gate:
/// - Per-area READ-ONLY GETs (`orders_read`, `portfolio_read`, `margin_read`,
///   `user_read`) — order/trade list+detail+status, positions, holdings,
///   margins, user profile. When flipped `true` these run CONFIG-GATED +
///   MARKET-HOURS-ONLY (the cold-path scheduled-read discipline). They place
///   NO order.
/// - `live_fire_requested` — a DECLARED INTENT flag ONLY. It is IGNORED unless
///   Gate 3 (the hardcoded [`crate::constants::GROWW_ORDER_LIVE_FIRE`] const)
///   is ALSO flipped in source AND the `groww_orders` cargo feature (Gate 2)
///   is built in. Setting it `true` alone fires nothing — a config value can
///   never, by itself, place a live Groww order. Flipping the actual
///   live-orders enable is a SEPARATE, future, dated operator action that
///   edits §39 + Gate 3 first.
///
/// Extension point: every FUTURE field on this struct MUST also be
/// `#[serde(default)]` so older TOMLs keep deserializing byte-identically
/// (the `GrowwSpot1mConfig` / `Spot1mRestConfig` precedent).
#[derive(Debug, Clone, Deserialize)]
pub struct GrowwOrdersConfig {
    /// Read-only order/trade GETs (list, detail, status, status-by-reference,
    /// trades). Default OFF. Market-hours-gated when enabled.
    #[serde(default)]
    pub orders_read: bool,
    /// Read-only portfolio GETs (positions user + by-symbol, holdings).
    /// Default OFF. Market-hours-gated when enabled.
    #[serde(default)]
    pub portfolio_read: bool,
    /// Read-only margin GETs (user margin detail + margin calculator).
    /// Default OFF. Market-hours-gated when enabled.
    #[serde(default)]
    pub margin_read: bool,
    /// Read-only user-profile GET (the user-detail endpoint) + exceptions
    /// surface.
    /// Default OFF. Market-hours-gated when enabled.
    #[serde(default)]
    pub user_read: bool,
    /// DECLARED-INTENT flag for placing live Groww orders. IGNORED unless the
    /// hardcoded [`crate::constants::GROWW_ORDER_LIVE_FIRE`] const (Gate 3) is
    /// ALSO `true` AND the `groww_orders` cargo feature (Gate 2) is built —
    /// a config value alone can NEVER fire an order. Default OFF; flipping the
    /// real enable is a separate future dated operator action.
    #[serde(default)]
    pub live_fire_requested: bool,
    /// Read-only smart-order (GTT/OCO) GETs (get / list — the OCO reconcile
    /// poller's read surface). Default OFF. Market-hours-gated when enabled.
    /// (Smart Orders area, 2026-07-15.)
    #[serde(default)]
    pub smart_orders_read: bool,
    /// Smart-order (GTT/OCO) MUTATION intent flag (create / modify /
    /// cancel). Like `live_fire_requested`, IGNORED unless Gate 2 (the
    /// `groww_orders` cargo feature) + Gate 3 (the
    /// [`crate::constants::GROWW_ORDER_LIVE_FIRE`] const) are ALSO flipped —
    /// a config value alone can NEVER fire a smart-order mutation.
    /// Default OFF.
    #[serde(default)]
    pub smart_orders_write: bool,
    /// OCO reconcile poller cadence in seconds (the GROWW-OCO-05 poller's
    /// design value). Default 15.
    #[serde(default = "default_groww_oco_reconcile_poll_secs")]
    pub oco_reconcile_poll_secs: u64,
    /// OCO sibling-leg cancel verification deadline in seconds — past it an
    /// unverified sibling cancel is the GROWW-OCO-02 double-fill exposure
    /// window. Default 30.
    #[serde(default = "default_groww_oco_sibling_cancel_deadline_secs")]
    pub oco_sibling_cancel_deadline_secs: u64,
    /// Gates the zero-HTTP PAPER executor + intent ledger + paper reconciler
    /// (ledger-only). Default OFF. Deliberately SEPARATE from `orders_read`
    /// (which authorizes read GETs): paper mode makes ZERO HTTP calls,
    /// including GETs — the paper lane can NEVER reach any HTTP endpoint
    /// regardless of every other flag (enforced type-level: the reqwest
    /// transport lives only in `oms/groww/api_client.rs`, + an import-scan
    /// ratchet). Read GETs stay gated ONLY by the per-area `*_read` flags;
    /// `paper_enabled` neither enables nor blocks them. Live mutations require
    /// ALL of: the `groww_orders` cargo feature + an `orders_read`-area
    /// runtime + `live_fire_requested = true` + `GROWW_ORDER_LIVE_FIRE = true`
    /// — and are UNAFFECTED by `paper_enabled`. At the future live flip,
    /// `paper_enabled == true` together with live is REFUSED at boot (one
    /// account, one lane).
    #[serde(default)]
    pub paper_enabled: bool,
    /// Fail-closed maximum order quantity a single order may request. Default
    /// `0` = refuse-all (pending the operator's 0-vs-1 answer). A requested
    /// quantity above this is refused BEFORE any HTTP with `GROWW-ORD-09` —
    /// the fail-closed verdict for Groww's absent slicing endpoint (there is
    /// no client-side split). Raising it is a conscious config change;
    /// exchange freeze limits are exchange-published and changing, never
    /// hardcoded.
    #[serde(default)]
    pub max_order_quantity: i64,
}

fn default_groww_oco_reconcile_poll_secs() -> u64 {
    15
}

fn default_groww_oco_sibling_cancel_deadline_secs() -> u64 {
    30
}

impl Default for GrowwOrdersConfig {
    /// MANUAL impl (2026-07-15, Smart Orders area): the derived `Default`
    /// would zero the u64 cadences and break the Default↔serde-default
    /// parity (an absent `[groww_orders]` section must produce exactly
    /// these values). Every gate bool stays FALSE (Gate 1 dark default).
    fn default() -> Self {
        Self {
            orders_read: false,
            portfolio_read: false,
            margin_read: false,
            user_read: false,
            live_fire_requested: false,
            smart_orders_read: false,
            smart_orders_write: false,
            oco_reconcile_poll_secs: default_groww_oco_reconcile_poll_secs(),
            oco_sibling_cancel_deadline_secs: default_groww_oco_sibling_cancel_deadline_secs(),
            paper_enabled: false,
            max_order_quantity: 0,
        }
    }
}

// NOTE: the pure `decide_orders_runtime(cfg, live_fire) -> RuntimeLanes`
// resolver (the 7-row truth table, spec-flags-response FLAG-1) lands in
// PR-A core (`oms/groww/`), NOT here — its first truth-table column is the
// compile-time `groww_orders` cargo feature, which a pure runtime fn over
// `(&GrowwOrdersConfig, bool)` cannot express; forcing it into `common`
// would misrepresent the feature gate.

/// 🔷 DHAN pre-trade margin gate (`[dhan_margin_gate]`).
///
/// Fail-safe: an absent section deserializes to DISABLED. Even when
/// `enabled = true`, the REST legs stay dark until the code-change master
/// lock `DHAN_MARGIN_GATE_REST_ALLOWED` (constants.rs) flips with a fresh
/// dated operator quote — config flips alone can never turn the REST legs on.
///
/// Shared-account safety (BruteX co-tenant on the same Dhan account):
/// `tenant_budget_percent` caps EACH entry to at most half of the
/// then-current pooled `availabelBalance` (per-entry, not cumulative);
/// `rest_self_cap_per_sec` self-caps our funds/margin REST usage — the
/// funds/margin endpoints' rate bucket is NOT named by Dhan's docs, so the
/// budget is Assumed Non-Trading (20/sec); the 5/sec default stays ≤ 50%
/// even under the more conservative 10/sec reading; live-probe before
/// raising.
#[derive(Debug, Clone, Deserialize)]
pub struct DhanMarginGateConfig {
    /// Master config gate. Serde default FALSE (absent section = disabled).
    #[serde(default)]
    pub enabled: bool,
    /// PER-ENTRY cap: percent of the THEN-CURRENT pooled account
    /// `availabelBalance` a single entry may consume. Hard-capped at 50
    /// (shared account — never assume the full account margin is ours).
    /// CUMULATIVE our-share is NOT capped — sequential entries each
    /// re-read the balance, so they can cumulatively consume more of the
    /// pool (a cumulative tenant ledger is a flagged follow-up for the
    /// OMS-wiring PR).
    #[serde(default = "default_margin_gate_tenant_budget_percent")]
    pub tenant_budget_percent: u8,
    /// Self-imposed funds/margin REST ceiling (requests/sec). Default 5:
    /// the funds/margin endpoints' rate bucket is NOT named by Dhan's docs
    /// — Assumed Non-Trading (20/sec); a 5/sec default stays ≤ 50% even
    /// under the more conservative 10/sec reading; live-probe before
    /// raising. Hard-capped at 10; minimum 2 (one entry check issues two
    /// REST calls in one burst).
    #[serde(default = "default_margin_gate_rest_self_cap_per_sec")]
    pub rest_self_cap_per_sec: u32,
}

/// Serde default for [`DhanMarginGateConfig::tenant_budget_percent`] — 50,
/// the shared-account hard cap (half the pooled balance is the most our
/// entries may ever consume).
fn default_margin_gate_tenant_budget_percent() -> u8 {
    50
}

/// Serde default for [`DhanMarginGateConfig::rest_self_cap_per_sec`] — 5.
/// The funds/margin endpoints' rate bucket is NOT named by Dhan's docs —
/// Assumed Non-Trading (20/sec); a 5/sec default stays ≤ 50% even under the
/// more conservative 10/sec reading; live-probe before raising.
fn default_margin_gate_rest_self_cap_per_sec() -> u32 {
    5
}

impl Default for DhanMarginGateConfig {
    /// Manual impl so `Default` matches the serde field defaults exactly
    /// (a derived `Default` would zero the budget/cap fields while an
    /// absent `[dhan_margin_gate]` section deserializes them to 50/5).
    fn default() -> Self {
        Self {
            enabled: false,
            tenant_budget_percent: default_margin_gate_tenant_budget_percent(),
            rest_self_cap_per_sec: default_margin_gate_rest_self_cap_per_sec(),
        }
    }
}

/// `[groww_rest_burst]` — burst-tier + warm-up selection for the
/// per-minute Groww REST legs (2026-07-14 auto-ladder). Fail-safe shape:
/// every field is `#[serde(default)]`, so an absent section (or a TOML
/// written before this PR) means `two_wave` + warm-up OFF;
/// `config/base.toml` opts warm-up in explicitly.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct GrowwRestBurstConfig {
    /// The configured burst tier (boot value — a live 429 demotes the
    /// SESSION, never this config; restart restores it).
    #[serde(default)]
    pub tier: GrowwRestBurstTier,
    /// Pre-boundary TLS warm-up: one unauthenticated GET per leg client at
    /// minute boundary − 4 s (3 s-bounded, response discarded). Default
    /// OFF (fail-safe); base.toml turns it on.
    #[serde(default)]
    pub warm_up: bool,
}

impl DhanMarginGateConfig {
    /// Boot-time validation — the shared-account envelope must hold.
    ///
    /// # Errors
    /// Returns a descriptive error when `tenant_budget_percent` is outside
    /// `1..=50` (the Dhan account is pooled with the BruteX co-tenant, so
    /// our entries may never claim more than half the pooled balance) or
    /// when `rest_self_cap_per_sec` is outside `2..=10` (the ceiling is
    /// half of the Assumed Non-Trading 20/sec bucket — the funds/margin
    /// endpoints' bucket is NOT named by Dhan's docs; at least 2 because
    /// one entry check issues two REST calls in one burst).
    pub fn validate(&self) -> Result<()> {
        if !(1..=50).contains(&self.tenant_budget_percent) {
            bail!(
                "dhan_margin_gate.tenant_budget_percent ({}) must be within 1..=50 — the Dhan \
                 account is shared with the BruteX co-tenant, so our entries may never claim \
                 more than half of the pooled available balance",
                self.tenant_budget_percent
            );
        }
        if !(2..=10).contains(&self.rest_self_cap_per_sec) {
            bail!(
                "dhan_margin_gate.rest_self_cap_per_sec ({}) must be within 2..=10 — the \
                 ceiling is half of the ASSUMED Non-Trading 20/sec bucket (the funds/margin \
                 endpoints' bucket is not named by Dhan's docs; the account is shared with \
                 the BruteX co-tenant) and at least 2 (one entry check bursts two REST calls)",
                self.rest_self_cap_per_sec
            );
        }
        Ok(())
    }
}

impl FeedsConfig {
    /// `true` when at least one feed provider is enabled. Boot wiring
    /// uses this to refuse a no-feed configuration (a trading system
    /// with every feed disabled has nothing to do). Pure, O(1).
    #[must_use]
    pub const fn any_enabled(&self) -> bool {
        self.dhan_enabled || self.groww_enabled
    }

    /// `true` when BOTH feeds run in parallel (the cross-check target:
    /// Dhan + Groww side by side). Pure, O(1).
    #[must_use]
    pub const fn both_enabled(&self) -> bool {
        self.dhan_enabled && self.groww_enabled
    }
}

/// Container for the `[in_mem]` TOML section (Wave-5 §K-L10, PR #504d).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct InMemConfig {
    #[serde(default)]
    pub tick_storage: TickStorageConfig,
}

/// `[in_mem.tick_storage]` — runtime-tunable TickStorage settings.
#[derive(Debug, Clone, Deserialize)]
pub struct TickStorageConfig {
    /// Pre-allocated `Vec<ParsedTick>` capacity per `(security_id,
    /// exchange_segment)` key on first push. Sized to cover the
    /// busiest contract's daily tick count without forcing a Vec
    /// realloc (`tv_in_mem_tick_storage_realloc_total` increments on
    /// overflow). Setting this to 0 falls back to the compile-time
    /// default (`DEFAULT_PER_INSTRUMENT_CAPACITY = 5_000`) inside
    /// `TickStorage::new` so a misconfigured TOML cannot trigger
    /// 1-byte-realloc-per-tick.
    #[serde(default = "TickStorageConfig::default_per_instrument_capacity")]
    pub per_instrument_capacity: usize,
}

impl TickStorageConfig {
    /// Default capacity = 5_000 per L10 sizing analysis (mirrors the
    /// trading crate constant `DEFAULT_PER_INSTRUMENT_CAPACITY`).
    /// Pinned by `test_tick_storage_default_per_instrument_capacity`.
    #[must_use]
    pub const fn default_per_instrument_capacity() -> usize {
        5_000
    }
}

impl Default for TickStorageConfig {
    fn default() -> Self {
        Self {
            per_instrument_capacity: Self::default_per_instrument_capacity(),
        }
    }
}

/// Container for the `[engine.timeframes]` TOML section. L8 pins the
/// "TF list source" to `config/base.toml`, so this struct exists to
/// give downstream code a stable handle to the configured set without
/// duplicating it in code (`tickvault_app::metrics_catalog::Tf::ALL`
/// already enumerates the 9 TFs at compile time post PR #517; this
/// section pins the runtime override / documentation surface).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct EngineConfig {
    #[serde(default)]
    pub timeframes: TimeframesConfig,
}

/// Wave-5 §K-L6 / L7 / L8 — the canonical list of operator-facing
/// timeframes driven by the in-memory `CascadeFanout`. The default
/// matches `tickvault_app::metrics_catalog::Tf::ALL` exactly (9 entries
/// post PR #517, no seconds-resolution engines, no sub-15m engines
/// other than 1m + 5m).
#[derive(Debug, Clone, Deserialize)]
pub struct TimeframesConfig {
    /// 9 entries by default per PR #517: 1m + 5m + 15m + 30m + 1h..4h + 1d.
    /// Operator may override per environment but the ratchet
    /// `test_engine_timeframes_default_excludes_seconds` blocks any
    /// re-introduction of the seconds-level engines (L7), and the
    /// `tf_symmetry_guard` blocks any re-introduction of the 12 sub-15m
    /// timeframes retired by PR #517.
    #[serde(default = "TimeframesConfig::default_list")]
    pub list: Vec<String>,
}

impl Default for TimeframesConfig {
    fn default() -> Self {
        Self {
            list: Self::default_list(),
        }
    }
}

impl TimeframesConfig {
    /// PR #517 — 9 timeframes, ordered ascending. Mirrors the compile-time
    /// `tickvault_app::metrics_catalog::Tf::ALL` list (the catalog stays
    /// the wire-format source of truth; this list is the runtime-tunable
    /// mirror for documentation + future config overrides). PR #517
    /// retired the 12 sub-15m timeframes (2m..14m) — they are blocked
    /// from re-introduction by `tf_symmetry_guard`.
    #[must_use]
    pub fn default_list() -> Vec<String> {
        vec![
            "1m".to_string(),
            "5m".to_string(),
            "15m".to_string(),
            "30m".to_string(),
            "1h".to_string(),
            "2h".to_string(),
            "3h".to_string(),
            "4h".to_string(),
            "1d".to_string(),
        ]
    }

    /// Returns `true` if the configured list contains a seconds-level
    /// timeframe. L7 explicitly retired all seconds engines; the
    /// ratchet `test_engine_timeframes_default_excludes_seconds`
    /// asserts this returns `false` for the default config.
    #[must_use]
    pub fn contains_seconds_tf(&self) -> bool {
        self.list.iter().any(|tf| tf.ends_with('s'))
    }
}

// PR #4 (2026-05-19): `Depth20RootConfig`, `Depth200RootConfig`,
// `DepthDynamicConfig`, and `DepthDynamicUniverseConfig` retired
// alongside the deleted depth-20/depth-200 pipelines per the
// 4-IDX_I LOCKED_UNIVERSE operator lock
// (.claude/rules/project/websocket-connection-scope-lock.md).

/// Wave 1 C9 feature-flag toggles. Default = `true` for every flag (the
/// new code path is the safe default).
///
/// # Status — flag plumbing only (Phase 1)
///
/// This struct ships the **config plumbing** for the 14 toggles. It does
/// NOT yet guarantee that flipping a flag to `false` reverts to a
/// pre-Wave code path: Wave 1 items 0–4 do their work unconditionally
/// today, and the corresponding flag is read at runtime only by future
/// Wave 2 / Wave 3 PRs that ship the actual runtime branching. Setting
/// `hotpath_async_writers = false` in `config/base.toml` will parse
/// cleanly but does NOT today bring back the deleted `std::fs::write`
/// path — that revert needs a code change.
///
/// The honest C9 rollback contract is:
///
/// 1. The flag exists in the config struct + `[features]` section so an
///    operator override file can carry a `false` value end-to-end.
/// 2. Every Wave 2 / Wave 3 item PR is required to wire its runtime
///    branch on the corresponding flag before merge (enforced by the
///    9-box plan checklist + `feature_flag_rollback_guard.rs`).
/// 3. Default is always `true` so a missing `[features]` section in any
///    override file cannot silently disable a Wave item.
///
/// `feature_flag_rollback_guard.rs` ratchets the three guarantees above.
/// The runtime-branch wiring is OUT of scope for this struct — see each
/// Wave 2 / Wave 3 PR for the per-item `if cfg.features.<flag> { ... }`
/// call sites.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct FeaturesConfig {
    /// Wave 1 Item 0 — sync std::fs writers moved to dedicated drain tasks.
    pub hotpath_async_writers: bool,
    /// Wave 1 Item 1 — Phase2EmitGuard panic-on-drop in debug, ERROR in release.
    pub phase2_emit_guard: bool,
    // PR #2 (2026-05-18): `stock_movers_full_universe` and
    // `option_movers_5s` flags retired alongside the deleted movers
    // pipeline.
    /// Wave 1 Item 4 — `previous_close` un-deprecate + segment-routed persist.
    pub previous_close_persist: bool,
    /// Wave 2 Item 5 — main-feed WS idle-sleep until 09:00 IST.
    pub ws_main_sleep_until_open: bool,
    /// Wave 2 Item 6 — depth + order-update WS idle-sleep until 09:00 IST.
    pub ws_depth_ou_sleep_until_open: bool,
    /// Wave 2 Item 7 — fast-boot 60-second deadline with mid-market degraded mode.
    pub fast_boot_60s_deadline: bool,
    // PR-C3 (2026-07-14): `tick_gap_detector_60s_coalesce` (Wave 2 Item 8)
    // retired alongside the deleted tick-gap detector (operator Q4-ii
    // 2026-07-13 — the detector was fed only by the retired Dhan WS lane).
    /// Wave 2 Item 9 — 6 audit tables (subscribe/disconnect/depth/etc).
    pub audit_tables_enabled: bool,
    /// Wave 3 Item 11 — Telegram bucket-coalescer + dispatcher hardening.
    pub telegram_bucket_coalescer: bool,
    /// Wave 3 Item 12 — market-open self-test at 09:15 IST.
    pub market_open_self_test: bool,
    /// Wave 3 Item 13 — composite real-time guarantee score gauge.
    pub realtime_guarantee_score: bool,
    // PR-D (2026-05-26): `historical_fetch_enabled` retired alongside
    // the deleted Dhan historical fetch chain.
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            hotpath_async_writers: true,
            phase2_emit_guard: true,
            previous_close_persist: true,
            ws_main_sleep_until_open: true,
            ws_depth_ou_sleep_until_open: true,
            fast_boot_60s_deadline: true,
            audit_tables_enabled: true,
            telegram_bucket_coalescer: true,
            market_open_self_test: true,
            realtime_guarantee_score: true,
        }
    }
}

// PR #2 (2026-05-18): `MoversConfig` struct retired alongside the
// deleted movers pipeline. Under the 4-IDX_I-only universe top-N
// gainers/losers/most-active queries are meaningless.

/// Trading execution mode — controls how orders are routed.
///
/// - `Paper`: Zero HTTP calls. Orders simulated locally with PAPER-{counter} IDs.
///   Use for strategy development with real market data.
/// - `Sandbox`: HTTP calls to `sandbox.dhan.co/v2/`. Orders fill at ₹100 (simulated).
///   Use for API integration testing before going live.
/// - `Live`: HTTP calls to `api.dhan.co/v2/`. Real exchange orders, real money.
///   Requires static IP. Use only when ready for production.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TradingMode {
    /// Local simulation — no HTTP calls, PAPER-{counter} order IDs.
    /// Default: safe — never touch real money.
    #[default]
    Paper,
    /// Dhan sandbox — HTTP to sandbox.dhan.co, orders fill at ₹100.
    Sandbox,
    /// Real trading — HTTP to api.dhan.co, real exchange orders.
    Live,
}

impl TradingMode {
    /// Returns true if this mode makes real HTTP calls to Dhan (sandbox or live).
    pub fn is_http_active(self) -> bool {
        matches!(self, Self::Sandbox | Self::Live)
    }

    /// Returns true if this mode is paper trading (no HTTP calls).
    pub fn is_paper(self) -> bool {
        self == Self::Paper
    }

    /// Returns true if this is the live production mode.
    pub fn is_live(self) -> bool {
        self == Self::Live
    }

    /// Returns true if this is the sandbox testing mode.
    pub fn is_sandbox(self) -> bool {
        self == Self::Sandbox
    }

    /// Returns the display name for logging.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paper => "paper",
            Self::Sandbox => "sandbox",
            Self::Live => "live",
        }
    }
}

/// Strategy and paper-trading configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct StrategyConfig {
    /// Path to the strategy TOML config file (relative to working directory).
    #[serde(default = "default_strategy_config_path")]
    pub config_path: String,
    /// Trading capital in rupees (for risk engine daily loss calculation).
    #[serde(default = "default_capital")]
    pub capital: f64,
    /// Dry-run mode: when true, NO real orders are placed. All orders are simulated.
    /// DEFAULT: true. This is a developer-only tool.
    /// DEPRECATED: Use `mode` instead. Kept for backward compatibility.
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
    /// Trading execution mode: paper (local sim), sandbox (Dhan DevPortal), live (real).
    /// Overrides dry_run when set. Default: paper.
    #[serde(default)]
    pub mode: TradingMode,
    /// S6-Step4: Sandbox-only enforcement until this date. If the current
    /// date is BEFORE this value, `mode = Live` is forbidden — the boot
    /// sequence panics. Format: `YYYY-MM-DD`. Default `2099-12-31`
    /// (2026-07-14 re-arm — the old `2026-06-30` default EXPIRED silently;
    /// the sentinel matches production.toml, so going live requires an
    /// explicit config edit with a fresh dated operator quote).
    ///
    /// Set to `1970-01-01` to disable the gate — that EXACT sentinel is
    /// exempt from the loud-expiry tripwire (`expired_live_gates`); any
    /// OTHER configured past date warns at every boot as a likely silent
    /// no-op (the 2026-06-30 incident class).
    #[serde(default = "default_sandbox_only_until")]
    pub sandbox_only_until: String,
}

fn default_sandbox_only_until() -> String {
    // 2026-07-14 re-arm: sentinel matching production.toml — an absent key
    // now means ARMED, never silently expired.
    "2099-12-31".to_string()
}

/// The documented intentional-disable value for `[strategy]
/// sandbox_only_until` (the field doc's canonical "disable this gate"
/// sentinel). Exactly this value is exempt from the loud-expiry tripwire —
/// any OTHER configured past date is treated as accidental expiry and
/// warned about at every boot (review round 1, 2026-07-14).
const SANDBOX_ONLY_UNTIL_DISABLE_SENTINEL: &str = "1970-01-01";

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            config_path: default_strategy_config_path(),
            capital: default_capital(),
            dry_run: default_dry_run(),
            mode: TradingMode::default(),
            sandbox_only_until: default_sandbox_only_until(),
        }
    }
}

impl StrategyConfig {
    /// S6-Step4: Returns Ok if the current IST date is past
    /// `sandbox_only_until` OR the trading mode is Paper/Sandbox/DryRun.
    /// Returns Err if Live trading is requested before the cutoff.
    ///
    /// Called at boot from `crates/app/src/main.rs`. A failure here is
    /// FATAL — the process panics rather than risk a real-money order
    /// in the sandbox-only window.
    ///
    /// # Errors
    /// Returns `Err(String)` describing the violation.
    // TEST-EXEMPT: covered by test_sandbox_only_until_blocks_real_orders, test_sandbox_date_parses, test_sandbox_already_past_returns_ok in this module
    pub fn check_sandbox_window(&self, today_ist: chrono::NaiveDate) -> Result<(), String> {
        // Live mode check: only enforce on Live trading.
        if !self.mode.is_live() && self.dry_run {
            return Ok(());
        }
        if !self.mode.is_live() {
            return Ok(());
        }

        let cutoff = chrono::NaiveDate::parse_from_str(&self.sandbox_only_until, "%Y-%m-%d")
            .map_err(|e| {
                format!(
                    "invalid sandbox_only_until '{}': {e}",
                    self.sandbox_only_until
                )
            })?;
        if today_ist <= cutoff {
            return Err(format!(
                "S6-Step4 SANDBOX-ONLY VIOLATION: today is {today_ist}, sandbox_only_until={cutoff}, \
                 mode=Live. Real orders are FORBIDDEN until {cutoff}. Set mode=sandbox or mode=paper, \
                 or wait until {cutoff} passes."
            ));
        }
        Ok(())
    }
}

fn default_strategy_config_path() -> String {
    "config/strategies.toml".to_string()
}

const fn default_capital() -> f64 {
    1_000_000.0
}

const fn default_dry_run() -> bool {
    true
}

/// Trading session timing configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct TradingConfig {
    /// Market open time in IST (e.g., "09:00:00").
    pub market_open_time: String,
    /// Market close time in IST (e.g., "15:30:00").
    pub market_close_time: String,
    /// Order submission cutoff in IST (e.g., "15:29:00").
    pub order_cutoff_time: String,
    /// Data collection start time in IST (e.g., "09:00:00").
    pub data_collection_start: String,
    /// Data collection end time in IST (e.g., "16:00:00").
    pub data_collection_end: String,
    /// Timezone identifier (always "Asia/Kolkata").
    pub timezone: String,
    /// Maximum orders per second (SEBI limit).
    pub max_orders_per_second: u32,
    /// NSE trading holidays with names for display.
    /// Source: official NSE circular (update annually).
    #[serde(default)]
    pub nse_holidays: Vec<NseHolidayEntry>,
    /// Muhurat Trading dates — special sessions on otherwise closed days.
    #[serde(default)]
    pub muhurat_trading_dates: Vec<NseHolidayEntry>,
    /// NSE mock trading session dates (Saturdays, ~monthly).
    /// Source: NSE Contingency Drill / Mock Trading Calendar (update annually).
    /// Mock sessions are NOT real trading days — no real orders, no settlement.
    /// Used for operational awareness and system testing readiness.
    #[serde(default)]
    pub nse_mock_trading_dates: Vec<NseHolidayEntry>,
}

/// A single NSE holiday or Muhurat trading date with display name.
#[derive(Debug, Clone, Deserialize)]
pub struct NseHolidayEntry {
    /// Date in YYYY-MM-DD format (IST).
    pub date: String,
    /// Human-readable holiday name for display.
    pub name: String,
}

/// Dhan API and WebSocket connection configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct DhanConfig {
    /// WebSocket V2 binary feed URL.
    pub websocket_url: String,
    /// Order update WebSocket URL (JSON-based, separate from binary feed).
    pub order_update_websocket_url: String,
    /// REST API base URL (for trading, data, renewal).
    pub rest_api_base_url: String,
    /// Sandbox API base URL (for sandbox mode order testing).
    /// Set in config/base.toml — no default in code (must come from config).
    #[serde(default)]
    pub sandbox_base_url: String,
    /// Auth base URL (for token generation — separate from REST API).
    /// Dhan uses `https://auth.dhan.co` for authentication endpoints.
    pub auth_base_url: String,
    /// Primary instrument CSV download URL.
    pub instrument_csv_url: String,
    /// Fallback instrument CSV download URL.
    pub instrument_csv_fallback_url: String,
    /// Maximum instruments per WebSocket connection.
    pub max_instruments_per_connection: usize,
    /// Maximum concurrent WebSocket connections.
    pub max_websocket_connections: usize,
}

/// WebSocket keep-alive and reconnection configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct WebSocketConfig {
    /// Expected server ping interval in seconds (Dhan server pings every 10s).
    pub ping_interval_secs: u64,
    /// Server disconnects after this many seconds with no pong.
    pub pong_timeout_secs: u64,
    /// Reserved for future use (server ping monitoring).
    pub max_consecutive_pong_failures: u32,
    /// Initial reconnection delay in milliseconds.
    pub reconnect_initial_delay_ms: u64,
    /// Maximum reconnection delay in milliseconds.
    pub reconnect_max_delay_ms: u64,
    /// Maximum reconnection attempts before giving up.
    pub reconnect_max_attempts: u32,
    /// Maximum instruments per subscription message (Dhan limit: 100).
    pub subscription_batch_size: usize,
    /// Delay in milliseconds between spawning successive WebSocket connections.
    /// Prevents all connections from hitting Dhan's server simultaneously at startup.
    /// 0 = no stagger (all spawn immediately). Only affects initial startup, not reconnects.
    pub connection_stagger_ms: u64,

    /// Per-conn activity watchdog threshold in seconds. Historical: the
    /// Dhan main-feed clamped this at boot (retired with the lane, PR-C2/
    /// C3 2026-07-13/14). Defaults to the legacy
    /// `WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS = 50` value when unset
    /// in TOML.
    #[serde(default = "default_activity_watchdog_threshold_secs")]
    pub activity_watchdog_threshold_secs: u64,
}

fn default_activity_watchdog_threshold_secs() -> u64 {
    // Mirror of WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS in the core crate.
    // We can't import it here (common is below core in the dep graph) so
    // we hard-pin and ratchet the equality via
    // `crates/core/src/websocket/activity_watchdog.rs::tests::
    //  test_legacy_threshold_default_matches_websocket_config_default`.
    50
}

/// QuestDB connection configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct QuestDbConfig {
    /// QuestDB Docker hostname.
    pub host: String,
    /// HTTP API port (web console + REST).
    pub http_port: u16,
    /// PostgreSQL wire protocol port.
    pub pg_port: u16,
    /// InfluxDB Line Protocol port (high-speed ingestion).
    pub ilp_port: u16,
}

impl QuestDbConfig {
    /// Builds the ILP connection string with retry and timeout settings.
    ///
    /// All 15+ ILP writers in the storage crate SHOULD use this method
    /// instead of raw `format!("tcp::addr=...")` to get consistent:
    /// - Retry timeout: 30s (recovers from transient QuestDB restarts)
    /// - Init buffer size: 64KB (matches WAL segment tuning)
    /// - Request timeout: 60s (generous for large batch flushes)
    ///
    /// Builds the ILP TCP connection string.
    ///
    /// NOTE: `retry_timeout`, `init_buf_size`, `request_timeout` are HTTP-only
    /// parameters in questdb-rs 6.1.0. TCP mode only supports `addr`.
    /// Connection resilience is handled by our writers (ring buffer + reconnect).
    pub fn build_ilp_conf_string(&self) -> String {
        format!("tcp::addr={}:{};", self.host, self.ilp_port)
    }
}

/// Partition retention configuration (separate from QuestDbConfig to avoid breaking existing code).
///
/// 2026-07-13 (disk-pressure remediation): grew the archive→verify→drop knobs.
/// Two retention classes exist:
/// - **market-data** (`ticks` + the 21 `candles_*` tables) → `market_data_hot_days`
/// - **everything else** (audit / daily-data tables) → `retention_days`
///
/// The destructive archive→verify→drop leg is gated on `archive_enabled`
/// (serde default **false**), so a config rollback (`archive_enabled = false`,
/// or simply deleting the key) restores the legacy detach-only behaviour
/// instantly. `market_data_hot_days` defaulting to 14 is safe-by-default
/// precisely BECAUSE the flow is fail-closed: nothing is ever dropped unless
/// its S3 copy has been row-count- and size-verified, and nothing at all
/// happens while `archive_enabled` is false.
#[derive(Debug, Clone, Deserialize)]
pub struct PartitionRetentionConfig {
    /// Hot partition retention in days for the STANDARD class (audit /
    /// daily-data tables). Partitions older than this are detached (legacy
    /// path) or archived→verified→dropped (when `archive_enabled`).
    /// Default: 90 days. Set to 0 to disable auto-detach.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// Hot window in days for the HIGH-VOLUME market-data class (`ticks` +
    /// the 21 `candles_*` tables + the per-minute chain tables since
    /// 2026-07-16). 90 days of ticks (~135+ GB) can never fit the volume —
    /// the hot window must be shorter, with S3 as the durable long-term
    /// store (aws-budget.md §5 hot-window-on-EBS doctrine; SEBI retention
    /// satisfied by the S3 copy). Default 35 since 2026-07-16 (was 14) —
    /// the operator's minimum-one-month spot window demand ("our entire
    /// one month should be stored and fetched from questdb even before
    /// premarket"). Only consulted when `archive_enabled = true`; clamped
    /// to a hard MIN_HOT_DAYS=2 floor at use (today + yesterday are
    /// untouchable).
    #[serde(default = "default_market_data_hot_days")]
    pub market_data_hot_days: u32,
    /// Master gate for the archive→verify→drop leg. serde default FALSE so
    /// the destructive behaviour must be explicitly configured on
    /// (config/base.toml sets it true for prod); flipping it off restores
    /// the legacy detach-only cycle byte-identically.
    #[serde(default)]
    pub archive_enabled: bool,
    /// S3 bucket receiving verified partition archives. Empty (the default)
    /// = derive `tv-<env>-cold` from TV_ENVIRONMENT/ENVIRONMENT (prod →
    /// `tv-prod-cold`, the bucket the instance role already reads/writes).
    #[serde(default)]
    pub archive_bucket: String,
    /// Per-run bound on archived partitions (oldest first) so the first
    /// catch-up sweep (weeks of hourly ticks partitions) converges over a
    /// few post-market runs instead of overrunning the 16:30 IST box stop.
    /// 0 = unlimited.
    #[serde(default = "default_max_partitions_per_run")]
    pub max_partitions_per_run: u32,
}

impl Default for PartitionRetentionConfig {
    fn default() -> Self {
        Self {
            retention_days: default_retention_days(),
            market_data_hot_days: default_market_data_hot_days(),
            archive_enabled: false,
            archive_bucket: String::new(),
            max_partitions_per_run: default_max_partitions_per_run(),
        }
    }
}

/// Default retention: 90 days of hot data.
const fn default_retention_days() -> u32 {
    90
}

/// Default market-data hot window: 35 days (2026-07-16 operator directive
/// — one month of spot history + weekend slack; was 14). Inert unless
/// `archive_enabled`; safe-by-default because the archive→verify→drop flow
/// is fail-closed (no verified S3 copy ⇒ no drop).
const fn default_market_data_hot_days() -> u32 {
    35
}

/// Default per-run archive bound: 200 partitions. At ~8–24 hourly ticks
/// partitions + ~22 daily candle partitions per aged-out day, one run covers
/// several days of backlog while staying far inside the post-market window.
const fn default_max_partitions_per_run() -> u32 {
    200
}

// `ValkeyConfig` struct + `default_valkey_password` helper DELETED in
// #O4 (2026-05-24). Valkey removed from the runtime; the dual-instance
// lock moved to AWS SSM Parameter Store in PR #764.

// `PrometheusConfig` struct DELETED in #O5 (2026-05-30) — Prometheus container removed in #O3.

/// Network timeout and retry configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    /// HTTP request timeout in milliseconds.
    pub request_timeout_ms: u64,
    /// WebSocket connection timeout in milliseconds.
    pub websocket_connect_timeout_ms: u64,
    /// Initial retry delay in milliseconds (exponential backoff).
    pub retry_initial_delay_ms: u64,
    /// Maximum retry delay in milliseconds.
    pub retry_max_delay_ms: u64,
    /// Maximum number of retry attempts.
    pub retry_max_attempts: u32,
}

/// JWT token lifecycle configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenConfig {
    /// Hours before expiry to trigger token refresh.
    pub refresh_before_expiry_hours: u64,
    /// Token validity duration in hours.
    pub token_validity_hours: u64,
}

/// Risk management configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    /// Maximum allowed daily loss as percentage of capital.
    pub max_daily_loss_percent: f64,
    /// Maximum position size in lots.
    pub max_position_size_lots: u32,
}

/// Logging configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    /// Log level filter (trace, debug, info, warn, error).
    pub level: String,
    /// Log output format (json, pretty).
    pub format: String,
    /// Write logs to stdout (IntelliJ console / docker logs).
    /// Default: false — prevents unbounded console buffer growth.
    /// File logging (`data/logs/`) is always active regardless of this flag.
    #[serde(default)]
    pub log_to_stdout: bool,
}

/// API server configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiConfig {
    /// Bind address for the HTTP server.
    pub host: String,
    /// HTTP server port.
    pub port: u16,
    /// Allowed CORS origins. Defaults to localhost dev origins.
    #[serde(default = "default_allowed_origins")]
    pub allowed_origins: Vec<String>,
    // `movers_v2_enabled` knob removed 2026-07-06 (audit finding) — the
    // `/api/movers?v=2` route it gated was deleted with the movers
    // pipeline in AWS-lifecycle PR #2 (2026-05-18); the knob had zero
    // runtime reads since.
}

fn default_allowed_origins() -> Vec<String> {
    vec![
        "http://localhost:3000".to_string(),
        "http://localhost:3001".to_string(),
    ]
}

/// Instrument CSV download and universe build configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct InstrumentConfig {
    /// Time of day (IST) to download fresh instrument CSV. Format: "HH:MM:SS".
    pub daily_download_time: String,
    /// Directory path for caching the last successful CSV download.
    pub csv_cache_directory: String,
    /// Cached CSV filename.
    pub csv_cache_filename: String,
    /// Download timeout in seconds (overrides network timeout for this large file).
    pub csv_download_timeout_secs: u64,
    /// Start of instrument build window (IST). Format: "HH:MM:SS".
    pub build_window_start: String,
    /// End of instrument build window (IST). Format: "HH:MM:SS".
    pub build_window_end: String,
}

/// Notification (Telegram alert) configuration.
///
/// Secrets (bot token, chat ID) come from SSM — never in config.
/// This struct holds non-secret settings only.
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationConfig {
    /// Telegram Bot API base URL.
    pub telegram_api_base_url: String,
    /// HTTP send timeout in milliseconds for notification POSTs.
    pub send_timeout_ms: u64,
    /// Enable SMS alerts via AWS SNS for Critical/High severity events.
    /// Phone number is fetched from SSM at `/tickvault/{env}/sns/phone-number`.
    pub sns_enabled: bool,
    /// Telegram UX Overhaul (2026-07-07) kill switch #1: episode live-edit
    /// coalescing (one incident = one live-edited bubble). `false` makes
    /// the `episode_key()` consultation a no-op → byte-identical legacy
    /// per-event dispatch.
    #[serde(default = "default_notification_episode_mode")]
    pub episode_mode: bool,
    /// Telegram UX Overhaul (2026-07-07) kill switch #2: LOW/MEDIUM digest
    /// window (seconds) during market hours. Clamped to
    /// [`NOTIFICATION_DIGEST_WINDOW_MIN_SECS`, `NOTIFICATION_DIGEST_WINDOW_MAX_SECS`]
    /// via [`Self::digest_window_secs_clamped`]. `60` == legacy 60s
    /// coalescing behavior.
    #[serde(default = "default_notification_digest_window_secs")]
    pub digest_window_secs: u64,
    /// Boot bubble (2026-07-09 operator escalation) kill switch: ONE
    /// consolidated, live-edited boot checklist bubble per boot. `false`
    /// routes the boot milestones back through their unchanged legacy
    /// immediate dispatch (per-event spray) WITHOUT touching the
    /// `episode_mode` WS-episode machinery.
    #[serde(default = "default_notification_boot_bubble")]
    pub boot_bubble: bool,
}

/// Lower clamp bound for `[notification] digest_window_secs` (== the legacy
/// 60s coalescer window, i.e. "digest off" behavior).
pub const NOTIFICATION_DIGEST_WINDOW_MIN_SECS: u64 = 60;

/// Upper clamp bound for `[notification] digest_window_secs` — one hour;
/// anything longer would hide LOW/MEDIUM signal for an entire session.
pub const NOTIFICATION_DIGEST_WINDOW_MAX_SECS: u64 = 3600;

fn default_notification_episode_mode() -> bool {
    true
}

fn default_notification_boot_bubble() -> bool {
    true
}

fn default_notification_digest_window_secs() -> u64 {
    900
}

impl NotificationConfig {
    /// The digest window clamped to
    /// [`NOTIFICATION_DIGEST_WINDOW_MIN_SECS`, `NOTIFICATION_DIGEST_WINDOW_MAX_SECS`].
    /// A fat-fingered `0` or `86400` can never silence or flood the
    /// operator — the clamp is applied at every consumer.
    #[must_use]
    pub fn digest_window_secs_clamped(&self) -> u64 {
        self.digest_window_secs.clamp(
            NOTIFICATION_DIGEST_WINDOW_MIN_SECS,
            NOTIFICATION_DIGEST_WINDOW_MAX_SECS,
        )
    }
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            // APPROVED: config default — overridable via TOML config file
            telegram_api_base_url: "https://api.telegram.org".to_string(),
            send_timeout_ms: 10_000,
            sns_enabled: false,
            episode_mode: default_notification_episode_mode(),
            digest_window_secs: default_notification_digest_window_secs(),
            boot_bubble: default_notification_boot_bubble(),
        }
    }
}

/// Observability stack configuration.
///
/// Controls Prometheus metrics export and OpenTelemetry tracing.
/// Metrics are served via an HTTP endpoint for Prometheus to scrape.
/// Traces are exported via OTLP gRPC to Jaeger.
#[derive(Debug, Clone, Deserialize)]
pub struct ObservabilityConfig {
    /// Bind address for the Prometheus metrics HTTP endpoint.
    ///
    /// Default `127.0.0.1` (loopback only) per the Wave-5 in-memory-store
    /// plan §AA / L123 (review-fold finding SEC-H1): the new
    /// `tv_subsystem_memory_estimated_bytes{component}` gauge would
    /// otherwise be reachable from any peer in the VPC over `0.0.0.0`,
    /// leaking the trading-universe size and per-subsystem footprint to
    /// non-Prometheus scrapers.
    ///
    /// Operators that scrape from a sidecar in the same pod / network
    /// namespace (the current AWS plan) keep loopback. Production
    /// scenarios that need a different bind address (e.g. dedicated
    /// scrape network) override this field per environment.
    #[serde(default = "ObservabilityConfig::default_metrics_bind_addr")]
    pub metrics_bind_addr: std::net::IpAddr,
    /// Port for the Prometheus metrics HTTP endpoint served by the application.
    pub metrics_port: u16,
    /// OTLP gRPC endpoint for trace export (e.g., tv-jaeger:4317).
    pub otlp_endpoint: String,
    /// Enable Prometheus metrics export.
    pub metrics_enabled: bool,
    /// Enable OpenTelemetry trace export to Jaeger.
    pub tracing_enabled: bool,
}

impl ObservabilityConfig {
    /// Default bind for the Prometheus `/metrics` HTTP listener.
    ///
    /// Pinned to loopback `127.0.0.1` (L123) so that the new
    /// per-subsystem memory gauge is not exposed to anything outside
    /// the local network namespace by default. The reconciliation
    /// ratchet (`test_default_metrics_bind_is_loopback`) fails the
    /// build if anyone changes this without updating the alert /
    /// dashboard / disaster-recovery docs.
    #[must_use]
    pub fn default_metrics_bind_addr() -> std::net::IpAddr {
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    }
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            metrics_bind_addr: Self::default_metrics_bind_addr(),
            metrics_port: 9091,
            otlp_endpoint: "http://tv-jaeger:4317".to_string(), // APPROVED: Docker DNS hostname for OTLP collector
            metrics_enabled: true,
            tracing_enabled: true,
        }
    }
}

// PR-C3 (2026-07-14, operator retirement directive 2026-07-13 — scope-lock
// amendment §B item 2): `SubscriptionScope` (the compile-time WS-scope
// contract), `effective_main_feed_pool_size`, and `SubscriptionConfig`
// (with the base.toml `[subscription]` section) were DELETED with the Dhan
// subscription planner — there is no Dhan WS subscription left to scope.
// Re-introducing ANY Dhan market-data subscription surface requires a
// fresh dated operator quote in websocket-connection-scope-lock.md FIRST
// (§D of the amendment).

/// Historical data fetching configuration.
///
/// Controls automated fetching of 1-minute OHLCV candles from Dhan's
/// intraday charts API for cross-verification with live tick data.
#[derive(Debug, Clone, Deserialize)]
pub struct HistoricalDataConfig {
    /// Enable automated historical candle fetching after market close.
    pub enabled: bool,
    /// Number of past trading days to fetch on startup for cross-verification.
    /// Dhan allows up to 90 days per request.
    pub lookback_days: u32,
    /// HTTP request timeout in seconds for historical data API calls.
    pub request_timeout_secs: u64,
    /// Maximum retry attempts for failed API requests.
    pub max_retries: u32,
    /// Delay in milliseconds between consecutive API requests (rate limiting).
    pub request_delay_ms: u64,
    /// Idempotency marker file path. Tracks the IST date of the last
    /// successful historical fetch so reboots within the same day skip
    /// re-fetching the 90-day window.
    #[serde(default = "default_historical_marker_path")]
    pub marker_path: String,
}

fn default_historical_marker_path() -> String {
    "data/state/historical_fetch_done.json".to_string()
}

impl Default for HistoricalDataConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lookback_days: 90,
            request_timeout_secs: 30,
            max_retries: 3,
            request_delay_ms: 500,
            marker_path: default_historical_marker_path(),
        }
    }
}

/// Index constituency download configuration.
///
/// Controls the download of NSE index constituent CSVs from niftyindices.com.
/// Used to build a bidirectional index-stock mapping for the trading system.
#[derive(Debug, Clone, Deserialize)]
pub struct IndexConstituencyConfig {
    /// Whether constituency download is enabled.
    #[serde(default = "default_constituency_enabled")]
    pub enabled: bool,
    /// HTTP request timeout in seconds for individual CSV downloads.
    #[serde(default = "default_constituency_download_timeout_secs")]
    pub download_timeout_secs: u64,
    /// Maximum concurrent CSV downloads.
    #[serde(default = "default_constituency_max_concurrent_downloads")]
    pub max_concurrent_downloads: usize,
    /// Delay in milliseconds between batches of concurrent downloads.
    /// Default 200ms to be respectful to niftyindices.com when downloading ~50 indices.
    #[serde(default = "default_constituency_inter_batch_delay_ms")]
    pub inter_batch_delay_ms: u64,
}

impl Default for IndexConstituencyConfig {
    fn default() -> Self {
        Self {
            enabled: default_constituency_enabled(),
            download_timeout_secs: default_constituency_download_timeout_secs(),
            max_concurrent_downloads: default_constituency_max_concurrent_downloads(),
            inter_batch_delay_ms: default_constituency_inter_batch_delay_ms(),
        }
    }
}

const fn default_constituency_enabled() -> bool {
    true
}

const fn default_constituency_download_timeout_secs() -> u64 {
    30
}

const fn default_constituency_max_concurrent_downloads() -> usize {
    5
}

const fn default_constituency_inter_batch_delay_ms() -> u64 {
    200
}

/// Infrastructure configuration — controls Docker auto-start behavior.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct InfrastructureConfig {
    /// Whether to auto-start Docker services on boot.
    /// When true (default): one-click Run — app auto-launches Docker Desktop
    /// and containers. Probes first, skips if already running. Zero manual steps.
    /// When false: run "Docker Restart" in IntelliJ first, then run the app.
    #[serde(default = "default_auto_start")]
    pub auto_start_docker: bool,
}

impl Default for InfrastructureConfig {
    fn default() -> Self {
        Self {
            auto_start_docker: true,
        }
    }
}

const fn default_auto_start() -> bool {
    true
}

/// Greeks engine configuration.
///
/// Controls Black-Scholes pricing parameters, IV solver settings,
/// and the periodic option chain fetch pipeline.
/// Defaults match Indian market conditions (NIFTY/BANKNIFTY).
#[derive(Debug, Clone, Deserialize)]
pub struct GreeksConfig {
    /// Enable the greeks pipeline (option chain fetch + compute + persist).
    #[serde(default = "default_greeks_enabled")]
    pub enabled: bool,
    /// Interval between option chain fetch cycles (seconds).
    #[serde(default = "default_greeks_fetch_interval_secs")]
    pub fetch_interval_secs: u64,
    /// Risk-free interest rate (annualized). India 91-day T-Bill rate.
    #[serde(default = "default_risk_free_rate")]
    pub risk_free_rate: f64,
    /// Continuous dividend yield (annualized). ~1.2% for NIFTY 50.
    #[serde(default = "default_dividend_yield")]
    pub dividend_yield: f64,
    /// Maximum Newton-Raphson iterations for IV solver.
    #[serde(default = "default_iv_solver_max_iterations")]
    pub iv_solver_max_iterations: u32,
    /// IV solver convergence tolerance.
    #[serde(default = "default_iv_solver_tolerance")]
    pub iv_solver_tolerance: f64,
    /// Day count divisor for theta conversion (365.0 = calendar, 252.0 = trading days).
    /// Calibrated to match Dhan's computation. Default: 365.0.
    #[serde(default = "default_day_count")]
    pub day_count: f64,
    /// Rate mode: "dhan" = fixed 10% (match Dhan/NSE), "theoretical" = RBI repo rate lookup.
    /// Default: "dhan".
    #[serde(default = "default_rate_mode")]
    pub rate_mode: String,
}

impl Default for GreeksConfig {
    fn default() -> Self {
        Self {
            enabled: default_greeks_enabled(),
            fetch_interval_secs: default_greeks_fetch_interval_secs(),
            risk_free_rate: default_risk_free_rate(),
            dividend_yield: default_dividend_yield(),
            iv_solver_max_iterations: default_iv_solver_max_iterations(),
            iv_solver_tolerance: default_iv_solver_tolerance(),
            day_count: default_day_count(),
            rate_mode: default_rate_mode(),
        }
    }
}

const fn default_greeks_enabled() -> bool {
    true
}

const fn default_greeks_fetch_interval_secs() -> u64 {
    60
}

// Calibrated against Dhan's live option chain data (2026-03-23).
// Dhan's theta best matches at r ≈ 0.10. Gamma/vega insensitive to rate for short-dated.
const fn default_risk_free_rate() -> f64 {
    0.10
}

// Calibrated: Dhan uses q=0.0 for index options (no continuous dividend).
const fn default_dividend_yield() -> f64 {
    0.0
}

const fn default_iv_solver_max_iterations() -> u32 {
    50
}

const fn default_iv_solver_tolerance() -> f64 {
    1e-8
}

const fn default_day_count() -> f64 {
    365.0
}

fn default_rate_mode() -> String {
    String::from("dhan")
}

// ---------------------------------------------------------------------------
// Configuration Validation
// ---------------------------------------------------------------------------

impl ApplicationConfig {
    /// Validates all configuration values at startup.
    ///
    /// Catches invalid time formats, SEBI violations, and nonsensical ranges
    /// before the system starts trading.
    ///
    /// # Errors
    /// Returns descriptive error for the first validation failure found.
    pub fn validate(&self) -> Result<()> {
        // Timezone must be Asia/Kolkata — SEBI requirement.
        if self.trading.timezone != "Asia/Kolkata" {
            bail!(
                "trading.timezone must be 'Asia/Kolkata', got '{}'",
                self.trading.timezone
            );
        }

        // Validate time string formats.
        // Helper: parse and validate a single time field.
        let parse_time = |field_name: &str, value: &str| -> Result<NaiveTime> {
            NaiveTime::parse_from_str(value, "%H:%M:%S").map_err(|_| {
                anyhow::anyhow!("{} is not a valid HH:MM:SS time: '{}'", field_name, value)
            })
        };

        parse_time("trading.market_open_time", &self.trading.market_open_time)?;
        parse_time("trading.market_close_time", &self.trading.market_close_time)?;
        parse_time("trading.order_cutoff_time", &self.trading.order_cutoff_time)?;
        parse_time(
            "trading.data_collection_start",
            &self.trading.data_collection_start,
        )?;
        parse_time(
            "trading.data_collection_end",
            &self.trading.data_collection_end,
        )?;
        parse_time(
            "instrument.daily_download_time",
            &self.instrument.daily_download_time,
        )?;
        // Parse and retain build window times for the comparison below.
        let window_start = parse_time(
            "instrument.build_window_start",
            &self.instrument.build_window_start,
        )?;
        let window_end = parse_time(
            "instrument.build_window_end",
            &self.instrument.build_window_end,
        )?;

        // SEBI: max_orders_per_second must not exceed the SEBI limit.
        if self.trading.max_orders_per_second > SEBI_MAX_ORDERS_PER_SECOND {
            bail!(
                "trading.max_orders_per_second ({}) exceeds SEBI limit ({})",
                self.trading.max_orders_per_second,
                SEBI_MAX_ORDERS_PER_SECOND
            );
        }

        // Token: refresh_before_expiry must be less than token validity.
        if self.token.refresh_before_expiry_hours >= self.token.token_validity_hours {
            bail!(
                "token.refresh_before_expiry_hours ({}) must be less than token.token_validity_hours ({})",
                self.token.refresh_before_expiry_hours,
                self.token.token_validity_hours
            );
        }

        // Network: timeouts and retries must be positive.
        if self.network.request_timeout_ms == 0 {
            bail!("network.request_timeout_ms must be > 0");
        }
        if self.network.retry_max_attempts == 0 {
            bail!("network.retry_max_attempts must be > 0");
        }

        // Risk: daily loss must be a finite positive number in (0, 100].
        // NaN/Inf must be rejected — NaN defeats comparisons (all return false)
        // and would corrupt financial calculations.
        if !self.risk.max_daily_loss_percent.is_finite()
            || self.risk.max_daily_loss_percent <= 0.0
            || self.risk.max_daily_loss_percent > 100.0
        {
            bail!(
                "risk.max_daily_loss_percent must be a finite value in (0, 100], got {}",
                self.risk.max_daily_loss_percent
            );
        }

        // Instrument: download timeout must be positive.
        if self.instrument.csv_download_timeout_secs == 0 {
            bail!("instrument.csv_download_timeout_secs must be > 0");
        }

        // Order runtime (2026-07-14): the reconcile cadence has a 60s floor
        // (a tighter loop is pointless in dry-run and a REST hammer live),
        // and the mark channel must stay bounded within a sane envelope.
        if self.order_runtime.enabled {
            if self.order_runtime.reconcile_interval_secs < 60 {
                bail!(
                    "order_runtime.reconcile_interval_secs ({}) must be >= 60",
                    self.order_runtime.reconcile_interval_secs
                );
            }
            if !(256..=65_536).contains(&self.order_runtime.mark_channel_capacity) {
                bail!(
                    "order_runtime.mark_channel_capacity ({}) must be in [256, 65536]",
                    self.order_runtime.mark_channel_capacity
                );
            }
        }

        // Instrument: build window start must be before end.
        // `window_start` and `window_end` already parsed above — no redundant re-parse.
        if window_start >= window_end {
            bail!(
                "instrument.build_window_start ({}) must be before build_window_end ({})",
                self.instrument.build_window_start,
                self.instrument.build_window_end
            );
        }

        // WebSocket: ping interval must be positive.
        if self.websocket.ping_interval_secs == 0 {
            bail!("websocket.ping_interval_secs must be > 0");
        }

        // WebSocket: subscription batch must be in [1, 100] (Dhan limit).
        if self.websocket.subscription_batch_size == 0
            || self.websocket.subscription_batch_size > crate::constants::SUBSCRIPTION_BATCH_SIZE
        {
            bail!(
                "websocket.subscription_batch_size must be in [1, {}], got {}",
                crate::constants::SUBSCRIPTION_BATCH_SIZE,
                self.websocket.subscription_batch_size
            );
        }

        // WebSocket: reconnect_max_attempts == 0 means infinite retries (production default).
        // No validation needed — 0 is a valid sentinel for "never give up".

        // Dhan: max_websocket_connections must be positive (prevents division-by-zero in pool).
        if self.dhan.max_websocket_connections == 0 {
            bail!("dhan.max_websocket_connections must be > 0");
        }

        // WebSocket: pong_timeout_secs must be positive (used in read timeout calculation).
        if self.websocket.pong_timeout_secs == 0 {
            bail!("websocket.pong_timeout_secs must be > 0");
        }

        // WebSocket: computed read timeout must be reasonable relative to Dhan's
        // 40s server timeout. We allow up to 2× the server timeout as an upper
        // bound — beyond that, the config is likely misconfigured.
        let computed_read_timeout = self
            .websocket
            .ping_interval_secs
            .saturating_mul(u64::from(self.websocket.max_consecutive_pong_failures) + 1)
            .saturating_add(self.websocket.pong_timeout_secs);
        let max_reasonable_timeout = crate::constants::SERVER_PING_TIMEOUT_SECS * 2;
        if computed_read_timeout > max_reasonable_timeout {
            bail!(
                "websocket read timeout ({}s = ping_interval × (max_failures+1) + pong_timeout) \
                 exceeds {}s — Dhan server disconnects at {}s",
                computed_read_timeout,
                max_reasonable_timeout,
                crate::constants::SERVER_PING_TIMEOUT_SECS
            );
        }

        // Notification: send timeout must be positive.
        if self.notification.send_timeout_ms == 0 {
            bail!("notification.send_timeout_ms must be > 0");
        }

        // Trading calendar: validate all holiday date strings parse correctly
        // and none fall on weekends. This also constructs the calendar to verify
        // internal consistency.
        TradingCalendar::from_config(&self.trading)?;

        // Historical: validate if enabled.
        if self.historical.enabled {
            if self.historical.lookback_days == 0
                || self.historical.lookback_days
                    > crate::constants::DHAN_INTRADAY_MAX_DAYS_PER_REQUEST
            {
                bail!(
                    "historical.lookback_days must be in [1, {}], got {}",
                    crate::constants::DHAN_INTRADAY_MAX_DAYS_PER_REQUEST,
                    self.historical.lookback_days
                );
            }
            if self.historical.request_timeout_secs == 0 {
                bail!("historical.request_timeout_secs must be > 0");
            }
        }

        // D1: Sandbox enforcement — prevent live trading before LIVE_TRADING_EARLIEST_DATE.
        // This is a fail-fast guard at config validation time. If someone sets
        // mode = "live" before the date, the application refuses to start.
        if self.strategy.mode.is_live() {
            let today = ist_date_from_utc(chrono::Utc::now());
            let earliest = match chrono::NaiveDate::from_ymd_opt(
                crate::constants::LIVE_TRADING_EARLIEST_YEAR,
                crate::constants::LIVE_TRADING_EARLIEST_MONTH,
                crate::constants::LIVE_TRADING_EARLIEST_DAY,
            ) {
                Some(d) => d,
                None => bail!("LIVE_TRADING_EARLIEST_DATE constants are invalid"),
            };
            if is_before_live_trading_earliest(today, earliest) {
                // E1 (deferred): a `tv_sandbox_gate_blocks_total` counter
                // would require pulling the `metrics` crate into common,
                // which is currently framework-free. The bail!() already
                // fires an ERROR log via anyhow chain at the boot caller,
                // and the ERROR log path fires Telegram via the existing
                // hook — so the operator already gets notified. Revisit
                // if we ever need a Prometheus time-series of block count.
                bail!(
                    "SANDBOX GUARD: live trading mode is locked until {}. \
                     Current date (IST): {}. Use mode = \"sandbox\" or \"paper\" until then.",
                    earliest,
                    today
                );
            }
        }

        // LOUD-EXPIRY tripwire (2026-07-14 re-arm): every date safety gate
        // that has silently passed its date is a no-op — the exact class bug
        // that left all three gates dead between 2026-07-01 and 2026-07-14.
        // One warn! per expired gate at every boot (runtime-only; no
        // time-bomb ratchet — unit tests inject dates).
        {
            let today = ist_date_from_utc(chrono::Utc::now());
            for gate in expired_live_gates(today, &self.strategy.sandbox_only_until) {
                tracing::warn!(
                    gate,
                    "date safety gate {gate} is in the PAST — it is a silent \
                     no-op; re-arm it with a dated operator quote"
                );
            }
        }

        // Gap 6: URL format validation — fail-fast on invalid URLs.
        // Catches typos and misconfiguration at boot instead of cryptic runtime errors.
        let validate_url = |name: &str, url: &str, required_scheme: &str| -> Result<()> {
            if url.is_empty() {
                bail!("{name} must not be empty");
            }
            if !url.starts_with(required_scheme) {
                bail!("{name} must start with '{required_scheme}', got '{url}'");
            }
            Ok(())
        };

        validate_url(
            "dhan.rest_api_base_url",
            &self.dhan.rest_api_base_url,
            "https://",
        )?;
        validate_url("dhan.auth_base_url", &self.dhan.auth_base_url, "https://")?;
        validate_url("dhan.websocket_url", &self.dhan.websocket_url, "wss://")?;
        validate_url(
            "dhan.order_update_websocket_url",
            &self.dhan.order_update_websocket_url,
            "wss://",
        )?;
        validate_url(
            "dhan.instrument_csv_url",
            &self.dhan.instrument_csv_url,
            "https://",
        )?;
        // sandbox_base_url is optional (empty when mode=paper).
        if !self.dhan.sandbox_base_url.is_empty() {
            validate_url(
                "dhan.sandbox_base_url",
                &self.dhan.sandbox_base_url,
                "https://",
            )?;
        }

        // 2026-07-14 operator pacing directive: the shared Dhan Data-API
        // limiter cap must stay inside the 2..=4 ladder, and the spot-1m
        // batch catch-up cadence must be a sane in-session interval —
        // both rejected at boot, BEFORE any REST task spawns.
        self.dhan_data_api.validate()?;
        self.spot_1m_rest.validate()?;

        // 2026-07-14: scheduled OMS reconcile cadence must stay inside the
        // 60..=3600s envelope — rejected at boot, BEFORE the pipeline spawns.
        self.oms_reconcile.validate()?;

        // 2026-07-14 Dhan margin gate: the shared-account budget/self-cap
        // envelope (≤50% of the pooled balance, ≤10 req/sec) is rejected at
        // boot, BEFORE any gate could consult it.
        self.dhan_margin_gate.validate()?;

        // 🔷 DHAN exit-order layer (Cluster B, 2026-07-14): verify-ladder
        // bounds always; freeze-limit + review-date sanity when enabled —
        // rejected at boot, BEFORE the trading pipeline spawns.
        self.exit_orders.validate()?;
        // Cadence scheduler (operator 2026-07-14): the structural zero-429
        // spacing floors are validated at boot, BEFORE the runner spawns
        // (fail-closed; the default cadence.enabled=false section is always
        // valid, so today's boot is unaffected).
        self.cadence.validate()?;

        // CAPTURE-LEG MUTUAL EXCLUSION (coordinator ruling B, 2026-07-16
        // — SUBSUME, NEVER SHARE, interim fail-closed path): the cadence
        // scheduler and the legacy per-minute RECORD-capture legs would
        // place DOUBLE demand on the same broker rate budgets if both ran
        // for one broker — no double demand is ever legal. Enabling the
        // cadence requires the legacy legs to stand down FIRST; full
        // subsumption (the cadence feeding the capture tables) is the
        // flagged follow-up PR. base.toml today (legacy legs on, cadence
        // off) stays valid.
        //
        // RS3 (2026-07-16): keyed on the LEG configs ALONE — deliberately
        // NOT on `feeds.*_enabled`. The cadence lanes activate on the
        // RUNTIME feed atomics (one toggle away from the boot flags),
        // while the legacy legs spawn on their OWN config gates
        // regardless of the feed flags — so the earlier boot-time key on
        // `feeds.*_enabled` admitted cadence=ON + feed=OFF + legs=ON, one
        // runtime feed enable away from reconstructing the forbidden
        // double demand. (Today both enable directions happen to be
        // unconditionally 409'd at the API — the PR-C2/S2b retired-lane
        // refusals — but those refusals exist for unrelated reasons and
        // must not be this invariant's only wall.) Fail-closed at the
        // root: cadence + ANY legacy leg of the same broker is refused,
        // whatever the feed flags say.
        if self.cadence.enabled {
            if self.spot_1m_rest.enabled || self.option_chain_1m.enabled {
                bail!(
                    "cadence.enabled requires the legacy Dhan per-minute legs to stand down first: set [spot_1m_rest].enabled = false and [option_chain_1m].enabled = false (no double demand on the Dhan Data-API budget is ever legal — coordinator ruling B, 2026-07-16; keyed on the leg configs alone since RS3, regardless of feeds.dhan_enabled)"
                );
            }
            if self.groww_spot_1m.enabled
                || self.groww_option_chain_1m.enabled
                || self.groww_contract_1m.enabled
            {
                bail!(
                    "cadence.enabled requires the legacy Groww per-minute legs to stand down first: set [groww_spot_1m].enabled = false, [groww_option_chain_1m].enabled = false and [groww_contract_1m].enabled = false (no double demand on the shared Groww rate budget is ever legal — coordinator ruling B, 2026-07-16; keyed on the leg configs alone since RS3, regardless of feeds.groww_enabled)"
                );
            }
        }

        // 2026-07-16 REST-era candle derivation: the boot catch-up window
        // must be a sane 1..=370-day envelope — rejected at boot, BEFORE
        // the fold task spawns.
        self.rest_candle_fold.validate()?;

        // 2026-07-16 RAM residency stores (PR-2): spot ring depth + chain
        // per-minute row cap must be sane — rejected at boot, BEFORE any
        // store installs.
        self.market_ram_store.validate()?;

        Ok(())
    }
}

/// IST calendar date for a UTC instant (UTC + 5:30, per
/// `IST_UTC_OFFSET_SECONDS_I64`).
///
/// Extracted from `ApplicationConfig::validate` (B6 mutation hardening,
/// 2026-07-03) so the offset arithmetic is deterministically testable — the
/// inline `chrono::Utc::now() + ...` form made the `+` vs `-` mutant killable
/// only within 5h30m of an IST midnight boundary (flaky by construction).
fn ist_date_from_utc(now_utc: chrono::DateTime<chrono::Utc>) -> chrono::NaiveDate {
    (now_utc + chrono::TimeDelta::seconds(crate::constants::IST_UTC_OFFSET_SECONDS_I64))
        .date_naive()
}

/// True when live trading must be refused: strictly BEFORE the earliest
/// permitted date. On the earliest date itself live mode is ALLOWED.
///
/// Extracted from `ApplicationConfig::validate` (B6 mutation hardening,
/// 2026-07-03) so the `<` boundary is deterministically testable instead of
/// depending on the wall-clock date at test time.
fn is_before_live_trading_earliest(
    today_ist: chrono::NaiveDate,
    earliest: chrono::NaiveDate,
) -> bool {
    today_ist < earliest
}

/// LOUD-EXPIRY tripwire (2026-07-14 re-arm): returns the names of the date
/// safety gates whose date is strictly in the PAST relative to `today_ist` —
/// i.e. gates that have become silent no-ops. All three gates expired
/// unnoticed between 2026-07-01 and 2026-07-14; `validate()` now warns once
/// per expired gate at every boot so that class of drift can never recur
/// silently. Pure (date injected) so unit tests never depend on the wall
/// clock — no time-bomb ratchets.
///
/// Gates checked (the single sources of truth):
/// - `LIVE_TRADING_EARLIEST` — the `LIVE_TRADING_EARLIEST_*` constants
///   (config-level Live-mode boot gate, strict `<` on IST date).
/// - `SANDBOX_DEADLINE_EPOCH_SECS` — the OMS `place_order` epoch sentinel
///   (converted to its UTC calendar date).
/// - `sandbox_only_until_default` — the serde default for
///   `[strategy] sandbox_only_until`.
/// - `sandbox_only_until_configured` — the CONFIGURED `[strategy]
///   sandbox_only_until` value (review round 1, 2026-07-14: the historical
///   incident WAS the configured base.toml value `2026-06-30` expiring, not
///   the compiled default). The documented disable sentinel
///   [`SANDBOX_ONLY_UNTIL_DISABLE_SENTINEL`] (`1970-01-01`) is exempt —
///   that expiry is intentional; a value equal to the compiled default is
///   also skipped (gate 3 already covers that exact date — no double warn).
///   An unparseable configured value is not this tripwire's job —
///   `check_sandbox_window` rejects it on the Live path.
fn expired_live_gates(
    today_ist: chrono::NaiveDate,
    configured_sandbox_only_until: &str,
) -> Vec<&'static str> {
    let mut expired = Vec::new();

    if let Some(earliest) = chrono::NaiveDate::from_ymd_opt(
        crate::constants::LIVE_TRADING_EARLIEST_YEAR,
        crate::constants::LIVE_TRADING_EARLIEST_MONTH,
        crate::constants::LIVE_TRADING_EARLIEST_DAY,
    ) && earliest < today_ist
    {
        expired.push("LIVE_TRADING_EARLIEST");
    }

    if let Some(deadline_utc) =
        chrono::DateTime::from_timestamp(crate::constants::SANDBOX_DEADLINE_EPOCH_SECS, 0)
        && deadline_utc.date_naive() < today_ist
    {
        expired.push("SANDBOX_DEADLINE_EPOCH_SECS");
    }

    if let Ok(cutoff) = chrono::NaiveDate::parse_from_str(&default_sandbox_only_until(), "%Y-%m-%d")
        && cutoff < today_ist
    {
        // NOTE: this label covers BOTH shapes — default-by-absence AND a CONFIGURED value explicitly equal to the (expired) default (gate 4's `!= default` dedup routes that shape here; pinned by test_expired_live_gates_all_at_2100).
        expired.push("sandbox_only_until_default");
    }

    if configured_sandbox_only_until != default_sandbox_only_until()
        && configured_sandbox_only_until != SANDBOX_ONLY_UNTIL_DISABLE_SENTINEL
        && let Ok(cutoff) =
            chrono::NaiveDate::parse_from_str(configured_sandbox_only_until, "%Y-%m-%d")
        && cutoff < today_ist
    {
        expired.push("sandbox_only_until_configured");
    }

    expired
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // =======================================================================
    // S6-Step4: Sandbox-only enforcement tests
    // =======================================================================

    fn make_sandbox_config(mode: TradingMode, dry_run: bool, until: &str) -> StrategyConfig {
        StrategyConfig {
            config_path: "test.toml".to_string(),
            capital: 100_000.0,
            dry_run,
            mode,
            sandbox_only_until: until.to_string(),
        }
    }

    #[test]
    fn test_sandbox_only_until_blocks_real_orders() {
        // Live trading + sandbox window not yet expired → BLOCKED.
        let cfg = make_sandbox_config(TradingMode::Live, false, "2026-06-30");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 4, 14).unwrap();
        let result = cfg.check_sandbox_window(today);
        assert!(
            result.is_err(),
            "live trading before cutoff must be blocked"
        );
        let err = result.unwrap_err();
        assert!(err.contains("SANDBOX-ONLY VIOLATION"), "error: {err}");
        assert!(err.contains("2026-06-30"), "error must cite cutoff: {err}");
    }

    #[test]
    fn test_sandbox_already_past_returns_ok() {
        // Live trading + sandbox window already expired → OK.
        let cfg = make_sandbox_config(TradingMode::Live, false, "2026-06-30");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        assert!(cfg.check_sandbox_window(today).is_ok());
    }

    #[test]
    fn test_sandbox_paper_mode_always_ok() {
        // Paper mode is always allowed regardless of cutoff.
        let cfg = make_sandbox_config(TradingMode::Paper, false, "2030-01-01");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 4, 14).unwrap();
        assert!(cfg.check_sandbox_window(today).is_ok());
    }

    #[test]
    fn test_sandbox_sandbox_mode_always_ok() {
        // Sandbox mode is always allowed.
        let cfg = make_sandbox_config(TradingMode::Sandbox, false, "2030-01-01");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 4, 14).unwrap();
        assert!(cfg.check_sandbox_window(today).is_ok());
    }

    #[test]
    fn test_sandbox_dry_run_with_paper_ok() {
        // Even with cutoff in the future, dry_run + Paper is OK.
        let cfg = make_sandbox_config(TradingMode::Paper, true, "2030-01-01");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 4, 14).unwrap();
        assert!(cfg.check_sandbox_window(today).is_ok());
    }

    #[test]
    fn test_sandbox_date_parses_invalid() {
        // Bad date string returns Err with parse details.
        let cfg = make_sandbox_config(TradingMode::Live, false, "not-a-date");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 4, 14).unwrap();
        let result = cfg.check_sandbox_window(today);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid sandbox_only_until"));
    }

    #[test]
    fn test_sandbox_date_parses_valid() {
        // Valid YYYY-MM-DD parses correctly.
        let cfg = make_sandbox_config(TradingMode::Live, false, "2026-12-31");
        let today = chrono::NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        assert!(cfg.check_sandbox_window(today).is_ok());
    }

    #[test]
    fn test_sandbox_default_value_is_2099_sentinel() {
        // 2026-07-14 re-arm: the default is the 2099-12-31 sentinel matching
        // production.toml — an absent key means ARMED, never silently expired.
        assert_eq!(default_sandbox_only_until(), "2099-12-31");
    }

    #[test]
    fn test_sandbox_exact_cutoff_day_still_blocks() {
        // On the cutoff date itself, Live is still blocked (inclusive).
        let cfg = make_sandbox_config(TradingMode::Live, false, "2026-06-30");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 30).unwrap();
        let result = cfg.check_sandbox_window(today);
        assert!(
            result.is_err(),
            "cutoff day must be blocked; only days AFTER are allowed"
        );
    }

    #[test]
    fn test_sandbox_live_with_dry_run_still_blocks() {
        // B6 mutation kill (config.rs:435 `&&`→`||` + delete-`!`): Live mode
        // MUST be blocked before the cutoff even when dry_run = true — the
        // dry_run early-exit applies ONLY to non-live modes. A mutant that
        // turns `!is_live && dry_run` into `!is_live || dry_run` (or drops
        // the `!`) would let a live+dry_run config skip the sandbox gate.
        let cfg = make_sandbox_config(TradingMode::Live, true, "2026-06-30");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 4, 14).unwrap();
        let result = cfg.check_sandbox_window(today);
        assert!(
            result.is_err(),
            "Live + dry_run before cutoff must STILL be blocked"
        );
        assert!(result.unwrap_err().contains("SANDBOX-ONLY VIOLATION"));
    }

    #[test]
    fn test_sandbox_non_live_with_dry_run_ok_before_cutoff() {
        // Companion boundary: non-live + dry_run takes the first early-exit.
        let cfg = make_sandbox_config(TradingMode::Paper, true, "2030-01-01");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 4, 14).unwrap();
        assert!(cfg.check_sandbox_window(today).is_ok());
    }

    // =======================================================================
    // B6 mutation kills: serde default helpers (exact pinned values)
    // =======================================================================

    #[test]
    fn test_default_activity_watchdog_threshold_secs_is_exactly_50() {
        // Kills `default_activity_watchdog_threshold_secs -> 0/1` mutants.
        // 50 mirrors WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS in the core crate
        // (cross-crate equality ratcheted there — see the fn's doc comment).
        assert_eq!(default_activity_watchdog_threshold_secs(), 50);
    }

    #[test]
    fn test_default_retention_days_is_exactly_90() {
        // Kills `default_retention_days -> 0/1` mutants. 90 days of hot
        // partitions is the documented retention default.
        assert_eq!(default_retention_days(), 90);
        assert_eq!(PartitionRetentionConfig::default().retention_days, 90);
    }

    #[test]
    fn test_partition_retention_serde_defaults_backward_compatible() {
        // A pre-2026-07-13 config carrying ONLY retention_days must parse
        // with the archive leg OFF and the documented class defaults —
        // missing keys = legacy behaviour (archive_enabled false).
        let cfg: PartitionRetentionConfig =
            toml::from_str("retention_days = 90").expect("legacy section must parse");
        assert_eq!(cfg.retention_days, 90);
        // 2026-07-16: default raised 14 → 35 (operator one-month spot window).
        assert_eq!(cfg.market_data_hot_days, 35);
        assert!(!cfg.archive_enabled, "archive leg must default OFF");
        assert!(cfg.archive_bucket.is_empty(), "bucket must default derived");
        assert_eq!(cfg.max_partitions_per_run, 200);
    }

    #[test]
    fn test_partition_retention_archive_enabled_default_false() {
        // The destructive leg must be explicitly configured on. Kills
        // `default -> true` mutants and pins the instant-rollback contract
        // (delete the key ⇒ detach-only legacy behaviour).
        assert!(!PartitionRetentionConfig::default().archive_enabled);
        // 2026-07-16: default raised 14 → 35 (operator one-month spot window).
        assert_eq!(default_market_data_hot_days(), 35);
        assert_eq!(default_max_partitions_per_run(), 200);
        let cfg: PartitionRetentionConfig =
            toml::from_str("").expect("empty section must parse via defaults");
        assert!(!cfg.archive_enabled);
    }

    #[test]
    fn test_partition_retention_full_section_parses() {
        let cfg: PartitionRetentionConfig = toml::from_str(
            "retention_days = 90\nmarket_data_hot_days = 35\narchive_enabled = true\narchive_bucket = \"tv-prod-cold\"\nmax_partitions_per_run = 50\n",
        )
        .expect("full section must parse");
        assert!(cfg.archive_enabled);
        assert_eq!(cfg.archive_bucket, "tv-prod-cold");
        assert_eq!(cfg.market_data_hot_days, 35);
        assert_eq!(cfg.max_partitions_per_run, 50);
    }

    // =======================================================================
    // [rest_candle_fold] — RestCandleFoldConfig pins (rest-candle plan Item 2)
    // =======================================================================

    #[test]
    fn test_rest_candle_fold_config_default_off() {
        // Fail-safe pin: the fold is OFF unless a config explicitly turns
        // it on — `Default` and the serde default (absent keys) must agree.
        assert!(!RestCandleFoldConfig::default().enabled);
        assert_eq!(RestCandleFoldConfig::default().catchup_days, 35);
        let cfg: RestCandleFoldConfig = toml::from_str("")
            .expect("empty [rest_candle_fold] section must parse via serde defaults");
        assert!(!cfg.enabled, "absent enabled key must deserialize to false");
        assert_eq!(cfg.catchup_days, 35, "serde catchup_days default is 35");
    }

    #[test]
    fn test_rest_candle_fold_config_validate_bounds() {
        // The 1..=370 catch-up envelope: both edges accepted, both
        // neighbours rejected (a 0/371 window is a config typo, not a
        // legitimate ask).
        let cfg = |catchup_days: u32| RestCandleFoldConfig {
            enabled: true,
            catchup_days,
        };
        assert!(cfg(1).validate().is_ok(), "lower edge 1 must be accepted");
        assert!(
            cfg(370).validate().is_ok(),
            "upper edge 370 must be accepted"
        );
        assert!(cfg(0).validate().is_err(), "0 must be rejected");
        assert!(cfg(371).validate().is_err(), "371 must be rejected");
    }

    // =======================================================================
    // [market_ram_store] — MarketRamStoreConfig pins (ram-residency plan
    // Item 1, operator directive 2026-07-16)
    // =======================================================================

    #[test]
    fn test_market_ram_store_config_default_off() {
        // Fail-safe: Default AND an empty-TOML deserialize are BOTH
        // disabled with the documented depth/cap defaults — an absent
        // [market_ram_store] section installs nothing.
        assert!(!MarketRamStoreConfig::default().enabled);
        assert_eq!(MarketRamStoreConfig::default().spot_days, 35);
        assert_eq!(MarketRamStoreConfig::default().chain_row_cap, 1_000);
        let cfg: MarketRamStoreConfig =
            toml::from_str("").expect("an empty [market_ram_store] section must deserialize");
        assert!(!cfg.enabled, "serde default must be OFF (fail-safe)");
        assert_eq!(cfg.spot_days, 35);
        assert_eq!(cfg.chain_row_cap, 1_000);
    }

    #[test]
    fn test_market_ram_store_config_validate_bounds() {
        // spot_days shares the 1..=370 envelope with
        // rest_candle_fold.catchup_days; chain_row_cap is 1..=10_000.
        let cfg = |spot_days: u32, chain_row_cap: u32| MarketRamStoreConfig {
            enabled: true,
            spot_days,
            chain_row_cap,
        };
        assert!(cfg(1, 1).validate().is_ok(), "lower edges must be accepted");
        assert!(
            cfg(370, 10_000).validate().is_ok(),
            "upper edges must be accepted"
        );
        assert!(cfg(0, 1_000).validate().is_err(), "spot_days 0 rejected");
        assert!(
            cfg(371, 1_000).validate().is_err(),
            "spot_days 371 rejected"
        );
        assert!(cfg(35, 0).validate().is_err(), "chain_row_cap 0 rejected");
        assert!(
            cfg(35, 10_001).validate().is_err(),
            "chain_row_cap 10001 rejected"
        );
    }

    // =======================================================================
    // B6 mutation kills: live-trading sandbox gate pure helpers
    // =======================================================================

    #[test]
    fn test_ist_date_from_utc_crosses_midnight_forward() {
        // 2026-01-01T20:00:00Z = 2026-01-02 01:30 IST → date must be Jan 2.
        // Kills `+`→`-` (UTC−5:30 would give Jan 1 14:30 → Jan 1).
        let utc = chrono::DateTime::parse_from_rfc3339("2026-01-01T20:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            ist_date_from_utc(utc),
            chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()
        );
    }

    #[test]
    fn test_ist_date_from_utc_early_utc_hours_stay_same_day() {
        // 2026-01-01T05:00:00Z = 2026-01-01 10:30 IST → Jan 1. Under the
        // `-` mutant this would be 2025-12-31 23:30 → Dec 31 (killed).
        let utc = chrono::DateTime::parse_from_rfc3339("2026-01-01T05:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            ist_date_from_utc(utc),
            chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
        );
    }

    #[test]
    fn test_is_before_live_trading_earliest_boundary() {
        // Kills `<`→`<=` (earliest day itself must be ALLOWED), `<`→`==`
        // (day before must block), and `<`→`>` (day after must not block).
        let earliest = chrono::NaiveDate::from_ymd_opt(
            crate::constants::LIVE_TRADING_EARLIEST_YEAR,
            crate::constants::LIVE_TRADING_EARLIEST_MONTH,
            crate::constants::LIVE_TRADING_EARLIEST_DAY,
        )
        .unwrap();
        let day_before = earliest.pred_opt().unwrap();
        let day_after = earliest.succ_opt().unwrap();
        assert!(
            is_before_live_trading_earliest(day_before, earliest),
            "day before earliest must be blocked"
        );
        assert!(
            !is_before_live_trading_earliest(earliest, earliest),
            "the earliest date itself must be allowed (strict <, not <=)"
        );
        assert!(
            !is_before_live_trading_earliest(day_after, earliest),
            "days after earliest must be allowed"
        );
    }

    // =======================================================================
    // LOUD-EXPIRY tripwire tests (2026-07-14 re-arm) — dates injected, never
    // wall-clock-dependent (no time-bomb tests).
    // =======================================================================

    #[test]
    fn test_expired_live_gates_empty_today() {
        // At the re-arm date (2026-07-14) every gate carries the 2099-12-31
        // sentinel — nothing is expired.
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 14).unwrap();
        assert!(
            expired_live_gates(today, &default_sandbox_only_until()).is_empty(),
            "no gate may read expired at the 2026-07-14 re-arm date"
        );
    }

    #[test]
    fn test_expired_live_gates_all_at_2100() {
        // Past the sentinel, all three compile-time gates are silent no-ops —
        // the tripwire must name every one of them. A configured value EQUAL
        // to the compiled default must NOT double-warn (gate 3 already covers
        // that exact date — no `sandbox_only_until_configured` entry).
        let today = chrono::NaiveDate::from_ymd_opt(2100, 1, 1).unwrap();
        let expired = expired_live_gates(today, &default_sandbox_only_until());
        assert_eq!(
            expired,
            vec![
                "LIVE_TRADING_EARLIEST",
                "SANDBOX_DEADLINE_EPOCH_SECS",
                "sandbox_only_until_default",
            ],
            "at 2100-01-01 all three date gates must be reported expired, \
             with NO duplicate configured entry for the default value"
        );
    }

    #[test]
    fn test_expired_live_gates_per_gate_granularity() {
        // The check is strictly-past per gate: ON the sentinel date itself no
        // gate is expired; the DAY AFTER, every 2099-12-31 gate is.
        let sentinel = chrono::NaiveDate::from_ymd_opt(2099, 12, 31).unwrap();
        assert!(
            expired_live_gates(sentinel, &default_sandbox_only_until()).is_empty(),
            "a gate dated today is NOT expired (strictly-past check)"
        );
        let day_after = sentinel.succ_opt().unwrap();
        assert_eq!(
            expired_live_gates(day_after, &default_sandbox_only_until()).len(),
            3,
            "the day after the sentinel every gate reads expired"
        );
    }

    #[test]
    fn test_expired_live_gates_configured_past_date_warns() {
        // Review round 1 (2026-07-14): the historical incident WAS the
        // configured base.toml value (2026-06-30) expiring — a configured
        // past date that is neither the default nor the disable sentinel
        // MUST be named by the tripwire.
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 14).unwrap();
        assert_eq!(
            expired_live_gates(today, "2026-06-30"),
            vec!["sandbox_only_until_configured"],
            "a configured past date (the 2026-06-30 incident class) must warn"
        );
    }

    #[test]
    fn test_expired_live_gates_configured_future_silent() {
        // A configured FUTURE date (non-default) is an armed gate — silent.
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 14).unwrap();
        assert!(
            expired_live_gates(today, "2097-01-01").is_empty(),
            "a configured future date must not warn"
        );
    }

    #[test]
    fn test_expired_live_gates_disable_sentinel_silent() {
        // The documented intentional-disable sentinel (exactly 1970-01-01)
        // is exempt — that expiry is deliberate, not drift.
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 14).unwrap();
        assert!(
            expired_live_gates(today, SANDBOX_ONLY_UNTIL_DISABLE_SENTINEL).is_empty(),
            "the documented 1970-01-01 disable sentinel must not warn"
        );
        // But any OTHER ancient date is NOT the sentinel — it warns.
        assert_eq!(
            expired_live_gates(today, "1970-01-02"),
            vec!["sandbox_only_until_configured"],
            "only the exact documented sentinel is exempt"
        );
    }

    #[test]
    fn test_expired_live_gates_configured_and_default_both_named_when_distinct() {
        // Past the default sentinel with a DIFFERENT configured past date:
        // both the default gate and the configured gate must be named.
        let today = chrono::NaiveDate::from_ymd_opt(2100, 1, 2).unwrap();
        let expired = expired_live_gates(today, "2099-06-30");
        assert!(
            expired.contains(&"sandbox_only_until_default"),
            "the expired compiled default must still be caught: {expired:?}"
        );
        assert!(
            expired.contains(&"sandbox_only_until_configured"),
            "the distinct expired configured value must also be caught: {expired:?}"
        );
    }

    // =======================================================================

    /// Helper: creates a valid ApplicationConfig for testing.
    /// Modify individual fields to test specific validation failures.
    fn make_valid_config() -> ApplicationConfig {
        ApplicationConfig {
            trading: TradingConfig {
                market_open_time: "09:00:00".to_string(),
                market_close_time: "15:30:00".to_string(),
                order_cutoff_time: "15:29:00".to_string(),
                data_collection_start: "09:00:00".to_string(),
                data_collection_end: "15:30:00".to_string(),
                timezone: "Asia/Kolkata".to_string(),
                max_orders_per_second: 10,
                nse_holidays: vec![
                    NseHolidayEntry {
                        date: "2026-01-26".to_string(),
                        name: "Republic Day".to_string(),
                    },
                    NseHolidayEntry {
                        date: "2026-03-03".to_string(),
                        name: "Holi".to_string(),
                    },
                ],
                muhurat_trading_dates: vec![NseHolidayEntry {
                    date: "2026-11-08".to_string(),
                    name: "Diwali 2026".to_string(),
                }],
                nse_mock_trading_dates: vec![],
            },
            dhan: DhanConfig {
                websocket_url: "wss://api-feed.dhan.co".to_string(),
                order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
                rest_api_base_url: "https://api.dhan.co/v2".to_string(),
                sandbox_base_url: "https://sandbox.dhan.co/v2".to_string(),
                auth_base_url: "https://auth.dhan.co".to_string(),
                instrument_csv_url: "https://images.dhan.co/api-data/api-scrip-master-detailed.csv"
                    .to_string(),
                instrument_csv_fallback_url: "https://images.dhan.co/api-data/api-scrip-master.csv"
                    .to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
            },
            websocket: WebSocketConfig {
                ping_interval_secs: 10,
                pong_timeout_secs: 10,
                max_consecutive_pong_failures: 2,
                reconnect_initial_delay_ms: 500,
                reconnect_max_delay_ms: 30000,
                reconnect_max_attempts: 10,
                subscription_batch_size: 100,
                // Phase 0 Item 6 (operator-locked 2026-05-13): reduced from
                // 10000ms to 2000ms. Under Phase 0 LEAN MVP (1 main-feed
                // conn) the stagger is a no-op; under legacy / Wave 5
                // scopes (up to 5 conns) 2s × 4 = 8s startup delay still
                // safely under Dhan's burst-rate thresholds while
                // shrinking boot time from 40s to 8s.
                connection_stagger_ms: 2000,
                activity_watchdog_threshold_secs: default_activity_watchdog_threshold_secs(),
            },
            questdb: QuestDbConfig {
                host: "tv-questdb".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            // `prometheus: PrometheusConfig` DELETED in #O5 (2026-05-30).
            network: NetworkConfig {
                request_timeout_ms: 5000,
                websocket_connect_timeout_ms: 10000,
                retry_initial_delay_ms: 100,
                retry_max_delay_ms: 30000,
                retry_max_attempts: 5,
            },
            token: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            risk: RiskConfig {
                max_daily_loss_percent: 2.0,
                max_position_size_lots: 100,
            },
            logging: LoggingConfig {
                level: "info".to_string(),
                format: "json".to_string(),
                log_to_stdout: false,
            },
            instrument: InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/tv-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            api: ApiConfig {
                host: "0.0.0.0".to_string(),
                port: 3001,
                allowed_origins: default_allowed_origins(),
            },
            notification: NotificationConfig::default(),
            observability: ObservabilityConfig::default(),
            historical: HistoricalDataConfig::default(),
            strategy: StrategyConfig::default(),
            index_constituency: IndexConstituencyConfig::default(),
            greeks: GreeksConfig::default(),
            infrastructure: InfrastructureConfig::default(),
            partition_retention: PartitionRetentionConfig::default(),
            // movers: MoversConfig retired in PR #2 (2026-05-18).
            features: FeaturesConfig::default(),
            engine: EngineConfig::default(),
            in_mem: InMemConfig::default(),
            feeds: FeedsConfig::default(),
            scoreboard: ScoreboardConfig::default(),
            brutex_crossverify: BrutexCrossverifyConfig::default(),
            spot_1m_rest: Spot1mRestConfig::default(),
            oms_reconcile: OmsReconcileConfig::default(),
            dhan_data_api: DhanDataApiConfig::default(),
            option_chain_1m: OptionChain1mConfig::default(),
            groww_spot_1m: GrowwSpot1mConfig::default(),
            groww_option_chain_1m: GrowwOptionChain1mConfig::default(),
            groww_rest_burst: GrowwRestBurstConfig::default(),
            tf_consistency: TfConsistencyConfig::default(),
            rest_candle_fold: RestCandleFoldConfig::default(),
            market_ram_store: MarketRamStoreConfig::default(),
            groww_contract_1m: GrowwContract1mConfig::default(),
            groww_universe: GrowwUniverseConfig::default(),
            groww_orders: GrowwOrdersConfig::default(),
            dhan_margin_gate: DhanMarginGateConfig::default(),
            exit_orders: ExitOrdersConfig::default(),
            cadence: CadenceConfig::default(),
        }
    }

    /// PR-0 (Groww order-side build, §39.2 Gate 1): every `[groww_orders]`
    /// gate defaults OFF — the safe, dark default. A missing section must
    /// produce exactly this.
    #[test]
    fn test_groww_orders_config_defaults_all_off() {
        let cfg = GrowwOrdersConfig::default();
        assert!(!cfg.orders_read, "orders_read must default off");
        assert!(!cfg.portfolio_read, "portfolio_read must default off");
        assert!(!cfg.margin_read, "margin_read must default off");
        assert!(!cfg.user_read, "user_read must default off");
        assert!(
            !cfg.live_fire_requested,
            "live_fire_requested must default off — and is inert without Gate 3"
        );
        // Smart Orders area (2026-07-15): the two new gate bools default
        // off; the two OCO cadences carry their design values (the manual
        // impl Default — a derive would zero them and break the
        // Default↔serde-default parity for an absent section).
        assert!(!cfg.smart_orders_read, "smart_orders_read must default off");
        assert!(
            !cfg.smart_orders_write,
            "smart_orders_write must default off"
        );
        assert_eq!(
            cfg.oco_reconcile_poll_secs, 15,
            "oco_reconcile_poll_secs must default to the 15s design value"
        );
        assert_eq!(
            cfg.oco_sibling_cancel_deadline_secs, 30,
            "oco_sibling_cancel_deadline_secs must default to the 30s design value"
        );
        assert!(
            !cfg.paper_enabled,
            "paper_enabled must default off (Gate 1) — the zero-HTTP paper lane is dark by default"
        );
        assert_eq!(
            cfg.max_order_quantity, 0,
            "max_order_quantity must default 0 (refuse-all) pending the operator's 0-vs-1 answer"
        );
    }

    /// PR-0 / Smart Orders area (2026-07-15): an ABSENT `[groww_orders]`
    /// section (empty TOML) must deserialize to EXACTLY `Default`. This pins
    /// Default↔serde-default parity so a future removal of any
    /// `#[serde(default…)]` attribute — which would make an absent field a
    /// hard deserialize ERROR (the two OCO cadences) or silently zero a u64 —
    /// fails the build. (`GrowwOrdersConfig` derives no `PartialEq`, so the
    /// eleven fields are compared explicitly against `Default`.)
    #[test]
    fn test_groww_orders_config_absent_section_serde_parity() {
        let parsed: GrowwOrdersConfig = toml::from_str("")
            .expect("absent [groww_orders] section must parse via serde defaults");
        let def = GrowwOrdersConfig::default();
        assert_eq!(parsed.orders_read, def.orders_read);
        assert_eq!(parsed.portfolio_read, def.portfolio_read);
        assert_eq!(parsed.margin_read, def.margin_read);
        assert_eq!(parsed.user_read, def.user_read);
        assert_eq!(parsed.live_fire_requested, def.live_fire_requested);
        assert_eq!(parsed.smart_orders_read, def.smart_orders_read);
        assert_eq!(parsed.smart_orders_write, def.smart_orders_write);
        assert_eq!(parsed.oco_reconcile_poll_secs, def.oco_reconcile_poll_secs);
        assert_eq!(
            parsed.oco_sibling_cancel_deadline_secs,
            def.oco_sibling_cancel_deadline_secs
        );
        assert_eq!(parsed.paper_enabled, def.paper_enabled);
        assert_eq!(parsed.max_order_quantity, def.max_order_quantity);
    }

    // -----------------------------------------------------------------------
    // TradingMode tests
    // -----------------------------------------------------------------------
    #[test]
    fn test_trading_mode_default_is_paper() {
        assert_eq!(TradingMode::default(), TradingMode::Paper);
    }

    #[test]
    fn test_trading_mode_paper_no_http() {
        assert!(!TradingMode::Paper.is_http_active());
        assert!(TradingMode::Paper.is_paper());
        assert!(!TradingMode::Paper.is_live());
        assert!(!TradingMode::Paper.is_sandbox());
    }

    #[test]
    fn test_trading_mode_sandbox_has_http() {
        assert!(TradingMode::Sandbox.is_http_active());
        assert!(!TradingMode::Sandbox.is_paper());
        assert!(!TradingMode::Sandbox.is_live());
        assert!(TradingMode::Sandbox.is_sandbox());
    }

    #[test]
    fn test_trading_mode_live_has_http() {
        assert!(TradingMode::Live.is_http_active());
        assert!(!TradingMode::Live.is_paper());
        assert!(TradingMode::Live.is_live());
        assert!(!TradingMode::Live.is_sandbox());
    }

    #[test]
    fn test_trading_mode_as_str() {
        assert_eq!(TradingMode::Paper.as_str(), "paper");
        assert_eq!(TradingMode::Sandbox.as_str(), "sandbox");
        assert_eq!(TradingMode::Live.as_str(), "live");
    }

    #[test]
    fn test_trading_mode_deserialize_lowercase() {
        let paper: TradingMode = serde_json::from_str("\"paper\"").unwrap();
        assert_eq!(paper, TradingMode::Paper);
        let sandbox: TradingMode = serde_json::from_str("\"sandbox\"").unwrap();
        assert_eq!(sandbox, TradingMode::Sandbox);
        let live: TradingMode = serde_json::from_str("\"live\"").unwrap();
        assert_eq!(live, TradingMode::Live);
    }

    #[test]
    fn test_trading_mode_serialize_lowercase() {
        assert_eq!(
            serde_json::to_string(&TradingMode::Paper).unwrap(),
            "\"paper\""
        );
        assert_eq!(
            serde_json::to_string(&TradingMode::Sandbox).unwrap(),
            "\"sandbox\""
        );
        assert_eq!(
            serde_json::to_string(&TradingMode::Live).unwrap(),
            "\"live\""
        );
    }

    // -----------------------------------------------------------------------

    #[test]
    fn test_valid_config_passes_validation() {
        let config = make_valid_config();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_wrong_timezone_fails() {
        let mut config = make_valid_config();
        config.trading.timezone = "UTC".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("Asia/Kolkata"));
    }

    #[test]
    fn test_invalid_time_format_fails() {
        let mut config = make_valid_config();
        config.trading.market_open_time = "9:15".to_string(); // Missing seconds
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("market_open_time"));
    }

    #[test]
    fn test_sebi_order_limit_exceeded_fails() {
        let mut config = make_valid_config();
        config.trading.max_orders_per_second = 11; // SEBI limit is 10
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("SEBI"));
    }

    #[test]
    fn test_sebi_order_limit_at_boundary_passes() {
        let mut config = make_valid_config();
        config.trading.max_orders_per_second = 10; // Exactly at limit
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_token_refresh_exceeds_validity_fails() {
        let mut config = make_valid_config();
        config.token.refresh_before_expiry_hours = 25;
        config.token.token_validity_hours = 24;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("refresh_before_expiry_hours"));
    }

    #[test]
    fn test_token_refresh_equals_validity_fails() {
        let mut config = make_valid_config();
        config.token.refresh_before_expiry_hours = 24;
        config.token.token_validity_hours = 24;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("refresh_before_expiry_hours"));
    }

    #[test]
    fn test_zero_request_timeout_fails() {
        let mut config = make_valid_config();
        config.network.request_timeout_ms = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("request_timeout_ms"));
    }

    #[test]
    fn test_zero_retry_attempts_fails() {
        let mut config = make_valid_config();
        config.network.retry_max_attempts = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("retry_max_attempts"));
    }

    #[test]
    fn test_zero_daily_loss_percent_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = 0.0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_negative_daily_loss_percent_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = -5.0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_daily_loss_percent_over_100_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = 100.1;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_daily_loss_percent_at_100_passes() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = 100.0;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_daily_loss_percent_nan_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = f64::NAN;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_daily_loss_percent_inf_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = f64::INFINITY;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_daily_loss_percent_neg_inf_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = f64::NEG_INFINITY;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_zero_csv_download_timeout_fails() {
        let mut config = make_valid_config();
        config.instrument.csv_download_timeout_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("csv_download_timeout_secs"));
    }

    #[test]
    fn test_invalid_instrument_download_time_fails() {
        let mut config = make_valid_config();
        config.instrument.daily_download_time = "not-a-time".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("daily_download_time"));
    }

    // --- WebSocket Config Validation ---

    #[test]
    fn test_websocket_zero_ping_interval_fails() {
        let mut config = make_valid_config();
        config.websocket.ping_interval_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("ping_interval_secs"));
    }

    #[test]
    fn test_websocket_zero_subscription_batch_size_fails() {
        let mut config = make_valid_config();
        config.websocket.subscription_batch_size = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("subscription_batch_size"));
    }

    #[test]
    fn test_websocket_subscription_batch_size_over_100_fails() {
        let mut config = make_valid_config();
        config.websocket.subscription_batch_size = 101;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("subscription_batch_size"));
    }

    #[test]
    fn test_websocket_zero_reconnect_max_attempts_means_infinite() {
        let mut config = make_valid_config();
        config.websocket.reconnect_max_attempts = 0;
        // 0 = infinite retries (production default). Must pass validation.
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_websocket_subscription_batch_size_exactly_100_passes() {
        let mut config = make_valid_config();
        config.websocket.subscription_batch_size = 100;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_websocket_subscription_batch_size_exactly_1_passes() {
        let mut config = make_valid_config();
        config.websocket.subscription_batch_size = 1;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_dhan_zero_max_websocket_connections_fails() {
        let mut config = make_valid_config();
        config.dhan.max_websocket_connections = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_websocket_connections"));
    }

    #[test]
    fn test_websocket_zero_pong_timeout_fails() {
        let mut config = make_valid_config();
        config.websocket.pong_timeout_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("pong_timeout_secs"));
    }

    #[test]
    fn test_notification_zero_send_timeout_fails() {
        let mut config = make_valid_config();
        config.notification.send_timeout_ms = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("notification.send_timeout_ms"));
    }

    #[test]
    fn test_build_window_start_after_end_fails() {
        let mut config = make_valid_config();
        config.instrument.build_window_start = "09:00:00".to_string();
        config.instrument.build_window_end = "08:30:00".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("build_window_start"));
    }

    #[test]
    fn test_build_window_start_equals_end_fails() {
        let mut config = make_valid_config();
        config.instrument.build_window_start = "08:30:00".to_string();
        config.instrument.build_window_end = "08:30:00".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("build_window_start"));
    }

    #[test]
    fn test_invalid_build_window_start_format_fails() {
        let mut config = make_valid_config();
        config.instrument.build_window_start = "8:25".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("build_window_start"));
    }

    // -----------------------------------------------------------------------
    // Cross-field and boundary validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_market_close_before_open_time_values() {
        // Verify that market_close_time > market_open_time in the default config.
        // The validator doesn't currently reject this cross-field case,
        // but the parsed times should have the expected ordering.
        let config = make_valid_config();
        let open = NaiveTime::parse_from_str(&config.trading.market_open_time, "%H:%M:%S").unwrap();
        let close =
            NaiveTime::parse_from_str(&config.trading.market_close_time, "%H:%M:%S").unwrap();
        assert!(
            close > open,
            "market_close_time ({close}) must be after market_open_time ({open})"
        );
    }

    #[test]
    fn test_order_cutoff_before_close() {
        // Verify that order_cutoff_time < market_close_time in the default config.
        let config = make_valid_config();
        let cutoff =
            NaiveTime::parse_from_str(&config.trading.order_cutoff_time, "%H:%M:%S").unwrap();
        let close =
            NaiveTime::parse_from_str(&config.trading.market_close_time, "%H:%M:%S").unwrap();
        assert!(
            cutoff < close,
            "order_cutoff_time ({cutoff}) must be before market_close_time ({close})"
        );
    }

    #[test]
    fn test_historical_lookback_zero_fails() {
        let mut config = make_valid_config();
        config.historical.enabled = true;
        config.historical.lookback_days = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("lookback_days"));
    }

    #[test]
    fn test_historical_lookback_over_90_fails() {
        let mut config = make_valid_config();
        config.historical.enabled = true;
        config.historical.lookback_days = 91;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("lookback_days"));
    }

    #[test]
    fn test_historical_lookback_at_90_passes() {
        let mut config = make_valid_config();
        config.historical.enabled = true;
        config.historical.lookback_days = 90;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_historical_lookback_at_1_passes() {
        let mut config = make_valid_config();
        config.historical.enabled = true;
        config.historical.lookback_days = 1;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_historical_request_timeout_zero_fails() {
        let mut config = make_valid_config();
        config.historical.enabled = true;
        config.historical.request_timeout_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("request_timeout_secs"));
    }

    #[test]
    fn test_historical_disabled_skips_validation() {
        let mut config = make_valid_config();
        config.historical.enabled = false;
        config.historical.lookback_days = 999; // Would fail if validated
        config.historical.request_timeout_secs = 0; // Would fail if validated
        assert!(
            config.validate().is_ok(),
            "disabled historical config should skip validation"
        );
    }

    #[test]
    fn test_websocket_excessive_read_timeout_fails() {
        let mut config = make_valid_config();
        // ping_interval_secs * (max_consecutive_pong_failures + 1) + pong_timeout_secs
        // = 30 * (10 + 1) + 30 = 360s, which exceeds 2 * SERVER_PING_TIMEOUT_SECS
        config.websocket.ping_interval_secs = 30;
        config.websocket.max_consecutive_pong_failures = 10;
        config.websocket.pong_timeout_secs = 30;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("websocket read timeout"));
    }

    // Mutation-targeted tests: catch `replace + with -`, `replace * with +`,
    // `replace + with *`, `replace > with >=` in validate() read timeout logic.

    #[test]
    fn test_websocket_read_timeout_exactly_at_boundary_passes() {
        // computed = ping * (failures + 1) + pong
        // With ping=10, failures=3, pong=10: computed = 10*(3+1)+10 = 50
        // max = SERVER_PING_TIMEOUT_SECS * 2 = 80
        // 50 < 80 → should pass
        let mut config = make_valid_config();
        config.websocket.ping_interval_secs = 10;
        config.websocket.max_consecutive_pong_failures = 3;
        config.websocket.pong_timeout_secs = 10;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_websocket_read_timeout_exactly_at_max_passes() {
        // computed = ping * (failures + 1) + pong = 80 exactly
        // max = 80
        // 80 > 80 is FALSE → should PASS (equal is OK)
        // This catches `replace > with >=` mutation — if >= were used, this would fail
        let mut config = make_valid_config();
        // 10 * (6 + 1) + 10 = 10 * 7 + 10 = 80
        config.websocket.ping_interval_secs = 10;
        config.websocket.max_consecutive_pong_failures = 6;
        config.websocket.pong_timeout_secs = 10;
        assert!(
            config.validate().is_ok(),
            "computed=80, max=80 → equal should PASS (> not >=)"
        );
    }

    #[test]
    fn test_websocket_read_timeout_one_above_max_fails() {
        // computed = 81, max = 80
        // 81 > 80 → should FAIL
        let mut config = make_valid_config();
        // 10 * (6 + 1) + 11 = 81
        config.websocket.ping_interval_secs = 10;
        config.websocket.max_consecutive_pong_failures = 6;
        config.websocket.pong_timeout_secs = 11;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("websocket read timeout"));
    }

    #[test]
    fn test_websocket_read_timeout_plus_one_matters() {
        // This catches `replace + with -` and `replace + with *` on the `+ 1`
        // With failures=0: computed = ping * (0 + 1) + pong = ping + pong
        // Mutation `+ 1` → `- 1`: computed = ping * (0 - 1) + pong = UNDERFLOW or 0 + pong
        // Mutation `+ 1` → `* 1`: computed = ping * (0 * 1) + pong = 0 + pong
        // Both mutations would give a DIFFERENT computed value
        let mut config = make_valid_config();
        // Normal: 40 * (0 + 1) + 41 = 40 + 41 = 81 > 80 → FAIL
        // Mutant (-1): 40 * (0 - 1) + 41 → saturating = 0 + 41 = 41 ≤ 80 → PASS (wrong!)
        // Mutant (*1): 40 * (0 * 1) + 41 = 0 + 41 = 41 ≤ 80 → PASS (wrong!)
        config.websocket.ping_interval_secs = 40;
        config.websocket.max_consecutive_pong_failures = 0;
        config.websocket.pong_timeout_secs = 41;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("websocket read timeout"),
            "failures=0 with +1 should compute 81 > 80"
        );
    }

    #[test]
    fn test_websocket_read_timeout_times_two_matters() {
        // This catches `replace * with +` on `SERVER_PING_TIMEOUT_SECS * 2`
        // Normal max = 40 * 2 = 80
        // Mutant max = 40 + 2 = 42
        // With computed = 50: normal 50 ≤ 80 → PASS, mutant 50 > 42 → FAIL (wrong!)
        let mut config = make_valid_config();
        // computed = 10 * (3 + 1) + 10 = 50
        config.websocket.ping_interval_secs = 10;
        config.websocket.max_consecutive_pong_failures = 3;
        config.websocket.pong_timeout_secs = 10;
        // 50 ≤ 80 → PASS (correct)
        // If max were 42 (mutant): 50 > 42 → FAIL (mutation caught!)
        assert!(
            config.validate().is_ok(),
            "computed=50 should be within max=80"
        );
    }

    // PR-C3 (2026-07-14): the SubscriptionScope / SubscriptionConfig /
    // effective_main_feed_pool_size test family (feed-mode parsing, scope
    // serde round-trips, the 1-conn pool pin, the dead-flags contract)
    // retired with the deleted subscription surface (scope-lock amendment
    // §B item 2). The PHASE_0_MAIN_FEED_CONNECTION_COUNT constant pin
    // survives below (historical capacity-math anchor).

    // PR #7b — `PHASE_0_MAIN_FEED_CONNECTION_COUNT` is locked at 1.
    #[test]
    fn test_phase_0_main_feed_connection_count_constant_pinned_at_1() {
        // Defensive ratchet — Phase 0 LEAN MVP locks this at 1. Changing
        // to >1 means the Phase 0 scope is no longer "lean" by definition;
        // operator must re-approve and update PHASE_0_TOTAL_SIDS_TARGET
        // capacity math.
        assert_eq!(crate::constants::PHASE_0_MAIN_FEED_CONNECTION_COUNT, 1);
    }

    // Phase 0 Item 6 (operator-locked 2026-05-13) — stagger default pin.
    #[test]
    fn test_connection_stagger_ms_default_pinned_at_2000() {
        // 2000ms = 2s × (N-1) staggers between conns. Under Phase 0 with
        // 1 conn the stagger is a no-op; under legacy with 5 conns it's
        // 8s total. The previous 10000ms (10s × 4 = 40s) caused needless
        // boot delay. Changing this constant requires re-approving the
        // Dhan burst-rate calc.
        let cfg = make_valid_config();
        assert_eq!(cfg.websocket.connection_stagger_ms, 2000);
    }

    // =====================================================================
    // Additional coverage: default impls, edge cases, more validation paths
    // =====================================================================

    #[test]
    fn test_strategy_config_default() {
        let config = StrategyConfig::default();
        assert_eq!(config.config_path, "config/strategies.toml");
        assert!((config.capital - 1_000_000.0).abs() < f64::EPSILON);
        assert!(config.dry_run, "dry_run must default to true");
    }

    #[test]
    fn test_notification_config_default() {
        let config = NotificationConfig::default();
        assert_eq!(config.telegram_api_base_url, "https://api.telegram.org");
        assert_eq!(config.send_timeout_ms, 10_000);
        assert!(!config.sns_enabled);
        // Telegram UX Overhaul (2026-07-07) kill switches.
        assert!(config.episode_mode, "episode mode ships ON by default");
        assert_eq!(config.digest_window_secs, 900);
        // Boot bubble (2026-07-09) ships ON by default.
        assert!(config.boot_bubble, "boot bubble ships ON by default");
    }

    #[test]
    fn test_notification_digest_window_secs_clamped_to_60_3600() {
        let mut config = NotificationConfig::default();
        config.digest_window_secs = 0;
        assert_eq!(
            config.digest_window_secs_clamped(),
            NOTIFICATION_DIGEST_WINDOW_MIN_SECS,
            "fat-fingered 0 clamps to the 60s legacy floor"
        );
        config.digest_window_secs = 86_400;
        assert_eq!(
            config.digest_window_secs_clamped(),
            NOTIFICATION_DIGEST_WINDOW_MAX_SECS,
            "fat-fingered 86400 clamps to the 3600s ceiling"
        );
        config.digest_window_secs = 900;
        assert_eq!(config.digest_window_secs_clamped(), 900);
        config.digest_window_secs = 60;
        assert_eq!(
            config.digest_window_secs_clamped(),
            60,
            "60 == legacy behavior passes through"
        );
    }

    #[test]
    fn test_notification_episode_knobs_deserialize_with_defaults() {
        // Old TOML without the new keys must still deserialize (serde
        // defaults) — config-file rollback safety.
        let toml_str = r#"
            telegram_api_base_url = "https://api.telegram.org"
            send_timeout_ms = 10000
            sns_enabled = false
        "#;
        let config: NotificationConfig =
            toml::from_str(toml_str).expect("legacy notification TOML must parse");
        assert!(config.episode_mode);
        assert_eq!(config.digest_window_secs, 900);
        assert!(
            config.boot_bubble,
            "legacy TOML defaults the boot bubble ON"
        );
    }

    // --- Wave-5 §K-L6/L7/L8 (PR #504c) ratchets -----------------------

    #[test]
    fn test_engine_timeframes_default_is_9_entries_per_pr517() {
        let cfg = TimeframesConfig::default();
        assert_eq!(
            cfg.list.len(),
            9,
            "PR #517 pins 9 timeframes (was 21 in L6) — drift in the runtime list \
             would silently desync from `metrics_catalog::Tf::ALL`."
        );
    }

    #[test]
    fn test_engine_timeframes_default_excludes_seconds_per_l7() {
        let cfg = TimeframesConfig::default();
        assert!(
            !cfg.contains_seconds_tf(),
            "L7 retired all seconds-resolution timeframes \
             (1s/3s/5s/10s/15s/30s) — the default list MUST NOT \
             contain any. Got: {:?}",
            cfg.list,
        );
    }

    #[test]
    fn test_engine_timeframes_default_matches_canonical_set() {
        // Exact-match the PR #517 list so a future commit cannot
        // silently re-order or substitute a TF.
        let cfg = TimeframesConfig::default();
        let expected: Vec<&str> = vec!["1m", "5m", "15m", "30m", "1h", "2h", "3h", "4h", "1d"];
        assert_eq!(cfg.list, expected, "PR #517 timeframe list drifted");
    }

    #[test]
    fn test_engine_timeframes_contains_seconds_tf_helper_detects_30s() {
        let cfg = TimeframesConfig {
            list: vec!["30s".to_string(), "1m".to_string()],
        };
        assert!(
            cfg.contains_seconds_tf(),
            "helper must detect the seconds suffix on `30s`"
        );
    }

    #[test]
    fn test_engine_config_default_inherits_pr517_timeframes() {
        let engine = EngineConfig::default();
        assert_eq!(engine.timeframes.list.len(), 9);
        assert!(!engine.timeframes.contains_seconds_tf());
    }

    // --- Wave-5 §K-L10 (PR #504d) ratchets ---------------------------

    #[test]
    fn test_tick_storage_default_per_instrument_capacity_is_5k() {
        // L10 + sizing analysis pin: 5_000 covers the busiest contract.
        // Drift requires plan amend.
        let cfg = TickStorageConfig::default();
        assert_eq!(cfg.per_instrument_capacity, 5_000);
        assert_eq!(TickStorageConfig::default_per_instrument_capacity(), 5_000);
    }

    #[test]
    fn test_in_mem_config_default_inherits_l10_tick_storage() {
        let cfg = InMemConfig::default();
        assert_eq!(cfg.tick_storage.per_instrument_capacity, 5_000);
    }

    #[test]
    fn test_observability_config_default() {
        let config = ObservabilityConfig::default();
        assert_eq!(config.metrics_port, 9091);
        assert!(config.metrics_enabled);
        assert!(config.tracing_enabled);
        assert!(config.otlp_endpoint.contains("4317"));
    }

    #[test]
    fn test_default_metrics_bind_addr_is_loopback() {
        // L123 (Wave-5 plan §AA, SEC-H1): the helper used by
        // `#[serde(default)]` MUST return a loopback address so that
        // configs that omit `metrics_bind_addr` do not silently
        // expose `/metrics` on `0.0.0.0`.
        let addr = ObservabilityConfig::default_metrics_bind_addr();
        assert!(
            addr.is_loopback(),
            "L123: default metrics bind must be loopback, got {addr}"
        );
        assert_eq!(addr, std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn test_observability_config_default_metrics_bind_addr_is_loopback() {
        // Mirror the field-level default in the full struct's
        // `Default` impl so future changes to `default()` cannot drift
        // away from the L123 contract.
        let config = ObservabilityConfig::default();
        assert!(
            config.metrics_bind_addr.is_loopback(),
            "Default ObservabilityConfig must bind metrics to loopback (L123)"
        );
    }

    #[test]
    fn test_historical_data_config_default() {
        let config = HistoricalDataConfig::default();
        assert!(config.enabled);
        assert_eq!(config.lookback_days, 90);
        assert_eq!(config.request_timeout_secs, 30);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.request_delay_ms, 500);
        assert_eq!(config.marker_path, "data/state/historical_fetch_done.json");
    }

    #[test]
    fn test_historical_data_config_default_marker_path_is_exact() {
        // Stable contract — operators / runbooks reference this exact path.
        assert_eq!(
            super::default_historical_marker_path(),
            "data/state/historical_fetch_done.json"
        );
    }

    #[test]
    fn test_index_constituency_config_default() {
        let config = IndexConstituencyConfig::default();
        assert!(config.enabled);
        assert_eq!(config.download_timeout_secs, 30);
        assert_eq!(config.max_concurrent_downloads, 5);
        assert_eq!(config.inter_batch_delay_ms, 200);
    }

    #[test]
    fn test_greeks_config_default() {
        let config = GreeksConfig::default();
        assert!(config.enabled);
        assert_eq!(config.fetch_interval_secs, 60);
        assert!((config.risk_free_rate - 0.10).abs() < f64::EPSILON);
        assert!((config.dividend_yield - 0.0).abs() < f64::EPSILON);
        assert_eq!(config.iv_solver_max_iterations, 50);
        assert!((config.iv_solver_tolerance - 1e-8).abs() < f64::EPSILON);
        assert!((config.day_count - 365.0).abs() < f64::EPSILON);
        assert_eq!(config.rate_mode, "dhan");
    }

    #[test]
    fn test_default_allowed_origins() {
        let origins = default_allowed_origins();
        assert_eq!(origins.len(), 2);
        assert!(origins.contains(&"http://localhost:3000".to_string()));
        assert!(origins.contains(&"http://localhost:3001".to_string()));
    }

    #[test]
    fn test_invalid_market_close_time_fails() {
        let mut config = make_valid_config();
        config.trading.market_close_time = "bad".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("market_close_time"));
    }

    #[test]
    fn test_invalid_order_cutoff_time_fails() {
        let mut config = make_valid_config();
        config.trading.order_cutoff_time = "bad".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("order_cutoff_time"));
    }

    #[test]
    fn test_invalid_data_collection_start_fails() {
        let mut config = make_valid_config();
        config.trading.data_collection_start = "bad".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("data_collection_start"));
    }

    #[test]
    fn test_invalid_data_collection_end_fails() {
        let mut config = make_valid_config();
        config.trading.data_collection_end = "bad".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("data_collection_end"));
    }

    #[test]
    fn test_invalid_build_window_end_format_fails() {
        let mut config = make_valid_config();
        config.instrument.build_window_end = "bad".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("build_window_end"));
    }

    #[test]
    fn test_invalid_holiday_date_fails_validation() {
        // Exercises the TradingCalendar::from_config error propagation (line 638)
        let mut config = make_valid_config();
        config.trading.nse_holidays = vec![NseHolidayEntry {
            date: "not-a-date".to_string(),
            name: "Bad Holiday".to_string(),
        }];
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("not-a-date"),
            "error should mention the bad date: {}",
            err
        );
    }

    // -----------------------------------------------------------------------
    // D1: Sandbox Guard Tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_sandbox_guard_blocks_live_before_earliest_date() {
        // Renamed 2026-07-14 (was `..._before_july` — era-named; the earliest
        // date is now the 2099-12-31 sentinel). Logic is constant-driven and
        // self-adapts to whichever era "today" is in.
        // Date-robust (2026-07-02 coverage-gate fix): the previous version only
        // called validate() with Live INSIDE `if today < earliest`, so from
        // 2026-07-01 the entire D1 live-mode guard block silently fell out of
        // coverage (the `common` floor regression on main). Now validate() runs
        // through the guard UNCONDITIONALLY and the correct branch is asserted
        // for whichever era "today" is in — the guard stays covered forever.
        let mut config = make_valid_config();
        config.strategy.mode = TradingMode::Live;
        let today = (chrono::Utc::now()
            + chrono::TimeDelta::seconds(crate::constants::IST_UTC_OFFSET_SECONDS_I64))
        .date_naive();
        let earliest = chrono::NaiveDate::from_ymd_opt(
            crate::constants::LIVE_TRADING_EARLIEST_YEAR,
            crate::constants::LIVE_TRADING_EARLIEST_MONTH,
            crate::constants::LIVE_TRADING_EARLIEST_DAY,
        )
        .unwrap();
        let result = config.validate();
        if today < earliest {
            let err = result.unwrap_err();
            assert!(
                err.to_string().contains("SANDBOX GUARD"),
                "should mention SANDBOX GUARD: {}",
                err
            );
        } else {
            // On/after the earliest date the D1 date gate passes and validation
            // proceeds through the URL checks.
            result.expect("live mode on/after LIVE_TRADING_EARLIEST_DATE must validate");
        }
    }

    #[test]
    fn test_sandbox_and_paper_modes_always_pass_guard() {
        let mut config = make_valid_config();
        config.strategy.mode = TradingMode::Sandbox;
        assert!(config.validate().is_ok(), "sandbox mode should always pass");

        config.strategy.mode = TradingMode::Paper;
        assert!(config.validate().is_ok(), "paper mode should always pass");
    }

    #[test]
    fn test_base_config_dry_run_is_true() {
        let config = make_valid_config();
        assert!(
            config.strategy.dry_run,
            "default config must have dry_run = true for safety"
        );
    }

    #[test]
    fn test_base_config_mode_is_not_live() {
        let config = make_valid_config();
        assert!(
            !config.strategy.mode.is_live(),
            "default config must NOT be in live mode"
        );
    }

    // -------------------------------------------------------------------
    // GAP 28: Depth config validation tests
    // -------------------------------------------------------------------

    #[test]
    fn test_default_config_trading_mode_is_paper_not_live() {
        let config = make_valid_config();
        assert!(config.strategy.mode.is_paper());
        assert!(!config.strategy.mode.is_live());
        assert!(!config.strategy.mode.is_sandbox());
        assert!(!config.strategy.mode.is_http_active());
    }

    #[test]
    fn test_build_ilp_conf_string_tcp_only() {
        let config = QuestDbConfig {
            host: "tv-questdb".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        let conf = config.build_ilp_conf_string();
        assert_eq!(conf, "tcp::addr=tv-questdb:9009;");
        // TCP mode does NOT support retry_timeout, init_buf_size, request_timeout
        // (those are HTTP-only in questdb-rs 6.1.0)
        assert!(!conf.contains("retry_timeout"));
        assert!(!conf.contains("init_buf_size"));
        assert!(!conf.contains("request_timeout"));
    }

    #[test]
    fn test_build_ilp_conf_string_custom_port() {
        let config = QuestDbConfig {
            host: "10.0.1.5".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 19009,
        };
        let conf = config.build_ilp_conf_string();
        assert!(conf.contains("tcp::addr=10.0.1.5:19009;"));
    }

    #[test]
    fn test_default_auto_start_is_true() {
        // Covers `default_auto_start` — referenced via `#[serde(default = ...)]`.
        // Boot sequence relies on Docker auto-start being on by default.
        assert!(super::default_auto_start());
    }

    // `test_default_valkey_password_is_empty_string` DELETED in #O4
    // (2026-05-24) along with the `default_valkey_password` helper.

    #[test]
    fn test_validate_url_empty_rest_api_base_url_rejected() {
        // Covers the `bail!("{name} must not be empty")` branch inside
        // the validate_url closure (config.rs:~998).
        let mut config = make_valid_config();
        config.dhan.rest_api_base_url.clear();
        let err = config.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must not be empty"),
            "expected empty-url rejection, got: {msg}"
        );
        assert!(
            msg.contains("dhan.rest_api_base_url"),
            "error must name the offending field: {msg}"
        );
    }

    #[test]
    fn test_validate_url_wrong_scheme_rejected() {
        // Covers the `bail!("{name} must start with ...")` branch
        // inside the validate_url closure (config.rs:~1001).
        let mut config = make_valid_config();
        config.dhan.rest_api_base_url = "http://api.dhan.co/v2/".to_string();
        let err = config.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("must start with 'https://'"),
            "expected scheme rejection, got: {msg}"
        );
    }

    // PR #2 (2026-05-18): MoversConfig defaults guard tests retired
    // alongside the deleted MoversConfig struct and movers pipeline.

    // -----------------------------------------------------------------------
    // PR #4 (2026-05-19) — DepthDynamicConfig tests retired alongside the
    // deletion of `Depth20RootConfig`, `Depth200RootConfig`, and the
    // depth-20 / depth-200 dynamic pipelines. Operator-locked: only the
    // 1 main-feed + 1 order-update WS remain.
    // -----------------------------------------------------------------------

    #[test]
    fn test_pr4_depth_config_structs_removed_marker() {
        // Placeholder ratchet so the test module is non-empty. The real
        // guard is the absence of `Depth20RootConfig` / `Depth200RootConfig`
        // / `DepthDynamicConfig` from the source — if anyone reintroduces
        // them, `cargo check` fails because no callers exist.
    }

    // `test_default_movers_v2_enabled_is_false_for_safety` removed
    // 2026-07-06 with the dead `movers_v2_enabled` knob (the gated
    // route was deleted in AWS-lifecycle PR #2, 2026-05-18).

    // =======================================================================
    // Groww second-feed scope (operator lock 2026-06-19) — `[feeds]` toggle.
    // See `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`.
    // Covers the toggle-permutation rows of the coverage matrix (Section G):
    // dhan ON/groww OFF (default), groww-only, both, both-off.
    // =======================================================================

    /// RATCHET: the default MUST keep Dhan ON and Groww OFF so a fresh
    /// deployment (or a missing `[feeds]` block) behaves byte-identically
    /// to today's Dhan-only system. Flipping either default requires a
    /// dated operator quote per the scope rule file.
    #[test]
    fn test_feeds_config_default_dhan_on_groww_off() {
        let feeds = FeedsConfig::default();
        assert!(
            feeds.dhan_enabled,
            "Dhan must default ON (system unchanged)"
        );
        assert!(
            !feeds.groww_enabled,
            "Groww must default OFF (opt-in; zero prod behaviour change)"
        );
    }

    /// Dual-feed scoreboard PR-A (2026-07-10): the `[scoreboard]` section
    /// defaults SAFE-ON (aggregation-only — reads existing tables, no hot
    /// path), trigger at 15:45:00 IST, and a missing section must default,
    /// never error.
    #[test]
    fn test_scoreboard_config_defaults_enabled_safe_on() {
        let sb = ScoreboardConfig::default();
        assert!(sb.enabled, "scoreboard must default ON (aggregation-only)");
        assert!(sb.telegram_enabled);
        assert!(sb.coverage_detail_rows);
        assert!(sb.presence_fold_enabled);
        assert!(sb.groww_lag_enabled);
        assert_eq!(
            sb.trigger_secs_of_day_ist, 56_700,
            "trigger must default to 15:45:00 IST"
        );
    }

    /// B12 rollback test (`scoreboard_flag_rollback`): flipping
    /// `[scoreboard] enabled = false` must round-trip through TOML — the
    /// tested off-switch path that disables the whole subsystem — and an
    /// EMPTY `[scoreboard]` section must fill every field from defaults.
    #[test]
    fn scoreboard_flag_rollback() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            scoreboard: ScoreboardConfig,
        }
        // Rollback shape: enabled=false parses and turns the subsystem off.
        let off: Wrapper = Figment::new()
            .merge(Toml::string("[scoreboard]\nenabled = false\n"))
            .extract()
            .expect("scoreboard rollback TOML must parse");
        assert!(!off.scoreboard.enabled, "rollback flag must stick");
        // Partial section: unspecified keys fill from serde defaults.
        assert!(off.scoreboard.telegram_enabled);
        assert_eq!(off.scoreboard.trigger_secs_of_day_ist, 56_700);
        // Missing section entirely → full defaults, never an error.
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[feeds]\ndhan_enabled = true\n"))
            .extract()
            .expect("missing [scoreboard] must default");
        assert!(missing.scoreboard.enabled);
    }

    /// BruteX crossverify (2026-07-12): the `[brutex_crossverify]` section
    /// defaults SAFE-OFF (a NEW subsystem must not activate on deploy day
    /// without an explicit config flip), trigger at 15:50:00 IST, S3
    /// wall-clock deadline at 16:05:00 IST, and a missing section must
    /// default, never error.
    #[test]
    fn test_brutex_crossverify_config_defaults_disabled_safe_off() {
        let bx = BrutexCrossverifyConfig::default();
        assert!(!bx.enabled, "brutex_crossverify must default OFF");
        assert!(bx.telegram_enabled);
        assert_eq!(
            bx.trigger_secs_of_day_ist, 57_000,
            "trigger must default to 15:50:00 IST"
        );
        assert_eq!(
            bx.deadline_secs_of_day_ist, 57_900,
            "S3 wall-clock deadline must default to 16:05:00 IST"
        );
        assert!(
            bx.trigger_secs_of_day_ist < bx.deadline_secs_of_day_ist,
            "trigger must precede the deadline"
        );
        assert_eq!(bx.bucket, "tv-prod-cold");
        assert_eq!(bx.prefix, "crossverify/groww");
        assert_eq!(bx.repoll_interval_secs, 120);
        assert_eq!(bx.max_object_bytes, 5 * 1024 * 1024);
        assert_eq!(bx.max_keys, 2_000);
        assert_eq!(bx.fetch_attempts_per_object, 3);
        assert_eq!(bx.price_tolerance_paise, 0, "exact match by default");
        assert!(!bx.compare_volume, "volume classification defaults OFF");
    }

    /// B12 rollback test (`brutex_crossverify_flag_rollback`): flipping
    /// `[brutex_crossverify] enabled = true` must round-trip through TOML
    /// (the tested ON-switch path — the subsystem is default-OFF, so the
    /// rollback IS the default), a partial section must fill every other
    /// field from serde defaults, and a MISSING section must default to
    /// the safe-off state, never error.
    #[test]
    fn brutex_crossverify_flag_rollback() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            brutex_crossverify: BrutexCrossverifyConfig,
        }
        // ON shape: enabled=true parses and turns the subsystem on.
        let on: Wrapper = Figment::new()
            .merge(Toml::string("[brutex_crossverify]\nenabled = true\n"))
            .extract()
            .expect("brutex_crossverify enable TOML must parse");
        assert!(on.brutex_crossverify.enabled, "enable flag must stick");
        // Partial section: unspecified keys fill from serde defaults.
        assert!(on.brutex_crossverify.telegram_enabled);
        assert_eq!(on.brutex_crossverify.trigger_secs_of_day_ist, 57_000);
        assert_eq!(on.brutex_crossverify.deadline_secs_of_day_ist, 57_900);
        assert_eq!(on.brutex_crossverify.bucket, "tv-prod-cold");
        // Missing section entirely → full defaults (OFF), never an error.
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[feeds]\ndhan_enabled = true\n"))
            .extract()
            .expect("missing [brutex_crossverify] must default");
        assert!(!missing.brutex_crossverify.enabled);
    }

    /// Per-minute spot 1m REST pipeline (operator grant 2026-07-12): the
    /// `[spot_1m_rest]` section is FAIL-SAFE default OFF — via `Default`,
    /// via a missing section, and via an empty section — and an explicit
    /// `enabled = true` (the `config/base.toml` shape) must round-trip.
    #[test]
    fn test_oms_reconcile_config_default_is_disabled() {
        let d = OmsReconcileConfig::default();
        assert!(
            !d.enabled,
            "oms_reconcile must default OFF (fail-safe; base.toml keeps it off too)"
        );
        assert_eq!(d.interval_secs, 300, "default cadence must be 5 minutes");
        assert!(
            d.trading_hours_only,
            "default must gate to NSE trading hours"
        );
    }

    #[test]
    fn test_oms_reconcile_config_serde_defaults_and_round_trip() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            oms_reconcile: OmsReconcileConfig,
        }
        // Missing section entirely → disabled with full defaults, never an error.
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [oms_reconcile] must default, not error");
        assert!(!missing.oms_reconcile.enabled);
        assert_eq!(missing.oms_reconcile.interval_secs, 300);
        assert!(missing.oms_reconcile.trading_hours_only);
        // Empty section (no keys) → disabled via the field-level defaults.
        let empty: Wrapper = Figment::new()
            .merge(Toml::string("[oms_reconcile]\n"))
            .extract()
            .expect("empty [oms_reconcile] must default, not error");
        assert!(!empty.oms_reconcile.enabled);
        assert_eq!(empty.oms_reconcile.interval_secs, 300);
        // Explicit values round-trip.
        let on: Wrapper = Figment::new()
            .merge(Toml::string(
                "[oms_reconcile]\nenabled = true\ninterval_secs = 120\ntrading_hours_only = false\n",
            ))
            .extract()
            .expect("explicit [oms_reconcile] values must round-trip");
        assert!(on.oms_reconcile.enabled);
        assert_eq!(on.oms_reconcile.interval_secs, 120);
        assert!(!on.oms_reconcile.trading_hours_only);
    }

    #[test]
    fn test_oms_reconcile_interval_validation_bounds() {
        let mut cfg = OmsReconcileConfig {
            interval_secs: 59,
            ..OmsReconcileConfig::default()
        };
        assert!(cfg.validate().is_err(), "59s is below the 60s floor");
        cfg.interval_secs = 60;
        assert!(cfg.validate().is_ok(), "60s floor is inclusive");
        cfg.interval_secs = 3600;
        assert!(cfg.validate().is_ok(), "3600s ceiling is inclusive");
        cfg.interval_secs = 3601;
        assert!(cfg.validate().is_err(), "3601s is above the ceiling");
    }

    #[test]
    fn test_application_config_validate_rejects_bad_reconcile_interval() {
        let mut config = make_valid_config();
        config.oms_reconcile.interval_secs = 59;
        let err = config
            .validate()
            .expect_err("59s reconcile interval must fail whole-config validation");
        assert!(
            err.to_string().contains("oms_reconcile.interval_secs"),
            "error must name the offending key, got: {err}"
        );
        config.oms_reconcile.interval_secs = 300;
        config
            .validate()
            .expect("default reconcile interval must pass whole-config validation");
    }

    #[test]
    fn test_spot_1m_rest_config_defaults_off_and_round_trips() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        assert!(
            !Spot1mRestConfig::default().enabled,
            "spot_1m_rest must default OFF (fail-safe; base.toml opts in)"
        );

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            spot_1m_rest: Spot1mRestConfig,
        }
        // Missing section entirely → disabled, never an error.
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [spot_1m_rest] must default, not error");
        assert!(!missing.spot_1m_rest.enabled);
        // Empty section (no keys) → disabled via the field-level default.
        let empty: Wrapper = Figment::new()
            .merge(Toml::string("[spot_1m_rest]\n"))
            .extract()
            .expect("empty [spot_1m_rest] must default, not error");
        assert!(!empty.spot_1m_rest.enabled);
        // Explicit ON (the base.toml shape) round-trips.
        let on: Wrapper = Figment::new()
            .merge(Toml::string("[spot_1m_rest]\nenabled = true\n"))
            .extract()
            .expect("explicit enabled = true must round-trip");
        assert!(on.spot_1m_rest.enabled);
    }

    /// 2026-07-14 serving-delay diagnostics rider: `diagnostics` is
    /// FAIL-SAFE default OFF (via `Default`, a missing section, and an
    /// empty section), the second-probe instant defaults to 11:00 IST on
    /// BOTH paths (manual `Default` == serde default — no derive drift),
    /// and the base.toml opt-in shape round-trips.
    #[test]
    fn test_spot_1m_rest_diagnostics_defaults_off_with_1100_second_probe() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        let d = Spot1mRestConfig::default();
        assert!(
            !d.diagnostics,
            "diagnostics must default OFF (log-only opt-in)"
        );
        assert_eq!(
            d.diagnostics_second_probe_secs_of_day_ist,
            11 * 3600,
            "second probe defaults to 11:00 IST"
        );

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            spot_1m_rest: Spot1mRestConfig,
        }
        // Pre-rider TOML (enabled only) → diagnostics off, probe at 11:00.
        let old: Wrapper = Figment::new()
            .merge(Toml::string("[spot_1m_rest]\nenabled = true\n"))
            .extract()
            .expect("pre-rider TOML must keep deserializing");
        assert!(!old.spot_1m_rest.diagnostics);
        assert_eq!(
            old.spot_1m_rest.diagnostics_second_probe_secs_of_day_ist,
            11 * 3600
        );
        // The base.toml opt-in shape + an explicit probe override round-trip.
        let on: Wrapper = Figment::new()
            .merge(Toml::string(
                "[spot_1m_rest]\nenabled = true\ndiagnostics = true\n\
                 diagnostics_second_probe_secs_of_day_ist = 43200\n",
            ))
            .extract()
            .expect("diagnostics opt-in must round-trip");
        assert!(on.spot_1m_rest.diagnostics);
        assert_eq!(
            on.spot_1m_rest.diagnostics_second_probe_secs_of_day_ist,
            12 * 3600
        );
    }

    /// 2026-07-14 operator pacing directive: `[dhan_data_api] target_rps`
    /// defaults to the directed 3 (via `Default`, a missing section, and
    /// an empty section), explicit values round-trip, and `validate()`
    /// rejects anything outside the 2..=4 ladder.
    #[test]
    fn test_dhan_data_api_config_default_is_3_and_round_trips() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        assert_eq!(
            DhanDataApiConfig::default().target_rps,
            3,
            "target_rps must default to the operator-directed 3"
        );

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            dhan_data_api: DhanDataApiConfig,
        }
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [dhan_data_api] must default, not error");
        assert_eq!(missing.dhan_data_api.target_rps, 3);
        let empty: Wrapper = Figment::new()
            .merge(Toml::string("[dhan_data_api]\n"))
            .extract()
            .expect("empty [dhan_data_api] must default, not error");
        assert_eq!(empty.dhan_data_api.target_rps, 3);
        let two: Wrapper = Figment::new()
            .merge(Toml::string("[dhan_data_api]\ntarget_rps = 2\n"))
            .extract()
            .expect("explicit target_rps must round-trip");
        assert_eq!(two.dhan_data_api.target_rps, 2);
    }

    /// The 2..=4 pacing ladder is REJECTED-at-boot enforced: 0/1/5 fail,
    /// every in-range value passes.
    #[test]
    fn test_dhan_data_api_config_validate_rejects_out_of_range() {
        for bad in [0_u32, 1, 5, 100] {
            let cfg = DhanDataApiConfig { target_rps: bad };
            assert!(
                cfg.validate().is_err(),
                "target_rps {bad} must be rejected (legal range 2..=4)"
            );
        }
        for good in [2_u32, 3, 4] {
            let cfg = DhanDataApiConfig { target_rps: good };
            assert!(cfg.validate().is_ok(), "target_rps {good} must pass");
        }
    }

    /// 🔷 DHAN margin gate (2026-07-14): `[dhan_margin_gate]` is FAIL-SAFE
    /// default OFF on every path — `Default`, a missing section, and an
    /// empty section — with the shared-account defaults (50% tenant budget,
    /// 5 req/sec self-cap — the funds/margin rate bucket is Assumed
    /// Non-Trading, unnamed by Dhan's docs) intact on all of them.
    #[test]
    fn test_dhan_margin_gate_config_default_is_disabled() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        let d = DhanMarginGateConfig::default();
        assert!(!d.enabled, "margin gate must default OFF (fail-safe)");
        assert_eq!(d.tenant_budget_percent, 50);
        assert_eq!(d.rest_self_cap_per_sec, 5);

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            dhan_margin_gate: DhanMarginGateConfig,
        }
        // Missing section entirely → disabled, never an error.
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [dhan_margin_gate] must default, not error");
        assert!(!missing.dhan_margin_gate.enabled);
        assert_eq!(missing.dhan_margin_gate.tenant_budget_percent, 50);
        assert_eq!(missing.dhan_margin_gate.rest_self_cap_per_sec, 5);
        // Empty section (no keys) → disabled via the field-level default.
        let empty: Wrapper = Figment::new()
            .merge(Toml::string("[dhan_margin_gate]\n"))
            .extract()
            .expect("empty [dhan_margin_gate] must default, not error");
        assert!(!empty.dhan_margin_gate.enabled);
        assert_eq!(empty.dhan_margin_gate.tenant_budget_percent, 50);
        assert_eq!(empty.dhan_margin_gate.rest_self_cap_per_sec, 5);
    }

    /// Shared-account tenant budget: 0% and >50% are REJECTED at boot
    /// (the Dhan account is pooled with the BruteX co-tenant — our entries
    /// may never claim more than half the pooled balance); the 1..=50
    /// boundaries pass.
    #[test]
    fn test_dhan_margin_gate_validate_rejects_over_50_percent() {
        for bad in [0_u8, 51, 100] {
            let cfg = DhanMarginGateConfig {
                tenant_budget_percent: bad,
                ..DhanMarginGateConfig::default()
            };
            assert!(
                cfg.validate().is_err(),
                "tenant_budget_percent {bad} must be rejected (legal range 1..=50)"
            );
        }
        for good in [1_u8, 50] {
            let cfg = DhanMarginGateConfig {
                tenant_budget_percent: good,
                ..DhanMarginGateConfig::default()
            };
            assert!(
                cfg.validate().is_ok(),
                "tenant_budget_percent {good} must pass"
            );
        }
    }

    /// Funds/margin REST self-cap: 0/1 (below the 2-call entry burst) and
    /// 11+ (over half of the ASSUMED Non-Trading 20/sec bucket — unnamed
    /// by Dhan's docs) are REJECTED; the 2..=10 boundaries pass.
    #[test]
    fn test_dhan_margin_gate_validate_rejects_rest_cap_out_of_range() {
        for bad in [0_u32, 1, 11, 100] {
            let cfg = DhanMarginGateConfig {
                rest_self_cap_per_sec: bad,
                ..DhanMarginGateConfig::default()
            };
            assert!(
                cfg.validate().is_err(),
                "rest_self_cap_per_sec {bad} must be rejected (legal range 2..=10)"
            );
        }
        for good in [2_u32, 10] {
            let cfg = DhanMarginGateConfig {
                rest_self_cap_per_sec: good,
                ..DhanMarginGateConfig::default()
            };
            assert!(
                cfg.validate().is_ok(),
                "rest_self_cap_per_sec {good} must pass"
            );
        }
    }

    /// 2026-07-14 fetch-mode flag: defaults to `per_minute` on every path
    /// (Default / missing key / pre-flag TOML), `batch_catchup` +
    /// `batch_interval_minutes` parse, and unknown mode strings are
    /// REJECTED (never silently coerced to a mode).
    #[test]
    fn test_spot1m_fetch_mode_defaults_per_minute_and_parses_batch() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        let d = Spot1mRestConfig::default();
        assert_eq!(d.fetch_mode, SpotFetchMode::PerMinute);
        assert_eq!(d.batch_interval_minutes, 5);

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            spot_1m_rest: Spot1mRestConfig,
        }
        // Pre-flag TOML (enabled only) → per_minute, interval 5.
        let old: Wrapper = Figment::new()
            .merge(Toml::string("[spot_1m_rest]\nenabled = true\n"))
            .extract()
            .expect("pre-flag TOML must keep deserializing");
        assert_eq!(old.spot_1m_rest.fetch_mode, SpotFetchMode::PerMinute);
        assert_eq!(old.spot_1m_rest.batch_interval_minutes, 5);
        // The batch shape round-trips.
        let batch: Wrapper = Figment::new()
            .merge(Toml::string(
                "[spot_1m_rest]\nenabled = true\nfetch_mode = \"batch_catchup\"\n\
                 batch_interval_minutes = 10\n",
            ))
            .extract()
            .expect("batch_catchup opt-in must round-trip");
        assert_eq!(batch.spot_1m_rest.fetch_mode, SpotFetchMode::BatchCatchup);
        assert_eq!(batch.spot_1m_rest.batch_interval_minutes, 10);
        // An unknown mode string is an ERROR, never a silent default.
        let junk: Result<Wrapper, _> = Figment::new()
            .merge(Toml::string(
                "[spot_1m_rest]\nfetch_mode = \"warp_speed\"\n",
            ))
            .extract();
        assert!(junk.is_err(), "unknown fetch_mode must be rejected");
    }

    /// Batch cadence bounds: 0 and 61+ are rejected; 1..=60 pass.
    #[test]
    fn test_spot1m_batch_interval_validate_bounds() {
        let mut cfg = Spot1mRestConfig::default();
        for bad in [0_u32, 61, 1_000] {
            cfg.batch_interval_minutes = bad;
            assert!(
                cfg.validate().is_err(),
                "batch_interval_minutes {bad} must be rejected (1..=60)"
            );
        }
        for good in [1_u32, 5, 60] {
            cfg.batch_interval_minutes = good;
            assert!(cfg.validate().is_ok(), "interval {good} must pass");
        }
    }

    /// Per-minute option-chain REST pipeline (operator grant 2026-07-12,
    /// PR-3): the `[option_chain_1m]` pipeline is DEFAULT-OFF (pending the
    /// live entitlement probe) with `probe_and_report` DEFAULT-ON — via
    /// `Default`, via a missing section, and via an empty section — and
    /// explicit values round-trip.
    #[test]
    fn test_option_chain_1m_default_off_config_gate() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        let d = OptionChain1mConfig::default();
        assert!(!d.enabled, "option_chain_1m must default OFF (entitlement)");
        assert!(d.probe_and_report, "probe_and_report must default ON");

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            option_chain_1m: OptionChain1mConfig,
        }
        // Missing section entirely → pipeline off, probe on, never an error.
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [option_chain_1m] must default, not error");
        assert!(!missing.option_chain_1m.enabled);
        assert!(missing.option_chain_1m.probe_and_report);
        // Empty section (no keys) → same via the field-level defaults.
        let empty: Wrapper = Figment::new()
            .merge(Toml::string("[option_chain_1m]\n"))
            .extract()
            .expect("empty [option_chain_1m] must default, not error");
        assert!(!empty.option_chain_1m.enabled);
        assert!(empty.option_chain_1m.probe_and_report);
        // Explicit values round-trip (the future opt-in shape).
        let on: Wrapper = Figment::new()
            .merge(Toml::string(
                "[option_chain_1m]\nenabled = true\nprobe_and_report = false\n",
            ))
            .extract()
            .expect("explicit values must round-trip");
        assert!(on.option_chain_1m.enabled);
        assert!(!on.option_chain_1m.probe_and_report);
    }

    /// Groww per-minute spot 1m REST leg (operator grant 2026-07-13,
    /// PR-2): the `[groww_spot_1m]` section is FAIL-SAFE default OFF —
    /// via `Default`, via a missing section, and via an empty section —
    /// and an explicit `enabled = true` (the base.toml shape) round-trips.
    #[test]
    fn test_groww_spot_1m_config_defaults_off_and_round_trips() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        assert!(
            !GrowwSpot1mConfig::default().enabled,
            "groww_spot_1m must default OFF (fail-safe; base.toml opts in)"
        );

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            groww_spot_1m: GrowwSpot1mConfig,
        }
        // Missing section entirely → disabled, never an error.
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [groww_spot_1m] must default, not error");
        assert!(!missing.groww_spot_1m.enabled);
        // Empty section (no keys) → disabled via the field-level default.
        let empty: Wrapper = Figment::new()
            .merge(Toml::string("[groww_spot_1m]\n"))
            .extract()
            .expect("empty [groww_spot_1m] must default, not error");
        assert!(!empty.groww_spot_1m.enabled);
        // Explicit ON (the base.toml shape) round-trips.
        let on: Wrapper = Figment::new()
            .merge(Toml::string("[groww_spot_1m]\nenabled = true\n"))
            .extract()
            .expect("explicit enabled = true must round-trip");
        assert!(on.groww_spot_1m.enabled);
    }

    /// Config-gate contract (Groww per-minute REST plan PR-3): the
    /// `[groww_option_chain_1m]` section is DEFAULT-OFF with
    /// probe-and-report ON — mirrors the Dhan `[option_chain_1m]` gate;
    /// an absent/empty section (or an older TOML) never errors.
    #[test]
    fn test_groww_option_chain_1m_config_defaults_off_probe_on_and_round_trips() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        let d = GrowwOptionChain1mConfig::default();
        assert!(
            !d.enabled,
            "groww_option_chain_1m must default OFF (pending the first live probe)"
        );
        assert!(
            d.probe_and_report,
            "probe_and_report must default ON (the operator learns the live verdict)"
        );

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            groww_option_chain_1m: GrowwOptionChain1mConfig,
        }
        // Missing section entirely → disabled + probe ON, never an error.
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [groww_option_chain_1m] must default, not error");
        assert!(!missing.groww_option_chain_1m.enabled);
        assert!(missing.groww_option_chain_1m.probe_and_report);
        // Empty section (no keys) → field-level defaults.
        let empty: Wrapper = Figment::new()
            .merge(Toml::string("[groww_option_chain_1m]\n"))
            .extract()
            .expect("empty [groww_option_chain_1m] must default, not error");
        assert!(!empty.groww_option_chain_1m.enabled);
        assert!(empty.groww_option_chain_1m.probe_and_report);
        // Explicit ON (the future flip shape) round-trips; probe can be
        // explicitly silenced.
        let on: Wrapper = Figment::new()
            .merge(Toml::string(
                "[groww_option_chain_1m]\nenabled = true\nprobe_and_report = false\n",
            ))
            .extract()
            .expect("explicit values must round-trip");
        assert!(on.groww_option_chain_1m.enabled);
        assert!(!on.groww_option_chain_1m.probe_and_report);
    }

    /// 2026-07-14 Groww REST burst auto-ladder: the `[groww_rest_burst]`
    /// section is fail-safe — absent/empty sections default to the
    /// rate-safe `two_wave` tier with warm-up OFF; explicit values (incl.
    /// the probe-gated `seven_concurrent` flip shape) round-trip; an
    /// unknown tier string is a loud config error, never a silent default.
    #[test]
    fn test_groww_rest_burst_config_defaults_and_round_trips() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        let d = GrowwRestBurstConfig::default();
        assert_eq!(d.tier, GrowwRestBurstTier::TwoWave);
        assert!(!d.warm_up, "warm-up must default OFF (base.toml opts in)");
        assert_eq!(GrowwRestBurstTier::TwoWave.as_str(), "two_wave");
        assert_eq!(
            GrowwRestBurstTier::SevenConcurrent.as_str(),
            "seven_concurrent"
        );

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            groww_rest_burst: GrowwRestBurstConfig,
        }
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [groww_rest_burst] must default, not error");
        assert_eq!(missing.groww_rest_burst.tier, GrowwRestBurstTier::TwoWave);
        assert!(!missing.groww_rest_burst.warm_up);
        let empty: Wrapper = Figment::new()
            .merge(Toml::string("[groww_rest_burst]\n"))
            .extract()
            .expect("empty [groww_rest_burst] must default, not error");
        assert_eq!(empty.groww_rest_burst.tier, GrowwRestBurstTier::TwoWave);
        assert!(!empty.groww_rest_burst.warm_up);
        let base_shape: Wrapper = Figment::new()
            .merge(Toml::string(
                "[groww_rest_burst]\ntier = \"two_wave\"\nwarm_up = true\n",
            ))
            .extract()
            .expect("the base.toml shape must round-trip");
        assert_eq!(
            base_shape.groww_rest_burst.tier,
            GrowwRestBurstTier::TwoWave
        );
        assert!(base_shape.groww_rest_burst.warm_up);
        let seven: Wrapper = Figment::new()
            .merge(Toml::string(
                "[groww_rest_burst]\ntier = \"seven_concurrent\"\n",
            ))
            .extract()
            .expect("the probe-gated promotion shape must round-trip");
        assert_eq!(
            seven.groww_rest_burst.tier,
            GrowwRestBurstTier::SevenConcurrent
        );
        // A typo'd tier is refused loudly — never silently two_wave.
        let bad: Result<Wrapper, _> = Figment::new()
            .merge(Toml::string("[groww_rest_burst]\ntier = \"seven\"\n"))
            .extract();
        assert!(
            bad.is_err(),
            "an unknown tier string must be a config error"
        );
    }

    /// Daily timeframe-consistency verifier (operator 2026-07-13): the
    /// `[tf_consistency]` section is fail-safe DEFAULT-OFF — via `Default`,
    /// via a missing section, and via an empty section — and the explicit
    /// base.toml opt-in round-trips.
    #[test]
    fn test_tf_consistency_config_default_off_and_round_trip() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        assert!(
            !TfConsistencyConfig::default().enabled,
            "tf_consistency must default OFF (fail-safe; base.toml opts in)"
        );

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            tf_consistency: TfConsistencyConfig,
        }
        // Missing section entirely → disabled, never an error.
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [tf_consistency] must default, not error");
        assert!(!missing.tf_consistency.enabled);
        // Empty section (no keys) → disabled via the field-level default.
        let empty: Wrapper = Figment::new()
            .merge(Toml::string("[tf_consistency]\n"))
            .extract()
            .expect("empty [tf_consistency] must default, not error");
        assert!(!empty.tf_consistency.enabled);
        // Explicit ON (the base.toml shape) round-trips.
        let on: Wrapper = Figment::new()
            .merge(Toml::string("[tf_consistency]\nenabled = true\n"))
            .extract()
            .expect("explicit enabled = true must round-trip");
        assert!(on.tf_consistency.enabled);
    }

    /// Cadence scheduler (operator 2026-07-14): the `[cadence]` section is
    /// fail-safe DEFAULT-OFF (the design's ratchet
    /// `!CadenceConfig::default().enabled`) — via `Default`, via a missing
    /// section, and via an empty section — and every field of the empty
    /// section deserializes to the judge-locked cadence table.
    #[test]
    fn test_cadence_config_default_off() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        let d = CadenceConfig::default();
        assert!(
            !d.enabled,
            "cadence must default OFF (fail-safe; the operator flips it)"
        );
        // The locked cadence table (2026-07-16 post-close shape).
        assert_eq!(d.dhan_burst_offset_ms, 1_000);
        assert_eq!(d.spot_window_cap, 4);
        assert_eq!(d.concurrency_degrade_after_dirty_cycles, 2);
        assert_eq!(d.concurrency_recover_after_clean_cycles, 3);
        assert_eq!(d.spot_min_post_close_ms, 300);
        assert_eq!(d.in_cycle_retry_max, 1);
        assert_eq!(d.dhan_lane_cutoff_ms, 15_000);
        assert_eq!(d.groww_anchor_offset_ms, 0);
        assert_eq!(d.groww_burst_timeout_ms, 800);
        assert_eq!(d.groww_request_timeout_ms, 1_500);
        assert_eq!(d.groww_lane_cutoff_ms, 6_000);
        assert_eq!(d.chain_min_spacing_ms, 3_000);
        // The 2026-07-15 pre-market expiry-resolution knobs.
        assert_eq!(d.expiry_retry_interval_ms, 60_000);
        assert_eq!(d.expiry_deadline_secs_of_day_ist, 32_100); // 08:55 IST
        assert!(d.validate().is_ok(), "the shipped defaults must validate");

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            cadence: CadenceConfig,
        }
        // Missing section entirely → disabled, never an error.
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [cadence] must default, not error");
        assert!(!missing.cadence.enabled);
        // Empty section (no keys) → disabled + the full locked table via
        // the field-level serde defaults (must equal `Default`).
        let empty: Wrapper = Figment::new()
            .merge(Toml::string("[cadence]\n"))
            .extract()
            .expect("empty [cadence] must default, not error");
        assert!(!empty.cadence.enabled);
        assert_eq!(empty.cadence.dhan_burst_offset_ms, d.dhan_burst_offset_ms);
        assert_eq!(empty.cadence.spot_window_cap, d.spot_window_cap);
        assert_eq!(
            empty.cadence.concurrency_degrade_after_dirty_cycles,
            d.concurrency_degrade_after_dirty_cycles
        );
        assert_eq!(empty.cadence.groww_lane_cutoff_ms, d.groww_lane_cutoff_ms);
        // Explicit ON round-trips.
        let on: Wrapper = Figment::new()
            .merge(Toml::string("[cadence]\nenabled = true\n"))
            .extract()
            .expect("explicit enabled = true must round-trip");
        assert!(on.cadence.enabled);
    }

    /// Cadence validate(): the spot ROLLING-1000ms-WINDOW cap is bounded
    /// 1..=5 (the Dhan Data-API hard cap is 5/sec) and the shared
    /// concurrency-ladder streak thresholds must be >= 1 — the structural
    /// zero-429 spot floor (operator spot-concurrency ladder 2026-07-15).
    #[test]
    fn test_cadence_config_validate_rejects_bad_spot_window_cap() {
        let mut cfg = CadenceConfig {
            spot_window_cap: 0,
            ..CadenceConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("spot_window_cap"),
            "unexpected error: {err}"
        );
        cfg.spot_window_cap = 6;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("spot_window_cap"),
            "unexpected error: {err}"
        );
        // The whole legal range 1..=5 validates (a cap below 4 floors the
        // concurrency ladder's step in `cadence::ladder` — never rejected
        // here).
        for cap in 1..=5u32 {
            cfg.spot_window_cap = cap;
            assert!(cfg.validate().is_ok(), "cap {cap} must be legal");
        }
        // The streak thresholds are Assumed operator knobs — but 0 would
        // degrade/recover EVERY cycle (never "continuous"); refused.
        cfg.spot_window_cap = 4;
        cfg.concurrency_degrade_after_dirty_cycles = 0;
        assert!(cfg.validate().is_err());
        cfg.concurrency_degrade_after_dirty_cycles = 2;
        cfg.concurrency_recover_after_clean_cycles = 0;
        assert!(cfg.validate().is_err());
    }

    /// Cadence validate(): the Groww three-choice fallback-shape ladder's
    /// structural bounds — the worst shape's verdict must fit inside the
    /// lane cutoff, and the worst-case cycle tail (verdict + 7 sequential
    /// fallback legs) must end strictly before the NEXT minute's burst
    /// (the no-overlap-into-next-burst bound, coordinator 2026-07-15).
    #[test]
    fn test_cadence_config_validate_groww_shape_no_overlap_bounds() {
        // The shipped defaults: worst verdict 0+3000+800 = 3800 < 6000
        // cutoff; worst tail 3800 + 7*1500 = 14300 < 60000.
        let d = CadenceConfig::default();
        assert!(d.validate().is_ok());
        // A cutoff at/below the choice-3 verdict is degenerate.
        let mut cfg = CadenceConfig {
            groww_lane_cutoff_ms: 3_800,
            ..CadenceConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("worst fallback shape"),
            "unexpected error: {err}"
        );
        // A negative burst anchor (pre-close) is refused.
        cfg.groww_lane_cutoff_ms = 6_000;
        cfg.groww_anchor_offset_ms = -1;
        assert!(cfg.validate().is_err());
        // A request timeout that lets the sequential fallback spill into
        // the next minute's burst is refused (7 legs * 8100ms > 56.2s
        // after the choice-3 verdict).
        cfg.groww_anchor_offset_ms = 0;
        cfg.groww_request_timeout_ms = 8_100;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("next minute's burst"),
            "unexpected error: {err}"
        );
    }

    /// Cadence validate(): the per-(underlying, expiry) 3s chain rule's
    /// spacing floor cannot be configured away (2026-07-16: the rule is
    /// per-key ONLY — different underlyings are explicitly concurrent,
    /// so there is no offsets vector and no global gate anymore), and
    /// the burst offset must be a genuine POST-close second.
    #[test]
    fn test_cadence_config_validate_rejects_sub_3s_chain_spacing() {
        // The spacing floor itself cannot be configured away.
        let mut cfg = CadenceConfig {
            chain_min_spacing_ms: 2_999,
            ..CadenceConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("chain_min_spacing_ms"),
            "unexpected error: {err}"
        );
        // A Groww cutoff at/below the burst verdict is degenerate.
        cfg.chain_min_spacing_ms = 3_000;
        cfg.groww_lane_cutoff_ms = 800;
        assert!(cfg.validate().is_err());
        // The burst is a POST-close fire: 0 and negative are refused.
        cfg.groww_lane_cutoff_ms = 6_000;
        cfg.dhan_burst_offset_ms = 0;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("dhan_burst_offset_ms"),
            "unexpected error: {err}"
        );
        cfg.dhan_burst_offset_ms = -1_000;
        assert!(cfg.validate().is_err());
        cfg.dhan_burst_offset_ms = 1_000;
        assert!(cfg.validate().is_ok());
    }

    /// Cadence validate(): the cutoff-ceiling + burst-feasibility bounds
    /// (CAD-NEW-3 / CAD-SEC-1 classes re-derived for the 2026-07-16
    /// post-close shape) — lane cutoffs must resolve inside one minute,
    /// and the nominal burst must land strictly before the Dhan cutoff
    /// (else its fires are silently discarded once the cutoff skip
    /// resolves the lane).
    #[test]
    fn test_cadence_config_validate_burst_band_and_cutoff_ceiling() {
        // A Dhan cutoff at/above one minute makes every cycle overrun.
        let mut cfg = CadenceConfig {
            dhan_lane_cutoff_ms: 120_000,
            ..CadenceConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("< 60000ms"),
            "unexpected error: {err}"
        );
        cfg.dhan_lane_cutoff_ms = 60_000;
        assert!(cfg.validate().is_err(), "exactly 60000 is degenerate");
        // Same ceiling on the Groww side.
        cfg.dhan_lane_cutoff_ms = 15_000;
        cfg.groww_lane_cutoff_ms = 60_000;
        assert!(cfg.validate().is_err());
        cfg.groww_lane_cutoff_ms = 6_000;
        assert!(cfg.validate().is_ok());
        // A nominal burst at/after the Dhan cutoff is silently discarded
        // every cycle (the CAD-NEW-3 class) — refused, exact boundary
        // included; the deepest-spot-bucket bound trips first (burst +
        // 4 windows).
        cfg.dhan_burst_offset_ms = 16_000;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("strictly before dhan_lane_cutoff_ms"),
            "unexpected error: {err}"
        );
        cfg.dhan_burst_offset_ms = 15_000;
        assert!(
            cfg.validate().is_err(),
            "a burst exactly AT the cutoff is equally discarded"
        );
        // Just inside the cutoff still fails the DEEPEST-spot-bucket
        // bound (14999 + 4000 >= 15000); a burst whose whole spill fits
        // is legal (10999 + 4000 < 15000).
        cfg.dhan_burst_offset_ms = 14_999;
        assert!(cfg.validate().is_err(), "deepest bucket past the cutoff");
        cfg.dhan_burst_offset_ms = 10_999;
        assert!(cfg.validate().is_ok(), "whole spill inside the cutoff");
        // The shipped defaults sit comfortably inside the band.
        assert!(CadenceConfig::default().validate().is_ok());
    }

    /// Cadence validate(): the deepest-spot-bucket feasibility bound
    /// covers BOTH schedule inputs of the 2026-07-16 shape — the burst
    /// offset AND the post-close clamp (build_cycle_slots takes
    /// max(burst, T + spot_min_post_close_ms) as the spot base, deepest
    /// bucket = base + 4 windows); a large negative Groww anchor is
    /// rejected; the expiry knobs are bounded sane.
    #[test]
    fn test_cadence_config_validate_rejects_out_of_range_burst_clamp_and_expiry_knobs() {
        // A clamp that pushes the deepest bucket to/past the cutoff is
        // refused (SEC-CAD-1 class: 11000 + 4000 >= 15000 — silently
        // discarded on the resolved-lane guard otherwise).
        let mut cfg = CadenceConfig {
            spot_min_post_close_ms: 20_000,
            ..CadenceConfig::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("spot_min_post_close_ms"),
            "unexpected error: {err}"
        );
        cfg.spot_min_post_close_ms = 11_000;
        assert!(
            cfg.validate().is_err(),
            "clamp deepest bucket exactly AT the cutoff (11000 + 4000 == 15000) is equally discarded"
        );
        cfg.spot_min_post_close_ms = 10_999;
        assert!(cfg.validate().is_ok(), "just inside the cutoff is legal");
        cfg.spot_min_post_close_ms = 300;
        // A LARGE NEGATIVE Groww anchor is rejected (the burst is a
        // post-close fire — a pre-close burst would race the close).
        cfg.groww_anchor_offset_ms = -60_000;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("groww_anchor_offset_ms"),
            "unexpected error: {err}"
        );
        // Expiry knobs: a non-positive retry interval and a deadline
        // outside the seconds-of-day domain are refused.
        cfg.groww_anchor_offset_ms = 0;
        cfg.expiry_retry_interval_ms = 0;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("expiry_retry_interval_ms"),
            "unexpected error: {err}"
        );
        // R3-F1 belt (a): a burst-window-capable interval (> one
        // minute) is refused — 65s @ a 09:14:58 wake would land the
        // last pre-era wave at 09:16:03, the spot-group instant.
        cfg.expiry_retry_interval_ms = 65_000;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("expiry_retry_interval_ms"),
            "unexpected error: {err}"
        );
        // The one-minute ceiling itself (the base.toml default) is legal.
        cfg.expiry_retry_interval_ms = 60_000;
        cfg.expiry_deadline_secs_of_day_ist = 86_400;
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("expiry_deadline_secs_of_day_ist"),
            "unexpected error: {err}"
        );
        cfg.expiry_deadline_secs_of_day_ist = 32_100;
        assert!(cfg.validate().is_ok());
    }

    /// PR-4 (Groww contract leg): the `[groww_contract_1m]` section is
    /// FAIL-SAFE default OFF — an absent section, an empty section, and an
    /// older TOML all deserialize to disabled with the pinned ATM-window
    /// default; explicit values round-trip.
    #[test]
    fn test_groww_contract_1m_config_defaults_off_and_round_trips() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        let d = GrowwContract1mConfig::default();
        assert!(
            !d.enabled,
            "groww_contract_1m must default OFF (fail-safe; depends on the chain leg)"
        );
        assert_eq!(
            d.strikes_each_side,
            crate::constants::GROWW_CONTRACT_1M_DEFAULT_STRIKES_EACH_SIDE,
            "the ATM window default is the pinned constant"
        );

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            groww_contract_1m: GrowwContract1mConfig,
        }
        // Missing section entirely → disabled, never an error.
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [groww_contract_1m] must default, not error");
        assert!(!missing.groww_contract_1m.enabled);
        assert_eq!(missing.groww_contract_1m.strikes_each_side, 2);
        // Empty section (no keys) → field-level defaults.
        let empty: Wrapper = Figment::new()
            .merge(Toml::string("[groww_contract_1m]\n"))
            .extract()
            .expect("empty [groww_contract_1m] must default, not error");
        assert!(!empty.groww_contract_1m.enabled);
        assert_eq!(empty.groww_contract_1m.strikes_each_side, 2);
        // Explicit values (the future flip shape) round-trip.
        let on: Wrapper = Figment::new()
            .merge(Toml::string(
                "[groww_contract_1m]\nenabled = true\nstrikes_each_side = 1\n",
            ))
            .extract()
            .expect("explicit values must round-trip");
        assert!(on.groww_contract_1m.enabled);
        assert_eq!(on.groww_contract_1m.strikes_each_side, 1);
    }

    /// 🔷 DHAN exit-order layer (Cluster B, 2026-07-14): the
    /// `[exit_orders]` section is FAIL-SAFE default OFF — via `Default`,
    /// a missing section, and an empty section — with the verify-ladder
    /// defaults (30s deadline, 5 attempts) intact; explicit values
    /// round-trip. LOCK #1 of the 4-lock OFF switch.
    #[test]
    fn test_exit_orders_config_defaults_off_and_round_trips() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        let d = ExitOrdersConfig::default();
        assert!(
            !d.enabled,
            "exit_orders must default OFF (LOCK #1 — fail-safe)"
        );
        assert_eq!(d.default_freeze_limit_qty, 0, "freeze default is UNSET");
        assert!(d.freeze_limits_reviewed_on.is_empty());
        assert_eq!(d.mpp_verify_deadline_secs, 30);
        assert_eq!(d.mpp_verify_max_attempts, 5);
        assert!((d.default_trailing_jump - 0.0).abs() < f64::EPSILON);

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            exit_orders: ExitOrdersConfig,
        }
        // Missing section entirely → disabled, never an error.
        let missing: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [exit_orders] must default, not error");
        assert!(!missing.exit_orders.enabled);
        assert_eq!(missing.exit_orders.mpp_verify_deadline_secs, 30);
        assert_eq!(missing.exit_orders.mpp_verify_max_attempts, 5);
        // Empty section (no keys) → field-level defaults (the base.toml
        // enabled = false shape must also parse).
        let empty: Wrapper = Figment::new()
            .merge(Toml::string("[exit_orders]\nenabled = false\n"))
            .extract()
            .expect("the base.toml [exit_orders] shape must parse");
        assert!(!empty.exit_orders.enabled);
        assert_eq!(empty.exit_orders.mpp_verify_deadline_secs, 30);
        // Explicit values (the future dry-run-enable shape) round-trip.
        let on: Wrapper = Figment::new()
            .merge(Toml::string(
                "[exit_orders]\nenabled = true\ndefault_freeze_limit_qty = 1800\n\
                 freeze_limits_reviewed_on = \"2026-07-14\"\nmpp_verify_deadline_secs = 45\n\
                 mpp_verify_max_attempts = 6\ndefault_trailing_jump = 0.5\n",
            ))
            .extract()
            .expect("explicit values must round-trip");
        assert!(on.exit_orders.enabled);
        assert_eq!(on.exit_orders.default_freeze_limit_qty, 1800);
        assert_eq!(on.exit_orders.freeze_limits_reviewed_on, "2026-07-14");
        assert_eq!(on.exit_orders.mpp_verify_deadline_secs, 45);
        assert_eq!(on.exit_orders.mpp_verify_max_attempts, 6);
        assert!((on.exit_orders.default_trailing_jump - 0.5).abs() < f64::EPSILON);
        // Deterministic (refuter round 2, 2026-07-14): injected today —
        // this assertion must never depend on the host wall clock.
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 14).unwrap_or_default();
        assert!(on.exit_orders.validate_with_today(today).is_ok());
    }

    /// `ExitOrdersConfig::validate` — every bound, boundary-tested
    /// (design §3.6): deadline 1..=300, attempts 1..=8, finite
    /// non-negative trailing jump always; freeze >= 1 + parseable
    /// review date only when enabled.
    #[test]
    fn test_exit_orders_config_validate_with_today_bounds_and_dates() {
        // Disabled defaults are always valid (the shipped state).
        assert!(ExitOrdersConfig::default().validate().is_ok());

        // Deadline bounds — 0 and 301 reject, 1 and 300 pass.
        for (deadline, ok) in [(0_u64, false), (301, false), (1, true), (300, true)] {
            let cfg = ExitOrdersConfig {
                mpp_verify_deadline_secs: deadline,
                ..Default::default()
            };
            assert_eq!(
                cfg.validate().is_ok(),
                ok,
                "deadline {deadline} boundary verdict must be {ok}"
            );
        }

        // Attempt bounds — 0 and 9 reject, 1 and 8 pass.
        for (attempts, ok) in [(0_u32, false), (9, false), (1, true), (8, true)] {
            let cfg = ExitOrdersConfig {
                mpp_verify_max_attempts: attempts,
                ..Default::default()
            };
            assert_eq!(
                cfg.validate().is_ok(),
                ok,
                "attempts {attempts} boundary verdict must be {ok}"
            );
        }

        // Trailing jump — NaN / infinity / negative reject; 0.0 passes.
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.05] {
            let cfg = ExitOrdersConfig {
                default_trailing_jump: bad,
                ..Default::default()
            };
            assert!(
                cfg.validate().is_err(),
                "trailing jump {bad} must reject (finite >= 0.0 required)"
            );
        }
        let cfg = ExitOrdersConfig {
            default_trailing_jump: 0.0,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());

        // Enabled-only gates: freeze must be >= 1 and the review date must
        // parse — the DISABLED default (freeze 0, empty date) stays valid.
        // Deterministic (refuter round 2, 2026-07-14): every enabled-arm
        // assertion injects a fixed today so the hardcoded review dates
        // can never flip against the host wall clock.
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 14).unwrap_or_default();
        let mut cfg = ExitOrdersConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(
            cfg.validate_with_today(today).is_err(),
            "enabled with freeze 0 (unset) must reject"
        );
        cfg.default_freeze_limit_qty = 1800;
        assert!(
            cfg.validate_with_today(today).is_err(),
            "enabled with empty review date must reject"
        );
        cfg.freeze_limits_reviewed_on = "14-07-2026".to_string();
        assert!(
            cfg.validate_with_today(today).is_err(),
            "enabled with non-%Y-%m-%d review date must reject"
        );
        cfg.freeze_limits_reviewed_on = "2026-07-14".to_string();
        assert!(
            cfg.validate_with_today(today).is_ok(),
            "enabled with sane values must pass"
        );
        cfg.default_freeze_limit_qty = -5;
        assert!(
            cfg.validate_with_today(today).is_err(),
            "negative freeze must reject"
        );
    }

    /// L2 (2026-07-14 hostile review): a FUTURE `freeze_limits_reviewed_on`
    /// rejects when enabled — a typo'd/backdated future review date would
    /// silence the >90-day staleness WARN forever. Past dates (even stale
    /// ones) stay valid — staleness is the WARN's job, not validate()'s.
    #[test]
    fn test_exit_orders_config_validate_rejects_future_review_date() {
        // Deterministic (refuter round 2, 2026-07-14): injected today —
        // the boundary assertions can never flip against the wall clock.
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 14).unwrap_or_default();
        let mut cfg = ExitOrdersConfig {
            enabled: true,
            default_freeze_limit_qty: 1800,
            freeze_limits_reviewed_on: "2026-07-15".to_string(),
            ..Default::default()
        };
        let err = cfg.validate_with_today(today).unwrap_err();
        assert!(
            err.to_string().contains("future"),
            "error must name the future date, got: {err}"
        );
        // Same-day review is valid (today-or-past rule, strict >).
        cfg.freeze_limits_reviewed_on = "2026-07-14".to_string();
        assert!(cfg.validate_with_today(today).is_ok());
        // A stale-but-past date validates (the staleness WARN handles it).
        cfg.freeze_limits_reviewed_on = "2020-01-01".to_string();
        assert!(cfg.validate_with_today(today).is_ok());
        // Disabled configs never evaluate the date at all — proven through
        // the PRODUCTION wall-clock entry point (also the pub-fn wiring
        // call-site pin for `validate()` delegating to the injected core).
        cfg.enabled = false;
        cfg.freeze_limits_reviewed_on = "2999-01-01".to_string();
        assert!(cfg.validate().is_ok());
    }

    /// `freeze_review_is_stale` — pure >90-day boundary + fail-safe on
    /// empty/garbage input (stale ⇒ the boot WARN fires, never a silent
    /// pass on an unparsable date).
    #[test]
    fn test_freeze_review_is_stale_boundary_and_fail_safe() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 14).unwrap_or_default();
        // Exactly 90 days old = NOT stale (strict > per the design).
        assert!(!freeze_review_is_stale("2026-04-15", today));
        // 91 days old = stale.
        assert!(freeze_review_is_stale("2026-04-14", today));
        // Same-day and future reviews are fresh.
        assert!(!freeze_review_is_stale("2026-07-14", today));
        // Empty / garbage / wrong format ⇒ STALE (fail-safe).
        assert!(freeze_review_is_stale("", today));
        assert!(freeze_review_is_stale("not-a-date", today));
        assert!(freeze_review_is_stale("14-07-2026", today));
    }

    /// `ApplicationConfig::validate` rejects a bad `[exit_orders]`
    /// section (the boot-time hook is actually wired).
    #[test]
    fn test_application_config_validate_rejects_bad_exit_orders() {
        let mut config = make_valid_config();
        config.exit_orders.mpp_verify_max_attempts = 0;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("mpp_verify_max_attempts"),
            "error must name the violated exit_orders bound, got: {err}"
        );
        // And the valid default passes end-to-end.
        let config = make_valid_config();
        assert!(config.validate().is_ok());
    }

    /// CAPTURE-LEG MUTUAL EXCLUSION (coordinator ruling B, 2026-07-16 —
    /// SUBSUME, NEVER SHARE, interim fail-closed path; RS3 hardening
    /// same day): enabling the cadence while ANY legacy per-minute
    /// RECORD-capture leg is still enabled is a validation ERROR (no
    /// double demand on one broker's rate budget is ever legal) —
    /// REGARDLESS of `feeds.*_enabled`, because the cadence lanes key on
    /// the RUNTIME feed atomics while the legacy legs spawn on their own
    /// config gates, so the boot-time feed flags bound neither side.
    /// Cadence on with the legs off is legal; cadence off with the legs
    /// on (the base.toml shape today) stays legal.
    #[test]
    fn test_application_config_validate_cadence_capture_leg_mutual_exclusion() {
        // Cadence ON + a legacy Dhan leg ON → error (feed flag irrelevant).
        let mut config = make_valid_config();
        config.cadence.enabled = true;
        config.feeds.dhan_enabled = true;
        config.spot_1m_rest.enabled = true;
        config.option_chain_1m.enabled = false;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("spot_1m_rest"),
            "error must name the standing Dhan leg, got: {err}"
        );
        config.spot_1m_rest.enabled = false;
        config.option_chain_1m.enabled = true;
        assert!(config.validate().is_err(), "chain leg alone also refuses");
        // Cadence ON + a legacy Groww leg ON → error (feed flag irrelevant).
        let mut config = make_valid_config();
        config.cadence.enabled = true;
        config.feeds.groww_enabled = true;
        config.spot_1m_rest.enabled = false;
        config.option_chain_1m.enabled = false;
        for flip in 0..3 {
            config.groww_spot_1m.enabled = flip == 0;
            config.groww_option_chain_1m.enabled = flip == 1;
            config.groww_contract_1m.enabled = flip == 2;
            let err = config.validate().unwrap_err();
            assert!(
                err.to_string().contains("Groww"),
                "error must name the Groww lane, got: {err}"
            );
        }
        // Cadence ON + all legs OFF → ok (the stand-down shape).
        config.groww_spot_1m.enabled = false;
        config.groww_option_chain_1m.enabled = false;
        config.groww_contract_1m.enabled = false;
        assert!(config.validate().is_ok(), "cadence on + legs off is legal");
        // Cadence OFF + legs ON (today's base.toml shape) → ok.
        let mut config = make_valid_config();
        config.cadence.enabled = false;
        config.spot_1m_rest.enabled = true;
        config.option_chain_1m.enabled = true;
        config.groww_spot_1m.enabled = true;
        assert!(
            config.validate().is_ok(),
            "cadence off + legacy legs on is today's valid shape"
        );
        // RS3 (2026-07-16): a boot-time-DISABLED feed lane's legs DO
        // block the cadence now — the pre-RS3 key on `feeds.*_enabled`
        // admitted cadence=ON + feed=OFF + legs=ON, which sat one
        // runtime /api/feeds enable away from reconstructing the
        // forbidden double demand (the cadence lanes key on the RUNTIME
        // atomics, not the boot flags). Fail-closed at the root.
        let mut config = make_valid_config();
        config.cadence.enabled = true;
        config.feeds.dhan_enabled = true;
        config.feeds.groww_enabled = false;
        config.spot_1m_rest.enabled = false;
        config.option_chain_1m.enabled = false;
        config.groww_spot_1m.enabled = true;
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("Groww"),
            "RS3: a runtime-reachable lane's legs must refuse regardless \
             of the boot feed flag, got: {err}"
        );
        // Same in the Dhan direction: feeds.dhan_enabled=false does not
        // exempt the Dhan legs from the exclusion.
        let mut config = make_valid_config();
        config.cadence.enabled = true;
        config.feeds.dhan_enabled = false;
        config.spot_1m_rest.enabled = true;
        assert!(
            config.validate().is_err(),
            "RS3: Dhan legs refuse with feeds.dhan_enabled=false too"
        );
    }

    /// A missing `[feeds]` section must fall back to the safe default
    /// (Dhan ON, Groww OFF) via `#[serde(default)]` — never error.
    #[test]
    fn test_feeds_config_missing_section_uses_default() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            feeds: FeedsConfig,
        }
        let wrapper: Wrapper = Figment::new()
            .merge(Toml::string("[other]\nx = 1\n"))
            .extract()
            .expect("missing [feeds] must use defaults, not error");
        assert!(wrapper.feeds.dhan_enabled);
        assert!(!wrapper.feeds.groww_enabled);
    }

    /// All four toggle permutations round-trip via figment and the
    /// `any_enabled` / `both_enabled` helpers report correctly.
    #[test]
    fn test_feeds_config_all_toggle_permutations() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        #[derive(Deserialize)]
        struct Wrapper {
            feeds: FeedsConfig,
        }
        // (dhan, groww, any_enabled, both_enabled)
        let cases = [
            (true, false, true, false),   // default: Dhan-only
            (false, true, true, false),   // Groww-only
            (true, true, true, true),     // both (the cross-check target)
            (false, false, false, false), // no-feed (boot must guard via any_enabled)
        ];
        for (dhan, groww, any, both) in cases {
            let toml = format!("[feeds]\ndhan_enabled = {dhan}\ngroww_enabled = {groww}\n");
            let wrapper: Wrapper = Figment::new()
                .merge(Toml::string(&toml))
                .extract()
                .expect("feeds toggle must round-trip");
            assert_eq!(wrapper.feeds.dhan_enabled, dhan);
            assert_eq!(wrapper.feeds.groww_enabled, groww);
            assert_eq!(
                wrapper.feeds.any_enabled(),
                any,
                "any_enabled wrong for dhan={dhan} groww={groww}"
            );
            assert_eq!(
                wrapper.feeds.both_enabled(),
                both,
                "both_enabled wrong for dhan={dhan} groww={groww}"
            );
        }
    }

    /// `any_enabled` is the boot no-feed guard signal: false ONLY when
    /// every feed is disabled.
    #[test]
    fn test_feeds_any_enabled_false_only_when_all_off() {
        assert!(
            !FeedsConfig {
                dhan_enabled: false,
                groww_enabled: false,
                ..Default::default()
            }
            .any_enabled()
        );
        assert!(
            FeedsConfig {
                dhan_enabled: true,
                groww_enabled: false,
                ..Default::default()
            }
            .any_enabled()
        );
        assert!(
            FeedsConfig {
                dhan_enabled: false,
                groww_enabled: true,
                ..Default::default()
            }
            .any_enabled()
        );
        assert!(
            FeedsConfig {
                dhan_enabled: true,
                groww_enabled: true,
                ..Default::default()
            }
            .any_enabled()
        );
    }
}
