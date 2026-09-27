//! Read-only SQL gate for the `questdb_sql` tool — a MIRROR of the operator
//! console's gate (`crates/aws-lambdas/src/operator_control.rs`).
//!
//! # Why a mirror and not a dependency
//!
//! The console's gate lives in the AWS Lambda crate, which pulls the whole
//! AWS SDK. This server is a small local binary; depending on that crate for
//! ninety lines would put the SDK in its build. So the constants and the
//! three functions below are COPIED, and `tests::gate_source_is_identical_to_the_operator_console`
//! reads the console's source at compile time and fails the build if any
//! copied item differs by a single byte. The two gates cannot drift apart
//! silently: the first edit to either one breaks this crate's tests.
//!
//! # What it guarantees (and what it does not)
//!
//! Until 2026-09-27 the `questdb_sql` tool sent the caller's text to the
//! live database's `/exec` endpoint as-is, so `drop table ticks` or
//! `truncate table ...` would have run. After this gate: exactly ONE
//! statement, no comments, first word `select`/`show`/`explain`/`with`, and
//! none of 25 mutating keywords anywhere as a whole word. `SELECT`/`WITH`
//! also get a row cap (`SQL_MAX_ROWS`). The gate is fail-closed: a string
//! literal that happens to contain a banned word is refused too.
//!
//! It is a TEXT gate, not a database permission. The stronger control is a
//! read-only database user, which QuestDB's `/exec` endpoint does not offer
//! in the version pinned here; if it ever does, this gate stays as the
//! second layer.

use std::sync::LazyLock;

use regex::Regex;

/// legacy: `_SQL_ALLOWED_PREFIXES` — read-only SQL gate: the first keyword
/// must be one of these.
pub const SQL_ALLOWED_PREFIXES: [&str; 4] = ["select", "show", "explain", "with"];

/// legacy: `_SQL_BANNED` — none of these mutating keywords may appear
/// anywhere (whole-word). The tail 12 are QuestDB-specific mutators
/// (DB-console hardening 2026-07-02) kept banned anywhere as belt-and-braces
/// on top of the single-statement rule.
pub const SQL_BANNED: [&str; 25] = [
    "insert",
    "update",
    "delete",
    "drop",
    "alter",
    "truncate",
    "create",
    "copy",
    "rename",
    "reindex",
    "vacuum",
    "grant",
    "revoke",
    "backup",
    "checkpoint",
    "snapshot",
    "cancel",
    "set",
    "refresh",
    "detach",
    "attach",
    "dedup",
    "squash",
    "resume",
    "suspend",
];

/// legacy: `_SQL_MAX_ROWS = 1000` — server-side row cap for the read-only
/// SQL console (both the Data-tab query box and the DB tab share `sql`).
pub const SQL_MAX_ROWS: i64 = 1000;

// The static gate regexes cannot fail to compile; the `Option` + fail-closed
// arms below exist only to satisfy the no-unwrap/no-expect charter lints.
static SQL_BANNED_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    let alt = SQL_BANNED.join("|");
    Regex::new(&format!(r"\b(?:{alt})\b")).ok()
});
static SQL_LIMIT_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\blimit\s+(-?\d+)(\s*,\s*-?\d+)?\s*$").ok());

/// First WORD of an already-lowercased query — legacy
/// `re.split(r"[^a-z]+", q, maxsplit=1)[0]` (the leading `[a-z]*` run).
fn first_word(q: &str) -> &str {
    let end = q
        .char_indices()
        .find(|(_, c)| !c.is_ascii_lowercase())
        .map_or(q.len(), |(i, _)| i);
    &q[..end]
}

/// legacy: `_is_safe_sql` (handler.py:220-253) — read-only gate (hardened
/// 2026-07-02). Rules, all fail-closed:
/// 1. SINGLE STATEMENT: one trailing ';' is stripped; ANY remaining ';'
///    rejects (closes the "select 1; <unlisted mutator>" chaining gap).
/// 2. NO SQL COMMENTS ('--' or '/*'): keeps the banned-word scan honest.
/// 3. First WORD must be an allowed read-only keyword (not just a prefix,
///    so "explainx ..." / "selector ..." are rejected).
/// 4. No mutating keyword (whole-word) anywhere — incl. QuestDB mutators.
///    False-positive rejects on string literals are accepted (fail-closed).
pub fn is_safe_sql(query: &str) -> bool {
    let mut q = query.trim().to_lowercase();
    if q.is_empty() {
        return false;
    }
    // Rule 1 — single statement: strip ONE trailing ';', reject any other.
    if let Some(stripped) = q.strip_suffix(';') {
        q = stripped.trim_end().to_string();
    }
    if q.is_empty() || q.contains(';') {
        return false;
    }
    // Rule 2 — no comments.
    if q.contains("--") || q.contains("/*") {
        return false;
    }
    // Rule 3 — allowed first word.
    if !SQL_ALLOWED_PREFIXES.contains(&first_word(&q)) {
        return false;
    }
    // Rule 4 — banned keywords anywhere (fail closed if the static regex
    // somehow failed to build — unreachable).
    match SQL_BANNED_RE.as_ref() {
        Some(re) => !re.is_match(&q),
        None => false,
    }
}

