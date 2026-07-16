//! Pure, total mapper: decoded push [`OrderDetailsBroadCastDto`] → the
//! neutral [`BrokerOrderEvent`] seam (order-push Stage C, 2026-07-16).
//!
//! # Contract
//! - **Total + no-panic:** every input maps; unknown enum values degrade to
//!   [`BrokerOrderStatus::Unknown`] while a raw-preserving label lands in
//!   [`BrokerOrderEvent::raw_status`] (annexure rule 15 / GROWW-ORD-07
//!   discipline — never guess, never lose the wire value).
//! - **Integer paise stay integer paise** end to end (the seam's price
//!   discipline; the proto decoder already yields `i64` paise).
//! - **Nothing fabricated:** the push DTO carries NO user reference id and NO
//!   parseable exchange timestamp (`timeStampFromMidNight` semantics are
//!   UNVERIFIED-LIVE), so `reference_id` and `exchange_ts_ms` are `None` —
//!   honestly absent, never invented. `received_at_ms` is caller-stamped at
//!   the decode instant so this function stays pure.
//! - A frame that carried only `stageAndTimeStamp` markers (no
//!   `orderDetailUpdateDto`) maps to `None` — there is no order identity to
//!   event on.
//!
//! # proto3 absent-vs-zero honesty
//! proto3 cannot distinguish an omitted scalar from an explicit `0`. The two
//! `Option` fields therefore degrade conservatively:
//! - `avg_fill_price_paise`: `Some` only when the wire value is `> 0` (a
//!   zero average price is never a real fill price).
//! - `remaining_qty`: `Some` only when the DTO shows fill activity
//!   (`remaining_qty > 0` or `filled_qty > 0`) — a both-zero DTO cannot be
//!   told apart from an omitted field, so it reads `None` rather than a
//!   fabricated `0` sentinel (the seam's "never a sentinel" rule).

use tickvault_common::broker_order_events::{BrokerOrderEvent, BrokerOrderStatus};
use tickvault_common::feed::Feed;

use super::proto::{OrderDetailUpdate, OrderDetailsBroadCastDto, order_status_name, segment_name};

/// Raw-preserving label for an order-status enum value outside the known
/// vocabulary (kept in `raw_status` so the wire value is never lost).
fn raw_status_label(raw: i32) -> String {
    order_status_name(raw).map_or_else(|| format!("GROWW_PUSH_STATUS_{raw}"), str::to_owned)
}

/// Raw-preserving label for a segment enum value outside the known vocabulary.
fn segment_label(raw: i32) -> String {
    segment_name(raw).map_or_else(|| format!("GROWW_PUSH_SEGMENT_{raw}"), str::to_owned)
}

/// Map one decoded [`OrderDetailUpdate`] into the neutral seam event.
/// Pure + total; see the module docs for the degrade rules.
#[must_use]
pub fn map_order_detail(detail: &OrderDetailUpdate, received_at_ms: i64) -> BrokerOrderEvent {
    let status = order_status_name(detail.order_status).map_or(
        BrokerOrderStatus::Unknown,
        BrokerOrderStatus::from_groww_status,
    );
    BrokerOrderEvent {
        broker: Feed::Groww,
        broker_order_id: detail.groww_order_id.clone(),
        // The push DTO carries no user reference/correlation id — honestly
        // absent (`gui_order_id`/`remark` semantics are UNVERIFIED-LIVE and
        // are never guessed into the reference slot).
        reference_id: None,
        status,
        raw_status: raw_status_label(detail.order_status),
        filled_qty: i64::from(detail.filled_qty),
        remaining_qty: if detail.remaining_qty > 0 || detail.filled_qty > 0 {
            Some(i64::from(detail.remaining_qty))
        } else {
            None
        },
        avg_fill_price_paise: (detail.avg_fill_price_paise > 0)
            .then_some(detail.avg_fill_price_paise),
        segment: segment_label(detail.segment),
        // `timeStampFromMidNight` semantics UNVERIFIED-LIVE — never fabricated
        // into an epoch timestamp.
        exchange_ts_ms: None,
        received_at_ms,
    }
}

