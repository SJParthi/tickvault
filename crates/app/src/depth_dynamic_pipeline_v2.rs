//! 2026-05-02 PR-C — Unified all-5-dynamic depth pipeline (redesign).
//!
//! Replaces Wave 5's "4 pinned + 1 dynamic" depth-20 allocator and the
//! Wave 5 depth-200 pool with a single shared spawner. Both depth-20
//! and depth-200 use the SAME spawner, parameterised by `PoolShape`:
//!
//! - **depth-20**: `PoolShape { conns=5, sids_per_conn=50 }` → 250 SIDs
//!   total dynamically chosen from `movers_1m` top-volume cohort,
//!   re-ranked by `change_pct DESC`. Configured via
//!   `[depth_20.dynamic]` in `config/base.toml`.
//! - **depth-200**: `PoolShape { conns=5, sids_per_conn=1 }` → 5 SIDs
//!   total. Same selector module, K=5.
//!
//! ## Pipeline shape
//!
//! ```
//!  ┌──────────────────┐  60s tick  ┌────────────────────────┐
//!  │ Tokio scheduler  ├───────────▶│ Stage 1: SQL cohort    │
//!  └──────────────────┘            │ FROM movers_1m WHERE … │
//!                                  └─────────┬──────────────┘
//!                                            ▼
//!                                  ┌─────────────────────────┐
//!                                  │ Stage 2: Rust re-rank   │
//!                                  │ select_top_k_dynamic    │
//!                                  └─────────┬───────────────┘
//!                                            ▼ Vec<SubKey>
//!                                  ┌─────────────────────────┐
//!                                  │ DynamicSubscriptionState│
//!                                  │ ::diff(next_set)        │
//!                                  └─────────┬───────────────┘
//!                                            ▼ Vec<DiffOp>
//!                                  ┌─────────────────────────┐
//!                                  │ dispatch_ops →          │
//!                                  │ DepthCommand::*         │
//!                                  │ → mpsc::Sender per conn │
//!                                  └─────────────────────────┘
//! ```
//!
//! ## Honest envelope per per-wave-guarantee-matrix.md §8
//!
//! - **O(1) latency:** the diff is `O(N + K)` per cycle on cold path
//!   (60s cadence). No tick-processor hot-path impact.
//! - **Uniqueness:** composite `(security_id, ExchangeSegment)` key per
//!   I-P1-11. Selector + state both use `SubKey`.
//! - **Zero disconnect:** Add/Remove commands are RequestCode 23/25 on
//!   the existing socket. Never disconnects, never reconnects.
//! - **Edge-triggered:** identical rank-sets → `DiffStats::is_no_op()`
//!   → zero wire traffic, zero audit churn.
//! - **Market-hours gated:** outside [09:00, 15:30) IST → tick handler
//!   skips; selector never queries an empty post-market window.
//!
//! ## Test coverage
//!
//! Pure-logic primitives (selector + diff state + DepthCommand
//! variants) live in `crates/core` with 66 ratchet tests (see PR-B
//! commits). This module is the orchestrator/glue layer that spawns
//! the tokio task; its internal helpers (`fetch_cohort_from_questdb`,
//! `dispatch_ops`, `build_remove_message_*`, `build_add_message_*`,
//! `is_within_market_hours_ist`) are unit-tested in this file.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    IST_UTC_OFFSET_NANOS, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY,
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_common::types::ExchangeSegment;
use tickvault_core::instrument::depth_dynamic_top_volume_selector::{
    MAX_COHORT_SIZE, MoverRow, SelectorConfig, build_cohort_sql, select_top_k_dynamic,
};
use tickvault_core::instrument::dynamic_subscription_state::{
    ConnIdx, DiffOp, DiffStats, DynamicSubscriptionState, PoolShape,
};
use tickvault_core::websocket::DepthCommand;
use tickvault_storage::depth_dynamic_diff_audit_persistence::append_depth_dynamic_diff_audit_row;

/// Re-selection cadence — pinned at 60s per the operator spec
/// (matches the legacy v1 pipeline so dashboards/alerts unchanged).
const RESELECT_INTERVAL_SECS: u64 = 60;

/// HTTP timeout for the QuestDB selector query.
const QUESTDB_SELECT_TIMEOUT_SECS: u64 = 10;

/// Audit-2026-05-03: default minimum cumulative session volume floor
/// for the redesigned Stage 1 liquidity gate. Replaces the legacy
/// `DEFAULT_COHORT_SIZE = 500`. Operators calibrate per-environment;
/// 10_000 is a defensive floor that excludes truly illiquid contracts
/// while admitting the typical NIFTY/BANKNIFTY option chain volumes.
pub const DEFAULT_MIN_LIQUIDITY_VOLUME: u64 = 10_000;

/// Default SQL freshness window if config does not set one. Filters
/// stale post-market data.
pub const DEFAULT_FRESHNESS_WINDOW_SECS: u32 = 60;

/// Pipeline configuration. Constructed at boot from
/// `config/base.toml::[depth_20.dynamic]` or
/// `config/base.toml::[depth_200.dynamic]`.
// Note: no `#[derive(Debug)]` because `NotificationService` does not
// implement Debug. Custom Debug impl below skips the notifier + registry.
#[derive(Clone)]
pub struct PipelineConfig {
    /// QuestDB connection details (HTTP /exec endpoint).
    pub questdb: QuestDbConfig,
    /// Selector parameters (instrument_types + exchange_segments + k).
    pub selector: SelectorConfig,
    /// Pool shape (conns + sids_per_conn). Caps the diff state's
    /// total capacity = `conns * sids_per_conn`.
    pub shape: PoolShape,
    /// Human-readable label for log lines and metric labels.
    /// Conventional values: `"depth-20-dynamic"` and
    /// `"depth-200-dynamic"`.
    pub label: &'static str,
    /// Audit-2026-05-03 (operator clarification): liquidity gate for
    /// Stage 1 SQL. Replaces the legacy `cohort_size` top-N parameter.
    /// Only contracts with `volume >= min_liquidity_volume` qualify
    /// for Stage 2 ranking — guarantees liquidity for live-trade
    /// depth subscriptions. Threaded from
    /// `[depth_*.dynamic.universe].min_liquidity_volume`.
    pub min_liquidity_volume: u64,
    /// SQL freshness window. Threaded from
    /// `[depth_*.dynamic.universe].window_secs`.
    pub window_secs: u32,
    /// 2026-05-02 — operator-requested symbol resolution for diff events.
    /// When `Some(_)`, every non-no-op cycle resolves added/removed SIDs
    /// to display labels via O(1) `get_with_segment` lookup (per I-P1-11).
    /// `None` keeps the legacy aggregate-counts-only behaviour for tests.
    pub registry: Option<Arc<tickvault_common::instrument_registry::InstrumentRegistry>>,
    /// Optional notifier for the diff Telegram event. Both `registry`
    /// and `notifier` must be `Some(_)` for the event to fire.
    pub notifier: Option<Arc<tickvault_core::notification::NotificationService>>,
    /// 2026-05-09 PR 5c.2 — optional in-RAM `CascadeFanout` reference.
    /// When `Some(_)` AND `registry` is also `Some(_)`, the cohort
    /// fetcher reads bars directly from the in-RAM cascade (via
    /// `fetch_cohort_from_ram`) instead of issuing the SQL query
    /// against the `movers_1m` matview. Falls back to the legacy SQL
    /// path when this is `None` (test fixtures, dormant deployments).
    /// Set to `Some(_)` from `main.rs` once `CascadeFanout` is
    /// initialised; the writer + matview chain can then be retired
    /// in a follow-up PR.
    pub cascade: Option<Arc<tickvault_trading::candles::CascadeFanout>>,
}

