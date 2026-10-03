//! `feed_aux_packets` — the Dhan feed packets the parser decodes and the candle
//! fold does not use (plan item 45h, 2026-10-02).
//!
//! ## Why this table exists
//!
//! Operator, 2026-09-29: *"nothign shdou lneevr ever be missed or removed or
//! deleted"*. The binding row is in `websocket-connection-scope-lock.md`
//! ("2026-09-29 — ZERO LOSS"): open-interest packets, market-status packets,
//! ticks stamped outside 09:00–15:40 and the prior-day last-trade snapshot on
//! connect are persisted, `feed` in every DEDUP key, and kept OUT of the candle
//! fold so no candle changes.
//!
//! Until this module those packets were decoded and dropped:
//!
//! | Packet | What happened before |
//! |---|---|
//! | open interest (code 5) | counted as `non_tick`, value discarded |
//! | previous close (code 6) | close kept in RAM only, previous OI discarded |
//! | market status (code 7) | counted as `non_tick`, discarded |
//! | disconnect (code 50) | counted and logged, reason not stored |
//! | connect snapshot (exchange day ≠ receipt day) | refused before any write |
//! | tick stamped outside 09:00–15:40 | refused by the `ticks` window gate |
//!
//! ## Why a separate table and not `ticks`
//!
//! `ticks` is gated to the session window (operator, 2026-09-05) and refuses a
//! tick whose exchange day is not the receipt day (2026-09-10): a row whose
//! designated `ts` is a previous day's trade time lands in a partition for a
//! day that already closed. Here the designated `ts` is ALWAYS the receipt
//! instant, never the trade time, so a stale snapshot cannot reach an old
//! partition and nothing in `ticks` or `candles_*` changes.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS feed_aux_packets (
//!     ts TIMESTAMP,
//!     received_at TIMESTAMP,
//!     feed SYMBOL,
//!     segment SYMBOL,
//!     kind SYMBOL,
//!     contract SYMBOL,
//!     security_id LONG,
//!     oi LONG,
//!     prev_close DOUBLE,
//!     prev_oi LONG,
//!     ltp DOUBLE,
//!     exchange_ts TIMESTAMP,
//!     volume LONG,
//!     day_open DOUBLE,
//!     day_high DOUBLE,
//!     day_low DOUBLE,
//!     day_close DOUBLE,
//!     oi_day_high LONG,
//!     oi_day_low LONG,
//!     reason_code INT,
//!     capture_seq LONG
//! ) timestamp(ts) PARTITION BY HOUR WAL
//! DEDUP UPSERT KEYS(ts, security_id, segment, kind, capture_seq, feed)
//! ```
//!
//! * `ts` — receipt instant, IST nanoseconds. A frame with no recorded receipt
//!   (a pre-TVW3 WAL record) takes its capture sequence instead, which is
//!   seeded from the wall clock at capture; `received_at` is then NULL so the
//!   row says so.
//! * `capture_seq` — the same `(frame_seq, packet_index)` derivation `ticks`
//!   uses, so a WAL replay of the frame writes byte-identical keys and the
//!   second write collapses onto the first.
//! * `kind` — one of [`AuxPacketKind`].
//! * Every other column is NULL when the packet does not carry it.
//!
//! ## Path class
//!
//! Rows are appended into the `ticks` writer's ILP buffer and ride its offload
//! thread, its rescue tier and its spill files (one ILP payload can hold rows
//! for several tables). Appending is O(1) and allocation-free apart from the
//! buffer's amortised growth, which the tick rows already pay.

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, TimestampNanos};
use tracing::error;

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::IST_UTC_OFFSET_NANOS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::price_precision::{f32_to_f64_clean, round_to_2dp};
use tickvault_common::sanitize::sanitize_ilp_symbol;
use tickvault_common::segment::segment_code_to_str;
use tickvault_common::tick_types::ParsedTick;

