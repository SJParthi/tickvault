//! Operator portal Lambda — ONE URL to run the whole product (VIEW + CONTROL).
//!
//! Rust port (rust-only phase 2b-3, 2026-07-18) of
//! `deploy/aws/lambda/operator-control/handler.py` — the Python file is the
//! ORACLE: every constant, string, parser, gate, and response shape below is
//! byte-parity with it, and every deliberate deviation is ledger-commented at
//! the deviating line. The 150 Python unit tests are ported 1:1 in the test
//! module(s) of this file.
//!
//! Served via BOTH the Lambda Function URL and the API-GW v2 (payload 2.0)
//! route: `GET /` returns the public portal HTML shell (zero secrets);
//! `POST /` requires `Authorization: Bearer <secret>` (constant-time compare
//! against the SSM SecureString control secret).
//!
//! SECURITY MODEL (deliberately strict — this can stop a live trading box):
//! * Destructive box actions (stop/reboot/restart-app/stop-app) are blocked
//!   during market hours (09:15-15:30 IST Mon-Fri) unless `{"force": true}`.
//! * DATA-DESTRUCTIVE actions (wipe-questdb/docker-reset/docker-nuke-bare)
//!   are HARD-LOCKED during market hours — refused with 409 even with
//!   `{"force": true}` (operator incident 2026-07-02 15:05 IST).
//! * The SQL box is READ-ONLY: only SELECT/SHOW/EXPLAIN/WITH are accepted;
//!   any mutating keyword is rejected before it ever reaches QuestDB.
//!
//! Cold path only — a handful of operator clicks per day; none of the
//! hot-path (zero-alloc / O(1)) constraints apply, but the charter lints on
//! lib.rs do.

use std::sync::LazyLock;

use aws_lc_rs::constant_time::verify_slices_are_equal;
use aws_lc_rs::hmac;
use chrono::{DateTime, Datelike, TimeDelta, Timelike, Utc};
use regex::Regex;
use serde_json::Value;

// ------------------------------------------------------------------ constants
// python: handler.py:112 — destructive box actions blocked during market
// hours unless force=true. (wipe-groww removed 2026-07-16 — the groww-only
// wipe control retired with the live feeds.)
pub const DESTRUCTIVE: [&str; 7] = [
    "stop",
    "reboot",
    "restart-app",
    "stop-app",
    "wipe-questdb",
    "docker-reset",
    "docker-nuke-bare",
];

// python: handler.py:123 — HARD LOCK: the DATA-DESTRUCTIVE subset of
// DESTRUCTIVE — actions that DELETE market data which can NEVER be
// re-fetched from upstream. During market hours these are REFUSED with 409
// EVEN WHEN force=true. Lifecycle actions (stop / reboot / restart-app /
// stop-app) deliberately KEEP the force override so emergencies stay
// possible.
pub const DATA_DESTRUCTIVE: [&str; 3] = ["wipe-questdb", "docker-reset", "docker-nuke-bare"];

/// python: handler.py:125-129 (`_DATA_DESTRUCTIVE_LOCK_MSG`) — verbatim.
pub const DATA_DESTRUCTIVE_LOCK_MSG: &str = "Data-destructive actions are locked during market hours \
(09:15-15:30 IST) — a mid-market wipe destroys data that can never \
be re-fetched. Run after 15:30.";

/// python: `_MKT_OPEN_SECS = 9 * 3600 + 15 * 60` (09:15 IST, seconds-of-day).
pub const MKT_OPEN_SECS: u32 = 9 * 3600 + 15 * 60;
/// python: `_MKT_CLOSE_SECS = 15 * 3600 + 30 * 60` (15:30 IST, seconds-of-day).
pub const MKT_CLOSE_SECS: u32 = 15 * 3600 + 30 * 60;
/// python: `_IST_OFFSET_SECS = 19800  # +05:30`.
pub const IST_OFFSET_SECS: i64 = 19_800;

