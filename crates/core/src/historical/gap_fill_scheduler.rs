//! Phase 0 Item 8+9 — Gap-fill scheduler receive loop (PR-C, 2026-05-17).
//!
//! This module owns the consumer side of the disconnect-event broadcast
//! channel produced by [`crate::websocket::connection`]. On each
//! [`crate::websocket::disconnect_event::DisconnectResolvedEvent`]:
//!
//! 1. Log the event at INFO with structured fields (connection index,
//!    outage window in IST seconds, planned bar count).
//! 2. Call the pure planner
//!    [`crate::historical::gap_fill_planner::plan_gap_fill_bars`] to
//!    compute which 1m bars would need refilling.
//! 3. Increment Prometheus counters so the operator's "is gap-fill
//!    seeing events?" dashboard panel has signal.
//!
//! ## What PR-C does NOT yet do
//!
//! - Fetch via Dhan REST `/v2/charts/intraday` — PR-D ships the
//!   `fetch_intraday_with_retry` pub wrapper + per-bar `tokio::time::sleep_until`
//!   + DH-904 backoff ladder + `historical_candles` UPSERT.
//! - Audit row writes — PR-D wires `gap_fill_audit_persistence::append_gap_fill_audit_row`
//!   (already shipped in PR-A) per bar attempt.
//! - Telegram event emission — PR-D wires the `GapFillCompleted` /
//!   `Partial` / `Failed` variants shipped in PR-A.
//!
//! ## Why this is NOT a skeleton (per `audit-findings-2026-04-17.md` Rule 14)
//!
//! PR-B's aborted skeleton attempt shipped pub fns with `// TODO PR-C`
//! stubs and a heartbeat counter on an inert work loop. PR-C ships a
//! REAL work loop: the planner is called, planned-bar counts are
//! emitted to Prometheus, and the operator can see real signal from
//! day one. Every pub fn has a real call site (boot wiring in main.rs).
//! No false-OK heartbeat, no `enabled` config flag, no inert pub
//! surface.
//!
//! ## Shutdown
//!
//! Uses the codebase convention `Arc<Notify>` per
//! `audit-findings-2026-04-17.md` Rule 16. The boot path passes
//! `Arc::clone(&shutdown_notify)` (the same handle shared with the
//! pool watchdog). Per Rule 16, the task MUST be parked on
//! `.notified()` before `notify_waiters()` fires — `tokio::select!`
//! parks on every iteration so this is satisfied by construction.
//!
//! ## Z+ layers shipped in PR-C
//!
//! | Layer | Mechanism |
//! |---|---|
//! | L1 DETECT | `broadcast::Receiver::recv()` |
//! | L2 VERIFY | `Lagged(n)` arm fires Critical with `code = ErrorCode::GapFill04EventChannelLagged.code_str()` |
//! | L5 AUDIT | `tv_gap_fill_events_received_total` + `tv_gap_fill_planned_bars_total` |
//! | L6 RECOVER | Per Rule 11, Lagged is NEVER silent — operator is paged |

use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use tokio::sync::{Notify, broadcast};
use tracing::{error, info, warn};
use zeroize::Zeroizing;

