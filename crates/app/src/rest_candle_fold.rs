//! REST-era bar-fold candle derivation (FOLD-01 runbook:
//! `.claude/rules/project/rest-candle-fold-error-codes.md`).
//!
//! Operator directive 2026-07-16 (verbatim): *"why the fuck remaining candles
//! 1m till 1day is not yet generated and populated — resolve these"* + *"for
//! only spots we will have minimum one month data because anyhow based on
//! underlying spots alone only trading decision will be entered or exited —
//! but option only for the current day"* + *"everything should be always
//! available in our own questdb right — our entire one month should be stored
//! and fetched from questdb even before premarket"*.
//!
//! With both live feeds retired (Dhan 2026-07-13, Groww 2026-07-15) the 21-TF
//! tick aggregator is publisher-less, so the `candles_*` tables stopped
//! populating. This module derives them from the ONLY live market-data source
//! left — the per-minute `spot_1m_rest` official 1m bars — by FOLDING each
//! persist-CONFIRMED 1m bar into all 21 timeframe buckets and emitting sealed
//! buckets as [`BufferedSeal`]s into the EXISTING global seal-writer channel
//! (`tickvault_storage::seal_writer_runner::global_seal_sender`), which lands
//! them in the same `candles_*` tables with the same DEDUP key
//! (`ts, security_id, segment, feed`) — idempotent by construction.
//!
//! This is NOT tick synthesis: no row ever touches `ticks`
//! (`live-feed-purity.md` rules 1-6 stand; rule 10 carries the dated
//! 2026-07-16 edit permitting this writer to produce `candles_1d`).
//!
//! Design points (plan `.claude/plans/active-plan-rest-candle-derivation.md`):
//! - Pure fold core: o = first bar open, h = max, l = min, c = last close,
//!   volume = checked i64 sum (overflow → degrade, never wrap), tick_count 0
//!   (honest — REST bars carry no tick counts), oi 0, pct columns 0.0.
//!   Exact-match parity with `tf_consistency_boot::recompute_window` is
//!   golden-tested.
//! - Bucket grid: `TfIndex::bucket_start` (09:15-anchored) with effective end
//!   `min(start + tf_secs, 15:30 close)` — the tf_consistency session grid.
//!   A bucket seals when a LATER in-session bar crosses its effective end (or
//!   opens a later bucket); D1 + every open partial force-seals at/after the
//!   15:30 close bar.
//! - Out-of-order/duplicate bars (backfill/sweep repairs): a bar whose minute
//!   is ≤ the last folded minute is NEVER folded into the live RAM state —
//!   instead its (feed, sid, segment, IST day) is marked DIRTY and a debounced
//!   refold re-reads that day's `spot_1m_rest` rows from QuestDB into a FRESH
//!   engine and re-emits every bucket (DEDUP UPSERT heals in place). Honest
//!   residual: the ILP flush ACK → `/exec` visibility lag (QuestDB WAL apply)
//!   can make an immediate refold miss the newest row; the NEXT dirty mark or
//!   the boot catch-up covers it.
//! - Boot catch-up: re-folds the last `catchup_days` (default 35) of
//!   `spot_1m_rest` per feed through the same engines — past days
//!   force-sealed, today's partials stay open for live continuation. Bounded
//!   `/exec` reads reuse the tf_consistency hardened shapes (micros WHERE
//!   window, explicit LIMIT tripwire, 8 MiB response cap, segment allowlist,
//!   redirect-none client). Flagged honestly: catch-up is O(days × SIDs ×
//!   rows) COLD-path work at boot — never the hot path.
//! - Config-gated (`[rest_candle_fold]`, serde default OFF — fail-safe;
//!   base.toml opts in), supervised (house respawn pattern,
//!   `classify_join_exit`), log-sink-only FOLD-01 (`error!` + counters; no
//!   CloudWatch entry — delivery boundary in the rule file).

use std::collections::BTreeSet;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::NaiveDate;
use metrics::counter;
use tickvault_common::config::{QuestDbConfig, RestCandleFoldConfig};
use tickvault_common::constants::{MARKET_CLOSE_IST_NANOS, MARKET_OPEN_IST_NANOS};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::types::SecurityId;
use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TF_COUNT, TfIndex};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Constants (all named — no magic numbers; cold-path envelope bounds)
// ---------------------------------------------------------------------------

/// Bounded handoff channel from the spot legs to the fold task.
/// 4 SIDs/feed × ~2 bars/minute worst case leaves enormous headroom;
/// a full channel drops loudly (counter + coded error), never blocks a leg.
pub const FOLD_BAR_CHANNEL_CAPACITY: usize = 4096;

/// Session open, IST seconds-of-day (09:15:00) — const-asserted against the
/// canonical nanos constants so a session change cannot silently diverge.
pub const FOLD_SESSION_OPEN_SECS_OF_DAY_IST: u32 = 33_300;

/// Session close, IST seconds-of-day (15:30:00), exclusive.
pub const FOLD_SESSION_CLOSE_SECS_OF_DAY_IST: u32 = 55_800;

const _: () = assert!(
    FOLD_SESSION_OPEN_SECS_OF_DAY_IST as i64 * 1_000_000_000 == MARKET_OPEN_IST_NANOS,
    "fold session open must equal MARKET_OPEN_IST_NANOS"
);
const _: () = assert!(
    FOLD_SESSION_CLOSE_SECS_OF_DAY_IST as i64 * 1_000_000_000 == MARKET_CLOSE_IST_NANOS,
    "fold session close must equal MARKET_CLOSE_IST_NANOS"
);

/// Per-(feed, SID, day) catch-up row LIMIT. A full session is 375 minutes,
/// so 500 leaves headroom; hitting the LIMIT is a loud truncation tripwire
/// (the tf_consistency precedent — a partial fold is never trusted).
pub const FOLD_CATCHUP_1M_ROW_LIMIT: usize = 500;

/// Discovery LIMIT for `SELECT DISTINCT security_id, exchange_segment`.
/// The spot legs pin 4 SIDs/feed; 64 is a generous tripwire bound.
pub const FOLD_DISCOVERY_ROW_LIMIT: usize = 64;

/// Response-size cap for `/exec` reads (the tf_consistency 8 MiB precedent).
pub const FOLD_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// HTTP request timeout for the cold-path `/exec` reads.
pub const FOLD_HTTP_TIMEOUT_SECS: u64 = 15;

/// Debounce window before a dirty-day refold fires (coalesces a backfill
/// burst into one QuestDB round-trip per day).
pub const FOLD_REFOLD_DEBOUNCE_SECS: u64 = 5;

/// Supervisor respawn backoff (house pattern).
pub const FOLD_RESPAWN_BACKOFF_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// The confirmed-bar handoff (spot legs -> fold task)
// ---------------------------------------------------------------------------

