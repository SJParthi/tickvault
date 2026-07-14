//! Compile-time constants for the tickvault system.
//!
//! All values known at compile time live here. Runtime values live in config TOML.
//! If you see a raw number or string literal in application code, it's a bug.

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 Binary Protocol — Packet Sizes
// Source: Dhan official API spec; verified in Python SDK v2 (src/dhanhq/marketfeed.py)
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
// Confirmed by Dhan (Hardik, 2026-04-06 madefortrade.in/t/63542):
// The limit of 5 is applicable for EACH websocket type INDEPENDENTLY:
//   - Live Market Feed: 5 connections
//   - 20-level Depth: 5 connections
//   - 200-level Depth: 5 connections
// These are separate pools, not a shared 5-connection cap.
// Depth connections are available post 3:30 PM (no data, but connectable).
// ---------------------------------------------------------------------------

/// Maximum instruments per single Live Market Feed WebSocket connection.
pub const MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION: usize = 5000;

/// Maximum concurrent Live Market Feed WebSocket connections per Dhan account.
/// NOTE: This limit is INDEPENDENT from depth connection limits.
pub const MAX_WEBSOCKET_CONNECTIONS: usize = 5;

/// Maximum concurrent 20-level depth WebSocket connections per Dhan account.
/// Independent limit from Live Market Feed and 200-level depth.
pub const MAX_TWENTY_DEPTH_CONNECTIONS: usize = 5;

/// Maximum concurrent 200-level depth WebSocket connections per Dhan account.
/// Independent limit from Live Market Feed and 20-level depth.
pub const MAX_TWO_HUNDRED_DEPTH_CONNECTIONS: usize = 5;

/// Total subscription capacity across all Live Market Feed connections.
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
// Source: Dhan official API spec; verified in Python SDK v2 (src/dhanhq/marketfeed.py)
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
// Dhan Data API Error Codes (WebSocket disconnect + REST error responses)
// Source: docs/dhan-ref/08-annexure-enums.md Section 11
// These codes appear in WebSocket disconnect packets (code 50) and REST
// error responses. All 12 codes from the reference doc are defined here.
// ---------------------------------------------------------------------------

/// 800 — Internal Server Error. Retry with backoff.
pub const DATA_API_INTERNAL_SERVER_ERROR: u16 = 800;

/// 804 — Requested number of instruments exceeds limit.
pub const DATA_API_INSTRUMENTS_EXCEED_LIMIT: u16 = 804;

/// 805 — Too many requests/connections — may result in user being blocked.
/// STOP ALL connections for 60s, then audit connection count.
pub const DATA_API_EXCEEDED_ACTIVE_CONNECTIONS: u16 = 805;

/// 806 — Data APIs not subscribed (plan/subscription issue).
pub const DATA_API_NOT_SUBSCRIBED: u16 = 806;

/// 807 — Access token expired. Refresh token, then reconnect.
pub const DATA_API_ACCESS_TOKEN_EXPIRED: u16 = 807;

/// 808 — Authentication Failed — Client ID or Access Token invalid.
/// Per annexure Section 11: this is "Authentication Failed", NOT "Invalid Client ID".
pub const DATA_API_AUTHENTICATION_FAILED: u16 = 808;

/// 809 — Access token invalid.
/// Per annexure Section 11: this is "Access token invalid", NOT "Authentication Failed".
pub const DATA_API_ACCESS_TOKEN_INVALID: u16 = 809;

/// 810 — Client ID invalid.
/// Per annexure Section 11: this is where "Client ID invalid" lives (NOT 808).
pub const DATA_API_CLIENT_ID_INVALID: u16 = 810;

/// 811 — Invalid Expiry Date.
pub const DATA_API_INVALID_EXPIRY_DATE: u16 = 811;

/// 812 — Invalid Date Format.
pub const DATA_API_INVALID_DATE_FORMAT: u16 = 812;

/// 813 — Invalid SecurityId.
pub const DATA_API_INVALID_SECURITY_ID: u16 = 813;

/// 814 — Invalid Request.
pub const DATA_API_INVALID_REQUEST: u16 = 814;

// Legacy aliases for backward compatibility with existing imports.
// These map to the same numeric values; only the names changed.
/// Legacy alias for `DATA_API_EXCEEDED_ACTIVE_CONNECTIONS` (805).
pub const DISCONNECT_EXCEEDED_ACTIVE_CONNECTIONS: u16 = DATA_API_EXCEEDED_ACTIVE_CONNECTIONS;
/// Legacy alias for `DATA_API_NOT_SUBSCRIBED` (806).
pub const DISCONNECT_DATA_API_SUBSCRIPTION_REQUIRED: u16 = DATA_API_NOT_SUBSCRIBED;
/// Legacy alias for `DATA_API_ACCESS_TOKEN_EXPIRED` (807).
pub const DISCONNECT_ACCESS_TOKEN_EXPIRED: u16 = DATA_API_ACCESS_TOKEN_EXPIRED;
/// Legacy alias for `DATA_API_AUTHENTICATION_FAILED` (808).
/// NOTE: Previously named `DISCONNECT_INVALID_CLIENT_ID` — renamed to match
/// annexure Section 11 which says 808 = "Authentication Failed".
pub const DISCONNECT_AUTHENTICATION_FAILED: u16 = DATA_API_AUTHENTICATION_FAILED;
/// Legacy alias for `DATA_API_ACCESS_TOKEN_INVALID` (809).
/// NOTE: Previously named `DISCONNECT_AUTH_FAILED` — renamed to match
/// annexure Section 11 which says 809 = "Access token invalid".
pub const DISCONNECT_ACCESS_TOKEN_INVALID: u16 = DATA_API_ACCESS_TOKEN_INVALID;

/// 810 — Client ID invalid. Distinct from 808 (auth failed).
pub const DISCONNECT_CLIENT_ID_INVALID: u16 = 810;

// ---------------------------------------------------------------------------
// Dhan WebSocket V2 — Response Codes (Binary Protocol)
// Source: Dhan official API spec; verified in Python SDK v2 (src/dhanhq/marketfeed.py)
// ---------------------------------------------------------------------------

/// Response code for index ticker packet (16 bytes).
pub const RESPONSE_CODE_INDEX_TICKER: u8 = 1;

/// Response code for ticker packet (16 bytes).
pub const RESPONSE_CODE_TICKER: u8 = 2;

/// Response code for market depth standalone packet (112 bytes).
/// Format: `<BHBIf100s>` — Header(8) + LTP(f32) + Depth(5×20 bytes).
/// Dhan API (Python SDK ref): `process_market_depth(data)`.
///
/// NOTE: Not in annexure Section 3 (gap between Ticker(2) and Quote(4)).
/// Documented and handled in Dhan API (Python SDK ref) v2 `process_market_depth()`.
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
// Source: Dhan official API spec; verified in Python SDK v2 (src/dhanhq/marketfeed.py)
// Subscribe codes: 15 (Ticker), 17 (Quote), 21 (Full).
// Unsubscribe = subscribe_code + 1: 16 (Ticker), 18 (Quote), 22 (Full).
// Disconnect = 12 (closes the WebSocket connection).
// ---------------------------------------------------------------------------

/// Connect Feed request code.
/// Annexure Section 2: code 11 = Connect Feed.
pub const FEED_REQUEST_CONNECT: u8 = 11;

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
/// Format: bid_qty(u32) + ask_qty(u32) + bid_orders(u16) + ask_orders(u16) + bid_price(f32) + ask_price(f32) = 20.
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
// Market Depth — Standard (5-Level)
// ---------------------------------------------------------------------------

/// Number of market depth levels in full quote.
pub const MARKET_DEPTH_LEVELS: usize = 5;

// ---------------------------------------------------------------------------
// Market Depth — Per-Level Field Offsets (within a single 20-byte level)
// Source: Dhan API (Python SDK ref) `<IIHHff>` format string.
// Used in Full (code 8) and Market Depth standalone (code 3) packets.
// ---------------------------------------------------------------------------

/// Bid quantity (u32 LE) offset within a depth level.
pub const DEPTH_LEVEL_OFFSET_BID_QTY: usize = 0;

/// Ask quantity (u32 LE) offset within a depth level.
pub const DEPTH_LEVEL_OFFSET_ASK_QTY: usize = 4;

/// Bid orders count (u16 LE) offset within a depth level.
pub const DEPTH_LEVEL_OFFSET_BID_ORDERS: usize = 8;

/// Ask orders count (u16 LE) offset within a depth level.
pub const DEPTH_LEVEL_OFFSET_ASK_ORDERS: usize = 10;

/// Bid price (f32 LE) offset within a depth level.
pub const DEPTH_LEVEL_OFFSET_BID_PRICE: usize = 12;

/// Ask price (f32 LE) offset within a depth level.
pub const DEPTH_LEVEL_OFFSET_ASK_PRICE: usize = 16;

// ---------------------------------------------------------------------------
// Deep Depth Protocol — 20-Level & 200-Level WebSocket Feeds
// Source: Dhan official API spec; verified in Python SDK (src/dhanhq/marketfeed.py), Dhan API docs.
// Separate WebSocket endpoints from the standard feed.
// Bid and ask sides arrive as SEPARATE binary packets.
// ---------------------------------------------------------------------------

/// 20-level depth: number of price levels per side.
pub const TWENTY_DEPTH_LEVELS: usize = 20;

/// 200-level depth: number of price levels per side.
pub const TWO_HUNDRED_DEPTH_LEVELS: usize = 200;

/// Deep depth header size in bytes.
/// Format: msg_length(u16) + feed_response_code(u8) + exchange_segment(u8)
///       + security_id(u32) + msg_sequence(u32) = 12 bytes.
pub const DEEP_DEPTH_HEADER_SIZE: usize = 12;

/// Deep depth level size in bytes.
/// Format: price(f64) + quantity(u32) + orders(u32) = 16 bytes.
pub const DEEP_DEPTH_LEVEL_SIZE: usize = 16;

// ---------------------------------------------------------------------------
// Deep Depth — Per-Level Field Offsets (within a single 16-byte level)
// Source: Dhan API (Python SDK ref) `<dII>` format string in fulldepth.py.
// ---------------------------------------------------------------------------

/// Price (f64 LE) offset within a deep depth level.
pub const DEEP_DEPTH_LEVEL_OFFSET_PRICE: usize = 0;

/// Quantity (u32 LE) offset within a deep depth level.
pub const DEEP_DEPTH_LEVEL_OFFSET_QUANTITY: usize = 8;

/// Orders count (u32 LE) offset within a deep depth level.
pub const DEEP_DEPTH_LEVEL_OFFSET_ORDERS: usize = 12;

/// Feed response code indicating a BID-side depth packet.
pub const DEEP_DEPTH_FEED_CODE_BID: u8 = 41;

/// Feed response code indicating an ASK-side depth packet.
pub const DEEP_DEPTH_FEED_CODE_ASK: u8 = 51;

/// 20-level depth body size per side (20 levels × 16 bytes).
pub const TWENTY_DEPTH_BODY_SIZE: usize = TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE;

/// 20-level depth total packet size per side (header + body).
pub const TWENTY_DEPTH_PACKET_SIZE: usize = DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_BODY_SIZE;

/// 200-level depth body size per side (200 levels × 16 bytes).
pub const TWO_HUNDRED_DEPTH_BODY_SIZE: usize = TWO_HUNDRED_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE;

/// 200-level depth total packet size per side (header + body).
pub const TWO_HUNDRED_DEPTH_PACKET_SIZE: usize =
    DEEP_DEPTH_HEADER_SIZE + TWO_HUNDRED_DEPTH_BODY_SIZE;

/// Maximum instruments per 20-depth WebSocket connection (Dhan limit).
pub const MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION: usize = 50;

/// Maximum instruments per 200-depth WebSocket connection (Dhan limit: 1).
pub const MAX_INSTRUMENTS_PER_TWO_HUNDRED_DEPTH_CONNECTION: usize = 1;

/// Subscription request code for 20-level depth feed.
pub const FEED_REQUEST_TWENTY_DEPTH: u8 = 23;

/// Unsubscription request code for full market depth feed (both 20 and 200 level).
/// Dhan Annexure: UnsubscribeFullDepth = 25. There is NO code 24.
/// Python SDK (fulldepth.py) also uses 25 for unsubscribe.
pub const FEED_UNSUBSCRIBE_TWENTY_DEPTH: u8 = 25;

// ---------------------------------------------------------------------------
// Deep Depth Protocol — Header Byte Offsets
// ---------------------------------------------------------------------------

/// Message length (u16 LE) offset in deep depth header.
pub const DEEP_DEPTH_HEADER_OFFSET_MSG_LENGTH: usize = 0;

/// Feed response code (u8) offset in deep depth header.
pub const DEEP_DEPTH_HEADER_OFFSET_FEED_CODE: usize = 2;

/// Exchange segment (u8) offset in deep depth header.
pub const DEEP_DEPTH_HEADER_OFFSET_EXCHANGE_SEGMENT: usize = 3;

/// Security ID (u32 LE) offset in deep depth header.
pub const DEEP_DEPTH_HEADER_OFFSET_SECURITY_ID: usize = 4;

/// Message sequence number (u32 LE) offset in deep depth header.
pub const DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE: usize = 8;

// ---------------------------------------------------------------------------
// Live Order Update WebSocket
// Source: DhanHQ API docs, Python SDK.
// Separate JSON-based WebSocket (NOT binary).
// ---------------------------------------------------------------------------

/// Message code for order update WebSocket login request.
pub const ORDER_UPDATE_LOGIN_MSG_CODE: u16 = 42;

/// Read timeout for order update WebSocket (seconds).
/// Order updates are infrequent — use longer timeout than market feed.
pub const ORDER_UPDATE_READ_TIMEOUT_SECS: u64 = 120;

/// Read timeout for order update WebSocket outside market hours (seconds).
/// Off-hours: no orders expected, Dhan sends no data. Use 10 minutes to
/// avoid noisy reconnect loops while still detecting dead connections.
pub const ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS: u64 = 600;

/// Maximum reconnection attempts for order update WebSocket.
pub const ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS: u32 = 10;

/// Initial reconnect delay for order update WebSocket (milliseconds).
/// 500ms matches the main feed + depth connection first-retry latency —
/// all 4 WebSocket types share a single aggressive first-retry bar so
/// a transient Dhan reset recovers within a few hundred ms. Doubles on
/// each failure up to `ORDER_UPDATE_RECONNECT_MAX_DELAY_MS`.
pub const ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS: u64 = 500;

/// Maximum reconnect delay for order update WebSocket (milliseconds).
pub const ORDER_UPDATE_RECONNECT_MAX_DELAY_MS: u64 = 60000;

/// Base floor (ms) applied to the MAIN-FEED reconnect wait after a Dhan feed
/// rate-limit (HTTP 429 / DATA-805 "too many requests/connections" class).
/// Dhan's documented guidance for the 805 class is "stop all connections for
/// 60s, then reconnect one at a time" — so the first post-429 reconnect waits
/// at least 60s instead of the instant (0ms) first-retry. Grows ×2 per
/// consecutive 429 up to `WS_RATE_LIMIT_BACKOFF_CAP_MS`. Prevents an
/// instant-reconnect loop from keeping the rate-limit alive forever.
pub const WS_RATE_LIMIT_BACKOFF_BASE_MS: u64 = 60_000;

/// Cap (ms) for the post-429 main-feed reconnect floor (5 minutes).
pub const WS_RATE_LIMIT_BACKOFF_CAP_MS: u64 = 300_000;

/// Fix B (2026-06-30) — a main-feed session that stays Connected for LESS than
/// this many milliseconds before the socket drops is treated as a "short
/// session". Dhan was observed to silently TCP-RST the main-feed socket ~5-6s
/// after each connect; the near-instant (0ms) first reconnect then re-subscribes
/// 775 SIDs immediately, tightening the connect/reset/re-subscribe loop and
/// hastening Dhan's per-IP 429. After a short session the first reconnect is
/// floored (see `WS_SHORT_SESSION_RECONNECT_FLOOR_MS`) so the loop slows down.
/// A session that lived AT OR PAST this threshold keeps the instant first retry
/// (fast recovery from a genuine long-lived drop is correct).
pub const WS_SHORT_SESSION_THRESHOLD_MS: u64 = 10_000;

/// Fix B (2026-06-30) — minimum first-reconnect delay (ms) applied ONLY after a
/// short session (uptime `< WS_SHORT_SESSION_THRESHOLD_MS`). Raises the attempt-0
/// delay from the near-instant 0ms to ~3s so a Dhan immediate-RST loop backs off
/// instead of hammering the endpoint. Never applied to attempts 1+, and never to
/// a long-lived session.
pub const WS_SHORT_SESSION_RECONNECT_FLOOR_MS: u64 = 3_000;

/// Timeout for the order update WebSocket auth response (seconds).
/// After sending the LoginReq (MsgCode 42), the server should respond
/// within this window. 10 seconds is generous for a JSON auth handshake.
pub const ORDER_UPDATE_AUTH_TIMEOUT_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// SEBI Compliance
// ---------------------------------------------------------------------------

/// Maximum orders per second allowed by SEBI regulation.
pub const SEBI_MAX_ORDERS_PER_SECOND: u32 = 10;

/// Minimum audit log retention in years (SEBI requirement).
pub const SEBI_AUDIT_RETENTION_YEARS: u32 = 5;

// ---------------------------------------------------------------------------
// OMS — Circuit Breaker
// ---------------------------------------------------------------------------

/// Consecutive API failures before the OMS circuit breaker opens.
pub const OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD: u32 = 3;

/// Seconds before the OMS circuit breaker transitions from Open to Half-Open.
pub const OMS_CIRCUIT_BREAKER_RESET_SECS: u64 = 30;

/// Maximum length for Dhan `correlationId` field.
/// Dhan spec: max 30 chars, charset `[a-zA-Z0-9 _-]`.
pub const DHAN_CORRELATION_ID_MAX_LENGTH: usize = 30;

// ---------------------------------------------------------------------------
// Risk — Tick Gap Detection Thresholds
// ---------------------------------------------------------------------------

/// Minimum ticks per security before gap detection activates (warmup phase).
/// Prevents false positives during initial subscription when ticks are sparse.
pub const TICK_GAP_MIN_TICKS_BEFORE_ACTIVE: u32 = 5;

/// Warning-level gap threshold in seconds. Gaps >= this trigger a WARN log.
/// 30 seconds without a tick indicates potential feed degradation.
pub const TICK_GAP_ALERT_THRESHOLD_SECS: u32 = 30;

/// Error-level gap threshold in seconds. Gaps >= this trigger an ERROR log
/// (which routes to Telegram alert).
///
/// **Raised from 120s → 300s on 2026-04-24** after live production log
/// showed 988 ERROR entries in 15 minutes for illiquid F&O options that
/// legitimately don't trade for 2-5 minutes at a time. A real feed
/// disconnect is detected by WS ping/pong within 40s (Dhan server
/// timeout) plus the feed-level stall watchdogs;
/// a 5-minute silence on ONE instrument while others keep ticking is
/// illiquidity, not disconnect. The WARN band (30s threshold) still
/// surfaces illiquidity to the aggregated 30s summary log.
pub const TICK_GAP_ERROR_THRESHOLD_SECS: u32 = 300;

/// Stale LTP threshold in seconds. If no tick arrives for a security within
/// this window (wall-clock time), it is considered stale/frozen. 600s = 10 min.
/// Checked periodically by `TickGapTracker::detect_stale_instruments()`.
pub const STALE_LTP_THRESHOLD_SECS: u64 = 600;

/// Cadence for the per-instrument stall-detection poller wired in
/// `run_slow_boot_observability`.
///
/// The poller calls `TickGapTracker::detect_stale_instruments()` which is
/// O(n) over tracked securities (n up to 25k in prod). Running it per-tick
/// would be O(n^2) over a session; running it hourly would delay detection
/// past an operator's natural notice window. 30s is the sweet spot:
/// matches the broader "is something wrong?" cadence used elsewhere
/// (depth rebalancer = 60s, QuestDB health = 2s), so operators develop
/// a consistent mental model of reaction latency.
///
/// Audit finding #2 (2026-04-24) wired the poller. See
/// `.claude/rules/project/audit-findings-2026-04-17.md` Rule 3
/// (background-worker market-hours awareness).
pub const STALE_LTP_SCAN_INTERVAL_SECS: u64 = 30;

/// Backlog-tick age threshold (seconds). A tick whose `exchange_timestamp`
/// is older than `now_ist - BACKLOG_TICK_AGE_THRESHOLD_SECS` is treated as
/// a historical / replay tick by [`tickvault_trading::risk::tick_gap_tracker`],
/// which SKIPS the gap-detection branch for it.
///
/// **2026-04-21 production evidence** (item I4 in the production-fixes
/// queue): after a 234-minute process downtime, Dhan replayed the backlog
/// of missed ticks on reconnect. For illiquid F&O instruments this
/// replay contains 15-minute+ spacing between consecutive exchange
/// timestamps — which the tick gap tracker (correctly, in live-stream
/// semantics) interprets as a disconnection. 365 instruments had WARN
/// gaps and 4,778 ERROR lines fired within 15 minutes, spamming
/// Telegram with non-actionable alerts.
///
/// 60 seconds is conservative: real-time Dhan ticks arrive with < 2 s
/// wall-clock-to-exchange-timestamp delta, so anything older than 60 s
/// is provably not a live-stream gap. The `TICK_GAP_ALERT_THRESHOLD_SECS`
/// (30 s) is only about gaps between CONSECUTIVE exchange timestamps,
/// which is independent of this wall-clock age filter.
pub const BACKLOG_TICK_AGE_THRESHOLD_SECS: u32 = 60;

/// Upper bound for the backlog filter in seconds. A tick whose age exceeds
/// this bound is considered `absurdly old` — outside any realistic Dhan
/// backlog replay window — and falls through to the normal gap-detection
/// logic. The bound exists to keep unit tests with stubbed timestamps
/// (commonly `1_700_000_000` from 2023) exercising the gap path
/// unchanged after the 2026-04-21 backlog-filter addition. 24 hours
/// covers every realistic production backlog (Dhan keeps ~1 trading
/// session of replay) while still filtering out 2-year-old test
/// timestamps.
pub const BACKLOG_TICK_AGE_MAX_SECS: u32 = 86_400;

// ---------------------------------------------------------------------------
// SSM Parameter Store — Secret Path Prefixes
// ---------------------------------------------------------------------------

/// Base path for all secrets in SSM Parameter Store.
///
/// Matches the repo name `tickvault`. The prior namespace was `/dlt` (from
/// the dhan-live-trader legacy name). Run `scripts/migrate-ssm-dlt-to-
/// tickvault.sh` once per environment to copy `/dlt/*` → `/tickvault/*`
/// before this code is deployed; the script is idempotent and a `--delete`
/// flag removes the old namespace after you verify the app boots cleanly.
pub const SSM_SECRET_BASE_PATH: &str = "/tickvault";

/// SSM service path segment for Dhan credentials.
pub const SSM_DHAN_SERVICE: &str = "dhan";

/// Dhan credentials secret names.
pub const DHAN_CLIENT_ID_SECRET: &str = "client-id";
pub const DHAN_CLIENT_SECRET_SECRET: &str = "client-secret";
pub const DHAN_TOTP_SECRET: &str = "totp-secret";
/// SSM secret name for Dhan sandbox client ID (DevPortal).
pub const DHAN_SANDBOX_CLIENT_ID_SECRET: &str = "sandbox-client-id";
/// SSM secret name for Dhan sandbox access token (DevPortal, 30-day validity).
pub const DHAN_SANDBOX_TOKEN_SECRET: &str = "sandbox-token";

// ---------------------------------------------------------------------------
// SSM Parameter Store — Groww Service (second feed, operator lock 2026-06-19)
// `.claude/rules/project/groww-second-feed-scope-2026-06-19.md`
// ---------------------------------------------------------------------------

/// SSM service path segment for the Groww feed.
/// Full path read by TickVault: `/tickvault/<env>/groww/access-token` ONLY.
pub const SSM_GROWW_SERVICE: &str = "groww";

/// SSM parameter name for the Groww ACCESS TOKEN — the daily token minted by
/// the bruteX-owned `groww-token-minter` Lambda (~06:05 IST, EventBridge) into
/// `/tickvault/<env>/groww/access-token` (SecureString, KMS
/// `alias/tickvault-groww`). TickVault is a READ-ONLY consumer of this ONE
/// parameter (IAM reader role `groww-token-minter-reader-tickvault`).
///
/// Operator lock 2026-07-02
/// (`.claude/rules/project/groww-shared-token-minter-2026-07-02.md`):
/// TickVault NEVER mints a Groww token, NEVER reads the `api-key` /
/// `totp-secret` credential parameters (Lambda-only), and NEVER writes any
/// `/tickvault/*/groww/*` parameter. The former credential-name constants were
/// deleted with the mint path; ratchet:
/// `crates/common/tests/groww_no_mint_guard.rs`.
pub const GROWW_ACCESS_TOKEN_SECRET: &str = "access-token";

/// Groww master instrument CSV (public static asset, no auth). Source of every
/// Groww `exchange_token` — joined to NTM ISINs to build the watch-list (§31).
pub const GROWW_INSTRUMENT_CSV_URL: &str =
    "https://growwapi-assets.groww.in/instruments/instrument.csv"; // APPROVED: constants.rs is the single static-URL source (same as INDEX_CONSTITUENCY_BASE_URL)

// ---------------------------------------------------------------------------
// Groww NATIVE-RUST shadow client (PR-R1, operator "go" 2026-07-04 —
// `.claude/rules/project/groww-second-feed-scope-2026-06-19.md` §35).
// Protocol extracted from the official growwapi-1.5.0 wheel (the sanctioned
// reference per §32; brutex code banned) — see `docs/groww-ref/`.
// ---------------------------------------------------------------------------

/// Groww live-feed NATS-over-WebSocket endpoint (wheel `feed.py::_GROWW_SOCKET_URL`).
pub const GROWW_SOCKET_URL: &str = "wss://socket-api.groww.in"; // APPROVED: constants.rs is the single WS-URL source

/// Groww per-session socket-token mint endpoint (wheel `client.py::generate_socket_token`).
///
/// LIVE-FEED-AUTH class (the KEEP class of
/// `no-rest-except-live-feed-2026-06-27.md` §2/§3 — recorded there 2026-07-04):
/// consumes the SSM-read ACCESS token (`Authorization: Bearer …`) to mint a
/// per-session NATS user JWT bound to a fresh ed25519 nkey. It does NOT mint
/// the access token (shared-minter lock 2026-07-02 untouched) and carries NO
/// market data. This is exactly the call the Python sidecar's SDK already
/// makes on every feed (re)construction.
pub const GROWW_SOCKET_TOKEN_URL: &str = "https://api.groww.in/v1/api/apex/v1/socket/token/create/"; // APPROVED: constants.rs is the single REST-URL source

/// Groww data directory (watch files, sidecar NDJSON, shadow NDJSON).
pub const GROWW_DATA_DIR: &str = "data/groww";

/// The native shadow client's OWN NDJSON capture file (same `GrowwTickLine`
/// line schema as the sidecar's `data/groww/live-ticks.ndjson`), rotated to
/// `rust-live-ticks-YYYYMMDD.ndjson` at the IST day boundary — mirrors the
/// sidecar's rotation so the parity comparer sees symmetrical archives.
pub const GROWW_NATIVE_SHADOW_NDJSON_PATH: &str = "data/groww/rust-live-ticks.ndjson";

/// First reconnect delay for the native shadow client (bounded expo backoff).
pub const GROWW_NATIVE_RECONNECT_BASE_SECS: u64 = 5;

/// Reconnect backoff ceiling for the native shadow client.
pub const GROWW_NATIVE_RECONNECT_MAX_SECS: u64 = 60;

/// Floor between access-token SSM re-reads after an auth-class failure —
/// mirrors the sidecar's `AUTH_RETRY_FLOOR_SECS` (token-minter lock 2026-07-02:
/// re-read, never mint).
pub const GROWW_NATIVE_AUTH_RETRY_FLOOR_SECS: u64 = 60;

/// Poll interval while waiting for today's watch file to appear.
pub const GROWW_NATIVE_WATCH_POLL_SECS: u64 = 30;

/// Bounded channel capacity between the shadow client's read loop and its
/// NDJSON writer task. Shadow-only: overflow drops are counted loudly
/// (`tv_groww_native_dropped_total`) and never touch the production capture
/// chain (the sidecar remains the durable path in R1).
pub const GROWW_NATIVE_WRITER_CHANNEL_CAPACITY: usize = 65_536;

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
// SSM Parameter Store — API Service (added 2026-04-25)
// ---------------------------------------------------------------------------

/// SSM service path segment for API server credentials.
///
/// 2026-04-25 security audit (PR #357): TV_API_TOKEN was previously read
/// from a process env var (Docker `environment:` block or systemd
/// `EnvironmentFile`), which is plaintext on disk and visible via
/// `docker inspect` / `/proc/<pid>/environ`. Project mandate per
/// `.claude/rules/project/rust-code.md` is "All secrets from AWS SSM
/// Parameter Store". This service path moves the bearer token into SSM
/// alongside Dhan, QuestDB, Grafana, and Telegram credentials.
pub const SSM_API_SERVICE: &str = "api";

/// SSM key for API bearer token (mutating-endpoint authentication).
///
/// Full path: `/tickvault/<env>/api/bearer-token`
pub const API_BEARER_TOKEN_SECRET: &str = "bearer-token";

