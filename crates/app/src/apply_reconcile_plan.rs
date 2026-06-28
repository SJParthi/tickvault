//! The async apply step of the daily reconciler — walks a
//! `Vec<ReconcileAction>` (from [`crate::lifecycle_reconcile_plan`]) and,
//! per action, performs the §24 audit-first write ordering:
//!
//! 1. **Append the `instrument_lifecycle_audit` row** carrying the REAL
//!    `transition_kind` + the §25 `*_after` snapshot.
//! 2. **Then** EITHER a full-row UPSERT into `instrument_lifecycle`
//!    (present transitions → `Active`) OR an in-place state UPDATE
//!    (disappearances → `expired_*`).
//!
//! If step 1 fails the action is skipped (counted as an error) — we do
//! NOT write the lifecycle change without its forensic row. If step 1
//! succeeds but step 2 fails, the audit row records the intended
//! transition and the next boot's idempotent re-run (deterministic CSV
//! diff + DEDUP UPSERT) completes the lifecycle write. Either way a crash
//! never produces a lifecycle change without an audit trail. See
//! [`crate::lifecycle_apply`] module docs for why this append-first
//! ordering replaces the literal §24 "pending → real" sequence (the
//! append-only audit table carries `transition_kind` in its DEDUP key).
//!
//! Errors never abort the loop — each is logged + counted and the walk
//! continues. The reconciler is idempotent, so a partial apply is
//! completed on the next boot.
//!
//! # Why a disappearance UPDATE always matches a row
//!
//! Disappearance actions (`Expired` / `SegmentMoved`) come from pass 2 of
//! [`crate::lifecycle_reconcile_plan::compute_reconcile_plan`], which
//! iterates the read-back `prev` map. That map is loaded by
//! [`crate::lifecycle_cache_loader`] from
//! `SELECT … FROM instrument_lifecycle WHERE dry_run = false`, so every
//! disappearance key corresponds to a row that **already exists** in the
//! table. The in-place state UPDATE therefore always matches exactly that
//! row — a 0-row UPDATE is not reachable in the normal (non-dry-run)
//! flow, so counting it as `expired` is honest.
//!
//! # Honest envelope — `dry_run` write isolation
//!
//! In `--dry-run-universe` mode the audit rows carry `dry_run = true`, but
//! the `instrument_lifecycle` table's DEDUP key is
//! `(ts, security_id, exchange_segment)` — it does NOT include `dry_run`.
//! So a dry-run UPSERT/UPDATE of an existing `(security_id,
//! exchange_segment)` mutates the SAME row as the real one. Read-side
//! isolation holds (the reconciler reads `WHERE dry_run = false`), but
//! write-side isolation is bounded by that DEDUP key. Full dry-run write
//! isolation (DEDUP key including `dry_run`, or a separate shadow table)
//! is a schema concern tracked for the orchestrator follow-up, NOT solved
//! in this apply step.
//!
//! The pure row-builders ([`build_audit_row_for_action`],
//! [`build_lifecycle_upsert_row`]) are unit-testable without QuestDB; the
//! async loop is exercised via a transport-error path (unreachable host).
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21 —
//! dormant in the default build.

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::HashMap;

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;
use tickvault_storage::instrument_lifecycle_persistence::{
    InstrumentLifecycleAuditRow, InstrumentLifecycleRow, LIFECYCLE_FEED_DHAN, LifecycleState,
    TransitionKind, append_instrument_lifecycle_audit_rows, append_instrument_lifecycle_rows,
    update_lifecycle_state,
};
use tracing::{error, info};

use crate::lifecycle_apply::{
    expiry_date_to_ist_nanos, is_upsert_transition, resolve_first_seen_nanos,
};
use crate::lifecycle_cache_loader::{LifecycleKey, PrevLifecycleAttrs};
use crate::lifecycle_reconcile_plan::ReconcileAction;
use crate::today_instrument::TodayInstrument;

/// Counter name: a derivative carried a non-empty expiry string that
/// failed to parse and collapsed to the `0` "no-expiry" sentinel (the
/// #848 LOW). Surfaced so the operator can spot a Dhan-side expiry-format
/// drift that would otherwise be silently conflated with spot/index rows.
const MALFORMED_EXPIRY_COUNTER: &str = "tv_lifecycle_malformed_expiry_total";

/// Tally of one `apply_reconcile_plan` run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// Present transitions whose full-row UPSERT succeeded.
    pub upserted: usize,
    /// Disappearance transitions whose in-place state UPDATE succeeded.
    pub expired: usize,
    /// Actions whose audit-append OR lifecycle write returned `Err` (the
    /// next idempotent boot completes them).
    pub errors: usize,
    /// Present actions whose expiry string was malformed (non-empty but
    /// parsed to the 0 sentinel). A subset of `upserted` — informational.
    pub malformed_expiry: usize,
    /// Present actions that had no matching today-detail row (invariant
    /// violation — should be 0; logged + skipped if it ever happens).
    pub skipped_missing_detail: usize,
}