/// QuestDB table name.
pub const FEED_AUX_PACKETS_TABLE: &str = "feed_aux_packets";

/// DEDUP UPSERT key. `ts` first (QuestDB requires the designated timestamp);
/// `security_id` + `segment` per I-P1-11; `kind` because one packet position
/// could in principle carry two kinds; `capture_seq` is the per-packet
/// tiebreaker that makes a replay idempotent; `feed` per the 2026-06-28
/// feed-in-key override.
pub const DEDUP_KEY_FEED_AUX_PACKETS: &str = "ts, security_id, segment, kind, capture_seq, feed";

/// Counter of rows appended, labelled by `kind`.
pub const FEED_AUX_ROWS_COUNTER: &str = "tv_feed_aux_rows_total";

/// Counter of packets that could not become a row (an id that does not fit the
/// `LONG` column). Unreachable for Dhan's 4-byte ids; it is the guard for a
/// wider feed.
pub const FEED_AUX_REFUSED_COUNTER: &str = "tv_feed_aux_rows_refused_total";

/// Timeout for the idempotent DDL requests.
const FEED_AUX_DDL_TIMEOUT_SECS: u64 = 10;

/// What a `feed_aux_packets` row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxPacketKind {
    /// Standalone open-interest packet (response code 5).
    OpenInterest,
    /// Previous close + previous OI packet (response code 6).
    PrevClose,
    /// Market status packet (response code 7, header only).
    MarketStatus,
    /// A tick whose exchange day is not its receipt day — the snapshot Dhan
    /// sends on connect for a contract that last traded on an earlier day
    /// (and its mirror, a forward-dated stamp). Refused by `ticks`.
    ConnectSnapshot,
    /// Server disconnect packet (response code 50); the reason is `reason_code`.
    Disconnect,
    /// A tick whose exchange time falls outside 09:00–15:40 IST (or outside
    /// the plausible band). Refused by `ticks`.
    OutOfWindowTick,
}

impl AuxPacketKind {
    /// Every kind, in index order.
    pub const ALL: [Self; 6] = [
        Self::OpenInterest,
        Self::PrevClose,
        Self::MarketStatus,
        Self::ConnectSnapshot,
        Self::Disconnect,
        Self::OutOfWindowTick,
    ];

    /// The `kind` SYMBOL value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenInterest => "oi",
            Self::PrevClose => "prev_close",
            Self::MarketStatus => "market_status",
            Self::ConnectSnapshot => "connect_snapshot",
            Self::Disconnect => "disconnect",
            Self::OutOfWindowTick => "out_of_window_tick",
        }
    }

    /// Position in [`Self::ALL`], for pre-resolved per-kind counters.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::OpenInterest => 0,
            Self::PrevClose => 1,
            Self::MarketStatus => 2,
            Self::ConnectSnapshot => 3,
            Self::Disconnect => 4,
            Self::OutOfWindowTick => 5,
        }
    }
}

/// One `feed_aux_packets` row. `Copy`, no heap. `None` columns are omitted
/// from the ILP line, so the cell is NULL.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AuxPacketRow {
    /// What this row records.
    pub kind: AuxPacketKind,
    /// I-P1-11 part 1, narrowed to the `LONG` column.
    pub security_id: i64,
    /// I-P1-11 part 2.
    pub segment: &'static str,
    /// Designated timestamp, IST nanoseconds — the receipt instant.
    pub ts_ist_nanos: i64,
    /// Receipt instant, IST nanoseconds. `None` when the frame carried none.
    pub received_at_ist_nanos: Option<i64>,
    /// Replay-stable per-packet sequence.
    pub capture_seq: i64,
    /// Open interest.
    pub oi: Option<i64>,
    /// Previous session close.
    pub prev_close: Option<f64>,
    /// Previous session open interest.
    pub prev_oi: Option<i64>,
    /// Last traded price.
    pub ltp: Option<f64>,
    /// Exchange last-trade time, IST nanoseconds.
    pub exchange_ts_ist_nanos: Option<i64>,
    /// Cumulative day volume.
    pub volume: Option<i64>,
    /// Day open.
    pub day_open: Option<f64>,
    /// Day high.
    pub day_high: Option<f64>,
    /// Day low.
    pub day_low: Option<f64>,
    /// Day close (previous session close on a Quote/Full packet).
    pub day_close: Option<f64>,
    /// Day high of open interest.
    pub oi_day_high: Option<i64>,
    /// Day low of open interest.
    pub oi_day_low: Option<i64>,
    /// Disconnect reason code.
    pub reason_code: Option<i64>,
}

