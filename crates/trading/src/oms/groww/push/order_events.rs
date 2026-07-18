//! Full-fidelity push-event CAPTURE builders + the bounded producer sink
//! (`ORDER-EVT-01` subsystem, 2026-07-18).
//!
//! This is the third, ADDITIVE lane at the push decode sites: the lossy
//! 11-field [`BrokerOrderEvent`](tickvault_common::broker_order_events::BrokerOrderEvent)
//! fan-out and the hint lane are UNTOUCHED. Here every decoded field of the
//! ORDER and POSITION push DTOs is mapped one-to-one onto the
//! `order_update_events` / `position_update_events` capture records
//! (`.claude/rules/project/order-update-events-error-codes.md` §2).
//!
//! # Contract
//! - **Pure + total builders:** unknown enum values degrade to
//!   raw-preserving `GROWW_PUSH_{PREFIX}_{raw}` labels (annexure rule 15 —
//!   never guess, never lose the wire value); absent submessages degrade to
//!   the documented sentinels (`-1` numeric / empty string), absent position
//!   legs stay `None` (→ NULL, never a fabricated `0.0`).
//! - **proto3 zeros are persisted verbatim** — a served `0` and an absent
//!   scalar are indistinguishable on the wire, so the capture records what
//!   the broker DELIVERED (the honest-envelope rule; the decision seam's
//!   `Option` degrades live in the OTHER lane, untouched).
//! - **Groww paise → rupees at construction** (the table columns are
//!   DOUBLE rupees; the Dhan wire already delivers f64 rupees).
//! - **Never blocks the push read loop:** [`GrowwPushCapture`] is
//!   `try_send`-only; a refused send is counted
//!   (`tv_order_update_events_dropped_total{reason}`) + coded
//!   (`ORDER-EVT-01`, stage `sink_drop`) and the event's capture row is
//!   honestly LOST (best-effort forensics — the design's delivery boundary).
//!
//! COLD-PATH: these builders run per push event (a handful per session),
//! never on the tick hot path.

use tickvault_common::broker_order_events::{
    BrokerOrderStatus, OrderUpdateEventRecord, PositionUpdateEventRecord, next_event_seq,
};
use tickvault_common::constants::IST_UTC_OFFSET_NANOS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tokio::sync::mpsc;
use tracing::error;

use super::proto::{
    ExchangePosition, OrderDetailsBroadCastDto, PositionDetailProto, buy_sell_name, duration_name,
    equity_type_name, exchange_name, order_status_name, order_type_name, position_exchange_name,
    product_name, segment_name, underlying_asset_name,
};

/// Sentinel for numeric fields the Groww push wire does not carry.
const ABSENT_I64: i64 = -1;

/// Receipt instant in IST epoch NANOS (the capture tables' designated `ts`).
///
/// `SystemTime` UTC nanos + the IST offset — the same UTC→IST conversion rule
/// as `received_at` in `data-integrity.md` (the WebSocket LTT no-offset rule
/// does not apply: this is a receipt stamp from the system clock, not an
/// exchange timestamp).
pub(crate) fn now_ist_epoch_nanos() -> i64 {
    let utc_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0_i64, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    utc_nanos.saturating_add(IST_UTC_OFFSET_NANOS)
}

/// Raw-preserving label for an enum value: the mapped name when known, else
/// `GROWW_PUSH_{prefix}_{raw}` (the order_mapper fallback convention).
fn enum_label(mapped: Option<&'static str>, prefix: &str, raw: i32) -> String {
    mapped.map_or_else(|| format!("GROWW_PUSH_{prefix}_{raw}"), str::to_owned)
}

/// Groww integer paise → rupees for the DOUBLE table columns.
#[allow(clippy::cast_precision_loss)] // APPROVED: capture DOUBLE column; paise magnitudes ≪ 2^52
fn paise_to_rupees(paise: i64) -> f64 {
    paise as f64 / 100.0
}

