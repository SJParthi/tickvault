//! `LiveCandleState` — the shared per-bucket OHLCV state struct.
//!
//! Extracted 2026-07-17 (stage-3 dead-WS sweep) from the DELETED
//! `aggregator_cell.rs` (the publisher-less 21-TF TICK aggregator died
//! with the live-feed retirements — Dhan 2026-07-13, Groww 2026-07-15).
//! The struct itself is load-bearing across the SURVIVING seal chain:
//! it is the payload of [`crate::candles::BufferedSeal`], consumed by the
//! storage seal-writer chain (`seal_writer_loop` / `ShadowCandleWriter` /
//! spill / DLQ) and PRODUCED today only by the REST-era candle fold
//! (`crates/app/src/rest_candle_fold.rs` — FOLD-01), which constructs it
//! from official `spot_1m_rest` bars. The tick-fold constructors
//! (`from_first_tick` / `fold_in_bucket` / `fold_late_hlc`) died with the
//! aggregator; construction is now literal-field (all fields `pub`).
//!
//! Field semantics are UNCHANGED from the deleted cell (the QuestDB
//! `candles_<tf>` column contract depends on them — see
//! `shadow_seal_columns.rs`).

/// Per-bucket live candle state (one open bucket of one timeframe).
///
/// The 3 Wave-5 pct fields plus `open_pct` / `open_gap_pct` stay `0.0`
/// in the REST-era runtime — the seal-time pct-stamping primitives were
/// removed with the `PrevDayCache` feeder (dead-code cleanup — BATCH-5).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveCandleState {
    /// Bucket-open IST epoch second (aligned to TF boundary).
    /// `0` means "slot never opened" — the empty/initial state.
    pub bucket_start_ist_secs: u32,
    /// Open price of this bucket.
    pub open: f64,
    /// Running high.
    pub high: f64,
    /// Running low.
    pub low: f64,
    /// Close (last folded price).
    pub close: f64,
    /// **Incremental** volume within this bucket.
    pub volume: u64,
    /// Tick-rule SIGNED volume accumulated across this bucket: buy-initiated
    /// volume minus sell-initiated volume, in the same units as
    /// [`Self::volume`].
    ///
    /// # What replaced what (2026-09-10)
    ///
    /// Until today `net_volume()` DERIVED a sign at seal time by comparing the
    /// bar's close against the previous bar's close, and signed the WHOLE
    /// bar's volume with it. That is a bar-DIRECTION proxy, not net volume: a
    /// bar that traded 900 lots on the offer and 1,000 on the bid but happened
    /// to close one tick up reported `+1,900`, when the honest answer is
    /// `-100`. The proxy is not merely imprecise — it has the WRONG SIGN
    /// whenever a bar's close disagrees with its flow, which is exactly the
    /// divergence a net-volume reader is looking for.
    ///
    /// # The rule
    ///
    /// Classic tick rule, evaluated ONCE per tick above the timeframe loop:
    /// the volume traded since the previous tick is BUY-initiated when this
    /// tick's price is above the previous tick's, SELL-initiated when below,
    /// and — the zero-tick case — carries the PREVIOUS tick's direction when
    /// the price is unchanged. Carrying rather than discarding matters:
    /// unchanged-price ticks are the majority on a liquid contract, and
    /// dropping them would under-report the bar's flow by most of its volume.
    ///
    /// # What this is NOT, stated because the vendor gives us no better
    ///
    /// This is INFERRED aggressor side, not reported aggressor side. Dhan's
    /// feed carries no buy/sell flag and no trade-by-trade tape — a Quote or
    /// Full packet is a periodic snapshot carrying a day-cumulative volume, so
    /// the "volume since the previous tick" is itself an aggregate of every
    /// trade in that interval, signed as a unit. Where several trades on both
    /// sides fall inside one snapshot interval, they are attributed together.
    /// `total_buy_qty` / `total_sell_qty` cannot help: those are RESTING order
    /// totals, and this file's fold says in as many words that nothing
    /// downstream may treat their difference as an imbalance of trades.
    ///
    /// The honest claim is therefore "tick-rule net volume", never "actual
    /// buy volume minus actual sell volume", and no surface may relabel it.
    pub net_volume_signed: i64,
    /// Cumulative-volume snapshot at bucket-open. Set ONCE per bucket
    /// open; retained for column-contract compatibility.
    pub bucket_start_cumulative: u64,
    /// Open Interest snapshot from the latest fold.
    pub oi: i64,
    /// Number of source rows/ticks folded into this bucket.
    pub tick_count: u32,
    /// IST epoch secs of the fold that set the current `close`.
    pub close_ts_ist_secs: u32,
    /// Previous-day close baseline (last non-zero value wins — a blank
    /// pre-market `0` never clobbers a real baseline). Feeds
    /// `close_pct_from_prev_day` at seal.
    pub prev_day_close: f64,
    /// `close - prev_day.close` / `prev_day.close` * 100.0. Stamped at
    /// seal time.
    pub close_pct_from_prev_day: f64,
    /// Close of the PREVIOUS sealed bar of this timeframe, snapshotted when
    /// THIS bucket opened. `0.0` means "no usable baseline" — first bar of
    /// the session, or a previous bar from an earlier trading day.
    ///
    /// Snapshotted at bucket OPEN, deliberately, not read at seal time. The
    /// late-tick amend path (`fold_late_hlc`) mutates `last_sealed[ord].close`
    /// in place and re-emits ONLY the amended bar. Reading the previous close
    /// at seal time would therefore let an amend to bar N retroactively change
    /// the sign of bar N+1 — which was already written and is never re-emitted.
    /// Snapshotting at open makes that impossible: N+1's baseline is frozen
    /// before N can be amended.
    ///
    /// Occupies the 8 bytes vacated by `oi_pct_from_prev_day`, whose DDL column
    /// was removed 2026-05-28 and which has been permanently `0.0` since (pinned
    /// by `the_dropped_volume_and_oi_percentages_are_deliberately_not_stamped`).
    pub bucket_open_prev_close: f64,
    /// Vendor's total PENDING buy-order quantity in the book, last non-zero
    /// value seen in this bucket. NOT executed volume — these are resting
    /// orders. `0` is the vendor's absent sentinel (Ticker-mode packets carry
    /// no book), so last-NON-ZERO wins, exactly as `oi` does: a lighter packet
    /// must never erase a real reading.
    pub total_buy_qty: u32,
    /// Vendor's total PENDING sell-order quantity. Same contract as
    /// [`Self::total_buy_qty`].
    ///
    /// This field and `total_buy_qty` together occupy the 8 bytes vacated by
    /// `volume_pct_from_prev_day` (same 2026-05-28 removal). The struct is
    /// therefore UNCHANGED at 128 bytes and every downstream size assertion
    /// holds without being raised.
    pub total_sell_qty: u32,
    /// Today's SESSION open (the official 09:15 open). Static per trading
    /// day; last non-zero value wins. Feeds `open_pct` at seal.
    pub session_open: f64,
    /// `(close - session_open) / session_open * 100.0` — % change vs the
    /// official 09:15 open. Stamped at seal time. `0.0` if `session_open`
    /// is `0.0` (div-by-zero guard).
    pub open_pct: f64,
    /// `(session_open - prev_day_close) / prev_day_close * 100.0` — the
    /// OPENING GAP % (gap-up positive, gap-down negative). Stamped at
    /// seal time. `0.0` if `prev_day_close` is `0.0` (div-by-zero guard).
    pub open_gap_pct: f64,
    /// `true` when [`Self::net_volume_signed`] was accumulated by the live
    /// fold; `false` when this bar was rebuilt from a source that does not
    /// carry it.
    ///
    /// # Why this exists, and why it is not a defensive nicety
    ///
    /// The 128-byte disk-spill record (`seal_spill::SEAL_SPILL_RECORD_SIZE`)
    /// is byte-for-byte FULL — every one of its 128 bytes is assigned — so a
    /// spilled bar cannot carry the accumulator without a coordinated on-disk
    /// format migration. Without this flag a replayed bar would arrive with
    /// `net_volume_signed == 0` and `volume > 0`, and `net_volume()` would
    /// report `Some(0)`: a confident "this bar's flow was perfectly balanced"
    /// for a bar nobody classified. That is a fabricated reading, and strictly
    /// worse than the `NULL` the column is designed to accept.
    ///
    /// So the flag is the honest half of a deliberately incomplete change:
    /// the LIVE path (fold → ring → writer) classifies and reports; the SPILL
    /// path (fold → ring evicted → disk → replay) reports `NULL` and says so.
    /// Carrying the accumulator through disk is a format bump plus a mixed-
    /// stride reader, and it is recorded as outstanding rather than rushed
    /// through beside a hot-path change.
    ///
    /// Costs ZERO bytes: it lands in padding the struct already had after its
    /// three trailing `u32`s (measured — `size_of` is 136 with and without).
    pub net_volume_classified: bool,
}