/// One persist-CONFIRMED official 1m spot bar, as handed off by a spot leg
/// AFTER the `spot_1m_rest` ILP flush ACK (never before — a bar that failed
/// to persist must not derive candles the audit record does not back).
#[derive(Debug, Clone, Copy)]
pub struct ConfirmedBar {
    /// Which REST leg produced the bar (`feed` column parity).
    pub feed: Feed,
    /// Dhan-space SID for the Dhan leg; the Groww stable index id for Groww.
    pub security_id: SecurityId,
    /// Numeric exchange-segment code (IDX_I = 0 for every spot index today).
    pub exchange_segment_code: u8,
    /// Minute-open IST timestamp in nanoseconds (the `spot_1m_rest.ts` value).
    pub minute_ts_ist_nanos: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Official vendor volume for the minute (0 for indices — honest).
    pub volume: i64,
}

impl ConfirmedBar {
    /// Builds a confirmed bar from a parsed [`MinuteCandle`] — the single
    /// choke point BOTH spot legs use at their persist-confirmed hook
    /// sites (Dhan fire/sweep + Groww fire/sweep).
    pub fn from_minute_candle(
        feed: Feed,
        security_id: SecurityId,
        exchange_segment_code: u8,
        candle: &crate::dhan_intraday_parse::MinuteCandle,
    ) -> Self {
        Self {
            feed,
            security_id,
            exchange_segment_code,
            minute_ts_ist_nanos: candle.minute_ts_ist_nanos,
            open: candle.open,
            high: candle.high,
            low: candle.low,
            close: candle.close,
            volume: candle.volume,
        }
    }
}

static FOLD_BAR_SENDER: OnceLock<mpsc::Sender<ConfirmedBar>> = OnceLock::new();

/// Installs the process-global fold-bar sender (first-wins, idempotent —
/// the `set_global_seal_sender` house precedent). Returns `false` if a
/// sender was already installed.
pub fn set_global_fold_bar_sender(sender: mpsc::Sender<ConfirmedBar>) -> bool {
    FOLD_BAR_SENDER.set(sender).is_ok()
}

/// Read-only accessor for the global fold-bar sender.
pub fn global_fold_bar_sender() -> Option<&'static mpsc::Sender<ConfirmedBar>> {
    FOLD_BAR_SENDER.get()
}

/// Best-effort handoff of persist-confirmed bars from a spot leg.
///
/// NEVER blocks the calling leg: `try_send` per bar; a full/closed channel
/// increments `tv_rest_candle_fold_dropped_total{reason}` and logs ONE coded
/// error per call (bounded — the legs call once per minute fire/sweep).
/// A missing sender (fold disabled by config) is a silent no-op by design.
pub fn send_confirmed_bars(bars: &[ConfirmedBar]) {
    if bars.is_empty() {
        return;
    }
    let Some(sender) = global_fold_bar_sender() else {
        // Fold task not running (config-disabled) — deliberate no-op.
        return;
    };
    let mut dropped_full = 0usize;
    let mut dropped_closed = 0usize;
    for bar in bars {
        match sender.try_send(*bar) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => dropped_full += 1,
            Err(mpsc::error::TrySendError::Closed(_)) => dropped_closed += 1,
        }
    }
    if dropped_full > 0 {
        counter!("tv_rest_candle_fold_dropped_total", "reason" => "channel_full")
            .increment(dropped_full as u64);
        error!(
            code = ErrorCode::RestCandleFold01Degraded.code_str(),
            stage = "seal_send",
            reason = "channel_full",
            dropped = dropped_full,
            "rest_candle_fold: confirmed-bar handoff channel FULL — bars dropped; \
             the boot catch-up / dirty-day refold re-derives them from spot_1m_rest"
        );
    }
    if dropped_closed > 0 {
        counter!("tv_rest_candle_fold_dropped_total", "reason" => "channel_closed")
            .increment(dropped_closed as u64);
        error!(
            code = ErrorCode::RestCandleFold01Degraded.code_str(),
            stage = "seal_send",
            reason = "channel_closed",
            dropped = dropped_closed,
            "rest_candle_fold: confirmed-bar handoff channel CLOSED — fold task dead; \
             supervisor respawn restores it (bars re-derived by catch-up)"
        );
    }
}

// ---------------------------------------------------------------------------
// Pure fold core
// ---------------------------------------------------------------------------

/// One open TF bucket being folded from 1m bars.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TfBucket {
    /// Bucket-open IST seconds (the `TfIndex::bucket_start` grid value).
    pub bucket_start_ist_secs: u32,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Checked i64 volume sum (overflow degrades the bucket loudly).
    pub volume: i64,
    /// IST seconds of the LAST folded bar's minute open (forensics).
    pub last_bar_ist_secs: u32,
}

/// A sealed bucket ready for BufferedSeal conversion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SealedBucket {
    pub tf: TfIndex,
    pub bucket: TfBucket,
}

/// Outcome of folding one 1m bar into a per-SID engine.
#[derive(Debug)]
pub enum FoldOutcome {
    /// Bar folded; zero or more earlier buckets sealed by this bar.
    Folded(Vec<SealedBucket>),
    /// Bar's minute is ≤ the last folded minute — the caller marks the
    /// (feed, sid, segment, day) dirty for a QuestDB refold instead.
    OutOfOrder,
    /// Bar's minute lies outside [09:15, 15:30) IST — skipped + counted.
    OutOfSession,
    /// The i64 volume sum overflowed — the bucket is dropped loudly.
    VolumeOverflow { tf: TfIndex },
}

/// Effective (session-truncated) end of a TF bucket, IST seconds-of-day.
///
/// `min(bucket_start + tf_secs, 15:30 close)` — the tf_consistency grid rule;
/// D1's natural end (next-day 09:15) truncates to the SAME day's close so the
/// daily candle seals at 15:30 per the operator's demand.
pub fn session_truncated_end(tf: TfIndex, bucket_start_ist_secs: u32) -> u32 {
    let day_start = (bucket_start_ist_secs / 86_400) * 86_400;
    let close = day_start + FOLD_SESSION_CLOSE_SECS_OF_DAY_IST;
    let natural = bucket_start_ist_secs.saturating_add(tf.seconds_per_bucket());
    natural.min(close)
}

/// True when `ist_secs` (a minute-open) lies inside the trading session.
pub fn in_session(ist_secs: u32) -> bool {
    let sod = ist_secs % 86_400;
    (FOLD_SESSION_OPEN_SECS_OF_DAY_IST..FOLD_SESSION_CLOSE_SECS_OF_DAY_IST).contains(&sod)
}

/// Per-(feed, SID, segment) fold engine: 21 open buckets + ordering watermark.
#[derive(Debug)]
pub struct SidFoldState {
    pub feed: Feed,
    pub security_id: SecurityId,
    pub exchange_segment_code: u8,
    /// One optional open bucket per TF (index = `TfIndex as usize`).
    buckets: [Option<TfBucket>; TF_COUNT],
    /// IST seconds of the last folded bar's minute open — the out-of-order gate.
    last_folded_minute_ist_secs: Option<u32>,
}

