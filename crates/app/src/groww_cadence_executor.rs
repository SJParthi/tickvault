//! The REAL **Groww** cadence executor — the Groww twin of
//! [`crate::dhan_cadence_executor`].
//!
//! Contract (cadence-error-codes.md §0b/§3b + the executor trait docs):
//!
//! - ONE bounded HTTP request per call — the RUNNER owns retries, the
//!   shape ladder, and pacing. The Groww lane is GATE-FREE BY
//!   CONSTRUCTION (no Groww rate rule is documented), so no executor arm
//!   ever touches the gate registry, and none may reference the shared
//!   Dhan Data-API limiter.
//! - Under the RS3 mutual exclusion (config.rs) the legacy per-minute
//!   Groww legs are OFF whenever the cadence scheduler is ON — this
//!   executor is then the SOLE author of `feed='groww'` rows in
//!   `spot_1m_rest` / `option_chain_1m`, and its persist-confirmed spot
//!   bars are the ONLY Groww feed for
//!   [`crate::rest_candle_fold::send_confirmed_bars`] (candles_* + the
//!   RAM stores).
//! - Token = the shared-minter SSM READ-ONLY [`GrowwTokenCache`]
//!   (`groww-shared-token-minter-2026-07-02.md`): NEVER minted, dropped
//!   on any 401/403 auth reject (the ~06:00 IST daily-reset arm), never
//!   logged, never in a URL.
//! - `fetch_expiry_list`: Groww has NO expirylist endpoint — the day's
//!   distinct sorted FUTURE option expiries are extracted from the daily
//!   instruments master (the `select_current_option_expiry` filter,
//!   §3e of cadence-error-codes.md), behind a tiny day-keyed cache of the
//!   RESOLVED lists (never the multi-MB rows). The cache lock is held
//!   ACROSS the download deliberately, serializing concurrent fires so
//!   one master download per day happens, not three.

use std::sync::Arc;

use chrono::NaiveDate;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    EXCHANGE_SEGMENT_IDX_I, GROWW_CHAIN_1M_UNDERLYINGS, GROWW_HISTORICAL_CANDLES_URL,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::types::SecurityId;
use tickvault_core::cadence::executor::{
    CadenceExecutor, CadenceFetchError, ChainFetchOk, ChainFetchRequest, ExpiryListRequest,
    SpotFetchRequest, SpotSnapshot, SpotTarget,
};
use tickvault_core::feed::groww::instruments::{GrowwInstrumentRow, stable_index_security_id};
use tickvault_core::instrument::index_extractor::canonicalize_index_symbol;
use tickvault_core::notification::NotificationService;
use tickvault_core::pipeline::chain_snapshot::ChainUnderlying;
use tickvault_storage::option_chain_1m_persistence::{
    OPTION_CHAIN_1M_FEED_GROWW, OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN, OptionChain1mRow,
    OptionChain1mWriter,
};
use tickvault_storage::rest_fetch_audit_persistence::{
    REST_FETCH_LEG_SPOT_1M, RestFetchAuditRow, RestFetchAuditWriter, RestFetchOutcome,
};
use tickvault_storage::spot_1m_rest_persistence::{
    SPOT_1M_REST_FEED_GROWW, SPOT_1M_REST_SEGMENT_IDX_I, SPOT_1M_REST_SOURCE_GROWW_CANDLES,
    Spot1mRestWriter,
};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::cadence_escalation::{
    EscalationLeg, LaneEscalation, TargetOutcome, emit_edge_action, emit_not_served,
    flush_off_worker, target_outcome_of,
};
use crate::dhan_cadence_executor::{date_to_yyyymmdd, deadline_remaining_ms, yyyymmdd_to_date};
use crate::groww_option_chain_1m_boot::{
    GrowwChainFetchFailure, build_chain_audit_row, chain_audit_append_best_effort,
    chain_audit_flush_best_effort, chain_error_class_for_status, download_master_bounded,
    groww_chain_fetch_once, groww_chain_url,
};
use crate::groww_spot_1m_boot::{
    FetchFailure, GrowwSpotTarget, GrowwTokenCache, audit_append_best_effort,
    audit_flush_best_effort, build_groww_spot_1m_row, core_spot_targets, groww_candles_query,
    groww_fetch_once, parse_groww_1m_candles, parse_groww_ga_failure, try_resolve_vix_target,
};
use crate::option_chain_1m_boot::{
    MoneynessWarnLatches, classify_chain_legs, fetched_at_ist_nanos_now,
    publish_chain_moneyness_snapshot, record_chain_moneyness_observability,
};
use crate::spot_1m_rest_boot::{
    ist_millis_of_day_now, minute_open_ist_nanos, stamp_held_ok_rows, today_ist,
};

/// Per-request HTTP client timeout, seconds — a BELT under the request
/// deadline (the deadline is the authority; the belt bounds a
/// black-holed socket). Same value as the Dhan cadence belt.
const GROWW_CADENCE_HTTP_TIMEOUT_SECS: u64 = 10;

/// Pure: map one Groww SPOT failure into the cadence taxonomy.
/// 429 → `RateLimited` (the SOLE ladder-arming class; Groww sends no
/// usable Retry-After through this seam → `None`); 401/403 → `Auth` (the
/// daily-reset class — the token cache re-reads SSM, never mints);
/// send-leg / any other non-2xx → `Transport` (non-arming — the safe
/// direction per the 2026-07-16 RateLimited-only correction).
#[must_use]
pub(crate) fn map_groww_spot_failure(f: &FetchFailure) -> CadenceFetchError {
    if f.rate_limited {
        return CadenceFetchError::RateLimited {
            retry_after_ms: None,
        };
    }
    if f.auth_rejected {
        return CadenceFetchError::Auth;
    }
    CadenceFetchError::Transport
}

/// Pure: map one Groww CHAIN failure into the cadence taxonomy — 429 is
/// checked FIRST (the sole arming class must never be masked by an
/// auth-shaped status).
#[must_use]
pub(crate) fn map_groww_chain_failure(f: &GrowwChainFetchFailure) -> CadenceFetchError {
    if f.rate_limited {
        return CadenceFetchError::RateLimited {
            retry_after_ms: None,
        };
    }
    if f.auth_rejected {
        return CadenceFetchError::Auth;
    }
    CadenceFetchError::Transport
}

