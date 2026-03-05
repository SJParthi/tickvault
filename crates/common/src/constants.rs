//! Compile-time constants for the dhan-live-trader system.
//!
//! All values known at compile time live here. Runtime values live in config TOML.
//! If you see a raw number or string literal in application code, it's a bug.

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 Binary Protocol — Packet Sizes
// Source: DhanHQ Python SDK v2 (src/dhanhq/marketfeed.py)
// All sizes verified against struct.calcsize() in the SDK.
// ---------------------------------------------------------------------------

/// Ticker packet: 16 bytes (header 8 + LTP float32 + LTT int32).
/// SDK format: `<BHBIfI`
pub const TICKER_PACKET_SIZE: usize = 16;

/// Quote packet: 50 bytes (header + LTP + LTQ + LTT + ATP + Vol + TSQ + TBQ + OHLC).
/// SDK format: `<BHBIfHIfIIIffff`
pub const QUOTE_PACKET_SIZE: usize = 50;

/// Market depth standalone packet: 112 bytes (header 8 + LTP float32 + depth 100 bytes).
/// SDK format: `<BHBIf100s`
pub const MARKET_DEPTH_PACKET_SIZE: usize = 112;

/// Full packet (quote + OI + 5-level market depth): 162 bytes.
/// SDK format: `<BHBIfHIfIIIIIIffff100s`
pub const FULL_QUOTE_PACKET_SIZE: usize = 162;

/// Previous close packet: 16 bytes (header 8 + prev_close float32 + prev_oi int32).
/// SDK format: `<BHBIfI`
pub const PREVIOUS_CLOSE_PACKET_SIZE: usize = 16;

/// Market status packet: 8 bytes (header only, no additional payload).
/// SDK format: `<BHBI`
pub const MARKET_STATUS_PACKET_SIZE: usize = 8;

/// Disconnect packet: 10 bytes (header 8 + disconnect_code uint16).
/// SDK format: `<BHBIH`
pub const DISCONNECT_PACKET_SIZE: usize = 10;

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

/// Server ping interval in seconds (Dhan server pings client every 10 seconds).
/// Client MUST respond with pong. tokio-tungstenite auto-pongs.
pub const SERVER_PING_INTERVAL_SECS: u64 = 10;

/// Server disconnects if no pong received within this many seconds.
pub const SERVER_PING_TIMEOUT_SECS: u64 = 40;

/// Maximum instruments per subscription message (Dhan limit).
pub const SUBSCRIPTION_BATCH_SIZE: usize = 100;

/// WebSocket V2 authentication type (always 2 for access token auth).
pub const WEBSOCKET_AUTH_TYPE: u8 = 2;

/// WebSocket V2 version string used in query parameters.
pub const WEBSOCKET_PROTOCOL_VERSION: &str = "2";

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Exchange Segment Codes (Binary Protocol)
// Source: DhanHQ Python SDK v2 (src/dhanhq/marketfeed.py)
// CRITICAL: These are binary wire codes, NOT subscription JSON strings.
// ---------------------------------------------------------------------------

/// IDX_I segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_IDX_I: u8 = 0;

/// NSE_EQ segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_NSE_EQ: u8 = 1;

/// NSE_FNO segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_NSE_FNO: u8 = 2;

/// NSE_CURRENCY segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_NSE_CURRENCY: u8 = 3;

/// BSE_EQ segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_BSE_EQ: u8 = 4;

/// MCX_COMM segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_MCX_COMM: u8 = 5;

// Note: code 6 is unused/skipped in Dhan's protocol.

/// BSE_CURRENCY segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_BSE_CURRENCY: u8 = 7;

/// BSE_FNO segment code in Dhan binary protocol.
pub const EXCHANGE_SEGMENT_BSE_FNO: u8 = 8;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Disconnect Error Codes
// Source: DhanHQ Python SDK v2 on_close handler (src/dhanhq/marketfeed.py)
// ---------------------------------------------------------------------------

/// 805 — Active WebSocket connections exceeded (max 5 per account).
pub const DISCONNECT_EXCEEDED_ACTIVE_CONNECTIONS: u16 = 805;

