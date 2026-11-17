//! The raw tick the drain stages (speed fix 6, 2026-11-17).
//!
//! Until this change the drain built a full [`TickRow`] for every tick: six
//! `f32` → `f64` price conversions (`tick_price_f64`: a multiply, an add, two
//! integer conversions, a divide and a narrowing compare each, about 40 ns
//! for the six on the x86 dev container), the segment string, and a 264-byte
//! row, then copied it into a 288-byte staged row. Nothing on the drain reads
//! a converted price: the gates, the quote route, the ledger and the
//! placement read only the timestamps and the capture sequence.
//!
//! So the drain now keeps the tick as it came off the wire, in a [`RawTick`]
//! of 88 bytes: the integer columns it must judge (`ts`, `received_at`, the
//! narrowed id, the capture sequence) are computed on the drain exactly as
//! before, and the prices stay `f32`. The thread that writes the text calls
//! [`RawTick::to_row`], which rebuilds the SAME [`TickRow`] with the same
//! functions, then the unchanged line writer: the bytes are identical in
//! both encoders (proved by the identity proptest and the PF-4 oracle).
//!
//! # What stays on the drain, at the same moment
//!
//! Every refusal and counter the drain applied before: the id-width and LTP
//! refusals (`checked_tick_identity`, shared with [`TickRow::from_parsed_tick`]),
//! the non-finite optional price count (`note_non_finite_optional_price`, in
//! the same field order), the band and window gates, the ledger and the
//! placement. A non-finite optional price is counted on the drain and stored
//! as `0.0`, which [`RawTick::to_row`] writes as NULL, exactly as the old
//! row's `None`. [`RawTick::to_row`] cannot fail and counts nothing.
//!
//! # Dropped from the drain (review, 2026-11-17)
//!
//! * The non-finite column tripwire (`tv_tick_nonfinite_columns_total`). On a
//!   drain tick it can never move: a non-finite LTP is refused by
//!   `checked_tick_identity` before a [`RawTick`] exists, and every
//!   non-finite optional price is stored as `0.0` (NULL). The old drain paid
//!   six compares per tick to add 0; the staging now keeps only a
//!   `debug_assert!` ([`RawTick::nonfinite_floats`]), and the proptest
//!   `tick_stage_raw_tests` proves it is 0 for every drain tick. The counter
//!   still counts the prepared rows the test-only `append_row` stages.
//! * The volume saturation counter. `ParsedTick::volume` is `u32`, so
//!   [`RawTick::to_row`] widens it with `i64::from` (lossless, the value the
//!   old `saturate_volume_to_i64` returned for every `u32`). A future change
//!   that widens the raw volume to `u64` fails to compile there instead of
//!   moving a counter late, on the encoding thread, and once per encode.
//!
//! # Complexity
//!
//! [`RawTick::from_parsed_tick`]: O(1), no allocation, no float conversion.
//! [`RawTick::to_row`]: O(1), the six conversions, on the encoding thread.

use tickvault_common::constants::IST_UTC_OFFSET_NANOS;
use tickvault_common::segment::segment_code_to_str;
use tickvault_common::tick_types::ParsedTick;

use super::{
    TickRow, TickRowError, checked_tick_identity, note_non_finite_optional_price,
    row_timestamp_ist_nanos, tick_price_f64,
};

/// Index of each price in [`RawTick::prices`].
const LTP: usize = 0;
const OPEN: usize = 1;
const HIGH: usize = 2;
const LOW: usize = 3;
const CLOSE: usize = 4;
const AVG: usize = 5;

/// One tick as the drain accepted it: the columns the drain must judge,
/// already computed, and the prices still `f32`. `Copy`, 88 bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RawTick {
    /// The id, already narrowed to the `LONG` column (refused if it cannot be).
    security_id: i64,
    /// The `ticks` designated timestamp (IST nanos), as the old row had it.
    ts_ist_nanos: i64,
    /// The replay-stable dedup tiebreaker.
    capture_seq: i64,
    /// Receipt time + the IST offset; meaningful only when `has_received`.
    received_at_ist_nanos: i64,
    /// ltp, open, high, low, close, avg, as received; a non-finite optional
    /// price was counted on the drain and is stored as `0.0` (NULL).
    prices: [f32; 6],
    /// volume, oi, oi_day_high, oi_day_low, total_buy_qty, total_sell_qty.
    quantities: [u32; 6],
    /// The vendor's last trade time, IST epoch seconds (`tick_quotes` keeps it).
    exchange_timestamp: u32,
    /// Last trade quantity.
    last_trade_quantity: u16,
    /// Exchange segment code (the header byte).
    segment_code: u8,
    /// Whether the tick carried a receipt time.
    has_received: bool,
}

const _: () = assert!(
    std::mem::size_of::<RawTick>() <= 88,
    "a raw tick must stay small: it is what the drain writes per tick"
);

