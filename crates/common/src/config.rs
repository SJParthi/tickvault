//! Configuration structs deserialized from `config/base.toml`.
//!
//! Every runtime value that might differ between environments lives here.
//! Secrets are NEVER in config — they come from AWS SSM Parameter Store.

use anyhow::{Result, bail};
use chrono::NaiveTime;
use serde::Deserialize;

use crate::constants::SEBI_MAX_ORDERS_PER_SECOND;
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
    #[serde(default)]
    pub subscription: SubscriptionConfig,
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
    /// `[spot_1m_rest]` — per-minute spot 1m REST pipeline (operator grant
    /// 2026-07-12, `no-rest-except-live-feed-2026-06-27.md` §8): every
    /// trading-day minute close in session, fetch that just-closed minute's
    /// official 1m OHLCV for the 3 IDX_I spot indices via Dhan
    /// `POST /v2/charts/intraday` and persist to `spot_1m_rest`. Absent
    /// section ⇒ DISABLED (fail-safe default off).
    #[serde(default)]
    pub spot_1m_rest: Spot1mRestConfig,
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
    /// toggle. Absent section ⇒ `two_wave` + warm-up OFF (fail-safe).
    #[serde(default)]
    pub groww_rest_burst: GrowwRestBurstConfig,
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
    /// Groww live feed (feed #2). Default OFF — opt-in so prod behaviour
    /// is unchanged until explicitly enabled.
    pub groww_enabled: bool,
    /// `[feeds.groww]` — Groww-feed tuning sub-table (auto-scale §34,
    /// operator authorization 2026-07-03). A missing sub-table keeps the
    /// single-connection behaviour byte-identical.
    #[serde(default)]
    pub groww: GrowwFeedTuning,
    /// Groww NATIVE-RUST SHADOW client (PR-R1 of the parity migration,
    /// operator "go" 2026-07-04 — `groww-second-feed-scope-2026-06-19.md`
    /// §35). Default OFF. When true, a supervised task connects the native
    /// Rust NATS-over-WebSocket client to Groww ALONGSIDE the Python
    /// sidecar (same watch set) and writes its OWN NDJSON file
    /// (`data/groww/rust-live-ticks.ndjson`, same line schema as the
    /// sidecar's capture file) for the future exact per-tick parity
    /// comparer. NO shared-table writes, NO strategy/order wiring, NO
    /// sidecar changes. `#[serde(default)]` so existing TOMLs without the
    /// key behave byte-identically.
    #[serde(default)]
    pub groww_native_shadow: bool,
}

impl Default for FeedsConfig {
    fn default() -> Self {
        Self {
            dhan_enabled: true,
            groww_enabled: false,
            groww: GrowwFeedTuning::default(),
            groww_native_shadow: false,
        }
    }
}

/// `[feeds.groww]` — Groww feed tuning container (auto-scale §34).
///
/// Nested under `[feeds]` so the TOML surface reads
/// `[feeds.groww.scale]` exactly as the design doc specifies, while the
/// existing flat `groww_enabled` key in `[feeds]` is untouched.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct GrowwFeedTuning {
    /// `[feeds.groww.scale]` — multi-connection auto-scale ladder config.
    #[serde(default)]
    pub scale: GrowwScaleConfig,
    /// S3 bucket for the sidecar's rotated capture archives
    /// (`live-ticks-YYYYMMDD.ndjson`) — 2026-07-13 disk-retention hardening.
    /// The supervisor injects this into the sidecar child as
    /// `TICKVAULT_GROWW_ARCHIVE_S3_BUCKET`; the sidecar uploads each rotated
    /// archive, VERIFIES the copy (head_object size match), and only then
    /// deletes the local file after a grace window. Empty (the default) =
    /// archival OFF: rotated archives are kept on disk (dev-Mac behaviour) —
    /// the sidecar NEVER deletes a file without a verified S3 copy.
    #[serde(default)]
    pub capture_archive_s3_bucket: String,
    /// Key prefix inside the archive bucket (`<prefix>/<filename>`).
    /// Empty = bucket root.
    #[serde(default)]
    pub capture_archive_s3_prefix: String,
}

/// Tier A ceiling (§34.2, operator lock 2026-07-03): the Monday-approved
/// maximum connection count. Raising `target_connections` above this
/// requires the Tier B live-measurement gate recorded with a dated note.
pub const GROWW_SCALE_TIER_A_MAX_CONNS: usize = 10;

/// Tier B ceiling (§34.2): 11–25 connections, gated on a live RAM/CPU/disk
/// measurement at the Tier A ceiling.
pub const GROWW_SCALE_TIER_B_MAX_CONNS: usize = 25;

/// Hard maximum connection count (§34.2 Tier C): 100 connections requires
/// infra sign-off (instance/EBS/QuestDB re-measure). `validate()` REJECTS
/// any `target_connections` above this regardless of tier evidence.
pub const GROWW_SCALE_HARD_MAX_CONNS: usize = 100;

/// Groww live-feed per-session subscription hard cap (documented "upto 1000
/// subscriptions are allowed at a time"). Mirrors
/// `tickvault-core::feed::groww::instruments::GROWW_MAX_SUBSCRIPTIONS`
/// (common cannot depend on core; core's `groww_scale_config_cap_matches`
/// ratchet pins the two constants equal).
pub const GROWW_SCALE_MAX_INSTRUMENTS_PER_CONN: usize = 1000;

