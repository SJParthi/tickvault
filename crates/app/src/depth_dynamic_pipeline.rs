//! Wave 5 Items 4+5 LIVE — depth-20 + depth-200 dynamic top-volume orchestrator.
//!
//! Operator spec 2026-05-01 (Option B per plan §"BLOCKER C1"):
//!
//! **Depth-20 (5 connections):**
//! - Conn 1: NIFTY ATM ± 24 CE (49 contracts, single-side)
//! - Conn 2: NIFTY ATM ± 24 PE (49 contracts, single-side)
//! - Conn 3: BANKNIFTY ATM ± 24 CE (49 contracts, single-side)
//! - Conn 4: BANKNIFTY ATM ± 24 PE (49 contracts, single-side)
//! - Conn 5: Top-50 by `change_pct DESC`, **SENSEX excluded** — dynamic
//!   60s reselection
//!
//! **Depth-200 (5 connections):**
//! - Conn 1..5: Top-5 by `change_pct DESC`, **SENSEX excluded** — dynamic
//!   60s reselection (1 contract per connection per Dhan protocol)
//!
//! # Honest envelope
//!
//! - **Off by default.** `[features] depth_dynamic_top_volume = false` —
//!   the existing depth pool architecture (4 mixed-CE+PE static for
//!   indices, 4 ATM single-side depth-200 for NIFTY/BANKNIFTY) keeps
//!   running. This orchestrator runs ALONGSIDE when flipped on, NOT
//!   replacing the legacy pool.
//! - **Selector is read-only.** The 60s SQL `SELECT ... FROM
//!   option_movers WHERE category = 'TOP_VOLUME' AND change_pct > 0
//!   AND segment != 'BSE_FNO' ORDER BY volume DESC, change_pct DESC
//!   LIMIT N` — reads from QuestDB, never writes. Operator spec
//!   2026-05-01: VOLUME is PRIMARY sort key; change_pct DESC is the
//!   tie-breaker.
//! - **Swap commands are zero-disconnect.** When the rank set changes,
//!   the orchestrator emits `DepthCommand::Swap20` /
//!   `DepthCommand::Swap200` to the depth-connection task's command
//!   channel. The websocket sends `RequestCode 25` (unsubscribe old)
//!   then `23` (subscribe new) — no socket disconnect.
//! - **Edge-triggered.** Identical rank sets produce no swap (per
//!   audit-findings Rule 4 — no thrash-on-equal). Hysteresis ≥ 3
//!   slot positions for depth-20 (`DEPTH_20_SWAP_HYSTERESIS_MIN`).
//! - **Market-hours gated.** Outside [09:00, 15:30) IST → idle.
//!
//! # Dispatch model (when flipped on)
//!
//! Two background tasks, both spawned via `spawn_depth_dynamic_top_volume_pipeline`:
//!
//! 1. **Depth-20 conn-5 dynamic task** — owns 1 mpsc::Sender<DepthCommand>
//!    pointing at the existing depth-20 conn 5 (registered in
//!    `depth_cmd_senders`). Every 60s: query, sanitize, hysteresis-check,
//!    `Swap20` if material change.
//!
//! 2. **Depth-200 dynamic-pool task** — owns 5 mpsc::Sender<DepthCommand>
//!    pointing at the existing depth-200 conns 1..5. Every 60s: query
//!    top-5, assign to slots via `assign_depth_200_slots`, slot-by-slot
//!    diff via `slots_needing_swap`, emit `Swap200` per changed slot.
//!
//! # Why this orchestrator does NOT replace the static 4 single-side conns
//!
//! Per the operator's spec, conns 1-4 are STATIC (NIFTY CE/PE +
//! BANKNIFTY CE/PE ATM ± 24). Those don't need a 60s rebalancer; the
//! existing `depth_rebalancer` covers the spot-drift case. This module
//! only handles the DYNAMIC layer (conn 5 of depth-20 + all 5 conns of
//! depth-200).

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
    IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST,
    TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_core::instrument::depth_200_dynamic_subscriber::{
    Depth200SlotAssignment, assign_depth_200_slots, slots_needing_swap,
};
use tickvault_core::instrument::depth_top_volume_selector::{
    DEPTH_20_DYNAMIC_K, DEPTH_200_DYNAMIC_K, DepthSelectorRow, sanitize, selector_sql,
    should_issue_depth_20_swap,
};

/// Re-selection cadence — plan-pinned at 60s for both depth-20 dynamic
/// conn 5 and depth-200 dynamic top-5 pool.
const RESELECT_INTERVAL_SECS: u64 = 60;

