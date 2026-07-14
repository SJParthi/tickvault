//! Groww per-minute spot 1m REST leg — PR-2 of the Groww per-minute REST
//! plan (operator grant 2026-07-13, `.claude/plans/active-plan-groww-rest-1m.md`;
//! authorization `groww-second-feed-scope-2026-06-19.md` §38 +
//! `no-rest-except-live-feed-2026-06-27.md` §9; runbook
//! `.claude/rules/project/rest-1m-pipeline-error-codes.md`).
//!
//! Every trading-day minute close in session — the 09:15 candle closes at
//! 09:16:00 IST; the last (15:29) candle closes at 15:30:00 — this task
//! wakes shortly after the boundary and fetches THAT just-closed minute's
//! official 1m OHLCV for the 3 CORE spot indices (`NSE-NIFTY` /
//! `NSE-BANKNIFTY` / `BSE-SENSEX`) plus — since the 2026-07-13 operator
//! scope addition (§38.7 of `groww-second-feed-scope-2026-06-19.md`) — the
//! RUNTIME-resolved INDIA VIX index (SPOT ONLY; identity resolved from the
//! day's Groww master via the watch file, never guessed; fail-soft skipped
//! for the day when unresolved; per-SID independent — a VIX failure can
//! never delay or page the core 3) via Groww `GET /v1/historical/candles`
//! (`candle_interval="1minute"`), then persists to the SAME `spot_1m_rest`
//! QuestDB table tagged `feed='groww'` (feed-in-key DEDUP — the Dhan rows
//! are untouched; a re-fetch UPSERTs in place). Cold path ONLY: the WS
//! pipelines, tick capture and trading are untouched. Whether Groww's
//! historical-candles endpoint SERVES India VIX is a live-probe UNKNOWN —
//! persistent VIX emptiness yields named forensics rows + one coalesced
//! per-day "VIX not served" coded warn, never silence; VIX volume 0/absent
//! is EXPECTED (indices are price-only).
//!
//! ## The #1499 lessons, baked in from day one
//! The Dhan spot leg's first live session (2026-07-13) proved the
//! same-date minute window unreliable; this module ships the hotfix
//! patterns from birth: (1) DAY-GRANULAR request window
//! (`start_time = D 00:00:00`, `end_time = D+1 00:00:00`) + CLIENT-SIDE
//! filtering to the exact minute — a body serving only STALE (earlier)
//! minutes is detected by the filter and rides the ladder as
//! target-absent; (2) flush-confirmed [`PersistTracker`] watermark;
//! (3) one-minute-lookback BACKFILL in every ladder outcome arm; (4) one
//! bounded ~15:31 IST post-session sweep — a minute the sweep STILL cannot
//! recover becomes a NAMED GAP (a `rest_fetch_audit` row + counter + the
//! edge page already fired) — never a silent hole.
//!
//! ## Schema grounding (per the 2026-07-13 operator scope addition)
//! - Endpoint / params / headers: the citable reference pack —
//!   `docs/groww-ref/11-historical-candles.md` (endpoint, `groww_symbol`
//!   identity, `"1minute"` interval literal, 30-day 1m range cap) +
//!   `docs/groww-ref/README.md` (reconciled claims table) — backed by the
//!   `growwapi` 1.5.0 wheel source (`client.py:875-917`, `:1362-1378`) —
//!   Verified.
//! - The candle row tuple `[ts, o, h, l, c, volume, oi]` is
//!   PRODUCTION-GROUNDED: bruteX calls these same endpoints daily on this
//!   account and its CSVs (real response data — `symbol,timestamp_ist,
//!   o,h,l,c,volume,oi`) live in our S3 (the §37 crossverify pack).
//! - The ts WIRE FORMAT (epoch seconds vs `yyyy-MM-dd HH:mm:ss` IST string)
//!   is Assumed → confirm-live (`docs/groww-ref/99-UNKNOWNS.md`): BOTH
//!   forms parse defensively and `tv_groww_spot1m_ts_form_total{form}`
//!   records which one the live server actually sends. Epoch numbers are
//!   treated as UTC epoch (standard Unix convention — Assumed); strings as
//!   IST wall-clock (Indian-exchange convention — Assumed).
//!
//! ## Just-closed-minute availability (honest probe)
//! Groww documents NO availability delay for the sealing minute
//! (`docs/groww-ref/11-historical-candles.md` — current-day serving is
//! documented-consistent, minute-freshness UNDOCUMENTED). Each fire
//! carries the bounded in-minute re-poll ladder
//! (`GROWW_SPOT_1M_RETRY_OFFSETS_MS`) and records
//! `tv_groww_spot1m_close_to_data_ms` (minute close → successful
//! retrieval) as the live measurement — the number is MEASURED, never
//! asserted (operator Quote 2). A minute whose candle never appears is
//! `outcome="empty"` — counted, edge-tracked, forensics-rowed, never
//! silent (Rule 11).
//!
//! ## Rate budget + pacing (the plan's capacity verdict)
//! Per `docs/groww-ref/15-rate-limits-and-capacity.md` (official limits +
//! capacity math + the §6 live-probe measurement plan): Groww's Live-Data
//! bucket is 10/s + 300/min, TYPE-pooled, shared with bruteX on the ONE
//! minter token; which bucket `/historical/*` counts against is
//! UNVERIFIED (Assumed Live Data — `docs/groww-ref/99-UNKNOWNS.md`). That
//! binds the minute-boundary pacing rule: the 3 symbols are fetched
//! SEQUENTIALLY, so at most ONE request is in flight at any instant
//! (ladder re-polls are ≥700 ms apart) — far inside the ≤6 req/s
//! boundary-burst ceiling. Worst case ~15 requests/minute (3 symbols × 5
//! ladder rungs) ≈ 5% of the 300/min budget. Every 429 is counted + its
//! shape captured (timestamp, endpoint, Retry-After presence, sanitized
//! body) — the live-probe (e) requirement.
//!
//! ## Token (the minter lock)
//! The shared Groww access token is read READ-ONLY from SSM via the
//! existing `fetch_groww_access_token` (bruteX minter Lambda owns the
//! daily ~06:05 IST mint; the ~06:00 IST daily token expiry is OFFICIALLY
//! documented — `docs/groww-ref/17-token-lifecycle.md`) and cached
//! in-process; an auth-class reject (401/403) DROPS the cache (never
//! cache past an auth failure) and re-reads at
//! ≥`GROWW_SPOT_1M_TOKEN_REREAD_FLOOR_SECS` pacing — NEVER a mint
//! (`groww-shared-token-minter-2026-07-02.md`).
//!
//! ## Boot wiring (module contract)
//! Spawned PROCESS-GLOBAL from main.rs (next to the scoreboard/conservation
//! tasks) gated on `config.groww_spot_1m.enabled` — deliberately NOT the
//! Dhan-gated `spawn_post_market_tasks` seam: a Dhan-off (Groww-only)
//! session still runs this leg. Supervised respawn wrapper
//! (classify_join_exit + `tv_groww_spot1m_task_respawn_total{reason}` +
//! bounded backoff); exits cleanly after 15:30 IST (post the one bounded
//! sweep) or on a non-trading day. Panic honesty (TICK-FLUSH-01
//! precedent): release `panic = "abort"` — the panic-respawn arm is an
//! unwind-build self-heal path only.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, NaiveDateTime};
use secrecy::{ExposeSecret, SecretString};
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE, GROWW_CANDLE_INTERVAL_1MIN, GROWW_DATA_DIR,
    GROWW_HISTORICAL_CANDLES_URL, GROWW_SPOT_1M_FIRE_DELAY_MS, GROWW_SPOT_1M_MAX_BODY_BYTES,
    GROWW_SPOT_1M_REQUEST_TIMEOUT_SECS, GROWW_SPOT_1M_RETRY_OFFSETS_MS,
    GROWW_SPOT_1M_SYMBOL_BUDGET_SECS, GROWW_SPOT_1M_SYMBOLS, GROWW_SPOT_1M_TOKEN_REREAD_FLOOR_SECS,
    GROWW_SPOT_1M_VIX_SYMBOL, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY,
    SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST, SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::capture_rest_error_body;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::auth::secret_manager::fetch_groww_access_token;
use tickvault_core::feed::groww::instruments::stable_index_security_id;
use tickvault_core::feed::groww::native::watch_reader::{
    WatchFileDoc, WatchFileKind, parse_watch_file,
};
use tickvault_core::instrument::index_extractor::canonicalize_index_symbol;
use tickvault_core::notification::{NotificationEvent, NotificationService};
use tickvault_storage::disk_health_watcher::classify_join_exit;
use tickvault_storage::rest_fetch_audit_persistence::{
    REST_FETCH_LEG_SPOT_1M, RestFetchAuditRow, RestFetchAuditWriter, RestFetchOutcome,
    ensure_rest_fetch_audit_table,
};
use tickvault_storage::spot_1m_rest_persistence::{
    SPOT_1M_REST_FEED_GROWW, SPOT_1M_REST_SEGMENT_IDX_I, SPOT_1M_REST_SOURCE_GROWW_CANDLES,
    Spot1mRestRow, Spot1mRestWriter, ensure_spot_1m_rest_table,
};

use crate::dhan_intraday_parse::MinuteCandle;
// The session-boundary scheduling primitives + edge tracker + body-cap
// helpers are REUSED from the Dhan spot leg (they are NSE-session facts and
// pure state machines — the option_chain_1m_boot precedent).
use crate::spot_1m_rest_boot::{
    EdgeAction, FailureEdge, accumulation_within_cap, count_missed_boundaries,
    declared_len_within_cap, fire_is_fresh, format_minute_ist_12h, minute_fully_failed,
    minute_open_ist_nanos, next_fire_after, select_minute_candle, spot_1m_day_is_over,
};

/// Backoff before the supervisor respawns a dead/failed scheduler run.
const GROWW_SPOT_1M_RESPAWN_BACKOFF_SECS: u64 = 30;
/// Post-session sweep instant, IST seconds-of-day: 15:31:00 — one minute
/// after the last (15:30:00) fire, once the whole session is final (the
/// #1499 sweep pattern: the 15:30:00 fire is the LAST, so a vendor-late
/// 15:29 candle has no in-session repair path without it).
const GROWW_SPOT_1M_SWEEP_FIRE_SECS_OF_DAY_IST: u32 = 15 * 3600 + 31 * 60;
/// Milliseconds per second / per day (wall-clock latency math).
const MILLIS_PER_SEC: i64 = 1_000;
const MILLIS_PER_DAY: i64 = 86_400_000;
/// Nanoseconds per second / per minute (IST-epoch math).
const NANOS_PER_SEC: i64 = 1_000_000_000;
const NANOS_PER_MINUTE: i64 = 60 * NANOS_PER_SEC;
/// Plausible-epoch guards for the defensive numeric-timestamp parse:
/// 2000-01-01 .. 2100-01-01 UTC. Anything outside is malformed, never a
/// silently-wrong minute bucket.
const MIN_PLAUSIBLE_EPOCH_SECS: i64 = 946_684_800;
const MAX_PLAUSIBLE_EPOCH_SECS: i64 = 4_102_444_800;
/// A numeric timestamp at/above this is epoch MILLISECONDS (the deprecated
/// V1 docstring mentions millis) — normalized to seconds before the
/// plausibility check.
const EPOCH_MILLIS_THRESHOLD: i64 = 100_000_000_000;

/// Everything the scheduler needs, cloneable so the supervisor can respawn
/// the inner run (all fields are `Arc`s or cheap owned copies). NO Dhan
/// token handle and NO Dhan base URL — this leg is fully independent of
/// the Dhan lane (token via the shared-minter SSM read; endpoint constant).
#[derive(Clone)]
pub struct GrowwSpot1mTaskParams {
    /// Telegram dispatcher for the edge page + recovery ping.
    pub notifier: Arc<NotificationService>,
    /// Trading calendar (weekday + NSE-holiday aware) — re-checked every
    /// loop iteration (audit-findings Rule 3).
    pub calendar: Arc<TradingCalendar>,
    /// QuestDB target for the `spot_1m_rest` + `rest_fetch_audit` tables.
    pub questdb: QuestDbConfig,
    /// Chain-leg sequencing signal (PR-3): the boundary seconds-of-day this
    /// leg just finished firing, published UNCONDITIONALLY at the END of
    /// every fire — success OR failure — via `send_replace` (never errors;
    /// the Dhan seam's exact semantics: the chain must never block on a
    /// failing spot leg, and its own fallback timer covers a dead/disabled
    /// one). `None` when the chain leg is disabled — zero publishes, the
    /// loop stays byte-identical to PR-2.
    pub minute_done_tx: Option<tokio::sync::watch::Sender<Option<u32>>>,
}

// ---------------------------------------------------------------------------
// Spot targets — the 3 const CORE indices + the RUNTIME-resolved INDIA VIX
// (operator scope 2026-07-13, relayed via the coordinator session; §38.7 of
// `groww-second-feed-scope-2026-06-19.md`). SPOT ONLY — no chain, no
// contracts.
// ---------------------------------------------------------------------------

/// One per-minute spot fetch target. The 3 CORE targets come verbatim from
/// [`GROWW_SPOT_1M_SYMBOLS`]; the 4th — INDIA VIX — is RUNTIME-resolved
/// from the day's ingested Groww master (the watch file the Groww lane
/// builds) because its exact `groww_symbol` is not provably pinnable
/// in-repo ("do NOT guess the literal" — operator scope 2026-07-13).
/// `core` drives the escalation edge: the 3-consecutive-minutes page keys
/// on the ORIGINAL 3 core indices ONLY, so a VIX-only failure can never
/// page the leg (per-SID independence — VIX failing can never delay or
/// escalate the other 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GrowwSpotTarget {
    /// The Groww candles identity (`groww_symbol`, e.g. `NSE-NIFTY`).
    groww_symbol: String,
    /// Human symbol persisted on rows (`NIFTY` / ... / `INDIA VIX`).
    symbol: &'static str,
    /// Groww exchange (`NSE` / `BSE`).
    exchange: String,
    /// Groww segment (`CASH` for indices).
    segment: String,
    /// The live-lane canonical Groww index id
    /// ([`stable_index_security_id`] of the groww_symbol).
    security_id: i64,
    /// `true` for the 3 §38 core indices; `false` for INDIA VIX.
    core: bool,
}

/// The 3 CORE targets from the const table. Pure.
fn core_spot_targets() -> Vec<GrowwSpotTarget> {
    GROWW_SPOT_1M_SYMBOLS
        .iter()
        .map(
            |&(groww_symbol, symbol, exchange, segment)| GrowwSpotTarget {
                groww_symbol: groww_symbol.to_string(),
                symbol,
                exchange: exchange.to_string(),
                segment: segment.to_string(),
                security_id: stable_index_security_id(groww_symbol),
                core: true,
            },
        )
        .collect()
}

/// Resolve the INDIA VIX target from a parsed watch-file doc. Match rule
/// (the house canonicalize path — the same two identity keys the Groww
/// index-coverage audit uses): an `index_value` entry whose
/// `exchange_token` OR display `symbol_name` canonicalizes to the
/// allowlisted `INDIA VIX` (the live 2026-06-28 master carries token
/// `INDIAVIX` + name "India Vix"; both keys resolve via
/// `INDEX_SYMBOL_ALIASES` / normalization). Fail-CLOSED on every anomaly
/// (`None` + the caller's coded warn): a stale file (date mismatch), a
/// missing `index_name` (no groww_symbol to query with), or a
/// `security_id` that disagrees with the canonical hash (derivation drift)
/// all refuse — NEVER a guessed identity. Pure.
fn vix_target_from_watch_doc(doc: &WatchFileDoc, expected_date: &str) -> Option<GrowwSpotTarget> {
    if doc.trading_date_ist != expected_date {
        return None;
    }
    let vix_canonical = canonicalize_index_symbol(GROWW_SPOT_1M_VIX_SYMBOL);
    let entry = doc.entries.iter().find(|e| {
        e.kind == WatchFileKind::IndexValue
            && (canonicalize_index_symbol(&e.exchange_token) == vix_canonical
                || e.symbol_name
                    .as_deref()
                    .is_some_and(|n| canonicalize_index_symbol(n) == vix_canonical))
    })?;
    let groww_symbol = entry.index_name.as_deref()?.trim();
    if groww_symbol.is_empty() {
        return None;
    }
    let security_id = stable_index_security_id(groww_symbol);
    if security_id != entry.security_id {
        // Derivation drift — refuse rather than persist rows under an id
        // the live lane would not join on.
        return None;
    }
    Some(GrowwSpotTarget {
        groww_symbol: groww_symbol.to_string(),
        symbol: GROWW_SPOT_1M_VIX_SYMBOL,
        exchange: entry.exchange.clone(),
        segment: entry.segment.clone(),
        security_id,
        core: false,
    })
}

/// One RUNTIME resolution attempt for the INDIA VIX target: read today's
/// watch file (`data/groww/groww-watch-<date>.json` — the day's ingested
/// Groww master product, built by the Groww lane activation) and match via
/// [`vix_target_from_watch_doc`]. Fail-SOFT: any miss returns `None`
/// (counted + warned once by the caller; the 3 core targets are never
/// blocked). Cold path, at most one small file read per minute.
// TEST-EXEMPT: thin fs-read shim — the parse + match + fail-closed arms are the pure vix_target_from_watch_doc, unit-tested below.
fn try_resolve_vix_target(trading_date: NaiveDate) -> Option<GrowwSpotTarget> {
    let date = trading_date.format("%Y-%m-%d").to_string();
    let path = crate::groww_native_shadow::watch_file_path_for(Path::new(GROWW_DATA_DIR), &date);
    let json = std::fs::read_to_string(&path).ok()?;
    let doc = parse_watch_file(&json).ok()?;
    vix_target_from_watch_doc(&doc, &date)
}

/// Lazy per-iteration VIX resolution: pushes the resolved target once (the
/// watch build lands asynchronously after boot, so a run-start-only
/// attempt would race it). The coded warn fires ONCE per run
/// (edge-latched); every failed attempt increments the counter so the
/// unresolved rate stays visible. An unresolved day degrades to the 3
/// core targets — never a guessed literal, never a blocked core fire.
fn ensure_vix_target(
    targets: &mut Vec<GrowwSpotTarget>,
    vix_warned: &mut bool,
    trading_date: NaiveDate,
) {
    if targets.iter().any(|t| !t.core) {
        return;
    }
    match try_resolve_vix_target(trading_date) {
        Some(vix) => {
            info!(
                groww_symbol = %vix.groww_symbol,
                exchange = %vix.exchange,
                segment = %vix.segment,
                security_id = vix.security_id,
                "groww_spot_1m: INDIA VIX target runtime-resolved from the \
                 day's Groww master (2026-07-13 operator scope) — the spot \
                 set is now 4 targets"
            );
            targets.push(vix);
        }
        None => {
            metrics::counter!("tv_groww_spot1m_vix_unresolved_total").increment(1);
            if !*vix_warned {
                *vix_warned = true;
                warn!(
                    code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                    stage = "vix_unresolved",
                    feed = SPOT_1M_REST_FEED_GROWW,
                    "SPOT1M-01: INDIA VIX could not be resolved from today's \
                     Groww master (watch file absent or carries no VIX index \
                     row) — the spot leg runs the 3 core indices; VIX is \
                     skipped (never guessed) and re-attempted each minute"
                );
            }
        }
    }
}