// `SSM_VALKEY_SERVICE` + `VALKEY_PASSWORD_SECRET` constants DELETED in
// #O4 (2026-05-24) — Valkey removed from the runtime. The dual-instance
// lock moved to `/tickvault/<env>/instance-lock` (PR #764, see
// `instance_lock::INSTANCE_LOCK_SSM_PATH_PREFIX`).

// ---------------------------------------------------------------------------
// SSM Parameter Store — SNS Service
// ---------------------------------------------------------------------------

/// SSM service path segment for SNS configuration.
pub const SSM_SNS_SERVICE: &str = "sns";

/// SSM key for the phone number to send SNS SMS alerts to.
/// Value must be in E.164 format: `+<country_code><number>` (e.g., "+919876543210").
pub const SNS_PHONE_NUMBER_SECRET: &str = "phone-number";

// ---------------------------------------------------------------------------
// Network / IP Verification — SSM Constants
// ---------------------------------------------------------------------------

/// SSM service path segment for network configuration.
pub const SSM_NETWORK_SERVICE: &str = "network";

/// SSM key for the expected static public IP (BSNL/ISP-assigned).
/// Value must be a valid IPv4 address string (e.g., "203.0.113.42").
pub const STATIC_IP_SECRET: &str = "static-ip";

/// Primary URL for public IP detection (AWS-owned, plain text response).
pub const PUBLIC_IP_CHECK_PRIMARY_URL: &str = "https://checkip.amazonaws.com"; // APPROVED: infrastructure constant

/// Fallback URL for public IP detection (plain text response).
pub const PUBLIC_IP_CHECK_FALLBACK_URL: &str = "https://api.ipify.org"; // APPROVED: infrastructure constant

/// Timeout in seconds for public IP detection HTTP requests.
pub const PUBLIC_IP_CHECK_TIMEOUT_SECS: u64 = 10;

/// Maximum retry attempts for public IP detection (primary + fallback).
pub const PUBLIC_IP_CHECK_MAX_RETRIES: u32 = 3;

/// GAP-NET-01 (AUTH-P12) — interval between runtime public-IP re-checks by
/// the mid-session IP monitor. 300s = 5 minutes, matching the boot-time
/// verify cadence; a confirmed mismatch therefore takes
/// `IP_MONITOR_MISMATCH_CONFIRM_THRESHOLD` × this interval of the SAME wrong
/// IP before any action fires (never a one-shot flap).
pub const IP_MONITOR_CHECK_INTERVAL_SECS: u64 = 300;

/// GAP-NET-01 (AUTH-P12) — number of CONSECUTIVE mismatched poll cycles the
/// runtime IP monitor requires before it acts (confirm-twice debounce). A
/// single transient metadata blip (1 cycle) never triggers an alert or halt;
/// only a sustained wrong IP does.
pub const IP_MONITOR_MISMATCH_CONFIRM_THRESHOLD: u32 = 2;

/// GAP-NET-01 (AUTH-P12) — grace delay after the CRITICAL halt-class ERROR is
/// logged, before the runtime IP monitor calls `std::process::exit`. Gives the
/// log/Telegram sinks a moment to flush the alert so the operator sees WHY the
/// process exited.
pub const IP_MONITOR_HALT_FLUSH_DELAY_SECS: u64 = 2;

/// Phase 0 Item 18b — Max retry attempts for the boot-time Dhan static
/// IP gate (`/v2/ip/getIP`) when `orders_allowed = false`. Bounds the
/// worst-case boot blocking time to `MAX_ATTEMPTS * INTERVAL_SECS`.
///
/// 30 attempts x 60 s = 30 min — long enough to absorb Dhan's
/// IP-whitelist propagation delay after an operator-side modify on
/// web.dhan.co, short enough that an unattended boot eventually
/// surfaces the failure for operator action.
pub const STATIC_IP_BOOT_RETRY_MAX_ATTEMPTS: u32 = 30;

/// Phase 0 Item 18b — Interval between retry attempts during the boot
/// gate. Dhan's API rate limit is 5 req/sec on data endpoints, so
/// 60 s is comfortably below any throttling threshold.
pub const STATIC_IP_BOOT_RETRY_INTERVAL_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Docker Container Naming
// ---------------------------------------------------------------------------

/// Container name prefix for all tickvault Docker services.
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

/// Filename for the rkyv binary cache of `FnoUniverse`.
/// Written alongside CSV cache; loaded first on market-hours restart for sub-ms recovery.
pub const BINARY_CACHE_FILENAME: &str = "fno-universe.rkyv";

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

/// Total subscribed indices: 6 F&O + 23 Display = 29.
///
/// 2026-04-25: Reduced from 31 → 29. FINNIFTY and MIDCPNIFTY were dropped from
/// the F&O index list (was 8 → now 6), see `VALIDATION_MUST_EXIST_INDICES`.
pub const TOTAL_SUBSCRIBED_INDEX_COUNT: usize = 29;

// ---------------------------------------------------------------------------
// F&O Universe — Full Chain Indices
// ---------------------------------------------------------------------------

/// Index symbols that get full option chain subscriptions (current expiry only).
///
/// 2026-04-25: Reduced from 5 to 3. FINNIFTY and MIDCPNIFTY were dropped to free
/// 25K WebSocket capacity for stock F&O ATM±25 coverage. Verified against live
/// QuestDB on 2026-04-25: 3 indices full-chain = 2,037 contracts; 216 stocks at
/// ATM±25 = 22,042 contracts; total subscription = 24,324 ≤ 25,000 (676 headroom).
pub const FULL_CHAIN_INDEX_SYMBOLS: &[&str] = &["NIFTY", "BANKNIFTY", "SENSEX"];

/// Number of full-chain indices.
pub const FULL_CHAIN_INDEX_COUNT: usize = 3;

/// ATM ± N strikes per side cap for stock F&O option subscriptions.
///
/// 2026-04-25: NSE intraday circuit breaker is 20% — ATM±25 strikes covers any
/// realistic intraday move on every F&O stock. Set as a CONSTANT (not config)
/// because the math is invariant: 25 above + 25 below = 51 strikes per side per
/// option type → ~103 contracts/stock × 216 stocks ≈ 22,042 contracts.
pub const STOCK_OPTION_ATM_STRIKES_EACH_SIDE: usize = 25;

/// Capacity ratchet target — Telegram WARN if planner exceeds this on any boot.
///
/// Hard cap is `MAX_TOTAL_SUBSCRIPTIONS` (25,000). The 24,500 target leaves 500
/// slots for capacity drift between F&O CSV updates (new strikes, new F&O stocks).
/// If breached, operator decides whether to drop a stock or tighten ATM cap.
pub const MAX_TOTAL_SUBSCRIPTIONS_TARGET: usize = 24_500;

// ---------------------------------------------------------------------------
// Phase 0 LEAN MVP — locked universe pins (2026-05-13 operator decision)
// ---------------------------------------------------------------------------

/// INDIA VIX SecurityId on the IDX_I segment.
///
/// Pinned from the Dhan instrument master CSV — cross-checked against
/// `DISPLAY_INDICES` entry `("INDIA VIX", 21, "Volatility")` above. Used by
/// Phase 0 planner to filter display indices to VIX only under the
/// `IndicesUnderlyingsOnly` scope (Item 1 of `topic-PHASE-0-LEAN-LOCKED.md`).
pub const INDIA_VIX_SECURITY_ID: crate::types::SecurityId = 21;

/// Phase 0 IDX_I subscription whitelist (4 SIDs in Ticker mode):
/// NIFTY (13) + BANKNIFTY (25) + SENSEX (51) + INDIA VIX (21).
///
/// Source of truth for the Phase 0 LEAN scope per
/// `topic-PHASE-0-LEAN-LOCKED.md` line 24. Adding/removing entries must
/// also update `PHASE_0_IDX_I_COUNT` (compile-time enforced below).
pub const PHASE_0_IDX_I_SYMBOLS: &[&str] = &["NIFTY", "BANKNIFTY", "SENSEX", "INDIA VIX"];

/// Phase 0 IDX_I count: 4 (= NIFTY + BANKNIFTY + SENSEX + INDIA VIX).
pub const PHASE_0_IDX_I_COUNT: usize = 4;

/// Phase 0 total subscription target: ~222 SIDs
/// (4 IDX_I + ~218 NSE_EQ F&O underlyings).
///
/// Hard ceiling — Telegram WARN if planner output exceeds this under the
/// `IndicesUnderlyingsOnly` scope. The 218 NSE_EQ count varies day-to-day
/// with the F&O underlying list (NSE adds/removes stocks); 230 leaves
/// headroom without admitting drift past the lean-MVP target.
pub const PHASE_0_TOTAL_SIDS_TARGET: usize = 230;

const _: () = assert!(
    PHASE_0_IDX_I_COUNT == PHASE_0_IDX_I_SYMBOLS.len(),
    "PHASE_0_IDX_I_COUNT does not match PHASE_0_IDX_I_SYMBOLS.len()"
);

/// Compile-time cross-check (Phase 0 Item 1 hostile-review H3 fix,
/// 2026-05-13): `INDIA_VIX_SECURITY_ID` MUST match the SID of the
/// `"INDIA VIX"` entry in `DISPLAY_INDEX_ENTRIES`. Without this assert
/// a future edit to `DISPLAY_INDEX_ENTRIES` (e.g. Dhan re-issues VIX
/// to a different SID and someone updates only the entries table)
/// would silently desync the constant from the universe builder's
/// view of VIX, causing the Phase 0 planner to drop VIX entirely.
const _: () = {
    // Linear scan — compile-time, ~24 iterations, free at runtime.
    let mut i = 0;
    let mut found = false;
    while i < DISPLAY_INDEX_ENTRIES.len() {
        let (name, sid, _) = DISPLAY_INDEX_ENTRIES[i];
        // const fn-safe byte compare for "INDIA VIX".
        if name.len() == 9
            && name.as_bytes()[0] == b'I'
            && name.as_bytes()[1] == b'N'
            && name.as_bytes()[2] == b'D'
            && name.as_bytes()[3] == b'I'
            && name.as_bytes()[4] == b'A'
            && name.as_bytes()[5] == b' '
            && name.as_bytes()[6] == b'V'
            && name.as_bytes()[7] == b'I'
            && name.as_bytes()[8] == b'X'
        {
            assert!(
                sid == INDIA_VIX_SECURITY_ID,
                "INDIA_VIX_SECURITY_ID must match DISPLAY_INDEX_ENTRIES INDIA VIX SID"
            );
            found = true;
        }
        i += 1;
    }
    assert!(
        found,
        "DISPLAY_INDEX_ENTRIES must contain an INDIA VIX entry — Phase 0 depends on it"
    );
};

/// AWS-lifecycle LOCKED main-feed pool size (PR #7b).
///
/// Under the single-variant Indices4Only scope only 4 IDX_I SIDs are
/// subscribed — well below the 5,000-per-connection Dhan cap. Spinning up
/// 5 main-feed WebSocket connections is wasteful: 4 of them would sit
/// idle holding zero instruments, fragment the token/IP usage budget,
/// and produce 4 false-positive "connection idle" watchdog signals.
///
/// Item 2 of `topic-PHASE-0-LEAN-LOCKED.md` reduces the live count to 1.
/// The other 4 stay defined in the codebase + ratchets — Phase 2 will
/// restore them by widening the scope, not by editing this constant.
///
/// Pool sizing is decided by `effective_main_feed_pool_size(scope, ..)`
/// in `crates/common/src/config.rs`; this constant is the locked Phase 0
/// value referenced from that helper and from the pool ratchet tests.
pub const PHASE_0_MAIN_FEED_CONNECTION_COUNT: usize = 1;

// ---------------------------------------------------------------------------
// Sub-PR #2 of 2026-05-27 daily-universe expansion — boot-step deadlines.
// ---------------------------------------------------------------------------
// Per `.claude/rules/project/daily-universe-scope-expansion-2026-05-27.md`
// section 19 ("Boot-step deadlines + EC2 cron heartbeat"). The §4 infinite-
// retry policy applies ONLY to the CSV fetch path. Pre-CSV boot steps
// (auth, IP whitelist, QuestDB DDL) have BOUNDED deadlines so a stuck Dhan
// auth / unreachable QuestDB / DNS-failed IP-whitelist endpoint fails fast
// rather than wedging the boot indefinitely.
//
// Consumed by Sub-PR #10's boot orchestrator (Step 6c) via
// `tokio::time::timeout(Duration::from_secs(BOOT_STEP_*_TIMEOUT_SECS), ...)`.
// Wiring is deferred to #10 so this PR is pure declarative (zero runtime
// behaviour change).

/// Step 6 (Dhan auth — TOTP -> JWT) total deadline. Includes up to 3 ×
/// 20s internal retries. Exceeding this fires Severity::Critical Telegram
/// + BOOT BLOCKS. Per rule file §19 boot-step deadline table.
pub const BOOT_STEP_AUTH_TIMEOUT_SECS: u64 = 60;

/// Step 6a (Dhan static-IP whitelist `GET /v2/ip/getIP`) deadline.
/// Exceeding fires Severity::Critical + BOOT BLOCKS. Per rule file §19.
pub const BOOT_STEP_IP_WHITELIST_TIMEOUT_SECS: u64 = 30;

/// Step 6b (QuestDB DDL — includes new `instrument_lifecycle` +
/// `instrument_lifecycle_audit` tables in Sub-PR #9) deadline. Exceeding
/// fires Severity::Critical via `BOOT-01`/`BOOT-02` runbook + BOOT BLOCKS.
/// Per rule file §19.
pub const BOOT_STEP_QUESTDB_DDL_TIMEOUT_SECS: u64 = 60;

/// Single-attempt Dhan Detailed-CSV fetch deadline (Sub-PR #3 will consume).
/// Per rule file §18 hardening contract. The §4 infinite-retry policy
/// wraps this per-attempt timeout — a single 60s-blocking GET is bounded;
/// failure → retry per §4 escalation ladder, not a permanent halt.
pub const INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS: u64 = 60;

/// Maximum daily-universe size (per rule file §2 + §4 + §15). Boot HALTS
/// if computed universe size is outside `[MIN_DAILY_UNIVERSE_SIZE,
/// MAX_DAILY_UNIVERSE_SIZE]`. Sub-PR #11 (boot-time-of-day guard) will
/// enforce; this PR declares the bound.
pub const MIN_DAILY_UNIVERSE_SIZE: usize = 100;

/// See `MIN_DAILY_UNIVERSE_SIZE`. Raised 400 → 1200 per rule file §31
/// (operator authorization 2026-06-06) to fit the NIFTY Total Market
/// stock expansion (~1,000 live SIDs + headroom). Still a boot-time
/// validation bound only — not a pre-allocation (the aggregator is sized
/// by the independent `AGGREGATOR_CAPACITY`).
pub const MAX_DAILY_UNIVERSE_SIZE: usize = 1200;

/// CSV download body size cap (per rule file §18 hardening contract).
/// Sub-PR #3 will enforce via `reqwest` bounded read. Expected real-world
/// CSV is 5-15 MB; 50 MB cap absorbs growth without admitting gigabyte
/// streams as a DoS surface.
pub const MAX_CSV_BODY_BYTES: usize = 50 * 1024 * 1024;

/// Dhan Detailed instrument-master CSV URL (public static file).
/// Operator-locked 2026-05-27 per rule file §3. Sub-PR #3 consumes this.
pub const DHAN_DETAILED_CSV_URL: &str =
    "https://images.dhan.co/api-data/api-scrip-master-detailed.csv"; // APPROVED: infrastructure constant — operator-locked daily-universe expansion 2026-05-27 §3

/// Connect-timeout for the Detailed CSV fetch (Sub-PR #3). Distinct
/// from the whole-response timeout `INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS`.
pub const CSV_CONNECT_TIMEOUT_SECS: u64 = 10;

/// NSE holiday-master cross-check URL (operator-verified 2026-05-27 per
/// `docs/operator/nse-trading-calendar-2026.md` §1c). Sub-PR #11c consumes
/// this for best-effort cross-validation of the operator-maintained
/// `nse_holidays` config list. This URL is an undocumented JSON endpoint
/// — its availability is NOT contractually guaranteed by NSE. Best-effort
/// only; the authoritative source is the operator-maintained config list.
pub const DHAN_NSE_HOLIDAY_CROSS_CHECK_URL: &str =
    "https://www.nseindia.com/api/holiday-master?type=trading"; // APPROVED: infrastructure constant — operator-verified NSE 2026 research 2026-05-27

// ---------------------------------------------------------------------------
// Holiday-calendar coverage-horizon staleness guard (W2 PR#5, 2026-07-10 —
// audit follow-up row 15). The `nse_holidays` config list covers one
// calendar year at a time; `is_holiday()` has NO year bound, so once the
// year rolls past the newest listed holiday, every un-listed weekday
// holiday silently reads as a trading day. The guard pages the operator
// BEFORE that cliff so the next official NSE circular gets pasted in time.
// ---------------------------------------------------------------------------

/// Page the operator when fewer than this many days of holiday-calendar
/// coverage remain (strict `<` — exactly 60 days left is not yet stale).
///
/// Why 60: NSE typically publishes the next calendar year's trading-holiday
/// circular in December. With coverage ending Dec 31, daily paging begins
/// ~Nov 2 — a bounded "watch for the circular, then paste it" reminder
/// window. 90 would start paging ~Oct 2, months before the circular
/// plausibly exists (pager fatigue for an un-actionable reminder); 30 (Dec
/// 1) leaves too little slack for a late circular + year-end operator
/// absence. The pages stop the moment the new year's list lands in config.
pub const CALENDAR_COVERAGE_WARN_THRESHOLD_DAYS: i64 = 60;

/// Re-check cadence for the calendar-staleness watchdog. The AWS box
/// restarts every trading weekday (08:30 IST auto-start), so the boot-time
/// check alone gives daily cadence in prod; the 6h in-process loop covers
/// long-lived local sessions. The per-IST-date alert latch keeps Telegram
/// at ≤1 page per process per day regardless of this cadence.
pub const CALENDAR_STALENESS_CHECK_INTERVAL_SECS: u64 = 21_600;

/// Retry pause when the staleness check finds a stale calendar but the
/// lazily-filled Telegram notifier slot is still empty (boot prefix) — the
/// alert is re-attempted at this pace instead of being lost for the day.
pub const CALENDAR_STALENESS_NOTIFIER_RETRY_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// F&O Universe — Index Aliases (FNO symbol → IDX_I symbol)
// ---------------------------------------------------------------------------

/// Aliases for indices whose published symbol differs from the canonical
/// allowlist / IDX_I-row symbol.
///
/// Format: `(alias_symbol, canonical_symbol)`. Both sides are stored
/// already-normalized (uppercase, single-spaced) so the lookup matches a
/// `normalize_index_symbol`-normalized input directly. Used in two directions:
/// (a) FNO underlying symbol → IDX_I row symbol, and (b) Dhan IDX_I row symbol
/// → canonical NSE-index-allowlist entry (`index_extractor::canonicalize_index_symbol`).
///
/// `NIFTY NEXT 50` is published by Dhan as both `NIFTYNXT50` and the spaced
/// `NIFTY NXT 50`; both must resolve to the allowlisted `NIFTY NEXT 50`, else
/// that index value is dropped at boot (live WARN `index allowlist MISS ...
/// NIFTY NEXT 50`, observed 2026-06-26).
pub const INDEX_SYMBOL_ALIASES: &[(&str, &str)] = &[
    ("NIFTYNXT50", "NIFTY NEXT 50"),
    ("NIFTY NXT 50", "NIFTY NEXT 50"),
    ("SENSEX50", "SNSX50"),
    // Groww→Dhan spelling bridges (2026-06-28): the Groww index DISPLAY `name`
    // for these three differs from the Dhan-allowlist trading-symbol spelling,
    // so the Groww index-coverage audit would falsely flag them absent. Additive
    // (alias→canonical) — they only ADD a fallback match, never remove one, so
    // the Dhan-side `extract_indices` path is unaffected for already-matching
    // rows. Stored already-normalized (uppercase, single-spaced).
    ("NIFTY MIDCAP SELECT", "MIDCPNIFTY"),
    ("NIFTY MIDCAP 50", "NIFTYMCAP50"),
    ("NIFTY TOTAL MARKET", "NIFTY TOTAL MKT"),
    // Groww→Dhan spelling bridge (2026-07-13, Groww spot-leg INDIA VIX
    // scope addition): the live Groww master (2026-06-28 capture — the
    // `REAL_GROWW_NSE_INDICES` fixture) publishes India VIX with the
    // compact token `INDIAVIX` (display name "India Vix" already
    // normalizes to the canonical "INDIA VIX"). Additive alias so the
    // runtime VIX resolution matches on EITHER identity key.
    ("INDIAVIX", "INDIA VIX"),
];

// ---------------------------------------------------------------------------
// F&O Universe — Validation: Must-Exist Price IDs
// ---------------------------------------------------------------------------