/// Spawns the unified all-5-dynamic depth pipeline. Drives the diff
/// state machine + selector + per-conn DepthCommand dispatch.
///
/// `cmd_senders` maps `ConnIdx (0..conns)` to the depth-connection
/// task's mpsc receiver. Caller (boot path) registers these senders
/// when spawning each depth WS connection.
///
/// Returns the JoinHandle for the supervisor to keep alive.
// TEST-EXEMPT: orchestrator spawner; internal helpers unit-tested below.
pub fn spawn_depth_dynamic_pool(
    cfg: PipelineConfig,
    cmd_senders: HashMap<ConnIdx, mpsc::Sender<DepthCommand>>,
    shutdown: Arc<Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        info!(label = cfg.label, "depth_dynamic_pool_v2 starting");
        let mut state = DynamicSubscriptionState::new(cfg.shape);
        // 2026-05-02 — operator-requested wall-clock alignment. Replace
        // boot-aligned `tokio::time::interval(60s)` with `interval_at`
        // anchored to today's 09:15:00 IST (or "now" if already past).
        // Without this fix, a boot at 08:50:30 IST would tick at
        // 08:51:30, 08:52:30, …, 09:14:30, then 09:15:30 — missing the
        // critical 09:15:00→09:15:30 window where movers_1s first
        // populates. Operator's spec: "at 09:15:00 it should be
        // captured and started — should not wait till 09:16".
        let first_cycle_at = compute_first_cycle_instant_today();
        let mut tick =
            tokio::time::interval_at(first_cycle_at, Duration::from_secs(RESELECT_INTERVAL_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // PR-C2 follow-up (hostile-bug-hunt fix C2/M1) — rising-edge
        // failure gates. Each error emits ERROR (Telegram) ONLY on the
        // rising edge; sustained failure logs at WARN (no Telegram);
        // recovery emits INFO. Audit-findings 2026-04-17 Rule 4.
        let mut cohort_failing = false;
        let mut diff_failing = false;
        let mut audit_failing = false;

        // PR-C2 follow-up (hostile-bug-hunt fix H3) — positive heartbeat
        // signal. Fires ONCE on the first non-no-op diff cycle so the
        // operator sees a green confirmation that v2 is producing data.
        // Audit-findings Rule 11: every error has a positive counterpart.
        let mut emitted_first_heartbeat = false;

        // PR-C2 follow-up (hostile-bug-hunt fix H2) — daily reset gate.
        // Tracks the most recent IST trading day observed; on rollover
        // (00:00 IST), resets `state` so cross-day rank shifts compute
        // against a clean baseline. The diff algorithm correctly handles
        // stale state (large-but-bounded delta) so this is hygienic, not
        // safety-critical, but avoids spurious 250-op churn at 09:00.
        let mut last_seen_ist_day = current_ist_trading_day();

        // 2026-05-02 — operator-requested two-phase cadence. First tick
        // of each trading day fires at 09:15:00 IST (initial subscribe);
        // every subsequent tick fires at the next :00 minute boundary
        // (09:16:00, 09:17:00, …). `realigned_to_minute` flips true
        // after the first in-market cycle dispatches, triggering the
        // interval reset to the minute-boundary schedule. Reset to
        // false on daily-rollover so tomorrow's 09:15:00 fires the
        // special first cycle again.
        let mut realigned_to_minute = false;

        // PR-C2 follow-up (hostile-bug-hunt fix L1) — off-hours sleep
        // edge-trigger. Tracks the in-market state to fire a single INFO
        // when the loop enters off-hours sleep, and a single INFO when
        // it wakes back up. Avoids "is the pipeline asleep or stuck?"
        // ambiguity for the operator. Init: assume in-market until
        // first tick proves otherwise.
        let mut was_in_market = true;

        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    info!(label = cfg.label, "depth_dynamic_pool_v2 shutdown notified");
                    break;
                }
                _ = tick.tick() => {
                    let in_market = is_within_market_hours_ist();
                    if in_market != was_in_market {
                        if in_market {
                            info!(
                                code = "DEPTH-DYN-V2-WAKE",
                                label = cfg.label,
                                "depth_dynamic_pool_v2 entered market hours — resuming 60s diff cycles"
                            );
                        } else {
                            info!(
                                code = "DEPTH-DYN-V2-SLEEP",
                                label = cfg.label,
                                "depth_dynamic_pool_v2 exited market hours — sleeping until 09:00 IST"
                            );
                        }
                        was_in_market = in_market;
                    }
                    if !in_market {
                        continue;
                    }
                    // 2026-05-02 — heartbeat counter for liveness alerting.
                    // Increments on EVERY in-market tick (including no-op
                    // cycles where rank-set is unchanged). Distinct from
                    // `tv_depth_dynamic_diff_cycles_total` which only fires
                    // on non-no-op cycles. Alert
                    // `tv-depth-dynamic-pipeline-stalled` fires if this
                    // counter doesn't increment for 5+ min during market
                    // hours — meaning the orchestrator is dead.
                    metrics::counter!(
                        "tv_depth_dynamic_heartbeat_total",
                        "feed" => cfg.label
                    )
                    .increment(1);
                    // PR-C2 follow-up H2: detect IST midnight rollover and
                    // reset state so the next diff computes against a clean
                    // baseline. Cheap (one i64 compare per 60s tick).
                    let today_ist = current_ist_trading_day();
                    if today_ist != last_seen_ist_day {
                        info!(
                            label = cfg.label,
                            previous = last_seen_ist_day,
                            current = today_ist,
                            "depth_dynamic_pool_v2 IST midnight rollover — resetting diff state \
                             + re-aligning interval to today's 09:15:00"
                        );
                        state = DynamicSubscriptionState::new(cfg.shape);
                        last_seen_ist_day = today_ist;
                        // 2026-05-02 — re-align the interval to today's
                        // 09:15:00 IST so the first cycle each day fires
                        // exactly at market open + 1s, not boot-time-aligned.
                        // Without this, the schedule drifts off the wall
                        // clock day-by-day (interval ticks every 60s
                        // forever from the original start_at instant).
                        tick = tokio::time::interval_at(
                            compute_first_cycle_instant_today(),
                            Duration::from_secs(RESELECT_INTERVAL_SECS),
                        );
                        tick.set_missed_tick_behavior(
                            tokio::time::MissedTickBehavior::Delay,
                        );
                        // Reset the minute-boundary realignment flag so
                        // tomorrow's first cycle fires at the special
                        // 09:15:00 anchor, then re-aligns to :00 boundaries.
                        realigned_to_minute = false;
                        // Skip the rest of this cycle — next tick.tick()
                        // will fire at today's 09:15:00.
                        continue;
                    }
                    // 2026-05-09 PR 5c.2 — RAM-first cohort fetch. Reads
                    // top-volume cohort from the in-RAM CascadeFanout
                    // when `cascade` is wired (see `PipelineConfig`); the
                    // SQL fallback applies when (a) cascade is None or
                    // (b) RAM returns an empty cohort (early boot before
                    // the cascade is primed). The fallback preserves
                    // production safety while we soak the RAM path.
                    let ram_cohort: Option<Vec<MoverRow>> =
                        if let (Some(cascade), Some(reg)) =
                            (cfg.cascade.as_ref(), cfg.registry.as_ref())
                        {
                            let bars = cascade.snapshot_1m();
                            let rows = fetch_cohort_from_ram(
                                &bars,
                                reg.as_ref(),
                                &cfg.selector.exchange_segments,
                                &cfg.selector.instrument_types,
                                cfg.min_liquidity_volume,
                                MAX_COHORT_SIZE,
                            );
                            if rows.is_empty() {
                                None
                            } else {
                                Some(rows)
                            }
                        } else {
                            None
                        };
                    let cohort = if let Some(rows) = ram_cohort {
                        if cohort_failing {
                            info!(
                                label = cfg.label,
                                "depth_dynamic_pool_v2 cohort fetch RECOVERED (via RAM)"
                            );
                            cohort_failing = false;
                        }
                        // 2026-05-09 PR 5c.2.5 — observability: emit a
                        // counter labelled by source so the operator
                        // can verify in Grafana that RAM is producing
                        // healthy cohorts (vs falling through to SQL).
                        // Once `ram_total >> sql_total` over a soak
                        // window, PR 5c.3 can safely delete the SQL
                        // fallback + writer infrastructure.
                        metrics::counter!(
                            "tv_depth_dynamic_cohort_source_total",
                            "label" => cfg.label,
                            "source" => "ram",
                        )
                        .increment(1);
                        rows
                    } else {
                    metrics::counter!(
                        "tv_depth_dynamic_cohort_source_total",
                        "label" => cfg.label,
                        "source" => "sql",
                    )
                    .increment(1);
                    match fetch_cohort_from_questdb(&cfg).await {
                        Ok(c) => {
                            if cohort_failing {
                                info!(
                                    label = cfg.label,
                                    "depth_dynamic_pool_v2 cohort fetch RECOVERED"
                                );
                                cohort_failing = false;
                            }
                            c
                        }
                        Err(err) => {
                            if cohort_failing {
                                warn!(
                                    label = cfg.label,
                                    ?err,
                                    "depth_dynamic_pool_v2 cohort fetch still failing (rising-edge already fired)"
                                );
                            } else {
                                error!(
                                    code = "DEPTH-DYN-V2-01",
                                    label = cfg.label,
                                    ?err,
                                    "depth_dynamic_pool_v2 cohort fetch failed"
                                );
                                cohort_failing = true;
                            }
                            continue;
                        }
                    }
                    };
                    let next_set = select_top_k_dynamic(&cohort, &cfg.selector);
                    if next_set.is_empty() {
                        warn!(
                            label = cfg.label,
                            cohort_size = cohort.len(),
                            "depth_dynamic_pool_v2 selector returned empty next_set — keeping previous"
                        );
                        continue;
                    }
                    let (ops, stats) = match state.diff(&next_set) {
                        Ok(r) => {
                            if diff_failing {
                                info!(
                                    label = cfg.label,
                                    "depth_dynamic_pool_v2 diff RECOVERED"
                                );
                                diff_failing = false;
                            }
                            r
                        }
                        Err(err) => {
                            if diff_failing {
                                warn!(
                                    label = cfg.label,
                                    ?err,
                                    "depth_dynamic_pool_v2 diff still failing (rising-edge already fired)"
                                );
                            } else {
                                error!(
                                    code = "DEPTH-DYN-V2-02",
                                    label = cfg.label,
                                    ?err,
                                    "depth_dynamic_pool_v2 diff failed (over capacity?)"
                                );
                                diff_failing = true;
                            }
                            continue;
                        }
                    };
                    if stats.is_no_op() {
                        // Edge-triggered: identical rank-sets produce zero work.
                        continue;
                    }
                    info!(
                        label = cfg.label,
                        added = stats.added,
                        removed = stats.removed,
                        retained = stats.retained,
                        "depth_dynamic_pool_v2 dispatching diff ops"
                    );

                    // PR-C2 follow-up H3: positive heartbeat — fires ONCE on
                    // the first non-no-op cycle. Operator's "is v2 working?"
                    // question now has a green confirmation, not just the
                    // absence of red errors.
                    if !emitted_first_heartbeat {
                        info!(
                            code = "DEPTH-DYN-V2-LIVE",
                            label = cfg.label,
                            initial_set_size = stats.added,
                            "depth_dynamic_pool_v2 first non-no-op cycle — pipeline confirmed live"
                        );
                        emitted_first_heartbeat = true;
                    }

                    // PR-D: emit Prom counters BEFORE dispatch so the
                    // gauge value is visible to alert rules even if a
                    // dispatch send subsequently fails.
                    emit_diff_metrics(cfg.label, &stats);

                    dispatch_ops(cfg.label, &ops, &cmd_senders, cfg.shape.sids_per_conn).await;

                    // PR-D: persist one audit row per non-no-op cycle.
                    match persist_diff_audit(&cfg, &ops, &stats).await {
                        Ok(()) => {
                            if audit_failing {
                                info!(
                                    label = cfg.label,
                                    "depth_dynamic_pool_v2 audit persist RECOVERED"
                                );
                                audit_failing = false;
                            }
                        }
                        Err(err) => {
                            if audit_failing {
                                warn!(
                                    label = cfg.label,
                                    ?err,
                                    "depth_dynamic_pool_v2 audit persist still failing (rising-edge already fired)"
                                );
                            } else {
                                error!(
                                    code = "DEPTH-DYN-V2-AUDIT-01",
                                    label = cfg.label,
                                    ?err,
                                    "depth_dynamic_pool_v2 audit row persist failed"
                                );
                                audit_failing = true;
                            }
                        }
                    }

                    // 2026-05-02 — operator-requested symbol-level Telegram.
                    // Resolve each Add/Remove op's (sid, segment) →
                    // display_label via O(1) registry lookup (per I-P1-11),
                    // build entries, fire DepthDynamicV2DiffApplied. If
                    // either registry or notifier is None (e.g. test
                    // fixtures), skip silently.
                    if let (Some(registry), Some(notifier)) =
                        (cfg.registry.as_ref(), cfg.notifier.as_ref())
                    {
                        let added_entries =
                            resolve_op_entries(&ops, registry, |op| {
                                matches!(op, DiffOp::Add { .. })
                            });
                        let removed_entries =
                            resolve_op_entries(&ops, registry, |op| {
                                matches!(op, DiffOp::Remove { .. })
                            });
                        // 2026-05-02 hostile-bug-hunt HIGH fix: surface
                        // unresolved drops as an explicit metric so a stale
                        // registry doesn't silently mask diff churn.
                        let unresolved =
                            (stats.added + stats.removed)
                                .saturating_sub(added_entries.len() + removed_entries.len());
                        if unresolved > 0 {
                            metrics::counter!(
                                "tv_depth_dynamic_unresolved_lookups_total",
                                "feed" => cfg.label
                            )
                            .increment(unresolved as u64);
                            warn!(
                                code = "DEPTH-DYN-V2-UNRESOLVED",
                                label = cfg.label,
                                unresolved,
                                stats_added = stats.added,
                                stats_removed = stats.removed,
                                resolved_added = added_entries.len(),
                                resolved_removed = removed_entries.len(),
                                "registry lookup dropped diff entries — Telegram counts \
                                 use stats; investigate if recurrent (stale registry?)"
                            );
                        }
                        notifier.notify(
                            tickvault_core::notification::NotificationEvent::DepthDynamicV2DiffApplied {
                                feed: cfg.label,
                                added: added_entries,
                                removed: removed_entries,
                                retained_count: stats.retained,
                                stats_added: stats.added,
                                stats_removed: stats.removed,
                            },
                        );
                    }

                    // 2026-05-02 — operator-requested cadence transition.
                    // After the FIRST in-market cycle dispatches (at
                    // 09:15:00), re-anchor the interval to the next
                    // wall-clock minute boundary so subsequent cycles
                    // fire at 09:16:00, 09:17:00, … instead of
                    // 09:16:01, 09:17:01, … (the natural drift of
                    // interval_at(09:15:00, 60s)).
                    if !realigned_to_minute {
                        let next_minute = compute_next_minute_boundary_instant();
                        info!(
                            code = "DEPTH-DYN-V2-CADENCE",
                            label = cfg.label,
                            "first cycle complete — re-anchoring to minute-boundary cadence"
                        );
                        tick = tokio::time::interval_at(
                            next_minute,
                            Duration::from_secs(RESELECT_INTERVAL_SECS),
                        );
                        tick.set_missed_tick_behavior(
                            tokio::time::MissedTickBehavior::Delay,
                        );
                        realigned_to_minute = true;
                    }
                }
            }
        }
        info!(label = cfg.label, "depth_dynamic_pool_v2 exited");
    })
}

