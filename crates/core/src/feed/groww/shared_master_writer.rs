//! Groww shared-master writer (PR-A, operator lock `groww-second-feed-scope-2026-06-19.md`).
//!
//! Persists the daily Groww instrument set ([`GrowwWatchSet`]) into the SAME shared
//! QuestDB master tables Dhan uses — `instrument_lifecycle` and `index_constituency`
//! — every row tagged `feed='groww'` (operator decision 2026-06-19: "same tables +
//! feed column", NO `groww_*` master table). PR #1229 already put `feed` in the DEDUP
//! key + DDL + row struct of both tables and stamps `feed='dhan'` on the Dhan side; this
//! module fills the `feed='groww'` rows.
//!
//! ## What this is (and is NOT)
//! This is COLD-PATH master/metadata persistence, fire-and-forget + degrade-safe. It is a
//! forensic/queryable record of "which Groww instruments did we track today", NOT the
//! `ticks` path (live-feed-purity.md is untouched — no synthesized ticks anywhere) and NOT
//! on the hot/order/recovery path. A QuestDB outage only loses the best-effort `feed='groww'`
//! rows; the next idempotent boot re-runs (DEDUP UPSERT).
//!
//! ## Uniqueness (I-P1-11 + feed-in-key)
//! The DEDUP key is the composite `(security_id, exchange_segment, feed)` (lifecycle) /
//! `(ts, index_name, security_id, exchange_segment, feed)` (constituency). A Dhan row and a
//! Groww row for the same instrument id are DISTINCT rows because `feed` differs — neither
//! overwrites the other.
//!
//! ## O(1) / allocation
//! [`groww_segment_label`] returns a zero-alloc `&'static str`. The row builders run once
//! per daily watch entry (cold path, O(n) over the ~779-instrument daily set — flagged O(n)
//! honestly; this is NOT a per-tick path) and allocate owned `String`s only for the borrowed
//! row structs, exactly as the Dhan reconciler does.

use tracing::warn;

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

use super::instruments::{GrowwWatchSet, WatchEntry, WatchKind};

/// §36 (2026-07-08): `YYYY-MM-DD` → IST-midnight nanos for the FUTIDX master
/// rows (0 sentinel on empty/unparsable — same semantics as the Dhan-side
/// `expiry_date_to_ist_nanos`). Cold path, once per daily master build.
fn expiry_str_to_ist_midnight_nanos(expiry: &str) -> i64 {
    let trimmed = expiry.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") else {
        return 0;
    };
    let Some(midnight) = date.and_hms_opt(0, 0, 0) else {
        return 0;
    };
    midnight.and_utc().timestamp_nanos_opt().unwrap_or(0)
}

// `Feed` / `ErrorCode` / the `error!`+`info!` macros are only used by the feature-gated
// builders + persist impl (the shared master tables exist only under `daily_universe_fetcher`).
#[cfg(feature = "daily_universe_fetcher")]
use tickvault_common::error_code::ErrorCode;
#[cfg(feature = "daily_universe_fetcher")]
use tickvault_common::feed::Feed;
#[cfg(feature = "daily_universe_fetcher")]
use tracing::{error, info};

// Audit-chain (feed='groww') support — pure diff + prior-snapshot read-back.
// Mirrors the Dhan reconciler (`crates/app/src/apply_reconcile_plan.rs`), but scoped
// to `crates/core` to avoid a core→app dependency, and tagged `feed='groww'`.
#[cfg(feature = "daily_universe_fetcher")]
use serde_json::Value;
#[cfg(feature = "daily_universe_fetcher")]
use std::collections::HashMap;
#[cfg(feature = "daily_universe_fetcher")]
use std::time::Duration;
#[cfg(feature = "daily_universe_fetcher")]
use tickvault_storage::instrument_lifecycle_persistence::{
    InstrumentLifecycleAuditRow, LifecycleState, QUESTDB_TABLE_INSTRUMENT_LIFECYCLE, TransitionKind,
};
#[cfg(feature = "daily_universe_fetcher")]
use tickvault_storage::lifecycle_reconciler::{
    ReconcileInput, classify_transition, is_stock_split,
};

/// Returns the canonical shared-table `exchange_segment` label for a Groww watch entry as a
/// ZERO-ALLOC `&'static str`.
///
/// Mapping (Groww `exchange` ∈ {`NSE`,`BSE`}, `segment` ∈ {`CASH`,`FNO`}):
/// - [`WatchKind::IndexValue`] → `"IDX_I"` (all index values live in the index segment,
///   matching the Dhan convention — a BSE SENSEX index value is still `IDX_I`).
/// - [`WatchKind::Ltp`] CASH → `"NSE_EQ"` (NSE) / `"BSE_EQ"` (BSE).
/// - [`WatchKind::Ltp`] FNO  → `"NSE_FNO"` (NSE) / `"BSE_FNO"` (BSE).
///
/// The watch set today is only IndexValue + Ltp-CASH (`instruments.rs`); the FNO arms are
/// forward-cover for a future F&O watch set. An unrecognized `exchange` cannot occur from the
/// resolvers (they emit only `"NSE"`/`"BSE"`); it is mapped to `"NSE_EQ"` with a one-line
/// `warn!` rather than panicking, so a future code change surfaces visibly (audit Rule 11)
/// without taking down the cold-path writer.
#[must_use]
pub fn groww_segment_label(entry: &WatchEntry) -> &'static str {
    match entry.kind {
        WatchKind::IndexValue => "IDX_I",
        WatchKind::Ltp => {
            let is_fno = entry.segment.eq_ignore_ascii_case("FNO");
            match (entry.exchange.as_str(), is_fno) {
                ("NSE", false) => "NSE_EQ",
                ("BSE", false) => "BSE_EQ",
                ("NSE", true) => "NSE_FNO",
                ("BSE", true) => "BSE_FNO",
                (other, _) => {
                    // `other` + `entry.segment` are CSV-derived (attacker-influenceable
                    // via a crafted Groww master row), so sanitize before logging:
                    // `sanitize_audit_string` strips control chars / newlines / BiDi
                    // overrides so a hostile row cannot inject forged lines into the
                    // structured log → CloudWatch/Telegram (rust-code.md "never log
                    // unsanitized untrusted input"). Message stays `&'static str`.
                    warn!(
                        exchange = %sanitize_audit_string(other),
                        segment = %sanitize_audit_string(&entry.segment),
                        "groww shared-master: unexpected exchange for an Ltp entry; \
                         defaulting segment label to NSE_EQ (no panic, surfaced)"
                    );
                    "NSE_EQ"
                }
            }
        }
    }
}

/// Converts a validated `YYYY-MM-DD` IST trading date into IST-midnight epoch NANOSECONDS in
/// the same convention the rest of the system uses for designated timestamps (IST epoch nanos,
/// no UTC offset added — `data-integrity.md`). Returns `0` for an unparseable date (the caller
/// already validates `watch_date`, so this is defense-in-depth, never a panic).
///
/// Feature-gated: only the (gated) persist path computes a designated `ts`.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
fn watch_date_to_ist_midnight_nanos(watch_date: &str) -> i64 {
    let Ok(date) = chrono::NaiveDate::parse_from_str(watch_date, "%Y-%m-%d") else {
        return 0;
    };
    // Days since the Unix epoch → IST-midnight nanos (the value represents IST wall-clock
    // midnight in IST-epoch-nanos space, matching the lifecycle/constituency boot convention).
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or(date);
    let days = (date - epoch).num_days();
    days.saturating_mul(i64::from(tickvault_common::constants::SECONDS_PER_DAY))
        .saturating_mul(1_000_000_000)
}

/// Pure gate: should the master-table APPEND be SKIPPED for this run?
///
/// Returns `true` exactly when `dry_run` is set — the §27 isolation rule: in a
/// dry run the rows are still BUILT (so the would-write counts are observable),
/// but NO ILP append runs, so a Day-1 dry-run never leaks into the live tables.
/// Pure, zero-alloc, no I/O — extracted from [`persist_groww_instruments`] so the
/// dry-run decision is unit-testable without touching QuestDB.
///
/// Module-internal (`fn`, not `pub`): the only caller is [`persist_groww_instruments`]
/// in this module; the in-module `#[cfg(test)]` tests call it directly. Keeping it
/// private (no external surface) is the correct shape for a module-local pure helper.
#[must_use]
fn should_skip_master_append(dry_run: bool) -> bool {
    dry_run
}

/// Pure classifier for a master-table persist FAILURE.
///
/// Maps the failing stage (`"lifecycle"` / `"constituency"`, or any other label
/// defensively) to the `(stage, ErrorCode)` pair the degrade-safe failure arm
/// emits: the stage label echoes back verbatim for the `tv_groww_master_persist_errors_total{stage}`
/// counter, and the code is ALWAYS [`ErrorCode::GrowwMaster01PersistFailed`]
/// (wire `GROWW-MASTER-01`). TOTAL — never panics, has no else-less arm. The
/// caller only logs+counts+returns with this; it NEVER aborts the feed, an
/// order, or a tick. Extracted so the degrade contract is unit-testable.
///
/// Module-internal (`fn`, not `pub`): the only callers are the two failure arms
/// of [`persist_groww_instruments`] in this module; the in-module `#[cfg(test)]`
/// tests call it directly. Keeping it private is the correct shape for a
/// module-local pure helper.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
fn classify_persist_failure(stage: &'static str) -> (&'static str, ErrorCode) {
    (stage, ErrorCode::GrowwMaster01PersistFailed)
}

// ── FIX 13b (2026-07-04): readiness gate + bounded per-stage retry ──────────

/// Max append attempts per master-table stage (1 initial + 2 retries).
#[cfg(feature = "daily_universe_fetcher")]
const MASTER_PERSIST_MAX_ATTEMPTS: u32 = 3;

/// QUIET QuestDB readiness probe attempts before the persist gives up
/// (`stage="readiness"` GROWW-MASTER-01). Deliberately NOT
/// `wait_for_questdb_ready` — that fn emits BOOT-01/BOOT-02 pages reserved
/// for the boot path; a cold-path master persist must never fire boot pages.
#[cfg(feature = "daily_universe_fetcher")]
const MASTER_READINESS_ATTEMPTS: u32 = 3;

/// Backoff between readiness probe attempts.
#[cfg(feature = "daily_universe_fetcher")]
const MASTER_READINESS_BACKOFF_SECS: u64 = 5;

/// Bounded wait for the `index_constituency` ts-pin migration gate (FIX 13a)
/// before the constituency append. Since the F14 hardening (2026-07-05) the
/// migration runs in the PROCESS-GLOBAL boot prefix on every boot mode
/// (spawned before the Groww activation watcher, regardless of
/// `feeds.dhan_enabled`), so the gate is marked within ~75s worst case
/// (60s quiet readiness probe + one bounded TRUNCATE attempt) — 120s leaves
/// ≥ 45s headroom. A timeout means the boot-prefix task is pathologically
/// delayed; the append proceeds best-effort (rows re-persist next boot if
/// the truncate lands after them).
#[cfg(feature = "daily_universe_fetcher")]
const CONSTITUENCY_MIGRATION_GATE_TIMEOUT_SECS: u64 = 120;

/// PURE retry predicate: another attempt allowed after attempt `n`?
/// (`n` is 1-based; retries happen while `n < MASTER_PERSIST_MAX_ATTEMPTS`.)
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
fn master_persist_should_retry(attempt: u32) -> bool {
    attempt < MASTER_PERSIST_MAX_ATTEMPTS
}

/// PURE backoff ladder for stage retries: attempt 1 → 2s, attempt 2 → 4s
/// (exponential, saturating; attempts beyond the ladder cap at 4s — total
/// worst-case sleep per stage is bounded at ~6s).
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
fn master_persist_backoff_secs(attempt: u32) -> u64 {
    2u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(1))
}

/// QUIET QuestDB readiness probe: `SELECT 1` over the shared probe client,
/// up to [`MASTER_READINESS_ATTEMPTS`] attempts with
/// [`MASTER_READINESS_BACKOFF_SECS`] backoff between them. `true` = ready.
/// Emits ONLY `warn!` per failed attempt (never BOOT-01/02 — see the const
/// doc above).
#[cfg(feature = "daily_universe_fetcher")]
// TEST-EXEMPT: network I/O probe (live QuestDB) — the attempt/backoff constants + retry predicate are unit-tested; the probe body is a bounded loop over the storage-crate-tested shared_probe_client.
async fn questdb_master_ready(questdb: &QuestDbConfig) -> bool {
    let url = format!(
        "http://{}:{}/exec?query=SELECT%201",
        questdb.host, questdb.http_port
    );
    for attempt in 1..=MASTER_READINESS_ATTEMPTS {
        match tickvault_storage::http_client::shared_probe_client() {
            Ok(client) => match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => return true,
                Ok(resp) => {
                    warn!(
                        status = %resp.status(),
                        attempt,
                        "[feeds] Groww master persist: QuestDB readiness probe non-2xx"
                    );
                }
                Err(err) => {
                    warn!(
                        ?err,
                        attempt, "[feeds] Groww master persist: QuestDB readiness probe failed"
                    );
                }
            },
            Err(err) => {
                warn!(
                    ?err,
                    attempt, "[feeds] Groww master persist: probe client build failed"
                );
            }
        }
        if attempt < MASTER_READINESS_ATTEMPTS {
            tokio::time::sleep(Duration::from_secs(MASTER_READINESS_BACKOFF_SECS)).await;
        }
    }
    false
}

