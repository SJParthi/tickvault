//! Concern-C regression ratchet — `close_pct_from_prev_day` MUST stay wired
//! end-to-end across the candle persistence chain.
//!
//! Background (active-plan.md "PR-4 HOTFIX", Concern C): the Engine-B candle
//! rewrite SILENTLY DROPPED the `*_pct_from_prev_day` columns from the candle
//! tables. The operator expected visible % changes; they were gone. PR-4b
//! (#860) re-added `close_pct_from_prev_day` (operator decision 2026-05-28:
//! only `close_pct` is persisted — spot instruments have no OI and indices
//! have no volume, so `oi_pct` / `volume_pct` stay dropped).
//!
//! That regression slipped past CI because NO source-scan / unit test pinned
//! the column to the DDL or the ILP write — the only tests were inside the
//! pure pct-computation module. This file is the missing ratchet: it scans
//! the storage `src/` and fails the build if `close_pct_from_prev_day` is
//! removed from ANY of the four links in the persist chain:
//!
//!   1. CREATE TABLE DDL          — `shadow_persistence.rs`
//!   2. ALTER self-heal           — `shadow_persistence.rs`
//!   3. ILP append column write   — `shadow_candle_writer.rs`
//!   4. seal row struct field     — `shadow_seal_columns.rs`
//!   5. spill record (zero-loss)  — `seal_spill.rs`
//!
//! See: `.claude/rules/project/observability-architecture.md` (schema
//! self-heal at boot) and `.claude/rules/project/wave-6-error-codes.md`
//! (AGGREGATOR-SEAL-01 — seal-time ILP write).

#![cfg(test)]

use std::path::PathBuf;

const PCT_COLUMN: &str = "close_pct_from_prev_day";

fn storage_src(file: &str) -> (PathBuf, String) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    let content = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "candle pct-column guard: cannot read {} — has the persist chain \
             been renamed/moved? Update this guard alongside the rename. ({e})",
            path.display()
        )
    });
    (path, content)
}

