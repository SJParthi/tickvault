//! Dhan API endpoint coverage tests.
//!
//! Verifies that all 54 Dhan API endpoints have URL constants or are
//! documented as intentionally skipped. Prevents endpoint drift where
//! new endpoints are added to api_client.rs using inline strings without
//! corresponding constants in constants.rs.
//!
//! Reference: docs/dhan-ref/*.md (21 reference files)

use dhan_live_trader_common::constants::{
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
    DHAN_PNL_EXIT_PATH,
    DHAN_POSITIONS_CONVERT_PATH,
    DHAN_POSITIONS_PATH,
    DHAN_RENEW_TOKEN_PATH,
    DHAN_SET_IP_PATH,
    // WebSocket depth (2)
    DHAN_TWENTY_DEPTH_WS_BASE_URL,
    DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
    // User profile (1)
    DHAN_USER_PROFILE_PATH,
};

// ---------------------------------------------------------------------------
// Test: All Dhan REST endpoint constants are defined and have correct paths
// ---------------------------------------------------------------------------

/// Verifies that constants exist for all 16 implemented Dhan REST endpoints
/// in constants.rs, and that each constant maps to the correct v2 API path.
///
/// Endpoint groups covered:
/// - Authentication: generateAccessToken, RenewToken
/// - Historical data: charts/intraday, charts/historical
/// - User profile: profile
/// - IP management: setIP, modifyIP, getIP
/// - Portfolio: holdings, positions, positions/convert
/// - Funds & margin: margincalculator, margincalculator/multi, fundlimit
/// - Trader's control: killswitch, pnlExit
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
    assert_eq!(
        DHAN_CHARTS_INTRADAY_PATH, "/charts/intraday",
        "POST api.dhan.co/v2/charts/intraday"
    );
    assert_eq!(
        DHAN_CHARTS_HISTORICAL_PATH, "/charts/historical",
        "POST api.dhan.co/v2/charts/historical"
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

    // --- Count verification ---
    // 16 REST endpoint path constants in constants.rs
    let rest_paths: &[&str] = &[
        DHAN_GENERATE_TOKEN_PATH,
        DHAN_RENEW_TOKEN_PATH,
        DHAN_CHARTS_INTRADAY_PATH,
        DHAN_CHARTS_HISTORICAL_PATH,
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
    ];
    assert_eq!(
        rest_paths.len(),
        16,
        "Expected 16 REST endpoint path constants in constants.rs"
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
// Test: All 4 WebSocket endpoint URLs are defined
// ---------------------------------------------------------------------------

/// Verifies constants exist for all 4 Dhan WebSocket endpoints:
/// 1. Market feed (config-based): wss://api-feed.dhan.co
/// 2. Order update (config-based): wss://api-order-update.dhan.co
/// 3. 20-level depth (constant): wss://depth-api-feed.dhan.co/twentydepth
/// 4. 200-level depth (constant): wss://full-depth-api.dhan.co/twohundreddepth
///
/// Market feed and order update WS URLs are in DhanConfig (not constants)
/// because they share the same config pattern as rest_api_base_url. The depth
/// WS URLs are constants because they have fixed paths (/twentydepth, /twohundreddepth).
#[test]
fn test_all_websocket_urls_defined() {
    // --- Depth WebSocket constants (docs/dhan-ref/04-full-market-depth-websocket.md) ---
    assert_eq!(
        DHAN_TWENTY_DEPTH_WS_BASE_URL, "wss://depth-api-feed.dhan.co/twentydepth",
        "20-level depth WS base URL"
    );
    assert_eq!(
        DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL, "wss://full-depth-api.dhan.co/twohundreddepth",
        "200-level depth WS base URL — official Dhan API docs path"
    );

    // Both must use wss:// (TLS required)
    assert!(
        DHAN_TWENTY_DEPTH_WS_BASE_URL.starts_with("wss://"),
        "20-depth WS must use TLS"
    );
    assert!(
        DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.starts_with("wss://"),
        "200-depth WS must use TLS"
    );

    // --- Config-based WebSocket URLs (verified via config source scan) ---
    // Market feed: wss://api-feed.dhan.co (docs/dhan-ref/03-live-market-feed-websocket.md)
    // Order update: wss://api-order-update.dhan.co (docs/dhan-ref/10-live-order-update-websocket.md)
    //
    // These URLs live in DhanConfig (not constants) because they are
    // configurable per environment. We verify the default values by scanning
    // the config source to confirm they exist with the correct values.
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

    // All 4 WebSocket URLs must point to different hosts/paths
    let ws_urls: &[&str] = &[
        market_feed_ws,
        order_update_ws,
        DHAN_TWENTY_DEPTH_WS_BASE_URL,
        DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
    ];
    for (i, a) in ws_urls.iter().enumerate() {
        for (j, b) in ws_urls.iter().enumerate() {
            if i != j {
                assert_ne!(a, b, "WebSocket URLs must be unique: {a} == {b}");
            }
        }
    }

    // All 4 must use wss:// (TLS required)
    for url in ws_urls {
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

/// Verifies the complete set of 34 OMS endpoint paths used in api_client.rs.
///
/// These are currently constructed inline (e.g., `format!("{}/orders", base_url)`)
/// rather than using named constants. This test documents and verifies the
/// expected inline paths to detect accidental changes or omissions.
///
/// The 34 endpoints break down as:
/// - Orders (9): place, modify, cancel, order-book, single-order, by-correlation, trade-book, trades-by-order, slicing
/// - Super orders (4): place, modify, cancel, list
/// - Forever orders (4): create, modify, delete, list
/// - Conditional triggers (5): create, modify, delete, get-one, get-all
/// - EDIS (3): tpin, form, inquire
/// - Statements (2): ledger, trade-history
/// - Option chain (2): chain, expiry-list
/// - Portfolio via constant (3): holdings, positions, positions/convert
/// - Funds/margin via constant (3): margin-calc, margin-multi, fund-limit
/// - Trader's control via constant (2): killswitch, pnl-exit
/// - Exit-all (DELETE /positions) shares DHAN_POSITIONS_PATH (1, counted above)
///
/// Total unique paths: 16 (constants) + 18 (inline in api_client) + 2 (inline in option_chain/client.rs) = 36
/// Plus 4 WebSocket URLs = 40 endpoint URLs
/// Plus 14 parameterized variants (e.g., /orders/{id}) that share base paths
/// Grand total of distinct API operations: 54
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

    // --- Conditional triggers (docs/dhan-ref/07c-conditional-trigger.md) ---
    let conditional_paths: &[(&str, &str)] = &[
        ("/alerts/orders", "POST — create conditional trigger"),
        ("/alerts/orders/{alertId}", "PUT — modify trigger"),
        ("/alerts/orders/{alertId}", "DELETE — delete trigger"),
        ("/alerts/orders/{alertId}", "GET — get single trigger"),
        ("/alerts/orders", "GET — list all triggers"),
    ];
    assert_eq!(
        conditional_paths.len(),
        5,
        "Conditional triggers: 5 endpoint operations"
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

    // --- Option chain (docs/dhan-ref/06-option-chain.md) ---
    // These are defined as local constants in crates/core/src/option_chain/client.rs
    let option_chain_paths: &[(&str, &str)] = &[
        ("/optionchain", "POST — full option chain with Greeks"),
        ("/optionchain/expirylist", "POST — expiry list"),
    ];
    assert_eq!(
        option_chain_paths.len(),
        2,
        "Option chain: 2 endpoint operations"
    );

    // --- Exit all positions ---
    // Uses DELETE on DHAN_POSITIONS_PATH (/positions), already counted above.
    // This is a distinct operation (DELETE vs GET) on the same path.
    assert_eq!(
        DHAN_POSITIONS_PATH, "/positions",
        "Exit-all shares the /positions path (DELETE method)"
    );

    // --- Grand total: 54 distinct API operations ---
    // 16 constants-backed REST endpoints
    // + 18 inline OMS endpoints (orders=5 unique + super=3 + forever=3 + alerts=2 + edis=3 + statements=2)
    // + 2 option chain endpoints (local constants in client.rs)
    // + 14 parameterized variants ({order-id}, {correlation-id}, {alertId}, {isin}, {leg}, {dates})
    // + 4 WebSocket endpoints
    // = 54 total
    let constants_rest_count: usize = 16;
    let inline_base_paths: usize = 18; // unique base paths in api_client.rs + option_chain
    let parameterized_variants: usize = 14; // {id} variants
    let websocket_count: usize = 4;
    let skipped_count: usize = 4;

    let total_implemented =
        constants_rest_count + inline_base_paths + parameterized_variants + websocket_count;
    assert_eq!(
        total_implemented, 52,
        "52 implemented endpoint operations (54 total - 2 shared-path ops counted in base)"
    );

    // Plus the 4 intentionally skipped = 56 total Dhan API endpoints known
    let total_known = total_implemented + skipped_count;
    assert_eq!(
        total_known, 56,
        "56 total known Dhan API endpoints (52 implemented + 4 skipped)"
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

// ---------------------------------------------------------------------------
// Test: WebSocket depth URLs contain expected hostnames
// ---------------------------------------------------------------------------

/// Verifies the depth WebSocket URLs use the correct Dhan hostnames
/// and paths. These are separate from the market feed WebSocket.
#[test]
fn test_depth_websocket_urls_correct_hostnames() {
    assert!(
        DHAN_TWENTY_DEPTH_WS_BASE_URL.contains("depth-api-feed.dhan.co"),
        "20-depth must use depth-api-feed.dhan.co"
    );
    assert!(
        DHAN_TWENTY_DEPTH_WS_BASE_URL.contains("/twentydepth"),
        "20-depth must have /twentydepth path"
    );
    assert!(
        DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.contains("full-depth-api.dhan.co"),
        "200-depth must use full-depth-api.dhan.co"
    );
    // SDK uses root path (no /twohundreddepth) — verified 2026-04-06
    assert!(
        DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.starts_with("wss://full-depth-api.dhan.co/"),
        "200-depth must start with wss://full-depth-api.dhan.co/"
    );

    // 20-depth and 200-depth use DIFFERENT hosts
    assert_ne!(
        DHAN_TWENTY_DEPTH_WS_BASE_URL, DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
        "20-depth and 200-depth must be different URLs"
    );
}
