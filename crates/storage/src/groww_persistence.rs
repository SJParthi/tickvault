//! Shared ILP flush-reconnect ladder primitives.
//!
//! **The Groww live-tick writer that originally lived here was DELETED
//! 2026-07-15** with the Groww live feed (operator directive: "remove the
//! whole Groww live feed; keep only spot 1m and option chain for both
//! brokers"). Groww rows in the shared `ticks` table are historical; the
//! REST legs write their own tables (`spot_1m_rest` / `option_chain_1m` /
//! `option_contract_1m_rest`).
//!
//! What remains is the bounded reconnect+replay ladder contract that the
//! shared candle chain adopted from the deleted writer:
//! [`is_connection_error`] (the pure SocketError classifier) +
//! [`GROWW_FLUSH_RECONNECT_MAX_RETRIES`] / [`GROWW_FLUSH_RECONNECT_BACKOFF_MS`]
//! — consumed by `shadow_candle_writer.rs` for every sealed-candle ILP flush
//! (both feeds). Renaming the module/constants is deliberately deferred to a
//! cosmetic follow-up (5 call sites in the shared candle writer).

use questdb::ErrorCode as QuestErrorCode;

/// Max in-wake reconnect+replay attempts on a flush that failed with a CONNECTION
/// error (broken pipe / connection reset / not-connected). Bounded so a sustained
/// QuestDB outage degrades to the producer's capture-at-receipt spill/DLQ net
/// (lock §32) instead of stalling the bridge wake forever. After these are
/// exhausted, `flush` returns `Err` with the buffer + pending RETAINED.
pub const GROWW_FLUSH_RECONNECT_MAX_RETRIES: usize = 3;

/// Exponential backoff (milliseconds) slept BETWEEN reconnect attempts. Indexed
/// by `attempt - 1`. Total wall-clock across all 3 retries ≤ 350ms, so the bridge
/// wake is never blocked for long — well inside the open-burst recovery budget.
pub const GROWW_FLUSH_RECONNECT_BACKOFF_MS: [u64; GROWW_FLUSH_RECONNECT_MAX_RETRIES] =
    [50, 100, 200];

/// Pure classifier: is this questdb error a recoverable CONNECTION fault (so a
/// reconnect + replay is worth attempting), as opposed to a structural error
/// (bad name / API misuse / server-rejected row) that re-sending would NOT fix?
///
/// Only `ErrorCode::SocketError` is a transport fault — questdb-rs maps every
/// broken-pipe / connection-reset / "not connected to database" `io::Error` to
/// `SocketError` (questdb-rs `sender/mod.rs` `map_io_to_socket_err` +
/// `flush_impl`'s not-connected guard). Every other code (`InvalidName`,
/// `InvalidApiCall`, `ServerFlushError`, `InvalidTimestamp`, …) is a property of
/// the buffered row or the request, NOT the socket — retrying re-hammers a row
/// that will fail again, so those return `false`. O(1), pure, no I/O.
#[must_use]
pub fn is_connection_error(err: &questdb::Error) -> bool {
    matches!(err.code(), QuestErrorCode::SocketError)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_connection_error_true_for_socket_error() {
        // A broken pipe / connection reset / not-connected all surface from
        // questdb-rs as ErrorCode::SocketError — the ONLY class worth a reconnect.
        let err = questdb::Error::new(
            QuestErrorCode::SocketError,
            "Could not flush buffer: Broken pipe (os error 32)",
        );
        assert!(
            is_connection_error(&err),
            "SocketError must classify as a connection error (retry-worthy)"
        );
    }

    #[test]
    fn test_is_connection_error_false_for_non_socket_error() {
        // Structural errors must NOT be retried — re-sending a bad row fails again.
        for code in [
            QuestErrorCode::InvalidName,
            QuestErrorCode::InvalidApiCall,
            QuestErrorCode::ServerFlushError,
            QuestErrorCode::InvalidTimestamp,
            QuestErrorCode::CouldNotResolveAddr,
        ] {
            let err = questdb::Error::new(code, "structural failure");
            assert!(
                !is_connection_error(&err),
                "non-SocketError code {code:?} must NOT classify as a connection error"
            );
        }
    }

    #[test]
    fn test_reconnect_backoff_schedule_caps_at_three() {
        // The retry budget is bounded — total wall-clock ≤ 350ms so the bridge
        // wake never stalls, and a sustained outage degrades to the spill net.
        assert_eq!(GROWW_FLUSH_RECONNECT_MAX_RETRIES, 3);
        assert_eq!(GROWW_FLUSH_RECONNECT_BACKOFF_MS, [50, 100, 200]);
        assert_eq!(
            GROWW_FLUSH_RECONNECT_BACKOFF_MS.len(),
            GROWW_FLUSH_RECONNECT_MAX_RETRIES,
            "backoff schedule length must match the retry count"
        );
        let total_ms: u64 = GROWW_FLUSH_RECONNECT_BACKOFF_MS.iter().sum();
        assert!(
            total_ms <= 350,
            "total backoff must stay bounded (≤350ms), got {total_ms}ms"
        );
        // The schedule is monotonic non-decreasing (exponential-ish).
        assert!(
            GROWW_FLUSH_RECONNECT_BACKOFF_MS
                .windows(2)
                .all(|w| w[0] <= w[1]),
            "backoff must be non-decreasing"
        );
    }
}
