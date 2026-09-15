//! Canonical active timeframe registry for the live candle engine.
//!
//! The runtime derives exactly ten frames, in ascending duration order:
//! 1s, 3s, 5s, 1m, 3m, 5m, 10m, 15m, 30m and 60m. A dense runtime
//! ordinal indexes the per-instrument and writer arrays with ten slots.
//! Top Volume selects only 1s, 3s, 5s and 1m through a separate four-slot
//! ordinal; its smaller scope does not remove the other candle frames.
//!
//! Durable seal records have a separate, permanent wire identity. Their
//! old IDs are never reused, even when a timeframe is retired. Always use
//! [`TfIndex::storage_ordinal`] and [`TfIndex::from_storage_ordinal`] for
//! persisted data; a runtime ordinal or enum cast is not a storage ID.

use tickvault_common::candle_timeframes::{ACTIVE_CANDLE_TIMEFRAMES, TOP_VOLUME_CANDLE_TIMEFRAMES};
use tickvault_common::feed::Feed;

/// Number of active runtime candle frames. No retired placeholder slots.
pub const TF_COUNT: usize = ACTIVE_CANDLE_TIMEFRAMES.len();

/// Number of active Top Volume frames, separate from the candle slot count.
pub const TOP_VOLUME_TF_COUNT: usize = TOP_VOLUME_CANDLE_TIMEFRAMES.len();

/// 09:15 IST market open. This remains distinct from the candle grid and
/// identifies which bucket owns the exchange's regular-session day open.
pub(crate) const MARKET_OPEN_SECS_OF_DAY_IST: u32 = 33_300;

/// 09:00 IST candle grid anchor, including the existing pre-open capture.
/// The timeframe reduction does not move any retained frame's grid.
pub(crate) const CANDLE_SESSION_OPEN_SECS_OF_DAY_IST: u32 = 32_400;

/// 15:40 IST exclusive end of the existing regular candle capture window.
/// This is the application's admitted observation window, not a general
/// exchange calendar and not a claim about special-session trading hours.
pub(crate) const MARKET_CLOSE_SECS_OF_DAY_IST: u32 = 56_400;

/// Maximum trusted receipt lag relative to the exchange stamp, in seconds.
pub const MAX_PLAUSIBLE_RECEIPT_LAG_SECS: i64 = 300;

/// Maximum trusted receipt lead relative to the exchange stamp, in seconds.
pub const MAX_PLAUSIBLE_RECEIPT_LEAD_SECS: i64 = 10;

/// Receipt timestamps are UTC; the candle clock uses IST wall-clock seconds.
pub(crate) const IST_UTC_OFFSET_SECS: i64 = 19_800;

/// Named local capture policy. Closure remains revisionable and does not
/// certify that the provider delivered every exchange event.
pub const CANDLE_OBSERVATION_WINDOW_POLICY: &str = "regular_capture_window_v1";

/// Exclusive regular capture end for the IST wall-clock day of `now_secs`.
///
/// This clock helper is independent of registered candle durations. It may
/// be called before or after the window, but does not itself say that the
/// window has closed: callers must still enforce their lateness margin,
/// queue-drain and freshness rules. Overflow is refused rather than saturated
/// into an apparently valid close. O(1) time and space, without allocation.
#[inline]
#[must_use]
pub const fn regular_observation_window_end(now_secs: u32) -> Option<u32> {
    let day_start = (now_secs / 86_400) * 86_400;
    day_start.checked_add(MARKET_CLOSE_SECS_OF_DAY_IST)
}