/// Escalation-edge tally for one fired minute, restricted to the 3 CORE
/// indices (operator scope 2026-07-13: VIX failing can never delay — nor
/// page — the other 3). The page/recover edge feeds ONLY from core
/// outcomes: a VIX-only fetch failure, a VIX-only row-append failure and a
/// VIX-only empty are all non-edge (still counted + forensics-rowed +
/// verdict-logged); a shared ILP FLUSH failure IS edge-relevant (it loses
/// core rows too). Conversely, all-3-core-failed pages even when VIX alone
/// succeeded. Pure state — unit-tested below.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MinuteEdgeTally {
    core_ok: usize,
    core_persist_failed: bool,
}

impl MinuteEdgeTally {
    /// Record one target's successful own-minute fetch.
    fn note_ok(&mut self, core: bool) {
        if core {
            self.core_ok = self.core_ok.saturating_add(1);
        }
    }

    /// Record one target's row-append failure (own or backfill row).
    fn note_append_failure(&mut self, core: bool) {
        if core {
            self.core_persist_failed = true;
        }
    }

    /// Record the shared ILP flush failing (loses core rows too).
    fn note_flush_failure(&mut self) {
        self.core_persist_failed = true;
    }

    /// The edge verdict for this minute — the shared fully-failed rule
    /// (`minute_fully_failed`) over CORE inputs only.
    fn fully_failed(self) -> bool {
        minute_fully_failed(self.core_ok, self.core_persist_failed)
    }
}

/// The end-of-session "India VIX not served" verdict (operator scope
/// 2026-07-13): `true` when a VIX target WAS resolved yet ZERO VIX minutes
/// ever persisted this run (fires + sweep included), while at least one
/// CORE index DID persist — so the emptiness is VIX-specific ("VIX not
/// served by Groww"), not a leg-wide outage (leg-wide outages already
/// paged via the escalation edge). Pure.
fn vix_not_served_verdict(targets: &[GrowwSpotTarget], tracker: &PersistTracker) -> bool {
    let Some(vix) = targets.iter().find(|t| !t.core) else {
        return false;
    };
    tracker.last_persisted(vix.security_id).is_none()
        && targets
            .iter()
            .any(|t| t.core && tracker.last_persisted(t.security_id).is_some())
}

// ---------------------------------------------------------------------------
// Pure request/window builders
// ---------------------------------------------------------------------------

/// The PROVEN day-granular window strings (`start = D 00:00:00`,
/// `end = D+1 00:00:00`, `yyyy-MM-dd HH:mm:ss`) — the #1499 lesson applied
/// to Groww from day one; the consumer filters client-side to the exact
/// minute. Month/day boundaries via `succ_opt` (cross-verify semantics).
/// Pure.
pub(crate) fn groww_day_window_strings(trading_date: NaiveDate) -> (String, String) {
    // `succ_opt` is `None` ONLY for `NaiveDate::MAX` (year 262142) — an
    // unreachable input for a live trading date. The fallback collapses the
    // window to zero width (start == end), which the server answers with an
    // empty candle set → the minute rides the ladder as target-absent
    // (typed degrade, never a panic) — explicit per hostile round 1 item 10.
    let next_day = trading_date.succ_opt().unwrap_or(trading_date);
    (
        trading_date.format("%Y-%m-%d 00:00:00").to_string(),
        next_day.format("%Y-%m-%d 00:00:00").to_string(),
    )
}

/// The `GET /v1/historical/candles` query params for ONE index and ONE
/// trading day: `exchange` / `segment` / `groww_symbol` /
/// `start_time` / `end_time` / `candle_interval="1minute"` (SDK-verified
/// param set; identity = the `groww_symbol`, never the token or the bare
/// trading symbol). Pure.
pub(crate) fn groww_candles_query(
    groww_symbol: &str,
    exchange: &str,
    segment: &str,
    trading_date: NaiveDate,
) -> [(&'static str, String); 6] {
    let (start_time, end_time) = groww_day_window_strings(trading_date);
    [
        ("exchange", exchange.to_string()),
        ("segment", segment.to_string()),
        ("groww_symbol", groww_symbol.to_string()),
        ("start_time", start_time),
        ("end_time", end_time),
        ("candle_interval", GROWW_CANDLE_INTERVAL_1MIN.to_string()),
    ]
}

// ---------------------------------------------------------------------------
// Pure response parsing (defensive dual timestamp forms — UNVERIFIED-LIVE)
// ---------------------------------------------------------------------------

/// Which wire form a candle timestamp arrived in — recorded via
/// `tv_groww_spot1m_ts_form_total{form}` since the live V2 format is
/// UNVERIFIED-LIVE (the docs-derived sources disagree).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GrowwTsForm {
    /// A JSON number — treated as UNIX epoch (UTC) seconds (millis
    /// normalized), +19800 → IST, minute-floored (Assumed convention).
    EpochSecs,
    /// A `"yyyy-MM-dd HH:mm:ss"` (or `T`-separated) string — treated as
    /// IST wall-clock, minute-floored (Assumed convention).
    IstString,
}

/// Parse one candle-tuple timestamp into the IST-minute bucket nanoseconds
/// our tables key on. Accepts BOTH plausible wire forms defensively;
/// `None` = malformed (counted by the caller, never a panic, never a
/// silently-wrong bucket). Pure.
fn parse_groww_candle_ts(v: &serde_json::Value) -> Option<(i64, GrowwTsForm)> {
    if let Some(num) = v.as_i64().or_else(|| {
        v.as_f64()
            .filter(|f| f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18)
            .map(|f| f as i64)
    }) {
        // Numeric: epoch millis normalize → plausibility gate → UTC+19800
        // → minute floor (the historical-data.md rule-8 conversion).
        let secs = if num >= EPOCH_MILLIS_THRESHOLD {
            num / 1_000
        } else {
            num
        };
        if !(MIN_PLAUSIBLE_EPOCH_SECS..=MAX_PLAUSIBLE_EPOCH_SECS).contains(&secs) {
            return None;
        }
        let ist_secs = secs.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
        let minute_floor = ist_secs - ist_secs.rem_euclid(60);
        return Some((
            minute_floor.saturating_mul(NANOS_PER_SEC),
            GrowwTsForm::EpochSecs,
        ));
    }
    let s = v.as_str()?;
    let parsed = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .ok()?;
    // IST wall-clock string → IST-as-epoch seconds (the same representation
    // `candles_1m.ts` uses) → minute floor.
    let ist_as_epoch = parsed.and_utc().timestamp();
    if !(MIN_PLAUSIBLE_EPOCH_SECS..=MAX_PLAUSIBLE_EPOCH_SECS).contains(&ist_as_epoch) {
        return None;
    }
    let minute_floor = ist_as_epoch - ist_as_epoch.rem_euclid(60);
    Some((
        minute_floor.saturating_mul(NANOS_PER_SEC),
        GrowwTsForm::IstString,
    ))
}

/// Per-body parse accounting — which ts forms were seen + how many rows
/// were malformed (all counted, never silent). Shared with the PR-4
/// contract leg (`groww_contract_1m_boot.rs`) — each leg records the
/// stats into its OWN counter names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GrowwParseStats {
    pub(crate) epoch_ts_rows: u32,
    pub(crate) string_ts_rows: u32,
    pub(crate) malformed_rows: u32,
}

/// One parsed Groww candle tuple `[ts, o, h, l, c, volume, oi]` — the
/// SHARED wire row (PR-4 refactor): the spot leg maps it to
/// [`MinuteCandle`] (dropping `oi` — indices carry none), the contract
/// leg consumes `oi` directly (the fill-model column).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GrowwCandleRow {
    pub(crate) minute_ts_ist_nanos: i64,
    pub(crate) open: f64,
    pub(crate) high: f64,
    pub(crate) low: f64,
    pub(crate) close: f64,
    pub(crate) volume: i64,
    /// Candle tuple element 6 (`open_interest`) — 0 when absent/null
    /// (indices legitimately carry none; contracts carry the real OI).
    pub(crate) oi: i64,
}

/// Parse a Groww candles response body into [`MinuteCandle`]s. Envelope:
/// `{"status":"SUCCESS","payload":{"candles":[[ts,o,h,l,c,volume,oi],...]}}`
/// (a bare top-level `candles` array is tolerated). Row tuples are
/// PRODUCTION-GROUNDED via bruteX's daily pulls; the ts form is parsed
/// defensively (both forms). Malformed bodies/rows parse to empty/skipped
/// + counted — never a panic (typed-degrade discipline). `volume` may be
/// null (indices) → 0; non-finite prices skip the row. Pure.
fn parse_groww_1m_candles(body: &str) -> (Vec<MinuteCandle>, GrowwParseStats) {
    let (rows, stats) = parse_groww_1m_candle_rows(body);
    let candles = rows
        .into_iter()
        .map(|r| MinuteCandle {
            minute_ts_ist_nanos: r.minute_ts_ist_nanos,
            open: r.open,
            high: r.high,
            low: r.low,
            close: r.close,
            volume: r.volume,
        })
        .collect();
    (candles, stats)
}

/// The SHARED candle-tuple parser (PR-4 refactor — one wire parser, two
/// consumers): identical envelope/row/timestamp semantics to the original
/// spot parser, PLUS the `oi` element (tuple[6], tolerant: absent/null →
/// 0) the contract leg consumes. Pure.
pub(crate) fn parse_groww_1m_candle_rows(body: &str) -> (Vec<GrowwCandleRow>, GrowwParseStats) {
    let mut stats = GrowwParseStats::default();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        stats.malformed_rows = 1;
        return (Vec::new(), stats);
    };
    if v.get("status").and_then(|s| s.as_str()) == Some("FAILURE") {
        return (Vec::new(), stats);
    }
    let candles = v
        .get("payload")
        .and_then(|p| p.get("candles"))
        .and_then(|c| c.as_array())
        .or_else(|| v.get("candles").and_then(|c| c.as_array()));
    let Some(rows) = candles else {
        stats.malformed_rows = 1;
        return (Vec::new(), stats);
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(tuple) = row.as_array() else {
            stats.malformed_rows = stats.malformed_rows.saturating_add(1);
            continue;
        };
        if tuple.len() < 6 {
            stats.malformed_rows = stats.malformed_rows.saturating_add(1);
            continue;
        }
        let Some((minute_ts_ist_nanos, form)) = parse_groww_candle_ts(&tuple[0]) else {
            stats.malformed_rows = stats.malformed_rows.saturating_add(1);
            continue;
        };
        let (Some(open), Some(high), Some(low), Some(close)) = (
            tuple[1].as_f64(),
            tuple[2].as_f64(),
            tuple[3].as_f64(),
            tuple[4].as_f64(),
        ) else {
            stats.malformed_rows = stats.malformed_rows.saturating_add(1);
            continue;
        };
        if !(open.is_finite() && high.is_finite() && low.is_finite() && close.is_finite()) {
            stats.malformed_rows = stats.malformed_rows.saturating_add(1);
            continue;
        }
        // Index volume is legitimately null/0 — stored verbatim, never a
        // malformed marker. `oi` (tuple[6]) is tolerant the same way: the
        // spot leg drops it, the contract leg persists it verbatim.
        let tolerant_i64 = |v: Option<&serde_json::Value>| -> i64 {
            v.and_then(|v| {
                v.as_i64().or_else(|| {
                    v.as_f64()
                        .filter(|f| f.is_finite() && f.abs() < 9.2e18)
                        .map(|f| f as i64)
                })
            })
            .unwrap_or(0)
        };
        let volume = tolerant_i64(tuple.get(5));
        let oi = tolerant_i64(tuple.get(6));
        match form {
            GrowwTsForm::EpochSecs => stats.epoch_ts_rows = stats.epoch_ts_rows.saturating_add(1),
            GrowwTsForm::IstString => {
                stats.string_ts_rows = stats.string_ts_rows.saturating_add(1);
            }
        }
        out.push(GrowwCandleRow {
            minute_ts_ist_nanos,
            open,
            high,
            low,
            close,
            volume,
            oi,
        });
    }
    (out, stats)
}

/// Emit the ts-form + malformed-row counters for one parsed body (the
/// UNVERIFIED-LIVE wire-format probe). Static labels only.
fn record_parse_stats(stats: GrowwParseStats) {
    if stats.epoch_ts_rows > 0 {
        metrics::counter!("tv_groww_spot1m_ts_form_total", "form" => "epoch")
            .increment(u64::from(stats.epoch_ts_rows));
    }
    if stats.string_ts_rows > 0 {
        metrics::counter!("tv_groww_spot1m_ts_form_total", "form" => "string")
            .increment(u64::from(stats.string_ts_rows));
    }
    if stats.malformed_rows > 0 {
        metrics::counter!("tv_groww_spot1m_parse_malformed_rows_total")
            .increment(u64::from(stats.malformed_rows));
    }
}

// ---------------------------------------------------------------------------
// Pure backfill / sweep / watermark primitives (the #1499 patterns)
// ---------------------------------------------------------------------------

/// The previous minute to BACKFILL on this fire, or `None` when no backfill
/// is due: the previous minute must be inside today's session (at or after
/// the 09:15 open) and must not already be persisted for this symbol.
/// One-minute lookback only — older gaps are the sweep's job. Pure.
fn backfill_minute_nanos(
    last_persisted: Option<i64>,
    target_minute_nanos: i64,
    session_first_minute_nanos: i64,
) -> Option<i64> {
    let prev = target_minute_nanos.saturating_sub(NANOS_PER_MINUTE);
    (prev >= session_first_minute_nanos && last_persisted.is_none_or(|l| l < prev)).then_some(prev)
}

/// All session minutes STILL MISSING above the persisted watermark at
/// sweep time: `(watermark, session_last]` step one minute (the whole
/// session when nothing was ever persisted). Bounded to ≤375 minutes by
/// construction. Pure.
fn sweep_missing_minutes(
    last_persisted: Option<i64>,
    session_first_minute_nanos: i64,
    session_last_minute_nanos: i64,
) -> Vec<i64> {
    let start = match last_persisted {
        Some(w) => w
            .saturating_add(NANOS_PER_MINUTE)
            .max(session_first_minute_nanos),
        None => session_first_minute_nanos,
    };
    let mut out = Vec::new();
    let mut m = start;
    while m <= session_last_minute_nanos {
        out.push(m);
        m = m.saturating_add(NANOS_PER_MINUTE);
    }
    out
}

/// The FIRST session minute a run's first fire actually covers: the
/// target's one-minute-lookback backfill minute when that is in-session,
/// else the target itself. Everything BEFORE it is the run's pre-boot
/// blind window (item 2). Pure.
fn first_covered_minute(target_minute_nanos: i64, session_first_minute_nanos: i64) -> i64 {
    let prev = target_minute_nanos.saturating_sub(NANOS_PER_MINUTE);
    if prev >= session_first_minute_nanos {
        prev
    } else {
        target_minute_nanos
    }
}

/// PRE-BOOT session minutes THIS RUN never covered (hostile round 1
/// item 2 — the mid-session-boot silent hole): everything in
/// `[session_first, first_covered)` when the run's first fire covered
/// `first_covered` (its first target's backfill minute, or the target
/// itself when the backfill was out-of-session), or the WHOLE session when
/// the run never fired at all. These are AUDIT-ONLY named gaps — §38
/// forbids a bulk backfill fetch, so the sweep names them (one
/// `rest_fetch_audit` row each, outcome `named_gap`, class `pre_boot`)
/// without ever fetching them. The same mechanism covers a supervisor
/// respawn (the fresh run cannot vouch for pre-respawn minutes; a
/// previously-persisted minute keeps its own `ok` row — `outcome` is in
/// the DEDUP key, so the gap row lands ALONGSIDE, never over it). Pure.
fn pre_boot_gap_minutes(
    first_covered_minute_nanos: Option<i64>,
    session_first_minute_nanos: i64,
    session_last_minute_nanos: i64,
) -> Vec<i64> {
    let end_exclusive = first_covered_minute_nanos
        .unwrap_or(i64::MAX)
        .min(session_last_minute_nanos.saturating_add(NANOS_PER_MINUTE));
    let mut out = Vec::new();
    let mut m = session_first_minute_nanos;
    while m < end_exclusive {
        out.push(m);
        m = m.saturating_add(NANOS_PER_MINUTE);
    }
    out
}

/// `true` when a vendor candle's OHLC is implausible — any non-positive
/// O/H/L/C or `high < low` (hostile round 1 item 9). The row is STILL
/// persisted verbatim (we mirror the vendor), but never silently: the
/// caller counts + logs one coded line per fired minute. Pure.
fn candle_ohlc_implausible(c: &MinuteCandle) -> bool {
    c.open <= 0.0 || c.high <= 0.0 || c.low <= 0.0 || c.close <= 0.0 || c.high < c.low
}

/// How many EXTRA candles beyond the first share the target minute bucket
/// (hostile round 1 item 11 — first-match-wins stays, duplicates are
/// counted + debug-logged, never invisible). Pure.
fn duplicate_minute_candles(candles: &[MinuteCandle], minute_nanos: i64) -> usize {
    candles
        .iter()
        .filter(|c| c.minute_ts_ist_nanos == minute_nanos)
        .count()
        .saturating_sub(1)
}

/// Per-symbol latest successfully PERSISTED minute (append + flush
/// confirmed). Drives the previous-minute backfill + the post-session
/// sweep; commits are max-merge so a double persist of the same minute
/// (DEDUP-idempotent server-side) never regresses the watermark. In-memory
/// only — a restart re-fills via the backfill lookback + the sweep.
#[derive(Debug, Default)]
struct PersistTracker {
    committed: HashMap<i64, i64>,
}

impl PersistTracker {
    /// The latest persisted minute (IST nanos) for this security id, if any.
    fn last_persisted(&self, security_id: i64) -> Option<i64> {
        self.committed.get(&security_id).copied()
    }

    /// Commit a persisted minute — max-merge (idempotent; never regresses).
    fn commit(&mut self, security_id: i64, minute_nanos: i64) {
        let entry = self.committed.entry(security_id).or_insert(minute_nanos);
        if *entry < minute_nanos {
            *entry = minute_nanos;
        }
    }
}

// ---------------------------------------------------------------------------
// Pure ladder / token pacing decisions
// ---------------------------------------------------------------------------

/// Sleep DELTAS (ms) between ladder attempts, derived from the constant
/// offsets-from-first-attempt so the schedule stays one source of truth.
/// Pure.
fn groww_retry_sleep_deltas_ms() -> [u64; 4] {
    let o = GROWW_SPOT_1M_RETRY_OFFSETS_MS;
    [
        o[0],
        o[1].saturating_sub(o[0]),
        o[2].saturating_sub(o[1]),
        o[3].saturating_sub(o[2]),
    ]
}

/// `true` when an SSM re-read of the shared token is allowed — the
/// token-minter lock's ≥60 s pacing (never hammer SSM on a 401 storm;
/// NEVER mint). A never-read cache is always allowed. Pure.
fn should_reread_token(last_read_ms: Option<i64>, now_ms: i64) -> bool {
    match last_read_ms {
        None => true,
        Some(last) => {
            now_ms.saturating_sub(last)
                >= (GROWW_SPOT_1M_TOKEN_REREAD_FLOOR_SECS as i64).saturating_mul(MILLIS_PER_SEC)
        }
    }
}