/// A price column: NULL for the "not carried" `0.0` and for a non-finite
/// value (QuestDB would refuse the whole batch over a NaN), otherwise the
/// clean widening rounded to 2 dp. O(1).
#[must_use]
pub fn aux_price(v: f32) -> Option<f64> {
    (v.is_finite() && v != 0.0).then(|| round_to_2dp(f32_to_f64_clean(v)))
}

/// A quantity column: NULL for the "not carried" `0`. O(1).
#[must_use]
pub fn aux_qty(v: u32) -> Option<i64> {
    (v != 0).then(|| i64::from(v))
}

impl AuxPacketRow {
    /// A row carrying only identity and time; the caller fills the columns its
    /// packet carries.
    ///
    /// `received_at_utc_nanos` is the frame's receipt in UTC nanoseconds, `0`
    /// when unknown. Returns `None` (counted) when `security_id` does not fit
    /// the `LONG` column — never wrapped, which would alias two instruments.
    #[must_use]
    pub fn from_header(
        kind: AuxPacketKind,
        security_id: u64,
        exchange_segment_code: u8,
        received_at_utc_nanos: i64,
        capture_seq: i64,
    ) -> Option<Self> {
        let Ok(id) = i64::try_from(security_id) else {
            metrics::counter!(FEED_AUX_REFUSED_COUNTER, "reason" => "security_id_width")
                .increment(1);
            error!(
                code = ErrorCode::StorageGapTickDedupSegment.code_str(),
                raw_security_id = security_id,
                kind = kind.as_str(),
                "feed_aux_packets row refused — security_id does not fit the LONG \
                 column; narrowing it would alias two instruments onto one key"
            );
            return None;
        };
        let received_at_ist_nanos = (received_at_utc_nanos != 0)
            .then(|| received_at_utc_nanos.saturating_add(IST_UTC_OFFSET_NANOS));
        // No receipt: the capture sequence is seeded from the wall clock (UTC
        // nanoseconds) at capture, so it is the nearest real time this packet
        // has. `received_at` stays NULL so the fallback is visible.
        let ts_ist_nanos = received_at_ist_nanos
            .unwrap_or_else(|| capture_seq.saturating_add(IST_UTC_OFFSET_NANOS));
        Some(Self {
            kind,
            security_id: id,
            segment: segment_code_to_str(exchange_segment_code),
            ts_ist_nanos,
            received_at_ist_nanos,
            capture_seq,
            oi: None,
            prev_close: None,
            prev_oi: None,
            ltp: None,
            exchange_ts_ist_nanos: None,
            volume: None,
            day_open: None,
            day_high: None,
            day_low: None,
            day_close: None,
            oi_day_high: None,
            oi_day_low: None,
            reason_code: None,
        })
    }

