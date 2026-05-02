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
    MoverRow, SelectorConfig, build_cohort_sql, select_top_k_dynamic,
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

/// Stage-1 SQL cohort size — top-N-by-volume rows fetched before
/// Stage-2 re-rank. Pinned at 500 (5× the largest K=250 used by
/// depth-20) so the cohort always has enough rows for Stage-2 to
/// produce a stable top-K under filter attrition.
const COHORT_SIZE: usize = 500;

/// SQL freshness window — only consider `movers_1m` rows from the
/// last 60 seconds. Filters stale post-market data.
const FRESHNESS_WINDOW_SECS: u32 = 60;

/// Pipeline configuration. Constructed at boot from
/// `config/base.toml::[depth_20.dynamic]` or
/// `config/base.toml::[depth_200.dynamic]`.
#[derive(Debug, Clone)]
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
        let mut tick = tokio::time::interval(Duration::from_secs(RESELECT_INTERVAL_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    info!(label = cfg.label, "depth_dynamic_pool_v2 shutdown notified");
                    break;
                }
                _ = tick.tick() => {
                    if !is_within_market_hours_ist() {
                        continue;
                    }
                    let cohort = match fetch_cohort_from_questdb(&cfg).await {
                        Ok(c) => c,
                        Err(err) => {
                            error!(
                                code = "DEPTH-DYN-V2-01",
                                label = cfg.label,
                                ?err,
                                "depth_dynamic_pool_v2 cohort fetch failed"
                            );
                            continue;
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
                        Ok(r) => r,
                        Err(err) => {
                            error!(
                                code = "DEPTH-DYN-V2-02",
                                label = cfg.label,
                                ?err,
                                "depth_dynamic_pool_v2 diff failed (over capacity?)"
                            );
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

                    // PR-D: emit Prom counters BEFORE dispatch so the
                    // gauge value is visible to alert rules even if a
                    // dispatch send subsequently fails.
                    emit_diff_metrics(cfg.label, &stats);

                    dispatch_ops(cfg.label, &ops, &cmd_senders, cfg.shape.sids_per_conn).await;

                    // PR-D: persist one audit row per non-no-op cycle.
                    // Best-effort — audit failure must NOT halt the
                    // pipeline (Wave 5 audit-finding Rule 5: flush
                    // failures route to ERROR for Telegram, but the
                    // persist is fire-and-forget here on the cold path).
                    if let Err(err) = persist_diff_audit(&cfg, &ops, &stats).await {
                        error!(
                            code = "DEPTH-DYN-V2-AUDIT-01",
                            label = cfg.label,
                            ?err,
                            "depth_dynamic_pool_v2 audit row persist failed"
                        );
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
        COHORT_SIZE,
        FRESHNESS_WINDOW_SECS,
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

/// Parses a QuestDB `/exec` response dataset into `Vec<MoverRow>`.
///
/// Expected column order (matches `build_cohort_sql`):
/// 1. `security_id` (LONG)
/// 2. `exchange_segment` (SYMBOL)
/// 3. `instrument_type` (SYMBOL)
/// 4. `volume` (LONG)
/// 5. `change_pct` (DOUBLE)
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
            if let Err(err) = tx.send(cmd).await {
                error!(
                    code = "DEPTH-DYN-V2-03",
                    label,
                    conn_idx,
                    op = op_kind,
                    ?err,
                    "depth_dynamic_pool_v2 command channel send failed (broker exited?)"
                );
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

    // ---- Constants ----

    #[test]
    fn test_reselect_interval_secs_is_60() {
        assert_eq!(RESELECT_INTERVAL_SECS, 60);
    }

    #[test]
    fn test_cohort_size_is_at_least_5x_max_k() {
        // Cohort size must be large enough to absorb Stage-2 filter
        // attrition for the largest K (=250 for depth-20).
        assert!(COHORT_SIZE >= 250);
    }

    #[test]
    fn test_freshness_window_secs_matches_reselect_interval() {
        // Avoids stale rows older than one cycle.
        assert_eq!(u64::from(FRESHNESS_WINDOW_SECS), RESELECT_INTERVAL_SECS);
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