/// Owned mirror of [`InstrumentLifecycleAuditRow`] so the pure builder can
/// return a `String`-backed value the async loop borrows from. (The
/// persistence struct holds `&str`, so it can't outlive a temporary.)
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedLifecycleAuditRow {
    pub ts_nanos_ist: i64,
    pub trading_date_ist_nanos: i64,
    pub security_id: i64,
    pub exchange_segment: String,
    pub from_state: Option<LifecycleState>,
    pub to_state: LifecycleState,
    pub transition_kind: TransitionKind,
    pub field_deltas: String,
    pub source_csv_sha256: String,
    pub operator_note: String,
    pub lifecycle_state_after: LifecycleState,
    pub lot_size_after: i32,
    pub tick_size_after: f64,
    pub expiry_date_after_nanos: i64,
    pub symbol_name_after: String,
    pub dry_run: bool,
    /// Broker-source label (operator override 2026-06-28 — feed in-key on every
    /// persisted table). Stamped `'dhan'` today (Dhan-only reconciler).
    pub feed: String,
}

impl OwnedLifecycleAuditRow {
    /// Borrow as the persistence-layer row for one `append` call.
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
            field_deltas: &self.field_deltas,
            source_csv_sha256: &self.source_csv_sha256,
            operator_note: &self.operator_note,
            lifecycle_state_after: self.lifecycle_state_after,
            lot_size_after: self.lot_size_after,
            tick_size_after: self.tick_size_after,
            expiry_date_after_nanos: self.expiry_date_after_nanos,
            symbol_name_after: &self.symbol_name_after,
            dry_run: self.dry_run,
            feed: &self.feed,
        }
    }
}

/// Owned mirror of [`InstrumentLifecycleRow`] (same rationale as the audit
/// row above).
#[derive(Debug, Clone, PartialEq)]
pub struct OwnedLifecycleRow {
    pub last_update_ts_nanos: i64,
    pub security_id: i64,
    pub exchange_segment: String,
    pub exchange_id: String,
    pub instrument_type: String,
    pub symbol_name: String,
    pub display_name: String,
    pub underlying_security_id: i64,
    pub underlying_symbol: String,
    pub lot_size: i32,
    pub tick_size: f64,
    pub expiry_date_nanos: i64,
    pub strike_price: f64,
    pub option_type: String,
    pub lifecycle_state: LifecycleState,
    pub lifecycle_state_locked: bool,
    pub first_seen_date_nanos: i64,
    pub last_seen_date_nanos: i64,
    pub last_active_date_nanos: i64,
    pub expired_date_nanos: i64,
    pub prev_symbol_chain: String,
    pub source_csv_sha256: String,
    pub dry_run: bool,
    /// Broker-source label (operator override 2026-06-28). Stamped `'dhan'`
    /// today (Dhan-only daily reconciler).
    pub feed: String,
}

impl OwnedLifecycleRow {
    /// Borrow as the persistence-layer row for one `append` call.
    #[must_use]
    pub fn as_row(&self) -> InstrumentLifecycleRow<'_> {
        InstrumentLifecycleRow {
            last_update_ts_nanos: self.last_update_ts_nanos,
            security_id: self.security_id,
            exchange_segment: &self.exchange_segment,
            exchange_id: &self.exchange_id,
            instrument_type: &self.instrument_type,
            symbol_name: &self.symbol_name,
            display_name: &self.display_name,
            underlying_security_id: self.underlying_security_id,
            underlying_symbol: &self.underlying_symbol,
            lot_size: self.lot_size,
            tick_size: self.tick_size,
            expiry_date_nanos: self.expiry_date_nanos,
            strike_price: self.strike_price,
            option_type: &self.option_type,
            lifecycle_state: self.lifecycle_state,
            lifecycle_state_locked: self.lifecycle_state_locked,
            first_seen_date_nanos: self.first_seen_date_nanos,
            last_seen_date_nanos: self.last_seen_date_nanos,
            last_active_date_nanos: self.last_active_date_nanos,
            expired_date_nanos: self.expired_date_nanos,
            prev_symbol_chain: &self.prev_symbol_chain,
            source_csv_sha256: &self.source_csv_sha256,
            dry_run: self.dry_run,
            feed: &self.feed,
        }
    }
}

/// The 8 Dhan derivative instrument types (FUT*/OPT*) — the ONLY rows that
/// carry a real expiry. Mirrors `DERIVATIVE_INSTRUMENT_PREFIXES` in
/// `crates/core/src/instrument/csv_parser.rs`; kept identical by
/// `test_malformed_expiry_derivative_type_list_is_the_canonical_eight`.
const DERIVATIVE_INSTRUMENT_TYPES: &[&str] = &[
    "FUTIDX", "OPTIDX", "FUTSTK", "OPTSTK", "FUTCUR", "OPTCUR", "FUTCOM", "OPTFUT",
];

#[must_use]
fn instrument_type_carries_expiry(instrument_type: &str) -> bool {
    DERIVATIVE_INSTRUMENT_TYPES
        .iter()
        .any(|t| instrument_type.eq_ignore_ascii_case(t))
}

/// A *derivative* row whose non-empty expiry string parsed to the `0`
/// sentinel is malformed: `expiry_date_to_ist_nanos` returns `0` for BOTH
/// "no expiry" and "unparseable", so we additionally require a non-empty
/// source string.
///
/// INDEX / EQUITY rows are NEVER flagged — they have no expiry by nature.
/// Dhan does not ship them with an empty field; it ships the literal
/// placeholder `0001-01-01`, which `NaiveDate` parses to year-1 midnight,
/// which is OUTSIDE the i64-nanos range (~1678–2262), so it collapses to the
/// same `0` sentinel as an unparseable string. The previous "non-empty + 0"
/// check therefore mis-flagged ~120 index rows EVERY boot, each emitting an
/// `error!("derivative expiry failed to parse")` that routes to Telegram —
/// pure pager noise, and categorically wrong (indices are not derivatives).
/// Gating on the instrument type fixes both: only the 8 FUT*/OPT* types can
/// be malformed.
#[must_use]
pub fn is_malformed_expiry(instrument_type: &str, raw_expiry: &str, parsed_nanos: i64) -> bool {
    instrument_type_carries_expiry(instrument_type)
        && !raw_expiry.trim().is_empty()
        && parsed_nanos == 0
}

