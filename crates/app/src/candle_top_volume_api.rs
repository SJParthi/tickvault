//! Authenticated, read-only JSON access to the candle-derived RAM rankings.
//!
//! List reads are diagnostic and bounded to 250 rows. Winner reads use the
//! runtime's strict, fixed-work data-quality accessor and require an explicit
//! feed, session, universe generation, metric and exact retained bucket. Neither
//! route queries QuestDB or submits an order. The server supplies freshness.
//! Winner reads admit only V3 signed whole-bar volume. Diagnostic reads retain
//! their publication's explicit metric version and corresponding row fields.

#[cfg(test)]
use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tickvault_api::middleware::{ApiAuthConfig, require_bearer_auth};
use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::feed::Feed;
use tickvault_trading::candles::{CANDLE_OBSERVATION_WINDOW_POLICY, TfIndex};

use crate::bucket_top_volume::{
    BucketCoverage, BucketDecisionRefusal, BucketDecisionRequest, BucketRankedContract,
    BucketTopVolumeRuntime, BucketTopVolumeSnapshot, BucketVolumeMetric, BucketWinnerSnapshot,
    global_bucket_top_volume_runtime,
};
use crate::volume_leaderboard::{MAX_TRACKED_CONTRACTS, OptionFamily};

pub const MAX_TOP_VOLUME_PAGE_ROWS: usize = 250;
/// An HTTP caller may tighten freshness or select this bounded tolerance; it
/// cannot turn a prior-session publication into current data with a huge age.
pub const MAX_TOP_VOLUME_AGE_SECS: u32 = 30;

#[derive(Clone)]
enum RuntimeSource {
    Global,
    #[cfg(test)]
    Isolated(Arc<BucketTopVolumeRuntime>),
}

impl RuntimeSource {
    fn get(&self) -> &BucketTopVolumeRuntime {
        match self {
            Self::Global => global_bucket_top_volume_runtime(),
            #[cfg(test)]
            Self::Isolated(runtime) => runtime,
        }
    }