/// Bounded failure slug for the forensics row — NEVER raw body text. Pure.
pub(crate) fn error_class_for_status(status: u16) -> &'static str {
    match status {
        0 => "transport",
        401 | 403 => "auth",
        429 => "rate_limited",
        400..=499 => "http_4xx",
        500..=599 => "http_5xx",
        _ => "http_other",
    }
}

// ---------------------------------------------------------------------------
// Wall-clock helpers (IST)
// ---------------------------------------------------------------------------

/// IST seconds-of-day from the wall clock (the rest_canary helper).
fn ist_secs_of_day_now() -> u32 {
    let now_ist = chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    // rem_euclid of a positive modulus is < SECONDS_PER_DAY; the cast is safe.
    now_ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32
}

/// IST milliseconds-of-day from the wall clock (close→data latency math).
fn ist_millis_of_day_now() -> i64 {
    let now_ist_ms = chrono::Utc::now()
        .timestamp_millis()
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS) * MILLIS_PER_SEC);
    now_ist_ms.rem_euclid(MILLIS_PER_DAY)
}

/// IST calendar date for "now" (the orphan-watchdog helper).
fn today_ist() -> NaiveDate {
    let utc = DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0).unwrap_or_default();
    (utc + ChronoDuration::seconds(i64::from(IST_UTC_OFFSET_SECONDS))).date_naive()
}

/// Wall-clock epoch millis (token re-read pacing).
fn epoch_millis_now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Retrieval wall-clock instant as IST nanoseconds (`Utc::now()` source ⇒
/// ADD the IST offset per `data-integrity.md`).
fn fetched_at_ist_nanos_now() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS).saturating_mul(NANOS_PER_SEC))
}

// ---------------------------------------------------------------------------
// Token cache (shared-minter READ-ONLY consumer)
// ---------------------------------------------------------------------------

/// In-process cache of the shared-minter Groww access token: read READ-ONLY
/// from SSM (`fetch_groww_access_token`), dropped on any auth-class reject
/// (never cached past an auth failure), re-read at ≥60 s pacing — NEVER
/// minted (`groww-shared-token-minter-2026-07-02.md`). Shared with the
/// PR-3 chain leg (`groww_option_chain_1m_boot.rs`) via [`Self::new_chain`]
/// — the token is ONE shared resource; each leg owns its OWN cache instance
/// so the read-failure error code + counter route to that leg's taxonomy.
pub(crate) struct GrowwTokenCache {
    token: Option<SecretString>,
    last_read_ms: Option<i64>,
    /// WHICH Groww REST leg owns this cache — routes the SSM read-failure
    /// emit to that leg's error-code/counter taxonomy. Behaviour (pacing,
    /// drop-on-auth-reject) is identical across legs.
    leg: GrowwRestLegRoute,
}

/// The owning REST leg of a [`GrowwTokenCache`] — error-routing only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GrowwRestLegRoute {
    /// Spot 1m leg (SPOT1M-01 / `tv_groww_spot1m_*`).
    Spot,
    /// Option-chain leg (CHAIN-02 / `tv_groww_chain1m_*`).
    Chain,
    /// Per-contract 1m leg (SPOT1M-01 leg=contract_1m /
    /// `tv_groww_contract1m_*` — PR-4).
    Contract,
}

impl GrowwTokenCache {
    // TEST-EXEMPT: trivial constructor — the pacing decision it feeds (should_reread_token) is unit-tested below.
    pub(crate) fn new() -> Self {
        Self {
            token: None,
            last_read_ms: None,
            leg: GrowwRestLegRoute::Spot,
        }
    }

    /// Chain-leg constructor (PR-3) — same cache semantics, chain-routed
    /// read-failure emits.
    // TEST-EXEMPT: trivial constructor — same tested semantics as new(); the routing flag is exercised by the chain wiring guard's stub scan.
    pub(crate) fn new_chain() -> Self {
        Self {
            leg: GrowwRestLegRoute::Chain,
            ..Self::new()
        }
    }

    /// Contract-leg constructor (PR-4) — same cache semantics,
    /// contract-routed read-failure emits.
    // TEST-EXEMPT: trivial constructor — same tested semantics as new(); the routing flag is exercised by the contract wiring guard's stub scan.
    pub(crate) fn new_contract() -> Self {
        Self {
            leg: GrowwRestLegRoute::Contract,
            ..Self::new()
        }
    }

    /// Test-only: a chain-leg cache whose last SSM read is "just now" and
    /// holds NO token — `ensure_token` then returns `None` WITHOUT any
    /// network read (the pacing floor), so the no-token fire arms are
    /// testable hermetically.
    #[cfg(test)]
    // TEST-EXEMPT: cfg(test)-only constructor consumed by the hermetic fire-arm tests below — guard raw-grep false positive
    pub(crate) fn for_test_paced_out(now_ms: i64) -> Self {
        Self {
            token: None,
            last_read_ms: Some(now_ms),
            leg: GrowwRestLegRoute::Chain,
        }
    }

    /// Test-only: a chain-leg cache PRELOADED with a token — `ensure_token`
    /// returns it without any SSM read, so the token-path fire arms are
    /// testable hermetically against a mock server.
    #[cfg(test)]
    // TEST-EXEMPT: cfg(test)-only constructor consumed by the hermetic fire-arm tests below — guard raw-grep false positive
    pub(crate) fn for_test_with_token(token: SecretString) -> Self {
        Self {
            token: Some(token),
            last_read_ms: None,
            leg: GrowwRestLegRoute::Chain,
        }
    }

    /// The cached token, reading from SSM (paced) when absent. `None` when
    /// the read is not yet due or failed — the caller counts the minute as
    /// a no-token miss and the next fire retries (fires are 60 s apart, so
    /// pacing never starves recovery).
    // TEST-EXEMPT: live-SSM async read — the pacing decision (should_reread_token) is unit-tested; the emit routing is pinned by the wiring guards.
    pub(crate) async fn ensure_token(&mut self) -> Option<SecretString> {
        if self.token.is_none() && should_reread_token(self.last_read_ms, epoch_millis_now()) {
            self.last_read_ms = Some(epoch_millis_now());
            match fetch_groww_access_token().await {
                Ok(token) => {
                    info!(
                        leg = ?self.leg,
                        "groww_rest_1m: shared access token read from SSM \
                         (read-only; minted by the token-minter Lambda)"
                    );
                    self.token = Some(token);
                }
                Err(err) if self.leg == GrowwRestLegRoute::Chain => {
                    metrics::counter!("tv_groww_chain1m_token_read_failed_total").increment(1);
                    error!(
                        code = ErrorCode::Chain02FetchDegraded.code_str(),
                        stage = "token_read",
                        feed = SPOT_1M_REST_FEED_GROWW,
                        ?err,
                        "CHAIN-02: shared Groww access token SSM read failed \
                         — the chain leg's minutes miss until the read \
                         succeeds (re-read paced at 60s; NEVER minted)"
                    );
                }
                Err(err) if self.leg == GrowwRestLegRoute::Contract => {
                    metrics::counter!("tv_groww_contract1m_token_read_failed_total").increment(1);
                    error!(
                        code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                        stage = "token_read",
                        feed = SPOT_1M_REST_FEED_GROWW,
                        leg = "contract_1m",
                        ?err,
                        "SPOT1M-01: shared Groww access token SSM read failed \
                         — the contract leg's minutes miss until the read \
                         succeeds (re-read paced at 60s; NEVER minted)"
                    );
                }
                Err(err) => {
                    metrics::counter!("tv_groww_spot1m_token_read_failed_total").increment(1);
                    error!(
                        code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                        stage = "token_read",
                        feed = SPOT_1M_REST_FEED_GROWW,
                        ?err,
                        "SPOT1M-01: shared Groww access token SSM read failed \
                         — this leg's minutes miss until the read succeeds \
                         (re-read paced at 60s; NEVER minted)"
                    );
                }
            }
        }
        self.token.clone()
    }

