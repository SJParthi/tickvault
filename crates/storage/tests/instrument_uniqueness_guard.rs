//! I-P1-08 Phase 2 — Single-row-per-instrument mechanical guard.
//!
//! The lifecycle tables (`fno_underlyings`, `derivative_contracts`,
//! `subscribed_indices`) must hold exactly ONE row per business key
//! regardless of how many times the app is started or how many calendar days
//! it runs across. This file hosts:
//!
//! 1. Static DDL + DEDUP assertions — read the runtime constants in
//!    `instrument_persistence.rs` and verify the schema invariants.
//! 2. A property-style simulation — model a caller writing the same universe
//!    N times on M days and prove the in-memory simulation produces exactly
//!    `|universe|` distinct rows every time.
//!
//! The simulation is intentionally pure Rust (no QuestDB) — it encodes the
//! DEDUP semantics we depend on so that a regression in the write logic
//! (e.g. accidentally writing the designated timestamp as `today_ist_nanos`
//! again) trips this guard before it ships.

#![cfg(test)]

use std::collections::HashMap;

use tickvault_storage::instrument_persistence::{
    INSTRUMENT_STATUS_ACTIVE, INSTRUMENT_STATUS_EXPIRED,
};

// ---------------------------------------------------------------------------
// Constants mirror — must match instrument_persistence.rs exactly.
// ---------------------------------------------------------------------------

const EXPECTED_STATUS_ACTIVE: &str = "active";
const EXPECTED_STATUS_EXPIRED: &str = "expired";

/// The three lifecycle tables. `instrument_build_metadata` is NOT a lifecycle
/// table — it accumulates one row per daily build by design.
const LIFECYCLE_TABLES: &[&str] = &[
    "fno_underlyings",
    "derivative_contracts",
    "subscribed_indices",
];

// ---------------------------------------------------------------------------
// Status constant guards
// ---------------------------------------------------------------------------

#[test]
fn status_constants_must_match_expected_strings() {
    // Mechanical: if someone renames INSTRUMENT_STATUS_ACTIVE in code without
    // updating Grafana dashboards / SQL UPDATE statements, this test fires.
    assert_eq!(
        INSTRUMENT_STATUS_ACTIVE, EXPECTED_STATUS_ACTIVE,
        "INSTRUMENT_STATUS_ACTIVE must remain 'active' — dashboards and \
         mark_missing_as_expired() hard-code the literal."
    );
    assert_eq!(
        INSTRUMENT_STATUS_EXPIRED, EXPECTED_STATUS_EXPIRED,
        "INSTRUMENT_STATUS_EXPIRED must remain 'expired' — Dhan support and \
         SEBI audit filters hard-code the literal."
    );
}

// ---------------------------------------------------------------------------
// Source-file DDL guards — read the Rust source and assert the lifecycle
// columns exist. Prevents someone from deleting `status` or `last_seen_date`
// from a DDL in a sneaky refactor.
// ---------------------------------------------------------------------------

fn instrument_persistence_source() -> String {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = manifest_dir.join("src/instrument_persistence.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "failed to read instrument_persistence.rs at {}: {err}",
            path.display()
        )
    })
}

#[test]
fn ddl_strings_must_contain_status_column_for_every_lifecycle_table() {
    let src = instrument_persistence_source();
    for &table in LIFECYCLE_TABLES {
        let marker = format!("CREATE TABLE IF NOT EXISTS {table}");
        let idx = src
            .find(&marker)
            .unwrap_or_else(|| panic!("DDL for `{table}` not found in source"));
        // Window ~1200 chars should cover any reasonable CREATE TABLE body.
        let end = idx.saturating_add(1200).min(src.len());
        let body = &src[idx..end];
        assert!(
            body.contains("status SYMBOL"),
            "DDL for `{table}` must contain `status SYMBOL` column \
             (I-P1-08 Phase 2 lifecycle)",
        );
        assert!(
            body.contains("last_seen_date TIMESTAMP"),
            "DDL for `{table}` must contain `last_seen_date TIMESTAMP` column",
        );
        assert!(
            body.contains("expired_date TIMESTAMP"),
            "DDL for `{table}` must contain `expired_date TIMESTAMP` column",
        );
    }
}

