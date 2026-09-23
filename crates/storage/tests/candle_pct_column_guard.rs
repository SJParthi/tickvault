//! Candle percentage-column ratchet — the wire names, the struct fields, and
//! the four columns the 2026-09-19 schema reset REMOVED.
//!
//! ## History (why this file exists at all)
//!
//! The Engine-B candle rewrite SILENTLY DROPPED the `*_pct_from_prev_day`
//! columns from the candle tables: the operator expected visible % changes and
//! they were simply gone. It slipped past CI because NO source scan pinned a
//! column to the DDL or to the ILP write — the only tests lived inside the pure
//! pct-computation module, which kept passing while nothing reached QuestDB.
//! This file is that missing ratchet.
//!
//! ## What it pins TODAY (rewritten 2026-09-19, the operator's schema reset)
//!
//! The reset renamed two columns and removed two, so this guard now pins BOTH
//! directions — a column that must be wired, and a column that must NOT come
//! back:
//!
//! | wire column | source field | direction |
//! |---|---|---|
//! | `percentage_change` | `change_pct` | must be WIRED |
//! | `open_percentage_change` | `open_pct` | must be WIRED |
//! | `close_pct_from_prev_day` | — | must be ABSENT from the wire |
//! | `open_gap_pct` | — | must be ABSENT from the wire |
//!
//! **The wire name and the struct field differ**, which is the whole reason the
//! helper below takes a PAIR. `shadow_seal_columns::from_buffered_seal` fills
//! `change_pct` and `close_pct_from_prev_day` from the SAME
//! `state.close_pct_from_prev_day`, so the two were byte-identical duplicates
//! under two names; the reset keeps one wire column and leaves the struct field
//! names alone.
//!
//! **Why the removed columns keep their STRUCT fields.** `seal_spill.rs` is a
//! fixed-offset binary record: deleting a field moves every byte range after it
//! and breaks replay of records already on disk. So the reset removes the four
//! names from the WIRE (DDL + self-heal + ILP append) and leaves the in-memory
//! and on-disk field layout untouched. A removal test that also demanded the
//! struct field disappear would be demanding a spill-format break.
//!
//! **Why removal needs its own test.** QuestDB's schema self-heal can ADD a
//! column but can never DROP or RENAME one, and ILP auto-creates any column a
//! writer names. So a single leftover `.column_f64("open_gap_pct", ..)` — or a
//! single stale entry in the self-heal manifest — silently re-creates a removed
//! column on the next boot, and the reset is undone with nothing failing.
//!
//! The five links this file scans, for every column in both directions:
//!
//!   1. CREATE TABLE DDL          — `shadow_persistence.rs`
//!   2. ALTER self-heal manifest  — `shadow_persistence.rs`
//!   3. ILP append column write   — `shadow_candle_writer.rs`
//!   4. seal row struct field     — `shadow_seal_columns.rs`
//!   5. spill record (zero-loss)  — `seal_spill.rs`
//!
//! See: `.claude/rules/project/observability-architecture.md` (schema self-heal
//! at boot) and `.claude/rules/project/wave-6-error-codes.md`
//! (AGGREGATOR-SEAL-01 — seal-time ILP write).

#![cfg(test)]

use std::path::PathBuf;

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
/// comment that merely mentions the column name. Keeps the test honest in BOTH
/// directions: a refactor that leaves a wired column only in a `//` comment
/// must fail, and a removal test must not be defeated by the dated note that
/// RECORDS the removal (those notes name every removed column by design).
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

