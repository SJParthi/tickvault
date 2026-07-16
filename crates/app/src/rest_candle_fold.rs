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
//!   volume = SATURATING i64 sum (saturation is counted + one coalesced warn
//!   per (feed, sid, day) — never a wrap, never a torn/partial fold),
//!   tick_count 0 (honest — REST bars carry no tick counts), oi 0, pct
//!   columns 0.0. Exact-match parity with
//!   `tf_consistency_boot::recompute_window` is golden-tested.
//! - Bucket grid: `TfIndex::bucket_start` (09:15-anchored) with effective end
//!   `min(start + tf_secs, 15:30 close)` — the tf_consistency session grid.
//!   A bucket seals when a LATER in-session bar crosses its effective end (or
//!   opens a later bucket); D1 + every open partial force-seals at/after the
//!   15:30 close bar.
//! - Out-of-order/duplicate bars (backfill/sweep repairs — 2026-07-16
//!   round-2 HIGH redesign; round-3 burst coalescing): every bar the
//!   process receives for the CURRENT trading day ALSO lands in a
//!   per-(feed, sid, segment) in-RAM **day-map** (minute → last-received
//!   bar, last-write-wins; ≤375 entries × 8 keys — trivial memory). A
//!   repair bar for the current day (out-of-order minute OR a value-UPDATE
//!   of an already-folded minute) updates the map; the consumer loop
//!   drains each arriving burst as ONE batch and refolds every dirty slot
//!   ONCE per batch from its map through a fresh engine (≤375 bars ×
//!   21 TFs — microseconds, cold path; a mid-day-outage sweep of N repairs
//!   costs one refold per slot, never N full-day refolds), swaps the live
//!   engine in place, and re-emits every bucket the refold closed (DEDUP
//!   UPSERT heals in place). Lossless for bars received IN-PROCESS THIS
//!   INCARNATION — the map holds every such bar, so the today REPAIR path
//!   performs NO QuestDB read and adds no ILP-ACK → `/exec` WAL-apply
//!   visibility race of its own. Honest crash-restart residual (round-3
//!   doc-honesty): the boot catch-up's TODAY seed IS an `/exec` read — a
//!   minute persisted seconds before a crash-restart can be WAL-invisible
//!   to that seed and stays out of the derived candles until the NEXT
//!   boot's catch-up; the 15:40 IST tf-verify (Blind/mismatch) is the
//!   pager for that window. A bar dated AFTER the wall-clock IST today
//!   NEVER rolls the live day forward (the BOUNDARY-01 future-skew class —
//!   dropped + counted, `reason="future_dated"`); with no current day yet
//!   (failed/empty catch-up) only a TODAY-dated first bar is adopted — a
//!   past-dated first bar routes to the past-day queue (a 1-bar adopted
//!   refold would force-seal + DEDUP-clobber a previously-correct closed
//!   day). Only a repair for a PAST day (the day-map has already rolled)
//!   takes the bounded `/exec` refold path — it emits corrected CLOSED-day
//!   seals only and NEVER touches a live engine (kills the round-2
//!   midnight-clobber class: "today" is the ENGINE's current day, never
//!   rolled past the wall clock). The remaining same-session WAL-apply
//!   visibility lag applies ONLY to that past-day cold path, where it is
//!   gated by the trigger-minute presence check + bounded requeue.
//! - Boot catch-up: re-folds the last `catchup_days` (default 35) of
//!   `spot_1m_rest` per feed through the same engines — past days
//!   force-sealed, today's partials stay open for live continuation. Days
//!   iterate NEWEST→OLDEST (per-day folds are independent and every seal is
//!   DEDUP-UPSERT-keyed, so cross-day order is irrelevant to correctness —
//!   any residual seal-channel drop therefore hits the OLDEST days, never
//!   the trading-relevant newest ones), and seal emission is PACED: a full
//!   seal channel sleeps-and-retries the SAME seal under a bounded per-day
//!   wait budget before counting a drop (this is a cold boot path — waiting
//!   is correct; the live per-minute fold stays non-blocking `try_send`).
//!   Bounded `/exec` reads reuse the tf_consistency hardened shapes (micros
//!   WHERE window, explicit LIMIT tripwire, streamed 8 MiB response cap,
//!   segment allowlist, redirect-none client). Flagged honestly: catch-up
//!   is O(days × SIDs × rows) COLD-path work at boot — never the hot path.
//!   Retention interplay (round-2 LOW-2 correction): `spot_1m_rest` is a
//!   STANDARD-class table (the 90-day `retention_days` window — it is NOT
//!   in the 35-day market-data class), so the default `catchup_days = 35`
//!   always has source rows; a `catchup_days` beyond whatever history the
//!   archive→verify→drop sweep retained simply finds nothing for those
//!   days — harmless, bounded queries, never an error.
//! - Config-gated (`[rest_candle_fold]`, serde default OFF — fail-safe;
//!   base.toml opts in), supervised (house respawn pattern,
//!   `classify_join_exit`; the receiver lives in a shared slot behind an
//!   RAII re-park guard so an unwind-build respawn actually resumes
//!   consuming — release builds run `panic = "abort"`, where the honest
//!   recovery is process restart + boot catch-up), log-sink-only FOLD-01
//!   (`error!` + counters; no CloudWatch entry — delivery boundary in the
//!   rule file). The dense positive liveness signal is
//!   `tv_rest_candle_fold_heartbeat_total` (one increment per
//!   [`FOLD_HEARTBEAT_INTERVAL_SECS`] loop tick — alarm-ready, but
//!   metrics-local today per the same delivery boundary).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
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

/// Debounce window before a PAST-day dirty refold fires (coalesces a
/// backfill burst into one QuestDB round-trip per day). Round-2: the
/// CURRENT day never enters this queue — today repairs refold from the
/// in-RAM day-map synchronously, no debounce, no `/exec`.
pub const FOLD_REFOLD_DEBOUNCE_SECS: u64 = 5;

/// Max refold attempts per PAST-day dirty mark (M2 — a refold whose
/// `/exec` read lags the triggering row's ILP-flush visibility, or whose
/// query fails, re-queues bounded times before degrading loudly; each
/// retry is spaced by the [`FOLD_REFOLD_DEBOUNCE_SECS`] debounce).
pub const FOLD_REFOLD_MAX_ATTEMPTS: u32 = 5;

/// Supervisor respawn backoff (house pattern).
pub const FOLD_RESPAWN_BACKOFF_SECS: u64 = 30;

/// Dense liveness heartbeat cadence for the fold loop (HIGH-3 —
/// `tv_rest_candle_fold_heartbeat_total` increments once per tick even
/// when zero bars/seals moved, so a dead fold task is a visible flatline).
pub const FOLD_HEARTBEAT_INTERVAL_SECS: u64 = 60;

/// Paced catch-up emission: sleep this long when the seal channel is full
/// (cold boot path — waiting beats dropping).
pub const FOLD_CATCHUP_PACE_SLEEP_MS: u64 = 100;

/// Paced catch-up emission: total wait budget per refolded
/// (feed, SID, day) before drops are counted — round-2 LOW-7 wording: the
/// budget is per `refold_day` CALL, i.e. per (feed, SID, day), not per
/// calendar day; a wedged seal-writer costs at most one minute of boot
/// delay for EACH such refold.
pub const FOLD_CATCHUP_PACE_BUDGET_MS: u64 = 60_000;

// ---------------------------------------------------------------------------
// The confirmed-bar handoff (spot legs -> fold task)
// ---------------------------------------------------------------------------

/// One persist-CONFIRMED official 1m spot bar, as handed off by a spot leg
/// AFTER the `spot_1m_rest` ILP flush ACK (never before — a bar that failed
/// to persist must not derive candles the audit record does not back).
#[derive(Debug, Clone, Copy, PartialEq)]
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
             a dropped bar never enters the day-map, so the NEXT boot's catch-up \
             re-derives it from spot_1m_rest (DEDUP-idempotent)"
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
             an unwind-build supervisor respawn resumes it, but in release builds \
             (panic = abort) the honest recovery is process restart + boot catch-up \
             (DEDUP-idempotent re-derivation from spot_1m_rest)"
        );
    }
}

/// Round-2 LOW-6: a spot-leg identity the fold's `SecurityId` space cannot
/// carry (a negative i64 — defensive; today's ids are the 4 positive IDX_I
/// / stable Groww index ids). Counted per occurrence + ONE coalesced warn
/// per process — never a silent skip at the handoff sites.
pub fn note_unfoldable_identity(feed: Feed, security_id: i64) {
    use std::sync::atomic::{AtomicBool, Ordering};
    counter!("tv_rest_candle_fold_dropped_total", "reason" => "bad_identity").increment(1);
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            code = ErrorCode::RestCandleFold01Degraded.code_str(),
            stage = "bad_identity",
            feed = feed.as_str(),
            security_id,
            "rest_candle_fold: spot-leg bar identity does not fit the fold's \
             SecurityId space — bar skipped (counted under \
             reason=bad_identity; further occurrences coalesce to the counter)"
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
    /// A volume Σ that would overflow SATURATES at `i64::MAX` (counted +
    /// one coalesced warn per (feed, sid, day) — M3: never a torn fold,
    /// the watermark always advances).
    Folded(Vec<SealedBucket>),
    /// Bar's minute is ≤ the last folded minute — the caller marks the
    /// (feed, sid, segment, day) dirty for a QuestDB refold instead.
    OutOfOrder,
    /// Bar's minute lies outside [09:15, 15:30) IST — skipped + counted.
    OutOfSession,
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
    /// Coalescing latch for the M3 volume-saturation warn (one warn per
    /// (feed, sid, day) — the value is the IST epoch-day already warned).
    saturation_warned_day: Option<u32>,
}

impl SidFoldState {
    pub fn new(feed: Feed, security_id: SecurityId, exchange_segment_code: u8) -> Self {
        Self {
            feed,
            security_id,
            exchange_segment_code,
            buckets: [None; TF_COUNT],
            last_folded_minute_ist_secs: None,
            saturation_warned_day: None,
        }
    }