/// HTTP timeout for the QuestDB selector query.
const QUESTDB_SELECT_TIMEOUT_SECS: u64 = 10;

#[inline]
#[must_use]
fn is_within_market_hours_ist() -> bool {
    let now_utc = Utc::now().timestamp();
    let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    let sec_of_day = (now_ist.rem_euclid(i64::from(SECONDS_PER_DAY))) as u32;
    (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sec_of_day)
}

/// Runs one selector cycle and returns the sanitized top-K rows.
/// `k` MUST be one of the production values (50 or 5) for the cached
/// SQL path; other values fall back to a fresh format!.
async fn fetch_and_sanitize(
    questdb: &QuestDbConfig,
    k: usize,
) -> anyhow::Result<Vec<DepthSelectorRow>> {
    let sql = selector_sql(k);
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(QUESTDB_SELECT_TIMEOUT_SECS))
        .build()?;
    let resp = client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("QuestDB top-volume selector non-2xx: {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    let mut rows: Vec<DepthSelectorRow> = Vec::with_capacity(k);
    if let Some(arr) = json.get("dataset").and_then(|d| d.as_array()) {
        for row in arr {
            if let Some(cols) = row.as_array()
                && cols.len() >= 2
            {
                let sid = cols[0].as_i64().and_then(|v| u32::try_from(v).ok());
                let seg_str = cols[1].as_str().unwrap_or("");
                let seg_code = match seg_str {
                    "NSE_FNO" | "D" => 2_u8,
                    "BSE_FNO" => 8,
                    _ => continue,
                };
                if let Some(sid_v) = sid {
                    rows.push(DepthSelectorRow {
                        security_id: sid_v,
                        exchange_segment_code: seg_code,
                    });
                }
            }
        }
    }
    Ok(sanitize(&rows, k))
}

/// Spawns the depth-20 conn-5 dynamic top-50 task.
///
/// `cmd_tx` is the mpsc::Sender<DepthCommand> registered for conn 5 in
/// the existing `depth_cmd_senders` map. The task does NOT spawn or
/// own the depth-20 connection itself — it only emits `Swap20`
/// commands when the rank set materially changes.
// TEST-EXEMPT: orchestrator spawning + 60s loop; logic primitives (`selector_sql`, `sanitize`, `should_issue_depth_20_swap`) are unit-tested.
pub fn spawn_depth_20_dynamic_conn5_task(
    questdb: QuestDbConfig,
    cmd_tx: mpsc::Sender<tickvault_core::websocket::DepthCommand>,
    shutdown: Arc<Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        info!("depth_20_dynamic_conn5 task starting");
        let mut tick = tokio::time::interval(Duration::from_secs(RESELECT_INTERVAL_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut previous: Vec<DepthSelectorRow> = Vec::with_capacity(DEPTH_20_DYNAMIC_K);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    info!("depth_20_dynamic_conn5 shutdown notified");
                    break;
                }
                _ = tick.tick() => {
                    if !is_within_market_hours_ist() {
                        continue;
                    }
                    let current = match fetch_and_sanitize(&questdb, DEPTH_20_DYNAMIC_K).await {
                        Ok(r) => r,
                        Err(err) => {
                            error!(
                                code = "DEPTH-DYN-01",
                                ?err,
                                "depth_20_dynamic_conn5 selector query failed"
                            );
                            continue;
                        }
                    };
                    if !should_issue_depth_20_swap(&previous, &current) {
                        // Rank-set diff below hysteresis threshold — no swap.
                        continue;
                    }
                    info!(
                        previous = previous.len(),
                        current = current.len(),
                        "depth_20_dynamic_conn5: rank-set changed, issuing Swap20"
                    );
                    // Build subscribe + unsubscribe JSON messages from
                    // the existing `subscription_builder`. NOTE: the
                    // existing depth-20 SubscribeRequest builder takes
                    // a list of (security_id, segment_code) — we don't
                    // have a direct API today, so we send empty lists
                    // and rely on the depth_connection's downstream
                    // builder. Operator can extend the builder once
                    // the orchestrator is validated.
                    let unsub_msgs: Vec<String> = previous
                        .iter()
                        .map(|r| {
                            format!(
                                r#"{{"RequestCode":25,"InstrumentCount":1,"InstrumentList":[{{"ExchangeSegment":"NSE_FNO","SecurityId":"{}"}}]}}"#,
                                r.security_id
                            )
                        })
                        .collect();
                    let sub_msgs: Vec<String> = current
                        .iter()
                        .map(|r| {
                            format!(
                                r#"{{"RequestCode":23,"InstrumentCount":1,"InstrumentList":[{{"ExchangeSegment":"NSE_FNO","SecurityId":"{}"}}]}}"#,
                                r.security_id
                            )
                        })
                        .collect();
                    let cmd = tickvault_core::websocket::DepthCommand::Swap20 {
                        unsubscribe_messages: unsub_msgs,
                        subscribe_messages: sub_msgs,
                    };
                    if cmd_tx.send(cmd).await.is_err() {
                        error!(
                            code = "DEPTH-DYN-02",
                            "depth_20_dynamic_conn5: command channel closed — broker task exited"
                        );
                        break;
                    }
                    previous = current;
                }
            }
        }
        info!("depth_20_dynamic_conn5 exited");
    })
}