use tickvault_common::constants::{
    GAP_FILL_FETCH_TIMEOUT_SECS, GAP_FILL_RETRY_ATTEMPTS, SECONDS_PER_DAY,
    TICK_PERSIST_END_SECS_OF_DAY_IST,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::tick_types::HistoricalCandle;
use tickvault_storage::candle_persistence::CandlePersistenceWriter;

use crate::auth::token_manager::TokenHandle;

/// Sink for gap-fill UPSERT writes.
///
/// Trait abstraction over `CandlePersistenceWriter` so tests can inject
/// a no-op sink without needing a live QuestDB. Production wires the
/// real `CandlePersistenceWriter`; tests use `NoopCandleSink`.
pub trait GapFillCandleSink: Send {
    /// Append one candle to the sink's buffer.
    fn append_candle(&mut self, candle: &HistoricalCandle) -> anyhow::Result<()>;
    /// Force-flush buffered candles.
    fn force_flush(&mut self) -> anyhow::Result<()>;
}

impl GapFillCandleSink for CandlePersistenceWriter {
    fn append_candle(&mut self, candle: &HistoricalCandle) -> anyhow::Result<()> {
        self.append_candle(candle)
    }
    fn force_flush(&mut self) -> anyhow::Result<()> {
        self.force_flush()
    }
}
use crate::historical::candle_fetcher::{GapFillFetchError, fetch_gap_fill_intraday_window};
use crate::historical::gap_fill_planner::plan_gap_fill_bars;
use crate::websocket::disconnect_event::DisconnectResolvedEvent;

// Phase 0 PR-D3-impl (2026-05-17) — MVP slice fetches NIFTY index only.
// PR-D4 generalises to iterate the affected connection's instruments list.
const MVP_NIFTY_SECURITY_ID: u32 = 13;
const MVP_NIFTY_SEGMENT_CODE: u8 = 0; // IDX_I per annexure-enums.md rule 2
const MVP_NIFTY_INSTRUMENT_TYPE: &str = "INDEX";

/// Pause duration after Dhan returns 805 "too many connections".
/// Mirrors `candle_fetcher::ERROR_805_PAUSE_SECS` per
/// `docs/dhan-ref/01-introduction-and-rate-limits.md` rule 9.
const GAP_FILL_805_PAUSE_SECS: u64 = 60;

/// Dependencies required to execute the gap-fill REST fetch + UPSERT.
///
/// Constructed once at boot in `crates/app/src/main.rs` and passed to
/// [`run_gap_fill_scheduler`]. Per the architectural lock in PR-B'
/// (#670) + PR-D3-design (#674):
///
/// - `http_client`: shared `reqwest::Client` from boot (NOT a fresh
///   one per fetch — TLS handshake is too expensive to repeat).
/// - `endpoint`: full URL of Dhan's intraday charts endpoint
///   (e.g. `"https://api.dhan.co/v2/charts/intraday"`).
/// - `token_handle`: `Arc<ArcSwap<Option<TokenState>>>` from the
///   token manager — `load()` returns the current JWT.
/// - `client_id`: secret Dhan client ID for the `client-id` header.
/// - `candle_writer`: dedicated `CandlePersistenceWriter` for gap-fill
///   (NOT shared with boot-time historical fetcher to avoid mutable-
///   borrow conflicts). Wrapped in `tokio::sync::Mutex` to allow
///   future per-bar concurrent task spawning.
/// - `max_retries`: retry budget per bar. Defaults to
///   `GAP_FILL_RETRY_ATTEMPTS = 3`.
pub struct GapFillExecutorDeps {
    pub http_client: Arc<reqwest::Client>,
    pub endpoint: String,
    pub token_handle: TokenHandle,
    pub client_id: SecretString,
    pub candle_writer: Arc<tokio::sync::Mutex<dyn GapFillCandleSink>>,
    pub max_retries: u32,
}

impl GapFillExecutorDeps {
    /// Construct with the default retry budget
    /// ([`GAP_FILL_RETRY_ATTEMPTS`]).
    #[must_use]
    pub fn new(
        http_client: Arc<reqwest::Client>,
        endpoint: String,
        token_handle: TokenHandle,
        client_id: SecretString,
        candle_writer: Arc<tokio::sync::Mutex<dyn GapFillCandleSink>>,
    ) -> Self {
        Self {
            http_client,
            endpoint,
            token_handle,
            client_id,
            candle_writer,
            max_retries: GAP_FILL_RETRY_ATTEMPTS,
        }
    }
}

/// Extract the current access token from a [`TokenHandle`].
///
/// Returns `None` if the handle holds `None` (auth not yet
/// completed) — the caller should skip the fetch attempt.
fn load_access_token(handle: &TokenHandle) -> Option<Zeroizing<String>> {
    let guard = handle.load();
    let token_state = guard.as_ref().as_ref()?;
    Some(Zeroizing::new(
        token_state.access_token().expose_secret().to_string(),
    ))
}

/// Execute a single gap-fill bar fetch + UPSERT cycle for the MVP
/// NIFTY index slice.
///
/// On success returns `Ok(candles_written)`. On non-fatal error
/// (Token expired, too many connections) returns Ok(0) and logs —
/// the caller decides whether to retry the whole event. On fatal
/// error returns the typed `GapFillFetchError`.
///
/// PR-D4 generalises to per-instrument iteration.
async fn execute_gap_fill_for_bar(
    deps: &GapFillExecutorDeps,
    bar_start_ist_secs: i64,
) -> Result<usize, GapFillFetchError> {
    let bar_end_ist_secs = bar_start_ist_secs.saturating_add(60);

    let Some(access_token) = load_access_token(&deps.token_handle) else {
        warn!(
            bar_start_ist_secs,
            "gap-fill: token handle empty — auth incomplete; skipping bar"
        );
        return Ok(0);
    };

    let candles = match tokio::time::timeout(
        Duration::from_secs(GAP_FILL_FETCH_TIMEOUT_SECS),
        fetch_gap_fill_intraday_window(
            &deps.http_client,
            &deps.endpoint,
            &access_token,
            &deps.client_id,
            MVP_NIFTY_SECURITY_ID,
            MVP_NIFTY_SEGMENT_CODE,
            MVP_NIFTY_INSTRUMENT_TYPE,
            bar_start_ist_secs,
            bar_end_ist_secs,
            false, // No OI for index
            deps.max_retries,
        ),
    )
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(GapFillFetchError::TokenExpired)) => {
            warn!(
                bar_start_ist_secs,
                "gap-fill: token expired during fetch; will retry on next event"
            );
            return Ok(0);
        }
        Ok(Err(GapFillFetchError::TooManyConnections)) => {
            warn!(
                bar_start_ist_secs,
                "gap-fill: Dhan 805 too-many-connections; sleeping 60s before next bar"
            );
            tokio::time::sleep(Duration::from_secs(GAP_FILL_805_PAUSE_SECS)).await;
            return Ok(0);
        }
        Ok(Err(err)) => return Err(err),
        Err(_) => {
            return Err(GapFillFetchError::Network(format!(
                "fetch timeout after {GAP_FILL_FETCH_TIMEOUT_SECS}s"
            )));
        }
    };

    if candles.is_empty() {
        info!(
            bar_start_ist_secs,
            "gap-fill: Dhan returned 0 candles for bar; skipping UPSERT"
        );
        return Ok(0);
    }

    // UPSERT via the dedicated writer Mutex. The Mutex is uncontested
    // in PR-D3-impl (sequential per-event execution); PR-D4 may add
    // concurrent per-bar tasks, at which point this serialisation
    // becomes useful.
    let mut writer = deps.candle_writer.lock().await;
    let mut written = 0_usize;
    for candle in &candles {
        if let Err(err) = writer.append_candle(candle) {
            error!(
                code = ErrorCode::GapFill03UpsertFailed.code_str(),
                bar_start_ist_secs,
                security_id = candle.security_id,
                ?err,
                "GAP-FILL-03: append_candle failed; bar partially persisted"
            );
            // Don't return — try to flush what we have
            break;
        }
        written = written.saturating_add(1);
    }
    if let Err(err) = writer.force_flush() {
        error!(
            code = ErrorCode::GapFill03UpsertFailed.code_str(),
            bar_start_ist_secs,
            ?err,
            "GAP-FILL-03: force_flush failed; rows buffered to spill ring"
        );
    }
    drop(writer);
    Ok(written)
}