/// Groww identity + moneyness-latch slot for a chain underlying — looked
/// up from the pinned [`GROWW_CHAIN_1M_UNDERLYINGS`] set by the plain
/// symbol. Returns `(slot, underlying, exchange, groww_symbol)`.
#[must_use]
pub fn groww_chain_identity(
    u: ChainUnderlying,
) -> Option<(usize, &'static str, &'static str, &'static str)> {
    GROWW_CHAIN_1M_UNDERLYINGS
        .into_iter()
        .enumerate()
        .find(|(_, (underlying, _, _))| *underlying == u.as_str())
        .map(|(slot, (underlying, exchange, groww_symbol))| {
            (slot, underlying, exchange, groww_symbol)
        })
}

/// Pure: the day's DISTINCT sorted FUTURE option expiries for one
/// underlying, extracted from the Groww instruments master with the SAME
/// filter as the legacy `select_current_option_expiry` (§3e — one
/// matcher, never a parallel one): `segment == "FNO"`, `CE`/`PE` rows,
/// exchange match, canonical-underlying match, ISO date parse, `>= today`.
/// Garbage dates skip silently (the master parse already counted them).
#[must_use]
pub fn groww_option_expiries(
    rows: &[GrowwInstrumentRow],
    exchange: &str,
    canonical_underlying: &str,
    today: NaiveDate,
) -> Vec<u32> {
    let mut dates: Vec<NaiveDate> = rows
        .iter()
        .filter(|r| {
            r.segment == "FNO"
                && (r.instrument_type == "CE" || r.instrument_type == "PE")
                && r.exchange == exchange
                && canonicalize_index_symbol(&r.underlying_symbol) == canonical_underlying
        })
        .filter_map(|r| NaiveDate::parse_from_str(r.expiry_date.trim(), "%Y-%m-%d").ok())
        .filter(|e| *e >= today)
        .collect();
    dates.sort_unstable();
    dates.dedup();
    dates.into_iter().map(date_to_yyyymmdd).collect()
}

/// Build the per-fetch `rest_fetch_audit` row for one cadence Groww SPOT
/// fire (the chain leg uses the module-shared
/// [`build_chain_audit_row`]). Pure.
#[allow(clippy::too_many_arguments)] // APPROVED: private forensics builder — a struct would be pure ceremony
fn build_groww_spot_audit_row(
    target_minute_ist_nanos: i64,
    trading_date_nanos: i64,
    security_id: i64,
    symbol: &'static str,
    attempts: i64,
    final_http_status: i64,
    rate_limited_count: i64,
    outcome: RestFetchOutcome,
    close_to_data_ms: i64,
    error_class: &'static str,
) -> RestFetchAuditRow {
    RestFetchAuditRow {
        close_to_persist_ms: -1,
        ts_ist_nanos: target_minute_ist_nanos,
        trading_date_ist_nanos: trading_date_nanos,
        feed: SPOT_1M_REST_FEED_GROWW,
        leg: REST_FETCH_LEG_SPOT_1M,
        security_id,
        exchange_segment: SPOT_1M_REST_SEGMENT_IDX_I,
        symbol,
        attempts,
        final_http_status,
        fetch_latency_ms: -1,
        close_to_data_ms,
        rate_limited_count,
        outcome,
        error_class,
    }
}

/// The REAL Groww cadence executor. Shared `&self` across the runner's
/// concurrent burst — the writers are `tokio::Mutex`-serialized (cold
/// path; lock order is always data-writer FIRST then audit-writer, never
/// both held at once — no deadlock shape).
pub struct GrowwCadenceExecutor {
    client: reqwest::Client,
    spot_writer: Mutex<Spot1mRestWriter>,
    chain_writer: Mutex<OptionChain1mWriter>,
    audit_writer: Mutex<RestFetchAuditWriter>,
    token_cache: Mutex<GrowwTokenCache>,
    moneyness_latches: Mutex<MoneynessWarnLatches>,
    /// Day-keyed cache of the RESOLVED expiry lists (one slot per pinned
    /// underlying) — the lock is held across the master download so
    /// concurrent fires serialize onto ONE download per day.
    expiry_cache: Mutex<Option<(NaiveDate, [Vec<u32>; 3])>>,
    /// Escalation-edge tally (fix round 2026-07-17): the legacy
    /// SPOT1M-01/CHAIN-02 3-consecutive-fully-failed-minutes paging edge,
    /// minute-bucketed executor-side (persist failure = failed minute).
    escalation: Mutex<LaneEscalation>,
    /// Typed Telegram sink for the escalation/recovery events (`None` in
    /// tests — the coded `error!` lines still fire).
    notifier: Option<Arc<NotificationService>>,
    /// Order-runtime mark tap (2026-07-18 — re-homed a SECOND time, from
    /// the stood-down legacy Groww per-minute legs to this executor's
    /// spot persist-confirm seam; PR #1624's cutover left the mark
    /// channel producer-less). GROWW LANE ONLY: the Dhan cadence
    /// executor must NEVER carry this tap — Dhan spot sids (13/25/51
    /// IDX_I) are a DIFFERENT id space than the Groww-native u64s
    /// (bit-62 `stable_index_security_id`) the paper book keys on, so
    /// cross-feeding would double-key the same instrument invisibly to
    /// the first-seen-SEGMENT tripwire (segments match across the
    /// split). `None` ⇒ `[order_runtime]` disabled — zero work.
    mark_forwarder: Option<crate::order_runtime::MarkForwarder>,
}