/// Known index security IDs that MUST exist after universe build.
/// (underlying_symbol, expected IDX_I security_id).
/// 2026-04-25: First 3 entries are the F&O full-chain indices (NIFTY,
/// BANKNIFTY, SENSEX) — DO NOT REORDER. `greeks_pipeline.rs` slices `[..3]`
/// to fetch greeks only for F&O indices. Display-only indices follow.
pub const VALIDATION_MUST_EXIST_INDICES: &[(&str, SecurityId)] = &[
    ("NIFTY", 13),
    ("BANKNIFTY", 25),
    ("SENSEX", 51),
    ("NIFTYNXT50", 38),
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

/// I-P0-02: Minimum expected derivative contract count after universe build.
/// A truncated CSV (< 100 derivatives) is almost certainly corrupt or incomplete.
/// Real NSE F&O universe has 5000+ contracts on any given day.
pub const VALIDATION_MIN_DERIVATIVE_COUNT: usize = 100;

// ---------------------------------------------------------------------------
// Application Identity
// ---------------------------------------------------------------------------

/// Application name used in logs, metrics, and tracing.
pub const APPLICATION_NAME: &str = "tickvault";

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

/// QuestDB table: NSE trading calendar (holidays + Muhurat sessions).
pub const QUESTDB_TABLE_NSE_HOLIDAYS: &str = "nse_holidays";

/// QuestDB table: NSE index constituency (daily snapshot from niftyindices.com).
pub const QUESTDB_TABLE_INDEX_CONSTITUENTS: &str = "index_constituents";

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
// Market Hours — Tick Persistence Window
// ---------------------------------------------------------------------------
// Only ticks with exchange timestamps inside [09:00, 15:30) IST are persisted
// to QuestDB. Pre-market and post-market ticks still flow through broadcast
// and candle aggregation but are NOT stored.

/// Seconds-of-day (IST) at which tick persistence starts: 09:00:00 = 9 × 3600.
pub const TICK_PERSIST_START_SECS_OF_DAY_IST: u32 = 32_400;

/// Seconds-of-day (IST) at which tick persistence ends: 15:30:00 = 15 × 3600 + 30 × 60.
/// The end is **exclusive** — a tick at exactly 15:30:00 is NOT persisted.
pub const TICK_PERSIST_END_SECS_OF_DAY_IST: u32 = 55_800;

/// CCL-06 (permutation-coverage audit §140): seconds-of-day (IST) at which the
/// Muhurat (Diwali evening) trading-session persist window OPENS: 18:00:00 =
/// 18 × 3600. This is a DELIBERATE SUPERSET of the historical announced NSE
/// Muhurat windows (2023 was 18:15–19:15, 2024 was 18:00–19:00) so a
/// slightly-early tick is still captured. Only active on a Muhurat date (gated
/// by the boot `muhurat` flag — see `crate::muhurat`); on every trading/mock
/// day this window is ignored, so the regular `[09:00, 15:30)` behaviour is
/// byte-for-byte unchanged.
pub const MUHURAT_PERSIST_START_SECS_OF_DAY_IST: u32 = 64_800;

/// CCL-06: seconds-of-day (IST) at which the Muhurat persist window CLOSES:
/// 19:30:00 = 19 × 3600 + 30 × 60. **Exclusive** — a tick at exactly 19:30:00
/// is NOT persisted (matches the half-open `[start, end)` contract of the
/// regular window). Superset upper bound (announced windows have ended by
/// 19:15) so a slightly-late tick is still captured.
pub const MUHURAT_PERSIST_END_SECS_OF_DAY_IST: u32 = 70_200;

/// Operator-locked 2026-05-25: post-market historical fetch + cross-verify
/// window START. Begins at 15:30:00 IST (= `TICK_PERSIST_END_SECS_OF_DAY_IST`).
/// Operations gated by this constant: 90-day historical fetch, current-day
/// intraday fetch (1m/5m/15m/60m), cross-verification.
pub const POST_MARKET_FETCH_WINDOW_START_SECS_OF_DAY_IST: u32 = 55_800;

/// Operator-locked 2026-05-25: post-market historical fetch + cross-verify
/// window END. Stops at 23:00:00 IST = 23 × 3600 = 82_800. After this
/// boundary, in-flight operations abort cleanly and tomorrow's 15:30 IST
/// retry picks up where today left off. The 7.5h window (15:30→23:00 IST)
/// gives ample retry budget for Dhan REST backoff ladders (DH-904 + DH-805)
/// while bounding the operation to a single trading-day boundary.
pub const POST_MARKET_FETCH_WINDOW_END_SECS_OF_DAY_IST: u32 = 82_800;

/// Seconds in a day (86,400). Used for modulo arithmetic in persist window check.
pub const SECONDS_PER_DAY: u32 = 86_400;

/// Phase 0 Item 20 — seconds-of-day (IST) at which the orphan-position
/// watchdog fires: 15:25:00 = 15 × 3600 + 25 × 60 = 55_500. The 5-minute
/// headroom before the 15:30 close lets the DETECT → AUDIT → Telegram
/// chain complete (Phase 0 dry-run) AND lets a Phase 1+ live exit
/// attempt complete before the exchange rejects late orders.
pub const ORPHAN_POSITION_WATCHDOG_TIME_SECS_IST: u32 = 55_500;

/// Drain buffer (seconds) after market close before aborting WebSocket handles.
/// Allows in-flight ticks (last 15:29 candle) to reach the tick processor channel
/// before the WebSocket read loop is killed.
pub const MARKET_CLOSE_DRAIN_BUFFER_SECS: u64 = 2;

/// Maximum acceptable boot time in seconds. If exceeded, CRITICAL Telegram alert fires.
/// Individual boot steps have their own timeouts; this is the overall ceiling.
pub const BOOT_TIMEOUT_SECS: u64 = 120;

// ---------------------------------------------------------------------------
// Authentication — TOTP Configuration
// ---------------------------------------------------------------------------

/// TOTP digit count (Dhan uses 6-digit codes).
pub const TOTP_DIGITS: usize = 6;

/// TOTP time period in seconds (standard 30-second window).
pub const TOTP_PERIOD_SECS: u64 = 30;

/// TOTP skew tolerance (number of periods to accept before/after current).
pub const TOTP_SKEW: u8 = 1;

/// Maximum TOTP retry attempts before treating as permanent failure.
/// TOTP can fail due to window boundary timing — retrying with a fresh
/// 30-second window resolves transient issues without masking bad secrets.
pub const TOTP_MAX_RETRIES: u32 = 2;

// ---------------------------------------------------------------------------
// Authentication — Dhan REST API Endpoint Paths
// ---------------------------------------------------------------------------

/// Path for initial token generation (appended to auth_base_url).
/// Endpoint: POST <https://auth.dhan.co/app/generateAccessToken>
pub const DHAN_GENERATE_TOKEN_PATH: &str = "/app/generateAccessToken";

/// Path for token renewal (appended to rest_api_base_url).
/// Endpoint: GET <https://api.dhan.co/v2/RenewToken>
pub const DHAN_RENEW_TOKEN_PATH: &str = "/RenewToken";

/// Path for intraday minute candle data (appended to rest_api_base_url).
/// Endpoint: POST <https://api.dhan.co/v2/charts/intraday>
pub const DHAN_CHARTS_INTRADAY_PATH: &str = "/charts/intraday";

/// Path for daily candle data (appended to rest_api_base_url).
/// Endpoint: POST <https://api.dhan.co/v2/charts/historical>
pub const DHAN_CHARTS_HISTORICAL_PATH: &str = "/charts/historical";

/// Path for the full option chain (appended to rest_api_base_url).
/// Endpoint: POST <https://api.dhan.co/v2/optionchain>
/// Requires BOTH `access-token` AND `client-id` headers; rate limit is
/// 1 unique request per 3 seconds (re-verified 2026-07-12 — UNCHANGED;
/// multiple DISTINCT underlyings may go concurrently within the window).
/// Re-added 2026-07-12 for the per-minute REST pipeline PR-3 — the
/// 2026-06-28 deletion removed the prior `OPTION_CHAIN_*` constants with
/// the retired subsystem; this is the §8 REBUILD, not a revival.
pub const DHAN_OPTION_CHAIN_PATH: &str = "/optionchain";

/// Path for the option-chain expiry list (appended to rest_api_base_url).
/// Endpoint: POST <https://api.dhan.co/v2/optionchain/expirylist>
/// Same headers + rate limit class as [`DHAN_OPTION_CHAIN_PATH`].
pub const DHAN_OPTION_CHAIN_EXPIRYLIST_PATH: &str = "/optionchain/expirylist";

// ---------------------------------------------------------------------------
// Authentication — User Profile & IP Management Endpoints
// ---------------------------------------------------------------------------

/// Path for user profile (appended to rest_api_base_url).
/// Endpoint: GET <https://api.dhan.co/v2/profile>
pub const DHAN_USER_PROFILE_PATH: &str = "/profile";

/// Path to set static IP for order APIs (appended to rest_api_base_url).
/// Endpoint: POST <https://api.dhan.co/v2/ip/setIP>
pub const DHAN_SET_IP_PATH: &str = "/ip/setIP";

/// Path to modify static IP for order APIs (appended to rest_api_base_url).
/// Endpoint: PUT <https://api.dhan.co/v2/ip/modifyIP>
/// WARNING: 7-day cooldown after modification.
pub const DHAN_MODIFY_IP_PATH: &str = "/ip/modifyIP";

/// Path to get current static IP configuration (appended to rest_api_base_url).
/// Endpoint: GET <https://api.dhan.co/v2/ip/getIP>
pub const DHAN_GET_IP_PATH: &str = "/ip/getIP";

// ---------------------------------------------------------------------------
// Portfolio & Positions — REST API Endpoint Paths
// ---------------------------------------------------------------------------

/// Path for holdings list (appended to rest_api_base_url).
/// Endpoint: GET <https://api.dhan.co/v2/holdings>
pub const DHAN_HOLDINGS_PATH: &str = "/holdings";

/// Path for positions list (appended to rest_api_base_url).
/// Endpoint: GET <https://api.dhan.co/v2/positions>
pub const DHAN_POSITIONS_PATH: &str = "/positions";

/// Path for position conversion (appended to rest_api_base_url).
/// Endpoint: POST <https://api.dhan.co/v2/positions/convert>
pub const DHAN_POSITIONS_CONVERT_PATH: &str = "/positions/convert";

// ---------------------------------------------------------------------------
// Funds & Margin — REST API Endpoint Paths
// ---------------------------------------------------------------------------

/// Path for single-instrument margin calculator (appended to rest_api_base_url).
/// Endpoint: POST <https://api.dhan.co/v2/margincalculator>
pub const DHAN_MARGIN_CALCULATOR_PATH: &str = "/margincalculator";

/// Path for multi-instrument margin calculator (appended to rest_api_base_url).
/// Endpoint: POST <https://api.dhan.co/v2/margincalculator/multi>
pub const DHAN_MARGIN_CALCULATOR_MULTI_PATH: &str = "/margincalculator/multi";

/// Path for fund limit query (appended to rest_api_base_url).
/// Endpoint: GET <https://api.dhan.co/v2/fundlimit>
pub const DHAN_FUND_LIMIT_PATH: &str = "/fundlimit";

/// Path for kill switch activate/deactivate (appended to rest_api_base_url).
/// Endpoint: POST <https://api.dhan.co/v2/killswitch?killSwitchStatus=ACTIVATE|DEACTIVATE>
pub const DHAN_KILL_SWITCH_PATH: &str = "/killswitch";

/// Path for P&L-based exit configuration (appended to rest_api_base_url).
/// Endpoint: POST/DELETE/GET <https://api.dhan.co/v2/pnlExit>
pub const DHAN_PNL_EXIT_PATH: &str = "/pnlExit";

// ---------------------------------------------------------------------------
// Full Market Depth — WebSocket Base URLs
// ---------------------------------------------------------------------------

/// 20-level depth WebSocket base URL.
/// Full URL: `wss://depth-api-feed.dhan.co/twentydepth?token=TOKEN&clientId=CLIENT_ID&authType=2`
pub const DHAN_TWENTY_DEPTH_WS_BASE_URL: &str = "wss://depth-api-feed.dhan.co/twentydepth"; // APPROVED: infrastructure constant

/// 200-level depth WebSocket base URL.
/// Full URL: `wss://full-depth-api.dhan.co/?token=TOKEN&clientId=CLIENT_ID&authType=2`
///
/// NOTE: On 2026-04-23 Parthiban verified with Dhan's official Python SDK
/// `dhanhq==2.2.0rc1` that the **root path `/`** (not `/twohundreddepth`) is
/// the working URL for our account at SecurityId 72271 at depth 200. The SDK
/// streamed 30+ minutes on root path, while our Rust client at
/// `/twohundreddepth` kept getting `Protocol(ResetWithoutClosingHandshake)`
/// for 2+ weeks. This reverses the advice in Dhan ticket #5519522 which
/// had told us to use `/twohundreddepth`. If this regresses, re-open that
/// ticket and cite the 2026-04-23 Python SDK verification in the reply.
/// Dhan also confirmed: use a Security ID close to current market price (ATM).
pub const DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL: &str = "wss://full-depth-api.dhan.co"; // APPROVED: infrastructure constant — Python SDK verified root path 2026-04-23

// ---------------------------------------------------------------------------
// Historical Data — Candle Fetch Constants
// ---------------------------------------------------------------------------

/// 1-minute candle interval identifier for Dhan intraday API.
pub const DHAN_CANDLE_INTERVAL_1MIN: &str = "1";

/// 5-minute candle interval identifier for Dhan intraday API.
pub const DHAN_CANDLE_INTERVAL_5MIN: &str = "5";

/// 15-minute candle interval identifier for Dhan intraday API.
pub const DHAN_CANDLE_INTERVAL_15MIN: &str = "15";

/// 25-minute candle interval identifier for Dhan intraday API.
/// Per docs/dhan-ref/05-historical-data.md: valid intervals are "1", "5", "15", "25", "60".
pub const DHAN_CANDLE_INTERVAL_25MIN: &str = "25";

/// 60-minute (1-hour) candle interval identifier for Dhan intraday API.
pub const DHAN_CANDLE_INTERVAL_60MIN: &str = "60";

/// Maximum days of intraday data per API request (Dhan limit: 90 days).
pub const DHAN_INTRADAY_MAX_DAYS_PER_REQUEST: u32 = 90;

/// NSE continuous trading session start time as HH:MM:SS in IST.
///
/// Used for Dhan historical API `fromDate` and cross-verification baseline.
/// Dhan's historical intraday API returns candles starting from 09:15 (continuous
/// trading). Pre-market data (09:00–09:14) is only available via live WebSocket.
///
/// For live data collection start, see `data_collection_start` in config (09:00).
/// For tick/candle persistence window, see `TICK_PERSIST_START_SECS_OF_DAY_IST` (09:00).
pub const MARKET_OPEN_TIME_IST: &str = "09:15:00";

/// NSE F&O last 1-minute candle start time as HH:MM:SS in IST.
/// The last 1-minute candle runs from 15:29:00 to 15:29:59 IST.
pub const MARKET_LAST_CANDLE_START_IST: &str = "15:29:00";

/// Market close time for intraday API toDate parameter (exclusive upper bound).
/// Dhan intraday API uses datetime format: "YYYY-MM-DD 15:30:00" is exclusive,
/// meaning the last candle returned starts at 15:29.
pub const MARKET_CLOSE_TIME_IST_EXCLUSIVE: &str = "15:30:00";

// ---------------------------------------------------------------------------
// Spot 1m REST pipeline (operator grant 2026-07-12 — PR-2, the SPOT half)
// ---------------------------------------------------------------------------

/// The 4 IDX_I spot indices the per-minute REST pipeline fetches, as
/// `(security_id, symbol)` pairs: NIFTY=13, BANKNIFTY=25, SENSEX=51,
/// INDIA VIX=21. Segment is always `IDX_I`, instrument `INDEX`.
///
/// INDIA VIX joined on 2026-07-13 (operator scope addition 2026-07-13,
/// relayed via the coordinator session: INDIA VIX joins the spot 1m pull,
/// SPOT ONLY, no option chain) — the original 2026-07-12 grant covered the
/// 3 tradeable major indices. The chain leg deliberately keeps its own
/// 3-underlying [`CHAIN_1M_UNDERLYINGS`] subset (const-asserted below the
/// chain constants), so VIX can never leak into the option-chain pipeline.
///
/// HONESTY — whether Dhan `/v2/charts/intraday` serves INDIA VIX 1m
/// candles at all is a LIVE-PROBE UNKNOWN: per-SID independence of the
/// fire (each SID rides its own budgeted ladder in its own JoinSet task,
/// and the failure edge's "fully failed" = ZERO SIDs succeeded) means a
/// never-serving VIX can never delay or fail the other 3; the per-SID
/// persistent-empty detector ([`SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD`])
/// pages it loudly instead of letting it rot silently.
///
/// Rate budget with 4 SIDs: one fire = 4 concurrent initial requests (one
/// per index) — inside the Data-API 5/sec budget; re-polls are staggered
/// by the deterministic per-SID jitter (0/150/300/450 ms), so no ladder
/// instant ever exceeds the initial 4-wide burst.
pub const SPOT_1M_REST_INDICES: [(SecurityId, &str); 4] = [
    (13, "NIFTY"),
    (25, "BANKNIFTY"),
    (51, "SENSEX"),
    (INDIA_VIX_SECURITY_ID, "INDIA VIX"),
];

/// Consecutive counted not-served minutes for ONE SID before the ONE
/// edge-latched per-SID `SPOT1M-01 stage="sid_not_served"` page fires
/// (operator scope addition 2026-07-13 — the INDIA VIX live-probe
/// companion). A minute COUNTS toward a SID's streak only when that SID
/// failed/was empty while ≥1 OTHER SID succeeded in the SAME minute — a
/// global-outage minute (zero SIDs served) neither counts nor resets, so
/// this detector distinguishes vendor-not-serving-this-index from a
/// general outage (which the [`SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD`]
/// edge owns). Re-armed only by that SID's own recovery.
pub const SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD: u32 = 10;

/// Post-minute-close fire delay (ms): the fetcher wakes ~300 ms after each
/// minute boundary so Dhan has a beat to seal the just-closed candle before
/// the first poll. The docs do NOT document just-closed-minute availability
/// latency — the bounded re-poll ladder below plus the
/// `tv_spot1m_close_to_data_ms` histogram are the honest live probe.
pub const SPOT_1M_REST_FIRE_DELAY_MS: u64 = 300;

/// Bounded in-minute re-poll ladder: offsets (ms) FROM THE FIRST ATTEMPT at
/// which the fetch is re-polled when the target minute's candle is not yet
/// in the response (or the attempt errored). After the last offset the
/// minute is counted failed/empty — never an unbounded in-minute retry.
/// Strictly increasing; worst case the last poll fires ~6.3 s after the
/// minute close (fire delay + final offset), well inside the next boundary.
pub const SPOT_1M_REST_RETRY_OFFSETS_MS: [u64; 4] = [700, 1_500, 3_000, 6_000];

/// Deterministic per-SID ladder jitter STEP (ms) — each spot SID shifts its
/// whole re-poll schedule by `slot × step` (slot = the SID's fixed position
/// in [`SPOT_1M_REST_INDICES`]), so the 4 concurrent ladders never re-poll
/// Dhan in lockstep (429-coordination follow-up 2026-07-13: the first live
/// session showed `/v2/charts/intraday` rate-limiting when consumers
/// align). Deterministic + pure — no randomness anywhere.
pub const SPOT_1M_REST_LADDER_JITTER_STEP_MS: u64 = 150;

/// Number of distinct jitter slots (== the pinned [`SPOT_1M_REST_INDICES`]
/// arity): worst-case jitter is `(slots - 1) × step` = 450 ms (4 slots
/// since the 2026-07-13 INDIA VIX scope addition).
pub const SPOT_1M_REST_LADDER_JITTER_SLOTS: u64 = 4;

/// Extra bounded backoff (ms) applied before the NEXT ladder attempt after
/// an HTTP 429 (DH-904 class) response — gives Dhan's rate-limit window a
/// beat instead of re-polling straight back into it (429-coordination
/// follow-up 2026-07-13). Applied at most once per remaining rung (≤ 4×);
/// the rung COUNT is unchanged (never an extra retry), and the worst-case
/// schedule still fits the hard per-SID budget (const-asserted below).
/// 429s stay counted via the existing `tv_spot1m_rate_limited_total`.
pub const SPOT_1M_REST_429_EXTRA_BACKOFF_MS: u64 = 2_000;

/// First per-minute fire boundary, IST seconds-of-day: 09:16:00 — the
/// close of the session's first (09:15) 1-minute candle.
pub const SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST: u32 = 9 * 3600 + 16 * 60;

/// Last per-minute fire boundary, IST seconds-of-day: 15:30:00 — the close
/// of the session's last (15:29) 1-minute candle. INCLUSIVE (the 15:30:00
/// boundary itself fires, targeting the 15:29 candle).
pub const SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST: u32 = 15 * 3600 + 30 * 60;

/// Consecutive fully-failed minutes (no SID succeeded) before the ONE
/// edge-triggered SPOT1M-01 escalation page fires. Re-armed only after a
/// successful minute (audit-findings Rule 4 — edge-triggered alerts only).
pub const SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD: u32 = 3;

/// A fire woken more than this many seconds past its minute boundary is
/// SKIPPED (suspend / clock-step defense — the rest_canary
/// `PROBE_STALE_GRACE_SECS` precedent scaled to the 60 s cadence).
pub const SPOT_1M_REST_FIRE_STALE_GRACE_SECS: u32 = 30;

/// Per-REQUEST HTTP timeout (secs) for a single intraday poll. Deliberately
/// SHORT (5 s, not the 15 s house Dhan-charts value): the fire budget is one
/// minute, and a black-holed peer must never let the ladder overrun it
/// (2026-07-12 hostile-review H2 — the 15 s value made the worst-case
/// ladder ~81 s).
pub const SPOT_1M_REST_REQUEST_TIMEOUT_SECS: u64 = 5;

/// HARD wall-clock budget (secs) for ONE index's whole in-minute ladder —
/// enforced with `tokio::time::timeout` around the ladder, so no
/// combination of stalls can push a fire past the next boundary. A budget
/// overrun counts as that SID's failure for the minute.
pub const SPOT_1M_REST_SID_BUDGET_SECS: u64 = 20;

/// Maximum accepted response body size (bytes) for one intraday poll —
/// one minute × one index is a few hundred bytes; even a grossly
/// over-delivering full-day columnar response is well under 2 MiB. Bodies
/// beyond the cap are rejected before buffering (the csv_downloader
/// `MAX_CSV_BODY_BYTES` §18 hardening pattern).
pub const SPOT_1M_REST_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

// Compile-time consistency: the fire window is anchored to the canonical
// session gate — first fire = market open + 60 s (the 09:15 candle closes
// at 09:16:00); last fire = the 15:30:00 close boundary itself.
const _: () = assert!(
    SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST as i64 * 1_000_000_000
        == MARKET_OPEN_IST_NANOS + 60 * 1_000_000_000,
    "SPOT_1M first fire must be market open + 60s (09:16:00 IST)"
);
const _: () = assert!(
    SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST as i64 * 1_000_000_000 == MARKET_CLOSE_IST_NANOS,
    "SPOT_1M last fire must be the 15:30:00 IST close boundary"
);
const _: () = assert!(
    SPOT_1M_REST_LADDER_JITTER_SLOTS == SPOT_1M_REST_INDICES.len() as u64,
    "SPOT_1M jitter slot count must equal the pinned index arity"
);
const _: () = assert!(
    SPOT_1M_REST_RETRY_OFFSETS_MS[0] < SPOT_1M_REST_RETRY_OFFSETS_MS[1]
        && SPOT_1M_REST_RETRY_OFFSETS_MS[1] < SPOT_1M_REST_RETRY_OFFSETS_MS[2]
        && SPOT_1M_REST_RETRY_OFFSETS_MS[2] < SPOT_1M_REST_RETRY_OFFSETS_MS[3],
    "SPOT_1M retry offsets must be strictly increasing"
);
// The REAL in-minute budget math (2026-07-12 hostile-review H2 fix — the
// earlier assert ignored per-request timeouts): the hard per-SID ladder
// budget, plus the post-boundary fire delay, must finish inside the minute;
// and the ladder's own schedule (last offset + one full request timeout)
// must fit inside that budget so the timeout only fires on genuine stalls.
const _: () = assert!(
    SPOT_1M_REST_FIRE_DELAY_MS + SPOT_1M_REST_SID_BUDGET_SECS * 1_000 < 60_000,
    "SPOT_1M per-SID ladder budget must finish inside the minute"
);
// 429-coordination follow-up (2026-07-13): the schedule bound now includes
// the worst-case deterministic jitter ((slots-1) × step = 450 ms at the
// 4-SID arity) AND a 429 extra backoff before EVERY remaining rung
// (4 × 2 s = 8 s) — the fully hostile schedule (6 s + 0.45 s + 8 s + one
// 5 s request timeout = 19.45 s) still fits the 20 s hard per-SID budget.
const _: () = assert!(
    SPOT_1M_REST_RETRY_OFFSETS_MS[3]
        + (SPOT_1M_REST_LADDER_JITTER_SLOTS - 1) * SPOT_1M_REST_LADDER_JITTER_STEP_MS
        + SPOT_1M_REST_RETRY_OFFSETS_MS.len() as u64 * SPOT_1M_REST_429_EXTRA_BACKOFF_MS
        + SPOT_1M_REST_REQUEST_TIMEOUT_SECS * 1_000
        < SPOT_1M_REST_SID_BUDGET_SECS * 1_000,
    "SPOT_1M ladder schedule (last offset + max jitter + max 429 backoffs + one request timeout) must fit the budget"
);

// ---------------------------------------------------------------------------
// Shared Dhan Data-API rate limiter + self-tuning (operator pacing directive
// 2026-07-14, relayed via the coordinator session: pace Dhan to 3 requests/
// sec — tunable DOWN to 2 — spread overflow into the next second(s), route
// the option-chain API through the SAME limiter, with incremental/
// decremental self-tuning: "if it accepts max 3 or 2, stick to that and
// split it up"). Consumed by `crates/app/src/dhan_data_api_limiter.rs`.
// Dhan-ONLY — the Groww legs have their own vendor budgets.
// ---------------------------------------------------------------------------

/// Hard FLOOR of the self-tuning ladder — the limiter never paces Dhan
/// Data-API traffic below 2 requests/sec (the operator's "3 or 2" bound).
pub const DHAN_DATA_API_RPS_FLOOR: u32 = 2;

/// Hard CEILING of the config-legal `[dhan_data_api] target_rps` range —
/// deliberately BELOW Dhan's published Data-API 5/sec budget so this
/// process can never claim the whole account budget for the per-minute
/// REST legs.
pub const DHAN_DATA_API_RPS_CEILING: u32 = 4;

/// Serde default for `[dhan_data_api] target_rps` — the operator-directed
/// 3 requests/sec pacing (2026-07-14).
pub const DHAN_DATA_API_DEFAULT_TARGET_RPS: u32 = 3;

/// Rolling window (minutes) over which observed HTTP-429s accumulate
/// toward a step-down decision.
pub const DHAN_DATA_API_TUNER_429_WINDOW_MINUTES: u64 = 2;

/// 429 count within the rolling window that trips ONE step-down to the
/// [`DHAN_DATA_API_RPS_FLOOR`] (edge-logged once; window cleared on the
/// transition so a single burst can never cascade).
pub const DHAN_DATA_API_TUNER_429_STEP_DOWN_THRESHOLD: u32 = 3;

/// Consecutive CLEAN minutes (zero 429s observed) at a reduced rate before
/// ONE step back UP one level toward the config target.
pub const DHAN_DATA_API_TUNER_CLEAN_MINUTES_FOR_STEP_UP: u32 = 10;

/// Adaptive-degrade threshold for the spot-1m ladder (2026-07-14 retry
/// shaping): after this many CONSECUTIVE no-data minutes (zero SIDs served
/// their own just-closed candle), the ladder drops to a single attempt per
/// minute (no re-polls) until ANY success re-arms the full ladder. The
/// 2026-07-14 live regime (0/980 served, ~244 wasted 429s from ladder
/// re-fires against all-empty responses) is the incident this bounds.
pub const SPOT_1M_REST_DEGRADE_AFTER_CONSECUTIVE_NO_DATA_MINUTES: u32 = 5;

// Compile-time consistency for the tuning ladder.
const _: () = assert!(
    DHAN_DATA_API_RPS_FLOOR >= 1
        && DHAN_DATA_API_RPS_FLOOR <= DHAN_DATA_API_DEFAULT_TARGET_RPS
        && DHAN_DATA_API_DEFAULT_TARGET_RPS <= DHAN_DATA_API_RPS_CEILING,
    "Dhan Data-API rps ladder must satisfy 1 <= floor <= default <= ceiling"
);
const _: () = assert!(
    DHAN_DATA_API_RPS_CEILING < 5,
    "Dhan Data-API ceiling must stay below the published 5/sec account budget"
);
const _: () = assert!(
    DHAN_DATA_API_TUNER_429_STEP_DOWN_THRESHOLD >= 1
        && DHAN_DATA_API_TUNER_429_WINDOW_MINUTES >= 1
        && DHAN_DATA_API_TUNER_CLEAN_MINUTES_FOR_STEP_UP >= 1,
    "Dhan Data-API tuner thresholds must be non-degenerate"
);
const _: () = assert!(
    SPOT_1M_REST_DEGRADE_AFTER_CONSECUTIVE_NO_DATA_MINUTES
        >= SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD,
    "adaptive degrade must not pre-empt the SPOT1M-01 escalation edge"
);

// ---------------------------------------------------------------------------
// Option-chain 1m REST pipeline (operator grant 2026-07-12 — PR-3, the
// OPTION-CHAIN half). The 3 underlyings are the [`CHAIN_1M_UNDERLYINGS`]
// subset below (NOT the full [`SPOT_1M_REST_INDICES`] set since the
// 2026-07-13 INDIA VIX spot-only scope addition) and the fire boundaries
// reuse SPOT_1M_REST_FIRST/LAST_FIRE_SECS_OF_DAY_IST — the chain leg is
// sequenced immediately AFTER the spot leg each minute.
// ---------------------------------------------------------------------------

/// The 3 underlyings of the per-minute option-chain leg: NIFTY=13,
/// BANKNIFTY=25, SENSEX=51 — the §8 grant's chain scope, UNCHANGED by the
/// 2026-07-13 INDIA VIX spot addition (operator scope addition 2026-07-13,
/// relayed via the coordinator session: INDIA VIX joins the spot 1m pull,
/// SPOT ONLY, no option chain). The const-asserts below pin this as the
/// VIX-free prefix of [`SPOT_1M_REST_INDICES`], so the spot set can never
/// silently widen the chain scope.
pub const CHAIN_1M_UNDERLYINGS: [(SecurityId, &str); 3] =
    [(13, "NIFTY"), (25, "BANKNIFTY"), (51, "SENSEX")];

// The chain scope is the strict SID prefix of the spot set…
const _: () = assert!(
    CHAIN_1M_UNDERLYINGS[0].0 == SPOT_1M_REST_INDICES[0].0
        && CHAIN_1M_UNDERLYINGS[1].0 == SPOT_1M_REST_INDICES[1].0
        && CHAIN_1M_UNDERLYINGS[2].0 == SPOT_1M_REST_INDICES[2].0,
    "CHAIN_1M_UNDERLYINGS must stay the SID prefix of SPOT_1M_REST_INDICES"
);
// …and INDIA VIX (spot-only, 2026-07-13) can never enter the chain scope.
const _: () = assert!(
    CHAIN_1M_UNDERLYINGS[0].0 != INDIA_VIX_SECURITY_ID
        && CHAIN_1M_UNDERLYINGS[1].0 != INDIA_VIX_SECURITY_ID
        && CHAIN_1M_UNDERLYINGS[2].0 != INDIA_VIX_SECURITY_ID,
    "INDIA VIX is SPOT-ONLY (2026-07-13 scope) — never an option-chain underlying"
);

/// Fallback post-boundary fire delay (ms) for the chain leg: the chain
/// task normally wakes when the SPOT leg signals its minute complete
/// (~0.3–1.5 s after the boundary); when the spot leg is disabled, dead,
/// or slow, this timer fires the chain anyway — sequencing is best-effort,
/// never a hard dependency ("never blocked forever if spot is dead").
pub const CHAIN_1M_FALLBACK_DELAY_MS: u64 = 2_500;

/// Defensive per-underlying minimum gap (secs) between two option-chain
/// requests for the SAME underlying — Dhan's documented limit is 1 unique
/// request per 3 seconds (option-chain.md rule 4; DISTINCT underlyings may
/// go concurrently). One request per underlying per minute leaves ~60 s
/// gaps, so this guard never engages in normal operation.
pub const CHAIN_1M_MIN_GAP_SECS: u64 = 3;

/// Per-REQUEST HTTP timeout (secs) for one option-chain / expirylist call.
/// Chains are BIG (hundreds of strikes × 2 legs) — 10 s, double the spot
/// leg's 5 s, still bounded well inside the minute by the budget below.
pub const CHAIN_1M_REQUEST_TIMEOUT_SECS: u64 = 10;

/// HARD wall-clock budget (secs) for ONE underlying's per-minute chain
/// fetch (`tokio::time::timeout` around the whole leg) — overruns can
/// never stack across boundaries; a budget trip is that underlying's
/// failure for the minute.
pub const CHAIN_1M_UNDERLYING_BUDGET_SECS: u64 = 20;

/// Maximum accepted response body size (bytes) for one option-chain call —
/// a full NIFTY chain (~150 strikes × 2 legs × ~17 fields) is ~200–400 KiB;
/// 8 MiB bounds a hostile/misbehaving server (csv_downloader §18 pattern,
/// streamed cap).
pub const CHAIN_1M_MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Consecutive fully-failed chain minutes (no underlying succeeded, or the
/// persist leg failed) before the ONE edge-triggered CHAIN-02 escalation
/// page fires — mirrors [`SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD`].
pub const CHAIN_1M_CONSECUTIVE_FAIL_PAGE_THRESHOLD: u32 = 3;

/// Bounded day-start expirylist retry backoffs (secs) BETWEEN attempts —
/// 3 attempts total (first try + these two backoffs). Each backoff is ≥
/// [`CHAIN_1M_MIN_GAP_SECS`]: a retry of the SAME unique request inside
/// Dhan's 1-unique-per-3s window would earn the very rate-limit reject it
/// retries (hostile-review L1). On final failure the chain pipeline
/// degrades to disabled-for-the-day (CHAIN-04; NEVER a guessed expiry —
/// option-chain.md rule 9).
pub const CHAIN_1M_EXPIRYLIST_RETRY_BACKOFF_SECS: [u64; 2] = [3, 6];

const _: () = assert!(
    CHAIN_1M_EXPIRYLIST_RETRY_BACKOFF_SECS[0] >= CHAIN_1M_MIN_GAP_SECS
        && CHAIN_1M_EXPIRYLIST_RETRY_BACKOFF_SECS[1] >= CHAIN_1M_MIN_GAP_SECS,
    "expirylist retries must not re-enter Dhan's 1-unique-per-3s window"
);

// Compile-time consistency: the chain leg's whole fire (fallback delay +
// per-underlying budget) must finish inside the minute, and the fallback
// delay must be LONGER than the spot leg's post-boundary fire delay (the
// chain is sequenced AFTER the spot fetch).
const _: () = assert!(
    CHAIN_1M_FALLBACK_DELAY_MS + CHAIN_1M_UNDERLYING_BUDGET_SECS * 1_000 < 60_000,
    "CHAIN_1M fire (fallback delay + underlying budget) must finish inside the minute"
);
const _: () = assert!(
    CHAIN_1M_FALLBACK_DELAY_MS > SPOT_1M_REST_FIRE_DELAY_MS,
    "CHAIN_1M fallback delay must trail the spot leg's fire delay (chain fires after spot)"
);
const _: () = assert!(
    CHAIN_1M_REQUEST_TIMEOUT_SECS < CHAIN_1M_UNDERLYING_BUDGET_SECS,
    "CHAIN_1M per-request timeout must fit inside the per-underlying budget"
);

// ---------------------------------------------------------------------------
// Groww spot 1m REST leg (operator grant 2026-07-13 — PR-2 of the Groww
// per-minute REST plan, `.claude/plans/active-plan-groww-rest-1m.md`;
// authorization recorded in `groww-second-feed-scope-2026-06-19.md` §38 +
// `no-rest-except-live-feed-2026-06-27.md` §9). Mirrors the Dhan spot leg
// (`SPOT_1M_REST_*` above) onto Groww's `GET /v1/historical/candles`
// endpoint. The fire boundaries, staleness grace and page threshold REUSE
// the SPOT_1M_REST_* session constants (they are NSE-session facts, not
// Dhan facts).
// ---------------------------------------------------------------------------

/// Groww historical/intraday candles endpoint (V2 — ground truth
/// `docs/groww-ref/11-historical-candles.md`; SDK-verified `client.py:903`
/// in the official `growwapi` 1.5.0 wheel). GET with query params
/// `exchange` / `segment` / `groww_symbol` / `start_time` / `end_time` /
/// `candle_interval`. Serves same-day 1-minute candles (the docs pose no
/// previous-day-only restriction; just-closed-minute freshness is
/// UNDOCUMENTED/UNVERIFIED-LIVE per `docs/groww-ref/99-UNKNOWNS.md` — the
/// ladder + histogram are the probe).
// APPROVED: constants.rs is the single static-URL source (same as GROWW_INSTRUMENT_CSV_URL)
pub const GROWW_HISTORICAL_CANDLES_URL: &str = "https://api.groww.in/v1/historical/candles";

/// Groww `candle_interval` literal for 1-minute candles (`"1minute"`, NOT
/// the Dhan-style `"1"` — interval literals + the 30-days-per-request 1m
/// range cap per `docs/groww-ref/11-historical-candles.md`; our
/// day-granular windows are far inside the cap).
pub const GROWW_CANDLE_INTERVAL_1MIN: &str = "1minute";

/// Groww API version header name + value — sent on every trade-API call by
/// the official SDK (`_build_headers`, `client.py:1362-1378`; header set
/// reconciled in `docs/groww-ref/README.md`) alongside
/// `Authorization: Bearer <token>`.
pub const GROWW_API_VERSION_HEADER: &str = "x-api-version";
/// Header value companion to [`GROWW_API_VERSION_HEADER`].
pub const GROWW_API_VERSION_VALUE: &str = "1.0";

/// The 3 CORE spot indices the Groww per-minute REST leg fetches, as
/// `(groww_symbol, human symbol, exchange, segment)` — the SAME logical
/// indices as [`SPOT_1M_REST_INDICES`] in Groww's identity space: the V2
/// candles endpoint takes the `groww_symbol` (`NSE-NIFTY` — NOT the
/// exchange token, NOT the bare trading symbol), with `segment=CASH` for
/// indices (`docs/groww-ref/09-prompt-nse-indices-data.md` +
/// `docs/groww-ref/11-historical-candles.md`). Persisted rows join the
/// live lane via `stable_index_security_id(groww_symbol)` + `feed='groww'`.
///
/// 2026-07-13 (operator scope addition, relayed — the §38.6 grant in
/// `groww-second-feed-scope-2026-06-19.md`): the Groww spot leg tracks a
/// 4th index — [`GROWW_SPOT_1M_VIX_SYMBOL`] (INDIA VIX) — which is
/// deliberately NOT in this const: its `groww_symbol` is RUNTIME-resolved
/// from the day's ingested Groww master (the watch file), never guessed.
/// This const stays the 3-CORE set; the escalation edge keys on it.
pub const GROWW_SPOT_1M_SYMBOLS: [(&str, &str, &str, &str); 3] = [
    ("NSE-NIFTY", "NIFTY", "NSE", "CASH"),
    ("NSE-BANKNIFTY", "BANKNIFTY", "NSE", "CASH"),
    ("BSE-SENSEX", "SENSEX", "BSE", "CASH"),
];

/// The 4th Groww spot-leg index — INDIA VIX (operator scope 2026-07-13,
/// relayed via the coordinator session: "add India VIX to the per-minute
/// spot pull on the Groww leg; resolve the correct Groww
/// exchange/segment/groww_symbol for the VIX index from the Groww master —
/// do NOT guess the literal"). This is the CANONICAL human symbol (the
/// `NSE_INDEX_ALLOWLIST` member, same literal as `PHASE_0_IDX_I_SYMBOLS`);
/// the Groww-side identity (`groww_symbol`/exchange/segment) is
/// RUNTIME-resolved by matching the day's Groww master index rows through
/// `canonicalize_index_symbol` (the live 2026-06-28 master lists it under
/// NSE as token `INDIAVIX`, display name "India Vix" — see the
/// `REAL_GROWW_NSE_INDICES` fixture in
/// `crates/core/src/feed/groww/instruments.rs`). SPOT ONLY — no chain, no
/// contracts; whether Groww's historical-candles endpoint SERVES India VIX
/// is a live-probe UNKNOWN (persistent empty = named forensics rows + one
/// daily coded warn, never silent).
pub const GROWW_SPOT_1M_VIX_SYMBOL: &str = "INDIA VIX";

// Arity guard: this CONST tracks the SAME 3 CORE logical indices as the
// chain-scope subset [`CHAIN_1M_UNDERLYINGS`]. The 2026-07-13 INDIA VIX
// scope addition put VIX on BOTH spot legs — the Dhan leg as the 4th
// `SPOT_1M_REST_INDICES` entry (fixed IDX_I SID 21) and the Groww leg as
// the runtime-resolved `GROWW_SPOT_1M_VIX_SYMBOL` target (deliberately NOT
// in this const set — its Groww identity comes from the day's master).
// VIX is SPOT ONLY on both legs; any FURTHER index needs a fresh dated
// operator quote.
const _: () = assert!(
    GROWW_SPOT_1M_SYMBOLS.len() == 3 && GROWW_SPOT_1M_SYMBOLS.len() == CHAIN_1M_UNDERLYINGS.len(),
    "GROWW_SPOT_1M_SYMBOLS must stay the 3 core indices (Groww VIX is runtime-resolved, not const)"
);

/// Post-minute-close fire delay (ms) for the Groww leg — mirrors
/// [`SPOT_1M_REST_FIRE_DELAY_MS`]. Groww's just-closed-minute availability
/// latency is undocumented; the ladder + histogram measure it.
pub const GROWW_SPOT_1M_FIRE_DELAY_MS: u64 = 300;

/// Bounded in-minute re-poll ladder for the Groww leg: offsets (ms) FROM
/// the first attempt — mirrors [`SPOT_1M_REST_RETRY_OFFSETS_MS`].
pub const GROWW_SPOT_1M_RETRY_OFFSETS_MS: [u64; 4] = [700, 1_500, 3_000, 6_000];

/// Per-REQUEST HTTP timeout (secs) for one Groww candles poll — mirrors
/// the Dhan leg's 5 s (bounded inside the minute; the SDK ships INFINITE
/// default timeouts, so this bound is entirely ours).
pub const GROWW_SPOT_1M_REQUEST_TIMEOUT_SECS: u64 = 5;

/// HARD wall-clock budget (secs) for ONE symbol's whole in-minute ladder.
/// 14 s (not the Dhan leg's 20 s — a DELIBERATE tightening; was 18 s at
/// 3 targets, re-derived 2026-07-13 when INDIA VIX made it 4 targets): the
/// Groww leg fetches its targets SEQUENTIALLY (the minute-boundary pacing
/// rule of `docs/groww-ref/15-rate-limits-and-capacity.md` — the
/// shared-token 10/s Live-Data bucket is TYPE-pooled and co-tenanted with
/// bruteX, so each boundary burst is spread to at most ONE in-flight
/// request at a time, far inside the ≤6 req/s ceiling), so the WHOLE fire
/// is bounded by 4 × budget (3 core + the runtime-resolved INDIA VIX);
/// 4 × 14 s + the fire delay still finishes inside the minute, and the
/// ladder schedule (last 6 s offset + one 5 s request timeout = 11 s)
/// still fits one budget — both const-asserted below.
pub const GROWW_SPOT_1M_SYMBOL_BUDGET_SECS: u64 = 14;

/// Maximum accepted response body size (bytes) for one Groww candles poll
/// — a full-day 375-row tuple response is ~20 KB; 2 MiB bounds a
/// hostile/misbehaving server (csv_downloader §18 streamed-cap pattern).
pub const GROWW_SPOT_1M_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Minimum spacing (secs) between SSM re-reads of the shared Groww access
/// token after an auth-class reject — the token-minter lock's ≥60 s pacing
/// (`groww-shared-token-minter-2026-07-02.md`): re-READ, never mint. The
/// daily ~06:00 IST token expiry is OFFICIALLY documented
/// (`docs/groww-ref/17-token-lifecycle.md`) — an in-session 401 means the
/// minter re-minted or the token was invalidated; the fresh value arrives
/// via the same SSM read.
pub const GROWW_SPOT_1M_TOKEN_REREAD_FLOOR_SECS: u64 = 60;

const _: () = assert!(
    GROWW_SPOT_1M_RETRY_OFFSETS_MS[0] < GROWW_SPOT_1M_RETRY_OFFSETS_MS[1]
        && GROWW_SPOT_1M_RETRY_OFFSETS_MS[1] < GROWW_SPOT_1M_RETRY_OFFSETS_MS[2]
        && GROWW_SPOT_1M_RETRY_OFFSETS_MS[2] < GROWW_SPOT_1M_RETRY_OFFSETS_MS[3],
    "GROWW_SPOT_1M retry offsets must be strictly increasing"
);
// SEQUENTIAL-fetch budget math: the whole fire (fire delay + 4 sequential
// per-target ladder budgets — the 3 core symbols + 1 for the
// runtime-resolved INDIA VIX target, 2026-07-13 operator scope) must
// finish inside the minute, and the ladder's own schedule (last offset +
// one full request timeout) must fit inside one symbol's budget so the
// timeout only fires on genuine stalls.
const _: () = assert!(
    GROWW_SPOT_1M_FIRE_DELAY_MS
        + (GROWW_SPOT_1M_SYMBOLS.len() as u64 + 1) * GROWW_SPOT_1M_SYMBOL_BUDGET_SECS * 1_000
        < 60_000,
    "GROWW_SPOT_1M sequential fire (delay + 4 x symbol budget incl. the runtime VIX target) must finish inside the minute"
);
const _: () = assert!(
    GROWW_SPOT_1M_RETRY_OFFSETS_MS[3] + GROWW_SPOT_1M_REQUEST_TIMEOUT_SECS * 1_000
        < GROWW_SPOT_1M_SYMBOL_BUDGET_SECS * 1_000,
    "GROWW_SPOT_1M ladder schedule (last offset + one request timeout) must fit the budget"
);

// ---------------------------------------------------------------------------
// Groww option-chain 1m REST leg (operator grant 2026-07-13 — PR-3 of the
// Groww per-minute REST plan, `.claude/plans/active-plan-groww-rest-1m.md`;
// authorization `groww-second-feed-scope-2026-06-19.md` §38 +
// `no-rest-except-live-feed-2026-06-27.md` §9). Mirrors the Dhan chain leg
// (`CHAIN_1M_*` above) onto Groww's option-chain endpoint. Session
// boundaries, staleness grace and the page threshold REUSE the
// SPOT_1M_REST_* session constants (NSE-session facts, not broker facts).
// ---------------------------------------------------------------------------

/// Groww option-chain REST endpoint prefix (documented + SDK-verified —
/// `docs/groww-ref/14-option-chain.md` §1; `client.py:490` in the official
/// `growwapi` 1.5.0 wheel). Full shape:
/// `GET {prefix}/exchange/{exchange}/underlying/{underlying}?expiry_date=YYYY-MM-DD`
/// — the `underlying` path param is the PLAIN symbol (`NIFTY`, NOT the
/// `groww_symbol`). Bearer + `x-api-version: 1.0` headers, same as the
/// candles endpoint. The response carries NO timestamp of any kind
/// (Verified-absence, §3) — the snapshot moment is stamped client-side.
// APPROVED: constants.rs is the single static-URL source (same as GROWW_INSTRUMENT_CSV_URL)
pub const GROWW_OPTION_CHAIN_URL_PREFIX: &str = "https://api.groww.in/v1/option-chain";

/// The 3 underlyings the Groww per-minute chain leg fetches, as
/// `(plain underlying, exchange, groww_symbol)` — the SAME logical indices
/// as [`GROWW_SPOT_1M_SYMBOLS`]: the chain endpoint takes the PLAIN symbol
/// + exchange (`docs/groww-ref/14-option-chain.md` §1); the `groww_symbol`
/// third element feeds `stable_index_security_id` so persisted rows carry
/// the SAME ids the Groww live lane uses.
pub const GROWW_CHAIN_1M_UNDERLYINGS: [(&str, &str, &str); 3] = [
    ("NIFTY", "NSE", "NSE-NIFTY"),
    ("BANKNIFTY", "NSE", "NSE-BANKNIFTY"),
    ("SENSEX", "BSE", "BSE-SENSEX"),
];

// Arity guard: the Groww chain leg tracks the SAME 3 logical indices as
// the CORE spot set — a 4th underlying needs a fresh dated operator
// quote. 2026-07-13: the spot leg's 4th index (INDIA VIX,
// GROWW_SPOT_1M_VIX_SYMBOL) is SPOT ONLY per the relayed operator scope
// ("no chain, no contracts") — the chain leg deliberately stays 3.
const _: () = assert!(
    GROWW_CHAIN_1M_UNDERLYINGS.len() == GROWW_SPOT_1M_SYMBOLS.len(),
    "GROWW_CHAIN_1M_UNDERLYINGS must mirror the 3-index CORE GROWW_SPOT_1M_SYMBOLS set"
);

/// Fallback post-boundary fire delay (ms) for the Groww chain leg —
/// mirrors [`CHAIN_1M_FALLBACK_DELAY_MS`]: the chain task normally wakes
/// when the GROWW spot leg signals its minute complete; when the spot leg
/// is disabled, dead, or slow, this timer fires the chain anyway
/// (sequencing is best-effort, never a hard dependency).
pub const GROWW_CHAIN_1M_FALLBACK_DELAY_MS: u64 = 2_500;

/// MECHANICAL cross-request minimum gap (ms) between ANY two consecutive
/// Groww chain requests — ONE scalar stamp spans underlyings, so a fire's
/// 3 requests spread over ≥ 2 s (chain leg ≤ 1 req/s sustained) instead
/// of bursting back-to-back at the minute boundary (hostile-round-1
/// MEDIUM-1). Groww documents NO chain-specific rate rule
/// (`docs/groww-ref/14-option-chain.md` §4 — the family is UNDOCUMENTED,
/// Unknown ≠ unlimited), so the VALUE is not doc-mandated (the Dhan
/// 1-per-3s contrast): 1 s keeps the combined spot+chain boundary burst
/// inside the ≤6 req/s pacing ceiling of
/// `docs/groww-ref/15-rate-limits-and-capacity.md` with bruteX co-tenancy
/// headroom. The wait runs INSIDE the per-underlying budget (the
/// min-gap + timeout < budget const-assert below stays coherent).
pub const GROWW_CHAIN_1M_MIN_GAP_MS: u64 = 1_000;

/// Per-REQUEST HTTP timeout (secs) for one Groww chain call — chains are
/// BIG (~100–300 KB per `docs/groww-ref/14-option-chain.md` §5, Assumed);
/// mirrors the Dhan chain leg's 10 s.
pub const GROWW_CHAIN_1M_REQUEST_TIMEOUT_SECS: u64 = 10;

/// HARD wall-clock budget (secs) for ONE underlying's per-minute chain
/// fetch (min-gap wait + one request). 15 s — DELIBERATELY tighter than
/// the Dhan chain's 20 s: the Groww leg fetches its 3 underlyings
/// SEQUENTIALLY (the ≤6 req/s shared-bucket pacing rule, the spot-leg
/// precedent), so the whole fire is bounded by 3 × budget and must still
/// finish inside the minute (const-asserted below).
pub const GROWW_CHAIN_1M_UNDERLYING_BUDGET_SECS: u64 = 15;

/// Maximum accepted response body size (bytes) for one Groww chain call —
/// ~100–300 KB expected; 8 MiB bounds a hostile/misbehaving server
/// (csv_downloader §18 streamed-cap pattern; mirrors the Dhan chain cap).
pub const GROWW_CHAIN_1M_MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Consecutive fully-failed Groww chain minutes before the ONE
/// edge-triggered escalation page fires — the chain leg REUSES the spot
/// `FailureEdge`, so this MUST equal the spot threshold (const-asserted).
pub const GROWW_CHAIN_1M_CONSECUTIVE_FAIL_PAGE_THRESHOLD: u32 = 3;

/// Bounded warmup retry backoffs (secs) BETWEEN Groww instruments-master
/// download attempts — 3 attempts total (first try + these two). The
/// master is a public static CSV (`GROWW_INSTRUMENT_CSV_URL` — the §3/§9
/// KEEP class, zero rate budget); on final failure the chain leg degrades
/// to disabled-for-the-day (NEVER a guessed expiry).
pub const GROWW_CHAIN_1M_MASTER_RETRY_BACKOFF_SECS: [u64; 2] = [3, 6];

const _: () = assert!(
    GROWW_CHAIN_1M_CONSECUTIVE_FAIL_PAGE_THRESHOLD == SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD,
    "the Groww chain leg reuses the spot FailureEdge — thresholds must agree"
);
// SEQUENTIAL-fetch budget math: the whole chain fire (fallback delay + 3
// sequential per-underlying budgets) must finish inside the minute; the
// per-request timeout + min-gap must fit one underlying's budget; and the
// fallback delay must TRAIL the Groww spot leg's post-boundary fire delay
// (the chain is sequenced AFTER the spot fetch).
const _: () = assert!(
    GROWW_CHAIN_1M_FALLBACK_DELAY_MS
        + (GROWW_CHAIN_1M_UNDERLYINGS.len() as u64) * GROWW_CHAIN_1M_UNDERLYING_BUDGET_SECS * 1_000
        < 60_000,
    "GROWW_CHAIN_1M sequential fire (fallback + 3 x underlying budget) must finish inside the minute"
);
const _: () = assert!(
    GROWW_CHAIN_1M_MIN_GAP_MS + GROWW_CHAIN_1M_REQUEST_TIMEOUT_SECS * 1_000
        < GROWW_CHAIN_1M_UNDERLYING_BUDGET_SECS * 1_000,
    "GROWW_CHAIN_1M min-gap + one request timeout must fit the per-underlying budget"
);
const _: () = assert!(
    GROWW_CHAIN_1M_FALLBACK_DELAY_MS > GROWW_SPOT_1M_FIRE_DELAY_MS,
    "GROWW_CHAIN_1M fallback delay must trail the Groww spot leg's fire delay"
);

// ---------------------------------------------------------------------------
// Groww per-contract 1m candle REST leg (operator grant 2026-07-13 — PR-4
// of the Groww per-minute REST plan, the FILL-MODEL leg;
// `.claude/plans/active-plan-groww-rest-1m.md`; authorization
// `groww-second-feed-scope-2026-06-19.md` §38 +
// `no-rest-except-live-feed-2026-06-27.md` §9). Same endpoint + headers +
// interval literal + body cap as the Groww spot leg (`GROWW_HISTORICAL_
// CANDLES_URL` / `GROWW_API_VERSION_*` / `GROWW_CANDLE_INTERVAL_1MIN` /
// `GROWW_SPOT_1M_MAX_BODY_BYTES` — one request, one day-window body shape),
// with `segment=FNO` and a per-contract `groww_symbol` identity like
// `NSE-NIFTY-04Jan24-19200-CE`. Sequenced AFTER the Groww chain leg (its
// per-minute `underlying_ltp` is the ATM anchor). Session boundaries,
// staleness grace and the page threshold REUSE the SPOT_1M_REST_* session
// constants (NSE-session facts, not broker facts).
// ---------------------------------------------------------------------------

/// Fallback post-boundary fire delay (ms) for the contract leg — the
/// contract task normally wakes when the GROWW CHAIN leg signals its
/// minute complete (spot → chain → contract sequencing); when the chain
/// leg is dead or slow, this timer fires the contract leg anyway
/// (sequencing is best-effort, never a hard dependency). 8 s trails the
/// chain's NORMAL completion window (chain fallback 2.5 s + 3 sequential
/// underlyings at the 1 s min-gap ≈ 5-7 s) so the fresh anchor usually
/// exists by the time selection runs.
pub const GROWW_CONTRACT_1M_FALLBACK_DELAY_MS: u64 = 8_000;

/// MECHANICAL cross-request minimum gap (ms) between ANY two consecutive
/// contract candle requests — the chain leg's min-gap PATTERN at a
/// contract-sized value: 500 ms → the contract leg contributes at most
/// 2.0 req/s. RE-DERIVED 2026-07-13 (was 300 ms) when INDIA VIX joined
/// the spot leg (3 → 4 sequential targets, §38.7 of the groww-scope
/// lock): the honest spot worst-SECOND is 3 requests (a fast-failing
/// target's last ladder rung in the same second as the NEXT target's
/// rung-0 + rung-1 at +0.7 s — the 4th target makes those transitions
/// MORE FREQUENT, never faster), so at 300 ms the worst overlap was
/// 3 + 1 + 3.33 = 7.33 req/s > the ≤6 req/s pacing ceiling of
/// `docs/groww-ref/15-rate-limits-and-capacity.md`. At 500 ms:
/// 3 + 1 + 2 = 6.00 req/s — exactly AT the self-imposed ceiling
/// (const-asserted below with exact rational math), with the broker's
/// documented 10/s hard ceiling far above. Pure pacing cost: 29 gaps ×
/// 500 ms = 14.5 s of the 45 s fire budget — still bounded.
pub const GROWW_CONTRACT_1M_MIN_GAP_MS: u64 = 500;

/// Per-REQUEST HTTP timeout (secs) for one contract candles call — the
/// spot leg's 5 s (same endpoint, same ~20 KB day-window body).
pub const GROWW_CONTRACT_1M_REQUEST_TIMEOUT_SECS: u64 = 5;

/// HARD envelope cap on contracts fetched per minute (the §38 "envelope
/// cap on contracts per minute"). The default ATM window fills it exactly:
/// (2 × 2 + 1) strikes × 2 legs × 3 underlyings = 30. A selection larger
/// than the cap (config-widened window) is truncated DETERMINISTICALLY
/// nearest-ATM-first — counted + one coded warn, never fetched past the
/// cap, never silent.
pub const GROWW_CONTRACT_1M_MAX_PER_MINUTE: usize = 30;

/// HARD wall-clock deadline (secs) for ONE minute's whole contract fire:
/// contracts not reached before the deadline are SKIPPED loudly (counted +
/// forensics rows), never fetched into the next minute. Normal fires
/// finish in ~10-20 s (cap × (min-gap + one round-trip)); the deadline
/// only engages on a stalling peer.
pub const GROWW_CONTRACT_1M_FIRE_BUDGET_SECS: u64 = 45;

/// Default ATM window half-width (strikes each side of ATM) for the
/// contract selection — the `[groww_contract_1m] strikes_each_side`
/// config default. 2 each side → 5 strikes × CE+PE × 3 underlyings = 30
/// contracts/minute = exactly the envelope cap (const-asserted below).
pub const GROWW_CONTRACT_1M_DEFAULT_STRIKES_EACH_SIDE: u32 = 2;

/// Maximum ATM-anchor age (minutes) the contract leg will still trust. A
/// chain-leg anchor OLDER than this (the chain leg died or is silently
/// failing while its last anchor sits frozen) makes the underlying
/// UNRESOLVED for the minute — a NAMED skip (coded warn `anchor_stale` +
/// counter + `rest_fetch_audit` rows), never a silently-frozen off-ATM
/// fetch window (round-1 review M3; the §38.8 decision-freshness
/// principle applied to the selection input). 5 minutes ≈ the 3-minute
/// escalation edge + margin: the chain leg's own paging fires first.
pub const GROWW_CONTRACT_1M_ANCHOR_MAX_AGE_MINUTES: u32 = 5;

const _: () = assert!(
    GROWW_CONTRACT_1M_FALLBACK_DELAY_MS > GROWW_CHAIN_1M_FALLBACK_DELAY_MS,
    "the contract leg is sequenced AFTER the chain leg — its fallback must trail the chain's"
);
const _: () = assert!(
    GROWW_CONTRACT_1M_FALLBACK_DELAY_MS + GROWW_CONTRACT_1M_FIRE_BUDGET_SECS * 1_000 < 60_000,
    "GROWW_CONTRACT_1M fire (fallback + fire budget) must finish inside the minute"
);
// Boundary-burst pacing ceiling (§38.3, RE-DERIVED 2026-07-13 for the
// VIX 4th spot target): the honest spot worst-SECOND is 3 requests
// (sequential targets, ≤1 in-flight, but a fast-failing target's last
// ladder rung can share one second with the next target's rung-0 +
// rung-1 at +0.7 s; the 4th target makes such transitions more
// frequent, never faster) + chain ≤ 1000/chain-gap req/s + contract ≤
// 1000/contract-gap req/s, all inside the ≤6 req/s shared-bucket
// ceiling even in the worst overlap. EXACT rational math via
// cross-multiplication (round-1 review LOW: integer division rounds
// 1000/gap DOWN — optimistic):
//   3 + 1000/kg + 1000/cg ≤ 6  ⇔  1000·kg + 1000·cg ≤ 3·cg·kg
// (all terms positive). Today: 1000·500 + 1000·1000 = 1.5M ≤
// 3·1000·500 = 1.5M — the worst-overlap burst is 3 + 1 + 2 = 6.00
// req/s, exactly AT the self-imposed ceiling (broker hard ceiling 10/s).
const _: () = assert!(
    1_000 * GROWW_CONTRACT_1M_MIN_GAP_MS + 1_000 * GROWW_CHAIN_1M_MIN_GAP_MS
        <= 3 * GROWW_CHAIN_1M_MIN_GAP_MS * GROWW_CONTRACT_1M_MIN_GAP_MS,
    "spot(3) + chain + contract worst-overlap boundary burst must stay inside the 6 req/s ceiling (exact rational math)"
);
// Per-minute request math (the §38.3 capacity envelope, re-derived
// 2026-07-13 for the VIX 4th spot target): worst case 30 contracts +
// the spot leg's 20 (3 core symbols + the runtime VIX target = 4 × 5
// ladder rungs) + 3 chain = 53 requests/min ≈ 17.7% of the 300/min
// shared budget (typical ≈ 37/min: 30 + 4 + 3). Const-asserted at ≤ 60
// (20% of the budget) so a future cap raise cannot silently eat the
// bruteX co-tenancy headroom.
const _: () = assert!(
    GROWW_CONTRACT_1M_MAX_PER_MINUTE
        + (GROWW_SPOT_1M_SYMBOLS.len() + 1) * (GROWW_SPOT_1M_RETRY_OFFSETS_MS.len() + 1)
        + GROWW_CHAIN_1M_UNDERLYINGS.len()
        <= 60,
    "the three Groww REST legs' worst-case per-minute request total must stay <= 20% of the 300/min budget"
);
// The DEFAULT ATM window fills the cap exactly — a wider default needs a
// fresh dated operator quote AND a cap re-derivation.
const _: () = assert!(
    ((2 * GROWW_CONTRACT_1M_DEFAULT_STRIKES_EACH_SIDE as usize + 1) * 2)
        * GROWW_CHAIN_1M_UNDERLYINGS.len()
        <= GROWW_CONTRACT_1M_MAX_PER_MINUTE,
    "the default ATM window (strikes x CE+PE x underlyings) must fit the per-minute contract cap"
);

/// Daily reset signal time (IST). After market close at 15:30,
/// historical re-fetch + cross-verification runs. At 16:00 IST the daily
/// reset signal fires (candle aggregator reset, indicator reset, etc.).
/// NOTE: Auto-shutdown is DISABLED. App runs 24/7 until manual Ctrl+C.
/// For AWS instance lifecycle, use systemd timer or cron for restart.
pub const APP_DAILY_RESET_TIME_IST: &str = "16:00:00";

/// Wave-2-D (G19) — daily reset time for the `TickGapDetector`. Fires
/// 5 minutes after market close so it cannot race the 15:30 close
/// signal. The papaya `last_seen` map is cleared at this point so
/// overnight silence does NOT register as a tick gap on next day's
/// market open. Pinned constant; see
/// `.claude/rules/project/disaster-recovery.md` Scenario 14
/// (Overnight wake) for the operator-visible flow.
pub const TICK_GAP_RESET_TIME_IST: &str = "15:35:00";

/// Wave-2-D — short post-fire settle window for the daily tick-gap
/// reset task. After firing `reset_daily()` at 15:35 IST, sleep this
/// long before recomputing the next-fire delay so we cannot race the
/// same boundary back into a near-zero sleep.
pub const TICK_GAP_RESET_SETTLE_SECS: u64 = 60;

/// Wave-2-D — bounded busy-loop avoidance for the daily tick-gap
/// reset task. If the post-fire recomputed delay is still zero (e.g.
/// the host clock is stuck), sleep this long before retrying so we
/// don't burn CPU. One hour is short enough that the task self-heals
/// within a single trading session if the clock recovers, and long
/// enough that we don't spin on a stuck system.
pub const TICK_GAP_RESET_BUSYLOOP_GUARD_SECS: u64 = 3600;

/// Number of 1-minute candles in the cross-verification window (09:15 to 15:29 = 375).
///
/// Historical candle coverage check (cross_verify module retired PR-C 2026-05-26).
/// This covers only the continuous trading session (09:15–15:29) because Dhan's
/// historical intraday API returns data starting from 09:15.
///
/// Live WebSocket data is collected and stored from 09:00 (pre-market), but
/// pre-market candles (09:00–09:14) have no historical counterpart for comparison.
/// Each candle covers [HH:MM:00, HH:MM:59]. Last candle at 15:29.
pub const CANDLES_PER_TRADING_DAY: usize = 375;

/// Batch size for QuestDB ILP flushes during historical candle ingestion.
pub const CANDLE_FLUSH_BATCH_SIZE: usize = 500;

/// Timeframe identifier for 1-minute candles in `historical_candles` table.
pub const TIMEFRAME_1M: &str = "1m";

/// Timeframe identifier for 5-minute candles in `historical_candles` table.
pub const TIMEFRAME_5M: &str = "5m";

/// Timeframe identifier for 15-minute candles in `historical_candles` table.
pub const TIMEFRAME_15M: &str = "15m";

/// Timeframe identifier for 60-minute candles in `historical_candles` table.
pub const TIMEFRAME_60M: &str = "60m";

/// Timeframe identifier for daily candles in `historical_candles` table.
pub const TIMEFRAME_1D: &str = "1d";

/// All intraday timeframes to fetch from Dhan API.
/// Each tuple: (interval for API request, timeframe label for storage).
pub const INTRADAY_TIMEFRAMES: &[(&str, &str)] = &[
    ("1", "1m"),
    ("5", "5m"),
    ("15", "15m"),
    ("25", "25m"),
    ("60", "60m"),
];

// ---------------------------------------------------------------------------
// Authentication — SSM Path Construction
// ---------------------------------------------------------------------------

/// Default environment name for SSM path construction.
/// Overridden by `TV_ENVIRONMENT` / `ENVIRONMENT` env var if present.
/// Single real env (operator 2026-06-30): dev/staging were collapsed into
/// `prod`. NO real orders are placed — `production.toml` locks `dry_run=true`.
pub const DEFAULT_SSM_ENVIRONMENT: &str = "prod";

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

// PR-E (2026-05-26): QUESTDB_TABLE_HISTORICAL_CANDLES retired alongside
// the deleted Dhan historical fetch chain. The table no longer exists.

/// QuestDB table: 1-second OHLCV candles aggregated from live ticks.
pub const QUESTDB_TABLE_CANDLES_1S: &str = "candles_1s";

/// Calendar alignment offset for QuestDB materialized views.
///
/// All timestamps are stored as IST epoch values. Live WebSocket data arrives
/// as IST epoch seconds (stored directly). Historical REST data has +19800s
/// applied. No offset needed — midnight "UTC" in our convention IS midnight IST.
pub const QUESTDB_IST_ALIGN_OFFSET: &str = "00:00";

// ---------------------------------------------------------------------------
// Pipeline — Tick Processing Constants
// ---------------------------------------------------------------------------

/// Default tick batch flush size for QuestDB ILP writes.
pub const TICK_FLUSH_BATCH_SIZE: usize = 1000;

/// Default tick flush interval in milliseconds.
pub const TICK_FLUSH_INTERVAL_MS: u64 = 1000;

/// Tick ring buffer capacity for QuestDB outage resilience.
/// Holds ticks in memory when QuestDB is down, drains on recovery.
///
/// **Rightsized 2026-05-20 for the 4-SID indices-only universe.**
/// The live universe is 4 IDX_I SIDs (NIFTY, BANKNIFTY, SENSEX,
/// INDIA VIX) in Quote mode — a tick rate of ~10-20/sec, not the
/// ~5,000/sec of the old 25K-instrument universe the 5M ring was
/// sized for.
///
/// - 100,000 ticks × ~200 bytes = ~20 MB RAM (pre-allocated at boot
///   via `VecDeque::with_capacity` in `tick_persistence.rs`).
/// - At a sustained ~20 ticks/sec = ~83 minutes before disk spill.
/// - At an extreme ~400 ticks/sec burst = ~4 minutes before spill.
/// - Both far exceed the ≤60-second QuestDB-outage SLA; the disk
///   spill + DLQ still backstop any overflow beyond that.
///
/// **Trim history:**
/// - 600K → 2M (PR #452, 2026-05-03): extreme-memory-pressure spec
///   for the old large universe.
/// - 2M → 5M (Wave 7-A4, 2026-05-11): RAM-first hardening, ~1.0 GB
///   pre-allocated, sized for the old ~5,000-tick/sec workload.
/// - 5M → 100K (2026-05-20): the universe narrowed to 4 IDX_I SIDs;
///   a 5M ring (~1 GB) buffered ~130 hours for a feed that needs
///   minutes. Trimmed to 100K — frees ~980 MB RAM on the 8 GB host.
///   Conservative resize: spill + DLQ retained. The deeper
///   simplification (10K + drop spill/DLQ) is tracked in
///   `docs/architecture/resilience-simplification-4-sids.md`.
pub const TICK_BUFFER_CAPACITY: usize = 100_000;

/// High watermark threshold for tick ring buffer (80% of capacity).
/// When buffer occupancy exceeds this, a WARN-level alert fires once
/// to signal imminent disk spill. Triggers Telegram via Loki ERROR rule.
/// 80% of 100,000 = 80,000 ticks.
pub const TICK_BUFFER_HIGH_WATERMARK: usize = TICK_BUFFER_CAPACITY * 4 / 5; // 80,000

/// Minimum free disk space (bytes) to log a warning before spill write.
/// 100 MB — below this, a WARN fires on each spill open to alert operator
/// that prolonged QuestDB outage may exhaust disk.
pub const TICK_SPILL_MIN_DISK_SPACE_BYTES: u64 = 100 * 1024 * 1024;

/// OMS HTTP client timeout (seconds). Applied at the client level as a default
/// for all Dhan order API calls. Individual endpoints may override with shorter
/// per-request timeouts. 5 seconds covers normal API latency (50-200ms) with
/// margin for Dhan-side processing spikes.
pub const OMS_HTTP_TIMEOUT_SECS: u64 = 5;

/// OMS HTTP connect timeout (seconds). TCP + TLS handshake deadline.
pub const OMS_HTTP_CONNECT_TIMEOUT_SECS: u64 = 3;

/// OMS HTTP connection pool idle timeout (seconds). Idle connections are
/// dropped after this duration to avoid stale sockets.
pub const OMS_HTTP_POOL_IDLE_TIMEOUT_SECS: u64 = 90;

/// Phase 0 Item 22a (2026-05-15) — DH-904 (rate-limited) exponential
/// backoff ladder for order placement / modify / cancel. Per
/// `.claude/rules/dhan/api-introduction.md` rule 8: "DH-904 must
/// trigger exponential backoff. 10s → 20s → 40s → 80s → give up +
/// CRITICAL alert. Never immediate retry."
///
/// Index `i` (0-based) is the wait AFTER the `i`-th rate-limit
/// response, BEFORE the next retry. Length is the maximum retry
/// budget — exceeded means escalate to CRITICAL.
///
/// Total worst-case wait between first rate-limit response and final
/// give-up: 10 + 20 + 40 + 80 = 150 seconds. After that, the
/// operator gets a CRITICAL Telegram + the OMS order is rejected to
/// the caller.
pub const DH904_BACKOFF_SECS: &[u64] = &[10, 20, 40, 80];

/// Maximum retry attempts for DH-904 — derived from the backoff
/// ladder length so the two cannot drift apart. Set as a separate
/// `usize` constant for ergonomic use in loop conditions.
pub const DH904_MAX_RETRY_ATTEMPTS: usize = DH904_BACKOFF_SECS.len();

/// Phase 0 Item 22f (2026-05-15) — Self-trade prevention cooldown
/// window in seconds. Per SEBI Circular SEBI/HO/MIRSD/DOP/CIR/P/2018/153
/// (wash-trade prevention), an account placing both legs of a trade
/// within a tight time window creates a regulatory risk that the
/// trade is classified as artificial / non-bona-fide. The 60s cooldown
/// applies AFTER a fill on the same `(security_id, exchange_segment)`:
/// subsequent orders on the opposite side are blocked at the OMS
/// pre-trade gate until the window expires.
///
/// One value applies to all instruments. Different exchanges may have
/// slightly different definitions; 60s is the safe upper bound across
/// NSE / BSE per industry convention.
pub const SELF_TRADE_COOLDOWN_SECS: u64 = 60;

/// Broadcast channel capacity for cold-path tick consumers (trading pipeline,
/// tick persistence, candle aggregation). Must be large enough to absorb bursts
/// during high-volatility events without lagging cold-path consumers.
///
/// CRITICAL: Overflow here = permanent tick loss (tokio broadcast evicts oldest).
/// Sized for 25K instruments:
/// - 262,144 ticks × ~112 bytes = ~28MB RAM
/// - At 10K ticks/sec = 26 seconds headroom before eviction
/// - At 25K ticks/sec = 10 seconds headroom
pub const TICK_BROADCAST_CAPACITY: usize = 262_144;

/// Broadcast channel capacity for order update consumers (OMS, risk engine).
/// 256 order updates × ~512 bytes each = ~128KB. At ~10 orders/sec = ~25 seconds.
pub const ORDER_UPDATE_BROADCAST_CAPACITY: usize = 256;

/// Resilience ring buffer capacity for live candle writer.
/// Holds candles in memory when QuestDB is down, drains on recovery.
///
/// Sized for 25K instruments (1 candle/instrument/sec):
/// - 200,000 candles × ~76 bytes = ~14.5MB RAM
/// - At 25K candles/sec = 8 seconds before disk spill
/// - Candle data is lower priority than raw ticks; 8s is sufficient
///   as disk spill handles overflow seamlessly.
pub const CANDLE_BUFFER_CAPACITY: usize = 200_000;

// 2026-06-28: the 9 OPTION_CHAIN_* constants were REMOVED with the entire
// option_chain REST subsystem (operator directive 2026-06-28). They were
// consumed only by the deleted `crates/core/src/option_chain/` module.

// PR-C (2026-05-26): CROSS_VERIFY_* constants RETIRED. The 4 cross-verify
// constants + their 4 source-pinning tests were deleted along with the
// entire Dhan historical fetch chain (candle_fetcher + cross_verify +
// post_open_cross_check + cross_verify_scheduler + post_market_fetch_window
// modules). Spot-only NIFTY 50 strategy locked; no L3 RECONCILE pass.

/// Default depth batch flush size for QuestDB ILP writes.
/// Each depth snapshot writes 5 rows (one per level), so effective row count = batch × 5.
pub const DEPTH_FLUSH_BATCH_SIZE: usize = 200;

/// Resilience ring buffer capacity for depth persistence writer.
/// Holds depth snapshots in memory when QuestDB is down, drains on recovery.
///
/// Sized for 200 instruments (4 connections × 50 instruments):
/// - 100,000 snapshots × 116 bytes = ~11MB RAM
/// - At ~250 snapshots/sec = ~6.7 minutes before disk spill
/// - Depth data arrives at lower rate than ticks; generous headroom.
pub const DEPTH_BUFFER_CAPACITY: usize = 100_000;

// ---------------------------------------------------------------------------
// Pipeline — Tick Validation Constants
// ---------------------------------------------------------------------------

/// Minimum valid exchange timestamp (epoch seconds).
/// Any tick with exchange_timestamp below this is a junk/initialization frame.
/// 946684800 = 2000-01-01T00:00:00Z — safely before any real market data.
pub const MINIMUM_VALID_EXCHANGE_TIMESTAMP: u32 = 946_684_800;

/// Maximum plausible last-traded price (INR). A tick whose LTP exceeds this is
/// a corrupt / garbage frame — NOT a real observation — and is filtered as junk
/// BEFORE it can poison a candle's high/low or a `ticks` row.
///
/// `is_valid_ltp` already rejects NaN / Inf / ≤ 0, but an absurd-but-FINITE
/// value (e.g. `f32::MAX` ≈ 3.4e38 from a mangled frame) slips through and the
/// O(1) candle fold sets `high = 3.4e38`, permanently corrupting that minute.
///
/// 100,000,000 (₹10 crore) is ~500× above the highest-priced real NSE
/// instrument (MRF ≈ ₹1.5 lakh; SENSEX ≈ 80k; BANKNIFTY ≈ 52k), so it can NEVER
/// reject a genuine price — it only catches corruption. Chosen as an absolute
/// ceiling (not a per-instrument band) precisely so a legitimate large move is
/// never dropped (prime directive: never miss a real tick).
pub const MAX_PLAUSIBLE_LTP: f32 = 100_000_000.0;

// ---------------------------------------------------------------------------
// IST Timezone Offset (i64)
// ---------------------------------------------------------------------------

/// IST offset from UTC in seconds (5 hours 30 minutes) as i64.
/// Used for: (1) converting Historical REST API UTC timestamps to IST,
/// (2) converting `received_at_nanos` (system clock UTC) to IST basis.
/// NOT needed for WebSocket `exchange_timestamp` which is already IST.
pub const IST_UTC_OFFSET_SECONDS_I64: i64 = 19_800;

/// IST offset from UTC in nanoseconds (5h 30m = 19,800,000,000,000 ns).
/// Used for converting `received_at_nanos` (system clock UTC) to IST basis.
pub const IST_UTC_OFFSET_NANOS: i64 = 19_800_000_000_000;

// ---------------------------------------------------------------------------
// Sandbox Guard — Live Trading Date Lock
// ---------------------------------------------------------------------------

/// Earliest date (IST) when `mode = "live"` is permitted.
/// Before this date, config validation rejects live mode at startup.
/// Change these constants ONLY with explicit approval from Parthiban.
pub const LIVE_TRADING_EARLIEST_YEAR: i32 = 2026;
pub const LIVE_TRADING_EARLIEST_MONTH: u32 = 7;
pub const LIVE_TRADING_EARLIEST_DAY: u32 = 1;

// ---------------------------------------------------------------------------
// Periodic Health Check
// ---------------------------------------------------------------------------

/// Interval between periodic health checks (disk space, memory RSS) in seconds.
pub const PERIODIC_HEALTH_CHECK_INTERVAL_SECS: u64 = 300;

/// Maximum age for spill files after successful drain (7 days in seconds).
/// Spill files older than this are auto-deleted during the periodic health check.
pub const SPILL_FILE_MAX_AGE_SECS: u64 = 7 * 24 * 3600;

/// Retention for confirmed-replay WAL segments in `<wal_dir>/archive/`
/// (7 days in seconds, matching [`SPILL_FILE_MAX_AGE_SECS`]) — 2026-07-13
/// disk-retention hardening; widened 2 → 7 days in review round 1 (F3).
///
/// Archived segments are POST-confirmed-replay copies: their frames were
/// re-injected into the live pipeline AND durably persisted before
/// `confirm_replayed` moved them out of `replaying/`. The only reader of
/// `archive/` after that point is the same-day 15:40 IST tick-conservation
/// audit (`count_frames_for_ist_day`), which counts frames for the CURRENT
/// day only (with a 3-day segment-creation pre-filter) — so even 2 days
/// was audit-safe. 7 days is chosen instead (F3) because the archive is
/// ALSO the only remaining copy for the documented confirm-on-channel
/// residual (`confirm_replayed` archives on frames-IN-CHANNEL, not
/// frames-PERSISTED — a crash after the archive move but before the
/// consumer drains leaves the archived segment as the sole triage source):
/// a 2-day window could erase that copy across a long weekend (crash
/// Friday → Monday prune) before anyone triaged; 7 days covers any
/// weekend/holiday gap, matching the spill-file sweep. Before this
/// retention existed, `archive/` grew ~0.15–0.6 GB/day unbounded on the
/// prod 30 GB volume.
pub const WS_WAL_ARCHIVE_RETENTION_SECS: u64 = 604_800;

/// Cadence of the WAL archive prune task (6 hours in seconds).
pub const WS_WAL_ARCHIVE_PRUNE_INTERVAL_SECS: u64 = 6 * 3600;

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
// Shutdown
// ---------------------------------------------------------------------------

/// Maximum time to wait for the tick processor to complete its final QuestDB
/// flush during graceful shutdown. After this, the processor is force-aborted.
pub const GRACEFUL_SHUTDOWN_TIMEOUT_SECS: u64 = 10;

/// Maximum time to wait for initial token acquisition at startup (seconds).
/// Prevents indefinite hang if Dhan API is unreachable on boot.
///
/// Audit Finding #7 (2026-05-03): MUST be `<= BOOT_TIMEOUT_SECS` so the
/// umbrella boot deadline cannot fire BEFORE this step has had a chance
/// to complete. The previous value (300s) exceeded the umbrella (120s)
/// — umbrella would alert at 120s while Dhan auth was still trying.
/// Pinned by `crates/common/tests/boot_timeout_consistency_guard.rs`.
///
/// Observed real-world Dhan auth latency: ~0.4s on a healthy network
/// (verified in 2026-05-03 boot logs). 90s gives 200x headroom for
/// network blips while still respecting the umbrella.
pub const TOKEN_INIT_TIMEOUT_SECS: u64 = 90;

/// Maximum consecutive renewal circuit-breaker cycles before the renewal loop
/// halts and raises a critical alert. Prevents infinite retry with expired token.
pub const TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES: u32 = 5;

/// Audit Finding #6 (2026-05-03): periodic token-sweep cadence.
///
/// The primary `renewal_loop` sleeps until the per-token refresh window
/// (typically about 23h after issuance) and retries with exponential
/// backoff plus circuit breaker. After
/// `TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES` consecutive failures the
/// loop HALTS and the token will eventually expire, leaving a gap with
/// no automatic recovery on a 24h+ uptime session.
///
/// The token-sweep task closes this gap. Every `TOKEN_SWEEP_INTERVAL_SECS`
/// seconds it calls `force_renewal_if_stale(threshold_secs = 14400)`
/// which renews if there is less than 4h headroom remaining. Even if
/// the primary loop has halted, the sweep keeps trying because each
/// call is independent and uses the same retry-with-backoff path.
///
/// Cadence: 4h (14_400s). Pinned to match the
/// `force_renewal_if_stale` threshold so the worst case is one full
/// sweep cycle (4h) of staleness on top of the 4h threshold, giving
/// 8h total worst-case before forced renewal.
pub const TOKEN_SWEEP_INTERVAL_SECS: u64 = 4 * 3600;

/// Audit Finding #6 (2026-05-03): token-sweep staleness threshold.
///
/// Passed to `force_renewal_if_stale(threshold_secs)`. If the loaded
/// token has fewer seconds of validity left than this, the sweep
/// triggers a renewal. 4h aligns with the existing WS-wake call sites
/// (main feed, depth, order update) so behaviour is uniform across all
/// renewal triggers.
pub const TOKEN_SWEEP_STALENESS_THRESHOLD_SECS: i64 = 4 * 3600;

/// GAP-02 (2026-07-14, Dhan noise lock backstop): the Dhan REST-only
/// stack's stale-token sweep cadence.
///
/// The lane's 4h token sweep dies with the lane (#1522); the REST-only
/// stack (`dhan_rest_stack.rs` Phase 3) runs its OWN sweep every this
/// many seconds, calling
/// `force_renewal_if_stale(TOKEN_SWEEP_STALENESS_THRESHOLD_SECS)` —
/// the renewal-loop-circuit-breaker-halt backstop. 900s (matching the
/// mid-session watchdog cadence) instead of the lane's 4h: the stack's
/// spot/chain legs die within minutes of a stale token, so the backstop
/// must react on the same timescale. Silent on no-op/success; a failure
/// logs via the renewal machinery's own paths. NOT market-hours gated
/// (a token that goes stale overnight must heal before the 09:16 first
/// fetch).
pub const DHAN_REST_STACK_TOKEN_SWEEP_INTERVAL_SECS: u64 = 900;

// DELETED: FRAME_SEND_TIMEOUT_SECS and FRAME_BACKPRESSURE_TIMEOUT_SECS
// Removed per P1.3 — WS readers use non-blocking try_send() + WAL spill,
// no backpressure timeout needed. Activity watchdog handles dead sockets.

/// SPSC frame channel capacity (WebSocket reader → tick processor).
/// All 5 WS connections multiplex frames into this single channel.
///
/// Sized for 25K instruments:
/// - 131,072 slots × ~1.5KB avg frame = ~187MB RAM
/// - At 10K ticks/sec = 13 seconds backpressure-free headroom
/// - At 25K ticks/sec = 5.2 seconds headroom
/// - On overflow: backpressure (blocks WS read, WS survives 40s ping timeout)
/// - ZERO frame loss guarantee — backpressure, never drop.
pub const FRAME_CHANNEL_CAPACITY: usize = 131_072;

/// WAL re-injection chunk size (STAGE-C.2b boot recovery — COLD path).
///
/// The boot-time WAL replay re-injects recovered frames into the pool's
/// frame channel with backpressured `send().await` (never `try_send`,
/// never drop). Every `WAL_REINJECT_CHUNK_SIZE` delivered frames the
/// injector calls `tokio::task::yield_now()` so the live WS read loop and
/// the tick processor get scheduled — a 1M+ frame replay must never
/// monopolize the runtime. Must stay well under
/// [`FRAME_CHANNEL_CAPACITY`] (131,072) so one chunk can always fit the
/// channel and per-chunk progress is guaranteed while the consumer drains.
pub const WAL_REINJECT_CHUNK_SIZE: usize = 8_192;

/// Per-send stall timeout for the WAL re-injection (seconds).
///
/// Each backpressured `send().await` during STAGE-C.2b re-injection is
/// wrapped in `tokio::time::timeout`. A healthy tick processor drains the
/// frame channel continuously, so 30 seconds of ZERO progress on a single
/// send means the consumer is wedged or dead — the injector aborts
/// (WS-REINJECT-01), leaving the remaining frames staged in the WAL
/// `replaying/` directory for re-replay next boot. Fail-closed: an abort
/// keeps the re-injection NOT-clean so `confirm_replayed()` never
/// archives a partially delivered replay (durable WAL floor preserved).
pub const WAL_REINJECT_SEND_TIMEOUT_SECS: u64 = 30;

/// Emit a WAL re-injection progress `info!` every this many delivered chunks.
///
/// 16 chunks × [`WAL_REINJECT_CHUNK_SIZE`] (8,192) = every ~131,072 frames —
/// one progress line per channel-capacity's worth of replay. A pathologically
/// large WAL (1M+ frames) drains inline before `notify_systemd_ready`, so an
/// operator tailing logs during a long boot sees periodic
/// `WAL re-injection progress` lines instead of a silent multi-second stall
/// (boot wall-clock scales linearly with WAL backlog size by design —
/// zero-drop trade-off; systemd tolerates it via `TimeoutStartSec=infinity`).
pub const WAL_REINJECT_PROGRESS_LOG_CHUNKS: u64 = 16;

/// Power-of-two exponent for the tick deduplication ring buffer.
///
/// Size = 2^16 = 65,536 slots x 8 bytes = 512 KiB.
/// Catches exact duplicate ticks (same security_id + timestamp + LTP)
/// resent by Dhan on WebSocket reconnection.
///
/// False negatives (missed duplicates due to hash collision/eviction) are safe:
/// QuestDB `DEDUP UPSERT KEYS(ts, security_id)` is the authoritative dedup layer.
pub const DEDUP_RING_BUFFER_POWER: u32 = 16;

// ---------------------------------------------------------------------------
// Indicator Engine — Ring Buffer & Warmup
// ---------------------------------------------------------------------------

/// Ring buffer capacity for windowed indicators (SMA, Stochastic, MFI).
/// Must be a power of two for bitmask-based O(1) indexing.
/// 64 slots covers periods up to 64 (SMA-50, RSI-14, ATR-14, etc.).
pub const INDICATOR_RING_BUFFER_CAPACITY: usize = 64;

/// Minimum ticks required before indicator values are considered reliable.
/// Equal to the longest standard indicator warmup (EMA-26 needs ~26 ticks).
/// After this many ticks, `IndicatorState::is_warm()` returns true.
pub const MAX_INDICATOR_WARMUP_TICKS: u16 = 30;

/// Maximum number of instruments the indicator engine can track.
/// Pre-allocated at startup; O(1) index lookup by security_id.
/// 25,000 covers full Dhan universe (5 connections × 5,000 instruments).
pub const MAX_INDICATOR_INSTRUMENTS: usize = 25_000;

/// Maximum number of strategy FSM instances.
pub const MAX_STRATEGY_INSTANCES: usize = 256;

// ---------------------------------------------------------------------------
// Options Greeks — Black-Scholes Parameters
// ---------------------------------------------------------------------------

/// Risk-free interest rate (annualized) for Black-Scholes pricing.
/// Uses India 91-day T-Bill rate (~6.8% as of Q1 2026).
/// Update quarterly from RBI T-Bill auction results.
pub const RISK_FREE_RATE: f64 = 0.068;

/// Continuous dividend yield (annualized) for NIFTY/BANKNIFTY.
/// ~1.2% for NIFTY 50 (varies; update quarterly).
pub const DIVIDEND_YIELD: f64 = 0.012;

/// Maximum Newton-Raphson iterations for IV solver.
/// 50 iterations is sufficient for convergence to 1e-8 tolerance.
pub const IV_SOLVER_MAX_ITERATIONS: u32 = 50;

/// IV solver convergence tolerance (precision of implied volatility).
pub const IV_SOLVER_TOLERANCE: f64 = 1e-8;

/// Minimum implied volatility bound (0.1% — prevents zero/negative IV).
pub const IV_MIN_BOUND: f64 = 0.001;

/// Maximum implied volatility bound (500% — extreme but valid for penny options).
pub const IV_MAX_BOUND: f64 = 5.0;

// ---------------------------------------------------------------------------
// Frontend — Tick Broadcast Channel
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Token Cache — Fast Restart
// ---------------------------------------------------------------------------

/// File path for the cached JWT token (inside Docker container).
///
/// `/tmp` is ephemeral on container restart but survives process crashes
/// within the same container — exactly the right lifetime for crash recovery.
/// Security: file permissions 0600, container isolation, 24h TTL token.
/// Token cache file path. Uses `data/cache/` (persistent filesystem) instead of `/tmp`
/// which may be tmpfs on Linux (wiped on reboot). The `data/` directory is the app's
/// persistent data root, also used by rkyv instrument cache and log files.
pub const TOKEN_CACHE_FILE_PATH: &str = "data/cache/tv-token-cache";

/// Minimum remaining token validity (hours) to accept a cached token.
///
/// If the cached token expires in less than this, a full re-auth is performed.
/// 1 hour gives the renewal loop time to refresh before expiry.
pub const TOKEN_CACHE_MIN_REMAINING_HOURS: i64 = 1;

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
// 6 F&O indices (from VALIDATION_MUST_EXIST_INDICES) + 23 display.
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

// Sanity: deep depth packet sizes are consistent.
const _: () = assert!(
    TWENTY_DEPTH_PACKET_SIZE
        == DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE,
    "TWENTY_DEPTH_PACKET_SIZE mismatch"
);
const _: () = assert!(
    TWO_HUNDRED_DEPTH_PACKET_SIZE
        == DEEP_DEPTH_HEADER_SIZE + TWO_HUNDRED_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE,
    "TWO_HUNDRED_DEPTH_PACKET_SIZE mismatch"
);

// Sanity: dedup ring buffer power must be in range [8, 24].
// 2^8 = 256 (minimum useful), 2^24 = 16M (8 bytes × 16M = 128 MiB max).
const _: () = assert!(
    DEDUP_RING_BUFFER_POWER >= 8 && DEDUP_RING_BUFFER_POWER <= 24,
    "DEDUP_RING_BUFFER_POWER must be in [8, 24]"
);

// Sanity: tick persist window — start < end, both within a day, values match IST times.
const _: () = assert!(
    TICK_PERSIST_START_SECS_OF_DAY_IST == 9 * 3600,
    "TICK_PERSIST_START must equal 09:00 IST (32400)"
);
const _: () = assert!(
    TICK_PERSIST_END_SECS_OF_DAY_IST == 15 * 3600 + 30 * 60,
    "TICK_PERSIST_END must equal 15:30 IST (55800)"
);
const _: () = assert!(
    TICK_PERSIST_START_SECS_OF_DAY_IST < TICK_PERSIST_END_SECS_OF_DAY_IST,
    "TICK_PERSIST_START must be before TICK_PERSIST_END"
);
const _: () = assert!(
    TICK_PERSIST_END_SECS_OF_DAY_IST < SECONDS_PER_DAY,
    "TICK_PERSIST_END must be within a single day"
);

// CCL-06: Muhurat persist window — start < end, both within a day, values match
// IST times, and the window is disjoint from + AFTER the regular window (so the
// two ranges never overlap and the Muhurat branch is purely additive).
const _: () = assert!(
    MUHURAT_PERSIST_START_SECS_OF_DAY_IST == 18 * 3600,
    "MUHURAT_PERSIST_START must equal 18:00 IST (64800)"
);
const _: () = assert!(
    MUHURAT_PERSIST_END_SECS_OF_DAY_IST == 19 * 3600 + 30 * 60,
    "MUHURAT_PERSIST_END must equal 19:30 IST (70200)"
);
const _: () = assert!(
    MUHURAT_PERSIST_START_SECS_OF_DAY_IST < MUHURAT_PERSIST_END_SECS_OF_DAY_IST,
    "MUHURAT_PERSIST_START must be before MUHURAT_PERSIST_END"
);
const _: () = assert!(
    MUHURAT_PERSIST_END_SECS_OF_DAY_IST < SECONDS_PER_DAY,
    "MUHURAT_PERSIST_END must be within a single day"
);
const _: () = assert!(
    TICK_PERSIST_END_SECS_OF_DAY_IST <= MUHURAT_PERSIST_START_SECS_OF_DAY_IST,
    "MUHURAT window must be disjoint from + after the regular [09:00, 15:30) window"
);

// Post-market fetch window invariants (PR #796, operator-locked 2026-05-25).
const _: () = assert!(
    POST_MARKET_FETCH_WINDOW_START_SECS_OF_DAY_IST == 15 * 3600 + 30 * 60,
    "POST_MARKET_FETCH_WINDOW_START must equal 15:30 IST (55800)"
);
const _: () = assert!(
    POST_MARKET_FETCH_WINDOW_END_SECS_OF_DAY_IST == 23 * 3600,
    "POST_MARKET_FETCH_WINDOW_END must equal 23:00 IST (82800)"
);
const _: () = assert!(
    POST_MARKET_FETCH_WINDOW_START_SECS_OF_DAY_IST < POST_MARKET_FETCH_WINDOW_END_SECS_OF_DAY_IST,
    "POST_MARKET_FETCH_WINDOW_START must be before POST_MARKET_FETCH_WINDOW_END"
);
const _: () = assert!(
    POST_MARKET_FETCH_WINDOW_END_SECS_OF_DAY_IST < SECONDS_PER_DAY,
    "POST_MARKET_FETCH_WINDOW_END must be within a single day"
);
const _: () = assert!(
    POST_MARKET_FETCH_WINDOW_START_SECS_OF_DAY_IST == TICK_PERSIST_END_SECS_OF_DAY_IST,
    "Fetch window must start exactly when market closes (15:30 IST)"
);

// Phase 0 Item 20 — orphan-position watchdog timing invariants.
const _: () = assert!(
    ORPHAN_POSITION_WATCHDOG_TIME_SECS_IST == 15 * 3600 + 25 * 60,
    "ORPHAN_POSITION_WATCHDOG_TIME must equal 15:25 IST (55500)"
);
const _: () = assert!(
    ORPHAN_POSITION_WATCHDOG_TIME_SECS_IST < TICK_PERSIST_END_SECS_OF_DAY_IST,
    "Watchdog must fire BEFORE 15:30 IST close (Phase 1+ exit must \
     land before NSE rejects late orders)"
);
const _: () = assert!(
    TICK_PERSIST_END_SECS_OF_DAY_IST - ORPHAN_POSITION_WATCHDOG_TIME_SECS_IST == 300,
    "Watchdog must fire exactly 5 minutes (300s) before close"
);
const _: () = assert!(SECONDS_PER_DAY == 86_400, "SECONDS_PER_DAY must be 86400");

// Sanity: indicator ring buffer capacity must be a power of two (bitmask indexing).
const _: () = assert!(
    INDICATOR_RING_BUFFER_CAPACITY.is_power_of_two(),
    "INDICATOR_RING_BUFFER_CAPACITY must be power of 2"
);

// ---------------------------------------------------------------------------
// Compile-Time Assertions — Binary Protocol Offset Chain Verification
// Source: Dhan API (Python SDK ref) struct.unpack format strings.
// Ensures every offset = previous_offset + previous_field_size.
// ---------------------------------------------------------------------------

// Header offset chain: <BHBi> = response_code(1) + msg_length(2) + segment(1) + security_id(4) = 8
const _: () = assert!(
    HEADER_OFFSET_RESPONSE_CODE == 0,
    "header: response_code at 0"
);
const _: () = assert!(HEADER_OFFSET_MESSAGE_LENGTH == 1, "header: msg_length at 1");
const _: () = assert!(HEADER_OFFSET_EXCHANGE_SEGMENT == 3, "header: segment at 3");
const _: () = assert!(HEADER_OFFSET_SECURITY_ID == 4, "header: security_id at 4");
const _: () = assert!(BINARY_HEADER_SIZE == 8, "header total = 8");

// Ticker offset chain: header(8) + LTP(f32=4) + LTT(u32=4) = 16
const _: () = assert!(TICKER_OFFSET_LTP == 8, "ticker: LTP at 8");
const _: () = assert!(TICKER_OFFSET_LTT == 12, "ticker: LTT at 12");
const _: () = assert!(TICKER_PACKET_SIZE == 16, "ticker total = 16");

// Quote offset chain: <BHBIfHIfIIIffff> = 50
const _: () = assert!(QUOTE_OFFSET_LTP == 8, "quote: LTP at 8");
const _: () = assert!(QUOTE_OFFSET_LTQ == 12, "quote: LTQ at 12");
const _: () = assert!(QUOTE_OFFSET_LTT == 14, "quote: LTT at 14");
const _: () = assert!(QUOTE_OFFSET_ATP == 18, "quote: ATP at 18");
const _: () = assert!(QUOTE_OFFSET_VOLUME == 22, "quote: volume at 22");
const _: () = assert!(QUOTE_OFFSET_TOTAL_SELL_QTY == 26, "quote: total_sell at 26");
const _: () = assert!(QUOTE_OFFSET_TOTAL_BUY_QTY == 30, "quote: total_buy at 30");
const _: () = assert!(QUOTE_OFFSET_OPEN == 34, "quote: open at 34");
const _: () = assert!(QUOTE_OFFSET_CLOSE == 38, "quote: close at 38");
const _: () = assert!(QUOTE_OFFSET_HIGH == 42, "quote: high at 42");
const _: () = assert!(QUOTE_OFFSET_LOW == 46, "quote: low at 46");
const _: () = assert!(QUOTE_PACKET_SIZE == 50, "quote total = 50");

// Full packet offset chain: diverges from Quote at 34
// <BHBIfHIfIIIIIIffff100s> = 162
const _: () = assert!(FULL_OFFSET_OI == 34, "full: OI at 34");
const _: () = assert!(FULL_OFFSET_OI_DAY_HIGH == 38, "full: OI high at 38");
const _: () = assert!(FULL_OFFSET_OI_DAY_LOW == 42, "full: OI low at 42");
const _: () = assert!(FULL_OFFSET_OPEN == 46, "full: open at 46");
const _: () = assert!(FULL_OFFSET_CLOSE == 50, "full: close at 50");
const _: () = assert!(FULL_OFFSET_HIGH == 54, "full: high at 54");
const _: () = assert!(FULL_OFFSET_LOW == 58, "full: low at 58");
const _: () = assert!(FULL_OFFSET_DEPTH_START == 62, "full: depth at 62");
const _: () = assert!(
    FULL_QUOTE_PACKET_SIZE
        == FULL_OFFSET_DEPTH_START + MARKET_DEPTH_LEVELS * MARKET_DEPTH_LEVEL_SIZE,
    "full total = 62 + 5*20 = 162"
);

// Market Depth standalone offset chain: <BHBIf100s> = 112
const _: () = assert!(MARKET_DEPTH_OFFSET_LTP == 8, "mkt_depth: LTP at 8");
const _: () = assert!(
    MARKET_DEPTH_OFFSET_DEPTH_START == 12,
    "mkt_depth: depth at 12"
);
const _: () = assert!(
    MARKET_DEPTH_PACKET_SIZE
        == MARKET_DEPTH_OFFSET_DEPTH_START + MARKET_DEPTH_LEVELS * MARKET_DEPTH_LEVEL_SIZE,
    "mkt_depth total = 12 + 5*20 = 112"
);

// Per-level offset chain: <IIHHff> = 20 bytes
const _: () = assert!(DEPTH_LEVEL_OFFSET_BID_QTY == 0, "level: bid_qty at 0");
const _: () = assert!(DEPTH_LEVEL_OFFSET_ASK_QTY == 4, "level: ask_qty at 4");
const _: () = assert!(DEPTH_LEVEL_OFFSET_BID_ORDERS == 8, "level: bid_orders at 8");
const _: () = assert!(
    DEPTH_LEVEL_OFFSET_ASK_ORDERS == 10,
    "level: ask_orders at 10"
);
const _: () = assert!(DEPTH_LEVEL_OFFSET_BID_PRICE == 12, "level: bid_price at 12");
const _: () = assert!(DEPTH_LEVEL_OFFSET_ASK_PRICE == 16, "level: ask_price at 16");
const _: () = assert!(MARKET_DEPTH_LEVEL_SIZE == 20, "level total = 20");

// Deep depth per-level offset chain: <dII> = 16 bytes
const _: () = assert!(DEEP_DEPTH_LEVEL_OFFSET_PRICE == 0, "deep_level: price at 0");
const _: () = assert!(
    DEEP_DEPTH_LEVEL_OFFSET_QUANTITY == 8,
    "deep_level: qty at 8"
);
const _: () = assert!(
    DEEP_DEPTH_LEVEL_OFFSET_ORDERS == 12,
    "deep_level: orders at 12"
);
const _: () = assert!(DEEP_DEPTH_LEVEL_SIZE == 16, "deep_level total = 16");

// Deep depth header offset chain: <hBBiI> = 12 bytes
const _: () = assert!(
    DEEP_DEPTH_HEADER_OFFSET_MSG_LENGTH == 0,
    "deep_hdr: msg_len at 0"
);
const _: () = assert!(
    DEEP_DEPTH_HEADER_OFFSET_FEED_CODE == 2,
    "deep_hdr: feed_code at 2"
);
const _: () = assert!(
    DEEP_DEPTH_HEADER_OFFSET_EXCHANGE_SEGMENT == 3,
    "deep_hdr: segment at 3"
);
const _: () = assert!(
    DEEP_DEPTH_HEADER_OFFSET_SECURITY_ID == 4,
    "deep_hdr: security_id at 4"
);
const _: () = assert!(
    DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE == 8,
    "deep_hdr: sequence at 8"
);
const _: () = assert!(DEEP_DEPTH_HEADER_SIZE == 12, "deep_hdr total = 12");

// OI packet: <BHBII> = 12 bytes
const _: () = assert!(OI_OFFSET_VALUE == 8, "oi: value at 8");
const _: () = assert!(OI_PACKET_SIZE == 12, "oi total = 12");

// Previous Close packet: <BHBIfI> = 16 bytes
const _: () = assert!(PREV_CLOSE_OFFSET_PRICE == 8, "prev_close: price at 8");
const _: () = assert!(PREV_CLOSE_OFFSET_OI == 12, "prev_close: OI at 12");
const _: () = assert!(PREVIOUS_CLOSE_PACKET_SIZE == 16, "prev_close total = 16");

// Disconnect packet: <BHBIH> = 10 bytes
const _: () = assert!(DISCONNECT_OFFSET_CODE == 8, "disconnect: code at 8");
const _: () = assert!(DISCONNECT_PACKET_SIZE == 10, "disconnect total = 10");

// ---------------------------------------------------------------------------
// Index Constituency — NSE index constituent CSV download
// Source: niftyindices.com public CSV endpoints
// ---------------------------------------------------------------------------

/// Base URL for NSE index constituent CSV downloads.
// O(1) EXEMPT: compile-time constant, not runtime allocation
pub const INDEX_CONSTITUENCY_BASE_URL: &str = "https://www.niftyindices.com/IndexConstituent/";

/// Cache filename for the serialized constituency map.
pub const INDEX_CONSTITUENCY_CSV_CACHE_FILENAME: &str = "constituency-map.json";

/// Minimum number of indices that must download successfully.
/// Below this threshold, a warning is emitted (map may be incomplete).
pub const INDEX_CONSTITUENCY_MIN_INDICES: usize = 15;

/// Retry: minimum backoff delay in seconds between download attempts.
pub const INDEX_CONSTITUENCY_RETRY_MIN_DELAY_SECS: u64 = 1;

/// Retry: maximum backoff delay in seconds between download attempts.
pub const INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS: u64 = 10;

/// Retry: maximum number of attempts per CSV download (operator 2026-06-08:
/// "retry at least five times"). Wired into `build_constituency_map` with
/// exponential backoff between `INDEX_CONSTITUENCY_RETRY_MIN_DELAY_SECS` and
/// `INDEX_CONSTITUENCY_RETRY_MAX_DELAY_SECS`. A transient niftyindices timeout /
/// 5xx / reset on one attempt no longer drops the whole NTM list for the day.
pub const INDEX_CONSTITUENCY_RETRY_MAX_TIMES: usize = 5;

/// User-Agent header for niftyindices.com requests.
/// niftyindices.com returns 403 Forbidden without a browser-like User-Agent.
pub const INDEX_CONSTITUENCY_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// NSE index slugs: `(display_name, csv_slug)`.
/// URL = `{INDEX_CONSTITUENCY_BASE_URL}{slug}.csv`
///
/// Comprehensive list covering all broad-based, sectoral, and thematic NSE indices.
/// Source: niftyindices.com public CSV endpoints.
/// Individual download failures are handled gracefully (logged + skipped).
pub const INDEX_CONSTITUENCY_SLUGS: &[(&str, &str)] = &[
    // -----------------------------------------------------------------------
    // Broad-Based Indices (16)
    // -----------------------------------------------------------------------
    ("Nifty 50", "ind_nifty50list"),
    ("Nifty Next 50", "ind_niftynext50list"),
    ("Nifty 100", "ind_nifty100list"),
    ("Nifty 200", "ind_nifty200list"),
    ("Nifty 500", "ind_nifty500list"),
    ("Nifty Midcap 50", "ind_niftymidcap50list"),
    ("Nifty Midcap 100", "ind_niftymidcap100list"),
    ("Nifty Midcap 150", "ind_niftymidcap150list"),
    ("Nifty Midcap Select", "ind_niftymidcapselect_list"),
    ("Nifty Smallcap 50", "ind_niftysmallcap50list"),
    ("Nifty Smallcap 100", "ind_niftysmallcap100list"),
    ("Nifty Smallcap 250", "ind_niftysmallcap250list"),
    ("Nifty LargeMidcap 250", "ind_niftylargemidcap250list"),
    ("Nifty MidSmallcap 400", "ind_niftymidsmallcap400list"),
    ("Nifty Microcap 250", "ind_niftymicrocap250_list"),
    ("Nifty Total Market", "ind_niftytotalmarket_list"),
    // -----------------------------------------------------------------------
    // Sectoral Indices (15)
    // -----------------------------------------------------------------------
    ("Nifty Auto", "ind_niftyautolist"),
    ("Nifty Bank", "ind_niftybanklist"),
    ("Nifty Financial Services", "ind_niftyfinancelist"),
    (
        "Nifty Financial Services 25/50",
        "ind_niftyfinancialservices25-50list",
    ),
    ("Nifty FMCG", "ind_niftyfmcglist"),
    ("Nifty Healthcare", "ind_niftyhealthcarelist"),
    ("Nifty IT", "ind_niftyitlist"),
    ("Nifty Media", "ind_niftymedialist"),
    ("Nifty Metal", "ind_niftymetallist"),
    ("Nifty Pharma", "ind_niftypharmalist"),
    ("Nifty PSU Bank", "ind_niftypsubanklist"),
    ("Nifty Private Bank", "ind_nifty_privatebanklist"),
    ("Nifty Realty", "ind_niftyrealtylist"),
    ("Nifty Consumer Durables", "ind_niftyconsumerdurableslist"),
    ("Nifty Oil & Gas", "ind_niftyoilgaslist"),
    // -----------------------------------------------------------------------
    // Thematic / Strategy Indices (18)
    // -----------------------------------------------------------------------
    ("Nifty Commodities", "ind_niftycommoditieslist"),
    ("Nifty India Consumption", "ind_niftyconsumptionlist"),
    ("Nifty CPSE", "ind_niftycpselist"),
    ("Nifty Energy", "ind_niftyenergylist"),
    ("Nifty Infrastructure", "ind_niftyinfralist"),
    ("Nifty MNC", "ind_niftymnclist"),
    ("Nifty PSE", "ind_niftypselist"),
    ("Nifty Growth Sectors 15", "ind_NiftyGrowth_Sectors15_Index"),
    ("Nifty100 Quality 30", "ind_nifty100Quality30list"),
    ("Nifty50 Value 20", "ind_Nifty50_Value20"),
    ("Nifty Alpha 50", "ind_nifty_Alpha_Index"),
    ("Nifty High Beta 50", "nifty_High_Beta50_Index"),
    ("Nifty Low Volatility 50", "nifty_low_Volatility50_Index"),
    ("Nifty India Digital", "ind_niftyIndiaDigital_list"),
    ("Nifty India Defence", "ind_niftyIndiaDefence_list"),
    (
        "Nifty India Manufacturing",
        "ind_niftyindiamanufacturing_list",
    ),
    ("Nifty Mobility", "ind_niftymobility_list"),
    (
        "Nifty500 Multicap 50:25:25",
        "ind_nifty500Multicap502525_list",
    ),
];

/// Number of slugs in `INDEX_CONSTITUENCY_SLUGS`.
pub const INDEX_CONSTITUENCY_SLUG_COUNT: usize = INDEX_CONSTITUENCY_SLUGS.len();

/// The SUBSCRIPTION constituent source (§31 item 4, operator lock 2026-06-06):
/// the live main-feed subscription is the **NIFTY Total Market union ONLY**
/// (NTM is the broadest basket and already contains the other indices' members).
/// The boot turn-on (NTM Sub-PR #10b) fetches THIS slug — NOT the full
/// `INDEX_CONSTITUENCY_SLUGS` — because the deduped union of all ~49 indices is
/// ~the entire NSE (~1,900 stocks), which would breach `MAX_DAILY_UNIVERSE_SIZE`
/// and HALT boot. The full list above is for the (map-only, not-subscribed)
/// §31-item-2 index→constituents mapping, a separate concern.
pub const NTM_CONSTITUENCY_SLUGS: &[(&str, &str)] =
    &[("Nifty Total Market", "ind_niftytotalmarket_list")];

/// Total wall-clock budget for the boot-time NTM constituency fetch (§31 Sub-PR
/// #10b). niftyindices.com can accept a connection then stall the body (the
/// per-GET 60s read timeout doesn't bound a slow-loris before the 09:00 open),
/// so the boot caller wraps the fetch in `tokio::time::timeout(this)` and
/// DEGRADES to the core universe on expiry (operator policy 2026-06-06).
pub const NTM_CONSTITUENCY_FETCH_TIMEOUT_SECS: u64 = 30;

/// Slugs known to return 404/HTML from niftyindices.com — skipped by the
/// CSV downloader so we don't log 14 WARNs + fire an aggregated ERROR every
/// boot for URLs we already know are wrong. The underlying CSVs almost
/// certainly still exist at niftyindices.com, but under a different
/// filename; the slugs here are the ones we shipped that the server now
/// rejects with a 404 HTML page (classified as `kind=missing`).
///
/// **Last audited: 2026-04-24** (Q8 — 14 slugs confirmed stale by live
/// post-market log review; HTTP headers, User-Agent, and Referer are all
/// correct, so this is a pure URL-path problem, not a bot-wall problem).
///
/// ## Operator action to re-enable one of these
///
/// 1. Open <https://www.niftyindices.com> in a normal browser
/// 2. Navigate to the index (e.g. Indices → Strategy → Nifty Alpha 50)
/// 3. Click the "Download CSV" button on the index page
/// 4. Inspect the download URL — copy the `ind_*.csv` filename
/// 5. Update the corresponding entry in [`INDEX_CONSTITUENCY_SLUGS`]
/// 6. Remove that slug from this list
///
/// ## Why we can't auto-fix from code
///
/// niftyindices.com is behind Cloudflare bot protection that blocks
/// sandbox/datacenter IPs (verified 2026-04-24: every WebFetch from
/// this environment returned 403). The correct slugs can only be
/// discovered from a real browser session.
pub const INDEX_CONSTITUENCY_KNOWN_STALE_SLUGS: &[&str] = &[];

// ---------------------------------------------------------------------------
// Wave 2 Item 7 (G14, G8) — Boot-time readiness + clock-skew probe constants
//
// Centralised here as the canonical pin so the escalation table is
// asserted by both `boot_probe.rs` (consumer) and the rule files
// (`wave-2-error-codes.md`, `wave-2-c-error-codes.md`). Any drift fails
// the escalation_table_constants_match_pinned_values ratchet.
// ---------------------------------------------------------------------------

/// Boot deadline before BOOT-02 fires + the app halts.
pub const BOOT_DEADLINE_SECS: u64 = 60;

/// Escalation thresholds for `wait_for_questdb_ready`. Pinned as the
/// single source of truth for the +5 / +10 / +20 / +30 / +60 table.
pub const BOOT_ESCALATION_DEBUG_AT_SECS: u64 = 5;
/// Boot escalation: INFO at +10s.
pub const BOOT_ESCALATION_INFO_AT_SECS: u64 = 10;
/// Boot escalation: WARN at +20s.
pub const BOOT_ESCALATION_WARN_AT_SECS: u64 = 20;
/// Boot escalation: ERROR `BOOT-01` at +30s.
pub const BOOT_ESCALATION_ERROR_AT_SECS: u64 = 30;

/// Hard threshold (signed seconds) for the boot-time clock-skew probe.
/// Exceeding this magnitude (`|skew| >= 2.0`) emits `BOOT-03` + HALT.
///
/// Justification: IST timestamp math + DEDUP UPSERT keys depend on
/// accurate wall-clock. ±2s can cross IST midnight, the 09:00:00
/// market-open gate, or the 15:30:00 close gate — silently splitting or
/// merging trading days in QuestDB.
pub const CLOCK_SKEW_HALT_THRESHOLD_SECS: f64 = 2.0;

// ---------------------------------------------------------------------------
// Phase 0 Item 11 — Dual-Gate Market-Hours (operator-locked 2026-05-13)
// ---------------------------------------------------------------------------
//
// Per `topic-PHASE-0-LEAN-LOCKED.md` §8: split market-hours filtering
// into TWO independent gates so a tick stamped `exchange_timestamp =
// 15:29:59.586 IST` is NOT rejected just because the local wall-clock
// has advanced to `15:30:00.100 IST` by the time the gate evaluates.
//
//   G1 EXCHANGE GATE (the truth)
//     Source:    tick.exchange_timestamp_ist (Dhan-stamped, IST)
//     Boundary:  [09:15:00.000, 15:30:00.000) — EXCLUSIVE on close
//     Decides:   whether the tick belongs to the current trading session.
//                The 15:29:59.999 tick is accepted; the 15:30:00.000 tick
//                is rejected.
//
//   G2 WALL-CLOCK GATE (wait window)
//     Source:    local now_ist()
//     Boundary:  open until 15:31:00 IST (60s grace after session close)
//     Decides:   when to close the WebSocket socket / seal the final bar.
//                The grace covers Dhan ingestion + network delay tail —
//                a tick stamped 15:29:59 that arrives at 15:30:30 wall-
//                clock IS still in scope; only after 15:31:00 IST does
//                the socket close.
//
// Why 60s grace and not 5s: Dhan ingestion + network at close-of-day
// spike can delay last ticks up to ~45s. 60s safely covers; matches
// T+1-minute industry trade-reporting tail.
//
// Banned-pattern hook (to be added in follow-up): rejects any
// `is_within_market_hours.*now()` — code must use
// `tick.exchange_timestamp_ist` not `now_ist()` for the G1 gate.

/// G1 Exchange Gate — session open in IST nanoseconds-of-day.
/// 09:15:00.000 IST = 9*3600*1e9 + 15*60*1e9 = 33_300 * 1e9.
pub const MARKET_OPEN_IST_NANOS: i64 = 33_300_000_000_000;

/// G1 Exchange Gate — session close in IST nanoseconds-of-day.
/// 15:30:00.000 IST = 55_800 * 1e9. **EXCLUSIVE** — a tick at exactly
/// 15:30:00.000 is REJECTED (matches the existing
/// `TICK_PERSIST_END_SECS_OF_DAY_IST = 55_800` exclusive contract).
pub const MARKET_CLOSE_IST_NANOS: i64 = 55_800_000_000_000;

/// G2 Wall-Clock Gate — grace period in seconds AFTER `MARKET_CLOSE_IST`
/// during which the WebSocket socket stays open to absorb late-arriving
/// ticks stamped before close.
pub const WS_GRACE_AFTER_CLOSE_SECS: u64 = 60;

/// Same value as [`WS_GRACE_AFTER_CLOSE_SECS`] but typed `u32` for use
/// with the `secs-of-day` arithmetic in tick-processor gate functions
/// where `TICK_PERSIST_END_SECS_OF_DAY_IST` is also `u32`. A compile-time
/// assert below pins the two to the same numeric value.
pub const WS_GRACE_AFTER_CLOSE_SECS_U32: u32 = 60;

const _: () = assert!(
    WS_GRACE_AFTER_CLOSE_SECS_U32 as u64 == WS_GRACE_AFTER_CLOSE_SECS,
    "WS_GRACE_AFTER_CLOSE_SECS_U32 must equal WS_GRACE_AFTER_CLOSE_SECS — \
     they describe the same 60s grace window in two integer widths"
);

/// G2 Wall-Clock Gate — number of seconds AFTER session-end bar
/// (15:29-15:30) before forcing the seal. Matches `WS_GRACE_AFTER_CLOSE_SECS`
/// by design: the 15:29 bar seals at 15:31:00 IST exchange-ts so any
/// late-arriving tick within the 60s grace lands in its true bar.
pub const BAR_FINAL_SEAL_OFFSET_SECS: u64 = 60;

/// Soft anomaly threshold — if a tick arrives with `exchange_ts` more
/// than `LATE_TICK_ANOMALY_THRESHOLD_MS` BEHIND the local wall-clock
/// receive time, it is counted by `tv_late_tick_after_boundary_total`
/// (the `LastTickAfterBoundary` Telegram variant was retired 2026-06-12 —
/// redundant with that counter; the per-tick hot path must not carry a
/// notifier). Helps operators spot Dhan-side ingestion lag or clock skew
/// without halting tick acceptance.
pub const LATE_TICK_ANOMALY_THRESHOLD_MS: u64 = 30_000;

// Compile-time consistency: BAR_FINAL_SEAL_OFFSET_SECS must equal
// WS_GRACE_AFTER_CLOSE_SECS — they're two views of the same 60s
// grace window. Drift would let the socket close before the final
// seal fires (or vice versa), leaving an inconsistent state.
const _: () = assert!(
    BAR_FINAL_SEAL_OFFSET_SECS == WS_GRACE_AFTER_CLOSE_SECS,
    "BAR_FINAL_SEAL_OFFSET_SECS and WS_GRACE_AFTER_CLOSE_SECS must be equal — \
     they describe the same 60s grace window in two roles"
);

// Compile-time consistency: MARKET_CLOSE_IST_NANOS must equal
// TICK_PERSIST_END_SECS_OF_DAY_IST converted to nanos. If anyone
// shifts one without the other, the G1 gate disagrees with the
// existing persist gate.
const _: () = assert!(
    MARKET_CLOSE_IST_NANOS == (TICK_PERSIST_END_SECS_OF_DAY_IST as i64) * 1_000_000_000,
    "MARKET_CLOSE_IST_NANOS must match TICK_PERSIST_END_SECS_OF_DAY_IST × 1e9"
);

const _: () = assert!(
    MARKET_OPEN_IST_NANOS < MARKET_CLOSE_IST_NANOS,
    "Session open must be strictly before session close"
);

/// Phase 0 Item 11 — G1 Exchange Gate. Returns `true` iff the tick's
/// `exchange_timestamp_ist` (IST nanoseconds-of-day) is within the
/// half-open session window `[09:15:00.000, 15:30:00.000)`.
///
/// This is the SOLE source-of-truth gate for "does this tick belong
/// to the trading session?". Local wall-clock is irrelevant here —
/// only the Dhan-stamped timestamp matters. The dual-gate fix
/// (`topic-PHASE-0-LEAN-LOCKED.md` §8) cured the bug where a tick at
/// `exchange_ts = 15:29:59.586` was rejected because the local clock
/// had advanced to `15:30:00.100`.
///
/// Pure `const fn`; tested by `test_g1_exchange_gate_*` ratchets.
#[inline]
#[must_use]
pub const fn g1_exchange_gate_accepts(exchange_ts_nanos_of_day: i64) -> bool {
    exchange_ts_nanos_of_day >= MARKET_OPEN_IST_NANOS
        && exchange_ts_nanos_of_day < MARKET_CLOSE_IST_NANOS
}

/// Phase 0 Item 11 — G2 Wall-Clock Gate. Returns `true` iff the local
/// wall-clock IST nanoseconds-of-day is within
/// `[09:15:00.000, 15:31:00.000)` — the session window PLUS the 60s
/// grace tail.
///
/// Used to decide when to close the WebSocket socket / seal the final
/// bar. NEVER used to decide whether a tick belongs to the session
/// (that's G1's job).
///
/// Pure `const fn`; tested by `test_g2_wall_clock_gate_*` ratchets.
#[inline]
#[must_use]
pub const fn g2_wall_clock_gate_accepts(wall_clock_ts_nanos_of_day: i64) -> bool {
    let grace_close_nanos = MARKET_CLOSE_IST_NANOS
        .saturating_add((WS_GRACE_AFTER_CLOSE_SECS as i64).saturating_mul(1_000_000_000));
    wall_clock_ts_nanos_of_day >= MARKET_OPEN_IST_NANOS
        && wall_clock_ts_nanos_of_day < grace_close_nanos
}

// ---------------------------------------------------------------------------
// Tests — Market Hours Constants
// ---------------------------------------------------------------------------

#[cfg(test)]
mod market_hours_tests {
    use super::*;

    #[test]
    fn test_tick_persist_start_matches_nine_am() {
        assert_eq!(TICK_PERSIST_START_SECS_OF_DAY_IST, 9 * 3600);
        assert_eq!(TICK_PERSIST_START_SECS_OF_DAY_IST, 32_400);
    }

    #[test]
    fn test_tick_persist_end_matches_three_thirty() {
        assert_eq!(TICK_PERSIST_END_SECS_OF_DAY_IST, 15 * 3600 + 30 * 60);
        assert_eq!(TICK_PERSIST_END_SECS_OF_DAY_IST, 55_800);
    }

    #[test]
    fn test_muhurat_window_constants_pinned() {
        // CCL-06: 18:00 IST open, 19:30 IST close (exclusive), superset of the
        // announced NSE Muhurat windows.
        assert_eq!(MUHURAT_PERSIST_START_SECS_OF_DAY_IST, 18 * 3600);
        assert_eq!(MUHURAT_PERSIST_START_SECS_OF_DAY_IST, 64_800);
        assert_eq!(MUHURAT_PERSIST_END_SECS_OF_DAY_IST, 19 * 3600 + 30 * 60);
        assert_eq!(MUHURAT_PERSIST_END_SECS_OF_DAY_IST, 70_200);
        // start < end, within a day.
        assert!(MUHURAT_PERSIST_START_SECS_OF_DAY_IST < MUHURAT_PERSIST_END_SECS_OF_DAY_IST);
        assert!(MUHURAT_PERSIST_END_SECS_OF_DAY_IST < SECONDS_PER_DAY);
        // Disjoint from + strictly after the regular window: the Muhurat range
        // is purely additive, never overlapping [09:00, 15:30).
        assert!(TICK_PERSIST_END_SECS_OF_DAY_IST <= MUHURAT_PERSIST_START_SECS_OF_DAY_IST);
    }

    #[test]
    fn test_seconds_per_day_correct() {
        assert_eq!(SECONDS_PER_DAY, 24 * 3600);
        assert_eq!(SECONDS_PER_DAY, 86_400);
    }

    #[test]
    fn test_market_close_drain_buffer_constant() {
        const { assert!(MARKET_CLOSE_DRAIN_BUFFER_SECS >= 1) };
        const { assert!(MARKET_CLOSE_DRAIN_BUFFER_SECS <= 5) };
    }

    #[test]
    fn test_persist_window_is_subset_of_day() {
        const { assert!(TICK_PERSIST_START_SECS_OF_DAY_IST < TICK_PERSIST_END_SECS_OF_DAY_IST) };
        const { assert!(TICK_PERSIST_END_SECS_OF_DAY_IST < SECONDS_PER_DAY) };
    }
}

// ---------------------------------------------------------------------------
// Tests — Wave 2 Item 7 (G14, G8) boot-time constants
// ---------------------------------------------------------------------------

#[cfg(test)]
mod boot_constants_tests {
    use super::*;

    #[test]
    fn test_boot_deadline_constant_is_60s() {
        const { assert!(BOOT_DEADLINE_SECS == 60) };
    }

    #[test]
    fn test_escalation_thresholds_match_pinned_table() {
        // The wave-2-error-codes.md and Item 7.2 spec pin this exact table:
        // +5s DEBUG, +10s INFO, +20s WARN, +30s ERROR (BOOT-01),
        // +60s CRITICAL+HALT (BOOT-02). The constants below MUST match;
        // any drift fails this ratchet and the rule cross-ref test.
        const { assert!(BOOT_ESCALATION_DEBUG_AT_SECS == 5) };
        const { assert!(BOOT_ESCALATION_INFO_AT_SECS == 10) };
        const { assert!(BOOT_ESCALATION_WARN_AT_SECS == 20) };
        const { assert!(BOOT_ESCALATION_ERROR_AT_SECS == 30) };
        const { assert!(BOOT_DEADLINE_SECS == 60) };
    }

    #[test]
    fn test_escalation_thresholds_strictly_monotonic() {
        const { assert!(BOOT_ESCALATION_DEBUG_AT_SECS < BOOT_ESCALATION_INFO_AT_SECS) };
        const { assert!(BOOT_ESCALATION_INFO_AT_SECS < BOOT_ESCALATION_WARN_AT_SECS) };
        const { assert!(BOOT_ESCALATION_WARN_AT_SECS < BOOT_ESCALATION_ERROR_AT_SECS) };
        const { assert!(BOOT_ESCALATION_ERROR_AT_SECS < BOOT_DEADLINE_SECS) };
    }

    #[test]
    fn test_clock_skew_threshold_constant_is_2s() {
        // Ratchet for Wave 2 Item 7.3 (G8). 2s is the hard upper bound
        // that keeps IST midnight + 09:00:00 + 15:30:00 gates from
        // silently slipping by one trading day or one market session.
        // Any change to this constant requires re-evaluation of the IST
        // boundary math and operator approval — see
        // .claude/rules/project/wave-2-c-error-codes.md (BOOT-03).
        let lo = CLOCK_SKEW_HALT_THRESHOLD_SECS - 2.0_f64;
        let hi = CLOCK_SKEW_HALT_THRESHOLD_SECS + 2.0_f64;
        assert!((-f64::EPSILON..=f64::EPSILON).contains(&lo));
        assert!((4.0 - f64::EPSILON..=4.0 + f64::EPSILON).contains(&hi));
    }

    #[test]
    fn test_clock_skew_threshold_is_strictly_positive() {
        const { assert!(CLOCK_SKEW_HALT_THRESHOLD_SECS > 0.0) };
    }
}

// ---------------------------------------------------------------------------
// Tests — Protocol & Constant Verification
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::assertions_on_constants)] // APPROVED: S4 — schema-stability tests intentionally assert on compile-time constants as a regression guard against silent protocol-value drift. Converting to `const { assert!(..) }` blocks compiles away the check message which defeats the guard purpose.
mod tests {
    use super::*;

