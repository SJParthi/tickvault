//! App-side daily lifecycle reconcile orchestrator — chains the merged
//! helpers into ONE async entry point the boot path calls.
//!
//! Given today's already-built [`DailyUniverse`] (the `core`
//! `build_universe_from_bytes` output) + the QuestDB config + the IST
//! timestamps, this runs the full reconcile:
//!
//! 1. [`extract_today_instruments`] — DailyUniverse → full-detail
//!    `Vec<TodayInstrument>`.
//! 2. [`build_today_maps`] — project the detail map (key → detail, for the
//!    apply UPSERT) + the classification attrs vec (for the planner).
//! 3. [`load_prev_lifecycle_at_boot`] — read yesterday's rows
//!    (`WHERE dry_run = false`). **Fail-closed:** a read ERROR aborts the
//!    reconcile (proceeding with an empty prior map would mis-classify
//!    every existing instrument as `Appeared` and skip the expiry pass).
//!    An EMPTY table is a successful read → Day-1 bootstrap (all
//!    `Appeared`, no expiries).
//! 4. [`compute_reconcile_plan`] — pure two-pass diff → `Vec<ReconcileAction>`.
//! 5. [`apply_reconcile_plan`] — §24 audit-first → UPSERT/UPDATE per action.
//!
//! Single responsibility: this module does NOT do HTTP fetch, retry/
//! backoff, cache persistence, `instrument_fetch_audit` writes, Telegram
//! emission, or the `main.rs` task spawn — those live in the OUTER boot
//! orchestrator (the final wiring PR). Keeping the I/O surface to just the
//! two QuestDB calls (`load_prev` + `apply`) makes the assembly logic
//! ([`build_today_maps`]) unit-testable and the async path exercisable via
//! the fail-closed (unreachable-host) leg.
//!
//! **Feature-gated** under `daily_universe_fetcher` per rule §21 —
//! dormant in the default build.

#![cfg(feature = "daily_universe_fetcher")]

use std::collections::HashMap;

use anyhow::{Context, Result};
use tickvault_common::config::QuestDbConfig;
use tickvault_core::instrument::daily_universe::DailyUniverse;
use tickvault_storage::instrument_fetch_audit_persistence::read_last_fetch_audit_sha;
use tickvault_storage::instrument_lifecycle_persistence::bump_active_last_seen;
use tracing::{info, warn};

use crate::apply_reconcile_plan::{ApplyOutcome, apply_reconcile_plan};
use crate::lifecycle_cache_loader::{
    LifecycleKey, LifecycleParseResult, load_prev_lifecycle_at_boot,
};
use crate::lifecycle_reconcile_plan::{TodayInstrumentAttrs, compute_reconcile_plan};
use crate::today_instrument::{TodayInstrument, extract_today_instruments};

/// Tally of one full reconcile run — surfaced to the boot orchestrator for
/// the Telegram summary + the `instrument_fetch_audit` outcome row.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconcileRunOutcome {
    /// Instruments in today's validated universe.
    pub universe_size: usize,
    /// Prior `(security_id, exchange_segment)` rows read back.
    pub prev_rows_loaded: usize,
    /// Read-back rows whose `lifecycle_state` SYMBOL was unknown (schema
    /// drift) — counted, not guessed.
    pub skipped_unknown_state: u32,
    /// Read-back rows with the wrong shape / non-coercible cells.
    pub skipped_malformed: u32,
    /// State transitions the planner emitted.
    pub plan_size: usize,
    /// Per-action apply tally.
    pub apply: ApplyOutcome,
    /// `true` when the O(1) warm-boot fast path ran: today's CSV SHA-256
    /// matched the last boot's, so the universe is unchanged and the full
    /// re-UPSERT was SKIPPED in favour of a single `last_seen` bump.
    pub warm_skipped: bool,
}

/// Project today's full-detail instruments into the two structures the
/// reconcile chain needs: the classification attrs vec (planner input,
/// ordered like the universe) and the detail map (apply UPSERT input,
/// keyed by `(security_id, exchange_segment)`). PURE.
///
/// On a duplicate composite key (which the validated universe forbids per
/// §26) the detail map keeps the LAST occurrence; the attrs vec keeps both
/// so [`compute_reconcile_plan`]'s `debug_assert` still surfaces the
/// contract violation in test/debug.
#[must_use]
pub fn build_today_maps(
    today: &[TodayInstrument],
) -> (
    Vec<TodayInstrumentAttrs>,
    HashMap<LifecycleKey, TodayInstrument>,
) {
    let mut attrs = Vec::with_capacity(today.len());
    let mut detail = HashMap::with_capacity(today.len());
    for ti in today {
        attrs.push(ti.to_classification_attrs());
        detail.insert((ti.security_id, ti.exchange_segment.clone()), ti.clone());
    }
    (attrs, detail)
}

