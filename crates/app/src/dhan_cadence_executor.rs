//! REAL Dhan cadence executor (2026-07-17 — the follow-on the cadence
//! scheduler PR flagged: "the real broker executors land later in
//! `crates/app`").
//!
//! One bounded request per call, NO impl-side retry/ladder, deadline
//! honored via `tokio::time::timeout` — the RUNNER owns retries, the
//! shape ladder, and the gate pacing (`cadence-error-codes.md` §0b/§3b:
//! cadence fires are gate-paced, NEVER routed through the shared
//! `dhan_data_api_limiter`; this file must never reference the limiter or
//! the gates — ratcheted by `cadence_executor_purity_guard`).
//!
//! Persistence contract (the legacy-leg subsumption — RS3's mutual
//! exclusion means the legacy per-minute legs are OFF when the cadence
//! lane is ON, so THIS executor is the sole author of the Dhan
//! `spot_1m_rest` / `option_chain_1m` / `rest_fetch_audit` rows):
//! - `fetch_spot`: fetch (limiter-free unpaced inner) → parse the target
//!   minute → persist via [`Spot1mRestWriter`] with the flush ACK → ONLY
//!   AFTER the ACK hand the confirmed bar to
//!   [`crate::rest_candle_fold::send_confirmed_bars`] (the ONLY feed for
//!   `candles_*` + the RAM store once the legacy legs are off) → return
//!   the [`SpotSnapshot`]. A persist failure is LOUD (SPOT1M-02) but the
//!   snapshot still returns; the unpersisted bar is NEVER folded. Honest
//!   bound: a QuestDB outage costs up to ~5s data-flush + ~5s audit-flush
//!   per fire, serialized behind the writer Mutex — later fires' snapshots
//!   can land past the decision ceiling and honest-skip decisions
//!   (fail-closed), while `block_in_place` keeps the runtime workers
//!   unblocked during the sync flush wait.
//! - `fetch_chain`: fetch → parse → classify moneyness → persist rows →
//!   publish the RAM chain snapshot (publish is persist-independent, the
//!   legacy Found-arm ordering) → return [`ChainFetchOk`] with the
//!   embedded underlying spot.
//! - `fetch_expiry_list`: the vendor-RAW expirylist as `yyyymmdd` ints —
//!   no policy math (the day-locked store owns selection).
//!
//! Token: resolved AT FIRE TIME via
//! [`tickvault_core::auth::token_manager::global_token_manager`] (never
//! boot-captured — the `dhan_rest_stack` Phase 2 registration can land
//! after the cadence spawn; a missing manager/token maps to
//! [`CadenceFetchError::Auth`]). The JWT is never logged.

use std::sync::Arc;

use chrono::NaiveDate;
use secrecy::ExposeSecret;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    CHAIN_1M_UNDERLYINGS, DHAN_CHARTS_INTRADAY_PATH, DHAN_OPTION_CHAIN_EXPIRYLIST_PATH,
    DHAN_OPTION_CHAIN_PATH, EXCHANGE_SEGMENT_IDX_I, SPOT_1M_REST_INDICES,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::types::SecurityId;
use tickvault_common::url_join::join_api_url;
use tickvault_core::auth::token_manager::global_token_manager;
use tickvault_core::cadence::executor::{
    CadenceExecutor, CadenceFetchError, ChainFetchOk, ChainFetchRequest, ExpiryListRequest,
    SpotFetchRequest, SpotSnapshot, SpotTarget,
};
use tickvault_core::notification::NotificationService;
use tickvault_core::pipeline::chain_snapshot::ChainUnderlying;

use crate::cadence_escalation::{
    EscalationLeg, LaneEscalation, emit_edge_action, flush_off_worker,
};
use tickvault_storage::option_chain_1m_persistence::{
    OPTION_CHAIN_1M_FEED_DHAN, OptionChain1mRow, OptionChain1mWriter,
};
use tickvault_storage::rest_fetch_audit_persistence::{RestFetchAuditWriter, RestFetchOutcome};
use tickvault_storage::spot_1m_rest_persistence::Spot1mRestWriter;
use tokio::sync::Mutex;
use tracing::{debug, error};

use crate::option_chain_1m_boot::{
    ChainFetchUnpacedFailure, MoneynessWarnLatches, build_dhan_chain_audit_row,
    chain_audit_append_best_effort, chain_audit_flush_best_effort, chain_fetch_once_unpaced,
    chain_request_body, classify_chain_legs, expirylist_request_body, fetched_at_ist_nanos_now,
    parse_expiry_list, parse_option_chain, publish_chain_moneyness_snapshot,
    record_chain_moneyness_observability,
};
use crate::spot_1m_rest_boot::{
    SpotFetchUnpacedFailure, audit_append_best_effort, audit_flush_best_effort,
    build_dhan_fetch_audit_row, build_spot_1m_row, ist_millis_of_day_now, minute_open_ist_nanos,
    parse_intraday_columnar_for_minutes, spot_1m_day_request_body, spot_1m_fetch_once_unpaced,
    stamp_held_ok_rows, today_ist,
};