    #[test]
    fn test_ist_offset_seconds_is_5h30m() {
        assert_eq!(IST_UTC_OFFSET_SECONDS, 19_800);
        assert_eq!(IST_UTC_OFFSET_SECONDS_I64, 19_800);
    }

    #[test]
    fn test_ist_offset_nanos_consistent_with_seconds() {
        let expected_nanos = IST_UTC_OFFSET_SECONDS_I64 * 1_000_000_000;
        assert_eq!(IST_UTC_OFFSET_NANOS, expected_nanos);
    }

    #[test]
    fn test_sebi_max_orders_per_second() {
        assert_eq!(SEBI_MAX_ORDERS_PER_SECOND, 10);
    }

    #[test]
    fn test_sebi_audit_retention_years() {
        assert_eq!(SEBI_AUDIT_RETENTION_YEARS, 5);
    }

    #[test]
    fn test_ticker_packet_size() {
        assert_eq!(TICKER_PACKET_SIZE, 16);
    }

    #[test]
    fn test_quote_packet_size() {
        assert_eq!(QUOTE_PACKET_SIZE, 50);
    }

    #[test]
    fn test_full_packet_size() {
        assert_eq!(FULL_QUOTE_PACKET_SIZE, 162);
    }

    #[test]
    fn test_oi_packet_size() {
        assert_eq!(OI_PACKET_SIZE, 12);
    }