    /// Drop the cached token after an auth-class reject (401/403 — the
    /// ~06:00 IST daily reset surfaces this way): the NEXT `ensure_token`
    /// re-reads SSM at the pacing floor. Never mints.
    // TEST-EXEMPT: one-line state drop — exercised via the mock-401 short-circuit tokio test below.
    pub(crate) fn note_auth_rejected(&mut self) {
        if self.token.take().is_some() {
            warn!(
                leg = ?self.leg,
                "groww_rest_1m: auth-class reject — cached token dropped; \
                 re-reading from SSM at >=60s pacing (never minting)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Fetch ladder
// ---------------------------------------------------------------------------

/// One symbol's per-minute fetch verdict after the bounded ladder. Every
/// arm carries the PREVIOUS-minute backfill candle when the backfill was
/// due and any 2xx body of the ladder contained it (the #1499
/// vendor-lateness recovery path).
#[derive(Clone, Debug, PartialEq)]
enum SymbolFetchOutcome {
    /// The target minute's candle was retrieved (`close_to_data_ms` =
    /// minute close → retrieval wall-clock latency).
    Found {
        candle: MinuteCandle,
        close_to_data_ms: i64,
        backfill_candle: Option<MinuteCandle>,
    },
    /// Every attempt got a parseable 2xx but the target minute never
    /// appeared (incl. the STALE-body case: only earlier minutes served —
    /// the client-side minute filter detected it). Counted
    /// `outcome="empty"`, included in the failure edge.
    Empty {
        backfill_candle: Option<MinuteCandle>,
    },
    /// The last attempt (transport / non-2xx) failure, bounded + redacted.
    Failed {
        reason: String,
        backfill_candle: Option<MinuteCandle>,
    },
}

/// One attempt's typed failure — classification from the REAL
/// `StatusCode`, never a substring scan.
#[derive(Clone, Debug, PartialEq)]
struct FetchFailure {
    /// HTTP status (0 = the request never got a response).
    status: u16,
    rate_limited: bool,
    auth_rejected: bool,
    msg: String,
}

/// Per-ladder forensics for the `rest_fetch_audit` row: attempts actually
/// made, 429 count, the FINAL attempt's status + round-trip, the bounded
/// failure slug, and whether an auth-class reject was seen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LadderForensics {
    attempts: u32,
    rate_limited_count: u32,
    final_http_status: u16,
    final_latency_ms: i64,
    error_class: &'static str,
    auth_rejected: bool,
}

/// Read a response body with the [`GROWW_SPOT_1M_MAX_BODY_BYTES`] cap
/// enforced BOTH on the declared `Content-Length` and on the streamed
/// accumulation (csv_downloader §18 pattern, via the shared pure cap fns).
async fn read_body_capped(mut resp: reqwest::Response) -> Result<String, String> {
    if !declared_len_within_cap(resp.content_length(), GROWW_SPOT_1M_MAX_BODY_BYTES) {
        return Err(format!(
            "body too large: declared {} bytes > cap {GROWW_SPOT_1M_MAX_BODY_BYTES}",
            resp.content_length().unwrap_or_default()
        ));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("read: {e}"))? {
        if !accumulation_within_cap(buf.len(), chunk.len(), GROWW_SPOT_1M_MAX_BODY_BYTES) {
            return Err(format!(
                "body exceeded cap {GROWW_SPOT_1M_MAX_BODY_BYTES} bytes mid-stream"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| "body not valid UTF-8".to_string())
}

/// One Groww candles REST round-trip → the raw 2xx body text. `Err`
/// carries the REAL status + a ≤300-char secret-redacted body (the
/// DHAN-REST-400 capture discipline; the token travels ONLY in the
/// `Authorization` header — never in the URL, never logged). A 429
/// additionally records the live-probe (e) shape: endpoint + Retry-After
/// presence + sanitized body, via one bounded `warn!` per occurrence.
async fn groww_fetch_once(
    client: &reqwest::Client,
    url: &str,
    query: &[(&'static str, String); 6],
    token: &SecretString,
) -> Result<String, FetchFailure> {
    let resp = client
        .get(url)
        .query(query)
        .bearer_auth(token.expose_secret())
        .header(GROWW_API_VERSION_HEADER, GROWW_API_VERSION_VALUE)
        .send()
        .await
        .map_err(|e| FetchFailure {
            status: 0,
            rate_limited: false,
            auth_rejected: false,
            msg: format!("send: {e}"),
        })?;
    let status = resp.status();
    if !status.is_success() {
        let rate_limited = status == reqwest::StatusCode::TOO_MANY_REQUESTS;
        let auth_rejected =
            status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN;
        let retry_after_present = resp.headers().contains_key("retry-after");
        let error_body = read_body_capped(resp).await.unwrap_or_default();
        let captured = capture_rest_error_body(&error_body);
        if rate_limited {
            // Live-probe (e): every 429's timestamp (this log's own ts),
            // endpoint, Retry-After presence and body shape — bounded +
            // sanitized, never raw.
            metrics::counter!("tv_groww_spot1m_rate_limited_total").increment(1);
            warn!(
                endpoint = url,
                retry_after_present,
                body = %captured,
                "groww_spot_1m: HTTP 429 rate-limited (shared Live-Data \
                 bucket suspect) — counted; never retried past the ladder"
            );
        }
        return Err(FetchFailure {
            status: status.as_u16(),
            rate_limited,
            auth_rejected,
            msg: format!("http {status} retry_after_present={retry_after_present} body={captured}"),
        });
    }
    read_body_capped(resp).await.map_err(|msg| FetchFailure {
        status: status.as_u16(),
        rate_limited: false,
        auth_rejected: false,
        msg,
    })
}

/// Bounded in-minute re-poll ladder for ONE symbol: first attempt at the
/// fire instant, then re-polls at [`GROWW_SPOT_1M_RETRY_OFFSETS_MS`] until
/// the target minute's candle appears — after the last offset the minute
/// is `Empty`/`Failed`, never an unbounded in-minute retry. Every 2xx body
/// is filtered CLIENT-SIDE to the exact minute (a stale body serving only
/// earlier minutes rides the ladder as target-absent) and mined for the
/// due previous-minute backfill (sticky — the first hit wins). An
/// AUTH-CLASS reject (401/403) SHORT-CIRCUITS the remaining rungs
/// immediately (hostile round 1 item 12 — every further rung with the same
/// dead token is a doomed 401; the caller drops the token cache + skips
/// the remaining symbols for this fire).
async fn fetch_minute_with_ladder(
    client: &reqwest::Client,
    url: &str,
    query: &[(&'static str, String); 6],
    token: &SecretString,
    target_minute_ist_nanos: i64,
    backfill_minute_ist_nanos: Option<i64>,
    minute_close_ms_of_day: i64,
) -> (SymbolFetchOutcome, LadderForensics) {
    let deltas = groww_retry_sleep_deltas_ms();
    let mut last_error: Option<String> = None;
    let mut backfill_found: Option<MinuteCandle> = None;
    let mut forensics = LadderForensics {
        error_class: "none",
        ..LadderForensics::default()
    };
    for attempt in 0..=deltas.len() {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(deltas[attempt - 1])).await;
        }
        let started = std::time::Instant::now();
        let result = groww_fetch_once(client, url, query, token).await;
        let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        #[allow(clippy::cast_precision_loss)] // APPROVED: histogram sample only
        metrics::histogram!("tv_groww_spot1m_fetch_duration_ms").record(latency_ms as f64);
        forensics.attempts = forensics.attempts.saturating_add(1);
        forensics.final_latency_ms = latency_ms;
        match result {
            Ok(body_text) => {
                forensics.final_http_status = 200;
                forensics.error_class = "none";
                let (candles, stats) = parse_groww_1m_candles(&body_text);
                record_parse_stats(stats);
                if backfill_found.is_none() {
                    backfill_found =
                        backfill_minute_ist_nanos.and_then(|b| select_minute_candle(&candles, b));
                }
                if let Some(candle) = select_minute_candle(&candles, target_minute_ist_nanos) {
                    // Item 11: first-match-wins stays; duplicates of the
                    // target minute are counted + debug-logged, never
                    // invisible.
                    let dupes = duplicate_minute_candles(&candles, target_minute_ist_nanos);
                    if dupes > 0 {
                        metrics::counter!("tv_groww_spot1m_duplicate_candle_total")
                            .increment(dupes as u64);
                        tracing::debug!(
                            target_minute_ist_nanos,
                            duplicates = dupes,
                            "groww_spot_1m: vendor served duplicate candles \
                             for the target minute — first match kept"
                        );
                    }
                    let close_to_data_ms =
                        (ist_millis_of_day_now() - minute_close_ms_of_day).max(0);
                    return (
                        SymbolFetchOutcome::Found {
                            candle,
                            close_to_data_ms,
                            backfill_candle: backfill_found,
                        },
                        forensics,
                    );
                }
                // 2xx without the target minute — either not sealed yet or
                // a STALE body (earlier minutes only); the next rung
                // re-polls. The empty/stale class is stamped only if the
                // whole ladder ends target-absent.
                forensics.error_class = "target_absent";
                last_error = None;
            }
            Err(failure) => {
                if failure.rate_limited {
                    forensics.rate_limited_count = forensics.rate_limited_count.saturating_add(1);
                }
                forensics.auth_rejected |= failure.auth_rejected;
                forensics.final_http_status = failure.status;
                forensics.error_class = error_class_for_status(failure.status);
                last_error = Some(failure.msg);
                if failure.auth_rejected {
                    // Item 12: an auth-class reject dooms every further
                    // rung with the same token — abort the ladder NOW; the
                    // caller drops the token cache + short-circuits the
                    // remaining symbols for this fire.
                    break;
                }
            }
        }
    }
    let outcome = match last_error {
        Some(reason) => SymbolFetchOutcome::Failed {
            reason,
            backfill_candle: backfill_found,
        },
        None => SymbolFetchOutcome::Empty {
            backfill_candle: backfill_found,
        },
    };
    (outcome, forensics)
}

/// The ladder wrapped in the HARD per-symbol wall-clock budget
/// ([`GROWW_SPOT_1M_SYMBOL_BUDGET_SECS`]): a budget overrun is that
/// symbol's failure for the minute. HONEST bound (hostile round 1 item 8):
/// the `constants.rs` const-asserts pin ONLY the three LADDERS + fire
/// delay inside the minute (3 × budget + delay < 60 s) — the SSM token
/// read and the ILP append/flush legs run OUTSIDE that budget, so a whole
/// fire CAN still overrun the next boundary under a stalling SSM/QuestDB;
/// every boundary that elapses that way is counted LOUDLY by
/// `count_missed_boundaries` (H2) + the `boundary_skipped` forensics rows,
/// never silently. The overrun arm's forensics are honest sentinels (the
/// timed-out ladder's partial state is dropped with its future — attempts
/// reads 0, class `budget_exceeded`).
async fn fetch_minute_bounded(
    client: &reqwest::Client,
    url: &str,
    query: &[(&'static str, String); 6],
    token: &SecretString,
    target_minute_ist_nanos: i64,
    backfill_minute_ist_nanos: Option<i64>,
    minute_close_ms_of_day: i64,
) -> (SymbolFetchOutcome, LadderForensics) {
    match tokio::time::timeout(
        Duration::from_secs(GROWW_SPOT_1M_SYMBOL_BUDGET_SECS),
        fetch_minute_with_ladder(
            client,
            url,
            query,
            token,
            target_minute_ist_nanos,
            backfill_minute_ist_nanos,
            minute_close_ms_of_day,
        ),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_elapsed) => {
            metrics::counter!("tv_groww_spot1m_symbol_budget_exceeded_total").increment(1);
            (
                SymbolFetchOutcome::Failed {
                    reason: format!(
                        "ladder budget exceeded ({GROWW_SPOT_1M_SYMBOL_BUDGET_SECS}s) — peer stalling"
                    ),
                    backfill_candle: None,
                },
                LadderForensics {
                    attempts: 0,
                    rate_limited_count: 0,
                    final_http_status: 0,
                    final_latency_ms: -1,
                    error_class: "budget_exceeded",
                    auth_rejected: false,
                },
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Row builders
// ---------------------------------------------------------------------------

/// Build one `spot_1m_rest` row (feed='groww') from a parsed candle. The
/// `close_to_data_ms` stamp is the caller's HONEST retrieval delay
/// (own-fire latency, or the > 60 s real delay for a backfilled/swept
/// minute).
fn build_groww_spot_1m_row(
    candle: &MinuteCandle,
    security_id: i64,
    symbol: &'static str,
    trading_date_nanos: i64,
    close_to_data_ms: i64,
) -> Spot1mRestRow {
    Spot1mRestRow {
        ts_ist_nanos: candle.minute_ts_ist_nanos,
        trading_date_ist_nanos: trading_date_nanos,
        security_id,
        symbol,
        open: candle.open,
        high: candle.high,
        low: candle.low,
        close: candle.close,
        volume: candle.volume,
        close_to_data_ms,
        fetched_at_ist_nanos: fetched_at_ist_nanos_now(),
    }
}

/// Map one symbol's ladder verdict to the typed forensics outcome. Pure.
fn audit_outcome_for(
    outcome: &SymbolFetchOutcome,
    forensics: &LadderForensics,
) -> RestFetchOutcome {
    match outcome {
        SymbolFetchOutcome::Found { .. } => RestFetchOutcome::Ok,
        SymbolFetchOutcome::Empty { .. } => RestFetchOutcome::Empty,
        SymbolFetchOutcome::Failed { .. } => {
            if forensics.error_class == "rate_limited" {
                RestFetchOutcome::RateLimited
            } else {
                RestFetchOutcome::Error
            }
        }
    }
}

/// Build the per-fetch `rest_fetch_audit` row for one symbol's fired
/// minute (success AND failure — the "how did that minute's pull actually
/// go?" record). Pure.
#[allow(clippy::too_many_arguments)] // APPROVED: private forensics builder — a struct would be pure ceremony
fn build_fetch_audit_row(
    target_minute_ist_nanos: i64,
    trading_date_nanos: i64,
    security_id: i64,
    symbol: &'static str,
    forensics: &LadderForensics,
    outcome: RestFetchOutcome,
    close_to_data_ms: i64,
    error_class: &'static str,
) -> RestFetchAuditRow {
    RestFetchAuditRow {
        ts_ist_nanos: target_minute_ist_nanos,
        trading_date_ist_nanos: trading_date_nanos,
        feed: SPOT_1M_REST_FEED_GROWW,
        leg: REST_FETCH_LEG_SPOT_1M,
        security_id,
        exchange_segment: SPOT_1M_REST_SEGMENT_IDX_I,
        symbol,
        attempts: i64::from(forensics.attempts),
        final_http_status: i64::from(forensics.final_http_status),
        fetch_latency_ms: forensics.final_latency_ms,
        close_to_data_ms,
        rate_limited_count: i64::from(forensics.rate_limited_count),
        outcome,
        error_class,
    }
}

/// Forensics rows for the targets SKIPPED after an auth-class reject
/// short-circuited the fire (hostile round 1 item 12): no request is ever
/// sent for them with the dead token — outcome `no_token`, class `auth`,
/// 0/-1 sentinels (no HTTP happened). Pure — unit-tested below.
fn build_auth_short_circuit_rows(
    remaining: &[GrowwSpotTarget],
    target_minute_ist_nanos: i64,
    trading_date_nanos: i64,
) -> Vec<RestFetchAuditRow> {
    let forensics = LadderForensics {
        attempts: 0,
        rate_limited_count: 0,
        final_http_status: 0,
        final_latency_ms: -1,
        error_class: "auth",
        auth_rejected: true,
    };
    remaining
        .iter()
        .map(|target| {
            build_fetch_audit_row(
                target_minute_ist_nanos,
                trading_date_nanos,
                target.security_id,
                target.symbol,
                &forensics,
                RestFetchOutcome::NoToken,
                -1,
                "auth",
            )
        })
        .collect()
}

/// Best-effort forensics append: a failure logs (coded) + counts and
/// RETURNS — the fetch loop, the verdict and the failure edge are never
/// affected by the forensics leg.
fn audit_append_best_effort(audit_writer: &mut RestFetchAuditWriter, row: &RestFetchAuditRow) {
    if let Err(err) = audit_writer.append_row(row) {
        metrics::counter!("tv_rest_fetch_audit_persist_errors_total", "stage" => "audit_append")
            .increment(1);
        error!(
            code = ErrorCode::Spot1m02PersistFailed.code_str(),
            stage = "audit_append",
            feed = SPOT_1M_REST_FEED_GROWW,
            ?err,
            "SPOT1M-02: rest_fetch_audit row append failed (forensics only — \
             the fetch loop is unaffected)"
        );
    }
}

/// Best-effort forensics flush (same never-affects-the-loop contract).
fn audit_flush_best_effort(audit_writer: &mut RestFetchAuditWriter) {
    if let Err(err) = audit_writer.flush() {
        metrics::counter!("tv_rest_fetch_audit_persist_errors_total", "stage" => "audit_flush")
            .increment(1);
        error!(
            code = ErrorCode::Spot1m02PersistFailed.code_str(),
            stage = "audit_flush",
            feed = SPOT_1M_REST_FEED_GROWW,
            ?err,
            "SPOT1M-02: rest_fetch_audit ILP flush failed — pending forensics \
             rows discarded (best-effort; the fetch loop is unaffected)"
        );
    }
}

// ---------------------------------------------------------------------------
// Scheduler run + supervisor
// ---------------------------------------------------------------------------

/// Run today's remaining minute-close fires (then the one bounded
/// post-session sweep), then return. Never panics; every fault path logs
/// (coded, `feed="groww"`) + counts and continues to the next minute.
// TEST-EXEMPT: live-deps async runner — every scheduling / parsing / edge / backfill decision is a pure fn unit-tested below; the HTTP leg mirrors the tested Dhan spot pattern; wiring pinned by crates/app/tests/groww_spot_1m_wiring_guard.rs.
pub async fn run_groww_spot_1m(params: GrowwSpot1mTaskParams) {
    // Idempotent DDL first (CREATE → ADD COLUMN self-heal → DEDUP ENABLE)
    // for BOTH tables; failures degrade loudly inside and never block.
    ensure_spot_1m_rest_table(&params.questdb).await;
    ensure_rest_fetch_audit_table(&params.questdb).await;

    if !params.calendar.is_trading_day_today() {
        info!("groww_spot_1m: non-trading day — skipping all minute fires");
        return;
    }
    // ONE long-lived client for the whole session (per-minute rebuild is
    // the exact TLS/resolver churn HTTP-CLIENT-01 §0 condemns); NEVER a
    // `Client::new()` panic fallback.
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(GROWW_SPOT_1M_REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "client_build",
                feed = SPOT_1M_REST_FEED_GROWW,
                ?err,
                "SPOT1M-01: HTTP client build failed — Groww per-minute spot \
                 fetch degraded; supervisor will retry after backoff"
            );
            metrics::counter!("tv_groww_spot1m_fetch_total", "outcome" => "error").increment(1);
            return;
        }
    };
    let mut writer = Spot1mRestWriter::new_with_feed(
        &params.questdb,
        SPOT_1M_REST_FEED_GROWW,
        SPOT_1M_REST_SOURCE_GROWW_CANDLES,
    );
    let mut audit_writer = RestFetchAuditWriter::new(&params.questdb);
    let mut edge = FailureEdge::default();
    let mut tracker = PersistTracker::default();
    let mut token_cache = GrowwTokenCache::new();
    // 2026-07-13 (operator scope, §38.7): the 4th target — INDIA VIX — is
    // RUNTIME-resolved from the day's watch file. Resolution is retried
    // lazily before every fire until it succeeds (the watch build lands
    // asynchronously after boot); an unresolved day degrades to the 3
    // core targets with ONE coded warn + a per-attempt counter.
    let mut targets = core_spot_targets();
    let mut vix_warned = false;
    // H1: the last boundary actually HANDLED (fired or skipped-stale) —
    // the next fire is always STRICTLY after it.
    let mut last_fired: Option<u32> = None;
    // Item 2: the first session minute THIS RUN actually covered (its first
    // fire's backfill minute, or the target when the backfill was
    // out-of-session). Everything before it is the run's pre-boot blind
    // window — the sweep names those minutes AUDIT-ONLY (no fetch; §38
    // forbids a bulk backfill). `None` = the run never fired.
    let mut first_covered: Option<i64> = None;
    info!(
        core_symbols = GROWW_SPOT_1M_SYMBOLS.len(),
        "groww_spot_1m: per-minute fetch loop armed (fires each minute close \
         09:16:00-15:30:00 IST, ~0.3-1.3s after the boundary; sequential \
         target pacing; INDIA VIX joins as the 4th target once \
         runtime-resolved — 2026-07-13 operator scope)"
    );

    loop {
        // Audit Rule 3: re-read the wall clock + trading-day verdict EVERY
        // iteration (a suspend can cross midnight and stale the verdict).
        if !params.calendar.is_trading_day_today() {
            info!("groww_spot_1m: no longer a trading day — exiting");
            return;
        }
        // Item 7: the trading date as of THIS iteration's start — a suspend
        // across IST midnight makes the post-wake `today_ist()` a DIFFERENT
        // day, and stamping pre-suspend session seconds onto it would file
        // forensics rows under the wrong trading date.
        let iter_date = today_ist();
        // 2026-07-13: retry the INDIA VIX runtime resolution until it
        // lands (fail-soft; the 3 core targets never wait on it).
        ensure_vix_target(&mut targets, &mut vix_warned, iter_date);
        let now = ist_secs_of_day_now();
        let Some(fire) = next_fire_after(now, last_fired) else {
            info!(
                "groww_spot_1m: past 15:30 IST — today's minute fires \
                 complete; running the one bounded post-session sweep"
            );
            run_post_session_sweep(
                &client,
                &params,
                &targets,
                &mut writer,
                &mut audit_writer,
                &mut tracker,
                &mut token_cache,
                first_covered,
            )
            .await;
            return;
        };
        let sleep_ms =
            u64::from(fire.saturating_sub(now)).saturating_mul(1_000) + GROWW_SPOT_1M_FIRE_DELAY_MS;
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;

        // Staleness gate: a suspend / clock step can wake us far past the
        // boundary. Skip + recompute; every boundary that elapsed while
        // asleep is COUNTED as missed (H2 — never silent).
        let woke = ist_secs_of_day_now();
        if !fire_is_fresh(fire, woke) {
            warn!(
                fire_secs = fire,
                woke_at_secs = woke,
                "groww_spot_1m: woke too far past the minute boundary \
                 (suspend/clock step?) — skipping this minute"
            );
            let missed = count_missed_boundaries(fire.saturating_sub(60), woke.saturating_sub(1));
            record_skipped_boundaries(
                &params,
                &targets,
                &mut edge,
                &mut audit_writer,
                missed,
                fire,
                iter_date,
            );
            if woke > fire {
                last_fired = Some((woke.min(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST) / 60) * 60);
            }
            continue;
        }

        // Item 2: the run's first fire covers its target + (when
        // in-session) the target's one-minute-lookback backfill minute —
        // everything earlier is the pre-boot blind window the sweep names.
        if first_covered.is_none() {
            let target = minute_open_ist_nanos(iter_date, fire.saturating_sub(60));
            let session_first =
                minute_open_ist_nanos(iter_date, SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST - 60);
            first_covered = Some(first_covered_minute(target, session_first));
        }
        fire_one_minute(
            &params,
            &client,
            &targets,
            &mut writer,
            &mut audit_writer,
            &mut edge,
            &mut tracker,
            &mut token_cache,
            fire,
        )
        .await;
        last_fired = Some(fire);
        // Chain-leg sequencing (PR-3): publish this fire's boundary
        // UNCONDITIONALLY — success OR failure (the chain must never block
        // on a failing spot leg; the Dhan seam's semantics). `send_replace`
        // never errors; no receiver = no cost.
        if let Some(tx) = &params.minute_done_tx {
            tx.send_replace(Some(fire));
        }
        // H2 overrun accounting: boundaries that fully elapsed DURING the
        // fire can never be fetched — count them loudly + feed the edge.
        let after = ist_secs_of_day_now();
        let missed = count_missed_boundaries(fire, after.saturating_sub(1));
        record_skipped_boundaries(
            &params,
            &targets,
            &mut edge,
            &mut audit_writer,
            missed,
            fire + 60,
            iter_date,
        );
        if missed > 0 {
            last_fired = Some((after.min(SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST) / 60) * 60);
        }
    }
}

/// Loud accounting for minute boundaries that elapsed UNFETCHED (fire
/// overrun / suspend / clock step): counter + ONE coalesced coded log +
/// each missed minute feeds the failure edge + one `outcome=skipped`
/// forensics row per (minute, symbol) so the hole is queryable (never
/// silent — Rule 11). `first_missed_boundary_secs` = the FIRST boundary
/// in the missed run (each targets the minute opening 60 s before it).
/// A skipped boundary skips EVERY current target (incl. a resolved INDIA
/// VIX), so forensics rows cover all of them; the edge feed below is the
/// whole-fire skip — core and VIX alike missed, so it is core-failed too.
fn record_skipped_boundaries(
    params: &GrowwSpot1mTaskParams,
    targets: &[GrowwSpotTarget],
    edge: &mut FailureEdge,
    audit_writer: &mut RestFetchAuditWriter,
    skipped: u32,
    first_missed_boundary_secs: u32,
    iter_date: NaiveDate,
) {
    if skipped == 0 {
        return;
    }
    metrics::counter!("tv_groww_spot1m_boundary_skipped_total").increment(u64::from(skipped));
    let around = format_minute_ist_12h(first_missed_boundary_secs);
    error!(
        code = ErrorCode::Spot1m01FetchDegraded.code_str(),
        stage = "boundary_skipped",
        feed = SPOT_1M_REST_FEED_GROWW,
        skipped,
        around = %around,
        "SPOT1M-01: Groww minute boundaries elapsed unfetched (fire overrun \
         / suspend) — those minutes stay absent (backfill/sweep may recover)"
    );
    // Item 7: a wake that crossed IST midnight would stamp the missed
    // PRE-SUSPEND session seconds onto the POST-WAKE date — wrong trading
    // date on every row. The counter + coalesced log above already fired;
    // skip the (mis-dated) forensics rows + edge accounting.
    let trading_date = today_ist();
    if trading_date != iter_date {
        warn!(
            %iter_date,
            %trading_date,
            "groww_spot_1m: wake crossed IST midnight — skipping the \
             boundary-skip forensics rows (they would carry the wrong \
             trading date); the day is over for this run"
        );
        return;
    }
    let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
    let skip_forensics = LadderForensics {
        attempts: 0,
        rate_limited_count: 0,
        final_http_status: 0,
        final_latency_ms: -1,
        error_class: "boundary_skipped",
        auth_rejected: false,
    };
    for i in 0..skipped {
        let boundary = first_missed_boundary_secs.saturating_add(i.saturating_mul(60));
        let minute_open_secs = boundary.saturating_sub(60);
        let target_nanos = minute_open_ist_nanos(trading_date, minute_open_secs);
        for target in targets {
            let row = build_fetch_audit_row(
                target_nanos,
                trading_date_nanos,
                target.security_id,
                target.symbol,
                &skip_forensics,
                RestFetchOutcome::Skipped,
                -1,
                "boundary_skipped",
            );
            audit_append_best_effort(audit_writer, &row);
        }
        if let EdgeAction::Page { consecutive } = edge.record_minute(true) {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "escalation",
                feed = SPOT_1M_REST_FEED_GROWW,
                consecutive,
                minute = %around,
                "SPOT1M-01: Groww per-minute spot fetch fully failed for \
                 consecutive minutes — paging (edge-triggered)"
            );
            params
                .notifier
                .notify(NotificationEvent::GrowwSpot1mFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: around.clone(),
                });
        }
    }
    audit_flush_best_effort(audit_writer);
}

/// One minute-close fire: SEQUENTIAL ladder fetches for the current
/// targets (3 core + the runtime-resolved INDIA VIX when present; pacing
/// rule — at most one in-flight request) → persist the target minute AND
/// the previous-minute backfill when due → forensics rows → counters →
/// edge accounting. Edge honesty: the verdict is the OWN target minute's
/// fetch + persist — a backfill hit never counts as this fire's `ok` and
/// never samples the own-fire histogram. Per-SID independence (2026-07-13
/// operator scope): each target runs its OWN bounded ladder/budget, so a
/// VIX failure can never delay the core 3, and the escalation edge feeds
/// ONLY from core outcomes via [`MinuteEdgeTally`].
#[allow(clippy::too_many_arguments)] // APPROVED: private fire sink over the run loop's owned state — a struct would be pure ceremony
async fn fire_one_minute(
    params: &GrowwSpot1mTaskParams,
    client: &reqwest::Client,
    targets: &[GrowwSpotTarget],
    writer: &mut Spot1mRestWriter,
    audit_writer: &mut RestFetchAuditWriter,
    edge: &mut FailureEdge,
    tracker: &mut PersistTracker,
    token_cache: &mut GrowwTokenCache,
    fire_secs_of_day: u32,
) {
    let minute_open_secs = fire_secs_of_day.saturating_sub(60);
    let minute_label = format_minute_ist_12h(minute_open_secs);
    let trading_date = today_ist();
    let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
    let target_nanos = minute_open_ist_nanos(trading_date, minute_open_secs);
    let session_first_nanos =
        minute_open_ist_nanos(trading_date, SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST - 60);
    let minute_close_ms = i64::from(fire_secs_of_day).saturating_mul(MILLIS_PER_SEC);

    let mut ok_count: usize = 0;
    let mut empty_count: usize = 0;
    let mut error_count: usize = 0;
    // M1: a minute is fully-OK for the edge ONLY when the fetch succeeded
    // AND append+flush confirmed (a day-long QuestDB outage must page).
    let mut persist_failed = false;
    // 2026-07-13: the escalation edge feeds ONLY from the 3 CORE indices —
    // a VIX-only failure never pages, core-all-failed pages even when VIX
    // alone succeeded.
    let mut tally = MinuteEdgeTally::default();
    let mut implausible_count: usize = 0;
    let mut sample_failure: Option<String> = None;
    // Minutes appended this fire, committed to the tracker ONLY after the
    // flush confirms (a failed flush discards the buffer — never a false
    // watermark advance).
    let mut staged: Vec<(i64, i64)> = Vec::new();

    if let Some(token) = token_cache.ensure_token().await {
        for (idx, target) in targets.iter().enumerate() {
            let security_id = target.security_id;
            let query = groww_candles_query(
                &target.groww_symbol,
                &target.exchange,
                &target.segment,
                trading_date,
            );
            let backfill_nanos = backfill_minute_nanos(
                tracker.last_persisted(security_id),
                target_nanos,
                session_first_nanos,
            );
            let (outcome, forensics) = fetch_minute_bounded(
                client,
                GROWW_HISTORICAL_CANDLES_URL,
                &query,
                &token,
                target_nanos,
                backfill_nanos,
                minute_close_ms,
            )
            .await;

            // Forensics row FIRST (success AND failure) — best-effort, the
            // verdict below is computed independently.
            let audit_outcome = audit_outcome_for(&outcome, &forensics);
            let audit_close_to_data = match &outcome {
                SymbolFetchOutcome::Found {
                    close_to_data_ms, ..
                } => *close_to_data_ms,
                _ => -1,
            };
            let audit_row = build_fetch_audit_row(
                target_nanos,
                trading_date_nanos,
                security_id,
                target.symbol,
                &forensics,
                audit_outcome,
                audit_close_to_data,
                if audit_outcome == RestFetchOutcome::Ok {
                    "none"
                } else {
                    forensics.error_class
                },
            );
            audit_append_best_effort(audit_writer, &audit_row);

            // Previous-minute backfill (any outcome arm): persist with the
            // HONEST real retrieval delay (> 60 s by construction). Never
            // counted as this fire's `ok`, never sampled into the own-fire
            // histogram.
            let (own_outcome, backfill_candle) = match outcome {
                SymbolFetchOutcome::Found {
                    candle,
                    close_to_data_ms,
                    backfill_candle,
                } => (Some((candle, close_to_data_ms)), backfill_candle),
                SymbolFetchOutcome::Empty { backfill_candle } => {
                    empty_count = empty_count.saturating_add(1);
                    metrics::counter!("tv_groww_spot1m_fetch_total", "outcome" => "empty")
                        .increment(1);
                    if !target.core {
                        // 2026-07-13 live-probe unknown: whether Groww's
                        // historical-candles endpoint SERVES India VIX.
                        // Its 2xx-without-the-minute emptiness gets its own
                        // static-label counter so the vendor answer is
                        // measurable; the daily sweep verdict pages the
                        // coalesced "VIX not served" warn.
                        metrics::counter!("tv_groww_spot1m_vix_empty_total").increment(1);
                    }
                    if sample_failure.is_none() {
                        sample_failure = Some(format!(
                            "{}: 2xx but the minute's candle never \
                             appeared within the re-poll ladder",
                            target.groww_symbol
                        ));
                    }
                    (None, backfill_candle)
                }
                SymbolFetchOutcome::Failed {
                    reason,
                    backfill_candle,
                } => {
                    error_count = error_count.saturating_add(1);
                    metrics::counter!("tv_groww_spot1m_fetch_total", "outcome" => "error")
                        .increment(1);
                    if sample_failure.is_none() {
                        sample_failure = Some(format!("{}: {reason}", target.groww_symbol));
                    }
                    (None, backfill_candle)
                }
            };
            if let Some((candle, close_to_data_ms)) = own_outcome {
                ok_count = ok_count.saturating_add(1);
                tally.note_ok(target.core);
                if candle_ohlc_implausible(&candle) {
                    implausible_count = implausible_count.saturating_add(1);
                }
                metrics::counter!("tv_groww_spot1m_fetch_total", "outcome" => "ok").increment(1);
                #[allow(clippy::cast_precision_loss)] // APPROVED: histogram sample only
                metrics::histogram!("tv_groww_spot1m_close_to_data_ms")
                    .record(close_to_data_ms as f64);
                let row = build_groww_spot_1m_row(
                    &candle,
                    security_id,
                    target.symbol,
                    trading_date_nanos,
                    close_to_data_ms,
                );
                if let Err(err) = writer.append_row(&row) {
                    persist_failed = true;
                    tally.note_append_failure(target.core);
                    metrics::counter!("tv_groww_spot1m_persist_errors_total", "stage" => "append")
                        .increment(1);
                    error!(
                        code = ErrorCode::Spot1m02PersistFailed.code_str(),
                        stage = "append",
                        feed = SPOT_1M_REST_FEED_GROWW,
                        security_id,
                        ?err,
                        "SPOT1M-02: spot_1m_rest (groww) row append failed"
                    );
                    if sample_failure.is_none() {
                        sample_failure = Some(format!("persist append failed: {err:#}"));
                    }
                } else {
                    staged.push((security_id, candle.minute_ts_ist_nanos));
                }
            }
            if let Some(backfill) = backfill_candle {
                if candle_ohlc_implausible(&backfill) {
                    implausible_count = implausible_count.saturating_add(1);
                }
                // Honest delay: the backfilled minute closed 60 s before
                // this fire's target minute close.
                let backfill_close_to_data_ms =
                    (ist_millis_of_day_now() - (minute_close_ms - 60 * MILLIS_PER_SEC)).max(0);
                let row = build_groww_spot_1m_row(
                    &backfill,
                    security_id,
                    target.symbol,
                    trading_date_nanos,
                    backfill_close_to_data_ms,
                );
                if let Err(err) = writer.append_row(&row) {
                    persist_failed = true;
                    tally.note_append_failure(target.core);
                    metrics::counter!("tv_groww_spot1m_persist_errors_total", "stage" => "append")
                        .increment(1);
                    error!(
                        code = ErrorCode::Spot1m02PersistFailed.code_str(),
                        stage = "append",
                        feed = SPOT_1M_REST_FEED_GROWW,
                        security_id,
                        ?err,
                        "SPOT1M-02: spot_1m_rest (groww) BACKFILL row append failed"
                    );
                } else {
                    metrics::counter!("tv_groww_spot1m_backfilled_total").increment(1);
                    info!(
                        security_id,
                        symbol = target.symbol,
                        backfill_close_to_data_ms,
                        "groww_spot_1m: previous minute backfilled from this \
                         fire's full-day response (DEDUP-idempotent)"
                    );
                    staged.push((security_id, backfill.minute_ts_ist_nanos));
                }
            }
            if forensics.auth_rejected {
                // Item 12: drop the dead token NOW and short-circuit the
                // remaining symbols for THIS fire — every further request
                // with the same rejected token is a doomed 401 (~15 wasted
                // rejects worst case). The next fire's ensure_token
                // re-reads SSM at the ≥60s floor (unchanged); NEVER a mint.
                token_cache.note_auth_rejected();
                let remaining = &targets[idx + 1..];
                if !remaining.is_empty() {
                    error_count = error_count.saturating_add(remaining.len());
                    warn!(
                        skipped_symbols = remaining.len(),
                        "groww_spot_1m: auth-class reject — remaining symbols \
                         short-circuited for this fire (no doomed requests); \
                         forensics rows still emitted"
                    );
                    for row in
                        build_auth_short_circuit_rows(remaining, target_nanos, trading_date_nanos)
                    {
                        metrics::counter!("tv_groww_spot1m_fetch_total", "outcome" => "error")
                            .increment(1);
                        audit_append_best_effort(audit_writer, &row);
                    }
                }
                break;
            }
        }
        if let Err(err) = writer.flush() {
            persist_failed = true;
            metrics::counter!("tv_groww_spot1m_persist_errors_total", "stage" => "flush")
                .increment(1);
            error!(
                code = ErrorCode::Spot1m02PersistFailed.code_str(),
                stage = "flush",
                feed = SPOT_1M_REST_FEED_GROWW,
                ?err,
                "SPOT1M-02: spot_1m_rest (groww) ILP flush failed — pending \
                 rows discarded (poisoned-buffer defense; minutes stay absent \
                 and re-fetchable via DEDUP-idempotent backfill/sweep)"
            );
            if sample_failure.is_none() {
                sample_failure = Some(format!("persist flush failed: {err:#}"));
            }
            tally.note_flush_failure();
        } else {
            // Flush confirmed — advance the per-symbol persisted watermark.
            for (security_id, minute_nanos) in staged {
                tracker.commit(security_id, minute_nanos);
            }
        }
        if implausible_count > 0 {
            // Item 9: implausible vendor OHLC (non-positive O/H/L/C or
            // high<low) is persisted VERBATIM (we mirror the vendor) but
            // never silently — one coded line per fired minute + counter.
            metrics::counter!("tv_groww_spot1m_implausible_ohlc_total")
                .increment(implausible_count as u64);
            warn!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "implausible_ohlc",
                feed = SPOT_1M_REST_FEED_GROWW,
                minute = %minute_label,
                candles = implausible_count,
                "SPOT1M-01: vendor candle(s) with implausible OHLC \
                 (non-positive or high<low) — persisted verbatim, flagged"
            );
        }
    } else {
        // No token at fire time — nothing can be sent; the whole minute is
        // a full miss (counted per target for honest rate math; core_ok
        // stays 0, so the edge sees a fully-failed minute) + one no_token
        // forensics row per target.
        error_count = targets.len();
        sample_failure = Some("no shared Groww access token available at fire time".to_string());
        let no_token_forensics = LadderForensics {
            attempts: 0,
            rate_limited_count: 0,
            final_http_status: 0,
            final_latency_ms: -1,
            error_class: "no_token",
            auth_rejected: false,
        };
        for target in targets {
            metrics::counter!("tv_groww_spot1m_fetch_total", "outcome" => "error").increment(1);
            let row = build_fetch_audit_row(
                target_nanos,
                trading_date_nanos,
                target.security_id,
                target.symbol,
                &no_token_forensics,
                RestFetchOutcome::NoToken,
                -1,
                "no_token",
            );
            audit_append_best_effort(audit_writer, &row);
        }
    }
    audit_flush_best_effort(audit_writer);

    record_minute_verdict(
        params,
        edge,
        &minute_label,
        ok_count,
        error_count,
        empty_count,
        persist_failed,
        tally,
        sample_failure.as_deref(),
    );
}

/// Coalesced per-minute verdict: ONE coded log per fired minute with any
/// failure, plus the edge-triggered escalation page / recovery ping
/// (typed Groww events). 2026-07-13: the edge verdict comes from the CORE
/// tally ([`MinuteEdgeTally`]) — a VIX-only failure never pages; the total
/// ok/error/empty counts stay in the log for full visibility.
#[allow(clippy::too_many_arguments)] // APPROVED: private verdict sink — a struct would be pure ceremony
fn record_minute_verdict(
    params: &GrowwSpot1mTaskParams,
    edge: &mut FailureEdge,
    minute_label: &str,
    ok_count: usize,
    error_count: usize,
    empty_count: usize,
    persist_failed: bool,
    tally: MinuteEdgeTally,
    sample_failure: Option<&str>,
) {
    let fully_failed = tally.fully_failed();
    let action = edge.record_minute(fully_failed);
    match action {
        EdgeAction::Page { consecutive } => {
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "escalation",
                feed = SPOT_1M_REST_FEED_GROWW,
                consecutive,
                minute = minute_label,
                sample = sample_failure.unwrap_or("none captured"),
                "SPOT1M-01: Groww per-minute spot fetch fully failed for \
                 consecutive minutes — paging (edge-triggered)"
            );
            params
                .notifier
                .notify(NotificationEvent::GrowwSpot1mFetchDegraded {
                    consecutive_failed_minutes: consecutive,
                    minute_ist: minute_label.to_string(),
                });
        }
        EdgeAction::Recover { failed_minutes } => {
            info!(
                failed_minutes,
                minute = minute_label,
                "groww_spot_1m: per-minute fetch recovered after a paged episode"
            );
            params
                .notifier
                .notify(NotificationEvent::GrowwSpot1mFetchRecovered {
                    minute_ist: minute_label.to_string(),
                    failed_minutes,
                });
        }
        EdgeAction::None => {
            if error_count > 0 || empty_count > 0 || persist_failed {
                // Coalesced ONCE per fire (never per retry); log-sink-only
                // — sub-edge failures never page (the escalation arm does).
                error!(
                    code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                    stage = "minute_failed",
                    feed = SPOT_1M_REST_FEED_GROWW,
                    minute = minute_label,
                    ok = ok_count,
                    core_ok = tally.core_ok,
                    errors = error_count,
                    empty = empty_count,
                    persist_failed,
                    sample = sample_failure.unwrap_or("none captured"),
                    "SPOT1M-01: Groww per-minute spot fetch degraded for this minute"
                );
            }
        }
    }
}

/// ONE bounded post-session repair sweep (~15:31:00 IST, single fire —
/// the #1499 pattern): once the session is final, re-fetch the proven day
/// window ONCE per symbol that still has session minutes above its
/// persisted watermark (the 15:29 candle after a vendor-late seal, a
/// flush-failed backfill row, any ≥2-fire-old gap the one-minute lookback
/// could not reach) and persist every one found. A minute the sweep STILL
/// cannot recover becomes a NAMED GAP — one `rest_fetch_audit` row per
/// (minute, symbol) with `error_class="named_gap"` + the still-missing
/// counter + one coalesced coded log — never a silent hole. Bounded: ≤3
/// requests, ≤375 minutes/symbol, DEDUP-idempotent re-appends; the
/// membership filter is O(missing × candles) ≤ 375×375 once per day —
/// cold path, flagged honestly.
// TEST-EXEMPT: live-deps async runner — the missing-minute selection (sweep_missing_minutes), row builders and named-gap mapping are pure fns unit-tested below; the HTTP+persist legs reuse the tested fire_one_minute pattern.
#[allow(clippy::too_many_arguments)] // APPROVED: private once-a-day sweep sink over the run loop's owned state — a struct would be pure ceremony
async fn run_post_session_sweep(
    client: &reqwest::Client,
    params: &GrowwSpot1mTaskParams,
    targets: &[GrowwSpotTarget],
    writer: &mut Spot1mRestWriter,
    audit_writer: &mut RestFetchAuditWriter,
    tracker: &mut PersistTracker,
    token_cache: &mut GrowwTokenCache,
    first_covered: Option<i64>,
) {
    // Wait for the sweep instant (a run reaching here right at 15:30:00
    // sleeps ~60s; a late boot past 15:31 fires immediately).
    let now = ist_secs_of_day_now();
    if now < GROWW_SPOT_1M_SWEEP_FIRE_SECS_OF_DAY_IST {
        tokio::time::sleep(Duration::from_secs(u64::from(
            GROWW_SPOT_1M_SWEEP_FIRE_SECS_OF_DAY_IST - now,
        )))
        .await;
    }
    // Same-day defense: a suspend across midnight or a day flip means the
    // session data is no longer "today's" — skip rather than stamp the
    // wrong trading date.
    let woke = ist_secs_of_day_now();
    if woke < SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST || !params.calendar.is_trading_day_today() {
        warn!(
            woke_at_secs = woke,
            "groww_spot_1m: post-session sweep woke outside today's session \
             (midnight wrap / non-trading day) — skipping"
        );
        return;
    }
    let trading_date = today_ist();
    let trading_date_nanos = minute_open_ist_nanos(trading_date, 0);
    let session_first =
        minute_open_ist_nanos(trading_date, SPOT_1M_REST_FIRST_FIRE_SECS_OF_DAY_IST - 60);
    let session_last =
        minute_open_ist_nanos(trading_date, SPOT_1M_REST_LAST_FIRE_SECS_OF_DAY_IST - 60);
    let token = token_cache.ensure_token().await;

    let mut swept: u64 = 0;
    let mut still_missing: u64 = 0;
    let mut pre_boot_named: u64 = 0;
    let mut staged: Vec<(i64, i64)> = Vec::new();
    let mut persist_failed = false;
    for target in targets {
        let security_id = target.security_id;
        // Item 2: the run's PRE-BOOT blind window [session_first,
        // first_covered) is named AUDIT-ONLY — one `named_gap`/`pre_boot`
        // forensics row per minute, NO fetch (§38 forbids a bulk
        // backfill). A whole-session blind run (never fired) names every
        // session minute and skips the fetch-sweep entirely.
        let pre_boot = pre_boot_gap_minutes(first_covered, session_first, session_last);
        if !pre_boot.is_empty() {
            pre_boot_named = pre_boot_named.saturating_add(pre_boot.len() as u64);
            metrics::counter!("tv_groww_spot1m_pre_boot_gap_total")
                .increment(pre_boot.len() as u64);
            record_named_gaps(
                audit_writer,
                &pre_boot,
                trading_date_nanos,
                security_id,
                target.symbol,
                RestFetchOutcome::NamedGap,
                "pre_boot",
                0,
            );
        }
        // The FETCH-sweep repairs only the run's OWN live window
        // [first_covered, session_last] above the persisted watermark.
        let Some(fetch_floor) = first_covered else {
            continue;
        };
        let missing = sweep_missing_minutes(
            tracker.last_persisted(security_id),
            fetch_floor.max(session_first),
            session_last,
        );
        if missing.is_empty() {
            continue;
        }
        let Some(token) = token.as_ref() else {
            // No token: every missing minute is a NAMED GAP (no_token).
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "sweep_failed",
                feed = SPOT_1M_REST_FEED_GROWW,
                security_id,
                "SPOT1M-01: no shared Groww access token at post-session \
                 sweep time — missing minutes stay absent (DEDUP-idempotent \
                 manual re-run remains possible)"
            );
            still_missing = still_missing.saturating_add(missing.len() as u64);
            record_named_gaps(
                audit_writer,
                &missing,
                trading_date_nanos,
                security_id,
                target.symbol,
                RestFetchOutcome::NoToken,
                "no_token",
                0,
            );
            continue;
        };
        let query = groww_candles_query(
            &target.groww_symbol,
            &target.exchange,
            &target.segment,
            trading_date,
        );
        let candles =
            match groww_fetch_once(client, GROWW_HISTORICAL_CANDLES_URL, &query, token).await {
                Ok(body_text) => {
                    let (candles, stats) = parse_groww_1m_candles(&body_text);
                    record_parse_stats(stats);
                    candles
                }
                Err(failure) => {
                    error!(
                        code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                        stage = "sweep_failed",
                        feed = SPOT_1M_REST_FEED_GROWW,
                        security_id,
                        reason = %failure.msg,
                        "SPOT1M-01: Groww post-session sweep fetch failed for this symbol"
                    );
                    still_missing = still_missing.saturating_add(missing.len() as u64);
                    record_named_gaps(
                        audit_writer,
                        &missing,
                        trading_date_nanos,
                        security_id,
                        target.symbol,
                        if failure.rate_limited {
                            RestFetchOutcome::RateLimited
                        } else {
                            RestFetchOutcome::Error
                        },
                        error_class_for_status(failure.status),
                        i64::from(failure.status),
                    );
                    continue;
                }
            };
        let mut found_for_sid: u64 = 0;
        let mut gaps: Vec<i64> = Vec::new();
        // Round-2 LOW: fetched-OK-but-append-failed minutes are a PERSIST
        // failure, not vendor absence — classed separately below.
        let mut persist_gaps: Vec<i64> = Vec::new();
        for minute_nanos in &missing {
            let Some(candle) = select_minute_candle(&candles, *minute_nanos) else {
                still_missing = still_missing.saturating_add(1);
                gaps.push(*minute_nanos);
                continue;
            };
            // Honest real retrieval delay for the swept minute.
            let close_ms_of_day = ((minute_nanos - trading_date_nanos) / NANOS_PER_SEC + 60)
                .saturating_mul(MILLIS_PER_SEC);
            let close_to_data_ms = (ist_millis_of_day_now() - close_ms_of_day).max(0);
            let row = build_groww_spot_1m_row(
                &candle,
                security_id,
                target.symbol,
                trading_date_nanos,
                close_to_data_ms,
            );
            if let Err(err) = writer.append_row(&row) {
                persist_failed = true;
                metrics::counter!("tv_groww_spot1m_persist_errors_total", "stage" => "append")
                    .increment(1);
                error!(
                    code = ErrorCode::Spot1m02PersistFailed.code_str(),
                    stage = "append",
                    feed = SPOT_1M_REST_FEED_GROWW,
                    security_id,
                    ?err,
                    "SPOT1M-02: Groww post-session sweep row append failed"
                );
                still_missing = still_missing.saturating_add(1);
                persist_gaps.push(*minute_nanos);
            } else {
                found_for_sid = found_for_sid.saturating_add(1);
                staged.push((security_id, *minute_nanos));
            }
        }
        // NAMED GAPS: the finally-unrecovered minutes for this symbol —
        // one queryable forensics row each, never a silent hole. Item 6
        // semantics: outcome `named_gap` (a fetch DID happen and answered
        // 200 without the minute — the status is the ACTUAL last one),
        // never the misleading 200+`error` pair. Round-2 LOW: minutes
        // whose fetch SUCCEEDED but whose ILP append failed are a PERSIST
        // failure, not vendor absence — class `persist_failed` (same
        // `named_gap` outcome, so the DEDUP semantics are unchanged).
        record_named_gaps(
            audit_writer,
            &gaps,
            trading_date_nanos,
            security_id,
            target.symbol,
            RestFetchOutcome::NamedGap,
            "named_gap",
            200,
        );
        record_named_gaps(
            audit_writer,
            &persist_gaps,
            trading_date_nanos,
            security_id,
            target.symbol,
            RestFetchOutcome::NamedGap,
            "persist_failed",
            200,
        );
        swept = swept.saturating_add(found_for_sid);
    }
    if let Err(err) = writer.flush() {
        persist_failed = true;
        metrics::counter!("tv_groww_spot1m_persist_errors_total", "stage" => "flush").increment(1);
        error!(
            code = ErrorCode::Spot1m02PersistFailed.code_str(),
            stage = "flush",
            feed = SPOT_1M_REST_FEED_GROWW,
            ?err,
            "SPOT1M-02: Groww post-session sweep ILP flush failed — pending \
             swept rows discarded (poisoned-buffer defense)"
        );
        still_missing = still_missing.saturating_add(swept);
        swept = 0;
        // Item 4: the staged-but-unflushed swept minutes are STILL absent
        // from the table — without their own rows the counter bump above
        // would be the only trace. Name each one (`named_gap` /
        // `flush_failed`; the fetch itself answered 200) BEFORE the
        // best-effort audit flush below.
        for target in targets {
            let gap_sid = target.security_id;
            let gap_symbol = target.symbol;
            let lost: Vec<i64> = staged
                .iter()
                .filter(|(sid, _)| *sid == gap_sid)
                .map(|(_, minute)| *minute)
                .collect();
            record_named_gaps(
                audit_writer,
                &lost,
                trading_date_nanos,
                gap_sid,
                gap_symbol,
                RestFetchOutcome::NamedGap,
                "flush_failed",
                200,
            );
        }
    } else {
        for (security_id, minute_nanos) in staged {
            tracker.commit(security_id, minute_nanos);
        }
    }
    audit_flush_best_effort(audit_writer);
    metrics::counter!("tv_groww_spot1m_sweep_backfilled_total").increment(swept);
    metrics::counter!("tv_groww_spot1m_sweep_still_missing_total").increment(still_missing);
    if vix_not_served_verdict(targets, tracker) {
        // Operator scope 2026-07-13 (§38.7): ZERO India VIX minutes
        // persisted across the whole session (fires + sweep) while the
        // core indices DID persist — the VIX-specific "not served" answer
        // to the live-probe unknown. ONE coded warn per day, edge-latched
        // by construction (the sweep fires once per session); the
        // per-minute forensics + named-gap rows above already name every
        // VIX minute. Delivery boundary: log-sink-only, per the SPOT1M
        // codes' §3 contract in rest-1m-pipeline-error-codes.md.
        metrics::counter!("tv_groww_spot1m_vix_not_served_total").increment(1);
        warn!(
            code = ErrorCode::Spot1m01FetchDegraded.code_str(),
            stage = "vix_not_served",
            feed = SPOT_1M_REST_FEED_GROWW,
            "SPOT1M-01: India VIX not served by Groww historical-candles — \
             named gap rows written; other-broker coverage unaffected"
        );
    }
    if still_missing > 0 || persist_failed || pre_boot_named > 0 {
        error!(
            code = ErrorCode::Spot1m01FetchDegraded.code_str(),
            stage = "sweep_incomplete",
            feed = SPOT_1M_REST_FEED_GROWW,
            swept,
            still_missing,
            pre_boot = pre_boot_named,
            persist_failed,
            "SPOT1M-01: Groww post-session sweep left session minutes absent \
             — NAMED GAPS recorded in rest_fetch_audit (pre-boot minutes are \
             audit-only by design; DEDUP-idempotent manual re-run remains \
             possible)"
        );
    } else {
        info!(
            swept,
            "groww_spot_1m: post-session sweep complete — every session \
             minute above the watermark is persisted"
        );
    }
}

/// Gap-row forensics (item 6 + round-2 LOW semantics): `final_http_status`
/// is the ACTUAL last status when a fetch happened for the gap (e.g. the
/// sweep 200 that lacked the minute, or the 200 whose row failed the ILP
/// append — class `persist_failed`), 0 sentinel when none did (pre-boot /
/// no-token) — and attempts mirrors it (1 when a fetch ran, else 0).
/// Never a fabricated status/attempt pair. Pure.
fn named_gap_forensics(error_class: &'static str, final_http_status: i64) -> LadderForensics {
    LadderForensics {
        attempts: u32::from(final_http_status != 0),
        rate_limited_count: 0,
        final_http_status: u16::try_from(final_http_status).unwrap_or(0),
        final_latency_ms: -1,
        error_class,
        auth_rejected: false,
    }
}

/// One queryable forensics row per finally-unrecovered minute (the NAMED
/// GAP contract: a hole is a row, never silence). Best-effort appends.
#[allow(clippy::too_many_arguments)] // APPROVED: private forensics sink — a struct would be pure ceremony
fn record_named_gaps(
    audit_writer: &mut RestFetchAuditWriter,
    gap_minutes_nanos: &[i64],
    trading_date_nanos: i64,
    security_id: i64,
    symbol: &'static str,
    outcome: RestFetchOutcome,
    error_class: &'static str,
    final_http_status: i64,
) {
    let forensics = named_gap_forensics(error_class, final_http_status);
    for minute_nanos in gap_minutes_nanos {
        let row = build_fetch_audit_row(
            *minute_nanos,
            trading_date_nanos,
            security_id,
            symbol,
            &forensics,
            outcome,
            -1,
            error_class,
        );
        audit_append_best_effort(audit_writer, &row);
    }
}

/// Spawn the supervised Groww per-minute scheduler. The supervisor
/// respawns a dead/failed run after a bounded backoff so the scheduler
/// can never die silently mid-session, and exits cleanly once today's
/// window is over (non-trading day / past 15:30 IST, post-sweep) or on
/// graceful-shutdown cancel.
// TEST-EXEMPT: tokio supervisor wiring over the unit-tested pure decisions (spot_1m_day_is_over / classify_join_exit); spawn site pinned by crates/app/tests/groww_spot_1m_wiring_guard.rs.
pub fn spawn_supervised_groww_spot_1m(
    params: GrowwSpot1mTaskParams,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let inner = tokio::spawn(run_groww_spot_1m(params.clone()));
            let result = inner.await;
            let reason = classify_join_exit(&result);
            let day_over = spot_1m_day_is_over(
                ist_secs_of_day_now(),
                params.calendar.is_trading_day_today(),
            );
            match &result {
                Ok(()) if day_over => {
                    info!("groww_spot_1m: day complete — supervisor exiting");
                    return;
                }
                Err(join_err) if join_err.is_cancelled() => {
                    // Graceful shutdown teardown — not an abort.
                    return;
                }
                _ => {}
            }
            metrics::counter!("tv_groww_spot1m_task_respawn_total", "reason" => reason)
                .increment(1);
            error!(
                code = ErrorCode::Spot1m01FetchDegraded.code_str(),
                stage = "task_respawn",
                feed = SPOT_1M_REST_FEED_GROWW,
                reason,
                "SPOT1M-01: Groww per-minute spot fetch task died mid-window \
                 — respawning after backoff"
            );
            tokio::time::sleep(Duration::from_secs(GROWW_SPOT_1M_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- day window + query params ------------------------------------------

    #[test]
    fn test_groww_day_window_strings_day_granular_and_month_boundary() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let (start, end) = groww_day_window_strings(date);
        assert_eq!(start, "2026-07-13 00:00:00");
        assert_eq!(end, "2026-07-14 00:00:00");
        // Month boundary (the cross-verify succ_opt semantics).
        let eom = NaiveDate::from_ymd_opt(2026, 7, 31).expect("valid date");
        let (start, end) = groww_day_window_strings(eom);
        assert_eq!(start, "2026-07-31 00:00:00");
        assert_eq!(end, "2026-08-01 00:00:00");
    }

    /// Regression lock (the #1499 lesson applied from day one): the query
    /// carries the DAY-GRANULAR window + the SDK-verified param set with
    /// the `groww_symbol` identity and the `"1minute"` interval literal.
    #[test]
    fn test_regression_groww_candles_query_uses_proven_day_window() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let q = groww_candles_query("NSE-NIFTY", "NSE", "CASH", date);
        let get = |k: &str| {
            q.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or("")
        };
        assert_eq!(get("exchange"), "NSE");
        assert_eq!(get("segment"), "CASH");
        assert_eq!(get("groww_symbol"), "NSE-NIFTY");
        // The proven full-day window — NEVER a same-date minute window.
        assert_eq!(get("start_time"), "2026-07-13 00:00:00");
        assert_eq!(get("end_time"), "2026-07-14 00:00:00");
        assert_eq!(get("candle_interval"), "1minute");
        assert_eq!(q.len(), 6);
    }

    // ---- 2026-07-13: INDIA VIX — the runtime-resolved 4th spot target -------

    /// A watch-file JSON fixture carrying the given entries array.
    fn watch_json(date: &str, entries: &str) -> String {
        format!(
            r#"{{"trading_date_ist":"{date}","feed":"groww","count":1,
                 "resolved_stocks":0,"indices":1,"entries":[{entries}]}}"#
        )
    }

    /// A live-master-shaped VIX index entry (token `INDIAVIX`, display
    /// name "India Vix" — the 2026-06-28 `REAL_GROWW_NSE_INDICES`
    /// spellings) with the given groww_symbol + security_id.
    fn vix_entry_json(groww_symbol: &str, security_id: i64) -> String {
        format!(
            r#"{{"exchange":"NSE","segment":"CASH","exchange_token":"INDIAVIX",
                 "kind":"index_value","security_id":{security_id},
                 "index_name":"{groww_symbol}","symbol_name":"India Vix"}}"#
        )
    }

    #[test]
    fn test_core_spot_targets_pin_the_three_core_indices() {
        let targets = core_spot_targets();
        assert_eq!(targets.len(), 3, "the CORE set stays exactly 3");
        for (target, &(groww_symbol, symbol, exchange, segment)) in
            targets.iter().zip(GROWW_SPOT_1M_SYMBOLS.iter())
        {
            assert_eq!(target.groww_symbol, groww_symbol);
            assert_eq!(target.symbol, symbol);
            assert_eq!(target.exchange, exchange);
            assert_eq!(target.segment, segment);
            assert_eq!(
                target.security_id,
                stable_index_security_id(groww_symbol),
                "core ids reuse the canonical live-lane hash"
            );
            assert!(target.core, "const-table targets are all CORE");
        }
    }

    /// FOUND arms: the live-master token spelling (`INDIAVIX`, via the
    /// 2026-07-13 alias) resolves; so does a name-only match (token
    /// unrecognizable, display name "India Vix"). The resolved target
    /// carries the master's groww_symbol verbatim, the canonical id, and
    /// `core = false` (never feeds the escalation edge).
    #[test]
    fn test_vix_target_resolves_via_token_alias_and_display_name() {
        let sid = stable_index_security_id("NSE-INDIAVIX");
        let doc = parse_watch_file(&watch_json(
            "2026-07-13",
            &vix_entry_json("NSE-INDIAVIX", sid),
        ))
        .expect("fixture parses");
        let vix = vix_target_from_watch_doc(&doc, "2026-07-13").expect("VIX resolves");
        assert_eq!(vix.groww_symbol, "NSE-INDIAVIX");
        assert_eq!(vix.symbol, GROWW_SPOT_1M_VIX_SYMBOL);
        assert_eq!(vix.exchange, "NSE");
        assert_eq!(vix.segment, "CASH");
        assert_eq!(vix.security_id, sid);
        assert!(!vix.core, "VIX must never join the core escalation edge");

        // Name-only key: an unrecognizable token still resolves through
        // the display name (the index-coverage-audit key pair).
        let name_only = format!(
            r#"{{"exchange":"NSE","segment":"CASH","exchange_token":"IVX-UNKNOWN",
                 "kind":"index_value","security_id":{sid},
                 "index_name":"NSE-INDIAVIX","symbol_name":"India Vix"}}"#
        );
        let doc = parse_watch_file(&watch_json("2026-07-13", &name_only)).expect("parses");
        let vix = vix_target_from_watch_doc(&doc, "2026-07-13").expect("name key resolves");
        assert_eq!(vix.groww_symbol, "NSE-INDIAVIX");
    }