impl GrowwCadenceExecutor {
    /// Build the executor. `Err` = the HTTP client could not be built
    /// (HTTP-CLIENT-01 class — the caller degrades loudly; NEVER a
    /// `Client::new()` panic fallback).
    // TEST-EXEMPT: thin constructor — client build is covered upstream; the fetch behavior is exercised via the mapping/ordering tests below.
    pub fn new(
        questdb: &QuestDbConfig,
        notifier: Option<Arc<NotificationService>>,
        mark_forwarder: Option<crate::order_runtime::MarkForwarder>,
    ) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(
                GROWW_CADENCE_HTTP_TIMEOUT_SECS,
            ))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("groww cadence HTTP client build failed: {e}"))?;
        Ok(Self {
            client,
            spot_writer: Mutex::new(Spot1mRestWriter::new_with_feed(
                questdb,
                SPOT_1M_REST_FEED_GROWW,
                SPOT_1M_REST_SOURCE_GROWW_CANDLES,
            )),
            chain_writer: Mutex::new(OptionChain1mWriter::new_with_feed(
                questdb,
                OPTION_CHAIN_1M_FEED_GROWW,
                OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN,
            )),
            audit_writer: Mutex::new(RestFetchAuditWriter::new(questdb)),
            token_cache: Mutex::new(GrowwTokenCache::new()),
            moneyness_latches: Mutex::new(MoneynessWarnLatches::default()),
            expiry_cache: Mutex::new(None),
            escalation: Mutex::new(LaneEscalation::new(Feed::Groww)),
            notifier,
            mark_forwarder,
        })
    }

    /// Test-only constructor with an injected token cache and lazy-failed
    /// writers (no live QuestDB in unit tests).
    #[cfg(test)]
    pub(crate) fn for_test(token_cache: GrowwTokenCache) -> Self {
        Self {
            client: reqwest::Client::new(),
            spot_writer: Mutex::new(Spot1mRestWriter::for_test_with_feed(
                SPOT_1M_REST_FEED_GROWW,
                SPOT_1M_REST_SOURCE_GROWW_CANDLES,
            )),
            chain_writer: Mutex::new(OptionChain1mWriter::for_test_with_feed(
                OPTION_CHAIN_1M_FEED_GROWW,
                OPTION_CHAIN_1M_SOURCE_GROWW_CHAIN,
            )),
            audit_writer: Mutex::new(RestFetchAuditWriter::for_test()),
            token_cache: Mutex::new(token_cache),
            moneyness_latches: Mutex::new(MoneynessWarnLatches::default()),
            expiry_cache: Mutex::new(None),
            escalation: Mutex::new(LaneEscalation::new(Feed::Groww)),
            notifier: None,
            mark_forwarder: None,
        }
    }

    /// Record one leg outcome into the lane escalation tally; when the
    /// outcome rolls the minute bucket, emit the FINALIZED minute's edge
    /// action (the legacy SPOT1M-01/CHAIN-02 escalation/recovery contract
    /// — fix round 2026-07-17) PLUS the previous minute's per-target
    /// NOT-SERVED verdicts (fix round 2 — the legacy Groww
    /// `stage="underlying_not_served"` pager on the CHAIN leg; the Groww
    /// SPOT leg deliberately passes `target = None` — the legacy leg had
    /// only the VIX-specific arms, no per-SID detector).
    /// `core = false` marks the INDIA VIX target: the Groww SPOT
    /// escalation edge keys on the 3 CORE indices only (the legacy
    /// `groww_spot_1m_boot.rs::MinuteEdgeTally` semantics — a VIX success
    /// must never mask core-all-failed, a VIX-only failure never pages).
    async fn record_leg_outcome(
        &self,
        leg: EscalationLeg,
        minute_secs: u32,
        ok: bool,
        core: bool,
        target: Option<(&'static str, SecurityId, TargetOutcome)>,
    ) {
        let today = today_ist();
        let (rolled_from, finalized, not_served) = {
            let mut esc = self.escalation.lock().await;
            let rolled_from = esc.roll_day_if_needed(today);
            let finalized = esc.record(leg, minute_secs, ok, core);
            let not_served = target.and_then(|(symbol, sid, outcome)| {
                esc.record_target(leg, minute_secs, symbol, sid, outcome)
            });
            (rolled_from, finalized, not_served)
        };
        if let Some(old_day) = rolled_from {
            info!(
                feed = "groww",
                old_day = %old_day,
                new_day = %today,
                "groww cadence escalation: IST day rolled — escalation edges + \
                 not-served streaks reset for the fresh day"
            );
        }
        if let Some((minute, action)) = finalized {
            emit_edge_action(Feed::Groww, leg, minute, action, self.notifier.as_ref());
        }
        if let Some((minute, emits)) = not_served {
            emit_not_served(Feed::Groww, leg, minute, &emits, self.notifier.as_ref());
        }
    }

    /// Resolve the Groww identity for a cadence spot target. INDIA VIX is
    /// RUNTIME-resolved from the day's watch file (§38.7 — never a
    /// guessed literal); unresolved VIX counts + degrades to `None` (the
    /// caller returns an honest `Empty`).
    fn spot_target_for(target: SpotTarget, trading_date: NaiveDate) -> Option<GrowwSpotTarget> {
        if matches!(target, SpotTarget::IndiaVix) {
            let resolved = try_resolve_vix_target(trading_date);
            if resolved.is_none() {
                metrics::counter!("tv_groww_spot1m_vix_unresolved_total").increment(1);
                debug!("groww cadence: INDIA VIX unresolved from the day's watch file");
            }
            return resolved;
        }
        core_spot_targets()
            .into_iter()
            .find(|t| t.symbol == target.as_str())
    }
}