    #[test]
    fn test_disconnect_packet_size() {
        assert_eq!(DISCONNECT_PACKET_SIZE, 10);
    }

    #[test]
    fn test_exchange_segment_code_6_is_unused() {
        assert_ne!(EXCHANGE_SEGMENT_IDX_I, 6);
        assert_ne!(EXCHANGE_SEGMENT_NSE_EQ, 6);
        assert_ne!(EXCHANGE_SEGMENT_NSE_FNO, 6);
        assert_ne!(EXCHANGE_SEGMENT_NSE_CURRENCY, 6);
        assert_ne!(EXCHANGE_SEGMENT_BSE_EQ, 6);
        assert_ne!(EXCHANGE_SEGMENT_MCX_COMM, 6);
        assert_ne!(EXCHANGE_SEGMENT_BSE_CURRENCY, 6);
        assert_ne!(EXCHANGE_SEGMENT_BSE_FNO, 6);
    }

    #[test]
    fn test_exchange_segment_sequential_except_gap() {
        assert_eq!(EXCHANGE_SEGMENT_IDX_I, 0);
        assert_eq!(EXCHANGE_SEGMENT_NSE_EQ, 1);
        assert_eq!(EXCHANGE_SEGMENT_NSE_FNO, 2);
        assert_eq!(EXCHANGE_SEGMENT_NSE_CURRENCY, 3);
        assert_eq!(EXCHANGE_SEGMENT_BSE_EQ, 4);
        assert_eq!(EXCHANGE_SEGMENT_MCX_COMM, 5);
        assert_eq!(EXCHANGE_SEGMENT_BSE_CURRENCY, 7);
        assert_eq!(EXCHANGE_SEGMENT_BSE_FNO, 8);
    }