/// Builds the `instrument_lifecycle` rows for the Groww watch set, all tagged `feed='groww'`.
///
/// Spot/index-only sentinels per the row contract (no derivatives in the watch set):
/// `lot_size=0`, `tick_size=0.0`, `expiry=0`, `strike=0.0`, `option_type=""`,
/// `underlying_security_id=0`. `instrument_type` is `INDEX` for an index value else `EQUITY`.
/// `symbol_name`/`display_name` come from the retained provenance (index → `index_name`;
/// stock → `symbol_name`), with the `exchange_token` as a last-resort non-empty fallback so a
/// row is never wholly anonymous.
///
/// Borrows from `set`; the returned rows borrow `&'static` labels + the entry strings, so the
/// caller keeps `set` alive for the append.
///
/// Feature-gated behind `daily_universe_fetcher` — the storage `InstrumentLifecycleRow` type
/// (and the whole `instrument_lifecycle` table machinery) only exists under that feature.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
pub fn build_groww_lifecycle_rows<'a>(
    set: &'a GrowwWatchSet,
    prior: &HashMap<GrowwLifecycleKey, GrowwPriorAttrs>,
    last_update_ts_nanos: i64,
    trading_date_ist_nanos: i64,
    dry_run: bool,
) -> Vec<tickvault_storage::instrument_lifecycle_persistence::InstrumentLifecycleRow<'a>> {
    use tickvault_storage::instrument_lifecycle_persistence::{
        InstrumentLifecycleRow, LifecycleState,
    };

    // Iterate the FULL pre-cap universe (`master_entries`), NOT the capped live
    // `entries` — the master table records every resolved instrument regardless of
    // the (smaller) live-subscribe cap, exactly as the Dhan side persists its full
    // `DailyUniverse` independent of subscription.
    set.master_entries
        .iter()
        .map(|e| {
            let is_index = e.kind == WatchKind::IndexValue;
            // Prefer the retained provenance; fall back to the subscribe token so the row is
            // never anonymous. `index_name` for indices, `symbol_name` for stocks.
            let symbol_name: &str = if is_index {
                e.index_name.as_deref().unwrap_or(&e.exchange_token)
            } else {
                e.symbol_name.as_deref().unwrap_or(&e.exchange_token)
            };
            let exchange_segment = groww_segment_label(e);
            // §36 (2026-07-08): an Ltp entry on segment FNO is one of the 4
            // index futures — label it FUTIDX (not EQUITY) and stamp its
            // expiry so the feed='groww' master rows are not mislabeled.
            let is_fno_future = !is_index && e.segment.eq_ignore_ascii_case("FNO");
            InstrumentLifecycleRow {
                last_update_ts_nanos,
                security_id: e.security_id,
                exchange_segment,
                // Groww exchange (`NSE`/`BSE`) — exchange_id is an optional SYMBOL.
                exchange_id: e.exchange.as_str(),
                instrument_type: if is_index {
                    "INDEX"
                } else if is_fno_future {
                    "FUTIDX"
                } else {
                    "EQUITY"
                },
                symbol_name,
                display_name: symbol_name,
                underlying_security_id: 0,
                underlying_symbol: "",
                lot_size: 0,
                tick_size: 0.0,
                expiry_date_nanos: if is_fno_future {
                    expiry_str_to_ist_midnight_nanos(e.expiry_date.as_deref().unwrap_or(""))
                } else {
                    0
                },
                strike_price: 0.0,
                option_type: "",
                lifecycle_state: LifecycleState::Active,
                lifecycle_state_locked: false,
                // SEBI first-seen preservation (fix 2026-07-05): the daily
                // UPSERT replaces the whole row, so first_seen MUST carry the
                // prior value for an already-known instrument — never reset
                // to today (mirror of the Dhan resolve_first_seen_nanos).
                first_seen_date_nanos: resolve_groww_first_seen_nanos(
                    e.security_id,
                    exchange_segment,
                    prior,
                    trading_date_ist_nanos,
                ),
                last_seen_date_nanos: trading_date_ist_nanos,
                last_active_date_nanos: trading_date_ist_nanos,
                expired_date_nanos: 0,
                prev_symbol_chain: "",
                source_csv_sha256: "",
                dry_run,
                feed: Feed::Groww.as_str(),
            }
        })
        .collect()
}

/// Builds the `index_constituency` rows for the Groww STOCK entries, all tagged `feed='groww'`.
///
/// Only stock (Ltp) entries with a retained `symbol_name` are constituents; index values are
/// not their own constituents. `index_name` is `"NIFTY Total Market"` (the NTM membership all
/// resolved Groww stocks belong to — the `GrowwWatchSet` stock set IS the NTM-resolved set per
/// `instruments.rs`). `via_isin` is `true` (the Groww resolver joins by ISIN). Feature-gated
/// behind `daily_universe_fetcher` (the storage row type lives there).
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
pub fn build_groww_constituency_rows<'a>(
    set: &'a GrowwWatchSet,
    trading_date_ist_nanos: i64,
    dry_run: bool,
) -> Vec<tickvault_storage::index_constituency_persistence::IndexConstituencyRow<'a>> {
    use tickvault_storage::index_constituency_persistence::IndexConstituencyRow;

    /// The NTM membership every resolved Groww stock belongs to (§31.1 — the Groww watch
    /// stock set is the NIFTY-Total-Market-resolved set).
    ///
    /// PINNED ASSUMPTION (test `test_groww_constituency_index_name_is_ntm_pinned`): today
    /// the Groww watch STOCK set is EXACTLY the NIFTY-Total-Market-resolved set
    /// (`instruments.rs`), so a single `index_name` is trivially correct for every
    /// constituent row. If the Groww watch set ever resolves stocks from MORE than one
    /// index, this hardcode becomes wrong and the per-entry `index_name` must be threaded
    /// through `WatchEntry` instead — tracked as a follow-up, NOT done here (no risky
    /// refactor while NTM is the only membership). The test pins the documented assumption.
    const GROWW_NTM_INDEX_NAME: &str = "NIFTY Total Market";

    // Iterate the FULL pre-cap universe (`master_entries`), NOT the capped live
    // `entries` — every resolved NTM stock is a constituent regardless of the
    // live-subscribe cap.
    set.master_entries
        .iter()
        .filter(|e| e.kind == WatchKind::Ltp && e.symbol_name.is_some())
        .map(|e| IndexConstituencyRow {
            trading_date_ist_nanos,
            index_name: GROWW_NTM_INDEX_NAME,
            security_id: e.security_id,
            exchange_segment: groww_segment_label(e),
            symbol_name: e.symbol_name.as_deref().unwrap_or(""),
            isin: e.isin.as_deref().unwrap_or(""),
            via_isin: true,
            source: "groww",
            dry_run,
            feed: Feed::Groww.as_str(),
        })
        .collect()
}

// ============================================================================
// instrument_lifecycle_audit (feed='groww') — pure diff + prior-snapshot read
// ============================================================================
//
// Closes the audit-coverage gap (operator 2026-06-29): the Groww master writer
// persisted `instrument_lifecycle` DATA rows but emitted ZERO transition rows,
// so `instrument_lifecycle_audit WHERE feed='groww'` was empty — no forensic
// "what changed in the Groww universe day-over-day" chain. We mirror the Dhan
// reconciler (`crates/app/src/apply_reconcile_plan.rs` + the pure storage
// `classify_transition`), tagged `feed='groww'`, scoped to `crates/core`.

/// Composite identity per I-P1-11 — `security_id` ALONE is not unique.
#[cfg(feature = "daily_universe_fetcher")]
type GrowwLifecycleKey = (i64, String);

/// HTTP timeout for the boot read-back SELECT of the prior `feed='groww'` snapshot.
#[cfg(feature = "daily_universe_fetcher")]
const GROWW_LIFECYCLE_QUERY_TIMEOUT_SECS: u64 = 15;

/// Hard cap on the QuestDB `/exec` success-response body we will buffer before
/// JSON-deserialize. The prior-snapshot read-back is at most a few hundred KB
/// (a few hundred `feed='groww'` rows), so 32 MiB is a generous ceiling that
/// still bounds a malformed / runaway QuestDB response. Over the cap → degrade
/// safe: return an error so the caller logs `GROWW-MASTER-01`, builds NO audit
/// rows, and continues (the data UPSERT still runs; next boot re-emits).
#[cfg(feature = "daily_universe_fetcher")]
const MAX_GROWW_EXEC_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// Prior per-instrument attributes (the `feed='groww'` snapshot) the diff classifier
/// compares against today's set. Mirror of the Dhan `PrevLifecycleAttrs`, scoped here.
#[cfg(feature = "daily_universe_fetcher")]
#[derive(Debug, Clone, PartialEq)]
pub struct GrowwPriorAttrs {
    pub state: LifecycleState,
    pub locked: bool,
    pub instrument_type: String,
    pub lot_size: i32,
    pub tick_size: f64,
    pub symbol_name: String,
    /// Prior `first_seen_date` in IST nanos (`0` = never set / NULL).
    /// Carried so the daily UPSERT PRESERVES the original first-seen date
    /// instead of resetting it to today (SEBI forensic — mirror of the
    /// Dhan `resolve_first_seen_nanos` guard in `crates/app/src/lifecycle_apply.rs`).
    pub first_seen_date_nanos: i64,
}

/// Today's primitive attributes for one Groww instrument (master-entry derived).
#[cfg(feature = "daily_universe_fetcher")]
#[derive(Debug, Clone, PartialEq)]
pub struct GrowwTodayAttrs {
    pub security_id: i64,
    pub exchange_segment: String,
    pub instrument_type: String,
    pub lot_size: i32,
    pub tick_size: f64,
    pub symbol_name: String,
}

/// Owned audit row so the pure diff can return `String`-backed values the async
/// loop borrows from (the persistence struct holds `&str`). Mirror of the Dhan
/// `OwnedLifecycleAuditRow`, `feed` fixed to `groww`.
#[cfg(feature = "daily_universe_fetcher")]
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedGrowwAuditRow {
    pub ts_nanos_ist: i64,
    pub trading_date_ist_nanos: i64,
    pub security_id: i64,
    pub exchange_segment: String,
    pub from_state: Option<LifecycleState>,
    pub to_state: LifecycleState,
    pub transition_kind: TransitionKind,
    pub symbol_name_after: String,
    pub lot_size_after: i32,
    pub tick_size_after: f64,
    /// §27 dry-run isolation flag, stamped at construction so the audit row's
    /// `dry_run` column is honest if these rows are ever appended by a direct
    /// caller during a dry-run. Today `emit_groww_lifecycle_audit` is reached
    /// only when `dry_run == false` (the `should_skip_master_append` early
    /// return guards it), so behaviour is unchanged — this is hardening.
    pub dry_run: bool,
}

#[cfg(feature = "daily_universe_fetcher")]
impl OwnedGrowwAuditRow {
    /// Borrow as the persistence-layer row for one batched append. `field_deltas`
    /// / `operator_note` / snapshot-only columns are left empty/sentinel — the
    /// Groww spot/index master carries no expiry/strike, and the simple
    /// appeared/updated/expired chain does not need a JSON field-delta payload.
    #[must_use]
    pub fn as_row(&self) -> InstrumentLifecycleAuditRow<'_> {
        InstrumentLifecycleAuditRow {
            ts_nanos_ist: self.ts_nanos_ist,
            trading_date_ist_nanos: self.trading_date_ist_nanos,
            security_id: self.security_id,
            exchange_segment: &self.exchange_segment,
            from_state: self.from_state,
            to_state: self.to_state,
            transition_kind: self.transition_kind,
            field_deltas: "",
            source_csv_sha256: "",
            operator_note: "",
            lifecycle_state_after: self.to_state,
            lot_size_after: self.lot_size_after,
            tick_size_after: self.tick_size_after,
            expiry_date_after_nanos: 0,
            symbol_name_after: &self.symbol_name_after,
            dry_run: self.dry_run,
            feed: Feed::Groww.as_str(),
        }
    }
}

/// Build the read-back SELECT for the prior `feed='groww'` lifecycle snapshot.
/// PURE. Reads ALL rows (active AND expired) so the diff can detect reactivation;
/// excludes §27 dry-run rows; scoped to `feed='groww'` so it never sees Dhan rows.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
pub fn build_groww_lifecycle_select_sql() -> String {
    // `cast(first_seen_date as long)` returns MICROS (the Dhan
    // `lifecycle_cache_loader` convention) so the UPSERT can preserve the
    // prior first-seen date instead of resetting it to today.
    format!(
        "SELECT security_id, exchange_segment, lifecycle_state, lifecycle_state_locked, \
         instrument_type, lot_size, tick_size, symbol_name, \
         cast(first_seen_date as long) \
         FROM {QUESTDB_TABLE_INSTRUMENT_LIFECYCLE} WHERE feed = 'groww' AND dry_run = false"
    )
}

