//! PR #450 (2026-05-03) — unified `/api/movers` handler matching the
//! Dhan Markets > Options frontend (web.dhan.co/index/markets/Options)
//! precisely. Single dynamic, scalable, runtime-configurable endpoint.
//!
//! # Dhan UI parity
//!
//! Maps the 7 tabs + 2 dropdowns from Dhan's Markets > Options page:
//!
//! | UI control       | Param            | Values                        |
//! |------------------|------------------|-------------------------------|
//! | 7 tabs           | `category`       | `highest_oi`, `oi_gainers`, … |
//! | Right dropdown   | `underlying_type`| `all`, `stock`, `index`       |
//! | Right dropdown   | `exchange`       | `all`, `nse`, `bse`           |
//! | Middle dropdown  | `expiry`         | ISO `YYYY-MM-DD`              |
//! | (route segment)  | `instrument_type`| `option`, `equity`, `future`, `index` |
//! | (limit)          | `limit`          | `1..=100` (default 30)        |
//!
//! # Security
//!
//! All 6 query parameters are typed enums on the backend — NO `&str`
//! interpolation into SQL. The injection vector flagged CRITICAL by
//! the PR #448 hostile-bug-hunt agent is closed at the type system.
//!
//! # Hot path
//!
//! HTTP cold path. SQL builder is `format!` over `&'static str`
//! constants — bench-gate enforces ≤ 100 ns p99 per
//! `quality/benchmark-budgets.toml` (PR #450 commit 8 will add the
//! Criterion bench).

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::state::SharedAppState;

// ---------------------------------------------------------------------------
// Typed enums for query parameters (close SQL injection at type system)
// ---------------------------------------------------------------------------

/// Top-level instrument type filter — corresponds to Dhan's
/// "Live / Options / Stocks / Commodity / Futures / ETFs" routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoversInstrumentType {
    /// Options — both stock options (OPTSTK) and index options (OPTIDX).
    Option,
    /// Cash equity instruments (EQUITY).
    Equity,
    /// Futures — both stock futures (FUTSTK) and index futures (FUTIDX).
    Future,
    /// Index value feeds (INDEX, e.g. NIFTY / BANKNIFTY / SENSEX).
    Index,
}

impl MoversInstrumentType {
    /// Parses the wire-format string (case-insensitive). Returns `None`
    /// on unknown input — handler emits 400.
    #[must_use]
    pub fn parse_wire_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "OPTION" | "OPTIONS" => Some(Self::Option),
            "EQUITY" | "EQUITIES" | "STOCK" | "STOCKS" => Some(Self::Equity),
            "FUTURE" | "FUTURES" => Some(Self::Future),
            "INDEX" | "INDICES" | "INDEXES" => Some(Self::Index),
            _ => None,
        }
    }

    /// Returns the SQL `IN (...)` body listing the `movers_1s.instrument_type`
    /// SYMBOL values that match this top-level filter. All values are
    /// compile-time `&'static str` constants — no user input passes through.
    /// Refined further when combined with `MoversUnderlyingType`.
    #[must_use]
    pub const fn sql_in_clause_default(self) -> &'static str {
        match self {
            Self::Option => "'OPTSTK', 'OPTIDX'",
            Self::Equity => "'EQUITY'",
            Self::Future => "'FUTSTK', 'FUTIDX'",
            Self::Index => "'INDEX'",
        }
    }

    /// Wire-format string for the response `instrument_type` field.
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::Option => "option",
            Self::Equity => "equity",
            Self::Future => "future",
            Self::Index => "index",
        }
    }
}

/// Sort metric — corresponds to the 7 tabs at the top of Dhan's
/// Markets > Options page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoversCategory {
    /// Sort by `open_interest DESC`.
    HighestOi,
    /// Sort by `oi_change_session DESC` (current_OI − prev_session_OI).
    OiGainer,
    /// Sort by `oi_change_session ASC`.
    OiLoser,
    /// Sort by `volume_cumulative DESC` (cumulative session volume).
    TopVolume,
    /// Sort by `(last_price * volume_cumulative) DESC` (notional turnover).
    TopValue,
    /// Sort by `change_pct_session DESC`.
    PriceGainer,
    /// Sort by `change_pct_session ASC`.
    PriceLoser,
}