    /// A request for an old snapshot cannot self-certify the current universe.
    fn require_current_generation(&self, version: u64) -> Result<(), ApiError> {
        match self {
            Self::Global
                if crate::contract_underlying_map::global_contract_underlying_map()
                    .current_version()
                    != version =>
            {
                Err(ApiError::conflict(
                    "different_epoch",
                    "The selected instrument universe has changed.",
                ))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone)]
struct ApiState {
    runtime: RuntimeSource,
    now_ist_secs: fn() -> Option<u32>,
}

fn server_now_ist_secs() -> Option<u32> {
    let shifted = chrono::Utc::now()
        .timestamp()
        .checked_add(i64::from(IST_UTC_OFFSET_SECONDS))?;
    u32::try_from(shifted).ok()
}

/// Build routes for merging into the existing application router. Authentication
/// shares the existing rotation-aware token holder. A disabled/empty auth
/// configuration refuses requests instead of exposing financial data.
pub fn build_candle_top_volume_router(auth: ApiAuthConfig) -> Router {
    router_with_state(
        auth,
        ApiState {
            runtime: RuntimeSource::Global,
            now_ist_secs: server_now_ist_secs,
        },
    )
}

fn router_with_state(auth: ApiAuthConfig, state: ApiState) -> Router {
    let configured = auth.enabled;
    Router::new()
        .route("/api/top-volume", axum::routing::get(list_top_volume))
        .route(
            "/api/top-volume/winner",
            axum::routing::get(top_volume_winner),
        )
        .with_state(state)
        .route_layer(axum::middleware::from_fn_with_state(
            auth,
            require_bearer_auth,
        ))
        .route_layer(axum::middleware::from_fn_with_state(
            configured,
            require_configured_auth,
        ))
}

async fn require_configured_auth(
    State(configured): State<bool>,
    request: Request,
    next: Next,
) -> Response {
    if !configured {
        return ApiError::unavailable(
            "authentication_unavailable",
            "API authentication is not configured.",
        )
        .into_response();
    }
    next.run(request).await
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FamilyQuery {
    Stock,
    Index,
}

impl FamilyQuery {
    fn family(self) -> OptionFamily {
        match self {
            Self::Stock => OptionFamily::Stock,
            Self::Index => OptionFamily::Index,
        }
    }
}

const fn default_limit() -> usize {
    50
}

const fn default_max_age() -> u32 {
    2
}

const fn default_require_closed() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    family: FamilyQuery,
    timeframe: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
    bucket_start_secs: Option<u32>,
    session_day: Option<u32>,
    universe_version: Option<u64>,
    revision: Option<u64>,
    #[serde(default = "default_max_age")]
    max_age_secs: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WinnerQuery {
    family: FamilyQuery,
    timeframe: String,
    feed: String,
    session_day: u32,
    universe_version: u64,
    bucket_start_secs: u32,
    metric: String,
    #[serde(default = "default_max_age")]
    max_age_secs: u32,
    #[serde(default = "default_require_closed")]
    require_closed: bool,
}

/// The Top Volume registry is the admission list. No duration aliases or other frames
/// are accepted, so a selector cannot silently query a different candle grid.
fn parse_timeframe(value: &str) -> Result<TfIndex, ApiError> {
    TfIndex::TOP_VOLUME_ALL
        .into_iter()
        .find(|tf| tf.table_name().trim_start_matches("candles_") == value)
        .ok_or_else(|| {
            ApiError::bad_request(
                "invalid_timeframe",
                "Select 1s, 3s, 5s or 1m for Top Volume.",
            )
        })
}

fn parse_metric(value: &str) -> Result<BucketVolumeMetric, ApiError> {
    let metric = BucketVolumeMetric::SignedBarVolumeVsOneLotV3;
    if value == metric.as_str() {
        Ok(metric)
    } else {
        Err(ApiError::bad_request(
            "invalid_metric",
            "Specify signed_bar_volume_vs_one_lot_v3 for the signed-volume winner.",
        ))
    }
}

fn check_age_limit(max_age_secs: u32) -> Result<(), ApiError> {
    if max_age_secs > MAX_TOP_VOLUME_AGE_SECS {
        return Err(ApiError::bad_request(
            "invalid_max_age",
            "Maximum age must be between 0 and 30 seconds.",
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl ApiError {
    fn bad_request(code: &'static str, message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message,
        }
    }

    fn conflict(code: &'static str, message: &'static str) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code,
            message,
        }
    }

    fn unavailable(code: &'static str, message: &'static str) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code,
            message,
        }
    }
}

impl From<BucketDecisionRefusal> for ApiError {
    fn from(reason: BucketDecisionRefusal) -> Self {
        match reason {
            BucketDecisionRefusal::UnsupportedTimeframe => Self::bad_request(
                "invalid_timeframe",
                "Select 1s, 3s, 5s or 1m for Top Volume.",
            ),
            BucketDecisionRefusal::Unavailable => {
                Self::unavailable("unavailable", "No prepared winner is available.")
            }
            BucketDecisionRefusal::DifferentEpoch => Self::conflict(
                "different_epoch",
                "The feed, session, universe or metric has changed.",
            ),
            BucketDecisionRefusal::DifferentBucket => Self::conflict(
                "different_bucket",
                "The requested bucket is outside the published winner retention window.",
            ),
            BucketDecisionRefusal::IncompleteUniverse => Self::unavailable(
                "incomplete_universe",
                "Eligible observations do not cover the declared universe.",
            ),
            BucketDecisionRefusal::OpenBucket => Self::conflict(
                "open_bucket",
                "The requested closed-bucket requirement is not satisfied.",
            ),
            BucketDecisionRefusal::StalePublication => Self::unavailable(
                "stale_publication",
                "The prepared winner exceeds the allowed publication age.",
            ),
            BucketDecisionRefusal::StaleInstrument => Self::unavailable(
                "stale_instrument",
                "An instrument observation exceeds the allowed age.",
            ),
            BucketDecisionRefusal::FutureClock => Self::unavailable(
                "future_clock",
                "An observation or publication is ahead of the server clock.",
            ),
            BucketDecisionRefusal::Empty => {
                Self::unavailable("empty", "No eligible contract is present.")
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({ "error": { "code": self.code, "message": self.message } })),
        )
            .into_response()
    }
}

fn json_response(value: impl Serialize) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(value)).into_response()
}

