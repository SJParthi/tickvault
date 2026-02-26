//! Compile-time constants for the dhan-live-trader system.
//!
//! All values known at compile time live here. Runtime values live in config TOML.
//! If you see a raw number or string literal in application code, it's a bug.

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 Binary Protocol — Packet Sizes
// ---------------------------------------------------------------------------

/// Ticker packet: 25 bytes (compact price data).
pub const TICKER_PACKET_SIZE: usize = 25;

/// Quote packet: 51 bytes (price + depth level 1).
pub const QUOTE_PACKET_SIZE: usize = 51;

/// Full quote packet (OI + market depth): 162 bytes.
pub const FULL_QUOTE_PACKET_SIZE: usize = 162;

/// Previous close packet: 20 bytes.
pub const PREVIOUS_CLOSE_PACKET_SIZE: usize = 20;

/// Market status packet: 10 bytes.
pub const MARKET_STATUS_PACKET_SIZE: usize = 10;

/// Maximum size of a single WebSocket binary frame from Dhan.
pub const MAX_WEBSOCKET_FRAME_SIZE: usize = 65536;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Connection Limits
// ---------------------------------------------------------------------------

/// Maximum instruments per single WebSocket connection.
pub const MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION: usize = 5000;

/// Maximum concurrent WebSocket connections per Dhan account.
pub const MAX_WEBSOCKET_CONNECTIONS: usize = 5;

/// Total subscription capacity across all connections.
pub const MAX_TOTAL_SUBSCRIPTIONS: usize =
    MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION * MAX_WEBSOCKET_CONNECTIONS;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Protocol Constants
// ---------------------------------------------------------------------------

/// Ping interval in seconds (Dhan spec: client sends ping every 10 seconds).
pub const PING_INTERVAL_SECS: u64 = 10;

/// Pong timeout in seconds (if no pong received after ping, log WARN).
pub const PONG_TIMEOUT_SECS: u64 = 10;

/// Server disconnects if no ping received within this many seconds.
pub const SERVER_PING_TIMEOUT_SECS: u64 = 40;

/// Maximum consecutive pong failures before triggering reconnect.
pub const MAX_CONSECUTIVE_PONG_FAILURES: u32 = 2;

/// Maximum instruments per subscription message (Dhan limit).
pub const SUBSCRIPTION_BATCH_SIZE: usize = 100;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Exchange Segment Codes (Binary Protocol)
// ---------------------------------------------------------------------------

/// IDX_I segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_IDX_I: u8 = 0;

/// NSE_EQ segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_NSE_EQ: u8 = 1;

/// NSE_FNO segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_NSE_FNO: u8 = 2;

/// BSE_EQ segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_BSE_EQ: u8 = 3;

/// BSE_FNO segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_BSE_FNO: u8 = 4;

/// MCX_COMM segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_MCX_COMM: u8 = 5;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Disconnect Error Codes
// ---------------------------------------------------------------------------

/// Exceeded max connections (5 per account).
pub const DISCONNECT_EXCEEDED_MAX_CONNECTIONS: u16 = 801;

/// Exceeded max instruments per connection (5000).
pub const DISCONNECT_EXCEEDED_MAX_INSTRUMENTS: u16 = 802;

/// Authentication failed — invalid access token.
pub const DISCONNECT_AUTH_FAILED: u16 = 803;

/// Token expired — must refresh and reconnect.
pub const DISCONNECT_TOKEN_EXPIRED: u16 = 804;

/// Server maintenance — wait and reconnect with backoff.
pub const DISCONNECT_SERVER_MAINTENANCE: u16 = 805;

/// Connection timeout — no ping received in 40 seconds.
pub const DISCONNECT_PING_TIMEOUT: u16 = 806;

/// Invalid subscription request format.
pub const DISCONNECT_INVALID_SUBSCRIPTION: u16 = 807;

/// Total subscription limit exceeded (25000 across all connections).
pub const DISCONNECT_SUBSCRIPTION_LIMIT: u16 = 808;

/// Force-disconnected by Dhan kill switch.
pub const DISCONNECT_FORCE_KILLED: u16 = 810;

/// Invalid client ID in connection headers.
pub const DISCONNECT_INVALID_CLIENT_ID: u16 = 814;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Response Codes (Binary Protocol)
// ---------------------------------------------------------------------------

/// Response code for index ticker packet (25 bytes).
pub const RESPONSE_CODE_INDEX_TICKER: u8 = 1;

/// Response code for ticker packet (25 bytes).
pub const RESPONSE_CODE_TICKER: u8 = 2;