    /// M3: volume Σ saturated at `i64::MAX` — count every occurrence, warn
    /// ONCE per (feed, sid, day) (coalesced; audit Rule 4 edge discipline).
    fn note_volume_saturation(&mut self, minute_secs: u32) {
        counter!("tv_rest_candle_fold_volume_saturated_total").increment(1);
        let day = minute_secs / 86_400;
        if self.saturation_warned_day != Some(day) {
            self.saturation_warned_day = Some(day);
            warn!(
                code = ErrorCode::RestCandleFold01Degraded.code_str(),
                stage = "volume_saturated",
                feed = self.feed.as_str(),
                security_id = self.security_id,
                "rest_candle_fold: volume sum saturated at i64::MAX for this \
                 (feed, sid, day) — stored volume is a FLOOR for the affected \
                 buckets; OHLC and the fold watermark are unaffected"
            );
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
                    // Same bucket — fold in place. M3: the volume Σ is
                    // SATURATING (counted + coalesced warn) so a poisoned
                    // vendor volume can never tear the 21-TF state mid-loop
                    // or stall the ordering watermark.
                    let vol = match existing.volume.checked_add(bar.volume) {
                        Some(v) => v,
                        None => {
                            self.note_volume_saturation(minute_secs);
                            i64::MAX
                        }
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
// Current-day day-map refold (2026-07-16 round-2 HIGH — lossless, RAM-only)
// ---------------------------------------------------------------------------

/// Rebuilds a fresh engine from the day-map (minute-ordered — `BTreeMap`
/// iteration), returning it plus every bucket the refold closed. Pure —
/// no I/O; ≤375 bars × 21 TFs of constant work (microseconds, cold path).
/// The caller emits the sealed buckets (DEDUP UPSERT heals in place) and
/// swaps the returned engine over the live one.
pub fn refold_from_day_map(
    feed: Feed,
    security_id: SecurityId,
    segment_code: u8,
    day_map: &BTreeMap<u32, ConfirmedBar>,
) -> (SidFoldState, Vec<SealedBucket>) {
    let mut engine = SidFoldState::new(feed, security_id, segment_code);
    let mut sealed = Vec::new();
    for bar in day_map.values() {
        if let FoldOutcome::Folded(s) = engine.fold_bar(bar) {
            sealed.extend(s);
        }
    }
    (engine, sealed)
}

/// What [`SidDayFold::apply_live_bar`] did with a bar (the single-bar
/// dispatch surface — test-visible; the production loop drains bursts
/// through [`apply_bar_batch`], which defers repairs and refolds each
/// dirty slot ONCE per batch — round-3).
#[derive(Debug)]
pub enum LiveBarAction {
    /// In-order bar folded (includes the first bar of a NEW trading day,
    /// whose fold transition-seals the previous day's residue). The seals
    /// are ready to emit.
    Folded(Vec<SealedBucket>),
    /// A current-day repair (out-of-order minute or value-UPDATE): the
    /// day-map was updated and the WHOLE day refolded from RAM — the live
    /// engine was swapped; every bucket the refold closed is re-emitted
    /// (DEDUP-idempotent).
    Refolded(Vec<SealedBucket>),
    /// Value-identical redelivery of a bar already in the day-map
    /// (`PartialEq` compare — a NaN field compares unequal and falls
    /// through to a harmless idempotent refold) — nothing new to derive;
    /// counted, never refolded.
    DuplicateNoop,
    /// The bar belongs to a day OLDER than the engine's current day (or,
    /// round-3 LOW: a PAST-dated first bar while no current day exists) —
    /// the caller queues the bounded PAST-day `/exec` refold (which never
    /// touches live engines).
    PastDay,
    /// Round-3 (the BOUNDARY-01 future-skew class): the bar's IST date is
    /// AFTER the wall-clock IST today — it must NEVER roll the live day
    /// forward; dropped + counted (`reason="future_dated"`).
    FutureDated,
    /// Outside [09:15, 15:30) IST (or an unrepresentable timestamp) —
    /// skipped + counted.
    OutOfSession,
}

/// Batch-mode dispatch (round-3 repair-burst coalescing): identical to
/// [`LiveBarAction`] except a current-day repair is ABSORBED into the
/// day-map and DEFERRED — the caller refolds each dirty slot ONCE per
/// drained batch via [`SidDayFold::refold_current_day`], so a
/// mid-day-outage sweep of N repairs costs one refold per slot, never N.
#[derive(Debug)]
pub enum DeferredBarAction {
    /// In-order bar folded; seals ready to emit.
    Folded(Vec<SealedBucket>),
    /// Current-day repair (out-of-order minute or value-UPDATE) landed in
    /// the day-map WITHOUT refolding — the slot is DIRTY; the caller
    /// refolds it once at batch end.
    RepairDeferred,
    /// Value-identical redelivery (`PartialEq`; NaN-unequal falls through
    /// to a harmless idempotent refold) — counted no-op.
    DuplicateNoop,
    /// Routes to the caller's bounded PAST-day `/exec` queue.
    PastDay,
    /// Future-dated (after the wall-clock IST today) — dropped + counted.
    FutureDated,
    /// Outside the session window — skipped + counted.
    OutOfSession,
}

/// Per-(feed, sid, segment) LIVE state: the fold engine plus the current
/// trading day's in-RAM bar map — the lossless refold source for bars
/// received in-process THIS INCARNATION (round-2 HIGH: today repairs
/// never read QuestDB, so the ILP-ACK → `/exec` WAL-apply visibility lag
/// cannot corrupt live buckets; the boot TODAY seed IS an `/exec` read —
/// see the module doc's crash-restart residual, round-3).
#[derive(Debug)]
pub struct SidDayFold {
    pub engine: SidFoldState,
    /// The trading day the engine + day-map currently cover. Day identity
    /// is the ENGINE's day — never the wall clock — so a post-midnight
    /// repair for the still-open session day refolds from the retained
    /// map instead of clobbering it (the round-2 midnight MEDIUM).
    current_day: Option<NaiveDate>,
    /// Minute-open IST secs → last-received bar (last-write-wins).
    day_map: BTreeMap<u32, ConfirmedBar>,
    /// The NEWEST repair bar deferred since the last refold (2026-07-16
    /// fix round — same-batch day-roll displaced repair): a repair deferred
    /// earlier in a batch lives only in the day-map; when a NEW day's first
    /// bar rolls the map in the SAME batch, the batch-end refold covers the
    /// NEW day and the old-day repair would be silently dropped until the
    /// next boot's catch-up. Retaining the newest deferred repair lets the
    /// roll arm park it for the past-day `/exec` queue instead.
    pending_repair: Option<ConfirmedBar>,
    /// A pending repair displaced by a day roll — drained by the batch
    /// caller via [`Self::take_displaced_repair`] into the bounded
    /// past-day `/exec` refold queue (the M2 stale-read gate then verifies
    /// the repair minute is visible before the old day re-emits).
    displaced_repair: Option<ConfirmedBar>,
}

impl SidDayFold {
    pub fn new(feed: Feed, security_id: SecurityId, exchange_segment_code: u8) -> Self {
        Self {
            engine: SidFoldState::new(feed, security_id, exchange_segment_code),
            current_day: None,
            day_map: BTreeMap::new(),
            pending_repair: None,
            displaced_repair: None,
        }
    }

    /// Seeds live state from the boot catch-up's TODAY refold: the open
    /// engine plus the bars it folded — so a mid-session restart has a
    /// COMPLETE day-map and the first post-boot repair refolds losslessly.
    pub fn from_catchup(engine: SidFoldState, day: NaiveDate, bars: &[ConfirmedBar]) -> Self {
        let mut day_map = BTreeMap::new();
        for bar in bars {
            if let Ok(minute_secs) = u32::try_from(bar.minute_ts_ist_nanos / 1_000_000_000)
                && in_session(minute_secs)
            {
                day_map.insert(minute_secs, *bar);
            }
        }
        Self {
            engine,
            current_day: Some(day),
            day_map,
            pending_repair: None,
            displaced_repair: None,
        }
    }

    /// Drains the repair bar displaced by a day roll (if any) — the batch
    /// caller pushes it onto the past-day `/exec` refold queue so the OLD
    /// day still heals THIS session (2026-07-16 fix round). Only
    /// [`apply_bar_batch`] can leave one parked: the single-bar
    /// [`Self::apply_live_bar`] surface refolds every repair immediately,
    /// so its pending slot is always empty by roll time.
    pub fn take_displaced_repair(&mut self) -> Option<ConfirmedBar> {
        self.displaced_repair.take()
    }

    /// Test/forensics accessor: number of bars retained for the current day.
    pub fn day_map_len(&self) -> usize {
        self.day_map.len()
    }

    /// Test/forensics accessor: the day the engine + map currently cover.
    pub fn current_day(&self) -> Option<NaiveDate> {
        self.current_day
    }

    /// Applies one live confirmed bar via the DEFERRED path (round-3
    /// repair-burst coalescing): in-order bars fold into the live engine
    /// AND land in the day-map; a current-day repair (out-of-order OR
    /// value-update) is ABSORBED into the day-map and returned as
    /// [`DeferredBarAction::RepairDeferred`] WITHOUT refolding — the
    /// caller refolds each dirty slot ONCE per drained batch. A NEW day's
    /// first bar rolls the map ONLY when `bar_date <= today` (round-3
    /// future-skew clamp); a PAST-day bar routes to the bounded `/exec`
    /// path. `today` is the wall-clock IST date the caller sampled.
    pub fn apply_live_bar_deferred(
        &mut self,
        bar: &ConfirmedBar,
        today: NaiveDate,
    ) -> DeferredBarAction {
        let Ok(minute_secs) = u32::try_from(bar.minute_ts_ist_nanos / 1_000_000_000) else {
            return DeferredBarAction::OutOfSession;
        };
        if !in_session(minute_secs) {
            return DeferredBarAction::OutOfSession;
        }
        let Some(bar_date) = ist_date_of_nanos(bar.minute_ts_ist_nanos) else {
            return DeferredBarAction::OutOfSession;
        };
        match self.current_day {
            Some(day) if bar_date < day => return DeferredBarAction::PastDay,
            Some(day) if bar_date > day => {
                // Round-3 (the BOUNDARY-01 future-skew class): roll ONLY
                // when bar_date <= the wall-clock IST today — a
                // future-dated bar must never roll the live day forward
                // (it would clear the real day's map and mis-home every
                // later real bar).
                if bar_date > today {
                    return DeferredBarAction::FutureDated;
                }
                // 2026-07-16 fix round (same-batch day-roll displaced
                // repair): a repair deferred earlier in THIS batch lives
                // only in the map being cleared — park it so the batch
                // caller routes its OLD day through the past-day `/exec`
                // refold queue instead of silently dropping the repair
                // (which would only heal at the next boot's catch-up).
                if let Some(displaced) = self.pending_repair.take() {
                    self.displaced_repair = Some(displaced);
                }
                // Day roll: the fold below transition-seals every previous
                // day's open bucket (a later-day minute is always > the
                // watermark, so this bar folds in-order); the map restarts
                // for the new day.
                self.day_map.clear();
                self.current_day = Some(bar_date);
            }
            None => {
                // Round-3 LOW: with no current day (failed/empty boot
                // catch-up) adopt ONLY the wall-clock IST today. A
                // PAST-dated first bar routes to the past-day `/exec`
                // queue — adopting it would let a later 1-bar refold
                // force-seal and DEDUP-clobber a previously-correct
                // closed day; a FUTURE-dated first bar is dropped.
                if bar_date > today {
                    return DeferredBarAction::FutureDated;
                }
                if bar_date < today {
                    return DeferredBarAction::PastDay;
                }
                self.current_day = Some(bar_date);
            }
            Some(_) => {}
        }
        match self.engine.fold_bar(bar) {
            FoldOutcome::Folded(sealed) => {
                self.day_map.insert(minute_secs, *bar);
                DeferredBarAction::Folded(sealed)
            }
            FoldOutcome::OutOfOrder => {
                if self.day_map.get(&minute_secs) == Some(bar) {
                    return DeferredBarAction::DuplicateNoop;
                }
                self.day_map.insert(minute_secs, *bar);
                // Keep the NEWEST deferred repair minute (parity with the
                // M2 stale-read gate, which requires the newest triggering
                // minute visible before a past-day refold emits).
                if self
                    .pending_repair
                    .is_none_or(|prev| prev.minute_ts_ist_nanos < bar.minute_ts_ist_nanos)
                {
                    self.pending_repair = Some(*bar);
                }
                DeferredBarAction::RepairDeferred
            }
            // Unreachable (session-checked above) — defensive.
            FoldOutcome::OutOfSession => DeferredBarAction::OutOfSession,
        }
    }

    /// Refolds the WHOLE current day from the in-RAM day-map through a
    /// fresh engine and swaps it live, returning every bucket the refold
    /// closed (re-emitted DEDUP-idempotently). Round-3: called ONCE per
    /// dirty slot per drained batch — never per repair bar.
    pub fn refold_current_day(&mut self) -> Vec<SealedBucket> {
        // The refold absorbs every deferred repair still homed in the map.
        self.pending_repair = None;
        let (engine, sealed) = refold_from_day_map(
            self.engine.feed,
            self.engine.security_id,
            self.engine.exchange_segment_code,
            &self.day_map,
        );
        self.engine = engine;
        sealed
    }

    /// Single-bar convenience surface (round-2 semantics preserved): a
    /// current-day repair refolds IMMEDIATELY. The production loop uses
    /// [`apply_bar_batch`] instead, which defers repairs and refolds each
    /// dirty slot once per drained batch (round-3).
    pub fn apply_live_bar(&mut self, bar: &ConfirmedBar, today: NaiveDate) -> LiveBarAction {
        match self.apply_live_bar_deferred(bar, today) {
            DeferredBarAction::Folded(sealed) => LiveBarAction::Folded(sealed),
            DeferredBarAction::RepairDeferred => LiveBarAction::Refolded(self.refold_current_day()),
            DeferredBarAction::DuplicateNoop => LiveBarAction::DuplicateNoop,
            DeferredBarAction::PastDay => LiveBarAction::PastDay,
            DeferredBarAction::FutureDated => LiveBarAction::FutureDated,
            DeferredBarAction::OutOfSession => LiveBarAction::OutOfSession,
        }
    }
}

/// Finds or creates the (feed, sid, segment) live slot.
fn slot_mut(
    engines: &mut Vec<SidDayFold>,
    feed: Feed,
    security_id: SecurityId,
    segment_code: u8,
) -> &mut SidDayFold {
    if let Some(pos) = engines.iter().position(|s| {
        s.engine.feed == feed
            && s.engine.security_id == security_id
            && s.engine.exchange_segment_code == segment_code
    }) {
        return &mut engines[pos];
    }
    engines.push(SidDayFold::new(feed, security_id, segment_code));
    let last = engines.len() - 1;
    &mut engines[last]
}

/// One per-slot seal emission produced by [`apply_bar_batch`].
#[derive(Debug)]
pub struct BatchEmission {
    pub feed: Feed,
    pub security_id: SecurityId,
    pub segment_code: u8,
    pub sealed: Vec<SealedBucket>,
    /// True when this group is a dirty-slot batch refold (counted once
    /// per slot per batch under `tv_rest_candle_fold_day_refolds_total`).
    pub is_refold: bool,
}

/// Outcome of applying one drained bar batch (round-3 burst coalescing).
#[derive(Debug, Default)]
pub struct BatchOutcome {
    /// Ordered emissions: in-order folds per bar, then EXACTLY ONE refold
    /// group per dirty slot.
    pub emissions: Vec<BatchEmission>,
    /// PAST-day bars for the caller's bounded `/exec` dirty queue.
    pub past_day: Vec<ConfirmedBar>,
    pub duplicates: u64,
    pub out_of_session: u64,
    /// Round-3: future-dated drops (a bar dated after the wall-clock IST
    /// today never rolls the live day forward).
    pub future_dated: u64,
}

/// Applies one drained batch of confirmed bars (round-3 repair-burst
/// coalescing): every bar lands in its slot via the DEFERRED path, then
/// each slot that took a current-day repair is refolded ONCE — a
/// mid-day-outage sweep delivering N out-of-order repairs costs one
/// full-day refold per slot instead of N. In-order seals emitted before a
/// same-slot repair may momentarily carry pre-repair values; the
/// batch-end refold re-emits them corrected (DEDUP UPSERT heals in place).
pub fn apply_bar_batch(
    engines: &mut Vec<SidDayFold>,
    batch: &[ConfirmedBar],
    today: NaiveDate,
) -> BatchOutcome {
    let mut out = BatchOutcome::default();
    let mut dirty: Vec<(Feed, SecurityId, u8)> = Vec::new();
    for bar in batch {
        let slot = slot_mut(
            engines,
            bar.feed,
            bar.security_id,
            bar.exchange_segment_code,
        );
        match slot.apply_live_bar_deferred(bar, today) {
            DeferredBarAction::Folded(sealed) => out.emissions.push(BatchEmission {
                feed: bar.feed,
                security_id: bar.security_id,
                segment_code: bar.exchange_segment_code,
                sealed,
                is_refold: false,
            }),
            DeferredBarAction::RepairDeferred => {
                let key = (bar.feed, bar.security_id, bar.exchange_segment_code);
                if !dirty.contains(&key) {
                    dirty.push(key);
                }
            }
            DeferredBarAction::DuplicateNoop => out.duplicates += 1,
            DeferredBarAction::PastDay => out.past_day.push(*bar),
            DeferredBarAction::FutureDated => out.future_dated += 1,
            DeferredBarAction::OutOfSession => out.out_of_session += 1,
        }
        // 2026-07-16 fix round: a day roll in THIS batch may have displaced
        // a just-deferred OLD-day repair from the cleared map — route it to
        // the past-day `/exec` queue so the old day still heals this
        // session instead of waiting for the next boot's catch-up.
        if let Some(displaced) = slot.take_displaced_repair() {
            out.past_day.push(displaced);
        }
    }
    for (feed, security_id, segment_code) in dirty {
        let slot = slot_mut(engines, feed, security_id, segment_code);
        out.emissions.push(BatchEmission {
            feed,
            security_id,
            segment_code,
            sealed: slot.refold_current_day(),
            is_refold: true,
        });
    }
    out
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
            // Round-2 LOW-5: dual-format volume read — int OR finite float
            // (the 2026-07-15 Dhan scientific-notation wire-drift precedent,
            // mirroring `dhan_intraday_parse::timestamp_epoch_secs`). A
            // non-finite float degrades to 0, never a skipped row.
            volume: cells[5]
                .as_i64()
                .or_else(|| {
                    cells[5]
                        .as_f64()
                        .filter(|f| f.is_finite())
                        .map(|f| f.round() as i64)
                })
                .unwrap_or(0),
        });
    }
    Some((rows, truncated))
}

/// Parses the discovery dataset into `(security_id, segment)` pairs,
/// refusing any segment outside the known allowlist (second-order-injection
/// defense — the tf_consistency `is_allowlisted_segment` precedent).
///
/// Returns `(pairs, truncated)` — `truncated` means the dataset reached the
/// explicit LIMIT (M4 tripwire: a partial instrument set is NEVER silently
/// folded; the caller skips that feed's catch-up loudly).
pub fn parse_spot_discovery(body: &str, limit: usize) -> Option<(Vec<(i64, String)>, bool)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let dataset = value.get("dataset")?.as_array()?;
    let truncated = dataset.len() >= limit;
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
    Some((out, truncated))
}