/// Build the full-fidelity ORDER capture record from a decoded push
/// broadcast. Returns `None` for a stage-marker-only frame (no
/// `orderDetailUpdateDto` — no order identity to capture).
///
/// `ts_ist_nanos` is caller-stamped at the decode instant so this stays pure.
#[must_use]
pub fn build_groww_order_event_record(
    dto: &OrderDetailsBroadCastDto,
    ts_ist_nanos: i64,
) -> Option<OrderUpdateEventRecord> {
    let detail = dto.order_detail.as_ref()?;
    let status = order_status_name(detail.order_status).map_or(
        BrokerOrderStatus::Unknown,
        BrokerOrderStatus::from_groww_status,
    );
    // Groww carries no dedicated reject-reason field; `remark` is the Assumed
    // carrier on a reject-class status (UNVERIFIED-LIVE — rule file §2).
    let reject_reason = if matches!(
        status,
        BrokerOrderStatus::Rejected | BrokerOrderStatus::Failed
    ) {
        detail.remark.clone()
    } else {
        String::new()
    };
    let stage_trail = dto
        .stage_and_time_stamp
        .iter()
        .map(|s| format!("{}@{}", s.stage_name, s.time_stamp_from_midnight))
        .collect::<Vec<_>>()
        .join("|");
    let detail_raw = format!(
        "order_status={} duration={} exchange={} segment={} product={} order_type={} \
         buy_sell={} qty={} price_paise={} trigger_price_paise={} filled_qty={} \
         remaining_qty={} avg_fill_price_paise={}",
        detail.order_status,
        detail.duration,
        detail.exchange,
        detail.segment,
        detail.product,
        detail.order_type,
        detail.buy_sell,
        detail.qty,
        detail.price_paise,
        detail.trigger_price_paise,
        detail.filled_qty,
        detail.remaining_qty,
        detail.avg_fill_price_paise,
    );
    Some(OrderUpdateEventRecord {
        ts_ist_nanos,
        feed: Feed::Groww,
        event_seq: next_event_seq(),
        order_id: detail.groww_order_id.clone(),
        exch_order_id: detail.exchange_order_id.clone(),
        // The push wire carries no user reference id — honestly empty.
        correlation_id: String::new(),
        status: status.as_str().to_owned(),
        raw_status: enum_label(
            order_status_name(detail.order_status),
            "STATUS",
            detail.order_status,
        ),
        leg_no: -1,
        product: enum_label(product_name(detail.product), "PRODUCT", detail.product),
        order_type: enum_label(
            order_type_name(detail.order_type),
            "ORDER_TYPE",
            detail.order_type,
        ),
        transaction_type: enum_label(buy_sell_name(detail.buy_sell), "BUY_SELL", detail.buy_sell),
        validity: enum_label(duration_name(detail.duration), "DURATION", detail.duration),
        exchange: enum_label(exchange_name(detail.exchange), "EXCHANGE", detail.exchange),
        exchange_segment: enum_label(segment_name(detail.segment), "SEGMENT", detail.segment),
        // The push DTO carries no resolvable Dhan-space id — honest sentinel.
        security_id: ABSENT_I64,
        symbol: detail.contract_id.clone(),
        quantity: i64::from(detail.qty),
        disclosed_qty: ABSENT_I64,
        remaining_qty: i64::from(detail.remaining_qty),
        traded_qty: i64::from(detail.filled_qty),
        price: paise_to_rupees(detail.price_paise),
        trigger_price: paise_to_rupees(detail.trigger_price_paise),
        avg_traded_price: paise_to_rupees(detail.avg_fill_price_paise),
        last_traded_price: 0.0,
        reject_reason,
        remarks: detail.remark.clone(),
        source: "push".to_owned(),
        off_mkt_flag: String::new(),
        opt_type: String::new(),
        algo_ord_no: String::new(),
        algo_id: String::new(),
        mkt_type: String::new(),
        series: String::new(),
        good_till_days_date: String::new(),
        ref_ltp: 0.0,
        tick_size: 0.0,
        multiplier: -1,
        instrument: String::new(),
        broker_create_time: String::new(),
        broker_update_time: String::new(),
        exchange_time: String::new(),
        // `timeStampFromMidNight` semantics UNVERIFIED-LIVE — never fabricated
        // into an epoch timestamp (mirrors the seam mapper's rule).
        exchange_ts_ms: ABSENT_I64,
        contract_id: detail.contract_id.clone(),
        gui_order_id: detail.gui_order_id.clone(),
        stage_trail,
        detail_raw,
    })
}

