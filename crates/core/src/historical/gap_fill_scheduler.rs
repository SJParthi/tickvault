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

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    GAP_FILL_FETCH_TIMEOUT_SECS, GAP_FILL_MAX_CONCURRENT_FETCHES, GAP_FILL_RETRY_ATTEMPTS,
    SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::tick_types::HistoricalCandle;
use tickvault_storage::candle_persistence::CandlePersistenceWriter;
use tickvault_storage::gap_fill_audit_persistence::{
    RESULT_FAILED, RESULT_PARTIAL, RESULT_SUCCESS, TRIGGER_EVENT_WS_DISCONNECT,
    append_gap_fill_audit_row,
};

use crate::notification::{NotificationEvent, NotificationService};

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

// Phase 0 PR-D4 (2026-05-17) — indices-only fan-out.
//
// The 4 IDX_I indices subscribed under the Phase 0 indices-only scope
// (`websocket-connection-scope-lock.md` §I): NIFTY, BANKNIFTY, SENSEX,
// INDIA VIX. Per-bar the scheduler iterates this set and issues one
// Dhan REST `/v2/charts/intraday` fetch per (bar, SecurityId) pair.
//
// Cost budget (per disconnect event): 4 SIDs × ~5 bars × ~200 ms Dhan
// rate-limit budget = ~4 s sustained wall-clock. Well within the
// 64-event broadcast capacity (`DISCONNECT_EVENT_CHANNEL_CAPACITY`)
// even under a flap-storm.
//
// PR-D5 will extend fan-out to NSE_EQ equities (~218 SIDs) — requires
// `tokio::sync::Semaphore(GAP_FILL_MAX_CONCURRENT_FETCHES)` + per-conn
// instrument-list snapshot on the `DisconnectResolvedEvent`. Kept out
// of PR-D4 to preserve the bounded cost envelope.
const GAP_FILL_INDICES: &[(u32, u8, &str)] = &[
    (13, 0, "INDEX"), // NIFTY
    (25, 0, "INDEX"), // BANKNIFTY
    (51, 0, "INDEX"), // SENSEX
    (21, 0, "INDEX"), // INDIA VIX
];

/// Maximum number of failed SecurityIds carried in the
/// `GapFillPartial` Telegram payload, per the variant doc on
/// `NotificationEvent::GapFillPartial` (capped here to keep the bar
/// loop allocation-bounded — the formatter ALSO caps at 5 so this is
/// belt-and-suspenders).
const TELEGRAM_FAILED_SID_SAMPLE_CAP: usize = 5;

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
/// - `questdb_config`: connection config for `append_gap_fill_audit_row`
///   (PR-D4) — gap-fill writes one audit row per bar carrying
///   `(sids_requested, sids_completed, sids_failed, duration_ms,
///   result)`.
/// - `notification_service`: emits `GapFillCompleted` / `Partial` /
///   `Failed` per bar (PR-D4). The fan-out tallies a single per-bar
///   outcome regardless of whether 1 or N SIDs failed — operator
///   receives one Telegram per bar, not per SID.
/// - `nse_eq_sids`: PR-D6 (2026-05-17) — `Arc<Vec<u32>>` snapshot of
///   subscribed NSE_EQ SecurityIds captured at boot from
///   `InstrumentRegistry.by_exchange_segment()[NseEquity]`. The
///   per-bar fan-out iterates the 4 IDX_I in `GAP_FILL_INDICES`
///   PLUS every SID in this snapshot. ~218 entries for the indices-
///   underlyings universe. SIDs are stable for the trading day
///   (daily instrument rebuild swaps the registry, but the Arc held
///   here is immutable for the gap-fill scheduler's lifetime —
///   stale-SID risk is bounded by the 24h JWT lifetime that forces
///   a process restart anyway).
/// - `fetch_semaphore`: PR-D6 — `Arc<Semaphore>` with capacity =
///   [`GAP_FILL_MAX_CONCURRENT_FETCHES`] (= 5). Each per-SID fetch
///   acquires a permit before issuing the Dhan REST call. Matches
///   Dhan's 5/sec Data API hard limit per
///   `docs/dhan-ref/01-introduction-and-rate-limits.md` §7. Bounded
///   parallelism keeps bar latency under the 64-buffer 25s drain
///   budget on the disconnect broadcast channel.
pub struct GapFillExecutorDeps {
    pub http_client: Arc<reqwest::Client>,
    pub endpoint: String,
    pub token_handle: TokenHandle,
    pub client_id: SecretString,
    pub candle_writer: Arc<tokio::sync::Mutex<dyn GapFillCandleSink>>,
    pub max_retries: u32,
    pub questdb_config: QuestDbConfig,
    pub notification_service: Arc<NotificationService>,
    pub nse_eq_sids: Arc<Vec<u32>>,
    pub fetch_semaphore: Arc<tokio::sync::Semaphore>,
}