/// M1 pure cap primitive: appends `chunk` to `acc` iff the result stays
/// within `cap`; returns `false` (acc unchanged) when it would exceed —
/// the caller fails the read CLOSED (oversize = query failure, never a
/// truncated parse).
pub fn accumulate_capped(acc: &mut Vec<u8>, chunk: &[u8], cap: usize) -> bool {
    if acc.len().saturating_add(chunk.len()) > cap {
        return false;
    }
    acc.extend_from_slice(chunk);
    true
}

// ---------------------------------------------------------------------------
// The runtime task
// ---------------------------------------------------------------------------

/// Dirty-day key for the PAST-day refold queue (round-2: the current day
/// never enters this queue — it refolds from the in-RAM day-map). `Feed`
/// derives no `Ord`, so the first element is the feed's index into
/// [`Feed::ALL`] (0 = Dhan, 1 = Groww) — converted back via
/// `Feed::ALL[idx]` at drain time.
type DirtyDayKey = (u8, SecurityId, u8, NaiveDate);

/// Index of a feed within [`Feed::ALL`] (the `DirtyDayKey` ordinal).
fn feed_ordinal(feed: Feed) -> u8 {
    match feed {
        Feed::Dhan => 0,
        Feed::Groww => 1,
    }
}

/// M2: one queued PAST-day refold mark. Carries the TRIGGERING bar's
/// minute (the newest out-of-order minute seen for the day) so the refold
/// can verify the `/exec` read actually SEES that row — the ILP-flush-ACK →
/// `/exec` visibility lag (QuestDB WAL apply) otherwise makes an immediate
/// refold re-emit the day WITHOUT the repair it was queued for (regressing
/// correct candles). `attempts` bounds the re-queue ladder. This gate is
/// the ONLY residual WAL-lag surface (round-2): past-day refolds emit
/// corrected CLOSED-day seals only and never touch a live engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyMark {
    /// Minute-open IST nanos of the newest triggering bar — the refold
    /// dataset must contain this row or the mark re-queues.
    pub required_minute_nanos: i64,
    /// Failed refold attempts so far (query failure OR stale read).
    pub attempts: u32,
}

/// M2 pure re-queue decision: after a failed refold attempt, re-queue while
/// the TOTAL attempts stay under [`FOLD_REFOLD_MAX_ATTEMPTS`]. Pure.
#[must_use]
pub fn should_requeue_refold(attempts_done: u32) -> bool {
    attempts_done < FOLD_REFOLD_MAX_ATTEMPTS
}

/// M2 pure stale-read check: `true` when the refold dataset contains the
/// required triggering minute (or no minute is required — boot catch-up).
/// Round-2: consumed ONLY by the PAST-day `/exec` refold path — the
/// current day refolds from the in-RAM day-map and needs no presence gate
/// (a presence-only check could never catch value-UPDATEs anyway, which is
/// exactly why the today `/exec` path was retired).
#[must_use]
pub fn rows_contain_minute(rows: &[SpotBarRow], required_minute_nanos: Option<i64>) -> bool {
    match required_minute_nanos {
        None => true,
        Some(m) => rows.iter().any(|r| r.ts_nanos == m),
    }
}

/// HIGH-2: the shared receiver slot the RAII guard re-parks into.
pub type SharedFoldReceiverSlot = Arc<Mutex<Option<mpsc::Receiver<ConfirmedBar>>>>;

/// HIGH-2 RAII re-park guard: takes the fold-bar receiver out of the shared
/// slot for one task incarnation and PUTS IT BACK on drop — unwind
/// included — so a supervisor respawn actually resumes consuming instead of
/// silently taking `None` forever (the pre-fix false-OK: the respawn
/// claimed recovery while the receiver had died with the old incarnation).
pub struct FoldReceiverGuard {
    slot: SharedFoldReceiverSlot,
    receiver: Option<mpsc::Receiver<ConfirmedBar>>,
}

impl FoldReceiverGuard {
    /// Takes the receiver from the slot; `None` when the slot is empty
    /// (a prior incarnation leaked it — a guard bug, LOUD at the caller).
    #[must_use]
    pub fn take(slot: &SharedFoldReceiverSlot) -> Option<Self> {
        let receiver = slot.lock().unwrap_or_else(PoisonError::into_inner).take()?;
        Some(Self {
            slot: Arc::clone(slot),
            receiver: Some(receiver),
        })
    }

    /// Receives the next confirmed bar (None = channel closed/shutdown).
    pub async fn recv(&mut self) -> Option<ConfirmedBar> {
        match self.receiver.as_mut() {
            Some(rx) => rx.recv().await,
            None => None,
        }
    }

    /// Non-blocking drain of the next queued bar (round-3 burst batching:
    /// the loop drains everything that arrived with the blocking recv so
    /// a repair burst coalesces into one refold per slot). `None` = empty
    /// OR closed (the next blocking `recv` reports the close).
    pub fn try_recv(&mut self) -> Option<ConfirmedBar> {
        self.receiver.as_mut()?.try_recv().ok()
    }
}

impl Drop for FoldReceiverGuard {
    fn drop(&mut self) {
        if let Some(rx) = self.receiver.take() {
            *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(rx);
        }
    }
}

/// Parameters for the fold task (all cold-path handles).
pub struct RestCandleFoldParams {
    pub config: RestCandleFoldConfig,
    pub questdb: QuestDbConfig,
    /// HIGH-2: the shared receiver slot — the task takes it through the
    /// RAII [`FoldReceiverGuard`] so every exit path (unwind included)
    /// re-parks it for the next incarnation.
    pub receiver_slot: SharedFoldReceiverSlot,
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
    /// Per-(feed, sid, segment) live state: engine + current-day bar map.
    engines: Vec<SidDayFold>,
    /// M2: PAST-day refold queue — key → the newest triggering minute +
    /// the bounded attempt count (the current day never lands here).
    dirty: BTreeMap<DirtyDayKey, DirtyMark>,
    dirty_since: Option<tokio::time::Instant>,
    client: Option<reqwest::Client>,
    exec_url: String,
}

