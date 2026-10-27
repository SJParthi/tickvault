//! `tick_quotes` — main-feed packets that carry no new trade (plan item M5,
//! owner Quote 41, 2026-10-27; `websocket-connection-scope-lock.md`
//! §"2026-10-27 — REPEAT QUOTES GET THEIR OWN TABLE").
//!
//! Measured on the live box on 2026-10-26 (depth already off): about 70-75%
//! of `ticks` rows were quotes with no new trade, each carrying its last trade
//! time as `ts`, minutes to hours old. Every such row landed behind the
//! table's newest `ts` and made QuestDB rewrite an older part of the
//! partition (about 51 KB written per tick, the root disk at 125 MiB/s all
//! session). `tick_ts_placement` (plan item M1) counts that shape.
//!
//! The drain now routes such a packet here instead. The row has every `ticks`
//! column plus `exchange_timestamp`, the vendor's last trade time, kept
//! unchanged. Its designated `ts` is the ARRIVAL time in IST, built exactly
//! like `received_at` (UTC receipt + the IST offset), so the table only ever
//! appends. `ticks.ts` is untouched: exchange time, never + 5:30.
//!
//! The rows ride the SAME tick writer and ILP buffer as `ticks`: the WAL,
//! ring, rescue, spill, park, replay and the offload thread carry both, and a
//! WAL replay decides the table from the same data in the same order. Nothing
//! is dropped, sampled or coalesced.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS tick_quotes (
//!     received_at TIMESTAMP, ts TIMESTAMP, contract SYMBOL, feed SYMBOL,
//!     segment SYMBOL, security_id LONG, ltp DOUBLE, open DOUBLE, high DOUBLE,
//!     low DOUBLE, close DOUBLE, volume LONG, oi LONG, oi_day_high LONG,
//!     oi_day_low LONG, avg_price DOUBLE, last_trade_qty LONG,
//!     total_buy_qty LONG, total_sell_qty LONG, payload_hash LONG,
//!     capture_seq LONG, exchange_timestamp TIMESTAMP
//! ) TIMESTAMP(ts) PARTITION BY HOUR WAL;
//! -- DEDUP UPSERT KEYS(ts, security_id, segment, capture_seq, feed)
//! ```
//!
//! `ts` = arrival (IST); `exchange_timestamp` = the vendor's last trade time
//! (IST, unchanged). Every other column means what it means in `ticks`.
//!
//! This module owns the table's name, DEDUP key, DDL and row counter; the row
//! itself is written by `tick_persistence::TickWriter::append_tick_quote_with_seq`.

use std::sync::OnceLock;

use metrics::Counter;

/// The table name.
pub const TICK_QUOTES_TABLE: &str = "tick_quotes";

/// DEDUP UPSERT KEYS, the same five keys as `ticks` (`feed` and `segment`
/// included, I-P1-11 + feed-in-key). `capture_seq` is replay-stable, so a
/// replayed packet collapses onto the row the live process wrote.
pub const DEDUP_KEY_TICK_QUOTES: &str = "ts, security_id, segment, capture_seq, feed";

/// Rows appended to `tick_quotes`, process-wide. Local `/metrics` only: no
/// alarm, no EMF name, no filter.
pub const TICK_QUOTES_ROWS_COUNTER: &str = "tv_tick_quotes_rows_total";

/// The idempotent `CREATE TABLE` for `tick_quotes`: every `ticks` column in
/// the same order, then `exchange_timestamp`; `PARTITION BY HOUR WAL` as
/// `ticks`. Pure.
#[must_use]
pub fn tick_quotes_create_ddl() -> String {
    // APPROVED: table DDL, built once at boot
    format!(
        "CREATE TABLE IF NOT EXISTS {TICK_QUOTES_TABLE} (\
            received_at TIMESTAMP, \
            ts TIMESTAMP, \
            contract SYMBOL, \
            feed SYMBOL, \
            segment SYMBOL, \
            security_id LONG, \
            ltp DOUBLE, \
            open DOUBLE, \
            high DOUBLE, \
            low DOUBLE, \
            close DOUBLE, \
            volume LONG, \
            oi LONG, \
            oi_day_high LONG, \
            oi_day_low LONG, \
            avg_price DOUBLE, \
            last_trade_qty LONG, \
            total_buy_qty LONG, \
            total_sell_qty LONG, \
            payload_hash LONG, \
            capture_seq LONG, \
            exchange_timestamp TIMESTAMP\
        ) TIMESTAMP(ts) PARTITION BY HOUR WAL"
    )
}