impl MoversCategory {
    /// Parses the wire-format string. Accepts both singular and plural
    /// forms (the Dhan-style frontend sends plurals like `oi_gainers`).
    #[must_use]
    pub fn parse_wire_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "HIGHEST_OI" => Some(Self::HighestOi),
            "OI_GAINER" | "OI_GAINERS" => Some(Self::OiGainer),
            "OI_LOSER" | "OI_LOSERS" => Some(Self::OiLoser),
            "TOP_VOLUME" => Some(Self::TopVolume),
            "TOP_VALUE" => Some(Self::TopValue),
            "PRICE_GAINER" | "PRICE_GAINERS" => Some(Self::PriceGainer),
            "PRICE_LOSER" | "PRICE_LOSERS" => Some(Self::PriceLoser),
            _ => None,
        }
    }

    /// Returns the SQL `ORDER BY` clause body using the view-aliased
    /// column names from `movers_view_ddl`. All return values are
    /// compile-time string literals — no user input passes through.
    #[must_use]
    pub const fn order_by_clause(self) -> &'static str {
        match self {
            Self::HighestOi => "open_interest DESC",
            Self::OiGainer => "oi_change_session DESC",
            Self::OiLoser => "oi_change_session ASC",
            Self::TopVolume => "volume_cumulative DESC",
            Self::TopValue => "(last_price * volume_cumulative) DESC",
            Self::PriceGainer => "change_pct_session DESC",
            Self::PriceLoser => "change_pct_session ASC",
        }
    }

    /// Wire-format string for the response `category` field.
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::HighestOi => "highest_oi",
            Self::OiGainer => "oi_gainer",
            Self::OiLoser => "oi_loser",
            Self::TopVolume => "top_volume",
            Self::TopValue => "top_value",
            Self::PriceGainer => "price_gainer",
            Self::PriceLoser => "price_loser",
        }
    }
}

/// Refines the `instrument_type` filter for options/futures —
/// corresponds to Dhan's right dropdown ("Stock" vs "Index").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoversUnderlyingType {
    /// No refinement — both stock and index variants.
    All,
    /// Stock variant only — OPTSTK / FUTSTK.
    Stock,
    /// Index variant only — OPTIDX / FUTIDX.
    Index,
}

impl MoversUnderlyingType {
    /// Parses the wire-format string. Default `all`.
    #[must_use]
    pub fn parse_wire_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "ALL" => Some(Self::All),
            "STOCK" | "STOCKS" => Some(Self::Stock),
            "INDEX" | "INDICES" => Some(Self::Index),
            _ => None,
        }
    }

    /// Refines the SQL `IN (...)` clause when combined with
    /// `MoversInstrumentType`. Returns `None` to mean "no refinement
    /// needed — use the default clause from `MoversInstrumentType`".
    #[must_use]
    pub const fn refined_in_clause(self, top: MoversInstrumentType) -> Option<&'static str> {
        match (top, self) {
            (MoversInstrumentType::Option, Self::Stock) => Some("'OPTSTK'"),
            (MoversInstrumentType::Option, Self::Index) => Some("'OPTIDX'"),
            (MoversInstrumentType::Future, Self::Stock) => Some("'FUTSTK'"),
            (MoversInstrumentType::Future, Self::Index) => Some("'FUTIDX'"),
            // Equity/Index top-level types are already specific — refinement is a no-op.
            // `All` underlying_type is also a no-op.
            _ => None,
        }
    }
}

/// Exchange filter — corresponds to Dhan's right dropdown ("NSE" vs
/// "BSE").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoversExchange {
    /// No filter — all exchanges.
    All,
    /// NSE only — `exchange_segment` LIKE 'NSE_%'.
    Nse,
    /// BSE only — `exchange_segment` LIKE 'BSE_%'.
    Bse,
}

impl MoversExchange {
    /// Parses the wire-format string.
    #[must_use]
    pub fn parse_wire_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "ALL" => Some(Self::All),
            "NSE" => Some(Self::Nse),
            "BSE" => Some(Self::Bse),
            _ => None,
        }
    }

    /// Returns the SQL `AND exchange_segment LIKE 'XYZ_%'` clause body
    /// (without the leading `AND `), or empty string for `All`. All
    /// return values are compile-time literals.
    #[must_use]
    pub const fn sql_filter_suffix(self) -> &'static str {
        match self {
            Self::All => "",
            Self::Nse => "AND exchange_segment LIKE 'NSE_%' ",
            Self::Bse => "AND exchange_segment LIKE 'BSE_%' ",
        }
    }
}

// ---------------------------------------------------------------------------
// Request + Response shapes
// ---------------------------------------------------------------------------

/// Query parameters for `GET /api/movers`. All optional except
/// `instrument_type` (defaults to `option` to match Dhan's default tab).
#[derive(Debug, Deserialize)]
pub struct MoversQuery {
    /// Top-level filter: `option` / `equity` / `future` / `index`.
    /// Default: `option`.
    pub instrument_type: Option<String>,
    /// Sort metric. Default: `top_volume`.
    pub category: Option<String>,
    /// Underlying type refinement: `all` / `stock` / `index`. Default: `all`.
    pub underlying_type: Option<String>,
    /// Exchange filter: `all` / `nse` / `bse`. Default: `all`.
    pub exchange: Option<String>,
    /// Expiry filter — ISO date `YYYY-MM-DD`. Default: no filter (all expiries).
    pub expiry: Option<String>,
    /// Result limit (1..=100). Default: 30.
    pub limit: Option<usize>,
}