#[test]
fn mark_missing_as_expired_update_targets_all_three_tables() {
    let src = instrument_persistence_source();
    let fn_start = src
        .find("async fn mark_missing_as_expired")
        .expect("mark_missing_as_expired fn must exist");
    // Grab a generous window — the function is small.
    let end = fn_start.saturating_add(4000).min(src.len());
    let body = &src[fn_start..end];
    // The loop iterates over constants (QUESTDB_TABLE_*); the literal table
    // names live in `constants.rs`. Accept either form — the constant name
    // maps 1:1 to the table name so this is still a mechanical check.
    let expected = [
        ("fno_underlyings", "QUESTDB_TABLE_FNO_UNDERLYINGS"),
        ("derivative_contracts", "QUESTDB_TABLE_DERIVATIVE_CONTRACTS"),
        ("subscribed_indices", "QUESTDB_TABLE_SUBSCRIBED_INDICES"),
    ];
    for (table, constant) in expected {
        assert!(
            body.contains(table) || body.contains(constant),
            "mark_missing_as_expired() must reference table `{table}` (literal \
             or via `{constant}` constant) in its UPDATE loop",
        );
    }
    assert!(
        body.contains("status = '"),
        "mark_missing_as_expired() must emit `status = 'expired'` UPDATE",
    );
    assert!(
        body.contains("last_seen_date <"),
        "mark_missing_as_expired() must predicate on `last_seen_date < today`",
    );
}

#[test]
fn build_snapshot_timestamp_must_stay_epoch_zero() {
    let src = instrument_persistence_source();
    let idx = src
        .find("fn build_snapshot_timestamp")
        .expect("build_snapshot_timestamp must exist");
    let end = idx.saturating_add(600).min(src.len());
    let body = &src[idx..end];
    assert!(
        body.contains("TimestampNanos::new(0)"),
        "build_snapshot_timestamp() MUST return TimestampNanos::new(0). \
         If this is changed, DEDUP stops collapsing onto the business key and \
         the tables will accumulate one row per day per instrument again."
    );
}

#[test]
fn no_delete_from_lifecycle_tables_in_source() {
    // SEBI: expired rows must never be physically removed.
    let src = instrument_persistence_source();
    for &table in LIFECYCLE_TABLES {
        let forbidden = format!("DELETE FROM {table}");
        assert!(
            !src.contains(&forbidden),
            "`{forbidden}` must not appear in instrument_persistence.rs — \
             lifecycle tables retain expired rows for SEBI 5-year audit.",
        );
    }
}

// ---------------------------------------------------------------------------
// Property-style simulation — models QuestDB DEDUP UPSERT behaviour.
//
// Invariant: after any sequence of `write_universe(today, universe)` +
// `mark_missing_as_expired(today)` calls, the table has exactly
// `|ever_seen_business_keys|` rows, and the row count of `status = active`
// equals `|today_universe|`.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Row {
    status: &'static str,
    last_seen: u32,  // day number, 1-based
    expired_on: u32, // 0 = null
}

#[derive(Default)]
struct SimulatedTable {
    rows: HashMap<String, Row>,
}