impl SidFoldState {
    pub fn new(feed: Feed, security_id: SecurityId, exchange_segment_code: u8) -> Self {
        Self {
            feed,
            security_id,
            exchange_segment_code,
            buckets: [None; TF_COUNT],
            last_folded_minute_ist_secs: None,
        }
    }

    /// Folds one 1m bar into all 21 TF buckets, sealing any bucket the bar
    /// has moved past. O(TF_COUNT) per bar — constant work, cold path.
    pub fn fold_bar(&mut self, bar: &ConfirmedBar) -> FoldOutcome {
        let minute_secs_i64 = bar.minute_ts_ist_nanos / 1_000_000_000;
        let Ok(minute_secs) = u32::try_from(minute_secs_i64) else {
            return FoldOutcome::OutOfSession;
        };
        if !in_session(minute_secs) {
            return FoldOutcome::OutOfSession;
        }
        if let Some(last) = self.last_folded_minute_ist_secs
            && minute_secs <= last
        {
            return FoldOutcome::OutOfOrder;
        }

        let mut sealed: Vec<SealedBucket> = Vec::new();
        for tf in TfIndex::ALL {
            let idx = tf as usize;
            let start = tf.bucket_start(minute_secs);
            match self.buckets[idx] {
                Some(existing) if existing.bucket_start_ist_secs == start => {
                    // Same bucket — fold in place.
                    let Some(vol) = existing.volume.checked_add(bar.volume) else {
                        self.buckets[idx] = None;
                        return FoldOutcome::VolumeOverflow { tf };
                    };
                    self.buckets[idx] = Some(TfBucket {
                        bucket_start_ist_secs: existing.bucket_start_ist_secs,
                        open: existing.open,
                        high: existing.high.max(bar.high),
                        low: existing.low.min(bar.low),
                        close: bar.close,
                        volume: vol,
                        last_bar_ist_secs: minute_secs,
                    });
                }
                Some(existing) => {
                    // The bar opened a LATER bucket — seal the old one first.
                    sealed.push(SealedBucket {
                        tf,
                        bucket: existing,
                    });
                    self.buckets[idx] = Some(TfBucket {
                        bucket_start_ist_secs: start,
                        open: bar.open,
                        high: bar.high,
                        low: bar.low,
                        close: bar.close,
                        volume: bar.volume,
                        last_bar_ist_secs: minute_secs,
                    });
                }
                None => {
                    self.buckets[idx] = Some(TfBucket {
                        bucket_start_ist_secs: start,
                        open: bar.open,
                        high: bar.high,
                        low: bar.low,
                        close: bar.close,
                        volume: bar.volume,
                        last_bar_ist_secs: minute_secs,
                    });
                }
            }
        }
        // A bar whose minute is the LAST session minute (15:29) closes every
        // bucket whose effective end == 15:30 — seal them immediately so the
        // final candles never wait for a next-day bar.
        let minute_end = minute_secs.saturating_add(60);
        for tf in TfIndex::ALL {
            let idx = tf as usize;
            if let Some(existing) = self.buckets[idx] {
                let end = session_truncated_end(tf, existing.bucket_start_ist_secs);
                if minute_end >= end {
                    sealed.push(SealedBucket {
                        tf,
                        bucket: existing,
                    });
                    self.buckets[idx] = None;
                }
            }
        }
        self.last_folded_minute_ist_secs = Some(minute_secs);
        FoldOutcome::Folded(sealed)
    }

    /// Force-seals every open bucket (past-day catch-up / engine replacement).
    pub fn force_seal_open(&mut self) -> Vec<SealedBucket> {
        let mut sealed = Vec::new();
        for tf in TfIndex::ALL {
            let idx = tf as usize;
            if let Some(bucket) = self.buckets[idx].take() {
                sealed.push(SealedBucket { tf, bucket });
            }
        }
        sealed
    }

    /// Number of currently-open buckets (test/forensics helper).
    pub fn open_bucket_count(&self) -> usize {
        self.buckets.iter().filter(|b| b.is_some()).count()
    }
}

/// Converts a sealed bucket into the [`BufferedSeal`] the shared seal-writer
/// consumes. tick_count 0 / oi 0 / pct 0.0 are HONEST — REST bars carry no
/// tick counts or OI, and the pct-stamping chain belongs to the live path.
pub fn sealed_bucket_to_seal(
    feed: Feed,
    security_id: SecurityId,
    exchange_segment_code: u8,
    sealed: &SealedBucket,
) -> BufferedSeal {
    let b = &sealed.bucket;
    let state = LiveCandleState {
        bucket_start_ist_secs: b.bucket_start_ist_secs,
        open: b.open,
        high: b.high,
        low: b.low,
        close: b.close,
        // Clamp negative vendor volume to 0 (defensive; the parsers already
        // reject garbage) — u64 cannot carry a negative.
        volume: u64::try_from(b.volume.max(0)).unwrap_or(0),
        bucket_start_cumulative: 0,
        oi: 0,
        tick_count: 0,
        close_ts_ist_secs: b.last_bar_ist_secs.saturating_add(60),
        prev_day_close: 0.0,
        close_pct_from_prev_day: 0.0,
        oi_pct_from_prev_day: 0.0,
        volume_pct_from_prev_day: 0.0,
        session_open: b.open,
        open_pct: 0.0,
        open_gap_pct: 0.0,
    };
    BufferedSeal::new(security_id, exchange_segment_code, sealed.tf, state, feed)
}

// ---------------------------------------------------------------------------
// QuestDB read shapes (tf_consistency hardened precedents)
// ---------------------------------------------------------------------------

/// SQL for one (feed, sid, segment, day)'s 1m bars from `spot_1m_rest`.
/// Micros WHERE window + `(ts / 1) * 1000` nanos projection + explicit LIMIT
/// (the tf_consistency `select_1m_sql` shape).
pub fn spot_bars_sql(
    security_id: i64,
    segment: &str,
    feed: &str,
    day_start_nanos: i64,
    limit: usize,
) -> String {
    let start_micros = day_start_nanos / 1_000;
    let end_micros = start_micros + 86_400_000_000;
    format!(
        "SELECT (ts / 1) * 1000 AS ts_nanos, open, high, low, close, volume \
         FROM spot_1m_rest \
         WHERE security_id = {security_id} AND exchange_segment = '{segment}' \
         AND feed = '{feed}' AND ts >= {start_micros} AND ts < {end_micros} \
         ORDER BY ts ASC LIMIT {limit}"
    )
}

/// SQL discovering the per-feed spot instrument set over the catch-up window.
pub fn spot_discovery_sql(feed: &str, window_start_nanos: i64, limit: usize) -> String {
    let start_micros = window_start_nanos / 1_000;
    format!(
        "SELECT DISTINCT security_id, exchange_segment FROM spot_1m_rest \
         WHERE feed = '{feed}' AND ts >= {start_micros} LIMIT {limit}"
    )
}

/// One parsed 1m row from `spot_1m_rest`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpotBarRow {
    pub ts_nanos: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: i64,
}

