//! O1 (2026-04-17) — Phase 2 subscription scheduler.
//!
//! Per `.claude/rules/project/live-market-feed-subscription.md`, the main
//! WebSocket feed subscribes to stock F&O derivatives at 09:12 IST once
//! real pre-market LTPs are available. Pre-Phase-2 the spec existed but
//! no code implemented it, so stock F&O was silently skipped every day.
//!
//! This module delivers the **scheduler half** of Phase 2:
//!
//! 1. A pure timing decision function (`next_phase2_trigger`) that
//!    resolves to exactly one of:
//!      * `SleepUntil(Duration)` — before 09:12 IST on a trading day,
//!      * `RunImmediate { minutes_late }` — past 09:12 but still before
//!        15:30 IST on a trading day (crash-recovery / late-start path),
//!      * `SkipToday { reason }` — weekend / holiday / post-market.
//!
//! 2. An async driver (`run_phase2_scheduler`) that:
//!      * resolves the decision,
//!      * sleeps / fires immediately / skips,
//!      * once awake, waits up to `LTP_WAIT_SECS_PER_ATTEMPT` for the
//!        SharedSpotPrices to contain the major-index LTPs,
//!      * on success emits a `Phase2Complete { added_count }` event,
//!      * on LTP timeout retries up to `MAX_LTP_ATTEMPTS` before emitting
//!        `Phase2Failed`.
//!
//! **Actual subscription dispatch is stubbed** (the scheduler logs the
//! size of the would-be delta and emits the `Phase2Complete` event with
//! that count). Wiring the `SubscribeCommand` channel into each
//! `WebSocketConnection` is the next commit.
//!
//! All decisions are driven by the injected `TradingCalendar` so weekend
//! / holiday behaviour is correct everywhere.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};
use tokio::time::Instant;
use tracing::{error, info, instrument, warn};

use crate::instrument::phase2_delta::compute_phase2_stock_subscriptions;
use crate::instrument::preopen_price_buffer::{PreOpenCloses, SharedPreOpenBuffer, snapshot};
use crate::notification::{NotificationEvent, NotificationService};
use tickvault_common::instrument_types::FnoUniverse;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_common::types::FeedMode;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Scheduled trigger: 09:13:00 IST.
///
/// **Why 09:13, not 09:12** (2026-04-22 bugfix): the pre-open buffer slot 4
/// covers 09:12:00..09:12:59. The LAST tick in that minute = the "09:12
/// close" per Parthiban's spec. If we wake up at 09:12:00 sharp we read the
/// buffer BEFORE any 09:12-minute tick has landed — slot 4 is always empty,
/// backtrack walks 09:11→09:10→... and typically finds nothing (stocks
/// don't trade during 09:08-09:12 pre-open buffer), so every stock ends up
/// in `stocks_skipped_no_price`. This produced the live "Added 0 stock F&O"
/// bug observed 2026-04-22 at 09:11 IST.
///
/// Firing at 09:13:00 guarantees the 09:12 minute bucket is fully closed
/// before we snapshot the buffer, so `backtrack_latest()` returns the real
/// 09:12 close for every stock that ticked during the pre-open window.
pub const PHASE2_TRIGGER_HOUR: u32 = 9;
pub const PHASE2_TRIGGER_MIN: u32 = 13;

/// Market-hours window in IST — Phase 2 is only meaningful between 09:12 and
/// 15:30 on a trading day. After 15:30 there is nothing to subscribe to.
pub const MARKET_OPEN_HOUR: u32 = 9;
pub const MARKET_OPEN_MIN: u32 = 15;
pub const MARKET_CLOSE_HOUR: u32 = 15;
pub const MARKET_CLOSE_MIN: u32 = 30;

/// How long the scheduler will wait for the `SharedSpotPrices` map to contain
/// the major-index LTPs on a single attempt before declaring it a miss.
pub const LTP_WAIT_SECS_PER_ATTEMPT: u64 = 30;

/// Poll cadence inside the LTP wait loop.
pub const LTP_POLL_MS: u64 = 500;

/// Max retry attempts when LTPs never arrive. 3 × 30s = up to 90s of Dhan
/// slowness tolerance before giving up for the day.
pub const MAX_LTP_ATTEMPTS: u32 = 3;

/// Underlyings whose LTPs gate Phase 2 readiness.
pub const GATING_INDEX_SYMBOLS: &[&str] = &["NIFTY", "BANKNIFTY"];

// ---------------------------------------------------------------------------
// Phase 2 instrument source — Static (legacy, alert-only) or Dynamic (delta).
// ---------------------------------------------------------------------------

/// Source of the Phase 2 instrument list. The scheduler chooses how to
/// produce the subscribe list based on this enum. `Static` carries a
/// precomputed list (legacy / alert-only path used by tests). `Dynamic`
/// computes the list at trigger time from the live pre-open price buffer
/// + universe — that is the real production path.
pub enum Phase2InstrumentsSource {
    /// Precomputed instrument list. Backward-compatible with the v1
    /// alert-only call site. Treated as authoritative — no recomputation
    /// happens at trigger time.
    Static(Vec<crate::websocket::types::InstrumentSubscription>),
    /// Dynamic delta — at trigger time the scheduler snapshots
    /// `buffer`, runs `compute_phase2_stock_subscriptions`, and
    /// dispatches the resulting list.
    Dynamic {
        /// Pre-open per-minute price buckets, populated by the
        /// snapshotter task in `crates/app/src/main.rs`.
        buffer: SharedPreOpenBuffer,
        /// FnoUniverse built at boot — owns the option chains used to
        /// pick ATM ± N strikes.
        universe: Arc<FnoUniverse>,
        /// Trading calendar — used by `select_stock_expiry` to enforce
        /// the > 2 trading-days-to-expiry guard for stock F&O.
        calendar: Arc<TradingCalendar>,
        /// ATM ± `strikes_each_side` per stock. Spec value is 25
        /// (matches `live-market-feed-subscription.md`).
        strikes_each_side: usize,
    },
}