/// 806 — Data API subscription required (plan/subscription issue).
pub const DISCONNECT_DATA_API_SUBSCRIPTION_REQUIRED: u16 = 806;

/// 807 — Access token expired. Refresh token, then reconnect.
pub const DISCONNECT_ACCESS_TOKEN_EXPIRED: u16 = 807;

/// 808 — Invalid client ID. Check SSM credentials.
pub const DISCONNECT_INVALID_CLIENT_ID: u16 = 808;

/// 809 — Authentication failed. Invalid credentials.
pub const DISCONNECT_AUTH_FAILED: u16 = 809;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Response Codes (Binary Protocol)
// Source: DhanHQ Python SDK v2 (src/dhanhq/marketfeed.py)
// ---------------------------------------------------------------------------

/// Response code for index ticker packet (16 bytes).
pub const RESPONSE_CODE_INDEX_TICKER: u8 = 1;

/// Response code for ticker packet (16 bytes).
pub const RESPONSE_CODE_TICKER: u8 = 2;

/// Response code for market depth standalone packet (112 bytes).
/// Format: `<BHBIf100s>` — Header(8) + LTP(f32) + Depth(5×20 bytes).
/// Dhan Python SDK: `process_market_depth(data)`.
pub const RESPONSE_CODE_MARKET_DEPTH: u8 = 3;

/// Response code for quote packet (50 bytes).
pub const RESPONSE_CODE_QUOTE: u8 = 4;

/// Response code for OI data packet (12 bytes).
pub const RESPONSE_CODE_OI: u8 = 5;

/// Response code for previous close packet (16 bytes).
pub const RESPONSE_CODE_PREVIOUS_CLOSE: u8 = 6;

/// Response code for market status packet (8 bytes, header only).
pub const RESPONSE_CODE_MARKET_STATUS: u8 = 7;

/// Response code for full packet (162 bytes).
pub const RESPONSE_CODE_FULL: u8 = 8;

/// Response code for disconnect (10 bytes).
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
// Source: DhanHQ Python SDK v2 (src/dhanhq/marketfeed.py)
// Subscribe codes: 15 (Ticker), 17 (Quote), 21 (Full).
// Unsubscribe = subscribe_code + 1: 16 (Ticker), 18 (Quote), 22 (Full).
// Disconnect = 12 (closes the WebSocket connection).
// ---------------------------------------------------------------------------

/// Disconnect request code (closes the WebSocket connection).
pub const FEED_REQUEST_DISCONNECT: u8 = 12;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Binary Header
// ---------------------------------------------------------------------------

/// Size of the binary response header in bytes.
/// Format: response_code(u8) + msg_length(u16) + exchange_segment(u8) + security_id(u32) = 8.
pub const BINARY_HEADER_SIZE: usize = 8;

/// OI data packet size in bytes (header 8 + OI int32 = 12).
/// SDK format: `<BHBII`
pub const OI_PACKET_SIZE: usize = 12;

/// Market depth level size in bytes (per level within Full packet).
/// Format: bid_qty(i32) + ask_qty(i32) + bid_orders(i16) + ask_orders(i16) + bid_price(f32) + ask_price(f32) = 20.
pub const MARKET_DEPTH_LEVEL_SIZE: usize = 20;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Feed Request Type Codes
// ---------------------------------------------------------------------------

/// Subscribe Ticker mode (compact price updates). Unsubscribe = 16.
pub const FEED_REQUEST_TICKER: u8 = 15;

/// Unsubscribe Ticker mode.
pub const FEED_UNSUBSCRIBE_TICKER: u8 = 16;

/// Subscribe Quote mode (price + volume + OHLC). Unsubscribe = 18.
pub const FEED_REQUEST_QUOTE: u8 = 17;

/// Unsubscribe Quote mode.
pub const FEED_UNSUBSCRIBE_QUOTE: u8 = 18;

/// Subscribe Full mode (quote + OI + 5-level depth). Unsubscribe = 22.
pub const FEED_REQUEST_FULL: u8 = 21;

/// Unsubscribe Full mode.
pub const FEED_UNSUBSCRIBE_FULL: u8 = 22;

// ---------------------------------------------------------------------------
// Market Depth
// ---------------------------------------------------------------------------

