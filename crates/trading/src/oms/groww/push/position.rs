//! Position-update handling — DECODE + COUNT + LOG ONLY (order-push Stage C,
//! 2026-07-16).
//!
//! The position-table write is a SEPARATE session's follow-up (the
//! broker-portfolio persistence area owns position storage); this module
//! deliberately persists nothing and mutates nothing. It exists so a
//! delivered position frame is never a silent byte-void: every decoded
//! update is counted (`tv_groww_push_position_updates_total`) with a lean
//! identity-only `debug!` (no per-field price dumps — the log layer is not a
//! position ledger), and every decode failure is a coded
//! `GROWW-PUSH-03` error + counter, skip-and-continue (a malformed frame
//! never ends the session — the runner's framing layer already bounded the
//! payload).

use tickvault_common::error_code::ErrorCode;
use tracing::{debug, error};

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

/// Decode + count + log one POSITION-subject MSG payload.
///
/// Never panics, never persists, never propagates an error — a decode
/// failure is coded + counted and the caller continues with the next frame.
pub fn handle_position_payload(payload: &[u8]) -> PositionHandleOutcome {
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
                "groww push: position update received (decode+count only — table write is a later session)"
            );
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
    fn valid_position_frame_is_counted() {
        assert_eq!(
            handle_position_payload(&valid_position_bytes()),
            PositionHandleOutcome::Counted
        );
    }

    #[test]
    fn empty_payload_decodes_to_default_and_counts() {
        // proto3: an empty body is a valid all-default message.
        assert_eq!(handle_position_payload(&[]), PositionHandleOutcome::Counted);
    }

    #[test]
    fn truncated_frame_is_a_decode_failure_not_a_panic() {
        // Length-delimited field claiming 100 bytes with only 1 present.
        let bytes = [0x0A, 0x64, 0x00];
        assert_eq!(
            handle_position_payload(&bytes),
            PositionHandleOutcome::DecodeFailed
        );
    }

    #[test]
    fn garbage_wire_type_is_a_decode_failure_not_a_panic() {
        // field 1, wire type 7 (unknown).
        let bytes = [0x0F, 0x01];
        assert_eq!(
            handle_position_payload(&bytes),
            PositionHandleOutcome::DecodeFailed
        );
    }
}
