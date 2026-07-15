//! C1 convergence ratchet — there is EXACTLY ONE function that builds the
//! `ticks` ILP row, shared by BOTH live feeds (Dhan + Groww).
//!
//! Before C1 the Dhan writer (`tick_persistence::build_tick_row_seq`) and the
//! (formerly also the Groww live-tick writer, retired 2026-07-15) each
//! hand-wrote a near-duplicate `ticks` ILP row. C1 converged that into the one
//! `tick_row_builder::build_tick_row_for_feed`. This source-scan guard fails the
//! build if a second hand-written `ticks` row-build path is re-introduced — the
//! mechanical proof that the two feeds can never silently diverge on the table
//! name, the DEDUP key columns, the ILP SYMBOL-before-column ordering, or the
//! NULL-not-0 policy.
//!
//! Mirrors the `GrowwCandle1mWriter`-deleted-style source guards already in the
//! repo: it scans the storage crate's two tick-writer source files and asserts
//! that the `.table(... "ticks" ...)` ILP entry point appears in EXACTLY ONE
//! place — the shared builder.

use std::fs;
use std::path::Path;

/// Read a storage `src` file by name. Panics with a clear message if missing so
/// a future rename surfaces loudly rather than silently passing.
fn read_src(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Strip the `#[cfg(test)] mod tests { ... }` tail so the scan only sees the
/// PRODUCTION region (test bodies legitimately mention these patterns in
/// assertions on the buffer bytes).
fn production_region(src: &str) -> &str {
    src.split("mod tests {")
        .next()
        .expect("file always has a head before any tests module")
}

#[test]
fn exactly_one_ticks_row_builder_function_exists() {
    // The shared builder is THE one ILP row-build entry point for the `ticks`
    // table. It is the only production site allowed to open an ILP row whose
    // table is the `ticks` table constant.
    let shared = read_src("tick_row_builder.rs");
    let shared_prod = production_region(&shared);
    let shared_table_opens = shared_prod
        .matches("TableName::new_unchecked(QUESTDB_TABLE_TICKS)")
        .count();
    assert_eq!(
        shared_table_opens, 1,
        "expected exactly ONE `.table(TableName::new_unchecked(QUESTDB_TABLE_TICKS))` in the \
         shared builder, found {shared_table_opens}"
    );

    // The Dhan writer MUST delegate — its production region must NOT contain a
    // raw `.table(TableName::new_unchecked(QUESTDB_TABLE_TICKS))` ILP open of
    // its own anymore (that moved into the shared builder).
    let dhan = read_src("tick_persistence.rs");
    let dhan_prod = production_region(&dhan);
    let dhan_table_opens = dhan_prod
        .matches("TableName::new_unchecked(QUESTDB_TABLE_TICKS)")
        .count();
    assert_eq!(
        dhan_table_opens, 0,
        "Dhan tick_persistence.rs must NOT hand-build a `ticks` ILP row — it must call the \
         shared `build_tick_row_for_feed`; found {dhan_table_opens} raw table-opens"
    );
    assert!(
        dhan_prod.contains("build_tick_row_for_feed"),
        "Dhan writer must delegate to the shared `build_tick_row_for_feed`"
    );

    // The Groww live-tick writer section was RETIRED 2026-07-15 with the
    // Groww live feed (the writer was deleted; groww_persistence.rs retains
    // only the shared reconnect-ladder primitives). The shared-builder +
    // Dhan pins above remain the ratchet.
}