impl std::fmt::Debug for Phase2InstrumentsSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Static(list) => f
                .debug_struct("Static")
                .field("instruments", &list.len())
                .finish(),
            Self::Dynamic {
                strikes_each_side, ..
            } => f
                .debug_struct("Dynamic")
                .field("strikes_each_side", strikes_each_side)
                .finish(),
        }
    }
}

/// Outcome of a successful Phase 2 dispatch — sent on the optional
/// `mpsc::Sender<Phase2PickCompleted>` so downstream consumers (PROMPT C
/// crash recovery) can persist the chosen reference prices and instrument
/// IDs without coupling to the scheduler internals.
///
/// Consumed by Phase 2 crash-recovery wiring — PROMPT C.
///
/// # Schema history
///
/// - v1 (2026-04-16): `instruments`, `reference_prices`, `trigger_date_ist`.
/// - v2 (plan item F, 2026-04-22): added `depth_20_selections` and
///   `depth_200_selections`. Both default to empty Vec until plan item B +
///   real C depth dispatch are wired; when they are, a restart mid-day
///   will replay the exact depth contracts subscribed at 09:13:30 IST
///   rather than recomputing ATM from possibly-drifted current spot.
///
/// On-disk snapshots written before v2 will have these fields missing.
/// The serde deserializer defaults them to empty (via `#[serde(default)]`
/// on the impl-side once persisted) so old snapshots remain loadable.
#[derive(Debug, Clone)]
pub struct Phase2PickCompleted {
    /// Main-feed stock F&O instruments dispatched to the WebSocket pool.
    pub instruments: Vec<crate::websocket::types::InstrumentSubscription>,
    /// Reference price per stock symbol that drove the ATM pick. Sorted
    /// (BTreeMap) so the persisted snapshot is byte-stable across reruns.
    pub reference_prices: BTreeMap<String, f64>,
    /// Trigger date (IST) — needed by PROMPT C to key the on-disk
    /// snapshot file.
    pub trigger_date_ist: NaiveDate,
    /// Plan item F (2026-04-22) v2 schema: 20-level depth contracts
    /// selected at 09:13:30 IST from the 09:12 index close. One batch
    /// per major underlying (NIFTY/BANKNIFTY/FINNIFTY/MIDCPNIFTY).
    /// Empty until plan B wires depth dispatch through this snapshot.
    pub depth_20_selections: Vec<crate::websocket::types::InstrumentSubscription>,
    /// Plan item F (2026-04-22) v2 schema: 200-level depth contracts
    /// (ATM CE + ATM PE for NIFTY and BANKNIFTY only = 4 entries).
    /// Empty until plan B wires depth dispatch through this snapshot.
    pub depth_200_selections: Vec<crate::websocket::types::InstrumentSubscription>,
}

// ---------------------------------------------------------------------------
// Decision types
// ---------------------------------------------------------------------------

/// Outcome of the pure timing decision. Independent of any async state.
#[derive(Debug, Clone, PartialEq)]
pub enum NextTrigger {
    /// Sleep for this long, then run Phase 2.
    SleepUntil { duration: Duration },
    /// Already past 09:12 IST but still within market hours — run right now.
    /// `minutes_late` is the number of minutes past the 09:12 trigger.
    RunImmediate { minutes_late: u64 },
    /// Not a trading day or already after market close — do nothing.
    SkipToday { reason: SkipReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    NonTradingDay,
    PostMarket,
}

/// Plan item J (2026-04-22): where a Phase 2 dispatch originated.
///
/// Written to `subscription_audit_log.dispatch_source` (plan item L)
/// so the operator can answer "which code path subscribed this
/// contract at 09:13:30 IST today". The variants are deliberately
/// coarse — we want low-cardinality strings for QuestDB SYMBOL
/// columns, not one variant per call site.
///
/// # Variants
///
/// - `Scheduler` — the normal 09:13 IST wake-up path. The `SleepUntil`
///   branch of `NextTrigger` fires this. Expected source of > 99% of
///   Phase 2 dispatches in steady state.
///
/// - `CrashRecovery` — app was restarted AFTER 09:13 IST but before
///   15:30 IST. The `RunImmediate { minutes_late: N }` branch fires
///   this. On-disk snapshot (if present) provides the 09:12 close;
///   otherwise current LTP is used as a best-effort fallback.
///
/// - `ManualRestart` — operator-initiated redeploy during market
///   hours. Distinct from `CrashRecovery` in that the app was shut
///   down cleanly and the snapshot IS expected to exist. Reserved
///   for future use when the shutdown handler persists a "clean"
///   flag.
///
/// - `ApiTrigger` — manual re-dispatch via a future `/api/phase2/run`
///   endpoint (not yet implemented). Reserved.
///
/// # Stability
///
/// The string form emitted by `as_str()` is written to QuestDB and
/// used in Grafana filter expressions. Once a value is shipped, it
/// must NEVER be renamed — only new variants may be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase2DispatchSource {
    Scheduler,
    CrashRecovery,
    ManualRestart,
    ApiTrigger,
}

