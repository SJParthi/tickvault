//! Position-update handling — DECODE + COUNT + LOG + CAPTURE (order-push
//! Stage C, 2026-07-16; capture lane added 2026-07-18).
//!
//! This module persists nothing ITSELF and mutates nothing. It exists so a
//! delivered position frame is never a silent byte-void: every decoded
//! update is counted (`tv_groww_push_position_updates_total`) with a lean
//! identity-only `debug!` (no per-field price dumps — the log layer is not a
//! position ledger), handed to the best-effort full-fidelity CAPTURE sink
//! ([`GrowwPushCapture`] → the `position_update_events` forensic table via
//! the app-side consumer; `ORDER-EVT-01` subsystem — a disabled/full sink
//! never blocks or fails this path), and every decode failure is a coded
//! `GROWW-PUSH-03` error + counter, skip-and-continue (a malformed frame
//! never ends the session — the runner's framing layer already bounded the
//! payload).

use tickvault_common::error_code::ErrorCode;
use tracing::{debug, error};

use super::order_events::{GrowwPushCapture, build_groww_position_event_record};
use super::proto::decode_position_detail;

/// What [`handle_position_payload`] did with one frame (returned for unit
/// tests + the runner's forensics; carries no decoded data — this layer is
/// count-and-log only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionHandleOutcome {
    /// The frame decoded; the update was counted.
    Counted,
    /// The frame failed protobuf decode; counted as a decode error and
    /// skipped (session survives).
    DecodeFailed,
}

/// Decode + count + log + capture one POSITION-subject MSG payload.
///
/// Never panics, never persists directly, never propagates an error — a
/// decode failure is coded + counted and the caller continues with the next
/// frame; the capture publish is `try_send` best-effort (never blocking).
pub fn handle_position_payload(
    payload: &[u8],
    capture: &GrowwPushCapture,
    ts_ist_nanos: i64,
) -> PositionHandleOutcome {
    match decode_position_detail(payload) {
        Ok(detail) => {
            metrics::counter!("tv_groww_push_position_updates_total").increment(1);
            // Lean identity-only line: the contract/ISIN names WHICH position
            // moved; quantities/prices deliberately never hit the log layer.
            let contract_id = detail
                .symbol_data
                .as_ref()
                .map(|s| s.contract_id.as_str())
                .unwrap_or_default();
            let symbol_isin = detail
                .position_info
                .as_ref()
                .map(|p| p.symbol_isin.as_str())
                .unwrap_or_default();
            debug!(
                contract_id,
                symbol_isin,
                "groww push: position update received (decode+count; full-fidelity capture via order_update_events consumer)"
            );
            capture.publish_position(build_groww_position_event_record(&detail, ts_ist_nanos));
            PositionHandleOutcome::Counted
        }
        Err(e) => {
            error!(
                code = ErrorCode::GrowwPush03DecodeFailed.code_str(),
                error = %e,
                payload_len = payload.len(),
                "groww push: position payload failed protobuf decode — frame skipped, session continues"
            );
            metrics::counter!("tv_groww_push_decode_errors_total", "kind" => "position")
                .increment(1);
            PositionHandleOutcome::DecodeFailed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-encoded minimal `PositionDetailProto`:
    /// field 1 (`symbolData`, len-delim) containing field 4 (`contractId`,
    /// string "X").
    fn valid_position_bytes() -> Vec<u8> {
        // inner SymbolInfo: tag (4<<3)|2 = 0x22, len 1, b"X"
        let inner = [0x22, 0x01, b'X'];
        // outer: tag (1<<3)|2 = 0x0A, len 3, inner
        let mut out = vec![0x0A, 0x03];
        out.extend_from_slice(&inner);
        out
    }

    #[test]
    fn test_handle_position_payload_counts_a_valid_frame() {
        assert_eq!(
            handle_position_payload(&valid_position_bytes(), &GrowwPushCapture::disabled(), 1),
            PositionHandleOutcome::Counted
        );
    }

    #[tokio::test]
    async fn valid_frame_publishes_a_capture_record() {
        let (order_tx, _order_rx) = tokio::sync::mpsc::channel(4);
        let (pos_tx, mut pos_rx) = tokio::sync::mpsc::channel(4);
        let capture = GrowwPushCapture::new(order_tx, pos_tx);
        assert_eq!(
            handle_position_payload(&valid_position_bytes(), &capture, 77),
            PositionHandleOutcome::Counted
        );
        let rec = pos_rx.recv().await.unwrap_or_else(|| {
            panic!("capture record expected");
        });
        assert_eq!(rec.ts_ist_nanos, 77);
        assert_eq!(rec.contract_id, "X");
    }

    #[test]
    fn empty_payload_decodes_to_default_and_counts() {
        // proto3: an empty body is a valid all-default message.
        assert_eq!(
            handle_position_payload(&[], &GrowwPushCapture::disabled(), 1),
            PositionHandleOutcome::Counted
        );
    }

    #[test]
    fn truncated_frame_is_a_decode_failure_not_a_panic() {
        // Length-delimited field claiming 100 bytes with only 1 present.
        let bytes = [0x0A, 0x64, 0x00];
        assert_eq!(
            handle_position_payload(&bytes, &GrowwPushCapture::disabled(), 1),
            PositionHandleOutcome::DecodeFailed
        );
    }

    #[test]
    fn garbage_wire_type_is_a_decode_failure_not_a_panic() {
        // field 1, wire type 7 (unknown).
        let bytes = [0x0F, 0x01];
        assert_eq!(
            handle_position_payload(&bytes, &GrowwPushCapture::disabled(), 1),
            PositionHandleOutcome::DecodeFailed
        );
    }
}