impl CadenceExecutor for GrowwCadenceExecutor {
    // TEST-EXEMPT: live-HTTP orchestration — every decision leg (identity map, deadline math, failure taxonomy, fold-after-ACK ordering) is a pure fn / source-order ratchet unit-tested below; the HTTP inner is the tested legacy fetch fn.
    async fn fetch_spot(&self, req: SpotFetchRequest) -> Result<SpotSnapshot, CadenceFetchError> {
        let escalation_minute_secs = req.cycle_minute_ist;
        let mut escalation_persist_ok = true;
        let result = async {
        let trading_date = today_ist();
        let Some(target) = Self::spot_target_for(req.target, trading_date) else {
            // Unresolved VIX (counted above) — an honest Empty, never a
            // guessed identity; the 3 core targets are unaffected.
            return Err(CadenceFetchError::Empty);
        };
        let Some(remaining_ms) =
            deadline_remaining_ms(req.deadline_epoch_ms, chrono::Utc::now().timestamp_millis())
        else {
            return Err(CadenceFetchError::Timeout);
        };
        let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
        let target_nanos = minute_open_ist_nanos(trading_date, req.cycle_minute_ist);
        let symbol = target.symbol;
        let security_id = target.security_id;
        let token = {
            let mut cache = self.token_cache.lock().await;
            cache.ensure_token().await
        };
        let Some(token) = token else {
            let mut audit = self.audit_writer.lock().await;
            audit_append_best_effort(
                &mut audit,
                &build_groww_spot_audit_row(
                    target_nanos,
                    trading_date_nanos,
                    security_id,
                    symbol,
                    0,
                    0,
                    0,
                    RestFetchOutcome::NoToken,
                    -1,
                    "no_token",
                ),
            );
            flush_off_worker(|| audit_flush_best_effort(&mut audit));
            return Err(CadenceFetchError::Auth);
        };
        let query = groww_candles_query(
            &target.groww_symbol,
            &target.exchange,
            &target.segment,
            trading_date,
        );
        let fetched = tokio::time::timeout(
            std::time::Duration::from_millis(remaining_ms),
            groww_fetch_once(&self.client, GROWW_HISTORICAL_CANDLES_URL, &query, &token),
        )
        .await;
        let body = match fetched {
            Err(_elapsed) => {
                let mut audit = self.audit_writer.lock().await;
                audit_append_best_effort(
                    &mut audit,
                    &build_groww_spot_audit_row(
                        target_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        1,
                        0,
                        0,
                        RestFetchOutcome::Error,
                        -1,
                        "timeout",
                    ),
                );
                flush_off_worker(|| audit_flush_best_effort(&mut audit));
                return Err(CadenceFetchError::Timeout);
            }
            Ok(Err(failure)) => {
                if failure.auth_rejected {
                    // Drop the cached token so the NEXT fire re-reads SSM
                    // (the ~06:00 IST daily-reset arm — NEVER a mint).
                    self.token_cache.lock().await.note_auth_rejected();
                }
                let mapped = map_groww_spot_failure(&failure);
                debug!(
                    target = symbol,
                    outcome = mapped.as_str(),
                    msg = %failure.msg,
                    "groww cadence spot fetch failed (runner owns retry/ladder)"
                );
                let (audit_outcome, class) = if failure.rate_limited {
                    (RestFetchOutcome::RateLimited, "rate_limited")
                } else if failure.auth_rejected {
                    (RestFetchOutcome::Error, "auth")
                } else {
                    (RestFetchOutcome::Error, "error")
                };
                let mut audit = self.audit_writer.lock().await;
                audit_append_best_effort(
                    &mut audit,
                    &build_groww_spot_audit_row(
                        target_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        1,
                        i64::from(failure.status),
                        i64::from(failure.rate_limited),
                        audit_outcome,
                        -1,
                        class,
                    ),
                );
                flush_off_worker(|| audit_flush_best_effort(&mut audit));
                return Err(mapped);
            }
            Ok(Ok(body)) => body,
        };
        // G1 (2026-07-14): a 2xx carrying the Groww FAILURE envelope is an
        // ERROR, never a benign empty.
        if parse_groww_ga_failure(&body).is_some() {
            let mut audit = self.audit_writer.lock().await;
            audit_append_best_effort(
                &mut audit,
                &build_groww_spot_audit_row(
                    target_nanos,
                    trading_date_nanos,
                    security_id,
                    symbol,
                    1,
                    200,
                    0,
                    RestFetchOutcome::Error,
                    -1,
                    "ga_failure",
                ),
            );
            flush_off_worker(|| audit_flush_best_effort(&mut audit));
            return Err(CadenceFetchError::Transport);
        }
        let (candles, _stats) = parse_groww_1m_candles(&body);
        let Some(candle) = candles
            .iter()
            .find(|c| c.minute_ts_ist_nanos == target_nanos)
            .copied()
        else {
            let mut audit = self.audit_writer.lock().await;
            audit_append_best_effort(
                &mut audit,
                &build_groww_spot_audit_row(
                    target_nanos,
                    trading_date_nanos,
                    security_id,
                    symbol,
                    1,
                    200,
                    0,
                    RestFetchOutcome::Empty,
                    -1,
                    "empty",
                ),
            );
            flush_off_worker(|| audit_flush_best_effort(&mut audit));
            return Err(CadenceFetchError::Empty);
        };
        let close_to_data_ms =
            (ist_millis_of_day_now() - (i64::from(req.cycle_minute_ist) + 60) * 1000).max(0);
        // Persist → flush ACK → fold handoff (STRICT ordering: an
        // unpersisted bar must never derive candles — the fold contract).
        let flush_ok = {
            let mut writer = self.spot_writer.lock().await;
            let row = build_groww_spot_1m_row(
                &candle,
                security_id,
                symbol,
                trading_date_nanos,
                close_to_data_ms,
            );
            if let Err(err) = writer.append_row(&row) {
                metrics::counter!("tv_groww_spot1m_persist_errors_total", "stage" => "append")
                    .increment(1);
                error!(
                    code = ErrorCode::Spot1m02PersistFailed.code_str(),
                    stage = "append",
                    feed = SPOT_1M_REST_FEED_GROWW,
                    security_id,
                    ?err,
                    "SPOT1M-02: cadence groww spot_1m_rest row append failed"
                );
                false
            } else {
                match flush_off_worker(|| writer.flush()) {
                    Ok(()) => true,
                    Err(err) => {
                        metrics::counter!(
                            "tv_groww_spot1m_persist_errors_total", "stage" => "flush"
                        )
                        .increment(1);
                        error!(
                            code = ErrorCode::Spot1m02PersistFailed.code_str(),
                            stage = "flush",
                            feed = SPOT_1M_REST_FEED_GROWW,
                            ?err,
                            "SPOT1M-02: cadence groww spot_1m_rest ILP flush failed — \
                             pending rows discarded (poisoned-buffer defense)"
                        );
                        false
                    }
                }
            }
        };
        if flush_ok {
            // The per-fire liveness heartbeat (feeds the
            // market-hours-liveness alarm) — PERSIST-gated (fix round 2,
            // the M1 fetch-ok-but-lost-is-not-ok rule): set ONLY after the
            // spot_1m_rest flush ACK. An all-failing session (fetch OR
            // persist) leaves the gauge unset so
            // tv-<env>-market-hours-liveness-missing pages (~09:25 IST);
            // mid-session death after first success remains the
            // pre-existing documented residual. Never pre-registered at
            // boot: the first in-session set IS the signal.
            metrics::gauge!("tv_rest_1m_fire_heartbeat").set(1.0);
            // ONLY after the flush ACK — the sole Groww candles_*/RAM-store
            // feed on the cadence runtime.
            if let Ok(sid_u64) = u64::try_from(security_id) {
                crate::rest_candle_fold::send_confirmed_bars(&[
                    crate::rest_candle_fold::ConfirmedBar::from_minute_candle(
                        Feed::Groww,
                        sid_u64,
                        EXCHANGE_SEGMENT_IDX_I,
                        &candle,
                    ),
                ]);
                // Order-runtime mark tap (2026-07-18 — re-homed from the
                // stood-down legacy spot leg's persist-confirm site):
                // forward the OWN-FIRE just-closed close ONLY after the
                // flush ACK (a mark must never reference a price the
                // audit record does not back — the legacy
                // groww_spot_1m_boot.rs gating mirrored verbatim).
                // GROWW LANE ONLY — never the Dhan executor: Dhan sids
                // (13/25/51) are a different id space than the
                // Groww-native u64s the paper book keys on;
                // cross-feeding would double-key instruments invisibly
                // to the first-seen-segment tripwire. Best-effort
                // try_send; a full channel drops the mark (counted —
                // the next minute close supersedes it); None ⇒ runtime
                // disabled, zero work.
                if let Some(forwarder) = self.mark_forwarder.as_ref() {
                    #[allow(clippy::cast_possible_truncation)]
                    // APPROVED: MarkUpdate carries f32 by contract (the
                    // wire LTP precision); price-level narrowing only.
                    forwarder.mark_forward(
                        sid_u64,
                        EXCHANGE_SEGMENT_IDX_I,
                        candle.close as f32,
                    );
                }
            } else {
                // Defensive: stable_index_security_id is bit-62 positive
                // by construction — a negative id here is a code bug.
                warn!(
                    security_id,
                    "groww cadence: spot security_id does not fit the fold id space — bar not folded"
                );
            }
        }
        metrics::counter!("tv_groww_spot1m_fetch_total", "outcome" => "ok").increment(1);
        #[allow(clippy::cast_precision_loss)] // APPROVED: histogram sample only
        metrics::histogram!("tv_groww_spot1m_close_to_data_ms").record(close_to_data_ms as f64);
        {
            let mut audit = self.audit_writer.lock().await;
            if flush_ok {
                for row in stamp_held_ok_rows(
                    vec![build_groww_spot_audit_row(
                        candle.minute_ts_ist_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        1,
                        200,
                        0,
                        RestFetchOutcome::Ok,
                        close_to_data_ms,
                        "none",
                    )],
                    true,
                    trading_date_nanos,
                    ist_millis_of_day_now(),
                ) {
                    audit_append_best_effort(&mut audit, &row);
                }
            } else {
                // Fetched-but-lost — a persist failure, never dressed as
                // vendor absence.
                audit_append_best_effort(
                    &mut audit,
                    &build_groww_spot_audit_row(
                        candle.minute_ts_ist_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        1,
                        200,
                        0,
                        RestFetchOutcome::NamedGap,
                        -1,
                        "persist_failed",
                    ),
                );
            }
            flush_off_worker(|| audit_flush_best_effort(&mut audit));
        }
        escalation_persist_ok = flush_ok;
        Ok(SpotSnapshot {
            price: candle.close,
            source_minute_ist: req.cycle_minute_ist,
            received_at_epoch_ms: chrono::Utc::now().timestamp_millis(),
        })
        }
        .await;
        // Escalation edge (fix round 2026-07-17): a persist failure counts
        // as a FAILED minute (fetch-ok-but-lost is not ok — the M1 rule).
        // No per-SID not-served verdict on the Groww spot leg (the legacy
        // leg had only the VIX arms — no invented semantics). The edge is
        // CORE-keyed: an INDIA VIX outcome is excluded from the tally (the
        // legacy `MinuteEdgeTally` semantics — hostile round 3, MEDIUM).
        self.record_leg_outcome(
            EscalationLeg::Spot,
            escalation_minute_secs,
            result.is_ok() && escalation_persist_ok,
            !matches!(req.target, SpotTarget::IndiaVix),
            None,
        )
        .await;
        result
    }