    /// FAIL-CLOSED arms — every anomaly refuses (`None`; the caller skips
    /// VIX for the day with the coded warn): no VIX row at all, a stale
    /// file (date mismatch), a missing `index_name`, an id that disagrees
    /// with the canonical hash, and a non-index (`ltp`) kind.
    #[test]
    fn test_vix_target_from_watch_doc_fail_closed_arms() {
        let sid = stable_index_security_id("NSE-INDIAVIX");

        // Absent: only a NIFTY entry.
        let nifty = format!(
            r#"{{"exchange":"NSE","segment":"CASH","exchange_token":"NIFTY",
                 "kind":"index_value","security_id":{}}}"#,
            stable_index_security_id("NSE-NIFTY")
        );
        let doc = parse_watch_file(&watch_json("2026-07-13", &nifty)).expect("parses");
        assert_eq!(vix_target_from_watch_doc(&doc, "2026-07-13"), None);

        // Stale file: yesterday's date must refuse.
        let doc = parse_watch_file(&watch_json(
            "2026-07-12",
            &vix_entry_json("NSE-INDIAVIX", sid),
        ))
        .expect("parses");
        assert_eq!(vix_target_from_watch_doc(&doc, "2026-07-13"), None);

        // Missing index_name: no groww_symbol to query with.
        let no_name = format!(
            r#"{{"exchange":"NSE","segment":"CASH","exchange_token":"INDIAVIX",
                 "kind":"index_value","security_id":{sid},"symbol_name":"India Vix"}}"#
        );
        let doc = parse_watch_file(&watch_json("2026-07-13", &no_name)).expect("parses");
        assert_eq!(vix_target_from_watch_doc(&doc, "2026-07-13"), None);

        // Derivation drift: entry id disagrees with the canonical hash.
        let doc = parse_watch_file(&watch_json(
            "2026-07-13",
            &vix_entry_json("NSE-INDIAVIX", sid + 1),
        ))
        .expect("parses");
        assert_eq!(vix_target_from_watch_doc(&doc, "2026-07-13"), None);

        // Wrong kind: an ltp (stock-class) entry never matches.
        let ltp_kind = format!(
            r#"{{"exchange":"NSE","segment":"CASH","exchange_token":"INDIAVIX",
                 "kind":"ltp","security_id":{sid},
                 "index_name":"NSE-INDIAVIX","symbol_name":"India Vix"}}"#
        );
        let doc = parse_watch_file(&watch_json("2026-07-13", &ltp_kind)).expect("parses");
        assert_eq!(vix_target_from_watch_doc(&doc, "2026-07-13"), None);
    }

    /// Per-SID independence (operator scope 2026-07-13): the escalation
    /// edge feeds ONLY from the 3 core indices. A VIX-only failure —
    /// fetch, empty, or VIX row append — never yields a fully-failed
    /// minute; all-core-failed does, even when VIX alone succeeded; a
    /// shared flush failure does (core rows are lost too).
    #[test]
    fn test_minute_edge_tally_vix_failure_never_pages_core_failure_does() {
        // All 3 core ok, VIX failed (no note_ok for VIX) → not fully failed.
        let mut tally = MinuteEdgeTally::default();
        for _ in 0..3 {
            tally.note_ok(true);
        }
        assert!(!tally.fully_failed(), "VIX-only failure must not page");

        // VIX append failure alone → still not fully failed.
        tally.note_append_failure(false);
        assert!(
            !tally.fully_failed(),
            "a VIX-only persist failure must not page"
        );

        // A CORE append failure flips it (M1: persist-gated).
        tally.note_append_failure(true);
        assert!(tally.fully_failed(), "a core persist failure must page");

        // All core failed while VIX alone succeeded → fully failed.
        let mut vix_only_ok = MinuteEdgeTally::default();
        vix_only_ok.note_ok(false);
        assert!(
            vix_only_ok.fully_failed(),
            "core-all-failed must page even when VIX succeeded"
        );

        // Shared flush failure → fully failed.
        let mut flush_fail = MinuteEdgeTally::default();
        for _ in 0..3 {
            flush_fail.note_ok(true);
        }
        flush_fail.note_flush_failure();
        assert!(flush_fail.fully_failed(), "a shared flush failure pages");
    }

    /// The daily "VIX not served" latch verdict: fires ONLY when a VIX
    /// target exists, zero VIX minutes persisted, and at least one core
    /// index persisted (VIX-specific emptiness — never on a leg-wide
    /// outage, never without a resolved VIX, never once VIX persisted).
    #[test]
    fn test_vix_not_served_verdict_arms() {
        let vix_sid = stable_index_security_id("NSE-INDIAVIX");
        let mut targets = core_spot_targets();
        let core_sid = targets[0].security_id;

        // No VIX target resolved → never fires.
        let mut tracker = PersistTracker::default();
        tracker.commit(core_sid, 1);
        assert!(!vix_not_served_verdict(&targets, &tracker));

        targets.push(GrowwSpotTarget {
            groww_symbol: "NSE-INDIAVIX".to_string(),
            symbol: GROWW_SPOT_1M_VIX_SYMBOL,
            exchange: "NSE".to_string(),
            segment: "CASH".to_string(),
            security_id: vix_sid,
            core: false,
        });

        // Core persisted + zero VIX minutes → fires.
        assert!(vix_not_served_verdict(&targets, &tracker));

        // Leg-wide outage (nothing persisted anywhere) → does NOT fire
        // (the escalation edge already paged that).
        let empty = PersistTracker::default();
        assert!(!vix_not_served_verdict(&targets, &empty));

        // VIX persisted at least one minute → does NOT fire.
        tracker.commit(vix_sid, 1);
        assert!(!vix_not_served_verdict(&targets, &tracker));
    }

    // ---- timestamp dual parse (UNVERIFIED-LIVE wire format) ------------------

    /// Epoch-seconds form (UTC Assumed): the 2026-07-13 09:15:00 IST
    /// candle is UTC epoch 1783914300 — must land on the SAME IST-minute
    /// bucket the matcher targets.
    #[test]
    fn test_parse_groww_candle_ts_epoch_secs_fixture() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let target = minute_open_ist_nanos(date, 9 * 3600 + 15 * 60);
        let utc_ts: i64 = 1_783_914_300;
        assert_eq!(target, (utc_ts + 19_800) * 1_000_000_000);
        let (nanos, form) =
            parse_groww_candle_ts(&serde_json::json!(utc_ts)).expect("epoch parses");
        assert_eq!(nanos, target);
        assert_eq!(form, GrowwTsForm::EpochSecs);
        // Epoch MILLIS normalize to the same bucket.
        let (nanos_ms, form_ms) =
            parse_groww_candle_ts(&serde_json::json!(utc_ts * 1_000)).expect("millis parse");
        assert_eq!(nanos_ms, target);
        assert_eq!(form_ms, GrowwTsForm::EpochSecs);
        // A float that is integral parses too (JSON number leniency).
        let (nanos_f, _) =
            parse_groww_candle_ts(&serde_json::json!(1_783_914_300.0)).expect("float parse");
        assert_eq!(nanos_f, target);
    }

    /// IST-string form (Assumed IST wall-clock): "2026-07-13 09:15:00"
    /// must land on the SAME IST-minute bucket; the ISO `T` separator is
    /// tolerated; sub-minute seconds floor to the minute.
    #[test]
    fn test_parse_groww_candle_ts_ist_string_fixture() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let target = minute_open_ist_nanos(date, 9 * 3600 + 15 * 60);
        let (nanos, form) = parse_groww_candle_ts(&serde_json::json!("2026-07-13 09:15:00"))
            .expect("string parses");
        assert_eq!(nanos, target);
        assert_eq!(form, GrowwTsForm::IstString);
        let (nanos_t, _) = parse_groww_candle_ts(&serde_json::json!("2026-07-13T09:15:00"))
            .expect("T-separated parses");
        assert_eq!(nanos_t, target);
        // Sub-minute seconds floor to the minute bucket.
        let (nanos_s, _) = parse_groww_candle_ts(&serde_json::json!("2026-07-13 09:15:42"))
            .expect("seconds floor");
        assert_eq!(nanos_s, target);
    }

    /// Boundary/garbage inputs: implausible epochs, negatives, malformed
    /// strings, null/bool → None (typed degrade, never a wrong bucket).
    #[test]
    fn test_parse_groww_candle_ts_rejects_garbage_and_implausible() {
        for v in [
            serde_json::json!(0),                     // 1970 — implausible
            serde_json::json!(-1_783_914_300),        // negative
            serde_json::json!(5_000_000_000_i64),     // year 2128 — implausible
            serde_json::json!(1e30),                  // absurd float
            serde_json::json!(1_783_914_300.5),       // fractional seconds
            serde_json::json!("13-07-2026 09:15:00"), // wrong date order
            serde_json::json!("2026-07-13"),          // date only
            serde_json::json!("not a timestamp"),     // garbage
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!([1_783_914_300]),
        ] {
            assert!(
                parse_groww_candle_ts(&v).is_none(),
                "must reject {v:?} rather than mis-bucket it"
            );
        }
    }

    // ---- body parse (row tuples — production-grounded shape) -----------------

    fn body_with_rows(rows: &str) -> String {
        format!(r#"{{"status":"SUCCESS","payload":{{"candles":[{rows}]}}}}"#)
    }

    #[test]
    fn test_parse_groww_1m_candles_happy_envelope_and_null_volume() {
        let utc_ts: i64 = 1_783_914_300; // 09:15 IST 2026-07-13
        let body = body_with_rows(&format!(
            r#"[{utc_ts},25461.3,25470.85,25455.0,25468.2,null,null],
               [{next},25468.2,25472.0,25460.1,25465.5,120,null]"#,
            next = utc_ts + 60
        ));
        let (candles, stats) = parse_groww_1m_candles(&body);
        assert_eq!(candles.len(), 2);
        assert_eq!(stats.epoch_ts_rows, 2);
        assert_eq!(stats.string_ts_rows, 0);
        assert_eq!(stats.malformed_rows, 0);
        // Null index volume stores 0 verbatim — never a malformed marker.
        assert_eq!(candles[0].volume, 0);
        assert_eq!(candles[1].volume, 120);
        assert_eq!(candles[0].open, 25_461.3);
        assert_eq!(candles[0].close, 25_468.2);
    }

    #[test]
    fn test_parse_groww_1m_candles_string_ts_form_counted() {
        let body = body_with_rows(r#"["2026-07-13 09:15:00",100.0,101.0,99.0,100.5,0,null]"#);
        let (candles, stats) = parse_groww_1m_candles(&body);
        assert_eq!(candles.len(), 1);
        assert_eq!(stats.string_ts_rows, 1);
        assert_eq!(stats.epoch_ts_rows, 0);
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        assert_eq!(
            candles[0].minute_ts_ist_nanos,
            minute_open_ist_nanos(date, 9 * 3600 + 15 * 60)
        );
    }

    /// STALE-body detection (a real code path, not prose): a 2xx body
    /// carrying ONLY earlier minutes yields target-absent from the
    /// client-side minute filter — the previous minute is still minable
    /// as backfill from the SAME body.
    #[test]
    fn test_stale_body_serving_previous_minute_is_target_absent_but_backfills() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let target = minute_open_ist_nanos(date, 9 * 3600 + 16 * 60); // 09:16
        let prev = target - NANOS_PER_MINUTE; // 09:15
        let utc_prev = prev / 1_000_000_000 - 19_800;
        let body = body_with_rows(&format!(r#"[{utc_prev},100.0,101.0,99.0,100.5,0,null]"#));
        let (candles, _) = parse_groww_1m_candles(&body);
        // The exact-minute filter DETECTS the stale body: target absent.
        assert!(select_minute_candle(&candles, target).is_none());
        // The vendor-lateness recovery path: prev is minable as backfill.
        assert_eq!(
            select_minute_candle(&candles, prev)
                .expect("prev minute present")
                .minute_ts_ist_nanos,
            prev
        );
    }

    #[test]
    fn test_parse_groww_1m_candles_failure_status_and_malformed_bodies() {
        // Explicit FAILURE envelope → empty, zero malformed (typed reject).
        let (candles, stats) = parse_groww_1m_candles(
            r#"{"status":"FAILURE","error":{"code":"GA000","message":"denied"}}"#,
        );
        assert!(candles.is_empty());
        assert_eq!(stats.malformed_rows, 0);
        // Malformed JSON / missing candles → empty + counted.
        for body in ["not json", "{}", r#"{"payload":{}}"#] {
            let (candles, stats) = parse_groww_1m_candles(body);
            assert!(candles.is_empty(), "body {body:?} must parse to empty");
            assert!(stats.malformed_rows > 0, "body {body:?} must count");
        }
        // Top-level candles array is tolerated.
        let (candles, _) =
            parse_groww_1m_candles(r#"{"candles":[[1783914300,1.0,2.0,0.5,1.5,0,null]]}"#);
        assert_eq!(candles.len(), 1);
    }

    #[test]
    fn test_parse_groww_1m_candles_malformed_rows_skipped_and_counted() {
        let good_ts: i64 = 1_783_914_300;
        let body = body_with_rows(&format!(
            r#"[{good_ts},100.0,101.0,99.0,100.5,0,null],
               ["garbage-ts",100.0,101.0,99.0,100.5,0,null],
               [{short}],
               42,
               [{good_ts},"NaN",101.0,99.0,100.5,0,null]"#,
            short = good_ts + 60
        ));
        let (candles, stats) = parse_groww_1m_candles(&body);
        assert_eq!(candles.len(), 1, "only the fully-valid row survives");
        assert_eq!(stats.malformed_rows, 4);
    }

    // ---- backfill / sweep / watermark (the #1499 patterns) --------------------

    #[test]
    fn test_backfill_minute_nanos_hit_and_not_needed() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let session_first = minute_open_ist_nanos(date, 9 * 3600 + 15 * 60);
        let target = minute_open_ist_nanos(date, 9 * 3600 + 17 * 60); // 09:17
        let prev = target - NANOS_PER_MINUTE; // 09:16
        // Nothing persisted yet → the previous minute is due.
        assert_eq!(
            backfill_minute_nanos(None, target, session_first),
            Some(prev)
        );
        // Watermark below prev → due.
        assert_eq!(
            backfill_minute_nanos(Some(prev - NANOS_PER_MINUTE), target, session_first),
            Some(prev)
        );
        // Watermark AT prev (already persisted) → not due.
        assert_eq!(
            backfill_minute_nanos(Some(prev), target, session_first),
            None
        );
        // Watermark ahead (target already persisted) → not due.
        assert_eq!(
            backfill_minute_nanos(Some(target), target, session_first),
            None
        );
    }

    #[test]
    fn test_backfill_minute_nanos_first_session_minute_has_no_backfill() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let session_first = minute_open_ist_nanos(date, 9 * 3600 + 15 * 60);
        // The 09:16:00 fire targets 09:15 — its previous minute is
        // pre-open → no backfill (session-first gate).
        assert_eq!(
            backfill_minute_nanos(None, session_first, session_first),
            None
        );
    }

    #[test]
    fn test_sweep_missing_minutes_noop_full_and_tail() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let first = minute_open_ist_nanos(date, 9 * 3600 + 15 * 60);
        let last = minute_open_ist_nanos(date, 15 * 3600 + 29 * 60);
        // Complete session → nothing to sweep.
        assert!(sweep_missing_minutes(Some(last), first, last).is_empty());
        // Nothing persisted → the whole 375-minute session.
        assert_eq!(sweep_missing_minutes(None, first, last).len(), 375);
        // Tail gap: watermark at 15:27 → 15:28 + 15:29 missing.
        let w = minute_open_ist_nanos(date, 15 * 3600 + 27 * 60);
        let tail = sweep_missing_minutes(Some(w), first, last);
        assert_eq!(tail, vec![w + NANOS_PER_MINUTE, w + 2 * NANOS_PER_MINUTE]);
        // A pre-session watermark clamps to session first.
        let early = sweep_missing_minutes(Some(first - 10 * NANOS_PER_MINUTE), first, last);
        assert_eq!(early.first().copied(), Some(first));
        assert_eq!(early.len(), 375);
    }

    #[test]
    fn test_persist_tracker_commit_max_merge_and_double_persist_idempotent() {
        let mut t = PersistTracker::default();
        assert_eq!(t.last_persisted(1), None);
        t.commit(1, 100);
        assert_eq!(t.last_persisted(1), Some(100));
        // Max-merge: an older commit never regresses the watermark.
        t.commit(1, 40);
        assert_eq!(t.last_persisted(1), Some(100));
        // Double persist of the same minute is idempotent.
        t.commit(1, 100);
        assert_eq!(t.last_persisted(1), Some(100));
        t.commit(1, 160);
        assert_eq!(t.last_persisted(1), Some(160));
        // Per-symbol isolation.
        assert_eq!(t.last_persisted(2), None);
    }

    // ---- ladder deltas / token pacing / classification -----------------------

    #[test]
    fn test_groww_retry_sleep_deltas_reconstruct_the_offset_schedule() {
        let deltas = groww_retry_sleep_deltas_ms();
        assert_eq!(deltas, [700, 800, 1_500, 3_000]);
        let mut cumulative = 0u64;
        for (delta, offset) in deltas.iter().zip(GROWW_SPOT_1M_RETRY_OFFSETS_MS.iter()) {
            cumulative += delta;
            assert_eq!(cumulative, *offset);
        }
    }

    #[test]
    fn test_should_reread_token_respects_60s_floor() {
        // Never read → always allowed.
        assert!(should_reread_token(None, 0));
        // Inside the floor → refused (never hammer SSM on a 401 storm).
        assert!(!should_reread_token(Some(1_000_000), 1_000_000 + 59_999));
        // At/after the floor → allowed.
        assert!(should_reread_token(Some(1_000_000), 1_000_000 + 60_000));
        assert!(should_reread_token(Some(1_000_000), 1_000_000 + 3_600_000));
        // Clock step backwards → refused (saturating diff is 0).
        assert!(!should_reread_token(Some(1_000_000), 500_000));
    }

    #[test]
    fn test_error_class_for_status_bounded_slugs() {
        assert_eq!(error_class_for_status(0), "transport");
        assert_eq!(error_class_for_status(401), "auth");
        assert_eq!(error_class_for_status(403), "auth");
        assert_eq!(error_class_for_status(429), "rate_limited");
        assert_eq!(error_class_for_status(404), "http_4xx");
        assert_eq!(error_class_for_status(500), "http_5xx");
        assert_eq!(error_class_for_status(599), "http_5xx");
        assert_eq!(error_class_for_status(302), "http_other");
    }

    // ---- forensics outcome mapping + row builder ------------------------------

    #[test]
    fn test_audit_outcome_for_maps_every_arm() {
        let mk_forensics = |class: &'static str| LadderForensics {
            attempts: 3,
            rate_limited_count: u32::from(class == "rate_limited"),
            final_http_status: 200,
            final_latency_ms: 10,
            error_class: class,
            auth_rejected: false,
        };
        let candle = MinuteCandle {
            minute_ts_ist_nanos: 0,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 0,
        };
        assert_eq!(
            audit_outcome_for(
                &SymbolFetchOutcome::Found {
                    candle,
                    close_to_data_ms: 5,
                    backfill_candle: None
                },
                &mk_forensics("none")
            ),
            RestFetchOutcome::Ok
        );
        assert_eq!(
            audit_outcome_for(
                &SymbolFetchOutcome::Empty {
                    backfill_candle: None
                },
                &mk_forensics("target_absent")
            ),
            RestFetchOutcome::Empty
        );
        assert_eq!(
            audit_outcome_for(
                &SymbolFetchOutcome::Failed {
                    reason: "x".into(),
                    backfill_candle: None
                },
                &mk_forensics("http_5xx")
            ),
            RestFetchOutcome::Error
        );
        assert_eq!(
            audit_outcome_for(
                &SymbolFetchOutcome::Failed {
                    reason: "429".into(),
                    backfill_candle: None
                },
                &mk_forensics("rate_limited")
            ),
            RestFetchOutcome::RateLimited
        );
    }

    #[test]
    fn test_build_fetch_audit_row_stamps_feed_leg_and_sentinels() {
        let forensics = LadderForensics {
            attempts: 2,
            rate_limited_count: 1,
            final_http_status: 429,
            final_latency_ms: 87,
            error_class: "rate_limited",
            auth_rejected: false,
        };
        let row = build_fetch_audit_row(
            1_770_000_900_000_000_000,
            1_769_990_400_000_000_000,
            stable_index_security_id("NSE-NIFTY"),
            "NIFTY",
            &forensics,
            RestFetchOutcome::RateLimited,
            -1,
            "rate_limited",
        );
        assert_eq!(row.feed, "groww");
        assert_eq!(row.leg, "spot_1m");
        assert_eq!(row.exchange_segment, "IDX_I");
        assert_eq!(row.attempts, 2);
        assert_eq!(row.final_http_status, 429);
        assert_eq!(row.fetch_latency_ms, 87);
        assert_eq!(row.rate_limited_count, 1);
        assert_eq!(row.close_to_data_ms, -1, "-1 sentinel on non-ok");
        assert_eq!(row.outcome, RestFetchOutcome::RateLimited);
        assert_eq!(row.error_class, "rate_limited");
        // The security id is the live lane's canonical Groww index id.
        assert_eq!(row.security_id, stable_index_security_id("NSE-NIFTY"));
    }

    #[test]
    fn test_build_groww_spot_1m_row_carries_candle_and_honest_latency() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let ts = minute_open_ist_nanos(date, 9 * 3600 + 15 * 60);
        let candle = MinuteCandle {
            minute_ts_ist_nanos: ts,
            open: 25_461.3,
            high: 25_470.85,
            low: 25_455.0,
            close: 25_468.2,
            volume: 0,
        };
        let sid = stable_index_security_id("BSE-SENSEX");
        let row = build_groww_spot_1m_row(&candle, sid, "SENSEX", ts - 1, 1_042);
        assert_eq!(row.ts_ist_nanos, ts);
        assert_eq!(row.security_id, sid);
        assert_eq!(row.symbol, "SENSEX");
        assert_eq!(row.open, 25_461.3);
        assert_eq!(row.close, 25_468.2);
        assert_eq!(row.volume, 0);
        assert_eq!(row.close_to_data_ms, 1_042);
    }

    // ---- symbol table sanity ---------------------------------------------------

    /// The 3 CORE symbols resolve to 3 DISTINCT canonical Groww index ids
    /// and their exchanges/segments match the docs-grounded identity
    /// table. (2026-07-13 operator scope: the 4th target — INDIA VIX — is
    /// runtime-resolved, so the CONST set deliberately stays 3 here.)
    #[test]
    fn test_groww_symbols_resolve_to_distinct_live_lane_ids() {
        let ids: Vec<i64> = GROWW_SPOT_1M_SYMBOLS
            .iter()
            .map(|(gs, _, _, _)| stable_index_security_id(gs))
            .collect();
        let mut deduped = ids.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), 3, "core ids must be distinct: {ids:?}");
        for (gs, _, exchange, segment) in GROWW_SPOT_1M_SYMBOLS {
            assert_eq!(segment, "CASH", "{gs}: indices are CASH segment");
            assert!(
                gs.starts_with(&format!("{exchange}-")),
                "{gs}: groww_symbol carries its exchange prefix"
            );
        }
    }

    // ---- hostile round 1: pre-boot gaps / plausibility / duplicates / 401 ----

    /// Item 2 (the coordinator's canonical case): a mid-session boot at
    /// 11:00 IST fires first at the 11:00 boundary (target = the 10:59
    /// open; its backfill covers the 10:58 open), so the run's pre-boot
    /// blind window is the opens [09:15, 10:57] — 103 minutes, named per
    /// symbol, NO fetch.
    #[test]
    fn test_pre_boot_gap_minutes_boot_at_11_names_0915_to_1057_opens() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let session_first = minute_open_ist_nanos(date, 9 * 3600 + 15 * 60);
        let session_last = minute_open_ist_nanos(date, 15 * 3600 + 29 * 60);
        let target_1059 = minute_open_ist_nanos(date, 10 * 3600 + 59 * 60);
        let covered = first_covered_minute(target_1059, session_first);
        assert_eq!(
            covered,
            target_1059 - NANOS_PER_MINUTE,
            "the first fire's backfill covers the 10:58 open"
        );
        let gaps = pre_boot_gap_minutes(Some(covered), session_first, session_last);
        assert_eq!(gaps.first().copied(), Some(session_first), "starts 09:15");
        assert_eq!(
            gaps.last().copied(),
            Some(covered - NANOS_PER_MINUTE),
            "ends at the 10:57 open — everything the run never covered"
        );
        assert_eq!(gaps.len(), 103, "opens 09:15..=10:57 inclusive");
    }

    /// Item 2 edge cases: a run that never fired names the WHOLE session;
    /// a boot before/at the session open has NO pre-boot window; the same
    /// mechanism covers a supervisor respawn (fresh run, later first fire).
    #[test]
    fn test_pre_boot_gap_minutes_never_fired_and_clean_boot() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let session_first = minute_open_ist_nanos(date, 9 * 3600 + 15 * 60);
        let session_last = minute_open_ist_nanos(date, 15 * 3600 + 29 * 60);
        // Never fired → the whole 375-minute session is pre-boot.
        assert_eq!(
            pre_boot_gap_minutes(None, session_first, session_last).len(),
            375
        );
        // Clean pre-open boot: first fire targets the 09:15 open (no
        // in-session backfill) → first covered == session first → no gap.
        let covered = first_covered_minute(session_first, session_first);
        assert_eq!(covered, session_first);
        assert!(pre_boot_gap_minutes(Some(covered), session_first, session_last).is_empty());
    }

    /// Item 9: non-positive O/H/L/C or high<low flags; a normal candle
    /// (incl. zero-volume index) does not.
    #[test]
    fn test_candle_ohlc_implausible_flags_bad_and_passes_good() {
        let good = MinuteCandle {
            minute_ts_ist_nanos: 1,
            open: 25_000.0,
            high: 25_010.0,
            low: 24_990.0,
            close: 25_005.0,
            volume: 0,
        };
        assert!(!candle_ohlc_implausible(&good));
        for bad in [
            MinuteCandle { open: 0.0, ..good },
            MinuteCandle { high: -1.0, ..good },
            MinuteCandle { low: 0.0, ..good },
            MinuteCandle {
                close: -0.5,
                ..good
            },
            MinuteCandle {
                high: 24_000.0,
                low: 24_500.0,
                ..good
            },
        ] {
            assert!(candle_ohlc_implausible(&bad), "must flag {bad:?}");
        }
    }

    /// Item 11: duplicates of the target minute beyond the first are
    /// counted (first-match-wins persists; the count makes it visible).
    #[test]
    fn test_duplicate_minute_candles_counts_extras_only() {
        let c = |ts: i64| MinuteCandle {
            minute_ts_ist_nanos: ts,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 0,
        };
        let candles = vec![c(100), c(200), c(200), c(200), c(300)];
        assert_eq!(duplicate_minute_candles(&candles, 200), 2);
        assert_eq!(duplicate_minute_candles(&candles, 100), 0);
        assert_eq!(duplicate_minute_candles(&candles, 999), 0);
    }

    /// Round-2 LOW + item 6: gap-row forensics semantics — a
    /// `persist_failed` gap (fetched OK, ILP append failed) carries the
    /// ACTUAL 200 + attempts 1; a no-fetch gap (`pre_boot` / `no_token`)
    /// carries the 0 sentinel + attempts 0; a sweep target-absence
    /// (`named_gap`) carries its actual 200 + attempts 1.
    #[test]
    fn test_named_gap_forensics_status_and_attempts_semantics() {
        let persist = named_gap_forensics("persist_failed", 200);
        assert_eq!(persist.final_http_status, 200, "actual status kept");
        assert_eq!(persist.attempts, 1, "a fetch ran for this gap");
        assert_eq!(persist.error_class, "persist_failed");
        assert_eq!(persist.final_latency_ms, -1);

        let absent = named_gap_forensics("named_gap", 200);
        assert_eq!(absent.final_http_status, 200);
        assert_eq!(absent.attempts, 1);

        for no_fetch in [
            named_gap_forensics("pre_boot", 0),
            named_gap_forensics("no_token", 0),
        ] {
            assert_eq!(no_fetch.final_http_status, 0, "0 sentinel — no fetch");
            assert_eq!(no_fetch.attempts, 0, "no fetch ⇒ no attempt");
        }
    }

    /// Item 12 (fire-level): the symbols skipped after an auth reject get
    /// `no_token`/`auth` forensics rows with 0/-1 sentinels — never a
    /// doomed request, never a silent skip.
    #[test]
    fn test_build_auth_short_circuit_rows_names_remaining_symbols() {
        // 2026-07-13: the short-circuit slice is target-typed now, so a
        // resolved INDIA VIX target is skipped + forensics-rowed exactly
        // like the core symbols.
        let targets = core_spot_targets();
        let rows = build_auth_short_circuit_rows(&targets[1..], 900, 0);
        assert_eq!(rows.len(), 2, "two targets remain after the first");
        for (row, target) in rows.iter().zip(&targets[1..]) {
            assert_eq!(row.outcome, RestFetchOutcome::NoToken);
            assert_eq!(row.error_class, "auth");
            assert_eq!(row.final_http_status, 0, "no HTTP happened");
            assert_eq!(row.attempts, 0);
            assert_eq!(row.fetch_latency_ms, -1);
            assert_eq!(row.close_to_data_ms, -1);
            assert_eq!(row.ts_ist_nanos, 900);
            assert_eq!(row.security_id, target.security_id);
            assert_eq!(row.symbol, target.symbol);
        }
        assert!(build_auth_short_circuit_rows(&[], 900, 0).is_empty());
    }

    /// Spawn a one-response-per-connection mock HTTP server; returns the
    /// candles URL pointing at it.
    async fn spawn_mock_candles_server(response: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buf = [0u8; 8192];
                        let _ = stream.read(&mut buf).await;
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            }
        });
        format!("http://127.0.0.1:{port}/v1/historical/candles")
    }

    fn mock_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("client")
    }

    /// Item 12 (ladder-level): an HTTP 401 SHORT-CIRCUITS the ladder on
    /// the FIRST attempt — no further doomed rungs (attempts == 1, not 5)
    /// — and the forensics carry the auth classification.
    #[tokio::test]
    async fn test_ladder_short_circuits_on_auth_reject() {
        const MOCK_401: &str = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\n\r\n{}";
        let url = spawn_mock_candles_server(MOCK_401).await;
        let client = mock_client();
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let query = groww_candles_query("NSE-NIFTY", "NSE", "CASH", date);
        let token = SecretString::from("test-token");
        let (outcome, forensics) =
            fetch_minute_with_ladder(&client, &url, &query, &token, 900, None, 0).await;
        assert_eq!(
            forensics.attempts, 1,
            "auth reject must abort the ladder on the first rung"
        );
        assert!(forensics.auth_rejected);
        assert_eq!(forensics.final_http_status, 401);
        assert_eq!(forensics.error_class, "auth");
        assert!(
            matches!(outcome, SymbolFetchOutcome::Failed { .. }),
            "got {outcome:?}"
        );
    }

    // ---- CI coverage hardening (PR #1507 Coverage & Perf: app 63.07% <
    // 63.3% floor — REAL tests over the runner arms; the floor is never
    // touched) --------------------------------------------------------------

    /// Deterministic params: TODAY is stamped an NSE holiday, so
    /// `is_trading_day_today()` is `false` regardless of the real weekday;
    /// QuestDB points at reserved port 1 (fast refused connections — the
    /// ensure-DDL degrade arms run for real, nothing live is touched).
    fn test_params() -> GrowwSpot1mTaskParams {
        use tickvault_common::config::{NseHolidayEntry, TradingConfig};
        let config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: today_ist().format("%Y-%m-%d").to_string(),
                name: "coverage-test holiday".to_string(),
            }],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        GrowwSpot1mTaskParams {
            notifier: NotificationService::disabled(),
            calendar: Arc::new(
                tickvault_common::trading_calendar::TradingCalendar::from_config(&config)
                    .expect("calendar"),
            ),
            questdb: QuestDbConfig {
                host: "127.0.0.1".to_string(),
                http_port: 1,
                pg_port: 1,
                ilp_port: 1,
            },
            // PR-3 rebase reconciliation: the chain leg's minute-done
            // sequencing sender — None in these fixtures (chain off).
            minute_done_tx: None,
        }
    }

    /// `record_skipped_boundaries` arms: zero → early return; a wake that
    /// crossed IST midnight → counter + log only (no mis-dated rows, item
    /// 7); same-day → per-(minute, symbol) rows + edge accounting (3
    /// skipped minutes trip the 3-consecutive page through the disabled
    /// notifier) + the best-effort flush error arm (disconnected writer).
    #[test]
    fn test_record_skipped_boundaries_zero_midnight_and_sameday_arms() {
        let params = test_params();
        let targets = core_spot_targets();
        let mut edge = FailureEdge::default();
        let mut aw = RestFetchAuditWriter::for_test();
        let boundary = 9 * 3600 + 17 * 60;

        record_skipped_boundaries(
            &params,
            &targets,
            &mut edge,
            &mut aw,
            0,
            boundary,
            today_ist(),
        );
        assert_eq!(aw.pending(), 0, "zero skipped is a no-op");

        let yesterday = today_ist().pred_opt().expect("valid date");
        record_skipped_boundaries(
            &params, &targets, &mut edge, &mut aw, 2, boundary, yesterday,
        );
        assert_eq!(aw.pending(), 0, "midnight-crossed wake writes no rows");

        record_skipped_boundaries(
            &params,
            &targets,
            &mut edge,
            &mut aw,
            3,
            boundary,
            today_ist(),
        );
        assert_eq!(
            aw.pending(),
            0,
            "same-day rows are appended then best-effort flushed (the \
             disconnected test writer's flush error arm discards them)"
        );
    }

    /// `fire_one_minute` no-token arm end-to-end: the pacing-gated token
    /// cache returns `None` WITHOUT any SSM read (last_read stamped now),
    /// every symbol counts as a full miss with a `no_token` forensics row,
    /// and 3 consecutive fully-failed minutes trip the edge page (via the
    /// disabled notifier). Nothing persists; no request leaves the box.
    #[tokio::test]
    async fn test_fire_one_minute_no_token_full_miss_and_edge() {
        let params = test_params();
        let client = mock_client();
        let mut writer = Spot1mRestWriter::for_test_with_feed(
            SPOT_1M_REST_FEED_GROWW,
            SPOT_1M_REST_SOURCE_GROWW_CANDLES,
        );
        let mut aw = RestFetchAuditWriter::for_test();
        let mut edge = FailureEdge::default();
        let mut tracker = PersistTracker::default();
        // Constructed via new() + field mutation (never a struct literal)
        // so concurrently-added fields keep this fixture compiling.
        let mut cache = GrowwTokenCache::new();
        cache.last_read_ms = Some(epoch_millis_now());
        let targets = core_spot_targets();
        for _ in 0..3 {
            fire_one_minute(
                &params,
                &client,
                &targets,
                &mut writer,
                &mut aw,
                &mut edge,
                &mut tracker,
                &mut cache,
                9 * 3600 + 16 * 60,
            )
            .await;
        }
        assert_eq!(writer.pending(), 0, "no candle rows without a token");
        assert!(cache.token.is_none(), "pacing gate blocks any SSM re-read");
        assert_eq!(tracker.last_persisted(1), None);
    }

    /// A core-only tally with `n` successful core fetches (test shorthand).
    fn tally_core_ok(n: usize) -> MinuteEdgeTally {
        let mut tally = MinuteEdgeTally::default();
        for _ in 0..n {
            tally.note_ok(true);
        }
        tally
    }

    /// `record_minute_verdict` arms: clean minute; sub-edge degraded log;
    /// 3 consecutive fully-failed → Page; then a good minute → Recover.
    /// 2026-07-13: the edge verdict comes from the CORE tally.
    #[test]
    fn test_record_minute_verdict_page_and_recover_arms() {
        let params = test_params();
        let mut edge = FailureEdge::default();
        record_minute_verdict(
            &params,
            &mut edge,
            "9:15 AM",
            3,
            0,
            0,
            false,
            tally_core_ok(3),
            None,
        );
        record_minute_verdict(
            &params,
            &mut edge,
            "9:16 AM",
            2,
            1,
            0,
            false,
            tally_core_ok(2),
            Some("one miss"),
        );
        for _ in 0..3 {
            record_minute_verdict(
                &params,
                &mut edge,
                "9:17 AM",
                0,
                3,
                0,
                false,
                tally_core_ok(0),
                Some("down"),
            );
        }
        record_minute_verdict(
            &params,
            &mut edge,
            "9:20 AM",
            3,
            0,
            0,
            false,
            tally_core_ok(3),
            None,
        );
    }

    /// Non-trading-day run: `run_groww_spot_1m` degrades its ensure-DDL
    /// legs against the refused-connection QuestDB config, then returns at
    /// the trading-day gate; the supervisor observes day-over and exits
    /// cleanly (never respawns into a holiday).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_supervisor_exits_cleanly_on_non_trading_day() {
        let handle = spawn_supervised_groww_spot_1m(test_params());
        tokio::time::timeout(Duration::from_secs(60), handle)
            .await
            .expect("supervisor must exit on a non-trading day")
            .expect("join");
    }

    /// Happy fetch path through a mock server: 200 body carrying the
    /// target minute AND its backfill minute → `Found` on the FIRST rung
    /// with the sticky backfill candle; the bounded wrapper passes it
    /// through untouched.
    #[tokio::test]
    async fn test_ladder_found_first_attempt_with_backfill_and_bounded_passthrough() {
        // IST minute 1_770_000_900 (multiple of 60, year 2026): epoch UTC
        // = IST − 19_800; the backfill row is one minute earlier.
        const TARGET_IST_SECS: i64 = 1_770_000_900;
        const BODY: &str = r#"{"status":"SUCCESS","payload":{"candles":[[1769981040,99.0,101.0,98.0,100.0,0],[1769981100,100.0,102.0,99.0,101.0,0]]}}"#;
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                BODY.len(),
                BODY
            )
            .into_boxed_str(),
        );
        let url = spawn_mock_candles_server(response).await;
        let client = mock_client();
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let query = groww_candles_query("NSE-NIFTY", "NSE", "CASH", date);
        let token = SecretString::from("test-token");
        let target_nanos = TARGET_IST_SECS * NANOS_PER_SEC;
        let backfill_nanos = target_nanos - NANOS_PER_MINUTE;
        let (outcome, forensics) = fetch_minute_with_ladder(
            &client,
            &url,
            &query,
            &token,
            target_nanos,
            Some(backfill_nanos),
            0,
        )
        .await;
        assert_eq!(forensics.attempts, 1, "found on the first rung");
        assert_eq!(forensics.final_http_status, 200);
        match outcome {
            SymbolFetchOutcome::Found {
                candle,
                backfill_candle,
                ..
            } => {
                assert_eq!(candle.minute_ts_ist_nanos, target_nanos);
                assert_eq!(candle.close, 101.0);
                let backfill = backfill_candle.expect("backfill candle mined from same body");
                assert_eq!(backfill.minute_ts_ist_nanos, backfill_nanos);
            }
            other => panic!("expected Found, got {other:?}"),
        }
        // The budget wrapper passes the same verdict through.
        let (bounded, bf) = fetch_minute_bounded(
            &client,
            &url,
            &query,
            &token,
            target_nanos,
            Some(backfill_nanos),
            0,
        )
        .await;
        assert!(matches!(bounded, SymbolFetchOutcome::Found { .. }));
        assert_eq!(bf.final_http_status, 200);
    }

    /// 429 classification through a mock server: rate_limited flagged +
    /// counted, Retry-After presence captured (the live-probe (e) shape),
    /// bounded redacted body — never a panic.
    #[tokio::test]
    async fn test_fetch_once_429_rate_limited_classification() {
        const MOCK_429: &str =
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: 2\r\n\r\n{}";
        let url = spawn_mock_candles_server(MOCK_429).await;
        let client = mock_client();
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let query = groww_candles_query("NSE-NIFTY", "NSE", "CASH", date);
        let token = SecretString::from("test-token");
        let failure = groww_fetch_once(&client, &url, &query, &token)
            .await
            .expect_err("429 must be a typed failure");
        assert!(failure.rate_limited);
        assert!(!failure.auth_rejected);
        assert_eq!(failure.status, 429);
        assert!(failure.msg.contains("retry_after_present=true"));
    }

    /// The declared-Content-Length body cap (§18 pattern): a 200 whose
    /// declared length exceeds the 2 MiB cap is refused BEFORE streaming.
    #[tokio::test]
    async fn test_fetch_once_declared_body_over_cap_refused() {
        const MOCK_HUGE: &str = "HTTP/1.1 200 OK\r\nContent-Length: 3000000\r\n\r\n{}";
        let url = spawn_mock_candles_server(MOCK_HUGE).await;
        let client = mock_client();
        let date = NaiveDate::from_ymd_opt(2026, 7, 13).expect("valid date");
        let query = groww_candles_query("NSE-NIFTY", "NSE", "CASH", date);
        let token = SecretString::from("test-token");
        let failure = groww_fetch_once(&client, &url, &query, &token)
            .await
            .expect_err("over-cap declared length must fail");
        assert_eq!(failure.status, 200, "the HTTP status itself was 200");
        assert!(
            failure.msg.contains("body too large"),
            "got: {}",
            failure.msg
        );
    }

    /// Named-gap + best-effort audit arms with the disconnected writer:
    /// rows append (pending rises), the best-effort flush error arm
    /// discards them loudly — the caller is never affected.
    #[test]
    fn test_record_named_gaps_appends_and_best_effort_flush_discards() {
        let mut aw = RestFetchAuditWriter::for_test();
        record_named_gaps(
            &mut aw,
            &[100, 100 + NANOS_PER_MINUTE],
            0,
            7,
            "NIFTY",
            RestFetchOutcome::NamedGap,
            "persist_failed",
            200,
        );
        assert_eq!(aw.pending(), 2, "one row per gap minute");
        audit_flush_best_effort(&mut aw);
        assert_eq!(aw.pending(), 0, "flush error arm discards (best-effort)");
        // Empty gaps are a no-op.
        record_named_gaps(
            &mut aw,
            &[],
            0,
            7,
            "NIFTY",
            RestFetchOutcome::NamedGap,
            "named_gap",
            200,
        );
        assert_eq!(aw.pending(), 0);
    }

    /// Wall-clock helper smokes (range assertions — the fns are thin IST
    /// conversions) + the parse-stats counter emitter + BOTH
    /// `note_auth_rejected` arms of the token cache.
    #[test]
    fn test_wall_clock_helpers_parse_stats_and_token_cache_arms() {
        assert!(ist_secs_of_day_now() < SECONDS_PER_DAY);
        assert!((0..MILLIS_PER_DAY).contains(&ist_millis_of_day_now()));
        assert!(epoch_millis_now() > 0);
        assert!(fetched_at_ist_nanos_now() > 0);
        let _today = today_ist();
        record_parse_stats(GrowwParseStats {
            epoch_ts_rows: 1,
            string_ts_rows: 1,
            malformed_rows: 1,
        });
        let mut cache = GrowwTokenCache::new();
        cache.note_auth_rejected(); // no cached token — silent arm
        assert!(cache.token.is_none());
        cache.token = Some(SecretString::from("t"));
        cache.note_auth_rejected(); // drop arm
        assert!(cache.token.is_none(), "auth reject drops the cached token");
    }
}