/// Per-request HTTP client timeout, seconds — a BELT under the request
/// deadline (the deadline is the authority; the client timeout bounds a
/// black-holed socket). Sized at the chain leg's legacy 10s ceiling so
/// neither leg's in-deadline request is cut short by the belt.
const DHAN_CADENCE_HTTP_TIMEOUT_SECS: u64 = 10;

/// Pure: milliseconds remaining until `deadline_epoch_ms` at
/// `now_epoch_ms` — `None` when the deadline already elapsed (the caller
/// returns [`CadenceFetchError::Timeout`] WITHOUT sending).
#[must_use]
pub fn deadline_remaining_ms(deadline_epoch_ms: i64, now_epoch_ms: i64) -> Option<u64> {
    let rem = deadline_epoch_ms.saturating_sub(now_epoch_ms);
    u64::try_from(rem).ok().filter(|r| *r > 0)
}

/// Pure: map one unpaced SPOT failure into the cadence taxonomy.
/// 429 → `RateLimited` (the SOLE ladder-arming class); 401/403 → `Auth`;
/// send-leg (no status) / any other non-2xx (5xx AND other 4xx) →
/// `Transport` (non-arming since the 2026-07-16 RateLimited-only
/// correction — the safe direction).
#[must_use]
pub(crate) fn map_spot_failure(f: &SpotFetchUnpacedFailure) -> CadenceFetchError {
    if f.rate_limited {
        return CadenceFetchError::RateLimited {
            retry_after_ms: f.retry_after_ms,
        };
    }
    match f.status {
        Some(401 | 403) => CadenceFetchError::Auth,
        _ => CadenceFetchError::Transport,
    }
}

/// Pure: map one unpaced CHAIN-family failure into the cadence taxonomy.
/// Same shape as the spot mapping plus the entitlement verdict (DH-902 /
/// 806-class → `Auth`: the token/entitlement machinery owns recovery,
/// never the shape ladder). 429 is checked FIRST — the sole arming class
/// must never be masked by an entitlement-shaped body.
#[must_use]
pub(crate) fn map_chain_failure(f: &ChainFetchUnpacedFailure) -> CadenceFetchError {
    if f.rate_limited {
        return CadenceFetchError::RateLimited {
            retry_after_ms: f.retry_after_ms,
        };
    }
    if f.entitlement {
        return CadenceFetchError::Auth;
    }
    match f.status {
        Some(401 | 403) => CadenceFetchError::Auth,
        _ => CadenceFetchError::Transport,
    }
}

/// Pure: `yyyymmdd` int → date (`None` on garbage — the executor returns
/// [`CadenceFetchError::Empty`] rather than fabricating an expiry).
#[must_use]
pub fn yyyymmdd_to_date(v: u32) -> Option<NaiveDate> {
    let year = i32::try_from(v / 10_000).ok()?;
    NaiveDate::from_ymd_opt(year, (v / 100) % 100, v % 100)
}

/// Pure: date → `yyyymmdd` int (the expiry-list wire form the day-locked
/// store consumes). A pathological negative year saturates to 0 rather
/// than wrapping (trading-era dates are 4-digit positive).
#[must_use]
pub fn date_to_yyyymmdd(d: NaiveDate) -> u32 {
    use chrono::Datelike;
    let year = u32::try_from(d.year()).unwrap_or(0);
    year * 10_000 + d.month() * 100 + d.day()
}

/// Dhan identity for a cadence spot target — looked up from the pinned
/// [`SPOT_1M_REST_INDICES`] set by the cross-feed plain symbol.
#[must_use]
pub fn dhan_spot_identity(target: SpotTarget) -> Option<(SecurityId, &'static str)> {
    SPOT_1M_REST_INDICES
        .into_iter()
        .find(|(_, symbol)| *symbol == target.as_str())
}

/// Dhan identity + moneyness-latch slot for a chain underlying — looked
/// up from the pinned [`CHAIN_1M_UNDERLYINGS`] set by the plain symbol.
#[must_use]
pub fn dhan_chain_identity(u: ChainUnderlying) -> Option<(usize, SecurityId, &'static str)> {
    CHAIN_1M_UNDERLYINGS
        .into_iter()
        .enumerate()
        .find(|(_, (_, symbol))| *symbol == u.as_str())
        .map(|(slot, (sid, symbol))| (slot, sid, symbol))
}

/// The REAL Dhan cadence executor. Shared `&self` across the runner's
/// concurrent all-7 burst — the writers are `tokio::Mutex`-serialized
/// (cold path, a handful of fires per minute; lock order is always
/// data-writer FIRST then audit-writer, never both held at once — no
/// deadlock shape).
pub struct DhanCadenceExecutor {
    client: reqwest::Client,
    intraday_url: String,
    chain_url: String,
    expirylist_url: String,
    spot_writer: Mutex<Spot1mRestWriter>,
    chain_writer: Mutex<OptionChain1mWriter>,
    audit_writer: Mutex<RestFetchAuditWriter>,
    moneyness_latches: Mutex<MoneynessWarnLatches>,
    /// Escalation-edge tally (fix round 2026-07-17): the legacy
    /// SPOT1M-01/CHAIN-02 3-consecutive-fully-failed-minutes paging edge,
    /// minute-bucketed executor-side (persist failure = failed minute).
    escalation: Mutex<LaneEscalation>,
    /// Typed Telegram sink for the escalation/recovery events (`None` in
    /// tests — the coded `error!` lines still fire).
    notifier: Option<Arc<NotificationService>>,
}