impl Phase2DispatchSource {
    /// Stable wire-format string written to QuestDB `subscription_audit_log`
    /// and emitted as a Prometheus label. DO NOT rename — these strings
    /// are persisted and queried by operators.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Scheduler => "scheduler",
            Self::CrashRecovery => "crash_recovery",
            Self::ManualRestart => "manual_restart",
            Self::ApiTrigger => "api_trigger",
        }
    }

    /// Derive the dispatch source from a `NextTrigger` decision. Returns
    /// `Some(source)` for `SleepUntil` / `RunImmediate`, and `None` for
    /// `SkipToday` since no dispatch happens in that branch.
    #[must_use]
    pub const fn from_next_trigger(trigger: &NextTrigger) -> Option<Self> {
        match trigger {
            NextTrigger::SleepUntil { .. } => Some(Self::Scheduler),
            NextTrigger::RunImmediate { .. } => Some(Self::CrashRecovery),
            NextTrigger::SkipToday { .. } => None,
        }
    }
}

impl std::fmt::Display for Phase2DispatchSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NonTradingDay => {
                "Phase 2 = the 09:12 IST stock-F&O ATM \u{00b1} 25 subscription \
                 burst (subscribes option chains for every F&O stock once \
                 pre-market price is finalised). Skipped because today is \
                 a non-trading day (weekend / NSE holiday). \
                 Action required: NONE \u{2014} Phase 2 will run on the next \
                 trading day's boot automatically."
            }
            Self::PostMarket => {
                "Phase 2 = the 09:12 IST stock-F&O ATM \u{00b1} 25 subscription \
                 burst (subscribes option chains for every F&O stock once \
                 pre-market price is finalised). Skipped because the app \
                 booted AFTER 15:30 IST (post-market window). \
                 Action required: NONE \u{2014} stock F&O contracts will subscribe \
                 automatically on the next market-open boot (next trading \
                 day 09:00 IST)."
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pure timing decision
// ---------------------------------------------------------------------------

/// Pure function: given the current IST wall-clock time and the trading
/// calendar, decide what the scheduler should do next.
///
/// No I/O, no allocation on the hot success path. Fully testable with
/// synthetic `now_ist` inputs — see `test_next_trigger_*` family below
/// for full scenario coverage (before-9:12, at-9:12, crash-recovery,
/// late-start, post-market, weekend, holiday, midnight).
// TEST-EXEMPT: covered by 9 scenario tests named `test_next_trigger_*`
pub fn next_phase2_trigger(
    now_ist_date: NaiveDate,
    now_ist_time: NaiveTime,
    calendar: &TradingCalendar,
) -> NextTrigger {
    // Weekend / holiday check.
    if !calendar.is_trading_day(now_ist_date) {
        return NextTrigger::SkipToday {
            reason: SkipReason::NonTradingDay,
        };
    }

    let trigger = match NaiveTime::from_hms_opt(PHASE2_TRIGGER_HOUR, PHASE2_TRIGGER_MIN, 0) {
        Some(t) => t,
        None => {
            return NextTrigger::SkipToday {
                reason: SkipReason::PostMarket,
            };
        }
    };
    let market_close = match NaiveTime::from_hms_opt(MARKET_CLOSE_HOUR, MARKET_CLOSE_MIN, 0) {
        Some(t) => t,
        None => {
            return NextTrigger::SkipToday {
                reason: SkipReason::PostMarket,
            };
        }
    };

    if now_ist_time >= market_close {
        return NextTrigger::SkipToday {
            reason: SkipReason::PostMarket,
        };
    }

    if now_ist_time < trigger {
        // Before 09:12 — sleep until the trigger.
        let secs_until = match (trigger - now_ist_time).num_seconds().try_into() {
            Ok(s) => s,
            Err(_) => 0,
        };
        return NextTrigger::SleepUntil {
            duration: Duration::from_secs(secs_until),
        };
    }

    // At or past 09:12 but before 15:30 — run immediately (crash-recovery
    // or late-start path).
    let minutes_late: u64 = (now_ist_time - trigger)
        .num_minutes()
        .try_into()
        .unwrap_or(0);
    NextTrigger::RunImmediate { minutes_late }
}

// ---------------------------------------------------------------------------
// SharedSpotPrices readiness check
// ---------------------------------------------------------------------------

/// Returns true when all symbols in `GATING_INDEX_SYMBOLS` have a fresh-enough
/// entry in the given shared spot-prices map. Fresh means simply "present" —
/// the rebalancer's separate staleness guard (O3) handles older-but-present.
pub async fn has_required_ltps(
    prices: &crate::instrument::depth_rebalancer::SharedSpotPrices,
) -> bool {
    let map = prices.read().await;
    GATING_INDEX_SYMBOLS
        .iter()
        .all(|sym| map.contains_key(*sym))
}

// ---------------------------------------------------------------------------
// Async driver
// ---------------------------------------------------------------------------

/// Returns the current IST date + time for scheduling decisions.
/// Uses `tickvault_common::trading_calendar::ist_offset` so the source of
/// truth for "what is IST" lives in one place.
fn current_ist() -> (NaiveDate, NaiveTime) {
    let now_utc = Utc::now();
    let ist_offset = tickvault_common::trading_calendar::ist_offset();
    let now_ist = ist_offset.from_utc_datetime(&now_utc.naive_utc());
    (now_ist.date_naive(), now_ist.time())
}

/// Runs the Phase 2 scheduler as a background task. Intended to be called
/// once per boot (from BOTH FAST BOOT and slow boot paths).
///
/// On entry it (a) computes the trigger decision via `next_phase2_trigger`,
/// (b) emits the matching Telegram event, and (c) if the decision says to
/// proceed, waits for LTPs and fires `Phase2Complete` (or `Phase2Failed`
/// after `MAX_LTP_ATTEMPTS`).
///
/// O1-B (2026-04-17): if `pool` and `phase2_instruments` are both `Some`,
/// the scheduler dispatches a real `SubscribeCommand::AddInstruments` to
/// the connection with the most spare capacity. If either is `None`, the
/// scheduler runs in alert-only mode (logs what it would subscribe, fires
/// `Phase2Complete` with the planned count for visibility). The
/// alert-only path is used by tests + the legacy code path that hasn't
/// yet computed the delta.
// TEST-EXEMPT: async driver with time-dependent sleep loop — decision logic covered by test_next_trigger_*, readiness by test_has_required_ltps_*, source resolution covered by phase2_wiring_integration tests
// Plan item N (2026-04-22): #[instrument] span groups all logs from this
// scheduler under a single trace span — operator can filter by `phase2`
// in tracing/Loki to see the complete lifecycle (sleep → wake → dispatch
// or skip/fail) without needing to know individual log call sites.
#[instrument(name = "phase2", skip_all, fields(feed_mode = ?feed_mode))]
pub async fn run_phase2_scheduler(
    spot_prices: crate::instrument::depth_rebalancer::SharedSpotPrices,
    calendar: Arc<TradingCalendar>,
    notifier: Arc<NotificationService>,
    pool: Option<Arc<crate::websocket::WebSocketConnectionPool>>,
    source: Option<Phase2InstrumentsSource>,
    feed_mode: FeedMode,
    pick_completed_tx: Option<tokio::sync::mpsc::Sender<Phase2PickCompleted>>,
) {
    info!(
        has_source = source.is_some(),
        has_pool = pool.is_some(),
        "Phase 2 scheduler starting"
    );

    let (today_ist, time_ist) = current_ist();
    let decision = next_phase2_trigger(today_ist, time_ist, &calendar);
    info!(?decision, "Phase 2 decision");

    match decision {
        NextTrigger::SkipToday { reason } => {
            notifier.notify(NotificationEvent::Phase2Skipped {
                reason: reason.as_str().to_string(),
            });
            return;
        }
        NextTrigger::SleepUntil { duration } => {
            info!(
                sleep_secs = duration.as_secs(),
                "Phase 2 sleeping until 09:12 IST"
            );
            tokio::time::sleep(duration).await;
            notifier.notify(NotificationEvent::Phase2Started { minutes_late: 0 });
        }
        NextTrigger::RunImmediate { minutes_late } => {
            warn!(
                minutes_late,
                "Phase 2 running immediately — late-start or crash-recovery path"
            );
            notifier.notify(NotificationEvent::Phase2RunImmediate { minutes_late });
            notifier.notify(NotificationEvent::Phase2Started { minutes_late });
        }
    }

    // Plan item K (2026-04-22): emit Phase 2 trigger latency (how late vs
    // the 09:13 IST target). Zero for the SleepUntil path, >0 for the
    // RunImmediate crash-recovery path.
    let trigger_latency_ms: u64 = match &decision {
        NextTrigger::SleepUntil { .. } => 0,
        NextTrigger::RunImmediate { minutes_late } => minutes_late.saturating_mul(60_000),
        NextTrigger::SkipToday { .. } => 0, // unreachable — returned earlier
    };
    metrics::histogram!("tv_phase2_trigger_latency_ms").record(trigger_latency_ms as f64);

    let start = Instant::now();
    for attempt in 1..=MAX_LTP_ATTEMPTS {
        if wait_for_ltps_once(&spot_prices).await {
            let duration_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

            // Resolve the instrument list at trigger time. For Static
            // sources we use the precomputed list as-is. For Dynamic
            // sources we snapshot the pre-open buffer and run the delta
            // computation now so the chosen prices reflect the actual
            // 09:12 close (or backtracked 09:11/09:10/...).
            //
            // 2026-04-22 diagnostic enhancement: when the plan ends up
            // empty we capture the skip-reason breakdown (stocks_skipped_*)
            // + preopen buffer population so the Telegram alert tells the
            // operator WHY "Added 0" happened instead of silently claiming
            // success. Before this change Phase2Complete always fired with
            // added_count=0 and the operator had no visibility.
            let mut diag_skipped_no_price: usize = 0;
            let mut diag_skipped_no_expiry: usize = 0;
            let mut diag_buffer_entries: usize = 0;
            let mut diag_sample_skipped: Vec<String> = Vec::new();
            let (instruments, reference_prices) = match source.as_ref() {
                Some(Phase2InstrumentsSource::Static(list)) => (list.clone(), BTreeMap::new()),
                Some(Phase2InstrumentsSource::Dynamic {
                    buffer,
                    universe,
                    calendar: dyn_cal,
                    strikes_each_side,
                }) => {
                    let snap = snapshot(buffer).await;
                    diag_buffer_entries = snap.len();
                    // Plan item K (2026-04-22): gauge on the preopen buffer
                    // population at trigger time. Low value here = upstream
                    // tick capture problem; the empty-plan runbook links
                    // this metric to the triage path.
                    metrics::gauge!("tv_phase2_preopen_buffer_entries")
                        .set(diag_buffer_entries as f64);
                    let compute_start = Instant::now();
                    let plan = compute_phase2_stock_subscriptions(
                        universe,
                        &snap,
                        dyn_cal,
                        today_ist,
                        *strikes_each_side,
                    );
                    let compute_ms =
                        u64::try_from(compute_start.elapsed().as_millis()).unwrap_or(u64::MAX);
                    metrics::histogram!("tv_phase2_compute_duration_ms").record(compute_ms as f64);
                    metrics::counter!("tv_phase2_stocks_subscribed_total")
                        .increment(plan.stocks_subscribed.len() as u64);
                    metrics::counter!("tv_phase2_stocks_skipped_no_price_total")
                        .increment(plan.stocks_skipped_no_price.len() as u64);
                    metrics::counter!("tv_phase2_stocks_skipped_no_eligible_expiry_total")
                        .increment(plan.stocks_skipped_no_eligible_expiry.len() as u64);
                    diag_skipped_no_price = plan.stocks_skipped_no_price.len();
                    diag_skipped_no_expiry = plan.stocks_skipped_no_eligible_expiry.len();
                    // Sample the first 5 skip victims for the diagnostic
                    // Telegram message — enough to see "is it all stocks?"
                    // without exploding the message size.
                    for s in plan.stocks_skipped_no_price.iter().take(5) {
                        diag_sample_skipped.push(format!("{s} (no-price)"));
                    }
                    if diag_sample_skipped.len() < 5 {
                        for s in plan
                            .stocks_skipped_no_eligible_expiry
                            .iter()
                            .take(5 - diag_sample_skipped.len())
                        {
                            diag_sample_skipped.push(format!("{s} (no-expiry)"));
                        }
                    }
                    let prices = derive_reference_prices(&plan.stocks_subscribed, &snap);
                    record_reference_price_source(&prices, &snap);
                    (plan.instruments, prices)
                }
                None => (Vec::new(), BTreeMap::new()),
            };

            // 2026-04-22: if the plan is empty, surface the diagnostics
            // via Phase2Failed (HIGH severity) instead of lying with
            // Phase2Complete { added_count: 0 }. This is what the operator
            // saw at 09:11 IST 2026-04-22 — "Added 0" with no root cause.
            if instruments.is_empty() {
                let reason = format!(
                    "Empty plan at trigger. Preopen buffer entries: {diag_buffer_entries}. \
                     Skipped no-price: {diag_skipped_no_price}. \
                     Skipped no-expiry: {diag_skipped_no_expiry}. \
                     Sample: [{}]. \
                     Likely cause: pre-open snapshotter received no stock ticks during \
                     09:08-09:12 IST window (check tv_phase2_snapshotter_ticks_buffered_total \
                     + tv_phase2_snapshotter_ticks_filtered_total).",
                    diag_sample_skipped.join(", ")
                );
                error!(
                    attempt,
                    buffer_entries = diag_buffer_entries,
                    skipped_no_price = diag_skipped_no_price,
                    skipped_no_expiry = diag_skipped_no_expiry,
                    "Phase 2: plan is EMPTY — no instruments to subscribe"
                );
                metrics::counter!("tv_phase2_runs_total", "outcome" => "empty_plan").increment(1);
                notifier.notify(NotificationEvent::Phase2Failed {
                    reason,
                    attempts: attempt,
                });
                return;
            }

            let planned_count = instruments.len();
            let dispatched_count = match (pool.as_ref(), instruments.is_empty()) {
                (Some(pool_ref), false) => {
                    let cmd = crate::websocket::SubscribeCommand::AddInstruments {
                        instruments: instruments.clone(),
                        feed_mode,
                    };
                    match pool_ref.dispatch_subscribe(cmd) {
                        Some(conn_id) => {
                            info!(
                                attempt,
                                duration_ms,
                                target_connection = conn_id,
                                added_count = instruments.len(),
                                "Phase 2: SubscribeCommand dispatched to pool"
                            );
                            instruments.len()
                        }
                        None => {
                            error!(
                                attempt,
                                instrument_count = instruments.len(),
                                "Phase 2: dispatch_subscribe returned None — pool has \
                                     no spare capacity or no channels installed"
                            );
                            metrics::counter!(
                                "tv_phase2_runs_total",
                                "outcome" => "dispatch_failed"
                            )
                            .increment(1);
                            notifier.notify(NotificationEvent::Phase2Failed {
                                reason: "pool dispatch returned None — no capacity or \
                                             no channels installed"
                                    .to_string(),
                                attempts: attempt,
                            });
                            return;
                        }
                    }
                }
                _ => {
                    info!(
                        attempt,
                        duration_ms,
                        planned_count,
                        "Phase 2: LTPs present — alert-only mode (no pool or \
                             empty instrument list)"
                    );
                    planned_count
                }
            };

            // Hand off to PROMPT C crash-recovery consumer. Best-effort
            // — a missing or backed-up channel must not delay dispatch.
            if !instruments.is_empty()
                && let Some(tx) = pick_completed_tx.as_ref()
            {
                let payload = Phase2PickCompleted {
                    instruments: instruments.clone(),
                    reference_prices: reference_prices.clone(),
                    trigger_date_ist: today_ist,
                    // Plan item F (2026-04-22) v2 schema — populated by
                    // plan item B once boot-time depth subscribe is
                    // deferred to 09:13:30. Empty today is correct —
                    // there is no depth dispatch in this code path yet.
                    depth_20_selections: Vec::new(),
                    depth_200_selections: Vec::new(),
                };
                if let Err(err) = tx.try_send(payload) {
                    warn!(
                        ?err,
                        "Phase 2: pick_completed channel send failed — \
                             crash-recovery snapshot will be missing this run"
                    );
                }
            }
            metrics::counter!("tv_phase2_runs_total", "outcome" => "complete").increment(1);
            metrics::histogram!("tv_phase2_run_ms").record(duration_ms as f64);
            notifier.notify(NotificationEvent::Phase2Complete {
                added_count: dispatched_count,
                duration_ms,
                // Plan item G (2026-04-22): depth dispatch will land in
                // item C — until then these are zero and the Telegram
                // message will show "Depth-20: 0 underlyings".
                depth_20_underlyings: 0,
                depth_200_contracts: 0,
            });
            return;
        }
        warn!(
            attempt,
            max_attempts = MAX_LTP_ATTEMPTS,
            "Phase 2: LTPs still absent — retrying"
        );
    }

    error!(
        attempts = MAX_LTP_ATTEMPTS,
        "Phase 2: FAILED — LTPs never arrived within retry budget"
    );
    metrics::counter!("tv_phase2_runs_total", "outcome" => "failed").increment(1);
    notifier.notify(NotificationEvent::Phase2Failed {
        reason: format!(
            "index LTPs (NIFTY/BANKNIFTY) not present after {} attempts of {}s each",
            MAX_LTP_ATTEMPTS, LTP_WAIT_SECS_PER_ATTEMPT
        ),
        attempts: MAX_LTP_ATTEMPTS,
    });
}

/// For each subscribed stock, walk its pre-open closes from 09:12 down
/// to 09:08 and pick the latest non-empty bucket as the reference price.
/// This must mirror `PreOpenCloses::backtrack_latest` so PROMPT C sees
/// the SAME price the ATM pick used.
fn derive_reference_prices(
    subscribed: &[String],
    snap: &std::collections::HashMap<String, PreOpenCloses>,
) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for symbol in subscribed {
        if let Some(closes) = snap.get(symbol)
            && let Some(price) = closes.backtrack_latest()
        {
            out.insert(symbol.clone(), price);
        }
    }
    out
}