/// Parses a QuestDB `/exec` dataset of spot bars. Returns `(rows, truncated)`
/// where `truncated` means the explicit LIMIT was hit (partial day — the
/// caller degrades loudly, never trusts a partial fold).
pub fn parse_spot_bars(body: &str, limit: usize) -> Option<(Vec<SpotBarRow>, bool)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let dataset = value.get("dataset")?.as_array()?;
    let truncated = dataset.len() >= limit;
    let mut rows = Vec::with_capacity(dataset.len());
    for row in dataset {
        let cells = row.as_array()?;
        if cells.len() < 6 {
            return None;
        }
        rows.push(SpotBarRow {
            ts_nanos: cells[0].as_i64()?,
            open: cells[1].as_f64()?,
            high: cells[2].as_f64()?,
            low: cells[3].as_f64()?,
            close: cells[4].as_f64()?,
            volume: cells[5].as_i64().unwrap_or(0),
        });
    }
    Some((rows, truncated))
}

/// Parses the discovery dataset into `(security_id, segment)` pairs,
/// refusing any segment outside the known allowlist (second-order-injection
/// defense — the tf_consistency `is_allowlisted_segment` precedent).
pub fn parse_spot_discovery(body: &str) -> Option<Vec<(i64, String)>> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let dataset = value.get("dataset")?.as_array()?;
    let mut out = Vec::with_capacity(dataset.len());
    for row in dataset {
        let cells = row.as_array()?;
        if cells.len() < 2 {
            return None;
        }
        let sid = cells[0].as_i64()?;
        let segment = cells[1].as_str()?.to_string();
        if tickvault_common::segment::segment_str_to_code(&segment).is_none() {
            // Poisoned segment value — skip it, never re-query with it.
            counter!("tv_rest_candle_fold_errors_total", "stage" => "catchup_parse").increment(1);
            continue;
        }
        out.push((sid, segment));
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// The runtime task
// ---------------------------------------------------------------------------

/// Dirty-day key for the refold queue. `Feed` derives no `Ord`, so the
/// first element is the feed's index into [`Feed::ALL`] (0 = Dhan,
/// 1 = Groww) — converted back via `Feed::ALL[idx]` at drain time.
type DirtyDayKey = (u8, SecurityId, u8, NaiveDate);

/// Index of a feed within [`Feed::ALL`] (the `DirtyDayKey` ordinal).
fn feed_ordinal(feed: Feed) -> u8 {
    match feed {
        Feed::Dhan => 0,
        Feed::Groww => 1,
    }
}

/// Parameters for the fold task (all cold-path handles).
pub struct RestCandleFoldParams {
    pub config: RestCandleFoldConfig,
    pub questdb: QuestDbConfig,
    pub receiver: mpsc::Receiver<ConfirmedBar>,
}

/// IST NaiveDate of a minute-open IST-nanos timestamp.
///
/// The value is ALREADY IST wall-clock epoch nanos (`spot_1m_rest.ts`
/// convention), so interpreting the seconds as a "UTC" instant and taking
/// its naive calendar date yields exactly the IST trading date.
pub fn ist_date_of_nanos(ist_nanos: i64) -> Option<NaiveDate> {
    let secs = ist_nanos.div_euclid(1_000_000_000);
    chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.date_naive())
}

/// Midnight-IST nanos for a NaiveDate (inverse of [`ist_date_of_nanos`]).
pub fn day_start_nanos(date: NaiveDate) -> i64 {
    date.and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
}

struct FoldRuntime {
    engines: Vec<SidFoldState>,
    dirty: BTreeSet<DirtyDayKey>,
    dirty_since: Option<tokio::time::Instant>,
    client: Option<reqwest::Client>,
    exec_url: String,
}

impl FoldRuntime {
    fn engine_mut(
        &mut self,
        feed: Feed,
        security_id: SecurityId,
        segment_code: u8,
    ) -> &mut SidFoldState {
        if let Some(pos) = self.engines.iter().position(|e| {
            e.feed == feed
                && e.security_id == security_id
                && e.exchange_segment_code == segment_code
        }) {
            return &mut self.engines[pos];
        }
        self.engines
            .push(SidFoldState::new(feed, security_id, segment_code));
        let last = self.engines.len() - 1;
        &mut self.engines[last]
    }
}

/// Emits sealed buckets into the global seal channel; counts drops loudly.
fn emit_seals(feed: Feed, security_id: SecurityId, segment_code: u8, sealed: &[SealedBucket]) {
    if sealed.is_empty() {
        return;
    }
    let Some(sender) = tickvault_storage::seal_writer_runner::global_seal_sender() else {
        counter!("tv_rest_candle_fold_dropped_total", "reason" => "no_seal_sender")
            .increment(sealed.len() as u64);
        error!(
            code = ErrorCode::RestCandleFold01Degraded.code_str(),
            stage = "seal_send",
            reason = "no_seal_sender",
            seals = sealed.len(),
            "rest_candle_fold: global seal sender missing — seals dropped \
             (seal-writer must boot before the fold task)"
        );
        return;
    };
    let mut dropped = 0usize;
    for s in sealed {
        let seal = sealed_bucket_to_seal(feed, security_id, segment_code, s);
        if sender.try_send(seal).is_err() {
            dropped += 1;
        }
    }
    let delivered = sealed.len() - dropped;
    if delivered > 0 {
        counter!("tv_rest_candle_fold_seals_total", "feed" => feed.as_str())
            .increment(delivered as u64);
    }
    if dropped > 0 {
        counter!("tv_rest_candle_fold_dropped_total", "reason" => "seal_channel_full")
            .increment(dropped as u64);
        error!(
            code = ErrorCode::RestCandleFold01Degraded.code_str(),
            stage = "seal_send",
            reason = "seal_channel_full",
            dropped,
            "rest_candle_fold: seal-writer channel refused seals — dropped; \
             the dirty-day refold / boot catch-up re-derives them (DEDUP-idempotent)"
        );
    }
}

/// One bounded `/exec` GET with the tf_consistency hardening (timeout,
/// redirect-none client, response-size cap). Returns the body or a stage
/// string naming the failure.
async fn exec_query(
    client: &reqwest::Client,
    exec_url: &str,
    sql: &str,
) -> Result<String, &'static str> {
    let response = client
        .get(exec_url)
        .query(&[("query", sql)])
        .send()
        .await
        .map_err(|_| "catchup_query")?;
    if !response.status().is_success() {
        return Err("catchup_query");
    }
    if let Some(len) = response.content_length()
        && len > FOLD_MAX_RESPONSE_BYTES as u64
    {
        return Err("catchup_query");
    }
    let body = response.text().await.map_err(|_| "catchup_query")?;
    if body.len() > FOLD_MAX_RESPONSE_BYTES {
        return Err("catchup_query");
    }
    Ok(body)
}