/// Leg tuple accessor: `(credit_qty, credit_price, debit_qty, debit_price)`.
fn leg_fields(
    leg: Option<&ExchangePosition>,
) -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
    leg.map_or((None, None, None, None), |l| {
        (
            Some(l.credit_qty),
            Some(l.credit_price),
            Some(l.debit_qty),
            Some(l.debit_price),
        )
    })
}

/// Build the full-fidelity POSITION capture record from a decoded push
/// payload. ALWAYS returns a record — an all-absent payload still evidences
/// a received event (sentinels/NULLs, never fabricated values).
///
/// `ts_ist_nanos` is caller-stamped at the decode instant so this stays pure.
#[must_use]
pub fn build_groww_position_event_record(
    pd: &PositionDetailProto,
    ts_ist_nanos: i64,
) -> PositionUpdateEventRecord {
    let sym = pd.symbol_data.as_ref();
    let (nse_credit_qty, nse_credit_price, nse_debit_qty, nse_debit_price) =
        leg_fields(pd.position_info.as_ref().and_then(|p| p.nse.as_ref()));
    let (bse_credit_qty, bse_credit_price, bse_debit_qty, bse_debit_price) =
        leg_fields(pd.position_info.as_ref().and_then(|p| p.bse.as_ref()));
    let detail_raw = sym.map_or_else(String::new, |s| {
        format!(
            "stocks_product={} equity_type={} underlying_asset_type={} exchange={}",
            s.stocks_product, s.equity_type, s.underlying_asset_type, s.exchange
        )
    });
    PositionUpdateEventRecord {
        ts_ist_nanos,
        feed: Feed::Groww,
        event_seq: next_event_seq(),
        symbol_isin: pd
            .position_info
            .as_ref()
            .map_or_else(String::new, |p| p.symbol_isin.clone()),
        // No resolvable Dhan-space id on the position wire — honest sentinel.
        security_id: ABSENT_I64,
        // The position push subject is the stocks_fo channel — FNO by
        // construction (the subject grammar; no segment field on the wire).
        exchange_segment: "FNO".to_owned(),
        symbol: sym.map_or_else(String::new, |s| {
            if s.display_name.is_empty() {
                s.contract_id.clone()
            } else {
                s.display_name.clone()
            }
        }),
        exchange: sym.map_or_else(String::new, |s| {
            enum_label(position_exchange_name(s.exchange), "EXCHANGE", s.exchange)
        }),
        tr_time_stamp: sym.map_or(ABSENT_I64, |s| s.tr_time_stamp),
        search_id: sym.map_or_else(String::new, |s| s.search_id.clone()),
        product: sym.map_or_else(String::new, |s| {
            enum_label(product_name(s.stocks_product), "PRODUCT", s.stocks_product)
        }),
        contract_id: sym.map_or_else(String::new, |s| s.contract_id.clone()),
        equity_type: sym.map_or_else(String::new, |s| {
            enum_label(
                equity_type_name(s.equity_type),
                "EQUITY_TYPE",
                s.equity_type,
            )
        }),
        display_name: sym.map_or_else(String::new, |s| s.display_name.clone()),
        underlying_id: sym.map_or_else(String::new, |s| s.underlying_id.clone()),
        nse_market_lot: sym.map_or(ABSENT_I64, |s| s.nse_market_lot),
        bse_market_lot: sym.map_or(ABSENT_I64, |s| s.bse_market_lot),
        underlying_asset_type: sym.map_or_else(String::new, |s| {
            enum_label(
                underlying_asset_name(s.underlying_asset_type),
                "ASSET",
                s.underlying_asset_type,
            )
        }),
        freeze_qty: sym.map_or(ABSENT_I64, |s| s.freeze_qty),
        nse_credit_qty,
        nse_credit_price,
        nse_debit_qty,
        nse_debit_price,
        bse_credit_qty,
        bse_credit_price,
        bse_debit_qty,
        bse_debit_price,
        detail_raw,
    }
}