/// Map a decoded ORDER-subject broadcast into the neutral seam event.
///
/// Returns `None` when the frame carried no `orderDetailUpdateDto` submessage
/// (a stage-marker-only frame has no order identity to event on).
#[must_use]
pub fn map_order_broadcast(
    dto: &OrderDetailsBroadCastDto,
    received_at_ms: i64,
) -> Option<BrokerOrderEvent> {
    dto.order_detail
        .as_ref()
        .map(|detail| map_order_detail(detail, received_at_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::broker_order_events::EventSource;

    fn sample_detail() -> OrderDetailUpdate {
        OrderDetailUpdate {
            qty: 50,
            price_paise: 1_234_500,
            trigger_price_paise: 0,
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
            remark: String::new(),
            contract_id: "NIFTY25JAN25000CE".to_owned(),
            gui_order_id: "gui-1".to_owned(),
        }
    }

    #[test]
    fn test_map_order_detail_maps_the_full_fidelity_fields() {
        let detail = sample_detail();
        let ev = map_order_detail(&detail, 1_700_000_000_123);
        assert_eq!(ev.broker, Feed::Groww);
        assert_eq!(ev.broker_order_id, "GMK1234567890");
        assert_eq!(ev.reference_id, None);
        assert_eq!(ev.status, BrokerOrderStatus::Filled);
        assert_eq!(ev.raw_status, "EXECUTED");
        assert_eq!(ev.filled_qty, 20);
        assert_eq!(ev.remaining_qty, Some(30));
        assert_eq!(ev.avg_fill_price_paise, Some(1_234_400));
        assert_eq!(ev.segment, "FNO");
        assert_eq!(ev.exchange_ts_ms, None);
        assert_eq!(ev.received_at_ms, 1_700_000_000_123);
    }

    #[test]
    fn status_mapping_table_composes_with_the_seam() {
        // (raw enum value, expected neutral status, expected raw label)
        let table: &[(i32, BrokerOrderStatus, &str)] = &[
            (0, BrokerOrderStatus::New, "NEW"),
            (1, BrokerOrderStatus::Pending, "ACKED"),
            (2, BrokerOrderStatus::Pending, "TRIGGER_PENDING"),
            (3, BrokerOrderStatus::Open, "APPROVED"),
            (4, BrokerOrderStatus::Rejected, "REJECTED"),
            (5, BrokerOrderStatus::Failed, "FAILED"),
            (6, BrokerOrderStatus::Filled, "EXECUTED"),
            (7, BrokerOrderStatus::Filled, "DELIVERY_AWAITED"),
            (8, BrokerOrderStatus::Cancelled, "CANCELLED"),
            (9, BrokerOrderStatus::Pending, "CANCELLATION_REQUESTED"),
            (10, BrokerOrderStatus::Pending, "MODIFICATION_REQUESTED"),
            (11, BrokerOrderStatus::Filled, "COMPLETED"),
        ];
        for &(raw, expected, label) in table {
            let mut detail = sample_detail();
            detail.order_status = raw;
            let ev = map_order_detail(&detail, 0);
            assert_eq!(ev.status, expected, "status enum {raw}");
            assert_eq!(ev.raw_status, label, "raw label for enum {raw}");
        }
    }

    #[test]
    fn unknown_status_enum_degrades_to_unknown_preserving_the_raw_value() {
        let mut detail = sample_detail();
        detail.order_status = 99;
        let ev = map_order_detail(&detail, 0);
        assert_eq!(ev.status, BrokerOrderStatus::Unknown);
        assert_eq!(ev.raw_status, "GROWW_PUSH_STATUS_99");
    }

    #[test]
    fn unknown_segment_enum_degrades_to_a_raw_preserving_label() {
        let mut detail = sample_detail();
        detail.segment = 7;
        let ev = map_order_detail(&detail, 0);
        assert_eq!(ev.segment, "GROWW_PUSH_SEGMENT_7");
    }

    #[test]
    fn segment_table_maps_the_known_vocabulary() {
        for (raw, label) in [(0, "CASH"), (1, "FNO"), (2, "CURRENCY"), (3, "COMMODITY")] {
            let mut detail = sample_detail();
            detail.segment = raw;
            assert_eq!(map_order_detail(&detail, 0).segment, label);
        }
    }

    #[test]
    fn absent_fields_degrade_to_none_not_sentinels() {
        // A proto3-default (all-zero) DTO: no fill activity, no avg price —
        // the Option fields read None, never a fabricated 0.
        let detail = OrderDetailUpdate::default();
        let ev = map_order_detail(&detail, 42);
        assert_eq!(ev.filled_qty, 0);
        assert_eq!(ev.remaining_qty, None);
        assert_eq!(ev.avg_fill_price_paise, None);
        assert_eq!(ev.broker_order_id, "");
        // proto3 default enum 0 = NEW (a legitimate wire value).
        assert_eq!(ev.status, BrokerOrderStatus::New);
        assert_eq!(ev.received_at_ms, 42);
    }

    #[test]
    fn fully_filled_order_keeps_the_genuine_remaining_zero() {
        let mut detail = sample_detail();
        detail.filled_qty = 50;
        detail.remaining_qty = 0;
        let ev = map_order_detail(&detail, 0);
        assert_eq!(ev.remaining_qty, Some(0));
        assert_eq!(ev.filled_qty, 50);
    }

    #[test]
    fn test_map_order_broadcast_without_order_detail_maps_to_none() {
        let dto = OrderDetailsBroadCastDto::default();
        assert_eq!(map_order_broadcast(&dto, 0), None);
    }

    #[test]
    fn broadcast_with_order_detail_maps_to_the_event() {
        let dto = OrderDetailsBroadCastDto {
            stage_and_time_stamp: Vec::new(),
            order_detail: Some(sample_detail()),
        };
        let ev = map_order_broadcast(&dto, 7).map(|e| (e.broker_order_id.clone(), e));
        let (id, ev) = ev.unwrap_or_else(|| panic!("event expected"));
        assert_eq!(id, "GMK1234567890");
        assert_eq!(ev.received_at_ms, 7);
        // The mapped event is push-transport material; the provenance enum
        // exists on the seam for the fan-out consumers.
        let _source = EventSource::Push;
    }
}