/// Number of market depth levels in full quote.
pub const MARKET_DEPTH_LEVELS: usize = 5;

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

/// SSM service path segment for Dhan credentials.
pub const SSM_DHAN_SERVICE: &str = "dhan";

/// Dhan credentials secret names.
pub const DHAN_CLIENT_ID_SECRET: &str = "client-id";
pub const DHAN_CLIENT_SECRET_SECRET: &str = "client-secret";
pub const DHAN_TOTP_SECRET: &str = "totp-secret";

// ---------------------------------------------------------------------------
// SSM Parameter Store — QuestDB Service
// ---------------------------------------------------------------------------

/// SSM service path segment for QuestDB credentials.
pub const SSM_QUESTDB_SERVICE: &str = "questdb";

/// SSM key for QuestDB PG wire protocol username.
pub const QUESTDB_PG_USER_SECRET: &str = "pg-user";

/// SSM key for QuestDB PG wire protocol password.
pub const QUESTDB_PG_PASSWORD_SECRET: &str = "pg-password";

// ---------------------------------------------------------------------------
// SSM Parameter Store — Grafana Service
// ---------------------------------------------------------------------------

/// SSM service path segment for Grafana credentials.
pub const SSM_GRAFANA_SERVICE: &str = "grafana";

/// SSM key for Grafana admin username.
pub const GRAFANA_ADMIN_USER_SECRET: &str = "admin-user";

/// SSM key for Grafana admin password.
pub const GRAFANA_ADMIN_PASSWORD_SECRET: &str = "admin-password";

// ---------------------------------------------------------------------------
// SSM Parameter Store — Telegram Service
// ---------------------------------------------------------------------------

/// SSM service path segment for Telegram bot credentials.
pub const SSM_TELEGRAM_SERVICE: &str = "telegram";

/// SSM key for Telegram bot token.
pub const TELEGRAM_BOT_TOKEN_SECRET: &str = "bot-token";

/// SSM key for Telegram chat ID.
pub const TELEGRAM_CHAT_ID_SECRET: &str = "chat-id";

// ---------------------------------------------------------------------------
// SSM Parameter Store — SNS Service
// ---------------------------------------------------------------------------

/// SSM service path segment for SNS configuration.
pub const SSM_SNS_SERVICE: &str = "sns";

/// SSM key for the phone number to send SNS SMS alerts to.
/// Value must be in E.164 format: `+<country_code><number>` (e.g., "+919876543210").
pub const SNS_PHONE_NUMBER_SECRET: &str = "phone-number";

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

/// Filename for the instrument build freshness marker.
/// Written on successful build; contains today's IST date as `YYYY-MM-DD`.
pub const INSTRUMENT_FRESHNESS_MARKER_FILENAME: &str = "instrument-build-date.txt";

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

/// Path for initial token generation (appended to auth_base_url).
/// Endpoint: POST <https://auth.dhan.co/app/generateAccessToken>
pub const DHAN_GENERATE_TOKEN_PATH: &str = "/app/generateAccessToken";

/// Path for token renewal (appended to rest_api_base_url).
/// Endpoint: GET <https://api.dhan.co/v2/RenewToken>
pub const DHAN_RENEW_TOKEN_PATH: &str = "/RenewToken";

// ---------------------------------------------------------------------------
// Authentication — SSM Path Construction
// ---------------------------------------------------------------------------

/// Default environment name for SSM path construction.
/// Overridden by `ENVIRONMENT` env var if present.
pub const DEFAULT_SSM_ENVIRONMENT: &str = "dev";

// ---------------------------------------------------------------------------
// Authentication — Circuit Breaker
// ---------------------------------------------------------------------------

/// Maximum consecutive token renewal failures before circuit breaker trips.
pub const TOKEN_RENEWAL_CIRCUIT_BREAKER_THRESHOLD: u32 = 3;

/// Circuit breaker reset timeout in seconds (try again after this).
pub const TOKEN_RENEWAL_CIRCUIT_BREAKER_RESET_SECS: u64 = 60;

