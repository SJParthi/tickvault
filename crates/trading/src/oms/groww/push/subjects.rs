//! Groww NATS subscription-subject builders for the ORDER/POSITION push
//! channel (order-push Stage B, 2026-07-16) — the grammar helpers of the
//! retired live-feed `subjects.rs`
//! (`dd7eaa5e^:crates/core/src/feed/groww/subjects.rs`) adapted to the three
//! order/position update subjects.
//!
//! The subject prefixes were extracted VERBATIM from the official
//! `growwapi-1.5.0` SDK (`growwapi/groww/constants.py::FeedConstants`,
//! wheel re-verified 2026-07-16):
//!
//! ```text
//! DERIVATIVES_ORDER_UPDATES    = "stocks_fo/order/updates.apex."
//! DERIVATIVES_POSITION_UPDATES = "stocks_fo/position/updates.apex."
//! EQUITY_ORDER_UPDATES         = "stocks/order/updates.apex."
//! ```
//!
//! The full subject is `<prefix><subscriptionId>` where `subscriptionId` is
//! the account-scoped id returned by the socket-token mint
//! ([`super::socket_token::SocketToken::subscription_id`]).
//!
//! Pairing with the decoders: the F&O/equity ORDER subjects deliver
//! [`super::proto::OrderDetailsBroadCastDto`] payloads; the F&O POSITION
//! subject delivers [`super::proto::PositionDetailProto`] payloads. The
//! subject — not the body — is what routes a MSG to the right decoder, so
//! [`classify_push_subject`] recovers `(kind, subscription_id)` from a
//! delivered MSG's subject and refuses anything outside the three known
//! prefixes (the same fail-closed reverse-parse discipline the live-feed
//! grammar carried).
//!
//! Cold path (subscribe-time / per-delivered-MSG on the order channel) —
//! `String` construction in the builders is intentional; the reverse parse
//! is zero-alloc (borrowed slice).

/// Subject prefix for F&O order updates (`SUB <prefix><subscriptionId>`).
pub const DERIVATIVES_ORDER_UPDATES: &str = "stocks_fo/order/updates.apex.";
/// Subject prefix for F&O position updates.
pub const DERIVATIVES_POSITION_UPDATES: &str = "stocks_fo/position/updates.apex.";
/// Subject prefix for equity order updates.
pub const EQUITY_ORDER_UPDATES: &str = "stocks/order/updates.apex.";

/// Which push stream a subject belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushSubjectKind {
    /// `stocks_fo/order/updates.apex.<id>` — F&O order/trade updates.
    FnoOrder,
    /// `stocks_fo/position/updates.apex.<id>` — F&O position updates.
    FnoPosition,
    /// `stocks/order/updates.apex.<id>` — equity order/trade updates.
    EquityOrder,
}

/// Join a subject prefix + the account subscription id. Cold-path `String`
/// (subscribe-time). Pre-sized to avoid reallocation.
fn join_subject(prefix: &str, subscription_id: &str) -> String {
    let mut s = String::with_capacity(prefix.len() + subscription_id.len());
    s.push_str(prefix);
    s.push_str(subscription_id);
    s
}

/// Build the F&O ORDER-updates subscription subject.
#[must_use]
pub fn fno_order_updates_subject(subscription_id: &str) -> String {
    join_subject(DERIVATIVES_ORDER_UPDATES, subscription_id)
}

/// Build the F&O POSITION-updates subscription subject.
#[must_use]
pub fn fno_position_updates_subject(subscription_id: &str) -> String {
    join_subject(DERIVATIVES_POSITION_UPDATES, subscription_id)
}

/// Build the EQUITY ORDER-updates subscription subject.
#[must_use]
pub fn equity_order_updates_subject(subscription_id: &str) -> String {
    join_subject(EQUITY_ORDER_UPDATES, subscription_id)
}

/// The three push prefixes we ever subscribe to. Any delivered MSG subject
/// must begin with exactly one of these to be routed. `stocks_fo/…` entries
/// are listed BEFORE `stocks/…` for reading clarity only — the prefixes are
/// mutually non-overlapping (`stocks_` vs `stocks/` diverge at byte 6), so
/// match order can never mis-classify.
const KNOWN_PREFIXES: [(&str, PushSubjectKind); 3] = [
    (DERIVATIVES_ORDER_UPDATES, PushSubjectKind::FnoOrder),
    (DERIVATIVES_POSITION_UPDATES, PushSubjectKind::FnoPosition),
    (EQUITY_ORDER_UPDATES, PushSubjectKind::EquityOrder),
];