/// Stage-1 fetcher: queries `movers_1m` for the top-volume cohort.
///
/// Returns `Vec<MoverRow>` ordered by volume DESC (Stage-2 re-ranks
/// by change_pct DESC).
async fn fetch_cohort_from_questdb(cfg: &PipelineConfig) -> anyhow::Result<Vec<MoverRow>> {
    let sql = build_cohort_sql(
        &cfg.selector.exchange_segments,
        &cfg.selector.instrument_types,
        cfg.min_liquidity_volume,
        cfg.window_secs,
    );
    let url = format!("http://{}:{}/exec", cfg.questdb.host, cfg.questdb.http_port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(QUESTDB_SELECT_TIMEOUT_SECS))
        .build()?;
    let resp = client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("QuestDB cohort selector non-2xx: {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    let dataset = json
        .get("dataset")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow::anyhow!("QuestDB response missing dataset"))?;

    parse_cohort_dataset(dataset)
}

/// 2026-05-09 PR 5c.1 — RAM-backed Stage-1 cohort fetcher (additive,
/// not yet wired). Replaces the QuestDB SQL path against `movers_1m`
/// with a direct read over an in-RAM `Vec<Bar>` slice (typically the
/// caller's `CascadeFanout::snapshot_1m()` output).
///
/// **Why a separate function (Option B per `active-plan-movers-cleanup-5b-5c-5d.md`):**
/// `Bar` carries `security_id` + `exchange_segment_code` but NOT
/// `instrument_type`. The `instrument_type` filter is essential —
/// today's default config restricts the cohort to `["OPTIDX"]`
/// (index-option contracts only). To preserve that semantic without
/// expanding `Bar` (which would touch the cascade hot path), this
/// function performs a per-row `InstrumentRegistry::get_with_segment`
/// lookup. The lookup is O(1) via the registry's `papaya` map and the
/// cohort caps at `MAX_COHORT_SIZE` rows, so total cost stays bounded.
///
/// **Equivalence to `build_cohort_sql`:** same filter set in the same
/// order:
/// 1. Time window — N/A in RAM (caller passes a fresh snapshot).
/// 2. `exchange_segment` filter — applied via `Bar.exchange_segment_code`.
/// 3. `instrument_type` filter — applied via registry lookup.
/// 4. `volume_cumulative >= min_liquidity_volume` — applied via
///    `Bar.volume`.
/// 5. ORDER BY `volume_cumulative` DESC, LIMIT `MAX_COHORT_SIZE` —
///    applied as in-place sort + truncate.
///
/// **Returns** `Vec<MoverRow>` shape-identical to
/// `fetch_cohort_from_questdb` so Stage-2 (`select_top_k_dynamic`)
/// is unchanged.
///
/// **Status:** dormant — not yet called from
/// `run_depth_dynamic_pipeline_v2`. Wire-up + writer deletion come in
/// PR 5c.2 once this function has CI-green tests pinning the
/// equivalence to the SQL path.
#[must_use]
pub fn fetch_cohort_from_ram(
    bars: &[tickvault_trading::candles::Bar],
    registry: &tickvault_common::instrument_registry::InstrumentRegistry,
    exchange_segments: &[String],
    instrument_types: &[String],
    min_liquidity_volume: u64,
    max_cohort_size: usize,
) -> Vec<MoverRow> {
    if bars.is_empty() || max_cohort_size == 0 {
        return Vec::new();
    }
    // Pre-allocate capacity bounded by the smaller of bars.len() and
    // max_cohort_size — cold path (60s cadence), bounded ≤ ~500 rows.
    let cap = bars.len().min(max_cohort_size);
    let mut out: Vec<MoverRow> = Vec::with_capacity(cap);

    for bar in bars {
        // Liquidity gate first — cheapest filter, fails fast.
        if (bar.volume as u64) < min_liquidity_volume {
            continue;
        }
        // Resolve segment code → ExchangeSegment enum → string label.
        // Falls back to skip on unknown code (defensive — should never
        // happen for a Bar produced by the cascade).
        let Some(seg_enum) = ExchangeSegment::from_byte(bar.exchange_segment_code) else {
            continue;
        };
        let seg_label = seg_enum.as_str();
        if !exchange_segments.is_empty() && !exchange_segments.iter().any(|s| s == seg_label) {
            continue;
        }
        // Composite-key registry lookup per I-P1-11 (security_id alone
        // is not unique across segments).
        let Some(record) = registry.get_with_segment(bar.security_id, seg_enum) else {
            continue;
        };
        // `instrument_type_tag()` returns the "OPTIDX"/"FUTSTK"/etc.
        // labels used by `movers_pipeline` / `build_cohort_sql`, NOT
        // `DhanInstrumentKind::as_str()` which returns "OptionIndex".
        // Equivalence to the SQL filter requires the tag form.
        let kind_label = record.instrument_type_tag();
        if !instrument_types.is_empty() && !instrument_types.iter().any(|s| s == kind_label) {
            continue;
        }
        // change_pct comes from PR 5b1.5's seal-time stamp on Bar.
        out.push(MoverRow {
            security_id: bar.security_id,
            exchange_segment: seg_label.to_string(),
            instrument_type: kind_label.to_string(),
            volume: bar.volume,
            change_pct: bar.close_pct_from_prev_day,
        });
    }

    // ORDER BY volume DESC. Stable sort preserves insertion order for
    // equal-volume rows.
    out.sort_by(|a, b| b.volume.cmp(&a.volume));
    if out.len() > max_cohort_size {
        out.truncate(max_cohort_size);
    }
    out
}