impl LiveCandleState {
    /// Empty/initial state — `bucket_start_ist_secs == 0` flags the
    /// "never opened" sentinel.
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            bucket_start_ist_secs: 0,
            open: 0.0,
            high: f64::NEG_INFINITY,
            low: f64::INFINITY,
            close: 0.0,
            volume: 0,
            net_volume_signed: 0,
            bucket_start_cumulative: 0,
            oi: 0,
            tick_count: 0,
            close_ts_ist_secs: 0,
            prev_day_close: 0.0,
            close_pct_from_prev_day: 0.0,
            bucket_open_prev_close: 0.0,
            total_buy_qty: 0,
            total_sell_qty: 0,
            session_open: 0.0,
            open_pct: 0.0,
            open_gap_pct: 0.0,
            net_volume_classified: false,
        }
    }

    /// Returns `true` if this slot has never been folded into (the
    /// boot/empty state). [`Self::bucket_start_ist_secs`] is the cheap
    /// check.
    #[inline]
    #[must_use]
    pub const fn is_uninitialised(&self) -> bool {
        self.bucket_start_ist_secs == 0
    }

    /// Stamps the three seal-time percentage columns from the baselines this
    /// bar has been carrying all along.
    ///
    /// **ADDED 2026-08-26, and the reason is worth more than the code.** The
    /// three fields below have existed since the Wave-5 seal-column work,
    /// their doc comments have said "Stamped at seal time" the whole time,
    /// `ShadowSealRow::from_buffered_seal` has copied them into the ILP row
    /// the whole time, and the columns have been in the candle DDL the whole
    /// time. **Nothing ever computed them.** `open_bucket` set all three to
    /// `0.0` and no other production line assigned any of them, so every
    /// candle ever written carried three zeros.
    ///
    /// Measured on the live box, 26 Aug 2026, session only: **17,409,304
    /// bars across six frames, zero of them with a non-zero `open_pct` or
    /// `open_gap_pct`.** That is the false-OK class in its purest form — a
    /// consumer reading `open_pct = 0` concludes "this instrument has not
    /// moved", not "this was never computed", and there is nothing in the
    /// row to tell the two apart.
    ///
    /// The baselines were never the problem: `prev_day_close` and
    /// `session_open` are refreshed from the exchange's own fields on every
    /// fold (last-non-zero-wins), and in the minute sampled all 189,396
    /// ticks carried both. Only the division was missing.
    ///
    /// # Which column is which (operator, 2026-08-26 — he corrected me)
    ///
    /// I first labelled these the other way round and he caught it:
    ///
    /// > "what eprcenatge change shdou l chekc with rpevd ay close right dude
    /// > … but for only for pre open 9.15 am open prcoe comapred with evry
    /// > minute or seocdn closed rpcoe"
    ///
    /// | Column | Question it answers | His name for it |
    /// |---|---|---|
    /// | `close_pct_from_prev_day` | close vs YESTERDAY'S CLOSE | **percentage change** |
    /// | `open_pct` | close vs TODAY'S 09:15 OPEN | **pre-open percentage change** |
    /// | `open_gap_pct` | 09:15 open vs yesterday's close | the overnight gap |
    ///
    /// His naming is the coherent one and mine was not. "Percentage change"
    /// on a market screen means change on the previous close — the market
    /// convention. And "pre-open" is his own name for the 09:15 open, because
    /// that price IS the pre-open call-auction equilibrium (his rule, stated
    /// three times: *"the finalised pre open 9.12 close price as 9.15 am open
    /// price"*). So "pre-open percentage" reads as "how far this bar has
    /// moved from the pre-open-determined open", which is exactly `open_pct`.
    ///
    /// The third column stays computed because it is free and it is a real
    /// question — but it is the GAP, not the pre-open percentage, and calling
    /// it that is what I got wrong.
    ///
    /// # Why zero stays the "not computable" answer
    ///
    /// A bar whose baseline never arrived stamps `0.0`, exactly as before.
    /// That is deliberate: zero is already this column's sentinel across
    /// every historical row, and inventing a different one (`NaN`, a
    /// negative flag) would break every existing reader to express something
    /// no reader currently asks. The honest signal for "no baseline" is the
    /// baseline column itself, which is also zero.
    ///
    /// # Complexity
    /// O(1) — three divisions on fields already in this struct. Zero
    /// allocation. Runs once per SEAL, never once per tick.
    #[inline]
    pub fn stamp_seal_percentages(&mut self) {
        self.close_pct_from_prev_day = pct_change(self.close, self.prev_day_close);
        self.open_pct = pct_change(self.close, self.session_open);
        self.open_gap_pct = pct_change(self.session_open, self.prev_day_close);
    }

    /// Tick-rule net volume for this bar: buy-initiated minus sell-initiated.
    ///
    /// Returns `None` — persisted as SQL NULL — when the question cannot be
    /// asked. That distinction is the whole reason this returns an `Option`:
    /// `0` here means "the bar's flow was balanced", and it must NOT also mean
    /// "no trade was classifiable". `close_pct_from_prev_day` collapses both
    /// onto `0.0` because its column has carried that sentinel since the first
    /// row ever written; this column can be honest.
    ///
    /// # What this replaced, and why the old answer could be the WRONG SIGN
    ///
    /// From its introduction to 2026-09-10 this function derived a sign at
    /// SEAL time — `close > bucket_open_prev_close` — and applied it to the
    /// whole bar's volume. It was documented honestly as "the quantity a
    /// broker chart plots as Net Volume", and it is not that: it is bar
    /// direction times bar volume. A bar that traded 1,000 lots into the bid
    /// and 900 into the offer, and closed one tick up on the last print,
    /// reported `+1,900` where the honest reading is `-100`. The failure is
    /// not a rounding difference; the sign is inverted precisely when flow and
    /// close disagree, which is the case a net-volume reader is looking at the
    /// column to find.
    ///
    /// See [`Self::net_volume_signed`] for the rule now used, and for the
    /// stated limit: this is INFERRED aggressor side, because the vendor
    /// publishes neither a trade tape nor a buy/sell flag.
    ///
    /// # The three refusals
    ///
    /// - **Not classified** (`!net_volume_classified`) — this bar was rebuilt
    ///   from a source that does not carry the accumulator, today meaning the
    ///   disk spill. Refused rather than reported as balanced; see that
    ///   field for why a `Some(0)` here would be a fabrication.
    ///
    /// - **Untraded bar** (`tick_count == 0`) — no tick was folded, so nothing
    ///   was classified. Distinct from a balanced bar.
    /// - **No classifiable volume** (`volume == 0`) — every tick in the bucket
    ///   arrived with the day-cumulative unchanged, which is what a repeated
    ///   snapshot of a quiet instrument looks like. There were prints to
    ///   count only if volume moved.
    ///
    /// A bar that traded and whose flow genuinely nets to zero returns
    /// `Some(0)`, and that is a real reading, not a refusal.
    ///
    /// # The invariant a reader may rely on
    ///
    /// `net_volume().abs() <= volume`, always. The accumulator is a sum of
    /// per-tick deltas each of which is bounded by that tick's contribution to
    /// `volume`, and the saturation below cannot widen it. A row violating it
    /// is a fold defect, not a market condition.
    ///
    /// # Complexity
    /// O(1) — one compare and one saturating convert on fields already in this
    /// struct. Zero allocation. Runs once per SEAL, never once per tick; the
    /// per-tick work is a single `i64` add in the fold.
    #[inline]
    #[must_use]
    pub fn net_volume(&self) -> Option<i64> {
        if !self.net_volume_classified || self.tick_count == 0 || self.volume == 0 {
            return None;
        }
        // The accumulator is already `i64` and each addition saturates, so the
        // magnitude cannot wrap. Clamping to the bar's own volume is belt-and
        // -braces on the stated invariant: an accumulator that somehow exceeded
        // the bar it belongs to would be a fold defect, and reporting a net
        // larger than the gross would look like a real market reading.
        let ceiling = i64::try_from(self.volume).unwrap_or(i64::MAX);
        Some(self.net_volume_signed.clamp(-ceiling, ceiling))
    }

    /// This bar's GROSS volume, signed by the bar's own direction: negative
    /// when the close fell against the previous bar's close of the SAME
    /// timeframe, positive otherwise.
    ///
    /// This is the operator's 2026-09-18 rule, and it is deliberately NOT
    /// [`Self::net_volume`]: it reports the bar's DIRECTION times the bar's
    /// volume, never its order flow. The two disagree precisely when a bar
    /// closes up on net selling, and the flow reading is the one this
    /// repository measured as honest — see [`Self::net_volume`]'s own note on
    /// what the old close-vs-close answer got wrong. The directive was put to
    /// the operator twice WITH that objection stated and ruled twice; the
    /// trade is recorded in `websocket-connection-scope-lock.md`
    /// § "2026-09-18 (FOURTH)", including what is lost.
    ///
    /// # The invariant every consumer relies on
    ///
    /// `signed_volume().unsigned_abs() == volume`, always. That equality is
    /// what makes a 10-minute bar DERIVABLE from ten 1-minute bars —
    /// `sum(abs(v))` recovers the gross and the sign is applied at the 10m
    /// level against the 10m previous close — and what lets a view render the
    /// chart-exact zero-on-flat form. Signing is therefore
    /// information-preserving; ZEROING a flat bar would not be, which is
    /// exactly why a flat bar is positive here rather than zero.
    ///
    /// # The three positives that do not mean "the close rose"
    ///
    /// - **Flat** (`close == bucket_open_prev_close`) — positive by the rule
    ///   above. A reader wanting TradingView's `0` renders it in SQL from the
    ///   stored `close`; the reverse is impossible, which is the whole reason
    ///   the stored form is this one.
    /// - **No previous close** (`bucket_open_prev_close == 0.0` — the
    ///   session's first bucket for this instrument) — positive, because
    ///   there is no previous close and so nothing fell. `0.0` is this
    ///   field's absent sentinel, not a price.
    /// - **Unorderable previous close** (NaN, infinity) — positive. NaN
    ///   compares `false` against everything, so without the explicit guard a
    ///   NaN baseline would fall through to the negative arm and sign a whole
    ///   bar on a comparison that never happened.
    ///
    /// # Complexity
    /// O(1) — one compare and one saturating convert on fields already in
    /// this struct. Zero allocation. Runs once per SEAL, never once per tick.
    #[inline]
    #[must_use]
    pub fn signed_volume(&self) -> i64 {
        // Saturating rather than wrapping: a volume above `i64::MAX` is a
        // fold defect, and `i64::MAX` reads as "impossibly large" where a
        // wrapped negative would read as a real sell bar.
        let gross = i64::try_from(self.volume).unwrap_or(i64::MAX);
        let prev = self.bucket_open_prev_close;
        if !prev.is_finite() || prev <= 0.0 {
            return gross;
        }
        // `-gross` cannot overflow: the most negative reachable value is
        // `-i64::MAX`, which is `i64::MIN + 1`.
        if self.close < prev { -gross } else { gross }
    }

    /// This bar's own price change against the PREVIOUS sealed bar of the same
    /// timeframe, as a percentage — or `None` when there is no usable baseline.
    ///
    /// # Why this exists (operator, 2026-09-18)
    ///
    /// *"why i cant see any normal percnetage change and volume percentage
    /// change columns sepaartely dude espeiclaly in top volume table"*.
    ///
    /// `top_volume` carried two percentage columns and NEITHER was the
    /// contract's own price move: `net_volume_chg_milli_pct` is a VOLUME
    /// figure (lots traded in the window), and `gain_pct` is the UNDERLYING
    /// stock's move. Nothing anywhere stored "what did this option contract's
    /// price do", because the leaderboard's `RankedContract` carries no price
    /// at all — the fold does.
    ///
    /// # Why the PREVIOUS BAR and not the previous DAY
    ///
    /// [`Self::close_pct_from_prev_day`] is stamped at SEAL
    /// ([`Self::stamp_seal_percentages`]), so on the OPEN bucket this method is
    /// read from it is still `0.0` — reading it would store a fabricated zero
    /// that is indistinguishable from a genuinely flat bar.
    /// `bucket_open_prev_close` is snapshotted when the bucket OPENS and is
    /// therefore live from the bar's first tick.
    ///
    /// It is also the RIGHT baseline for the row it lands beside: it is the
    /// exact comparison [`Self::signed_volume`] derives its sign from. A reader
    /// seeing a negative volume can see, in the next column, the price fall
    /// that made it negative — rather than having to know the rule.
    ///
    /// # The refusal
    ///
    /// `None` when `bucket_open_prev_close` is not finite or not strictly
    /// positive — `0.0` is the documented "no usable baseline" sentinel (the
    /// session's first bar of this timeframe, or a slot whose previous bar was
    /// never sealed), and a negative baseline would invert the sign so a fall
    /// persisted as a rise. This is the SAME gate `signed_volume` applies, so
    /// the two columns can never disagree about whether a baseline existed:
    /// where this is `None`, the volume is unsigned-positive by that rule.
    ///
    /// Rounded to 2 decimals by `pct_change`, the shared house helper — so
    /// this percentage rounds by exactly the rule every other percentage in
    /// the fold rounds by, and matches the vendor's published precision.
    ///
    /// # Complexity
    /// O(1) — two compares and one division, no allocation.
    #[must_use]
    pub fn close_chg_pct_from_prev_bar(&self) -> Option<f64> {
        let baseline = self.bucket_open_prev_close;
        if !baseline.is_finite() || baseline <= 0.0 {
            return None;
        }
        let pct = pct_change(self.close, baseline);
        pct.is_finite().then_some(pct)
    }
}