/// Strip line comments + doc comments so the guard pins the LIVE code, not a
/// comment that merely mentions the column name. Keeps the test honest: a
/// future refactor that leaves the column only in a `//` comment must fail.
fn code_only(content: &str) -> String {
    content
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("//!") && !t.starts_with("///")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ============================================================================
// 1. CREATE TABLE DDL must declare the column as DOUBLE
// ============================================================================

#[test]
fn ddl_create_table_declares_close_pct_double_column() {
    let (path, content) = storage_src("shadow_persistence.rs");
    let code = code_only(&content);
    assert!(
        code.contains(PCT_COLUMN) && code.contains("DOUBLE"),
        "{}: candle CREATE TABLE DDL must declare `{PCT_COLUMN} DOUBLE`. \
         Concern-C regression: the Engine-B rewrite dropped the pct columns \
         once already. Re-add it to the `create_ddl` format string.",
        path.display()
    );
    // Pin the exact column-in-DDL token so a stray mention elsewhere can't
    // satisfy the check above.
    assert!(
        code.contains(&format!("{PCT_COLUMN}     DOUBLE"))
            || code.contains(&format!("{PCT_COLUMN} DOUBLE")),
        "{}: `{PCT_COLUMN}` must appear as a `DOUBLE` column in the candle \
         CREATE TABLE body.",
        path.display()
    );
}

// ============================================================================
// 2. ALTER ADD COLUMN IF NOT EXISTS self-heal must cover the column
// ============================================================================

#[test]
fn ddl_self_heal_alter_adds_close_pct_column() {
    let (path, content) = storage_src("shadow_persistence.rs");
    let code = code_only(&content);
    assert!(
        code.contains("ALTER TABLE")
            && code.contains("ADD COLUMN IF NOT EXISTS")
            && code.contains(PCT_COLUMN),
        "{}: candle tables created under the pre-2026-05-28 10-column schema \
         must auto-migrate via `ALTER TABLE {{table}} ADD COLUMN IF NOT EXISTS \
         {PCT_COLUMN} DOUBLE;`. Without this, an upgraded deployment keeps the \
         old schema and the % column stays empty forever (schema self-heal — \
         observability-architecture.md).",
        path.display()
    );
}

// ============================================================================
// 3. ILP append_seal must actually WRITE the column to QuestDB
// ============================================================================

#[test]
fn ilp_append_seal_writes_close_pct_column() {
    let (path, content) = storage_src("shadow_candle_writer.rs");
    let code = code_only(&content);
    assert!(
        code.contains(&format!(".column_f64(\"{PCT_COLUMN}\"")),
        "{}: the ILP `append_seal` builder must emit \
         `.column_f64(\"{PCT_COLUMN}\", row.{PCT_COLUMN})`. A DDL column that \
         is never written stays NULL — the operator sees no % change. This is \
         the second half of the Concern-C contract (DDL + write).",
        path.display()
    );
}

// ============================================================================
// 4. The seal row struct must carry the field (DDL ↔ struct ↔ write parity)
// ============================================================================

#[test]
fn seal_row_struct_carries_close_pct_field() {
    let (path, content) = storage_src("shadow_seal_columns.rs");
    let code = code_only(&content);
    assert!(
        code.contains(&format!("pub {PCT_COLUMN}: f64")),
        "{}: the persisted seal-row struct must carry `pub {PCT_COLUMN}: f64` \
         so the seal-time computed value reaches the ILP writer unchanged.",
        path.display()
    );
}

// ============================================================================
// 5. Spill record must preserve the field (zero-loss recovery contract)
// ============================================================================

#[test]
fn spill_record_preserves_close_pct_field() {
    let (path, content) = storage_src("seal_spill.rs");
    let code = code_only(&content);
    assert!(
        code.contains(PCT_COLUMN),
        "{}: the on-disk spill record must carry `{PCT_COLUMN}` so a seal that \
         overflows the ring → spill → DLQ still re-persists its % change on \
         replay. Operator charter: zero loss inside the rescue envelope.",
        path.display()
    );
}

// ============================================================================
// Self-tests for the helper (so the guard itself can't silently no-op)
// ============================================================================

// ============================================================================
// 6. Operator request 2026-06-02 — `change_pct` + `open_gap_pct` columns must
//    stay wired end-to-end across the SAME persist chain (DDL + ALTER + ILP
//    write + seal-row struct + spill record). `change_pct` is DERIVED from
//    `close_pct_from_prev_day` at the seal-row extractor (no LiveCandleState
//    field), so its struct/spill presence is the row/serialized-seal field.
// ============================================================================

/// Assert a derived/stamped pct column is wired across DDL, ALTER, ILP write,
/// seal-row struct and spill record.
fn assert_pct_column_wired_end_to_end(col: &str) {
    let (sp_path, sp) = storage_src("shadow_persistence.rs");
    let sp = code_only(&sp);
    assert!(
        sp.contains(&format!("{col}                  DOUBLE"))
            || sp.contains(&format!("{col}                DOUBLE"))
            || sp.contains(&format!("{col} DOUBLE")),
        "{}: candle CREATE TABLE DDL must declare `{col} DOUBLE` (operator \
         request 2026-06-02).",
        sp_path.display()
    );
    assert!(
        sp.contains(&format!("ADD COLUMN IF NOT EXISTS {col} DOUBLE")),
        "{}: candle schema self-heal must `ALTER TABLE {{table}} ADD COLUMN IF \
         NOT EXISTS {col} DOUBLE;` so upgraded deployments backfill the column.",
        sp_path.display()
    );

    let (w_path, w) = storage_src("shadow_candle_writer.rs");
    let w = code_only(&w);
    assert!(
        w.contains(&format!(".column_f64(\"{col}\"")),
        "{}: the ILP `append_seal` builder must emit `.column_f64(\"{col}\", \
         row.{col})` — a DDL column never written stays NULL.",
        w_path.display()
    );

    let (r_path, r) = storage_src("shadow_seal_columns.rs");
    let r = code_only(&r);
    assert!(
        r.contains(&format!("pub {col}: f64")),
        "{}: the persisted seal-row struct must carry `pub {col}: f64`.",
        r_path.display()
    );

    let (s_path, s) = storage_src("seal_spill.rs");
    let s = code_only(&s);
    assert!(
        s.contains(&format!("pub {col}: f64")),
        "{}: the on-disk spill record must carry `pub {col}: f64` so an \
         overflowed seal still re-persists the % on replay (zero-loss).",
        s_path.display()
    );
}

#[test]
fn change_pct_column_wired_end_to_end() {
    assert_pct_column_wired_end_to_end("change_pct");
}

#[test]
fn open_gap_pct_column_wired_end_to_end() {
    assert_pct_column_wired_end_to_end("open_gap_pct");
}

#[test]
fn self_test_code_only_strips_comments_keeps_code() {
    let sample = "// close_pct_from_prev_day in a comment\nlet x = close_pct_from_prev_day;";
    let stripped = code_only(sample);
    assert!(!stripped.contains("in a comment"));
    assert!(stripped.contains("let x = close_pct_from_prev_day;"));
}

// ============================================================================
// 7. Net volume + the book totals — the SAME five-link chain.
//
//    These are LONG columns, not DOUBLE, and `net_volume` is the one column in
//    the whole candle schema that is deliberately OMITTED rather than
//    zero-filled when absent: omitting an ILP column persists NULL, and NULL is
//    the honest value for "this process did not classify this bar's flow" — a
//    disk-spill replay, a REST-folded bar, or a bar with no ticks or no volume.
//    Writing `0` would claim perfectly balanced flow about a bar nobody
//    measured, and would draw a FLAT bar on a chart. That distinction is the
//    thing most likely to be "tidied away" by a future refactor, so it is
//    pinned by name here.
//
//    ⚠ CORRECTED 2026-09-10: the NULL reason above read "there was no previous
//    bar to compare against", which belonged to the retired close-vs-close
//    definition. `net_volume` is now tick-rule signed order flow, and a
//    first-of-day bar with ticks reports a real value rather than NULL.
// ============================================================================

/// Assert a LONG candle column is wired across DDL, ALTER self-heal, ILP
/// write, seal-row struct and the console view.
fn assert_long_column_wired_end_to_end(col: &str, row_type: &str) {
    let (sp_path, sp_raw) = storage_src("shadow_persistence.rs");
    let sp = code_only(&sp_raw);
    assert!(
        sp.contains(&format!("{col}                  LONG"))
            || sp.contains(&format!("{col}               LONG"))
            || sp.contains(&format!("{col}              LONG"))
            || sp.contains(&format!("{col} LONG")),
        "{}: candle CREATE TABLE DDL must declare `{col} LONG`.",
        sp_path.display()
    );
    assert!(
        sp.contains(&format!("ADD COLUMN IF NOT EXISTS {col} LONG")),
        "{}: candle tables created before {col} existed must auto-migrate via \
         `ALTER TABLE {{table}} ADD COLUMN IF NOT EXISTS {col} LONG;`. Without \
         it an upgraded deployment keeps the old schema and the column stays \
         empty forever (schema self-heal).",
        sp_path.display()
    );

    let (w_path, w_raw) = storage_src("shadow_candle_writer.rs");
    let w = code_only(&w_raw);
    assert!(
        w.contains(&format!(".column_i64(\"{col}\"")),
        "{}: the ILP append builder must emit `.column_i64(\"{col}\", ...)` — \
         a DDL column never written stays NULL forever.",
        w_path.display()
    );

    let (r_path, r_raw) = storage_src("shadow_seal_columns.rs");
    let r = code_only(&r_raw);
    assert!(
        r.contains(&format!("pub {col}: {row_type}")),
        "{}: the persisted seal-row struct must carry `pub {col}: {row_type}`.",
        r_path.display()
    );

    let (v_path, v_raw) = storage_src("console_views.rs");
    let v = code_only(&v_raw);
    assert!(
        v.contains(&format!("c.{col}")),
        "{}: the `candles_named` analyst view must project `c.{col}` — a \
         stored column an operator cannot see in the console is a column that \
         does not exist as far as they are concerned.",
        v_path.display()
    );
}

#[test]
fn net_volume_column_wired_end_to_end() {
    // `Option<i64>`, not `i64`: the Option IS the NULL, and collapsing it to a
    // bare i64 is exactly the regression this pins.
    assert_long_column_wired_end_to_end("net_volume", "Option<i64>");
}

#[test]
fn total_buy_qty_column_wired_end_to_end() {
    assert_long_column_wired_end_to_end("total_buy_qty", "i64");
}

#[test]
fn total_sell_qty_column_wired_end_to_end() {
    assert_long_column_wired_end_to_end("total_sell_qty", "i64");
}

#[test]
fn net_volume_is_written_conditionally_so_absent_persists_as_null() {
    // The behavioural half the presence checks above cannot see: the write
    // must sit behind an `if let Some(...)`. An unconditional
    // `.column_i64("net_volume", row.net_volume.unwrap_or(0))` would satisfy
    // every other assertion in this file while silently turning "no previous
    // bar" into "the price did not move" on every first bar of every session,
    // for all 24 timeframes, forever.
    let (path, raw) = storage_src("shadow_candle_writer.rs");
    let code = code_only(&raw);
    assert!(
        code.contains("if let Some(net_volume) = row.net_volume"),
        "{}: `net_volume` must be written only when present, so an absent \
         baseline persists as NULL rather than as a fabricated flat bar. \
         Restore the `if let Some(net_volume) = row.net_volume` guard.",
        path.display()
    );
    assert!(
        !code.contains("row.net_volume.unwrap_or(0)"),
        "{}: never zero-fill `net_volume`. `0` means the price did not move; \
         NULL means there was no previous bar. Collapsing them makes the \
         day's first bar indistinguishable from a flat one.",
        path.display()
    );
}
