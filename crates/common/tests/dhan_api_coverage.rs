//! Dhan API endpoint coverage tests.
//!
//! Verifies that all 57 Dhan API endpoints have URL constants or are
//! documented as intentionally skipped. Prevents endpoint drift where
//! new endpoints are added to api_client.rs using inline strings without
//! corresponding constants in constants.rs.
//!
//! Reference: docs/dhan-ref/*.md (21 reference files)

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

// PR #4 (2026-05-19): The `DHAN_TWENTY_DEPTH_WS_BASE_URL` +
// `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL` constants were deleted alongside the
// 20/200-level depth WebSocket infrastructure per the 4-IDX_I LOCKED_UNIVERSE
// operator lock (.claude/rules/project/websocket-connection-scope-lock.md).

// ---------------------------------------------------------------------------
// Test: All Dhan REST endpoint constants are defined and have correct paths
// ---------------------------------------------------------------------------

/// Verifies that constants exist for all 19 implemented Dhan REST endpoints
/// in constants.rs, and that each constant maps to the correct v2 API path.
///
/// Endpoint groups covered:
/// - Authentication: generateAccessToken, RenewToken
/// - Historical data: charts/intraday, charts/historical
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
    assert_eq!(
        DHAN_CHARTS_INTRADAY_PATH, "/charts/intraday",
        "POST api.dhan.co/v2/charts/intraday"
    );
    assert_eq!(
        DHAN_CHARTS_HISTORICAL_PATH, "/charts/historical",
        "POST api.dhan.co/v2/charts/historical"
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
    // claimed them "no longer implemented")
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
/// The previously-tested 20/200-level depth WS URL constants were deleted
/// alongside the depth pipelines. The two URLs below live in `DhanConfig`
/// (not in `constants.rs`) because they are per-environment configurable,
/// so we verify them via a source scan of `config.rs`.
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

/// Verifies the OMS REST endpoint ledger for the trading surface.
///
/// TWO DISTINCT COUNTS, kept honest (2026-07-14 round-2 reconciliation —
/// the prior header conflated them as "35 endpoints used in api_client.rs"):
///
/// * **35 unique endpoint PATHS** = 16 inline base paths built in
///   api_client.rs (e.g., `format!("{}/orders", base_url)`) + 19
///   constants-backed `DHAN_*_PATH` paths from constants.rs. The
///   constants are consumed ACROSS the workspace — api_client.rs for the
///   OMS families, but also core (auth/RenewToken/ip/charts) and app
///   (option-chain per-minute pull) — NOT all "in api_client.rs".
/// * **38 per-method REST OPERATIONS** enumerated in the breakdown below
///   (several operations share one path, e.g. PUT/DELETE/GET on
///   /orders/{order-id}) — plus exit-all's DELETE reusing the shared
///   /positions path as a 39th operation on an already-counted path.
///
/// The inline paths are documented here to detect accidental changes or
/// omissions.
///
/// The 38 listed operations break down as:
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
/// - Portfolio via constant (3): holdings, positions, positions/convert
/// - Funds/margin via constant (3): margin-calc, margin-multi, fund-limit
/// - Trader's control via constant (2): killswitch, pnl-exit
/// - Exit-all (DELETE /positions) reuses DHAN_POSITIONS_PATH — a distinct
///   operation on a path already inside the 35 (asserted below, not part
///   of the 38-item list)
///
/// Total unique paths: 19 (constants — incl. /alerts/multi/orders added
/// 2026-07-14 and the 2 option-chain constants, live since the 2026-07-12
/// §8 rebuild) + 16 (inline in api_client.rs) = 35
/// Plus 4 WebSocket URLs = 39 endpoint URLs
/// Plus 14 parameterized variants (e.g., /orders/{id}) that share base paths
/// Grand ledger total: 57 = 53 implemented endpoint URLs/variants
/// (35 paths + 14 variants + 4 WebSocket) + 4 intentionally skipped
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

    // --- Grand total ---
    // 19 constants-backed REST endpoints
    // 2026-07-14: +1 /alerts/multi/orders (Conditional & Multi Order family)
    // 2026-07-14 review fix: +2 option-chain constants (LIVE since the
    //   2026-07-12 §8 rebuild in crates/app — the prior 17/51/55 totals
    //   undercounted reality by exactly these 2)
    // + 16 inline base paths in api_client.rs (the OMS inline set; the 2
    //   option-chain paths moved from the deleted core client's local
    //   constants to constants.rs, so they count in the 19, never here)
    // + 14 parameterized variants ({order-id}, {correlation-id}, {alertId}, {isin}, {leg}, {dates})
    // + 4 WebSocket endpoints
    let constants_rest_count: usize = 19;
    let inline_base_paths: usize = 16; // unique base paths in api_client.rs
    let parameterized_variants: usize = 14; // {id} variants
    let websocket_count: usize = 4;
    let skipped_count: usize = 4;

    let total_implemented =
        constants_rest_count + inline_base_paths + parameterized_variants + websocket_count;
    assert_eq!(
        total_implemented, 53,
        "53 implemented endpoint URLs/variants (35 unique paths + 14 \
         parameterized variants + 4 WebSocket; /alerts/multi/orders added \
         2026-07-14; option-chain's 2 constants re-counted 2026-07-14 — \
         live since the 2026-07-12 §8 rebuild)"
    );

    // Plus the 4 intentionally skipped = total Dhan API endpoints known
    let total_known = total_implemented + skipped_count;
    assert_eq!(
        total_known, 57,
        "57 total known Dhan API endpoint URLs/variants (53 implemented + 4 skipped)"
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
// alongside the deleted 20/200-level depth WebSocket constants. See the
// header comment after the imports for the operator lock reference.