/// Emits sealed buckets into the global seal channel; counts drops loudly.
/// Round-3 LOW: a CLOSED channel (seal-writer gone — shutdown/teardown) is
/// a DISTINCT condition from backpressure and is labeled
/// `reason="seal_channel_closed"`, never conflated with
/// `seal_channel_full` (consistent with the paced path).
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
    let mut dropped_full = 0usize;
    let mut dropped_closed = 0usize;
    for s in sealed {
        let seal = sealed_bucket_to_seal(feed, security_id, segment_code, s);
        match sender.try_send(seal) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => dropped_full += 1,
            Err(mpsc::error::TrySendError::Closed(_)) => dropped_closed += 1,
        }
    }
    let delivered = sealed.len() - dropped_full - dropped_closed;
    if delivered > 0 {
        counter!("tv_rest_candle_fold_seals_total", "feed" => feed.as_str())
            .increment(delivered as u64);
    }
    if dropped_full > 0 {
        counter!("tv_rest_candle_fold_dropped_total", "reason" => "seal_channel_full")
            .increment(dropped_full as u64);
        error!(
            code = ErrorCode::RestCandleFold01Degraded.code_str(),
            stage = "seal_send",
            reason = "seal_channel_full",
            dropped = dropped_full,
            "rest_candle_fold: seal-writer channel refused live seals — dropped; \
             the affected buckets stay underived THIS session unless a dirty \
             refold re-covers that day — otherwise only the NEXT boot's catch-up \
             re-derives them (DEDUP-idempotent)"
        );
    }
    if dropped_closed > 0 {
        counter!("tv_rest_candle_fold_dropped_total", "reason" => "seal_channel_closed")
            .increment(dropped_closed as u64);
        error!(
            code = ErrorCode::RestCandleFold01Degraded.code_str(),
            stage = "seal_send",
            reason = "seal_channel_closed",
            dropped = dropped_closed,
            "rest_candle_fold: seal-writer channel CLOSED (shutdown/teardown) — \
             live seals dropped; the NEXT boot's catch-up re-derives them \
             (DEDUP-idempotent)"
        );
    }
}

/// HIGH-1 pure pacing decision for the cold catch-up/refold emission path:
/// while the per-day wait budget has room, sleep-and-retry the SAME seal;
/// past the budget, give up (the caller counts an honest drop).
#[derive(Debug, PartialEq, Eq)]
pub enum PaceAction {
    /// Sleep `sleep_ms`, then retry the same seal.
    SleepAndRetry { sleep_ms: u64 },
    /// Per-day budget exhausted — count the drop.
    GiveUp,
}

/// Decide the next pacing step given the total milliseconds already waited
/// for the CURRENT day. Pure.
#[must_use]
pub fn catchup_pace_action(waited_ms: u64) -> PaceAction {
    if waited_ms >= FOLD_CATCHUP_PACE_BUDGET_MS {
        PaceAction::GiveUp
    } else {
        PaceAction::SleepAndRetry {
            sleep_ms: FOLD_CATCHUP_PACE_SLEEP_MS,
        }
    }
}

/// PACED seal emission for the catch-up/refold cold path (HIGH-1): a full
/// seal channel sleeps-and-retries the SAME seal under the shared per-day
/// wait budget (`waited_ms` accumulates across every emission of one
/// refolded day) instead of dropping ~thousands of seals into the void at
/// boot. A CLOSED channel (seal-writer gone — shutdown) stops immediately.
async fn emit_seals_paced(
    feed: Feed,
    security_id: SecurityId,
    segment_code: u8,
    sealed: &[SealedBucket],
    waited_ms: &mut u64,
) {
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
    let mut dropped_full = 0usize;
    let mut dropped_closed = 0usize;
    let mut delivered = 0usize;
    'seals: for s in sealed {
        let mut seal = sealed_bucket_to_seal(feed, security_id, segment_code, s);
        loop {
            match sender.try_send(seal) {
                Ok(()) => {
                    delivered += 1;
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    // Seal-writer gone (shutdown) — pacing cannot help, and
                    // EVERY remaining seal in this slice is undeliverable:
                    // count them ALL as dropped (round-2 LOW-1 — the old
                    // `dropped += 1` under-counted the abandoned tail).
                    // Round-3 LOW: labeled `seal_channel_closed`, distinct
                    // from the budget-exhausted backpressure drops.
                    dropped_closed = sealed.len() - delivered - dropped_full;
                    break 'seals;
                }
                Err(mpsc::error::TrySendError::Full(returned)) => {
                    match catchup_pace_action(*waited_ms) {
                        PaceAction::SleepAndRetry { sleep_ms } => {
                            counter!("tv_rest_candle_fold_paced_waits_total").increment(1);
                            *waited_ms = waited_ms.saturating_add(sleep_ms);
                            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                            seal = returned;
                        }
                        PaceAction::GiveUp => {
                            dropped_full += 1;
                            break;
                        }
                    }
                }
            }
        }
    }
    if delivered > 0 {
        counter!("tv_rest_candle_fold_seals_total", "feed" => feed.as_str())
            .increment(delivered as u64);
    }
    if dropped_full > 0 {
        counter!("tv_rest_candle_fold_dropped_total", "reason" => "seal_channel_full")
            .increment(dropped_full as u64);
        error!(
            code = ErrorCode::RestCandleFold01Degraded.code_str(),
            stage = "seal_send",
            reason = "seal_channel_full",
            dropped = dropped_full,
            waited_ms = *waited_ms,
            "rest_candle_fold: seal-writer channel still refused seals after the \
             per-day paced-wait budget — dropped; the affected day's candles \
             remain UNDERIVED until a later dirty refold or the next boot's \
             catch-up re-runs (nothing re-derives them automatically this pass)"
        );
    }
    if dropped_closed > 0 {
        counter!("tv_rest_candle_fold_dropped_total", "reason" => "seal_channel_closed")
            .increment(dropped_closed as u64);
        error!(
            code = ErrorCode::RestCandleFold01Degraded.code_str(),
            stage = "seal_send",
            reason = "seal_channel_closed",
            dropped = dropped_closed,
            "rest_candle_fold: seal-writer channel CLOSED during paced catch-up \
             emission (shutdown) — the remaining seals of this slice were \
             dropped; the NEXT boot's catch-up re-derives them (DEDUP-idempotent)"
        );
    }
}

/// One bounded `/exec` GET with the tf_consistency hardening (timeout,
/// redirect-none client, response-size cap). Returns the body or a stage
/// string naming the failure.
///
/// M1: the body is read as a STREAMED capped accumulation (the
/// `partition_archive::read_body_capped` precedent) — a chunked-transfer
/// response with no `Content-Length` can never force an unbounded buffer;
/// exceeding the cap fails CLOSED as a query failure.
async fn exec_query(
    client: &reqwest::Client,
    exec_url: &str,
    sql: &str,
) -> Result<String, &'static str> {
    let mut response = client
        .get(exec_url)
        .query(&[("query", sql)])
        .send()
        .await
        .map_err(|_| "catchup_query")?;
    if !response.status().is_success() {
        return Err("catchup_query");
    }
    // Fast refusal on a DECLARED oversize body before reading anything.
    if let Some(len) = response.content_length()
        && len > FOLD_MAX_RESPONSE_BYTES as u64
    {
        return Err("catchup_query");
    }
    let mut body: Vec<u8> = Vec::new(); // O(1) EXEMPT: cold-path bounded body read
    while let Some(chunk) = response.chunk().await.map_err(|_| "catchup_query")? {
        if !accumulate_capped(&mut body, &chunk, FOLD_MAX_RESPONSE_BYTES) {
            return Err("catchup_query");
        }
    }
    String::from_utf8(body).map_err(|_| "catchup_parse")
}

/// One (feed, sid, segment, day) refold target — bundled so `refold_day`
/// stays within the arg-count lint without an `#[allow]`.
struct RefoldSpec<'a> {
    feed: Feed,
    security_id: SecurityId,
    segment_code: u8,
    segment_str: &'a str,
    date: NaiveDate,
    is_today: bool,
    required_minute_nanos: Option<i64>,
}

/// The boot catch-up's TODAY product: the still-open engine plus the bars
/// it folded — the caller seeds a [`SidDayFold`] (engine + day-map) from it
/// so a mid-session restart has a COMPLETE in-RAM refold source.
struct TodayCatchup {
    engine: SidFoldState,
    bars: Vec<ConfirmedBar>,
}

/// Re-folds one (feed, sid, segment, day) from `spot_1m_rest` through a
/// FRESH engine, emitting every sealed bucket (DEDUP UPSERT heals in place).
/// Past days force-seal (and NEVER touch live engines — round-2); `is_today`
/// (boot catch-up only) keeps partials open and returns the fresh engine +
/// folded bars so the caller can seed the live day-map.
async fn refold_day(
    client: &reqwest::Client,
    exec_url: &str,
    spec: RefoldSpec<'_>,
) -> Result<Option<TodayCatchup>, &'static str> {
    let RefoldSpec {
        feed,
        security_id,
        segment_code,
        segment_str,
        date,
        is_today,
        required_minute_nanos,
    } = spec;
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
    // M2: a dirty-mark refold must SEE the triggering repair row — the
    // ILP-flush ACK can lead `/exec` visibility (QuestDB WAL apply), and
    // re-emitting the day WITHOUT the repair would REGRESS already-correct
    // candles. Stale read → NO emission; the caller re-queues bounded.
    if !rows_contain_minute(&rows, required_minute_nanos) {
        return Err("refold_stale_read");
    }
    // HIGH-1: paced emission budget shared across this whole day's seals.
    let mut waited_ms = 0u64;
    let mut engine = SidFoldState::new(feed, security_id, segment_code);
    let mut rows_folded = 0u64;
    let mut folded_bars: Vec<ConfirmedBar> = Vec::new();
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
                if is_today {
                    folded_bars.push(bar);
                }
                emit_seals_paced(feed, security_id, segment_code, &sealed, &mut waited_ms).await;
            }
            FoldOutcome::OutOfOrder | FoldOutcome::OutOfSession => {
                // QuestDB rows are ts-ordered; out-of-session rows are skipped.
            }
        }
    }
    counter!("tv_rest_candle_fold_catchup_rows_total", "feed" => feed.as_str())
        .increment(rows_folded);
    if is_today {
        Ok(Some(TodayCatchup {
            engine,
            bars: folded_bars,
        }))
    } else {
        let sealed = engine.force_seal_open();
        emit_seals_paced(feed, security_id, segment_code, &sealed, &mut waited_ms).await;
        Ok(None)
    }
}

