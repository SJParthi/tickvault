//! Dhan API endpoint coverage tests.
//!
//! Verifies the 45 known Dhan API endpoint URLs/path templates: 19
//! constants-backed paths (18 with LIVE consumers + 1 ORPHAN —
//! `/charts/historical`, whose consumers were deleted by the merged Phase
//! C-3 #1569; round-5 truth-sync 2026-07-15, MEASURED by
//! `test_charts_historical_constant_is_orphaned_post_phase_c3`), 19 inline
//! path templates in api_client.rs (MEASURED by a source scan — round-3
//! fix 2026-07-14: the inline count was previously a hardcoded scalar
//! nothing reconciled against the actual file; round-7 fix 2026-07-15:
//! the scan covers all three URL-build shapes — positional `"{}/…`,
//! captured-identifier `"{base}/…`, and split-argument-literal
//! `format!("{}{}", base, "/…")` — the latter two were previously
//! invisible, so a new endpoint in either shape dodged the pinned-set
//! equality; the residual uncovered shapes are disclosed on
//! `extract_inline_url_templates`), 1 overlap (`/positions`),
//! 2 live WebSocket URLs, 2 ORPHANED depth WebSocket URL constants
//! (round-8 truth-sync 2026-07-15: `DHAN_TWENTY_DEPTH_WS_BASE_URL` +
//! `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL` are RETAINED pub consts in
//! constants.rs with ZERO consumers since AWS-lifecycle PR #4 deleted the
//! depth infrastructure — the prior header narrated the CONSTANTS
//! THEMSELVES deleted and counted them nowhere; MEASURED by
//! `test_depth_ws_url_constants_are_orphaned`), and 4 intentionally
//! skipped endpoints — 45 in total. Prevents endpoint drift where new
//! endpoints are added to api_client.rs using inline strings without this
//! ledger (and its constants) being updated.
//!
//! Reference: docs/dhan-ref/*.md (21 reference files)

use std::fs;
use std::path::{Path, PathBuf};

use tickvault_common::constants::{
    // Conditional & Multi Order (1) — 2026-07-14
    DHAN_ALERTS_MULTI_ORDERS_PATH,
    // Historical data (2)
    DHAN_CHARTS_HISTORICAL_PATH,
    DHAN_CHARTS_INTRADAY_PATH,
    // Funds & margin (3)
    DHAN_FUND_LIMIT_PATH,
    // Authentication (2)
    DHAN_GENERATE_TOKEN_PATH,
    // IP management (3)
    DHAN_GET_IP_PATH,
    // Portfolio (3)
    DHAN_HOLDINGS_PATH,
    // Trader's control (2)
    DHAN_KILL_SWITCH_PATH,
    DHAN_MARGIN_CALCULATOR_MULTI_PATH,
    DHAN_MARGIN_CALCULATOR_PATH,
    DHAN_MODIFY_IP_PATH,
    // Option chain (2) — 2026-07-12 §8 rebuild, consumed by
    // crates/app/src/option_chain_1m_boot.rs (scheduled pull)
    DHAN_OPTION_CHAIN_EXPIRYLIST_PATH,
    DHAN_OPTION_CHAIN_PATH,
    DHAN_PNL_EXIT_PATH,
    DHAN_POSITIONS_CONVERT_PATH,
    DHAN_POSITIONS_PATH,
    DHAN_RENEW_TOKEN_PATH,
    DHAN_SET_IP_PATH,
    // User profile (1)
    DHAN_USER_PROFILE_PATH,
};

// PR #4 (2026-05-19) deleted the 20/200-level depth WebSocket
// INFRASTRUCTURE per the 4-IDX_I LOCKED_UNIVERSE operator lock
// (.claude/rules/project/websocket-connection-scope-lock.md). The
// `DHAN_TWENTY_DEPTH_WS_BASE_URL` + `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL`
// constants themselves were RETAINED (constants.rs) and are ORPHANED —
// zero consumers anywhere (round-8 truth-sync 2026-07-15: this comment
// previously narrated the constants deleted while both are live pub
// consts on the tree — the mirror image of the round-1 option-chain
// drift). Measured by `test_depth_ws_url_constants_are_orphaned` below;
// depth re-introduction is FORBIDDEN without a scope-lock rule edit FIRST.

// ---------------------------------------------------------------------------
// Test: All Dhan REST endpoint constants are defined and have correct paths
// ---------------------------------------------------------------------------