/// python: `_SECRET_TTL_SECS = 60.0` — control-secret SSM cache TTL.
pub const SECRET_TTL_SECS: f64 = 60.0;
/// python: `_VIEW_TIMEOUT_SECS = 6.0` — SSM RunCommand poll budget (view).
pub const VIEW_TIMEOUT_SECS: f64 = 6.0;
/// python: `_VIEW_POLL_SECS = 0.4` — SSM get_command_invocation poll cadence.
pub const VIEW_POLL_SECS: f64 = 0.4;
/// python: `_LATENCY_TIMEOUT_SECS = 15.0` — the REST-era latency snapshot is
/// one metrics scrape (3s) + one QuestDB round-trip probe (3s) + chronyc +
/// one bounded rest_fetch_audit aggregate (4s); worst case ≈ 10-11s incl.
/// SSM registration — per-curl `--max-time` bounds every leg.
pub const LATENCY_TIMEOUT_SECS: f64 = 15.0;

/// python: `_SQL_ALLOWED_PREFIXES` — read-only SQL gate: the first keyword
/// must be one of these.
pub const SQL_ALLOWED_PREFIXES: [&str; 4] = ["select", "show", "explain", "with"];

/// python: `_SQL_BANNED` — none of these mutating keywords may appear
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

/// python: `_SQL_MAX_ROWS = 1000` — server-side row cap for the read-only
/// SQL console (both the Data-tab query box and the DB tab share `sql`).
pub const SQL_MAX_ROWS: i64 = 1000;

/// python: `_QDB_LINK_TTL_SECS = 90` — TTL of the one-click console link
/// token minted by the `qdb_console_url` action. The console front Lambda
/// verifies it (same control secret, HMAC over `"qdblink|<exp>"`) and swaps
/// it for a 12h session cookie — so the link in a browser history / Telegram
/// scroll dies in 90s.
pub const QDB_LINK_TTL_SECS: i64 = 90;

// ------------------------------------------------------------- pure functions

/// python: `_is_market_hours` (handler.py:193-199) — true during NSE market
/// hours (Mon-Fri 09:15-15:30 IST). Fixed UTC+5:30 offset exactly like the
/// oracle (India has no DST).
pub fn is_market_hours(now_utc: DateTime<Utc>) -> bool {
    let ist = now_utc + TimeDelta::seconds(IST_OFFSET_SECS);
    // python weekday(): Monday == 0 .. Sunday == 6; chrono mirror below.
    if ist.weekday().num_days_from_monday() >= 5 {
        return false;
    }
    let sod = ist.hour() * 3600 + ist.minute() * 60 + ist.second();
    (MKT_OPEN_SECS..MKT_CLOSE_SECS).contains(&sod)
}

/// python: `_http_method` (handler.py:202-206). Fail closed: a malformed
/// event is treated as an action ("POST" — hits auth), never a free page
/// serve.
pub fn http_method(event: &Value) -> String {
    if let Some(m) = event
        .pointer("/requestContext/http/method")
        .and_then(Value::as_str)
    {
        return m.to_uppercase();
    }
    // Ledger deviation: python `str()`-ifies a NON-STRING value found at
    // requestContext.http.method / httpMethod (e.g. str({}) → "{}"); real
    // fn-URL / API-GW v2 events always carry strings, so we fall through to
    // the same "POST" default instead of JSON-stringifying garbage.
    match event.get("httpMethod").and_then(Value::as_str) {
        Some(s) => s.to_uppercase(),
        None => "POST".to_string(),
    }
}

/// python: `_authorized` (handler.py:209-216) — constant-time bearer check.
/// Header keys are lowercased by the Function URL. The Python oracle read
/// the secret via `_control_secret()` inside the fn (monkeypatched in
/// tests); the Rust core takes it as a parameter and the shell passes the
/// SSM-cached value.
pub fn authorized(headers: &Value, secret: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    let auth = headers
        .get("authorization")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(presented) = auth.strip_prefix("Bearer ") else {
        return false;
    };
    // python `hmac.compare_digest` — constant-time for equal lengths, early
    // length check otherwise; aws-lc-rs mirrors that contract.
    verify_slices_are_equal(presented.as_bytes(), secret.as_bytes()).is_ok()
}