impl SimulatedTable {
    fn write_universe(&mut self, today: u32, universe: &[&'static str]) {
        // ILP UPSERT — every row gets status=active, last_seen=today, expired=0
        for &key in universe {
            self.rows.insert(
                key.to_owned(),
                Row {
                    status: EXPECTED_STATUS_ACTIVE,
                    last_seen: today,
                    expired_on: 0,
                },
            );
        }
    }

    fn mark_missing_as_expired(&mut self, today: u32) {
        for row in self.rows.values_mut() {
            if row.status == EXPECTED_STATUS_ACTIVE && row.last_seen < today {
                row.status = EXPECTED_STATUS_EXPIRED;
                row.expired_on = today;
            }
        }
    }

    fn count_total(&self) -> usize {
        self.rows.len()
    }

    fn count_active(&self) -> usize {
        self.rows
            .values()
            .filter(|r| r.status == EXPECTED_STATUS_ACTIVE)
            .count()
    }

    fn count_expired(&self) -> usize {
        self.rows
            .values()
            .filter(|r| r.status == EXPECTED_STATUS_EXPIRED)
            .count()
    }
}

#[test]
fn same_day_n_writes_produce_exactly_universe_count_rows() {
    let universe = ["NIFTY", "BANKNIFTY", "RELIANCE", "TCS"];
    let mut table = SimulatedTable::default();
    for _ in 0..1000 {
        table.write_universe(1, &universe);
        table.mark_missing_as_expired(1);
    }
    assert_eq!(table.count_total(), universe.len());
    assert_eq!(table.count_active(), universe.len());
    assert_eq!(table.count_expired(), 0);
}

#[test]
fn cross_day_same_universe_never_grows() {
    let universe = ["NIFTY", "BANKNIFTY", "RELIANCE", "TCS"];
    let mut table = SimulatedTable::default();
    for day in 1..=30 {
        table.write_universe(day, &universe);
        table.mark_missing_as_expired(day);
    }
    assert_eq!(table.count_total(), universe.len());
    assert_eq!(table.count_active(), universe.len());
    assert_eq!(table.count_expired(), 0);
}

#[test]
fn expired_contract_flips_to_expired_on_day_of_absence() {
    let mut table = SimulatedTable::default();
    table.write_universe(1, &["NIFTY", "BANKNIFTY"]);
    table.mark_missing_as_expired(1);
    assert_eq!(table.count_active(), 2);

    // Day 2: BANKNIFTY disappears.
    table.write_universe(2, &["NIFTY"]);
    table.mark_missing_as_expired(2);
    assert_eq!(table.count_total(), 2, "expired rows must be retained");
    assert_eq!(table.count_active(), 1);
    assert_eq!(table.count_expired(), 1);
    let banknifty = table.rows.get("BANKNIFTY").expect("row retained");
    assert_eq!(banknifty.status, EXPECTED_STATUS_EXPIRED);
    assert_eq!(banknifty.expired_on, 2);
}

#[test]
fn reappearing_contract_reactivates_and_clears_expired_date() {
    let mut table = SimulatedTable::default();
    table.write_universe(1, &["NIFTY", "BANKNIFTY"]);
    table.mark_missing_as_expired(1);

    // Day 2: BANKNIFTY gone.
    table.write_universe(2, &["NIFTY"]);
    table.mark_missing_as_expired(2);
    assert_eq!(table.count_expired(), 1);

    // Day 3: BANKNIFTY reappears (e.g. corporate action resumed trading).
    table.write_universe(3, &["NIFTY", "BANKNIFTY"]);
    table.mark_missing_as_expired(3);
    let banknifty = table.rows.get("BANKNIFTY").expect("still one row");
    assert_eq!(banknifty.status, EXPECTED_STATUS_ACTIVE);
    assert_eq!(banknifty.expired_on, 0);
    assert_eq!(banknifty.last_seen, 3);
    assert_eq!(table.count_total(), 2);
}

#[test]
fn new_contract_added_on_later_day_shows_as_active() {
    let mut table = SimulatedTable::default();
    table.write_universe(1, &["NIFTY"]);
    table.mark_missing_as_expired(1);

    table.write_universe(2, &["NIFTY", "SENSEX"]);
    table.mark_missing_as_expired(2);
    assert_eq!(table.count_total(), 2);
    assert_eq!(table.count_active(), 2);
    let sensex = table.rows.get("SENSEX").expect("newly added row");
    assert_eq!(sensex.status, EXPECTED_STATUS_ACTIVE);
    assert_eq!(sensex.last_seen, 2);
}

#[test]
fn thirty_day_mixed_churn_preserves_unique_rows_invariant() {
    // Simulate 30 days with shifting universes. Check invariants after each day.
    // All keys ever seen must live forever (no DELETE). count_total monotonic.
    let base = ["NIFTY", "BANKNIFTY", "FINNIFTY", "MIDCPNIFTY"];
    let mut table = SimulatedTable::default();
    let mut prev_total = 0usize;
    for day in 1..=30u32 {
        // Drop one stock every 3 days, add a new one every 5 days.
        let mut universe: Vec<&'static str> = base.to_vec();
        if day % 3 == 0 {
            universe.pop();
        }
        // Inject a new synthetic stock identified by the day number via static strs.
        if day % 5 == 0 {
            let key: &'static str = match day {
                5 => "NEW_5",
                10 => "NEW_10",
                15 => "NEW_15",
                20 => "NEW_20",
                25 => "NEW_25",
                30 => "NEW_30",
                _ => unreachable!(),
            };
            universe.push(key);
        }

        // Run the same-day write loop 3× to test same-day idempotency.
        for _ in 0..3 {
            table.write_universe(day, &universe);
            table.mark_missing_as_expired(day);
        }

        assert!(
            table.count_total() >= prev_total,
            "row count must never decrease (day {day}: prev={prev_total}, now={})",
            table.count_total()
        );
        assert_eq!(
            table.count_active(),
            universe.len(),
            "count(active) must equal |today_universe| on day {day}"
        );
        prev_total = table.count_total();
    }
    // After 30 days, no row has been deleted.
    assert!(table.count_total() >= base.len());
}