/// `[feeds.groww.scale]` — Groww multi-connection AUTO-SCALE ladder
/// (operator authorization 2026-07-03, §34 of
/// `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`).
///
/// `enabled = false` (the default) routes through the existing
/// single-connection sidecar path — zero new processes, zero new file
/// paths, byte-identical behaviour. When enabled, the ladder grows the
/// sidecar fleet through `ladder` rungs toward `target_connections`, each
/// connection owning a disjoint range-based shard of ≤
/// `instruments_per_conn` instruments, advancing ONLY while every gate
/// holds for `gate_hold_minutes` inside the `advance_window_ist` window,
/// and auto-correcting every failure (rollback to last-healthy + expo
/// hold; fleet-wide failure → global cooldown + halve). See
/// `.claude/rules/project/groww-scale-error-codes.md` for the failure
/// taxonomy (GROWW-SCALE-01..04).
#[derive(Debug, Clone, Deserialize)]
pub struct GrowwScaleConfig {
    /// Master switch. Default OFF — single-connection path, byte-identical.
    #[serde(default)]
    pub enabled: bool,
    /// Ladder ceiling. Default 10 = Tier A (§34.2). Values above
    /// [`GROWW_SCALE_HARD_MAX_CONNS`] are rejected at boot.
    #[serde(default = "default_groww_scale_target_connections")]
    pub target_connections: usize,
    /// Instruments per connection shard. Default 1000 = the documented
    /// Groww per-session cap ([`GROWW_SCALE_MAX_INSTRUMENTS_PER_CONN`]).
    #[serde(default = "default_groww_scale_instruments_per_conn")]
    pub instruments_per_conn: usize,
    /// Day-1 rungs the ladder climbs (strictly increasing; last rung ≤
    /// `target_connections`). Default `[1, 2, 5, 10]`.
    #[serde(default = "default_groww_scale_ladder")]
    pub ladder: Vec<usize>,
    /// How long EVERY advance gate must hold before the next rung fires.
    #[serde(default = "default_groww_scale_gate_hold_minutes")]
    pub gate_hold_minutes: u64,
    /// Advance gate: box CPU must be below this percentage.
    #[serde(default = "default_groww_scale_gate_max_cpu_pct")]
    pub gate_max_cpu_pct: f64,
    /// Advance gate: free disk in the capture directory must exceed this
    /// percentage of the volume.
    #[serde(default = "default_groww_scale_gate_min_disk_free_pct")]
    pub gate_min_disk_free_pct: f64,
    /// Advance gate: per-shard capture lag (now − max tick ts) p99 must be
    /// below this many milliseconds. Default 30_000 (30s).
    #[serde(default = "default_groww_scale_gate_max_capture_lag_ms")]
    pub gate_max_capture_lag_ms: u64,
    /// Base hold after a failed rung attempt; doubles per consecutive
    /// failure at the SAME rung, capped at 4h (ladder-side constant).
    #[serde(default = "default_groww_scale_rollback_hold_base_minutes")]
    pub rollback_hold_base_minutes: u64,
    /// ADVANCING is allowed only inside this IST window (`["HH:MM","HH:MM"]`,
    /// start < end) or pre-open. Default `["09:20", "14:30"]` — never in the
    /// open/close burst windows.
    #[serde(default = "default_groww_scale_advance_window_ist")]
    pub advance_window_ist: [String; 2],
    /// §34 PR-3 cap-probe mode: when `true` the ladder runs EXACTLY
    /// 2 connections × 600 instruments (overriding `ladder` /
    /// `target_connections` / `instruments_per_conn`), classifies whether the
    /// Groww limit is per-CONNECTION or per-ACCOUNT, prints the verdict, and
    /// then holds at 2 conns for the session. Default OFF.
    #[serde(default)]
    pub probe_mode: bool,
    /// §34 PR-3 weekend SMOKE mode: when `true` AND the market is CLOSED
    /// (weekend / NSE holiday / off-hours), the ladder still exercises the
    /// full machinery (shard cut, fleet spawn, rung climbing) with the
    /// tick-dependent gates honestly SKIPPED (no live market ⇒ no ticks by
    /// design, never a failure), and every outcome is labelled SMOKE so a
    /// machinery-validated run is never mistaken for a live validation.
    /// Has NO effect while the market is open (normal gates apply).
    /// Default OFF — production keeps the off-hours ladder freeze.
    #[serde(default)]
    pub weekend_smoke: bool,
}

fn default_groww_scale_target_connections() -> usize {
    GROWW_SCALE_TIER_A_MAX_CONNS
}
fn default_groww_scale_instruments_per_conn() -> usize {
    GROWW_SCALE_MAX_INSTRUMENTS_PER_CONN
}
fn default_groww_scale_ladder() -> Vec<usize> {
    vec![1, 2, 5, 10]
}
fn default_groww_scale_gate_hold_minutes() -> u64 {
    15
}
fn default_groww_scale_gate_max_cpu_pct() -> f64 {
    70.0
}
fn default_groww_scale_gate_min_disk_free_pct() -> f64 {
    20.0
}
fn default_groww_scale_gate_max_capture_lag_ms() -> u64 {
    30_000
}
fn default_groww_scale_rollback_hold_base_minutes() -> u64 {
    10
}
fn default_groww_scale_advance_window_ist() -> [String; 2] {
    [String::from("09:20"), String::from("14:30")]
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
/// break (the `GrowwFeedTuning` sub-table precedent).
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
        }
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

impl Default for GrowwScaleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            target_connections: default_groww_scale_target_connections(),
            instruments_per_conn: default_groww_scale_instruments_per_conn(),
            ladder: default_groww_scale_ladder(),
            gate_hold_minutes: default_groww_scale_gate_hold_minutes(),
            gate_max_cpu_pct: default_groww_scale_gate_max_cpu_pct(),
            gate_min_disk_free_pct: default_groww_scale_gate_min_disk_free_pct(),
            gate_max_capture_lag_ms: default_groww_scale_gate_max_capture_lag_ms(),
            rollback_hold_base_minutes: default_groww_scale_rollback_hold_base_minutes(),
            advance_window_ist: default_groww_scale_advance_window_ist(),
            probe_mode: false,
            weekend_smoke: false,
        }
    }
}