/// Verifies the 19 Dhan REST endpoint path constants in constants.rs, and
/// that each constant maps to the correct v2 API path. 18 of the 19 have
/// LIVE consumers; `/charts/historical` is ORPHANED (round-5 truth-sync
/// 2026-07-15: the merged Phase C-3 deletion — #1569, the 2026-07-13
/// Dhan-lane retirement — removed BOTH former consumers,
/// `crates/app/src/prev_day_ohlcv_boot.rs` + `cross_verify_1m_boot.rs`;
/// zero senders anywhere, pinned by
/// `test_charts_historical_constant_is_orphaned_post_phase_c3`).
///
/// Endpoint groups covered:
/// - Authentication: generateAccessToken, RenewToken
/// - Historical data: charts/intraday (LIVE — app spot-1m pull),
///   charts/historical (ORPHANED — constant only, zero consumers since
///   Phase C-3 #1569)
/// - Option chain: optionchain, optionchain/expirylist (2026-07-12 §8
///   rebuild — LIVE consumers in crates/app/src/option_chain_1m_boot.rs,
///   the per-minute scheduled pull; NOT api_client.rs)
/// - User profile: profile
/// - IP management: setIP, modifyIP, getIP
/// - Portfolio: holdings, positions, positions/convert
/// - Funds & margin: margincalculator, margincalculator/multi, fundlimit
/// - Trader's control: killswitch, pnlExit
/// - Conditional & Multi Order: alerts/multi/orders (2026-07-14)
#[test]
fn test_all_dhan_rest_endpoint_constants_defined() {
    // --- Authentication (docs/dhan-ref/02-authentication.md) ---
    assert_eq!(
        DHAN_GENERATE_TOKEN_PATH, "/app/generateAccessToken",
        "POST auth.dhan.co/app/generateAccessToken"
    );
    assert_eq!(
        DHAN_RENEW_TOKEN_PATH, "/RenewToken",
        "GET api.dhan.co/v2/RenewToken"
    );

    // --- Historical data (docs/dhan-ref/05-historical-data.md) ---
    // charts/intraday: LIVE — sole consumer is
    // crates/app/src/spot_1m_rest_boot.rs (the §8 per-minute scheduled
    // pull; APP crate, not core — round-5 attribution truth-sync).
    assert_eq!(
        DHAN_CHARTS_INTRADAY_PATH, "/charts/intraday",
        "POST api.dhan.co/v2/charts/intraday"
    );
    // charts/historical: ORPHANED — the merged Phase C-3 deletion (#1569,
    // 2026-07-13 Dhan-lane retirement) removed both former consumers
    // (prev_day_ohlcv_boot.rs + cross_verify_1m_boot.rs); the constant is
    // retained with ZERO senders anywhere (measured by
    // test_charts_historical_constant_is_orphaned_post_phase_c3). Retiring
    // the constant OR re-consuming it updates this ledger in the same PR —
    // never silently.
    assert_eq!(
        DHAN_CHARTS_HISTORICAL_PATH, "/charts/historical",
        "POST api.dhan.co/v2/charts/historical (ORPHANED — no live sender)"
    );

    // --- Option chain (docs/dhan-ref/06-option-chain.md) ---
    // Deleted 2026-06-28 with the retired core option_chain subsystem;
    // REBUILT 2026-07-12 as the app-crate per-minute scheduled pull
    // (no-rest-except-live-feed-2026-06-27.md §8; [option_chain_1m].enabled
    // = true in base.toml since 2026-07-13). Consumer:
    // crates/app/src/option_chain_1m_boot.rs — constants-backed, live.
    assert_eq!(
        DHAN_OPTION_CHAIN_PATH, "/optionchain",
        "POST api.dhan.co/v2/optionchain"
    );
    assert_eq!(
        DHAN_OPTION_CHAIN_EXPIRYLIST_PATH, "/optionchain/expirylist",
        "POST api.dhan.co/v2/optionchain/expirylist"
    );

    // --- User profile (docs/dhan-ref/02-authentication.md) ---
    assert_eq!(
        DHAN_USER_PROFILE_PATH, "/profile",
        "GET api.dhan.co/v2/profile"
    );

    // --- IP management (docs/dhan-ref/02-authentication.md) ---
    assert_eq!(
        DHAN_SET_IP_PATH, "/ip/setIP",
        "POST api.dhan.co/v2/ip/setIP"
    );
    assert_eq!(
        DHAN_MODIFY_IP_PATH, "/ip/modifyIP",
        "PUT api.dhan.co/v2/ip/modifyIP"
    );
    assert_eq!(DHAN_GET_IP_PATH, "/ip/getIP", "GET api.dhan.co/v2/ip/getIP");

    // --- Portfolio (docs/dhan-ref/12-portfolio-positions.md) ---
    assert_eq!(
        DHAN_HOLDINGS_PATH, "/holdings",
        "GET api.dhan.co/v2/holdings"
    );
    assert_eq!(
        DHAN_POSITIONS_PATH, "/positions",
        "GET/DELETE api.dhan.co/v2/positions"
    );
    assert_eq!(
        DHAN_POSITIONS_CONVERT_PATH, "/positions/convert",
        "POST api.dhan.co/v2/positions/convert"
    );

    // --- Funds & margin (docs/dhan-ref/13-funds-margin.md) ---
    assert_eq!(
        DHAN_MARGIN_CALCULATOR_PATH, "/margincalculator",
        "POST api.dhan.co/v2/margincalculator"
    );
    assert_eq!(
        DHAN_MARGIN_CALCULATOR_MULTI_PATH, "/margincalculator/multi",
        "POST api.dhan.co/v2/margincalculator/multi"
    );
    assert_eq!(
        DHAN_FUND_LIMIT_PATH, "/fundlimit",
        "GET api.dhan.co/v2/fundlimit"
    );

    // --- Trader's control (docs/dhan-ref/15-traders-control.md) ---
    assert_eq!(
        DHAN_KILL_SWITCH_PATH, "/killswitch",
        "POST/GET api.dhan.co/v2/killswitch"
    );
    assert_eq!(
        DHAN_PNL_EXIT_PATH, "/pnlExit",
        "POST/DELETE/GET api.dhan.co/v2/pnlExit"
    );

    // --- Conditional & Multi Order (docs/dhan-ref/07c-conditional-trigger.md) ---
    assert_eq!(
        DHAN_ALERTS_MULTI_ORDERS_PATH, "/alerts/multi/orders",
        "POST api.dhan.co/v2/alerts/multi/orders"
    );

    // --- Count verification ---
    // 19 REST endpoint path constants in constants.rs
    // (2026-07-14: +1 /alerts/multi/orders — Conditional & Multi Order family;
    // 2026-07-14 review fix: +2 option-chain constants — they were LIVE since
    // the 2026-07-12 §8 rebuild but absent from this ledger, which falsely
    // claimed them "no longer implemented";
    // 2026-07-15 round-5 truth-sync: DHAN_CHARTS_HISTORICAL_PATH stays in the
    // 19 as a CONSTANTS census entry but is ORPHANED — zero consumers since
    // the merged Phase C-3 #1569)
    let rest_paths: &[&str] = &[
        DHAN_GENERATE_TOKEN_PATH,
        DHAN_RENEW_TOKEN_PATH,
        DHAN_CHARTS_INTRADAY_PATH,
        DHAN_CHARTS_HISTORICAL_PATH,
        DHAN_OPTION_CHAIN_PATH,
        DHAN_OPTION_CHAIN_EXPIRYLIST_PATH,
        DHAN_USER_PROFILE_PATH,
        DHAN_SET_IP_PATH,
        DHAN_MODIFY_IP_PATH,
        DHAN_GET_IP_PATH,
        DHAN_HOLDINGS_PATH,
        DHAN_POSITIONS_PATH,
        DHAN_POSITIONS_CONVERT_PATH,
        DHAN_MARGIN_CALCULATOR_PATH,
        DHAN_MARGIN_CALCULATOR_MULTI_PATH,
        DHAN_FUND_LIMIT_PATH,
        DHAN_KILL_SWITCH_PATH,
        DHAN_PNL_EXIT_PATH,
        DHAN_ALERTS_MULTI_ORDERS_PATH,
    ];
    assert_eq!(
        rest_paths.len(),
        19,
        "Expected 19 REST endpoint path constants in constants.rs"
    );

    // All paths must start with '/'
    for path in rest_paths {
        assert!(
            path.starts_with('/'),
            "REST endpoint path must start with '/': {path}"
        );
    }

    // No duplicate paths
    let mut sorted = rest_paths.to_vec();
    sorted.sort_unstable();
    for window in sorted.windows(2) {
        assert_ne!(
            window[0], window[1],
            "Duplicate REST endpoint path: {}",
            window[0]
        );
    }
}

// ---------------------------------------------------------------------------
// Test: The two remaining WebSocket endpoint URLs are defined
// ---------------------------------------------------------------------------