/// Represent an `f64` as a JSON value WITHOUT panicking on non-finite
/// input. `serde_json` cannot encode `NaN`/`±Inf` as a number; a malformed
/// CSV could yield one (`parse::<f64>("inf")` succeeds). Falling back to
/// the string form keeps the forensic delta honest and panic-free
/// (the `json!` macro's expression arm would `.unwrap()` here).
#[must_use]
fn json_f64_safe(v: f64) -> serde_json::Value {
    serde_json::Number::from_f64(v).map_or_else(
        || serde_json::Value::String(v.to_string()),
        serde_json::Value::Number,
    )
}

/// Compact JSON of the fields that changed between the prior row and
/// today's detail — written into the audit row's `field_deltas` for an
/// `Updated` transition (§6). Empty string when nothing comparable
/// changed. Deterministic key order. Only the four classifier-relevant
/// mutable fields are compared. Panic-free on non-finite `tick_size`.
#[must_use]
pub fn build_field_deltas_json(prev: &PrevLifecycleAttrs, today: &TodayInstrument) -> String {
    let mut obj = serde_json::Map::new();
    if prev.lot_size != today.lot_size {
        obj.insert(
            "lot_size".to_string(),
            serde_json::Value::Array(vec![
                serde_json::Value::from(prev.lot_size),
                serde_json::Value::from(today.lot_size),
            ]),
        );
    }
    // A non-finite tick_size makes the `> EPSILON` test false (NaN) or
    // true (Inf); `json_f64_safe` handles both without panicking.
    if (prev.tick_size - today.tick_size).abs() > f64::EPSILON {
        obj.insert(
            "tick_size".to_string(),
            serde_json::Value::Array(vec![
                json_f64_safe(prev.tick_size),
                json_f64_safe(today.tick_size),
            ]),
        );
    }
    if prev.symbol_name != today.symbol_name {
        obj.insert(
            "symbol_name".to_string(),
            serde_json::Value::Array(vec![
                serde_json::Value::from(prev.symbol_name.as_str()),
                serde_json::Value::from(today.symbol_name.as_str()),
            ]),
        );
    }
    if prev.instrument_type != today.instrument_type {
        obj.insert(
            "instrument_type".to_string(),
            serde_json::Value::Array(vec![
                serde_json::Value::from(prev.instrument_type.as_str()),
                serde_json::Value::from(today.instrument_type.as_str()),
            ]),
        );
    }
    if obj.is_empty() {
        return String::new();
    }
    serde_json::Value::Object(obj).to_string()
}

/// Build the forensic audit row for one action. PURE. The `*_after`
/// snapshot uses the action's carried attrs (today's for a present
/// transition, the prior row's for a disappearance — see
/// [`ReconcileAction`]). `expiry_date_after_nanos` comes from today's
/// detail when present (UPSERT case) else `0`. `field_deltas` is populated
/// only for an `Updated` transition with both prior + today detail.
#[must_use]
// APPROVED: each arg is a distinct, named input to a pure builder.
#[allow(clippy::too_many_arguments)]
pub fn build_audit_row_for_action(
    action: &ReconcileAction,
    today_detail: Option<&TodayInstrument>,
    prev: Option<&PrevLifecycleAttrs>,
    now_ist_nanos: i64,
    today_ist_nanos: i64,
    source_csv_sha256: &str,
    dry_run: bool,
) -> OwnedLifecycleAuditRow {
    let (security_id, exchange_segment) = &action.key;
    let expiry_after = today_detail
        .map(|t| expiry_date_to_ist_nanos(&t.expiry_date))
        .unwrap_or(0);
    let field_deltas = match (action.transition_kind, prev, today_detail) {
        (TransitionKind::Updated, Some(p), Some(t)) => build_field_deltas_json(p, t),
        _ => String::new(),
    };
    OwnedLifecycleAuditRow {
        ts_nanos_ist: now_ist_nanos,
        trading_date_ist_nanos: today_ist_nanos,
        security_id: *security_id,
        exchange_segment: exchange_segment.clone(),
        from_state: action.from_state,
        to_state: action.to_state,
        transition_kind: action.transition_kind,
        field_deltas,
        source_csv_sha256: source_csv_sha256.to_string(),
        operator_note: String::new(), // automated reconcile — no operator note.
        lifecycle_state_after: action.to_state,
        lot_size_after: action.lot_size,
        tick_size_after: action.tick_size,
        expiry_date_after_nanos: expiry_after,
        symbol_name_after: action.symbol_name.clone(),
        dry_run,
        feed: LIFECYCLE_FEED_DHAN.to_string(),
    }
}