impl GrowwScaleConfig {
    /// Validates the auto-scale envelope at boot, BEFORE any sidecar
    /// process spawns (fail-closed per §34). Pure — no I/O, no clock.
    ///
    /// # Errors
    /// Returns a descriptive error for the first violation found:
    /// connection/instrument caps, non-increasing ladder, ladder rung above
    /// the target, malformed/inverted advance window, zero gate values, or
    /// non-finite percentage gates.
    pub fn validate(&self) -> Result<()> {
        if self.target_connections == 0 || self.target_connections > GROWW_SCALE_HARD_MAX_CONNS {
            bail!(
                "feeds.groww.scale.target_connections must be in [1, {}], got {}",
                GROWW_SCALE_HARD_MAX_CONNS,
                self.target_connections
            );
        }
        if self.instruments_per_conn == 0
            || self.instruments_per_conn > GROWW_SCALE_MAX_INSTRUMENTS_PER_CONN
        {
            bail!(
                "feeds.groww.scale.instruments_per_conn must be in [1, {}] (Groww per-session cap), got {}",
                GROWW_SCALE_MAX_INSTRUMENTS_PER_CONN,
                self.instruments_per_conn
            );
        }
        if self.ladder.is_empty() {
            bail!("feeds.groww.scale.ladder must not be empty");
        }
        for pair in self.ladder.windows(2) {
            if pair[1] <= pair[0] {
                bail!(
                    "feeds.groww.scale.ladder must be strictly increasing, got {:?}",
                    self.ladder
                );
            }
        }
        if self.ladder[0] == 0 {
            bail!("feeds.groww.scale.ladder rungs must be >= 1");
        }
        // Strictly-increasing + non-empty ⇒ last() exists and is the max.
        if let Some(&last) = self.ladder.last()
            && last > self.target_connections
        {
            bail!(
                "feeds.groww.scale.ladder last rung ({}) exceeds target_connections ({})",
                last,
                self.target_connections
            );
        }
        if self.gate_hold_minutes == 0 {
            bail!("feeds.groww.scale.gate_hold_minutes must be > 0");
        }
        if self.rollback_hold_base_minutes == 0 {
            bail!("feeds.groww.scale.rollback_hold_base_minutes must be > 0");
        }
        if self.gate_max_capture_lag_ms == 0 {
            bail!("feeds.groww.scale.gate_max_capture_lag_ms must be > 0");
        }
        if !self.gate_max_cpu_pct.is_finite()
            || self.gate_max_cpu_pct <= 0.0
            || self.gate_max_cpu_pct > 100.0
        {
            bail!(
                "feeds.groww.scale.gate_max_cpu_pct must be a finite value in (0, 100], got {}",
                self.gate_max_cpu_pct
            );
        }
        if !self.gate_min_disk_free_pct.is_finite()
            || self.gate_min_disk_free_pct < 0.0
            || self.gate_min_disk_free_pct >= 100.0
        {
            bail!(
                "feeds.groww.scale.gate_min_disk_free_pct must be a finite value in [0, 100), got {}",
                self.gate_min_disk_free_pct
            );
        }
        let parse_hm = |field: &str, value: &str| -> Result<NaiveTime> {
            NaiveTime::parse_from_str(value, "%H:%M").map_err(|_| {
                anyhow::anyhow!(
                    "feeds.groww.scale.advance_window_ist {} is not a valid HH:MM time: '{}'",
                    field,
                    value
                )
            })
        };
        let start = parse_hm("start", &self.advance_window_ist[0])?;
        let end = parse_hm("end", &self.advance_window_ist[1])?;
        if start >= end {
            bail!(
                "feeds.groww.scale.advance_window_ist start ('{}') must be before end ('{}')",
                self.advance_window_ist[0],
                self.advance_window_ist[1]
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
    /// Wave 2 Item 8 — tick-gap detector 60-second alert coalescing.
    pub tick_gap_detector_60s_coalesce: bool,
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
            tick_gap_detector_60s_coalesce: true,
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
    /// sequence panics. Format: `YYYY-MM-DD`. Default `2026-06-30` per
    /// Parthiban's "no real orders until June end" requirement.
    ///
    /// Set to `1970-01-01` (or any past date) to disable the gate.
    #[serde(default = "default_sandbox_only_until")]
    pub sandbox_only_until: String,
}

fn default_sandbox_only_until() -> String {
    // Per Parthiban — sandbox-only until June end 2026.
    "2026-06-30".to_string()
}

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

    /// Per-conn activity watchdog threshold in seconds. AWS-lifecycle
    /// LOCKED (PR #7b) — under `SubscriptionScope::Indices4Only` main.rs
    /// overrides this at boot to `WATCHDOG_THRESHOLD_IDX_I_SECS = 3` (the
    /// expected 1–3 tick/sec window for IDX_I). Defaults to the legacy
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
    /// the 21 `candles_*` tables, ~1.2–2 GB/day combined). 90 days of ticks
    /// (~135+ GB) can never fit the 30 GB volume — the hot window must be
    /// shorter, with S3 as the durable long-term store (aws-budget.md §5
    /// hot-window-on-EBS doctrine; SEBI retention satisfied by the S3 copy).
    /// Only consulted when `archive_enabled = true`; clamped to a hard
    /// MIN_HOT_DAYS=2 floor at use (today + yesterday are untouchable).
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

/// Default market-data hot window: 14 days. Inert unless `archive_enabled`;
/// safe-by-default because the archive→verify→drop flow is fail-closed
/// (no verified S3 copy ⇒ no drop).
const fn default_market_data_hot_days() -> u32 {
    14
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

/// Subscription scope gate (Wave 5 Item 1).
///
/// Selects between the legacy full-universe subscription (216 stock F&O +
/// 3 indices full chain ≈ 24,324 instruments) and the indices-only scope
/// (NIFTY + BANKNIFTY + SENSEX with ALL future expiries + every strike;
/// cash equities + IDX_I unchanged ≈ 10-11K instruments — see
/// `subscription_planner.rs` Section 3 for the all-expiries policy
/// reverted on 2026-05-02 per operator's term-structure-visibility
/// requirement). Production count varies day-to-day with weekly expiry
/// roll + new strike addition; range observed 9.5K–11.5K.
///
/// Single-variant enum. AWS-lifecycle LOCKED scope per
/// `.claude/rules/project/websocket-connection-scope-lock.md` +
/// operator-charter §I (lock 2026-05-15). PR #7b retired the 3 legacy
/// variants (`FullUniverse`, `IndicesOnlyAllExpiries`,
/// `IndicesUnderlyingsOnly`); the enum is preserved as a 1-variant
/// type so future scope expansion must go through this rule file
/// and a new enum variant (instead of a boolean flag).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionScope {
    /// AWS-lifecycle LOCKED scope (operator lock 2026-05-15 §I).
    /// Subscribe ONLY the 4 IDX_I SIDs: NIFTY=13, BANKNIFTY=25,
    /// SENSEX=51, INDIA VIX=21. NO derivatives, NO sectoral display
    /// indices, NO NSE_EQ. Target: 4 SIDs on a single main-feed
    /// WebSocket connection.
    #[default]
    #[serde(rename = "indices_4_only")]
    Indices4Only,

    /// Daily-universe scope (operator lock 2026-05-27 — see
    /// `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`).
    /// Subscribe ~250 SIDs daily-fetched from Dhan Detailed CSV: all
    /// NSE `IDX_I` indices + 1 BSE SENSEX `IDX_I` index + every unique
    /// `UNDERLYING_SECURITY_ID` referenced by `FUTSTK/OPTSTK/FUTIDX/
    /// OPTIDX` rows (resolved to NSE_EQ spots). All in Quote mode
    /// (request code 17, 50-byte response packets carrying day OHLC).
    /// Target: ~250 SIDs on a single main-feed WebSocket connection
    /// (Dhan cap = 5,000 SIDs/conn). Fully landed once Sub-PRs
    /// #2-#13 of the 14-sub-PR sequence ship. Currently NOT the
    /// `#[default]` — code path activation happens incrementally.
    #[serde(rename = "daily_universe")]
    DailyUniverse,
}

impl SubscriptionScope {
    /// Stable string label used for tracing fields, the
    /// `tv_subscription_scope` info-gauge, and audit rows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Indices4Only => "indices_4_only",
            Self::DailyUniverse => "daily_universe",
        }
    }
}

/// AWS-lifecycle LOCKED (PR #7b) — main-feed WebSocket connection pool
/// size is ALWAYS 1 under the single-variant `Indices4Only` scope.
/// 4 IDX_I SIDs fit comfortably on a single connection (Dhan cap =
/// 5,000 instruments/conn). The `configured` parameter is preserved
/// for call-site compatibility but is ignored — collapsing it would
/// touch every `dhan.max_websocket_connections` plumbing site.
///
/// Pure function. Tested by
/// `test_effective_main_feed_pool_size_is_always_one_under_indices4only`.
#[inline]
#[must_use]
pub const fn effective_main_feed_pool_size(_scope: SubscriptionScope, _configured: usize) -> usize {
    crate::constants::PHASE_0_MAIN_FEED_CONNECTION_COUNT
}

/// Subscription planner configuration.
///
/// Controls which instruments are subscribed and at what feed mode.
/// Indices get full chain (all expiries, all strikes). Stocks get current
/// expiry only with ATM ± N strike filtering.
#[derive(Debug, Clone, Deserialize)]
pub struct SubscriptionConfig {
    /// AWS-lifecycle LOCKED scope. Single variant: `Indices4Only`.
    /// See `websocket-connection-scope-lock.md`.
    #[serde(default)]
    pub scope: SubscriptionScope,

