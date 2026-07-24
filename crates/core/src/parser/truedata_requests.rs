//! TrueData (feed #4) client→server control requests.
//!
//! **Ground truth:** `TrueData Market Data API Documentation v 2.6`
//! pages 12–14 and 18, read directly. Every method name below is quoted
//! VERBATIM from the PDF — note they are **lowercase** (`addsymbol`, not
//! `addSymbol`); the camelCase spellings that appear in some third-party
//! write-ups are wrong.
//!
//! # The four documented requests
//!
//! ```json
//! {"method":"addsymbol","symbols":["NIFTY BANK","NIFTY-I"]}
//! {"method":"removesymbol","symbols":["MINDTREE"]}
//! {"method":"getmarketstatus"}
//! {"method":"logout"}
//! ```
//!
//! `{"method":"touchline"}` also exists but is **DEPRECATED** (v2.6 p.15) —
//! the touchline snapshot is pushed automatically on every subscribe, so we
//! never send it. There is no builder for it here by design.
//!
//! # Cold path
//!
//! These are control frames sent a handful of times per session (connect,
//! daily universe subscribe, shutdown) — NOT the tick hot path. They
//! allocate a `String`, which is correct and deliberate here; the hot-path
//! zero-allocation contract applies to `truedata::decode_trade_binary`.
//!
//! # Symbols are NAMES, not ids
//!
//! You subscribe by symbol NAME (`"NIFTY-I"`, `"RELIANCE"`, `"NIFTY 50"`,
//! `"RELIANCE_BSE"`); the numeric SymbolID comes BACK in the `addsymbol`
//! reply and is a **session-scoped routing key only** — never the persisted
//! `security_id` (scope-lock §10, identity contract).

/// Maximum symbols accepted in a single `addsymbol`/`removesymbol` request.
///
/// v2.6 p.13 states the cap is "As per your Plan's symbols limit" — i.e.
/// the account-level `maxsymbols` from the auth reply, with no separate
/// per-message cap documented. We nonetheless chunk requests so one frame
/// never grows unbounded; this bound is OURS (defensive), not the vendor's.
pub const TD_MAX_SYMBOLS_PER_REQUEST: usize = 100;

/// The `addsymbol` method literal (v2.6 p.12, verbatim, lowercase).
pub const TD_METHOD_ADD_SYMBOL: &str = "addsymbol";
/// The `removesymbol` method literal (v2.6 p.13, verbatim, lowercase).
pub const TD_METHOD_REMOVE_SYMBOL: &str = "removesymbol";
/// The `getmarketstatus` method literal (v2.6 p.14, verbatim, lowercase).
pub const TD_METHOD_GET_MARKET_STATUS: &str = "getmarketstatus";
/// The `logout` method literal (v2.6 p.18, verbatim, lowercase).
pub const TD_METHOD_LOGOUT: &str = "logout";

/// Escapes a symbol for embedding in a JSON string literal.
///
/// TrueData symbols contain spaces (`"NIFTY 50"`, `"NIFTY BANK"`) and
/// underscores (`"RELIANCE_BSE"`) but never quotes or backslashes in any
/// documented example. We escape anyway — a malformed symbol must produce
/// invalid-but-safe JSON, never a broken frame that desynchronises the
/// socket. Control characters are dropped.
fn push_escaped(out: &mut String, symbol: &str) {
    for ch in symbol.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
}

/// Builds a `{"method":"<method>","symbols":[...]}` frame.
fn build_symbol_request(method: &str, symbols: &[&str]) -> String {
    // Pre-size: method + JSON scaffolding + a generous per-symbol estimate.
    let estimated = 32usize
        .saturating_add(method.len())
        .saturating_add(symbols.len().saturating_mul(24));
    let mut out = String::with_capacity(estimated);
    out.push_str("{\"method\":\"");
    out.push_str(method);
    out.push_str("\",\"symbols\":[");
    for (index, symbol) in symbols.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('"');
        push_escaped(&mut out, symbol);
        out.push('"');
    }
    out.push_str("]}");
    out
}