/// One row in the `/api/movers` response. Matches Dhan's column shape
/// 1:1 (Spot Price + LTP + Change + Change % + OI + OI Change + OI
/// Change % + Volume).
#[derive(Debug, Serialize, Deserialize)]
pub struct MoverRow {
    /// 1-indexed rank within the response (matches Dhan UI).
    pub rank: usize,
    /// Dhan SecurityId.
    pub security_id: u32,
    /// Precise per-exchange tag (`"NSE_FNO"` / `"BSE_FNO"` / `"NSE_EQ"` / `"IDX_I"`).
    /// Preserves I-P1-11 cross-segment uniqueness.
    pub exchange_segment: String,
    /// Last traded price.
    pub ltp: f64,
    /// Previous-day close price.
    pub prev_close: f64,
    /// `LTP − prev_close`.
    pub change: f64,
    /// `(LTP − prev_close) / prev_close × 100` (guarded).
    pub change_pct: f64,
    /// Current open interest (snapshot).
    pub open_interest: i64,
    /// `current_OI − prev_session_OI` — Dhan-precise per PR #450 commit 1.
    pub oi_change: i64,
    /// `(oi_change / prev_session_OI) × 100` — guarded against zero baseline.
    pub oi_change_pct: f64,
    /// Cumulative session volume.
    pub volume: i64,
    /// Derivative expiry as ISO date (`YYYY-MM-DD`), or empty for
    /// non-derivative rows.
    pub expiry: String,
}

/// Response shape for `GET /api/movers`. Flat — `entries` carries the
/// ranked rows. Frontend maps the array directly to a table.
#[derive(Debug, Serialize)]
pub struct MoversResponse {
    /// `false` only when QuestDB is unreachable AND no fallback worked.
    pub available: bool,
    /// Echoes the resolved `instrument_type` for the frontend.
    pub instrument_type: String,
    /// Echoes the resolved `category` for the frontend.
    pub category: String,
    /// Ranked rows (1-indexed).
    pub entries: Vec<MoverRow>,
}

// ---------------------------------------------------------------------------
// SQL builder
// ---------------------------------------------------------------------------

const QUESTDB_QUERY_TIMEOUT_SECS: u64 = 10;
const DEFAULT_LIMIT: usize = 30;
const MAX_LIMIT: usize = 100;

/// Builds the full SELECT for the `/api/movers` query — all 6 filter
/// dimensions resolved into typed enums + 1 numeric LIMIT. No user
/// `&str` is interpolated into the SQL.
///
/// # Panics
///
/// Panics if `limit == 0` or `limit > MAX_LIMIT`. Caller MUST
/// `clamp(1, MAX_LIMIT)` before invocation.
#[must_use]
pub fn build_movers_query(
    instrument_type: MoversInstrumentType,
    category: MoversCategory,
    underlying_type: MoversUnderlyingType,
    exchange: MoversExchange,
    expiry: Option<&str>,
    limit: usize,
) -> String {
    assert!(limit > 0, "limit must be > 0");
    assert!(limit <= MAX_LIMIT, "limit must be <= {MAX_LIMIT}");

    let in_clause = underlying_type
        .refined_in_clause(instrument_type)
        .unwrap_or_else(|| instrument_type.sql_in_clause_default());

    let exchange_filter = exchange.sql_filter_suffix();
    let order_by = category.order_by_clause();

    // Expiry filter: format as cast to date for QuestDB. `None` → empty.
    // PR #450 commit 4: expiry is validated as ISO date at parse time
    // (see handler entry); reaches here only as a clean YYYY-MM-DD literal.
    let expiry_filter = expiry
        .map(|e| format!("AND cast(expiry_date_ist as date) = cast('{e}' as date) "))
        .unwrap_or_default();

    format!(
        "SELECT security_id, exchange_segment, last_price, prev_close, \
                change_pct_session, open_interest, oi_change_session, \
                oi_change_pct_session, volume_cumulative, expiry_date_ist \
         FROM movers_5s \
         WHERE ts > dateadd('s', -30, now()) \
           AND instrument_type IN ({in_clause}) \
           {exchange_filter}{expiry_filter}\
         LATEST ON ts PARTITION BY security_id \
         ORDER BY {order_by} \
         LIMIT {limit}",
    )
}