/// Parses a QuestDB `/exec` response dataset into `Vec<MoverRow>`.
///
/// Expected column order (matches `build_cohort_sql`):
/// 1. `security_id` (LONG)
/// 2. `exchange_segment` (SYMBOL)
/// 3. `instrument_type` (SYMBOL)
/// 4. `volume_cumulative` (LONG) — last(volume) in `movers_1m`
/// 5. `change_pct_session` (DOUBLE) — vs prev_close in `movers_1m`
///
/// Rows with malformed columns are silently skipped (defensive).
fn parse_cohort_dataset(dataset: &[serde_json::Value]) -> anyhow::Result<Vec<MoverRow>> {
    let mut rows: Vec<MoverRow> = Vec::with_capacity(dataset.len());
    for row in dataset {
        let cols = match row.as_array() {
            Some(c) if c.len() >= 5 => c,
            _ => continue,
        };
        let security_id = match cols[0].as_i64().and_then(|v| u32::try_from(v).ok()) {
            Some(id) => id,
            None => continue,
        };
        let exchange_segment = match cols[1].as_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let instrument_type = match cols[2].as_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        let volume = cols[3].as_i64().unwrap_or(0);
        let change_pct = cols[4].as_f64().unwrap_or(0.0);
        rows.push(MoverRow {
            security_id,
            exchange_segment,
            instrument_type,
            volume,
            change_pct,
        });
    }
    Ok(rows)
}

/// Translates `Vec<DiffOp>` into per-conn `DepthCommand` and dispatches
/// over the registered mpsc senders.
///
/// `sids_per_conn = 1` → depth-200: emits 1 `Add200` / `Remove200` cmd
/// per op (the Dhan flat-JSON wire format takes a single SID per
/// frame).
/// `sids_per_conn > 1` → depth-20: ops are GROUPED by `conn_idx` and
/// emitted as ONE `AddSubscriptions20` + ONE `RemoveSubscriptions20`
/// per conn per cycle. This bounds Vec allocations to ≤ 2 × `conns`
/// per 60s cycle (= 10 for the canonical 5-conn pool) regardless of
/// op count, addressing the hot-path-reviewer HIGH finding (per-op
/// `vec![one_msg]` was 250-allocs/cycle worst case).
///
/// # Hot-path classification
///
/// This function runs on the 60s diff cycle inside the unified
/// pipeline_v2 supervisor — NOT on the per-tick parser hot path.
/// The 60s cadence + bounded ≤ 250-op/cycle upper bound make
/// this a "warm" supervisor path; the project's hot-path bans
/// (per `.claude/rules/project/hot-path.md`) target the per-tick
/// parser/ILP hot path. Even so, the rebatching above keeps
/// allocations bounded by pool shape, not op volume.
async fn dispatch_ops(
    label: &'static str,
    ops: &[DiffOp],
    senders: &HashMap<ConnIdx, mpsc::Sender<DepthCommand>>,
    sids_per_conn: u16,
) {
    if sids_per_conn == 1 {
        // depth-200 — single-SID-per-frame wire format. One cmd per op.
        for op in ops {
            match op {
                DiffOp::Remove {
                    conn_idx,
                    security_id,
                    exchange_segment,
                } => {
                    let cmd = DepthCommand::Remove200 {
                        unsubscribe_message: build_remove_200_message(
                            *security_id,
                            *exchange_segment,
                        ),
                    };
                    send_or_warn(label, *conn_idx, "Remove", senders, cmd).await;
                }
                DiffOp::Add {
                    conn_idx,
                    security_id,
                    exchange_segment,
                } => {
                    let cmd = DepthCommand::Add200 {
                        subscribe_message: build_add_200_message(*security_id, *exchange_segment),
                    };
                    send_or_warn(label, *conn_idx, "Add", senders, cmd).await;
                }
            }
        }
        return;
    }

    // depth-20 path — group per-conn so each conn receives at most ONE
    // batched Add cmd + ONE batched Remove cmd per cycle.
    let conn_count = senders.len().max(1);
    let mut adds_by_conn: HashMap<ConnIdx, Vec<String>> = HashMap::with_capacity(conn_count);
    let mut removes_by_conn: HashMap<ConnIdx, Vec<String>> = HashMap::with_capacity(conn_count);
    for op in ops {
        match op {
            DiffOp::Remove {
                conn_idx,
                security_id,
                exchange_segment,
            } => {
                removes_by_conn
                    .entry(*conn_idx)
                    .or_insert_with(|| Vec::with_capacity(usize::from(sids_per_conn)))
                    .push(build_remove_20_message(*security_id, *exchange_segment));
            }
            DiffOp::Add {
                conn_idx,
                security_id,
                exchange_segment,
            } => {
                adds_by_conn
                    .entry(*conn_idx)
                    .or_insert_with(|| Vec::with_capacity(usize::from(sids_per_conn)))
                    .push(build_add_20_message(*security_id, *exchange_segment));
            }
        }
    }
    for (conn_idx, msgs) in removes_by_conn {
        let cmd = DepthCommand::RemoveSubscriptions20 {
            unsubscribe_messages: msgs,
        };
        send_or_warn(label, conn_idx, "Remove", senders, cmd).await;
    }
    for (conn_idx, msgs) in adds_by_conn {
        let cmd = DepthCommand::AddSubscriptions20 {
            subscribe_messages: msgs,
        };
        send_or_warn(label, conn_idx, "Add", senders, cmd).await;
    }
}

async fn send_or_warn(
    label: &'static str,
    conn_idx: ConnIdx,
    op_kind: &'static str,
    senders: &HashMap<ConnIdx, mpsc::Sender<DepthCommand>>,
    cmd: DepthCommand,
) {
    match senders.get(&conn_idx) {
        Some(tx) => {
            // PR-C2 follow-up M2 — non-blocking try_send. The orchestrator
            // 60s tick MUST NOT block on a slow conn task (e.g. one stuck in
            // TLS handshake at boot or a transient network freeze). The
            // 4-deep mpsc channel is sized to absorb a single 60s burst; if
            // it's full we drop the cmd and rely on the next 60s diff to
            // either no-op (set unchanged) or resend the missed Add/Remove.
            match tx.try_send(cmd) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    error!(
                        code = "DEPTH-DYN-V2-03",
                        label,
                        conn_idx,
                        op = op_kind,
                        "depth_dynamic_pool_v2 command channel FULL — dropping cmd; \
                         next 60s diff will resend if still needed"
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    error!(
                        code = "DEPTH-DYN-V2-03",
                        label,
                        conn_idx,
                        op = op_kind,
                        "depth_dynamic_pool_v2 command channel CLOSED (broker exited?)"
                    );
                }
            }
        }
        None => {
            error!(
                code = "DEPTH-DYN-V2-04",
                label,
                conn_idx,
                op = op_kind,
                "depth_dynamic_pool_v2 no sender registered for conn (boot wiring bug)"
            );
        }
    }
}

