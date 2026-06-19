//! Groww NATS subscription-subject builder (operator 2026-06-19 "implement
//! everything"; §32 native-Rust path) — the "request" side of request→response.
//!
//! Groww's live feed is NATS-over-WebSocket: to receive an instrument's ticks the
//! client `SUB`scribes to a per-instrument subject `<prefix>.<exchange_token>`.
//! The subject prefixes were extracted verbatim from the official
//! `growwapi-1.5.0` SDK (`groww/constants.py::FeedConstants`), see
//! `docs/groww-ref/`.
//!
//! Pairing with the decoder: the protobuf tick body (`feed::groww::proto`) carries
//! NO instrument identity — the SDK takes identity from the subscription subject.
//! So [`token_from_subject`] recovers the `exchange_token` from a delivered MSG's
//! subject, which the connector maps back to our `(security_id, segment)`.
//!
//! Cold path (subscribe-time, once per instrument at connect) — `String`
//! construction here is intentional and outside any hot path. The reverse parse
//! is zero-alloc (returns a borrowed slice).

use tickvault_common::types::ExchangeSegment;

/// Subject prefix for NSE cash-equity live price (`SUB <prefix><token>`).
pub const EQUITY_NSE_LIVE_PRICE: &str = "/ld/eq/nse/price.";
/// Subject prefix for BSE cash-equity live price.
pub const EQUITY_BSE_LIVE_PRICE: &str = "/ld/eq/bse/price.";
/// Subject prefix for NSE F&O live price.
pub const DERIVATIVES_NSE_LIVE_PRICE: &str = "/ld/fo/nse/price.";
/// Subject prefix for BSE F&O live price.
pub const DERIVATIVES_BSE_LIVE_PRICE: &str = "/ld/fo/bse/price.";
/// Subject prefix for NSE index value.
pub const NSE_LIVE_INDEX: &str = "/ld/indices/nse/price.";
/// Subject prefix for BSE index value.
pub const BSE_LIVE_INDEX: &str = "/ld/indices/bse/price.";

/// Join a subject prefix + the instrument's Groww `exchange_token`. Cold-path
/// `String` (subscribe-time). Pre-sized to avoid reallocation.
fn join_subject(prefix: &str, exchange_token: &str) -> String {
    let mut s = String::with_capacity(prefix.len() + exchange_token.len());
    s.push_str(prefix);
    s.push_str(exchange_token);
    s
}

/// Build the live-price (LTP) subscription subject for an EQUITY or F&O segment.
///
/// Returns `None` for `IdxI` (use [`index_value_subject`]) and for currency /
/// commodity segments (out of scope for the second feed).
#[must_use]
pub fn live_price_subject(segment: ExchangeSegment, exchange_token: &str) -> Option<String> {
    let prefix = match segment {
        ExchangeSegment::NseEquity => EQUITY_NSE_LIVE_PRICE,
        ExchangeSegment::BseEquity => EQUITY_BSE_LIVE_PRICE,
        ExchangeSegment::NseFno => DERIVATIVES_NSE_LIVE_PRICE,
        ExchangeSegment::BseFno => DERIVATIVES_BSE_LIVE_PRICE,
        _ => return None,
    };
    Some(join_subject(prefix, exchange_token))
}

/// Build the index-value subscription subject. `IdxI` does not encode the
/// exchange, so the caller passes `on_bse` (true for the one BSE SENSEX index,
/// false for the NSE indices).
#[must_use]
pub fn index_value_subject(on_bse: bool, exchange_token: &str) -> String {
    let prefix = if on_bse {
        BSE_LIVE_INDEX
    } else {
        NSE_LIVE_INDEX
    };
    join_subject(prefix, exchange_token)
}

/// The six live-price + index subject prefixes we ever subscribe to. Any
/// delivered MSG subject must begin with exactly one of these for its token to be
/// trusted.
const KNOWN_PREFIXES: [&str; 6] = [
    EQUITY_NSE_LIVE_PRICE,
    EQUITY_BSE_LIVE_PRICE,
    DERIVATIVES_NSE_LIVE_PRICE,
    DERIVATIVES_BSE_LIVE_PRICE,
    NSE_LIVE_INDEX,
    BSE_LIVE_INDEX,
];