    /// Feed mode for all subscriptions. Always Full for maximum data (LTP, OI, depth).
    /// IDX_I instruments are forced to Ticker at connection level (Dhan limitation).
    /// Valid values: "Ticker", "Quote", "Full".
    pub feed_mode: String,

    /// Whether to subscribe stock equity price feeds (NSE_EQ segment).
    pub subscribe_stock_equities: bool,

    /// Number of strikes above ATM for stock options.
    pub stock_atm_strikes_above: usize,

    /// Number of strikes below ATM for stock options.
    pub stock_atm_strikes_below: usize,

    /// Default LTP to use for ATM calculation when no live price is available.
    /// When the system first starts, there are no live prices yet.
    /// This fallback ensures we subscribe to a reasonable strike range.
    /// Once live prices arrive, dynamic rebalancing (Phase 2) will adjust.
    pub stock_default_atm_fallback_enabled: bool,

    /// Enable 20-level depth feed (separate WebSocket, uses 1 of 5 connection slots).
    /// Subscribes ATM ± 5 strikes for NIFTY and BANKNIFTY on the depth endpoint.
    #[serde(default)]
    pub enable_twenty_depth: bool,

    /// Maximum instruments to subscribe on the 20-level depth feed (max 50 per connection).
    /// Default 49 = ATM + 24 CE above + 24 PE below.
    #[serde(default = "default_twenty_depth_max_instruments")]
    pub twenty_depth_max_instruments: usize,
}

fn default_twenty_depth_max_instruments() -> usize {
    49
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            scope: SubscriptionScope::default(),
            feed_mode: "Full".to_string(),
            subscribe_stock_equities: true,
            stock_atm_strikes_above: 25,
            stock_atm_strikes_below: 25,
            stock_default_atm_fallback_enabled: true,
            enable_twenty_depth: false,
            twenty_depth_max_instruments: 49,
        }
    }
}