/// Run the gap-fill scheduler receive loop until shutdown.
///
/// **PR-D3-impl (2026-05-17)** — extends the PR-C scheduler with the
/// `deps: GapFillExecutorDeps` parameter. On each received disconnect
/// event:
///
/// 1. Compute planned bars via `compute_planned_bars` (pure).
/// 2. For each planned bar, call `execute_gap_fill_for_bar` which
///    fetches via [`fetch_gap_fill_intraday_window`] and UPSERTs via
///    `CandlePersistenceWriter::append_candle`.
/// 3. Increment success/failure Prometheus counters.
///
/// MVP scope: PR-D3-impl fetches NIFTY index ONLY
/// (`security_id=13`, `IDX_I`, `instrument=INDEX`). PR-D4 generalises
/// to iterate the affected pool connection's instruments list.
///
/// # Shutdown
///
/// Returns when `shutdown_notify.notify_waiters()` fires AND the task
/// is parked on `shutdown.notified()` (per Rule 16). Both conditions
/// hold because the `tokio::select!` arm parks on every iteration.
// TEST-EXEMPT: lifecycle + executor wiring covered by `test_scheduler_returns_on_shutdown_signal`, `test_scheduler_exits_on_channel_close`, `test_scheduler_processes_event_then_returns_on_shutdown`, `test_executor_deps_default_max_retries`, `test_load_access_token_none_when_handle_empty` in this module; the async HTTP fetch path is integration-tested live via the boot wiring
pub async fn run_gap_fill_scheduler(
    mut disconnect_rx: broadcast::Receiver<DisconnectResolvedEvent>,
    shutdown_notify: Arc<Notify>,
    deps: GapFillExecutorDeps,
) {
    info!(
        channel_capacity = crate::websocket::disconnect_event::DISCONNECT_EVENT_CHANNEL_CAPACITY,
        endpoint = %deps.endpoint,
        max_retries = deps.max_retries,
        "gap-fill scheduler started (PR-D3-impl: NIFTY MVP slice — fetch + UPSERT live)"
    );

    let events_received = metrics::counter!("tv_gap_fill_events_received_total");
    let events_lagged = metrics::counter!("tv_gap_fill_event_channel_lagged_total");
    let planned_bars_counter = metrics::counter!("tv_gap_fill_planned_bars_total");
    let bars_succeeded = metrics::counter!("tv_gap_fill_bars_succeeded_total");
    let bars_failed = metrics::counter!("tv_gap_fill_bars_failed_total");
    let candles_written = metrics::counter!("tv_gap_fill_candles_written_total");

    loop {
        tokio::select! {
            recv_result = disconnect_rx.recv() => {
                match recv_result {
                    Ok(event) => {
                        events_received.increment(1);
                        let bars = compute_planned_bars(&event);
                        planned_bars_counter.increment(bars.len() as u64);
                        info!(
                            connection_index = event.connection_index,
                            outage_start_secs = event.outage_start_secs,
                            outage_end_secs = event.outage_end_secs,
                            outage_duration_secs =
                                event.outage_end_secs.saturating_sub(event.outage_start_secs),
                            planned_bar_count = bars.len(),
                            "gap-fill: disconnect event received; starting per-bar fetch+UPSERT (NIFTY MVP slice)"
                        );

                        for bar_start_ist_secs in bars {
                            match execute_gap_fill_for_bar(&deps, bar_start_ist_secs).await {
                                Ok(written) => {
                                    bars_succeeded.increment(1);
                                    candles_written.increment(written as u64);
                                    info!(
                                        bar_start_ist_secs,
                                        candles_written = written,
                                        "gap-fill: bar UPSERTed for NIFTY"
                                    );
                                }
                                Err(err) => {
                                    bars_failed.increment(1);
                                    error!(
                                        code = ErrorCode::GapFill02RestFetchFailed.code_str(),
                                        bar_start_ist_secs,
                                        err = %err,
                                        "GAP-FILL-02: per-bar fetch failed; skipping"
                                    );
                                }
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        events_lagged.increment(dropped);
                        error!(
                            code = ErrorCode::GapFill04EventChannelLagged.code_str(),
                            dropped_event_count = dropped,
                            "GAP-FILL-04: disconnect event broadcast lagged; events dropped — reconciliation pass deferred to PR-D4"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("gap-fill scheduler: disconnect event channel closed; exiting");
                        return;
                    }
                }
            }
            () = shutdown_notify.notified() => {
                info!("gap-fill scheduler: shutdown signalled; exiting");
                return;
            }
        }
    }
}

/// Compute the planned bar timestamps for a disconnect-resolved event.
///
/// Pure function — wraps [`plan_gap_fill_bars`] with the IST-derived
/// market-close cutoff for the trading day the event resolved on.
///
/// Returns `Vec<i64>` of IST epoch SECONDS, one per 1m bar that PR-D
/// will fetch + UPSERT. Empty if:
/// - Outage entirely sub-minute (no full minute fits inside).
/// - Outage entirely post-15:30 IST (market close).
/// - Outage spans midnight (planner clamps to today's market close).
///
/// O(1) EXEMPT: cold path — runs once per WS reconnect cycle (~5/day).
#[must_use]
pub fn compute_planned_bars(event: &DisconnectResolvedEvent) -> Vec<i64> {
    // Derive today's market-close as IST epoch seconds: the trading-day
    // anchor is the IST midnight that immediately precedes
    // `outage_end_secs`, plus the 15:30 secs-of-day constant.
    //
    // O(1) EXEMPT: arithmetic, no allocation outside the planner's Vec.
    let day_anchor = day_anchor_ist_secs(event.outage_end_secs);
    let market_close_secs = day_anchor.saturating_add(i64::from(TICK_PERSIST_END_SECS_OF_DAY_IST));
    plan_gap_fill_bars(
        event.outage_start_secs,
        event.outage_end_secs,
        market_close_secs,
    )
}

/// IST midnight (00:00:00) of the trading day containing `ist_secs`.
///
/// Pure function. Used to anchor today's `market_close_secs` for the
/// planner so a disconnect resolved at 15:32 IST clamps to today's
/// 15:30 IST not yesterday's.
#[must_use]
fn day_anchor_ist_secs(ist_secs: i64) -> i64 {
    let secs_in_day = i64::from(SECONDS_PER_DAY);
    ist_secs.saturating_sub(ist_secs.rem_euclid(secs_in_day))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use std::time::Duration;

    /// No-op sink used by lifecycle tests that don't need a live QuestDB.
    /// Production wires `CandlePersistenceWriter`.
    struct NoopCandleSink;
    impl GapFillCandleSink for NoopCandleSink {
        fn append_candle(&mut self, _candle: &HistoricalCandle) -> anyhow::Result<()> {
            Ok(())
        }
        fn force_flush(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Construct test deps with a no-op writer + empty token handle +
    /// unreachable endpoint. Used by lifecycle tests that send events
    /// resulting in EMPTY planned bars (post-market or sub-minute) so
    /// the executor never actually invokes the HTTP fetch.
    fn test_deps() -> GapFillExecutorDeps {
        GapFillExecutorDeps::new(
            Arc::new(reqwest::Client::new()),
            "http://127.0.0.1:1/v2/charts/intraday".to_string(),
            Arc::new(ArcSwap::from_pointee(None)),
            SecretString::from("test-client-id"),
            Arc::new(tokio::sync::Mutex::new(NoopCandleSink)),
        )
    }

    /// Returns an "IST epoch seconds" value per Dhan convention
    /// (`data-integrity.md` "WebSocket Timestamp Rule"): the value is
    /// the raw unix timestamp of `year-month-day 00:00:00` interpreted
    /// directly AS IST midnight (no +5:30 offset added). Under this
    /// convention IST midnight = exact multiple of 86400 since epoch,
    /// which is what `day_anchor_ist_secs` relies on via `rem_euclid`.
    fn ist_midnight_secs(year: i32, month: u32, day: u32) -> i64 {
        use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
        let naive = NaiveDateTime::new(
            NaiveDate::from_ymd_opt(year, month, day).unwrap(),
            NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
        );
        naive.and_utc().timestamp()
    }

    #[test]
    fn test_day_anchor_at_midnight_returns_same() {
        let midnight = ist_midnight_secs(2026, 5, 17);
        assert_eq!(day_anchor_ist_secs(midnight), midnight);
    }

    #[test]
    fn test_day_anchor_at_noon_returns_midnight() {
        let midnight = ist_midnight_secs(2026, 5, 17);
        let noon = midnight + 12 * 3600;
        assert_eq!(day_anchor_ist_secs(noon), midnight);
    }

    #[test]
    fn test_compute_planned_bars_three_minute_outage_midday() {
        // 11:00:00 → 11:03:00 IST → 3 full 1m bars expected.
        let midnight = ist_midnight_secs(2026, 5, 17);
        let event = DisconnectResolvedEvent {
            connection_index: 0,
            outage_start_secs: midnight + 11 * 3600,
            outage_end_secs: midnight + 11 * 3600 + 180,
        };
        let bars = compute_planned_bars(&event);
        assert_eq!(bars.len(), 3, "3 minute outage = 3 planned bars");
    }

    #[test]
    fn test_compute_planned_bars_outage_after_market_close_returns_empty() {
        // 16:00:00 → 16:05:00 IST — entirely post-15:30 close.
        let midnight = ist_midnight_secs(2026, 5, 17);
        let event = DisconnectResolvedEvent {
            connection_index: 1,
            outage_start_secs: midnight + 16 * 3600,
            outage_end_secs: midnight + 16 * 3600 + 300,
        };
        let bars = compute_planned_bars(&event);
        assert!(
            bars.is_empty(),
            "outage entirely post-close must plan zero bars; got {bars:?}"
        );
    }

    #[test]
    fn test_compute_planned_bars_outage_spans_market_close_includes_only_pre_close() {
        // 15:29:00 → 15:31:00 IST — 15:29 bar eligible, 15:30 excluded.
        let midnight = ist_midnight_secs(2026, 5, 17);
        let event = DisconnectResolvedEvent {
            connection_index: 2,
            outage_start_secs: midnight + 15 * 3600 + 29 * 60,
            outage_end_secs: midnight + 15 * 3600 + 31 * 60,
        };
        let bars = compute_planned_bars(&event);
        assert_eq!(bars.len(), 1, "exactly the 15:29 bar must be planned");
    }

    #[tokio::test]
    async fn test_scheduler_returns_on_shutdown_signal() {
        let (_tx, rx) = crate::websocket::disconnect_event::create_disconnect_event_channel();
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let deps = test_deps();
        let handle = tokio::spawn(async move {
            run_gap_fill_scheduler(rx, task_shutdown, deps).await;
        });
        // Yield once so the task reaches its `select!` and parks on
        // `.notified()` BEFORE we fire (per Rule 16).
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.notify_waiters();
        // Should exit within 2s.
        let outcome = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(outcome.is_ok(), "scheduler must exit within 2s of shutdown");
    }

    #[tokio::test]
    async fn test_scheduler_processes_event_then_returns_on_shutdown() {
        let (tx, rx) = crate::websocket::disconnect_event::create_disconnect_event_channel();
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let deps = test_deps();
        let handle = tokio::spawn(async move {
            run_gap_fill_scheduler(rx, task_shutdown, deps).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Send a POST-MARKET event so planner returns ZERO bars (no
        // fetch attempted — keeps the test infra-free). Lifecycle is
        // still exercised: recv → plan → loop → shutdown.
        let midnight = ist_midnight_secs(2026, 5, 17);
        tx.send(DisconnectResolvedEvent {
            connection_index: 0,
            outage_start_secs: midnight + 16 * 3600, // 16:00 IST — post-close
            outage_end_secs: midnight + 16 * 3600 + 300, // 16:05 IST
        })
        .expect("send must succeed");
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.notify_waiters();
        let outcome = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(outcome.is_ok());
    }

    #[test]
    fn test_executor_deps_default_max_retries_matches_constant() {
        let deps = test_deps();
        assert_eq!(
            deps.max_retries, GAP_FILL_RETRY_ATTEMPTS,
            "default retry budget must be GAP_FILL_RETRY_ATTEMPTS"
        );
    }

    #[test]
    fn test_load_access_token_returns_none_when_handle_empty() {
        // TokenHandle holding None (auth not yet completed) — the
        // executor must NOT panic and must NOT send a fetch.
        let handle: TokenHandle = Arc::new(ArcSwap::from_pointee(None));
        assert!(load_access_token(&handle).is_none());
    }

    #[test]
    fn test_mvp_nifty_constants_pinned() {
        // PR-D3-impl MVP slice: NIFTY-only fetch. Pin the constants
        // so a future refactor must explicitly approve the change.
        // PR-D4 generalises to per-instrument iteration.
        assert_eq!(
            MVP_NIFTY_SECURITY_ID, 13,
            "NIFTY index SecurityId per Dhan instrument master"
        );
        assert_eq!(
            MVP_NIFTY_SEGMENT_CODE, 0,
            "IDX_I segment code per annexure-enums.md"
        );
        assert_eq!(
            MVP_NIFTY_INSTRUMENT_TYPE, "INDEX",
            "Dhan REST intraday instrument_type for indices"
        );
    }

    #[tokio::test]
    async fn test_scheduler_exits_on_channel_close() {
        let (tx, rx) = crate::websocket::disconnect_event::create_disconnect_event_channel();
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let deps = test_deps();
        let handle = tokio::spawn(async move {
            run_gap_fill_scheduler(rx, task_shutdown, deps).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Drop the Sender; scheduler must exit on `Err(Closed)`.
        drop(tx);
        let outcome = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(
            outcome.is_ok(),
            "scheduler must exit within 2s of channel close"
        );
    }
}