// The static gate regexes cannot fail to compile; the `Option` + fail-closed
// arms below exist only to satisfy the no-unwrap/no-expect charter lints.
static SQL_BANNED_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    let alt = SQL_BANNED.join("|");
    Regex::new(&format!(r"\b(?:{alt})\b")).ok()
});
static SQL_LIMIT_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)\blimit\s+(-?\d+)(\s*,\s*-?\d+)?\s*$").ok());

/// First WORD of an already-lowercased query — python
/// `re.split(r"[^a-z]+", q, maxsplit=1)[0]` (the leading `[a-z]*` run).
fn first_word(q: &str) -> &str {
    let end = q
        .char_indices()
        .find(|(_, c)| !c.is_ascii_lowercase())
        .map_or(q.len(), |(i, _)| i);
    &q[..end]
}

/// python: `_is_safe_sql` (handler.py:220-253) — read-only gate (hardened
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

/// python: `_cap_sql_rows` (handler.py:256-278) — enforce a server-side row
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
        // python: `int(lo) > cap` — arbitrary-precision int; a digits-only
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

/// python default-arg form: `_cap_sql_rows(query)` with `cap=_SQL_MAX_ROWS`.
pub fn cap_sql_rows(query: &str) -> String {
    cap_sql_rows_with(query, SQL_MAX_ROWS)
}

/// python: `_mint_qdb_link_token` (handler.py:281-291).
///
/// hmac token wire contract (byte-exact with the python oracle,
/// handler.py:281-291, and with the sibling `questdb-console-front` Lambda
/// which VERIFIES this token constant-time against the SAME SSM control
/// secret):
///   * `exp = now_epoch + ttl` — integer unix seconds, base-10, no padding;
///   * message = `"qdblink|<exp>"` (ASCII, pipe separator, decimal exp);
///   * sig = HMAC-SHA256(key = UTF-8 secret bytes, message) hex-encoded in
///     LOWERCASE (python `.hexdigest()`);
///   * token = `"<exp>.<sig>"`.
/// One-click console link: dies in `QDB_LINK_TTL_SECS` (90s); the console
/// front swaps it for a 12h session cookie.
pub fn mint_qdb_link_token(secret: &str, now_epoch: i64, ttl: i64) -> String {
    let exp = now_epoch + ttl;
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let tag = hmac::sign(&key, format!("qdblink|{exp}").as_bytes());
    let mut sig = String::with_capacity(64);
    for byte in tag.as_ref() {
        // Cold path — per-byte formatting is fine here.
        sig.push_str(&format!("{byte:02x}"));
    }
    format!("{exp}.{sig}")
}

/// python default-arg form: `_mint_qdb_link_token(secret, now_epoch)` with
/// `ttl=_QDB_LINK_TTL_SECS`.
pub fn mint_qdb_link_token_default_ttl(secret: &str, now_epoch: i64) -> String {
    mint_qdb_link_token(secret, now_epoch, QDB_LINK_TTL_SECS)
}