/// Response code for quote packet (51 bytes).
pub const RESPONSE_CODE_QUOTE: u8 = 4;

/// Response code for OI data packet (17 bytes).
pub const RESPONSE_CODE_OI: u8 = 5;

/// Response code for previous close packet (20 bytes).
pub const RESPONSE_CODE_PREVIOUS_CLOSE: u8 = 6;

/// Response code for market status packet (10 bytes).
pub const RESPONSE_CODE_MARKET_STATUS: u8 = 7;

/// Response code for full packet (162 bytes).
pub const RESPONSE_CODE_FULL: u8 = 8;

/// Response code for disconnect (variable length).
pub const RESPONSE_CODE_DISCONNECT: u8 = 50;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Market Status Codes
// ---------------------------------------------------------------------------

/// Market is closed.
pub const MARKET_STATUS_CLOSED: u16 = 0;

/// Pre-open session.
pub const MARKET_STATUS_PRE_OPEN: u16 = 1;

/// Market is open for continuous trading.
pub const MARKET_STATUS_OPEN: u16 = 2;

/// Post-close session.
pub const MARKET_STATUS_POST_CLOSE: u16 = 3;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Subscription Request Codes
// ---------------------------------------------------------------------------

/// Unsubscription request code.
pub const FEED_REQUEST_UNSUBSCRIBE: u8 = 12;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Binary Header
// ---------------------------------------------------------------------------

/// Size of the binary response header in bytes.
pub const BINARY_HEADER_SIZE: usize = 8;

/// OI data packet size in bytes.
pub const OI_PACKET_SIZE: usize = 17;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Feed Request Type Codes
// ---------------------------------------------------------------------------

/// Ticker feed mode (compact price updates).
pub const FEED_REQUEST_TICKER: u8 = 15;

/// Quote feed mode (price + best bid/ask).
pub const FEED_REQUEST_QUOTE: u8 = 17;

/// Full market depth feed mode (OI + 5-level depth).
pub const FEED_REQUEST_FULL: u8 = 21;

// ---------------------------------------------------------------------------
// Market Depth
// ---------------------------------------------------------------------------

/// Number of market depth levels in full quote.
pub const MARKET_DEPTH_LEVELS: usize = 5;

// ---------------------------------------------------------------------------
// Trading Candles
// ---------------------------------------------------------------------------

/// Number of 1-minute trading candles per day (9:15 to 15:30).
pub const TRADING_CANDLES_PER_DAY: usize = 375;

// ---------------------------------------------------------------------------
// SEBI Compliance
// ---------------------------------------------------------------------------

/// Maximum orders per second allowed by SEBI regulation.
pub const SEBI_MAX_ORDERS_PER_SECOND: u32 = 10;

/// Minimum audit log retention in years (SEBI requirement).
pub const SEBI_AUDIT_RETENTION_YEARS: u32 = 5;

// ---------------------------------------------------------------------------
// SSM Parameter Store — Secret Path Prefixes
// ---------------------------------------------------------------------------

/// Base path for all secrets in SSM Parameter Store.
pub const SSM_SECRET_BASE_PATH: &str = "/dlt";

/// Dhan credentials secret names.
pub const DHAN_CLIENT_ID_SECRET: &str = "client-id";
pub const DHAN_CLIENT_SECRET_SECRET: &str = "client-secret";
pub const DHAN_TOTP_SECRET: &str = "totp-secret";

// ---------------------------------------------------------------------------
// Docker Container Naming
// ---------------------------------------------------------------------------

/// Container name prefix for all dhan-live-trader Docker services.
pub const DOCKER_CONTAINER_PREFIX: &str = "dlt";

// ---------------------------------------------------------------------------
// Ring Buffer Capacities
// ---------------------------------------------------------------------------

/// Tick ring buffer capacity (SPSC, must be power of 2 for rtrb).
pub const TICK_RING_BUFFER_CAPACITY: usize = 65536;

/// Order event ring buffer capacity.
pub const ORDER_EVENT_RING_BUFFER_CAPACITY: usize = 4096;

// ---------------------------------------------------------------------------
// Instrument CSV — Download & Parse
// ---------------------------------------------------------------------------

/// Minimum expected rows in Dhan instrument CSV (sanity check for row count).
pub const INSTRUMENT_CSV_MIN_ROWS: usize = 100_000;

/// Minimum expected bytes for a valid instrument CSV download.
/// Dhan's CSV is ~40MB; anything under 1MB is suspiciously small.
pub const INSTRUMENT_CSV_MIN_BYTES: usize = 1_000_000;