    #[test]
    fn test_data_api_error_codes_match_annexure_section_11() {
        // All 12 codes from docs/dhan-ref/08-annexure-enums.md Section 11
        assert_eq!(DATA_API_INTERNAL_SERVER_ERROR, 800);
        assert_eq!(DATA_API_INSTRUMENTS_EXCEED_LIMIT, 804);
        assert_eq!(DATA_API_EXCEEDED_ACTIVE_CONNECTIONS, 805);
        assert_eq!(DATA_API_NOT_SUBSCRIBED, 806);
        assert_eq!(DATA_API_ACCESS_TOKEN_EXPIRED, 807);
        assert_eq!(DATA_API_AUTHENTICATION_FAILED, 808);
        assert_eq!(DATA_API_ACCESS_TOKEN_INVALID, 809);
        assert_eq!(DATA_API_CLIENT_ID_INVALID, 810);
        assert_eq!(DATA_API_INVALID_EXPIRY_DATE, 811);
        assert_eq!(DATA_API_INVALID_DATE_FORMAT, 812);
        assert_eq!(DATA_API_INVALID_SECURITY_ID, 813);
        assert_eq!(DATA_API_INVALID_REQUEST, 814);
    }

    #[test]
    fn test_disconnect_legacy_aliases_match_primary_constants() {
        assert_eq!(DISCONNECT_EXCEEDED_ACTIVE_CONNECTIONS, 805);
        assert_eq!(DISCONNECT_DATA_API_SUBSCRIPTION_REQUIRED, 806);
        assert_eq!(DISCONNECT_ACCESS_TOKEN_EXPIRED, 807);
        assert_eq!(DISCONNECT_AUTHENTICATION_FAILED, 808);
        assert_eq!(DISCONNECT_ACCESS_TOKEN_INVALID, 809);
    }