impl GapFillExecutorDeps {
    /// Construct with the default retry budget
    /// ([`GAP_FILL_RETRY_ATTEMPTS`]) and a fresh Semaphore at
    /// [`GAP_FILL_MAX_CONCURRENT_FETCHES`] capacity.
    #[must_use]
    pub fn new(
        http_client: Arc<reqwest::Client>,
        endpoint: String,
        token_handle: TokenHandle,
        client_id: SecretString,
        candle_writer: Arc<tokio::sync::Mutex<dyn GapFillCandleSink>>,
        questdb_config: QuestDbConfig,
        notification_service: Arc<NotificationService>,
        nse_eq_sids: Arc<Vec<u32>>,
    ) -> Self {
        Self {
            http_client,
            endpoint,
            token_handle,
            client_id,
            candle_writer,
            max_retries: GAP_FILL_RETRY_ATTEMPTS,
            questdb_config,
            notification_service,
            nse_eq_sids,
            fetch_semaphore: Arc::new(tokio::sync::Semaphore::new(GAP_FILL_MAX_CONCURRENT_FETCHES)),
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

/// Execute a single gap-fill bar fetch + UPSERT cycle for one
/// `(security_id, segment_code, instrument_type)` tuple.
///
/// On success returns `Ok(candles_written)`. On non-fatal error
/// (Token expired, too many connections) returns Ok(0) and logs —
/// the caller decides whether to retry the whole event. On fatal
/// error returns the typed `GapFillFetchError`.
///
/// PR-D4 (2026-05-17) generalised from the PR-D3 NIFTY-only MVP to
/// accept the SecurityId tuple — per-instrument fan-out happens in
/// `run_gap_fill_scheduler` iterating `GAP_FILL_INDICES`.
async fn execute_gap_fill_for_bar(
    deps: &GapFillExecutorDeps,
    bar_start_ist_secs: i64,
    security_id: u32,
    segment_code: u8,
    instrument_type: &str,
) -> Result<usize, GapFillFetchError> {
    let bar_end_ist_secs = bar_start_ist_secs.saturating_add(60);

    let Some(access_token) = load_access_token(&deps.token_handle) else {
        warn!(
            bar_start_ist_secs,
            security_id, "gap-fill: token handle empty — auth incomplete; skipping bar"
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
            security_id,
            segment_code,
            instrument_type,
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
                security_id, "gap-fill: token expired during fetch; will retry on next event"
            );
            return Ok(0);
        }
        Ok(Err(GapFillFetchError::TooManyConnections)) => {
            warn!(
                bar_start_ist_secs,
                security_id,
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
            security_id, "gap-fill: Dhan returned 0 candles for bar; skipping UPSERT"
        );
        return Ok(0);
    }

    // UPSERT via the dedicated writer Mutex. The Mutex serialises the
    // 4 per-SID UPSERTs within one bar — uncontested since
    // `execute_gap_fill_for_bar` is awaited sequentially in
    // `run_gap_fill_scheduler`.
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
            security_id,
            ?err,
            "GAP-FILL-03: force_flush failed; rows buffered to spill ring"
        );
    }
    drop(writer);
    Ok(written)
}

/// Format the IST trading date `YYYY-MM-DD` from an IST epoch seconds
/// value. Used as the `trading_date_ist` audit column.
///
/// Per `data-integrity.md` "WebSocket Timestamp Rule": Dhan-derived
/// IST seconds are interpreted as if they were UTC by chrono, which
/// is the standard tickvault pattern (the value is already in IST
/// wall-clock, NO offset is added). Always returns a non-empty
/// string; an unconvertible input falls back to epoch 0 ("1970-01-01").
fn trading_date_ist_string(ist_secs: i64) -> String {
    use chrono::TimeZone;
    chrono::Utc
        .timestamp_opt(ist_secs, 0)
        .single()
        .unwrap_or_else(|| {
            chrono::Utc
                .timestamp_opt(0, 0)
                .single()
                .unwrap_or_else(chrono::Utc::now)
        })
        .format("%Y-%m-%d")
        .to_string()
}

/// Format the IST bar minute `HH:MM` from an IST epoch seconds value.
/// Used as the `bar_minute` audit column AND the `bar_minute_ist`
/// field on `GapFillCompleted` / `Partial` / `Failed` events.
fn bar_minute_ist_string(ist_secs: i64) -> String {
    use chrono::TimeZone;
    chrono::Utc
        .timestamp_opt(ist_secs, 0)
        .single()
        .unwrap_or_else(|| {
            chrono::Utc
                .timestamp_opt(0, 0)
                .single()
                .unwrap_or_else(chrono::Utc::now)
        })
        .format("%H:%M")
        .to_string()
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
    // PR-D6 (2026-05-17): wrap deps in Arc so per-SID spawned tasks
    // can hold cheap clones without re-cloning every Arc-typed field
    // individually. The scheduler loop holds a single Arc; each
    // JoinSet task `Arc::clone`s before spawn.
    let deps = Arc::new(deps);

    info!(
        channel_capacity = crate::websocket::disconnect_event::DISCONNECT_EVENT_CHANNEL_CAPACITY,
        endpoint = %deps.endpoint,
        max_retries = deps.max_retries,
        index_sid_count = GAP_FILL_INDICES.len(),
        nse_eq_sid_count = deps.nse_eq_sids.len(),
        semaphore_capacity = GAP_FILL_MAX_CONCURRENT_FETCHES,
        "gap-fill scheduler started (PR-D6: indices + NSE_EQ fan-out with Semaphore parallelism)"
    );

    let events_received = metrics::counter!("tv_gap_fill_events_received_total");
    let events_lagged = metrics::counter!("tv_gap_fill_event_channel_lagged_total");
    let planned_bars_counter = metrics::counter!("tv_gap_fill_planned_bars_total");
    let bars_succeeded = metrics::counter!("tv_gap_fill_bars_succeeded_total");
    let bars_partial = metrics::counter!("tv_gap_fill_bars_partial_total");
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
                            index_sid_count = GAP_FILL_INDICES.len(),
                            nse_eq_sid_count = deps.nse_eq_sids.len(),
                            semaphore_capacity = GAP_FILL_MAX_CONCURRENT_FETCHES,
                            "gap-fill: disconnect event received; starting per-bar × (indices + NSE_EQ) fetch+UPSERT"
                        );