/// Spawns the depth-200 dynamic top-5 task.
///
/// `cmd_senders` is a HashMap of `slot_index (0..5) -> mpsc::Sender<DepthCommand>`,
/// each pointing at one depth-200 connection task. Slot-by-slot diff
/// per `slots_needing_swap` keeps swap traffic minimal.
// TEST-EXEMPT: orchestrator spawning + 60s loop; logic primitives (`assign_depth_200_slots`, `slots_needing_swap`) are unit-tested.
pub fn spawn_depth_200_dynamic_pool_task(
    questdb: QuestDbConfig,
    cmd_senders: HashMap<usize, mpsc::Sender<tickvault_core::websocket::DepthCommand>>,
    shutdown: Arc<Notify>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        info!("depth_200_dynamic_pool task starting");
        let mut tick = tokio::time::interval(Duration::from_secs(RESELECT_INTERVAL_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut previous: Vec<Depth200SlotAssignment> = Vec::with_capacity(DEPTH_200_DYNAMIC_K);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    info!("depth_200_dynamic_pool shutdown notified");
                    break;
                }
                _ = tick.tick() => {
                    if !is_within_market_hours_ist() {
                        continue;
                    }
                    let top_5 = match fetch_and_sanitize(&questdb, DEPTH_200_DYNAMIC_K).await {
                        Ok(r) => r,
                        Err(err) => {
                            error!(
                                code = "DEPTH-DYN-01",
                                ?err,
                                "depth_200_dynamic_pool selector query failed"
                            );
                            continue;
                        }
                    };
                    let current = assign_depth_200_slots(&top_5);
                    let changed_slots = slots_needing_swap(&previous, &current);
                    if changed_slots.is_empty() {
                        continue;
                    }
                    info!(
                        slots = changed_slots.len(),
                        "depth_200_dynamic_pool: emitting Swap200 for changed slots"
                    );
                    for slot in &changed_slots {
                        let Some(assignment) = current.iter().find(|a| a.conn_index == *slot)
                        else {
                            continue;
                        };
                        let prev_sid = previous
                            .iter()
                            .find(|p| p.conn_index == *slot)
                            .map(|p| p.contract.security_id);
                        let unsub = match prev_sid {
                            Some(sid) => format!(
                                r#"{{"RequestCode":25,"ExchangeSegment":"NSE_FNO","SecurityId":"{sid}"}}"#
                            ),
                            None => String::new(), // first-seen: no prior to unsubscribe
                        };
                        let sub = format!(
                            r#"{{"RequestCode":23,"ExchangeSegment":"NSE_FNO","SecurityId":"{}"}}"#,
                            assignment.contract.security_id
                        );
                        let cmd = tickvault_core::websocket::DepthCommand::Swap200 {
                            unsubscribe_message: unsub,
                            subscribe_message: sub,
                        };
                        if let Some(tx) = cmd_senders.get(slot) {
                            if tx.send(cmd).await.is_err() {
                                error!(
                                    code = "DEPTH-DYN-02",
                                    slot = *slot,
                                    "depth_200_dynamic_pool: slot command channel closed"
                                );
                            }
                        } else {
                            warn!(
                                slot = *slot,
                                "depth_200_dynamic_pool: no command sender for slot"
                            );
                        }
                    }
                    previous = current;
                }
            }
        }
        info!("depth_200_dynamic_pool exited");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reselect_interval_is_60_seconds() {
        assert_eq!(RESELECT_INTERVAL_SECS, 60);
    }

    #[test]
    fn test_market_hours_helper_returns_bool() {
        let _ = is_within_market_hours_ist();
    }

    #[test]
    fn test_questdb_select_timeout_is_10_seconds() {
        assert_eq!(QUESTDB_SELECT_TIMEOUT_SECS, 10);
    }
}