/// legacy: `_cap_sql_rows` (handler.py:256-278) — enforce a server-side row
/// cap on an already-validated read-only query. A trailing `LIMIT n` above
/// the cap is clamped to the cap; a SELECT/WITH query with no trailing LIMIT
/// gets `LIMIT <cap>` appended. SHOW/EXPLAIN are left untouched. QuestDB's
/// range/negative forms (`LIMIT lo,hi` / `LIMIT -n`) are left as-is — the
/// `/exp?limit=` param + `head` on the box remain the belt-and-braces
/// output bound in every case.
pub fn cap_sql_rows_with(query: &str, cap: i64) -> String {
    let mut q = query.trim();
    if let Some(stripped) = q.strip_suffix(';') {
        q = stripped.trim_end();
    }
    if let Some(re) = SQL_LIMIT_RE.as_ref()
        && let Some(m) = re.captures(q)
    {
        let lo = m.get(1).map_or("", |g| g.as_str());
        let hi = m.get(2);
        // legacy: `int(lo) > cap` — arbitrary-precision int; a digits-only
        // string that overflows i64 is necessarily > cap, so a failed parse
        // clamps too (same verdict as the oracle).
        let over = lo.parse::<i64>().map_or(true, |n| n > cap);
        if hi.is_none() && !lo.starts_with('-') && over {
            let start = m.get(0).map_or(q.len(), |g| g.start());
            return format!("{}LIMIT {cap}", &q[..start]);
        }
        return q.to_string();
    }
    let lowered = q.to_lowercase();
    if matches!(first_word(&lowered), "select" | "with") {
        return format!("{q} LIMIT {cap}");
    }
    q.to_string()
}

/// legacy default-arg form: `_cap_sql_rows(query)` with `cap=_SQL_MAX_ROWS`.
pub fn cap_sql_rows(query: &str) -> String {
    cap_sql_rows_with(query, SQL_MAX_ROWS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The operator console's source, read at COMPILE time. A move or rename
    /// of that file is a build error here, never a silently skipped check.
    const CONSOLE_SOURCE: &str = include_str!("../../aws-lambdas/src/operator_control.rs");
    const OWN_SOURCE: &str = include_str!("sql_gate.rs");

    /// One top-level item's source text: from the line starting with
    /// `marker` to the item's end — the marker line itself when it ends in
    /// `;`, else the next line that starts in column 0 (the closing `}` /
    /// `];` / `});` of a top-level item).
    fn item_text<'a>(source: &'a str, marker: &str) -> Vec<&'a str> {
        let lines: Vec<&str> = source.lines().collect();
        let start = lines
            .iter()
            .position(|l| l.starts_with(marker))
            .unwrap_or_else(|| panic!("marker not found: {marker}"));
        let mut out = vec![lines[start]];
        if lines[start].trim_end().ends_with(';') {
            return out;
        }
        for l in &lines[start + 1..] {
            out.push(l);
            if !l.is_empty() && !l.starts_with(' ') {
                break;
            }
        }
        out
    }

    const MIRRORED_ITEMS: [&str; 9] = [
        "pub const SQL_ALLOWED_PREFIXES",
        "pub const SQL_BANNED",
        "pub const SQL_MAX_ROWS",
        "static SQL_BANNED_RE",
        "static SQL_LIMIT_RE",
        "fn first_word",
        "pub fn is_safe_sql",
        "pub fn cap_sql_rows_with",
        "pub fn cap_sql_rows(",
    ];

    #[test]
    fn gate_source_is_identical_to_the_operator_console() {
        for marker in MIRRORED_ITEMS {
            let theirs = item_text(CONSOLE_SOURCE, marker);
            let ours = item_text(OWN_SOURCE, marker);
            // Anti-vacuity: every mirrored item is at least one full line,
            // and every multi-line item spans more than its opening line.
            assert!(!theirs.is_empty() && !theirs[0].is_empty(), "{marker}");
            assert_eq!(
                ours, theirs,
                "`{marker}` differs from the operator console's copy — edit both in the same change"
            );
        }
    }

    #[test]
    fn item_text_bites_on_a_one_byte_change() {
        let a = "pub fn f() {\n    1\n}\n";
        let b = "pub fn f() {\n    2\n}\n";
        assert_ne!(item_text(a, "pub fn f"), item_text(b, "pub fn f"));
        assert_eq!(item_text(a, "pub fn f").len(), 3);
    }

    #[test]
    fn test_sql_gate_is_safe_sql_refuses_destructive_statements() {
        for q in [
            "drop table ticks",
            "truncate table ticks",
            "DELETE FROM ticks",
            "alter table ticks drop partition list '2026-09-27'",
            "insert into ticks values (1)",
            "update ticks set ltp = 0",
            "create table x (a int)",
            "select 1; drop table ticks",
            "select 1 -- ; drop",
            "select /* */ 1",
            "vacuum table ticks",
            "copy ticks to '/tmp/x'",
            "with x as (select 1) insert into y select * from x",
            "",
            "   ",
        ] {
            assert!(!is_safe_sql(q), "must refuse: {q:?}");
        }
    }

    #[test]
    fn test_sql_gate_cap_sql_rows_read_only_statements_pass_and_get_a_row_cap() {
        for q in [
            "select count() from ticks",
            "SHOW TABLES",
            "explain select * from ticks",
            "with x as (select 1) select * from x",
            "select * from ticks;",
        ] {
            assert!(is_safe_sql(q), "must pass: {q:?}");
        }
        assert_eq!(
            cap_sql_rows("select * from ticks"),
            "select * from ticks LIMIT 1000"
        );
        assert_eq!(
            cap_sql_rows("select * from ticks limit 50000"),
            "select * from ticks LIMIT 1000"
        );
        assert_eq!(cap_sql_rows("show tables"), "show tables");
    }

    #[test]
    fn test_sql_gate_cap_sql_rows_with_clamps_to_the_given_cap() {
        assert_eq!(cap_sql_rows_with("select 1", 5), "select 1 LIMIT 5");
        assert_eq!(cap_sql_rows_with("select 1 limit 9", 5), "select 1 LIMIT 5");
        assert_eq!(cap_sql_rows_with("select 1 limit 3", 5), "select 1 limit 3");
    }
}