impl DhanCadenceExecutor {
    /// Build the executor. `Err` = the HTTP client could not be built
    /// (HTTP-CLIENT-01 class — the caller degrades loudly; NEVER a
    /// `Client::new()` panic fallback).
    // TEST-EXEMPT: thin constructor — client build + join_api_url are covered upstream; the fetch behavior is exercised via the mapping/ordering tests below.
    pub fn new(
        rest_api_base_url: &str,
        questdb: &QuestDbConfig,
        notifier: Option<Arc<NotificationService>>,
    ) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(
                DHAN_CADENCE_HTTP_TIMEOUT_SECS,
            ))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("dhan cadence HTTP client build failed: {e}"))?;
        Ok(Self {
            client,
            intraday_url: join_api_url(rest_api_base_url, DHAN_CHARTS_INTRADAY_PATH),
            chain_url: join_api_url(rest_api_base_url, DHAN_OPTION_CHAIN_PATH),
            expirylist_url: join_api_url(rest_api_base_url, DHAN_OPTION_CHAIN_EXPIRYLIST_PATH),
            spot_writer: Mutex::new(Spot1mRestWriter::new(questdb)),
            chain_writer: Mutex::new(OptionChain1mWriter::new(questdb)),
            audit_writer: Mutex::new(RestFetchAuditWriter::new(questdb)),
            moneyness_latches: Mutex::new(MoneynessWarnLatches::default()),
            escalation: Mutex::new(LaneEscalation::default()),
            notifier,
        })
    }

    /// Record one leg outcome into the lane escalation tally; when the
    /// outcome rolls the minute bucket, emit the FINALIZED minute's edge
    /// action (the legacy SPOT1M-01/CHAIN-02 escalation/recovery contract
    /// — fix round 2026-07-17).
    async fn record_leg_outcome(&self, leg: EscalationLeg, minute_secs: u32, ok: bool) {
        let finalized = self.escalation.lock().await.record(leg, minute_secs, ok);
        if let Some((minute, action)) = finalized {
            emit_edge_action(Feed::Dhan, leg, minute, action, self.notifier.as_ref());
        }
    }

    /// Fire-time auth resolution — JWT + client-id from the global token
    /// manager registered by `dhan_rest_stack` Phase 2 (or the boot lane).
    /// `Err(Auth)` when the manager is not registered yet or no token
    /// state exists; the JWT is NEVER logged. Resolving the client-id
    /// HERE (not at construction) keeps the cadence spawn free of any
    /// boot-ordering dependency on the SSM credential fetch.
    fn auth_at_fire_time() -> Result<(secrecy::SecretString, String), CadenceFetchError> {
        let Some(manager) = global_token_manager() else {
            debug!("dhan cadence executor: no global token manager at fire time");
            return Err(CadenceFetchError::Auth);
        };
        let guard = manager.token_handle().load();
        let jwt = guard
            .as_ref()
            .as_ref()
            .map(|state| state.access_token().clone())
            .ok_or(CadenceFetchError::Auth)?;
        Ok((jwt, manager.client_id_string()))
    }
}