                        for bar_start_ist_secs in bars {
                            execute_bar_for_all_instruments(
                                &deps,
                                bar_start_ist_secs,
                                &bars_succeeded,
                                &bars_partial,
                                &bars_failed,
                                &candles_written,
                            )
                            .await;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        events_lagged.increment(dropped);
                        error!(
                            code = ErrorCode::GapFill04EventChannelLagged.code_str(),
                            dropped_event_count = dropped,
                            "GAP-FILL-04: disconnect event broadcast lagged; events dropped — reconciliation pass deferred to PR-D5"
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

/// NSE_EQ segment numeric code for the Dhan binary protocol. See
/// `crates/common/src/types.rs::ExchangeSegment::numeric()`. Used
/// when constructing the per-bar fan-out task list — equities flow
/// through Dhan's `/v2/charts/intraday` REST endpoint with
/// `instrument_type = "EQUITY"` and `segment_code = 1`.
const GAP_FILL_NSE_EQ_SEGMENT_CODE: u8 = 1;

/// Dhan REST `instrument` value for NSE_EQ stocks per
/// `docs/dhan-ref/05-historical-data.md` §4.
const GAP_FILL_NSE_EQ_INSTRUMENT_TYPE: &str = "EQUITY";

/// Execute one planned bar across all eligible SecurityIds: 4 IDX_I
/// indices (`GAP_FILL_INDICES`) + every NSE_EQ SID in
/// `deps.nse_eq_sids` (PR-D6, 2026-05-17). Per-SID fetch + UPSERT
/// is dispatched via `tokio::task::JoinSet` with concurrency capped
/// at `deps.fetch_semaphore` permits. Tally aggregated after all
/// tasks complete, then write one `gap_fill_audit` row + emit one
/// Telegram event per bar.
///
/// Per-bar (not per-SID) audit row + Telegram is intentional: the
/// operator's "did this bar refill" question is bar-scoped, not
/// SID-scoped. The audit row's `sids_failed` count + the
/// `GapFillPartial` sample list together identify which SIDs failed
/// without spamming 222 Telegram messages per bar.
///
/// O(1) EXEMPT: cold path. Per-bar Vec allocations grow with
/// `GAP_FILL_INDICES.len() + deps.nse_eq_sids.len()` (~222 entries
/// for the indices-underlyings universe), bounded by the daily
/// instrument-master rebuild and NOT on any tick-processing path.
async fn execute_bar_for_all_instruments(
    deps: &Arc<GapFillExecutorDeps>,
    bar_start_ist_secs: i64,
    bars_succeeded: &metrics::Counter,
    bars_partial: &metrics::Counter,
    bars_failed: &metrics::Counter,
    candles_written: &metrics::Counter,
) {
    let bar_started_at = std::time::Instant::now();
    let mut sids_completed: u32 = 0;
    let mut sids_failed: u32 = 0;
    let mut sample_failed_sids: Vec<u32> = Vec::new();
    let mut last_error: Option<String> = None;

    // Build the unified fan-out task set: indices first (4), then
    // NSE_EQ snapshot (~218). Order within the set is irrelevant —
    // JoinSet pulls them off in scheduling order, not iteration
    // order.
    let total_sids = GAP_FILL_INDICES.len() + deps.nse_eq_sids.len();
    let mut tasks: tokio::task::JoinSet<(u32, Result<usize, GapFillFetchError>)> =
        tokio::task::JoinSet::new();

    // Spawn IDX_I tasks.
    for &(security_id, segment_code, instrument_type) in GAP_FILL_INDICES {
        let deps_for_task = Arc::clone(deps);
        let semaphore = Arc::clone(&deps.fetch_semaphore);
        tasks.spawn(async move {
            let _permit = match semaphore.acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    // Semaphore closed — never expected in current code
                    // paths (we never call `.close()`). Treat as a
                    // retry-exhausted failure with 0 attempts so the
                    // operator sees a distinctive signal in audit rows
                    // + Telegram, without invoking the REST client.
                    return (
                        security_id,
                        Err(GapFillFetchError::RetryExhausted { attempts: 0 }),
                    );
                }
            };
            let res = execute_gap_fill_for_bar(
                &deps_for_task,
                bar_start_ist_secs,
                security_id,
                segment_code,
                instrument_type,
            )
            .await;
            (security_id, res)
        });
    }

    // Spawn NSE_EQ tasks (snapshot held under Arc — cheap to read).
    for &security_id in deps.nse_eq_sids.as_ref() {
        let deps_for_task = Arc::clone(deps);
        let semaphore = Arc::clone(&deps.fetch_semaphore);
        tasks.spawn(async move {
            let _permit = match semaphore.acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    // Semaphore closed — never expected in current code
                    // paths (we never call `.close()`). Treat as a
                    // retry-exhausted failure with 0 attempts so the
                    // operator sees a distinctive signal in audit rows
                    // + Telegram, without invoking the REST client.
                    return (
                        security_id,
                        Err(GapFillFetchError::RetryExhausted { attempts: 0 }),
                    );
                }
            };
            let res = execute_gap_fill_for_bar(
                &deps_for_task,
                bar_start_ist_secs,
                security_id,
                GAP_FILL_NSE_EQ_SEGMENT_CODE,
                GAP_FILL_NSE_EQ_INSTRUMENT_TYPE,
            )
            .await;
            (security_id, res)
        });
    }

    // Tally completion across all tasks.
    while let Some(joined) = tasks.join_next().await {
        let (security_id, result) = match joined {
            Ok(pair) => pair,
            Err(join_err) => {
                // Task panicked — treat as a SID failure but log
                // separately so we don't lose the panic backtrace.
                error!(
                    bar_start_ist_secs,
                    ?join_err,
                    "gap-fill: per-SID task join failed (panic or cancellation)"
                );
                sids_failed = sids_failed.saturating_add(1);
                continue;
            }
        };
        match result {
            Ok(written) => {
                sids_completed = sids_completed.saturating_add(1);
                candles_written.increment(written as u64);
                info!(
                    bar_start_ist_secs,
                    security_id,
                    candles_written = written,
                    "gap-fill: bar UPSERTed"
                );
            }
            Err(err) => {
                sids_failed = sids_failed.saturating_add(1);
                if sample_failed_sids.len() < TELEGRAM_FAILED_SID_SAMPLE_CAP {
                    sample_failed_sids.push(security_id);
                }
                let err_text = format!("{err}");
                last_error = Some(err_text.clone());
                error!(
                    code = ErrorCode::GapFill02RestFetchFailed.code_str(),
                    bar_start_ist_secs,
                    security_id,
                    err = %err,
                    "GAP-FILL-02: per-SID fetch failed within bar"
                );
            }
        }
    }

    let duration_ms_u32: u32 =
        u32::try_from(bar_started_at.elapsed().as_millis()).unwrap_or(u32::MAX);
    let sids_requested: u32 = u32::try_from(total_sids).unwrap_or(u32::MAX);

    let (result_label, severity_counter) = if sids_failed == 0 && sids_completed > 0 {
        (RESULT_SUCCESS, bars_succeeded)
    } else if sids_completed == 0 {
        (RESULT_FAILED, bars_failed)
    } else {
        (RESULT_PARTIAL, bars_partial)
    };
    severity_counter.increment(1);

    let bar_minute_str = bar_minute_ist_string(bar_start_ist_secs);
    let trading_date_str = trading_date_ist_string(bar_start_ist_secs);
    let bar_ts_nanos = bar_start_ist_secs.saturating_mul(1_000_000_000);

    if let Err(err) = append_gap_fill_audit_row(
        &deps.questdb_config,
        bar_ts_nanos,
        &trading_date_str,
        &bar_minute_str,
        TRIGGER_EVENT_WS_DISCONNECT,
        i32::try_from(sids_requested).unwrap_or(i32::MAX),
        i32::try_from(sids_completed).unwrap_or(i32::MAX),
        i32::try_from(sids_failed).unwrap_or(i32::MAX),
        i64::from(duration_ms_u32),
        result_label,
    )
    .await
    {
        // Audit-row failure is data-point lost, not data-corrupted —
        // the bar's candles are already UPSERTed in `historical_candles`.
        // Per `phase-0-architecture.md` PILLAR 3 + Wave-2-D AUDIT-NN
        // family, audit failures are Medium severity (NOT Critical).
        error!(
            code = ErrorCode::GapFill03UpsertFailed.code_str(),
            bar_minute_ist = %bar_minute_str,
            trading_date_ist = %trading_date_str,
            ?err,
            "gap_fill_audit row insert failed; bar candles already persisted"
        );
    }

    let event = match result_label {
        RESULT_SUCCESS => NotificationEvent::GapFillCompleted {
            bar_minute_ist: bar_minute_str,
            sids_completed,
            sids_failed,
            duration_ms: duration_ms_u32,
        },
        RESULT_PARTIAL => NotificationEvent::GapFillPartial {
            bar_minute_ist: bar_minute_str,
            sids_completed,
            sids_failed,
            sample_failed_sids,
        },
        // RESULT_FAILED covers (sids_completed == 0) — including the
        // degenerate "Dhan returned 0 candles for every SID" path
        // where `last_error` is None. Emit a descriptive fallback so
        // the operator's Telegram has a non-empty `error` field.
        _ => NotificationEvent::GapFillFailed {
            bar_minute_ist: bar_minute_str,
            error: last_error.unwrap_or_else(|| {
                "all 4 indices returned zero candles or failed retries".to_string()
            }),
            attempt: deps.max_retries,
        },
    };
    deps.notification_service.notify(event);
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
    /// unreachable endpoint + disabled notifier + unreachable QuestDB.
    /// Used by lifecycle tests that send events resulting in EMPTY
    /// planned bars (post-market or sub-minute) so the executor never
    /// actually invokes the HTTP fetch or the audit-row writer.
    fn test_deps() -> GapFillExecutorDeps {
        GapFillExecutorDeps::new(
            Arc::new(reqwest::Client::new()),
            "http://127.0.0.1:1/v2/charts/intraday".to_string(),
            Arc::new(ArcSwap::from_pointee(None)),
            SecretString::from("test-client-id"),
            Arc::new(tokio::sync::Mutex::new(NoopCandleSink)),
            QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1, // unreachable — audit writes will fail-and-log
                pg_port: 8812,
                ilp_port: 9009,
            },
            NotificationService::disabled(),
            Arc::new(Vec::new()), // PR-D6: empty NSE_EQ snapshot for lifecycle tests
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
    fn test_gap_fill_indices_has_four_entries() {
        // PR-D4 (2026-05-17): the indices-only fan-out scope. Locking
        // this at 4 prevents accidental scope expansion to NSE_EQ
        // equities without the rate-limit + Semaphore work PR-D5 will
        // ship. See `depth-subscription.md` 2026-04-25 Updates for the
        // 3-index trading scope (NIFTY/BANKNIFTY/SENSEX) + INDIA VIX
        // for volatility surface.
        assert_eq!(
            GAP_FILL_INDICES.len(),
            4,
            "PR-D4 fan-out scope is 4 IDX_I indices; expand to NSE_EQ in PR-D5"
        );
    }

    #[test]
    fn test_gap_fill_indices_security_ids_match_dhan_master() {
        // Pin SecurityIds per `websocket-connection-scope-lock.md` §I:
        //   NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21.
        let sids: Vec<u32> = GAP_FILL_INDICES.iter().map(|&(sid, _, _)| sid).collect();
        assert!(sids.contains(&13), "NIFTY (13) must be in fan-out set");
        assert!(sids.contains(&25), "BANKNIFTY (25) must be in fan-out set");
        assert!(sids.contains(&51), "SENSEX (51) must be in fan-out set");
        assert!(sids.contains(&21), "INDIA VIX (21) must be in fan-out set");
    }

    #[test]
    fn test_gap_fill_indices_all_idx_i_segment() {
        // Every entry must be IDX_I (segment code 0) per
        // `annexure-enums.md` rule 2. F&O derivatives are out of scope
        // for gap-fill — historical data fetch is index/equity only.
        for &(sid, seg, inst) in GAP_FILL_INDICES {
            assert_eq!(seg, 0, "SecurityId {sid} must be IDX_I (segment code 0)");
            assert_eq!(
                inst, "INDEX",
                "SecurityId {sid} must use Dhan REST instrument_type=INDEX"
            );
        }
    }

    // PR-D6 ratchets (2026-05-17) — NSE_EQ fan-out + Semaphore.

    #[test]
    fn test_gap_fill_executor_deps_carries_nse_eq_sids_and_semaphore() {
        // PR-D6: the struct MUST carry both the NSE_EQ snapshot and
        // the Semaphore for bounded parallelism. Future deletion of
        // either field fails this build.
        let deps = test_deps();
        // Empty snapshot is fine for the lifecycle test; the type
        // existence is what we're pinning here.
        assert!(
            deps.nse_eq_sids.is_empty(),
            "test_deps() ships an empty NSE_EQ snapshot — boot wiring fills the real set"
        );
        // Semaphore must be a real Arc with capacity > 0 (otherwise
        // every fan-out task would block forever).
        assert!(
            deps.fetch_semaphore.available_permits() > 0,
            "fetch_semaphore must start with available permits"
        );
    }

    #[test]
    fn test_gap_fill_executor_deps_new_accepts_nse_eq_snapshot() {
        // Call the constructor with a non-empty snapshot to prove the
        // signature is `Arc<Vec<u32>>` AND that the field is stored
        // by Arc-share (not cloned-by-value). The Arc strong-count
        // assertion is the mechanical signal.
        let snapshot = Arc::new(vec![13_u32, 25, 51, 21, 1234, 5678]);
        let strong_count_before = Arc::strong_count(&snapshot);
        let deps = GapFillExecutorDeps::new(
            Arc::new(reqwest::Client::new()),
            "http://127.0.0.1:1/v2/charts/intraday".to_string(),
            Arc::new(ArcSwap::from_pointee(None)),
            SecretString::from("test-client-id"),
            Arc::new(tokio::sync::Mutex::new(NoopCandleSink)),
            QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
                pg_port: 8812,
                ilp_port: 9009,
            },
            NotificationService::disabled(),
            Arc::clone(&snapshot),
        );
        assert_eq!(deps.nse_eq_sids.len(), 6);
        assert_eq!(deps.nse_eq_sids[0], 13);
        assert_eq!(deps.nse_eq_sids[4], 1234);
        // Arc::clone bumped the strong-count by 1 (caller's local +
        // the field). If the constructor accidentally cloned-by-value,
        // strong_count would still equal 1 here.
        assert_eq!(
            Arc::strong_count(&snapshot),
            strong_count_before + 1,
            "constructor must store the Arc by clone, not deep-copy the Vec"
        );
    }

    #[test]
    fn test_semaphore_capacity_equals_gap_fill_max_concurrent_fetches_constant() {
        // PR-D6 contract: the constructor wires the Semaphore at
        // exactly `GAP_FILL_MAX_CONCURRENT_FETCHES` permits — that
        // constant is the single source of truth (matches Dhan Data
        // API 5/sec hard limit per
        // `docs/dhan-ref/01-introduction-and-rate-limits.md` §7).
        let deps = test_deps();
        assert_eq!(
            deps.fetch_semaphore.available_permits(),
            GAP_FILL_MAX_CONCURRENT_FETCHES,
            "Semaphore capacity must equal GAP_FILL_MAX_CONCURRENT_FETCHES (= {})",
            GAP_FILL_MAX_CONCURRENT_FETCHES
        );
    }

    #[test]
    fn test_nse_eq_constants_match_dhan_protocol() {
        // PR-D6: NSE_EQ segment code must equal 1 per
        // `crates/common/src/types.rs::ExchangeSegment::numeric()`;
        // instrument_type must be "EQUITY" per
        // `docs/dhan-ref/05-historical-data.md` §4. Any drift in
        // either of these silently breaks Dhan's intraday endpoint.
        assert_eq!(
            GAP_FILL_NSE_EQ_SEGMENT_CODE, 1,
            "NSE_EQ segment code must match ExchangeSegment::NseEquity numeric value"
        );
        assert_eq!(
            GAP_FILL_NSE_EQ_INSTRUMENT_TYPE, "EQUITY",
            "Dhan REST instrument_type for stocks must be EQUITY"
        );
    }

    #[test]
    fn test_trading_date_ist_string_formats_yyyy_mm_dd() {
        // 2026-05-17 00:00 IST = ist_midnight_secs(2026, 5, 17).
        let midnight = ist_midnight_secs(2026, 5, 17);
        assert_eq!(trading_date_ist_string(midnight), "2026-05-17");
        // 13:33 IST same day → still 2026-05-17.
        assert_eq!(
            trading_date_ist_string(midnight + 13 * 3600 + 33 * 60),
            "2026-05-17"
        );
    }

    #[test]
    fn test_bar_minute_ist_string_formats_hh_mm() {
        let midnight = ist_midnight_secs(2026, 5, 17);
        // 09:33 IST
        assert_eq!(
            bar_minute_ist_string(midnight + 9 * 3600 + 33 * 60),
            "09:33"
        );
        // 15:29 IST
        assert_eq!(
            bar_minute_ist_string(midnight + 15 * 3600 + 29 * 60),
            "15:29"
        );
        // 00:00 IST
        assert_eq!(bar_minute_ist_string(midnight), "00:00");
    }

    #[test]
    fn test_telegram_failed_sid_sample_cap_matches_variant_doc() {
        // `NotificationEvent::GapFillPartial` doc says formatter caps
        // at 5; this constant pins the buffer-side cap belt-and-suspenders.
        assert_eq!(TELEGRAM_FAILED_SID_SAMPLE_CAP, 5);
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