#[derive(Debug, Serialize)]
struct CoverageResponse {
    expected_contracts: usize,
    observed_contracts: usize,
    eligible_contracts: usize,
    uncertain_contracts: usize,
    unavailable_net_contracts: usize,
    closed_contracts: usize,
    complete_for_declared_universe: bool,
    all_closed: bool,
    integrity_fault: bool,
}

impl From<BucketCoverage> for CoverageResponse {
    fn from(c: BucketCoverage) -> Self {
        Self {
            expected_contracts: c.expected_contracts,
            observed_contracts: c.observed_contracts,
            eligible_contracts: c.eligible_contracts,
            uncertain_contracts: c.uncertain_contracts,
            unavailable_net_contracts: c.unavailable_net_contracts,
            closed_contracts: c.closed_contracts,
            complete_for_declared_universe: c.complete_for_declared_universe(),
            all_closed: c.all_closed(),
            integrity_fault: c.integrity_fault,
        }
    }
}

#[derive(Debug, Serialize)]
struct FreshnessResponse {
    server_now_secs: u32,
    max_age_secs: u32,
    publication_age_secs: Option<u32>,
    oldest_instrument_age_secs: Option<u32>,
    newest_instrument_age_secs: Option<u32>,
    publication_within_age: bool,
    instruments_within_age: bool,
    future_clock: bool,
    same_server_session_day: bool,
}

#[derive(Debug, Serialize)]
struct BoardHeader {
    feed: &'static str,
    family: &'static str,
    timeframe: &'static str,
    metric_version: &'static str,
    metric_formula: &'static str,
    quantity_basis: &'static str,
    timestamp_axis: &'static str,
    integer_encoding: &'static str,
    display_rounding: &'static str,
    session_day: u32,
    universe_version: String,
    bucket_start_secs: u32,
    bucket_end_secs: u32,
    /// Actual local observation interval, distinct from the stable nominal grid.
    observation_window_end_secs: Option<u32>,
    closure_basis: &'static str,
    closure_finality: &'static str,
    revision: String,
    published_secs: u32,
    coverage: CoverageResponse,
    freshness: FreshnessResponse,
}