/// Bounded, non-blocking producer sink for the capture records.
///
/// Cloned into the push runner; `None` channels = capture disabled (the
/// `[order_update_events]` gate is OFF or the consumer never spawned) — every
/// publish is then a silent no-op, byte-identical to the pre-capture
/// behaviour. A refused `try_send` on a LIVE channel is counted + coded
/// (`ORDER-EVT-01` stage `sink_drop`) and never blocks the read loop.
#[derive(Clone, Default)]
pub struct GrowwPushCapture {
    orders: Option<mpsc::Sender<OrderUpdateEventRecord>>,
    positions: Option<mpsc::Sender<PositionUpdateEventRecord>>,
}

impl GrowwPushCapture {
    /// Live capture sink feeding the app-side consumer's two channels.
    #[must_use]
    pub fn new(
        orders: mpsc::Sender<OrderUpdateEventRecord>,
        positions: mpsc::Sender<PositionUpdateEventRecord>,
    ) -> Self {
        Self {
            orders: Some(orders),
            positions: Some(positions),
        }
    }

    /// Disabled sink — every publish is a no-op (capture gate OFF).
    #[must_use]
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Publish an ORDER capture record (best-effort, never blocking).
    pub fn publish_order(&self, record: OrderUpdateEventRecord) {
        let Some(tx) = self.orders.as_ref() else {
            return;
        };
        if let Err(err) = tx.try_send(record) {
            record_sink_drop("order", err_reason(&err));
        }
    }

    /// Publish a POSITION capture record (best-effort, never blocking).
    pub fn publish_position(&self, record: PositionUpdateEventRecord) {
        let Some(tx) = self.positions.as_ref() else {
            return;
        };
        if let Err(err) = tx.try_send(record) {
            record_sink_drop("position", err_reason(&err));
        }
    }
}

/// Static drop-reason label for a refused `try_send`.
fn err_reason<T>(err: &mpsc::error::TrySendError<T>) -> &'static str {
    match err {
        mpsc::error::TrySendError::Full(_) => "full",
        mpsc::error::TrySendError::Closed(_) => "closed",
    }
}

/// Count + code one refused capture publish (ORDER-EVT-01 `sink_drop`).
fn record_sink_drop(channel: &'static str, reason: &'static str) {
    metrics::counter!("tv_order_update_events_dropped_total", "reason" => reason).increment(1);
    error!(
        code = ErrorCode::OrderEvt01PersistFailed.code_str(),
        stage = "sink_drop",
        feed = "groww",
        channel,
        reason,
        "push-event capture row dropped ({channel}, {reason}) — best-effort \
         forensics; the push read loop and the decision seam are unaffected"
    );
}

#[cfg(test)]
mod tests {
    use super::super::proto::{OrderDetailUpdate, PositionInfo, StageAndTimeStamp, SymbolInfo};
    use super::*;