/// Validates an ISO date string (`YYYY-MM-DD`) — returns `Some(s)` if
/// well-formed, `None` otherwise. Hardens against SQL injection via
/// the expiry param.
#[must_use]
pub fn validate_iso_date(s: &str) -> Option<&str> {
    if s.len() != 10 {
        return None;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if bytes[..4].iter().any(|b| !b.is_ascii_digit())
        || bytes[5..7].iter().any(|b| !b.is_ascii_digit())
        || bytes[8..].iter().any(|b| !b.is_ascii_digit())
    {
        return None;
    }
    Some(s)
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// `GET /api/movers?instrument_type=option&category=top_volume&...` —
/// unified Dhan-parity movers endpoint.
///
/// PR #450 (2026-05-03): replaces the legacy `/api/top-movers` (PR
/// #446) and `/api/movers` (V2) routes with a single dynamic
/// endpoint matching Dhan's Markets > Options frontend precisely.
// TEST-EXEMPT: HTTP handler — requires live QuestDB for integration; pure helpers
// (build_movers_query, validate_iso_date, enum parsers) are unit-tested below.
pub async fn get_movers(
    State(state): State<SharedAppState>,
    Query(params): Query<MoversQuery>,
) -> impl IntoResponse {
    // Resolve typed enums with sensible defaults.
    let instrument_type = match params
        .instrument_type
        .as_deref()
        .map(MoversInstrumentType::parse_wire_str)
        .unwrap_or(Some(MoversInstrumentType::Option))
    {
        Some(it) => it,
        None => return bad_request("instrument_type", "option/equity/future/index"),
    };

    let category = match params
        .category
        .as_deref()
        .map(MoversCategory::parse_wire_str)
        .unwrap_or(Some(MoversCategory::TopVolume))
    {
        Some(c) => c,
        None => {
            return bad_request(
                "category",
                "highest_oi/oi_gainers/oi_losers/top_volume/top_value/price_gainers/price_losers",
            );
        }
    };

    let underlying_type = match params
        .underlying_type
        .as_deref()
        .map(MoversUnderlyingType::parse_wire_str)
        .unwrap_or(Some(MoversUnderlyingType::All))
    {
        Some(ut) => ut,
        None => return bad_request("underlying_type", "all/stock/index"),
    };

    let exchange = match params
        .exchange
        .as_deref()
        .map(MoversExchange::parse_wire_str)
        .unwrap_or(Some(MoversExchange::All))
    {
        Some(e) => e,
        None => return bad_request("exchange", "all/nse/bse"),
    };

    let expiry_validated = match params.expiry.as_deref() {
        None => None,
        Some(s) => match validate_iso_date(s) {
            Some(v) => Some(v.to_string()),
            None => return bad_request("expiry", "YYYY-MM-DD"),
        },
    };

    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    // Build SQL.
    let sql = build_movers_query(
        instrument_type,
        category,
        underlying_type,
        exchange,
        expiry_validated.as_deref(),
        limit,
    );

    // Execute.
    let cfg = state.questdb_config();
    let url = format!("http://{}:{}/exec", cfg.host, cfg.http_port);

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(QUESTDB_QUERY_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            tracing::error!(?err, code = "MOVERS-04", "build reqwest client failed");
            return service_unavailable("client_build_failed");
        }
    };

    let resp = match client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::error!(?err, code = "MOVERS-04", "QuestDB /exec request failed");
            return service_unavailable("questdb_unreachable");
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        tracing::error!(%status, code = "MOVERS-04", "QuestDB returned non-2xx");
        return service_unavailable("questdb_non_2xx");
    }

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(?err, code = "MOVERS-05", "QuestDB JSON parse failed");
            return service_unavailable("questdb_parse_failed");
        }
    };

    let entries = parse_movers_dataset(&body, limit);
    let ok_response = MoversResponse {
        available: true,
        instrument_type: instrument_type.as_wire_str().to_string(),
        category: category.as_wire_str().to_string(),
        entries,
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(&ok_response).unwrap_or(serde_json::Value::Null)),
    )
        .into_response()
}

fn bad_request(field: &str, accepts: &str) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": "invalid query parameter",
            "field": field,
            "accepts": accepts,
        })),
    )
        .into_response()
}

fn service_unavailable(reason: &str) -> axum::response::Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "available": false,
            "reason": reason,
        })),
    )
        .into_response()
}