impl RawTick {
    /// Builds the raw tick on the drain, with every refusal and counter
    /// [`TickRow::from_parsed_tick`] applies, in the same order, and none of
    /// its price conversions. O(1), no allocation.
    ///
    /// # Errors
    /// As [`TickRow::from_parsed_tick`].
    pub(crate) fn from_parsed_tick(
        tick: &ParsedTick,
        capture_seq: i64,
    ) -> Result<Self, TickRowError> {
        let security_id = checked_tick_identity(tick)?;
        // The same non-finite test, counter and order as `opt_price` in
        // `TickRow::from_parsed_tick` (open, high, low, close, then average).
        // `0.0` is the value that row maps to NULL.
        let opt_price = |v: f32| {
            if v.is_finite() {
                v
            } else {
                note_non_finite_optional_price(
                    tick.security_id,
                    segment_code_to_str(tick.exchange_segment_code),
                );
                0.0
            }
        };
        let open = opt_price(tick.day_open);
        let high = opt_price(tick.day_high);
        let low = opt_price(tick.day_low);
        let close = opt_price(tick.day_close);
        let avg = opt_price(tick.average_traded_price);
        let has_received = tick.received_at_nanos != 0;
        let received_at_ist_nanos = tick.received_at_nanos.saturating_add(IST_UTC_OFFSET_NANOS);
        Ok(Self {
            security_id,
            ts_ist_nanos: row_timestamp_ist_nanos(
                tick.exchange_timestamp,
                has_received.then_some(received_at_ist_nanos),
            ),
            capture_seq,
            received_at_ist_nanos,
            prices: [tick.last_traded_price, open, high, low, close, avg],
            quantities: [
                tick.volume,
                tick.open_interest,
                tick.oi_day_high,
                tick.oi_day_low,
                tick.total_buy_quantity,
                tick.total_sell_quantity,
            ],
            exchange_timestamp: tick.exchange_timestamp,
            last_trade_quantity: tick.last_trade_quantity,
            segment_code: tick.exchange_segment_code,
            has_received,
        })
    }

    /// The `ticks` designated timestamp (IST nanos).
    #[must_use]
    pub(crate) const fn ts_ist_nanos(&self) -> i64 {
        self.ts_ist_nanos
    }

    /// The receipt time + IST offset, `None` when the tick carried none.
    #[must_use]
    pub(crate) const fn received_at_ist_nanos(&self) -> Option<i64> {
        if self.has_received {
            Some(self.received_at_ist_nanos)
        } else {
            None
        }
    }

    /// The capture sequence.
    #[must_use]
    pub(crate) const fn capture_seq(&self) -> i64 {
        self.capture_seq
    }

    /// The non-finite float columns the converted row would hold: the
    /// tripwire `count_nonfinite_tick_floats` gives on [`Self::to_row`],
    /// judged on the `f32` (a finite `f32` converts to a finite `f64`). Always
    /// 0 for a tick built by [`Self::from_parsed_tick`]; the staging checks it
    /// with a `debug_assert!` only (not on the release drain). Six compares,
    /// O(1).
    #[must_use]
    pub(crate) fn nonfinite_floats(&self) -> u64 {
        self.prices.iter().filter(|v| !v.is_finite()).count() as u64
    }

    /// The `ticks` row this tick writes: the row [`TickRow::from_parsed_tick`]
    /// builds from the same tick, bit for bit. Runs on the encoding thread;
    /// counts nothing and cannot fail. O(1).
    #[must_use]
    pub(crate) fn to_row(self) -> TickRow {
        let price = |i: usize| self.prices.get(i).copied().unwrap_or(0.0);
        let qty = |i: usize| self.quantities.get(i).copied().unwrap_or(0);
        let opt_price = |i: usize| {
            let v = price(i);
            (v != 0.0).then(|| tick_price_f64(v))
        };
        let opt_qty = |i: usize| {
            let v = qty(i);
            (v != 0).then(|| i64::from(v))
        };
        TickRow {
            security_id: self.security_id,
            segment: segment_code_to_str(self.segment_code),
            ltp: tick_price_f64(price(LTP)),
            // `u32` → `i64` is lossless: no saturation, no counter (see the
            // module docs). Widening the raw volume breaks this line.
            volume: i64::from(qty(0)),
            ts_ist_nanos: self.ts_ist_nanos,
            capture_seq: self.capture_seq,
            open: opt_price(OPEN),
            high: opt_price(HIGH),
            low: opt_price(LOW),
            close: opt_price(CLOSE),
            oi: opt_qty(1),
            oi_day_high: opt_qty(2),
            oi_day_low: opt_qty(3),
            avg_price: opt_price(AVG),
            last_trade_qty: (self.last_trade_quantity != 0)
                .then(|| i64::from(self.last_trade_quantity)),
            total_buy_qty: opt_qty(4),
            total_sell_qty: opt_qty(5),
            received_at_ist_nanos: self.received_at_ist_nanos(),
            payload_hash: None,
        }
    }

    /// The `tick_quotes` row and its `exchange_timestamp` (plan item M5): the
    /// `ticks` row with the designated `ts` replaced by the arrival time (the
    /// `received_at` value), and the vendor's last trade time in IST nanos,
    /// `None` for a never-traded `0` (no offset: Dhan's LTT is already IST).
    /// Staged only for a tick with a receipt time. O(1).
    #[must_use]
    pub(crate) fn to_quote_row(self) -> (TickRow, Option<i64>) {
        let row = TickRow {
            ts_ist_nanos: self.received_at_ist_nanos,
            ..self.to_row()
        };
        let exchange_ts = (self.exchange_timestamp != 0)
            .then(|| i64::from(self.exchange_timestamp) * 1_000_000_000);
        (row, exchange_ts)
    }
}

#[cfg(test)]
#[path = "tick_stage_raw_tests.rs"]
mod tick_stage_raw_tests;