/// `(value - baseline) / baseline * 100`, or `0.0` when that is not a
/// meaningful question to ask.
///
/// Four refusals, each for a reason that has bitten this repository before:
///
/// - **Baseline not finite** — `NaN`/`inf` propagate silently through a
///   division and land in a persisted column looking like a real percentage.
/// - **Baseline not strictly positive** — zero is the documented "no
///   baseline yet" sentinel, and a NEGATIVE baseline would flip the sign of
///   the result, so a fall would persist as a rise.
/// - **Value not finite** — same propagation hazard from the other operand.
/// - **Quotient not finite** — reachable from a subnormal baseline that
///   passes the positivity test and still overflows the division.
///
/// Every refusal returns the SAME value the column held before this function
/// existed, so a refusal can never be worse than the status quo.
#[inline]
#[must_use]
fn pct_change(value: f64, baseline: f64) -> f64 {
    // ROUNDED TO 2 DECIMALS -- operator directive 2026-09-03: "always our
    // percentage should be purely based only two digits after decimal points
    // which should be matched to Dhan".
    //
    // This is the vendor-matching rule, not cosmetics. Dhan publishes
    // percentage change to 2 decimals, and the daily cross-verification
    // compares our aggregation against their tape; a column carrying
    // 0.38198662315043225 where the vendor says 0.38 cannot be compared for
    // equality at all, only with a tolerance nobody has calibrated.
    //
    // Rounded at the SOURCE rather than at each reader: these values are
    // persisted, spilled to disk, and read back by the console, the API and
    // the comparator, and a rounding applied at one reader and not another is
    // how two surfaces come to disagree about the same bar. The precision
    // discarded here is below the tick size of every instrument we carry.
    //
    // `round_to_2dp` is the house helper (`price_precision.rs`) already used
    // for prices; reusing it means percentages and prices can never round by
    // two different rules.
    tickvault_common::price_precision::round_to_2dp(pct_change_raw(value, baseline))
}