    // TEST-EXEMPT: live-HTTP orchestration — decision legs are pure fns unit-tested below; parse/classify/publish/persist are the tested legacy building blocks.
    async fn fetch_chain(&self, req: ChainFetchRequest) -> Result<ChainFetchOk, CadenceFetchError> {
        let escalation_minute_secs = req.cycle_minute_ist;
        let mut escalation_persist_ok = true;
        let result = async {
            let Some((slot, underlying, exchange, groww_symbol)) =
                groww_chain_identity(req.underlying)
            else {
                return Err(CadenceFetchError::Malformed);
            };
            let Some(remaining_ms) =
                deadline_remaining_ms(req.deadline_epoch_ms, chrono::Utc::now().timestamp_millis())
            else {
                return Err(CadenceFetchError::Timeout);
            };
            // The runner NEVER guesses an expiry; with no day-locked winner
            // the fire is an honest Empty (non-arming).
            let Some(expiry_date) = req.expiry_yyyymmdd.and_then(yyyymmdd_to_date) else {
                debug!(
                    underlying,
                    expiry = ?req.expiry_yyyymmdd,
                    "groww cadence chain fire without a resolved expiry — Empty"
                );
                return Err(CadenceFetchError::Empty);
            };
            let trading_date = today_ist();
            let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
            let target_nanos = minute_open_ist_nanos(trading_date, req.cycle_minute_ist);
            let security_id = stable_index_security_id(groww_symbol);
            // The pinned symbol is &'static — reuse the const entry so the
            // audit row's `symbol: &'static str` holds.
            let symbol: &'static str = underlying;
            let token = {
                let mut cache = self.token_cache.lock().await;
                cache.ensure_token().await
            };
            let Some(token) = token else {
                let mut audit = self.audit_writer.lock().await;
                chain_audit_append_best_effort(
                    &mut audit,
                    &build_chain_audit_row(
                        target_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        0,
                        0,
                        -1,
                        -1,
                        0,
                        RestFetchOutcome::NoToken,
                        "no_token",
                    ),
                );
                flush_off_worker(|| chain_audit_flush_best_effort(&mut audit));
                return Err(CadenceFetchError::Auth);
            };
            let url = groww_chain_url(exchange, underlying);
            let expiry_str = expiry_date.format("%Y-%m-%d").to_string();
            let fetched = tokio::time::timeout(
                std::time::Duration::from_millis(remaining_ms),
                groww_chain_fetch_once(&self.client, &url, &expiry_str, &token),
            )
            .await;
            let body_text = match fetched {
                Err(_elapsed) => {
                    let mut audit = self.audit_writer.lock().await;
                    chain_audit_append_best_effort(
                        &mut audit,
                        &build_chain_audit_row(
                            target_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
                            0,
                            -1,
                            -1,
                            0,
                            RestFetchOutcome::Error,
                            "timeout",
                        ),
                    );
                    flush_off_worker(|| chain_audit_flush_best_effort(&mut audit));
                    return Err(CadenceFetchError::Timeout);
                }
                Ok(Err(failure)) => {
                    if failure.auth_rejected {
                        self.token_cache.lock().await.note_auth_rejected();
                    }
                    let mapped = map_groww_chain_failure(&failure);
                    debug!(
                        underlying,
                        outcome = mapped.as_str(),
                        msg = %failure.msg,
                        "groww cadence chain fetch failed (runner owns retry/ladder)"
                    );
                    let audit_outcome = if failure.rate_limited {
                        RestFetchOutcome::RateLimited
                    } else {
                        RestFetchOutcome::Error
                    };
                    let mut audit = self.audit_writer.lock().await;
                    chain_audit_append_best_effort(
                        &mut audit,
                        &build_chain_audit_row(
                            target_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
                            i64::from(failure.status),
                            -1,
                            -1,
                            i64::from(failure.rate_limited),
                            audit_outcome,
                            chain_error_class_for_status(failure.status),
                        ),
                    );
                    flush_off_worker(|| chain_audit_flush_best_effort(&mut audit));
                    return Err(mapped);
                }
                Ok(Ok(text)) => text,
            };
            let Some(chain) =
                crate::groww_option_chain_1m_boot::parse_groww_option_chain(&body_text)
            else {
                let mut audit = self.audit_writer.lock().await;
                chain_audit_append_best_effort(
                    &mut audit,
                    &build_chain_audit_row(
                        target_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        1,
                        200,
                        -1,
                        -1,
                        0,
                        RestFetchOutcome::Error,
                        "parse",
                    ),
                );
                flush_off_worker(|| chain_audit_flush_best_effort(&mut audit));
                return Err(CadenceFetchError::Malformed);
            };
            if chain.legs.is_empty() {
                // The 2026-07-14 empty-vs-drift split: entries our extraction
                // dropped = an ERROR (leg_shape_drift), a literally-empty map
                // = an honest Empty.
                let (audit_outcome, class, mapped) = if chain.strikes_seen > 0 {
                    (
                        RestFetchOutcome::Error,
                        "leg_shape_drift",
                        CadenceFetchError::Malformed,
                    )
                } else {
                    (
                        RestFetchOutcome::Empty,
                        "empty_chain",
                        CadenceFetchError::Empty,
                    )
                };
                let mut audit = self.audit_writer.lock().await;
                chain_audit_append_best_effort(
                    &mut audit,
                    &build_chain_audit_row(
                        target_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        1,
                        200,
                        -1,
                        -1,
                        0,
                        audit_outcome,
                        class,
                    ),
                );
                flush_off_worker(|| chain_audit_flush_best_effort(&mut audit));
                return Err(mapped);
            }
            let close_to_data_ms =
                (ist_millis_of_day_now() - (i64::from(req.cycle_minute_ist) + 60) * 1000).max(0);
            // A vendor-omitted underlying_ltp maps to 0.0 for classification
            // (every row UNKNOWN — fail-soft, never a dropped row).
            let moneyness_spot = if chain.underlying_ltp_missing {
                0.0
            } else {
                chain.underlying_ltp
            };
            // Classify ONCE per fire; the RAM snapshot rows are built BEFORE
            // the persist loop (the legacy Found-arm ordering).
            let cls = classify_chain_legs(
                underlying,
                moneyness_spot,
                chain.legs.iter().map(|l| (l.strike, l.leg, l.ltp)),
            );
            {
                let mut latches = self.moneyness_latches.lock().await;
                let minute_label = format!(
                    "{:02}:{:02}",
                    req.cycle_minute_ist / 3600,
                    (req.cycle_minute_ist % 3600) / 60
                );
                record_chain_moneyness_observability(
                    OPTION_CHAIN_1M_FEED_GROWW,
                    underlying,
                    slot,
                    trading_date,
                    &minute_label,
                    &cls,
                    &mut latches,
                );
            }
            let expiry_nanos = minute_open_ist_nanos(expiry_date, 0);
            let fetched_at = fetched_at_ist_nanos_now();
            let mut persist_failed = false;
            {
                let mut writer = self.chain_writer.lock().await;
                for ((leg, leg_moneyness), leg_depth) in chain
                    .legs
                    .iter()
                    .zip(cls.row_moneyness.iter())
                    .zip(cls.row_depth.iter())
                {
                    let row = OptionChain1mRow {
                        ts_ist_nanos: target_nanos,
                        trading_date_ist_nanos: trading_date_nanos,
                        underlying_security_id: security_id,
                        underlying_symbol: symbol,
                        expiry_ist_nanos: expiry_nanos,
                        strike: leg.strike,
                        leg: leg.leg,
                        contract_security_id: 0,
                        last_price: leg.ltp,
                        iv: leg.iv,
                        delta: leg.delta,
                        theta: leg.theta,
                        gamma: leg.gamma,
                        vega: leg.vega,
                        oi: leg.oi,
                        volume: leg.volume,
                        previous_oi: 0,
                        // RAW vendor value (0.0 = omitted — the
                        // underlying_ltp_missing forensics convention).
                        underlying_spot: chain.underlying_ltp,
                        fetched_at_ist_nanos: fetched_at,
                        moneyness: leg_moneyness.as_str(),
                        // Signed depth (2026-07-17 merge) — classified from
                        // the GUARDED moneyness_spot above; None (→ NULL)
                        // whenever that spot / the strike were invalid.
                        moneyness_depth: *leg_depth,
                    };
                    if let Err(err) =
                        writer.append_row_ext(&row, Some(leg.rho), Some(close_to_data_ms))
                    {
                        persist_failed = true;
                        metrics::counter!(
                            "tv_groww_chain1m_persist_errors_total", "stage" => "append"
                        )
                        .increment(1);
                        error!(
                            code = ErrorCode::Chain03PersistFailed.code_str(),
                            stage = "append",
                            feed = OPTION_CHAIN_1M_FEED_GROWW,
                            underlying,
                            ?err,
                            "CHAIN-03: cadence groww option_chain_1m row append failed"
                        );
                        break;
                    }
                }
                if !persist_failed && let Err(err) = flush_off_worker(|| writer.flush()) {
                    persist_failed = true;
                    metrics::counter!("tv_groww_chain1m_persist_errors_total", "stage" => "flush")
                        .increment(1);
                    error!(
                        code = ErrorCode::Chain03PersistFailed.code_str(),
                        stage = "flush",
                        feed = OPTION_CHAIN_1M_FEED_GROWW,
                        ?err,
                        "CHAIN-03: cadence groww option_chain_1m ILP flush failed — \
                     pending rows discarded (poisoned-buffer defense)"
                    );
                }
            }
            {
                let mut audit = self.audit_writer.lock().await;
                if persist_failed {
                    chain_audit_append_best_effort(
                        &mut audit,
                        &build_chain_audit_row(
                            target_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
                            200,
                            -1,
                            -1,
                            0,
                            RestFetchOutcome::NamedGap,
                            "persist_failed",
                        ),
                    );
                } else {
                    for row in stamp_held_ok_rows(
                        vec![build_chain_audit_row(
                            target_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
                            200,
                            -1,
                            close_to_data_ms,
                            0,
                            RestFetchOutcome::Ok,
                            "none",
                        )],
                        true,
                        trading_date_nanos,
                        ist_millis_of_day_now(),
                    ) {
                        chain_audit_append_best_effort(&mut audit, &row);
                    }
                }
                flush_off_worker(|| chain_audit_flush_best_effort(&mut audit));
            }
            let underlying_spot = (!chain.underlying_ltp_missing
                && chain.underlying_ltp.is_finite()
                && chain.underlying_ltp > 0.0)
                .then_some(chain.underlying_ltp);
            // Publish is persist-independent (the RAM decision surface is
            // never degraded by a QuestDB outage).
            publish_chain_moneyness_snapshot(
                Feed::Groww,
                underlying,
                target_nanos,
                fetched_at,
                moneyness_spot,
                expiry_nanos,
                cls,
            );
            metrics::counter!("tv_groww_chain1m_fetch_total", "outcome" => "ok").increment(1);
            #[allow(clippy::cast_precision_loss)] // APPROVED: histogram sample only
            metrics::histogram!("tv_groww_chain1m_close_to_data_ms")
                .record(close_to_data_ms as f64);
            escalation_persist_ok = !persist_failed;
            Ok(ChainFetchOk {
                underlying_spot,
                published_to_registry: true,
            })
        }
        .await;
        // Escalation edge (fix round 2026-07-17): persist failure = failed
        // minute (the M1 rule). Not-served verdict (fix round 2):
        // FETCH-level per-underlying — an UNRESOLVED-expiry Empty is a
        // per-target HOLD (an expiry-resolution failure is never a
        // vendor-not-serving verdict; the legacy expiry_unresolved arm
        // never fed the sink).
        let expiry_unresolved = req.expiry_yyyymmdd.and_then(yyyymmdd_to_date).is_none();
        let target = if expiry_unresolved && matches!(result, Err(CadenceFetchError::Empty)) {
            None
        } else {
            target_outcome_of(&result).and_then(|outcome| {
                groww_chain_identity(req.underlying)
                    .map(|(_, underlying, _, _)| (underlying, 0, outcome))
            })
        };
        self.record_leg_outcome(
            EscalationLeg::Chain,
            escalation_minute_secs,
            result.is_ok() && escalation_persist_ok,
            true,
            target,
        )
        .await;
        result
    }