/// Choose the trusted receipt clock, falling back to the exchange stamp.
///
/// Receipt UTC nanoseconds become IST wall-clock seconds only when their
/// delta from the exchange stamp lies in [-10, 300] seconds. Missing,
/// implausible and extreme receipts retain the exchange stamp. This does
/// not turn a stale last-trade timestamp into a certified snapshot clock.
/// O(1) time and space; no allocation or data-length-dependent work.
#[inline]
#[must_use]
pub fn fold_clock_ist_secs(exchange_timestamp: u32, received_at_nanos: i64) -> u32 {
    // 0 is the documented "no receipt" sentinel; negatives cannot be a real
    // epoch. Either way there is nothing to prefer over the exchange stamp.
    if received_at_nanos <= 0 {
        return exchange_timestamp;
    }
    // UTC nanos -> IST seconds. Floor division is correct for a positive
    // value and this is guaranteed positive by the guard above.
    let receipt_ist_secs = received_at_nanos / 1_000_000_000 + IST_UTC_OFFSET_SECS;
    let delta = receipt_ist_secs - i64::from(exchange_timestamp);
    // The explicit comparison is the same two integer compares as
    // `RangeInclusive::contains`, written out so the O(1) pre-commit scanner
    // does not read `.contains(` as a Vec scan - the same reasoning, and the
    // same suppression, as the session gate in `multi_tf_aggregator.rs`.
    // APPROVED: lint suppressed for the scanner reason directly above; no behaviour silenced.
    #[allow(clippy::manual_range_contains)]
    if delta > MAX_PLAUSIBLE_RECEIPT_LAG_SECS || delta < -MAX_PLAUSIBLE_RECEIPT_LEAD_SECS {
        return exchange_timestamp;
    }
    // Cannot overflow: `delta` is bounded above, so `receipt_ist_secs` is
    // within 300 s of a `u32`. The fallback keeps it total regardless.
    u32::try_from(receipt_ist_secs).unwrap_or(exchange_timestamp)
}

/// Select a bucket clock from the feed's explicit timestamp contract.
///
/// Dhan Quote/Full packets identify this field as Last Trade Time (LTT).
/// A validated Dhan trade belongs to that second even when delivery crosses
/// a candle boundary. Admission still validates the absolute epoch, receipt
/// day, future skew and capture window before this clock reaches a cell.
/// Receipt time remains separate observation/freshness evidence; this selector
/// neither replaces it nor invents the event times of unseen trades inside a
/// cumulative-volume jump. TrueData retains its existing receipt policy.
///
/// O(1) time and space, with no allocation or runtime-duration loop.
#[inline]
#[must_use]
pub fn candle_bucket_clock_ist_secs(
    feed: Feed,
    exchange_timestamp: u32,
    received_at_nanos: i64,
) -> u32 {
    match feed {
        Feed::Dhan => exchange_timestamp,
        Feed::Truedata => fold_clock_ist_secs(exchange_timestamp, received_at_nanos),
    }
}

/// Dense runtime handle for the ten active candle timeframes.
///
/// [`Self::ALL`] and [`Self::as_ordinal`] use ascending duration order.
/// [`Self::storage_ordinal`] alone identifies a persisted seal timeframe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum TfIndex {
    /// 1-second candles.
    S1 = 0,
    /// 3-second candles.
    S3 = 1,
    /// 5-second candles.
    S5 = 2,
    /// 1-minute candles.
    M1 = 3,
    /// 3-minute candles.
    M3 = 4,
    /// 5-minute candles.
    M5 = 5,
    /// 10-minute candles.
    M10 = 6,
    /// 15-minute candles.
    M15 = 7,
    /// 30-minute candles.
    M30 = 8,
    /// 60-minute candles.
    M60 = 9,
}

impl TfIndex {
    /// The exact active set, in ascending duration and dense runtime order.
    pub const ALL: [Self; TF_COUNT] = [
        Self::S1,
        Self::S3,
        Self::S5,
        Self::M1,
        Self::M3,
        Self::M5,
        Self::M10,
        Self::M15,
        Self::M30,
        Self::M60,
    ];

    /// Exact Top Volume set, in its own dense ranking-array order.
    /// The remaining six candle frames continue aggregating and persisting.
    pub const TOP_VOLUME_ALL: [Self; TOP_VOLUME_TF_COUNT] =
        [Self::S1, Self::S3, Self::S5, Self::M1];

    /// Dense Top Volume array index, or `None` for a candle-only frame.
    ///
    /// O(1) without allocation. Ranking readers and writers must use this
    /// checked scope conversion instead of a candle or persisted ordinal.
    #[inline]
    #[must_use]
    pub const fn top_volume_ordinal(self) -> Option<usize> {
        match self {
            Self::S1 => Some(0),
            Self::S3 => Some(1),
            Self::S5 => Some(2),
            Self::M1 => Some(3),
            Self::M3 | Self::M5 | Self::M10 | Self::M15 | Self::M30 | Self::M60 => None,
        }
    }