    #[test]
    fn test_response_code_disconnect_is_50() {
        assert_eq!(RESPONSE_CODE_DISCONNECT, 50);
    }

    #[test]
    fn test_max_connections_is_5() {
        assert_eq!(MAX_WEBSOCKET_CONNECTIONS, 5);
    }

    #[test]
    fn test_max_instruments_per_connection_is_5000() {
        assert_eq!(MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION, 5000);
    }

    #[test]
    fn test_subscription_batch_size_within_spec() {
        let batch = SUBSCRIPTION_BATCH_SIZE;
        assert!(batch <= 100);
    }

    #[test]
    fn test_total_subscriptions_consistent() {
        assert_eq!(
            MAX_TOTAL_SUBSCRIPTIONS,
            MAX_WEBSOCKET_CONNECTIONS * MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION
        );
    }

    #[test]
    fn test_deep_depth_header_is_12_not_8() {
        assert_eq!(DEEP_DEPTH_HEADER_SIZE, 12);
        assert_eq!(BINARY_HEADER_SIZE, 8);
        assert_ne!(DEEP_DEPTH_HEADER_SIZE, BINARY_HEADER_SIZE);
    }

    #[test]
    fn test_deep_depth_bid_ask_codes() {
        assert_eq!(DEEP_DEPTH_FEED_CODE_BID, 41);
        assert_eq!(DEEP_DEPTH_FEED_CODE_ASK, 51);
    }

    #[test]
    fn test_market_open_time() {
        assert_eq!(MARKET_OPEN_TIME_IST, "09:15:00");
    }

    #[test]
    fn test_order_update_login_msg_code_is_42() {
        assert_eq!(ORDER_UPDATE_LOGIN_MSG_CODE, 42);
    }

    #[test]
    fn test_order_update_auth_timeout_is_reasonable() {
        const { assert!(ORDER_UPDATE_AUTH_TIMEOUT_SECS >= 5) };
        const { assert!(ORDER_UPDATE_AUTH_TIMEOUT_SECS <= 30) };
    }

    #[test]
    fn test_indicator_warmup_ticks_positive() {
        const { assert!(MAX_INDICATOR_WARMUP_TICKS > 0) };
    }

    #[test]
    fn test_indicator_ring_buffer_capacity_power_of_two() {
        assert!(INDICATOR_RING_BUFFER_CAPACITY.is_power_of_two());
    }

    // --- Greeks Constants ---

    #[test]
    fn test_risk_free_rate_reasonable() {
        // India T-Bill rate should be between 3% and 12%
        assert!(RISK_FREE_RATE > 0.03 && RISK_FREE_RATE < 0.12);
    }

    #[test]
    fn test_dividend_yield_reasonable() {
        // NIFTY dividend yield between 0.5% and 3%
        assert!(DIVIDEND_YIELD > 0.005 && DIVIDEND_YIELD < 0.03);
    }

    #[test]
    fn test_iv_solver_max_iterations_positive() {
        assert!(IV_SOLVER_MAX_ITERATIONS > 0);
        assert!(IV_SOLVER_MAX_ITERATIONS <= 200);
    }

    #[test]
    fn test_iv_solver_tolerance_small_positive() {
        assert!(IV_SOLVER_TOLERANCE > 0.0);
        assert!(IV_SOLVER_TOLERANCE < 1e-4);
    }

    #[test]
    fn test_iv_bounds_valid_range() {
        assert!(IV_MIN_BOUND > 0.0);
        assert!(IV_MAX_BOUND > IV_MIN_BOUND);
        assert!(IV_MAX_BOUND <= 10.0);
    }

    // --- Display Index Constants ---

    #[test]
    fn test_display_index_entries_count_matches_constant() {
        assert_eq!(DISPLAY_INDEX_ENTRIES.len(), DISPLAY_INDEX_COUNT);
    }

    #[test]
    fn test_display_index_entries_have_nonzero_security_ids() {
        for &(name, security_id, _subcategory) in DISPLAY_INDEX_ENTRIES {
            assert!(
                security_id > 0,
                "Display index '{}' has zero security_id",
                name
            );
        }
    }

    #[test]
    fn test_display_index_entries_have_valid_subcategories() {
        let valid_subcategories = [
            "Volatility",
            "BroadMarket",
            "MidCap",
            "SmallCap",
            "Sectoral",
            "Thematic",
        ];
        for &(name, _, subcategory) in DISPLAY_INDEX_ENTRIES {
            assert!(
                valid_subcategories.contains(&subcategory),
                "Display index '{}' has invalid subcategory '{}'",
                name,
                subcategory
            );
        }
    }

    #[test]
    fn test_display_index_entries_unique_security_ids() {
        let mut seen = std::collections::HashSet::new();
        for &(name, security_id, _) in DISPLAY_INDEX_ENTRIES {
            assert!(
                seen.insert(security_id),
                "Duplicate security_id {} for index '{}'",
                security_id,
                name
            );
        }
    }

    // --- Full Chain Index Constants ---

    #[test]
    fn test_full_chain_index_count_matches_symbols() {
        assert_eq!(FULL_CHAIN_INDEX_SYMBOLS.len(), FULL_CHAIN_INDEX_COUNT);
    }

    #[test]
    fn test_full_chain_index_symbols_nonempty() {
        for &sym in FULL_CHAIN_INDEX_SYMBOLS {
            assert!(!sym.is_empty(), "Full chain index symbol is empty");
        }
    }

    /// 2026-04-25 ratchet: full-chain set is exactly 3 indices (NIFTY, BANKNIFTY,
    /// SENSEX). FINNIFTY + MIDCPNIFTY were dropped to free 25K WS capacity for
    /// stock F&O ATM±25 coverage. Regressing this resurrects the 40K-contract
    /// over-subscription bug.
    #[test]
    fn test_full_chain_index_symbols_is_exactly_three() {
        assert_eq!(FULL_CHAIN_INDEX_SYMBOLS.len(), 3);
        assert_eq!(FULL_CHAIN_INDEX_COUNT, 3);
        assert!(FULL_CHAIN_INDEX_SYMBOLS.contains(&"NIFTY"));
        assert!(FULL_CHAIN_INDEX_SYMBOLS.contains(&"BANKNIFTY"));
        assert!(FULL_CHAIN_INDEX_SYMBOLS.contains(&"SENSEX"));
        assert!(!FULL_CHAIN_INDEX_SYMBOLS.contains(&"FINNIFTY"));
        assert!(!FULL_CHAIN_INDEX_SYMBOLS.contains(&"MIDCPNIFTY"));
    }

    /// 2026-04-25 ratchet: stock option ATM±N cap is 25 strikes per side.
    /// NSE intraday circuit is 20% — ±25 strikes mathematically covers any move.
    #[test]
    fn test_stock_option_atm_strikes_each_side_is_25() {
        assert_eq!(STOCK_OPTION_ATM_STRIKES_EACH_SIDE, 25);
    }

    /// 2026-04-25 ratchet: capacity warn threshold is below the hard cap and
    /// above the verified live total of 24,324.
    #[test]
    fn test_max_total_subscriptions_target_below_hard_limit() {
        assert!(MAX_TOTAL_SUBSCRIPTIONS_TARGET < MAX_TOTAL_SUBSCRIPTIONS);
        assert!(MAX_TOTAL_SUBSCRIPTIONS_TARGET >= 24_324);
        assert_eq!(MAX_TOTAL_SUBSCRIPTIONS, 25_000);
    }

    // --- Validation Constants ---

    #[test]
    fn test_validation_must_exist_indices_nonempty() {
        assert!(!VALIDATION_MUST_EXIST_INDICES.is_empty());
        for &(sym, sid) in VALIDATION_MUST_EXIST_INDICES {
            assert!(!sym.is_empty());
            assert!(sid > 0);
        }
    }

    #[test]
    fn test_validation_fno_stock_min_less_than_max() {
        assert!(VALIDATION_FNO_STOCK_MIN_COUNT < VALIDATION_FNO_STOCK_MAX_COUNT);
    }

    // --- Computed Packet Size Constants ---

    #[test]
    fn test_twenty_depth_packet_size_computed_correctly() {
        assert_eq!(
            TWENTY_DEPTH_PACKET_SIZE,
            DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE
        );
    }

    #[test]
    fn test_two_hundred_depth_packet_size_computed_correctly() {
        assert_eq!(
            TWO_HUNDRED_DEPTH_PACKET_SIZE,
            DEEP_DEPTH_HEADER_SIZE + TWO_HUNDRED_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE
        );
    }

    #[test]
    fn test_twenty_depth_body_size() {
        assert_eq!(TWENTY_DEPTH_BODY_SIZE, 20 * 16);
        assert_eq!(TWENTY_DEPTH_BODY_SIZE, 320);
    }

    #[test]
    fn test_two_hundred_depth_body_size() {
        assert_eq!(TWO_HUNDRED_DEPTH_BODY_SIZE, 200 * 16);
        assert_eq!(TWO_HUNDRED_DEPTH_BODY_SIZE, 3200);
    }

    // --- Candle Constants ---

    #[test]
    fn test_candles_per_trading_day() {
        assert_eq!(CANDLES_PER_TRADING_DAY, 375);
    }

    #[test]
    fn test_intraday_timeframes_count() {
        assert_eq!(INTRADAY_TIMEFRAMES.len(), 5);
    }

    #[test]
    fn test_intraday_timeframes_valid_intervals() {
        for &(interval, _label) in INTRADAY_TIMEFRAMES {
            assert!(
                ["1", "5", "15", "25", "60"].contains(&interval),
                "Invalid intraday interval: '{}'",
                interval
            );
        }
    }

    // --- SSM Path Constants ---

    #[test]
    fn test_ssm_paths_nonempty() {
        assert!(!SSM_SECRET_BASE_PATH.is_empty());
        assert!(!SSM_DHAN_SERVICE.is_empty());
        assert!(!DHAN_CLIENT_ID_SECRET.is_empty());
        assert!(!DHAN_CLIENT_SECRET_SECRET.is_empty());
        assert!(!DHAN_TOTP_SECRET.is_empty());
    }

    // --- Market Status Codes ---

    #[test]
    fn test_market_status_codes_distinct() {
        let codes = [
            MARKET_STATUS_CLOSED,
            MARKET_STATUS_PRE_OPEN,
            MARKET_STATUS_OPEN,
            MARKET_STATUS_POST_CLOSE,
        ];
        let mut unique = std::collections::HashSet::new();
        for code in codes {
            assert!(
                unique.insert(code),
                "Duplicate market status code: {}",
                code
            );
        }
    }

    // --- Feed Request Codes ---

    #[test]
    fn test_feed_request_codes_correct() {
        assert_eq!(FEED_REQUEST_CONNECT, 11);
        assert_eq!(FEED_REQUEST_DISCONNECT, 12);
        assert_eq!(FEED_REQUEST_TICKER, 15);
        assert_eq!(FEED_UNSUBSCRIBE_TICKER, 16);
        assert_eq!(FEED_REQUEST_QUOTE, 17);
        assert_eq!(FEED_UNSUBSCRIBE_QUOTE, 18);
        assert_eq!(FEED_REQUEST_FULL, 21);
        assert_eq!(FEED_UNSUBSCRIBE_FULL, 22);
    }

    // --- Response Codes ---

    #[test]
    fn test_response_codes_correct() {
        assert_eq!(RESPONSE_CODE_INDEX_TICKER, 1);
        assert_eq!(RESPONSE_CODE_TICKER, 2);
        assert_eq!(RESPONSE_CODE_MARKET_DEPTH, 3);
        assert_eq!(RESPONSE_CODE_QUOTE, 4);
        assert_eq!(RESPONSE_CODE_OI, 5);
        assert_eq!(RESPONSE_CODE_PREVIOUS_CLOSE, 6);
        assert_eq!(RESPONSE_CODE_MARKET_STATUS, 7);
        assert_eq!(RESPONSE_CODE_FULL, 8);
    }

    // --- TOTP Constants ---

    #[test]
    fn test_totp_constants_valid() {
        assert_eq!(TOTP_DIGITS, 6);
        assert_eq!(TOTP_PERIOD_SECS, 30);
        assert!(TOTP_MAX_RETRIES >= 1);
    }

    // --- Network Constants ---

    #[test]
    fn test_public_ip_check_urls_are_https() {
        assert!(PUBLIC_IP_CHECK_PRIMARY_URL.starts_with("https://"));
        assert!(PUBLIC_IP_CHECK_FALLBACK_URL.starts_with("https://"));
    }

    #[test]
    fn test_static_ip_boot_retry_constants_pinned() {
        // Phase 0 Item 18b — operator-locked 30 attempts x 60 s.
        // Worst-case boot blocking = 30 min, bounded by these constants.
        // Any future change MUST come with explicit operator approval
        // because EventBridge starts the AWS instance at 8:30 IST and a
        // boot blocked beyond 30 min would miss the 09:00 IST market
        // open.
        assert_eq!(STATIC_IP_BOOT_RETRY_MAX_ATTEMPTS, 30);
        assert_eq!(STATIC_IP_BOOT_RETRY_INTERVAL_SECS, 60);
    }

    #[test]
    fn test_static_ip_boot_retry_worst_case_under_30_min() {
        // Sanity bound — if both constants are bumped beyond reason
        // (e.g. someone makes attempts u32::MAX) this catches it as a
        // unit-test failure rather than at boot time.
        let worst_case_secs =
            u64::from(STATIC_IP_BOOT_RETRY_MAX_ATTEMPTS) * STATIC_IP_BOOT_RETRY_INTERVAL_SECS;
        // 30 min = 1800 s. Use <= to allow exact value, < would be off-by-one.
        assert!(
            worst_case_secs <= 1800,
            "boot retry budget must stay <= 30 min, got {worst_case_secs} s"
        );
    }

    // --- Ring Buffer Constants ---

    #[test]
    fn test_tick_ring_buffer_is_power_of_two() {
        assert!(TICK_RING_BUFFER_CAPACITY.is_power_of_two());
        assert_eq!(TICK_RING_BUFFER_CAPACITY, 65536);
    }

    #[test]
    fn test_order_event_ring_buffer_is_power_of_two() {
        assert!(ORDER_EVENT_RING_BUFFER_CAPACITY.is_power_of_two());
    }

    // --- QuestDB Table Names ---

    #[test]
    fn test_questdb_table_names_nonempty() {
        let tables = [
            QUESTDB_TABLE_BUILD_METADATA,
            QUESTDB_TABLE_FNO_UNDERLYINGS,
            QUESTDB_TABLE_DERIVATIVE_CONTRACTS,
            QUESTDB_TABLE_SUBSCRIBED_INDICES,
            QUESTDB_TABLE_NSE_HOLIDAYS,
            QUESTDB_TABLE_INDEX_CONSTITUENTS,
            QUESTDB_TABLE_TICKS,
            QUESTDB_TABLE_MARKET_DEPTH,
            QUESTDB_TABLE_PREVIOUS_CLOSE,
        ];
        for table in tables {
            assert!(!table.is_empty(), "QuestDB table name is empty");
        }
    }

    // --- Depth Level Offsets ---

    #[test]
    fn test_depth_level_offsets_within_bounds() {
        assert!(DEPTH_LEVEL_OFFSET_BID_QTY < MARKET_DEPTH_LEVEL_SIZE);
        assert!(DEPTH_LEVEL_OFFSET_ASK_QTY < MARKET_DEPTH_LEVEL_SIZE);
        assert!(DEPTH_LEVEL_OFFSET_BID_ORDERS < MARKET_DEPTH_LEVEL_SIZE);
        assert!(DEPTH_LEVEL_OFFSET_ASK_ORDERS < MARKET_DEPTH_LEVEL_SIZE);
        assert!(DEPTH_LEVEL_OFFSET_BID_PRICE < MARKET_DEPTH_LEVEL_SIZE);
        assert!(DEPTH_LEVEL_OFFSET_ASK_PRICE < MARKET_DEPTH_LEVEL_SIZE);
    }

    // --- Deep Depth Header Offsets ---

    #[test]
    fn test_deep_depth_header_offsets_within_bounds() {
        assert!(DEEP_DEPTH_HEADER_OFFSET_MSG_LENGTH < DEEP_DEPTH_HEADER_SIZE);
        assert!(DEEP_DEPTH_HEADER_OFFSET_FEED_CODE < DEEP_DEPTH_HEADER_SIZE);
        assert!(DEEP_DEPTH_HEADER_OFFSET_EXCHANGE_SEGMENT < DEEP_DEPTH_HEADER_SIZE);
        assert!(DEEP_DEPTH_HEADER_OFFSET_SECURITY_ID < DEEP_DEPTH_HEADER_SIZE);
        assert!(DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE < DEEP_DEPTH_HEADER_SIZE);
    }

    // --- Packet Header Offsets ---

    #[test]
    fn test_header_offsets_within_bounds() {
        assert!(HEADER_OFFSET_RESPONSE_CODE < BINARY_HEADER_SIZE);
        assert!(HEADER_OFFSET_MESSAGE_LENGTH < BINARY_HEADER_SIZE);
        assert!(HEADER_OFFSET_EXCHANGE_SEGMENT < BINARY_HEADER_SIZE);
        assert!(HEADER_OFFSET_SECURITY_ID < BINARY_HEADER_SIZE);
    }

    // --- API Endpoint Paths ---

    #[test]
    fn test_api_paths_start_with_slash() {
        let paths = [
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
        ];
        for path in paths {
            assert!(
                path.starts_with('/'),
                "API path does not start with '/': '{}'",
                path
            );
        }
    }

    // --- Unsubscribe code for depth ---

    #[test]
    fn test_depth_unsubscribe_code_is_25() {
        assert_eq!(FEED_UNSUBSCRIBE_TWENTY_DEPTH, 25);
    }

    // --- Application constants ---

    #[test]
    fn test_application_name() {
        assert_eq!(APPLICATION_NAME, "tickvault");
    }

    #[test]
    fn test_application_version_nonempty() {
        assert!(!APPLICATION_VERSION.is_empty());
    }

    // -----------------------------------------------------------------------
    // Phase 0 Item 11 — Dual-gate market-hours ratchets (operator-locked 2026-05-13)
    // -----------------------------------------------------------------------

    /// Constant pin — MARKET_OPEN_IST_NANOS = 09:15:00.000 IST.
    #[test]
    fn test_market_open_ist_nanos_pinned_at_0915() {
        // 9h * 3600s/h + 15m * 60s/m = 33_300 secs = 33_300 * 1e9 nanos.
        assert_eq!(MARKET_OPEN_IST_NANOS, 33_300_000_000_000);
    }

    /// Constant pin — MARKET_CLOSE_IST_NANOS = 15:30:00.000 IST (exclusive).
    #[test]
    fn test_market_close_ist_nanos_pinned_at_1530_exclusive() {
        // 15h * 3600 + 30m * 60 = 55_800 secs.
        assert_eq!(MARKET_CLOSE_IST_NANOS, 55_800_000_000_000);
    }

