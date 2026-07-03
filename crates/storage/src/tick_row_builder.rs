//! Shared `ticks` ILP **row builder** — the single source of the wire bytes for
//! BOTH live feeds (C1 of the C1/C2 feed-convergence plan,
//! `.claude/plans/active-plan-c1c2-feed-convergence.md`).
//!
//! ## Why this exists (the convergence)
//!
//! Before C1 the Dhan writer (`tick_persistence::build_tick_row_seq`) and the
//! Groww writer (`groww_persistence::GrowwLiveTickWriter::append_row`) each
//! HAND-WROTE a near-duplicate `ticks` ILP row (~25 lines of duplicated
//! `.symbol(...)` / `.column_*(...)` / `.at(...)` calls). Two hand-written row
//! paths into ONE shared table is exactly the drift risk the audit flagged: a
//! future column/DEDUP-key change could land on one path and not the other.
//!
//! C1 converges ONLY that duplicated **row-string build** into the single
//! [`build_tick_row_for_feed`] below. The deliverable is the shared BUILDER, not
//! a facade — both writers now construct a [`RawTickFields`] and call this one
//! function, so the table name, the DEDUP key columns, the SYMBOL-before-column
//! ILP ordering, and the NULL-not-0 policy can NEVER diverge between feeds.
//!
//! ## Honest envelope (operator-charter §F)
//!
//! C1 removes ~25 lines of duplicated ILP-append; it does NOT unify the
//! resilience tier. Dhan keeps its ring→spill→DLQ rescue chain
//! (`tick_persistence`); Groww keeps its lazy-connect buffer whose durable floor
//! is the sidecar capture-at-receipt file (lock §32). Only the row STRING build
//! is shared. `capture_seq` SOURCES stay per-feed (Dhan = WAL `frame_seq`; Groww
//! = `next_capture_seq` seeded from `ts_ist_nanos`).
//!
//! ## The NULL-not-0 contract (load-bearing)
//!
//! For every `Option` column that is `None`, the builder **OMITS the column
//! token entirely** so QuestDB stores NULL — it NEVER writes `0`. A Groww LTP
//! feed supplies no OHLC / OI / avg_price / qty fields / payload_hash, so
//! those stay NULL (not a misleading `0`). Since 2026-07-03 (lag forensics
//! fix #3) Groww DOES stamp `received_at` (the bridge's per-wake receipt
//! clock, plausibility-gated — a broken clock still yields `None` → NULL).
//! Pinned by the NULL-not-0 wire-byte tests.
//!
//! ## Pre-validated ILP names (latency, inherited from `build_tick_row_seq`)
//!
//! questdb-rs re-validates every `&str` table/column name char-by-char on EVERY
//! row; passing `TableName`/`ColumnName::new_unchecked` wrappers skips that. It is
//! sound because every literal below is a static lowercase-ASCII identifier —
//! mechanically proven by `tick_persistence::tests::
//! test_tick_row_unchecked_ilp_names_pass_validation`, whose unchecked-name
//! literal-scan over `tick_persistence.rs` is unaffected (the Dhan caller still
//! routes through this builder, the literals live here now but are equally
//! static), AND by [`tests::shared_builder_unchecked_ilp_names_pass_validation`]
//! in this module.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, ColumnName, TableName, TimestampNanos};

use tickvault_common::constants::QUESTDB_TABLE_TICKS;
use tickvault_common::feed::Feed;