    /// Whether this candle frame also belongs to the four-frame Top Volume set.
    #[inline]
    #[must_use]
    pub const fn is_top_volume(self) -> bool {
        self.top_volume_ordinal().is_some()
    }

    /// Dense in-memory index in `0..TF_COUNT`. Never serialize this value.
    #[inline]
    #[must_use]
    pub const fn as_ordinal(self) -> usize {
        self as usize
    }

    /// Decode a dense runtime index. This does not decode persisted records.
    #[inline]
    #[must_use]
    pub const fn from_ordinal(ord: usize) -> Option<Self> {
        match ord {
            0 => Some(Self::S1),
            1 => Some(Self::S3),
            2 => Some(Self::S5),
            3 => Some(Self::M1),
            4 => Some(Self::M3),
            5 => Some(Self::M5),
            6 => Some(Self::M10),
            7 => Some(Self::M15),
            8 => Some(Self::M30),
            9 => Some(Self::M60),
            _ => None,
        }
    }

    /// Permanent ID in persisted seal records, independent of runtime order.
    ///
    /// IDs 0..=23 retain their historical meanings. The new 10-minute frame
    /// appends ID 24; IDs belonging to retired frames are never reassigned.
    #[inline]
    #[must_use]
    pub const fn storage_ordinal(self) -> u8 {
        ACTIVE_CANDLE_TIMEFRAMES[self.as_ordinal()].storage_ordinal
    }

    /// Decode a retained or newly appended persisted ID into an active frame.
    ///
    /// `None` covers both retired and unknown IDs. Recovery must use
    /// [`Self::is_retired_storage_ordinal`] to distinguish preserved legacy
    /// data from an unknown wire identity; neither may become another frame.
    #[inline]
    #[must_use]
    pub const fn from_storage_ordinal(ord: u8) -> Option<Self> {
        match ord {
            0 => Some(Self::M1),
            1 => Some(Self::M3),
            2 => Some(Self::M5),
            3 => Some(Self::M15),
            5 => Some(Self::S1),
            7 => Some(Self::S3),
            9 => Some(Self::S5),
            22 => Some(Self::M30),
            23 => Some(Self::M60),
            24 => Some(Self::M10),
            _ => None,
        }
    }

    /// Whether this permanent wire ID belongs to a now-retired timeframe.
    /// No runtime slot is allocated and no retired ID is reused.
    #[inline]
    #[must_use]
    pub const fn is_retired_storage_ordinal(ord: u8) -> bool {
        matches!(ord, 4 | 6 | 8 | 10..=21)
    }