/// PURE classifier: does a non-2xx QuestDB `/exec` response mean "the table does
/// not exist yet" (a fresh / brand-new database) rather than a genuine outage?
///
/// On a fresh-clone / fresh-QuestDB boot the `instrument_lifecycle` table is not
/// created until LATER in the persist orchestration, so the audit prior-snapshot
/// SELECT runs against a missing table. QuestDB answers that with HTTP 400 and a
/// body containing `table does not exist` (sometimes `does not exist` /
/// `table not found`). That is NOT a QuestDB outage — it is the Day-1 empty-prior
/// case, and the diff MUST treat it as an empty prior (every today row `appeared`),
/// not as a hard read failure that suppresses ALL audit rows.
///
/// Returns `true` ONLY when the response is a client error (4xx — QuestDB rejected
/// the query) AND the body names a missing/absent table. A 5xx, a connection
/// refusal, or any other body is NOT classified as table-missing, so a genuine
/// outage still degrades safely (the caller bails → logs `GROWW-MASTER-01` →
/// builds no audit rows → next boot re-emits).
///
/// PURE, zero-I/O — extracted so the table-missing → empty-prior decision is
/// unit-testable without a live QuestDB.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
fn is_table_missing_response(is_client_error: bool, body: &str) -> bool {
    if !is_client_error {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    lower.contains("table does not exist")
        || lower.contains("does not exist")
        || lower.contains("table not found")
}

/// Parse the QuestDB `dataset` array into the prior-attrs map. PURE — no I/O.
/// Unknown `lifecycle_state` / malformed rows are SKIPPED (counted by the caller
/// only implicitly — never guessed), mirroring the Dhan loader's discipline.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
pub fn parse_groww_lifecycle_dataset(
    dataset: &Value,
) -> HashMap<GrowwLifecycleKey, GrowwPriorAttrs> {
    let mut out: HashMap<GrowwLifecycleKey, GrowwPriorAttrs> = HashMap::new();
    let Some(rows) = dataset.as_array() else {
        return out;
    };
    for row in rows {
        let Some(cells) = row.as_array() else {
            continue;
        };
        if cells.len() < 9 {
            continue;
        }
        let (
            Some(security_id),
            Some(exchange_segment),
            Some(state_str),
            Some(locked),
            Some(instrument_type),
            Some(lot_size),
            Some(tick_size),
            Some(symbol_name),
        ) = (
            cells[0].as_i64(),
            cells[1].as_str(),
            cells[2].as_str(),
            cells[3].as_bool(),
            cells[4].as_str(),
            cells[5].as_i64(),
            cells[6].as_f64(),
            cells[7].as_str(),
        )
        else {
            continue;
        };
        let Some(state) = LifecycleState::from_wire(state_str) else {
            continue; // schema drift — counted-not-guessed (skipped).
        };
        // 9th cell: `cast(first_seen_date as long)` micros; NULL-tolerant
        // (a never-set first_seen must not drop the whole row — it just
        // falls back to today at resolve time).
        let first_seen_micros = cells[8].as_i64().unwrap_or(0);
        out.insert(
            (security_id, exchange_segment.to_string()),
            GrowwPriorAttrs {
                state,
                locked,
                instrument_type: instrument_type.to_string(),
                lot_size: i32::try_from(lot_size).unwrap_or(0),
                tick_size,
                symbol_name: symbol_name.to_string(),
                first_seen_date_nanos: first_seen_micros.saturating_mul(1_000),
            },
        );
    }
    out
}

/// Resolve the `first_seen_date` nanos to write on a Groww UPSERT row.
///
/// If a prior `feed='groww'` row exists with a real (`> 0`) first-seen date,
/// PRESERVE it — the daily full-row UPSERT must not reset "first ever seen"
/// to today (SEBI forensic, Rule 11; the Dhan reconciler enforces the same
/// via `resolve_first_seen_nanos`). Otherwise (brand-new instrument, or a
/// prior row whose first-seen was never set) use `today_ist_nanos`.
///
/// Cold-path O(1) lookup per master entry (the `(i64, String)` key needs one
/// owned `String` per call — honestly O(n) allocations over the ~779-entry
/// daily set, never per-tick).
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
fn resolve_groww_first_seen_nanos(
    security_id: i64,
    exchange_segment: &str,
    prior: &HashMap<GrowwLifecycleKey, GrowwPriorAttrs>,
    today_ist_nanos: i64,
) -> i64 {
    match prior.get(&(security_id, exchange_segment.to_string())) {
        Some(p) if p.first_seen_date_nanos > 0 => p.first_seen_date_nanos,
        _ => today_ist_nanos,
    }
}

/// PURE routing filter: which audit transitions require a lifecycle DATA
/// state-flip UPDATE (disappearances → non-active `to_state`), mirroring the
/// Dhan apply loop's `is_upsert_transition` split — present transitions
/// (`to_state` active) are covered by the full-row UPSERT; disappearance
/// transitions (`Expired` / `SegmentMoved`) get an in-place feed-scoped
/// state flip so the master row stops reading `active` forever.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
fn groww_disappearance_flips(rows: &[OwnedGrowwAuditRow]) -> Vec<&OwnedGrowwAuditRow> {
    rows.iter().filter(|r| !r.to_state.is_active()).collect()
}

/// Extract today's primitive attrs from the Groww master set. PURE. Iterates the
/// FULL pre-cap `master_entries` (not the capped live `entries`), exactly like the
/// lifecycle-row + constituency builders, so the audit chain covers the whole
/// universe regardless of the live-subscribe cap. Spot/index sentinels: `lot=0`,
/// `tick=0.0`. `instrument_type` is `INDEX` for an index value else `EQUITY`.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
pub fn groww_today_attrs(set: &GrowwWatchSet) -> Vec<GrowwTodayAttrs> {
    set.master_entries
        .iter()
        .map(|e| {
            let is_index = e.kind == WatchKind::IndexValue;
            let symbol_name: &str = if is_index {
                e.index_name.as_deref().unwrap_or(&e.exchange_token)
            } else {
                e.symbol_name.as_deref().unwrap_or(&e.exchange_token)
            };
            GrowwTodayAttrs {
                security_id: e.security_id,
                exchange_segment: groww_segment_label(e).to_string(),
                instrument_type: if is_index { "INDEX" } else { "EQUITY" }.to_string(),
                lot_size: 0,
                tick_size: 0.0,
                symbol_name: symbol_name.to_string(),
            }
        })
        .collect()
}

/// PURE diff classifier — produce one `instrument_lifecycle_audit` (feed='groww')
/// row per state transition, given today's master attrs + the prior `feed='groww'`
/// snapshot. Reuses the SAME pure `classify_transition` the Dhan reconciler uses
/// (appeared / updated / split / reactivated / expired / segment_moved / no-op).
///
/// Two passes mirror `compute_reconcile_plan`:
/// 1. present today → classify vs prior (None prior ⇒ `appeared`).
/// 2. disappeared (prior key absent today) → `expired` / `segment_moved`.
///
/// Day-1 (empty `prior`) ⇒ every today row is `appeared`, no expired pass — the
/// same bootstrap behaviour the Dhan side has. Composite `(security_id,
/// exchange_segment)` identity per I-P1-11. NO I/O; deterministic.
///
/// `now_ist_nanos` stamps the audit `ts`; `today_ist_nanos` is the IST-midnight
/// business/partition date. `dry_run` is stamped on every emitted row so the
/// §27 isolation column is honest for any direct caller.
#[cfg(feature = "daily_universe_fetcher")]
#[must_use]
pub fn build_groww_audit_rows(
    prior: &HashMap<GrowwLifecycleKey, GrowwPriorAttrs>,
    today: &[GrowwTodayAttrs],
    now_ist_nanos: i64,
    today_ist_nanos: i64,
    dry_run: bool,
) -> Vec<OwnedGrowwAuditRow> {
    use std::collections::HashSet;

    let mut rows = Vec::new();

    let today_keys: HashSet<GrowwLifecycleKey> = today
        .iter()
        .map(|t| (t.security_id, t.exchange_segment.clone()))
        .collect();
    // §23 segment-move detection: which segments does each security_id span today.
    let mut today_sid_segments: HashMap<i64, HashSet<String>> = HashMap::new();
    for t in today {
        today_sid_segments
            .entry(t.security_id)
            .or_default()
            .insert(t.exchange_segment.clone());
    }

    // ── Pass 1: present today ──
    for t in today {
        let key = (t.security_id, t.exchange_segment.clone());
        let prev = prior.get(&key);
        let prev_state = prev.map(|p| p.state);
        let prev_locked = prev.is_some_and(|p| p.locked);
        let fields_changed = prev.is_some_and(|p| {
            p.lot_size != t.lot_size
                || (p.tick_size - t.tick_size).abs() > f64::EPSILON
                || p.symbol_name != t.symbol_name
                || p.instrument_type != t.instrument_type
        });
        let is_split =
            prev.is_some_and(|p| is_stock_split(p.lot_size, t.lot_size, p.tick_size, t.tick_size));
        let input = ReconcileInput {
            prev_state,
            prev_locked,
            present_in_today_csv: true,
            segment_changed: false, // segment moves are recorded on the OLD key's disappearance
            fields_changed,
            is_split,
            instrument_type: &t.instrument_type,
        };
        if let Some((kind, to_state)) = classify_transition(&input) {
            rows.push(OwnedGrowwAuditRow {
                ts_nanos_ist: now_ist_nanos,
                trading_date_ist_nanos: today_ist_nanos,
                security_id: t.security_id,
                exchange_segment: t.exchange_segment.clone(),
                from_state: prev_state,
                to_state,
                transition_kind: kind,
                symbol_name_after: t.symbol_name.clone(),
                lot_size_after: t.lot_size,
                tick_size_after: t.tick_size,
                dry_run,
            });
        }
    }

    // ── Pass 2: disappeared (prior rows not in today's set) ──
    for (key, p) in prior {
        if today_keys.contains(key) {
            continue; // handled in pass 1
        }
        let (security_id, old_segment) = key;
        let segment_changed = today_sid_segments
            .get(security_id)
            .is_some_and(|segs| segs.iter().any(|s| s != old_segment));
        let input = ReconcileInput {
            prev_state: Some(p.state),
            prev_locked: p.locked,
            present_in_today_csv: false,
            segment_changed,
            fields_changed: false,
            is_split: false,
            instrument_type: &p.instrument_type,
        };
        if let Some((kind, to_state)) = classify_transition(&input) {
            rows.push(OwnedGrowwAuditRow {
                ts_nanos_ist: now_ist_nanos,
                trading_date_ist_nanos: today_ist_nanos,
                security_id: *security_id,
                exchange_segment: old_segment.clone(),
                from_state: Some(p.state),
                to_state,
                transition_kind: kind,
                // Disappearance carries the PRIOR attrs forward (no fresh values today).
                symbol_name_after: p.symbol_name.clone(),
                lot_size_after: p.lot_size,
                tick_size_after: p.tick_size,
                dry_run,
            });
        }
    }

    rows
}

/// Read the prior `feed='groww'` lifecycle snapshot for the audit diff. Best-effort
/// — returns `Err` on QuestDB unavailability so the caller logs `GROWW-MASTER-01`
/// and continues with NO audit rows (a Day-1 / empty table yields an empty map,
/// which the diff treats as "every today row is `appeared`").
///
/// Cold path — called once per Groww master persist.
#[cfg(feature = "daily_universe_fetcher")]
// TEST-EXEMPT: live HTTP read-back; the pure SQL builder + parser are unit-tested.
async fn load_prev_groww_lifecycle(
    questdb: &QuestDbConfig,
) -> anyhow::Result<HashMap<GrowwLifecycleKey, GrowwPriorAttrs>> {
    use anyhow::Context;

    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(GROWW_LIFECYCLE_QUERY_TIMEOUT_SECS))
        .build()
        .context("build reqwest client for Groww lifecycle read-back")?;
    let sql = build_groww_lifecycle_select_sql();
    let resp = client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
        .context("HTTP GET Groww lifecycle read-back against QuestDB")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let is_client_error = status.is_client_error();
        let body: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        // Fresh DB: the `instrument_lifecycle` table does not exist yet (it is
        // created later in the persist orchestration). QuestDB returns 4xx +
        // "table does not exist". Treat that as an EMPTY prior snapshot (Day-1 →
        // every today row `appeared`) — NOT a hard read failure that would
        // suppress all audit rows. A genuine outage (5xx / refused / other body)
        // still bails → degrade-safe per the caller's GROWW-MASTER-01 arm.
        if is_table_missing_response(is_client_error, &body) {
            info!(
                "[feeds] Groww lifecycle prior-snapshot: instrument_lifecycle table absent \
                 (fresh DB) — treating as empty prior (Day-1, all appeared)"
            );
            return Ok(HashMap::new());
        }
        anyhow::bail!("QuestDB Groww lifecycle read-back returned HTTP {status}: {body}");
    }
    #[derive(serde::Deserialize)]
    struct ExecResponse {
        #[serde(default)]
        dataset: Value,
    }
    // Bound the success body before buffering+deserialize (the error path already
    // caps at 200 chars above). Read the connection-timeout-bounded bytes and reject
    // anything over the cap so a malformed / runaway QuestDB response cannot force an
    // unbounded allocation. Degrade-safe: an error here means NO audit rows.
    let body = resp
        .bytes()
        .await
        .context("read QuestDB Groww lifecycle read-back body")?;
    if body.len() > MAX_GROWW_EXEC_RESPONSE_BYTES {
        anyhow::bail!(
            "QuestDB Groww lifecycle read-back body {} bytes exceeds cap {} bytes",
            body.len(),
            MAX_GROWW_EXEC_RESPONSE_BYTES
        );
    }
    let parsed: ExecResponse = serde_json::from_slice(&body)
        .context("parse QuestDB JSON dataset for Groww lifecycle read-back")?;
    Ok(parse_groww_lifecycle_dataset(&parsed.dataset))
}

/// Emit the `instrument_lifecycle_audit` (feed='groww') transition chain for one
/// Groww master persist. COLD-PATH, best-effort, degrade-safe: an append failure
/// logs `error!(code=GROWW-MASTER-01)`, bumps
/// `tv_groww_master_persist_errors_total{stage="lifecycle_audit"}`, and returns
/// `None` — it NEVER aborts Groww activation, the live feed, or any order/tick
/// path. Called audit-first (§24), BEFORE the lifecycle DATA UPSERT.
///
/// Takes the prior `feed='groww'` snapshot (hoisted to the caller 2026-07-05 so
/// the SAME snapshot also drives first-seen preservation on the DATA UPSERT and
/// the disappearance state-flips).
///
/// Returns:
/// - `Some(rows)` — the audit chain is durably recorded (or there were no
///   transitions; `rows` may be empty). The caller may proceed with the
///   lifecycle DATA UPSERT + the disappearance flips derived from `rows`.
/// - `None` — the audit append FAILED. Per §24 (mirror of the Dhan apply
///   loop's audit-batch gate) the caller MUST SKIP the lifecycle DATA writes
///   this run, so a state flip can never land without its forensic row —
///   otherwise tomorrow's diff would see the flipped state and NEVER re-emit
///   the lost transition. Next boot re-runs idempotently.
#[cfg(feature = "daily_universe_fetcher")]
// TEST-EXEMPT: cold-path ILP/HTTP orchestration; the pure diff/parser/builders + the degrade-safe failure arm (source-scanned) carry coverage; the bulk audit append is unit-tested + boot-integration-exercised in storage.
async fn emit_groww_lifecycle_audit(
    questdb: &QuestDbConfig,
    prior: &HashMap<GrowwLifecycleKey, GrowwPriorAttrs>,
    set: &GrowwWatchSet,
    now_ist_nanos: i64,
    trading_date_ist_nanos: i64,
    dry_run: bool,
) -> Option<Vec<OwnedGrowwAuditRow>> {
    let today = groww_today_attrs(set);
    let owned = build_groww_audit_rows(
        prior,
        &today,
        now_ist_nanos,
        trading_date_ist_nanos,
        dry_run,
    );
    if owned.is_empty() {
        info!("[feeds] Groww lifecycle-audit: no transitions to record (feed=groww)");
        return Some(owned);
    }
    let audit_rows: Vec<InstrumentLifecycleAuditRow<'_>> =
        owned.iter().map(OwnedGrowwAuditRow::as_row).collect();

    tickvault_storage::instrument_lifecycle_persistence::ensure_instrument_lifecycle_audit_table(
        questdb,
    )
    .await;
    match tickvault_storage::instrument_lifecycle_persistence::append_instrument_lifecycle_audit_rows(
        questdb,
        &audit_rows,
    )
    .await
    {
        Ok(()) => {
            info!(
                audit_rows = audit_rows.len(),
                "[feeds] Groww instrument_lifecycle_audit transition rows persisted (feed=groww)"
            );
            Some(owned)
        }
        Err(err) => {
            let (stage, code) = classify_persist_failure("lifecycle_audit");
            metrics::counter!("tv_groww_master_persist_errors_total", "stage" => stage)
                .increment(1);
            error!(
                code = code.code_str(),
                stage,
                rows = audit_rows.len(),
                ?err,
                "[feeds] Groww instrument_lifecycle_audit persist failed — skipping the \
                 lifecycle DATA UPSERT + state-flips this run (§24 audit-first; a data \
                 change must never land without its forensic row); best-effort cold-path \
                 chain, feed + ticks unaffected, next boot re-runs idempotently"
            );
            None
        }
    }
}