/// Builds an `addsymbol` request (v2.6 p.12).
///
/// The reply carries `symbollist` — this is where the numeric SymbolID for
/// each name is learned, and it MUST be re-learned on every reconnect
/// (SymbolID stability across sessions is UNDOCUMENTED — scope-lock §10.6).
#[must_use]
pub fn build_add_symbol(symbols: &[&str]) -> String {
    build_symbol_request(TD_METHOD_ADD_SYMBOL, symbols)
}

/// Builds a `removesymbol` request (v2.6 p.13).
#[must_use]
pub fn build_remove_symbol(symbols: &[&str]) -> String {
    build_symbol_request(TD_METHOD_REMOVE_SYMBOL, symbols)
}

/// The complete `getmarketstatus` request frame (v2.6 p.14).
///
/// A compile-time constant: the frame takes no arguments, so there is
/// nothing to format and nothing to allocate.
pub const TD_REQUEST_GET_MARKET_STATUS: &str = r#"{"method":"getmarketstatus"}"#;

/// The complete `logout` request frame (v2.6 p.18).
///
/// A compile-time constant, for the same reason as
/// [`TD_REQUEST_GET_MARKET_STATUS`].
pub const TD_REQUEST_LOGOUT: &str = r#"{"method":"logout"}"#;

/// Returns the `getmarketstatus` request (v2.6 p.14).
///
/// The reply is a JSON object of per-segment states, e.g.
/// `{"success":true,"NSE_EQ":"CLOSED","NSE_FO":"CLOSED","MCX":"OPEN"}`.
///
/// Returns a `&'static str` — this frame is fixed, so it costs no
/// allocation at all.
#[must_use]
pub const fn build_get_market_status() -> &'static str {
    TD_REQUEST_GET_MARKET_STATUS
}

/// Returns the `logout` request (v2.6 p.18).
///
/// Sending this on graceful shutdown is what avoids the ghost-session
/// class: TrueData permits ONE login per key, and a dirty disconnect leaves
/// the server-side session held, forcing the force-logout HTTP call plus a
/// **1-minute** wait (v2.6 p.19) before the next login can succeed.
///
/// Returns a `&'static str` — fixed frame, zero allocation.
#[must_use]
pub const fn build_logout() -> &'static str {
    TD_REQUEST_LOGOUT
}