impl BoardHeader {
    fn from_winner(snapshot: &BucketWinnerSnapshot, now: u32, max_age: u32) -> Self {
        let coverage = snapshot.coverage;
        let publication_age = now.checked_sub(snapshot.published_secs);
        let oldest_age = coverage
            .oldest_observed_secs
            .and_then(|t| now.checked_sub(t));
        let newest_age = coverage
            .newest_observed_secs
            .and_then(|t| now.checked_sub(t));
        let future_clock = snapshot.published_secs > now
            || coverage.oldest_observed_secs.is_some_and(|t| t > now)
            || coverage.newest_observed_secs.is_some_and(|t| t > now);
        let (metric_formula, quantity_basis) = match snapshot.metric {
            BucketVolumeMetric::SignedBarVolumeVsOneLotV3 => (
                "100 * (volume / lot_size - 1)",
                "bar_close_vs_frozen_previous_close",
            ),
            BucketVolumeMetric::SignedEstimatedNetVsOneLotV2 => (
                "100 * (estimated_net_volume / lot_size - 1)",
                "candle_tick_rule_estimate",
            ),
            BucketVolumeMetric::GrossActivityVsOneLotV1 => {
                ("100 * (gross_volume / lot_size - 1)", "candle_gross_volume")
            }
        };
        Self {
            feed: snapshot.feed.as_str(),
            family: snapshot.family.as_str(),
            timeframe: snapshot.tf.table_name().trim_start_matches("candles_"),
            metric_version: snapshot.metric.as_str(),
            metric_formula,
            quantity_basis,
            timestamp_axis: "IST_wall_clock_epoch_seconds",
            integer_encoding: "ids_versions_and_quantities_are_decimal_strings",
            display_rounding: "three_decimals_truncated_toward_zero_exact_ratio_controls_rank",
            session_day: snapshot.session_day,
            universe_version: snapshot.universe_version.to_string(),
            bucket_start_secs: snapshot.bucket_start_secs,
            bucket_end_secs: snapshot.bucket_end_secs,
            observation_window_end_secs: snapshot
                .tf
                .observation_window_end(snapshot.bucket_start_secs),
            closure_basis: CANDLE_OBSERVATION_WINDOW_POLICY,
            closure_finality: "revisable_local_window_not_provider_finality",
            revision: snapshot.revision.to_string(),
            published_secs: snapshot.published_secs,
            coverage: coverage.into(),
            freshness: FreshnessResponse {
                server_now_secs: now,
                max_age_secs: max_age,
                publication_age_secs: publication_age,
                oldest_instrument_age_secs: oldest_age,
                newest_instrument_age_secs: newest_age,
                publication_within_age: publication_age.is_some_and(|age| age <= max_age),
                instruments_within_age: oldest_age.is_some_and(|age| age <= max_age)
                    && !future_clock,
                future_clock,
                same_server_session_day: now / 86_400 == snapshot.session_day,
            },
        }
    }

    fn from_board(snapshot: &BucketTopVolumeSnapshot, now: u32, max_age: u32) -> Self {
        // A fixed-size header copy, never a second runtime read: rows and
        // header therefore belong to the same retained publication revision.
        Self::from_winner(
            &BucketWinnerSnapshot {
                feed: snapshot.feed,
                family: snapshot.family,
                tf: snapshot.tf,
                session_day: snapshot.session_day,
                universe_version: snapshot.universe_version,
                metric: snapshot.metric,
                bucket_start_secs: snapshot.bucket_start_secs,
                bucket_end_secs: snapshot.bucket_end_secs,
                revision: snapshot.revision,
                published_secs: snapshot.published_secs,
                coverage: snapshot.coverage,
                winner: snapshot.first().copied(),
            },
            now,
            max_age,
        )
    }
}

#[derive(Debug, Serialize)]
struct QualityResponse {
    baseline_known: bool,
    counter_ambiguous: bool,
    volume_missing: bool,
    attribution_uncertain: bool,
    persisted_flags: u32,
}

#[derive(Debug, Serialize)]
struct RowResponse {
    rank: usize,
    security_id: String,
    segment: &'static str,
    underlying_id: String,
    lot_size: String,
    instrument_definition_version: String,
    #[serde(flatten)]
    quantity: RowQuantityResponse,
    /// Exact rational percentage. These are decimal integer strings so a
    /// JavaScript client need not round an i128 through IEEE-754 first.
    score_numerator: Option<String>,
    score_denominator: Option<String>,
    bucket_revision: String,
    last_observed_secs: u32,
    closed: bool,
    quality: QualityResponse,
}

/// A publication's metric version selects its wire vocabulary. Legacy
/// diagnostic data must never acquire V3 labels by passing through the API.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum RowQuantityResponse {
    SignedBar(SignedBarVolumeResponse),
    Legacy(LegacyVolumeResponse),
}

#[derive(Debug, Serialize)]
struct SignedBarVolumeResponse {
    /// One whole-bar magnitude with direction from the frozen previous close.
    /// Unknown direction is JSON null; a known unchanged close is "0".
    volume: Option<String>,
    signed_lots: Option<String>,
    volume_chg_pct: Option<String>,
}