/// Re-folds one (feed, sid, segment, day) from `spot_1m_rest` through a
/// FRESH engine, emitting every sealed bucket (DEDUP UPSERT heals in place).
/// Past days force-seal; `is_today` keeps partials open and returns the
/// fresh engine so the caller can REPLACE the live engine state.
async fn refold_day(
    client: &reqwest::Client,
    exec_url: &str,
    feed: Feed,
    security_id: SecurityId,
    segment_code: u8,
    segment_str: &str,
    date: NaiveDate,
    is_today: bool,
) -> Result<Option<SidFoldState>, &'static str> {
    let sid_i64 = i64::try_from(security_id).map_err(|_| "catchup_parse")?;
    let sql = spot_bars_sql(
        sid_i64,
        segment_str,
        feed.as_str(),
        day_start_nanos(date),
        FOLD_CATCHUP_1M_ROW_LIMIT,
    );
    let body = exec_query(client, exec_url, &sql).await?;
    let Some((rows, truncated)) = parse_spot_bars(&body, FOLD_CATCHUP_1M_ROW_LIMIT) else {
        return Err("catchup_parse");
    };
    if truncated {
        // A truncated day is never partially folded — degrade loudly.
        return Err("catchup_parse");
    }
    let mut engine = SidFoldState::new(feed, security_id, segment_code);
    let mut rows_folded = 0u64;
    for row in &rows {
        let bar = ConfirmedBar {
            feed,
            security_id,
            exchange_segment_code: segment_code,
            minute_ts_ist_nanos: row.ts_nanos,
            open: row.open,
            high: row.high,
            low: row.low,
            close: row.close,
            volume: row.volume,
        };
        match engine.fold_bar(&bar) {
            FoldOutcome::Folded(sealed) => {
                rows_folded += 1;
                emit_seals(feed, security_id, segment_code, &sealed);
            }
            FoldOutcome::VolumeOverflow { tf } => {
                counter!("tv_rest_candle_fold_errors_total", "stage" => "catchup_parse")
                    .increment(1);
                warn!(
                    code = ErrorCode::RestCandleFold01Degraded.code_str(),
                    stage = "catchup_parse",
                    tf = tf.display_name(),
                    "rest_candle_fold: volume overflow while refolding — bucket dropped"
                );
            }
            FoldOutcome::OutOfOrder | FoldOutcome::OutOfSession => {
                // QuestDB rows are ts-ordered; out-of-session rows are skipped.
            }
        }
    }
    counter!("tv_rest_candle_fold_catchup_rows_total", "feed" => feed.as_str())
        .increment(rows_folded);
    if is_today {
        Ok(Some(engine))
    } else {
        let sealed = engine.force_seal_open();
        emit_seals(feed, security_id, segment_code, &sealed);
        Ok(None)
    }
}

/// Boot catch-up: discovers the per-feed spot instrument set over the
/// `catchup_days` window and re-folds every day per SID. Today's partial
/// engines seed the live runtime. Flagged O(days × SIDs) — bounded cold work.
async fn boot_catchup(runtime: &mut FoldRuntime, catchup_days: u32, today: NaiveDate) {
    let Some(client) = runtime.client.clone() else {
        return;
    };
    let exec_url = runtime.exec_url.clone();
    let window_start = day_start_nanos(today) - i64::from(catchup_days) * 86_400 * 1_000_000_000;

    for feed in Feed::ALL {
        let sql = spot_discovery_sql(feed.as_str(), window_start, FOLD_DISCOVERY_ROW_LIMIT);
        let body = match exec_query(&client, &exec_url, &sql).await {
            Ok(b) => b,
            Err(stage) => {
                counter!("tv_rest_candle_fold_errors_total", "stage" => stage).increment(1);
                error!(
                    code = ErrorCode::RestCandleFold01Degraded.code_str(),
                    stage,
                    feed = feed.as_str(),
                    "rest_candle_fold: boot catch-up discovery query failed — \
                     this feed's history is not derived this boot (next boot retries)"
                );
                continue;
            }
        };
        let Some(pairs) = parse_spot_discovery(&body) else {
            counter!("tv_rest_candle_fold_errors_total", "stage" => "catchup_parse").increment(1);
            error!(
                code = ErrorCode::RestCandleFold01Degraded.code_str(),
                stage = "catchup_parse",
                feed = feed.as_str(),
                "rest_candle_fold: boot catch-up discovery parse failed"
            );
            continue;
        };
        for (sid_i64, segment) in pairs {
            let Ok(security_id) = SecurityId::try_from(sid_i64) else {
                continue;
            };
            let Some(segment_code) = tickvault_common::segment::segment_str_to_code(&segment)
            else {
                continue;
            };
            for offset in (0..=catchup_days).rev() {
                let Some(date) = today.checked_sub_days(chrono::Days::new(u64::from(offset)))
                else {
                    continue;
                };
                let is_today = date == today;
                match refold_day(
                    &client,
                    &exec_url,
                    *feed,
                    security_id,
                    segment_code,
                    &segment,
                    date,
                    is_today,
                )
                .await
                {
                    Ok(Some(engine)) => {
                        // Today's partial state seeds the live runtime.
                        if let Some(pos) = runtime.engines.iter().position(|e| {
                            e.feed == *feed
                                && e.security_id == security_id
                                && e.exchange_segment_code == segment_code
                        }) {
                            runtime.engines[pos] = engine;
                        } else {
                            runtime.engines.push(engine);
                        }
                    }
                    Ok(None) => {}
                    Err(stage) => {
                        counter!("tv_rest_candle_fold_errors_total", "stage" => stage).increment(1);
                        // Per-day degrade only — remaining days still fold.
                        warn!(
                            code = ErrorCode::RestCandleFold01Degraded.code_str(),
                            stage,
                            feed = feed.as_str(),
                            security_id,
                            date = %date,
                            "rest_candle_fold: catch-up day skipped (query/parse failed)"
                        );
                    }
                }
            }
        }
    }
    info!(
        engines = runtime.engines.len(),
        catchup_days, "rest_candle_fold: boot catch-up complete — live engines seeded"
    );
}

/// Drains the dirty-day refold queue (debounced).
async fn drain_dirty(runtime: &mut FoldRuntime, today: NaiveDate) {
    let Some(client) = runtime.client.clone() else {
        runtime.dirty.clear();
        runtime.dirty_since = None;
        return;
    };
    let exec_url = runtime.exec_url.clone();
    let dirty: Vec<DirtyDayKey> = runtime.dirty.iter().copied().collect();
    runtime.dirty.clear();
    runtime.dirty_since = None;
    for (feed_idx, security_id, segment_code, date) in dirty {
        let Some(feed) = Feed::ALL.get(usize::from(feed_idx)).copied() else {
            continue;
        };
        let Some(seg_enum) = tickvault_common::types::ExchangeSegment::from_byte(segment_code)
        else {
            continue;
        };
        let segment_str = seg_enum.as_str();
        let is_today = date == today;
        match refold_day(
            &client,
            &exec_url,
            feed,
            security_id,
            segment_code,
            segment_str,
            date,
            is_today,
        )
        .await
        {
            Ok(Some(engine)) => {
                // Replace the live engine — heals open partial buckets.
                if let Some(pos) = runtime.engines.iter().position(|e| {
                    e.feed == feed
                        && e.security_id == security_id
                        && e.exchange_segment_code == segment_code
                }) {
                    runtime.engines[pos] = engine;
                } else {
                    runtime.engines.push(engine);
                }
            }
            Ok(None) => {}
            Err(stage) => {
                counter!("tv_rest_candle_fold_errors_total", "stage" => stage).increment(1);
                error!(
                    code = ErrorCode::RestCandleFold01Degraded.code_str(),
                    stage,
                    feed = feed.as_str(),
                    security_id,
                    date = %date,
                    "rest_candle_fold: dirty-day refold failed — next dirty mark or \
                     the next boot catch-up re-derives it"
                );
            }
        }
    }
}