/// PR #4 (2026-05-19) — under the 4-IDX_I LOCKED_UNIVERSE only two WebSocket
/// connections are spawned forever (see
/// `.claude/rules/project/websocket-connection-scope-lock.md`):
/// 1. Market feed (config-based): `wss://api-feed.dhan.co`
/// 2. Order update (config-based): `wss://api-order-update.dhan.co`
///
/// The previously-tested 20/200-level depth WS URL CONSTANTS still exist
/// in constants.rs but are ORPHANED — their consumers (the depth
/// pipelines) were deleted by PR #4; see
/// `test_depth_ws_url_constants_are_orphaned` (round-8 truth-sync
/// 2026-07-15 — this doc previously narrated the constants deleted). The
/// two LIVE URLs below live in `DhanConfig` (not in `constants.rs`)
/// because they are per-environment configurable, so we verify them via a
/// source scan of `config.rs`.
#[test]
fn test_all_websocket_urls_defined() {
    let config_source = include_str!("../src/config.rs");

    // Market feed WS default
    let market_feed_ws = "wss://api-feed.dhan.co";
    assert!(
        config_source.contains(market_feed_ws),
        "Market feed WS URL '{market_feed_ws}' must appear in config.rs defaults"
    );

    // Order update WS default
    let order_update_ws = "wss://api-order-update.dhan.co";
    assert!(
        config_source.contains(order_update_ws),
        "Order update WS URL '{order_update_ws}' must appear in config.rs defaults"
    );

    // Both must point at different hosts.
    assert_ne!(
        market_feed_ws, order_update_ws,
        "Main feed and order update WebSockets must use distinct hosts"
    );

    // Both must use wss:// (TLS required).
    for url in [market_feed_ws, order_update_ws] {
        assert!(
            url.starts_with("wss://"),
            "WebSocket URL must use TLS (wss://): {url}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test: Intentionally skipped endpoints are documented
// ---------------------------------------------------------------------------

/// Documents the 4 Dhan API endpoints intentionally NOT given constants.
///
/// These endpoints are skipped because:
/// - LTP/OHLC/Quote REST: We use Live Market Feed WebSocket for continuous
///   data instead of REST polling. The REST quote endpoints require a
///   `client-id` header and have a 1/sec rate limit, making them unsuitable
///   for our latency requirements.
/// - Per-segment instrument download: We use the public CSV download
///   (images.dhan.co) which is free and doesn't count against Data API rate
///   limits, unlike `GET /v2/instrument/{segment}` which requires auth and
///   counts against the 5/sec, 100K/day data API quota.
///
/// If any of these endpoints are implemented in the future, add a constant
/// to constants.rs and add it to `test_all_dhan_rest_endpoint_constants_defined`.
#[test]
fn test_skipped_endpoints_documented() {
    // Skipped endpoint 1: POST /v2/marketfeed/ltp
    // Reason: We use WebSocket Live Market Feed for real-time LTP.
    // Reference: docs/dhan-ref/11-market-quote-rest.md

    // Skipped endpoint 2: POST /v2/marketfeed/ohlc
    // Reason: We use WebSocket Live Market Feed for real-time OHLC.
    // Reference: docs/dhan-ref/11-market-quote-rest.md

    // Skipped endpoint 3: POST /v2/marketfeed/quote
    // Reason: We use WebSocket Live Market Feed for real-time full quotes.
    // Reference: docs/dhan-ref/11-market-quote-rest.md

    // Skipped endpoint 4: GET /v2/instrument/{exchangeSegment}
    // Reason: We use the public CSV at images.dhan.co which is free and
    // doesn't count against Data API rate limits (5/sec, 100K/day).
    // Reference: docs/dhan-ref/09-instrument-master.md

    let skipped_endpoints: &[(&str, &str, &str)] = &[
        (
            "POST /v2/marketfeed/ltp",
            "Use WebSocket Live Market Feed instead (zero REST polling)",
            "docs/dhan-ref/11-market-quote-rest.md",
        ),
        (
            "POST /v2/marketfeed/ohlc",
            "Use WebSocket Live Market Feed instead (zero REST polling)",
            "docs/dhan-ref/11-market-quote-rest.md",
        ),
        (
            "POST /v2/marketfeed/quote",
            "Use WebSocket Live Market Feed instead (zero REST polling)",
            "docs/dhan-ref/11-market-quote-rest.md",
        ),
        (
            "GET /v2/instrument/{exchangeSegment}",
            "Use public CSV at images.dhan.co (free, no auth, no rate limit)",
            "docs/dhan-ref/09-instrument-master.md",
        ),
    ];
    assert_eq!(
        skipped_endpoints.len(),
        4,
        "Exactly 4 endpoints intentionally skipped"
    );

    // Verify none of these paths accidentally exist as constants.
    // If they do, this test needs updating (move to the defined list).
    let rest_source = include_str!("../src/constants.rs");
    assert!(
        !rest_source.contains("\"/marketfeed/ltp\""),
        "LTP REST endpoint found in constants.rs — update dhan_api_coverage.rs"
    );
    assert!(
        !rest_source.contains("\"/marketfeed/ohlc\""),
        "OHLC REST endpoint found in constants.rs — update dhan_api_coverage.rs"
    );
    assert!(
        !rest_source.contains("\"/marketfeed/quote\""),
        "Quote REST endpoint found in constants.rs — update dhan_api_coverage.rs"
    );
    assert!(
        !rest_source.contains("\"/instrument/\""),
        "Per-segment instrument endpoint found in constants.rs — update dhan_api_coverage.rs"
    );
}

// ---------------------------------------------------------------------------
// Test: OMS endpoints that use inline paths in api_client.rs
// ---------------------------------------------------------------------------

/// Normalizes every `{…}` placeholder run in a path template to the bare
/// `{}` form, so a captured-identifier template
/// (`"{base}/orders/{order_id}"`) and its positional twin
/// (`"{}/orders/{}"`) yield the SAME ledger entry.
fn normalize_placeholders(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(character) = chars.next() {
        if character == '{' {
            for inner in chars.by_ref() {
                if inner == '}' {
                    break;
                }
            }
            out.push_str("{}");
        } else {
            out.push(character);
        }
    }
    out
}

/// Extracts the unique inline URL path TEMPLATES built in api_client.rs'
/// production region. THREE URL-build shapes are extracted (round-7 —
/// the original scan matched ONLY shape 1's `"{}/` needle, so a new
/// endpoint written in shape 2 or 3 shipped invisible to the pinned-set
/// equality this ledger's drift claim rests on):
///
/// 1. Positional base placeholder —
///    `format!("{}/orders/{}", self.base_url, order_id)` → `/orders/{}`.
/// 2. Captured-identifier base — `format!("{base}/orders/{order_id}")` →
///    `/orders/{}` (placeholders normalized, so both spellings map to one
///    entry).
/// 3. Split-argument literal — `format!("{}{}", self.base_url, "/orders")`
///    → `/orders` (the FIRST string literal argument of a `"{}{}`-prefixed
///    concat format, when it starts with `/`).
///
/// Query strings are cut at `?`; placeholders are kept (normalized to
/// `{}`) so parameterized templates count distinctly; constants-backed
/// sites (`format!("{}{}", base, constants::DHAN_*_PATH)`) carry NO
/// literal path argument and are deliberately NOT extracted (they are the
/// constants side of the ledger). Returns a sorted, deduped set.
///
/// HONEST ENVELOPE: this is a text scan over the three shapes above, not
/// a parser. NOT covered (a new endpoint in one of these shapes would
/// dodge the pinned-set equality and must be caught by human review):
/// byte-assembled / `push_str`-built URLs, `reqwest::Url::join`, a split
/// literal that is not the first string literal of its statement, and a
/// concat format string with a literal tail (`"{}{}/x"`). None exist in
/// api_client.rs today; the `/alerts` family additionally has its own
/// token-general census in
/// `crates/trading/tests/conditional_gate_guard.rs`.
fn extract_inline_url_templates(production: &str) -> Vec<String> {
    let mut templates = std::collections::BTreeSet::new();

    // Shape 1: positional base placeholder — `"{}/…`.
    let positional_needle = "\"{}/";
    let mut search_from = 0;
    while let Some(position) = production[search_from..].find(positional_needle) {
        // Path starts at the '/' after the `"{}` base placeholder.
        let path_start = search_from + position + 3;
        let rest = &production[path_start..];
        let end = rest.find('"').unwrap_or(rest.len());
        let raw = &rest[..end];
        let template = raw.split('?').next().unwrap_or(raw);
        templates.insert(normalize_placeholders(template));
        search_from = path_start + end;
    }

    // Shape 2: captured-identifier base — `"{base}/…` (round-7).
    let mut search_from = 0;
    while let Some(position) = production[search_from..].find("\"{") {
        let ident_start = search_from + position + 2;
        search_from = ident_start;
        let ident_len = production[ident_start..]
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .map(char::len_utf8)
            .sum::<usize>();
        if ident_len == 0 {
            continue;
        }
        let rest = &production[ident_start + ident_len..];
        let Some(after_brace) = rest.strip_prefix('}') else {
            continue;
        };
        if !after_brace.starts_with('/') {
            continue;
        }
        let end = after_brace.find('"').unwrap_or(after_brace.len());
        let raw = &after_brace[..end];
        let template = raw.split('?').next().unwrap_or(raw);
        templates.insert(normalize_placeholders(template));
    }

    // Shape 3: split-argument literal — a `"{}{}`-prefixed concat format
    // string whose argument list carries a string literal starting with
    // `/` (round-7). The scan is bounded to the statement (`;`), and a
    // constants-backed argument list carries no string literal at all.
    let concat_needle = "\"{}{}";
    let mut search_from = 0;
    while let Some(position) = production[search_from..].find(concat_needle) {
        let fmt_body_start = search_from + position + 1;
        search_from = fmt_body_start + concat_needle.len();
        let Some(fmt_close) = production[fmt_body_start..].find('"') else {
            break;
        };
        let args_start = fmt_body_start + fmt_close + 1;
        let statement_end = production[args_start..]
            .find(';')
            .map_or(production.len(), |offset| args_start + offset);
        let args = &production[args_start..statement_end];
        if let Some(literal_open) = args.find('"') {
            let literal_body = &args[literal_open + 1..];
            if literal_body.starts_with('/') {
                let end = literal_body.find('"').unwrap_or(literal_body.len());
                let raw = &literal_body[..end];
                let template = raw.split('?').next().unwrap_or(raw);
                templates.insert(normalize_placeholders(template));
            }
        }
    }

    templates.into_iter().collect()
}

/// Self-test for [`extract_inline_url_templates`] (vacuous-pass defense):
/// the extractor must see inline templates in ALL THREE URL-build shapes
/// (positional, captured-identifier, split-argument literal — round-7:
/// the latter two previously shipped invisible), normalize captured
/// placeholders, dedup repeats, strip query strings, and skip
/// constants-backed sites (single-line AND multi-line, with or without a
/// query tail in the format string).
#[test]
fn test_inline_template_extractor_self_test() {
    let synthetic = concat!(
        "let a = format!(\"{}/orders\", self.base_url);\n",
        "let b = format!(\"{}/orders/{}\", self.base_url, order_id);\n",
        "let c = format!(\"{}/orders\", self.base_url);\n", // dup of a
        "let d = format!(\"{}/ledger?from-date={}&to-date={}\", self.base_url, f, t);\n",
        "let e = format!(\"{}{}\", self.base_url, constants::DHAN_HOLDINGS_PATH);\n",
        // Shape 2 (round-7): captured-identifier base, with and without a
        // captured path parameter — normalized to the positional spelling.
        "let f = format!(\"{base}/margins\");\n",
        "let g = format!(\"{base}/margins/{order_id}\");\n",
        // Shape 3 (round-7): split-argument literal, single-line,
        // multi-line, and with a query string inside the literal.
        "let h = format!(\"{}{}\", self.base_url, \"/split\");\n",
        "let i = format!(\n    \"{}{}\",\n    self.base_url,\n    \"/multiline\"\n);\n",
        "let j = format!(\"{}{}\", self.base_url, \"/q?x=1\");\n",
        // Constants-backed multi-line + query-tail concat sites carry NO
        // literal argument and stay skipped (the constants side of the
        // ledger).
        "let k = format!(\n    \"{}{}\",\n    self.base_url,\n    constants::DHAN_POSITIONS_PATH\n);\n",
        "let l = format!(\"{}{}?killSwitchStatus=ACTIVATE\", self.base_url, constants::DHAN_KILL_SWITCH_PATH);\n",
    );
    assert_eq!(
        extract_inline_url_templates(synthetic),
        vec![
            "/ledger".to_string(),
            "/margins".to_string(),
            "/margins/{}".to_string(),
            "/multiline".to_string(),
            "/orders".to_string(),
            "/orders/{}".to_string(),
            "/q".to_string(),
            "/split".to_string(),
        ],
        "extractor must cover all three URL-build shapes, normalize \
         captured placeholders, dedup, strip query strings, keep {{}} \
         params, and skip constants-backed sites"
    );
}

/// Verifies the OMS REST endpoint ledger for the trading surface.
///
/// TWO DISTINCT COUNTS, kept honest (2026-07-14 round-3 reconciliation —
/// the round-2 header's "35 unique paths = 16 inline + 19 constants"
/// arithmetic was irreproducible: its own breakdown tables enumerate 18
/// unique inline templates, the real api_client.rs carries 19 — incl.
/// get_positions' inline `/positions`, which the header mislabeled
/// constants-backed — and `websocket_count = 4` contradicted this file's
/// own two-WS test):
///
/// * **37 unique endpoint path TEMPLATES** = 19 inline templates built in
///   api_client.rs (MEASURED below by a source scan of its production
///   region, pinned against an explicit list) + 19 constants-backed
///   `DHAN_*_PATH` paths from constants.rs − 1 overlap (`/positions` is
///   BOTH inline in get_positions AND constants-backed in
///   exit_all_positions; the inline retrofit is a flagged follow-up). The
///   constants are consumed ACROSS the workspace — api_client.rs for the
///   OMS families, but also core (auth/RenewToken/profile via
///   token_manager.rs + ip via ip_verifier.rs) and app (charts/intraday
///   spot-1m + option-chain per-minute pulls) — NOT all "in
///   api_client.rs". `/charts/historical` has NO consumer at all
///   (ORPHANED since the merged Phase C-3 #1569 — round-5 truth-sync
///   2026-07-15; the prior "core (auth/RenewToken/ip/charts)" attribution
///   was wrong for BOTH charts constants: intraday's consumer is the APP
///   crate and historical's consumers are deleted).
/// * **41 per-method REST OPERATIONS** enumerated in the breakdown below
///   (several operations share one path, e.g. PUT/DELETE/GET on
///   /orders/{order-id}) — plus exit-all's DELETE reusing the shared
///   /positions path as a 42nd operation on an already-counted path. The
///   operations list scopes the OMS/option-chain/portfolio/funds/control
///   families; the auth/ip/charts/profile constants are single-operation
///   paths counted on the constants side (charts/historical: ORPHANED —
///   see above). (Round-4 reconciliation: the
///   round-3 "38" scalar counted trader's control PER-PATH (2) while this
///   file's own annotations pin POST/GET /killswitch + POST/DELETE/GET
///   /pnlExit = 5 method+path operations — matching api_client.rs' 6
///   control fns, activate/deactivate sharing POST /killswitch via query
///   param. The scalar is now MECHANICAL: the tally assert in the test
///   body sums the family counts to 41.)
///
/// The inline templates are MEASURED from the file, so the ledger's
/// drift-detection purpose is mechanical: a new inline endpoint (or a
/// template change) in any of the three extracted URL-build shapes —
/// positional `"{}/…`, captured-identifier `"{base}/…`, or
/// split-argument-literal `format!("{}{}", base, "/…")` (round-7) — fails
/// the pinned-set equality below. Residual uncovered shapes
/// (byte-assembled / `push_str` / `Url::join` builds) are disclosed on
/// `extract_inline_url_templates` — human-review territory, not a
/// mechanical claim.
///
/// The 41 listed operations break down as:
/// - Orders (9): place, modify, cancel, order-book, single-order, by-correlation, trade-book, trades-by-order, slicing
/// - Super orders (4): place, modify, cancel, list
/// - Forever orders (4): create, modify, delete, list
/// - Conditional & Multi Order (6): create, modify, delete, get-one, get-all, place-multi (2026-07-14; place-multi is constants-backed)
/// - EDIS (3): tpin, form, inquire
/// - Statements (2): ledger, trade-history
/// - Option chain via constants (2): optionchain, expirylist — core client
///   deleted 2026-06-28, REBUILT 2026-07-12 as the app-crate per-minute
///   scheduled pull (crates/app/src/option_chain_1m_boot.rs; counted in the
///   19 constants, NOT in api_client.rs)
/// - Portfolio (3): holdings + positions/convert via constants;
///   get-positions builds INLINE `/positions` (round-3 truth-sync — the
///   prior "via constant" label was wrong for this one; exit-all is the
///   consumer of DHAN_POSITIONS_PATH, and the inline retrofit is a flagged
///   follow-up)
/// - Funds/margin via constant (3): margin-calc POST, margin-multi POST,
///   fund-limit GET
/// - Trader's control via constant (5): killswitch POST/GET, pnl-exit
///   POST/DELETE/GET — per this file's own path annotations and
///   api_client.rs' 6 control fns (activate_kill_switch /
///   deactivate_kill_switch share POST /killswitch via query param;
///   get_kill_switch_status; configure_pnl_exit / stop_pnl_exit /
///   get_pnl_exit_status). The round-3 per-path "2" contradicted both.
/// - Exit-all (DELETE /positions) reuses DHAN_POSITIONS_PATH — a distinct
///   operation on a path already inside the 37 (asserted below, not part
///   of the 41-item list)
///
/// Total unique path templates: 19 (constants — incl. /alerts/multi/orders
/// added 2026-07-14 and the 2 option-chain constants, live since the
/// 2026-07-12 §8 rebuild) + 19 (inline in api_client.rs, MEASURED) − 1
/// (the /positions overlap) = 37
/// Plus 2 LIVE WebSocket URLs (the two-WS lock —
/// `test_all_websocket_urls_defined`
/// in THIS file; the round-2 scalar 4 contradicted it) = 39 implemented
/// URLs/templates — of which `/charts/historical` is ORPHANED (constant
/// retained, ZERO consumers since the merged Phase C-3 #1569 — round-5
/// truth-sync 2026-07-15), so 38 have live senders/consumers today.
/// Plus 2 ORPHANED depth WebSocket URL constants (round-8 truth-sync
/// 2026-07-15: `DHAN_TWENTY_DEPTH_WS_BASE_URL` +
/// `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL` — retained pub consts, ZERO
/// consumers since PR #4; NOT in the live websocket_count, which the
/// two-WS lock pins at 2 CONNECTIONS; measured by
/// `test_depth_ws_url_constants_are_orphaned`).
/// Grand ledger total: 45 = 39 implemented + 2 orphaned depth WS
/// constants + 4 intentionally skipped
#[test]
fn test_oms_inline_endpoint_paths_documented() {
    // --- Orders (docs/dhan-ref/07-orders.md) ---
    // 9 endpoints, 5 unique base paths
    let order_paths: &[(&str, &str)] = &[
        ("/orders", "POST — place order"),
        ("/orders/{order-id}", "PUT — modify order"),
        ("/orders/{order-id}", "DELETE — cancel order"),
        ("/orders/{order-id}", "GET — single order"),
        ("/orders", "GET — order book"),
        (
            "/orders/external/{correlation-id}",
            "GET — by correlation ID",
        ),
        ("/orders/slicing", "POST — order slicing"),
        ("/trades", "GET — trade book"),
        ("/trades/{order-id}", "GET — trades by order"),
    ];
    assert_eq!(order_paths.len(), 9, "Orders: 9 endpoint operations");

    // --- Super orders (docs/dhan-ref/07a-super-order.md) ---
    let super_order_paths: &[(&str, &str)] = &[
        ("/super/orders", "POST — place super order"),
        ("/super/orders/{order-id}", "PUT — modify super order"),
        (
            "/super/orders/{order-id}/{leg}",
            "DELETE — cancel super order leg",
        ),
        ("/super/orders", "GET — list super orders"),
    ];
    assert_eq!(
        super_order_paths.len(),
        4,
        "Super orders: 4 endpoint operations"
    );

    // --- Forever orders (docs/dhan-ref/07b-forever-order.md) ---
    let forever_order_paths: &[(&str, &str)] = &[
        ("/forever/orders", "POST — create forever order"),
        ("/forever/orders/{order-id}", "PUT — modify forever order"),
        (
            "/forever/orders/{order-id}",
            "DELETE — delete forever order",
        ),
        ("/forever/orders", "GET — list all forever orders"),
    ];
    assert_eq!(
        forever_order_paths.len(),
        4,
        "Forever orders: 4 endpoint operations"
    );

    // --- Conditional & Multi Order (docs/dhan-ref/07c-conditional-trigger.md) ---
    // 2026-07-14: /alerts/multi/orders added (constants-backed via
    // DHAN_ALERTS_MULTI_ORDERS_PATH — the 5 legacy /alerts/orders paths stay
    // inline in api_client.rs; retrofit is a flagged follow-up).
    let conditional_paths: &[(&str, &str)] = &[
        ("/alerts/orders", "POST — create conditional trigger"),
        ("/alerts/orders/{alertId}", "PUT — modify trigger"),
        ("/alerts/orders/{alertId}", "DELETE — delete trigger"),
        ("/alerts/orders/{alertId}", "GET — get single trigger"),
        ("/alerts/orders", "GET — list all triggers"),
        (
            "/alerts/multi/orders",
            "POST — place multi order (up to 15 sequence-keyed legs)",
        ),
    ];
    assert_eq!(
        conditional_paths.len(),
        6,
        "Conditional & Multi Order: 6 endpoint operations"
    );

    // --- EDIS (docs/dhan-ref/07d-edis.md) ---
    let edis_paths: &[(&str, &str)] = &[
        ("/edis/tpin", "GET — generate T-PIN"),
        ("/edis/form", "POST — eDIS form"),
        ("/edis/inquire/{isin}", "GET — eDIS inquiry"),
    ];
    assert_eq!(edis_paths.len(), 3, "EDIS: 3 endpoint operations");

    // --- Statements (docs/dhan-ref/14-statements-trade-history.md) ---
    let statement_paths: &[(&str, &str)] = &[
        ("/ledger", "GET — ledger with query params"),
        (
            "/trades/{from-date}/{to-date}/{page}",
            "GET — trade history (path params, 0-indexed)",
        ),
    ];
    assert_eq!(
        statement_paths.len(),
        2,
        "Statements: 2 endpoint operations"
    );

    // --- Option chain — deleted 2026-06-28, REBUILT 2026-07-12 (§8) ---
    // The core option_chain REST client (crates/core/src/option_chain/) was
    // deleted with the retired subsystem (operator directive 2026-06-28).
    // A NEW per-minute scheduled-pull surface was authorized 2026-07-12
    // (no-rest-except-live-feed-2026-06-27.md §8) and is LIVE: constants
    // DHAN_OPTION_CHAIN_PATH + DHAN_OPTION_CHAIN_EXPIRYLIST_PATH are
    // consumed by crates/app/src/option_chain_1m_boot.rs (enabled in
    // base.toml since 2026-07-13). Both are counted in the 19
    // constants-backed endpoints above — there is NO api_client.rs sender.

    // --- Exit all positions ---
    // Uses DELETE on DHAN_POSITIONS_PATH (/positions), already counted above.
    // This is a distinct operation (DELETE vs GET) on the same path.
    assert_eq!(
        DHAN_POSITIONS_PATH, "/positions",
        "Exit-all shares the /positions path (DELETE method)"
    );

    // --- Per-method OPERATIONS tally (MEASURED — round-4 fix 2026-07-14) ---
    // The round-3 header's "38 operations" scalar was irreproducible
    // narrative: it counted trader's control PER-PATH (2) while this file's
    // own annotations pin POST/GET /killswitch + POST/DELETE/GET /pnlExit
    // = 5 method+path operations (api_client.rs implements all 6 control
    // fns; activate/deactivate share POST /killswitch via query param).
    // The tally below makes the scalar mechanical: a drift in any family's
    // operation count fails this assert instead of hiding in prose.
    let listed_family_ops = order_paths.len()
        + super_order_paths.len()
        + forever_order_paths.len()
        + conditional_paths.len()
        + edis_paths.len()
        + statement_paths.len();
    assert_eq!(
        listed_family_ops, 28,
        "28 operations enumerated in the per-family lists above \
         (9 orders + 4 super + 4 forever + 6 conditional/multi + 3 EDIS + \
         2 statements)"
    );
    let option_chain_ops: usize = 2; // POST optionchain + POST expirylist (app-crate consumer)
    let portfolio_ops: usize = 3; // GET holdings + GET positions (inline) + POST convert
    let funds_margin_ops: usize = 3; // POST margincalculator + POST …/multi + GET fundlimit
    let traders_control_ops: usize = 5; // POST/GET killswitch + POST/DELETE/GET pnlExit
    let per_method_operations = listed_family_ops
        + option_chain_ops
        + portfolio_ops
        + funds_margin_ops
        + traders_control_ops;
    assert_eq!(
        per_method_operations, 41,
        "41 per-method REST operations across the scoped families — plus \
         exit-all's DELETE /positions as the 42nd operation on an \
         already-counted path (asserted above)"
    );

    // --- Inline template census (MEASURED — round-3 fix 2026-07-14) ---
    // Nothing previously counted the actual file: the round-2 "16 inline"
    // scalar was irreproducible narrative (the file carries 19 unique
    // inline templates, and the breakdown tables enumerated 18). The scan
    // + pinned-set equality below make inline-endpoint drift mechanical.
    let api_client_source = fs::read_to_string("../trading/src/oms/api_client.rs")
        .expect("api_client.rs must be readable for the inline endpoint census");
    let production = api_client_source
        .split("#[cfg(test)]\nmod tests")
        .next()
        .expect("split always yields a first segment");
    let inline_templates = extract_inline_url_templates(production);
    let expected_inline_templates: [&str; 19] = [
        "/alerts/orders",
        "/alerts/orders/{}",
        "/edis/form",
        "/edis/inquire/{}",
        "/edis/tpin",
        "/forever/orders",
        "/forever/orders/{}",
        "/ledger",
        "/orders",
        "/orders/external/{}",
        "/orders/slicing",
        "/orders/{}",
        "/positions",
        "/super/orders",
        "/super/orders/{}",
        "/super/orders/{}/{}",
        "/trades",
        "/trades/{}",
        "/trades/{}/{}/{}",
    ];
    assert_eq!(
        inline_templates, expected_inline_templates,
        "the MEASURED api_client.rs inline URL template set must equal the \
         pinned 19-template ledger — a new/changed inline endpoint updates \
         this list (and the totals below) in the same PR"
    );
    let parameterized_inline = inline_templates
        .iter()
        .filter(|template| template.contains("{}"))
        .count();
    assert_eq!(
        parameterized_inline, 9,
        "9 of the 19 inline templates are parameterized ({{}} path segments)"
    );
    // The single constants↔inline overlap: get_positions builds an INLINE
    // /positions while exit_all_positions consumes DHAN_POSITIONS_PATH
    // (the inline retrofit is a flagged follow-up).
    assert!(
        inline_templates.iter().any(|t| t == "/positions"),
        "the /positions overlap template must be in the inline set"
    );

    // --- Grand total (every scalar reproducible from this file) ---
    // 19 constants-backed REST endpoints (pinned by
    //   test_all_dhan_rest_endpoint_constants_defined; incl.
    //   /alerts/multi/orders 2026-07-14 + the 2 option-chain constants,
    //   live since the 2026-07-12 §8 rebuild; /charts/historical is
    //   ORPHANED — constant retained, zero consumers since Phase C-3
    //   #1569, measured by
    //   test_charts_historical_constant_is_orphaned_post_phase_c3)
    // + 19 inline templates in api_client.rs (MEASURED above)
    // − 1 overlap (/positions: inline get_positions + constants exit-all)
    // + 2 WebSocket endpoints (the two-WS lock —
    //   test_all_websocket_urls_defined; the round-2 scalar 4 contradicted
    //   that test in this same file)
    // + 2 ORPHANED depth WebSocket URL constants (round-8 truth-sync
    //   2026-07-15: DHAN_TWENTY_DEPTH_WS_BASE_URL +
    //   DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL are RETAINED pub consts in
    //   constants.rs with ZERO consumers since PR #4 deleted the depth
    //   infrastructure — the prior census narrated them "deleted" and
    //   counted these two KNOWN wss:// endpoints nowhere; measured by
    //   test_depth_ws_url_constants_are_orphaned. Deliberately NOT in the
    //   LIVE websocket_count — the two-WS lock pins 2 CONNECTIONS forever,
    //   and depth re-introduction requires a scope-lock rule edit FIRST.)
    let constants_rest_count: usize = 19;
    let inline_template_count = inline_templates.len();
    let overlap_paths: usize = 1; // "/positions"
    let websocket_count: usize = 2;
    let orphaned_depth_ws_constant_count: usize = 2;
    let skipped_count: usize = 4;

    let unique_path_templates = constants_rest_count + inline_template_count - overlap_paths;
    assert_eq!(
        unique_path_templates, 37,
        "37 unique endpoint path templates (19 constants + 19 measured \
         inline − the /positions overlap)"
    );

    let total_implemented = unique_path_templates + websocket_count;
    assert_eq!(
        total_implemented, 39,
        "39 implemented endpoint URLs/templates (37 unique path templates \
         + 2 WebSocket URLs; incl. the ORPHANED /charts/historical — \
         constant retained, zero consumers since Phase C-3 #1569)"
    );

    // Plus the 2 orphaned depth WS URL constants + the 4 intentionally
    // skipped = total Dhan API endpoints known
    let total_known = total_implemented + orphaned_depth_ws_constant_count + skipped_count;
    assert_eq!(
        total_known, 45,
        "45 total known Dhan API endpoint URLs/templates (39 implemented + \
         2 orphaned depth WS constants + 4 skipped)"
    );
}

// ---------------------------------------------------------------------------
// Test: REST base URLs in DhanConfig default to v2
// ---------------------------------------------------------------------------

/// Verifies the DhanConfig defaults use the correct v2 base URLs.
/// Dhan v1 is deprecated. All REST calls must use v2.
/// Reference: docs/dhan-ref/01-introduction-and-rate-limits.md
#[test]
fn test_dhan_config_base_urls_are_v2() {
    let config_source = include_str!("../src/config.rs");

    // REST API base URL must be v2
    let rest_base = "https://api.dhan.co/v2";
    assert!(
        config_source.contains(rest_base),
        "REST API base URL '{rest_base}' must appear in config.rs defaults"
    );
    assert!(
        rest_base.ends_with("/v2"),
        "REST API base URL must end with /v2 (v1 is deprecated)"
    );
    assert!(rest_base.starts_with("https://"), "REST API must use HTTPS");

    // Auth base URL
    let auth_base = "https://auth.dhan.co";
    assert!(
        config_source.contains(auth_base),
        "Auth base URL '{auth_base}' must appear in config.rs defaults"
    );
    assert!(
        auth_base.starts_with("https://"),
        "Auth base URL must use HTTPS"
    );
}

// ---------------------------------------------------------------------------
// Test: Path constants don't contain full URLs (separation of concerns)
// ---------------------------------------------------------------------------

/// Verifies that REST path constants contain only the path portion,
/// not the full URL. The base URL is provided by DhanConfig at runtime.
#[test]
fn test_path_constants_are_paths_not_full_urls() {
    let paths: &[&str] = &[
        DHAN_GENERATE_TOKEN_PATH,
        DHAN_RENEW_TOKEN_PATH,
        DHAN_CHARTS_INTRADAY_PATH,
        DHAN_CHARTS_HISTORICAL_PATH,
        DHAN_OPTION_CHAIN_PATH,
        DHAN_OPTION_CHAIN_EXPIRYLIST_PATH,
        DHAN_USER_PROFILE_PATH,
        DHAN_SET_IP_PATH,
        DHAN_MODIFY_IP_PATH,
        DHAN_GET_IP_PATH,
        DHAN_HOLDINGS_PATH,
        DHAN_POSITIONS_PATH,
        DHAN_POSITIONS_CONVERT_PATH,
        DHAN_MARGIN_CALCULATOR_PATH,
        DHAN_MARGIN_CALCULATOR_MULTI_PATH,
        DHAN_FUND_LIMIT_PATH,
        DHAN_KILL_SWITCH_PATH,
        DHAN_PNL_EXIT_PATH,
        DHAN_ALERTS_MULTI_ORDERS_PATH,
    ];
    for path in paths {
        assert!(
            !path.contains("https://"),
            "Path constant must not contain full URL: {path}"
        );
        assert!(
            !path.contains("http://"),
            "Path constant must not contain HTTP URL: {path}"
        );
        assert!(
            !path.contains("api.dhan.co"),
            "Path constant must not contain hostname: {path}"
        );
    }
}

// PR #4 (2026-05-19): `test_depth_websocket_urls_correct_hostnames` retired
// alongside the deleted 20/200-level depth WebSocket INFRASTRUCTURE (the
// URL constants themselves remain, ORPHANED — see
// `test_depth_ws_url_constants_are_orphaned` below; round-8 truth-sync
// 2026-07-15). See the header comment after the imports for the operator
// lock reference.

// ---------------------------------------------------------------------------
// Test: /charts/historical is ORPHANED post-Phase-C-3 (measured, not narrated)
// ---------------------------------------------------------------------------

/// Recursively collects every `.rs` file under `dir` whose path contains
/// `/src/` (production trees only — `target`, `tests/` and `benches/` dirs
/// excluded). Mirror of the gate-guard walker in
/// `crates/trading/tests/conditional_gate_guard.rs`.
fn collect_workspace_src_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if name == "target" || name == "tests" || name == "benches" {
                continue;
            }
            collect_workspace_src_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
            && path.to_string_lossy().contains("/src/")
        {
            out.push(path);
        }
    }
}

/// Pins the ORPHAN status of `DHAN_CHARTS_HISTORICAL_PATH` (round-5 ledger
/// truth-sync, 2026-07-15): the merged Phase C-3 deletion (#1569, the
/// 2026-07-13 Dhan-lane retirement) removed BOTH former consumers
/// (`crates/app/src/prev_day_ohlcv_boot.rs` + `cross_verify_1m_boot.rs`),
/// so the constant is retained with ZERO senders anywhere — a de-facto
/// unimplemented endpoint that must not be silently re-consumed OR
/// silently narrated as live. This scan makes the ledger's orphan
/// annotations SELF-TRUING: re-adding a consumer (any
/// `DHAN_CHARTS_HISTORICAL_PATH` use, or an inline `/charts/historical`
/// literal on a CODE line) fails HERE until the annotations in this file
/// move orphan → live in the same PR; deleting the constant instead fails
/// `test_all_dhan_rest_endpoint_constants_defined` until the counts move.
/// (The round-1 confirmed drift was the mirror image — option-chain
/// endpoints narrated "no longer implemented" while LIVE; nothing
/// mechanical pinned constants→consumer existence in either direction
/// until this test.) FULL-LINE comment lines are excluded — several
/// modules legitimately MENTION the retired endpoint in doc comments
/// (tick_types.rs, …); a comment cannot
/// send HTTP. A trailing-comment mention on a code line would over-count
/// and fail LOUDLY — the conservative direction for an orphan pin.
#[test]
fn test_charts_historical_constant_is_orphaned_post_phase_c3() {
    let mut files = Vec::new();
    collect_workspace_src_files(Path::new("../../crates"), &mut files);
    assert!(
        files.len() > 50,
        "the orphan scan must see the workspace production tree (found {} files)",
        files.len()
    );
    let mut consumers = Vec::new();
    for file in files {
        let path_text = file.to_string_lossy().replace('\\', "/");
        // The declaration site itself — separator-anchored (the gate-guard
        // round-5 lesson: a bare ends_with would also skip a rogue
        // `mycommon/src/constants.rs`).
        if path_text.ends_with("/common/src/constants.rs") {
            continue;
        }
        let Ok(source) = fs::read_to_string(&file) else {
            continue;
        };
        let has_code_mention = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .any(|line| {
                line.contains("DHAN_CHARTS_HISTORICAL_PATH") || line.contains("/charts/historical")
            });
        if has_code_mention {
            consumers.push(path_text);
        }
    }
    assert!(
        consumers.is_empty(),
        "DHAN_CHARTS_HISTORICAL_PATH is ORPHANED (the merged Phase C-3 \
         #1569 deleted its consumers) — a new consumer/code mention must \
         update this ledger's orphan annotations (orphan -> live) in the \
         same PR. Found:\n{}",
        consumers.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Test: the 2 depth WS URL constants are ORPHANED (measured, not narrated)
// ---------------------------------------------------------------------------

/// Production region of a source file: everything before the
/// `#[cfg(test)]\nmod tests` module marker (mirror of the gate-guard
/// splitter in `crates/trading/tests/conditional_gate_guard.rs`). Needed
/// here because `crates/core/src/websocket/tls.rs` legitimately carries a
/// `full-depth-api.dhan.co` literal inside a TEST assert message (the
/// 2026-04-23 ALPN regression pin) — a test string cannot open a socket.
fn production_region(source: &str) -> &str {
    match source.find("#[cfg(test)]\nmod tests") {
        Some(index) => &source[..index],
        None => source,
    }
}

/// Pins the ORPHAN status of the two depth WebSocket URL constants
/// (round-8 ledger truth-sync, 2026-07-15): `DHAN_TWENTY_DEPTH_WS_BASE_URL`
/// + `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL` are RETAINED pub consts
/// (constants.rs) whose consumers were deleted by AWS-lifecycle PR #4
/// (2026-05-19, the depth-20/200 infrastructure removal). The prior ledger
/// narrated the CONSTANTS THEMSELVES deleted — the mirror image of the
/// round-1 confirmed drift (option-chain endpoints narrated "no longer
/// implemented" while LIVE). This scan makes the orphan annotation
/// SELF-TRUING, exactly like
/// `test_charts_historical_constant_is_orphaned_post_phase_c3`: a new
/// consumer (a constant reference, or an inline depth hostname literal on
/// a production code line) fails HERE until the annotations move
/// orphan -> live in the same PR — and depth re-introduction is separately
/// FORBIDDEN FOREVER by
/// `.claude/rules/project/websocket-connection-scope-lock.md`, which
/// requires a rule edit BEFORE any such code, so a trip here is a
/// scope-lock violation signal, not a formality. Deleting the constants
/// instead fails the value assertions below (and the 45-endpoint census)
/// until the counts move. FULL-LINE comment lines are excluded
/// (ws_event_types.rs' WsType doc comments legitimately MENTION the depth
/// URLs) and test modules are excluded via [`production_region`] (tls.rs'
/// ALPN assert message). A trailing-comment mention on a code line would
/// over-count and fail LOUDLY — the conservative direction for an orphan
/// pin.
#[test]
fn test_depth_ws_url_constants_are_orphaned() {
    use tickvault_common::constants::{
        DHAN_TWENTY_DEPTH_WS_BASE_URL, DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
    };
    // The constants EXIST (retained, wss://, distinct depth hosts) — the
    // census's 2 orphaned-depth-WS entries are these, reproducibly.
    assert_eq!(
        DHAN_TWENTY_DEPTH_WS_BASE_URL, "wss://depth-api-feed.dhan.co/twentydepth",
        "20-level depth WS URL constant (ORPHANED — zero consumers)"
    );
    assert_eq!(
        DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL, "wss://full-depth-api.dhan.co",
        "200-level depth WS URL constant (ORPHANED — zero consumers; root \
         path per the 2026-04-23 Python SDK verification)"
    );

    let mut files = Vec::new();
    collect_workspace_src_files(Path::new("../../crates"), &mut files);
    assert!(
        files.len() > 50,
        "the depth-orphan scan must see the workspace production tree \
         (found {} files)",
        files.len()
    );
    const DEPTH_NEEDLES: [&str; 4] = [
        "DHAN_TWENTY_DEPTH_WS_BASE_URL",
        "DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL",
        "depth-api-feed.dhan.co",
        "full-depth-api.dhan.co",
    ];
    let mut consumers = Vec::new();
    for file in files {
        let path_text = file.to_string_lossy().replace('\\', "/");
        // The declaration site itself — separator-anchored (the gate-guard
        // round-5 lesson).
        if path_text.ends_with("/common/src/constants.rs") {
            continue;
        }
        let Ok(source) = fs::read_to_string(&file) else {
            continue;
        };
        let has_code_mention = production_region(&source)
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .any(|line| DEPTH_NEEDLES.iter().any(|needle| line.contains(needle)));
        if has_code_mention {
            consumers.push(path_text);
        }
    }
    assert!(
        consumers.is_empty(),
        "the 2 depth WS URL constants are ORPHANED (PR #4 deleted their \
         consumers; depth is FORBIDDEN FOREVER per \
         websocket-connection-scope-lock.md) — a new consumer/code mention \
         requires the scope-lock rule edit FIRST and must update this \
         ledger's orphan annotations (orphan -> live) in the same PR. \
         Found:\n{}",
        consumers.join("\n")
    );
}