/// Feed-agnostic input to [`build_tick_row_for_feed`], wide enough for BOTH
/// feeds. Required-for-both fields are plain; per-feed-OPTIONAL columns are
/// `Option<T>` so a feed that does not supply a value yields a NULL cell (the
/// builder omits the token), never `0`.
///
/// ### Numeric carriers (E3 / E4)
///
/// - `volume: i64` — Groww keeps cumulative day volume as `i64` (it exceeds
///   `u32` intraday). Dhan's `ParsedTick.volume` is `u32`; the caller widens it
///   losslessly (`i64::from(u32)`) BEFORE building this struct. A Groww `i64`
///   passes through unchanged — it is NEVER funnelled through a `u32` field.
/// - `ltp: f64` — the common LTP carrier. Dhan's WebSocket LTP is `f32`; the
///   caller applies `round_to_2dp(f32_to_f64_clean(f32))` so the `f32` widens to
///   `f64` EXACTLY (shortest-decimal, no IEEE-754 widening artifact — see
///   `data-integrity.md`) BEFORE building this struct. Groww's native `f64` is
///   carried verbatim. Selecting `f64` as the carrier means the conversion +
///   rounding stays a per-feed concern at the CALL site, not in this builder.
///
/// All fields are `Copy`; no heap allocation. `segment` + `feed` are
/// `&'static str` so the SYMBOL writes are zero-alloc O(1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RawTickFields {
    // --- required for both feeds ---
    /// Composite-key part 1 (I-P1-11), widened to `i64` for the LONG column.
    pub security_id: i64,
    /// Composite-key part 2 (I-P1-11) — `&'static` segment string
    /// (`IDX_I` / `NSE_EQ` / `NSE_FNO` / …) for the `segment` SYMBOL.
    pub segment: &'static str,
    /// Last traded price (common `f64` carrier — see struct docs).
    pub ltp: f64,
    /// Cumulative day volume (`i64` — Groww-wide, Dhan widened from `u32`).
    pub volume: i64,
    /// Designated timestamp: IST epoch nanoseconds (NO offset applied here — the
    /// caller normalises Dhan `exchange_timestamp × 1e9` / Groww `ts_ist_nanos`).
    pub ts_ist_nanos: i64,
    /// Replay-stable dedup tiebreaker (TICK-SEQ-01). Per-feed SOURCE; this
    /// builder takes it as a value and never generates one.
    pub capture_seq: i64,

    // --- per-feed OPTIONAL columns: None => NULL cell (never 0) ---
    /// Day open price. `None` for an LTP-only feed (Groww) → NULL.
    pub open: Option<f64>,
    /// Day high price. `None` → NULL.
    pub high: Option<f64>,
    /// Day low price. `None` → NULL.
    pub low: Option<f64>,
    /// Previous-session close (the Quote/Full `close` field). `None` → NULL.
    pub close: Option<f64>,
    /// Open interest. `None` → NULL.
    pub oi: Option<i64>,
    /// Average traded price (VWAP). `None` → NULL.
    pub avg_price: Option<f64>,
    /// Last trade quantity. `None` → NULL.
    pub last_trade_qty: Option<i64>,
    /// Total buy quantity. `None` → NULL.
    pub total_buy_qty: Option<i64>,
    /// Total sell quantity. `None` → NULL.
    pub total_sell_qty: Option<i64>,
    /// Raw exchange timestamp LONG = IST epoch SECONDS (verbatim audit column).
    /// `None` → NULL. Both feeds today supply it, but it is optional so a future
    /// feed without a wall-second stamp leaves NULL rather than a misleading 0.
    pub exchange_timestamp: Option<i64>,
    /// Local receive timestamp as IST nanoseconds (the `received_at` TIMESTAMP
    /// column). Dhan supplies it; Groww does NOT (`None` → NULL).
    pub received_at_ist_nanos: Option<i64>,
    /// Deterministic content fingerprint (`payload_hash` LONG). Dhan supplies it;
    /// Groww does NOT (`None` → NULL).
    pub payload_hash: Option<i64>,
}