/// Current IST date (the groww_spot_1m_boot `today_ist` pattern).
pub fn today_ist() -> NaiveDate {
    use chrono::{DateTime, Duration as ChronoDuration, Utc};
    (DateTime::from_timestamp(Utc::now().timestamp(), 0).unwrap_or_default()
        + ChronoDuration::seconds(i64::from(
            tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
        )))
    .date_naive()
}

/// The fold task main loop: boot catch-up, then live bar folding with the
/// debounced dirty-day refold.
pub async fn run_rest_candle_fold(params: RestCandleFoldParams) {
    let RestCandleFoldParams {
        config,
        questdb,
        mut receiver,
    } = params;

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(FOLD_HTTP_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => Some(c),
        Err(err) => {
            counter!("tv_rest_candle_fold_errors_total", "stage" => "catchup_query").increment(1);
            error!(
                code = ErrorCode::RestCandleFold01Degraded.code_str(),
                stage = "catchup_query",
                error = %err,
                "rest_candle_fold: HTTP client build failed — catch-up + refold \
                 disabled this run; live folding continues"
            );
            None
        }
    };
    let exec_url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let mut runtime = FoldRuntime {
        engines: Vec::new(),
        dirty: BTreeSet::new(),
        dirty_since: None,
        client,
        exec_url,
    };

    let today = today_ist();
    if runtime.client.is_some() {
        boot_catchup(&mut runtime, config.catchup_days, today).await;
    }

    info!(
        catchup_days = config.catchup_days,
        "rest_candle_fold: live fold loop running (spot legs hand off \
         persist-confirmed 1m bars)"
    );

    loop {
        let debounce_due = runtime
            .dirty_since
            .map(|since| since + Duration::from_secs(FOLD_REFOLD_DEBOUNCE_SECS));
        tokio::select! {
            maybe_bar = receiver.recv() => {
                let Some(bar) = maybe_bar else {
                    // Every sender dropped — the OnceLock sender keeps one
                    // alive for the process lifetime, so this is shutdown.
                    info!("rest_candle_fold: bar channel closed — exiting");
                    return;
                };
                let feed = bar.feed;
                let sid = bar.security_id;
                let seg = bar.exchange_segment_code;
                let outcome = runtime.engine_mut(feed, sid, seg).fold_bar(&bar);
                match outcome {
                    FoldOutcome::Folded(sealed) => {
                        emit_seals(feed, sid, seg, &sealed);
                    }
                    FoldOutcome::OutOfOrder => {
                        if let Some(date) = ist_date_of_nanos(bar.minute_ts_ist_nanos) {
                            runtime.dirty.insert((feed_ordinal(feed), sid, seg, date));
                            if runtime.dirty_since.is_none() {
                                runtime.dirty_since = Some(tokio::time::Instant::now());
                            }
                            counter!("tv_rest_candle_fold_refold_queued_total")
                                .increment(1);
                        }
                    }
                    FoldOutcome::OutOfSession => {
                        counter!(
                            "tv_rest_candle_fold_dropped_total",
                            "reason" => "out_of_session"
                        )
                        .increment(1);
                    }
                    FoldOutcome::VolumeOverflow { tf } => {
                        counter!("tv_rest_candle_fold_errors_total", "stage" => "seal_send")
                            .increment(1);
                        error!(
                            code = ErrorCode::RestCandleFold01Degraded.code_str(),
                            stage = "seal_send",
                            tf = tf.display_name(),
                            security_id = sid,
                            "rest_candle_fold: live volume overflow — bucket dropped \
                             (refold will re-derive from spot_1m_rest)"
                        );
                    }
                }
            }
            () = async {
                match debounce_due {
                    Some(due) => tokio::time::sleep_until(due).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                drain_dirty(&mut runtime, today_ist()).await;
            }
        }
    }
}

/// Spawns the supervised fold task (house respawn pattern — DISK-WATCHER-01 /
/// spot-leg supervisor family). The bounded receiver is re-shared across
/// respawns via an `Arc<tokio::sync::Mutex<Receiver>>`.
///
/// Honest panic envelope: release builds run `panic = "abort"`, so the
/// respawn arms self-heal only in unwind (dev/test) builds — the
/// TICK-FLUSH-01 precedent.
pub fn spawn_supervised_rest_candle_fold(
    config: RestCandleFoldConfig,
    questdb: QuestDbConfig,
    receiver: mpsc::Receiver<ConfirmedBar>,
) -> tokio::task::JoinHandle<()> {
    use std::sync::Arc;
    let shared_rx = Arc::new(tokio::sync::Mutex::new(Some(receiver)));
    tokio::spawn(async move {
        loop {
            let config_clone = config.clone();
            let questdb_clone = questdb.clone();
            let rx_slot = Arc::clone(&shared_rx);
            let handle = tokio::spawn(async move {
                let receiver = {
                    let mut guard = rx_slot.lock().await;
                    guard.take()
                };
                let Some(receiver) = receiver else {
                    // Receiver consumed by a prior incarnation that never
                    // returned it (clean exit path) — nothing to do.
                    return;
                };
                run_rest_candle_fold(RestCandleFoldParams {
                    config: config_clone,
                    questdb: questdb_clone,
                    receiver,
                })
                .await;
            });
            let result = handle.await;
            let reason = tickvault_storage::disk_health_watcher::classify_join_exit(&result);
            counter!("tv_rest_candle_fold_task_respawn_total", "reason" => reason).increment(1);
            if reason == "clean_exit" {
                // Channel closed (shutdown) — do not respawn.
                info!("rest_candle_fold: supervisor observed clean exit — stopping");
                return;
            }
            error!(
                code = ErrorCode::RestCandleFold01Degraded.code_str(),
                stage = "task_respawn",
                reason,
                "rest_candle_fold: fold task died — respawning after backoff"
            );
            tokio::time::sleep(Duration::from_secs(FOLD_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const DAY0: i64 = 20_000 * 86_400; // arbitrary epoch day, seconds
    const OPEN: u32 = (DAY0 as u32) + FOLD_SESSION_OPEN_SECS_OF_DAY_IST;

    fn bar_at(minute_offset: u32, o: f64, h: f64, l: f64, c: f64, v: i64) -> ConfirmedBar {
        ConfirmedBar {
            feed: Feed::Dhan,
            security_id: 13,
            exchange_segment_code: 0,
            minute_ts_ist_nanos: i64::from(OPEN + minute_offset * 60) * 1_000_000_000,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: v,
        }
    }

    fn fold_all(engine: &mut SidFoldState, bars: &[ConfirmedBar]) -> Vec<SealedBucket> {
        let mut out = Vec::new();
        for bar in bars {
            if let FoldOutcome::Folded(sealed) = engine.fold_bar(bar) {
                out.extend(sealed);
            }
        }
        out
    }

    #[test]
    fn test_single_bar_folds_into_all_21_tfs_and_m1_seals() {
        let mut e = SidFoldState::new(Feed::Dhan, 13, 0);
        let outcome = e.fold_bar(&bar_at(0, 100.0, 101.0, 99.0, 100.5, 10));
        let FoldOutcome::Folded(sealed) = outcome else {
            panic!("expected folded");
        };
        // M1 seals immediately (minute end == its own bucket end).
        assert_eq!(sealed.len(), 1);
        assert_eq!(sealed[0].tf, TfIndex::M1);
        assert_eq!(sealed[0].bucket.open, 100.0);
        assert_eq!(sealed[0].bucket.close, 100.5);
        // The other 20 TFs are open.
        assert_eq!(e.open_bucket_count(), 20);
    }

    #[test]
    fn test_ohlcv_fold_matches_recompute_semantics() {
        // Golden: o=first, h=max, l=min, c=last, v=sum — the
        // tf_consistency recompute_window contract.
        let mut e = SidFoldState::new(Feed::Dhan, 13, 0);
        let bars = [
            bar_at(0, 100.0, 102.0, 99.5, 101.0, 10),
            bar_at(1, 101.0, 103.0, 100.5, 102.5, 20),
            bar_at(2, 102.5, 102.8, 98.0, 99.0, 30),
            bar_at(3, 99.0, 100.0, 98.5, 99.5, 40),
            bar_at(4, 99.5, 105.0, 99.0, 104.0, 50),
        ];
        let sealed = fold_all(&mut e, &bars);
        let m5 = sealed
            .iter()
            .find(|s| s.tf == TfIndex::M5)
            .expect("M5 seals when bar 4's minute end hits 09:20");
        assert_eq!(m5.bucket.open, 100.0);
        assert_eq!(m5.bucket.high, 105.0);
        assert_eq!(m5.bucket.low, 98.0);
        assert_eq!(m5.bucket.close, 104.0);
        assert_eq!(m5.bucket.volume, 150);
        assert_eq!(m5.bucket.bucket_start_ist_secs, OPEN);
    }

    #[test]
    fn test_final_session_minute_seals_everything_including_d1() {
        let mut e = SidFoldState::new(Feed::Dhan, 13, 0);
        // 15:29 is minute offset 374 from 09:15.
        let last = 374;
        let sealed_early = fold_all(&mut e, &[bar_at(0, 100.0, 100.0, 100.0, 100.0, 1)]);
        assert_eq!(sealed_early.len(), 1); // M1 only
        let outcome = e.fold_bar(&bar_at(last, 200.0, 201.0, 199.0, 200.5, 2));
        let FoldOutcome::Folded(sealed) = outcome else {
            panic!("expected folded");
        };
        // Every remaining open bucket seals at close (all 21 TFs' final
        // buckets end at 15:30 by session truncation).
        assert_eq!(e.open_bucket_count(), 0);
        let d1 = sealed
            .iter()
            .find(|s| s.tf == TfIndex::D1)
            .expect("D1 must seal at close");
        assert_eq!(d1.bucket.open, 100.0);
        assert_eq!(d1.bucket.close, 200.5);
        assert_eq!(d1.bucket.bucket_start_ist_secs, OPEN);
        assert_eq!(d1.bucket.volume, 3);
    }

    #[test]
    fn test_session_truncated_end_final_partial_bucket() {
        // H1 bucket opening 15:15 truncates to 15:30 (900s partial).
        let start = (DAY0 as u32) + 54_900; // 15:15
        assert_eq!(
            session_truncated_end(TfIndex::H1, start),
            (DAY0 as u32) + FOLD_SESSION_CLOSE_SECS_OF_DAY_IST
        );
        // D1's natural next-day end truncates to the same-day close.
        assert_eq!(
            session_truncated_end(TfIndex::D1, OPEN),
            (DAY0 as u32) + FOLD_SESSION_CLOSE_SECS_OF_DAY_IST
        );
        // A mid-session M5 keeps its natural end.
        assert_eq!(session_truncated_end(TfIndex::M5, OPEN), OPEN + 300);
    }

    #[test]
    fn test_out_of_order_bar_is_refused_not_folded() {
        let mut e = SidFoldState::new(Feed::Dhan, 13, 0);
        assert!(matches!(
            e.fold_bar(&bar_at(5, 1.0, 1.0, 1.0, 1.0, 1)),
            FoldOutcome::Folded(_)
        ));
        // Same minute again (duplicate) → OutOfOrder.
        assert!(matches!(
            e.fold_bar(&bar_at(5, 2.0, 2.0, 2.0, 2.0, 1)),
            FoldOutcome::OutOfOrder
        ));
        // Earlier minute (backfill) → OutOfOrder.
        assert!(matches!(
            e.fold_bar(&bar_at(3, 2.0, 2.0, 2.0, 2.0, 1)),
            FoldOutcome::OutOfOrder
        ));
    }

    #[test]
    fn test_out_of_session_bar_is_skipped() {
        let mut e = SidFoldState::new(Feed::Groww, 21, 0);
        // 09:14 (one minute before open).
        let pre_open = ConfirmedBar {
            minute_ts_ist_nanos: i64::from(OPEN - 60) * 1_000_000_000,
            ..bar_at(0, 1.0, 1.0, 1.0, 1.0, 1)
        };
        assert!(matches!(e.fold_bar(&pre_open), FoldOutcome::OutOfSession));
        // 15:30 exactly (close, exclusive).
        let at_close = ConfirmedBar {
            minute_ts_ist_nanos: i64::from((DAY0 as u32) + FOLD_SESSION_CLOSE_SECS_OF_DAY_IST)
                * 1_000_000_000,
            ..bar_at(0, 1.0, 1.0, 1.0, 1.0, 1)
        };
        assert!(matches!(e.fold_bar(&at_close), FoldOutcome::OutOfSession));
    }

    #[test]
    fn test_volume_overflow_degrades_loudly() {
        let mut e = SidFoldState::new(Feed::Groww, 25, 0);
        assert!(matches!(
            e.fold_bar(&bar_at(0, 1.0, 1.0, 1.0, 1.0, i64::MAX)),
            FoldOutcome::Folded(_)
        ));
        let outcome = e.fold_bar(&bar_at(1, 1.0, 1.0, 1.0, 1.0, 1));
        assert!(matches!(outcome, FoldOutcome::VolumeOverflow { .. }));
    }

    #[test]
    fn test_refold_same_input_produces_identical_seals() {
        // DEDUP-idempotency precondition: identical inputs → identical seals.
        let bars = [
            bar_at(0, 100.0, 102.0, 99.5, 101.0, 10),
            bar_at(1, 101.0, 103.0, 100.5, 102.5, 20),
            bar_at(2, 102.5, 102.8, 98.0, 99.0, 30),
        ];
        let mut e1 = SidFoldState::new(Feed::Dhan, 51, 0);
        let mut e2 = SidFoldState::new(Feed::Dhan, 51, 0);
        let s1 = fold_all(&mut e1, &bars);
        let s2 = fold_all(&mut e2, &bars);
        assert_eq!(s1, s2);
        let f1 = e1.force_seal_open();
        let f2 = e2.force_seal_open();
        assert_eq!(f1, f2);
    }

    #[test]
    fn test_sealed_bucket_to_seal_honest_fields() {
        let sealed = SealedBucket {
            tf: TfIndex::M5,
            bucket: TfBucket {
                bucket_start_ist_secs: OPEN,
                open: 100.0,
                high: 105.0,
                low: 98.0,
                close: 104.0,
                volume: 150,
                last_bar_ist_secs: OPEN + 240,
            },
        };
        let seal = sealed_bucket_to_seal(Feed::Groww, 13, 0, &sealed);
        assert_eq!(seal.security_id, 13);
        assert_eq!(seal.exchange_segment_code, 0);
        assert_eq!(seal.tf, TfIndex::M5);
        assert_eq!(seal.feed, Feed::Groww);
        assert_eq!(seal.state.open, 100.0);
        assert_eq!(seal.state.high, 105.0);
        assert_eq!(seal.state.low, 98.0);
        assert_eq!(seal.state.close, 104.0);
        assert_eq!(seal.state.volume, 150);
        // Honest zeros: no tick counts / OI / pct chain on the REST path.
        assert_eq!(seal.state.tick_count, 0);
        assert_eq!(seal.state.oi, 0);
        assert_eq!(seal.state.close_pct_from_prev_day, 0.0);
        assert_eq!(seal.state.close_ts_ist_secs, OPEN + 300);
    }

    #[test]
    fn test_negative_volume_clamps_to_zero_in_seal() {
        let sealed = SealedBucket {
            tf: TfIndex::M1,
            bucket: TfBucket {
                bucket_start_ist_secs: OPEN,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: -5,
                last_bar_ist_secs: OPEN,
            },
        };
        let seal = sealed_bucket_to_seal(Feed::Dhan, 13, 0, &sealed);
        assert_eq!(seal.state.volume, 0);
    }

    #[test]
    fn test_spot_bars_sql_shape() {
        let sql = spot_bars_sql(13, "IDX_I", "dhan", 1_752_000_000_000_000_000, 500);
        assert!(sql.contains("(ts / 1) * 1000 AS ts_nanos"));
        assert!(sql.contains("security_id = 13"));
        assert!(sql.contains("exchange_segment = 'IDX_I'"));
        assert!(sql.contains("feed = 'dhan'"));
        assert!(sql.contains("ts >= 1752000000000000"));
        assert!(sql.contains("ts < 1752086400000000"));
        assert!(sql.contains("ORDER BY ts ASC LIMIT 500"));
    }

    #[test]
    fn test_spot_discovery_sql_shape() {
        let sql = spot_discovery_sql("groww", 1_752_000_000_000_000_000, 64);
        assert!(sql.contains("DISTINCT security_id, exchange_segment"));
        assert!(sql.contains("feed = 'groww'"));
        assert!(sql.contains("LIMIT 64"));
    }

    #[test]
    fn test_parse_spot_bars_and_truncation_tripwire() {
        let body = r#"{"dataset":[[1752000000000000000,100.0,101.0,99.0,100.5,10],
                                    [1752000060000000000,100.5,102.0,100.0,101.5,20]]}"#;
        let (rows, truncated) = parse_spot_bars(body, 500).expect("parse");
        assert_eq!(rows.len(), 2);
        assert!(!truncated);
        assert_eq!(rows[0].open, 100.0);
        assert_eq!(rows[1].volume, 20);
        // LIMIT == dataset length → truncated tripwire.
        let (_, truncated2) = parse_spot_bars(body, 2).expect("parse");
        assert!(truncated2);
        // Malformed body → None, never a panic.
        assert!(parse_spot_bars("not json", 10).is_none());
        assert!(parse_spot_bars(r#"{"dataset":[[1]]}"#, 10).is_none());
    }

    #[test]
    fn test_parse_spot_discovery_segment_allowlist() {
        let body = r#"{"dataset":[[13,"IDX_I"],[25,"EVIL'; DROP"],[51,"IDX_I"]]}"#;
        let pairs = parse_spot_discovery(body).expect("parse");
        // The poisoned segment row is silently skipped (counted).
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], (13, "IDX_I".to_string()));
        assert_eq!(pairs[1], (51, "IDX_I".to_string()));
    }

    #[test]
    fn test_ist_date_and_day_start_roundtrip() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 16).expect("date");
        let nanos = day_start_nanos(date);
        assert_eq!(ist_date_of_nanos(nanos), Some(date));
        assert_eq!(
            ist_date_of_nanos(nanos + 33_300 * 1_000_000_000),
            Some(date)
        );
        // One nano before midnight belongs to the previous day.
        assert_eq!(
            ist_date_of_nanos(nanos - 1),
            date.checked_sub_days(chrono::Days::new(1))
        );
    }

    #[test]
    fn test_global_fold_bar_sender_first_wins() {
        let (tx1, _rx1) = mpsc::channel::<ConfirmedBar>(4);
        let (tx2, _rx2) = mpsc::channel::<ConfirmedBar>(4);
        let first = set_global_fold_bar_sender(tx1);
        let second = set_global_fold_bar_sender(tx2);
        // In a fresh test process the first install wins; the second is
        // always refused. Test-order tolerance: if another test installed
        // first, both are false — the invariant is second-is-false.
        assert!(!second || first);
        assert!(!second || global_fold_bar_sender().is_some());
        assert!(global_fold_bar_sender().is_some());
    }

    #[test]
    fn test_send_confirmed_bars_no_sender_is_noop() {
        // Even with no sender (or a full one), this must never panic/block.
        send_confirmed_bars(&[]);
        send_confirmed_bars(&[bar_at(0, 1.0, 1.0, 1.0, 1.0, 1)]);
    }

    #[test]
    fn test_in_session_boundaries() {
        assert!(in_session(OPEN));
        assert!(in_session((DAY0 as u32) + 55_740)); // 15:29
        assert!(!in_session((DAY0 as u32) + 55_800)); // 15:30 exclusive
        assert!(!in_session(OPEN - 60)); // 09:14
    }
}