/// Recover the `exchange_token` from a delivered MSG subject (zero-alloc).
///
/// Only a subject that begins with one of the [`KNOWN_PREFIXES`] yields a token
/// (the suffix after the prefix) — so an UNRELATED subject the SDK also defines
/// (`…/price_detailed.<t>`, `…/book.<t>`) or any malformed/garbage subject returns
/// `None` and can NEVER mis-route a detail/depth payload as a price tick, nor
/// yield a bogus identity (3-agent hostile-review hardening, 2026-06-19). The
/// token must be non-empty.
#[must_use]
pub fn token_from_subject(subject: &str) -> Option<&str> {
    KNOWN_PREFIXES
        .iter()
        .find_map(|prefix| subject.strip_prefix(prefix))
        .filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_live_price_subject_equity_and_fno() {
        assert_eq!(
            live_price_subject(ExchangeSegment::NseEquity, "1234").as_deref(),
            Some("/ld/eq/nse/price.1234")
        );
        assert_eq!(
            live_price_subject(ExchangeSegment::BseEquity, "9").as_deref(),
            Some("/ld/eq/bse/price.9")
        );
        assert_eq!(
            live_price_subject(ExchangeSegment::NseFno, "55001").as_deref(),
            Some("/ld/fo/nse/price.55001")
        );
        assert_eq!(
            live_price_subject(ExchangeSegment::BseFno, "77").as_deref(),
            Some("/ld/fo/bse/price.77")
        );
    }

    #[test]
    fn test_live_price_subject_none_for_idx_and_currency() {
        assert_eq!(live_price_subject(ExchangeSegment::IdxI, "13"), None);
    }

    #[test]
    fn test_index_value_subject_nse_and_bse() {
        assert_eq!(index_value_subject(false, "13"), "/ld/indices/nse/price.13");
        assert_eq!(index_value_subject(true, "51"), "/ld/indices/bse/price.51");
    }

    #[test]
    fn test_token_from_subject_extracts_suffix() {
        assert_eq!(token_from_subject("/ld/eq/nse/price.1234"), Some("1234"));
        assert_eq!(token_from_subject("/ld/indices/bse/price.51"), Some("51"));
    }

    #[test]
    fn test_token_from_subject_rejects_malformed() {
        assert_eq!(token_from_subject("no-dot-here"), None);
        assert_eq!(token_from_subject("/ld/eq/nse/price."), None); // known prefix, empty token
        assert_eq!(token_from_subject(""), None);
    }

    #[test]
    fn test_token_from_subject_rejects_unrelated_known_sdk_subjects() {
        // The SDK ALSO defines price_detailed/book/order subjects. A delivered
        // MSG on one of those must NOT yield a token (it would mis-route a
        // detail/depth payload as a price tick). Only the 6 price/index prefixes
        // are trusted (hostile-review hardening).
        assert_eq!(token_from_subject("/ld/eq/nse/price_detailed.1234"), None);
        assert_eq!(token_from_subject("/ld/eq/nse/book.1234"), None);
        assert_eq!(token_from_subject("/ld/fo/nse/price_detailed.55001"), None);
        assert_eq!(token_from_subject("stocks/order/updates.apex.xyz"), None);
        assert_eq!(
            token_from_subject("/ld/eq/nse/price.1234.5"),
            Some("1234.5")
        ); // token after the known prefix, verbatim
    }

    #[test]
    fn test_build_then_parse_round_trips_the_token() {
        // Every built subject must parse back to exactly the token put in.
        for (seg, tok) in [
            (ExchangeSegment::NseEquity, "1333"),
            (ExchangeSegment::NseFno, "49081"),
            (ExchangeSegment::BseEquity, "500325"),
            (ExchangeSegment::BseFno, "842"),
        ] {
            let subject = live_price_subject(seg, tok).expect("equity/fno builds");
            assert_eq!(
                token_from_subject(&subject),
                Some(tok),
                "round-trip {seg:?}"
            );
        }
        let idx = index_value_subject(false, "13");
        assert_eq!(token_from_subject(&idx), Some("13"));
    }

    #[test]
    fn test_prefixes_match_sdk_constants() {
        // Pinned verbatim against growwapi-1.5.0 groww/constants.py::FeedConstants
        // — all 6, so a wrong-prefix regression (= silent no-data) fails the build.
        assert_eq!(EQUITY_NSE_LIVE_PRICE, "/ld/eq/nse/price.");
        assert_eq!(EQUITY_BSE_LIVE_PRICE, "/ld/eq/bse/price.");
        assert_eq!(DERIVATIVES_NSE_LIVE_PRICE, "/ld/fo/nse/price.");
        assert_eq!(DERIVATIVES_BSE_LIVE_PRICE, "/ld/fo/bse/price.");
        assert_eq!(NSE_LIVE_INDEX, "/ld/indices/nse/price.");
        assert_eq!(BSE_LIVE_INDEX, "/ld/indices/bse/price.");
    }
}