    fn sample_detail() -> OrderDetailUpdate {
        OrderDetailUpdate {
            qty: 50,
            price_paise: 1_234_500,
            trigger_price_paise: 1_200_000,
            filled_qty: 20,
            remaining_qty: 30,
            avg_fill_price_paise: 1_234_400,
            groww_order_id: "GMK1234567890".to_owned(),
            exchange_order_id: "1100000012345678".to_owned(),
            order_status: 6, // EXECUTED
            duration: 1,     // DAY
            exchange: 1,     // NSE
            segment: 1,      // FNO
            product: 1,      // MIS
            order_type: 1,   // L
            buy_sell: 0,     // B
            remark: "ok".to_owned(),
            contract_id: "NIFTY25JAN25000CE".to_owned(),
            gui_order_id: "gui-1".to_owned(),
        }
    }

    fn sample_dto() -> OrderDetailsBroadCastDto {
        OrderDetailsBroadCastDto {
            stage_and_time_stamp: vec![
                StageAndTimeStamp {
                    time_stamp_from_midnight: 34_200_000.0,
                    stage_name: "ACKED".to_owned(),
                },
                StageAndTimeStamp {
                    time_stamp_from_midnight: 34_200_500.0,
                    stage_name: "EXECUTED".to_owned(),
                },
            ],
            order_detail: Some(sample_detail()),
        }
    }

    fn sample_symbol_info() -> SymbolInfo {
        SymbolInfo {
            tr_time_stamp: 1_752_800_000,
            search_id: "nifty-25000-ce".to_owned(),
            stocks_product: 1, // MIS
            contract_id: "NIFTY25JAN25000CE".to_owned(),
            equity_type: 2, // OPTION
            display_name: "NIFTY 25000 CE".to_owned(),
            underlying_id: "NIFTY".to_owned(),
            nse_market_lot: 75,
            bse_market_lot: 0,
            underlying_asset_type: 1, // EQUITY_ASSET_INDICES
            freeze_qty: 1800,
            exchange: 1, // NSE
        }
    }

    #[test]
    fn test_build_groww_order_event_record_maps_every_detail_field() {
        let dto = sample_dto();
        let rec = build_groww_order_event_record(&dto, 123).unwrap_or_else(|| {
            panic!("record expected");
        });
        assert_eq!(rec.ts_ist_nanos, 123);
        assert_eq!(rec.feed, Feed::Groww);
        assert!(rec.event_seq >= 1);
        assert_eq!(rec.order_id, "GMK1234567890");
        assert_eq!(rec.exch_order_id, "1100000012345678");
        assert_eq!(rec.correlation_id, "");
        assert_eq!(rec.status, "FILLED");
        assert_eq!(rec.raw_status, "EXECUTED");
        assert_eq!(rec.leg_no, -1);
        assert_eq!(rec.product, "MIS");
        assert_eq!(rec.order_type, "L");
        assert_eq!(rec.transaction_type, "B");
        assert_eq!(rec.validity, "DAY");
        assert_eq!(rec.exchange, "NSE");
        assert_eq!(rec.exchange_segment, "FNO");
        assert_eq!(rec.security_id, -1);
        assert_eq!(rec.symbol, "NIFTY25JAN25000CE");
        assert_eq!(rec.quantity, 50);
        assert_eq!(rec.disclosed_qty, -1);
        assert_eq!(rec.remaining_qty, 30);
        assert_eq!(rec.traded_qty, 20);
        assert!((rec.price - 12_345.0).abs() < 1e-9);
        assert!((rec.trigger_price - 12_000.0).abs() < 1e-9);
        assert!((rec.avg_traded_price - 12_344.0).abs() < 1e-9);
        assert!((rec.last_traded_price - 0.0).abs() < f64::EPSILON);
        // EXECUTED is not a reject-class status → reject_reason stays empty.
        assert_eq!(rec.reject_reason, "");
        assert_eq!(rec.remarks, "ok");
        assert_eq!(rec.source, "push");
        assert_eq!(rec.off_mkt_flag, "");
        assert_eq!(rec.opt_type, "");
        assert_eq!(rec.algo_ord_no, "");
        assert_eq!(rec.algo_id, "");
        assert_eq!(rec.mkt_type, "");
        assert_eq!(rec.series, "");
        assert_eq!(rec.good_till_days_date, "");
        assert!((rec.ref_ltp - 0.0).abs() < f64::EPSILON);
        assert!((rec.tick_size - 0.0).abs() < f64::EPSILON);
        assert_eq!(rec.multiplier, -1);
        assert_eq!(rec.instrument, "");
        assert_eq!(rec.broker_create_time, "");
        assert_eq!(rec.broker_update_time, "");
        assert_eq!(rec.exchange_time, "");
        assert_eq!(rec.exchange_ts_ms, -1);
        assert_eq!(rec.contract_id, "NIFTY25JAN25000CE");
        assert_eq!(rec.gui_order_id, "gui-1");
        assert_eq!(rec.stage_trail, "ACKED@34200000|EXECUTED@34200500");
        assert!(rec.detail_raw.contains("order_status=6"));
        assert!(rec.detail_raw.contains("price_paise=1234500"));
        assert!(rec.detail_raw.contains("buy_sell=0"));
    }