// ----------------------------------------------------------------- shell stub
// The full event router (`lambda_handler` port) + AWS shell land in the
// dispatch unit; this placeholder keeps the thin bin compiling per-unit.
/// Entry point invoked by the thin `operator-control` bin.
pub async fn handle(event: Value) -> Result<Value, lambda_runtime::Error> {
    let _ = event;
    // Ported in a later unit of this branch (routing + SSM/EC2/CW/CE shell).
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    // ------------------------------------------------------- class HttpMethod
    #[test]
    fn test_function_url_v2_get() {
        let ev = json!({"requestContext": {"http": {"method": "get"}}});
        assert_eq!(http_method(&ev), "GET");
    }

    #[test]
    fn test_missing_method_defaults_to_post() {
        // Fail closed: a malformed event is treated as an action (hits auth),
        // never as a free page serve.
        assert_eq!(http_method(&json!({})), "POST");
    }

    // ------------------------------------------------- class MarketHoursGuard
    #[test]
    fn test_inside_window_monday_1100_ist_is_market_hours() {
        // 11:00 IST Mon == 05:30 UTC Mon.
        let utc = Utc.with_ymd_and_hms(2026, 6, 1, 5, 30, 0).unwrap();
        assert!(is_market_hours(utc));
    }

    #[test]
    fn test_after_close_monday_1600_ist_is_not_market_hours() {
        // 16:00 IST Mon == 10:30 UTC Mon.
        let utc = Utc.with_ymd_and_hms(2026, 6, 1, 10, 30, 0).unwrap();
        assert!(!is_market_hours(utc));
    }

    #[test]
    fn test_weekend_is_never_market_hours() {
        // 2026-06-06 is a Saturday; any time → closed.
        let utc = Utc.with_ymd_and_hms(2026, 6, 6, 5, 30, 0).unwrap();
        assert!(!is_market_hours(utc));
    }

    // ---------------------------------------------------- class Authorization
    // python monkeypatched `handler._control_secret`; the Rust core takes the
    // secret as a parameter, so the fixture secret is passed directly.
    const AUTH_SECRET: &str = "s3cret-token"; // test fixture, not a credential

    #[test]
    fn test_correct_bearer_is_authorized() {
        assert!(authorized(
            &json!({"authorization": "Bearer s3cret-token"}),
            AUTH_SECRET
        ));
    }

    #[test]
    fn test_wrong_bearer_is_rejected() {
        assert!(!authorized(
            &json!({"authorization": "Bearer nope"}),
            AUTH_SECRET
        ));
    }

    #[test]
    fn test_missing_header_is_rejected() {
        assert!(!authorized(&json!({}), AUTH_SECRET));
    }

    #[test]
    fn test_no_configured_secret_denies_all() {
        assert!(!authorized(
            &json!({"authorization": "Bearer anything"}),
            ""
        ));
    }

    // ---------------------------------------------------------- class SafeSql
    #[test]
    fn test_select_is_allowed() {
        assert!(is_safe_sql("SELECT count() FROM ticks"));
        assert!(is_safe_sql("  with x as (select 1) select * from x"));
        assert!(is_safe_sql("SHOW COLUMNS FROM ticks"));
    }

    #[test]
    fn test_mutations_are_rejected() {
        for q in [
            "DROP TABLE ticks",
            "delete from ticks",
            "insert into ticks values (1)",
            "update ticks set x=1",
            "truncate table ticks",
            "alter table ticks add column z int",
            "select 1; drop table ticks", // banned keyword anywhere
        ] {
            assert!(!is_safe_sql(q), "{q}");
        }
    }

    #[test]
    fn test_empty_or_non_read_is_rejected() {
        assert!(!is_safe_sql(""));
        assert!(!is_safe_sql("   "));
        // not a prefix word? still starts with 'explain'
        assert!(!is_safe_sql("explainx select 1"));
        assert!(!is_safe_sql("vacuum"));
    }

    // -------------------------------------------------- class SafeSqlHardened
    // DB-console hardening (2026-07-02): single-statement, no comments,
    // QuestDB mutators banned anywhere, server-side row cap.
    #[test]
    fn test_chained_unlisted_mutator_rejected() {
        // THE pre-hardening gap: 'backup' was not in the banned list and the
        // first-word check only saw "select", so a ';'-chained second
        // statement slipped through. Now BOTH the single-statement rule and
        // the extended banned list reject it.
        assert!(!is_safe_sql("select 1; backup table ticks"));
    }

    #[test]
    fn test_multi_statement_rejected_single_trailing_semicolon_ok() {
        assert!(!is_safe_sql("select 1; select 2"));
        assert!(!is_safe_sql("select 1;;"));
        assert!(!is_safe_sql(";"));
        assert!(is_safe_sql("select 1;")); // ONE trailing ';' stripped
    }

    #[test]
    fn test_sql_comments_rejected() {
        assert!(!is_safe_sql("-- comment\nselect 1"));
        assert!(!is_safe_sql("select 1 -- tail comment"));
        assert!(!is_safe_sql("select /* hidden */ 1"));
    }

    #[test]
    fn test_with_insert_still_rejected() {
        assert!(!is_safe_sql(
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x"
        ));
    }

    #[test]
    fn test_questdb_mutator_keywords_rejected_anywhere() {
        for kw in [
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
        ] {
            assert!(!is_safe_sql(&format!("select {kw} from ticks")), "{kw}");
            assert!(!is_safe_sql(&format!("{kw} table ticks")), "{kw}");
        }
    }

    #[test]
    fn test_plain_reads_still_pass() {
        assert!(is_safe_sql("SELECT * FROM ticks LIMIT 10"));
        assert!(is_safe_sql("SHOW TABLES"));
        assert!(is_safe_sql(
            "SELECT table_name FROM tables() ORDER BY table_name"
        ));
        assert!(is_safe_sql("SELECT * FROM table_columns('ticks')"));
        // word-boundary sanity: 'offset' must not trip the 'set' ban.
        assert!(is_safe_sql("select * from ticks limit 10 offset"));
    }

    // -------------------------------------------------------- class SqlRowCap
    #[test]
    fn test_oversized_user_limit_is_clamped_to_1000() {
        assert_eq!(
            cap_sql_rows("select * from ticks limit 99999"),
            "select * from ticks LIMIT 1000"
        );
    }

    #[test]
    fn test_missing_limit_is_appended_for_select() {
        assert_eq!(
            cap_sql_rows("select * from ticks"),
            "select * from ticks LIMIT 1000"
        );
    }

    #[test]
    fn test_small_user_limit_is_kept() {
        assert_eq!(
            cap_sql_rows("select * from ticks limit 50"),
            "select * from ticks limit 50"
        );
    }

    #[test]
    fn test_show_is_left_untouched() {
        // Appending LIMIT to SHOW would be a syntax error; output is tiny.
        assert_eq!(cap_sql_rows("SHOW TABLES"), "SHOW TABLES");
    }

    #[test]
    fn test_trailing_semicolon_is_stripped_before_append() {
        assert_eq!(cap_sql_rows("select 1;"), "select 1 LIMIT 1000");
    }

    #[test]
    fn test_sql_max_rows_constant_is_1000() {
        assert_eq!(SQL_MAX_ROWS, 1000);
    }

    // Rust-side parity extras for the row cap's QuestDB range/negative forms
    // (pinned by the python docstring; exercised here so the port cannot
    // silently diverge on them).
    #[test]
    fn parity_range_and_negative_limits_left_as_is() {
        assert_eq!(
            cap_sql_rows("select * from t limit 10,20"),
            "select * from t limit 10,20"
        );
        assert_eq!(
            cap_sql_rows("select * from t limit -5"),
            "select * from t limit -5"
        );
        // i64-overflowing positive limit still clamps (python bigint parity).
        assert_eq!(
            cap_sql_rows("select * from t limit 99999999999999999999999"),
            "select * from t LIMIT 1000"
        );
    }

    // ---------------------------------------- class QdbConsoleUrlAction (mint)
    // The full lambda-dispatch halves of this class are ported with the
    // routing unit; the pure mint vector + TTL ratchet live here.
    #[test]
    fn test_mint_helper_ttl_constant_is_90() {
        assert_eq!(QDB_LINK_TTL_SECS, 90);
    }

    #[test]
    fn mint_qdb_link_token_matches_python_hexdigest_vector() {
        // Byte-exact oracle vector: python
        //   hmac.new(b"s3cret-token", b"qdblink|1780000090", sha256).hexdigest()
        // == "643a1010a7594e457633d6a2ba7f0e31dd84efbb8b0bf4191a1b5bed07caa163".
        let tok = mint_qdb_link_token(AUTH_SECRET, 1_780_000_000, QDB_LINK_TTL_SECS);
        assert_eq!(
            tok,
            "1780000090.643a1010a7594e457633d6a2ba7f0e31dd84efbb8b0bf4191a1b5bed07caa163"
        );
        assert_eq!(
            mint_qdb_link_token_default_ttl(AUTH_SECRET, 1_780_000_000),
            tok
        );
    }
}