/// The unrounded quotient, split out ONLY so a fixture can still pin the
/// arithmetic itself.
///
/// This is not a second code path: `pct_change` is exactly
/// `round_to_2dp(pct_change_raw(..))`, and nothing else calls this. It exists
/// because the 2-decimal rounding costs the live-NIFTY fixture its
/// discriminating power -- rounded, every drift smaller than 0.005 percentage
/// points is invisible to an assertion on the stamped column, so a real
/// arithmetic regression could land green. Keeping the raw value observable
/// lets that fixture assert BOTH: the vendor-matched 2-decimal value a reader
/// sees, and the full-precision quotient that proves the formula did not move.
#[inline]
#[must_use]
fn pct_change_raw(value: f64, baseline: f64) -> f64 {
    if !baseline.is_finite() || baseline <= 0.0 || !value.is_finite() {
        return 0.0;
    }
    let pct = (value - baseline) / baseline * 100.0;
    if !pct.is_finite() {
        return 0.0;
    }
    pct
}

#[cfg(test)]
mod tests {
    use super::{LiveCandleState, pct_change, pct_change_raw};

    /// The four percentage columns must carry AT MOST two decimals, because
    /// Dhan publishes two and the daily cross-verification compares our
    /// aggregation against their tape. Operator directive 2026-09-03.
    ///
    /// MEASURED on the live console the same day, before this rounding:
    /// `close_pct_from_prev_day` read 0.38198662315043225 and `open_gap_pct`
    /// read 0.3491612811500996 -- seventeen digits against the vendor's two.
    #[test]
    fn every_percentage_carries_at_most_two_decimals() {
        // The exact live values that prompted the directive.
        for (value, baseline) in [
            (24_005.8_f64, 23_914.4_f64),
            (24_009.9_f64, 23_914.4_f64),
            (100.0_f64, 3.0_f64),
            (1.0_f64, 3.0_f64),
            (0.1_f64, 99_999.0_f64),
        ] {
            let pct = pct_change(value, baseline);
            let rendered = format!("{pct}");
            if let Some(fraction) = rendered.split_once('.').map(|(_, f)| f) {
                assert!(
                    fraction.len() <= 2,
                    "pct_change({value}, {baseline}) rendered {rendered:?} with \
                     {} decimals -- Dhan publishes 2, and a column the vendor \
                     cannot be compared against for equality is a column the \
                     cross-verification has to guess a tolerance for",
                    fraction.len()
                );
            }
        }
    }