/// Writes ONE `ticks` ILP row for `feed` from `fields` into `buffer` (no flush).
///
/// The single converged row-string builder for BOTH live feeds (C1). `feed` is a
/// compile-time `Copy` enum — the SYMBOL `feed=…` is `feed.as_str()` (`&'static
/// str`, zero-alloc); there is NO `dyn`/trait-object dispatch on this per-tick
/// path. ILP requires ALL symbols (`segment`, `feed`) before any `column_*`, so
/// they are written first. For every `Option` column that is `None`, the
/// matching `column_*` call is SKIPPED → the cell is NULL (never `0`).
///
/// O(1); zero heap allocation beyond the ILP buffer's own internal growth.
#[inline]
pub fn build_tick_row_for_feed(
    buffer: &mut Buffer,
    fields: &RawTickFields,
    feed: Feed,
) -> Result<()> {
    let ts = TimestampNanos::new(fields.ts_ist_nanos);

    // SYMBOLS first (ILP requires all symbols before any column_*).
    buffer
        .table(TableName::new_unchecked(QUESTDB_TABLE_TICKS))
        .context("table name")?
        .symbol(ColumnName::new_unchecked("segment"), fields.segment)
        .context("segment")?
        // `feed` is the broker source, part of the DEDUP key (operator
        // 2026-06-19). `feed.as_str()` is the stable wire label ('dhan'/'groww').
        .symbol(ColumnName::new_unchecked("feed"), feed.as_str())
        .context("feed")?;

    // Required-for-both columns.
    buffer
        .column_i64(ColumnName::new_unchecked("security_id"), fields.security_id)
        .context("security_id")?
        .column_f64(ColumnName::new_unchecked("ltp"), fields.ltp)
        .context("ltp")?;

    // Per-feed OPTIONAL price/qty columns — each omitted (NULL) when None.
    // Written in the SAME order as the legacy Dhan path so the on-wire byte
    // sequence is byte-identical for a fully-populated (Dhan) row.
    if let Some(v) = fields.open {
        buffer
            .column_f64(ColumnName::new_unchecked("open"), v)
            .context("open")?;
    }
    if let Some(v) = fields.high {
        buffer
            .column_f64(ColumnName::new_unchecked("high"), v)
            .context("high")?;
    }
    if let Some(v) = fields.low {
        buffer
            .column_f64(ColumnName::new_unchecked("low"), v)
            .context("low")?;
    }
    if let Some(v) = fields.close {
        buffer
            .column_f64(ColumnName::new_unchecked("close"), v)
            .context("close")?;
    }

    // volume is required-for-both; in the legacy Dhan order it sits after
    // close and before oi.
    buffer
        .column_i64(ColumnName::new_unchecked("volume"), fields.volume)
        .context("volume")?;

    if let Some(v) = fields.oi {
        buffer
            .column_i64(ColumnName::new_unchecked("oi"), v)
            .context("oi")?;
    }
    if let Some(v) = fields.avg_price {
        buffer
            .column_f64(ColumnName::new_unchecked("avg_price"), v)
            .context("avg_price")?;
    }
    if let Some(v) = fields.last_trade_qty {
        buffer
            .column_i64(ColumnName::new_unchecked("last_trade_qty"), v)
            .context("last_trade_qty")?;
    }
    if let Some(v) = fields.total_buy_qty {
        buffer
            .column_i64(ColumnName::new_unchecked("total_buy_qty"), v)
            .context("total_buy_qty")?;
    }
    if let Some(v) = fields.total_sell_qty {
        buffer
            .column_i64(ColumnName::new_unchecked("total_sell_qty"), v)
            .context("total_sell_qty")?;
    }
    if let Some(v) = fields.exchange_timestamp {
        buffer
            .column_i64(ColumnName::new_unchecked("exchange_timestamp"), v)
            .context("exchange_timestamp")?;
    }
    if let Some(v) = fields.received_at_ist_nanos {
        buffer
            .column_ts(
                ColumnName::new_unchecked("received_at"),
                TimestampNanos::new(v),
            )
            .context("received_at")?;
    }
    if let Some(v) = fields.payload_hash {
        buffer
            .column_i64(ColumnName::new_unchecked("payload_hash"), v)
            .context("payload_hash")?;
    }

    // capture_seq is required-for-both and is the LAST column before `at(ts)`
    // (matches the legacy Dhan order).
    buffer
        .column_i64(ColumnName::new_unchecked("capture_seq"), fields.capture_seq)
        .context("capture_seq")?
        .at(ts)
        .context("designated timestamp")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use questdb::ingress::ProtocolVersion;

    /// A fully-populated (Dhan-shape) `RawTickFields` — every Option is Some.
    fn dhan_shape_fields() -> RawTickFields {
        RawTickFields {
            security_id: 13,
            segment: "IDX_I",
            ltp: 23_146.45,
            volume: 1_234_567,
            ts_ist_nanos: 1_780_000_000_000_000_000,
            capture_seq: 42,
            open: Some(23_100.00),
            high: Some(23_200.00),
            low: Some(23_050.00),
            close: Some(23_090.00),
            oi: Some(987_654),
            avg_price: Some(23_145.10),
            last_trade_qty: Some(50),
            total_buy_qty: Some(10_000),
            total_sell_qty: Some(9_000),
            exchange_timestamp: Some(1_780_000_000),
            received_at_ist_nanos: Some(1_780_000_000_111_000_000),
            payload_hash: Some(-987_654_321),
        }
    }

    /// A Groww-shape `RawTickFields` — only the required-for-both columns are
    /// populated; every Dhan-only column is None (→ NULL on the wire).
    fn groww_shape_fields() -> RawTickFields {
        RawTickFields {
            security_id: 1_333,
            segment: "NSE_EQ",
            ltp: 2_847.55,
            volume: 1_234_567,
            ts_ist_nanos: 1_780_000_000_123_000_000,
            capture_seq: 42,
            open: None,
            high: None,
            low: None,
            close: None,
            oi: None,
            avg_price: None,
            last_trade_qty: None,
            total_buy_qty: None,
            total_sell_qty: None,
            exchange_timestamp: Some(1_780_000_000),
            received_at_ist_nanos: None,
            payload_hash: None,
        }
    }

    fn build(fields: &RawTickFields, feed: Feed) -> String {
        let mut buf = Buffer::new(ProtocolVersion::V1);
        build_tick_row_for_feed(&mut buf, fields, feed).expect("build");
        String::from_utf8_lossy(buf.as_bytes()).into_owned()
    }

    #[test]
    fn dhan_row_contains_every_column() {
        let text = build(&dhan_shape_fields(), Feed::Dhan);
        assert!(text.starts_with("ticks,"), "shared ticks table: {text}");
        assert!(text.contains("feed=dhan"), "dhan feed tag: {text}");
        assert!(text.contains("IDX_I"), "segment");
        for token in [
            "security_id=",
            "ltp=",
            "open=",
            "high=",
            "low=",
            "close=",
            "volume=",
            "oi=",
            "avg_price=",
            "last_trade_qty=",
            "total_buy_qty=",
            "total_sell_qty=",
            "exchange_timestamp=",
            "received_at=",
            "payload_hash=",
            "capture_seq=",
        ] {
            assert!(
                text.contains(token),
                "dhan row must contain {token}: {text}"
            );
        }
    }

    /// `build_tick_row_for_feed` must lay the row out in the EXACT legacy Dhan
    /// order: ALL symbols (`segment`, `feed`) first (ILP requirement), then the
    /// fixed column sequence `security_id → ltp → open → high → low → close →
    /// volume → oi → avg_price → last_trade_qty → total_buy_qty →
    /// total_sell_qty → exchange_timestamp → received_at → payload_hash →
    /// capture_seq`. The docstring pins this as byte-identical-golden; the other
    /// tests only assert token PRESENCE, so this is the only check of ORDER.
    #[test]
    fn test_build_tick_row_for_feed_dhan_emits_legacy_column_order() {
        let text = build(&dhan_shape_fields(), Feed::Dhan);

        // Both symbols must appear before ANY `column_*` token. The first
        // `=`-column token is `security_id=`; everything left of it is the
        // table + symbol region.
        let first_col = text
            .find("security_id=")
            .expect("security_id column present");
        let symbol_region = &text[..first_col];
        assert!(
            symbol_region.contains("segment=") && symbol_region.contains("feed=dhan"),
            "both symbols (segment, feed) must precede the first column: {text}"
        );

        // The 16 columns must appear strictly left-to-right in the legacy order.
        let ordered = [
            "security_id=",
            "ltp=",
            "open=",
            "high=",
            "low=",
            "close=",
            "volume=",
            "oi=",
            "avg_price=",
            "last_trade_qty=",
            "total_buy_qty=",
            "total_sell_qty=",
            "exchange_timestamp=",
            "received_at=",
            "payload_hash=",
            "capture_seq=",
        ];
        let mut cursor = first_col;
        for token in ordered {
            let pos = text[cursor..]
                .find(token)
                .map(|rel| cursor + rel)
                .unwrap_or_else(|| panic!("column {token} missing or out of order: {text}"));
            assert!(
                pos >= cursor,
                "column {token} must not precede the prior column (pos={pos}, cursor={cursor}): {text}"
            );
            cursor = pos + token.len();
        }

        // capture_seq is the LAST column, and the designated timestamp `at(ts)`
        // closes the line — so no column token may follow capture_seq's value.
        let cap = text.find("capture_seq=").expect("capture_seq present");
        let tail = &text[cap..];
        for following in ["ltp=", "open=", "volume=", "oi=", "received_at="] {
            assert!(
                !tail.contains(following),
                "no column ({following}) may appear after capture_seq: {text}"
            );
        }
    }

    /// E1 + NULL-not-0: a Groww row OMITS every Dhan-only column entirely
    /// (→ NULL), and never emits a `=0` placeholder for them.
    #[test]
    fn groww_row_omits_dhan_only_columns_null_not_zero() {
        let text = build(&groww_shape_fields(), Feed::Groww);
        assert!(text.starts_with("ticks,"), "shared ticks table: {text}");
        assert!(text.contains("feed=groww"), "groww feed tag: {text}");
        // present subset
        for token in [
            "security_id=",
            "ltp=",
            "volume=",
            "exchange_timestamp=",
            "capture_seq=",
        ] {
            assert!(
                text.contains(token),
                "groww row must contain {token}: {text}"
            );
        }
        // absent columns — NO token at all (NULL), never =0
        for token in [
            "open=",
            "high=",
            "low=",
            "close=",
            "oi=",
            "avg_price=",
            "last_trade_qty=",
            "total_buy_qty=",
            "total_sell_qty=",
            "received_at=",
            "payload_hash=",
        ] {
            assert!(
                !text.contains(token),
                "groww row must NOT contain {token} (must be NULL, not 0): {text}"
            );
        }
    }

    /// E3 — the volume carrier is `i64`: a Dhan `u32` widens losslessly and a
    /// Groww `i64` (which exceeds `u32` intraday) passes through unchanged.
    /// `RawTickFields.volume` is `i64`, so neither can truncate.
    #[test]
    fn volume_carrier_dhan_u32_widens_and_groww_i64_passes_through() {
        // Dhan: u32::MAX widens to i64 with no loss.
        let dhan_vol_u32: u32 = u32::MAX;
        let widened: i64 = i64::from(dhan_vol_u32);
        assert_eq!(widened, 4_294_967_295_i64, "u32 widens losslessly to i64");
        let mut f = dhan_shape_fields();
        f.volume = widened;
        assert!(build(&f, Feed::Dhan).contains("volume=4294967295i"));

        // Groww: an i64 cum_volume larger than u32::MAX is carried verbatim — it
        // is NEVER funnelled through a u32 (which would truncate it).
        let groww_vol_i64: i64 = 5_000_000_000; // > u32::MAX
        assert!(
            groww_vol_i64 > i64::from(u32::MAX),
            "value exceeds u32 range"
        );
        let mut g = groww_shape_fields();
        g.volume = groww_vol_i64;
        assert!(build(&g, Feed::Groww).contains("volume=5000000000i"));
    }

    /// E4 — the LTP carrier is `f64`: the Dhan caller pre-applies
    /// `round_to_2dp(f32_to_f64_clean(f32))`, so a Dhan `f32` widens EXACTLY
    /// (`10.20_f32` → `10.2`, not `10.19999980926514`). This builder carries the
    /// already-converted `f64` verbatim (Groww's native f64 is likewise verbatim).
    #[test]
    fn ltp_carrier_dhan_f32_widens_exactly() {
        let raw_f32: f32 = 10.20;
        let clean = tickvault_common::price_precision::round_to_2dp(
            tickvault_common::price_precision::f32_to_f64_clean(raw_f32),
        );
        assert!(
            (clean - 10.2).abs() < f64::EPSILON,
            "f32 10.20 must widen to exactly 10.2, got {clean}"
        );
        let mut f = dhan_shape_fields();
        f.ltp = clean;
        let text = build(&f, Feed::Dhan);
        assert!(
            text.contains("ltp=10.2"),
            "clean-widened ltp on wire: {text}"
        );
        assert!(
            !text.contains("10.19999"),
            "no IEEE-754 widening artifact: {text}"
        );
    }

    /// Explicit `received_at=` absence assertion (task requirement #3).
    #[test]
    fn groww_row_has_no_received_at_token() {
        let text = build(&groww_shape_fields(), Feed::Groww);
        assert!(
            !text.contains("received_at="),
            "Groww row must NOT carry received_at (Dhan-only): {text}"
        );
    }

    /// The feed SYMBOL is driven by the `Feed` enum, not a literal.
    #[test]
    fn feed_symbol_matches_feed_enum_label() {
        assert!(
            build(&dhan_shape_fields(), Feed::Dhan)
                .contains(&format!("feed={}", Feed::Dhan.as_str()))
        );
        assert!(
            build(&groww_shape_fields(), Feed::Groww)
                .contains(&format!("feed={}", Feed::Groww.as_str()))
        );
    }

    /// Two distinct `capture_seq` values for otherwise-identical Groww rows are
    /// BOTH emitted (the zero-loss tiebreaker — TICK-SEQ-01 `45→75→45` class).
    #[test]
    fn distinct_capture_seq_both_emitted() {
        let mut buf = Buffer::new(ProtocolVersion::V1);
        let mut a = groww_shape_fields();
        a.capture_seq = 100;
        let mut b = groww_shape_fields();
        b.capture_seq = 101;
        build_tick_row_for_feed(&mut buf, &a, Feed::Groww).expect("a");
        build_tick_row_for_feed(&mut buf, &b, Feed::Groww).expect("b");
        let text = String::from_utf8_lossy(buf.as_bytes());
        assert!(text.contains("capture_seq=100"));
        assert!(text.contains("capture_seq=101"));
    }

    /// Latency ratchet mirror: every `new_unchecked("…")` literal in THIS file's
    /// production region is a valid ILP identifier under the CHECKED validators.
    /// (`build_tick_row_seq`'s own ratchet still scans `tick_persistence.rs`;
    /// this one covers the converged builder that moved here.)
    #[test]
    fn shared_builder_unchecked_ilp_names_pass_validation() {
        let source = include_str!("tick_row_builder.rs");
        let production_region = source
            .split("mod tests {")
            .next()
            .unwrap_or_else(|| panic!("tests module marker missing"));
        let mut found = 0_usize;
        for chunk in production_region.split("new_unchecked(\"").skip(1) {
            let Some(end) = chunk.find('"') else {
                panic!("unterminated new_unchecked literal");
            };
            let name = &chunk[..end];
            ColumnName::new(name)
                .unwrap_or_else(|e| panic!("invalid ILP column name {name:?}: {e}"));
            found += 1;
        }
        // 2 SYMBOLs (segment, feed) + 16 columns = 18 literal names.
        assert_eq!(
            found, 18,
            "expected 18 new_unchecked literals, found {found}"
        );
        TableName::new(QUESTDB_TABLE_TICKS)
            .unwrap_or_else(|e| panic!("invalid ILP table name {QUESTDB_TABLE_TICKS:?}: {e}"));
        // Every new_unchecked( call must be a string literal or the allowlisted
        // QUESTDB_TABLE_TICKS const (mirrors the tick_persistence ratchet).
        let total = production_region.matches("new_unchecked(").count();
        let literal = production_region.matches("new_unchecked(\"").count();
        let allowlisted = production_region
            .matches("new_unchecked(QUESTDB_TABLE_TICKS)")
            .count();
        assert_eq!(
            total,
            literal + allowlisted,
            "non-literal new_unchecked arg present"
        );
    }
}