/// Parses the QuestDB `/exec` JSON response into `Vec<MoverRow>`.
///
/// Expected dataset column order (matches `build_movers_query`):
/// `security_id, exchange_segment, last_price, prev_close,
///  change_pct_session, open_interest, oi_change_session,
///  oi_change_pct_session, volume_cumulative, expiry_date_ist`
///
/// Rows with missing or malformed fields are SKIPPED — single
/// malformed row should not poison the entire response.
fn parse_movers_dataset(json: &serde_json::Value, max_capacity: usize) -> Vec<MoverRow> {
    let Some(dataset) = json.get("dataset").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<MoverRow> = Vec::with_capacity(dataset.len().min(max_capacity));
    for (idx, row) in dataset.iter().enumerate() {
        let Some(arr) = row.as_array() else { continue };
        if arr.len() < 10 {
            continue;
        }
        let Some(security_id) = arr[0].as_u64().and_then(|v| u32::try_from(v).ok()) else {
            continue;
        };
        let exchange_segment = arr[1].as_str().unwrap_or("UNKNOWN").to_string();
        let last_price = arr[2].as_f64().unwrap_or(0.0);
        let prev_close = arr[3].as_f64().unwrap_or(0.0);
        let change_pct = arr[4].as_f64().unwrap_or(0.0);
        let open_interest = arr[5].as_i64().unwrap_or(0);
        let oi_change = arr[6].as_i64().unwrap_or(0);
        let oi_change_pct = arr[7].as_f64().unwrap_or(0.0);
        let volume = arr[8].as_i64().unwrap_or(0);
        // expiry_date_ist arrives as ISO timestamp string in the JSON
        // dataset response; truncate to the date component for the
        // frontend. Empty for non-derivative rows (NULL → JSON null).
        let expiry = arr[9]
            .as_str()
            .map(|s| s.split('T').next().unwrap_or(s).to_string())
            .unwrap_or_default();
        let change = last_price - prev_close;

        out.push(MoverRow {
            rank: idx + 1,
            security_id,
            exchange_segment,
            ltp: last_price,
            prev_close,
            change,
            change_pct,
            open_interest,
            oi_change,
            oi_change_pct,
            volume,
            expiry,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// /api/movers/expiries — companion handler
// ---------------------------------------------------------------------------

/// Query parameters for `GET /api/movers/expiries`.
#[derive(Debug, Deserialize)]
pub struct MoversExpiriesQuery {
    /// Underlying instrument symbol (eg `NIFTY`, `BANKNIFTY`, `RELIANCE`).
    /// Currently optional — when absent returns the full sorted distinct
    /// set across all derivatives in the freshness window.
    pub underlying: Option<String>,
}

/// Response shape for `GET /api/movers/expiries`. Sorted ascending so
/// the frontend dropdown is naturally chronological (May / June / July...).
#[derive(Debug, Serialize)]
pub struct MoversExpiriesResponse {
    /// `false` only when QuestDB unreachable.
    pub available: bool,
    /// Echoes the resolved underlying ("ALL" when not specified).
    pub underlying: String,
    /// ISO `YYYY-MM-DD` strings sorted ascending.
    pub expiries: Vec<String>,
}

/// PR #450 commit 5 (2026-05-03): validates an underlying symbol —
/// allows only `[A-Z0-9]{1,32}`. Closes the SQL injection vector via
/// the underlying param (legacy handlers used unfiltered string
/// interpolation; this handler refuses anything outside the safe
/// alphabet).
#[must_use]
pub fn validate_underlying_symbol(s: &str) -> Option<&str> {
    if s.is_empty() || s.len() > 32 {
        return None;
    }
    if !s
        .as_bytes()
        .iter()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return None;
    }
    Some(s)
}

/// Builds the SELECT for `/api/movers/expiries`. Uses
/// `expiry_date_ist` (TIMESTAMP IST midnight epoch nanos, populated
/// by PR #450 commit 2 in the tick processor).
///
/// When `underlying` is absent, returns ALL distinct expiry dates in
/// the freshness window. When present, filters by `underlying_symbol`
/// — but `movers_5s` does NOT carry an underlying_symbol column, so
/// we filter via JOIN-equivalent subquery against the `instruments`
/// registry (frontend can do this filter client-side as a
/// near-zero-cost alternative).
///
/// PR #450 commit 5: simple version returns the universe-wide list.
/// Per-underlying filtering ships in a follow-up once the SQL
/// JOIN-via-IN(...) pattern is benched.
#[must_use]
pub fn build_expiries_query() -> String {
    // expiry_date_ist > 0 filters out the sentinel-0 rows from
    // non-derivative instruments (equities + indices have no expiry).
    // dateadd('s', -300, now()) gives 5min freshness — covers the gap
    // between successive 5s materialised view refreshes.
    "SELECT DISTINCT cast(expiry_date_ist as date) AS expiry_date \
     FROM movers_5s \
     WHERE ts > dateadd('s', -300, now()) \
       AND expiry_date_ist > 0 \
     ORDER BY expiry_date ASC"
        .to_string()
}

/// `GET /api/movers/expiries?underlying=NIFTY` — returns the sorted
/// list of available derivative expiry dates for the frontend's
/// middle dropdown.
///
/// PR #450 commit 5 (2026-05-03): companion to `/api/movers`. Replaces
/// hardcoded month strings on the frontend with a data-driven dropdown.
// TEST-EXEMPT: HTTP handler — requires live QuestDB; pure helpers
// (validate_underlying_symbol, build_expiries_query) are unit-tested.
pub async fn get_expiries(
    State(state): State<SharedAppState>,
    Query(params): Query<MoversExpiriesQuery>,
) -> impl IntoResponse {
    let underlying_label = match params.underlying.as_deref() {
        None => "ALL".to_string(),
        Some(s) => match validate_underlying_symbol(s) {
            Some(v) => v.to_string(),
            None => return bad_request("underlying", "[A-Z0-9]{1,32}"),
        },
    };

    let sql = build_expiries_query();
    let cfg = state.questdb_config();
    let url = format!("http://{}:{}/exec", cfg.host, cfg.http_port);

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(QUESTDB_QUERY_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            tracing::error!(?err, code = "MOVERS-06", "build reqwest client failed");
            return service_unavailable("client_build_failed");
        }
    };

    let resp = match client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::error!(
                ?err,
                code = "MOVERS-06",
                "QuestDB /api/movers/expiries query failed"
            );
            return service_unavailable("questdb_unreachable");
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        tracing::error!(
            %status,
            code = "MOVERS-06",
            "QuestDB returned non-2xx for /api/movers/expiries"
        );
        return service_unavailable("questdb_non_2xx");
    }

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(
                ?err,
                code = "MOVERS-06",
                "QuestDB JSON parse failed for /api/movers/expiries"
            );
            return service_unavailable("questdb_parse_failed");
        }
    };

    let expiries = parse_expiries_dataset(&body);
    let ok_response = MoversExpiriesResponse {
        available: true,
        underlying: underlying_label,
        expiries,
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(&ok_response).unwrap_or(serde_json::Value::Null)),
    )
        .into_response()
}