/// Emit per-source-minute counters for the prices we actually picked,
/// so operators can see whether 09:12 succeeded or how often we fell
/// back to 09:11/09:10/09:09/09:08.
fn record_reference_price_source(
    reference_prices: &BTreeMap<String, f64>,
    snap: &std::collections::HashMap<String, PreOpenCloses>,
) {
    for symbol in reference_prices.keys() {
        let Some(closes) = snap.get(symbol) else {
            continue;
        };
        // closes.closes[0..5] = 09:08, 09:09, 09:10, 09:11, 09:12.
        // backtrack_latest scans from index 4 down to 0; record the
        // first non-empty slot's source-minute label.
        let labels: [&str; 5] = ["09:08", "09:09", "09:10", "09:11", "09:12"];
        for idx in (0..closes.closes.len()).rev() {
            if closes.closes[idx].is_some() {
                metrics::counter!(
                    "tv_phase2_reference_price_source_total",
                    "source" => labels[idx]
                )
                .increment(1);
                break;
            }
        }
    }
}

async fn wait_for_ltps_once(
    prices: &crate::instrument::depth_rebalancer::SharedSpotPrices,
) -> bool {
    let wait_start = Instant::now();
    let max_wait = Duration::from_secs(LTP_WAIT_SECS_PER_ATTEMPT);
    loop {
        if has_required_ltps(prices).await {
            return true;
        }
        if wait_start.elapsed() >= max_wait {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(LTP_POLL_MS)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests — pure functions + decision coverage
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::trading_calendar::TradingCalendar;

    fn base_trading_config() -> tickvault_common::config::TradingConfig {
        tickvault_common::config::TradingConfig {
            market_open_time: "09:15:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "16:00:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        }
    }

    fn weekday_calendar() -> TradingCalendar {
        TradingCalendar::from_config(&base_trading_config()).expect("empty-holiday calendar builds")
    }

    fn calendar_with_holidays(holidays: Vec<NaiveDate>) -> TradingCalendar {
        let mut cfg = base_trading_config();
        cfg.nse_holidays = holidays
            .into_iter()
            .map(|d| tickvault_common::config::NseHolidayEntry {
                date: d.format("%Y-%m-%d").to_string(),
                name: "Test Holiday".to_string(),
            })
            .collect();
        TradingCalendar::from_config(&cfg).expect("calendar with holidays builds")
    }

    fn weekday_date() -> NaiveDate {
        // 2026-04-17 was a Friday.
        NaiveDate::from_ymd_opt(2026, 4, 17).expect("valid")
    }

    fn saturday_date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 4, 18).expect("valid")
    }

    #[test]
    fn test_next_trigger_before_913_sleeps() {
        // 2026-04-22: trigger moved 09:12 → 09:13.
        let cal = weekday_calendar();
        let now = NaiveTime::from_hms_opt(8, 58, 0).unwrap();
        match next_phase2_trigger(weekday_date(), now, &cal) {
            NextTrigger::SleepUntil { duration } => {
                // 8:58 -> 9:13 = 15 min = 900 s.
                assert_eq!(duration.as_secs(), 900);
            }
            other => panic!("expected SleepUntil, got {other:?}"),
        }
    }

    #[test]
    fn test_next_trigger_at_exactly_913_runs_immediate() {
        // 2026-04-22: trigger moved from 09:12 → 09:13 so the 09:12 close
        // bucket is complete before we read the pre-open buffer.
        let cal = weekday_calendar();
        let now = NaiveTime::from_hms_opt(9, 13, 0).unwrap();
        match next_phase2_trigger(weekday_date(), now, &cal) {
            NextTrigger::RunImmediate { minutes_late } => assert_eq!(minutes_late, 0),
            other => panic!("expected RunImmediate, got {other:?}"),
        }
    }

    #[test]
    fn test_next_trigger_at_912_is_still_sleep_until_913() {
        // 09:12:00 is BEFORE the trigger now — must sleep to 09:13:00.
        let cal = weekday_calendar();
        let now = NaiveTime::from_hms_opt(9, 12, 0).unwrap();
        match next_phase2_trigger(weekday_date(), now, &cal) {
            NextTrigger::SleepUntil { duration } => {
                assert_eq!(duration, Duration::from_secs(60));
            }
            other => panic!("expected SleepUntil, got {other:?}"),
        }
    }

    #[test]
    fn test_next_trigger_crash_recovery_at_10_am_runs_immediate() {
        let cal = weekday_calendar();
        let now = NaiveTime::from_hms_opt(10, 0, 0).unwrap();
        match next_phase2_trigger(weekday_date(), now, &cal) {
            NextTrigger::RunImmediate { minutes_late } => assert_eq!(minutes_late, 47),
            other => panic!("expected RunImmediate, got {other:?}"),
        }
    }

    #[test]
    fn test_next_trigger_fresh_start_at_1130_runs_immediate() {
        let cal = weekday_calendar();
        let now = NaiveTime::from_hms_opt(11, 30, 0).unwrap();
        match next_phase2_trigger(weekday_date(), now, &cal) {
            NextTrigger::RunImmediate { minutes_late } => assert_eq!(minutes_late, 137),
            other => panic!("expected RunImmediate, got {other:?}"),
        }
    }

    #[test]
    fn test_next_trigger_post_market_skips() {
        let cal = weekday_calendar();
        let now = NaiveTime::from_hms_opt(15, 30, 0).unwrap();
        assert!(matches!(
            next_phase2_trigger(weekday_date(), now, &cal),
            NextTrigger::SkipToday {
                reason: SkipReason::PostMarket
            }
        ));
    }

    #[test]
    fn test_next_trigger_after_1530_skips() {
        let cal = weekday_calendar();
        let now = NaiveTime::from_hms_opt(16, 45, 0).unwrap();
        assert!(matches!(
            next_phase2_trigger(weekday_date(), now, &cal),
            NextTrigger::SkipToday {
                reason: SkipReason::PostMarket
            }
        ));
    }

    #[test]
    fn test_next_trigger_saturday_skips() {
        let cal = weekday_calendar();
        let now = NaiveTime::from_hms_opt(10, 0, 0).unwrap();
        assert!(matches!(
            next_phase2_trigger(saturday_date(), now, &cal),
            NextTrigger::SkipToday {
                reason: SkipReason::NonTradingDay
            }
        ));
    }

    #[test]
    fn test_next_trigger_holiday_skips() {
        // 2026-08-15 = Independence Day. Note: 2026-08-15 is a Saturday, so
        // the calendar already rejects it as non-trading via the weekend
        // check. Use 2026-08-14 (Friday) as a realistic weekday holiday.
        let holiday = NaiveDate::from_ymd_opt(2026, 8, 14).unwrap();
        let cal = calendar_with_holidays(vec![holiday]);
        let now = NaiveTime::from_hms_opt(10, 0, 0).unwrap();
        assert!(matches!(
            next_phase2_trigger(holiday, now, &cal),
            NextTrigger::SkipToday {
                reason: SkipReason::NonTradingDay
            }
        ));
    }

    #[test]
    fn test_next_trigger_at_901_sleeps_correctly() {
        // 2026-04-22: trigger moved 09:12 → 09:13.
        let cal = weekday_calendar();
        let now = NaiveTime::from_hms_opt(9, 1, 0).unwrap();
        match next_phase2_trigger(weekday_date(), now, &cal) {
            NextTrigger::SleepUntil { duration } => {
                // 9:01 -> 9:13 = 12 min = 720 s.
                assert_eq!(duration.as_secs(), 720);
            }
            other => panic!("expected SleepUntil, got {other:?}"),
        }
    }

    #[test]
    fn test_next_trigger_midnight_sleeps_9h13m() {
        // 2026-04-22: trigger moved 09:12 → 09:13.
        let cal = weekday_calendar();
        let now = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        match next_phase2_trigger(weekday_date(), now, &cal) {
            NextTrigger::SleepUntil { duration } => {
                // 00:00 -> 09:13 = 9h 13m = 33180 s.
                assert_eq!(duration.as_secs(), 33180);
            }
            other => panic!("expected SleepUntil, got {other:?}"),
        }
    }

    #[test]
    fn test_skip_reason_strings_stable() {
        assert!(SkipReason::NonTradingDay.as_str().contains("weekend"));
        assert!(SkipReason::PostMarket.as_str().contains("15:30"));
    }

    #[test]
    fn test_constants_are_sane() {
        assert_eq!(PHASE2_TRIGGER_HOUR, 9);
        // 2026-04-22: trigger moved from 09:12 → 09:13 so the 09:12 close
        // minute bucket is fully captured before we read the pre-open buffer.
        assert_eq!(PHASE2_TRIGGER_MIN, 13);
        // Trigger must still be BEFORE market open (09:15) so we're
        // subscribed in time for the opening auction.
        assert!(PHASE2_TRIGGER_MIN < MARKET_OPEN_MIN);
        assert!(MAX_LTP_ATTEMPTS >= 2, "need at least one retry");
        assert!(
            LTP_WAIT_SECS_PER_ATTEMPT <= 60,
            "per-attempt wait must be bounded"
        );
        assert!(
            !GATING_INDEX_SYMBOLS.is_empty(),
            "must gate on at least one index"
        );
    }

    #[tokio::test]
    async fn test_has_required_ltps_false_when_empty() {
        use crate::instrument::depth_rebalancer::new_shared_spot_prices;
        let prices = new_shared_spot_prices();
        assert!(!has_required_ltps(&prices).await);
    }

    #[tokio::test]
    async fn test_has_required_ltps_false_when_missing_one() {
        use crate::instrument::depth_rebalancer::{new_shared_spot_prices, update_spot_price};
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "NIFTY", 23500.0).await;
        // BANKNIFTY missing → false.
        assert!(!has_required_ltps(&prices).await);
    }

    #[tokio::test]
    async fn test_has_required_ltps_true_when_all_present() {
        use crate::instrument::depth_rebalancer::{new_shared_spot_prices, update_spot_price};
        let prices = new_shared_spot_prices();
        update_spot_price(&prices, "NIFTY", 23500.0).await;
        update_spot_price(&prices, "BANKNIFTY", 51000.0).await;
        assert!(has_required_ltps(&prices).await);
    }

    // ========================================================================
    // Plan item J (2026-04-22) — Phase2DispatchSource enum tests
    // ========================================================================

    #[test]
    fn test_phase2_dispatch_source_as_str_is_stable() {
        // These strings are written to QuestDB subscription_audit_log and
        // used in Grafana filters. DO NOT rename — persisted and queried.
        assert_eq!(Phase2DispatchSource::Scheduler.as_str(), "scheduler");
        assert_eq!(
            Phase2DispatchSource::CrashRecovery.as_str(),
            "crash_recovery"
        );
        assert_eq!(
            Phase2DispatchSource::ManualRestart.as_str(),
            "manual_restart"
        );
        assert_eq!(Phase2DispatchSource::ApiTrigger.as_str(), "api_trigger");
    }

    #[test]
    fn test_phase2_dispatch_source_display_matches_as_str() {
        assert_eq!(
            format!("{}", Phase2DispatchSource::Scheduler),
            Phase2DispatchSource::Scheduler.as_str()
        );
        assert_eq!(
            format!("{}", Phase2DispatchSource::CrashRecovery),
            Phase2DispatchSource::CrashRecovery.as_str()
        );
    }

    #[test]
    fn test_phase2_dispatch_source_as_str_are_all_unique() {
        let variants = [
            Phase2DispatchSource::Scheduler,
            Phase2DispatchSource::CrashRecovery,
            Phase2DispatchSource::ManualRestart,
            Phase2DispatchSource::ApiTrigger,
        ];
        let mut seen = std::collections::HashSet::new();
        for v in variants {
            assert!(
                seen.insert(v.as_str()),
                "duplicate as_str() mapping for {v:?}"
            );
        }
        assert_eq!(seen.len(), 4, "expected 4 distinct wire strings");
    }

    #[test]
    fn test_phase2_dispatch_source_from_sleep_until_is_scheduler() {
        let trigger = NextTrigger::SleepUntil {
            duration: Duration::from_secs(60),
        };
        assert_eq!(
            Phase2DispatchSource::from_next_trigger(&trigger),
            Some(Phase2DispatchSource::Scheduler)
        );
    }

    #[test]
    fn test_phase2_dispatch_source_from_run_immediate_is_crash_recovery() {
        let trigger = NextTrigger::RunImmediate { minutes_late: 5 };
        assert_eq!(
            Phase2DispatchSource::from_next_trigger(&trigger),
            Some(Phase2DispatchSource::CrashRecovery)
        );
    }

    #[test]
    fn test_phase2_dispatch_source_from_skip_today_is_none() {
        let trigger = NextTrigger::SkipToday {
            reason: SkipReason::NonTradingDay,
        };
        assert_eq!(Phase2DispatchSource::from_next_trigger(&trigger), None);
    }

    #[test]
    fn test_phase2_dispatch_source_equality_and_clone() {
        let a = Phase2DispatchSource::Scheduler;
        let b = Phase2DispatchSource::Scheduler;
        let c = Phase2DispatchSource::CrashRecovery;
        assert_eq!(a, b);
        assert_ne!(a, c);
        let cloned = a;
        assert_eq!(a, cloned);
    }

    // ========================================================================
    // Plan item F (2026-04-22) — Phase2PickCompleted v2 schema ratchet
    // ========================================================================

    #[test]
    fn test_phase2_pick_completed_v2_has_depth_20_selections_field() {
        // Compile-time guard: removing or renaming the field breaks this
        // assert. Plan F = durable crash-recovery needs depth contracts
        // to be replayable on restart mid-day.
        let payload = Phase2PickCompleted {
            instruments: Vec::new(),
            reference_prices: BTreeMap::new(),
            trigger_date_ist: NaiveDate::from_ymd_opt(2026, 4, 22).expect("valid date"),
            depth_20_selections: Vec::new(),
            depth_200_selections: Vec::new(),
        };
        assert!(payload.depth_20_selections.is_empty());
    }

    #[test]
    fn test_phase2_pick_completed_v2_has_depth_200_selections_field() {
        let payload = Phase2PickCompleted {
            instruments: Vec::new(),
            reference_prices: BTreeMap::new(),
            trigger_date_ist: NaiveDate::from_ymd_opt(2026, 4, 22).expect("valid date"),
            depth_20_selections: Vec::new(),
            depth_200_selections: Vec::new(),
        };
        assert!(payload.depth_200_selections.is_empty());
    }

    #[test]
    fn test_phase2_pick_completed_v2_clone_preserves_depth_fields() {
        let original = Phase2PickCompleted {
            instruments: Vec::new(),
            reference_prices: BTreeMap::new(),
            trigger_date_ist: NaiveDate::from_ymd_opt(2026, 4, 22).expect("valid date"),
            depth_20_selections: Vec::new(),
            depth_200_selections: Vec::new(),
        };
        let cloned = original.clone();
        assert_eq!(
            cloned.depth_20_selections.len(),
            original.depth_20_selections.len()
        );
        assert_eq!(
            cloned.depth_200_selections.len(),
            original.depth_200_selections.len()
        );
    }
}