/// Persist the Groww instrument set into the SHARED `instrument_lifecycle` +
/// `index_constituency` master tables, tagged `feed='groww'`.
///
/// COLD-PATH, fire-and-forget, degrade-safe: any failure logs `error!(code=GROWW-MASTER-01)`,
/// increments `tv_groww_master_persist_errors_total{stage}`, and RETURNS — it NEVER aborts
/// Groww activation, the live feed, or any order/tick path. When `dry_run` is true the rows are
/// built and the would-write counts logged, but NO append runs (rule §27 isolation).
///
/// `watch_date` is a validated `YYYY-MM-DD` IST date (the caller already validates it).
///
/// Feature-gated impl: both shared master tables (`instrument_lifecycle`,
/// `index_constituency`) and their row types / append fns live behind the storage
/// `daily_universe_fetcher` feature. The non-gated stub below keeps the call site compiling
/// when the feature is off (in which case the master tables themselves do not exist).
#[cfg(feature = "daily_universe_fetcher")]
// TEST-EXEMPT: cold-path ILP I/O orchestration; not unit-testable without live QuestDB. The pure builders (groww_segment_label / build_groww_lifecycle_rows / build_groww_constituency_rows) + the should_skip_master_append / classify_persist_failure gates are unit-tested below; the bulk append fns it reuses are unit-tested + boot-integration-exercised in the storage crate.
pub async fn persist_groww_instruments(
    questdb: &QuestDbConfig,
    set: &GrowwWatchSet,
    watch_date: &str,
    dry_run: bool,
) {
    let now_ist_nanos = chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
    let trading_date_ist_nanos = watch_date_to_ist_midnight_nanos(watch_date);

    let constituency_rows = build_groww_constituency_rows(set, trading_date_ist_nanos, dry_run);

    if should_skip_master_append(dry_run) {
        // Dry-run: no QuestDB reads either (§27 isolation), so the would-write
        // lifecycle count is built against an EMPTY prior (count is identical —
        // first_seen resolution never changes the row count).
        let lifecycle_rows = build_groww_lifecycle_rows(
            set,
            &HashMap::new(),
            now_ist_nanos,
            trading_date_ist_nanos,
            dry_run,
        );
        info!(
            lifecycle_rows = lifecycle_rows.len(),
            constituency_rows = constituency_rows.len(),
            watch_date = %watch_date,
            "[feeds] Groww shared-master DRY-RUN — rows built, NOT written (isolation)"
        );
        return;
    }

    // ── FIX 13b (2026-07-04): QuestDB readiness gate ──
    // One-shot appends previously lost the day's feed='groww' rows on any
    // transient QuestDB hiccup at activation time. Probe quietly first; if
    // QuestDB never answers, degrade with the SAME GROWW-MASTER-01 contract
    // (stage="readiness") and return — the next boot/activation re-runs.
    if !questdb_master_ready(questdb).await {
        let (stage, code) = classify_persist_failure("readiness");
        metrics::counter!("tv_groww_master_persist_errors_total", "stage" => stage).increment(1);
        error!(
            code = code.code_str(),
            stage,
            attempts = MASTER_READINESS_ATTEMPTS,
            "[feeds] Groww shared-master persist skipped — QuestDB not ready; \
             best-effort cold-path master write; feed + ticks unaffected, next boot re-runs"
        );
        return;
    }

    // ── instrument_lifecycle table — CREATE FIRST (fresh-DB fix) ──
    // The audit emit below reads the prior `feed='groww'` snapshot from
    // `instrument_lifecycle`. On a fresh-clone / fresh-QuestDB boot that table does
    // not exist yet, and (before this) the SELECT 4xx'd → the audit took its
    // degrade-safe early return → ZERO `instrument_lifecycle_audit` rows on Day-1.
    // Ensuring the (idempotent) table FIRST means the read targets a real (empty)
    // table → Day-1 classifies all entries as `appeared`. The `load_prev_*` reader
    // also treats a table-missing response as an empty prior, so this is
    // belt-and-suspenders, not the sole guard.
    tickvault_storage::instrument_lifecycle_persistence::ensure_instrument_lifecycle_table(questdb)
        .await;

    // ── Prior `feed='groww'` snapshot — loaded ONCE (2026-07-05 fix) ──
    // The SAME snapshot drives all three lifecycle consumers below:
    // 1. the audit diff (which transitions happened),
    // 2. first-seen preservation on the DATA UPSERT (SEBI — never reset to today),
    // 3. the disappearance state-flips (derived from the audit rows).
    // A read failure degrades the WHOLE lifecycle chain for this run (audit +
    // DATA + flips are all skipped): writing the DATA UPSERT against an empty
    // prior would silently RESET every first_seen_date to today — strictly worse
    // than deferring to the next idempotent boot. The constituency append below
    // is prior-independent and still runs. Stage label stays `lifecycle_audit`
    // per the GROWW-MASTER-01 runbook ("EITHER the prior-snapshot read OR the
    // audit append").
    let prior = match load_prev_groww_lifecycle(questdb).await {
        Ok(prior) => prior,
        Err(err) => {
            let (stage, code) = classify_persist_failure("lifecycle_audit");
            metrics::counter!("tv_groww_master_persist_errors_total", "stage" => stage)
                .increment(1);
            error!(
                code = code.code_str(),
                stage,
                ?err,
                "[feeds] Groww lifecycle prior-snapshot read failed — skipping the \
                 lifecycle audit + DATA UPSERT + state-flips this run (an UPSERT \
                 without the prior would reset first_seen_date to today); \
                 best-effort cold-path chain, feed + ticks unaffected, next boot \
                 re-runs idempotently"
            );
            persist_groww_constituency(questdb, &constituency_rows).await;
            return;
        }
    };

    // ── instrument_lifecycle_audit (feed='groww') — AUDIT-FIRST (§24) ──
    // Diff today's master set against yesterday's `feed='groww'` snapshot and emit
    // the appeared/updated/expired transition chain BEFORE the lifecycle DATA UPSERT.
    // `None` = the audit append FAILED → per §24 the DATA UPSERT + state-flips are
    // SKIPPED this run (a data change must never land without its forensic row —
    // and a flipped DATA row would suppress tomorrow's re-diff of the lost
    // transition). The constituency append below still runs.
    let audit_rows = emit_groww_lifecycle_audit(
        questdb,
        &prior,
        set,
        now_ist_nanos,
        trading_date_ist_nanos,
        dry_run,
    )
    .await;

    if let Some(audit_rows) = audit_rows {
        // ── instrument_lifecycle DATA UPSERT (FIX 13b: bounded retry) ──
        let lifecycle_rows =
            build_groww_lifecycle_rows(set, &prior, now_ist_nanos, trading_date_ist_nanos, dry_run);
        let mut attempt: u32 = 0;
        loop {
            attempt = attempt.saturating_add(1);
            match tickvault_storage::instrument_lifecycle_persistence::append_instrument_lifecycle_rows(
                questdb,
                &lifecycle_rows,
            )
            .await
            {
                Ok(()) => {
                    info!(
                        lifecycle_rows = lifecycle_rows.len(),
                        "[feeds] Groww instrument_lifecycle rows persisted (feed=groww)"
                    );
                    break;
                }
                Err(err) if master_persist_should_retry(attempt) => {
                    let backoff_secs = master_persist_backoff_secs(attempt);
                    warn!(
                        ?err,
                        attempt,
                        backoff_secs,
                        "[feeds] Groww instrument_lifecycle persist attempt failed — retrying"
                    );
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                }
                Err(err) => {
                    // Degrade-safe FINAL failure: log + count + continue to the next
                    // table (unchanged contract). The classifier keeps the stage
                    // label + GROWW-MASTER-01 code in one place.
                    let (stage, code) = classify_persist_failure("lifecycle");
                    metrics::counter!("tv_groww_master_persist_errors_total", "stage" => stage)
                        .increment(1);
                    error!(
                        code = code.code_str(),
                        stage,
                        rows = lifecycle_rows.len(),
                        attempts = attempt,
                        ?err,
                        "[feeds] Groww instrument_lifecycle persist failed — best-effort cold-path \
                         master write; feed + ticks unaffected, next boot re-runs"
                    );
                    break;
                }
            }
        }

        // ── Disappearance state-flips (Gap-1 fix, 2026-07-05) ──
        // A disappeared instrument is ABSENT from today's UPSERT batch, so its
        // master DATA row would stay `active` forever (and the audit diff would
        // re-emit a duplicate `expired` transition EVERY day). Flip each
        // disappeared row's `lifecycle_state` in place via the FEED-SCOPED
        // UPDATE (`feed='groww' AND dry_run=false` — a Dhan row sharing the
        // composite key is never touched; NEVER a DELETE, SEBI §25). Runs AFTER
        // the audit append succeeded (§24 audit-first). Best-effort per flip.
        let flips = groww_disappearance_flips(&audit_rows);
        let mut flipped: usize = 0;
        for flip in &flips {
            match tickvault_storage::instrument_lifecycle_persistence::update_lifecycle_state_for_feed(
                questdb,
                flip.security_id,
                &flip.exchange_segment,
                flip.to_state,
                trading_date_ist_nanos,
                now_ist_nanos,
                Feed::Groww.as_str(),
            )
            .await
            {
                Ok(()) => flipped = flipped.saturating_add(1),
                Err(err) => {
                    let (stage, code) = classify_persist_failure("lifecycle_flip");
                    metrics::counter!("tv_groww_master_persist_errors_total", "stage" => stage)
                        .increment(1);
                    error!(
                        code = code.code_str(),
                        stage,
                        security_id = flip.security_id,
                        exchange_segment = %flip.exchange_segment,
                        to_state = flip.to_state.as_str(),
                        ?err,
                        "[feeds] Groww lifecycle disappearance state-flip failed — \
                         best-effort cold-path flip; the audit row is durable and the \
                         next boot's diff re-emits + re-flips idempotently"
                    );
                }
            }
        }
        if !flips.is_empty() {
            info!(
                flips = flips.len(),
                flipped, "[feeds] Groww lifecycle disappearance state-flips applied (feed=groww)"
            );
        }
    }

    persist_groww_constituency(questdb, &constituency_rows).await;
}