/// Build the full lifecycle row for a PRESENT (UPSERT) transition. PURE.
/// Returns the row + whether its expiry was malformed (non-empty but
/// parsed to 0). `first_seen_date` is preserved from the prior row via
/// [`resolve_first_seen_nanos`] (never reset to today — Rule 11). The
/// instrument is always written `Active` with `expired_date = 0` and
/// `lifecycle_state_locked = false`; `prev_symbol_chain` is left empty
/// (rename-chain maintenance is a separate concern, §23).
#[must_use]
// APPROVED: each arg is a distinct, named input to a pure builder.
#[allow(clippy::too_many_arguments)]
pub fn build_lifecycle_upsert_row(
    action: &ReconcileAction,
    today: &TodayInstrument,
    prev: &HashMap<LifecycleKey, PrevLifecycleAttrs>,
    now_ist_nanos: i64,
    today_ist_nanos: i64,
    source_csv_sha256: &str,
    dry_run: bool,
) -> (OwnedLifecycleRow, bool) {
    let expiry_nanos = expiry_date_to_ist_nanos(&today.expiry_date);
    let malformed = is_malformed_expiry(&today.instrument_type, &today.expiry_date, expiry_nanos);
    let first_seen = resolve_first_seen_nanos(&action.key, prev, today_ist_nanos);
    let row = OwnedLifecycleRow {
        last_update_ts_nanos: now_ist_nanos,
        security_id: today.security_id,
        exchange_segment: today.exchange_segment.clone(),
        exchange_id: today.exchange_id.clone(),
        instrument_type: today.instrument_type.clone(),
        symbol_name: today.symbol_name.clone(),
        display_name: today.display_name.clone(),
        underlying_security_id: today.underlying_security_id,
        underlying_symbol: today.underlying_symbol.clone(),
        lot_size: today.lot_size,
        tick_size: today.tick_size,
        expiry_date_nanos: expiry_nanos,
        strike_price: today.strike_price,
        option_type: today.option_type.clone(),
        lifecycle_state: action.to_state, // Active for a present transition.
        lifecycle_state_locked: false,
        first_seen_date_nanos: first_seen,
        last_seen_date_nanos: today_ist_nanos,
        last_active_date_nanos: today_ist_nanos,
        expired_date_nanos: 0,
        prev_symbol_chain: String::new(),
        source_csv_sha256: source_csv_sha256.to_string(),
        dry_run,
        feed: LIFECYCLE_FEED_DHAN.to_string(),
    };
    (row, malformed)
}

/// Walk the reconcile plan applying the §24 audit-first ordering per
/// action. Cold path — called once by the daily reconciler at boot
/// (~250 actions max). Errors are logged + counted, never fatal.
///
/// `today_detail` maps each present action's key to its full Dhan detail
/// (from [`crate::today_instrument::extract_today_instruments`]);
/// disappearance actions are absent from it (they UPDATE in place).
/// `prev` is the read-back map (for `first_seen` preservation +
/// `field_deltas`). `now_ist_nanos` stamps the audit `ts` + each row's
/// `last_update_ts`; `today_ist_nanos` is the IST-midnight business date
/// (`last_seen` / `last_active` / `expired_date`).
// APPROVED: each arg is a distinct, named input to a pure builder.
#[allow(clippy::too_many_arguments)]
pub async fn apply_reconcile_plan(
    questdb_config: &QuestDbConfig,
    plan: &[ReconcileAction],
    today_detail: &HashMap<LifecycleKey, TodayInstrument>,
    prev: &HashMap<LifecycleKey, PrevLifecycleAttrs>,
    now_ist_nanos: i64,
    today_ist_nanos: i64,
    source_csv_sha256: &str,
    dry_run: bool,
) -> ApplyOutcome {
    let mut outcome = ApplyOutcome::default();

    // ── Phase 1 — build ALL rows in memory (no I/O). PERF 2026-05-29: the
    // previous per-action loop did 2 HTTP round-trips PER ROW, each building
    // a fresh client — ~219K rows = ~438K round-trips = boot took minutes→
    // hours. We now collect owned audit + lifecycle rows + disappearances,
    // then write them in chunked multi-row INSERTs (a handful of requests).
    let mut owned_audits: Vec<OwnedLifecycleAuditRow> = Vec::with_capacity(plan.len());
    let mut owned_upserts: Vec<OwnedLifecycleRow> = Vec::new();
    // Disappearances are rare (a handful/day) → kept as per-row UPDATEs.
    let mut disappearances: Vec<(i64, String, LifecycleState)> = Vec::new();

    for action in plan {
        let (security_id, exchange_segment) = &action.key;
        let detail = today_detail.get(&action.key);
        let prev_attrs = prev.get(&action.key);

        // A present (UPSERT) transition needs today's detail. Resolve BEFORE
        // queuing the audit row so a missing-detail invariant violation skips
        // the WHOLE action (§24: no audit row without a fulfillable change).
        let upsert_detail = if is_upsert_transition(action.to_state) {
            match detail {
                Some(today) => Some(today),
                None => {
                    error!(
                        security_id,
                        exchange_segment = exchange_segment.as_str(),
                        transition_kind = action.transition_kind.as_str(),
                        "present transition has no today-detail row — skipping action (invariant)"
                    );
                    outcome.skipped_missing_detail += 1;
                    continue;
                }
            }
        } else {
            None
        };

        // Queue the audit row (written FIRST, as a batch, per §24).
        owned_audits.push(build_audit_row_for_action(
            action,
            detail,
            prev_attrs,
            now_ist_nanos,
            today_ist_nanos,
            source_csv_sha256,
            dry_run,
        ));

        if let Some(today) = upsert_detail {
            let (row, malformed) = build_lifecycle_upsert_row(
                action,
                today,
                prev,
                now_ist_nanos,
                today_ist_nanos,
                source_csv_sha256,
                dry_run,
            );
            if malformed {
                outcome.malformed_expiry += 1;
                metrics::counter!(MALFORMED_EXPIRY_COUNTER).increment(1);
                error!(
                    security_id,
                    exchange_segment = exchange_segment.as_str(),
                    raw_expiry = sanitize_audit_string(&today.expiry_date).as_str(),
                    "derivative expiry string failed to parse — wrote 0 (no-expiry) sentinel"
                );
            }
            owned_upserts.push(row);
        } else {
            disappearances.push((*security_id, exchange_segment.clone(), action.to_state));
        }
    }

    // ── Phase 2 — §24 audit-first: write ALL audit rows (batched) BEFORE any
    // lifecycle write. If the audit batch fails, skip the lifecycle batch so
    // we never have a lifecycle change without its forensic chain (next boot
    // re-runs — idempotent via DEDUP UPSERT KEYS).
    let audit_rows: Vec<InstrumentLifecycleAuditRow<'_>> = owned_audits
        .iter()
        .map(OwnedLifecycleAuditRow::as_row)
        .collect();
    if let Err(err) = append_instrument_lifecycle_audit_rows(questdb_config, &audit_rows).await {
        error!(
            ?err,
            audit_rows = audit_rows.len(),
            "lifecycle audit BATCH append failed — skipping lifecycle writes (next boot re-runs)"
        );
        outcome.errors += audit_rows.len();
        return finish_apply(outcome);
    }

    // ── Phase 3 — batched lifecycle UPSERT.
    let upsert_rows: Vec<InstrumentLifecycleRow<'_>> = owned_upserts
        .iter()
        .map(OwnedLifecycleRow::as_row)
        .collect();
    match append_instrument_lifecycle_rows(questdb_config, &upsert_rows).await {
        Ok(()) => outcome.upserted += upsert_rows.len(),
        Err(err) => {
            error!(
                ?err,
                upsert_rows = upsert_rows.len(),
                "lifecycle UPSERT BATCH failed (audit rows written; next boot re-runs)"
            );
            outcome.errors += upsert_rows.len();
        }
    }

    // ── Phase 4 — disappearance state-flips (rare → per-row UPDATE).
    for (security_id, exchange_segment, to_state) in &disappearances {
        match update_lifecycle_state(
            questdb_config,
            *security_id,
            exchange_segment,
            *to_state,
            today_ist_nanos,
            now_ist_nanos,
        )
        .await
        {
            Ok(()) => outcome.expired += 1,
            Err(err) => {
                error!(
                    security_id,
                    exchange_segment = exchange_segment.as_str(),
                    ?err,
                    "lifecycle state UPDATE failed (audit row written; next boot re-runs)"
                );
                outcome.errors += 1;
            }
        }
    }

    finish_apply(outcome)
}