/// Expected download time for instrument CSV in seconds.
pub const INSTRUMENT_CSV_DOWNLOAD_TIMEOUT_SECS: u64 = 120;

/// Maximum retry attempts for instrument CSV download.
pub const INSTRUMENT_CSV_MAX_DOWNLOAD_RETRIES: usize = 3;

/// Initial backoff delay for instrument CSV download retry in milliseconds.
pub const INSTRUMENT_CSV_RETRY_INITIAL_DELAY_MS: u64 = 2000;

/// Maximum backoff delay for instrument CSV download retry in milliseconds.
pub const INSTRUMENT_CSV_RETRY_MAX_DELAY_MS: u64 = 8000;

// ---------------------------------------------------------------------------
// Instrument CSV — Column Names (for auto-detection from header)
// ---------------------------------------------------------------------------

pub const CSV_COLUMN_EXCH_ID: &str = "EXCH_ID";
pub const CSV_COLUMN_SEGMENT: &str = "SEGMENT";
pub const CSV_COLUMN_SECURITY_ID: &str = "SECURITY_ID";
pub const CSV_COLUMN_INSTRUMENT: &str = "INSTRUMENT";
pub const CSV_COLUMN_UNDERLYING_SECURITY_ID: &str = "UNDERLYING_SECURITY_ID";
pub const CSV_COLUMN_UNDERLYING_SYMBOL: &str = "UNDERLYING_SYMBOL";
pub const CSV_COLUMN_SYMBOL_NAME: &str = "SYMBOL_NAME";
pub const CSV_COLUMN_DISPLAY_NAME: &str = "DISPLAY_NAME";
pub const CSV_COLUMN_SERIES: &str = "SERIES";
pub const CSV_COLUMN_LOT_SIZE: &str = "LOT_SIZE";
pub const CSV_COLUMN_EXPIRY_DATE: &str = "SM_EXPIRY_DATE";
pub const CSV_COLUMN_STRIKE_PRICE: &str = "STRIKE_PRICE";
pub const CSV_COLUMN_OPTION_TYPE: &str = "OPTION_TYPE";
pub const CSV_COLUMN_TICK_SIZE: &str = "TICK_SIZE";
pub const CSV_COLUMN_EXPIRY_FLAG: &str = "EXPIRY_FLAG";

// ---------------------------------------------------------------------------
// Instrument CSV — Segment & Instrument Codes (text values in CSV)
// ---------------------------------------------------------------------------

/// CSV segment value for indices.
pub const CSV_SEGMENT_INDEX: &str = "I";

/// CSV segment value for equity.
pub const CSV_SEGMENT_EQUITY: &str = "E";

/// CSV segment value for derivatives.
pub const CSV_SEGMENT_DERIVATIVE: &str = "D";

/// CSV exchange value for NSE.
pub const CSV_EXCHANGE_NSE: &str = "NSE";

/// CSV exchange value for BSE.
pub const CSV_EXCHANGE_BSE: &str = "BSE";

/// CSV series value for equity stocks.
pub const CSV_SERIES_EQUITY: &str = "EQ";

/// CSV instrument value: index future.
pub const CSV_INSTRUMENT_FUTIDX: &str = "FUTIDX";

/// CSV instrument value: stock future.
pub const CSV_INSTRUMENT_FUTSTK: &str = "FUTSTK";

/// CSV instrument value: index option.
pub const CSV_INSTRUMENT_OPTIDX: &str = "OPTIDX";

/// CSV instrument value: stock option.
pub const CSV_INSTRUMENT_OPTSTK: &str = "OPTSTK";

/// CSV option type value: call.
pub const CSV_OPTION_TYPE_CALL: &str = "CE";

/// CSV option type value: put.
pub const CSV_OPTION_TYPE_PUT: &str = "PE";

/// Marker substring in symbol names indicating test instruments.
pub const CSV_TEST_SYMBOL_MARKER: &str = "TEST";

// ---------------------------------------------------------------------------
// F&O Universe — Display Indices (23 IDX_I security IDs)
// ---------------------------------------------------------------------------

use crate::types::SecurityId;