/// The `index_constituency` append chain (FIX 13a migration gate + FIX 13b
/// bounded retry), factored out of [`persist_groww_instruments`] so the
/// prior-snapshot-read degrade arm can still run it — the constituency rows
/// are prior-independent, and skipping them just because the LIFECYCLE prior
/// read failed would widen the degrade beyond what the failure implies.
/// Same degrade-safe GROWW-MASTER-01 contract (stage `constituency`).
#[cfg(feature = "daily_universe_fetcher")]
// TEST-EXEMPT: cold-path ILP I/O orchestration extracted verbatim from persist_groww_instruments (same coverage story: pure builders + retry gates unit-tested; bulk append unit-tested + boot-integration-exercised in storage).
async fn persist_groww_constituency(
    questdb: &QuestDbConfig,
    constituency_rows: &[tickvault_storage::index_constituency_persistence::IndexConstituencyRow<
        '_,
    >],
) {
    // ── index_constituency (FIX 13a gate + FIX 13b bounded retry) ──
    // The one-shot ts-pin TRUNCATE migration wipes ALL rows — QuestDB has no
    // row-level DELETE, so it cannot be feed-scoped. Await the migration gate
    // (bounded) so our feed='groww' rows land AFTER the truncate. Since the
    // F14 hardening (2026-07-05) the migration runs in the PROCESS-GLOBAL
    // boot prefix on every boot mode (never only behind the Dhan lane), is
    // exactly-once in-process, and marks the gate within ~75s worst case —
    // so a timeout here means the boot-prefix task (incl. its quiet QuestDB
    // readiness probe) is pathologically delayed, NOT that no truncate can
    // run. Proceeding on timeout is best-effort: if the truncate lands after
    // this append, the rows are re-persisted next boot/activation
    // (GROWW-MASTER-01 envelope).
    if !tickvault_storage::index_constituency_persistence::index_constituency_migration_gate()
        .wait(Duration::from_secs(
            CONSTITUENCY_MIGRATION_GATE_TIMEOUT_SECS,
        ))
        .await
    {
        warn!(
            timeout_secs = CONSTITUENCY_MIGRATION_GATE_TIMEOUT_SECS,
            "[feeds] Groww index_constituency persist: ts-pin migration gate not marked \
             within the bounded wait — appending best-effort; if the one-shot table \
             migration's truncate lands after this append, today's feed='groww' rows \
             are wiped and re-persisted at the next boot/activation"
        );
    }
    tickvault_storage::index_constituency_persistence::ensure_index_constituency_table(questdb)
        .await;
    let mut attempt: u32 = 0;
    loop {
        attempt = attempt.saturating_add(1);
        match tickvault_storage::index_constituency_persistence::append_index_constituency_rows(
            questdb,
            constituency_rows,
        )
        .await
        {
            Ok(()) => {
                info!(
                    constituency_rows = constituency_rows.len(),
                    "[feeds] Groww index_constituency rows persisted (feed=groww)"
                );
                break;
            }
            Err(err) if master_persist_should_retry(attempt) => {
                let backoff_secs = master_persist_backoff_secs(attempt);
                warn!(
                    ?err,
                    attempt,
                    backoff_secs,
                    "[feeds] Groww index_constituency persist attempt failed — retrying"
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            }
            Err(err) => {
                // Degrade-safe FINAL failure: log + count + return. Same
                // classifier, never aborts the feed or activation.
                let (stage, code) = classify_persist_failure("constituency");
                metrics::counter!("tv_groww_master_persist_errors_total", "stage" => stage)
                    .increment(1);
                error!(
                    code = code.code_str(),
                    stage,
                    rows = constituency_rows.len(),
                    attempts = attempt,
                    ?err,
                    "[feeds] Groww index_constituency persist failed — best-effort cold-path \
                     master write; feed + ticks unaffected, next boot re-runs"
                );
                break;
            }
        }
    }
}

/// No-op fallback when `daily_universe_fetcher` is OFF — the shared master tables (and their
/// storage modules) don't exist in that build, so there is nothing to persist. Keeps the
/// `groww_activation` call site compiling identically regardless of feature.
#[cfg(not(feature = "daily_universe_fetcher"))]
// TEST-EXEMPT: empty no-op stub (feature OFF — no master surface to write); exercised by test_persist_is_noop_when_feature_off.
pub async fn persist_groww_instruments(
    _questdb: &QuestDbConfig,
    _set: &GrowwWatchSet,
    _watch_date: &str,
    _dry_run: bool,
) {
    // The `instrument_lifecycle` / `index_constituency` master tables are gated behind
    // `daily_universe_fetcher`; without it there is no master surface to write.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_master_writer_labels_fno_ltp_entries_futidx() {
        // §36 (2026-07-08): an FNO Ltp entry (index future) must be labeled
        // FUTIDX (not EQUITY) in the feed='groww' master rows, with its
        // expiry stamped; CASH stocks stay EQUITY; indices stay INDEX.
        use super::super::instruments::{GrowwWatchSet, WatchEntry, WatchKind};
        let set = GrowwWatchSet {
            entries: vec![],
            master_entries: vec![
                WatchEntry {
                    exchange: "NSE".to_owned(),
                    segment: "FNO".to_owned(),
                    exchange_token: "61001".to_owned(),
                    kind: WatchKind::Ltp,
                    security_id: 61001,
                    isin: None,
                    symbol_name: Some("NSE-NIFTY-30Jul26-FUT".to_owned()),
                    index_name: None,
                    expiry_date: Some("2026-07-30".to_owned()),
                },
                WatchEntry {
                    exchange: "NSE".to_owned(),
                    segment: "CASH".to_owned(),
                    exchange_token: "2885".to_owned(),
                    kind: WatchKind::Ltp,
                    security_id: 2885,
                    isin: Some("INE002A01018".to_owned()),
                    symbol_name: Some("RELIANCE".to_owned()),
                    index_name: None,
                    expiry_date: None,
                },
            ],
            resolved_stocks: 1,
            unresolved_stocks: vec![],
            indices: 0,
        };
        let rows = build_groww_lifecycle_rows(&set, &std::collections::HashMap::new(), 1, 1, false);
        assert_eq!(rows.len(), 2);
        let fut = rows.iter().find(|r| r.security_id == 61001).expect("fut");
        assert_eq!(fut.instrument_type, "FUTIDX");
        assert_eq!(fut.exchange_segment, "NSE_FNO");
        assert!(fut.expiry_date_nanos > 0, "expiry stamped");
        let stock = rows.iter().find(|r| r.security_id == 2885).expect("eq");
        assert_eq!(stock.instrument_type, "EQUITY");
        assert_eq!(stock.expiry_date_nanos, 0);
    }
    use crate::feed::groww::instruments::{GrowwWatchSet, WatchEntry, WatchKind};

    // Used only by the feature-gated row-builder tests below.
    #[cfg(feature = "daily_universe_fetcher")]
    fn stock(token: &str, isin: &str, symbol: &str) -> WatchEntry {
        WatchEntry {
            exchange: "NSE".to_string(),
            segment: "CASH".to_string(),
            exchange_token: token.to_string(),
            kind: WatchKind::Ltp,
            security_id: token.parse::<i64>().unwrap_or(0),
            isin: Some(isin.to_string()),
            symbol_name: Some(symbol.to_string()),
            index_name: None,
            expiry_date: None,
        }
    }

    fn index(name: &str, exchange: &str, sid: i64) -> WatchEntry {
        WatchEntry {
            exchange: exchange.to_string(),
            segment: "CASH".to_string(),
            exchange_token: name.to_string(),
            kind: WatchKind::IndexValue,
            security_id: sid,
            isin: None,
            symbol_name: None,
            index_name: Some(format!("{exchange}-{name}")),
            expiry_date: None,
        }
    }

    fn ltp(exchange: &str, segment: &str) -> WatchEntry {
        WatchEntry {
            exchange: exchange.to_string(),
            segment: segment.to_string(),
            exchange_token: "1".to_string(),
            kind: WatchKind::Ltp,
            security_id: 1,
            isin: Some("INE000000001".to_string()),
            symbol_name: Some("X".to_string()),
            index_name: None,
            expiry_date: None,
        }
    }

    fn set_of(entries: Vec<WatchEntry>) -> GrowwWatchSet {
        let indices = entries
            .iter()
            .filter(|e| e.kind == WatchKind::IndexValue)
            .count();
        let resolved_stocks = entries.iter().filter(|e| e.kind == WatchKind::Ltp).count();
        // Uncapped case: the live-subscribe set and the master set are identical.
        let master_entries = entries.clone();
        GrowwWatchSet {
            entries,
            master_entries,
            resolved_stocks,
            unresolved_stocks: Vec::new(),
            indices,
        }
    }

    /// Builds a set where the live-subscribe `entries` is CAPPED to `entries`, but
    /// the master `master_entries` is the FULL pre-cap superset — the decoupling the
    /// row builders must honor. Used by the "uses master_entries not capped" tests.
    fn capped_set(capped: Vec<WatchEntry>, full: Vec<WatchEntry>) -> GrowwWatchSet {
        let indices = full
            .iter()
            .filter(|e| e.kind == WatchKind::IndexValue)
            .count();
        let resolved_stocks = full.iter().filter(|e| e.kind == WatchKind::Ltp).count();
        GrowwWatchSet {
            entries: capped,
            master_entries: full,
            resolved_stocks,
            unresolved_stocks: Vec::new(),
            indices,
        }
    }

    // ── groww_segment_label: every (kind, exchange, segment) permutation ──

    #[test]
    fn test_groww_segment_label_idx_i() {
        assert_eq!(groww_segment_label(&index("NIFTY", "NSE", 123)), "IDX_I");
        // A BSE SENSEX index value is still IDX_I (index segment).
        assert_eq!(groww_segment_label(&index("SENSEX", "BSE", 456)), "IDX_I");
    }

    #[test]
    fn test_groww_segment_label_nse_eq() {
        assert_eq!(groww_segment_label(&ltp("NSE", "CASH")), "NSE_EQ");
    }

    #[test]
    fn test_groww_segment_label_bse_eq() {
        assert_eq!(groww_segment_label(&ltp("BSE", "CASH")), "BSE_EQ");
    }

    #[test]
    fn test_groww_segment_label_nse_fno() {
        assert_eq!(groww_segment_label(&ltp("NSE", "FNO")), "NSE_FNO");
        // Case-insensitive segment match.
        assert_eq!(groww_segment_label(&ltp("NSE", "fno")), "NSE_FNO");
    }

    #[test]
    fn test_groww_segment_label_bse_fno() {
        assert_eq!(groww_segment_label(&ltp("BSE", "FNO")), "BSE_FNO");
    }

    #[test]
    fn test_groww_segment_label_unknown_exchange_falls_back_to_nse_eq_no_panic() {
        // Cannot occur from the resolvers, but must never panic — defensive fallback.
        assert_eq!(groww_segment_label(&ltp("MCX", "CASH")), "NSE_EQ");
    }

    /// LOW 1 (Groww adversarial security review, 2026-06-28): the unknown-exchange
    /// `warn!` logs the CSV-derived `exchange` + `segment`. A crafted Groww master
    /// row could embed a newline/control char to forge log lines (log injection →
    /// CloudWatch/Telegram). The fix sanitizes both values with
    /// `sanitize_audit_string` BEFORE logging. This source-scan ratchet pins that
    /// the warn path uses the sanitizer (a future refactor that drops it fails the
    /// build), and the assertion below proves the sanitizer actually strips the
    /// injection chars from a hostile exchange value.
    #[test]
    fn test_unknown_exchange_warn_sanitizes_csv_derived_values() {
        use tickvault_common::sanitize::sanitize_audit_string;

        // 1) Calling the fallback path with a hostile exchange must not panic and
        //    still returns the safe `&'static str` label (the log line is the only
        //    place the raw value would have appeared).
        let hostile = ltp("EVIL\n[FORGED] credential=leaked\r", "CA\nSH");
        assert_eq!(
            groww_segment_label(&hostile),
            "NSE_EQ",
            "hostile exchange still maps to the safe fallback label"
        );

        // 2) The sanitizer strips the injection vectors a crafted row would carry.
        let clean = sanitize_audit_string("EVIL\n[FORGED] credential=leaked\r");
        assert!(!clean.contains('\n'), "newline must be stripped: {clean}");
        assert!(
            !clean.contains('\r'),
            "carriage return must be stripped: {clean}"
        );

        // 3) Source-scan ratchet: the unknown-exchange warn arm MUST route the
        //    CSV-derived values through sanitize_audit_string. Scope to the
        //    `groww_segment_label` body so the scan can't be satisfied by this
        //    test's own text.
        let file = include_str!("shared_master_writer.rs");
        let start = file
            .find("pub fn groww_segment_label(")
            .expect("groww_segment_label present");
        let end = file[start..]
            .find("fn watch_date_to_ist_midnight_nanos")
            .map(|o| start + o)
            .expect("next fn follows groww_segment_label");
        let body = &file[start..end];
        assert!(
            body.contains("sanitize_audit_string(other)"),
            "the unknown-exchange warn must sanitize the CSV-derived `exchange`"
        );
        assert!(
            body.contains("sanitize_audit_string(&entry.segment)"),
            "the unknown-exchange warn must sanitize the CSV-derived `segment`"
        );
    }

    // ── build_groww_lifecycle_rows (feature-gated: storage row type) ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_lifecycle_rows_feed_is_groww() {
        let set = set_of(vec![
            stock("2885", "INE002A01018", "RELIANCE"),
            index("NIFTY", "NSE", 999),
        ]);
        let rows = build_groww_lifecycle_rows(&set, &HashMap::new(), 111, 222, false);
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(r.feed, "groww");
        }
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_lifecycle_rows_index_is_index_idx_i() {
        let set = set_of(vec![index("NIFTY", "NSE", 999)]);
        let rows = build_groww_lifecycle_rows(&set, &HashMap::new(), 111, 222, false);
        assert_eq!(rows[0].instrument_type, "INDEX");
        assert_eq!(rows[0].exchange_segment, "IDX_I");
        assert_eq!(rows[0].symbol_name, "NSE-NIFTY");
        assert_eq!(rows[0].security_id, 999);
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_lifecycle_rows_stock_is_equity_nse_eq() {
        let set = set_of(vec![stock("2885", "INE002A01018", "RELIANCE")]);
        let rows = build_groww_lifecycle_rows(&set, &HashMap::new(), 111, 222, false);
        assert_eq!(rows[0].instrument_type, "EQUITY");
        assert_eq!(rows[0].exchange_segment, "NSE_EQ");
        assert_eq!(rows[0].symbol_name, "RELIANCE");
        assert_eq!(rows[0].security_id, 2885);
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_lifecycle_rows_spot_sentinels() {
        let set = set_of(vec![stock("2885", "INE002A01018", "RELIANCE")]);
        let rows = build_groww_lifecycle_rows(&set, &HashMap::new(), 111, 222, false);
        let r = &rows[0];
        assert_eq!(r.lot_size, 0);
        assert_eq!(r.tick_size, 0.0);
        assert_eq!(r.expiry_date_nanos, 0);
        assert_eq!(r.strike_price, 0.0);
        assert_eq!(r.option_type, "");
        assert_eq!(r.underlying_security_id, 0);
        assert_eq!(r.underlying_symbol, "");
        assert_eq!(r.last_update_ts_nanos, 111);
        assert_eq!(r.first_seen_date_nanos, 222);
        assert!(r.lifecycle_state.is_active());
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_no_row_has_empty_feed() {
        // A feed-less / empty-feed row must be impossible: the builder always stamps a
        // non-empty feed, even when isin/symbol provenance is absent.
        let mut anon = stock("7", "X", "Y");
        anon.isin = None;
        anon.symbol_name = None;
        let set = set_of(vec![anon, index("NIFTY", "NSE", 1)]);
        let rows = build_groww_lifecycle_rows(&set, &HashMap::new(), 1, 2, false);
        for r in &rows {
            assert!(!r.feed.is_empty(), "feed must never be empty");
            assert_eq!(r.feed, "groww");
            // Falls back to the subscribe token so the row is never anonymous.
            assert!(!r.symbol_name.is_empty());
        }
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_regression_dhan_and_groww_same_id_distinct_under_composite_key() {
        // Regression: 2026-06-28 — feed-in-key. A Dhan row and a Groww row for the SAME
        // (security_id, exchange_segment) must be DISTINCT under the composite DEDUP key
        // (security_id, exchange_segment, feed). Pure in-memory proof of the key tuple — no
        // live QuestDB. Both rows share id+segment; only `feed` differs, so the key tuples
        // are distinct and a HashSet keeps both.
        use std::collections::HashSet;
        let set = set_of(vec![stock("2885", "INE002A01018", "RELIANCE")]);
        let groww = &build_groww_lifecycle_rows(&set, &HashMap::new(), 1, 2, false)[0];

        // The Dhan-side key tuple for the same instrument id+segment, feed='dhan'.
        let dhan_key = (groww.security_id, groww.exchange_segment, "dhan");
        let groww_key = (groww.security_id, groww.exchange_segment, groww.feed);

        assert_eq!(dhan_key.0, groww_key.0, "same security_id");
        assert_eq!(dhan_key.1, groww_key.1, "same exchange_segment");
        assert_ne!(dhan_key.2, groww_key.2, "feed differs");

        let mut keys: HashSet<(i64, &str, &str)> = HashSet::new();
        assert!(keys.insert(dhan_key), "dhan row inserts");
        assert!(
            keys.insert(groww_key),
            "groww row inserts as a DISTINCT row (feed differs)"
        );
        assert_eq!(
            keys.len(),
            2,
            "both feeds' rows coexist under the composite key"
        );
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_watch_date_to_ist_midnight_nanos() {
        // 1970-01-02 = 1 day after epoch = 86400 * 1e9 IST-midnight nanos.
        assert_eq!(
            watch_date_to_ist_midnight_nanos("1970-01-02"),
            86_400_i64 * 1_000_000_000
        );
        // Epoch day itself = 0.
        assert_eq!(watch_date_to_ist_midnight_nanos("1970-01-01"), 0);
        // Unparseable → 0 (defense-in-depth, no panic).
        assert_eq!(watch_date_to_ist_midnight_nanos("not-a-date"), 0);
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_constituency_rows_feed_is_groww() {
        let set = set_of(vec![
            stock("2885", "INE002A01018", "RELIANCE"),
            // An index entry is NOT its own constituent — must be filtered out.
            index("NIFTY", "NSE", 999),
        ]);
        let rows = build_groww_constituency_rows(&set, 222, false);
        assert_eq!(rows.len(), 1, "only the stock is a constituent row");
        let r = &rows[0];
        assert_eq!(r.feed, "groww");
        assert_eq!(r.symbol_name, "RELIANCE");
        assert_eq!(r.isin, "INE002A01018");
        assert_eq!(r.security_id, 2885);
        assert_eq!(r.exchange_segment, "NSE_EQ");
        assert!(r.via_isin);
        assert_eq!(r.source, "groww");
        assert_eq!(r.index_name, "NIFTY Total Market");
    }

    // ── full-universe master: builders iterate master_entries, not capped entries ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_lifecycle_rows_uses_master_entries_not_capped() {
        // Regression: the master must record the FULL pre-cap universe. Live
        // `entries` is capped to 2, but `master_entries` holds the full 5 → the
        // lifecycle builder MUST emit 5 rows, not 2.
        let full = vec![
            index("NIFTY", "NSE", 999),
            stock("2885", "INE002A01018", "RELIANCE"),
            stock("1594", "INE009A01021", "INFY"),
            stock("3045", "INE040A01034", "HDFCBANK"),
            stock("4963", "INE467B01029", "TCS"),
        ];
        let capped = full[..2].to_vec();
        let set = capped_set(capped, full);
        assert_eq!(set.entries.len(), 2, "live subscribe set is capped");
        let rows = build_groww_lifecycle_rows(&set, &HashMap::new(), 111, 222, false);
        assert_eq!(
            rows.len(),
            5,
            "lifecycle rows must cover the full master universe, not the capped entries"
        );
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_constituency_rows_uses_master_entries_not_capped() {
        // Same decoupling for constituency: full master has 4 stocks (+1 index);
        // live `entries` capped to 1 → constituency builder MUST emit 4 rows (the
        // index is filtered out), not be limited by the cap.
        let full = vec![
            index("NIFTY", "NSE", 999),
            stock("2885", "INE002A01018", "RELIANCE"),
            stock("1594", "INE009A01021", "INFY"),
            stock("3045", "INE040A01034", "HDFCBANK"),
            stock("4963", "INE467B01029", "TCS"),
        ];
        let capped = full[..1].to_vec();
        let set = capped_set(capped, full);
        let rows = build_groww_constituency_rows(&set, 222, false);
        assert_eq!(
            rows.len(),
            4,
            "constituency rows must cover every full-master stock (4), not the capped entries"
        );
    }

    // ── F1: dry-run builds rows but skips the append (§27 isolation) ──

    #[test]
    fn test_persist_dry_run_skips_append() {
        // The pure gate: dry_run=true → SKIP append; dry_run=false → do append.
        assert!(
            should_skip_master_append(true),
            "dry_run must skip the master append (isolation)"
        );
        assert!(
            !should_skip_master_append(false),
            "live run must NOT skip the append"
        );
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_persist_dry_run_builds_rows_but_skips_append() {
        // Even when the append is skipped, the rows are still BUILT (so the
        // would-write counts are observable) — "build-but-don't-write".
        let set = set_of(vec![
            stock("2885", "INE002A01018", "RELIANCE"),
            index("NIFTY", "NSE", 999),
        ]);
        let lifecycle = build_groww_lifecycle_rows(&set, &HashMap::new(), 111, 222, true);
        let constituency = build_groww_constituency_rows(&set, 222, true);
        assert!(should_skip_master_append(true), "dry-run skips the write");
        assert!(
            !lifecycle.is_empty(),
            "dry-run STILL builds lifecycle rows (build-but-don't-write)"
        );
        assert!(
            !constituency.is_empty(),
            "dry-run STILL builds constituency rows"
        );
        // The dry_run flag is stamped on every built row so the isolation column
        // (`dry_run`) is correct if these rows were ever inspected.
        assert!(lifecycle.iter().all(|r| r.dry_run));
        assert!(constituency.iter().all(|r| r.dry_run));
    }

    // ── F2: persist-failure classifies GROWW-MASTER-01, degrade-safe (no abort) ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_persist_failure_classifies_groww_master_01_without_abort() {
        // Pure classifier is TOTAL — every stage maps to GROWW-MASTER-01, echoing
        // the stage label back for the per-stage counter. It returns a value (it
        // never panics / aborts), proving the failure arm only logs+counts+returns.
        for stage in ["lifecycle", "constituency", "unexpected_stage"] {
            let (echoed, code) = classify_persist_failure(stage);
            assert_eq!(echoed, stage, "stage label echoes back for the counter");
            assert_eq!(
                code.code_str(),
                "GROWW-MASTER-01",
                "every persist failure is GROWW-MASTER-01"
            );
        }
        // Source-scan: the persist orchestration only log+count+returns on failure —
        // never aborts the feed. Scope the scan to the PRODUCTION fn body (between its
        // signature and the `#[cfg(not(...))]` stub) so this test's own assertion
        // strings — which necessarily NAME the banned tokens — are not self-matched.
        let file = include_str!("shared_master_writer.rs");
        // NB: the needle is split with `concat!` so this assertion string does not
        // itself trip the pub-fn-test-guard's `pub async fn ` grep (it is a string
        // literal, not a declaration). The runtime value is identical.
        let start = file
            .find(concat!("pub async f", "n persist_groww_instruments("))
            .expect("persist fn present");
        let end = file[start..]
            .find("#[cfg(not(feature = \"daily_universe_fetcher\"))]")
            .map(|o| start + o)
            .expect("non-gated stub follows the gated impl");
        let persist_body = &file[start..end];
        for banned in [
            "panic!(",
            "process::exit",
            "unreachable!(",
            "todo!(",
            ".abort(",
        ] {
            assert!(
                !persist_body.contains(banned),
                "degrade-safe persist_groww_instruments must never use {banned} — \
                 failure arms log+count+return"
            );
        }
        // Both failure arms must route through the classifier + bump the counter.
        assert!(
            persist_body.matches("classify_persist_failure(").count() >= 2,
            "both failure arms must route through classify_persist_failure"
        );
        assert!(
            persist_body.contains("tv_groww_master_persist_errors_total"),
            "failure arms must increment the per-stage counter"
        );
    }

    // ── F5: brownfield — a legacy feed=NULL/"" row never collides with dhan/groww ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_null_feed_row_distinct_from_groww_under_key() {
        // Regression: 2026-06-28 — feed-in-key brownfield. A backfilled/legacy row
        // with feed="" (NULL before self-heal stamps 'dhan') must be DISTINCT from
        // both the dhan and groww rows for the same (security_id, exchange_segment).
        use std::collections::HashSet;
        let set = set_of(vec![stock("2885", "INE002A01018", "RELIANCE")]);
        let groww = &build_groww_lifecycle_rows(&set, &HashMap::new(), 1, 2, false)[0];

        let legacy_key = (groww.security_id, groww.exchange_segment, "");
        let dhan_key = (groww.security_id, groww.exchange_segment, "dhan");
        let groww_key = (groww.security_id, groww.exchange_segment, groww.feed);

        // Same id + segment across all three; only `feed` differs.
        assert_eq!(legacy_key.0, groww_key.0);
        assert_eq!(legacy_key.1, groww_key.1);
        assert_ne!(legacy_key.2, dhan_key.2);
        assert_ne!(legacy_key.2, groww_key.2);

        let mut keys: HashSet<(i64, &str, &str)> = HashSet::new();
        assert!(keys.insert(legacy_key), "legacy NULL-feed row inserts");
        assert!(keys.insert(dhan_key), "dhan row inserts distinctly");
        assert!(keys.insert(groww_key), "groww row inserts distinctly");
        assert_eq!(
            keys.len(),
            3,
            "legacy(NULL)/dhan/groww are 3 DISTINCT rows under the composite key"
        );
    }

    // ── F6: IST-midnight convention matches the Dhan reconciler date→nanos ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_groww_ist_midnight_matches_dhan_lifecycle_convention() {
        // The Dhan daily-universe orchestrator (crates/app/src/main.rs) computes the
        // designated trading-date nanos as:
        //     now_ist.date_naive().and_hms_opt(0,0,0).and_utc().timestamp_nanos_opt()
        // i.e. IST-midnight epoch nanos with NO UTC offset added (data-integrity.md).
        // The Groww writer's `watch_date_to_ist_midnight_nanos` must produce the
        // IDENTICAL value for the same date, else dhan + groww rows for the same date
        // would carry different `ts` and silently split. Both reduce to
        // days_since_epoch * 86400 * 1e9, so they are exactly equal for every
        // parseable date — proven here against the Dhan formula directly.
        fn dhan_convention(date: chrono::NaiveDate) -> i64 {
            date.and_hms_opt(0, 0, 0)
                .and_then(|m| m.and_utc().timestamp_nanos_opt())
                .unwrap_or(0)
        }
        for ymd in [
            "1970-01-01",
            "1970-01-02",
            "2025-12-25",
            "2026-06-28",
            "2024-02-29", // leap day
        ] {
            let date = chrono::NaiveDate::parse_from_str(ymd, "%Y-%m-%d").unwrap();
            assert_eq!(
                watch_date_to_ist_midnight_nanos(ymd),
                dhan_convention(date),
                "Groww IST-midnight nanos must equal the Dhan reconciler convention for {ymd}"
            );
        }
        // Unparseable → 0 (defense-in-depth), never a panic.
        assert_eq!(watch_date_to_ist_midnight_nanos("not-a-date"), 0);
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_groww_constituency_index_name_is_ntm_pinned() {
        // Pins the documented single-membership assumption: today the Groww watch
        // stock set is exactly the NIFTY-Total-Market-resolved set, so every
        // constituency row carries `index_name = "NIFTY Total Market"`. If the watch
        // set ever spans multiple indices, this test fails — forcing the per-entry
        // index_name follow-up noted in the builder's doc comment.
        let set = set_of(vec![
            stock("2885", "INE002A01018", "RELIANCE"),
            stock("1594", "INE009A01021", "INFY"),
        ]);
        let rows = build_groww_constituency_rows(&set, 222, false);
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter().all(|r| r.index_name == "NIFTY Total Market"),
            "all Groww constituents are pinned to the single NTM membership"
        );
    }

    // ── AUDIT CHAIN (feed='groww'): diff + parser ──────────────────────────

    #[cfg(feature = "daily_universe_fetcher")]
    fn groww_prior(
        state: LifecycleState,
        locked: bool,
        itype: &str,
        lot: i32,
        tick: f64,
        sym: &str,
    ) -> GrowwPriorAttrs {
        GrowwPriorAttrs {
            state,
            locked,
            instrument_type: itype.to_string(),
            lot_size: lot,
            tick_size: tick,
            symbol_name: sym.to_string(),
            first_seen_date_nanos: 0,
        }
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_groww_today_attrs_index_and_stock_shapes() {
        let set = set_of(vec![
            index("NIFTY", "NSE", 999),
            stock("2885", "INE002A01018", "RELIANCE"),
        ]);
        let today = groww_today_attrs(&set);
        assert_eq!(today.len(), 2);
        let idx = today.iter().find(|t| t.security_id == 999).unwrap();
        assert_eq!(idx.instrument_type, "INDEX");
        assert_eq!(idx.exchange_segment, "IDX_I");
        assert_eq!(idx.symbol_name, "NSE-NIFTY");
        assert_eq!(idx.lot_size, 0);
        let stk = today.iter().find(|t| t.security_id == 2885).unwrap();
        assert_eq!(stk.instrument_type, "EQUITY");
        assert_eq!(stk.exchange_segment, "NSE_EQ");
        assert_eq!(stk.symbol_name, "RELIANCE");
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_audit_rows_day1_all_appeared() {
        // Empty prior (Day-1) → every today row is `appeared`, NO expired pass,
        // every row tagged feed='groww'.
        let set = set_of(vec![
            index("NIFTY", "NSE", 999),
            stock("2885", "INE002A01018", "RELIANCE"),
        ]);
        let today = groww_today_attrs(&set);
        let prior = HashMap::new();
        let rows = build_groww_audit_rows(&prior, &today, 111, 222, false);
        assert_eq!(rows.len(), 2, "both appeared");
        for r in &rows {
            assert_eq!(r.transition_kind, TransitionKind::Appeared);
            assert_eq!(r.to_state, LifecycleState::Active);
            assert_eq!(r.from_state, None);
            assert_eq!(r.ts_nanos_ist, 111);
            assert_eq!(r.trading_date_ist_nanos, 222);
            assert_eq!(r.as_row().feed, "groww");
            // Live run → dry_run stamped false on the row AND propagated to as_row().
            assert!(!r.dry_run);
            assert!(!r.as_row().dry_run);
        }
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_audit_rows_dry_run_propagates_to_as_row() {
        // §27 isolation: a dry-run build stamps dry_run=true on every emitted row,
        // and `as_row()` propagates it (NOT a hardcoded false) so a future direct
        // caller during a dry-run can never write dry_run=false audit rows.
        let set = set_of(vec![
            index("NIFTY", "NSE", 999),
            stock("2885", "INE002A01018", "RELIANCE"),
        ]);
        let today = groww_today_attrs(&set);
        let prior = HashMap::new();
        let rows = build_groww_audit_rows(&prior, &today, 1, 2, true);
        assert_eq!(rows.len(), 2, "both appeared on Day-1");
        for r in &rows {
            assert!(r.dry_run, "dry_run must be stamped on the built row");
            assert!(
                r.as_row().dry_run,
                "as_row() must propagate dry_run, not hardcode false"
            );
        }
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_audit_rows_unchanged_is_no_row() {
        // Present both days, no field change → NO audit row.
        let set = set_of(vec![stock("2885", "INE002A01018", "RELIANCE")]);
        let today = groww_today_attrs(&set);
        let mut prior = HashMap::new();
        prior.insert(
            (2885, "NSE_EQ".to_string()),
            groww_prior(LifecycleState::Active, false, "EQUITY", 0, 0.0, "RELIANCE"),
        );
        let rows = build_groww_audit_rows(&prior, &today, 1, 2, false);
        assert!(rows.is_empty(), "unchanged active row → no audit row");
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_audit_rows_symbol_change_is_updated() {
        // symbol_name changed RELIANCE → RELIANCE-NEW → `updated`.
        let set = set_of(vec![stock("2885", "INE002A01018", "RELIANCE-NEW")]);
        let today = groww_today_attrs(&set);
        let mut prior = HashMap::new();
        prior.insert(
            (2885, "NSE_EQ".to_string()),
            groww_prior(LifecycleState::Active, false, "EQUITY", 0, 0.0, "RELIANCE"),
        );
        let rows = build_groww_audit_rows(&prior, &today, 1, 2, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].transition_kind, TransitionKind::Updated);
        assert_eq!(rows[0].to_state, LifecycleState::Active);
        assert_eq!(rows[0].symbol_name_after, "RELIANCE-NEW");
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_audit_rows_disappeared_is_expired() {
        // In prior, absent today → `expired`, carrying the prior attrs.
        let set = set_of(vec![]);
        let today = groww_today_attrs(&set);
        let mut prior = HashMap::new();
        prior.insert(
            (2885, "NSE_EQ".to_string()),
            groww_prior(LifecycleState::Active, false, "EQUITY", 0, 0.0, "RELIANCE"),
        );
        let rows = build_groww_audit_rows(&prior, &today, 1, 2, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].transition_kind, TransitionKind::Expired);
        // EQUITY disappearance → ExpiredFromFno.
        assert_eq!(rows[0].to_state, LifecycleState::ExpiredFromFno);
        assert_eq!(rows[0].from_state, Some(LifecycleState::Active));
        assert_eq!(rows[0].symbol_name_after, "RELIANCE");
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_audit_rows_reactivation() {
        // Was expired, reappears today → `reactivated`.
        let set = set_of(vec![stock("2885", "INE002A01018", "RELIANCE")]);
        let today = groww_today_attrs(&set);
        let mut prior = HashMap::new();
        prior.insert(
            (2885, "NSE_EQ".to_string()),
            groww_prior(
                LifecycleState::ExpiredFromFno,
                false,
                "EQUITY",
                0,
                0.0,
                "RELIANCE",
            ),
        );
        let rows = build_groww_audit_rows(&prior, &today, 1, 2, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].transition_kind, TransitionKind::Reactivated);
        assert_eq!(rows[0].to_state, LifecycleState::Active);
        assert_eq!(rows[0].from_state, Some(LifecycleState::ExpiredFromFno));
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_audit_rows_locked_disappeared_skipped() {
        // A locked prior row that disappears must NOT auto-expire.
        let set = set_of(vec![]);
        let today = groww_today_attrs(&set);
        let mut prior = HashMap::new();
        prior.insert(
            (2885, "NSE_EQ".to_string()),
            groww_prior(LifecycleState::Active, true, "EQUITY", 0, 0.0, "RELIANCE"),
        );
        let rows = build_groww_audit_rows(&prior, &today, 1, 2, false);
        assert!(rows.is_empty(), "locked row must not auto-expire");
    }

    // ── GAP-1 (2026-07-05): disappearance flips the DATA row + no daily dup ──

    /// A disappeared stock yields EXACTLY ONE `Expired` audit transition, and
    /// `groww_disappearance_flips` selects it (non-active `to_state`) for the
    /// feed-scoped DATA state-flip while present/appeared rows are excluded.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_disappearance_yields_one_expired_transition_and_flip() {
        // Day 2: RELIANCE disappeared; INFY is still present.
        let set = set_of(vec![stock("1594", "INE009A01021", "INFY")]);
        let today = groww_today_attrs(&set);
        let mut prior = HashMap::new();
        prior.insert(
            (2885, "NSE_EQ".to_string()),
            groww_prior(LifecycleState::Active, false, "EQUITY", 0, 0.0, "RELIANCE"),
        );
        prior.insert(
            (1594, "NSE_EQ".to_string()),
            groww_prior(LifecycleState::Active, false, "EQUITY", 0, 0.0, "INFY"),
        );
        let rows = build_groww_audit_rows(&prior, &today, 1, 2, false);
        let expired: Vec<_> = rows
            .iter()
            .filter(|r| r.transition_kind == TransitionKind::Expired)
            .collect();
        assert_eq!(
            expired.len(),
            1,
            "the disappearance must emit exactly ONE Expired transition"
        );
        assert_eq!(expired[0].security_id, 2885);
        assert!(!expired[0].to_state.is_active());

        // The flip filter selects ONLY the disappearance (non-active to_state).
        let flips = groww_disappearance_flips(&rows);
        assert_eq!(flips.len(), 1, "one disappearance → one DATA state-flip");
        assert_eq!(flips[0].security_id, 2885);
        assert_eq!(flips[0].exchange_segment, "NSE_EQ");
    }

    /// Day-after idempotency (the daily-duplicate-audit bug this PR fixes): once
    /// the DATA row is flipped to `expired_from_fno`, the NEXT day's diff (prior
    /// state now non-active, instrument still absent) emits ZERO transitions —
    /// no duplicate `Expired` audit row per day, and no further flips.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_day_after_flip_emits_zero_duplicate_transitions() {
        let set = set_of(vec![]);
        let today = groww_today_attrs(&set);
        let mut prior = HashMap::new();
        prior.insert(
            (2885, "NSE_EQ".to_string()),
            groww_prior(
                LifecycleState::ExpiredFromFno,
                false,
                "EQUITY",
                0,
                0.0,
                "RELIANCE",
            ),
        );
        let rows = build_groww_audit_rows(&prior, &today, 1, 2, false);
        assert!(
            rows.is_empty(),
            "an already-expired prior row that stays absent must be a no-op — \
             the pre-fix behaviour re-emitted a duplicate Expired row EVERY day"
        );
        assert!(groww_disappearance_flips(&rows).is_empty());
    }

    /// `groww_disappearance_flips` is a pure filter on `to_state.is_active()`:
    /// active targets (appeared/updated/reactivated) are never flipped;
    /// non-active targets (expired/segment-moved-old-key) always are.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_groww_disappearance_flips_filters_on_to_state() {
        let mk = |sid: i64, to_state: LifecycleState, kind: TransitionKind| OwnedGrowwAuditRow {
            ts_nanos_ist: 1,
            trading_date_ist_nanos: 2,
            security_id: sid,
            exchange_segment: "NSE_EQ".to_string(),
            from_state: Some(LifecycleState::Active),
            to_state,
            transition_kind: kind,
            symbol_name_after: "X".to_string(),
            lot_size_after: 0,
            tick_size_after: 0.0,
            dry_run: false,
        };
        let rows = vec![
            mk(1, LifecycleState::Active, TransitionKind::Appeared),
            mk(2, LifecycleState::ExpiredFromFno, TransitionKind::Expired),
            mk(3, LifecycleState::Active, TransitionKind::Reactivated),
        ];
        let flips = groww_disappearance_flips(&rows);
        assert_eq!(flips.len(), 1);
        assert_eq!(flips[0].security_id, 2);
    }

    // ── GAP-2 (2026-07-05): first_seen preservation across the daily UPSERT ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_resolve_groww_first_seen_preserves_prior_and_falls_back() {
        let mut prior = HashMap::new();
        let mut attrs = groww_prior(LifecycleState::Active, false, "EQUITY", 0, 0.0, "RELIANCE");
        attrs.first_seen_date_nanos = 555;
        prior.insert((2885, "NSE_EQ".to_string()), attrs);
        // Known instrument with a real prior first_seen → PRESERVED.
        assert_eq!(
            resolve_groww_first_seen_nanos(2885, "NSE_EQ", &prior, 999),
            555
        );
        // Same sid on a DIFFERENT segment is a distinct identity (I-P1-11) → today.
        assert_eq!(
            resolve_groww_first_seen_nanos(2885, "IDX_I", &prior, 999),
            999
        );
        // Unknown instrument → today.
        assert_eq!(
            resolve_groww_first_seen_nanos(1594, "NSE_EQ", &prior, 999),
            999
        );
        // Prior row with the 0 sentinel (never set / NULL) → today.
        prior.insert(
            (1594, "NSE_EQ".to_string()),
            groww_prior(LifecycleState::Active, false, "EQUITY", 0, 0.0, "INFY"),
        );
        assert_eq!(
            resolve_groww_first_seen_nanos(1594, "NSE_EQ", &prior, 999),
            999
        );
    }

    /// Day-2 UPSERT regression (the SEBI first_seen reset bug this PR fixes):
    /// the daily full-row UPSERT for an ALREADY-KNOWN instrument must carry the
    /// PRIOR `first_seen_date`, never reset it to today; a brand-new instrument
    /// gets today.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_lifecycle_rows_preserves_first_seen_on_day2() {
        let set = set_of(vec![
            stock("2885", "INE002A01018", "RELIANCE"), // known since day 1
            stock("1594", "INE009A01021", "INFY"),     // brand new today
        ]);
        let mut prior = HashMap::new();
        let mut attrs = groww_prior(LifecycleState::Active, false, "EQUITY", 0, 0.0, "RELIANCE");
        attrs.first_seen_date_nanos = 1_000; // day-1 first_seen
        prior.insert((2885, "NSE_EQ".to_string()), attrs);

        let today_nanos = 2_000; // day-2 IST midnight
        let rows = build_groww_lifecycle_rows(&set, &prior, 3_000, today_nanos, false);
        let rel = rows
            .iter()
            .find(|r| r.security_id == 2885)
            .expect("RELIANCE row");
        assert_eq!(
            rel.first_seen_date_nanos, 1_000,
            "day-2 UPSERT must PRESERVE the day-1 first_seen_date (SEBI forensic)"
        );
        assert_eq!(rel.last_seen_date_nanos, today_nanos);
        let infy = rows
            .iter()
            .find(|r| r.security_id == 1594)
            .expect("INFY row");
        assert_eq!(
            infy.first_seen_date_nanos, today_nanos,
            "a brand-new instrument's first_seen is today"
        );
    }

    /// Source-scan ratchet (Gap-1 wiring): the persist orchestration MUST apply
    /// the FEED-SCOPED disappearance state-flips (`update_lifecycle_state_for_feed`
    /// via `groww_disappearance_flips`) AFTER the lifecycle DATA UPSERT — and the
    /// flip call carries the Groww feed label so a Dhan row sharing the composite
    /// key can never be mutated.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_persist_wires_feed_scoped_disappearance_flips_after_upsert() {
        let file = include_str!("shared_master_writer.rs");
        let persist_start = file
            .find(concat!("pub async f", "n persist_groww_instruments("))
            .expect("persist fn present");
        let persist_body = &file[persist_start..];
        let upsert = persist_body
            .find("append_instrument_lifecycle_rows(")
            .expect("lifecycle DATA UPSERT present");
        let flips = persist_body
            .find("groww_disappearance_flips(")
            .expect("disappearance flip filter wired into persist");
        let flip_call = persist_body
            .find("update_lifecycle_state_for_feed(")
            .expect("feed-scoped state-flip wired into persist");
        assert!(
            upsert < flips && flips < flip_call,
            "disappearance flips must run AFTER the lifecycle DATA UPSERT \
             (audit-first §24 already gates both on the durable audit append)"
        );
        assert!(
            persist_body.contains("Feed::Groww.as_str()"),
            "the state-flip must pass the Groww feed label (feed-scoped UPDATE)"
        );
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_build_groww_lifecycle_select_sql_scopes_to_groww_feed() {
        let sql = build_groww_lifecycle_select_sql();
        assert!(sql.contains("FROM instrument_lifecycle"));
        assert!(
            sql.contains("feed = 'groww'"),
            "must scope the prior snapshot to feed='groww'"
        );
        assert!(
            sql.contains("dry_run = false"),
            "must exclude §27 dry-run rows"
        );
        assert!(sql.contains("security_id"));
        assert!(sql.contains("exchange_segment"));
        assert!(sql.contains("lifecycle_state"));
        assert!(
            sql.contains("cast(first_seen_date as long)"),
            "must SELECT the prior first_seen_date (micros) so the daily UPSERT preserves it"
        );
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_parse_groww_lifecycle_dataset_well_formed_and_skips() {
        let dataset = serde_json::json!([
            [
                2885,
                "NSE_EQ",
                "active",
                false,
                "EQUITY",
                0,
                0.0,
                "RELIANCE",
                1_750_000_000_000_000_i64 // first_seen_date micros
            ],
            // unknown state → skipped (counted-not-guessed).
            [3, "NSE_EQ", "future_state", false, "EQUITY", 0, 0.0, "X", 0],
            // < 9 cells (legacy 8-cell shape without first_seen) → skipped.
            [9, "NSE_EQ", "active", false, "EQUITY", 0, 0.0, "Y"],
            // NULL first_seen (never set) → kept, sentinel 0.
            [
                1594,
                "NSE_EQ",
                "active",
                false,
                "EQUITY",
                0,
                0.0,
                "INFY",
                serde_json::Value::Null
            ],
            // not an array → skipped.
            "garbage"
        ]);
        let prior = parse_groww_lifecycle_dataset(&dataset);
        assert_eq!(
            prior.len(),
            2,
            "only the well-formed known-state rows survive"
        );
        let attrs = prior
            .get(&(2885, "NSE_EQ".to_string()))
            .expect("row present");
        assert_eq!(attrs.state, LifecycleState::Active);
        assert_eq!(attrs.symbol_name, "RELIANCE");
        assert!(!attrs.locked);
        // Micros from QuestDB → nanos in the prior snapshot (×1000).
        assert_eq!(attrs.first_seen_date_nanos, 1_750_000_000_000_000_000);
        let null_first_seen = prior
            .get(&(1594, "NSE_EQ".to_string()))
            .expect("NULL-first_seen row present");
        assert_eq!(
            null_first_seen.first_seen_date_nanos, 0,
            "NULL first_seen parses to the 0 sentinel (→ resolver falls back to today)"
        );
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_parse_groww_lifecycle_dataset_empty_and_non_array() {
        assert!(parse_groww_lifecycle_dataset(&serde_json::json!([])).is_empty());
        assert!(parse_groww_lifecycle_dataset(&serde_json::json!({"x": 1})).is_empty());
    }

    // ── FRESH-DB FIX: table-missing 4xx → empty prior (Day-1), not a hard bail ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_is_table_missing_response_classifies_questdb_missing_table() {
        // QuestDB returns 4xx + this body when the table has not been created yet
        // (fresh-clone / fresh-DB boot). It MUST be classified as table-missing so
        // the reader returns an empty prior (Day-1) instead of suppressing all
        // audit rows. The exact body QuestDB emits, plus its variants.
        for body in [
            r#"{"error":"table does not exist"}"#,
            r#"{"error":"table does not exist [table=instrument_lifecycle]"}"#,
            "table not found",
            // Case-insensitive match (QuestDB casing has varied across versions).
            "TABLE DOES NOT EXIST",
        ] {
            assert!(
                is_table_missing_response(true, body),
                "client-error + missing-table body must classify as table-missing: {body}"
            );
        }
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_is_table_missing_response_rejects_genuine_outage() {
        // A 5xx (server error / genuine QuestDB outage) is NOT table-missing even if
        // the body somehow mentioned "exist" — only a CLIENT error (4xx, query
        // rejected) counts. This preserves the degrade-safe bail for real outages.
        assert!(
            !is_table_missing_response(false, r#"{"error":"table does not exist"}"#),
            "a 5xx (server error) must NOT be classified as table-missing — genuine outage bails"
        );
        // A 4xx whose body is some OTHER rejection (e.g. a syntax error) is NOT
        // table-missing → still bails, so we never silently swallow a real query bug.
        assert!(
            !is_table_missing_response(true, r#"{"error":"unexpected token: GROWW"}"#),
            "a 4xx with an unrelated body must NOT be treated as table-missing"
        );
        assert!(
            !is_table_missing_response(true, ""),
            "an empty body is NOT table-missing — degrade-safe bail"
        );
    }

    /// Day-1 / fresh-DB behaviour, end-to-end on the PURE path: an empty prior
    /// (which is exactly what the table-missing classifier now returns) makes the
    /// diff classify EVERY Groww master entry as `appeared`, so a brand-new
    /// database DOES get its `instrument_lifecycle_audit` (feed='groww') rows on the
    /// first boot. This is the regression that was silently empty before this fix.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_fresh_db_empty_prior_yields_day1_appeared_audit_rows() {
        // A representative Groww master set (indices + resolved stocks).
        let set = set_of(vec![
            index("NIFTY", "NSE", 999),
            stock("2885", "INE002A01018", "RELIANCE"),
            stock("1594", "INE009A01021", "INFY"),
        ]);
        let today = groww_today_attrs(&set);

        // The fresh-DB read now returns an empty prior (table-missing → empty map).
        let empty_prior: HashMap<GrowwLifecycleKey, GrowwPriorAttrs> = HashMap::new();
        assert!(
            is_table_missing_response(true, r#"{"error":"table does not exist"}"#),
            "the fresh-DB response classifies as table-missing → empty prior"
        );

        let rows = build_groww_audit_rows(&empty_prior, &today, 111, 222, false);
        assert_eq!(
            rows.len(),
            3,
            "fresh DB must produce one Day-1 `appeared` audit row per master entry, not zero"
        );
        for r in &rows {
            assert_eq!(r.transition_kind, TransitionKind::Appeared);
            assert_eq!(r.from_state, None);
            assert_eq!(r.as_row().feed, "groww");
        }
    }

    /// Source-scan ratchet (Fix 2 wiring): the persist orchestration MUST create the
    /// `instrument_lifecycle` table BEFORE the audit emit reads its prior snapshot,
    /// so a fresh DB never reads a missing table. Pins the ordering so a future
    /// refactor cannot silently reintroduce the Day-1 empty-audit bug.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_persist_ensures_lifecycle_table_before_audit_emit() {
        let file = include_str!("shared_master_writer.rs");
        let persist_start = file
            .find(concat!("pub async f", "n persist_groww_instruments("))
            .expect("persist fn present");
        let persist_body = &file[persist_start..];
        let ensure = persist_body
            .find("ensure_instrument_lifecycle_table(questdb)")
            .expect("ensure_instrument_lifecycle_table call present in persist");
        // 2026-07-05: the prior snapshot is now loaded ONCE in persist (hoisted
        // out of the emit) and passed into the audit diff — the ordering ratchet
        // pins ensure-table → prior-read → audit-emit → DATA UPSERT.
        let prior_read = persist_body
            .find("load_prev_groww_lifecycle(questdb)")
            .expect("prior-snapshot read present in persist");
        let emit = persist_body
            .find("emit_groww_lifecycle_audit(")
            .expect("audit emit call present in persist");
        assert!(
            ensure < prior_read,
            "instrument_lifecycle table must be ensured BEFORE the audit prior-snapshot read \
             (fresh-DB Day-1 fix)"
        );
        assert!(
            prior_read < emit,
            "the prior snapshot must be loaded BEFORE the audit diff consumes it"
        );
        // §24 audit-first is still preserved: the emit precedes the DATA UPSERT.
        let upsert = persist_body
            .find("append_instrument_lifecycle_rows(")
            .expect("lifecycle DATA UPSERT present");
        assert!(
            emit < upsert,
            "audit emission must still run BEFORE the lifecycle DATA UPSERT (§24 audit-first)"
        );
    }

    /// Fix 3 regression: the `index_constituency` persist creates its table BEFORE
    /// appending rows, so it does NOT share the Day-1 fresh-DB ordering bug that
    /// affected the audit read. (The operator's "0 constituency rows" observation
    /// was a stale-binary artifact — the current main code, pinned here, writes
    /// constituency rows on the first boot.) Source-scan pins ensure-before-append.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_persist_ensures_constituency_table_before_append() {
        let file = include_str!("shared_master_writer.rs");
        let persist_start = file
            .find(concat!("pub async f", "n persist_groww_instruments("))
            .expect("persist fn present");
        let persist_body = &file[persist_start..];
        let ensure = persist_body
            .find("ensure_index_constituency_table(questdb)")
            .expect("ensure_index_constituency_table call present in persist");
        let append = persist_body
            .find("append_index_constituency_rows(")
            .expect("append_index_constituency_rows call present in persist");
        assert!(
            ensure < append,
            "index_constituency table must be ensured BEFORE appending rows — \
             constituency persist is immune to the fresh-DB ordering bug"
        );
    }

    /// Fix 3 regression: the constituency builder DOES produce rows for a resolved
    /// Groww stock set (the build itself is not the reason for 0 rows). A resolved
    /// stock always carries `symbol_name: Some(_)` (the resolver sets it), so every
    /// resolved stock becomes a constituency row.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_constituency_build_is_nonempty_for_resolved_stock_set() {
        let set = set_of(vec![
            index("NIFTY", "NSE", 999), // filtered out (not a constituent)
            stock("2885", "INE002A01018", "RELIANCE"),
            stock("1594", "INE009A01021", "INFY"),
            stock("3045", "INE040A01034", "HDFCBANK"),
        ]);
        let rows = build_groww_constituency_rows(&set, 222, false);
        assert_eq!(
            rows.len(),
            3,
            "every resolved stock (symbol_name set) yields one constituency row; the index is filtered"
        );
        assert!(rows.iter().all(|r| r.feed == "groww"));
    }

    /// Source-scan ratchet: the Groww audit emitter MUST be degrade-safe — its
    /// failure arms log+count+return, never panic/abort, and the `emit_*` call is
    /// wired audit-FIRST into the persist orchestration. Scoped to the production
    /// fn bodies so the assertion strings don't self-match.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_groww_audit_emitter_is_degrade_safe_and_wired() {
        let file = include_str!("shared_master_writer.rs");
        let start = file
            .find("async fn emit_groww_lifecycle_audit(")
            .expect("emit fn present");
        let end = file[start..]
            .find(concat!("pub async f", "n persist_groww_instruments("))
            .map(|o| start + o)
            .expect("persist fn follows emit");
        let emit_body = &file[start..end];
        for banned in ["panic!(", "process::exit", "unreachable!(", "todo!("] {
            assert!(
                !emit_body.contains(banned),
                "degrade-safe emit_groww_lifecycle_audit must never use {banned}"
            );
        }
        // The append-failure arm routes through the classifier + bumps the
        // counter (the prior-snapshot read arm moved to persist, 2026-07-05).
        assert!(
            emit_body
                .matches("classify_persist_failure(\"lifecycle_audit\")")
                .count()
                >= 1,
            "the audit-append failure arm must classify as lifecycle_audit/GROWW-MASTER-01"
        );
        assert!(
            emit_body.contains("tv_groww_master_persist_errors_total"),
            "audit failure arms must increment the per-stage counter"
        );
        // Wired audit-FIRST: the emit call precedes the lifecycle DATA UPSERT.
        let persist_start = file
            .find(concat!("pub async f", "n persist_groww_instruments("))
            .expect("persist fn present");
        let persist_body = &file[persist_start..];
        // The hoisted prior-snapshot read failure arm classifies identically
        // (the GROWW-MASTER-01 runbook: stage `lifecycle_audit` covers EITHER
        // the prior-snapshot read OR the audit append).
        assert!(
            persist_body
                .matches("classify_persist_failure(\"lifecycle_audit\")")
                .count()
                >= 1,
            "the prior-snapshot read failure arm must classify as lifecycle_audit/GROWW-MASTER-01"
        );
        let emit_call = persist_body
            .find("emit_groww_lifecycle_audit(")
            .expect("emit call wired into persist");
        let lifecycle_upsert = persist_body
            .find("append_instrument_lifecycle_rows(")
            .expect("lifecycle UPSERT present");
        assert!(
            emit_call < lifecycle_upsert,
            "audit emission must run BEFORE the lifecycle DATA UPSERT (§24 audit-first)"
        );
    }

    // ── F3: feature OFF — persist_groww_instruments is a compiled no-op ──

    #[cfg(not(feature = "daily_universe_fetcher"))]
    #[tokio::test]
    async fn test_persist_is_noop_when_feature_off() {
        // With `daily_universe_fetcher` OFF the shared master tables don't exist, so
        // the stub must compile + return WITHOUT touching QuestDB. We call it with an
        // empty set + a bogus questdb config; it must return cleanly (no connect, no
        // panic). Proves the call site compiles identically regardless of feature.
        use tickvault_common::config::QuestDbConfig;
        let set = set_of(vec![]);
        let questdb = QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        };
        // Returns () without any network I/O — the stub body is empty.
        persist_groww_instruments(&questdb, &set, "2026-06-28", false).await;
    }

    // ── FIX 13b (2026-07-04): readiness + bounded-retry primitives ──

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_master_persist_backoff_secs_ladder() {
        // attempt 1 → 2s, attempt 2 → 4s, beyond the ladder caps at 4s.
        assert_eq!(master_persist_backoff_secs(1), 2);
        assert_eq!(master_persist_backoff_secs(2), 4);
        assert_eq!(master_persist_backoff_secs(3), 4);
        assert_eq!(master_persist_backoff_secs(u32::MAX), 4);
        // Defensive: attempt 0 (unreachable — attempts are 1-based) still bounded.
        assert_eq!(master_persist_backoff_secs(0), 2);
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_master_persist_should_retry_boundaries() {
        assert!(master_persist_should_retry(1));
        assert!(master_persist_should_retry(2));
        assert!(!master_persist_should_retry(MASTER_PERSIST_MAX_ATTEMPTS));
        assert!(!master_persist_should_retry(
            MASTER_PERSIST_MAX_ATTEMPTS + 1
        ));
        assert_eq!(MASTER_PERSIST_MAX_ATTEMPTS, 3, "operator-locked 3 attempts");
    }

    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn test_classify_persist_failure_readiness_stage() {
        let (stage, code) = classify_persist_failure("readiness");
        assert_eq!(stage, "readiness");
        assert_eq!(code.code_str(), "GROWW-MASTER-01");
    }

    /// FIX 13a ratchet (source-scan): the constituency append must await the
    /// ts-pin migration gate FIRST, so the Dhan-lane TRUNCATE can never wipe
    /// just-written `feed='groww'` rows (QuestDB has no row-level DELETE).
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn ratchet_constituency_append_waits_for_migration_gate() {
        let src = include_str!("shared_master_writer.rs");
        let gate = src
            .find("index_constituency_migration_gate()")
            .expect("the constituency stage must await the migration gate");
        let append = src
            .find("append_index_constituency_rows(")
            .expect("the constituency append must exist");
        assert!(
            gate < append,
            "FIX 13a regression: the migration-gate wait must precede the \
             index_constituency append in persist_groww_instruments"
        );
    }

    /// F13 wording pin (2026-07-05): the gate-timeout `warn!` must NOT carry
    /// the FALSE "Dhan lane likely OFF; no truncate can run" premise (untrue
    /// on the dhan+groww profile AND under the D2b runtime-enable lifecycle —
    /// it misdirected triage), and MUST state the honest best-effort
    /// consequence instead.
    #[cfg(feature = "daily_universe_fetcher")]
    #[test]
    fn ratchet_gate_timeout_warn_is_honest() {
        let src = include_str!("shared_master_writer.rs");
        // Needle assembled at runtime so THIS test's own source (which
        // include_str! sees) never contains the banned literal.
        let false_premise = ["no truncate", " can run)"].concat();
        assert!(
            !src.contains(&false_premise),
            "F13 regression: the gate-timeout warn re-acquired the false \
             'Dhan lane OFF => no truncate' premise"
        );
        assert!(
            src.contains("re-persisted at the next boot/activation"),
            "F13 regression: the gate-timeout warn lost its honest \
             best-effort consequence wording"
        );
    }
}
