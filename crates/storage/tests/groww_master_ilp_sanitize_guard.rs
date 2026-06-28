//! F4 (PR-C dual-feed hardening) — ILP-injection source-scan ratchet.
//!
//! The Groww shared-master writer (`crates/core/src/feed/groww/shared_master_writer.rs`)
//! feeds `feed='groww'` rows — including operator/CSV-derived strings like
//! `symbol_name`, `index_name`, `display_name`, `prev_symbol_chain`, `isin` — into the
//! SAME two storage append fns Dhan uses:
//!   * `instrument_lifecycle_persistence::build_lifecycle_ilp_row`
//!   * `index_constituency_persistence::build_index_constituency_ilp_row`
//!
//! Every one of those string/symbol row fields MUST be wrapped in a `sanitize_ilp_*`
//! call before it is written to the ILP buffer, otherwise a crafted symbol could inject
//! ILP tag/field delimiters (`,`/`=`/space/newline) and corrupt the line protocol stream
//! (ILP injection). This guard scans those two builder fns and FAILS THE BUILD if any
//! `.symbol("...", <arg>)` or `.column_str("...", <arg>)` passes a RAW `row.<field>`
//! String without a `sanitize_ilp_symbol(...)` / `sanitize_ilp_string(...)` wrapper.
//!
//! A future edit that writes a Groww string raw → red build. See PR-C plan F4 +
//! `.claude/rules/project/groww-shared-master-error-codes.md`.

#![cfg(test)]

use std::path::PathBuf;

fn read_src(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Extract the body of the named fn (from its signature line to the next
/// top-level `fn ` / end-of-buffer marker), so we scan ONLY that builder.
fn extract_fn_body<'a>(src: &'a str, fn_sig_marker: &str) -> &'a str {
    let start = src
        .find(fn_sig_marker)
        .unwrap_or_else(|| panic!("fn marker not found: {fn_sig_marker}"));
    let rest = &src[start..];
    // The builder fns are short and immediately followed by `fn ` (another
    // builder/append) or `pub async fn`. Cut at the next `\nfn ` or `\npub `
    // after the opening, whichever comes first; fall back to end.
    let after = &rest[fn_sig_marker.len()..];
    let cut = after
        .find("\nfn ")
        .into_iter()
        .chain(after.find("\npub "))
        .min()
        .unwrap_or(after.len());
    &rest[..fn_sig_marker.len() + cut]
}

/// For every `.symbol(...)` / `.column_str(...)` STATEMENT (which rustfmt may wrap
/// across several lines) that references a raw `row.<field>` String, the statement
/// must also contain a `sanitize_ilp_` wrapper.
///
/// Enum-derived `&'static str` writes — `row.<field>.as_str()` (e.g.
/// `row.lifecycle_state.as_str()`) — are a CONTROLLED, closed set of values, NOT
/// arbitrary operator/CSV input, so they are legitimately written raw and are
/// exempted. The injection risk is only for free-text String fields.
fn assert_all_row_string_writes_sanitized(body: &str, fn_name: &str) {
    let mut checked = 0usize;
    // Split into statements (terminated by `?;`) so a rustfmt-wrapped multi-line
    // `.symbol(\n  "x",\n  sanitize_ilp_symbol(row.x).as_ref(),\n)?;` is ONE unit.
    for stmt in body.split("?;") {
        let is_string_write = stmt.contains(".symbol(") || stmt.contains(".column_str(");
        if !is_string_write {
            continue;
        }
        if !stmt.contains("row.") {
            continue;
        }
        // Exempt enum-derived `.as_str()` writes of a row field (controlled set).
        // If the ONLY `row.` reference is a `.as_str()` enum write, skip it.
        let has_non_enum_row_field = stmt.match_indices("row.").any(|(i, _)| {
            // The field name ends at the first `.`/`)`/`,`/space/newline.
            let after = &stmt[i + "row.".len()..];
            let field_end = after
                .find(['.', ')', ',', ' ', '\n'])
                .unwrap_or(after.len());
            let tail = &after[field_end..];
            // `.as_str()` immediately following the field name = enum-derived → exempt.
            !tail.trim_start().starts_with(".as_str()")
        });
        if !has_non_enum_row_field {
            continue; // all row.* references are enum `.as_str()` — exempt
        }
        checked += 1;
        assert!(
            stmt.contains("sanitize_ilp_"),
            "{fn_name}: ILP string/symbol write of a raw `row.` String field WITHOUT a \
             sanitize_ilp_* wrapper (ILP injection risk):\n{}",
            stmt.trim()
        );
    }
    // Sanity: the scan actually found writes (guards against a refactor that
    // renamed the builder and silently made this test a no-op).
    assert!(
        checked >= 3,
        "{fn_name}: expected to scan >=3 non-enum row.* string/symbol writes, found \
         {checked} — did the builder fn get renamed/restructured?"
    );
}

#[test]
fn test_groww_master_string_columns_are_ilp_sanitized() {
    // instrument_lifecycle builder — symbol_name/exchange_segment/instrument_type/
    // feed/source_csv_sha256/exchange_id/underlying_symbol/option_type (symbols) +
    // display_name/prev_symbol_chain (strings).
    let lifecycle_src = read_src("instrument_lifecycle_persistence.rs");
    let lifecycle_body = extract_fn_body(&lifecycle_src, "fn build_lifecycle_ilp_row(");
    assert_all_row_string_writes_sanitized(lifecycle_body, "build_lifecycle_ilp_row");

    // index_constituency builder — index_name/exchange_segment/symbol_name/source/
    // feed/isin (all symbols).
    let constituency_src = read_src("index_constituency_persistence.rs");
    let constituency_body =
        extract_fn_body(&constituency_src, "fn build_index_constituency_ilp_row(");
    assert_all_row_string_writes_sanitized(constituency_body, "build_index_constituency_ilp_row");
}