    /// Rounding must not turn a real move into a zero, and must not invent one.
    /// A percentage that rounds to 0.00 is REPORTED as 0.00 -- that is the
    /// vendor's own resolution, not a suppressed signal -- but the sign and
    /// magnitude of anything at or above 0.005% must survive intact.
    #[test]
    fn rounding_preserves_sign_and_does_not_fabricate_movement() {
        assert_eq!(pct_change(101.0, 100.0), 1.0);
        assert_eq!(pct_change(99.0, 100.0), -1.0);
        // 0.004% rounds to zero: below the vendor's own resolution.
        assert_eq!(pct_change(100.004, 100.0), 0.0);
        // 0.006% survives as 0.01, with its sign.
        assert_eq!(pct_change(100.006, 100.0), 0.01);
        assert_eq!(pct_change(99.994, 100.0), -0.01);
        // The guards above the arithmetic are unchanged.
        assert_eq!(pct_change(100.0, 0.0), 0.0);
        assert_eq!(pct_change(f64::NAN, 100.0), 0.0);
        assert_eq!(pct_change(100.0, f64::NAN), 0.0);
    }

    /// Builds a sealed-shaped state carrying real baselines.
    fn sealed(close: f64, session_open: f64, prev_day_close: f64) -> LiveCandleState {
        LiveCandleState {
            bucket_start_ist_secs: 33_300,
            close,
            session_open,
            prev_day_close,
            ..LiveCandleState::empty()
        }
    }

    #[test]
    fn test_empty_is_uninitialised_sentinel() {
        let s = LiveCandleState::empty();
        assert!(s.is_uninitialised());
        assert_eq!(s.bucket_start_ist_secs, 0);
        assert_eq!(s.tick_count, 0);
        assert_eq!(s.volume, 0);
        // Extreme sentinels so the first fold's min/max always win.
        assert!(s.high.is_infinite() && s.high < 0.0);
        assert!(s.low.is_infinite() && s.low > 0.0);
    }

    #[test]
    fn test_opened_state_is_not_uninitialised() {
        let s = LiveCandleState {
            bucket_start_ist_secs: 33_300, // 09:15:00 IST secs-of-day-shaped value
            ..LiveCandleState::empty()
        };
        assert!(!s.is_uninitialised());
    }
    /// The live NIFTY numbers from 2026-08-26, used as the fixture precisely
    /// so this test fails if the arithmetic ever drifts from what the
    /// operator was shown.
    ///
    /// Read from production: yesterday's close 24,334.55; today's official
    /// 09:15 open 24,341.95; price at 15:19 IST 24,273.15.
    ///
    /// # Why this asserts TWICE per column (2026-09-03)
    ///
    /// The 2-decimal rounding the operator ordered would otherwise have
    /// GUTTED this fixture. Its whole purpose is to fail on an arithmetic
    /// drift, and against a rounded column every drift smaller than 0.005
    /// percentage points is invisible: -0.282 63 and -0.284 99 both stamp
    /// -0.28. Loosening the literals to the rounded values and calling it
    /// updated would have left a test that still passes and no longer
    /// guards anything -- the false-OK class this repository keeps
    /// recording.
    ///
    /// So each column is pinned on both sides: the RAW quotient at the
    /// original tolerance, which is the drift detector, and the STAMPED
    /// value at exact equality, which is what a reader and the vendor
    /// comparison actually see. A formula change fails the first; a change
    /// to the rounding rule fails the second.
    #[test]
    fn the_live_nifty_numbers_produce_the_percentages_the_operator_was_shown() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.stamp_seal_percentages();

        // PRE-OPEN percentage change: down 0.283% from the 09:15 open.
        assert!(
            (pct_change_raw(24_273.15, 24_341.95) - -0.282_63).abs() < 0.000_5,
            "pre-open pct drifted: {}",
            pct_change_raw(24_273.15, 24_341.95)
        );
        assert_eq!(s.open_pct, -0.28, "pre-open pct was {}", s.open_pct);

        // The overnight GAP: the 09:15 open was 0.030% above yesterday's
        // close. This is NOT what he calls the pre-open percentage.
        assert!(
            (pct_change_raw(24_341.95, 24_334.55) - 0.030_41).abs() < 0.000_5,
            "gap pct drifted: {}",
            pct_change_raw(24_341.95, 24_334.55)
        );
        assert_eq!(s.open_gap_pct, 0.03, "gap pct was {}", s.open_gap_pct);