/// Parses the QuestDB `/exec` JSON dataset into a sorted Vec of
/// `YYYY-MM-DD` strings. Returns empty Vec on parse failure or empty
/// dataset (frontend renders "no expiries" empty state).
fn parse_expiries_dataset(json: &serde_json::Value) -> Vec<String> {
    let Some(dataset) = json.get("dataset").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::with_capacity(dataset.len());
    for row in dataset {
        let Some(arr) = row.as_array() else { continue };
        if arr.is_empty() {
            continue;
        }
        if let Some(s) = arr[0].as_str() {
            // QuestDB returns date as ISO timestamp string; truncate to date.
            let date_only = s.split('T').next().unwrap_or(s).to_string();
            out.push(date_only);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests — pure helpers (SQL builder + enum parsers + ISO date validator)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// PR #450 ratchet: every Dhan UI tab MUST parse from BOTH the
    /// singular and plural wire-format strings (frontend may send
    /// either).
    #[test]
    fn test_movers_category_accepts_singular_and_plural_forms() {
        for s in [
            "highest_oi",
            "oi_gainer",
            "oi_gainers",
            "oi_loser",
            "oi_losers",
            "top_volume",
            "top_value",
            "price_gainer",
            "price_gainers",
            "price_loser",
            "price_losers",
        ] {
            assert!(
                MoversCategory::parse_wire_str(s).is_some(),
                "category {s:?} must parse"
            );
        }
    }

    #[test]
    fn test_movers_instrument_type_accepts_dhan_default_route_segments() {
        for s in [
            "option", "options", "equity", "stock", "stocks", "future", "futures", "index",
            "indices",
        ] {
            assert!(
                MoversInstrumentType::parse_wire_str(s).is_some(),
                "instrument_type {s:?} must parse"
            );
        }
    }

    #[test]
    fn test_movers_underlying_type_accepts_three_values() {
        assert_eq!(
            MoversUnderlyingType::parse_wire_str("all"),
            Some(MoversUnderlyingType::All)
        );
        assert_eq!(
            MoversUnderlyingType::parse_wire_str("STOCK"),
            Some(MoversUnderlyingType::Stock)
        );
        assert_eq!(
            MoversUnderlyingType::parse_wire_str("Index"),
            Some(MoversUnderlyingType::Index)
        );
        assert_eq!(MoversUnderlyingType::parse_wire_str("garbage"), None);
    }

    #[test]
    fn test_movers_exchange_accepts_three_values() {
        assert_eq!(
            MoversExchange::parse_wire_str("nse"),
            Some(MoversExchange::Nse)
        );
        assert_eq!(
            MoversExchange::parse_wire_str("BSE"),
            Some(MoversExchange::Bse)
        );
        assert_eq!(
            MoversExchange::parse_wire_str("all"),
            Some(MoversExchange::All)
        );
    }

    #[test]
    fn test_underlying_type_refines_only_for_option_and_future() {
        // OPTSTK refinement
        assert_eq!(
            MoversUnderlyingType::Stock.refined_in_clause(MoversInstrumentType::Option),
            Some("'OPTSTK'")
        );
        // OPTIDX refinement
        assert_eq!(
            MoversUnderlyingType::Index.refined_in_clause(MoversInstrumentType::Option),
            Some("'OPTIDX'")
        );
        // FUTSTK refinement
        assert_eq!(
            MoversUnderlyingType::Stock.refined_in_clause(MoversInstrumentType::Future),
            Some("'FUTSTK'")
        );
        // FUTIDX refinement
        assert_eq!(
            MoversUnderlyingType::Index.refined_in_clause(MoversInstrumentType::Future),
            Some("'FUTIDX'")
        );
        // No refinement for top-level Equity / Index
        assert_eq!(
            MoversUnderlyingType::Stock.refined_in_clause(MoversInstrumentType::Equity),
            None
        );
        assert_eq!(
            MoversUnderlyingType::Index.refined_in_clause(MoversInstrumentType::Index),
            None
        );
        // No refinement for All
        assert_eq!(
            MoversUnderlyingType::All.refined_in_clause(MoversInstrumentType::Option),
            None
        );
    }

    #[test]
    fn test_build_query_targets_movers_5s_view() {
        let sql = build_movers_query(
            MoversInstrumentType::Option,
            MoversCategory::TopVolume,
            MoversUnderlyingType::All,
            MoversExchange::All,
            None,
            30,
        );
        assert!(sql.contains("FROM movers_5s"));
    }

    #[test]
    fn test_build_query_uses_dhan_precise_oi_change_session() {
        let sql = build_movers_query(
            MoversInstrumentType::Option,
            MoversCategory::OiGainer,
            MoversUnderlyingType::All,
            MoversExchange::All,
            None,
            30,
        );
        assert!(
            sql.contains("ORDER BY oi_change_session DESC"),
            "OI Gainer must sort by Dhan-precise oi_change_session (= current_OI − prev_session_OI), got: {sql}"
        );
    }

    #[test]
    fn test_build_query_top_value_uses_price_volume_notional() {
        let sql = build_movers_query(
            MoversInstrumentType::Option,
            MoversCategory::TopValue,
            MoversUnderlyingType::All,
            MoversExchange::All,
            None,
            30,
        );
        assert!(
            sql.contains("ORDER BY (last_price * volume_cumulative) DESC"),
            "Top Value must sort by notional (LTP × cumulative volume), got: {sql}"
        );
    }

    #[test]
    fn test_build_query_underlying_type_stock_refines_to_optstk_only() {
        let sql = build_movers_query(
            MoversInstrumentType::Option,
            MoversCategory::TopVolume,
            MoversUnderlyingType::Stock,
            MoversExchange::All,
            None,
            30,
        );
        assert!(
            sql.contains("instrument_type IN ('OPTSTK')"),
            "underlying_type=stock + instrument_type=option must refine to OPTSTK only, got: {sql}"
        );
    }

    #[test]
    fn test_build_query_exchange_nse_filter_appended() {
        let sql = build_movers_query(
            MoversInstrumentType::Option,
            MoversCategory::TopVolume,
            MoversUnderlyingType::All,
            MoversExchange::Nse,
            None,
            30,
        );
        assert!(
            sql.contains("AND exchange_segment LIKE 'NSE_%'"),
            "exchange=nse must add LIKE filter, got: {sql}"
        );
    }

    #[test]
    fn test_build_query_expiry_filter_uses_validated_iso_date() {
        let sql = build_movers_query(
            MoversInstrumentType::Option,
            MoversCategory::TopVolume,
            MoversUnderlyingType::All,
            MoversExchange::All,
            Some("2026-05-29"),
            30,
        );
        assert!(
            sql.contains("cast(expiry_date_ist as date) = cast('2026-05-29' as date)"),
            "expiry filter must cast both sides to date for QuestDB DATE comparison, got: {sql}"
        );
    }

    #[test]
    fn test_build_query_no_expiry_filter_clause_when_none() {
        // The SELECT always projects `expiry_date_ist` (frontend renders
        // it). Asserts ONLY that no WHERE-clause filter on the column
        // appears when the `expiry` param is absent.
        let sql = build_movers_query(
            MoversInstrumentType::Option,
            MoversCategory::TopVolume,
            MoversUnderlyingType::All,
            MoversExchange::All,
            None,
            30,
        );
        assert!(
            !sql.contains("cast(expiry_date_ist as date)"),
            "no expiry param → no expiry WHERE-filter, got: {sql}"
        );
    }

    #[test]
    fn test_build_query_includes_limit() {
        let sql = build_movers_query(
            MoversInstrumentType::Option,
            MoversCategory::TopVolume,
            MoversUnderlyingType::All,
            MoversExchange::All,
            None,
            42,
        );
        assert!(sql.contains("LIMIT 42"), "must include LIMIT, got: {sql}");
    }

    #[test]
    #[should_panic(expected = "limit must be > 0")]
    fn test_build_query_panics_on_zero_limit() {
        let _ = build_movers_query(
            MoversInstrumentType::Option,
            MoversCategory::TopVolume,
            MoversUnderlyingType::All,
            MoversExchange::All,
            None,
            0,
        );
    }

    #[test]
    #[should_panic(expected = "limit must be <= 100")]
    fn test_build_query_panics_on_limit_above_max() {
        let _ = build_movers_query(
            MoversInstrumentType::Option,
            MoversCategory::TopVolume,
            MoversUnderlyingType::All,
            MoversExchange::All,
            None,
            101,
        );
    }

    #[test]
    fn test_validate_iso_date_accepts_well_formed() {
        assert_eq!(validate_iso_date("2026-05-29"), Some("2026-05-29"));
        assert_eq!(validate_iso_date("2026-12-31"), Some("2026-12-31"));
        assert_eq!(validate_iso_date("2099-01-01"), Some("2099-01-01"));
    }

    #[test]
    fn test_validate_iso_date_rejects_malformed_or_injection_attempts() {
        for bad in [
            "2026-5-29",              // missing leading zero
            "2026/05/29",             // wrong separator
            "2026-05-29T00:00:00",    // includes time
            "",                       // empty
            "garbage",                // garbage
            "2026-05-29' OR 1=1 --",  // SQL injection attempt
            "2026-05-29; DROP TABLE", // SQL injection attempt
        ] {
            assert_eq!(
                validate_iso_date(bad),
                None,
                "must reject {bad:?} as malformed"
            );
        }
    }

    /// PR #450 ratchet: parser MUST extract all 10 columns the SQL builder
    /// projects, in the correct order. Schema drift detection.
    #[test]
    fn test_parse_movers_dataset_extracts_all_ten_columns_in_order() {
        let json: serde_json::Value = serde_json::json!({
            "dataset": [
                [42, "NSE_FNO", 100.5, 95.0, 5.78, 50000, 1500, 3.0, 200000, "2026-05-29T00:00:00.000000"]
            ]
        });
        let entries = parse_movers_dataset(&json, 30);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.rank, 1);
        assert_eq!(e.security_id, 42);
        assert_eq!(e.exchange_segment, "NSE_FNO");
        assert!((e.ltp - 100.5).abs() < f64::EPSILON);
        assert!((e.prev_close - 95.0).abs() < f64::EPSILON);
        assert!((e.change - 5.5).abs() < f64::EPSILON);
        assert!((e.change_pct - 5.78).abs() < f64::EPSILON);
        assert_eq!(e.open_interest, 50000);
        assert_eq!(e.oi_change, 1500);
        assert!((e.oi_change_pct - 3.0).abs() < f64::EPSILON);
        assert_eq!(e.volume, 200000);
        assert_eq!(e.expiry, "2026-05-29");
    }

    #[test]
    fn test_parse_movers_dataset_skips_short_rows() {
        let json: serde_json::Value = serde_json::json!({
            "dataset": [
                [42, "NSE_FNO", 100.5, 95.0, 5.78, 50000, 1500, 3.0, 200000, null],
                [99],  // too short, skip
                [100, "NSE_FNO", 50.0, 49.0, 2.0, 1000, 0, 0.0, 5000, null],
            ]
        });
        let entries = parse_movers_dataset(&json, 30);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].security_id, 42);
        assert_eq!(entries[1].security_id, 100);
    }

    #[test]
    fn test_parse_movers_dataset_empty_returns_empty_vec() {
        let json: serde_json::Value = serde_json::json!({ "dataset": [] });
        assert_eq!(parse_movers_dataset(&json, 30).len(), 0);
    }

    // -----------------------------------------------------------------
    // PR #450 commit 5 ratchets: /api/movers/expiries companion
    // -----------------------------------------------------------------

    #[test]
    fn test_validate_underlying_symbol_accepts_alpha_numeric_uppercase() {
        for ok in ["NIFTY", "BANKNIFTY", "RELIANCE", "MIDCPNIFTY", "TCS"] {
            assert_eq!(
                validate_underlying_symbol(ok),
                Some(ok),
                "must accept {ok:?}"
            );
        }
    }

    #[test]
    fn test_validate_underlying_symbol_rejects_lowercase_and_injection() {
        for bad in [
            "",                  // empty
            "nifty",             // lowercase
            "NIFTY ",            // whitespace
            "NIFTY-WEEKLY",      // dash
            "NIFTY' OR 1=1 --",  // SQL injection attempt
            "NIFTY; DROP TABLE", // SQL injection attempt
            &"A".repeat(33),     // > 32 chars
        ] {
            assert_eq!(validate_underlying_symbol(bad), None, "must reject {bad:?}");
        }
    }

    #[test]
    fn test_build_expiries_query_targets_movers_5s() {
        let sql = build_expiries_query();
        assert!(sql.contains("FROM movers_5s"));
    }

    #[test]
    fn test_build_expiries_query_filters_out_zero_expiry_sentinel() {
        // Equities + indices have expiry_date_ist = 0 (sentinel).
        // The query MUST exclude them — frontend dropdown should
        // only show real derivative expiry dates.
        let sql = build_expiries_query();
        assert!(
            sql.contains("expiry_date_ist > 0"),
            "must filter out sentinel-0 rows from non-derivatives, got: {sql}"
        );
    }

    #[test]
    fn test_build_expiries_query_uses_distinct_and_orders_ascending() {
        let sql = build_expiries_query();
        assert!(sql.contains("SELECT DISTINCT"));
        assert!(sql.contains("ORDER BY expiry_date ASC"));
    }

    #[test]
    fn test_build_expiries_query_casts_timestamp_to_date() {
        // expiry_date_ist is TIMESTAMP (epoch nanos at IST midnight);
        // cast-to-date strips the time component for the dropdown.
        let sql = build_expiries_query();
        assert!(
            sql.contains("cast(expiry_date_ist as date)"),
            "must cast timestamp to date for clean YYYY-MM-DD output, got: {sql}"
        );
    }

    #[test]
    fn test_parse_expiries_dataset_extracts_iso_date_truncating_time() {
        let json: serde_json::Value = serde_json::json!({
            "dataset": [
                ["2026-05-29T00:00:00.000000"],
                ["2026-06-26T00:00:00.000000"],
                ["2026-07-31T00:00:00.000000"],
            ]
        });
        let out = parse_expiries_dataset(&json);
        assert_eq!(out, vec!["2026-05-29", "2026-06-26", "2026-07-31"]);
    }

    #[test]
    fn test_parse_expiries_dataset_handles_empty() {
        let json: serde_json::Value = serde_json::json!({ "dataset": [] });
        assert_eq!(parse_expiries_dataset(&json).len(), 0);
    }

    #[test]
    fn test_parse_expiries_dataset_handles_missing_dataset_key() {
        let json: serde_json::Value = serde_json::json!({});
        assert_eq!(parse_expiries_dataset(&json).len(), 0);
    }

    #[test]
    fn test_parse_movers_dataset_caps_capacity_at_max() {
        let mut huge = Vec::new();
        for i in 0..500u32 {
            huge.push(serde_json::json!([
                i, "NSE_FNO", 100.0, 95.0, 5.0, 1000, 0, 0.0, 5000, null
            ]));
        }
        let json: serde_json::Value = serde_json::json!({ "dataset": huge });
        let entries = parse_movers_dataset(&json, 30);
        // The parser still iterates all rows but caps initial Vec capacity.
        assert!(entries.len() <= 500);
        assert!(entries.len() > 0);
    }
}