    #[test]
    fn test_build_groww_order_event_record_stage_marker_only_frame_is_none() {
        let dto = OrderDetailsBroadCastDto {
            stage_and_time_stamp: vec![StageAndTimeStamp {
                time_stamp_from_midnight: 1.0,
                stage_name: "ACKED".to_owned(),
            }],
            order_detail: None,
        };
        assert!(build_groww_order_event_record(&dto, 1).is_none());
    }

    #[test]
    fn reject_class_status_carries_remark_as_reject_reason() {
        let mut dto = sample_dto();
        if let Some(d) = dto.order_detail.as_mut() {
            d.order_status = 4; // REJECTED
            d.remark = "margin shortfall".to_owned();
        }
        let rec = build_groww_order_event_record(&dto, 1).unwrap_or_else(|| {
            panic!("record expected");
        });
        assert_eq!(rec.status, "REJECTED");
        assert_eq!(rec.reject_reason, "margin shortfall");
        assert_eq!(rec.remarks, "margin shortfall");
    }

    #[test]
    fn unknown_enums_degrade_to_raw_preserving_labels() {
        let mut dto = sample_dto();
        if let Some(d) = dto.order_detail.as_mut() {
            d.order_status = 99;
            d.segment = 7;
            d.product = 42;
            d.exchange = 9;
            d.order_type = 8;
            d.buy_sell = 5;
            d.duration = 6;
        }
        let rec = build_groww_order_event_record(&dto, 1).unwrap_or_else(|| {
            panic!("record expected");
        });
        assert_eq!(rec.status, "UNKNOWN");
        assert_eq!(rec.raw_status, "GROWW_PUSH_STATUS_99");
        assert_eq!(rec.exchange_segment, "GROWW_PUSH_SEGMENT_7");
        assert_eq!(rec.product, "GROWW_PUSH_PRODUCT_42");
        assert_eq!(rec.exchange, "GROWW_PUSH_EXCHANGE_9");
        assert_eq!(rec.order_type, "GROWW_PUSH_ORDER_TYPE_8");
        assert_eq!(rec.transaction_type, "GROWW_PUSH_BUY_SELL_5");
        assert_eq!(rec.validity, "GROWW_PUSH_DURATION_6");
    }

    #[test]
    fn event_seq_is_distinct_across_consecutive_builds() {
        let dto = sample_dto();
        let a = build_groww_order_event_record(&dto, 1).map(|r| r.event_seq);
        let b = build_groww_order_event_record(&dto, 1).map(|r| r.event_seq);
        assert!(a.is_some() && b.is_some() && a != b);
    }