impl SubscriptionConfig {
    /// Parses the feed_mode string into a `FeedMode` enum.
    ///
    /// # Errors
    /// Returns error if the string is not a recognized feed mode.
    pub fn parsed_feed_mode(&self) -> Result<crate::types::FeedMode> {
        match self.feed_mode.as_str() {
            "Ticker" => Ok(crate::types::FeedMode::Ticker),
            "Quote" => Ok(crate::types::FeedMode::Quote),
            "Full" => Ok(crate::types::FeedMode::Full),
            other => bail!(
                "subscription.feed_mode must be Ticker/Quote/Full, got '{}'",
                other
            ),
        }
    }
}

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

        // §34 (2026-07-03): Groww multi-connection auto-scale — the ladder
        // envelope is validated at boot, BEFORE any sidecar process spawns
        // (fail-closed; the default scale.enabled=false section is always
        // valid, so today's single-conn boot is unaffected).
        self.feeds.groww.scale.validate()?;

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
    fn test_sandbox_default_value_is_2026_06_30() {
        // The default value is exactly the date Parthiban specified.
        assert_eq!(default_sandbox_only_until(), "2026-06-30");
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
        assert_eq!(cfg.market_data_hot_days, 14);
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
        assert_eq!(default_market_data_hot_days(), 14);
        assert_eq!(default_max_partitions_per_run(), 200);
        let cfg: PartitionRetentionConfig =
            toml::from_str("").expect("empty section must parse via defaults");
        assert!(!cfg.archive_enabled);
    }

    #[test]
    fn test_partition_retention_full_section_parses() {
        let cfg: PartitionRetentionConfig = toml::from_str(
            "retention_days = 90\nmarket_data_hot_days = 14\narchive_enabled = true\narchive_bucket = \"tv-prod-cold\"\nmax_partitions_per_run = 50\n",
        )
        .expect("full section must parse");
        assert!(cfg.archive_enabled);
        assert_eq!(cfg.archive_bucket, "tv-prod-cold");
        assert_eq!(cfg.market_data_hot_days, 14);
        assert_eq!(cfg.max_partitions_per_run, 50);
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
            subscription: SubscriptionConfig::default(),
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
            spot_1m_rest: Spot1mRestConfig::default(),
            option_chain_1m: OptionChain1mConfig::default(),
            groww_spot_1m: GrowwSpot1mConfig::default(),
            groww_option_chain_1m: GrowwOptionChain1mConfig::default(),
            groww_rest_burst: GrowwRestBurstConfig::default(),
            tf_consistency: TfConsistencyConfig::default(),
            groww_contract_1m: GrowwContract1mConfig::default(),
        }
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

    #[test]
    fn test_feed_mode_ticker_passes() {
        let config = SubscriptionConfig {
            feed_mode: "Ticker".to_string(),
            ..SubscriptionConfig::default()
        };
        assert!(config.parsed_feed_mode().is_ok());
    }

    #[test]
    fn test_feed_mode_quote_passes() {
        let config = SubscriptionConfig {
            feed_mode: "Quote".to_string(),
            ..SubscriptionConfig::default()
        };
        assert!(config.parsed_feed_mode().is_ok());
    }

    // AWS-lifecycle PR #7 Slice 1 — subscription.scope default is
    // Indices4Only (LOCKED scope, 4 IDX_I SIDs only).
    #[test]
    fn test_subscription_scope_default_is_indices4only() {
        let scope = SubscriptionScope::default();
        assert_eq!(scope, SubscriptionScope::Indices4Only);
        assert_eq!(scope.as_str(), "indices_4_only");
        let cfg = SubscriptionConfig::default();
        assert_eq!(cfg.scope, SubscriptionScope::Indices4Only);
    }

    // AWS-lifecycle PR #7 Slice 1 — `indices_4_only` round-trips via figment.
    #[test]
    fn test_indices4only_serde_roundtrip() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        let toml_indices4 = r#"
            [subscription]
            scope = "indices_4_only"
            feed_mode = "Ticker"
            subscribe_stock_equities = false
            stock_atm_strikes_above = 25
            stock_atm_strikes_below = 25
            stock_default_atm_fallback_enabled = true
        "#;
        #[derive(Deserialize)]
        struct Wrapper {
            subscription: SubscriptionConfig,
        }
        let wrapper: Wrapper = Figment::new()
            .merge(Toml::string(toml_indices4))
            .extract()
            .expect("indices_4_only scope must round-trip");
        assert_eq!(wrapper.subscription.scope, SubscriptionScope::Indices4Only);
        assert_eq!(wrapper.subscription.scope.as_str(), "indices_4_only");
    }

    // Sub-PR #1 of 2026-05-27 daily-universe expansion — the enum
    // grew from 1 to 2 variants. Adding/removing variants without
    // updating this test fails the build (match exhaustiveness).
    // See `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`.
    #[test]
    fn test_subscription_scope_has_exactly_two_variants() {
        // Compile-time guarantee: match must be exhaustive. If a
        // third variant is added or one is removed without updating
        // this test, the build fails.
        for s in [
            SubscriptionScope::Indices4Only,
            SubscriptionScope::DailyUniverse,
        ] {
            let label = match s {
                SubscriptionScope::Indices4Only => "indices_4_only",
                SubscriptionScope::DailyUniverse => "daily_universe",
            };
            assert_eq!(label, s.as_str());
        }
    }

    // Sub-PR #1 of 2026-05-27 — DailyUniverse variant exists and has
    // the stable wire-format label "daily_universe". Pinned so any
    // future rename forces a rule-file edit first.
    #[test]
    fn test_subscription_scope_daily_universe_label() {
        assert_eq!(SubscriptionScope::DailyUniverse.as_str(), "daily_universe");
    }

    // Sub-PR #1 of 2026-05-27 — `daily_universe` round-trips via figment.
    #[test]
    fn test_daily_universe_serde_roundtrip() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        let toml_daily = r#"
            [subscription]
            scope = "daily_universe"
            feed_mode = "Quote"
            subscribe_stock_equities = false
            stock_atm_strikes_above = 25
            stock_atm_strikes_below = 25
            stock_default_atm_fallback_enabled = true
        "#;
        #[derive(Deserialize)]
        struct Wrapper {
            subscription: SubscriptionConfig,
        }
        let wrapper: Wrapper = Figment::new()
            .merge(Toml::string(toml_daily))
            .extract()
            .expect("daily_universe scope must round-trip");
        assert_eq!(wrapper.subscription.scope, SubscriptionScope::DailyUniverse);
        assert_eq!(wrapper.subscription.scope.as_str(), "daily_universe");
    }

    // Sub-PR #1 of 2026-05-27 — default is STILL `Indices4Only` after
    // this PR. Activation of `DailyUniverse` as default lands later
    // once Sub-PRs #2-#13 wire the supporting code paths (CSV fetch,
    // lifecycle table, universe builder, etc.). This test fails if
    // someone flips the default prematurely.
    #[test]
    fn test_subscription_scope_default_still_indices4only_sub_pr_1() {
        assert_eq!(
            SubscriptionScope::default(),
            SubscriptionScope::Indices4Only
        );
    }

    // PR #7b — the 3 dead flags (subscribe_*_derivatives,
    // subscribe_display_indices) were retired. Trying to set them in
    // TOML must fail-loud (figment rejects unknown fields when the
    // deserializer is strict — here we just confirm the fields are
    // absent from the struct so the build of any old TOML test
    // expecting them is impossible).
    #[test]
    fn test_subscription_config_has_no_derivatives_flags() {
        let cfg = SubscriptionConfig::default();
        // Field-access-by-name on a non-existent field is a compile
        // error; this test is here to document the contract. Any
        // future addition of `subscribe_*_derivatives` or
        // `subscribe_display_indices` to SubscriptionConfig must
        // delete this test first, which forces a rule-file review.
        let _ = cfg.feed_mode;
        let _ = cfg.scope;
        let _ = cfg.subscribe_stock_equities;
    }

    // AWS-lifecycle PR #7 Slice 1 — Indices4Only pool size always 1
    // (4 SIDs fit on a single main-feed connection).
    #[test]
    fn test_effective_main_feed_pool_size_is_always_one_under_indices4only() {
        for configured in [0, 1, 2, 3, 4, 5, 10, 100] {
            assert_eq!(
                effective_main_feed_pool_size(SubscriptionScope::Indices4Only, configured),
                crate::constants::PHASE_0_MAIN_FEED_CONNECTION_COUNT,
                "Indices4Only must emit exactly {} main-feed conn regardless of configured={configured}",
                crate::constants::PHASE_0_MAIN_FEED_CONNECTION_COUNT,
            );
        }
    }

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

    #[test]
    fn test_feed_mode_full_passes() {
        let config = SubscriptionConfig {
            feed_mode: "Full".to_string(),
            ..SubscriptionConfig::default()
        };
        assert!(config.parsed_feed_mode().is_ok());
    }

    #[test]
    fn test_feed_mode_invalid_string_fails() {
        let config = SubscriptionConfig {
            feed_mode: "invalid".to_string(),
            ..SubscriptionConfig::default()
        };
        let err = config.parsed_feed_mode().unwrap_err();
        assert!(err.to_string().contains("Ticker/Quote/Full"));
    }

    #[test]
    fn test_feed_mode_case_sensitive() {
        let config = SubscriptionConfig {
            feed_mode: "ticker".to_string(), // lowercase — must fail
            ..SubscriptionConfig::default()
        };
        assert!(
            config.parsed_feed_mode().is_err(),
            "feed_mode is case-sensitive — 'ticker' should fail"
        );
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
    fn test_subscription_config_default() {
        let config = SubscriptionConfig::default();
        assert_eq!(config.feed_mode, "Full");
        assert!(config.subscribe_stock_equities);
        assert_eq!(config.stock_atm_strikes_above, 25);
        assert_eq!(config.stock_atm_strikes_below, 25);
        assert!(config.stock_default_atm_fallback_enabled);
    }

    #[test]
    fn test_default_allowed_origins() {
        let origins = default_allowed_origins();
        assert_eq!(origins.len(), 2);
        assert!(origins.contains(&"http://localhost:3000".to_string()));
        assert!(origins.contains(&"http://localhost:3001".to_string()));
    }

    #[test]
    fn test_feed_mode_empty_string_fails() {
        let config = SubscriptionConfig {
            feed_mode: String::new(),
            ..SubscriptionConfig::default()
        };
        assert!(config.parsed_feed_mode().is_err());
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
    fn test_sandbox_guard_blocks_live_before_july() {
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
    fn test_subscription_config_default_has_depth_disabled() {
        let config = SubscriptionConfig::default();
        assert!(!config.enable_twenty_depth);
    }

    #[test]
    fn test_subscription_config_default_depth_max_instruments() {
        let config = SubscriptionConfig::default();
        assert_eq!(config.twenty_depth_max_instruments, 49);
    }

    #[test]
    fn test_subscription_config_depth_max_instruments_matches_dhan_limit() {
        // Dhan docs: max 50 instruments per 20-level depth connection
        // We use 49 = ATM + 24 CE above + 24 PE below
        let config = SubscriptionConfig::default();
        assert!(config.twenty_depth_max_instruments <= 50);
    }

    #[test]
    fn test_subscription_config_all_fields_present() {
        let config = SubscriptionConfig::default();
        assert_eq!(config.feed_mode, "Full");
        assert!(config.subscribe_stock_equities);
        assert_eq!(config.stock_atm_strikes_above, 25);
        assert_eq!(config.stock_atm_strikes_below, 25);
        assert!(config.stock_default_atm_fallback_enabled);
        // Depth fields
        assert!(!config.enable_twenty_depth);
        assert_eq!(config.twenty_depth_max_instruments, 49);
    }

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
    fn test_default_twenty_depth_max_instruments_is_49() {
        // Covers the top-level `default_twenty_depth_max_instruments`
        // fn referenced via `#[serde(default = ...)]` — never called
        // directly in production, so we exercise it here. Per Dhan
        // 20-level limit (max 50/conn) our policy is ATM ± 24 = 49.
        assert_eq!(super::default_twenty_depth_max_instruments(), 49);
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

    /// PR-R1 (2026-07-04): the native-Rust shadow client is DEFAULT-OFF —
    /// both via `Default` and via a `[feeds]` TOML that omits the key
    /// (`#[serde(default)]`). Flipping the default requires a fresh dated
    /// operator quote per `groww-second-feed-scope-2026-06-19.md` §35.
    #[test]
    fn test_feeds_config_groww_native_shadow_defaults_off() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        assert!(
            !FeedsConfig::default().groww_native_shadow,
            "native shadow client must default OFF"
        );

        #[derive(Deserialize)]
        struct Wrapper {
            feeds: FeedsConfig,
        }
        let wrapper: Wrapper = Figment::new()
            .merge(Toml::string(
                "[feeds]\ndhan_enabled = true\ngroww_enabled = false\n",
            ))
            .extract()
            .expect("missing groww_native_shadow key must default, not error");
        assert!(!wrapper.feeds.groww_native_shadow);

        let wrapper_on: Wrapper = Figment::new()
            .merge(Toml::string(
                "[feeds]\ndhan_enabled = true\ngroww_enabled = false\ngroww_native_shadow = true\n",
            ))
            .extract()
            .expect("explicit groww_native_shadow must round-trip");
        assert!(wrapper_on.feeds.groww_native_shadow);
    }

    /// Disk-retention hardening (2026-07-13): the Groww capture-archive S3
    /// fields default EMPTY (= archival OFF, dev-Mac behaviour unchanged) —
    /// both via `Default` and via a TOML that omits the keys.
    #[test]
    fn test_feeds_groww_capture_archive_defaults_empty() {
        let tuning = GrowwFeedTuning::default();
        assert!(
            tuning.capture_archive_s3_bucket.is_empty(),
            "archive bucket must default empty (archival OFF)"
        );
        assert!(tuning.capture_archive_s3_prefix.is_empty());

        use figment::Figment;
        use figment::providers::{Format, Toml};
        #[derive(Deserialize)]
        struct Wrapper {
            feeds: FeedsConfig,
        }
        let wrapper: Wrapper = Figment::new()
            .merge(Toml::string(
                "[feeds]\ndhan_enabled = true\ngroww_enabled = false\n",
            ))
            .extract()
            .expect("missing capture_archive keys must default, not error");
        assert!(wrapper.feeds.groww.capture_archive_s3_bucket.is_empty());
        assert!(wrapper.feeds.groww.capture_archive_s3_prefix.is_empty());
    }

    /// Disk-retention hardening (2026-07-13): explicit `[feeds.groww]`
    /// capture-archive keys round-trip.
    #[test]
    fn test_feeds_groww_capture_archive_round_trip() {
        use figment::Figment;
        use figment::providers::{Format, Toml};
        #[derive(Deserialize)]
        struct Wrapper {
            feeds: FeedsConfig,
        }
        let wrapper: Wrapper = Figment::new()
            .merge(Toml::string(
                "[feeds]\ndhan_enabled = true\ngroww_enabled = false\n\
                 [feeds.groww]\ncapture_archive_s3_bucket = \"tv-prod-cold\"\n\
                 capture_archive_s3_prefix = \"groww-capture\"\n",
            ))
            .extract()
            .expect("explicit capture_archive keys must round-trip");
        assert_eq!(
            wrapper.feeds.groww.capture_archive_s3_bucket,
            "tv-prod-cold"
        );
        assert_eq!(
            wrapper.feeds.groww.capture_archive_s3_prefix,
            "groww-capture"
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

    /// Per-minute spot 1m REST pipeline (operator grant 2026-07-12): the
    /// `[spot_1m_rest]` section is FAIL-SAFE default OFF — via `Default`,
    /// via a missing section, and via an empty section — and an explicit
    /// `enabled = true` (the `config/base.toml` shape) must round-trip.
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

    // =======================================================================
    // Groww multi-connection auto-scale (§34, operator authorization
    // 2026-07-03) — `[feeds.groww.scale]` contract tests.
    // See `.claude/rules/project/groww-scale-error-codes.md` +
    // `.claude/plans/active-plan-groww-autoscale.md`.
    // =======================================================================

    /// RATCHET: the scale defaults MUST be OFF + Tier A. Flipping
    /// `enabled` to true by default, or raising the default target above
    /// the Tier A ceiling, requires a fresh dated operator quote (§34.4).
    #[test]
    fn test_groww_scale_defaults_are_off_and_tier_a() {
        let scale = GrowwScaleConfig::default();
        assert!(!scale.enabled, "scale must default OFF (single-conn path)");
        assert_eq!(scale.target_connections, GROWW_SCALE_TIER_A_MAX_CONNS);
        assert_eq!(
            scale.instruments_per_conn,
            GROWW_SCALE_MAX_INSTRUMENTS_PER_CONN
        );
        assert_eq!(scale.ladder, vec![1, 2, 5, 10], "day-1 rungs per design");
        assert_eq!(scale.gate_hold_minutes, 15);
        assert!((scale.gate_max_cpu_pct - 70.0).abs() < f64::EPSILON);
        assert!((scale.gate_min_disk_free_pct - 20.0).abs() < f64::EPSILON);
        assert_eq!(scale.gate_max_capture_lag_ms, 30_000);
        assert_eq!(scale.rollback_hold_base_minutes, 10);
        assert_eq!(scale.advance_window_ist[0], "09:20");
        assert_eq!(scale.advance_window_ist[1], "14:30");
        assert!(!scale.probe_mode, "probe_mode must default OFF");
        assert!(!scale.weekend_smoke, "weekend_smoke must default OFF");
        // Tier ordering sanity: A < B < hard max.
        assert!(GROWW_SCALE_TIER_A_MAX_CONNS < GROWW_SCALE_TIER_B_MAX_CONNS);
        assert!(GROWW_SCALE_TIER_B_MAX_CONNS < GROWW_SCALE_HARD_MAX_CONNS);
    }

    /// PR-3: `probe_mode` + `weekend_smoke` round-trip via figment TOML.
    #[test]
    fn test_groww_scale_probe_mode_and_weekend_smoke_parse_from_toml() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            feeds: FeedsConfig,
        }
        let toml = concat!(
            "[feeds]
",
            "dhan_enabled = false
",
            "groww_enabled = true
",
            "[feeds.groww.scale]
",
            "enabled = true
",
            "probe_mode = true
",
            "weekend_smoke = true
",
        );
        let wrapper: Wrapper = Figment::new()
            .merge(Toml::string(toml))
            .extract()
            .expect("probe/smoke TOML must parse");
        assert!(wrapper.feeds.groww.scale.probe_mode);
        assert!(wrapper.feeds.groww.scale.weekend_smoke);
        // A probe/smoke config is still a VALID envelope (booleans never
        // break the ladder-bound validation).
        assert!(wrapper.feeds.groww.scale.validate().is_ok());
    }

    /// `[feeds.groww.scale]` parses from TOML (partial keys allowed —
    /// unspecified fields fall back to defaults).
    #[test]
    fn test_groww_scale_parses_from_toml() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            feeds: FeedsConfig,
        }
        let toml = concat!(
            "[feeds]\n",
            "dhan_enabled = false\n",
            "groww_enabled = true\n",
            "\n",
            "[feeds.groww.scale]\n",
            "enabled = true\n",
            "target_connections = 4\n",
            "instruments_per_conn = 500\n",
            "ladder = [1, 2, 4]\n",
        );
        let wrapper: Wrapper = Figment::new()
            .merge(Toml::string(toml))
            .extract()
            .expect("[feeds.groww.scale] must parse");
        let scale = &wrapper.feeds.groww.scale;
        assert!(scale.enabled);
        assert_eq!(scale.target_connections, 4);
        assert_eq!(scale.instruments_per_conn, 500);
        assert_eq!(scale.ladder, vec![1, 2, 4]);
        // Unspecified keys fall back to defaults.
        assert_eq!(scale.gate_hold_minutes, 15);
        assert_eq!(scale.advance_window_ist[0], "09:20");
        assert!(scale.validate().is_ok());
    }

    /// A missing `[feeds.groww]` / `[feeds.groww.scale]` sub-table must
    /// fall back to the safe OFF default — never error (byte-identical
    /// single-conn behaviour).
    #[test]
    fn test_groww_scale_missing_section_defaults() {
        use figment::Figment;
        use figment::providers::{Format, Toml};

        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            feeds: FeedsConfig,
        }
        let wrapper: Wrapper = Figment::new()
            .merge(Toml::string(
                "[feeds]\ndhan_enabled = true\ngroww_enabled = false\n",
            ))
            .extract()
            .expect("missing [feeds.groww.scale] must use defaults, not error");
        assert!(!wrapper.feeds.groww.scale.enabled);
        assert_eq!(
            wrapper.feeds.groww.scale.target_connections,
            GROWW_SCALE_TIER_A_MAX_CONNS
        );
    }

    /// The shipped defaults must validate clean (a fresh deployment can
    /// never fail boot on the scale section).
    #[test]
    fn test_groww_scale_validate_accepts_defaults() {
        assert!(GrowwScaleConfig::default().validate().is_ok());
    }

    /// FINANCIAL/ENVELOPE BOUNDARY: per-conn cap 0 and >1000 (the Groww
    /// per-session hard cap) are both rejected; exactly 1000 is accepted.
    #[test]
    fn test_groww_scale_validate_rejects_zero_per_conn() {
        let per_conn = |n: usize| GrowwScaleConfig {
            instruments_per_conn: n,
            ..Default::default()
        };
        assert!(
            per_conn(0).validate().is_err(),
            "0 per-conn must be rejected"
        );
        assert!(
            per_conn(GROWW_SCALE_MAX_INSTRUMENTS_PER_CONN + 1)
                .validate()
                .is_err(),
            "1001 per-conn must be rejected"
        );
        assert!(
            per_conn(GROWW_SCALE_MAX_INSTRUMENTS_PER_CONN)
                .validate()
                .is_ok(),
            "exactly 1000 is the boundary-OK"
        );
    }

    /// ENVELOPE BOUNDARY: target_connections 0 and >100 (Tier C hard max)
    /// are rejected; exactly 100 passes the config envelope (tier
    /// EVIDENCE is a review-time gate per §34.2, not a config check).
    #[test]
    fn test_groww_scale_validate_rejects_over_hard_max() {
        let mut scale = GrowwScaleConfig {
            target_connections: 0,
            ..Default::default()
        };
        assert!(scale.validate().is_err(), "0 target must be rejected");
        scale.target_connections = GROWW_SCALE_HARD_MAX_CONNS + 1;
        assert!(scale.validate().is_err(), "101 target must be rejected");
        scale.target_connections = GROWW_SCALE_HARD_MAX_CONNS;
        assert!(scale.validate().is_ok(), "exactly 100 is the boundary-OK");
    }

    /// The ladder must be strictly increasing and non-empty; rung 0 is
    /// rejected.
    #[test]
    fn test_groww_scale_validate_rejects_non_increasing_ladder() {
        let mut scale = GrowwScaleConfig {
            ladder: vec![10, 5],
            ..Default::default()
        };
        assert!(scale.validate().is_err(), "decreasing ladder rejected");
        scale.ladder = vec![1, 1, 2];
        assert!(scale.validate().is_err(), "duplicate rung rejected");
        scale.ladder = vec![];
        assert!(scale.validate().is_err(), "empty ladder rejected");
        scale.ladder = vec![0, 1];
        assert!(scale.validate().is_err(), "rung 0 rejected");
        scale.ladder = vec![1, 2, 5, 10];
        assert!(scale.validate().is_ok());
    }

    /// The last rung must not exceed target_connections (the ladder can
    /// never climb past its own ceiling).
    #[test]
    fn test_groww_scale_validate_rejects_ladder_above_target() {
        let mut scale = GrowwScaleConfig {
            target_connections: 5,
            ladder: vec![1, 2, 5, 10],
            ..Default::default()
        };
        assert!(scale.validate().is_err(), "rung 10 > target 5 rejected");
        scale.ladder = vec![1, 2, 5];
        assert!(
            scale.validate().is_ok(),
            "rung == target is the boundary-OK"
        );
    }

    /// The advance window must be two valid HH:MM strings with start < end.
    #[test]
    fn test_groww_scale_validate_rejects_bad_window() {
        let mut scale = GrowwScaleConfig {
            advance_window_ist: [String::from("nine-ish"), String::from("14:30")],
            ..Default::default()
        };
        assert!(scale.validate().is_err(), "non-time string rejected");
        scale.advance_window_ist = [String::from("25:00"), String::from("26:00")];
        assert!(scale.validate().is_err(), "hour 25 rejected");
        scale.advance_window_ist = [String::from("14:30"), String::from("09:20")];
        assert!(scale.validate().is_err(), "inverted window rejected");
        scale.advance_window_ist = [String::from("09:20"), String::from("09:20")];
        assert!(scale.validate().is_err(), "zero-width window rejected");
        scale.advance_window_ist = [String::from("09:20"), String::from("14:30")];
        assert!(scale.validate().is_ok());
    }
}