/// Fail-closed guard on a PARTIAL prior read. PURE. A skipped prior row
/// (schema-drift `lifecycle_state` SYMBOL, or a malformed/wrong-shape row)
/// is UNKNOWN prior state — the same hazard class as a read error.
/// Reconciling against the surviving subset would re-classify the dropped
/// instrument as `Appeared` if it's still in today's CSV, which resets its
/// SEBI `first_seen_date` to today (Rule 11 violation) and downgrades a
/// would-be `Reactivated` to `Appeared`. Refuse rather than silently
/// corrupt the forensic chain; the operator investigates the drift and the
/// idempotent next boot re-runs on a clean read.
///
/// # Errors
///
/// Returns `Err` when any prior row was skipped during read-back.
pub fn ensure_prev_readback_complete(prev: &LifecycleParseResult) -> Result<()> {
    if prev.skipped_unknown_state > 0 || prev.skipped_malformed > 0 {
        anyhow::bail!(
            "lifecycle read-back skipped {} unknown-state + {} malformed prior rows — \
             refusing to reconcile against a partial prior state (would corrupt first_seen / \
             reactivation classification)",
            prev.skipped_unknown_state,
            prev.skipped_malformed,
        );
    }
    Ok(())
}

/// Run the full daily lifecycle reconcile against today's universe. Cold
/// path — called once at boot. See module docs for the 5-step chain +
/// fail-closed semantics.
///
/// # Errors
///
/// Returns `Err` when the prior-state read-back fails (QuestDB
/// unavailable) OR when any prior row was skipped as malformed /
/// unknown-state (see [`ensure_prev_readback_complete`]) — both abort the
/// reconcile rather than apply against an unknown / partial prior state.
/// Per-action apply failures are NOT errors here: they are logged +
/// counted inside [`apply_reconcile_plan`] and reflected in
/// `ReconcileRunOutcome::apply`, so **the caller MUST inspect
/// `outcome.apply.errors`** to decide whether to retry (the idempotent
/// next boot re-runs them).
// APPROVED: each arg is a distinct, named input to the boot orchestrator.
#[allow(clippy::too_many_arguments)]
pub async fn run_lifecycle_reconcile(
    questdb_config: &QuestDbConfig,
    universe: &DailyUniverse,
    now_ist_nanos: i64,
    today_ist_nanos: i64,
    source_csv_sha256: &str,
    dry_run: bool,
) -> Result<ReconcileRunOutcome> {
    let today = extract_today_instruments(universe);
    let (today_attrs, today_detail) = build_today_maps(&today);

    // Fail-closed: an Err here is QuestDB unavailability, NOT an empty
    // table. Reconciling against an empty-because-failed prior map would
    // mis-expire nothing and re-`Appeared` everything.
    let prev = load_prev_lifecycle_at_boot(questdb_config).await.context(
        "lifecycle read-back failed — refusing to reconcile against unknown prior state",
    )?;

    // Fail-closed on a PARTIAL prior read (see `ensure_prev_readback_complete`).
    if let Err(err) = ensure_prev_readback_complete(&prev) {
        warn!(
            skipped_unknown_state = prev.skipped_unknown_state,
            skipped_malformed = prev.skipped_malformed,
            "lifecycle read-back skipped rows — aborting reconcile (fail-closed)"
        );
        return Err(err);
    }

    // ── O(1) WARM-BOOT fast path ──────────────────────────────────────────
    // If the lifecycle table is non-empty AND today's CSV SHA-256 matches the
    // last boot's, the universe is byte-identical and already persisted — the
    // full re-UPSERT (up to ~219K rows) is pure waste. Do ONE `last_seen` bump
    // instead: O(1) HTTP requests regardless of universe size. Fail-safe: any
    // read error OR empty table OR SHA mismatch falls through to the FULL
    // reconcile (never skip on an unknown/changed prior state).
    if !prev.rows.is_empty()
        && let Ok(Some(last_sha)) = read_last_fetch_audit_sha(questdb_config).await
        && last_sha == source_csv_sha256
    {
        bump_active_last_seen(questdb_config, today_ist_nanos, now_ist_nanos)
            .await
            .context("O(1) warm-boot last_seen bump failed")?;
        info!(
            universe_size = today.len(),
            prev_rows_loaded = prev.rows.len(),
            sha = %source_csv_sha256,
            "daily lifecycle reconcile WARM-SKIP — CSV SHA unchanged, O(1) last_seen bump (full re-UPSERT skipped)"
        );
        return Ok(ReconcileRunOutcome {
            universe_size: today.len(),
            prev_rows_loaded: prev.rows.len(),
            skipped_unknown_state: prev.skipped_unknown_state,
            skipped_malformed: prev.skipped_malformed,
            plan_size: 0,
            apply: ApplyOutcome::default(),
            warm_skipped: true,
        });
    }

    let plan = compute_reconcile_plan(&today_attrs, &prev.rows);
    let apply = apply_reconcile_plan(
        questdb_config,
        &plan,
        &today_detail,
        &prev.rows,
        now_ist_nanos,
        today_ist_nanos,
        source_csv_sha256,
        dry_run,
    )
    .await;

    let outcome = ReconcileRunOutcome {
        universe_size: today.len(),
        prev_rows_loaded: prev.rows.len(),
        skipped_unknown_state: prev.skipped_unknown_state,
        skipped_malformed: prev.skipped_malformed,
        plan_size: plan.len(),
        apply,
        warm_skipped: false,
    };
    info!(
        universe_size = outcome.universe_size,
        prev_rows_loaded = outcome.prev_rows_loaded,
        plan_size = outcome.plan_size,
        upserted = outcome.apply.upserted,
        expired = outcome.apply.expired,
        errors = outcome.apply.errors,
        dry_run,
        "daily lifecycle reconcile complete"
    );
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_core::instrument::csv_parser::CsvRow;
    use tickvault_core::instrument::daily_universe::{InstrumentRole, SubscriptionTarget};

    fn target(
        role: InstrumentRole,
        sid: &str,
        seg: &str,
        itype: &str,
        sym: &str,
    ) -> SubscriptionTarget {
        SubscriptionTarget {
            role,
            is_fno_underlying: role == InstrumentRole::FnoUnderlying,
            is_index_constituent: role == InstrumentRole::IndexConstituent,
            csv_row: CsvRow {
                security_id: sid.to_string(),
                exch_id: "NSE".to_string(),
                segment: seg.to_string(),
                instrument: itype.to_string(),
                symbol_name: sym.to_string(),
                ..Default::default()
            },
        }
    }

    fn universe() -> DailyUniverse {
        DailyUniverse {
            subscription_targets: vec![
                target(InstrumentRole::Index, "13", "IDX_I", "INDEX", "NIFTY"),
                target(
                    InstrumentRole::FnoUnderlying,
                    "11536",
                    "NSE_EQ",
                    "EQUITY",
                    "TCS",
                ),
            ],
            fno_contracts: vec![],
        }
    }

    fn cfg_unreachable() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 1,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    #[test]
    fn test_build_today_maps_projects_attrs_and_detail() {
        let today = extract_today_instruments(&universe());
        let (attrs, detail) = build_today_maps(&today);
        assert_eq!(attrs.len(), 2, "one attrs row per instrument, ordered");
        assert_eq!(attrs[0].security_id, 13);
        assert_eq!(attrs[1].security_id, 11536);
        assert_eq!(detail.len(), 2, "detail keyed by (sid, segment)");
        assert!(detail.contains_key(&(13, "IDX_I".to_string())));
        let tcs = detail
            .get(&(11536, "NSE_EQ".to_string()))
            .expect("TCS detail present");
        assert_eq!(tcs.symbol_name, "TCS");
    }

    #[test]
    fn test_build_today_maps_empty_universe() {
        let (attrs, detail) = build_today_maps(&[]);
        assert!(attrs.is_empty());
        assert!(detail.is_empty());
    }

    #[test]
    fn test_ensure_prev_readback_complete_ok_when_clean() {
        let clean = LifecycleParseResult::default(); // no skips, empty rows
        assert!(ensure_prev_readback_complete(&clean).is_ok());
    }

    #[test]
    fn test_ensure_prev_readback_complete_errors_on_unknown_state() {
        let mut r = LifecycleParseResult::default();
        r.skipped_unknown_state = 1;
        assert!(
            ensure_prev_readback_complete(&r).is_err(),
            "a schema-drift state SYMBOL must fail-closed"
        );
    }

    #[test]
    fn test_ensure_prev_readback_complete_errors_on_malformed() {
        let mut r = LifecycleParseResult::default();
        r.skipped_malformed = 3;
        assert!(
            ensure_prev_readback_complete(&r).is_err(),
            "a malformed prior row must fail-closed"
        );
    }

    #[tokio::test]
    async fn test_run_lifecycle_reconcile_fails_closed_when_readback_unavailable() {
        // QuestDB unreachable → load_prev errors → the reconcile is
        // ABORTED (Err), never applied against an unknown prior state.
        let cfg = cfg_unreachable();
        let result = run_lifecycle_reconcile(&cfg, &universe(), 1, 1, "sha", false).await;
        assert!(
            result.is_err(),
            "read-back failure must abort the reconcile (fail-closed)"
        );
    }

    #[tokio::test]
    async fn test_run_lifecycle_reconcile_empty_universe_fails_closed_on_readback() {
        // Even with an empty universe, the read-back is attempted first;
        // an unreachable QuestDB still fails closed.
        let cfg = cfg_unreachable();
        let empty = DailyUniverse {
            subscription_targets: vec![],
            fno_contracts: vec![],
        };
        let result = run_lifecycle_reconcile(&cfg, &empty, 1, 1, "sha", false).await;
        assert!(result.is_err());
    }
}