/// Emit the completion log + return the outcome (shared by the normal path
/// and the audit-batch-failure early return).
fn finish_apply(outcome: ApplyOutcome) -> ApplyOutcome {
    info!(
        upserted = outcome.upserted,
        expired = outcome.expired,
        errors = outcome.errors,
        malformed_expiry = outcome.malformed_expiry,
        skipped_missing_detail = outcome.skipped_missing_detail,
        "reconcile apply complete"
    );
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_500_000_000_000_000;
    const TODAY: i64 = 1_700_438_400_000_000_000;

    fn prev_attrs(
        state: LifecycleState,
        lot: i32,
        tick: f64,
        sym: &str,
        itype: &str,
        first_seen: i64,
    ) -> PrevLifecycleAttrs {
        PrevLifecycleAttrs {
            state,
            locked: false,
            instrument_type: itype.to_string(),
            lot_size: lot,
            tick_size: tick,
            symbol_name: sym.to_string(),
            first_seen_date_nanos: first_seen,
        }
    }

    fn today_option() -> TodayInstrument {
        TodayInstrument {
            security_id: 43581,
            exchange_segment: "NSE_FNO".to_string(),
            exchange_id: "NSE".to_string(),
            instrument_type: "OPTSTK".to_string(),
            symbol_name: "TCS-CE".to_string(),
            underlying_security_id: 11536,
            underlying_symbol: "TCS".to_string(),
            display_name: "TCS 4000 CALL".to_string(),
            lot_size: 175,
            tick_size: 0.05,
            expiry_date: "2025-12-25".to_string(),
            strike_price: 4000.5,
            option_type: "CE".to_string(),
        }
    }

    fn appeared_action(key: LifecycleKey, lot: i32, tick: f64, sym: &str) -> ReconcileAction {
        ReconcileAction {
            key,
            from_state: None,
            to_state: LifecycleState::Active,
            transition_kind: TransitionKind::Appeared,
            instrument_type: "OPTSTK".to_string(),
            lot_size: lot,
            tick_size: tick,
            symbol_name: sym.to_string(),
        }
    }

    fn expired_action(key: LifecycleKey) -> ReconcileAction {
        ReconcileAction {
            from_state: Some(LifecycleState::Active),
            to_state: LifecycleState::ExpiredFromFno,
            transition_kind: TransitionKind::Expired,
            instrument_type: "EQUITY".to_string(),
            lot_size: 250,
            tick_size: 0.05,
            symbol_name: "TCS".to_string(),
            key,
        }
    }

    // ---- is_malformed_expiry ----

    #[test]
    fn test_is_malformed_expiry_nonempty_zero_is_malformed() {
        // A derivative whose expiry string is non-empty yet parsed to 0.
        assert!(is_malformed_expiry("OPTSTK", "not-a-date", 0));
        assert!(is_malformed_expiry("FUTIDX", "  2025-13-99  ", 0));
    }

    #[test]
    fn test_is_malformed_expiry_empty_is_not_malformed() {
        assert!(
            !is_malformed_expiry("OPTSTK", "", 0),
            "even a derivative with an empty expiry string is not 'malformed'"
        );
        assert!(!is_malformed_expiry("FUTSTK", "   ", 0));
    }

    #[test]
    fn test_is_malformed_expiry_valid_parse_is_not_malformed() {
        assert!(!is_malformed_expiry(
            "OPTSTK",
            "2025-12-25",
            1_735_084_800_000_000_000
        ));
    }

    /// Regression (2026-05-29): the live boot log emitted ~120
    /// `error!("derivative expiry failed to parse")` lines for INDEX rows.
    /// Dhan ships index/equity rows with the placeholder `0001-01-01`, which
    /// parses to year-1 (outside i64-nanos) → the `0` sentinel. Non-derivative
    /// rows must NEVER be flagged malformed regardless of their expiry string.
    #[test]
    fn test_is_malformed_expiry_index_0001_sentinel_is_not_malformed() {
        assert!(
            !is_malformed_expiry("INDEX", "0001-01-01", 0),
            "INDEX rows carry the 0001-01-01 no-expiry placeholder — not malformed"
        );
        assert!(
            !is_malformed_expiry("EQUITY", "0001-01-01", 0),
            "EQUITY rows have no expiry — not malformed"
        );
        // Even a genuinely garbage string on a non-derivative is not flagged:
        // the error is categorically about *derivative* expiry.
        assert!(!is_malformed_expiry("INDEX", "garbage", 0));
    }

    /// A derivative that somehow carries the 0001-01-01 sentinel (a real data
    /// problem — derivatives MUST have an expiry) IS still flagged.
    #[test]
    fn test_is_malformed_expiry_derivative_with_sentinel_is_malformed() {
        assert!(is_malformed_expiry("OPTSTK", "0001-01-01", 0));
    }

    /// The derivative-type list must stay identical to the canonical 8 prefixes
    /// in `crates/core/src/instrument/csv_parser.rs::DERIVATIVE_INSTRUMENT_PREFIXES`.
    /// If Dhan adds a derivative class, update BOTH lists together.
    #[test]
    fn test_malformed_expiry_derivative_type_list_is_the_canonical_eight() {
        let mut got = DERIVATIVE_INSTRUMENT_TYPES.to_vec();
        got.sort_unstable();
        let mut want = vec![
            "FUTIDX", "OPTIDX", "FUTSTK", "OPTSTK", "FUTCUR", "OPTCUR", "FUTCOM", "OPTFUT",
        ];
        want.sort_unstable();
        assert_eq!(
            got, want,
            "derivative type list drifted from the canonical 8"
        );
    }

    #[test]
    fn test_instrument_type_carries_expiry_is_case_insensitive() {
        assert!(instrument_type_carries_expiry("optstk"));
        assert!(instrument_type_carries_expiry("OPTSTK"));
        assert!(!instrument_type_carries_expiry("index"));
        assert!(!instrument_type_carries_expiry("INDEX"));
        assert!(!instrument_type_carries_expiry("EQUITY"));
    }

    // ---- build_field_deltas_json ----

    #[test]
    fn test_build_field_deltas_json_reports_changed_fields() {
        let prev = prev_attrs(LifecycleState::Active, 100, 0.05, "OLD", "EQUITY", TODAY);
        let mut today = today_option();
        today.lot_size = 175;
        today.symbol_name = "NEW".to_string();
        today.instrument_type = "EQUITY".to_string(); // keep type equal
        let json = build_field_deltas_json(&prev, &today);
        assert!(json.contains("\"lot_size\":[100,175]"), "got {json}");
        assert!(
            json.contains("\"symbol_name\":[\"OLD\",\"NEW\"]"),
            "got {json}"
        );
        assert!(!json.contains("instrument_type"), "type unchanged: {json}");
    }

    #[test]
    fn test_build_field_deltas_json_empty_when_unchanged() {
        let today = today_option();
        let prev = prev_attrs(
            LifecycleState::Active,
            today.lot_size,
            today.tick_size,
            &today.symbol_name,
            &today.instrument_type,
            TODAY,
        );
        assert_eq!(build_field_deltas_json(&prev, &today), "");
    }

    #[test]
    fn test_build_field_deltas_json_non_finite_tick_does_not_panic() {
        // A malformed CSV could parse tick_size to inf/NaN. Must not panic
        // (serde_json can't encode non-finite as a number) — fall back to
        // the string form so the forensic delta is still recorded.
        let prev = prev_attrs(LifecycleState::Active, 1, 0.05, "X", "EQUITY", TODAY);
        let mut today = today_option();
        today.lot_size = 1;
        today.symbol_name = "X".to_string();
        today.instrument_type = "EQUITY".to_string();
        today.tick_size = f64::INFINITY;
        let json = build_field_deltas_json(&prev, &today);
        assert!(
            json.contains("tick_size"),
            "inf tick delta recorded: {json}"
        );
        assert!(
            json.contains("inf"),
            "non-finite rendered as string: {json}"
        );

        // NaN prev: the `> EPSILON` test is false → no tick delta, no panic.
        let nan_prev = prev_attrs(LifecycleState::Active, 1, f64::NAN, "X", "EQUITY", TODAY);
        let mut nan_today = today_option();
        nan_today.lot_size = 1;
        nan_today.symbol_name = "X".to_string();
        nan_today.instrument_type = "EQUITY".to_string();
        nan_today.tick_size = 0.05;
        let _ = build_field_deltas_json(&nan_prev, &nan_today); // must not panic
    }

    #[test]
    fn test_build_field_deltas_json_escapes_quotes() {
        let prev = prev_attrs(LifecycleState::Active, 1, 0.05, "A\"B", "EQUITY", TODAY);
        let mut today = today_option();
        today.symbol_name = "C".to_string();
        today.lot_size = 1;
        today.tick_size = 0.05;
        today.instrument_type = "EQUITY".to_string();
        let json = build_field_deltas_json(&prev, &today);
        // serde_json escapes the embedded quote → valid JSON, no SQL break.
        assert!(json.contains("A\\\"B"), "got {json}");
    }

    // ---- build_audit_row_for_action ----

    #[test]
    fn test_build_audit_row_for_action_appeared_present() {
        let today = today_option();
        let action = appeared_action((43581, "NSE_FNO".to_string()), 175, 0.05, "TCS-CE");
        let audit =
            build_audit_row_for_action(&action, Some(&today), None, NOW, TODAY, "sha123", false);
        assert_eq!(audit.ts_nanos_ist, NOW);
        assert_eq!(audit.trading_date_ist_nanos, TODAY);
        assert_eq!(audit.security_id, 43581);
        assert_eq!(audit.exchange_segment, "NSE_FNO");
        assert_eq!(audit.from_state, None);
        assert_eq!(audit.to_state, LifecycleState::Active);
        assert_eq!(audit.transition_kind, TransitionKind::Appeared);
        assert_eq!(audit.lifecycle_state_after, LifecycleState::Active);
        assert_eq!(audit.lot_size_after, 175);
        assert_eq!(audit.symbol_name_after, "TCS-CE");
        assert!(audit.expiry_date_after_nanos > 0, "option has expiry");
        assert_eq!(audit.field_deltas, "", "appeared → no deltas");
        assert_eq!(audit.source_csv_sha256, "sha123");
        assert!(!audit.dry_run);
    }

    #[test]
    fn test_build_audit_row_updated_carries_field_deltas() {
        let today = today_option();
        let prev = prev_attrs(LifecycleState::Active, 100, 0.05, "TCS-CE", "OPTSTK", TODAY);
        let action = ReconcileAction {
            key: (43581, "NSE_FNO".to_string()),
            from_state: Some(LifecycleState::Active),
            to_state: LifecycleState::Active,
            transition_kind: TransitionKind::Updated,
            instrument_type: "OPTSTK".to_string(),
            lot_size: 175,
            tick_size: 0.05,
            symbol_name: "TCS-CE".to_string(),
        };
        let audit = build_audit_row_for_action(
            &action,
            Some(&today),
            Some(&prev),
            NOW,
            TODAY,
            "sha",
            false,
        );
        assert!(
            audit.field_deltas.contains("lot_size"),
            "updated lot_size 100→175 must appear: {}",
            audit.field_deltas
        );
    }

    #[test]
    fn test_build_audit_row_disappearance_no_expiry_after() {
        // Disappearance has no today-detail → expiry_after = 0, no deltas.
        let action = expired_action((99887, "NSE_EQ".to_string()));
        let prev = prev_attrs(LifecycleState::Active, 250, 0.05, "TCS", "EQUITY", TODAY);
        let audit = build_audit_row_for_action(&action, None, Some(&prev), NOW, TODAY, "sha", true);
        assert_eq!(audit.transition_kind, TransitionKind::Expired);
        assert_eq!(audit.to_state, LifecycleState::ExpiredFromFno);
        assert_eq!(audit.expiry_date_after_nanos, 0);
        assert_eq!(audit.field_deltas, "");
        assert_eq!(audit.lot_size_after, 250, "carries prior attrs");
        assert!(audit.dry_run, "dry_run flag flows through");
    }

    #[test]
    fn test_owned_rows_as_row_borrow_roundtrip() {
        // The owned mirrors must hand back persistence rows whose borrowed
        // fields equal the owned source (no field transposition).
        let audit = build_audit_row_for_action(
            &appeared_action((43581, "NSE_FNO".to_string()), 175, 0.05, "TCS-CE"),
            Some(&today_option()),
            None,
            NOW,
            TODAY,
            "sha",
            false,
        );
        let ar = audit.as_row();
        assert_eq!(ar.security_id, audit.security_id);
        assert_eq!(ar.exchange_segment, audit.exchange_segment);
        assert_eq!(ar.transition_kind, audit.transition_kind);
        assert_eq!(ar.symbol_name_after, audit.symbol_name_after);

        let (life, _) = build_lifecycle_upsert_row(
            &appeared_action((43581, "NSE_FNO".to_string()), 175, 0.05, "TCS-CE"),
            &today_option(),
            &HashMap::new(),
            NOW,
            TODAY,
            "sha",
            false,
        );
        let lr = life.as_row();
        assert_eq!(lr.security_id, life.security_id);
        assert_eq!(lr.underlying_symbol, life.underlying_symbol);
        assert_eq!(lr.option_type, life.option_type);
        assert_eq!(lr.lifecycle_state, life.lifecycle_state);
    }

    // ---- build_lifecycle_upsert_row ----

    #[test]
    fn test_build_lifecycle_upsert_row_full_detail() {
        let today = today_option();
        let action = appeared_action((43581, "NSE_FNO".to_string()), 175, 0.05, "TCS-CE");
        let prev = HashMap::new();
        let (row, malformed) =
            build_lifecycle_upsert_row(&action, &today, &prev, NOW, TODAY, "sha", false);
        assert!(!malformed, "valid expiry");
        assert_eq!(row.security_id, 43581);
        assert_eq!(row.exchange_id, "NSE");
        assert_eq!(row.underlying_security_id, 11536);
        assert_eq!(row.underlying_symbol, "TCS");
        assert_eq!(row.display_name, "TCS 4000 CALL");
        assert_eq!(row.lot_size, 175);
        assert!((row.strike_price - 4000.5).abs() < f64::EPSILON);
        assert_eq!(row.option_type, "CE");
        assert_eq!(row.lifecycle_state, LifecycleState::Active);
        assert!(!row.lifecycle_state_locked);
        assert!(row.expiry_date_nanos > 0);
        assert_eq!(row.expired_date_nanos, 0, "active row not expired");
        assert_eq!(row.last_seen_date_nanos, TODAY);
        assert_eq!(row.last_active_date_nanos, TODAY);
        assert_eq!(row.last_update_ts_nanos, NOW);
        assert_eq!(row.prev_symbol_chain, "");
        // Brand new (empty prev) → first_seen = today.
        assert_eq!(row.first_seen_date_nanos, TODAY);
    }

    #[test]
    fn test_build_lifecycle_upsert_row_preserves_prior_first_seen() {
        let today = today_option();
        let action = appeared_action((43581, "NSE_FNO".to_string()), 175, 0.05, "TCS-CE");
        let mut prev = HashMap::new();
        prev.insert(
            (43581, "NSE_FNO".to_string()),
            prev_attrs(LifecycleState::Active, 175, 0.05, "TCS-CE", "OPTSTK", 999),
        );
        let (row, _) = build_lifecycle_upsert_row(&action, &today, &prev, NOW, TODAY, "sha", false);
        assert_eq!(
            row.first_seen_date_nanos, 999,
            "prior first_seen preserved, NOT reset to today"
        );
    }

    #[test]
    fn test_build_lifecycle_upsert_row_flags_malformed_expiry() {
        let mut today = today_option();
        today.expiry_date = "not-a-date".to_string();
        let action = appeared_action((43581, "NSE_FNO".to_string()), 175, 0.05, "TCS-CE");
        let prev = HashMap::new();
        let (row, malformed) =
            build_lifecycle_upsert_row(&action, &today, &prev, NOW, TODAY, "sha", false);
        assert!(malformed, "non-empty unparseable expiry flagged");
        assert_eq!(row.expiry_date_nanos, 0, "wrote the 0 sentinel");
    }

    // ---- apply_reconcile_plan (async, transport-error path) ----

    fn unreachable_questdb() -> QuestDbConfig {
        // Connection-refused on a closed loopback port → fast Err, no
        // real QuestDB needed. Exercises the audit-first error path.
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    #[tokio::test]
    async fn test_apply_reconcile_plan_all_audit_appends_fail_counts_errors() {
        let cfg = unreachable_questdb();
        let plan = vec![
            appeared_action((43581, "NSE_FNO".to_string()), 175, 0.05, "TCS-CE"),
            expired_action((99887, "NSE_EQ".to_string())),
        ];
        let mut detail = HashMap::new();
        detail.insert((43581, "NSE_FNO".to_string()), today_option());
        let prev = HashMap::new();
        let outcome =
            apply_reconcile_plan(&cfg, &plan, &detail, &prev, NOW, TODAY, "sha", false).await;
        // Audit-first append fails for both → neither lifecycle write runs.
        assert_eq!(outcome.errors, 2);
        assert_eq!(outcome.upserted, 0);
        assert_eq!(outcome.expired, 0);
        assert_eq!(outcome.skipped_missing_detail, 0);
    }

    #[tokio::test]
    async fn test_apply_reconcile_plan_present_missing_detail_skips_before_audit() {
        // A present (Active) transition with NO today-detail row must be
        // skipped ENTIRELY — counted as skipped_missing_detail, NOT errors,
        // and crucially WITHOUT writing an orphan audit row (the skip
        // happens before any network call, so the unreachable host is
        // never touched for this action).
        let cfg = unreachable_questdb();
        let plan = vec![appeared_action(
            (43581, "NSE_FNO".to_string()),
            175,
            0.05,
            "TCS-CE",
        )];
        let detail = HashMap::new(); // deliberately missing the key
        let prev = HashMap::new();
        let outcome =
            apply_reconcile_plan(&cfg, &plan, &detail, &prev, NOW, TODAY, "sha", false).await;
        assert_eq!(outcome.skipped_missing_detail, 1);
        assert_eq!(outcome.errors, 0, "skip happens before the audit write");
        assert_eq!(outcome.upserted, 0);
        assert_eq!(outcome.expired, 0);
    }

    #[tokio::test]
    async fn test_apply_reconcile_plan_empty_plan_is_all_zero() {
        let cfg = unreachable_questdb();
        let detail = HashMap::new();
        let prev = HashMap::new();
        let outcome =
            apply_reconcile_plan(&cfg, &[], &detail, &prev, NOW, TODAY, "sha", false).await;
        assert_eq!(outcome, ApplyOutcome::default());
    }
}