/// Display indices: (display name, IDX_I security ID, subcategory string).
/// Subscribed for market dashboard and sentiment display.
/// Subcategory must match `IndexSubcategory::as_str()` values.
pub const DISPLAY_INDEX_ENTRIES: &[(&str, SecurityId, &str)] = &[
    ("INDIA VIX", 21, "Volatility"),
    ("NIFTY 100", 17, "BroadMarket"),
    ("NIFTY 200", 18, "BroadMarket"),
    ("NIFTY 500", 19, "BroadMarket"),
    ("NIFTYMCAP50", 20, "MidCap"),
    ("NIFTY MIDCAP 150", 1, "MidCap"),
    ("NIFTY SMALLCAP 50", 22, "SmallCap"),
    ("NIFTY SMALLCAP 100", 5, "SmallCap"),
    ("NIFTY SMALLCAP 250", 3, "SmallCap"),
    ("NIFTY AUTO", 14, "Sectoral"),
    ("NIFTY PVT BANK", 15, "Sectoral"),
    ("NIFTY FMCG", 28, "Sectoral"),
    ("NIFTY ENERGY", 42, "Sectoral"),
    ("NIFTYINFRA", 43, "Sectoral"),
    ("NIFTYIT", 29, "Sectoral"),
    ("NIFTY MEDIA", 30, "Sectoral"),
    ("NIFTY METAL", 31, "Sectoral"),
    ("NIFTY MNC", 44, "Sectoral"),
    ("NIFTY PHARMA", 32, "Sectoral"),
    ("NIFTY PSU BANK", 33, "Sectoral"),
    ("NIFTY REALTY", 34, "Sectoral"),
    ("NIFTY SERV SECTOR", 46, "Sectoral"),
    ("NIFTY CONSUMPTION", 40, "Thematic"),
];

/// Number of display indices.
pub const DISPLAY_INDEX_COUNT: usize = 23;

/// Total subscribed indices: 8 F&O + 23 Display = 31.
pub const TOTAL_SUBSCRIBED_INDEX_COUNT: usize = 31;

// ---------------------------------------------------------------------------
// F&O Universe — Full Chain Indices
// ---------------------------------------------------------------------------

/// Index symbols that get full option chain subscriptions.
pub const FULL_CHAIN_INDEX_SYMBOLS: &[&str] =
    &["NIFTY", "BANKNIFTY", "FINNIFTY", "MIDCPNIFTY", "SENSEX"];

/// Number of full-chain indices.
pub const FULL_CHAIN_INDEX_COUNT: usize = 5;

// ---------------------------------------------------------------------------
// F&O Universe — Index Aliases (FNO symbol → IDX_I symbol)
// ---------------------------------------------------------------------------

/// Aliases for indices whose FNO underlying symbol differs from IDX_I row symbol.
/// Format: (fno_underlying_symbol, idx_i_symbol_name).
pub const INDEX_SYMBOL_ALIASES: &[(&str, &str)] =
    &[("NIFTYNXT50", "NIFTY NEXT 50"), ("SENSEX50", "SNSX50")];

// ---------------------------------------------------------------------------
// F&O Universe — Validation: Must-Exist Price IDs
// ---------------------------------------------------------------------------

/// Known index security IDs that MUST exist after universe build.
/// (underlying_symbol, expected IDX_I security_id).
pub const VALIDATION_MUST_EXIST_INDICES: &[(&str, SecurityId)] = &[
    ("NIFTY", 13),
    ("BANKNIFTY", 25),
    ("FINNIFTY", 27),
    ("MIDCPNIFTY", 442),
    ("NIFTYNXT50", 38),
    ("SENSEX", 51),
    ("BANKEX", 69),
    ("SENSEX50", 83),
];

/// Known equity security IDs that MUST exist after universe build.
/// (symbol, expected NSE_EQ security_id).
pub const VALIDATION_MUST_EXIST_EQUITIES: &[(&str, SecurityId)] = &[("RELIANCE", 2885)];

/// Known stock symbols that MUST be present in the F&O universe.
pub const VALIDATION_MUST_EXIST_FNO_STOCKS: &[&str] = &["RELIANCE", "HDFCBANK", "INFY", "TCS"];

/// Minimum expected number of F&O stock underlyings.
pub const VALIDATION_FNO_STOCK_MIN_COUNT: usize = 150;

/// Maximum expected number of F&O stock underlyings.
pub const VALIDATION_FNO_STOCK_MAX_COUNT: usize = 300;

// ---------------------------------------------------------------------------
// Application Identity
// ---------------------------------------------------------------------------

/// Application name used in logs, metrics, and tracing.
pub const APPLICATION_NAME: &str = "dhan-live-trader";

/// Application version (updated with each release).
pub const APPLICATION_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// QuestDB ILP — Table Names
// ---------------------------------------------------------------------------

/// QuestDB table: daily instrument build health and statistics.
pub const QUESTDB_TABLE_BUILD_METADATA: &str = "instrument_build_metadata";

/// QuestDB table: daily F&O underlying snapshots.
pub const QUESTDB_TABLE_FNO_UNDERLYINGS: &str = "fno_underlyings";