/// Collapse every run of whitespace to one space.
///
/// The CREATE TABLE body aligns its types into a column, so the gap between a
/// name and its type is whatever the longest name in the table happens to be.
/// Matching a literal run of spaces makes the guard fail on a purely cosmetic
/// realignment — and, worse, makes it PASS vacuously if someone "fixes" that
/// failure by loosening the pattern instead of normalizing.
fn squeeze(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut in_ws = false;
    for ch in content.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

/// `needle` occurs in `haystack` as a whole TOKEN: an identifier character at
/// either EDGE of the needle may not be continued by one in the haystack.
///
/// 2026-09-22: every check here used a bare `contains`, and the 2026-09-19
/// rename made that vacuous in the worst possible place. `percentage_change`
/// is a SUFFIX of `open_percentage_change`, so `contains("percentage_change
/// DOUBLE")` was satisfied by the OPEN column alone — deleting the
/// `percentage_change` DDL line kept the wiring test green. The same shape
/// applies to `c.total_buy_qty` inside `c.total_buy_qty_x`, and in the removal
/// direction to a DIFFERENT column that merely ends in a removed name.
///
/// Edges that are not identifier characters (`(`, `"`, `.`) carry their own
/// boundary, so they are not checked — which keeps every existing quoted
/// needle behaving exactly as before.
fn contains_token(haystack: &str, needle: &str) -> bool {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let check_left = needle.chars().next().is_some_and(is_ident);
    let check_right = needle.chars().next_back().is_some_and(is_ident);
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(needle) {
        let at = from + rel;
        let end = at + needle.len();
        let left_ok = !check_left || !haystack[..at].chars().next_back().is_some_and(is_ident);
        let right_ok = !check_right || !haystack[end..].chars().next().is_some_and(is_ident);
        if left_ok && right_ok {
            return true;
        }
        from = at + 1;
    }
    false
}

// ============================================================================
// 1. Columns that must be WIRED — all five links, wire name vs struct field
// ============================================================================

/// Assert a pct column is wired across DDL, self-heal, ILP write, seal-row
/// struct and spill record.
///
/// `wire` is the QuestDB column name; `field` is the Rust struct field that
/// feeds it. Since 2026-09-19 they DIFFER, so a single-name assertion would
/// either pin the DDL and miss the struct, or pin the struct and pass while the
/// DDL declares a column nothing writes.
fn assert_pct_column_wired_end_to_end(wire: &str, field: &str) {
    let (sp_path, sp) = storage_src("shadow_persistence.rs");
    let sp = squeeze(&code_only(&sp));
    assert!(
        contains_token(&sp, &format!("{wire} DOUBLE")),
        "{}: candle CREATE TABLE DDL must declare `{wire} DOUBLE`.",
        sp_path.display()
    );
    assert!(
        contains_token(&sp, &format!("(\"{wire}\", \"DOUBLE\")")),
        "{}: the candle schema self-heal manifest must carry \
         `(\"{wire}\", \"DOUBLE\")` so a table created before this column \
         existed backfills it via `ALTER TABLE .. ADD COLUMN IF NOT EXISTS`. \
         Without it an upgraded deployment keeps the old schema and the column \
         stays empty forever.",
        sp_path.display()
    );

    let (w_path, w) = storage_src("shadow_candle_writer.rs");
    let w = code_only(&w);
    assert!(
        contains_token(&w, &format!(".column_f64(\"{wire}\", row.{field})")),
        "{}: the ILP `append_seal` builder must emit \
         `.column_f64(\"{wire}\", row.{field})` — a DDL column never written \
         stays NULL, and a wire name fed from the wrong field is worse than an \
         empty one.",
        w_path.display()
    );

    let (r_path, r) = storage_src("shadow_seal_columns.rs");
    let r = code_only(&r);
    assert!(
        contains_token(&r, &format!("pub {field}: f64")),
        "{}: the persisted seal-row struct must carry `pub {field}: f64` — it \
         is the source of the `{wire}` column.",
        r_path.display()
    );

    let (s_path, s) = storage_src("seal_spill.rs");
    let s = code_only(&s);
    assert!(
        contains_token(&s, &format!("pub {field}: f64")),
        "{}: the on-disk spill record must carry `pub {field}: f64` so a seal \
         that overflows the ring -> spill -> DLQ still re-persists `{wire}` on \
         replay (zero-loss inside the rescue envelope).",
        s_path.display()
    );
}

#[test]
fn percentage_change_column_wired_end_to_end() {
    assert_pct_column_wired_end_to_end("percentage_change", "change_pct");
}

#[test]
fn open_percentage_change_column_wired_end_to_end() {
    assert_pct_column_wired_end_to_end("open_percentage_change", "open_pct");
}

// ============================================================================
// 2. Columns that must stay REMOVED from the wire (2026-09-19 schema reset)
// ============================================================================

/// Assert a removed column is gone from all three WIRE links, and say why each
/// one on its own would undo the reset.
///
/// Deliberately does NOT assert the struct/spill field is gone: `seal_spill.rs`
/// is a fixed-offset binary record, so dropping a field shifts every byte range
/// after it and breaks replay of records already written. The reset is a WIRE
/// change.
fn assert_column_removed_from_the_wire(wire: &str) {
    let (sp_path, sp) = storage_src("shadow_persistence.rs");
    let sp_code = squeeze(&code_only(&sp));
    assert!(
        !contains_token(&sp_code, &format!("{wire} DOUBLE")),
        "{}: `{wire}` was REMOVED from the candle schema on 2026-09-19 and \
         must not reappear in the CREATE TABLE body.",
        sp_path.display()
    );
    assert!(
        !contains_token(&sp_code, &format!("(\"{wire}\"")),
        "{}: `{wire}` must NEVER appear in the candle self-heal manifest. \
         QuestDB can ADD a column but can never DROP or RENAME one, so a stale \
         manifest entry re-adds on the next boot exactly what the reset took \
         out — and nothing fails.",
        sp_path.display()
    );

    let (w_path, w) = storage_src("shadow_candle_writer.rs");
    let w_code = code_only(&w);
    assert!(
        !contains_token(&w_code, &format!(".column_f64(\"{wire}\"")),
        "{}: the ILP builder must not append `{wire}` — ILP AUTO-CREATES any \
         column a writer names, so one leftover append silently re-creates the \
         removed column on a fresh table.",
        w_path.display()
    );
}

#[test]
fn close_pct_from_prev_day_is_gone_from_the_wire() {
    // It was a byte-identical duplicate of `change_pct` (both filled from the
    // same `state.close_pct_from_prev_day`); the survivor is the renamed
    // `percentage_change`. The STRUCT field of this name stays — it is the
    // source both columns always read.
    assert_column_removed_from_the_wire("close_pct_from_prev_day");
}

#[test]
fn open_gap_pct_is_gone_from_the_wire() {
    assert_column_removed_from_the_wire("open_gap_pct");
}

// ============================================================================
// 3. Self-tests — the guard itself must be able to fail
// ============================================================================

#[test]
fn self_test_code_only_strips_comments_keeps_code() {
    let sample = "// close_pct_from_prev_day in a comment\nlet x = close_pct_from_prev_day;";
    let stripped = code_only(sample);
    assert!(!stripped.contains("in a comment"));
    assert!(stripped.contains("let x = close_pct_from_prev_day;"));
}

#[test]
fn self_test_contains_token_refuses_a_column_that_merely_contains_another() {
    // The live vacuity this closes: the OPEN column satisfied the plain
    // `contains` check for `percentage_change`.
    assert!(!contains_token(
        "open_percentage_change DOUBLE,",
        "percentage_change DOUBLE"
    ));
    assert!(contains_token(
        "open_percentage_change DOUBLE, percentage_change DOUBLE,",
        "percentage_change DOUBLE"
    ));
    assert!(!contains_token(
        "pub close_change_pct: f64,",
        "change_pct: f64"
    ));
    assert!(!contains_token(
        "SELECT c.total_buy_qty_x",
        "c.total_buy_qty"
    ));
    assert!(!contains_token(
        "SELECT abc.total_buy_qty",
        "c.total_buy_qty"
    ));
    assert!(contains_token(
        "SELECT c.total_buy_qty, c.x",
        "c.total_buy_qty"
    ));
    // A needle bounded by punctuation keeps plain-substring behaviour.
    assert!(contains_token(
        "x.column_f64(\"percentage_change\", row.change_pct)?",
        ".column_f64(\"percentage_change\", row.change_pct)"
    ));
    assert!(!contains_token(
        ".column_f64(\"open_percentage_change\", row.open_pct)",
        ".column_f64(\"percentage_change\""
    ));
}

#[test]
fn self_test_squeeze_normalizes_ddl_alignment() {
    // The removal check and the wiring check both depend on this: without it a
    // cosmetic realignment of the CREATE TABLE body flips either verdict.
    assert!(squeeze("percentage_change        DOUBLE, \\").contains("percentage_change DOUBLE"));
    assert!(squeeze("percentage_change DOUBLE,").contains("percentage_change DOUBLE"));
}

#[test]
fn self_test_removal_check_would_catch_a_resurrected_column() {
    // Bite-proof: the three patterns the removal assertion searches for must
    // actually match the three shapes a resurrection takes. If a future
    // refactor changes the manifest or the ILP call shape, this fails HERE —
    // loudly — instead of the removal test passing vacuously.
    let ddl = squeeze("                open_gap_pct            DOUBLE, \\");
    assert!(ddl.contains("open_gap_pct DOUBLE"));
    assert!(squeeze("    (\"open_gap_pct\", \"DOUBLE\"),").contains("(\"open_gap_pct\""));
    assert!(
        ".column_f64(\"open_gap_pct\", row.open_gap_pct)".contains(".column_f64(\"open_gap_pct\"")
    );
}

// ============================================================================
// 4. The book totals — the SAME five-link chain, for LONG columns.
//
//    ⚠ 2026-09-18: this section used to be "Net volume + the book totals" and
//    described `net_volume` as the one candle column deliberately OMITTED
//    rather than zero-filled, so an absent classification persisted as NULL.
//    That column is DELETED by the 2026-09-18 directive and `volume` carries
//    the sign instead; the two tests that pinned it are replaced further down
//    by their inverses. The text is corrected rather than dropped because a
//    reader arriving from the retired test names needs to find out where the
//    behaviour went.
// ============================================================================

/// Assert a LONG candle column is wired across DDL, self-heal manifest, ILP
/// write and seal-row struct (the console view was retired 2026-09-22).
fn assert_long_column_wired_end_to_end(col: &str, row_type: &str) {
    let (sp_path, sp_raw) = storage_src("shadow_persistence.rs");
    let sp = squeeze(&code_only(&sp_raw));
    assert!(
        contains_token(&sp, &format!("{col} LONG")),
        "{}: candle CREATE TABLE DDL must declare `{col} LONG`.",
        sp_path.display()
    );
    assert!(
        contains_token(&sp, &format!("(\"{col}\", \"LONG\")")),
        "{}: the candle schema self-heal manifest must carry \
         `(\"{col}\", \"LONG\")` so a table created before this column existed \
         backfills it. Without it an upgraded deployment keeps the old schema \
         and the column stays empty forever (schema self-heal).",
        sp_path.display()
    );

    let (w_path, w_raw) = storage_src("shadow_candle_writer.rs");
    let w = code_only(&w_raw);
    assert!(
        contains_token(&w, &format!(".column_i64(\"{col}\"")),
        "{}: the ILP append builder must emit `.column_i64(\"{col}\", ...)` — \
         a DDL column never written stays NULL forever.",
        w_path.display()
    );

    let (r_path, r_raw) = storage_src("shadow_seal_columns.rs");
    let r = code_only(&r_raw);
    assert!(
        contains_token(&r, &format!("pub {col}: {row_type}")),
        "{}: the persisted seal-row struct must carry `pub {col}: {row_type}`.",
        r_path.display()
    );

    // 2026-09-22 ("no views anywhere"): the `candles_named` analyst view that
    // this helper used to require `c.{col}` in is GONE. The operator reads the
    // `candles_<tf>` TABLES directly, and every candle table carries its own
    // `contract` column, so a stored column IS a visible column — there is no
    // projection layer left for it to go missing from.
}

// ⚠ RETIRED 2026-09-18 — `net_volume_column_wired_end_to_end` and
// `net_volume_is_written_conditionally_so_absent_persists_as_null` are GONE,
// and the two tests below pin the OPPOSITE. The operator's 2026-09-18
// directive deletes the `net_volume` column and signs `volume` instead; the
// ruling, and what it costs, are recorded in
// `.claude/rules/project/websocket-connection-scope-lock.md`
// § "2026-09-18 (FOURTH)". The tests are replaced rather than deleted because
// a column removed from the CREATE while its `ADD COLUMN IF NOT EXISTS`
// self-heal survives is silently re-added on the next boot — that is a named
// REJECT in the rule section, and it needs a ratchet, not a comment.

#[test]
fn total_buy_qty_column_wired_end_to_end() {
    assert_long_column_wired_end_to_end("total_buy_qty", "i64");
}

#[test]
fn total_sell_qty_column_wired_end_to_end() {
    assert_long_column_wired_end_to_end("total_sell_qty", "i64");
}

#[test]
fn net_volume_is_gone_from_every_link_of_the_chain() {
    // The inverse of `assert_long_column_wired_end_to_end`, over the same five
    // links. One surviving link is enough to resurrect the column: the ALTER
    // self-heal alone re-adds it on the next boot, and a view that names it
    // fails to CREATE on a fresh volume.
    for (file, what) in [
        (
            "shadow_persistence.rs",
            "the CREATE DDL and its ALTER self-heal",
        ),
        ("shadow_seal_columns.rs", "the seal-row struct"),
        ("shadow_candle_writer.rs", "the ILP write"),
        ("console_views.rs", "the `candles_named` view"),
    ] {
        let (path, raw) = storage_src(file);
        let code = code_only(&raw);
        // `net_volume_chg_milli_pct` is a DIFFERENT column on a DIFFERENT
        // table (`top_volume`), it is the one the operator asked to sort by,
        // and it carries `net_volume` as a PREFIX. So carve that identifier
        // out first, then refuse the bare word wherever it survives.
        //
        // A LITERAL list -- `\"net_volume\"`, `net_volume:`, `net_volume LONG`,
        // `c.net_volume` -- was what this checked until 2026-09-18, and it had
        // three evasions, all reachable by ordinary edits rather than malice:
        // a different SQL alias (`b.net_volume`, which the 10m view's own
        // nesting would naturally produce), a different column TYPE
        // (`net_volume DOUBLE`), and a name spliced through `format!` from a
        // const. Matching the WORD closes all three at once.
        let scrubbed = code.replace("net_volume_chg_milli_pct", "");
        let mut rest = scrubbed.as_str();
        while let Some(at) = rest.find("net_volume") {
            let tail = &rest[at + "net_volume".len()..];
            let next_is_identifier = tail
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            assert!(
                next_is_identifier,
                "{}: the bare word `net_volume` still appears in {what}. The \
                 2026-09-18 directive deletes the `net_volume` candle column; \
                 every link must go in ONE change or the next boot re-adds it.",
                path.display()
            );
            rest = tail;
        }
    }
}

#[test]
fn the_persisted_volume_is_the_signed_one() {
    // The behavioural half no presence check can see. `volume` must be read
    // from `LiveCandleState::signed_volume()`, which owns the sign rule in one
    // place. A regression to `i64::try_from(seal.state.volume)` — the shape
    // this replaced — compiles, passes every column-presence assertion, and
    // silently un-signs every candle in the database.
    let (path, raw) = storage_src("shadow_seal_columns.rs");
    let code = code_only(&raw);
    assert!(
        code.contains("seal.state.signed_volume()"),
        "{}: the persisted `volume` must come from \
         `LiveCandleState::signed_volume()` (2026-09-18 directive).",
        path.display()
    );
    assert!(
        !code.contains("i64::try_from(seal.state.volume)"),
        "{}: `volume` must NOT be widened straight from the unsigned state \
         field — that is the un-signed shape this replaced.",
        path.display()
    );
}