/// The ordered self-heal statements for `tick_quotes`: CREATE, one
/// `ADD COLUMN IF NOT EXISTS` per column, then the DEDUP key. Never a DROP.
/// `columns` is the `ticks` column list, so the two tables cannot drift. Pure.
#[must_use]
pub fn tick_quotes_ensure_statements(columns: &[(&str, &str)]) -> Vec<String> {
    let mut out = vec![tick_quotes_create_ddl()]; // APPROVED: boot DDL statement list, never per row
    for (col, ty) in columns
        .iter()
        .copied()
        .chain(std::iter::once(("exchange_timestamp", "TIMESTAMP")))
    {
        // APPROVED: per-column ADD COLUMN, boot schema self-heal
        out.push(format!(
            "ALTER TABLE {TICK_QUOTES_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty}"
        ));
    }
    // APPROVED: DEDUP ENABLE statement, boot only
    out.push(format!(
        "ALTER TABLE {TICK_QUOTES_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_TICK_QUOTES})"
    ));
    out
}

fn rows_counter() -> &'static Counter {
    static HANDLE: OnceLock<Counter> = OnceLock::new();
    HANDLE.get_or_init(|| metrics::counter!(TICK_QUOTES_ROWS_COUNTER))
}

/// Registers the row counter at 0. Called when a tick writer is built, so the
/// series exists before the first row.
pub fn register_baseline() {
    rows_counter().increment(0);
}