/// Splits a symbol universe into request-sized chunks.
///
/// Returns one JSON `addsymbol` frame per chunk. An empty universe yields
/// an empty Vec (we never send a pointless empty subscribe).
///
/// The result Vec is pre-sized to the exact chunk count — no growth
/// reallocation, no iterator `collect`.
#[must_use]
pub fn build_add_symbol_batches(symbols: &[&str]) -> Vec<String> {
    let chunk_count = symbols.len().div_ceil(TD_MAX_SYMBOLS_PER_REQUEST);
    let mut batches = Vec::with_capacity(chunk_count);
    for chunk in symbols.chunks(TD_MAX_SYMBOLS_PER_REQUEST) {
        batches.push(build_add_symbol(chunk));
    }
    batches
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_symbol_matches_documented_shape() {
        // v2.6 p.12 sample, verbatim shape.
        let got = build_add_symbol(&["NIFTY BANK", "NIFTY-I", "RELIANCE20DECFUT"]);
        assert_eq!(
            got,
            r#"{"method":"addsymbol","symbols":["NIFTY BANK","NIFTY-I","RELIANCE20DECFUT"]}"#
        );
    }

    #[test]
    fn test_build_remove_symbol_matches_documented_shape() {
        let got = build_remove_symbol(&["MINDTREE"]);
        assert_eq!(got, r#"{"method":"removesymbol","symbols":["MINDTREE"]}"#);
    }

    #[test]
    fn test_get_market_status_matches_documented_shape() {
        assert_eq!(build_get_market_status(), r#"{"method":"getmarketstatus"}"#);
    }

    #[test]
    fn test_logout_matches_documented_shape() {
        assert_eq!(build_logout(), r#"{"method":"logout"}"#);
    }

    #[test]
    fn test_method_literals_are_lowercase_verbatim() {
        // The PDF spells these lowercase; camelCase would be silently
        // rejected by the server, so pin them.
        assert_eq!(TD_METHOD_ADD_SYMBOL, "addsymbol");
        assert_eq!(TD_METHOD_REMOVE_SYMBOL, "removesymbol");
        assert_eq!(TD_METHOD_GET_MARKET_STATUS, "getmarketstatus");
        assert_eq!(TD_METHOD_LOGOUT, "logout");
    }

    #[test]
    fn test_symbols_with_spaces_and_suffixes_survive_verbatim() {
        // Index names carry spaces; BSE equities carry the _BSE suffix.
        let got = build_add_symbol(&["NIFTY 50", "INDIA VIX", "RELIANCE_BSE", "SENSEX"]);
        assert!(got.contains(r#""NIFTY 50""#));
        assert!(got.contains(r#""INDIA VIX""#));
        assert!(got.contains(r#""RELIANCE_BSE""#));
        assert!(got.contains(r#""SENSEX""#));
    }

    #[test]
    fn test_empty_symbol_list_produces_empty_array_not_panic() {
        assert_eq!(
            build_add_symbol(&[]),
            r#"{"method":"addsymbol","symbols":[]}"#
        );
    }

    #[test]
    fn test_build_add_symbol_batches_empty_universe() {
        assert!(build_add_symbol_batches(&[]).is_empty());
    }

    #[test]
    fn test_build_add_symbol_batches_split_at_cap() {
        let names: Vec<String> = (0..250).map(|i| format!("SYM{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let batches = build_add_symbol_batches(&refs);
        assert_eq!(batches.len(), 3, "250 symbols at cap 100 => 3 frames");
        // Every batch is a well-formed addsymbol frame.
        for b in &batches {
            assert!(b.starts_with(r#"{"method":"addsymbol","symbols":["#));
            assert!(b.ends_with("]}"));
        }
        // First and last symbol survive the chunking.
        assert!(batches[0].contains(r#""SYM0""#));
        assert!(batches[2].contains(r#""SYM249""#));
    }

    #[test]
    fn test_build_add_symbol_batches_exact_cap_one_frame() {
        let names: Vec<String> = (0..TD_MAX_SYMBOLS_PER_REQUEST)
            .map(|i| format!("S{i}"))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(build_add_symbol_batches(&refs).len(), 1);
    }

    #[test]
    fn test_quotes_and_backslashes_are_escaped_not_injected() {
        // Defensive: a malformed symbol must never break out of the string
        // literal and desynchronise the socket.
        let got = build_add_symbol(&["BAD\"NAME", "BACK\\SLASH"]);
        assert!(got.contains(r#"BAD\"NAME"#));
        assert!(got.contains(r"BACK\\SLASH"));
    }

    #[test]
    fn test_control_characters_are_dropped() {
        let got = build_add_symbol(&["NEW\nLINE\tTAB"]);
        assert!(!got.contains('\n'));
        assert!(!got.contains('\t'));
        assert!(got.contains("NEWLINETAB"));
    }

    #[test]
    fn test_single_symbol_request_is_well_formed() {
        let got = build_add_symbol(&["NIFTY-I"]);
        assert_eq!(got, r#"{"method":"addsymbol","symbols":["NIFTY-I"]}"#);
    }

    #[test]
    fn test_no_touchline_builder_exists() {
        // touchline is DEPRECATED (v2.6 p.15) and the snapshot arrives
        // automatically on subscribe. This test documents the deliberate
        // absence so nobody "helpfully" adds one back.
        let src = include_str!("truedata_requests.rs");
        // Split so this needle does not match its own occurrence here.
        let needle = concat!("pub fn ", "build_touchline");
        assert!(
            !src.contains(needle),
            "touchline is deprecated (v2.6 p.15) — do not add a builder"
        );
    }
}