    #[test]
    fn test_build_groww_position_event_record_maps_all_symbol_and_position_fields() {
        let pd = PositionDetailProto {
            symbol_data: Some(sample_symbol_info()),
            position_info: Some(PositionInfo {
                symbol_isin: "INE123A01016".to_owned(),
                bse: None,
                nse: Some(ExchangePosition {
                    credit_qty: 75.0,
                    credit_price: 12_344.0,
                    debit_qty: 0.0,
                    debit_price: 0.0,
                }),
            }),
        };
        let rec = build_groww_position_event_record(&pd, 456);
        assert_eq!(rec.ts_ist_nanos, 456);
        assert_eq!(rec.feed, Feed::Groww);
        assert!(rec.event_seq >= 1);
        assert_eq!(rec.symbol_isin, "INE123A01016");
        assert_eq!(rec.security_id, -1);
        assert_eq!(rec.exchange_segment, "FNO");
        assert_eq!(rec.symbol, "NIFTY 25000 CE");
        assert_eq!(rec.exchange, "NSE");
        assert_eq!(rec.tr_time_stamp, 1_752_800_000);
        assert_eq!(rec.search_id, "nifty-25000-ce");
        assert_eq!(rec.product, "MIS");
        assert_eq!(rec.contract_id, "NIFTY25JAN25000CE");
        assert_eq!(rec.equity_type, "OPTION");
        assert_eq!(rec.display_name, "NIFTY 25000 CE");
        assert_eq!(rec.underlying_id, "NIFTY");
        assert_eq!(rec.nse_market_lot, 75);
        assert_eq!(rec.bse_market_lot, 0);
        assert_eq!(rec.underlying_asset_type, "EQUITY_ASSET_INDICES");
        assert_eq!(rec.freeze_qty, 1800);
        assert_eq!(rec.nse_credit_qty, Some(75.0));
        assert_eq!(rec.nse_credit_price, Some(12_344.0));
        assert_eq!(rec.nse_debit_qty, Some(0.0));
        assert_eq!(rec.nse_debit_price, Some(0.0));
        // Absent BSE leg stays None — never a fabricated 0.0.
        assert_eq!(rec.bse_credit_qty, None);
        assert_eq!(rec.bse_credit_price, None);
        assert_eq!(rec.bse_debit_qty, None);
        assert_eq!(rec.bse_debit_price, None);
        assert!(rec.detail_raw.contains("equity_type=2"));
        assert!(rec.detail_raw.contains("underlying_asset_type=1"));
    }

    #[test]
    fn test_absent_submessages_degrade_to_sentinels() {
        let pd = PositionDetailProto::default();
        let rec = build_groww_position_event_record(&pd, 9);
        assert_eq!(rec.symbol_isin, "");
        assert_eq!(rec.symbol, "");
        assert_eq!(rec.exchange, "");
        assert_eq!(rec.tr_time_stamp, -1);
        assert_eq!(rec.nse_market_lot, -1);
        assert_eq!(rec.bse_market_lot, -1);
        assert_eq!(rec.freeze_qty, -1);
        assert_eq!(rec.product, "");
        assert_eq!(rec.equity_type, "");
        assert_eq!(rec.underlying_asset_type, "");
        assert_eq!(rec.nse_credit_qty, None);
        assert_eq!(rec.bse_debit_price, None);
        assert_eq!(rec.detail_raw, "");
    }

    #[test]
    fn position_symbol_falls_back_to_contract_id_when_display_name_empty() {
        let mut sym = sample_symbol_info();
        sym.display_name = String::new();
        let pd = PositionDetailProto {
            symbol_data: Some(sym),
            position_info: None,
        };
        let rec = build_groww_position_event_record(&pd, 1);
        assert_eq!(rec.symbol, "NIFTY25JAN25000CE");
        assert_eq!(rec.symbol_isin, "");
    }