/// Classify a delivered MSG subject and recover the subscription id
/// (zero-alloc — the id is a borrowed suffix slice).
///
/// Only a subject beginning with one of the three known push prefixes yields
/// a classification — a market-data subject (`/ld/eq/nse/price.<t>`), any
/// other SDK subject, or a malformed/garbage subject returns `None` and can
/// NEVER route a foreign payload into the order/position decoders. The
/// subscription id must be non-empty.
#[must_use]
pub fn classify_push_subject(subject: &str) -> Option<(PushSubjectKind, &str)> {
    KNOWN_PREFIXES.iter().find_map(|(prefix, kind)| {
        subject
            .strip_prefix(prefix)
            .filter(|id| !id.is_empty())
            .map(|id| (*kind, id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fno_order_updates_subject_builds() {
        assert_eq!(
            fno_order_updates_subject("sub-123"),
            "stocks_fo/order/updates.apex.sub-123"
        );
    }

    #[test]
    fn test_fno_position_updates_subject_builds() {
        assert_eq!(
            fno_position_updates_subject("sub-123"),
            "stocks_fo/position/updates.apex.sub-123"
        );
    }

    #[test]
    fn test_equity_order_updates_subject_builds() {
        assert_eq!(
            equity_order_updates_subject("sub-123"),
            "stocks/order/updates.apex.sub-123"
        );
    }

    /// Edge case: an EMPTY subscription id builds a prefix-only subject the
    /// classifier then REFUSES — the round-trip can never yield a bogus
    /// empty identity (fail-closed grammar, edge/zero-length boundary).
    #[test]
    fn test_fno_order_updates_subject_edge_empty_id_round_trip_refused() {
        let s = fno_order_updates_subject("");
        assert_eq!(s, DERIVATIVES_ORDER_UPDATES);
        assert_eq!(classify_push_subject(&s), None);
    }

    /// Same zero-length edge for the position subject builder.
    #[test]
    fn test_fno_position_updates_subject_edge_empty_id_round_trip_refused() {
        let s = fno_position_updates_subject("");
        assert_eq!(classify_push_subject(&s), None);
    }

    /// Same zero-length edge for the equity order subject builder.
    #[test]
    fn test_equity_order_updates_subject_edge_empty_id_round_trip_refused() {
        let s = equity_order_updates_subject("");
        assert_eq!(classify_push_subject(&s), None);
    }

    #[test]
    fn test_classify_push_subject_extracts_kind_and_id() {
        assert_eq!(
            classify_push_subject("stocks_fo/order/updates.apex.abc-1"),
            Some((PushSubjectKind::FnoOrder, "abc-1"))
        );
        assert_eq!(
            classify_push_subject("stocks_fo/position/updates.apex.abc-1"),
            Some((PushSubjectKind::FnoPosition, "abc-1"))
        );
        assert_eq!(
            classify_push_subject("stocks/order/updates.apex.abc-1"),
            Some((PushSubjectKind::EquityOrder, "abc-1"))
        );
    }

    #[test]
    fn test_classify_push_subject_rejects_malformed_and_foreign() {
        // market-data subjects and other SDK subjects must never route here.
        assert_eq!(classify_push_subject("/ld/eq/nse/price.1234"), None);
        assert_eq!(classify_push_subject("/ld/fo/nse/price_detailed.1"), None);
        assert_eq!(
            classify_push_subject("stocks/position/updates.apex.x"),
            None
        );
        assert_eq!(classify_push_subject("no-dot-here"), None);
        assert_eq!(classify_push_subject(""), None);
        // known prefix, empty id → refused.
        assert_eq!(classify_push_subject("stocks/order/updates.apex."), None);
    }

    /// Every built subject must classify back to exactly the id + kind put in.
    #[test]
    fn test_build_then_classify_round_trips() {
        for (built, kind) in [
            (fno_order_updates_subject("s1"), PushSubjectKind::FnoOrder),
            (
                fno_position_updates_subject("s1"),
                PushSubjectKind::FnoPosition,
            ),
            (
                equity_order_updates_subject("s1"),
                PushSubjectKind::EquityOrder,
            ),
        ] {
            assert_eq!(classify_push_subject(&built), Some((kind, "s1")));
        }
    }

    /// Pinned verbatim against growwapi-1.5.0 `groww/constants.py::FeedConstants`
    /// — all 3, so a wrong-prefix regression (= silent no-data) fails the build.
    #[test]
    fn test_prefixes_match_sdk_constants() {
        assert_eq!(DERIVATIVES_ORDER_UPDATES, "stocks_fo/order/updates.apex.");
        assert_eq!(
            DERIVATIVES_POSITION_UPDATES,
            "stocks_fo/position/updates.apex."
        );
        assert_eq!(EQUITY_ORDER_UPDATES, "stocks/order/updates.apex.");
    }
}