        // PERCENTAGE CHANGE: versus yesterday's close. The headline
        // number, and the market convention.
        assert!(
            (pct_change_raw(24_273.15, 24_334.55) - -0.252_52).abs() < 0.000_5,
            "percentage change drifted: {}",
            pct_change_raw(24_273.15, 24_334.55)
        );
        assert_eq!(
            s.close_pct_from_prev_day, -0.25,
            "percentage change was {}",
            s.close_pct_from_prev_day
        );
    }

    /// The rounded wrapper and the raw helper must never be two different
    /// formulas -- the split exists only so the fixture above can see the
    /// unrounded value, and a second code path is exactly what would make it
    /// lie. Swept across the refusal boundaries as well as ordinary values.
    #[test]
    fn the_rounded_column_is_always_the_rounded_raw_value() {
        for (value, baseline) in [
            (24_273.15_f64, 24_341.95_f64),
            (24_341.95, 24_334.55),
            (94.07, 102.26),
            (100.0, 100.0),
            (0.0, 100.0),
            (100.0, 0.0),               // refused: baseline is the sentinel
            (100.0, -1.0),              // refused: negative baseline flips the sign
            (f64::NAN, 100.0),          // refused: non-finite value
            (100.0, f64::NAN),          // refused: non-finite baseline
            (100.0, f64::MIN_POSITIVE), // refused: quotient overflows
        ] {
            assert_eq!(
                pct_change(value, baseline),
                tickvault_common::price_precision::round_to_2dp(pct_change_raw(value, baseline)),
                "the wrapper drifted from the raw helper at ({value}, {baseline})"
            );
        }
    }

    /// The three columns are three DIFFERENT questions, and an instrument can
    /// be strong on one and weak on another. Varun Beverages did exactly this
    /// on 2026-08-26: it gapped up 2.26% overnight and then fell 5.93% from
    /// that open — positive gap, negative pre-open percentage, on the same
    /// day. That is the whole reason both columns are worth carrying.
    #[test]
    fn a_gap_up_that_then_falls_reports_opposite_signs_on_the_two_columns() {
        let mut s = sealed(94.07, 102.26, 100.0);
        s.stamp_seal_percentages();
        assert!(
            s.open_gap_pct > 2.0,
            "gap should be positive: {}",
            s.open_gap_pct
        );
        assert!(
            s.open_pct < -5.0,
            "intraday should be negative: {}",
            s.open_pct
        );
    }

    #[test]
    fn an_unmoved_price_stamps_exactly_zero_not_a_rounding_artefact() {
        let mut s = sealed(24_341.95, 24_341.95, 24_341.95);
        s.stamp_seal_percentages();
        assert_eq!(s.open_pct, 0.0);
        assert_eq!(s.open_gap_pct, 0.0);
        assert_eq!(s.close_pct_from_prev_day, 0.0);
    }

    /// Pre-open, and for indices for the first several ticks of every
    /// morning, the exchange sends no baseline at all. Zero in, zero out —
    /// the same value the column held before this code existed, so a
    /// refusal can never be worse than the status quo.
    #[test]
    fn a_missing_baseline_stamps_zero_exactly_as_before() {
        let mut s = sealed(24_273.15, 0.0, 0.0);
        s.stamp_seal_percentages();
        assert_eq!(s.open_pct, 0.0);
        assert_eq!(s.open_gap_pct, 0.0);
        assert_eq!(s.close_pct_from_prev_day, 0.0);
    }

    /// A NEGATIVE baseline is the dangerous one: the division still yields a
    /// finite number, but with the sign flipped, so a fall would persist as
    /// a rise. Refused rather than trusted.
    #[test]
    fn a_negative_baseline_is_refused_not_sign_flipped() {
        assert_eq!(pct_change(110.0, -100.0), 0.0);
    }

    #[test]
    fn non_finite_operands_never_reach_a_persisted_column() {
        assert_eq!(pct_change(f64::NAN, 100.0), 0.0);
        assert_eq!(pct_change(f64::INFINITY, 100.0), 0.0);
        assert_eq!(pct_change(f64::NEG_INFINITY, 100.0), 0.0);
        assert_eq!(pct_change(100.0, f64::NAN), 0.0);
        assert_eq!(pct_change(100.0, f64::INFINITY), 0.0);
    }

    /// A subnormal baseline passes `> 0.0` and still overflows the division.
    /// The finite check on the QUOTIENT is what catches it — the operand
    /// checks alone are not enough.
    #[test]
    fn a_subnormal_baseline_that_overflows_the_division_is_refused() {
        let out = pct_change(1.0, f64::MIN_POSITIVE / 2.0);
        assert!(
            out == 0.0 || out.is_finite(),
            "a subnormal baseline produced {out}"
        );
        assert_eq!(pct_change(f64::MAX, 5e-324), 0.0);
    }

    #[test]
    fn a_price_of_zero_against_a_real_baseline_reports_minus_one_hundred() {
        // An option going to zero is an ordinary expiry-day outcome, not an
        // error, and -100% is the correct answer for it.
        let mut s = sealed(0.0, 5.6, 5.6);
        s.stamp_seal_percentages();
        assert!((s.open_pct - -100.0).abs() < f64::EPSILON);
    }

    /// Stamping twice must not compound — the amended-late path re-stamps an
    /// already-stamped bar every time a late tick moves the close.
    #[test]
    fn stamping_twice_is_idempotent_for_an_unchanged_close() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.stamp_seal_percentages();
        let first = (s.open_pct, s.open_gap_pct, s.close_pct_from_prev_day);
        s.stamp_seal_percentages();
        assert_eq!(
            first,
            (s.open_pct, s.open_gap_pct, s.close_pct_from_prev_day)
        );
    }

    /// The two dropped percentage fields (`oi_pct_from_prev_day`,
    /// `volume_pct_from_prev_day`) are GONE, not merely unstamped.
    ///
    /// Their DDL columns were removed 2026-05-28 (spot has no OI, indices have
    /// no volume) and the fields then sat in every bar holding a permanent
    /// `0.0` — 16 bytes per state, multiplied by `TF_COUNT` slots and again by
    /// `last_sealed`, in a struct pinned at exactly 128 bytes by three separate
    /// compile-time assertions with zero slack between them.
    ///
    /// Reclaiming those 16 bytes is what pays for `bucket_open_prev_close`
    /// (8) + `total_buy_qty` (4) + `total_sell_qty` (4). This test is the
    /// replacement for the old "they are never stamped" pin: it asserts the
    /// struct did not grow, which is the property the assertions downstream
    /// actually depend on.
    #[test]
    fn the_state_is_136_bytes_and_every_size_assert_knows_it() {
        assert_eq!(
            std::mem::size_of::<LiveCandleState>(),
            136,
            "LiveCandleState changed size — BufferedSeal (<=152), AggregatorCell \
             (MAX_AGGREGATOR_CELL_BYTES) and SerializedSeal (SEAL_SPILL_RECORD_SIZE) \
             all assume this figure and every one of them is at zero slack today. \
             128 -> 136 on 2026-09-10 for `net_volume_signed`; the cost is recorded \
             in aws-budget.md."
        );
    }

    /// The classified MARKER is free — it must land in existing padding.
    ///
    /// If it ever stops being free, the two RAM budgets move again and the
    /// arithmetic recorded beside them goes stale. Asserting the size WITHOUT
    /// it is not possible from here, so this asserts the property that makes
    /// it free: the struct is a multiple of its 8-byte alignment with room to
    /// spare after the three trailing `u32`s.
    #[test]
    fn the_classified_marker_costs_nothing() {
        assert_eq!(std::mem::align_of::<LiveCandleState>(), 8);
        // 11 f64 + 2 u64 + 2 i64 + 3 u32 + 1 bool = 133 bytes of payload,
        // which is why 136 has room and the flag is free.
        assert_eq!(std::mem::size_of::<LiveCandleState>(), 136);
    }

    /// The three refusals, each one a real hazard rather than defensive noise.
    #[test]
    fn net_volume_refuses_every_question_it_cannot_answer() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.volume = 1_000;
        s.net_volume_classified = true;
        s.net_volume_signed = 400;
        s.tick_count = 3;

        // Sanity: with all three preconditions met it DOES answer.
        assert_eq!(s.net_volume(), Some(400));

        // Not classified: a bar rebuilt from a source that does not carry the
        // accumulator. Reporting `Some(0)` here would say "perfectly balanced"
        // about a bar nobody classified.
        s.net_volume_classified = false;
        assert_eq!(s.net_volume(), None);
        s.net_volume_classified = true;

        // Untraded bar: no tick was folded, so nothing was classified.
        s.tick_count = 0;
        assert_eq!(s.net_volume(), None);
        s.tick_count = 3;

        // No classifiable volume: every tick repeated the same day-cumulative,
        // so there were no prints to attribute to a side.
        s.volume = 0;
        assert_eq!(s.net_volume(), None);
    }

    /// A traded bar whose flow genuinely nets to zero is a REAL reading.
    ///
    /// This is the distinction the `Option` exists for, and it is the one a
    /// naive implementation collapses: `Some(0)` means balanced, `None` means
    /// unanswerable, and a reader must be able to tell them apart.
    #[test]
    fn a_balanced_bar_reports_zero_and_that_is_not_a_refusal() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.volume = 1_000;
        s.tick_count = 8;
        s.net_volume_classified = true;
        s.net_volume_signed = 0;
        assert_eq!(
            s.net_volume(),
            Some(0),
            "500 lots bought and 500 sold is a balanced bar, not an unknown one"
        );
    }

    /// THE INVARIANT: the net can never exceed the gross it is drawn from.
    ///
    /// A row violating it would look like a real market reading — "this bar
    /// traded 1,000 lots, of which 5,000 were buys" — so the clamp is the last
    /// line of defence behind the fold's own arithmetic.
    #[test]
    fn net_volume_can_never_exceed_the_bars_own_volume() {
        let mut s = sealed(24_273.15, 24_341.95, 24_334.55);
        s.tick_count = 4;
        s.net_volume_classified = true;
        s.volume = 1_000;

        s.net_volume_signed = 5_000;
        assert_eq!(
            s.net_volume(),
            Some(1_000),
            "clamped to the bar's own gross"
        );
        s.net_volume_signed = -5_000;
        assert_eq!(s.net_volume(), Some(-1_000), "and on the sell side too");

        // The saturating ceiling: a `u64` volume past `i64::MAX` must not wrap
        // the clamp bound into a negative number, which would invert the sign.
        s.volume = u64::MAX;
        s.net_volume_signed = i64::MIN;
        let nv = s.net_volume().expect("classified and traded");
        assert!(nv < 0, "a sell-heavy bar must never report as buy-heavy");
    }

    /// The stored sign is the bar's DIRECTION and the magnitude is untouched —
    /// the operator's 2026-09-18 rule, verbatim: *"our current volume si
    /// rpecisley correct dude but we just need to accept this negative sign"*.
    #[test]
    fn a_bar_that_closed_down_signs_its_whole_gross_volume_negative() {
        let mut s = sealed(24_290.00, 24_341.95, 24_334.55);
        s.bucket_open_prev_close = 24_300.00;
        s.volume = 1_900;
        assert_eq!(s.signed_volume(), -1_900);
    }

    #[test]
    fn a_bar_that_closed_up_signs_positive() {
        let mut s = sealed(24_310.00, 24_341.95, 24_334.55);
        s.bucket_open_prev_close = 24_300.00;
        s.volume = 1_900;
        assert_eq!(s.signed_volume(), 1_900);
    }

    /// A FLAT bar is POSITIVE, never zero. Zeroing is what TradingView's
    /// built-in Net Volume does, and adopting it in the STORED column would
    /// destroy that bar's magnitude — breaking `abs(v) == volume`, and with it
    /// the 10m derivation and every view that renders the chart-exact form.
    /// A view can turn `+gross` into `0`; nothing turns `0` back into `+gross`.
    #[test]
    fn a_flat_bar_is_positive_never_zero_so_the_magnitude_survives() {
        let mut s = sealed(24_300.00, 24_341.95, 24_334.55);
        s.bucket_open_prev_close = 24_300.00;
        s.volume = 1_900;
        assert_eq!(s.signed_volume(), 1_900, "a flat bar keeps its magnitude");
    }

    /// The session's first bucket has no previous close. `0.0` is that field's
    /// absent sentinel, not a price, so nothing fell and the bar is positive.
    #[test]
    fn the_sessions_first_bucket_has_no_previous_close_and_is_positive() {
        let mut s = sealed(24_290.00, 24_341.95, 24_334.55);
        s.bucket_open_prev_close = 0.0;
        s.volume = 1_900;
        assert_eq!(s.signed_volume(), 1_900);
    }

    /// NaN compares `false` against everything, so without the explicit guard
    /// a NaN baseline falls through to the negative arm and signs a whole bar
    /// on a comparison that never happened.
    #[test]
    fn a_non_finite_previous_close_cannot_order_and_never_signs_a_bar_negative() {
        for prev in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let mut s = sealed(24_290.00, 24_341.95, 24_334.55);
            s.bucket_open_prev_close = prev;
            s.volume = 1_900;
            assert_eq!(
                s.signed_volume(),
                1_900,
                "prev={prev} must not sign negative"
            );
        }
    }

    /// `signed_volume().unsigned_abs() == volume`, ALWAYS. This is the
    /// invariant the 10m derivation rests on: `sum(abs(v))` over ten 1m bars
    /// recovers the gross, and the sign is applied at the 10m level. A change
    /// that breaks this equality silently makes every derived frame wrong.
    #[test]
    fn signed_volume_always_lets_the_magnitude_survive_the_sign() {
        for (close, prev) in [
            (24_290.00, 24_300.00),
            (24_310.00, 24_300.00),
            (24_300.00, 24_300.00),
            (24_290.00, 0.0),
            (24_290.00, f64::NAN),
        ] {
            for volume in [0_u64, 1, 1_900, 20_000_000] {
                let mut s = sealed(close, 24_341.95, 24_334.55);
                s.bucket_open_prev_close = prev;
                s.volume = volume;
                assert_eq!(
                    s.signed_volume().unsigned_abs(),
                    volume,
                    "close={close} prev={prev} volume={volume}"
                );
            }
        }
    }

    /// The ruled-on trade, pinned so nobody "fixes" it back by accident.
    ///
    /// A bar that traded 1,000 into the bid and 900 into the offer has a flow
    /// of −100 — but if its close ticked UP, the stored column reads `+1,900`.
    /// That inversion is exactly what `net_volume()`'s own note records the
    /// close-vs-close rule getting wrong, and it is what the operator ruled
    /// for twice on 2026-09-18 with the objection stated. The in-memory flow
    /// accumulator still disagrees; it simply no longer reaches a column.
    #[test]
    fn the_sign_follows_the_close_even_when_the_flow_disagrees() {
        let mut s = sealed(24_301.00, 24_341.95, 24_334.55);
        s.bucket_open_prev_close = 24_300.00;
        s.volume = 1_900;
        s.net_volume_signed = -100;
        s.net_volume_classified = true;
        s.tick_count = 2;

        assert_eq!(s.signed_volume(), 1_900, "direction, not flow");
        assert_eq!(
            s.net_volume(),
            Some(-100),
            "the flow reading still disagrees"
        );
    }

    /// A bar whose close ROSE against the previous sealed bar's close reports
    /// a positive percentage — and it is the SAME comparison the signed
    /// volume derives its sign from, which is why these two columns can be
    /// read side by side without a reader having to guess which baseline
    /// each one used.
    #[test]
    fn close_chg_pct_from_prev_bar_rises_against_the_previous_bar_close() {
        let s = LiveCandleState {
            bucket_start_ist_secs: 33_300,
            close: 101.0,
            bucket_open_prev_close: 100.0,
            ..LiveCandleState::empty()
        };
        assert_eq!(s.close_chg_pct_from_prev_bar(), Some(1.0));
    }

    /// The falling case, paired with the volume sign it explains: a bar that
    /// closed BELOW the previous bar's close reports a negative percentage,
    /// and `signed_volume` on the same bar is negative for the same reason.
    /// Asserting both here is what stops one of the two rules drifting.
    #[test]
    fn a_falling_bar_reports_a_negative_change_beside_a_negative_volume() {
        let s = LiveCandleState {
            bucket_start_ist_secs: 33_300,
            close: 99.0,
            bucket_open_prev_close: 100.0,
            volume: 500,
            ..LiveCandleState::empty()
        };
        assert_eq!(s.close_chg_pct_from_prev_bar(), Some(-1.0));
        assert_eq!(
            s.signed_volume(),
            -500,
            "the sign of the volume and the sign of the price change are the \
             same comparison — if these two ever disagree, one of the rules moved"
        );
    }

    /// A FLAT bar is 0.00%, not `None`. The baseline exists and the bar is
    /// genuinely unchanged; reporting absence there would read as "no
    /// previous bar", which is a different fact.
    #[test]
    fn a_flat_bar_reports_zero_rather_than_absence() {
        let s = LiveCandleState {
            bucket_start_ist_secs: 33_300,
            close: 100.0,
            bucket_open_prev_close: 100.0,
            ..LiveCandleState::empty()
        };
        assert_eq!(s.close_chg_pct_from_prev_bar(), Some(0.0));
    }

    /// `bucket_open_prev_close == 0.0` is the documented NO-BASELINE sentinel
    /// — the session's first bucket, and any bar whose predecessor was never
    /// sealed. It must report `None`, never a percentage computed against
    /// zero: dividing by it yields an infinity that would reach the column as
    /// a fabricated number.
    #[test]
    fn the_session_first_bar_has_no_baseline_and_reports_none() {
        let s = LiveCandleState {
            bucket_start_ist_secs: 33_300,
            close: 100.0,
            bucket_open_prev_close: 0.0,
            ..LiveCandleState::empty()
        };
        assert_eq!(s.close_chg_pct_from_prev_bar(), None);
    }

    /// A non-finite or negative baseline is refused for the same reason: the
    /// only honest answer is "no usable baseline", and a NaN reaching a
    /// persisted column is the poisoning class this repository has already
    /// paid for once.
    #[test]
    fn a_corrupt_baseline_is_refused_rather_than_propagated() {
        for baseline in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let s = LiveCandleState {
                bucket_start_ist_secs: 33_300,
                close: 100.0,
                bucket_open_prev_close: baseline,
                ..LiveCandleState::empty()
            };
            assert_eq!(
                s.close_chg_pct_from_prev_bar(),
                None,
                "baseline {baseline} must yield no reading"
            );
        }
    }

    /// The accessor rounds by the SHARED rule, not a second one of its own:
    /// it goes through `pct_change`, so the 2026-09-03 vendor-matching
    /// 2-decimal directive applies to it automatically. A local formula here
    /// would be how this column comes to disagree with the four that already
    /// exist.
    #[test]
    fn close_chg_pct_from_prev_bar_rounds_by_the_same_two_decimal_rule() {
        let s = LiveCandleState {
            bucket_start_ist_secs: 33_300,
            close: 24_273.15,
            bucket_open_prev_close: 24_334.55,
            ..LiveCandleState::empty()
        };
        let got = s.close_chg_pct_from_prev_bar().expect("baseline is usable");
        assert_eq!(
            got,
            pct_change(24_273.15, 24_334.55),
            "the accessor must reuse pct_change, not restate the arithmetic"
        );
        let rendered = format!("{got}");
        if let Some(fraction) = rendered.split_once('.').map(|(_, f)| f) {
            assert!(
                fraction.len() <= 2,
                "rendered {rendered:?} with {} decimals — Dhan publishes 2",
                fraction.len()
            );
        }
    }
}