/// HIGH-1: catch-up day OFFSETS in NEWEST→OLDEST order (offset 0 = today).
/// Per-day folds are independent and every seal is DEDUP-UPSERT-keyed, so
/// cross-day order never affects correctness — iterating newest-first means
/// any residual paced-drop hits the OLDEST days, never the newest. Pure.
///
/// Round-2 LOW-3: yields EXACTLY `catchup_days` days — today (offset 0)
/// plus `catchup_days - 1` past days — matching the config docs' "the last
/// `catchup_days` of stored bars" (the old `0..=catchup_days` inclusive
/// range silently folded one extra day).
pub fn catchup_day_offsets(catchup_days: u32) -> std::ops::Range<u32> {
    0..catchup_days
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
        let Some((pairs, truncated)) = parse_spot_discovery(&body, FOLD_DISCOVERY_ROW_LIMIT) else {
            counter!("tv_rest_candle_fold_errors_total", "stage" => "catchup_parse").increment(1);
            error!(
                code = ErrorCode::RestCandleFold01Degraded.code_str(),
                stage = "catchup_parse",
                feed = feed.as_str(),
                "rest_candle_fold: boot catch-up discovery parse failed"
            );
            continue;
        };
        if truncated {
            // M4: a LIMIT-hit instrument set is PARTIAL — never silently
            // fold a subset; skip this feed's catch-up loudly.
            counter!("tv_rest_candle_fold_errors_total", "stage" => "discovery_truncated")
                .increment(1);
            error!(
                code = ErrorCode::RestCandleFold01Degraded.code_str(),
                stage = "discovery_truncated",
                feed = feed.as_str(),
                limit = FOLD_DISCOVERY_ROW_LIMIT,
                "rest_candle_fold: boot catch-up discovery hit its LIMIT — the \
                 instrument set is partial; this feed's catch-up is SKIPPED this \
                 boot (raise the named constant in a reviewed PR, never silently)"
            );
            continue;
        }
        // LOW: identities the SecurityId space cannot carry (e.g. a negative
        // i64 read back from QuestDB) — counted + ONE coalesced warn per
        // feed, never a silent skip.
        let mut bad_identity = 0u64;
        for (sid_i64, segment) in pairs {
            let Ok(security_id) = SecurityId::try_from(sid_i64) else {
                bad_identity += 1;
                counter!("tv_rest_candle_fold_dropped_total", "reason" => "bad_identity")
                    .increment(1);
                continue;
            };
            let Some(segment_code) = tickvault_common::segment::segment_str_to_code(&segment)
            else {
                continue;
            };
            // HIGH-1: NEWEST→OLDEST (offset 0 = today) so any residual
            // paced-drop hits the oldest days; within-day stays
            // chronological (ORDER BY ts ASC).
            for offset in catchup_day_offsets(catchup_days) {
                let Some(date) = today.checked_sub_days(chrono::Days::new(u64::from(offset)))
                else {
                    continue;
                };
                let is_today = date == today;
                match refold_day(
                    &client,
                    &exec_url,
                    RefoldSpec {
                        feed: *feed,
                        security_id,
                        segment_code,
                        segment_str: &segment,
                        date,
                        is_today,
                        required_minute_nanos: None,
                    },
                )
                .await
                {
                    Ok(Some(today_catchup)) => {
                        // Today's partial state seeds the live runtime —
                        // engine AND day-map (round-2: a mid-session
                        // restart must be able to refold today losslessly
                        // from RAM).
                        let seeded = SidDayFold::from_catchup(
                            today_catchup.engine,
                            today,
                            &today_catchup.bars,
                        );
                        if let Some(pos) = runtime.engines.iter().position(|s| {
                            s.engine.feed == *feed
                                && s.engine.security_id == security_id
                                && s.engine.exchange_segment_code == segment_code
                        }) {
                            runtime.engines[pos] = seeded;
                        } else {
                            runtime.engines.push(seeded);
                        }
                    }
                    Ok(None) => {}
                    Err(stage) => {
                        counter!("tv_rest_candle_fold_errors_total", "stage" => stage).increment(1);
                        // Per-day degrade only — remaining days still fold
                        // (error! for FOLD-01 consistency with the rule
                        // file's High-severity degrade contract).
                        error!(
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
        if bad_identity > 0 {
            warn!(
                code = ErrorCode::RestCandleFold01Degraded.code_str(),
                stage = "catchup_parse",
                feed = feed.as_str(),
                bad_identity,
                "rest_candle_fold: discovery rows with unrepresentable security \
                 ids skipped (counted under reason=bad_identity)"
            );
        }
    }
    info!(
        engines = runtime.engines.len(),
        catchup_days, "rest_candle_fold: boot catch-up complete — live engines seeded"
    );
}

/// Drains the PAST-day refold queue (debounced). Round-2 HIGH: this path
/// handles STRICTLY-OLDER-than-the-engine's-day repairs only — it emits
/// corrected CLOSED-day seals and NEVER touches a live engine (the old
/// today-arm that re-read `/exec` and REPLACED the live engine — losing
/// value-updates and race-window bars to WAL-apply lag — is retired; the
/// current day refolds from the in-RAM day-map instead). M2: a failed
/// refold (query failure OR a `/exec` read that does not yet SEE the
/// triggering repair row — WAL-apply lag) RE-QUEUES the mark bounded by
/// [`FOLD_REFOLD_MAX_ATTEMPTS`] (each retry spaced by the debounce);
/// exhaustion degrades loudly and defers to the next boot's catch-up.
async fn drain_dirty(runtime: &mut FoldRuntime) {
    let Some(client) = runtime.client.clone() else {
        // Round-2 LOW-4: no HTTP client exists for this task incarnation
        // (built ONCE at task start — it can never appear later, so
        // keeping the marks would spin the debounce timer forever). Drop
        // them LOUDLY: counted + coded, never a silent clear.
        let dropped = runtime.dirty.len();
        runtime.dirty.clear();
        runtime.dirty_since = None;
        if dropped > 0 {
            counter!("tv_rest_candle_fold_dropped_total", "reason" => "no_http_client")
                .increment(dropped as u64);
            error!(
                code = ErrorCode::RestCandleFold01Degraded.code_str(),
                stage = "catchup_query",
                reason = "no_http_client",
                dropped,
                "rest_candle_fold: past-day refold marks dropped — no HTTP \
                 client this run (build failed at task start); the next \
                 boot's catch-up re-derives those days (DEDUP-idempotent)"
            );
        }
        return;
    };
    let exec_url = runtime.exec_url.clone();
    let dirty: Vec<(DirtyDayKey, DirtyMark)> =
        runtime.dirty.iter().map(|(k, v)| (*k, *v)).collect();
    runtime.dirty.clear();
    runtime.dirty_since = None;
    for ((feed_idx, security_id, segment_code, date), mark) in dirty {
        let Some(feed) = Feed::ALL.get(usize::from(feed_idx)).copied() else {
            continue;
        };
        let Some(seg_enum) = tickvault_common::types::ExchangeSegment::from_byte(segment_code)
        else {
            continue;
        };
        let segment_str = seg_enum.as_str();
        match refold_day(
            &client,
            &exec_url,
            RefoldSpec {
                feed,
                security_id,
                segment_code,
                segment_str,
                date,
                // Past-day by construction (apply_live_bar routes only
                // strictly-older-than-current-day bars here) — force-seal,
                // never return an engine.
                is_today: false,
                required_minute_nanos: Some(mark.required_minute_nanos),
            },
        )
        .await
        {
            Ok(_) => {}
            Err(stage) => {
                counter!("tv_rest_candle_fold_errors_total", "stage" => stage).increment(1);
                let attempts_done = mark.attempts.saturating_add(1);
                if should_requeue_refold(attempts_done) {
                    // Re-queue (bounded): keep the NEWEST required minute if
                    // a fresh mark landed while this drain ran.
                    let key = (feed_idx, security_id, segment_code, date);
                    let entry = runtime.dirty.entry(key).or_insert(DirtyMark {
                        required_minute_nanos: mark.required_minute_nanos,
                        attempts: attempts_done,
                    });
                    entry.required_minute_nanos =
                        entry.required_minute_nanos.max(mark.required_minute_nanos);
                    entry.attempts = entry.attempts.max(attempts_done);
                    if runtime.dirty_since.is_none() {
                        runtime.dirty_since = Some(tokio::time::Instant::now());
                    }
                    warn!(
                        code = ErrorCode::RestCandleFold01Degraded.code_str(),
                        stage,
                        feed = feed.as_str(),
                        security_id,
                        date = %date,
                        attempts = attempts_done,
                        "rest_candle_fold: dirty-day refold not applied (stale read \
                         or query failure) — re-queued for the next debounce window"
                    );
                } else {
                    error!(
                        code = ErrorCode::RestCandleFold01Degraded.code_str(),
                        stage,
                        feed = feed.as_str(),
                        security_id,
                        date = %date,
                        attempts = attempts_done,
                        "rest_candle_fold: dirty-day refold retries EXHAUSTED — the \
                         repair is NOT applied this session; the next boot's \
                         catch-up re-derives the day (DEDUP-idempotent)"
                    );
                }
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

/// The fold task main loop: boot catch-up, then live bar folding — today
/// repairs refold synchronously from the in-RAM day-map (round-2 HIGH);
/// only PAST-day repairs take the debounced `/exec` refold queue.
pub async fn run_rest_candle_fold(params: RestCandleFoldParams) {
    let RestCandleFoldParams {
        config,
        questdb,
        receiver_slot,
    } = params;

    // HIGH-2: the receiver rides the RAII re-park guard — every exit path
    // (unwind included) returns it to the shared slot so a supervisor
    // respawn actually resumes consuming. An empty slot is a guard bug and
    // is LOUD, never a silent Ok.
    let Some(mut receiver_guard) = FoldReceiverGuard::take(&receiver_slot) else {
        counter!("tv_rest_candle_fold_errors_total", "stage" => "receiver_lost").increment(1);
        error!(
            code = ErrorCode::RestCandleFold01Degraded.code_str(),
            stage = "receiver_lost",
            "rest_candle_fold: fold-bar receiver missing from the shared slot — \
             a prior incarnation leaked it (re-park guard bug); the fold task \
             CANNOT run and live bars will not derive candles until restart"
        );
        return;
    };

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
        dirty: BTreeMap::new(),
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

    // HIGH-3: dense positive liveness signal — one increment per interval
    // tick regardless of bar traffic (a dead fold task is a flatline; the
    // series is alarm-ready but metrics-local per the delivery boundary).
    let mut heartbeat = tokio::time::interval(Duration::from_secs(FOLD_HEARTBEAT_INTERVAL_SECS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let debounce_due = runtime
            .dirty_since
            .map(|since| since + Duration::from_secs(FOLD_REFOLD_DEBOUNCE_SECS));
        tokio::select! {
            maybe_bar = receiver_guard.recv() => {
                let Some(first) = maybe_bar else {
                    // Every sender dropped — the OnceLock sender keeps one
                    // alive for the process lifetime, so this is shutdown.
                    info!("rest_candle_fold: bar channel closed — exiting");
                    return;
                };
                // Round-3 (repair-burst coalescing): drain everything that
                // arrived with this bar in ONE batch — a mid-day-outage
                // sweep delivering N out-of-order repairs then costs one
                // full-day refold per dirty slot instead of N. Bounded by
                // the handoff channel capacity.
                let mut batch = vec![first]; // O(1) EXEMPT: cold-path bounded drain
                while let Some(more) = receiver_guard.try_recv() {
                    batch.push(more);
                }
                let outcome = apply_bar_batch(&mut runtime.engines, &batch, today_ist());
                for emission in &outcome.emissions {
                    if emission.is_refold {
                        // Round-2 HIGH: a current-day repair refolded the
                        // whole day from the in-RAM day-map (lossless — no
                        // /exec, no WAL race); re-emit every closed bucket
                        // (DEDUP UPSERT heals in place). Round-3: one
                        // refold per dirty slot per drained batch.
                        counter!(
                            "tv_rest_candle_fold_day_refolds_total",
                            "feed" => emission.feed.as_str()
                        )
                        .increment(1);
                    }
                    emit_seals(
                        emission.feed,
                        emission.security_id,
                        emission.segment_code,
                        &emission.sealed,
                    );
                }
                if outcome.duplicates > 0 {
                    counter!("tv_rest_candle_fold_duplicate_bars_total")
                        .increment(outcome.duplicates);
                }
                if outcome.out_of_session > 0 {
                    counter!(
                        "tv_rest_candle_fold_dropped_total",
                        "reason" => "out_of_session"
                    )
                    .increment(outcome.out_of_session);
                }
                if outcome.future_dated > 0 {
                    // Round-3 (the BOUNDARY-01 future-skew class): counted
                    // per bar, ONE coalesced coded error per drained batch
                    // that carried future-dated bars (≤1/minute at the
                    // legs' fire cadence — never per-bar spam).
                    counter!(
                        "tv_rest_candle_fold_dropped_total",
                        "reason" => "future_dated"
                    )
                    .increment(outcome.future_dated);
                    error!(
                        code = ErrorCode::RestCandleFold01Degraded.code_str(),
                        stage = "future_dated",
                        dropped = outcome.future_dated,
                        "rest_candle_fold: future-dated bar(s) dropped — a bar \
                         whose IST date is AFTER the wall-clock IST today can \
                         never roll the live day forward (the BOUNDARY-01 \
                         future-skew class); counted under reason=future_dated"
                    );
                }
                for bar in &outcome.past_day {
                    if let Some(date) = ist_date_of_nanos(bar.minute_ts_ist_nanos) {
                        // M2: keep the NEWEST triggering minute — the
                        // PAST-day refold must SEE that row before
                        // re-emitting (the only residual WAL-lag gate).
                        let key = (
                            feed_ordinal(bar.feed),
                            bar.security_id,
                            bar.exchange_segment_code,
                            date,
                        );
                        let entry = runtime.dirty.entry(key).or_insert(DirtyMark {
                            required_minute_nanos: bar.minute_ts_ist_nanos,
                            attempts: 0,
                        });
                        entry.required_minute_nanos = entry
                            .required_minute_nanos
                            .max(bar.minute_ts_ist_nanos);
                        if runtime.dirty_since.is_none() {
                            runtime.dirty_since = Some(tokio::time::Instant::now());
                        }
                        counter!("tv_rest_candle_fold_refold_queued_total")
                            .increment(1);
                    }
                }
            }
            () = async {
                match debounce_due {
                    Some(due) => tokio::time::sleep_until(due).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                drain_dirty(&mut runtime).await;
            }
            _ = heartbeat.tick() => {
                counter!("tv_rest_candle_fold_heartbeat_total").increment(1);
            }
        }
    }
}

/// Spawns the supervised fold task (house respawn pattern — DISK-WATCHER-01 /
/// spot-leg supervisor family). The bounded receiver lives in a shared slot
/// behind the RAII [`FoldReceiverGuard`] — an unwinding incarnation RE-PARKS
/// it on drop, so the respawned incarnation actually resumes consuming
/// (HIGH-2: the pre-fix `Option::take` leaked the receiver with the dead
/// incarnation and every respawn was a vacuous no-op reading as clean_exit).
///
/// Honest panic envelope: release builds run `panic = "abort"`, so the
/// respawn arms self-heal only in unwind (dev/test) builds — in release the
/// real recovery is process restart + boot catch-up (the TICK-FLUSH-01
/// precedent).
pub fn spawn_supervised_rest_candle_fold(
    config: RestCandleFoldConfig,
    questdb: QuestDbConfig,
    receiver: mpsc::Receiver<ConfirmedBar>,
) -> tokio::task::JoinHandle<()> {
    let shared_slot: SharedFoldReceiverSlot = Arc::new(Mutex::new(Some(receiver)));
    tokio::spawn(async move {
        loop {
            let handle = tokio::spawn(run_rest_candle_fold(RestCandleFoldParams {
                config: config.clone(),
                questdb: questdb.clone(),
                receiver_slot: Arc::clone(&shared_slot),
            }));
            let result = handle.await;
            let reason = tickvault_storage::disk_health_watcher::classify_join_exit(&result);
            counter!("tv_rest_candle_fold_task_respawn_total", "reason" => reason).increment(1);
            if reason == "clean_exit" {
                // Channel closed (shutdown) or the loud receiver_lost arm —
                // do not respawn.
                info!("rest_candle_fold: supervisor observed clean exit — stopping");
                return;
            }
            error!(
                code = ErrorCode::RestCandleFold01Degraded.code_str(),
                stage = "task_respawn",
                reason,
                "rest_candle_fold: fold task died — respawning after backoff \
                 (the re-park guard returned the receiver, so the respawn resumes \
                 consuming; unwind builds only — release panics abort the process)"
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
    fn test_volume_saturates_never_tears_the_fold() {
        // M3: an overflowing Σ SATURATES at i64::MAX — the fold stays
        // atomic (no torn TF state), the watermark advances, later bars
        // keep folding normally.
        let mut e = SidFoldState::new(Feed::Groww, 25, 0);
        assert!(matches!(
            e.fold_bar(&bar_at(0, 1.0, 1.0, 1.0, 1.0, i64::MAX)),
            FoldOutcome::Folded(_)
        ));
        let outcome = e.fold_bar(&bar_at(1, 1.0, 2.0, 0.5, 1.5, 1));
        let FoldOutcome::Folded(_) = outcome else {
            panic!("saturation must still FOLD (never a torn early-return)");
        };
        // Every still-open multi-minute bucket carries the saturated floor.
        let m5 = e.buckets[TfIndex::M5 as usize].expect("M5 open");
        assert_eq!(m5.volume, i64::MAX);
        assert_eq!(m5.high, 2.0);
        assert_eq!(m5.low, 0.5);
        assert_eq!(m5.close, 1.5);
        // Watermark advanced normally — the NEXT bar folds, not OutOfOrder.
        assert!(matches!(
            e.fold_bar(&bar_at(2, 1.0, 1.0, 1.0, 1.0, 1)),
            FoldOutcome::Folded(_)
        ));
    }

    #[test]
    fn test_catchup_pace_action_budget_boundaries() {
        // HIGH-1 pure pacing decision: retry inside the budget, give up at
        // and past it.
        assert_eq!(
            catchup_pace_action(0),
            PaceAction::SleepAndRetry {
                sleep_ms: FOLD_CATCHUP_PACE_SLEEP_MS
            }
        );
        assert_eq!(
            catchup_pace_action(FOLD_CATCHUP_PACE_BUDGET_MS - 1),
            PaceAction::SleepAndRetry {
                sleep_ms: FOLD_CATCHUP_PACE_SLEEP_MS
            }
        );
        assert_eq!(
            catchup_pace_action(FOLD_CATCHUP_PACE_BUDGET_MS),
            PaceAction::GiveUp
        );
        assert_eq!(
            catchup_pace_action(FOLD_CATCHUP_PACE_BUDGET_MS + 1),
            PaceAction::GiveUp
        );
    }

    #[test]
    fn test_catchup_day_offsets_newest_first() {
        // HIGH-1: offset 0 = today (newest); the LAST offset is the oldest
        // day — a residual paced-drop therefore hits the oldest days only.
        // Round-2 LOW-3: EXACTLY `catchup_days` days total (today + N-1
        // back-days) — the old inclusive range yielded one extra day.
        let mut offsets = catchup_day_offsets(3);
        assert_eq!(offsets.next(), Some(0));
        assert_eq!(catchup_day_offsets(3).last(), Some(2));
        assert_eq!(catchup_day_offsets(3).count(), 3);
        assert_eq!(catchup_day_offsets(1).collect::<Vec<_>>(), vec![0]);
        // 0 is rejected by config validate (1..=370); the pure fn is
        // honestly empty for it, never a phantom today fold.
        assert_eq!(catchup_day_offsets(0).count(), 0);
    }

    #[test]
    fn test_should_requeue_refold_bounds() {
        // M2: attempts 1..4 re-queue; the 5th failure exhausts.
        assert!(should_requeue_refold(1));
        assert!(should_requeue_refold(FOLD_REFOLD_MAX_ATTEMPTS - 1));
        assert!(!should_requeue_refold(FOLD_REFOLD_MAX_ATTEMPTS));
        assert!(!should_requeue_refold(FOLD_REFOLD_MAX_ATTEMPTS + 1));
    }

    #[test]
    fn test_rows_contain_minute_stale_read_gate() {
        let rows = [
            SpotBarRow {
                ts_nanos: 1_000,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 0,
            },
            SpotBarRow {
                ts_nanos: 2_000,
                open: 1.0,
                high: 1.0,
                low: 1.0,
                close: 1.0,
                volume: 0,
            },
        ];
        // Boot catch-up (no required minute) always passes.
        assert!(rows_contain_minute(&rows, None));
        assert!(rows_contain_minute(&rows, Some(2_000)));
        // The triggering repair row is not yet /exec-visible → stale read.
        assert!(!rows_contain_minute(&rows, Some(3_000)));
        assert!(!rows_contain_minute(&[], Some(1_000)));
    }

    #[test]
    fn test_accumulate_capped_streamed_body_cap() {
        // M1: appends within the cap; refuses (acc unchanged) past it —
        // a chunked no-Content-Length response can never balloon memory.
        let mut acc = Vec::new();
        assert!(accumulate_capped(&mut acc, b"abcd", 8));
        assert!(accumulate_capped(&mut acc, b"efgh", 8));
        assert_eq!(acc, b"abcdefgh");
        assert!(!accumulate_capped(&mut acc, b"i", 8));
        assert_eq!(acc, b"abcdefgh", "refusal must leave acc unchanged");
        let mut empty = Vec::new();
        assert!(!accumulate_capped(&mut empty, b"too big", 3));
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn test_receiver_guard_take_reparks_on_panic_and_respawn_resumes() {
        // HIGH-2: a panicking incarnation must RE-PARK the receiver during
        // unwind so the respawned incarnation actually receives bars.
        let (tx, rx) = mpsc::channel::<ConfirmedBar>(4);
        let slot: SharedFoldReceiverSlot = Arc::new(Mutex::new(Some(rx)));
        let slot_for_task = Arc::clone(&slot);
        let handle = tokio::spawn(async move {
            let mut guard = FoldReceiverGuard::take(&slot_for_task).expect("receiver present");
            let _ = guard.recv().await;
            panic!("simulated fold task death");
        });
        tx.send(bar_at(0, 1.0, 1.0, 1.0, 1.0, 1))
            .await
            .expect("send 1");
        let join = handle.await;
        assert!(join.is_err(), "the incarnation must have panicked");
        // The guard re-parked the receiver during unwind — the "respawn"
        // takes it again and RESUMES consuming.
        let mut guard = FoldReceiverGuard::take(&slot).expect("re-parked receiver");
        tx.send(bar_at(1, 2.0, 2.0, 2.0, 2.0, 1))
            .await
            .expect("send 2");
        let got = guard.recv().await.expect("respawned incarnation receives");
        assert_eq!(got.open, 2.0);
        drop(guard);
        // Clean drop re-parks too.
        assert!(
            slot.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some(),
            "guard drop must re-park the receiver"
        );
    }

    /// M7: REAL cross-implementation golden test — the fold engine and the
    /// INDEPENDENT tf_consistency recompute (`recompute_window` over
    /// `bucket_grid` windows) must agree EXACTLY on every bucket of every
    /// TF over the same synthetic full trading day.
    #[test]
    fn test_golden_fold_agrees_with_tf_consistency_recompute() {
        use crate::tf_consistency_boot::{CandleRow, bucket_grid, recompute_window};

        // Synthetic 375-minute day with varied OHLCV per minute.
        let mut bars = Vec::new();
        for i in 0u32..375 {
            let base = 100.0 + f64::from(i) * 0.5;
            bars.push(bar_at(
                i,
                base,
                base + f64::from(i % 7),
                base - f64::from(i % 5),
                base + f64::from(i % 3) - 1.0,
                i64::from(i % 11) * 3,
            ));
        }
        let mut engine = SidFoldState::new(Feed::Dhan, 13, 0);
        let sealed = fold_all(&mut engine, &bars);
        assert_eq!(
            engine.open_bucket_count(),
            0,
            "the 15:29 bar must seal every bucket"
        );

        let mut buckets_checked = 0usize;
        for tf in TfIndex::ALL {
            for window in bucket_grid(tf.seconds_per_bucket()) {
                // Independent membership: 1m bars whose minute-open
                // seconds-of-day lie in [start, end_effective).
                let members: Vec<CandleRow> = bars
                    .iter()
                    .filter(|b| {
                        let sod = u32::try_from((b.minute_ts_ist_nanos / 1_000_000_000) % 86_400)
                            .expect("sod fits");
                        (window.start_secs_of_day..window.end_effective_secs_of_day).contains(&sod)
                    })
                    .map(|b| CandleRow {
                        ts_nanos: b.minute_ts_ist_nanos,
                        open: b.open,
                        high: b.high,
                        low: b.low,
                        close: b.close,
                        volume: b.volume,
                        tick_count: 0,
                    })
                    .collect();
                let recomputed = recompute_window(&members).expect("no overflow");
                let folded = sealed.iter().find(|s| {
                    s.tf == tf
                        && s.bucket.bucket_start_ist_secs % 86_400 == window.start_secs_of_day
                });
                match (recomputed, folded) {
                    (Some(rec), Some(f)) => {
                        buckets_checked += 1;
                        assert_eq!(f.bucket.open, rec.open, "{tf:?} {window:?} open");
                        assert_eq!(f.bucket.high, rec.high, "{tf:?} {window:?} high");
                        assert_eq!(f.bucket.low, rec.low, "{tf:?} {window:?} low");
                        assert_eq!(f.bucket.close, rec.close, "{tf:?} {window:?} close");
                        assert_eq!(f.bucket.volume, rec.volume, "{tf:?} {window:?} volume");
                    }
                    (None, None) => {}
                    (rec, f) => panic!(
                        "presence must agree for {tf:?} {window:?}: recompute={rec:?} \
                         folded={f:?}"
                    ),
                }
            }
        }
        // 375 M1 + 75 M5 + ... — a full day must check hundreds of buckets;
        // guard against a vacuous pass.
        assert!(
            buckets_checked > 400,
            "golden compare must cover the full day ({buckets_checked} buckets)"
        );
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
    fn test_parse_spot_discovery_segment_allowlist_and_truncation() {
        let body = r#"{"dataset":[[13,"IDX_I"],[25,"EVIL'; DROP"],[51,"IDX_I"]]}"#;
        let (pairs, truncated) =
            parse_spot_discovery(body, FOLD_DISCOVERY_ROW_LIMIT).expect("parse");
        // The poisoned segment row is silently skipped (counted).
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], (13, "IDX_I".to_string()));
        assert_eq!(pairs[1], (51, "IDX_I".to_string()));
        assert!(!truncated);
        // M4: dataset length == LIMIT trips the truncation tripwire (the
        // caller skips that feed's catch-up loudly, never a partial fold).
        let (_, truncated_at_limit) = parse_spot_discovery(body, 3).expect("parse");
        assert!(truncated_at_limit);
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

    // -----------------------------------------------------------------
    // Round-2 HIGH: current-day day-map refold (lossless, RAM-only)
    // -----------------------------------------------------------------

    /// A bar on the day AFTER `DAY0`, `minute_offset` minutes past 09:15.
    fn next_day_bar_at(minute_offset: u32, o: f64, h: f64, l: f64, c: f64, v: i64) -> ConfirmedBar {
        ConfirmedBar {
            minute_ts_ist_nanos: i64::from(OPEN + 86_400 + minute_offset * 60) * 1_000_000_000,
            ..bar_at(0, o, h, l, c, v)
        }
    }

    /// The IST trading date of the synthetic DAY0 test day.
    fn day0_date() -> NaiveDate {
        ist_date_of_nanos(i64::from(OPEN) * 1_000_000_000).expect("day0 date")
    }

    /// The day after [`day0_date`] (the `next_day_bar_at` day).
    fn day1_date() -> NaiveDate {
        day0_date()
            .checked_add_days(chrono::Days::new(1))
            .expect("day1 date")
    }

    fn apply_all(
        state: &mut SidDayFold,
        bars: &[ConfirmedBar],
        today: NaiveDate,
    ) -> Vec<SealedBucket> {
        let mut out = Vec::new();
        for bar in bars {
            match state.apply_live_bar(bar, today) {
                LiveBarAction::Folded(sealed) | LiveBarAction::Refolded(sealed) => {
                    out.extend(sealed);
                }
                other => panic!("expected fold/refold, got {other:?}"),
            }
        }
        out
    }

    #[test]
    fn test_apply_live_bar_value_update_repair_refolds_to_recompute() {
        // The round-2 HIGH regression case the old /exec path could NOT
        // heal: a VALUE-UPDATE of an already-folded minute (presence-only
        // gating misses it entirely). The day-map refold must rebuild the
        // engine to EXACTLY the state a fresh in-order fold over the
        // UPDATED bar set produces (which the golden test proves equals
        // the tf_consistency recompute — transitively exact).
        let mut bars: Vec<ConfirmedBar> = (0..60)
            .map(|i| {
                let base = 100.0 + f64::from(i);
                bar_at(i, base, base + 1.0, base - 1.0, base + 0.5, 5)
            })
            .collect();
        let mut state = SidDayFold::new(Feed::Dhan, 13, 0);
        apply_all(&mut state, &bars, day0_date());
        assert_eq!(state.day_map_len(), 60);

        // Repair minute 10 with NEW values (same minute — a value UPDATE).
        let repair = bar_at(10, 500.0, 600.0, 50.0, 550.0, 99);
        let LiveBarAction::Refolded(refolded_seals) = state.apply_live_bar(&repair, day0_date())
        else {
            panic!("a current-day value-update must REFOLD from the day-map");
        };
        bars[10] = repair;

        // Reference: a fresh engine folding the UPDATED bar set in order.
        let mut reference = SidFoldState::new(Feed::Dhan, 13, 0);
        let reference_seals = fold_all(&mut reference, &bars);
        assert_eq!(
            refolded_seals, reference_seals,
            "the day-map refold must re-emit exactly the reference seal set"
        );
        // Open (unsealed) residue must match too — the swapped engine IS
        // the reference engine.
        assert_eq!(
            state.engine.open_bucket_count(),
            reference.open_bucket_count()
        );
        assert_eq!(state.engine.force_seal_open(), reference.force_seal_open());
        // The corrected M15 bucket [09:15, 09:30) must carry the repair.
        let m15 = refolded_seals
            .iter()
            .find(|s| s.tf == TfIndex::M15 && s.bucket.bucket_start_ist_secs == OPEN)
            .expect("first M15 bucket sealed");
        assert_eq!(m15.bucket.high, 600.0);
        assert_eq!(m15.bucket.low, 50.0);
    }

    #[test]
    fn test_apply_live_bar_late_missing_minute_refolds_losslessly() {
        // A minute that FAILED at fire time and arrives later via the
        // sweep (out-of-order, previously-absent) must slot in losslessly.
        let all_bars: Vec<ConfirmedBar> = (0..6)
            .map(|i| bar_at(i, 10.0 + f64::from(i), 11.0 + f64::from(i), 9.0, 10.5, 1))
            .collect();
        let mut with_gap = all_bars.clone();
        let late = with_gap.remove(3);

        let mut state = SidDayFold::new(Feed::Groww, 25, 0);
        apply_all(&mut state, &with_gap, day0_date());
        let LiveBarAction::Refolded(_) = state.apply_live_bar(&late, day0_date()) else {
            panic!("a late previously-missing minute must refold");
        };
        let mut reference = SidFoldState::new(Feed::Groww, 25, 0);
        fold_all(&mut reference, &all_bars);
        assert_eq!(state.day_map_len(), 6);
        assert_eq!(state.engine.force_seal_open(), reference.force_seal_open());
    }

    #[test]
    fn test_day_map_len_and_current_day_accessors() {
        let mut state = SidDayFold::new(Feed::Dhan, 13, 0);
        assert_eq!(state.day_map_len(), 0);
        assert_eq!(state.current_day(), None);
        assert!(matches!(
            state.apply_live_bar(&bar_at(0, 1.0, 1.0, 1.0, 1.0, 1), day0_date()),
            LiveBarAction::Folded(_)
        ));
        assert_eq!(state.day_map_len(), 1);
        assert!(state.current_day().is_some());
    }

    #[test]
    fn test_apply_live_bar_duplicate_bar_is_noop() {
        let mut state = SidDayFold::new(Feed::Dhan, 13, 0);
        let bar = bar_at(0, 1.0, 2.0, 0.5, 1.5, 7);
        assert!(matches!(
            state.apply_live_bar(&bar, day0_date()),
            LiveBarAction::Folded(_)
        ));
        // Value-identical redelivery (PartialEq — a NaN field compares
        // unequal and falls through to a harmless idempotent refold):
        // nothing new — no refold, no re-emit.
        assert!(matches!(
            state.apply_live_bar(&bar, day0_date()),
            LiveBarAction::DuplicateNoop
        ));
        assert_eq!(state.day_map_len(), 1);
    }

    #[test]
    fn test_apply_live_bar_day_roll_clears_day_map_and_seals_residue() {
        let mut state = SidDayFold::new(Feed::Dhan, 13, 0);
        apply_all(&mut state, &[bar_at(0, 1.0, 1.0, 1.0, 1.0, 1)], day0_date());
        let day0 = state.current_day().expect("day set");
        assert_eq!(state.day_map_len(), 1);

        // First bar of the NEXT trading day: the fold transition-seals the
        // old day's residue (incl. its D1) and the map restarts. Today has
        // reached day1, so the roll is legal (round-3 clamp).
        let LiveBarAction::Folded(sealed) =
            state.apply_live_bar(&next_day_bar_at(0, 2.0, 2.0, 2.0, 2.0, 1), day1_date())
        else {
            panic!("a new day's first bar folds in-order");
        };
        assert!(
            sealed
                .iter()
                .any(|s| s.tf == TfIndex::D1 && s.bucket.bucket_start_ist_secs == OPEN),
            "old day's D1 residue must seal on the day roll"
        );
        assert_eq!(state.day_map_len(), 1, "day-map restarts for the new day");
        let day1 = state.current_day().expect("rolled day set");
        assert_eq!(
            day1,
            day0.checked_add_days(chrono::Days::new(1)).expect("d+1")
        );
    }

    #[test]
    fn test_same_batch_day_roll_take_displaced_repair_routes_to_past_day_queue() {
        // 2026-07-16 fix round: a batch carrying [old-day repair (deferred),
        // new-day first bar (rolls the map)] must NOT silently drop the
        // repair — the displaced OLD-day repair routes to the past-day
        // /exec queue so the old day heals THIS session, not at next boot.
        let mut engines: Vec<SidDayFold> = Vec::new();
        let seed: Vec<ConfirmedBar> = (0..3)
            .map(|i| bar_at(i, 10.0, 11.0, 9.0, 10.5, 1))
            .collect();
        let seeded = apply_bar_batch(&mut engines, &seed, day0_date());
        assert!(seeded.past_day.is_empty(), "in-order seed queues nothing");

        let repair = bar_at(1, 500.0, 600.0, 50.0, 550.0, 9);
        let roll = next_day_bar_at(0, 2.0, 2.0, 2.0, 2.0, 1);
        let outcome = apply_bar_batch(&mut engines, &[repair, roll], day1_date());
        assert_eq!(
            outcome.past_day,
            vec![repair],
            "the displaced old-day repair must route to the past-day queue"
        );
        // The map + engine moved on to the NEW day (the old day heals via
        // the /exec queue), and nothing is left parked in the slot.
        assert_eq!(engines[0].current_day(), Some(day1_date()));
        assert_eq!(engines[0].day_map_len(), 1);
        assert!(engines[0].take_displaced_repair().is_none());

        // A repair-free day roll displaces nothing (regression guard for
        // the pending-repair lifetime: cleared by every refold).
        let mut fresh: Vec<SidDayFold> = Vec::new();
        apply_bar_batch(&mut fresh, &seed, day0_date());
        let rolled = apply_bar_batch(&mut fresh, &[roll], day1_date());
        assert!(rolled.past_day.is_empty(), "clean roll queues nothing");
    }

    #[test]
    fn test_apply_live_bar_past_day_routes_to_exec_queue_not_live_state() {
        // Round-2 MEDIUM (midnight clobber): "past" is relative to the
        // ENGINE's day. A repair for a STRICTLY OLDER day must not touch
        // the live engine or the day-map — the caller queues the bounded
        // /exec past-day refold instead.
        let mut state = SidDayFold::new(Feed::Dhan, 13, 0);
        apply_all(
            &mut state,
            &[next_day_bar_at(5, 2.0, 2.0, 2.0, 2.0, 1)],
            day1_date(),
        );
        assert_eq!(state.day_map_len(), 1);
        let open_before = state.engine.open_bucket_count();

        let yesterday_repair = bar_at(374, 9.0, 9.0, 9.0, 9.0, 1);
        assert!(matches!(
            state.apply_live_bar(&yesterday_repair, day1_date()),
            LiveBarAction::PastDay
        ));
        assert_eq!(
            state.day_map_len(),
            1,
            "day-map untouched by a past-day bar"
        );
        assert_eq!(state.engine.open_bucket_count(), open_before);
        // The live engine still folds the current day normally afterwards.
        assert!(matches!(
            state.apply_live_bar(&next_day_bar_at(6, 3.0, 3.0, 3.0, 3.0, 1), day1_date()),
            LiveBarAction::Folded(_)
        ));
    }

    #[test]
    fn test_apply_live_bar_after_from_catchup_seeding_refolds_losslessly() {
        // Mid-session restart: the boot catch-up seeds engine + day-map,
        // so the FIRST post-boot repair already refolds over the complete
        // day — not just post-restart bars.
        let bars: Vec<ConfirmedBar> = (0..10)
            .map(|i| bar_at(i, 50.0 + f64::from(i), 51.0, 49.0, 50.5, 2))
            .collect();
        let mut catchup_engine = SidFoldState::new(Feed::Groww, 51, 0);
        fold_all(&mut catchup_engine, &bars);
        let day = ist_date_of_nanos(bars[0].minute_ts_ist_nanos).expect("date");
        let mut state = SidDayFold::from_catchup(catchup_engine, day, &bars);
        assert_eq!(state.day_map_len(), 10);
        assert_eq!(state.current_day(), Some(day));

        // Repair a pre-restart minute — the refold must include ALL
        // seeded bars plus the repair.
        let repair = bar_at(4, 500.0, 900.0, 1.0, 450.0, 3);
        let LiveBarAction::Refolded(_) = state.apply_live_bar(&repair, day) else {
            panic!("seeded state must refold current-day repairs");
        };
        let mut updated = bars.clone();
        updated[4] = repair;
        let mut reference = SidFoldState::new(Feed::Groww, 51, 0);
        fold_all(&mut reference, &updated);
        assert_eq!(state.engine.force_seal_open(), reference.force_seal_open());
    }

    #[test]
    fn test_refold_from_day_map_matches_direct_fold() {
        let bars: Vec<ConfirmedBar> = (0..7)
            .map(|i| bar_at(i, 1.0 + f64::from(i), 2.0 + f64::from(i), 0.5, 1.5, 4))
            .collect();
        let mut day_map = BTreeMap::new();
        for bar in &bars {
            let secs = u32::try_from(bar.minute_ts_ist_nanos / 1_000_000_000).expect("secs");
            day_map.insert(secs, *bar);
        }
        let (mut engine, sealed) = refold_from_day_map(Feed::Dhan, 13, 0, &day_map);
        let mut reference = SidFoldState::new(Feed::Dhan, 13, 0);
        let reference_seals = fold_all(&mut reference, &bars);
        assert_eq!(sealed, reference_seals);
        assert_eq!(engine.force_seal_open(), reference.force_seal_open());
    }

    #[test]
    fn test_note_unfoldable_identity_counts_and_never_panics() {
        // LOW-6: counted + one coalesced warn — repeated calls must be
        // safe (the latch coalesces, the counter keeps counting).
        note_unfoldable_identity(Feed::Groww, -1);
        note_unfoldable_identity(Feed::Groww, i64::MIN);
    }

    #[test]
    fn test_parse_spot_bars_float_volume_fallback() {
        // Round-2 LOW-5: a float-serialized volume (the 2026-07-15 Dhan
        // scientific-notation drift class) must parse, not zero out.
        let body = r#"{"dataset":[[1752000000000000000,100.0,101.0,99.0,100.5,2.0E1],
                                    [1752000060000000000,100.5,102.0,100.0,101.5,10.6],
                                    [1752000120000000000,101.5,103.0,101.0,102.5,"junk"]]}"#;
        let (rows, truncated) = parse_spot_bars(body, 500).expect("parse");
        assert!(!truncated);
        assert_eq!(rows[0].volume, 20, "scientific-notation float volume");
        assert_eq!(rows[1].volume, 11, "fractional float rounds");
        assert_eq!(rows[2].volume, 0, "non-numeric degrades to 0, row kept");
    }

    // -----------------------------------------------------------------
    // Round-3: repair-burst coalescing + future-date clamp + None-day arms
    // -----------------------------------------------------------------

    #[test]
    fn test_apply_bar_batch_repair_burst_coalesces_into_one_refold_per_slot() {
        // Round-3 MEDIUM: 40 out-of-order repairs delivered as ONE drained
        // batch must trigger exactly ONE refold for the slot (one refold
        // emission — one refold's worth of re-emitted seals), with values
        // exact vs a fresh in-order fold over the updated bar set.
        let today = day0_date();
        let mut bars: Vec<ConfirmedBar> = (0..60)
            .map(|i| {
                let base = 100.0 + f64::from(i);
                bar_at(i, base, base + 1.0, base - 1.0, base + 0.5, 5)
            })
            .collect();
        let mut engines: Vec<SidDayFold> = Vec::new();
        let seed = apply_bar_batch(&mut engines, &bars, today);
        assert!(
            seed.emissions.iter().all(|e| !e.is_refold),
            "in-order seeding must not refold"
        );
        assert_eq!(engines.len(), 1);

        // 40 out-of-order repairs (minutes 0..40, NEW values), one batch.
        let repairs: Vec<ConfirmedBar> = (0..40)
            .map(|i| bar_at(i, 500.0 + f64::from(i), 600.0, 50.0, 550.0, 9))
            .collect();
        for (i, repair) in repairs.iter().enumerate() {
            bars[i] = *repair;
        }
        let outcome = apply_bar_batch(&mut engines, &repairs, today);
        let refolds: Vec<&BatchEmission> =
            outcome.emissions.iter().filter(|e| e.is_refold).collect();
        assert_eq!(
            refolds.len(),
            1,
            "40 repairs in one batch must coalesce into ONE refold"
        );
        assert!(
            outcome.emissions.iter().all(|e| e.is_refold),
            "repairs must produce no in-order emissions"
        );
        // Exactly one refold's worth of re-emitted seals — value-exact vs
        // a fresh in-order fold over the UPDATED bar set.
        let mut reference = SidFoldState::new(Feed::Dhan, 13, 0);
        let reference_seals = fold_all(&mut reference, &bars);
        assert_eq!(refolds[0].sealed, reference_seals);
        assert_eq!(
            engines[0].engine.force_seal_open(),
            reference.force_seal_open(),
            "the swapped live engine must equal the reference engine"
        );
    }

    #[test]
    fn test_apply_live_bar_deferred_and_refold_current_day() {
        // Round-3 primitives: a deferred repair updates the day-map WITHOUT
        // refolding; refold_current_day then rebuilds once, exactly.
        let today = day0_date();
        let bars: Vec<ConfirmedBar> = (0..5)
            .map(|i| bar_at(i, 10.0, 11.0, 9.0, 10.5, 1))
            .collect();
        let mut state = SidDayFold::new(Feed::Dhan, 13, 0);
        apply_all(&mut state, &bars, today);

        let repair = bar_at(1, 500.0, 600.0, 50.0, 550.0, 9);
        assert!(matches!(
            state.apply_live_bar_deferred(&repair, today),
            DeferredBarAction::RepairDeferred
        ));
        assert_eq!(state.day_map_len(), 5, "repair landed in the map only");

        let sealed = state.refold_current_day();
        let mut updated = bars.clone();
        updated[1] = repair;
        let mut reference = SidFoldState::new(Feed::Dhan, 13, 0);
        let reference_seals = fold_all(&mut reference, &updated);
        assert_eq!(sealed, reference_seals);
        assert_eq!(state.engine.force_seal_open(), reference.force_seal_open());
    }

    #[test]
    fn test_apply_live_bar_future_dated_never_rolls_day() {
        // Round-3 (BOUNDARY-01 future-skew class): a bar dated AFTER the
        // wall-clock IST today neither rolls the day nor folds; real
        // current-day bars keep working after it.
        let today = day0_date();
        let mut state = SidDayFold::new(Feed::Dhan, 13, 0);
        apply_all(&mut state, &[bar_at(0, 1.0, 1.0, 1.0, 1.0, 1)], today);
        let day_before = state.current_day();

        assert!(matches!(
            state.apply_live_bar(&next_day_bar_at(0, 9.0, 9.0, 9.0, 9.0, 1), today),
            LiveBarAction::FutureDated
        ));
        assert_eq!(state.current_day(), day_before, "day must not roll");
        assert_eq!(state.day_map_len(), 1, "day-map untouched");

        // Real bars for the current day keep folding.
        assert!(matches!(
            state.apply_live_bar(&bar_at(1, 2.0, 2.0, 2.0, 2.0, 1), today),
            LiveBarAction::Folded(_)
        ));
        // A legitimate roll works once the wall clock reaches the new day.
        assert!(matches!(
            state.apply_live_bar(&next_day_bar_at(0, 3.0, 3.0, 3.0, 3.0, 1), day1_date()),
            LiveBarAction::Folded(_)
        ));
        assert_eq!(state.current_day(), Some(day1_date()));
    }

    #[test]
    fn test_first_bar_none_day_adopts_today_only() {
        // Round-3 LOW, arm 1: a TODAY-dated first bar is adopted.
        let mut state = SidDayFold::new(Feed::Dhan, 13, 0);
        assert!(matches!(
            state.apply_live_bar(&bar_at(0, 1.0, 1.0, 1.0, 1.0, 1), day0_date()),
            LiveBarAction::Folded(_)
        ));
        assert_eq!(state.current_day(), Some(day0_date()));

        // Arm 2: a PAST-dated first bar routes to the past-day /exec queue
        // (never adopted — adopting would let a later 1-bar refold
        // force-seal + DEDUP-clobber a previously-correct closed day).
        let mut past_state = SidDayFold::new(Feed::Dhan, 13, 0);
        assert!(matches!(
            past_state.apply_live_bar(&bar_at(0, 1.0, 1.0, 1.0, 1.0, 1), day1_date()),
            LiveBarAction::PastDay
        ));
        assert_eq!(past_state.current_day(), None, "past bar must not adopt");
        assert_eq!(past_state.day_map_len(), 0);

        // Arm 3 (clamp): a FUTURE-dated first bar is dropped, never adopted.
        let mut future_state = SidDayFold::new(Feed::Dhan, 13, 0);
        let yesterday = day0_date()
            .checked_sub_days(chrono::Days::new(1))
            .expect("d-1");
        assert!(matches!(
            future_state.apply_live_bar(&bar_at(0, 1.0, 1.0, 1.0, 1.0, 1), yesterday),
            LiveBarAction::FutureDated
        ));
        assert_eq!(future_state.current_day(), None);
        assert_eq!(future_state.day_map_len(), 0);
    }

    #[tokio::test]
    async fn test_receiver_guard_try_recv_drains_burst() {
        // Round-3: the loop drains bursts via try_recv — non-blocking,
        // FIFO, None on empty.
        let (tx, rx) = mpsc::channel::<ConfirmedBar>(8);
        let slot: SharedFoldReceiverSlot = Arc::new(Mutex::new(Some(rx)));
        let mut guard = FoldReceiverGuard::take(&slot).expect("receiver present");
        assert!(guard.try_recv().is_none(), "empty channel drains to None");
        tx.send(bar_at(0, 1.0, 1.0, 1.0, 1.0, 1))
            .await
            .expect("send 1");
        tx.send(bar_at(1, 2.0, 2.0, 2.0, 2.0, 1))
            .await
            .expect("send 2");
        assert_eq!(guard.try_recv().map(|b| b.open), Some(1.0));
        assert_eq!(guard.try_recv().map(|b| b.open), Some(2.0));
        assert!(guard.try_recv().is_none());
    }
}