    /// Spot 1m REST pipeline (operator grant 2026-07-12) — the index set
    /// is pinned to NIFTY=13, BANKNIFTY=25, SENSEX=51 + INDIA VIX=21
    /// (operator scope addition 2026-07-13, relayed via the coordinator
    /// session: INDIA VIX joins the spot 1m pull, spot only, no option
    /// chain), and the fire window is [09:16:00, 15:30:00] IST inclusive.
    #[test]
    fn test_spot_1m_rest_constants_pinned() {
        assert_eq!(
            SPOT_1M_REST_INDICES,
            [
                (13, "NIFTY"),
                (25, "BANKNIFTY"),
                (51, "SENSEX"),
                (21, "INDIA VIX")
            ]
        );
        assert_eq!(SPOT_1M_REST_INDICES[3].0, INDIA_VIX_SECURITY_ID);
        // The chain leg stays the VIX-free 3-underlying §8 scope.
        assert_eq!(
            CHAIN_1M_UNDERLYINGS,
            [(13, "NIFTY"), (25, "BANKNIFTY"), (51, "SENSEX")]
        );
        assert!(
            CHAIN_1M_UNDERLYINGS
                .iter()
                .all(|&(sid, _)| sid != INDIA_VIX_SECURITY_ID),
            "INDIA VIX is SPOT-ONLY — never a chain underlying"
        );
        // Per-SID not-served detector threshold (~10 minutes).
        assert_eq!(SPOT_1M_REST_SID_NOT_SERVED_THRESHOLD, 10);
        assert_eq!(SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST, 33_360); // 09:16:00
        assert_eq!(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST, 55_800); // 15:30:00
        // Both boundaries are exact minute marks.
        assert_eq!(SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST % 60, 0);
        assert_eq!(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST % 60, 0);
        assert_eq!(SPOT_1M_REST_FIRE_DELAY_MS, 300);
        assert_eq!(SPOT_1M_REST_RETRY_OFFSETS_MS, [700, 1_500, 3_000, 6_000]);
        assert!(
            SPOT_1M_REST_RETRY_OFFSETS_MS
                .windows(2)
                .all(|w| w[0] < w[1]),
            "re-poll ladder must be strictly increasing"
        );
        assert_eq!(SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD, 3);
        assert!(u64::from(SPOT_1M_REST_FIRE_STALE_GRACE_SECS) * 1_000 < 60_000);
        // 2026-07-12 H2 fix: the REAL minute budget — short per-request
        // timeout + a hard per-SID ladder budget that fits the minute.
        assert_eq!(SPOT_1M_REST_REQUEST_TIMEOUT_SECS, 5);
        assert_eq!(SPOT_1M_REST_SID_BUDGET_SECS, 20);
        assert!(
            SPOT_1M_REST_FIRE_DELAY_MS + SPOT_1M_REST_SID_BUDGET_SECS * 1_000 < 60_000,
            "budget must finish inside the minute"
        );
        assert!(
            SPOT_1M_REST_RETRY_OFFSETS_MS[3] + SPOT_1M_REST_REQUEST_TIMEOUT_SECS * 1_000
                < SPOT_1M_REST_SID_BUDGET_SECS * 1_000,
            "ladder schedule must fit the budget"
        );
        // 429-coordination follow-up (2026-07-13): deterministic per-SID
        // jitter + bounded 429 extra backoff, worst case still inside the
        // hard 20 s per-SID budget (6 s + 0.45 s + 8 s + 5 s = 19.45 s at
        // the 4-SID arity).
        assert_eq!(SPOT_1M_REST_LADDER_JITTER_STEP_MS, 150);
        assert_eq!(SPOT_1M_REST_LADDER_JITTER_SLOTS, 4);
        assert_eq!(
            SPOT_1M_REST_LADDER_JITTER_SLOTS as usize,
            SPOT_1M_REST_INDICES.len(),
            "jitter slots must equal the pinned index arity"
        );
        assert_eq!(SPOT_1M_REST_429_EXTRA_BACKOFF_MS, 2_000);
        assert!(
            SPOT_1M_REST_RETRY_OFFSETS_MS[3]
                + (SPOT_1M_REST_LADDER_JITTER_SLOTS - 1) * SPOT_1M_REST_LADDER_JITTER_STEP_MS
                + SPOT_1M_REST_RETRY_OFFSETS_MS.len() as u64 * SPOT_1M_REST_429_EXTRA_BACKOFF_MS
                + SPOT_1M_REST_REQUEST_TIMEOUT_SECS * 1_000
                < SPOT_1M_REST_SID_BUDGET_SECS * 1_000,
            "worst-case jittered + 429-backed-off schedule must fit the budget"
        );
        assert_eq!(SPOT_1M_REST_MAX_BODY_BYTES, 2 * 1024 * 1024);
    }

    /// Option-chain 1m REST pipeline (operator grant 2026-07-12, PR-3) —
    /// the endpoint paths + the chain leg's bounded timing envelope.
    #[test]
    fn test_chain_1m_constants_pinned() {
        assert_eq!(DHAN_OPTION_CHAIN_PATH, "/optionchain");
        assert_eq!(DHAN_OPTION_CHAIN_EXPIRYLIST_PATH, "/optionchain/expirylist");
        assert!(DHAN_OPTION_CHAIN_PATH.starts_with('/'));
        assert!(DHAN_OPTION_CHAIN_EXPIRYLIST_PATH.starts_with('/'));
        // The chain fires AFTER the spot leg; its fallback timer trails the
        // spot fire delay and the whole fire fits inside the minute.
        assert_eq!(CHAIN_1M_FALLBACK_DELAY_MS, 2_500);
        assert!(CHAIN_1M_FALLBACK_DELAY_MS > SPOT_1M_REST_FIRE_DELAY_MS);
        assert_eq!(CHAIN_1M_REQUEST_TIMEOUT_SECS, 10);
        assert_eq!(CHAIN_1M_UNDERLYING_BUDGET_SECS, 20);
        assert!(CHAIN_1M_FALLBACK_DELAY_MS + CHAIN_1M_UNDERLYING_BUDGET_SECS * 1_000 < 60_000);
        assert!(CHAIN_1M_REQUEST_TIMEOUT_SECS < CHAIN_1M_UNDERLYING_BUDGET_SECS);
        // Dhan's documented option-chain limit: 1 unique request / 3s.
        assert_eq!(CHAIN_1M_MIN_GAP_SECS, 3);
        assert_eq!(CHAIN_1M_CONSECUTIVE_FAIL_PAGE_THRESHOLD, 3);
        assert_eq!(CHAIN_1M_EXPIRYLIST_RETRY_BACKOFF_SECS, [3, 6]);
        // Each expirylist retry backoff clears the 1-unique-per-3s window
        // (retrying the SAME request inside it earns the reject it retries).
        assert!(CHAIN_1M_EXPIRYLIST_RETRY_BACKOFF_SECS[0] >= CHAIN_1M_MIN_GAP_SECS);
        assert_eq!(CHAIN_1M_MAX_BODY_BYTES, 8 * 1024 * 1024);
    }

    /// Groww spot 1m REST leg (operator grant 2026-07-13, PR-2 of the
    /// Groww per-minute REST plan) — endpoint identity, the 3-symbol table,
    /// the SEQUENTIAL-fetch timing envelope and the token re-read floor.
    #[test]
    fn test_groww_spot_1m_constants_pinned() {
        assert_eq!(
            GROWW_HISTORICAL_CANDLES_URL,
            "https://api.groww.in/v1/historical/candles"
        );
        assert!(GROWW_HISTORICAL_CANDLES_URL.starts_with("https://"));
        // The Groww interval literal — never the Dhan-style "1".
        assert_eq!(GROWW_CANDLE_INTERVAL_1MIN, "1minute");
        assert_eq!(GROWW_API_VERSION_HEADER, "x-api-version");
        assert_eq!(GROWW_API_VERSION_VALUE, "1.0");
        // The 3-CORE-symbol table mirrors the Dhan set in Groww identity
        // space: groww_symbol (NOT token / bare trading symbol), segment
        // CASH. 2026-07-13 operator scope (relayed): the 4th Groww spot
        // index — INDIA VIX — is RUNTIME-resolved from the day's master
        // (never a guessed const literal), so it is deliberately NOT here.
        assert_eq!(
            GROWW_SPOT_1M_SYMBOLS,
            [
                ("NSE-NIFTY", "NIFTY", "NSE", "CASH"),
                ("NSE-BANKNIFTY", "BANKNIFTY", "NSE", "CASH"),
                ("BSE-SENSEX", "SENSEX", "BSE", "CASH"),
            ]
        );
        // The Groww CONST set stays the 3 core indices; the Dhan set is 4
        // (VIX = the fixed 4th entry) and the Groww 4th target is the
        // runtime-resolved VIX (2026-07-13 scope, both legs SPOT ONLY).
        assert_eq!(GROWW_SPOT_1M_SYMBOLS.len(), 3);
        assert_eq!(GROWW_SPOT_1M_SYMBOLS.len(), CHAIN_1M_UNDERLYINGS.len());
        assert_eq!(SPOT_1M_REST_INDICES.len(), 4);
        // The canonical VIX human symbol (2026-07-13 scope addition) — the
        // NSE_INDEX_ALLOWLIST / PHASE_0_IDX_I_SYMBOLS literal, and NEVER a
        // groww_symbol (the Groww identity is runtime-resolved).
        assert_eq!(GROWW_SPOT_1M_VIX_SYMBOL, "INDIA VIX");
        assert!(PHASE_0_IDX_I_SYMBOLS.contains(&GROWW_SPOT_1M_VIX_SYMBOL));
        assert!(!GROWW_SPOT_1M_VIX_SYMBOL.contains('-'));
        // Timing envelope mirrors the Dhan leg, tightened for the
        // SEQUENTIAL 4-target fire (3 core + the runtime VIX target;
        // pacing rule: ≤1 in-flight request). Budget re-derived 18 → 14 on
        // 2026-07-13 so 4 sequential budgets + the fire delay still finish
        // inside the minute.
        assert_eq!(GROWW_SPOT_1M_FIRE_DELAY_MS, 300);
        assert_eq!(GROWW_SPOT_1M_RETRY_OFFSETS_MS, [700, 1_500, 3_000, 6_000]);
        assert!(
            GROWW_SPOT_1M_RETRY_OFFSETS_MS
                .windows(2)
                .all(|w| w[0] < w[1]),
            "offsets strictly increasing"
        );
        assert_eq!(GROWW_SPOT_1M_REQUEST_TIMEOUT_SECS, 5);
        assert_eq!(GROWW_SPOT_1M_SYMBOL_BUDGET_SECS, 14);
        assert!(
            GROWW_SPOT_1M_FIRE_DELAY_MS
                + (GROWW_SPOT_1M_SYMBOLS.len() as u64 + 1)
                    * GROWW_SPOT_1M_SYMBOL_BUDGET_SECS
                    * 1_000
                < 60_000,
            "sequential fire (incl. the runtime VIX target) must finish inside the minute"
        );
        assert!(
            GROWW_SPOT_1M_RETRY_OFFSETS_MS[3] + GROWW_SPOT_1M_REQUEST_TIMEOUT_SECS * 1_000
                < GROWW_SPOT_1M_SYMBOL_BUDGET_SECS * 1_000,
            "ladder schedule must fit one symbol's budget"
        );
        assert_eq!(GROWW_SPOT_1M_MAX_BODY_BYTES, 2 * 1024 * 1024);
        // Token-minter lock pacing: re-READ from SSM at ≥60 s, never mint.
        assert_eq!(GROWW_SPOT_1M_TOKEN_REREAD_FLOOR_SECS, 60);
    }

    /// Constant pins — the Groww per-minute option-chain leg (PR-3 of the
    /// Groww per-minute REST plan; every value grounded in
    /// `docs/groww-ref/14-option-chain.md` + `15-rate-limits-and-capacity.md`
    /// or the mirrored Dhan chain leg).
    #[test]
    fn test_groww_chain_1m_constants_pinned() {
        // Documented + SDK-verified endpoint prefix; the token travels in
        // the Authorization header, NEVER the URL.
        assert_eq!(
            GROWW_OPTION_CHAIN_URL_PREFIX,
            "https://api.groww.in/v1/option-chain"
        );
        assert!(GROWW_OPTION_CHAIN_URL_PREFIX.starts_with("https://"));
        assert!(!GROWW_OPTION_CHAIN_URL_PREFIX.contains("token"));
        // The 3-underlying table: PLAIN symbol + exchange for the path
        // params, groww_symbol for the stable live-lane id.
        assert_eq!(
            GROWW_CHAIN_1M_UNDERLYINGS,
            [
                ("NIFTY", "NSE", "NSE-NIFTY"),
                ("BANKNIFTY", "NSE", "NSE-BANKNIFTY"),
                ("SENSEX", "BSE", "BSE-SENSEX"),
            ]
        );
        assert_eq!(
            GROWW_CHAIN_1M_UNDERLYINGS.len(),
            GROWW_SPOT_1M_SYMBOLS.len()
        );
        // Every chain groww_symbol matches its spot-leg twin (same stable
        // id space — the persisted rows must join the live lane).
        for ((_, _, chain_gs), (spot_gs, ..)) in GROWW_CHAIN_1M_UNDERLYINGS
            .iter()
            .zip(GROWW_SPOT_1M_SYMBOLS.iter())
        {
            assert_eq!(chain_gs, spot_gs, "chain/spot groww_symbol drift");
        }
        // Sequencing + pacing envelope (sequential 3-underlying fire).
        assert_eq!(GROWW_CHAIN_1M_FALLBACK_DELAY_MS, 2_500);
        assert!(GROWW_CHAIN_1M_FALLBACK_DELAY_MS > GROWW_SPOT_1M_FIRE_DELAY_MS);
        assert_eq!(GROWW_CHAIN_1M_MIN_GAP_MS, 1_000);
        assert_eq!(GROWW_CHAIN_1M_REQUEST_TIMEOUT_SECS, 10);
        assert_eq!(GROWW_CHAIN_1M_UNDERLYING_BUDGET_SECS, 15);
        assert!(
            GROWW_CHAIN_1M_FALLBACK_DELAY_MS
                + (GROWW_CHAIN_1M_UNDERLYINGS.len() as u64)
                    * GROWW_CHAIN_1M_UNDERLYING_BUDGET_SECS
                    * 1_000
                < 60_000,
            "sequential chain fire must finish inside the minute"
        );
        assert!(
            GROWW_CHAIN_1M_MIN_GAP_MS + GROWW_CHAIN_1M_REQUEST_TIMEOUT_SECS * 1_000
                < GROWW_CHAIN_1M_UNDERLYING_BUDGET_SECS * 1_000,
            "min-gap + one request timeout must fit the underlying budget"
        );
        assert_eq!(GROWW_CHAIN_1M_MAX_BODY_BYTES, 8 * 1024 * 1024);
        // The chain leg reuses the spot FailureEdge — thresholds agree.
        assert_eq!(
            GROWW_CHAIN_1M_CONSECUTIVE_FAIL_PAGE_THRESHOLD,
            SPOT_1M_REST_CONSECUTIVE_FAIL_PAGE_THRESHOLD
        );
        // Warmup master-download retries: bounded, 3 attempts total.
        assert_eq!(GROWW_CHAIN_1M_MASTER_RETRY_BACKOFF_SECS, [3, 6]);
    }

    /// PR-4 (Groww contract leg): the fill-model leg's pacing + envelope
    /// constants — the cap, the ATM-window default, and the boundary-burst
    /// math are all mechanical (the const asserts re-prove them at compile
    /// time; this test pins the VALUES so a drift is a conscious edit).
    #[test]
    fn test_groww_contract_1m_constants_pinned() {
        assert_eq!(GROWW_CONTRACT_1M_FALLBACK_DELAY_MS, 8_000);
        assert!(GROWW_CONTRACT_1M_FALLBACK_DELAY_MS > GROWW_CHAIN_1M_FALLBACK_DELAY_MS);
        assert_eq!(GROWW_CONTRACT_1M_MIN_GAP_MS, 500);
        assert_eq!(GROWW_CONTRACT_1M_REQUEST_TIMEOUT_SECS, 5);
        assert_eq!(GROWW_CONTRACT_1M_MAX_PER_MINUTE, 30);
        assert_eq!(GROWW_CONTRACT_1M_FIRE_BUDGET_SECS, 45);
        assert_eq!(GROWW_CONTRACT_1M_DEFAULT_STRIKES_EACH_SIDE, 2);
        assert_eq!(GROWW_CONTRACT_1M_ANCHOR_MAX_AGE_MINUTES, 5);
        // Fallback + fire budget fit the minute.
        assert!(
            GROWW_CONTRACT_1M_FALLBACK_DELAY_MS + GROWW_CONTRACT_1M_FIRE_BUDGET_SECS * 1_000
                < 60_000
        );
        // Worst-overlap boundary burst inside the 6 req/s family ceiling
        // (re-derived 2026-07-13 for the VIX 4th spot target): spot worst
        // second = 3 requests (target-transition instant) + chain + contract.
        // EXACT rational cross-multiplication:
        //   3 + 1000/kg + 1000/cg <= 6  <=>  1000*kg + 1000*cg <= 3*cg*kg
        // — today 1000*500 + 1000*1000 = 1.5M = 3*1000*500 exactly:
        // 3 + 1 + 2 = 6.00 req/s AT the self-imposed ceiling.
        assert!(
            1_000 * GROWW_CONTRACT_1M_MIN_GAP_MS + 1_000 * GROWW_CHAIN_1M_MIN_GAP_MS
                <= 3 * GROWW_CHAIN_1M_MIN_GAP_MS * GROWW_CONTRACT_1M_MIN_GAP_MS
        );
        // Per-minute request math (the capacity envelope): 30 contracts +
        // the spot leg's worst 20 (3 core + the runtime VIX target = 4
        // targets x 5 ladder rungs) + 3 chain = 53/min ~ 17.7% of the
        // 300/min shared budget (typical ~37/min).
        assert_eq!(
            GROWW_CONTRACT_1M_MAX_PER_MINUTE
                + (GROWW_SPOT_1M_SYMBOLS.len() + 1) * (GROWW_SPOT_1M_RETRY_OFFSETS_MS.len() + 1)
                + GROWW_CHAIN_1M_UNDERLYINGS.len(),
            53
        );
        // The default ATM window fills the cap exactly (5 strikes x 2 legs
        // x 3 underlyings = 30) — a wider default needs a dated quote.
        assert_eq!(
            (2 * GROWW_CONTRACT_1M_DEFAULT_STRIKES_EACH_SIDE as usize + 1)
                * 2
                * GROWW_CHAIN_1M_UNDERLYINGS.len(),
            GROWW_CONTRACT_1M_MAX_PER_MINUTE
        );
    }

    /// Constant pin — 60s grace after close.
    #[test]
    fn test_ws_grace_after_close_secs_pinned_at_60() {
        assert_eq!(WS_GRACE_AFTER_CLOSE_SECS, 60);
    }

    /// Constant pin — final seal offset matches grace by design.
    #[test]
    fn test_bar_final_seal_offset_matches_grace() {
        assert_eq!(BAR_FINAL_SEAL_OFFSET_SECS, WS_GRACE_AFTER_CLOSE_SECS);
    }

    /// Constant pin — late-tick anomaly threshold.
    #[test]
    fn test_late_tick_anomaly_threshold_ms_pinned_at_30000() {
        assert_eq!(LATE_TICK_ANOMALY_THRESHOLD_MS, 30_000);
    }

    /// Plan §8 row 1: a tick stamped `exchange_ts = 15:29:59.586` must
    /// be ACCEPTED by G1 even if the local clock has advanced past
    /// 15:30:00.x. The G1 gate looks ONLY at exchange_ts; the wall
    /// clock is irrelevant. This was the original bug yesterday.
    #[test]
    fn test_tick_with_exchange_ts_15_29_59_586_accepted_even_if_local_recv_15_30_00_100() {
        // exchange_ts = 15:29:59.586 IST nanoseconds-of-day.
        // 15h*3600 + 29m*60 + 59s = 55_799 secs, + 0.586s = 55_799_586 ms = 55_799_586_000_000 nanos.
        let exchange_ts = 55_799_586_000_000_i64;
        assert!(
            g1_exchange_gate_accepts(exchange_ts),
            "Plan §8: tick stamped 15:29:59.586 MUST be accepted by G1, \
             regardless of local wall-clock — this was the bug",
        );
    }

    /// Plan §8 row 2: a tick stamped exactly at 15:30:00.000 IST must
    /// be REJECTED by G1 (close is EXCLUSIVE).
    #[test]
    fn test_tick_with_exchange_ts_15_30_00_000_rejected() {
        let exchange_ts = MARKET_CLOSE_IST_NANOS;
        assert!(
            !g1_exchange_gate_accepts(exchange_ts),
            "MARKET_CLOSE is EXCLUSIVE — a tick at 15:30:00.000 must be rejected",
        );
    }

    /// Plan §8 row 1 negative: pre-09:15 ticks are REJECTED.
    #[test]
    fn test_g1_exchange_gate_rejects_pre_market_open() {
        // 09:14:59.999 IST.
        let exchange_ts = MARKET_OPEN_IST_NANOS - 1;
        assert!(!g1_exchange_gate_accepts(exchange_ts));
    }

    /// G1 boundary inclusivity — exactly 09:15:00.000 is ACCEPTED.
    #[test]
    fn test_g1_exchange_gate_accepts_exact_market_open() {
        assert!(g1_exchange_gate_accepts(MARKET_OPEN_IST_NANOS));
    }

    /// G1 boundary exclusivity — 15:29:59.999_999_999 is the last
    /// accepted nano. 15:30:00.000_000_000 is the first rejected.
    #[test]
    fn test_g1_exchange_gate_boundary_exclusive_on_close() {
        assert!(g1_exchange_gate_accepts(MARKET_CLOSE_IST_NANOS - 1));
        assert!(!g1_exchange_gate_accepts(MARKET_CLOSE_IST_NANOS));
    }

    /// Plan §8 row 3: WS socket stays open until 15:31:00. G2 accepts
    /// 15:30:30 (during grace).
    #[test]
    fn test_ws_socket_stays_open_until_15_31_00() {
        // 15:30:30 IST = 55_830 secs = 55_830_000_000_000 nanos.
        let wall_clock = 55_830_000_000_000_i64;
        assert!(
            g2_wall_clock_gate_accepts(wall_clock),
            "G2 must accept wall-clock 15:30:30 (30s into the 60s grace tail)",
        );
    }

    /// G2 rejects after 15:31:00.
    #[test]
    fn test_g2_wall_clock_gate_rejects_after_grace_close() {
        // 15:31:00 IST exactly = grace boundary (exclusive on close).
        let grace_close = MARKET_CLOSE_IST_NANOS + (60_i64 * 1_000_000_000);
        assert!(!g2_wall_clock_gate_accepts(grace_close));
        // 15:31:00.001 → also rejected.
        assert!(!g2_wall_clock_gate_accepts(grace_close + 1_000_000));
    }

    /// Plan §8 row 4 ("final bar seals at 15:31:00 not 15:30:00") —
    /// BAR_FINAL_SEAL_OFFSET_SECS = 60 means the 15:29 bar's forced
    /// seal happens 60s past `MARKET_CLOSE_IST_NANOS`.
    #[test]
    fn test_final_bar_seals_at_15_31_00_not_15_30_00() {
        let seal_offset_nanos = (BAR_FINAL_SEAL_OFFSET_SECS as i64) * 1_000_000_000;
        let forced_seal_at = MARKET_CLOSE_IST_NANOS + seal_offset_nanos;
        let expected_15_31_00 = 55_860_000_000_000_i64;
        assert_eq!(forced_seal_at, expected_15_31_00);
    }

    /// Plan §8: G1 gate function MUST NOT call local-clock helpers.
    /// Source-scan verifies the helper body uses only the
    /// `exchange_ts_nanos_of_day` argument.
    #[test]
    fn test_market_gate_does_not_call_local_now() {
        let src = include_str!("constants.rs");
        // Find the g1_exchange_gate_accepts function body.
        let fn_marker = "pub const fn g1_exchange_gate_accepts(";
        let start = src
            .find(fn_marker)
            .expect("g1_exchange_gate_accepts must exist");
        // Take ~500 chars after — the body is small.
        let body_end = (start + 500).min(src.len());
        let body = &src[start..body_end];
        assert!(
            !body.contains("Utc::now"),
            "G1 gate body MUST NOT call Utc::now — that was the bug",
        );
        assert!(
            !body.contains("now_ist"),
            "G1 gate body MUST NOT call now_ist — must use exchange_ts param",
        );
        assert!(
            !body.contains("SystemTime"),
            "G1 gate body MUST NOT call SystemTime — pure const fn",
        );
    }

    // 2026-06-28: the PR #8b option_chain heart-piece constant-pinning tests
    // were REMOVED with the OPTION_CHAIN_* constants (option_chain subsystem
    // deletion, operator directive 2026-06-28).

    #[test]
    fn test_index_symbol_aliases_cover_nifty_next_50_published_forms() {
        // 2026-06-26: Dhan publishes NIFTY NEXT 50 as both `NIFTYNXT50` and
        // the spaced `NIFTY NXT 50`; BOTH must alias to the canonical
        // `NIFTY NEXT 50` so the index-extractor self-heals the rename.
        let has = |alias: &str, canonical: &str| {
            INDEX_SYMBOL_ALIASES
                .iter()
                .any(|(a, c)| *a == alias && *c == canonical)
        };
        assert!(
            has("NIFTYNXT50", "NIFTY NEXT 50"),
            "compact NIFTYNXT50 alias preserved"
        );
        assert!(
            has("NIFTY NXT 50", "NIFTY NEXT 50"),
            "spaced NIFTY NXT 50 alias added (2026-06-26)"
        );
        // The pre-existing FNO-direction alias is untouched.
        assert!(has("SENSEX50", "SNSX50"), "SENSEX50 alias preserved");
    }

    #[test]
    fn test_index_symbol_aliases_cover_groww_dhan_spelling_bridges() {
        // 2026-06-28: the Groww index DISPLAY name for these three differs from
        // the Dhan-allowlist trading-symbol spelling; the additive bridge aliases
        // let the Groww index-coverage audit canonicalize them correctly so they
        // are not falsely flagged absent.
        let has = |alias: &str, canonical: &str| {
            INDEX_SYMBOL_ALIASES
                .iter()
                .any(|(a, c)| *a == alias && *c == canonical)
        };
        assert!(has("NIFTY MIDCAP SELECT", "MIDCPNIFTY"));
        assert!(has("NIFTY MIDCAP 50", "NIFTYMCAP50"));
        assert!(has("NIFTY TOTAL MARKET", "NIFTY TOTAL MKT"));
        // 2026-07-13 (Groww spot-leg INDIA VIX scope): the live Groww
        // master's compact token spelling must canonicalize to the
        // allowlisted "INDIA VIX" so the runtime VIX resolution matches on
        // the token key too (the display name "India Vix" already
        // normalizes without an alias).
        assert!(has("INDIAVIX", "INDIA VIX"));
    }

    // =======================================================================
    // B6 mutation kills (2026-07-03) — exact computed values for every
    // arithmetic const initializer the unmasked mutation gate found MISSED.
    // Each assert pins the fully-evaluated number so ANY operator swap
    // (`*`→`+`, `*`→`/`, `+`→`-`, `/`→`%`, ...) in the initializer fails.
    // =======================================================================

    #[test]
    fn test_max_csv_body_bytes_is_exactly_50_mib() {
        // 50 * 1024 * 1024 — kills the 4 arithmetic mutants at the initializer.
        assert_eq!(MAX_CSV_BODY_BYTES, 52_428_800);
    }

    #[test]
    fn test_tick_buffer_high_watermark_is_exactly_80_percent() {
        // TICK_BUFFER_CAPACITY * 4 / 5 — kills `*`/`/` swaps at the initializer.
        assert_eq!(TICK_BUFFER_CAPACITY, 100_000);
        assert_eq!(TICK_BUFFER_HIGH_WATERMARK, 80_000);
    }

    #[test]
    fn test_tick_spill_min_disk_space_is_exactly_100_mib() {
        // 100 * 1024 * 1024 — kills the 4 arithmetic mutants at the initializer.
        assert_eq!(TICK_SPILL_MIN_DISK_SPACE_BYTES, 104_857_600);
    }

    #[test]
    fn test_spill_file_max_age_is_exactly_7_days() {
        // 7 * 24 * 3600 — kills the 4 arithmetic mutants at the initializer.
        assert_eq!(SPILL_FILE_MAX_AGE_SECS, 604_800);
    }

    #[test]
    fn test_stock_contracts_per_expiry_is_exactly_43() {
        // (1 + 10 + 10) * 2 + 1 = 43 — kills all 8 arithmetic mutants at the
        // initializer (every operator swap yields a value ≠ 43).
        assert_eq!(STOCK_ATM_STRIKES_ABOVE, 10);
        assert_eq!(STOCK_ATM_STRIKES_BELOW, 10);
        assert_eq!(STOCK_CONTRACTS_PER_EXPIRY, 43);
    }

    #[test]
    fn test_token_sweep_interval_is_exactly_4_hours() {
        // 4 * 3600 — kills the `*`→`+` / `*`→`/` mutants at the initializer.
        assert_eq!(TOKEN_SWEEP_INTERVAL_SECS, 14_400);
    }

    #[test]
    fn test_token_sweep_staleness_threshold_is_exactly_4_hours() {
        // 4 * 3600 — kills the `*`→`+` / `*`→`/` mutants at the initializer,
        // and pins the alignment with TOKEN_SWEEP_INTERVAL_SECS.
        assert_eq!(TOKEN_SWEEP_STALENESS_THRESHOLD_SECS, 14_400);
        assert_eq!(
            TOKEN_SWEEP_STALENESS_THRESHOLD_SECS,
            TOKEN_SWEEP_INTERVAL_SECS as i64
        );
    }

    /// PR-R1 (2026-07-04): pins the Groww native-shadow constants — the wheel-
    /// verified endpoints, the shadow NDJSON path (distinct from the sidecar's
    /// `live-ticks.ndjson`), and the bounded-backoff/pacing envelope
    /// (base <= cap; auth floor mirrors the sidecar's 60s token-minter pacing).
    #[test]
    fn test_groww_native_constants_pinned() {
        assert_eq!(GROWW_SOCKET_URL, "wss://socket-api.groww.in");
        assert_eq!(
            GROWW_SOCKET_TOKEN_URL,
            "https://api.groww.in/v1/api/apex/v1/socket/token/create/"
        );
        assert!(GROWW_SOCKET_TOKEN_URL.starts_with("https://"));
        assert_eq!(GROWW_DATA_DIR, "data/groww");
        assert_eq!(
            GROWW_NATIVE_SHADOW_NDJSON_PATH,
            "data/groww/rust-live-ticks.ndjson"
        );
        assert!(GROWW_NATIVE_SHADOW_NDJSON_PATH.starts_with(GROWW_DATA_DIR));
        assert_ne!(
            GROWW_NATIVE_SHADOW_NDJSON_PATH, "data/groww/live-ticks.ndjson",
            "shadow capture must never collide with the sidecar's file"
        );
        assert_eq!(GROWW_NATIVE_RECONNECT_BASE_SECS, 5);
        assert_eq!(GROWW_NATIVE_RECONNECT_MAX_SECS, 60);
        assert!(GROWW_NATIVE_RECONNECT_BASE_SECS <= GROWW_NATIVE_RECONNECT_MAX_SECS);
        assert_eq!(GROWW_NATIVE_AUTH_RETRY_FLOOR_SECS, 60);
        assert_eq!(GROWW_NATIVE_WATCH_POLL_SECS, 30);
        assert_eq!(GROWW_NATIVE_WRITER_CHANNEL_CAPACITY, 65_536);
    }
}