impl CadenceExecutor for DhanCadenceExecutor {
    // TEST-EXEMPT: live-HTTP orchestration — every decision leg (identity map, deadline math, failure taxonomy, fold-after-ACK ordering) is a pure fn / source-order ratchet unit-tested below; the HTTP inner is the tested unpaced fn.
    async fn fetch_spot(&self, req: SpotFetchRequest) -> Result<SpotSnapshot, CadenceFetchError> {
        let escalation_minute_secs = req.cycle_minute_ist;
        let mut escalation_persist_ok = true;
        let result = async {
            let Some((security_id, symbol)) = dhan_spot_identity(req.target) else {
                // Unreachable for the pinned 4-target enum — never a panic.
                return Err(CadenceFetchError::Malformed);
            };
            let Some(remaining_ms) =
                deadline_remaining_ms(req.deadline_epoch_ms, chrono::Utc::now().timestamp_millis())
            else {
                return Err(CadenceFetchError::Timeout);
            };
            let trading_date = today_ist();
            let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
            let target_nanos = minute_open_ist_nanos(trading_date, req.cycle_minute_ist);
            // Spot needs no client-id header — only the JWT.
            let jwt = match Self::auth_at_fire_time() {
                Ok((jwt, _client_id)) => jwt,
                Err(e) => {
                    let mut audit = self.audit_writer.lock().await;
                    audit_append_best_effort(
                        &mut audit,
                        &build_dhan_fetch_audit_row(
                            target_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            0,
                            0,
                            RestFetchOutcome::NoToken,
                            -1,
                            "no_token",
                        ),
                    );
                    flush_off_worker(|| audit_flush_best_effort(&mut audit));
                    return Err(e);
                }
            };
            let body = spot_1m_day_request_body(&security_id.to_string(), trading_date);
            let fetched = tokio::time::timeout(
                std::time::Duration::from_millis(remaining_ms),
                spot_1m_fetch_once_unpaced(
                    &self.client,
                    &self.intraday_url,
                    jwt.expose_secret(),
                    &body,
                ),
            )
            .await;
            let fetch_verdict = match fetched {
                Err(_elapsed) => Err((
                    CadenceFetchError::Timeout,
                    RestFetchOutcome::Error,
                    "timeout",
                )),
                Ok(Err(failure)) => {
                    let mapped = map_spot_failure(&failure);
                    debug!(
                        target = symbol,
                        outcome = mapped.as_str(),
                        msg = %failure.msg,
                        "dhan cadence spot fetch failed (runner owns retry/ladder)"
                    );
                    if failure.rate_limited {
                        Err((mapped, RestFetchOutcome::RateLimited, "rate_limited"))
                    } else {
                        Err((mapped, RestFetchOutcome::Error, "error"))
                    }
                }
                Ok(Ok(body)) => Ok(body),
            };
            let fetched_body = match fetch_verdict {
                Ok(body) => body,
                Err((mapped, audit_outcome, class)) => {
                    let rate_limited_count =
                        i64::from(matches!(audit_outcome, RestFetchOutcome::RateLimited));
                    let mut audit = self.audit_writer.lock().await;
                    audit_append_best_effort(
                        &mut audit,
                        &build_dhan_fetch_audit_row(
                            target_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
                            rate_limited_count,
                            audit_outcome,
                            -1,
                            class,
                        ),
                    );
                    flush_off_worker(|| audit_flush_best_effort(&mut audit));
                    return Err(mapped);
                }
            };
            let (candle, _backfill, stats) =
                parse_intraday_columnar_for_minutes(&fetched_body.text, target_nanos, None);
            let Some(candle) = candle else {
                // 2xx without the target minute — the honest empty split:
                // zero-candle body vs a body serving the day with a LAG.
                let class = match stats {
                    Some((rows, _)) if rows > 0 => "empty_stale",
                    _ => "empty_no_rows",
                };
                let mut audit = self.audit_writer.lock().await;
                audit_append_best_effort(
                    &mut audit,
                    &build_dhan_fetch_audit_row(
                        target_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        1,
                        0,
                        RestFetchOutcome::Empty,
                        -1,
                        class,
                    ),
                );
                flush_off_worker(|| audit_flush_best_effort(&mut audit));
                return Err(CadenceFetchError::Empty);
            };
            let close_to_data_ms =
                (ist_millis_of_day_now() - (i64::from(req.cycle_minute_ist) + 60) * 1000).max(0);
            // The per-fire liveness heartbeat (feeds the market-hours-liveness
            // alarm) — success-gated (fix round 2026-07-17): set ONLY after a
            // successful vendor fetch carrying the target row. An all-failing
            // session must leave the gauge unset so
            // tv-<env>-market-hours-liveness-missing pages (~09:25 IST);
            // mid-session death after first success remains the pre-existing
            // documented residual. Never pre-registered at boot: the first
            // in-session set IS the signal.
            metrics::gauge!("tv_rest_1m_fire_heartbeat").set(1.0);
            // Persist → flush ACK → fold handoff (STRICT ordering: an
            // unpersisted bar must never derive candles — the fold contract).
            let flush_ok = {
                let mut writer = self.spot_writer.lock().await;
                let row = build_spot_1m_row(
                    &candle,
                    security_id,
                    symbol,
                    trading_date_nanos,
                    close_to_data_ms,
                );
                if let Err(err) = writer.append_row(&row) {
                    metrics::counter!("tv_spot1m_persist_errors_total", "stage" => "append")
                        .increment(1);
                    error!(
                        code = ErrorCode::Spot1m02PersistFailed.code_str(),
                        stage = "append",
                        security_id,
                        ?err,
                        "SPOT1M-02: cadence spot_1m_rest row append failed"
                    );
                    false
                } else {
                    match flush_off_worker(|| writer.flush()) {
                        Ok(()) => true,
                        Err(err) => {
                            metrics::counter!("tv_spot1m_persist_errors_total", "stage" => "flush")
                                .increment(1);
                            error!(
                                code = ErrorCode::Spot1m02PersistFailed.code_str(),
                                stage = "flush",
                                ?err,
                                "SPOT1M-02: cadence spot_1m_rest ILP flush failed — \
                             pending rows discarded (poisoned-buffer defense)"
                            );
                            false
                        }
                    }
                }
            };
            if flush_ok {
                // ONLY after the flush ACK — the sole candles_*/RAM-store feed
                // on the cadence runtime.
                crate::rest_candle_fold::send_confirmed_bars(&[
                    crate::rest_candle_fold::ConfirmedBar::from_minute_candle(
                        Feed::Dhan,
                        security_id,
                        EXCHANGE_SEGMENT_IDX_I,
                        &candle,
                    ),
                ]);
            }
            metrics::counter!("tv_spot1m_fetch_total", "outcome" => "ok").increment(1);
            #[allow(clippy::cast_precision_loss)] // APPROVED: histogram sample only
            metrics::histogram!("tv_spot1m_close_to_data_ms").record(close_to_data_ms as f64);
            {
                let mut audit = self.audit_writer.lock().await;
                if flush_ok {
                    for row in stamp_held_ok_rows(
                        vec![build_dhan_fetch_audit_row(
                            candle.minute_ts_ist_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
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
                        &build_dhan_fetch_audit_row(
                            candle.minute_ts_ist_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
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
        self.record_leg_outcome(
            EscalationLeg::Spot,
            escalation_minute_secs,
            result.is_ok() && escalation_persist_ok,
        )
        .await;
        result
    }

    // TEST-EXEMPT: live-HTTP orchestration — decision legs are pure fns unit-tested below; parse/classify/publish/persist are the tested legacy building blocks.
    async fn fetch_chain(&self, req: ChainFetchRequest) -> Result<ChainFetchOk, CadenceFetchError> {
        let escalation_minute_secs = req.cycle_minute_ist;
        let mut escalation_persist_ok = true;
        let result = async {
            let Some((slot, security_id, symbol)) = dhan_chain_identity(req.underlying) else {
                return Err(CadenceFetchError::Malformed);
            };
            let Some(remaining_ms) =
                deadline_remaining_ms(req.deadline_epoch_ms, chrono::Utc::now().timestamp_millis())
            else {
                return Err(CadenceFetchError::Timeout);
            };
            // The runner NEVER guesses an expiry; with no day-locked winner
            // the fire is an honest Empty (non-arming) — this executor keeps
            // no warmup fallback of its own.
            let Some(expiry_date) = req.expiry_yyyymmdd.and_then(yyyymmdd_to_date) else {
                debug!(
                    symbol,
                    expiry = ?req.expiry_yyyymmdd,
                    "dhan cadence chain fire without a resolved expiry — Empty"
                );
                return Err(CadenceFetchError::Empty);
            };
            let trading_date = today_ist();
            let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
            let target_nanos = minute_open_ist_nanos(trading_date, req.cycle_minute_ist);
            let (jwt, client_id) = match Self::auth_at_fire_time() {
                Ok(pair) => pair,
                Err(e) => {
                    let mut audit = self.audit_writer.lock().await;
                    chain_audit_append_best_effort(
                        &mut audit,
                        &build_dhan_chain_audit_row(
                            target_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            0,
                            0,
                            -1,
                            RestFetchOutcome::NoToken,
                            "no_token",
                        ),
                    );
                    flush_off_worker(|| chain_audit_flush_best_effort(&mut audit));
                    return Err(e);
                }
            };
            let expiry_str = expiry_date.format("%Y-%m-%d").to_string();
            let body = chain_request_body(security_id, &expiry_str);
            let fetched = tokio::time::timeout(
                std::time::Duration::from_millis(remaining_ms),
                chain_fetch_once_unpaced(
                    &self.client,
                    &self.chain_url,
                    jwt.expose_secret(),
                    &client_id,
                    &body,
                ),
            )
            .await;
            let body_text = match fetched {
                Err(_elapsed) => {
                    let mut audit = self.audit_writer.lock().await;
                    chain_audit_append_best_effort(
                        &mut audit,
                        &build_dhan_chain_audit_row(
                            target_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
                            0,
                            -1,
                            RestFetchOutcome::Error,
                            "timeout",
                        ),
                    );
                    flush_off_worker(|| chain_audit_flush_best_effort(&mut audit));
                    return Err(CadenceFetchError::Timeout);
                }
                Ok(Err(failure)) => {
                    let mapped = map_chain_failure(&failure);
                    debug!(
                        symbol,
                        outcome = mapped.as_str(),
                        msg = %failure.msg,
                        "dhan cadence chain fetch failed (runner owns retry/ladder)"
                    );
                    let (audit_outcome, class) = if failure.rate_limited {
                        (RestFetchOutcome::RateLimited, "rate_limited")
                    } else if failure.entitlement {
                        (RestFetchOutcome::Error, "entitlement")
                    } else {
                        (RestFetchOutcome::Error, "error")
                    };
                    let mut audit = self.audit_writer.lock().await;
                    chain_audit_append_best_effort(
                        &mut audit,
                        &build_dhan_chain_audit_row(
                            target_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
                            i64::from(failure.status.unwrap_or(0)),
                            -1,
                            audit_outcome,
                            class,
                        ),
                    );
                    flush_off_worker(|| chain_audit_flush_best_effort(&mut audit));
                    return Err(mapped);
                }
                Ok(Ok(text)) => text,
            };
            let Some(chain) = parse_option_chain(&body_text) else {
                let mut audit = self.audit_writer.lock().await;
                chain_audit_append_best_effort(
                    &mut audit,
                    &build_dhan_chain_audit_row(
                        target_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        1,
                        200,
                        -1,
                        RestFetchOutcome::Error,
                        "error",
                    ),
                );
                flush_off_worker(|| chain_audit_flush_best_effort(&mut audit));
                return Err(CadenceFetchError::Malformed);
            };
            if chain.legs.is_empty() {
                let mut audit = self.audit_writer.lock().await;
                chain_audit_append_best_effort(
                    &mut audit,
                    &build_dhan_chain_audit_row(
                        target_nanos,
                        trading_date_nanos,
                        security_id,
                        symbol,
                        1,
                        200,
                        -1,
                        RestFetchOutcome::Empty,
                        "empty_chain",
                    ),
                );
                flush_off_worker(|| chain_audit_flush_best_effort(&mut audit));
                return Err(CadenceFetchError::Empty);
            }
            let close_to_data_ms =
                (ist_millis_of_day_now() - (i64::from(req.cycle_minute_ist) + 60) * 1000).max(0);
            // Classify ONCE per fire; the RAM snapshot rows are built BEFORE
            // the persist loop so a persist failure can never degrade the RAM
            // decision surface (the legacy Found-arm ordering).
            let cls = classify_chain_legs(
                symbol,
                chain.underlying_spot,
                chain.legs.iter().map(|l| (l.strike, l.leg, l.last_price)),
            );
            {
                let mut latches = self.moneyness_latches.lock().await;
                let minute_label = format!(
                    "{:02}:{:02}",
                    req.cycle_minute_ist / 3600,
                    (req.cycle_minute_ist % 3600) / 60
                );
                record_chain_moneyness_observability(
                    OPTION_CHAIN_1M_FEED_DHAN,
                    symbol,
                    slot,
                    trading_date,
                    &minute_label,
                    &cls,
                    &mut latches,
                );
            }
            let expiry_nanos = minute_open_ist_nanos(expiry_date, 0);
            let fetched_at = fetched_at_ist_nanos_now();
            let underlying_sid = i64::try_from(security_id).unwrap_or(i64::MAX);
            let mut persist_failed = false;
            {
                let mut writer = self.chain_writer.lock().await;
                for (leg, leg_moneyness) in chain.legs.iter().zip(cls.row_moneyness.iter()) {
                    let row = OptionChain1mRow {
                        ts_ist_nanos: target_nanos,
                        trading_date_ist_nanos: trading_date_nanos,
                        underlying_security_id: underlying_sid,
                        underlying_symbol: symbol,
                        expiry_ist_nanos: expiry_nanos,
                        strike: leg.strike,
                        leg: leg.leg,
                        contract_security_id: leg.contract_security_id,
                        last_price: leg.last_price,
                        iv: leg.implied_volatility,
                        delta: leg.delta,
                        theta: leg.theta,
                        gamma: leg.gamma,
                        vega: leg.vega,
                        oi: leg.oi,
                        volume: leg.volume,
                        previous_oi: leg.previous_oi,
                        underlying_spot: chain.underlying_spot,
                        fetched_at_ist_nanos: fetched_at,
                        moneyness: leg_moneyness.as_str(),
                    };
                    if let Err(err) = writer.append_row(&row) {
                        persist_failed = true;
                        metrics::counter!("tv_chain1m_persist_errors_total", "stage" => "append")
                            .increment(1);
                        error!(
                            code = ErrorCode::Chain03PersistFailed.code_str(),
                            stage = "append",
                            symbol,
                            ?err,
                            "CHAIN-03: cadence option_chain_1m row append failed"
                        );
                        break;
                    }
                }
                if !persist_failed && let Err(err) = flush_off_worker(|| writer.flush()) {
                    persist_failed = true;
                    metrics::counter!("tv_chain1m_persist_errors_total", "stage" => "flush")
                        .increment(1);
                    error!(
                        code = ErrorCode::Chain03PersistFailed.code_str(),
                        stage = "flush",
                        ?err,
                        "CHAIN-03: cadence option_chain_1m ILP flush failed — \
                     pending rows discarded (poisoned-buffer defense)"
                    );
                }
            }
            {
                let mut audit = self.audit_writer.lock().await;
                if persist_failed {
                    chain_audit_append_best_effort(
                        &mut audit,
                        &build_dhan_chain_audit_row(
                            target_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
                            200,
                            -1,
                            RestFetchOutcome::NamedGap,
                            "persist_failed",
                        ),
                    );
                } else {
                    for row in stamp_held_ok_rows(
                        vec![build_dhan_chain_audit_row(
                            target_nanos,
                            trading_date_nanos,
                            security_id,
                            symbol,
                            1,
                            200,
                            close_to_data_ms,
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
            // The embedded underlying spot BEFORE `cls` moves into publish
            // (Dhan val_f64 defaults an absent last_price to 0.0 → None).
            let underlying_spot = (chain.underlying_spot.is_finite()
                && chain.underlying_spot > 0.0)
                .then_some(chain.underlying_spot);
            // Publish is persist-independent (the RAM decision surface is
            // never degraded by a QuestDB outage).
            publish_chain_moneyness_snapshot(
                Feed::Dhan,
                symbol,
                target_nanos,
                fetched_at,
                chain.underlying_spot,
                expiry_nanos,
                cls,
            );
            metrics::counter!("tv_chain1m_fetch_total", "outcome" => "ok").increment(1);
            #[allow(clippy::cast_precision_loss)] // APPROVED: histogram sample only
            metrics::histogram!("tv_chain1m_close_to_data_ms").record(close_to_data_ms as f64);
            escalation_persist_ok = !persist_failed;
            Ok(ChainFetchOk {
                underlying_spot,
                published_to_registry: true,
            })
        }
        .await;
        // Escalation edge (fix round 2026-07-17): persist failure = failed
        // minute (the M1 rule).
        self.record_leg_outcome(
            EscalationLeg::Chain,
            escalation_minute_secs,
            result.is_ok() && escalation_persist_ok,
        )
        .await;
        result
    }

    // TEST-EXEMPT: live-HTTP orchestration — mapping/parse legs are pure fns tested below + in option_chain_1m_boot.
    async fn fetch_expiry_list(
        &self,
        req: ExpiryListRequest,
    ) -> Result<Vec<u32>, CadenceFetchError> {
        let Some((_slot, security_id, symbol)) = dhan_chain_identity(req.underlying) else {
            return Err(CadenceFetchError::Malformed);
        };
        let Some(remaining_ms) =
            deadline_remaining_ms(req.deadline_epoch_ms, chrono::Utc::now().timestamp_millis())
        else {
            return Err(CadenceFetchError::Timeout);
        };
        let (jwt, client_id) = Self::auth_at_fire_time()?;
        let body = expirylist_request_body(security_id);
        let fetched = tokio::time::timeout(
            std::time::Duration::from_millis(remaining_ms),
            chain_fetch_once_unpaced(
                &self.client,
                &self.expirylist_url,
                jwt.expose_secret(),
                &client_id,
                &body,
            ),
        )
        .await;
        let body_text = match fetched {
            Err(_elapsed) => return Err(CadenceFetchError::Timeout),
            Ok(Err(failure)) => {
                let mapped = map_chain_failure(&failure);
                debug!(
                    symbol,
                    outcome = mapped.as_str(),
                    msg = %failure.msg,
                    "dhan cadence expirylist fetch failed"
                );
                return Err(mapped);
            }
            Ok(Ok(text)) => text,
        };
        // Top-level JSON garbage is Malformed; a parseable envelope with
        // zero (or partially garbage) dates is the VENDOR-RAW answer —
        // returned as-is, the day-locked store's pure policy validates.
        if serde_json::from_str::<serde_json::Value>(&body_text).is_err() {
            return Err(CadenceFetchError::Malformed);
        }
        Ok(parse_expiry_list(&body_text)
            .into_iter()
            .map(date_to_yyyymmdd)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cadence_map_spot_failure_taxonomy() {
        // 429 → RateLimited with the Retry-After hint threaded through.
        let f = SpotFetchUnpacedFailure {
            status: Some(429),
            rate_limited: true,
            retry_after_ms: Some(2_000),
            msg: String::new(),
        };
        assert_eq!(
            map_spot_failure(&f),
            CadenceFetchError::RateLimited {
                retry_after_ms: Some(2_000)
            }
        );
        // 401/403 → Auth.
        for status in [401_u16, 403] {
            let f = SpotFetchUnpacedFailure {
                status: Some(status),
                rate_limited: false,
                retry_after_ms: None,
                msg: String::new(),
            };
            assert_eq!(map_spot_failure(&f), CadenceFetchError::Auth);
        }
        // Send-leg (no status) and 5xx and other 4xx → Transport
        // (non-arming — the 2026-07-16 RateLimited-only correction).
        for status in [None, Some(500), Some(503), Some(400)] {
            let f = SpotFetchUnpacedFailure {
                status,
                rate_limited: false,
                retry_after_ms: None,
                msg: String::new(),
            };
            assert_eq!(map_spot_failure(&f), CadenceFetchError::Transport);
        }
    }

    #[test]
    fn test_cadence_map_chain_failure_taxonomy() {
        // Entitlement wins over the raw status class (Auth — the token /
        // entitlement machinery owns recovery, never the shape ladder).
        let f = ChainFetchUnpacedFailure {
            status: Some(400),
            rate_limited: false,
            retry_after_ms: None,
            entitlement: true,
            msg: String::new(),
        };
        assert_eq!(map_chain_failure(&f), CadenceFetchError::Auth);
        // 429 outranks entitlement (the sole arming class must never be
        // masked by an entitlement-shaped body).
        let f = ChainFetchUnpacedFailure {
            status: Some(429),
            rate_limited: true,
            retry_after_ms: None,
            entitlement: true,
            msg: String::new(),
        };
        assert_eq!(
            map_chain_failure(&f),
            CadenceFetchError::RateLimited {
                retry_after_ms: None
            }
        );
        for status in [None, Some(500), Some(502)] {
            let f = ChainFetchUnpacedFailure {
                status,
                rate_limited: false,
                retry_after_ms: None,
                entitlement: false,
                msg: String::new(),
            };
            assert_eq!(map_chain_failure(&f), CadenceFetchError::Transport);
        }
        let f = ChainFetchUnpacedFailure {
            status: Some(401),
            rate_limited: false,
            retry_after_ms: None,
            entitlement: false,
            msg: String::new(),
        };
        assert_eq!(map_chain_failure(&f), CadenceFetchError::Auth);
    }

    #[test]
    fn test_cadence_deadline_remaining_ms_boundary() {
        assert_eq!(deadline_remaining_ms(1_000, 999), Some(1));
        assert_eq!(deadline_remaining_ms(1_000, 1_000), None);
        assert_eq!(deadline_remaining_ms(1_000, 1_001), None);
        assert_eq!(deadline_remaining_ms(i64::MIN, 0), None);
    }

    #[test]
    fn test_cadence_yyyymmdd_roundtrip_and_garbage() {
        let d = NaiveDate::from_ymd_opt(2026, 7, 17).expect("valid date");
        assert_eq!(date_to_yyyymmdd(d), 20_260_717);
        assert_eq!(yyyymmdd_to_date(20_260_717), Some(d));
        assert_eq!(yyyymmdd_to_date(20_261_345), None); // month 13
        assert_eq!(yyyymmdd_to_date(0), None);
    }

    #[test]
    fn test_cadence_dhan_identity_lookups_pinned() {
        assert_eq!(dhan_spot_identity(SpotTarget::Nifty), Some((13, "NIFTY")));
        assert_eq!(
            dhan_spot_identity(SpotTarget::BankNifty),
            Some((25, "BANKNIFTY"))
        );
        assert_eq!(dhan_spot_identity(SpotTarget::Sensex), Some((51, "SENSEX")));
        assert!(dhan_spot_identity(SpotTarget::IndiaVix).is_some());
        assert_eq!(
            dhan_chain_identity(ChainUnderlying::Nifty),
            Some((0, 13, "NIFTY"))
        );
        assert_eq!(
            dhan_chain_identity(ChainUnderlying::Banknifty),
            Some((1, 25, "BANKNIFTY"))
        );
        assert_eq!(
            dhan_chain_identity(ChainUnderlying::Sensex),
            Some((2, 51, "SENSEX"))
        );
    }

    #[tokio::test]
    async fn test_cadence_no_token_at_fire_time_maps_to_auth() {
        // The app lib-test binary never registers a global token manager
        // (orphan_position_watchdog_boot's own test pins is_none), so the
        // fire-time resolution maps to Auth — never a panic, never a sent
        // request.
        assert!(matches!(
            DhanCadenceExecutor::auth_at_fire_time(),
            Err(CadenceFetchError::Auth)
        ));
    }

    #[test]
    fn test_cadence_fold_handoff_only_after_flush_ack_source_order() {
        // Source-order ratchet (house pattern): in fetch_spot the fold
        // handoff is (a) present exactly once in the production region,
        // (b) AFTER the spot writer flush, (c) inside the `if flush_ok`
        // guard — an unpersisted bar must never derive candles.
        let src = include_str!("dhan_cadence_executor.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or_default();
        let handoffs = prod.matches("send_confirmed_bars(").count();
        assert_eq!(handoffs, 1, "exactly one fold handoff site");
        let flush_pos = prod
            .find("writer.flush()")
            .expect("spot flush site present");
        let guard_pos = prod.find("if flush_ok {").expect("flush_ok guard present");
        let handoff_pos = prod
            .find("send_confirmed_bars(")
            .expect("fold handoff present");
        assert!(
            flush_pos < guard_pos && guard_pos < handoff_pos,
            "fold handoff must sit after the flush ACK inside the flush_ok guard"
        );
    }

    #[test]
    fn test_cadence_spot_fire_sets_rest_1m_heartbeat_gauge() {
        // Deliverable-4 pin, SUCCESS-GATED since the 2026-07-17 fix
        // round: the tv_rest_1m_fire_heartbeat gauge (the
        // market-hours-liveness alarm's liveness signal) is set ONLY
        // after a successful vendor fetch carrying the target row — an
        // all-failing session must leave the gauge unset so the alarm
        // pages (~09:25 IST), never a fire-time false-green.
        let src = include_str!("dhan_cadence_executor.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or_default();
        let fetch_spot_pos = prod
            .find("async fn fetch_spot")
            .expect("fetch_spot present");
        let hb_pos = prod
            .find("tv_rest_1m_fire_heartbeat")
            .expect("heartbeat gauge set in production region");
        assert!(
            hb_pos > fetch_spot_pos,
            "heartbeat is set inside fetch_spot"
        );
        // Success-gated: AFTER the empty-target failure branch (the last
        // fetch-outcome return before the success path) and BEFORE the
        // persist leg — i.e. exactly at the successful-fetch point.
        let empty_pos = prod
            .find("return Err(CadenceFetchError::Empty);")
            .expect("empty-target branch present");
        let persist_pos = prod
            .find("// Persist → flush ACK")
            .expect("persist ordering comment present");
        assert!(
            empty_pos < hb_pos && hb_pos < persist_pos,
            "heartbeat must be success-gated: after the empty-target \
             branch, before the persist leg (never fire-time)"
        );
    }
}