/// Dhan `generateAccessToken` cooldown in seconds.
///
/// Dhan enforces an undocumented 2-minute cooldown between token generation
/// requests. We use 125 seconds (2 min + 5 sec safety margin) to ensure
/// we never hit the rate limit on retry.
pub const DHAN_TOKEN_GENERATION_COOLDOWN_SECS: u64 = 125;

/// Maximum backoff delay for boot-time auth retry in seconds.
///
/// Exponential backoff is capped at this value to prevent unbounded waits.
pub const AUTH_RETRY_MAX_BACKOFF_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Binary Packet Byte Offsets
// Source: SDK struct.unpack format strings, verified against Python SDK.
// CRITICAL: Quote and Full packets DIVERGE at offset 34.
// ---------------------------------------------------------------------------

// --- Header offsets (shared by ALL packets) ---

/// Response code byte position in binary header.
pub const HEADER_OFFSET_RESPONSE_CODE: usize = 0;

/// Message length (u16 LE) position in binary header.
pub const HEADER_OFFSET_MESSAGE_LENGTH: usize = 1;

/// Exchange segment byte position in binary header.
pub const HEADER_OFFSET_EXCHANGE_SEGMENT: usize = 3;

/// Security ID (u32 LE) position in binary header.
pub const HEADER_OFFSET_SECURITY_ID: usize = 4;

// --- Market Depth standalone packet body offsets (112 bytes total) ---
// Format: <BHBIf100s> — same LTP offset as Ticker, depth at offset 12.

/// LTP (f32 LE) offset in Market Depth standalone packet.
pub const MARKET_DEPTH_OFFSET_LTP: usize = 8;

/// Start of 5-level market depth (100 bytes) in Market Depth standalone packet.
pub const MARKET_DEPTH_OFFSET_DEPTH_START: usize = 12;

// --- Ticker packet body offsets (16 bytes total) ---

/// Last traded price (f32 LE) offset in Ticker packet.
pub const TICKER_OFFSET_LTP: usize = 8;

/// Last trade time (u32 LE, epoch seconds) offset in Ticker packet.
pub const TICKER_OFFSET_LTT: usize = 12;

// --- Quote packet body offsets (50 bytes total) ---
// Format: <BHBIfHIfIIIffff

/// LTP (f32 LE) offset in Quote packet.
pub const QUOTE_OFFSET_LTP: usize = 8;

/// Last trade quantity (u16 LE) offset in Quote packet.
pub const QUOTE_OFFSET_LTQ: usize = 12;

/// Last trade time (u32 LE) offset in Quote packet.
pub const QUOTE_OFFSET_LTT: usize = 14;

/// Average traded price (f32 LE) offset in Quote packet.
pub const QUOTE_OFFSET_ATP: usize = 18;

/// Cumulative volume (u32 LE) offset in Quote packet.
pub const QUOTE_OFFSET_VOLUME: usize = 22;

/// Total sell quantity (u32 LE) offset in Quote packet.
pub const QUOTE_OFFSET_TOTAL_SELL_QTY: usize = 26;

/// Total buy quantity (u32 LE) offset in Quote packet.
pub const QUOTE_OFFSET_TOTAL_BUY_QTY: usize = 30;

/// Day open (f32 LE) offset in Quote packet — DIVERGES from Full at this offset!
pub const QUOTE_OFFSET_OPEN: usize = 34;

/// Previous close (f32 LE) offset in Quote packet.
pub const QUOTE_OFFSET_CLOSE: usize = 38;

/// Day high (f32 LE) offset in Quote packet.
pub const QUOTE_OFFSET_HIGH: usize = 42;

/// Day low (f32 LE) offset in Quote packet.
pub const QUOTE_OFFSET_LOW: usize = 46;

// --- Full packet body offsets (162 bytes total) ---
// Format: <BHBIfHIfIIIIIIffff100s
// CRITICAL: Shares layout 0-33 with Quote, then DIVERGES at offset 34.
// Full puts OI u32 fields at 34-45, then OHLC floats at 46-61, then depth.

/// Open interest (u32 LE) offset in Full packet — Quote has Open(f32) here!
pub const FULL_OFFSET_OI: usize = 34;

/// OI day high (u32 LE) offset in Full packet — Quote has Close(f32) here!
pub const FULL_OFFSET_OI_DAY_HIGH: usize = 38;