    /// A row carrying a whole tick: the price, trade time, volume, day OHLC and
    /// open-interest fields the packet carried. Used for the two tick shapes
    /// `ticks` refuses (a connect snapshot and an out-of-window tick).
    #[must_use]
    pub fn from_tick(kind: AuxPacketKind, tick: &ParsedTick, capture_seq: i64) -> Option<Self> {
        let mut row = Self::from_header(
            kind,
            tick.security_id,
            tick.exchange_segment_code,
            tick.received_at_nanos,
            capture_seq,
        )?;
        row.ltp = aux_price(tick.last_traded_price);
        // Dhan's LTT is already IST epoch seconds.
        row.exchange_ts_ist_nanos = (tick.exchange_timestamp != 0)
            .then(|| i64::from(tick.exchange_timestamp).saturating_mul(1_000_000_000));
        row.volume = aux_qty(tick.volume);
        row.day_open = aux_price(tick.day_open);
        row.day_high = aux_price(tick.day_high);
        row.day_low = aux_price(tick.day_low);
        row.day_close = aux_price(tick.day_close);
        row.oi = aux_qty(tick.open_interest);
        row.oi_day_high = aux_qty(tick.oi_day_high);
        row.oi_day_low = aux_qty(tick.oi_day_low);
        Some(row)
    }
}