/// Notes one appended `tick_quotes` row. One counter add, O(1), no
/// allocation after [`register_baseline`].
pub fn note_row() {
    rows_counter().increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tick_quotes_dedup_key_includes_feed_and_segment() {
        let keys: Vec<&str> = DEDUP_KEY_TICK_QUOTES.split(',').map(str::trim).collect();
        assert_eq!(
            keys,
            ["ts", "security_id", "segment", "capture_seq", "feed"]
        );
        let stmts = tick_quotes_ensure_statements(&[("ltp", "DOUBLE")]);
        assert_eq!(
            stmts.last().map(String::as_str),
            Some(
                "ALTER TABLE tick_quotes DEDUP ENABLE UPSERT KEYS(ts, security_id, segment, \
                 capture_seq, feed)"
            )
        );
    }

    #[test]
    fn test_tick_quotes_ensure_statements_are_create_then_alter_then_dedup() {
        let stmts = tick_quotes_ensure_statements(&[("ltp", "DOUBLE"), ("feed", "SYMBOL")]);
        assert_eq!(stmts.len(), 1 + 3 + 1);
        assert!(stmts[0].starts_with("CREATE TABLE IF NOT EXISTS tick_quotes ("));
        assert!(stmts[0].ends_with("TIMESTAMP(ts) PARTITION BY HOUR WAL"));
        assert!(stmts[0].contains("exchange_timestamp TIMESTAMP"));
        assert_eq!(
            stmts[3],
            "ALTER TABLE tick_quotes ADD COLUMN IF NOT EXISTS exchange_timestamp TIMESTAMP"
        );
        for s in &stmts {
            assert!(!s.to_ascii_uppercase().contains("DROP"), "{s}");
        }
    }

    #[test]
    fn test_register_baseline_and_note_row_resolve_one_handle() {
        register_baseline();
        note_row();
        register_baseline();
        assert!(std::ptr::eq(rows_counter(), rows_counter()));
    }

    // -- the writer side (`TickWriter::append_tick_quote_with_seq`) --------

    use crate::tick_persistence::{TICKS_COLUMNS, TickWriter};
    use tickvault_common::constants::IST_UTC_OFFSET_NANOS;
    use tickvault_common::feed::Feed;
    use tickvault_common::tick_types::ParsedTick;

    /// A Full-packet tick received at 12:30:00.111 IST whose last trade was an
    /// hour earlier (11:30:00 IST): the repeat-quote shape.
    fn repeat_quote() -> ParsedTick {
        ParsedTick {
            security_id: 52_175,
            exchange_segment_code: 2, // NSE_FNO
            last_traded_price: 101.5,
            last_trade_quantity: 75,
            exchange_timestamp: 1_779_971_400 - 3_600,
            received_at_nanos: 1_779_951_600_111_000_000,
            average_traded_price: 100.2,
            volume: 4_500,
            total_sell_quantity: 9_000,
            total_buy_quantity: 10_000,
            day_open: 99.0,
            day_close: 98.0,
            day_high: 104.0,
            day_low: 97.5,
            open_interest: 120_000,
            oi_day_high: 0,
            oi_day_low: 0,
            iv: f64::NAN,
            delta: f64::NAN,
            gamma: f64::NAN,
            theta: f64::NAN,
            vega: f64::NAN,
        }
    }

    fn arrival_ist_nanos(tick: &ParsedTick) -> i64 {
        tick.received_at_nanos + IST_UTC_OFFSET_NANOS
    }

    /// Regression (2026-10-27, plan item M5): a repeat quote goes to
    /// `tick_quotes` with the ARRIVAL time as its designated `ts` and the
    /// vendor's trade time kept in `exchange_timestamp`; nothing goes to
    /// `ticks`. Before the split this row landed in `ticks` at 11:30, an hour
    /// behind the newest row.
    #[test]
    fn test_regression_repeat_quote_goes_to_tick_quotes_with_arrival_ts() {
        let tick = repeat_quote();
        let mut w = TickWriter::for_test(Feed::Dhan);
        w.append_tick_quote_with_seq(&tick, 41)
            .expect("append must succeed"); // APPROVED: test
        let buf = w.buffer_utf8();
        assert!(!buf.lines().any(|l| l.starts_with("ticks,")), "{buf}");
        let line = buf
            .lines()
            .find(|l| l.starts_with("tick_quotes,"))
            .unwrap_or_else(|| panic!("no tick_quotes line: {buf}")); // APPROVED: test
        assert!(line.contains(",feed=dhan"), "{line}");
        assert!(line.contains("segment=NSE_FNO"), "{line}");
        assert!(line.contains("capture_seq=41i"), "{line}");
        assert!(
            line.ends_with(&format!(" {}", arrival_ist_nanos(&tick))),
            "designated ts must be the arrival (received_at) time: {line}"
        );
        let ltt_micros = i64::from(tick.exchange_timestamp) * 1_000_000;
        assert!(
            line.contains(&format!("exchange_timestamp={ltt_micros}t")),
            "the vendor trade time is kept unchanged: {line}"
        );
        assert_eq!(w.quote_rows_appended(), 1);
        assert_eq!(w.pending(), 1);
    }

    /// A packet the drain does not route to quotes keeps today's `ticks` row:
    /// exchange time as `ts`, no `exchange_timestamp` column.
    #[test]
    fn test_new_trade_stays_in_ticks_with_exchange_ts() {
        let tick = repeat_quote();
        let mut w = TickWriter::for_test(Feed::Dhan);
        w.append_tick_with_seq(&tick, 42).expect("append"); // APPROVED: test
        let buf = w.buffer_utf8();
        let line = buf
            .lines()
            .find(|l| l.starts_with("ticks,"))
            .unwrap_or_else(|| panic!("no ticks line: {buf}")); // APPROVED: test
        assert!(
            line.ends_with(&format!(
                " {}",
                i64::from(tick.exchange_timestamp) * 1_000_000_000
            )),
            "{line}"
        );
        assert!(!line.contains("exchange_timestamp="), "{line}");
        assert!(!buf.lines().any(|l| l.starts_with("tick_quotes,")), "{buf}");
        assert_eq!(w.quote_rows_appended(), 0);
    }

    /// With no receipt time the row cannot be stamped at arrival, so it is
    /// written to `ticks` rather than dropped or stamped with a guess.
    #[test]
    fn test_append_tick_quote_with_seq_without_receipt_keeps_quote_rows_appended_at_zero() {
        let mut tick = repeat_quote();
        tick.received_at_nanos = 0;
        let mut w = TickWriter::for_test(Feed::Dhan);
        w.append_tick_quote_with_seq(&tick, 43).expect("append"); // APPROVED: test
        let buf = w.buffer_utf8();
        assert_eq!(
            buf.lines().filter(|l| l.starts_with("ticks,")).count(),
            1,
            "{buf}"
        );
        assert!(!buf.lines().any(|l| l.starts_with("tick_quotes,")), "{buf}");
        assert_eq!(w.quote_rows_appended(), 0);
    }

    /// The `ticks` gates are applied before the table is chosen: a quote that
    /// `ticks` would refuse (post-close trade time) is kept in
    /// `feed_aux_packets` exactly as before, never in either tick table.
    #[test]
    fn test_append_tick_quote_with_seq_applies_the_same_window_gate_as_ticks() {
        let mut tick = repeat_quote();
        // 15:45 IST trade time and receipt.
        tick.exchange_timestamp = 1_779_971_400 + 3 * 3_600 + 15 * 60;
        tick.received_at_nanos += (3 * 3_600 + 15 * 60) * 1_000_000_000;
        for to_quotes in [false, true] {
            let mut w = TickWriter::for_test(Feed::Dhan);
            let appended = if to_quotes {
                w.append_tick_quote_with_seq(&tick, 44)
            } else {
                w.append_tick_with_seq(&tick, 44)
            };
            appended.expect("a refusal is Ok"); // APPROVED: test
            let buf = w.buffer_utf8();
            assert_eq!(w.aux_rows_appended(), 1, "to_quotes={to_quotes}: {buf}");
            assert!(
                !buf.lines()
                    .any(|l| l.starts_with("ticks,") || l.starts_with("tick_quotes,")),
                "to_quotes={to_quotes}: {buf}"
            );
        }
    }

    /// The `tick_quotes` columns are the `ticks` columns plus
    /// `exchange_timestamp`, so the two tables cannot drift.
    #[test]
    fn test_tick_quotes_create_ddl_carries_every_ticks_column() {
        let ddl = tick_quotes_create_ddl();
        for (col, ty) in TICKS_COLUMNS {
            assert!(
                ddl.contains(&format!("{col} {ty}")),
                "{col} {ty} missing: {ddl}"
            );
        }
        assert!(ddl.contains("ts TIMESTAMP"));
        assert_eq!(
            tick_quotes_ensure_statements(TICKS_COLUMNS).len(),
            1 + TICKS_COLUMNS.len() + 1 + 1
        );
    }
}