/// QuestDB table: daily derivative contract snapshots.
pub const QUESTDB_TABLE_DERIVATIVE_CONTRACTS: &str = "derivative_contracts";

/// QuestDB table: daily subscribed index snapshots (8 F&O + 23 Display = 31).
pub const QUESTDB_TABLE_SUBSCRIBED_INDICES: &str = "subscribed_indices";

// ---------------------------------------------------------------------------
// QuestDB ILP — Ingestion Configuration
// ---------------------------------------------------------------------------

/// Number of ILP rows to buffer before flushing to QuestDB.
/// Prevents unbounded memory growth when writing ~150K derivative contracts.
pub const ILP_FLUSH_BATCH_SIZE: usize = 10_000;

// ---------------------------------------------------------------------------
// Timezone — IST (India Standard Time)
// ---------------------------------------------------------------------------

/// IST offset from UTC in seconds (5 hours 30 minutes = 19,800 seconds).
pub const IST_UTC_OFFSET_SECONDS: i32 = 19_800;

// ---------------------------------------------------------------------------
// Authentication — TOTP Configuration
// ---------------------------------------------------------------------------

/// TOTP digit count (Dhan uses 6-digit codes).
pub const TOTP_DIGITS: usize = 6;

/// TOTP time period in seconds (standard 30-second window).
pub const TOTP_PERIOD_SECS: u64 = 30;

/// TOTP skew tolerance (number of periods to accept before/after current).
pub const TOTP_SKEW: u8 = 1;

// ---------------------------------------------------------------------------
// Authentication — Dhan REST API Endpoint Paths
// ---------------------------------------------------------------------------

/// Path for initial token generation (appended to rest_api_base_url).
pub const DHAN_GENERATE_TOKEN_PATH: &str = "/generateAccessToken";

/// Path for token renewal (appended to rest_api_base_url).
pub const DHAN_RENEW_TOKEN_PATH: &str = "/renewToken";

// ---------------------------------------------------------------------------
// Authentication — SSM Path Construction
// ---------------------------------------------------------------------------

/// Default environment name for SSM path construction.
/// Overridden by `ENVIRONMENT` env var if present.
pub const DEFAULT_SSM_ENVIRONMENT: &str = "dev";

/// SSM service path segment for Dhan credentials.
pub const SSM_DHAN_SERVICE: &str = "dhan";

// ---------------------------------------------------------------------------
// Authentication — Circuit Breaker
// ---------------------------------------------------------------------------

/// Maximum consecutive token renewal failures before circuit breaker trips.
pub const TOKEN_RENEWAL_CIRCUIT_BREAKER_THRESHOLD: u32 = 3;

/// Circuit breaker reset timeout in seconds (try again after this).
pub const TOKEN_RENEWAL_CIRCUIT_BREAKER_RESET_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Compile-Time Assertions
// ---------------------------------------------------------------------------

// Enforce: DISPLAY_INDEX_COUNT must equal DISPLAY_INDEX_ENTRIES length.
const _: () = assert!(
    DISPLAY_INDEX_COUNT == DISPLAY_INDEX_ENTRIES.len(),
    "DISPLAY_INDEX_COUNT does not match DISPLAY_INDEX_ENTRIES.len()"
);

// Enforce: FULL_CHAIN_INDEX_COUNT must equal FULL_CHAIN_INDEX_SYMBOLS length.
const _: () = assert!(
    FULL_CHAIN_INDEX_COUNT == FULL_CHAIN_INDEX_SYMBOLS.len(),
    "FULL_CHAIN_INDEX_COUNT does not match FULL_CHAIN_INDEX_SYMBOLS.len()"
);

// Enforce: TOTAL_SUBSCRIBED_INDEX_COUNT is the sum of F&O and display.
// 8 F&O indices (from VALIDATION_MUST_EXIST_INDICES) + 23 display.
const _: () = assert!(
    TOTAL_SUBSCRIBED_INDEX_COUNT == VALIDATION_MUST_EXIST_INDICES.len() + DISPLAY_INDEX_COUNT,
    "TOTAL_SUBSCRIBED_INDEX_COUNT mismatch"
);

// Sanity: ring buffer capacities must be powers of 2 (rtrb requirement).
const _: () = assert!(
    TICK_RING_BUFFER_CAPACITY.is_power_of_two(),
    "TICK_RING_BUFFER_CAPACITY must be power of 2"
);
const _: () = assert!(
    ORDER_EVENT_RING_BUFFER_CAPACITY.is_power_of_two(),
    "ORDER_EVENT_RING_BUFFER_CAPACITY must be power of 2"
);