/// Appends one row to an ILP buffer (no flush). Symbols first, as ILP
/// requires; `None` columns omitted (NULL).
///
/// # Errors
/// Propagates ILP buffer errors.
pub(crate) fn append_aux_row(buffer: &mut Buffer, feed: &str, row: &AuxPacketRow) -> Result<()> {
    buffer
        .table(FEED_AUX_PACKETS_TABLE)
        .context("table")?
        .symbol("segment", sanitize_ilp_symbol(row.segment).as_ref())
        .context("segment")?
        .symbol("feed", sanitize_ilp_symbol(feed).as_ref())
        .context("feed")?
        .symbol("kind", sanitize_ilp_symbol(row.kind.as_str()).as_ref())
        .context("kind")?;
    let labels = crate::candle_contract_labels::candle_contract_labels();
    if let Some(name) = labels.get(&(row.security_id, row.segment)) {
        buffer
            .symbol("contract", sanitize_ilp_symbol(name).as_ref())
            .context("contract")?;
    }
    buffer
        .column_i64("security_id", row.security_id)
        .context("security_id")?;
    if let Some(v) = row.oi {
        buffer.column_i64("oi", v).context("oi")?;
    }
    if let Some(v) = row.prev_close {
        buffer.column_f64("prev_close", v).context("prev_close")?;
    }
    if let Some(v) = row.prev_oi {
        buffer.column_i64("prev_oi", v).context("prev_oi")?;
    }
    if let Some(v) = row.ltp {
        buffer.column_f64("ltp", v).context("ltp")?;
    }
    if let Some(v) = row.exchange_ts_ist_nanos {
        buffer
            .column_ts("exchange_ts", TimestampNanos::new(v))
            .context("exchange_ts")?;
    }
    if let Some(v) = row.volume {
        buffer.column_i64("volume", v).context("volume")?;
    }
    if let Some(v) = row.day_open {
        buffer.column_f64("day_open", v).context("day_open")?;
    }
    if let Some(v) = row.day_high {
        buffer.column_f64("day_high", v).context("day_high")?;
    }
    if let Some(v) = row.day_low {
        buffer.column_f64("day_low", v).context("day_low")?;
    }
    if let Some(v) = row.day_close {
        buffer.column_f64("day_close", v).context("day_close")?;
    }
    if let Some(v) = row.oi_day_high {
        buffer.column_i64("oi_day_high", v).context("oi_day_high")?;
    }
    if let Some(v) = row.oi_day_low {
        buffer.column_i64("oi_day_low", v).context("oi_day_low")?;
    }
    if let Some(v) = row.reason_code {
        buffer.column_i64("reason_code", v).context("reason_code")?;
    }
    if let Some(v) = row.received_at_ist_nanos {
        buffer
            .column_ts("received_at", TimestampNanos::new(v))
            .context("received_at")?;
    }
    buffer
        .column_i64("capture_seq", row.capture_seq)
        .context("capture_seq")?
        .at(TimestampNanos::new(row.ts_ist_nanos))
        .context("designated timestamp")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// The idempotent `CREATE TABLE` DDL. Pure.
#[must_use]
pub fn feed_aux_create_ddl() -> String {
    // APPROVED: table DDL, built once at boot in feed_aux_create_ddl
    format!(
        "CREATE TABLE IF NOT EXISTS {FEED_AUX_PACKETS_TABLE} (\
            ts TIMESTAMP, \
            received_at TIMESTAMP, \
            feed SYMBOL, \
            segment SYMBOL, \
            kind SYMBOL, \
            contract SYMBOL, \
            security_id LONG, \
            oi LONG, \
            prev_close DOUBLE, \
            prev_oi LONG, \
            ltp DOUBLE, \
            exchange_ts TIMESTAMP, \
            volume LONG, \
            day_open DOUBLE, \
            day_high DOUBLE, \
            day_low DOUBLE, \
            day_close DOUBLE, \
            oi_day_high LONG, \
            oi_day_low LONG, \
            reason_code INT, \
            capture_seq LONG\
        ) TIMESTAMP(ts) PARTITION BY HOUR WAL"
    )
}

/// Every non-designated column with its type, for the per-column self-heal.
const FEED_AUX_COLUMNS: &[(&str, &str)] = &[
    ("received_at", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("segment", "SYMBOL"),
    ("kind", "SYMBOL"),
    ("contract", "SYMBOL"),
    ("security_id", "LONG"),
    ("oi", "LONG"),
    ("prev_close", "DOUBLE"),
    ("prev_oi", "LONG"),
    ("ltp", "DOUBLE"),
    ("exchange_ts", "TIMESTAMP"),
    ("volume", "LONG"),
    ("day_open", "DOUBLE"),
    ("day_high", "DOUBLE"),
    ("day_low", "DOUBLE"),
    ("day_close", "DOUBLE"),
    ("oi_day_high", "LONG"),
    ("oi_day_low", "LONG"),
    ("reason_code", "INT"),
    ("capture_seq", "LONG"),
];

/// The ordered statements [`ensure_feed_aux_table`] issues: CREATE → per-column
/// `ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`. Never a DROP.
#[must_use]
pub fn feed_aux_ensure_statements() -> Vec<String> {
    let mut out = vec![feed_aux_create_ddl()]; // APPROVED: boot DDL statement list, never per row
    for (col, ty) in FEED_AUX_COLUMNS {
        // APPROVED: per-column ADD COLUMN, boot schema self-heal
        out.push(format!(
            "ALTER TABLE {FEED_AUX_PACKETS_TABLE} ADD COLUMN IF NOT EXISTS {col} {ty}"
        ));
    }
    // APPROVED: DEDUP ENABLE statement, boot only
    out.push(format!(
        "ALTER TABLE {FEED_AUX_PACKETS_TABLE} DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_FEED_AUX_PACKETS})"
    ));
    out
}

/// Idempotently self-heals the table (CREATE → ADD COLUMN → DEDUP ENABLE).
/// Returns `true` only when every statement was accepted; the boot retries on
/// `false` (`candle_ddl_boot::run_live_table_ddl_at_boot`).
// TEST-EXEMPT: live-QuestDB DDL runner; the statement set is unit-tested via
// feed_aux_ensure_statements(), and the unreachable arm by the tokio test below.
#[must_use = "a false verdict means the DEDUP key may be missing; retry it"]
pub async fn ensure_feed_aux_table(questdb_config: &QuestDbConfig) -> bool {
    // APPROVED: QuestDB base URL, once per ensure at boot
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(FEED_AUX_DDL_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                stage = "feed_aux_ensure_client_build",
                ?err,
                "feed_aux_packets table not ensured — HTTP client build failed; the \
                 first ILP write may auto-create it WITHOUT its DEDUP key"
            );
            return false;
        }
    };
    let mut every_statement_accepted = true;
    for ddl in &feed_aux_ensure_statements() {
        match client
            .get(&base_url)
            .query(&[("query", ddl.as_str())])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    stage = "feed_aux_ensure_ddl",
                    %status,
                    ddl = ddl.as_str(),
                    body = %body.chars().take(200).collect::<String>(),
                    "feed_aux_packets DDL returned non-2xx — the DEDUP key may be missing"
                );
                every_statement_accepted = false;
            }
            Err(err) => {
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    stage = "feed_aux_ensure_ddl",
                    ?err,
                    ddl = ddl.as_str(),
                    "feed_aux_packets DDL request failed"
                );
                every_statement_accepted = false;
            }
        }
    }
    every_statement_accepted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick() -> ParsedTick {
        ParsedTick {
            security_id: 52_175,
            exchange_segment_code: 2,
            last_traded_price: 142.5,
            exchange_timestamp: 1_779_300_000,
            received_at_nanos: 1_779_350_000_000_000_000,
            volume: 1_200,
            day_open: 140.0,
            day_high: 150.25,
            day_low: 139.0,
            day_close: 141.0,
            open_interest: 9_000,
            oi_day_high: 9_500,
            oi_day_low: 8_000,
            ..Default::default()
        }
    }

    fn line(row: &AuxPacketRow) -> String {
        let mut buf = Buffer::new(questdb::ingress::ProtocolVersion::V1);
        append_aux_row(&mut buf, "dhan", row).expect("append");
        String::from_utf8(buf.as_bytes().to_vec()).expect("utf8")
    }

    #[test]
    fn test_dedup_key_feed_aux_packets_has_ts_first_segment_kind_seq_and_feed() {
        let cols: Vec<&str> = DEDUP_KEY_FEED_AUX_PACKETS.split(", ").collect();
        assert_eq!(cols[0], "ts");
        for c in ["security_id", "segment", "kind", "capture_seq", "feed"] {
            assert!(cols.contains(&c), "{c} missing from {cols:?}");
        }
    }

    #[test]
    fn test_aux_packet_kind_as_str_and_index_are_distinct_and_aligned() {
        for (i, k) in AuxPacketKind::ALL.iter().enumerate() {
            assert_eq!(k.index(), i);
        }
        let names: std::collections::BTreeSet<&str> =
            AuxPacketKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(names.len(), AuxPacketKind::ALL.len());
    }

    #[test]
    fn test_aux_price_nulls_zero_and_non_finite_and_widens_cleanly() {
        assert_eq!(aux_price(0.0), None);
        assert_eq!(aux_price(f32::NAN), None);
        assert_eq!(aux_price(f32::INFINITY), None);
        assert_eq!(aux_price(23_925.65), Some(23_925.65));
    }

    #[test]
    fn test_aux_qty_nulls_zero() {
        assert_eq!(aux_qty(0), None);
        assert_eq!(aux_qty(7), Some(7));
    }

    #[test]
    fn test_from_header_stamps_receipt_as_designated_ts() {
        let row =
            AuxPacketRow::from_header(AuxPacketKind::MarketStatus, 13, 0, 1_000, 77).expect("fits");
        assert_eq!(row.ts_ist_nanos, 1_000 + IST_UTC_OFFSET_NANOS);
        assert_eq!(
            row.received_at_ist_nanos,
            Some(1_000 + IST_UTC_OFFSET_NANOS)
        );
        assert_eq!(row.segment, "IDX_I");
        assert_eq!(row.capture_seq, 77);
    }

    #[test]
    fn test_from_header_without_receipt_falls_back_to_capture_seq_and_nulls_received_at() {
        let row =
            AuxPacketRow::from_header(AuxPacketKind::OpenInterest, 13, 2, 0, 5_000).expect("fits");
        assert_eq!(row.ts_ist_nanos, 5_000 + IST_UTC_OFFSET_NANOS);
        assert_eq!(row.received_at_ist_nanos, None);
    }

    #[test]
    fn test_from_header_refuses_an_id_wider_than_long() {
        assert!(
            AuxPacketRow::from_header(AuxPacketKind::OpenInterest, u64::MAX, 2, 1, 1).is_none()
        );
    }

    #[test]
    fn test_from_tick_carries_trade_fields_and_never_the_trade_time_as_ts() {
        let t = tick();
        let row = AuxPacketRow::from_tick(AuxPacketKind::ConnectSnapshot, &t, 9).expect("fits");
        assert_eq!(row.ts_ist_nanos, t.received_at_nanos + IST_UTC_OFFSET_NANOS);
        assert_eq!(
            row.exchange_ts_ist_nanos,
            Some(i64::from(t.exchange_timestamp) * 1_000_000_000)
        );
        assert_eq!(row.ltp, Some(142.5));
        assert_eq!(row.volume, Some(1_200));
        assert_eq!(row.oi_day_high, Some(9_500));
        assert_eq!(row.oi_day_low, Some(8_000));
        assert_eq!(row.day_high, Some(150.25));
    }

    #[test]
    fn test_append_aux_row_writes_symbols_first_and_omits_absent_columns() {
        let mut row =
            AuxPacketRow::from_header(AuxPacketKind::PrevClose, 13, 0, 1_000, 3).expect("fits");
        row.prev_close = Some(24_500.5);
        row.prev_oi = Some(42);
        let l = line(&row);
        assert!(
            l.starts_with("feed_aux_packets,segment=IDX_I,feed=dhan,kind=prev_close "),
            "{l}"
        );
        assert!(l.contains("prev_close=24500.5"), "{l}");
        assert!(l.contains("prev_oi=42i"), "{l}");
        assert!(l.contains("capture_seq=3i"), "{l}");
        for absent in ["ltp=", "oi=", "volume=", "reason_code=", "exchange_ts="] {
            assert!(
                !l.contains(&format!(",{absent}")),
                "{absent} must be NULL: {l}"
            );
        }
        assert!(
            l.trim_end()
                .ends_with(&(1_000 + IST_UTC_OFFSET_NANOS).to_string())
        );
    }

    #[test]
    fn test_feed_aux_create_ddl_is_hour_partitioned_wal() {
        let ddl = feed_aux_create_ddl();
        assert!(ddl.starts_with("CREATE TABLE IF NOT EXISTS feed_aux_packets ("));
        assert!(ddl.ends_with("TIMESTAMP(ts) PARTITION BY HOUR WAL"));
        for (col, ty) in FEED_AUX_COLUMNS {
            assert!(ddl.contains(&format!("{col} {ty}")), "{col} {ty} missing");
        }
    }

    #[test]
    fn test_feed_aux_ensure_statements_are_create_then_alter_then_dedup() {
        let s = feed_aux_ensure_statements();
        assert_eq!(s.len(), FEED_AUX_COLUMNS.len() + 2);
        assert!(s[0].starts_with("CREATE TABLE IF NOT EXISTS"));
        assert!(
            s[1..s.len() - 1]
                .iter()
                .all(|x| x.contains("ADD COLUMN IF NOT EXISTS"))
        );
        assert_eq!(
            s[s.len() - 1],
            format!(
                "ALTER TABLE feed_aux_packets DEDUP ENABLE UPSERT KEYS({DEDUP_KEY_FEED_AUX_PACKETS})"
            )
        );
        assert!(s.iter().all(|x| !x.contains("DROP")));
    }

    /// These rows are received market data (zero-loss, 2026-09-29): no reset
    /// or wipe list may name the table.
    #[test]
    fn test_feed_aux_packets_is_never_on_a_reset_list() {
        assert!(!crate::fresh_start_reset::RESET_TABLES.contains(&FEED_AUX_PACKETS_TABLE));
        assert!(!crate::fresh_start_reset::RESET_VIEWS.contains(&FEED_AUX_PACKETS_TABLE));
    }

    #[tokio::test]
    async fn test_ensure_feed_aux_table_reports_false_when_questdb_is_unreachable() {
        let cfg = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        assert!(!ensure_feed_aux_table(&cfg).await);
    }
}