/// OI day low (u32 LE) offset in Full packet — Quote has High(f32) here!
pub const FULL_OFFSET_OI_DAY_LOW: usize = 42;

/// Day open (f32 LE) offset in Full packet.
pub const FULL_OFFSET_OPEN: usize = 46;

/// Previous close (f32 LE) offset in Full packet.
pub const FULL_OFFSET_CLOSE: usize = 50;

/// Day high (f32 LE) offset in Full packet.
pub const FULL_OFFSET_HIGH: usize = 54;

/// Day low (f32 LE) offset in Full packet.
pub const FULL_OFFSET_LOW: usize = 58;

/// Start of 5-level market depth (100 bytes) in Full packet.
pub const FULL_OFFSET_DEPTH_START: usize = 62;

// --- OI packet body offset (12 bytes total) ---

/// Open interest (u32 LE) offset in standalone OI packet.
pub const OI_OFFSET_VALUE: usize = 8;

// --- Previous Close packet body offsets (16 bytes total) ---

/// Previous close price (f32 LE) offset.
pub const PREV_CLOSE_OFFSET_PRICE: usize = 8;

/// Previous OI (u32 LE) offset.
pub const PREV_CLOSE_OFFSET_OI: usize = 12;

// --- Disconnect packet body offset (10 bytes total) ---

/// Disconnect code (u16 LE) offset.
pub const DISCONNECT_OFFSET_CODE: usize = 8;

// ---------------------------------------------------------------------------
// QuestDB ILP — Table Names
// ---------------------------------------------------------------------------

/// QuestDB table: raw tick data from WebSocket feed.
pub const QUESTDB_TABLE_TICKS: &str = "ticks";

/// QuestDB table: 5-level market depth snapshots from Full (code 8) and Market Depth (code 3) packets.
pub const QUESTDB_TABLE_MARKET_DEPTH: &str = "market_depth";

/// QuestDB table: previous close reference data from code 6 packets.
pub const QUESTDB_TABLE_PREVIOUS_CLOSE: &str = "previous_close";

// ---------------------------------------------------------------------------
// Pipeline — Tick Processing Constants
// ---------------------------------------------------------------------------

/// Default tick batch flush size for QuestDB ILP writes.
pub const TICK_FLUSH_BATCH_SIZE: usize = 1000;

/// Default tick flush interval in milliseconds.
pub const TICK_FLUSH_INTERVAL_MS: u64 = 1000;

/// Default depth batch flush size for QuestDB ILP writes.
/// Each depth snapshot writes 5 rows (one per level), so effective row count = batch × 5.
pub const DEPTH_FLUSH_BATCH_SIZE: usize = 200;

// ---------------------------------------------------------------------------
// Pipeline — Tick Validation Constants
// ---------------------------------------------------------------------------

/// Minimum valid exchange timestamp (epoch seconds).
/// Any tick with exchange_timestamp below this is a junk/initialization frame.
/// 946684800 = 2000-01-01T00:00:00Z — safely before any real market data.
pub const MINIMUM_VALID_EXCHANGE_TIMESTAMP: u32 = 946_684_800;

// ---------------------------------------------------------------------------
// IST Timezone Offset (i64)
// ---------------------------------------------------------------------------

/// IST offset from UTC in seconds (5 hours 30 minutes) as i64 for tick timestamp conversion.
pub const IST_UTC_OFFSET_SECONDS_I64: i64 = 19_800;

// ---------------------------------------------------------------------------
// Subscription Planner — ATM Strike Range
// ---------------------------------------------------------------------------

/// Number of strikes above ATM to subscribe for stock options.
pub const STOCK_ATM_STRIKES_ABOVE: usize = 10;

/// Number of strikes below ATM to subscribe for stock options.
pub const STOCK_ATM_STRIKES_BELOW: usize = 10;

/// Total option contracts per stock per side (CE + PE) at one expiry:
/// ATM + 10 above + 10 below = 21 strikes × 2 sides = 42, plus 1 future = 43.
pub const STOCK_CONTRACTS_PER_EXPIRY: usize =
    (1 + STOCK_ATM_STRIKES_ABOVE + STOCK_ATM_STRIKES_BELOW) * 2 + 1;

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