/// Builds a 20-level subscribe JSON for a single SID. Uses Dhan
/// `RequestCode 23` per `dhan-full-market-depth.md` rule 12.
#[must_use]
fn build_add_20_message(security_id: u32, exchange_segment: ExchangeSegment) -> String {
    format!(
        r#"{{"RequestCode":23,"InstrumentCount":1,"InstrumentList":[{{"ExchangeSegment":"{}","SecurityId":"{}"}}]}}"#,
        exchange_segment.as_str(),
        security_id
    )
}

/// Builds a 20-level unsubscribe JSON for a single SID. Uses Dhan
/// `RequestCode 25` per `dhan-annexure-enums.md` rule 3
/// (`UnsubscribeFullDepth = 25`, NOT 24).
#[must_use]
fn build_remove_20_message(security_id: u32, exchange_segment: ExchangeSegment) -> String {
    format!(
        r#"{{"RequestCode":25,"InstrumentCount":1,"InstrumentList":[{{"ExchangeSegment":"{}","SecurityId":"{}"}}]}}"#,
        exchange_segment.as_str(),
        security_id
    )
}

/// Builds a 200-level subscribe JSON for a single SID — flat structure
/// (no `InstrumentList` array) per `dhan-full-market-depth.md` rule 10.
#[must_use]
fn build_add_200_message(security_id: u32, exchange_segment: ExchangeSegment) -> String {
    format!(
        r#"{{"RequestCode":23,"ExchangeSegment":"{}","SecurityId":"{}"}}"#,
        exchange_segment.as_str(),
        security_id
    )
}

/// Builds a 200-level unsubscribe JSON for a single SID — flat
/// structure. Uses Dhan `RequestCode 25`.
#[must_use]
fn build_remove_200_message(security_id: u32, exchange_segment: ExchangeSegment) -> String {
    format!(
        r#"{{"RequestCode":25,"ExchangeSegment":"{}","SecurityId":"{}"}}"#,
        exchange_segment.as_str(),
        security_id
    )
}

/// PR-D: emit per-cycle Prometheus counters + gauge.
///
/// Counters:
/// - `tv_depth_dynamic_diff_adds_total{feed}` — cumulative SIDs added
/// - `tv_depth_dynamic_diff_removes_total{feed}` — cumulative SIDs removed
/// - `tv_depth_dynamic_diff_cycles_total{feed}` — total non-no-op cycles
///
/// Gauge:
/// - `tv_depth_dynamic_set_size{feed}` — current `retained + added`
///   (the size of `next_set` after this cycle)
///
/// All emitted on the cold path (60s cycle). Per audit-findings Rule 12,
/// the Grafana panels wrap these counters in `increase(...[5m])` so a
/// process restart doesn't lie to the operator.
fn emit_diff_metrics(label: &'static str, stats: &DiffStats) {
    metrics::counter!(
        "tv_depth_dynamic_diff_adds_total",
        "feed" => label
    )
    .increment(stats.added as u64);
    metrics::counter!(
        "tv_depth_dynamic_diff_removes_total",
        "feed" => label
    )
    .increment(stats.removed as u64);
    metrics::counter!(
        "tv_depth_dynamic_diff_cycles_total",
        "feed" => label
    )
    .increment(1);
    let set_size = (stats.retained + stats.added) as f64;
    metrics::gauge!(
        "tv_depth_dynamic_set_size",
        "feed" => label
    )
    .set(set_size);
}

/// PR-D: persist one audit row per non-no-op cycle. Captures the
/// removed + added SID lists so the operator can reconstruct
/// "what changed at time T" weeks later.
async fn persist_diff_audit(
    cfg: &PipelineConfig,
    ops: &[DiffOp],
    stats: &DiffStats,
) -> anyhow::Result<()> {
    // Collect SID lists from the ops vector.
    let mut removed_sids: Vec<u32> = Vec::with_capacity(stats.removed);
    let mut added_sids: Vec<u32> = Vec::with_capacity(stats.added);
    for op in ops {
        match op {
            DiffOp::Remove { security_id, .. } => removed_sids.push(*security_id),
            DiffOp::Add { security_id, .. } => added_sids.push(*security_id),
        }
    }
    // Sort for deterministic audit output (the diff itself is already
    // deterministic but defensive).
    removed_sids.sort_unstable();
    added_sids.sort_unstable();

    let ts_nanos_ist = Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(IST_UTC_OFFSET_NANOS);

    append_depth_dynamic_diff_audit_row(
        &cfg.questdb,
        ts_nanos_ist,
        cfg.label,
        stats.removed as u64,
        stats.added as u64,
        stats.retained as u64,
        &removed_sids,
        &added_sids,
    )
    .await
}

/// Returns `true` when `now` is inside the IST market-data persist
/// window `[09:00:00, 15:30:00)`. Mirrors the helper in
/// `depth_rebalancer::is_within_market_hours_ist`.
#[inline]
#[must_use]
fn is_within_market_hours_ist() -> bool {
    let now_utc = Utc::now().timestamp();
    let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    let sec_of_day = (now_ist.rem_euclid(i64::from(SECONDS_PER_DAY))) as u32;
    (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sec_of_day)
}

/// 2026-05-02 — operator-requested symbol resolution helper. Maps
/// `DiffOp` entries (filtered by predicate) to `DepthDiffEntry` records
/// via the instrument registry's O(1) composite-key lookup (per I-P1-11).
/// Ops where the registry has no entry are silently dropped — defence
/// in depth (production paths only emit IDs already in the registry).
fn resolve_op_entries(
    ops: &[DiffOp],
    registry: &tickvault_common::instrument_registry::InstrumentRegistry,
    predicate: impl Fn(&DiffOp) -> bool,
) -> Vec<tickvault_core::notification::DepthDiffEntry> {
    let mut out: Vec<tickvault_core::notification::DepthDiffEntry> = Vec::with_capacity(ops.len());
    for op in ops {
        if !predicate(op) {
            continue;
        }
        let (sid, segment) = match op {
            DiffOp::Add {
                security_id,
                exchange_segment,
                ..
            }
            | DiffOp::Remove {
                security_id,
                exchange_segment,
                ..
            } => (*security_id, *exchange_segment),
        };
        if let Some(inst) = registry.get_with_segment(sid, segment) {
            out.push(tickvault_core::notification::DepthDiffEntry {
                security_id: sid,
                exchange_segment: segment.as_str(),
                display_label: inst.display_label.clone(),
                underlying_symbol: inst.underlying_symbol.clone(),
            });
        }
    }
    out
}

/// Returns the current IST trading day as a `(year, ordinal_day)`
/// tuple — a cheap comparable identity that rolls over at 00:00 IST.
/// Used by the daily-reset gate (PR-C2 follow-up H2).
#[inline]
#[must_use]
fn current_ist_trading_day() -> i64 {
    let now_utc = Utc::now().timestamp();
    let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    now_ist.div_euclid(i64::from(SECONDS_PER_DAY))
}

/// Wall-clock target for the FIRST selection cycle of each trading
/// day: 09:15:00 IST per operator clarification 2026-05-03.
///
/// Earlier 09:15:00 was over-engineering — movers compute per-tick
/// (not averaged), so the F&O opening tick at 09:15:00 IS the highest-
/// information `change_pct` of the day and MUST be captured. Aligns
/// with the exchange's stated MARKET start (09:15:00).
const FIRST_CYCLE_SECS_OF_DAY_IST: u32 = 9 * 3600 + 15 * 60;

/// Operator-requested cadence (audit-2026-05-03 boundary update):
///   * Tick 1 at 09:15:00 IST (initial subscribe — uses
///     `compute_first_cycle_instant_today`). Captures F&O opening tick.
///   * Tick 2 at 09:16:00 IST (first minute-aligned re-rebalance)
///   * Tick 3 at 09:17:00 IST
///   * Tick N at 09:14+N:00 IST
///
/// After the first cycle fires at 09:15:00, the interval is re-anchored
/// to the next minute boundary so subsequent ticks land on `:00`
/// seconds — matching the operator's stated "from 9.16.00 dynamic
/// movement should happen" cadence.
///
/// Returns an Instant for the next wall-clock minute boundary
/// (`current_second_of_minute == 0`). Always >= `Instant::now()`.
#[inline]
#[must_use]
fn compute_next_minute_boundary_instant() -> tokio::time::Instant {
    let now_utc = Utc::now().timestamp();
    // Seconds into the current minute, in [0, 59].
    let secs_into_minute = now_utc.rem_euclid(60);
    // If we're EXACTLY at :00, fire 60s from now (the next :00). Avoids
    // a zero-duration sleep that could fire mid-cycle.
    let secs_to_boundary = if secs_into_minute == 0 {
        60
    } else {
        60 - secs_into_minute
    };
    // APPROVED: secs_to_boundary is in [1, 60], fits u64.
    #[allow(clippy::cast_sign_loss)]
    let dur = Duration::from_secs(secs_to_boundary as u64);
    tokio::time::Instant::now() + dur
}