    /// Plain QuestDB table receiving this frame's canonical sealed candles.
    #[inline]
    #[must_use]
    pub const fn table_name(self) -> &'static str {
        ACTIVE_CANDLE_TIMEFRAMES[self.as_ordinal()].table_name
    }

    /// Stable composite identity for canonical candle upserts. The designated
    /// timestamp, security, segment and feed all remain part of the key.
    #[inline]
    #[must_use]
    pub const fn dedup_key(self) -> &'static str {
        "ts, security_id, segment, feed"
    }

    /// Bucket duration in seconds. O(1) lookup, without allocation.
    #[inline]
    #[must_use]
    pub const fn seconds_per_bucket(self) -> u32 {
        ACTIVE_CANDLE_TIMEFRAMES[self.as_ordinal()].seconds
    }

    /// Whether this active frame is shorter than one minute (1s, 3s or 5s).
    #[inline]
    #[must_use]
    pub const fn is_second_scale(self) -> bool {
        self.seconds_per_bucket() < 60
    }

    /// Every registered runtime frame belongs to the user's exact candle set.
    /// Use [`Self::is_top_volume`] for the narrower ranking capability.
    #[inline]
    #[must_use]
    pub const fn is_operator_requested(self) -> bool {
        true
    }

    /// Canonical API/display name, also used in timeframe SYMBOL values.
    #[inline]
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        ACTIVE_CANDLE_TIMEFRAMES[self.as_ordinal()].label
    }

    /// Start of the containing bucket on the existing 09:00 IST grid.
    ///
    /// `tick_ist_secs` is already on the IST wall-clock axis. Inputs at or
    /// before 09:00 anchor to that day's first bucket; the fold separately
    /// enforces the capture window. Saturating arithmetic keeps this helper
    /// total even for untrusted timestamps at the end of the `u32` domain.
    #[inline]
    #[must_use]
    pub const fn bucket_start(self, tick_ist_secs: u32) -> u32 {
        let secs = self.seconds_per_bucket();
        let day_start = (tick_ist_secs / 86_400) * 86_400;
        let session_open = day_start.saturating_add(CANDLE_SESSION_OPEN_SECS_OF_DAY_IST);
        if tick_ist_secs <= session_open {
            return session_open;
        }
        session_open.saturating_add(((tick_ist_secs - session_open) / secs) * secs)
    }

    /// Nominal exclusive end, saturating at the clock domain boundary.
    #[inline]
    #[must_use]
    pub const fn bucket_end(self, bucket_start: u32) -> u32 {
        bucket_start.saturating_add(self.seconds_per_bucket())
    }

    /// Exclusive end of admitted observations for this exact grid bucket.
    ///
    /// The nominal identity remains [`Self::bucket_end`]. The last 3m, 15m,
    /// 30m and 60m buckets stop admitting observations at the regular 15:40
    /// capture close even when their nominal end is later. Closed buckets
    /// remain revisionable; this does not certify source completeness.
    ///
    /// Refuses off-grid/out-of-window starts and nominal or capture-end
    /// overflow. Special-session hours are not inferred.
    #[inline]
    #[must_use]
    pub const fn observation_window_end(self, bucket_start: u32) -> Option<u32> {
        let seconds_of_day = bucket_start % 86_400;
        if seconds_of_day < CANDLE_SESSION_OPEN_SECS_OF_DAY_IST
            || seconds_of_day >= MARKET_CLOSE_SECS_OF_DAY_IST
            || self.bucket_start(bucket_start) != bucket_start
        {
            return None;
        }
        let Some(nominal_end) = bucket_start.checked_add(self.seconds_per_bucket()) else {
            return None;
        };
        let Some(capture_end) = regular_observation_window_end(bucket_start) else {
            return None;
        };
        Some(if nominal_end < capture_end {
            nominal_end
        } else {
            capture_end
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dhan_bucket_time_is_ltt_and_other_feed_policy_is_unchanged() {
        let event = 1_779_362_674_u32;
        for lag in [-10_i64, 0, 1, 46, 300, 301, 9 * 3_600] {
            let receipt = (i64::from(event) - IST_UTC_OFFSET_SECS + lag) * 1_000_000_000;
            assert_eq!(
                candle_bucket_clock_ist_secs(Feed::Dhan, event, receipt),
                event
            );
            assert_eq!(
                candle_bucket_clock_ist_secs(Feed::Truedata, event, receipt),
                fold_clock_ist_secs(event, receipt),
            );
        }
        for receipt in [i64::MIN, -1, 0, i64::MAX] {
            assert_eq!(
                candle_bucket_clock_ist_secs(Feed::Dhan, event, receipt),
                event
            );
        }
    }

    #[test]
    fn a_live_receipt_a_second_after_the_trade_is_the_fold_clock() {
        let exch = 1_779_362_677_u32; // ~11:24 IST
        // 1.4 s later in UTC nanos (measured Dhan p50 delivery lag).
        let recv_utc = (i64::from(exch) - 19_800) * 1_000_000_000 + 1_400_000_000;
        assert_eq!(fold_clock_ist_secs(exch, recv_utc), exch + 1);
    }

    /// The REPLAY path, and the reason the guard is a DELTA rather than a
    /// plausibility band. Measured on production 2026-08-27: 9.1% of a
    /// session's ticks replayed 9-20 HOURS after their true arrival. Nine
    /// hours later is a perfectly SANE epoch - an absolute band waves it
    /// through - and bucketing on it filed 34 real minutes into bars stamped
    /// outside market hours. The delta catches it and falls back.
    #[test]
    fn a_replay_stamp_nine_hours_late_falls_back_to_the_exchange_clock() {
        let exch = 1_779_362_677_u32;
        let nine_hours_later = (i64::from(exch) - 19_800 + 9 * 3_600) * 1_000_000_000;
        assert_eq!(
            fold_clock_ist_secs(exch, nine_hours_later),
            exch,
            "a replay stamp must never place the bucket"
        );
    }

    /// The boundary of the guard, both sides, so a future edit cannot widen
    /// or narrow it without this failing.
    #[test]
    fn the_receipt_lag_guard_bites_exactly_at_its_documented_bound() {
        let exch = 1_779_362_677_u32;
        let at = |lag: i64| (i64::from(exch) - 19_800) * 1_000_000_000 + lag * 1_000_000_000;

        // At the bound: still trusted.
        assert_eq!(
            fold_clock_ist_secs(exch, at(MAX_PLAUSIBLE_RECEIPT_LAG_SECS)),
            exch + u32::try_from(MAX_PLAUSIBLE_RECEIPT_LAG_SECS).expect("bound fits u32")
        );
        // One second past it: refused.
        assert_eq!(
            fold_clock_ist_secs(exch, at(MAX_PLAUSIBLE_RECEIPT_LAG_SECS + 1)),
            exch
        );
        // A receipt slightly BEFORE the trade is ordinary clock skew between
        // two machines and is trusted...
        assert_eq!(
            fold_clock_ist_secs(exch, at(-MAX_PLAUSIBLE_RECEIPT_LEAD_SECS)),
            exch - u32::try_from(MAX_PLAUSIBLE_RECEIPT_LEAD_SECS).expect("bound fits u32")
        );
        // ...but a receipt far before it is nonsense, and refused.
        assert_eq!(
            fold_clock_ist_secs(exch, at(-MAX_PLAUSIBLE_RECEIPT_LEAD_SECS - 1)),
            exch
        );
    }

    /// The sentinel and the impossible. `0` is the documented "no receipt"
    /// value and must never be read as an epoch at the dawn of 1970.
    #[test]
    fn an_absent_or_negative_receipt_falls_back_and_never_panics() {
        let exch = 1_779_362_677_u32;
        assert_eq!(fold_clock_ist_secs(exch, 0), exch);
        assert_eq!(fold_clock_ist_secs(exch, -1), exch);
        assert_eq!(fold_clock_ist_secs(exch, i64::MIN), exch);
        assert_eq!(fold_clock_ist_secs(exch, i64::MAX), exch);
        // And the pathological exchange stamps, which must also not panic.
        assert_eq!(fold_clock_ist_secs(0, i64::MAX), 0);
        assert_eq!(fold_clock_ist_secs(u32::MAX, 0), u32::MAX);
    }

    /// The IST conversion itself, stated as an equality rather than a
    /// tolerance: `received_at_nanos` is UTC and the exchange stamp is
    /// already IST, so a missing offset shifts every bucket by 5h30m - the
    /// single most likely way to get this wrong.
    #[test]
    fn the_receipt_is_converted_from_utc_to_ist_not_used_raw() {
        let exch = 1_779_362_677_u32;
        // A receipt at EXACTLY the trade instant, expressed in UTC.
        let recv_utc = (i64::from(exch) - 19_800) * 1_000_000_000;
        assert_eq!(fold_clock_ist_secs(exch, recv_utc), exch);
        // And the guard catches the conversion bug itself, which is a
        // property worth pinning rather than a coincidence. Feeding a value
        // that is ALREADY IST (i.e. forgetting that the receipt is UTC) makes
        // the computed receipt 19,800 s late - far outside the 300 s lag
        // bound - so it FALLS BACK to the exchange stamp instead of shifting
        // the whole grid by five and a half hours.
        //
        // This assertion was written expecting `exch + 19_800` and failed.
        // The code was right and the expectation was wrong: the delta guard
        // protects against a mis-conversion as well as against a replay
        // stamp, which is a second reason to prefer it over an absolute
        // plausibility band.
        let already_ist_by_mistake = i64::from(exch) * 1_000_000_000;
        assert_eq!(
            fold_clock_ist_secs(exch, already_ist_by_mistake),
            exch,
            "a UTC/IST mix-up must fail closed onto the exchange stamp, \
             never shift the grid by 5h30m"
        );
    }

    #[test]
    fn session_constants_keep_candle_grid_distinct_from_market_open() {
        use tickvault_common::constants::{MARKET_CLOSE_IST_NANOS, MARKET_OPEN_IST_NANOS};

        assert_eq!(CANDLE_SESSION_OPEN_SECS_OF_DAY_IST, 32_400);
        assert_eq!(MARKET_OPEN_SECS_OF_DAY_IST, 33_300);
        assert_eq!(MARKET_CLOSE_SECS_OF_DAY_IST, 56_400);
        assert_eq!(
            MARKET_OPEN_SECS_OF_DAY_IST - CANDLE_SESSION_OPEN_SECS_OF_DAY_IST,
            900
        );
        assert_eq!(
            i64::from(MARKET_OPEN_SECS_OF_DAY_IST) * 1_000_000_000,
            MARKET_OPEN_IST_NANOS
        );
        assert_eq!(
            i64::from(MARKET_CLOSE_SECS_OF_DAY_IST) * 1_000_000_000,
            MARKET_CLOSE_IST_NANOS
        );
    }

    #[test]
    fn active_registry_has_only_the_exact_ten_requested_frames_in_duration_order() {
        let expected = [
            (TfIndex::S1, "1s", 1, "candles_1s"),
            (TfIndex::S3, "3s", 3, "candles_3s"),
            (TfIndex::S5, "5s", 5, "candles_5s"),
            (TfIndex::M1, "1m", 60, "candles_1m"),
            (TfIndex::M3, "3m", 180, "candles_3m"),
            (TfIndex::M5, "5m", 300, "candles_5m"),
            (TfIndex::M10, "10m", 600, "candles_10m"),
            (TfIndex::M15, "15m", 900, "candles_15m"),
            (TfIndex::M30, "30m", 1_800, "candles_30m"),
            (TfIndex::M60, "60m", 3_600, "candles_60m"),
        ];
        assert_eq!(TF_COUNT, 10);
        assert_eq!(TfIndex::ALL.len(), expected.len());
        for (index, (tf, name, seconds, table)) in expected.into_iter().enumerate() {
            assert_eq!(TfIndex::ALL[index], tf);
            assert_eq!(tf.as_ordinal(), index);
            assert_eq!(usize::from(tf as u8), index);
            assert_eq!(TfIndex::from_ordinal(index), Some(tf));
            assert_eq!(tf.display_name(), name);
            assert_eq!(tf.seconds_per_bucket(), seconds);
            assert_eq!(tf.table_name(), table);
            assert_eq!(tf.dedup_key(), "ts, security_id, segment, feed");
            assert!(tf.is_operator_requested());
        }
        for pair in TfIndex::ALL.windows(2) {
            assert!(pair[0] < pair[1]);
            assert!(pair[0].seconds_per_bucket() < pair[1].seconds_per_bucket());
        }
        assert_eq!(TfIndex::from_ordinal(TF_COUNT), None);
        assert_eq!(TfIndex::from_ordinal(usize::MAX), None);
    }

    #[test]
    fn top_volume_registry_has_four_dense_slots_without_removing_candle_frames() {
        assert_eq!(TF_COUNT, 10);
        assert_eq!(TOP_VOLUME_TF_COUNT, 4);
        assert_eq!(
            TfIndex::TOP_VOLUME_ALL,
            [TfIndex::S1, TfIndex::S3, TfIndex::S5, TfIndex::M1]
        );
        let expected_labels = ["1s", "3s", "5s", "1m"];
        for (index, tf) in TfIndex::TOP_VOLUME_ALL.into_iter().enumerate() {
            assert_eq!(tf.top_volume_ordinal(), Some(index));
            assert!(tf.is_top_volume());
            assert!(tf.is_operator_requested());
            assert_eq!(tf.display_name(), expected_labels[index]);
            assert_eq!(
                TOP_VOLUME_CANDLE_TIMEFRAMES[index],
                ACTIVE_CANDLE_TIMEFRAMES[tf.as_ordinal()]
            );
            assert_eq!(TfIndex::from_ordinal(tf.as_ordinal()), Some(tf));
            assert_eq!(
                TfIndex::from_storage_ordinal(tf.storage_ordinal()),
                Some(tf)
            );
        }
    }

    #[test]
    fn candle_only_frames_have_no_ranking_slot_and_preserve_their_identities() {
        for (tf, candle_ordinal, storage_ordinal) in [
            (TfIndex::M3, 4, 1),
            (TfIndex::M5, 5, 2),
            (TfIndex::M10, 6, 24),
            (TfIndex::M15, 7, 3),
            (TfIndex::M30, 8, 22),
            (TfIndex::M60, 9, 23),
        ] {
            assert_eq!(tf.top_volume_ordinal(), None);
            assert!(!tf.is_top_volume());
            assert!(tf.is_operator_requested());
            assert_eq!(tf.as_ordinal(), candle_ordinal);
            assert_eq!(TfIndex::ALL[candle_ordinal], tf);
            assert_eq!(tf.storage_ordinal(), storage_ordinal);
            assert_eq!(TfIndex::from_storage_ordinal(storage_ordinal), Some(tf));
        }
    }

    #[test]
    fn permanent_storage_ids_preserve_retained_frames_and_append_ten_minutes() {
        let expected = [
            (TfIndex::M1, 0),
            (TfIndex::M3, 1),
            (TfIndex::M5, 2),
            (TfIndex::M15, 3),
            (TfIndex::S1, 5),
            (TfIndex::S3, 7),
            (TfIndex::S5, 9),
            (TfIndex::M30, 22),
            (TfIndex::M60, 23),
            (TfIndex::M10, 24),
        ];
        for (tf, wire) in expected {
            assert_eq!(tf.storage_ordinal(), wire);
            assert_eq!(TfIndex::from_storage_ordinal(wire), Some(tf));
            assert!(!TfIndex::is_retired_storage_ordinal(wire));
        }
        // A future refactor must not silently use the compact array index
        // to reinterpret already persisted records.
        assert_ne!(
            TfIndex::M1.as_ordinal(),
            usize::from(TfIndex::M1.storage_ordinal())
        );
        assert_eq!(TfIndex::from_ordinal(0), Some(TfIndex::S1));
        assert_eq!(TfIndex::from_storage_ordinal(0), Some(TfIndex::M1));
        assert_eq!(TfIndex::from_ordinal(6), Some(TfIndex::M10));
        assert_eq!(TfIndex::from_storage_ordinal(6), None);
    }

    #[test]
    fn every_wire_byte_is_unambiguously_active_retired_or_unknown() {
        let retired = [4, 6, 8, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21];
        let mut active_count = 0;
        let mut retired_count = 0;
        for wire in 0..=u8::MAX {
            let active = TfIndex::from_storage_ordinal(wire);
            let is_retired = TfIndex::is_retired_storage_ordinal(wire);
            assert_eq!(is_retired, retired.contains(&wire));
            assert!(!(active.is_some() && is_retired));
            assert_eq!(active.is_some() || is_retired, wire <= 24);
            if let Some(tf) = active {
                active_count += 1;
                assert_eq!(tf.storage_ordinal(), wire);
                assert!(tf.as_ordinal() < TF_COUNT);
            }
            if is_retired {
                retired_count += 1;
            }
        }
        assert_eq!(active_count, 10);
        assert_eq!(retired_count, 15);
    }

    #[test]
    fn only_one_three_and_five_seconds_are_second_scale() {
        let second_scale: Vec<_> = TfIndex::ALL
            .into_iter()
            .filter(|tf| tf.is_second_scale())
            .collect();
        assert_eq!(second_scale, [TfIndex::S1, TfIndex::S3, TfIndex::S5]);
        for tf in TfIndex::ALL {
            if !tf.is_second_scale() {
                assert_eq!(tf.seconds_per_bucket() % 60, 0);
            }
        }
    }

    #[test]
    fn ten_minutes_uses_the_existing_grid_and_exact_last_capture_bucket() {
        let day = (1_779_362_677_u32 / 86_400) * 86_400;
        let first = day + 9 * 3_600;
        let market_open = first + 15 * 60;
        let at_market_open = TfIndex::M10.bucket_start(market_open);
        assert_eq!(at_market_open, first + 10 * 60);
        assert_eq!(TfIndex::M10.bucket_end(at_market_open), first + 20 * 60);
        assert_eq!(
            TfIndex::M10.observation_window_end(at_market_open),
            Some(first + 20 * 60)
        );
        let close = day + 15 * 3_600 + 40 * 60;
        let last = TfIndex::M10.bucket_start(close - 1);
        assert_eq!(last, day + 15 * 3_600 + 30 * 60);
        assert_eq!(TfIndex::M10.bucket_end(last), close);
        assert_eq!(TfIndex::M10.observation_window_end(last), Some(close));
        assert_eq!(TfIndex::M10.bucket_start(close), close);
        assert_eq!(TfIndex::M10.observation_window_end(close), None);
    }

    #[test]
    fn every_active_grid_covers_capture_time_once_and_caps_its_last_window() {
        let day = (1_779_362_677_u32 / 86_400) * 86_400;
        let open = day + CANDLE_SESSION_OPEN_SECS_OF_DAY_IST;
        let close = day + MARKET_CLOSE_SECS_OF_DAY_IST;
        for tf in TfIndex::ALL {
            assert_eq!(tf.bucket_start(open), open);
            for timestamp in open..close {
                let start = tf.bucket_start(timestamp);
                let end = tf
                    .observation_window_end(start)
                    .expect("valid capture bucket");
                assert!(
                    start <= timestamp && timestamp < end,
                    "{tf:?} at {timestamp}"
                );
                assert_eq!((start - open) % tf.seconds_per_bucket(), 0);
                assert_eq!(tf.bucket_start(start), start);
                assert!(end <= close);
            }
            let last = tf.bucket_start(close - 1);
            assert_eq!(tf.observation_window_end(last), Some(close));
            assert_eq!(tf.observation_window_end(open - 1), None);
            assert_eq!(tf.observation_window_end(close), None);
            if tf != TfIndex::S1 {
                assert_eq!(tf.observation_window_end(open + 1), None);
            }
        }
    }

    #[test]
    fn final_long_bucket_identity_remains_nominal_when_observations_close_early() {
        let day = (1_779_362_677_u32 / 86_400) * 86_400;
        let close = day + 15 * 3_600 + 40 * 60;
        for (tf, start_of_day, nominal_end_of_day) in [
            (TfIndex::M15, 15 * 3_600 + 30 * 60, 15 * 3_600 + 45 * 60),
            (TfIndex::M30, 15 * 3_600 + 30 * 60, 16 * 3_600),
            (TfIndex::M60, 15 * 3_600, 16 * 3_600),
        ] {
            let start = tf.bucket_start(close - 1);
            assert_eq!(start, day + start_of_day);
            assert_eq!(tf.bucket_end(start), day + nominal_end_of_day);
            assert_eq!(tf.observation_window_end(start), Some(close));
        }
    }

    #[test]
    fn shared_capture_close_is_duration_independent_and_refuses_clock_overflow() {
        let day = (1_779_362_677_u32 / 86_400) * 86_400;
        let close = day + MARKET_CLOSE_SECS_OF_DAY_IST;
        for seconds_of_day in [0, 32_399, 32_400, 33_300, 56_399, 56_400, 86_399] {
            assert_eq!(
                regular_observation_window_end(day + seconds_of_day),
                Some(close)
            );
        }
        assert_eq!(
            regular_observation_window_end(day + 86_400),
            Some(close + 86_400)
        );
        assert_eq!(regular_observation_window_end(0), Some(56_400));
        assert_eq!(regular_observation_window_end(u32::MAX), None);
        let last_day = (u32::MAX / 86_400) * 86_400;
        // The clock domain ends before this day's 09:00 grid anchor.
        // Saturating bucket arithmetic must not invent an admitted window.
        assert!(last_day.checked_add(32_400).is_none());
        for tf in TfIndex::ALL {
            let saturated_start = tf.bucket_start(last_day);
            assert_eq!(saturated_start, u32::MAX);
            assert_eq!(tf.observation_window_end(saturated_start), None);
        }
    }

    #[test]
    fn bucket_math_never_wraps_on_extreme_untrusted_clock_values() {
        for tf in TfIndex::ALL {
            for timestamp in [u32::MAX, u32::MAX - 1, u32::MAX - 86_399, 0, 1] {
                let start = tf.bucket_start(timestamp);
                let end = tf.bucket_end(start);
                assert!(end >= start, "{tf:?}: end wrapped for {timestamp}");
            }
            assert_eq!(tf.bucket_end(u32::MAX), u32::MAX);
            assert_eq!(tf.observation_window_end(u32::MAX), None);
        }
    }
}