    // TEST-EXEMPT: live-HTTP orchestration — the master-derived expiry extraction is the pure fn `groww_option_expiries` unit-tested below; the download inner is the tested legacy bounded fn.
    async fn fetch_expiry_list(
        &self,
        req: ExpiryListRequest,
    ) -> Result<Vec<u32>, CadenceFetchError> {
        let Some((slot, _underlying, _exchange, _groww_symbol)) =
            groww_chain_identity(req.underlying)
        else {
            return Err(CadenceFetchError::Malformed);
        };
        let Some(remaining_ms) =
            deadline_remaining_ms(req.deadline_epoch_ms, chrono::Utc::now().timestamp_millis())
        else {
            return Err(CadenceFetchError::Timeout);
        };
        let today = today_ist();
        // The lock is held ACROSS the download deliberately — concurrent
        // per-underlying fires serialize onto ONE master download per day.
        let mut cache = self.expiry_cache.lock().await;
        if let Some((day, lists)) = cache.as_ref()
            && *day == today
        {
            return Ok(lists[slot].clone());
        }
        let rows = match tokio::time::timeout(
            std::time::Duration::from_millis(remaining_ms),
            download_master_bounded(),
        )
        .await
        {
            Err(_elapsed) => return Err(CadenceFetchError::Timeout),
            Ok(Err(err)) => {
                debug!(%err, "groww cadence: instruments master download failed");
                return Err(CadenceFetchError::Transport);
            }
            Ok(Ok(rows)) => rows,
        };
        let lists: [Vec<u32>; 3] = [
            groww_option_expiries(
                &rows,
                GROWW_CHAIN_1M_UNDERLYINGS[0].1,
                GROWW_CHAIN_1M_UNDERLYINGS[0].0,
                today,
            ),
            groww_option_expiries(
                &rows,
                GROWW_CHAIN_1M_UNDERLYINGS[1].1,
                GROWW_CHAIN_1M_UNDERLYINGS[1].0,
                today,
            ),
            groww_option_expiries(
                &rows,
                GROWW_CHAIN_1M_UNDERLYINGS[2].1,
                GROWW_CHAIN_1M_UNDERLYINGS[2].0,
                today,
            ),
        ];
        let out = lists[slot].clone();
        *cache = Some((today, lists));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spot_failure(status: u16, rate_limited: bool, auth_rejected: bool) -> FetchFailure {
        FetchFailure {
            status,
            rate_limited,
            auth_rejected,
            msg: "test".to_string(),
        }
    }

    fn chain_failure(
        status: u16,
        rate_limited: bool,
        auth_rejected: bool,
    ) -> GrowwChainFetchFailure {
        GrowwChainFetchFailure {
            status,
            rate_limited,
            auth_rejected,
            msg: "test".to_string(),
        }
    }

    fn master_row(
        exchange: &str,
        instrument_type: &str,
        segment: &str,
        underlying_symbol: &str,
        expiry_date: &str,
    ) -> GrowwInstrumentRow {
        GrowwInstrumentRow {
            exchange: exchange.to_string(),
            exchange_token: "1".to_string(),
            groww_symbol: "X".to_string(),
            name: String::new(),
            instrument_type: instrument_type.to_string(),
            segment: segment.to_string(),
            series: String::new(),
            isin: String::new(),
            underlying_symbol: underlying_symbol.to_string(),
            expiry_date: expiry_date.to_string(),
        }
    }

    #[test]
    fn test_groww_cadence_map_spot_failure_taxonomy() {
        assert!(matches!(
            map_groww_spot_failure(&spot_failure(429, true, false)),
            CadenceFetchError::RateLimited {
                retry_after_ms: None
            }
        ));
        assert!(matches!(
            map_groww_spot_failure(&spot_failure(401, false, true)),
            CadenceFetchError::Auth
        ));
        assert!(matches!(
            map_groww_spot_failure(&spot_failure(403, false, true)),
            CadenceFetchError::Auth
        ));
        // Send-leg (no status) + 5xx + other 4xx are all Transport.
        for status in [0, 500, 502, 400, 404] {
            assert!(matches!(
                map_groww_spot_failure(&spot_failure(status, false, false)),
                CadenceFetchError::Transport
            ));
        }
    }

    #[test]
    fn test_groww_cadence_map_chain_failure_taxonomy() {
        // 429 outranks an auth-shaped flag (the sole arming class is
        // never masked).
        assert!(matches!(
            map_groww_chain_failure(&chain_failure(429, true, true)),
            CadenceFetchError::RateLimited {
                retry_after_ms: None
            }
        ));
        assert!(matches!(
            map_groww_chain_failure(&chain_failure(401, false, true)),
            CadenceFetchError::Auth
        ));
        for status in [0, 500, 400] {
            assert!(matches!(
                map_groww_chain_failure(&chain_failure(status, false, false)),
                CadenceFetchError::Transport
            ));
        }
    }

    #[test]
    fn test_groww_cadence_chain_identity_lookups_pinned() {
        assert_eq!(
            groww_chain_identity(ChainUnderlying::Nifty),
            Some((0, "NIFTY", "NSE", "NSE-NIFTY"))
        );
        assert_eq!(
            groww_chain_identity(ChainUnderlying::Banknifty),
            Some((1, "BANKNIFTY", "NSE", "NSE-BANKNIFTY"))
        );
        assert_eq!(
            groww_chain_identity(ChainUnderlying::Sensex),
            Some((2, "SENSEX", "BSE", "BSE-SENSEX"))
        );
    }

    #[test]
    fn test_groww_cadence_option_expiries_distinct_sorted_future_only() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 17).unwrap_or_default();
        let rows = vec![
            // Duplicate expiry across CE/PE → deduped.
            master_row("NSE", "CE", "FNO", "NIFTY", "2026-07-23"),
            master_row("NSE", "PE", "FNO", "NIFTY", "2026-07-23"),
            // Later expiry, out of order → sorted after.
            master_row("NSE", "CE", "FNO", "NIFTY", "2026-07-30"),
            // Past expiry → dropped.
            master_row("NSE", "CE", "FNO", "NIFTY", "2026-07-16"),
            // FUT row → excluded (options only).
            master_row("NSE", "FUT", "FNO", "NIFTY", "2026-08-27"),
            // Wrong exchange → excluded.
            master_row("BSE", "CE", "FNO", "NIFTY", "2026-08-06"),
            // Wrong underlying → excluded.
            master_row("NSE", "CE", "FNO", "BANKNIFTY", "2026-08-13"),
            // Garbage date → skipped silently.
            master_row("NSE", "CE", "FNO", "NIFTY", "garbage"),
        ];
        assert_eq!(
            groww_option_expiries(&rows, "NSE", "NIFTY", today),
            vec![20_260_723, 20_260_730]
        );
    }

    #[tokio::test]
    async fn test_groww_cadence_no_token_at_fire_time_maps_to_auth() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let exec = GrowwCadenceExecutor::for_test(GrowwTokenCache::for_test_paced_out(now_ms));
        let spot = exec
            .fetch_spot(SpotFetchRequest {
                feed: Feed::Groww,
                target: SpotTarget::Nifty,
                cycle_minute_ist: 9 * 3600 + 16 * 60,
                deadline_epoch_ms: now_ms + 60_000,
            })
            .await;
        assert!(matches!(spot, Err(CadenceFetchError::Auth)));
        let chain = exec
            .fetch_chain(ChainFetchRequest {
                feed: Feed::Groww,
                underlying: ChainUnderlying::Nifty,
                cycle_minute_ist: 9 * 3600 + 16 * 60,
                expiry_yyyymmdd: Some(20_260_723),
                deadline_epoch_ms: now_ms + 60_000,
            })
            .await;
        assert!(matches!(chain, Err(CadenceFetchError::Auth)));
    }

    /// Source-order ratchet: the fold handoff exists EXACTLY once in the
    /// production region and sits strictly AFTER the flush call, inside
    /// the `if flush_ok` arm — an unpersisted bar can never derive
    /// candles.
    #[test]
    fn test_groww_cadence_fold_handoff_only_after_flush_ack_source_order() {
        let src = include_str!("groww_cadence_executor.rs");
        // Split at the TEST MODULE marker (the file also carries a
        // #[cfg(test)] for_test constructor inside the prod region).
        let marker = concat!("#[cfg(test)]", "\nmod tests");
        let prod = src.split(marker).next().unwrap_or_default();
        let handoffs = prod.matches("send_confirmed_bars(").count();
        assert_eq!(
            handoffs, 1,
            "exactly ONE fold handoff site expected in the production region"
        );
        let flush_pos = prod.find("writer.flush()").unwrap_or_default();
        let gate_pos = prod.find("if flush_ok {").unwrap_or_default();
        let handoff_pos = prod.find("send_confirmed_bars(").unwrap_or_default();
        assert!(flush_pos > 0 && gate_pos > 0 && handoff_pos > 0);
        assert!(
            flush_pos < gate_pos && gate_pos < handoff_pos,
            "persist flush must precede the flush_ok gate which must precede the fold handoff"
        );
    }

    /// Source-order ratchet, PERSIST-GATED since the 2026-07-17 fix
    /// round 2 (the M1 fetch-ok-but-lost-is-not-ok rule): the liveness
    /// heartbeat is set ONLY inside the `if flush_ok` guard — after the
    /// spot_1m_rest flush ACK, before the fold handoff. An all-failing
    /// session (fetch OR persist) must leave the gauge unset so the
    /// market-hours-liveness alarm pages, never a fetch-time or
    /// fire-time false-green.
    #[test]
    fn test_groww_cadence_spot_fire_sets_rest_1m_heartbeat_gauge() {
        let src = include_str!("groww_cadence_executor.rs");
        // Split at the TEST MODULE marker (the file also carries a
        // #[cfg(test)] for_test constructor inside the prod region).
        let marker = concat!("#[cfg(test)]", "\nmod tests");
        let prod = src.split(marker).next().unwrap_or_default();
        let fetch_pos = prod.find("async fn fetch_spot").unwrap_or_default();
        let heartbeat_pos = prod.find("tv_rest_1m_fire_heartbeat").unwrap_or_default();
        let flush_pos = prod.find("writer.flush()").unwrap_or_default();
        let gate_pos = prod.find("if flush_ok {").unwrap_or_default();
        let handoff_pos = prod.find("send_confirmed_bars(").unwrap_or_default();
        assert!(fetch_pos > 0 && heartbeat_pos > 0 && flush_pos > 0 && gate_pos > 0);
        assert!(
            fetch_pos < heartbeat_pos
                && flush_pos < gate_pos
                && gate_pos < heartbeat_pos
                && heartbeat_pos < handoff_pos,
            "the heartbeat gauge must be persist-gated: inside fetch_spot's \
             flush_ok guard after the flush ACK, before the fold handoff"
        );
    }
}