/// 2026-05-02 — operator-requested first-cycle alignment.
///
/// Returns a `tokio::time::Instant` for the next 09:15:00 IST anchor
/// point. Behaviour:
///   * Pre-09:15:00 today → returns Instant for today's 09:15:00
///   * Post-09:15:00 today → returns `Instant::now()` so the first
///     `tick.tick().await` fires immediately (boot-after-market-open
///     case, e.g. crash recovery at 11:30 IST)
///
/// Combined with `tokio::time::interval_at(start, 60s)`, the first
/// selection cycle of each day fires within OS-scheduler latency
/// (~1ms typical) of 09:15:00 IST, eliminating the up-to-60-second
/// blind spot the legacy boot-aligned `interval(60s)` introduced.
///
/// Subsequent ticks fire at +60s, +120s, …; daily-rollover handler
/// re-creates the interval to keep the alignment from drifting.
#[inline]
#[must_use]
fn compute_first_cycle_instant_today() -> tokio::time::Instant {
    let now_utc = Utc::now().timestamp();
    let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    let secs_into_day = now_ist.rem_euclid(i64::from(SECONDS_PER_DAY));
    let target = i64::from(FIRST_CYCLE_SECS_OF_DAY_IST);
    let offset_secs = (target - secs_into_day).max(0);
    // APPROVED: offset_secs is non-negative i64 ≤ 86400, fits u64 cleanly.
    #[allow(clippy::cast_sign_loss)]
    let offset_dur = Duration::from_secs(offset_secs as u64);
    tokio::time::Instant::now() + offset_dur
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nse_fno() -> ExchangeSegment {
        ExchangeSegment::NseFno
    }

    fn idx_i() -> ExchangeSegment {
        ExchangeSegment::IdxI
    }

    // ---- build_add_20_message / build_remove_20_message ----

    #[test]
    fn test_build_add_20_message_uses_request_code_23() {
        let msg = build_add_20_message(50001, nse_fno());
        assert!(msg.contains(r#""RequestCode":23"#));
        assert!(msg.contains(r#""ExchangeSegment":"NSE_FNO""#));
        assert!(msg.contains(r#""SecurityId":"50001""#));
        assert!(msg.contains(r#""InstrumentCount":1"#));
        assert!(msg.contains(r#""InstrumentList""#));
    }

    #[test]
    fn test_build_remove_20_message_uses_request_code_25_not_24() {
        // Per dhan-annexure-enums.md rule 3 — UnsubscribeFullDepth = 25.
        let msg = build_remove_20_message(50000, nse_fno());
        assert!(msg.contains(r#""RequestCode":25"#));
        assert!(!msg.contains(r#""RequestCode":24"#));
    }

    #[test]
    fn test_build_add_20_message_emits_security_id_as_string_not_int() {
        // Per dhan-live-market-feed.md rule 15 — SecurityId is STRING in JSON.
        let msg = build_add_20_message(1333, nse_fno());
        assert!(msg.contains(r#""SecurityId":"1333""#));
        assert!(!msg.contains(r#""SecurityId":1333"#));
    }

    // ---- build_add_200_message / build_remove_200_message ----

    #[test]
    fn test_build_add_200_message_is_flat_no_instrument_list() {
        // Per dhan-full-market-depth.md rule 10 — 200-level uses flat
        // JSON (no InstrumentList array).
        let msg = build_add_200_message(50002, nse_fno());
        assert!(msg.contains(r#""RequestCode":23"#));
        assert!(!msg.contains("InstrumentList"));
        assert!(!msg.contains("InstrumentCount"));
        assert!(msg.contains(r#""ExchangeSegment":"NSE_FNO""#));
        assert!(msg.contains(r#""SecurityId":"50002""#));
    }

    #[test]
    fn test_build_remove_200_message_uses_request_code_25_flat() {
        let msg = build_remove_200_message(50003, nse_fno());
        assert!(msg.contains(r#""RequestCode":25"#));
        assert!(!msg.contains("InstrumentList"));
    }

    #[test]
    fn test_build_messages_emit_idx_i_segment_for_index_subscribers() {
        // Future-proof: the builders work for IDX_I too if the operator
        // expands the universe to include index spot in depth (today
        // only NSE_FNO + BSE_FNO derivatives are subscribed).
        let msg = build_add_200_message(13, idx_i());
        assert!(msg.contains(r#""ExchangeSegment":"IDX_I""#));
    }

    // ---- parse_cohort_dataset ----

    #[test]
    fn test_parse_cohort_dataset_parses_valid_rows() {
        let dataset = serde_json::json!([
            [50000_i64, "NSE_FNO", "OPTSTK", 1_000_000_i64, 5.5_f64],
            [50001_i64, "NSE_FNO", "OPTSTK", 800_000_i64, 4.2_f64],
        ]);
        let rows = parse_cohort_dataset(dataset.as_array().unwrap()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].security_id, 50000);
        assert_eq!(rows[0].exchange_segment, "NSE_FNO");
        assert_eq!(rows[0].instrument_type, "OPTSTK");
        assert_eq!(rows[0].volume, 1_000_000);
        assert!((rows[0].change_pct - 5.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_cohort_dataset_skips_short_rows() {
        let dataset = serde_json::json!([
            [50000_i64, "NSE_FNO"],                                 // too short
            [50001_i64, "NSE_FNO", "OPTSTK", 800_000_i64, 4.2_f64], // valid
        ]);
        let rows = parse_cohort_dataset(dataset.as_array().unwrap()).unwrap();
        assert_eq!(rows.len(), 1, "short row dropped, valid kept");
        assert_eq!(rows[0].security_id, 50001);
    }

    #[test]
    fn test_parse_cohort_dataset_skips_rows_with_invalid_security_id() {
        let dataset = serde_json::json!([
            ["not_a_number", "NSE_FNO", "OPTSTK", 800_000_i64, 4.2_f64], // bad security_id
            [-1_i64, "NSE_FNO", "OPTSTK", 800_000_i64, 4.2_f64],         // negative
            [50001_i64, "NSE_FNO", "OPTSTK", 800_000_i64, 4.2_f64],      // valid
        ]);
        let rows = parse_cohort_dataset(dataset.as_array().unwrap()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].security_id, 50001);
    }

    #[test]
    fn test_parse_cohort_dataset_handles_empty_dataset() {
        let dataset = serde_json::json!([]);
        let rows = parse_cohort_dataset(dataset.as_array().unwrap()).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn test_parse_cohort_dataset_defaults_volume_and_change_pct_when_missing() {
        // Defensive: QuestDB returning JSON null on a numeric column
        // shouldn't crash the selector. Default to 0.
        let dataset = serde_json::json!([[50000_i64, "NSE_FNO", "OPTSTK", null, null],]);
        let rows = parse_cohort_dataset(dataset.as_array().unwrap()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].volume, 0);
        assert!((rows[0].change_pct - 0.0).abs() < f64::EPSILON);
    }

    // ---- is_within_market_hours_ist (smoke) ----

    #[test]
    fn test_is_within_market_hours_ist_returns_bool() {
        let _ = is_within_market_hours_ist();
    }

    /// PR-C2 follow-up H2 ratchet — current_ist_trading_day must return
    /// a stable comparable identity that rolls over at IST midnight.
    /// We can't pin to a specific value without mocking the clock, but
    /// we can assert (a) it's positive, (b) two consecutive calls within
    /// a few millis return the same value (no rollover mid-test).
    #[test]
    fn test_current_ist_trading_day_is_stable_within_a_call() {
        let a = current_ist_trading_day();
        let b = current_ist_trading_day();
        assert!(a > 0, "trading day epoch must be positive, got {a}");
        assert_eq!(
            a, b,
            "two consecutive calls must return same day-of-epoch (no rollover mid-test)"
        );
    }

    /// Ratchet: today's epoch-day must be within a sane window
    /// (post-2020, pre-2050) — guards against accidentally returning
    /// raw epoch seconds or some other unit.
    #[test]
    fn test_current_ist_trading_day_in_sane_decade_range() {
        let day = current_ist_trading_day();
        // 2020-01-01 = day 18262, 2050-01-01 = day 29220.
        assert!(
            (18000..30000).contains(&day),
            "trading day epoch out of sane range: {day}"
        );
    }

    /// Audit-2026-05-03 (operator clarification): first cycle pinned
    /// at 09:15:00 IST (was 09:15:00). The F&O opening tick MUST be
    /// captured — it carries the highest-information change_pct of
    /// the day. Aligns with movers MARKET start (09:15:00) and the
    /// exchange's stated MARKET window boundary.
    #[test]
    fn test_first_cycle_secs_of_day_ist_pinned_at_09_15_00_per_operator_clarification() {
        // 09:15:00 IST = 9*3600 + 15*60 = 33_300
        assert_eq!(FIRST_CYCLE_SECS_OF_DAY_IST, 33_300);
    }

    /// Ratchet: the helper returns an Instant that is `Instant::now()`
    /// or in the future. Cannot return a past-relative Instant because
    /// `Instant + Duration` is monotone non-decreasing.
    #[test]
    fn test_compute_first_cycle_instant_today_never_in_the_past() {
        let before = tokio::time::Instant::now();
        let target = compute_first_cycle_instant_today();
        // `before` was sampled before the helper ran; helper internally
        // calls Instant::now() AFTER `before`, so target >= before always.
        assert!(target >= before);
    }

    /// Ratchet: when called multiple times within the same second, the
    /// helper must return Instants that are within ~1 second of each
    /// other — bounding clock-skew-style drift.
    #[test]
    fn test_compute_first_cycle_instant_today_stable_within_call_window() {
        let a = compute_first_cycle_instant_today();
        let b = compute_first_cycle_instant_today();
        let diff = if b > a {
            b.duration_since(a)
        } else {
            a.duration_since(b)
        };
        // Two calls within milliseconds should produce Instants within 1s.
        // (offset_secs is integer-second granularity; jitter < 1s.)
        assert!(
            diff.as_secs() <= 1,
            "two consecutive calls produced Instants {diff:?} apart — expected ≤ 1s"
        );
    }

    /// Operator-requested cadence: minute-boundary alignment AFTER the
    /// first 09:15:00 cycle. The helper must return an Instant that is
    /// 1..=60 seconds away from now — never zero, never more than 60.
    #[test]
    fn test_compute_next_minute_boundary_instant_within_one_minute_window() {
        let before = tokio::time::Instant::now();
        let target = compute_next_minute_boundary_instant();
        let delta = target.saturating_duration_since(before);
        assert!(
            delta.as_secs() >= 1 && delta.as_secs() <= 60,
            "next minute boundary must be 1..=60s away, got {delta:?}"
        );
    }

    /// Ratchet: minute boundary helper aligns to wall-clock :00 seconds.
    /// Two consecutive calls separated by < 1s must produce Instants
    /// that target the SAME minute boundary (within sub-second jitter).
    #[test]
    fn test_compute_next_minute_boundary_instant_aligns_to_wall_clock() {
        let a = compute_next_minute_boundary_instant();
        let b = compute_next_minute_boundary_instant();
        let diff = if b > a {
            b.duration_since(a)
        } else {
            a.duration_since(b)
        };
        // Both calls hit the same :00 boundary unless we crossed a
        // second boundary mid-test (rare but possible). Either way
        // the gap is at most 1 second.
        assert!(
            diff.as_secs() <= 1,
            "two consecutive minute-boundary calls produced Instants {diff:?} apart"
        );
    }

    /// 2026-05-02 — heartbeat counter source-scan ratchet. Pins the
    /// presence of `tv_depth_dynamic_heartbeat_total` increment in the
    /// orchestrator. Removing it would silently disable the
    /// `tv-depth-dynamic-pipeline-stalled` Prometheus alert (operator
    /// would no longer get a Telegram on a dead orchestrator).
    #[test]
    fn test_heartbeat_counter_increment_is_present_in_orchestrator() {
        let src = include_str!("depth_dynamic_pipeline_v2.rs");
        assert!(
            src.contains("\"tv_depth_dynamic_heartbeat_total\""),
            "heartbeat counter increment must remain in spawn_depth_dynamic_pool"
        );
        // Must also be tagged with the feed label for per-pool isolation.
        assert!(
            src.contains("\"feed\" => cfg.label"),
            "heartbeat counter must carry the feed label for per-pool isolation"
        );
    }

    // ---- Constants ----

    #[test]
    fn test_reselect_interval_secs_is_60() {
        assert_eq!(RESELECT_INTERVAL_SECS, 60);
    }

    /// Audit-2026-05-03 ratchet: default min-volume floor must be
    /// strictly positive (zero would bypass the liquidity gate).
    /// Replaces the legacy `test_cohort_size_default_is_at_least_5x_max_k`.
    #[test]
    fn test_default_min_liquidity_volume_is_positive_floor() {
        assert!(
            DEFAULT_MIN_LIQUIDITY_VOLUME > 0,
            "default min_liquidity_volume must reject illiquid contracts"
        );
        // Pin the specific value to make config-drift detectable.
        assert_eq!(DEFAULT_MIN_LIQUIDITY_VOLUME, 10_000);
    }

    #[test]
    fn test_freshness_window_secs_default_matches_reselect_interval() {
        // Avoids stale rows older than one cycle.
        assert_eq!(
            u64::from(DEFAULT_FRESHNESS_WINDOW_SECS),
            RESELECT_INTERVAL_SECS
        );
    }

    // ---- dispatch_ops shape ----

    #[tokio::test]
    async fn test_dispatch_ops_routes_remove_to_owning_conn_for_depth_20() {
        // Build 5 conn senders, dispatch a Remove on conn 2, verify
        // ONLY conn 2's receiver got the message.
        let mut senders: HashMap<ConnIdx, mpsc::Sender<DepthCommand>> = HashMap::new();
        let mut receivers: Vec<mpsc::Receiver<DepthCommand>> = Vec::new();
        for c in 0..5 {
            let (tx, rx) = mpsc::channel(8);
            senders.insert(c, tx);
            receivers.push(rx);
        }
        let ops = vec![DiffOp::Remove {
            conn_idx: 2,
            security_id: 50000,
            exchange_segment: ExchangeSegment::NseFno,
        }];
        dispatch_ops("depth-20-test", &ops, &senders, 50).await;
        // Only conn 2 receives.
        for (c, rx) in receivers.iter_mut().enumerate() {
            let got = rx.try_recv();
            if c == 2 {
                let msg = got.expect("conn 2 must receive Remove");
                assert!(matches!(msg, DepthCommand::RemoveSubscriptions20 { .. }));
            } else {
                assert!(got.is_err(), "conn {c} must NOT receive any command");
            }
        }
    }

    #[tokio::test]
    async fn test_dispatch_ops_uses_remove_200_variant_when_sids_per_conn_is_1() {
        let mut senders: HashMap<ConnIdx, mpsc::Sender<DepthCommand>> = HashMap::new();
        let (tx, mut rx) = mpsc::channel(8);
        senders.insert(0, tx);
        let ops = vec![DiffOp::Remove {
            conn_idx: 0,
            security_id: 50000,
            exchange_segment: ExchangeSegment::NseFno,
        }];
        dispatch_ops("depth-200-test", &ops, &senders, 1).await;
        let msg = rx.try_recv().expect("conn 0 must receive Remove200");
        assert!(matches!(msg, DepthCommand::Remove200 { .. }));
    }

    #[tokio::test]
    async fn test_dispatch_ops_uses_add_subscriptions_20_when_sids_per_conn_gt_1() {
        let mut senders: HashMap<ConnIdx, mpsc::Sender<DepthCommand>> = HashMap::new();
        let (tx, mut rx) = mpsc::channel(8);
        senders.insert(3, tx);
        let ops = vec![DiffOp::Add {
            conn_idx: 3,
            security_id: 50001,
            exchange_segment: ExchangeSegment::NseFno,
        }];
        dispatch_ops("depth-20-test", &ops, &senders, 50).await;
        let msg = rx
            .try_recv()
            .expect("conn 3 must receive AddSubscriptions20");
        assert!(matches!(msg, DepthCommand::AddSubscriptions20 { .. }));
    }

    #[tokio::test]
    async fn test_dispatch_ops_handles_missing_sender_for_conn_idx_gracefully() {
        // Sender map has no entry for conn 4 — dispatch must NOT panic.
        let senders: HashMap<ConnIdx, mpsc::Sender<DepthCommand>> = HashMap::new();
        let ops = vec![DiffOp::Add {
            conn_idx: 4,
            security_id: 50000,
            exchange_segment: ExchangeSegment::NseFno,
        }];
        // Should log error code DEPTH-DYN-V2-04 but not panic.
        dispatch_ops("depth-20-test", &ops, &senders, 50).await;
    }

    /// Ratchet for the hot-path-reviewer rebatch fix.
    ///
    /// Before the rebatch: 100 ops on the same conn would send 100
    /// individual `AddSubscriptions20` commands (each with a
    /// `vec![one_msg]`), wasting 100 Vec allocations per cycle.
    ///
    /// After the rebatch: 100 ops on the same conn must collapse to
    /// EXACTLY ONE `AddSubscriptions20` command whose
    /// `subscribe_messages.len() == 100`.
    #[tokio::test]
    async fn test_dispatch_ops_rebatches_adds_per_conn_for_depth_20() {
        let mut senders: HashMap<ConnIdx, mpsc::Sender<DepthCommand>> = HashMap::new();
        let (tx, mut rx) = mpsc::channel(8);
        senders.insert(2, tx);

        // 50 Add ops all targeting conn 2 — must produce 1 cmd, not 50.
        let ops: Vec<DiffOp> = (0..50)
            .map(|i| DiffOp::Add {
                conn_idx: 2,
                security_id: 60_000_u32 + i,
                exchange_segment: ExchangeSegment::NseFno,
            })
            .collect();

        dispatch_ops("depth-20-test", &ops, &senders, 50).await;

        let cmd = rx
            .try_recv()
            .expect("conn 2 must receive ONE batched AddSubscriptions20");
        match cmd {
            DepthCommand::AddSubscriptions20 { subscribe_messages } => {
                assert_eq!(
                    subscribe_messages.len(),
                    50,
                    "rebatch must collapse 50 Add ops into ONE cmd with 50 messages"
                );
            }
            other => panic!("expected AddSubscriptions20, got {other:?}"),
        }
        // No further commands.
        assert!(
            rx.try_recv().is_err(),
            "conn 2 must NOT receive a second batched cmd"
        );
    }

    /// Ratchet: same rebatch behaviour for Remove ops.
    #[tokio::test]
    async fn test_dispatch_ops_rebatches_removes_per_conn_for_depth_20() {
        let mut senders: HashMap<ConnIdx, mpsc::Sender<DepthCommand>> = HashMap::new();
        let (tx, mut rx) = mpsc::channel(8);
        senders.insert(1, tx);

        let ops: Vec<DiffOp> = (0..30)
            .map(|i| DiffOp::Remove {
                conn_idx: 1,
                security_id: 70_000_u32 + i,
                exchange_segment: ExchangeSegment::NseFno,
            })
            .collect();

        dispatch_ops("depth-20-test", &ops, &senders, 50).await;

        let cmd = rx
            .try_recv()
            .expect("conn 1 must receive ONE batched RemoveSubscriptions20");
        match cmd {
            DepthCommand::RemoveSubscriptions20 {
                unsubscribe_messages,
            } => {
                assert_eq!(unsubscribe_messages.len(), 30);
            }
            other => panic!("expected RemoveSubscriptions20, got {other:?}"),
        }
    }

    // ---- 2026-05-09 PR 5c.1 — fetch_cohort_from_ram ratchets ----
    //
    // The new RAM-backed cohort fetcher must produce the same shape +
    // semantics as the legacy `fetch_cohort_from_questdb` SQL path.
    // These tests pin the contract so PR 5c.2 (the actual call-site
    // swap) lands without behavioural drift.

    fn make_test_bar(
        security_id: u32,
        segment_code: u8,
        volume: i64,
        change_pct: f64,
    ) -> tickvault_trading::candles::Bar {
        tickvault_trading::candles::Bar {
            bucket_start_ist_secs: 33_000,
            bucket_end_ist_secs: 33_001,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
            volume,
            volume_cum_day_at_end: volume,
            oi: 0,
            tick_count: 1,
            security_id,
            exchange_segment_code: segment_code,
            sealed: true,
            prev_day_close: 100.0,
            prev_day_oi: 0,
            close_pct_from_prev_day: change_pct,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
        }
    }

    fn make_optidx_subscribed(
        security_id: u32,
        segment: ExchangeSegment,
    ) -> tickvault_common::instrument_registry::SubscribedInstrument {
        use chrono::NaiveDate;
        use tickvault_common::instrument_registry::SubscribedInstrument;
        use tickvault_common::instrument_types::DhanInstrumentKind;
        use tickvault_common::types::{FeedMode, OptionType};
        SubscribedInstrument {
            security_id,
            exchange_segment: segment,
            category: tickvault_common::instrument_registry::SubscriptionCategory::IndexDerivative,
            display_label: format!("OPT-{security_id}"),
            underlying_symbol: "NIFTY".to_string(),
            instrument_kind: Some(DhanInstrumentKind::OptionIndex),
            expiry_date: Some(NaiveDate::from_ymd_opt(2026, 5, 29).unwrap()),
            strike_price: Some(25_000.0),
            option_type: Some(OptionType::Call),
            feed_mode: FeedMode::Quote,
        }
    }

    #[test]
    fn test_fetch_cohort_from_ram_empty_input_returns_empty() {
        let registry = tickvault_common::instrument_registry::InstrumentRegistry::empty();
        let out = fetch_cohort_from_ram(
            &[],
            &registry,
            &["NSE_FNO".to_string()],
            &["OPTIDX".to_string()],
            100,
            500,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn test_fetch_cohort_from_ram_zero_max_size_returns_empty() {
        let registry = tickvault_common::instrument_registry::InstrumentRegistry::empty();
        let bars = vec![make_test_bar(123, 2, 10_000, 5.0)];
        let out = fetch_cohort_from_ram(
            &bars,
            &registry,
            &["NSE_FNO".to_string()],
            &["OPTIDX".to_string()],
            100,
            0,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn test_fetch_cohort_from_ram_filters_by_min_liquidity() {
        let inst = make_optidx_subscribed(123, ExchangeSegment::NseFno);
        let registry =
            tickvault_common::instrument_registry::InstrumentRegistry::from_instruments(vec![inst]);
        let bars = vec![
            make_test_bar(123, 2, 50, 5.0),        // below min_liquidity
            make_test_bar(124, 2, 1_000_000, 3.0), // above
        ];
        let out = fetch_cohort_from_ram(
            &bars,
            &registry,
            &["NSE_FNO".to_string()],
            &["OPTIDX".to_string()],
            100, // min_liquidity_volume
            500,
        );
        // 124 not registered so also drops; 123 below threshold so drops.
        assert!(out.is_empty());
    }

    #[test]
    fn test_fetch_cohort_from_ram_filters_by_exchange_segment() {
        let inst = make_optidx_subscribed(123, ExchangeSegment::IdxI);
        let registry =
            tickvault_common::instrument_registry::InstrumentRegistry::from_instruments(vec![inst]);
        let bars = vec![make_test_bar(123, 0, 1_000_000, 5.0)]; // IDX_I = 0
        let out_fno = fetch_cohort_from_ram(
            &bars,
            &registry,
            &["NSE_FNO".to_string()],
            &[], // any instrument_type
            1,
            500,
        );
        assert!(
            out_fno.is_empty(),
            "IDX_I bar must be filtered out by NSE_FNO segment filter"
        );
        let out_idx = fetch_cohort_from_ram(&bars, &registry, &["IDX_I".to_string()], &[], 1, 500);
        assert_eq!(out_idx.len(), 1);
    }

    #[test]
    fn test_fetch_cohort_from_ram_emits_optidx_tag_not_dhan_kind_string() {
        // Ratchet: kind label must use `instrument_type_tag()` ("OPTIDX")
        // NOT `DhanInstrumentKind::as_str()` ("OptionIndex"). The legacy
        // SQL path in `build_cohort_sql` filters on the tag form, so the
        // RAM path MUST match for filter equivalence.
        let inst = make_optidx_subscribed(123, ExchangeSegment::NseFno);
        let registry =
            tickvault_common::instrument_registry::InstrumentRegistry::from_instruments(vec![inst]);
        let bars = vec![make_test_bar(123, 2, 1_000_000, 5.0)];
        let out = fetch_cohort_from_ram(
            &bars,
            &registry,
            &["NSE_FNO".to_string()],
            &["OPTIDX".to_string()], // tag form, not "OptionIndex"
            1,
            500,
        );
        assert_eq!(out.len(), 1, "OPTIDX tag MUST match — got: {out:?}");
        assert_eq!(out[0].instrument_type, "OPTIDX");
    }

    #[test]
    fn test_fetch_cohort_from_ram_sorts_volume_desc_and_truncates() {
        let inst1 = make_optidx_subscribed(101, ExchangeSegment::NseFno);
        let inst2 = make_optidx_subscribed(102, ExchangeSegment::NseFno);
        let inst3 = make_optidx_subscribed(103, ExchangeSegment::NseFno);
        let registry =
            tickvault_common::instrument_registry::InstrumentRegistry::from_instruments(vec![
                inst1, inst2, inst3,
            ]);
        let bars = vec![
            make_test_bar(101, 2, 1_000, 5.0),
            make_test_bar(102, 2, 3_000, 4.0),
            make_test_bar(103, 2, 2_000, 6.0),
        ];
        let out = fetch_cohort_from_ram(
            &bars,
            &registry,
            &[],
            &["OPTIDX".to_string()],
            100,
            2, // truncate
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].security_id, 102, "highest volume first");
        assert_eq!(out[1].security_id, 103, "second-highest volume next");
    }

    /// Ratchet: depth-200 (sids_per_conn = 1) MUST stay one-cmd-per-op
    /// because the wire format takes a single SID per frame.
    #[tokio::test]
    async fn test_dispatch_ops_does_not_rebatch_for_depth_200() {
        let mut senders: HashMap<ConnIdx, mpsc::Sender<DepthCommand>> = HashMap::new();
        let (tx, mut rx) = mpsc::channel(8);
        senders.insert(0, tx);

        // 3 Add ops all on conn 0 — depth-200 must emit 3 separate Add200 cmds.
        let ops: Vec<DiffOp> = (0..3)
            .map(|i| DiffOp::Add {
                conn_idx: 0,
                security_id: 80_000_u32 + i,
                exchange_segment: ExchangeSegment::NseFno,
            })
            .collect();

        dispatch_ops("depth-200-test", &ops, &senders, 1).await;

        let mut count = 0;
        while let Ok(cmd) = rx.try_recv() {
            assert!(matches!(cmd, DepthCommand::Add200 { .. }));
            count += 1;
        }
        assert_eq!(count, 3, "depth-200 must emit 3 separate Add200 cmds");
    }
}