    #[test]
    fn unknown_position_enums_degrade_to_raw_preserving_labels() {
        let mut sym = sample_symbol_info();
        sym.stocks_product = 7; // the POSITION file's `_unknown = 7`
        sym.equity_type = 9;
        sym.underlying_asset_type = 3;
        sym.exchange = 8;
        let pd = PositionDetailProto {
            symbol_data: Some(sym),
            position_info: None,
        };
        let rec = build_groww_position_event_record(&pd, 1);
        assert_eq!(rec.product, "GROWW_PUSH_PRODUCT_7");
        assert_eq!(rec.equity_type, "GROWW_PUSH_EQUITY_TYPE_9");
        assert_eq!(rec.underlying_asset_type, "GROWW_PUSH_ASSET_3");
        assert_eq!(rec.exchange, "GROWW_PUSH_EXCHANGE_8");
    }

    #[test]
    fn test_now_ist_epoch_nanos_is_positive_and_past_2026() {
        let n = now_ist_epoch_nanos();
        // 2026-01-01 00:00:00 UTC in nanos — the stamp must be past it.
        assert!(n > 1_767_225_600_000_000_000);
    }

    #[test]
    fn test_groww_push_capture_disabled_publish_is_a_noop() {
        let capture = GrowwPushCapture::disabled();
        let dto = sample_dto();
        let rec = build_groww_order_event_record(&dto, 1).unwrap_or_else(|| {
            panic!("record expected");
        });
        // No channel — must not panic, must not block.
        capture.publish_order(rec);
        capture.publish_position(build_groww_position_event_record(
            &PositionDetailProto::default(),
            1,
        ));
    }

    #[tokio::test]
    async fn test_groww_push_capture_new_publishes_into_both_channels() {
        let (order_tx, mut order_rx) = mpsc::channel(4);
        let (pos_tx, mut pos_rx) = mpsc::channel(4);
        let capture = GrowwPushCapture::new(order_tx, pos_tx);
        let dto = sample_dto();
        let rec = build_groww_order_event_record(&dto, 1).unwrap_or_else(|| {
            panic!("record expected");
        });
        capture.publish_order(rec.clone());
        capture.publish_position(build_groww_position_event_record(
            &PositionDetailProto::default(),
            2,
        ));
        let got = order_rx.recv().await.unwrap_or_else(|| {
            panic!("order record expected");
        });
        assert_eq!(got.order_id, rec.order_id);
        let got_pos = pos_rx.recv().await.unwrap_or_else(|| {
            panic!("position record expected");
        });
        assert_eq!(got_pos.ts_ist_nanos, 2);
    }

    #[tokio::test]
    async fn test_groww_push_capture_publish_order_full_channel_drops_without_blocking() {
        let (order_tx, _order_rx_keepalive) = mpsc::channel(1);
        let (pos_tx, _pos_rx_keepalive) = mpsc::channel(1);
        let capture = GrowwPushCapture::new(order_tx, pos_tx);
        let dto = sample_dto();
        for _ in 0..3 {
            let rec = build_groww_order_event_record(&dto, 1).unwrap_or_else(|| {
                panic!("record expected");
            });
            // Capacity 1 with no consumer: publishes past the first must take
            // the `full` drop arm — and never block/panic.
            capture.publish_order(rec);
        }
    }

    #[tokio::test]
    async fn test_groww_push_capture_publish_position_closed_channel_drops_without_blocking() {
        let (order_tx, order_rx) = mpsc::channel(1);
        let (pos_tx, pos_rx) = mpsc::channel(1);
        drop(order_rx);
        drop(pos_rx);
        let capture = GrowwPushCapture::new(order_tx, pos_tx);
        // Closed channels: both publishes take the `closed` drop arm.
        let dto = sample_dto();
        if let Some(rec) = build_groww_order_event_record(&dto, 1) {
            capture.publish_order(rec);
        }
        capture.publish_position(build_groww_position_event_record(
            &PositionDetailProto::default(),
            1,
        ));
    }
}