/// Original fields retained only for explicitly versioned V1/V2 diagnostics.
#[derive(Debug, Serialize)]
struct LegacyVolumeResponse {
    gross_volume: String,
    net_volume: Option<String>,
    gross_lots: Option<String>,
    signed_net_lots: Option<String>,
    net_volume_chg_pct: Option<String>,
}

fn fixed_milli(magnitude: u128, negative: bool) -> String {
    let sign = if negative { "-" } else { "" };
    format!("{sign}{}.{:03}", magnitude / 1_000, magnitude % 1_000)
}

impl RowResponse {
    fn from_row(row: BucketRankedContract, rank: usize, metric: BucketVolumeMetric) -> Self {
        let ratio = row.score_ratio(metric);
        let signed_volume = row.estimated_net_volume.map(|volume| volume.to_string());
        let signed_lots = row
            .estimated_net_lots_milli()
            .map(|lots| fixed_milli(lots.unsigned_abs(), lots < 0));
        let volume_chg_pct = row
            .score_milli_pct(metric)
            .map(|score| fixed_milli(score.unsigned_abs(), score < 0));
        let quantity = match metric {
            BucketVolumeMetric::SignedBarVolumeVsOneLotV3 => {
                RowQuantityResponse::SignedBar(SignedBarVolumeResponse {
                    volume: signed_volume,
                    signed_lots,
                    volume_chg_pct,
                })
            }
            BucketVolumeMetric::SignedEstimatedNetVsOneLotV2
            | BucketVolumeMetric::GrossActivityVsOneLotV1 => {
                RowQuantityResponse::Legacy(LegacyVolumeResponse {
                    gross_volume: row.gross_volume.to_string(),
                    net_volume: signed_volume,
                    gross_lots: row.gross_lots_milli().map(|lots| fixed_milli(lots, false)),
                    signed_net_lots: signed_lots,
                    net_volume_chg_pct: volume_chg_pct,
                })
            }
        };
        Self {
            rank,
            security_id: row.security_id.to_string(),
            segment: row.segment.as_str(),
            underlying_id: row.underlying_id.to_string(),
            lot_size: row.lot_size.to_string(),
            instrument_definition_version: row.instrument_definition_version.to_string(),
            quantity,
            score_numerator: ratio.map(|(n, _)| n.to_string()),
            score_denominator: ratio.map(|(_, d)| d.to_string()),
            bucket_revision: row.revision.to_string(),
            last_observed_secs: row.last_observed_secs,
            closed: row.closed,
            quality: QualityResponse {
                baseline_known: row.quality.baseline_known,
                counter_ambiguous: row.quality.counter_ambiguous,
                volume_missing: row.quality.volume_missing,
                attribution_uncertain: row.quality.attribution_uncertain,
                persisted_flags: row.volume_quality,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct ListResponse {
    kind: &'static str,
    header: BoardHeader,
    total_eligible_rows: usize,
    offset: usize,
    returned_rows: usize,
    next_offset: Option<usize>,
    rows: Vec<RowResponse>,
}

#[derive(Debug, Serialize)]
struct WinnerResponse {
    kind: &'static str,
    data_checks_passed: bool,
    require_closed: bool,
    header: BoardHeader,
    row: RowResponse,
}

fn read_list(state: &ApiState, query: &ListQuery) -> Result<ListResponse, ApiError> {
    if query.limit == 0
        || query.limit > MAX_TOP_VOLUME_PAGE_ROWS
        || query.offset > MAX_TRACKED_CONTRACTS
    {
        return Err(ApiError::bad_request(
            "invalid_page",
            "Limit must be 1 to 250 and offset within the admitted universe bound.",
        ));
    }
    check_age_limit(query.max_age_secs)?;
    if query.offset != 0
        && (query.bucket_start_secs.is_none()
            || query.session_day.is_none()
            || query.universe_version.is_none()
            || query.revision.is_none())
    {
        return Err(ApiError::bad_request(
            "page_identity_required",
            "Subsequent pages must name the session, universe, bucket and revision from the first page.",
        ));
    }
    let tf = parse_timeframe(&query.timeframe)?;
    let now = (state.now_ist_secs)().ok_or_else(|| {
        ApiError::unavailable(
            "server_clock_out_of_range",
            "The server candle clock is unavailable.",
        )
    })?;
    let snapshot = state
        .runtime
        .get()
        .load(query.family.family(), tf)
        .ok_or_else(|| {
            ApiError::unavailable(
                "unavailable",
                "No materialized board is available for this family and timeframe.",
            )
        })?;
    state
        .runtime
        .require_current_generation(snapshot.universe_version)?;
    if query
        .bucket_start_secs
        .is_some_and(|start| start != snapshot.bucket_start_secs)
    {
        return Err(ApiError::conflict(
            "different_bucket",
            "This RAM endpoint retains only the latest published bucket.",
        ));
    }
    if query
        .session_day
        .is_some_and(|day| day != snapshot.session_day)
        || query
            .universe_version
            .is_some_and(|version| version != snapshot.universe_version)
    {
        return Err(ApiError::conflict(
            "different_epoch",
            "The requested session or universe is not the published one.",
        ));
    }
    if query
        .revision
        .is_some_and(|revision| revision != snapshot.revision)
    {
        return Err(ApiError::conflict(
            "different_revision",
            "The board changed; restart pagination from its new first page.",
        ));
    }
    let start = query.offset.min(snapshot.rows.len());
    let end = start.saturating_add(query.limit).min(snapshot.rows.len());
    let rows: Vec<_> = snapshot.rows[start..end]
        .iter()
        .copied()
        .enumerate()
        .map(|(index, row)| RowResponse::from_row(row, start + index + 1, snapshot.metric))
        .collect();
    Ok(ListResponse {
        kind: "diagnostic_board",
        header: BoardHeader::from_board(&snapshot, now, query.max_age_secs),
        total_eligible_rows: snapshot.rows.len(),
        offset: query.offset,
        returned_rows: rows.len(),
        next_offset: (end < snapshot.rows.len()).then_some(end),
        rows,
    })
}

fn read_winner(state: &ApiState, query: &WinnerQuery) -> Result<WinnerResponse, ApiError> {
    check_age_limit(query.max_age_secs)?;
    let tf = parse_timeframe(&query.timeframe)?;
    let feed = Feed::parse(&query.feed)
        .ok_or_else(|| ApiError::bad_request("invalid_feed", "Specify a supported feed label."))?;
    let metric = parse_metric(&query.metric)?;
    let now = (state.now_ist_secs)().ok_or_else(|| {
        ApiError::unavailable(
            "server_clock_out_of_range",
            "The server candle clock is unavailable.",
        )
    })?;
    let snapshot = state
        .runtime
        .get()
        .load_winner_for_decision(BucketDecisionRequest {
            feed,
            family: query.family.family(),
            tf,
            session_day: query.session_day,
            universe_version: query.universe_version,
            metric,
            bucket_start_secs: query.bucket_start_secs,
            now_secs: now,
            max_age_secs: query.max_age_secs,
            require_closed: query.require_closed,
        })?;
    state
        .runtime
        .require_current_generation(snapshot.universe_version)?;
    let row = snapshot
        .first()
        .copied()
        .ok_or_else(|| ApiError::from(BucketDecisionRefusal::Empty))?;
    Ok(WinnerResponse {
        kind: "prepared_winner",
        data_checks_passed: true,
        require_closed: query.require_closed,
        header: BoardHeader::from_winner(&snapshot, now, query.max_age_secs),
        row: RowResponse::from_row(row, 1, snapshot.metric),
    })
}

async fn list_top_volume(
    State(state): State<ApiState>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    read_list(&state, &query).map(json_response)
}

async fn top_volume_winner(
    State(state): State<ApiState>,
    Query(query): Query<WinnerQuery>,
) -> Result<Response, ApiError> {
    read_winner(&state, &query).map(json_response)
}

#[cfg(test)]
mod tests;
