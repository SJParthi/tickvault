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
use serde_json::{Value, json};

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
///
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

// -------------------------------------------------------------- more constants

/// python: `_REST_LAT_QUERY_MAX_SECS = 4` (handler.py:441).
pub const REST_LAT_QUERY_MAX_SECS: u64 = 4;

/// python: `_FEED_API_UNREACHABLE` (handler.py:636-638) — verbatim.
pub const FEED_API_UNREACHABLE: &str =
    "app API unreachable on the box (curl to 127.0.0.1:3001 failed — is the app running?)";

/// python: `_BOX_UNREACHABLE` (handler.py:639) — verbatim.
pub const BOX_UNREACHABLE: &str = "box unreachable (SSM offline or instance stopped)";

/// python: `_FEEDS_TIMEOUT_SECS = 28.0` (handler.py:685). Budget arithmetic
/// (review fix M1, 2026-07-16): the snapshot's curls are `--max-time` bounded
/// at 8s + 8s (app /api/feeds + /api/feeds/health) + 4s + 4s (the two
/// rest_fetch_audit reads) = 24s worst case, PLUS SSM registration/poll
/// overhead → 28s. The budget MUST exceed the sum of every `--max-time` in
/// `FEEDS_VIEW_COMMANDS` (drift-proof test parses them).
pub const FEEDS_TIMEOUT_SECS: f64 = 28.0;

/// python: `_MAIN_SHA_TTL_SECS = 60.0` (handler.py:875).
pub const MAIN_SHA_TTL_SECS: f64 = 60.0;
/// python: `_MAIN_SHA_MAX_AGE_SECS = 600.0` (handler.py:879) — on sustained
/// GitHub failure the cached main-HEAD value must not be served forever; past
/// this hard max-age a failed refresh degrades to "unknown".
pub const MAIN_SHA_MAX_AGE_SECS: f64 = 600.0;

/// The single-page operator portal — BYTE-IDENTICAL to the RAW pre-footer
/// python `_console_html()` template (handler.py:1510+; extracted verbatim
/// from the running oracle before the python source is deleted in this PR —
/// sha256 identity is ratcheted in the test module). The provenance footer is
/// spliced in at serve time by [`html_resp`], mirroring python
/// `_html_resp()`'s `html.replace("</body>", footer + "\n</body>", 1)`.
pub const CONSOLE_HTML: &str = include_str!("operator_control_console.html");

// --------------------------------------------------------- python-parity glue

/// python `bool(x)` truthiness for JSON values (`bool(payload.get("force",
/// False))`): null/absent → false, numbers → != 0, strings/arrays/objects →
/// non-empty.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// python `str(payload.get(key, "")).strip()` for the string fields the
/// router reads (action / confirm / command_id / query). Ledger deviation: a
/// NON-STRING value (e.g. a JSON number) falls to "" instead of python's
/// `str()`-ification ("5" / "True") — untested edge, every real caller sends
/// strings.
fn payload_str<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
}

/// python `urllib.parse.quote(s)` with the default `safe='/'`: unreserved
/// ALPHA / DIGIT / `_.-~` plus `/` pass through; every other byte of the
/// UTF-8 encoding is `%XX` (uppercase hex) — pinned by the captured
/// `sql_commands_example` oracle golden.
fn url_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => {
                // Cold path — per-byte formatting is fine here.
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// python `round(x, 1)` byte-parity: both operate on the same IEEE double
/// with round-half-to-even of the true decimal expansion (verified vectors:
/// 5200.55 → 5200.6, 1450.04 → 1450.0).
fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

/// python `html.escape(s)` (quote=True): `&` first, then `< > " '`.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Last `n` CHARS of a string — python `s[-1500:]` slicing semantics
/// (char-boundary safe on UTF-8).
fn last_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// First `n` CHARS — python `s[:500]` slicing semantics.
fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// The token/hex gate regexes cannot fail to compile; the `Option` +
// fail-closed arms exist only to satisfy the no-unwrap/no-expect lints.
static TOKEN_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^[a-z0-9_-]{1,32}$").ok());
static FEED_NAME_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_-]{0,31}$").ok());
static HEX_SHA_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{7,40}$").ok());

/// python `re.fullmatch(r"[a-z0-9_-]{1,32}", s)` — the feed/leg/outcome
/// charset gate (defense in depth on our own symbols).
fn is_token(s: &str) -> bool {
    TOKEN_RE.as_ref().is_some_and(|re| re.is_match(s))
}

/// python `_num` (handler.py:514-520 / 732-738): ""/null/nan → None, else
/// float-or-None.
fn parse_num(s: &str) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let low = s.to_lowercase();
    if low == "null" || low == "nan" {
        return None;
    }
    s.parse::<f64>().ok()
}

// ------------------------------------------------------------------ view snap

/// python: `_parse_feed_counts` (handler.py:357-372) — parse a *_BY_FEED
/// value (';'-joined QuestDB CSV rows like `"dhan",123;"groww",45;`) into
/// {feed: count_str}. Malformed fragments are skipped; an empty/absent value
/// yields {} (the UI then shows nothing rather than fabricated zeros).
pub fn parse_feed_counts(raw: &str) -> Value {
    let mut out = serde_json::Map::new();
    for part in raw.split(';') {
        let part = part.trim();
        let Some((name, cnt)) = part.split_once(',') else {
            continue;
        };
        let name = name.trim().trim_matches('"');
        let cnt = cnt.trim().trim_matches('"');
        if !name.is_empty() && !cnt.is_empty() {
            out.insert(name.to_string(), Value::String(cnt.to_string()));
        }
    }
    Value::Object(out)
}

/// python: `_sum_counts` (handler.py:375-380) — sum the parseable
/// non-negative integer count strings; "" when NONE parsed (box stopped /
/// QuestDB down) — the hero then shows nothing rather than a fabricated 0.
pub fn sum_counts(vals: &[&str]) -> String {
    let mut sum: u128 = 0;
    let mut any = false;
    for v in vals {
        let t = v.trim();
        // python `.isdigit()` then arbitrary-precision int(); u128 covers any
        // real count (ledger deviation: a >u128 digit string is skipped).
        if !t.is_empty()
            && t.chars().all(|c| c.is_ascii_digit())
            && let Ok(n) = t.parse::<u128>()
        {
            sum = sum.saturating_add(n);
            any = true;
        }
    }
    if any { sum.to_string() } else { String::new() }
}

/// python: `_parse_view` (handler.py:383-418) — parse the labeled snapshot
/// stdout into the structured view dict.
pub fn parse_view(stdout: &str) -> Value {
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut errors: Vec<Value> = Vec::new();
    let mut in_errors = false;
    for line in stdout.lines() {
        if line == "ERRORS_BEGIN" {
            in_errors = true;
            continue;
        }
        if line == "ERRORS_END" {
            in_errors = false;
            continue;
        }
        if in_errors {
            if !line.trim().is_empty() {
                errors.push(Value::String(line.trim_end().to_string()));
            }
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            fields.insert(key.trim().to_string(), val.trim().to_string());
        }
    }
    let get = |k: &str| fields.get(k).cloned().unwrap_or_default();
    let (spot, chain, contracts) = (get("SPOT_TODAY"), get("CHAIN_TODAY"), get("CONTRACT_TODAY"));
    json!({
        "app": get("APP"),
        // Official minute candles captured today by the REST pulls — the
        // three LIVE tables, per table + per feed. See VIEW_COMMANDS.
        "rows_today": {"spot": spot, "chain": chain, "contracts": contracts},
        "rows_today_total": sum_counts(&[&spot, &chain, &contracts]),
        "rows_by_feed": {
            "spot": parse_feed_counts(&get("SPOT_BY_FEED")),
            "chain": parse_feed_counts(&get("CHAIN_BY_FEED")),
            "contracts": parse_feed_counts(&get("CONTRACT_BY_FEED")),
        },
        "dedup_key_columns": get("DEDUP_KEYS"),
        "recent_errors": errors,
    })
}

// --------------------------------------------------------------- latency snap

/// python: `_avg_ns` (handler.py:486-493) — sum/count → average nanoseconds,
/// or None if no samples.
pub fn avg_ns(sum_v: &str, count_v: &str) -> Option<f64> {
    let s: f64 = sum_v.trim().parse().ok()?;
    let c: f64 = count_v.trim().parse().ok()?;
    if c > 0.0 { Some(s / c) } else { None }
}

/// python: `_parse_rest_lat_row` (handler.py:496-532) — parse one
/// `<feed>,<leg>,<ok_rows>,<p50>,<p99>` CSV line (latency in MILLISECONDS —
/// close_to_data_ms is stored in ms), or None for anything malformed — a
/// failed on-box query yields nothing, never a fabricated row.
pub fn parse_rest_lat_row(raw: &str) -> Option<Value> {
    let parts: Vec<&str> = raw.split(',').map(|p| p.trim().trim_matches('"')).collect();
    if parts.len() != 5 {
        return None;
    }
    let (feed, leg) = (parts[0], parts[1]);
    if !is_token(feed) || !is_token(leg) {
        return None;
    }
    let rows = parse_num(parts[2])?;
    if rows < 0.0 {
        return None;
    }
    let (p50, p99) = (parse_num(parts[3]), parse_num(parts[4]));
    Some(json!({
        "feed": feed,
        "leg": leg,
        // python `int(rows)` truncation; `as i64` saturates on the untested
        // inf edge instead of raising.
        "ok_rows": rows as i64,
        "p50_ms": p50.map(round1),
        "p99_ms": p99.map(round1),
    }))
}

/// python: `sec_to_ms` inside `_parse_latency` (handler.py:567-571) —
/// `f"{float(s) * 1000:.1f}"`, "" on unparseable.
fn sec_to_ms(s: &str) -> String {
    s.trim()
        .parse::<f64>()
        .map(|f| format!("{:.1}", f * 1000.0))
        .unwrap_or_default()
}

/// python: `_parse_latency` (handler.py:535-585) — parse the labeled latency
/// snapshot. RESTLAT_ROW lines carry the per-(feed, leg) rest_fetch_audit
/// aggregate; the METRICS block carries the dormant order-placement
/// histogram. Empty stdout (box stopped) degrades to an empty table + blank
/// shields — the page says so honestly instead of fabricating numbers.
pub fn parse_latency(stdout: &str) -> Value {
    let mut metrics: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut rest_rows: Vec<Value> = Vec::new();
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut in_metrics = false;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("RESTLAT_ROW=") {
            if let Some(row) = parse_rest_lat_row(rest) {
                rest_rows.push(row);
            }
            continue;
        }
        if line == "METRICS_BEGIN" {
            in_metrics = true;
            continue;
        }
        if line == "METRICS_END" {
            in_metrics = false;
            continue;
        }
        if in_metrics {
            let bits: Vec<&str> = line.split_whitespace().collect();
            if bits.len() == 2 {
                metrics.insert(bits[0].to_string(), bits[1].to_string());
            }
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            fields.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let order_avg = avg_ns(
        metrics
            .get("tv_order_placement_duration_ns_sum")
            .map_or("", String::as_str),
        metrics
            .get("tv_order_placement_duration_ns_count")
            .map_or("", String::as_str),
    );
    json!({
        // Per-(feed, leg) "how fast after each minute closed" rows. Empty
        // list = the box returned no rows (stopped box, empty fetch log, or
        // query failed) — the page says so honestly.
        "rest_latency": rest_rows,
        "questdb_ms": sec_to_ms(fields.get("QDB").map_or("", String::as_str)),
        "clock_skew_ms": sec_to_ms(fields.get("SKEW").map_or("", String::as_str)),
        "order_place_avg_ns": order_avg.map(round1),
    })
}

// --------------------------------------------------------------- storage snap

/// python: `_parse_storage` (handler.py:598-614) — parse df/du output into
/// disk_used/free/pct + db_size.
pub fn parse_storage(stdout: &str) -> Value {
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in stdout.lines() {
        if let Some((k, v)) = line.split_once('=') {
            fields.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let g = |k: &str| {
        fields
            .get(k)
            .map_or(String::new(), |v| v.replace('G', "").trim().to_string())
    };
    json!({
        "disk_used_gb": g("DISK_USED"),
        "disk_free_gb": g("DISK_FREE"),
        "disk_pct": fields.get("DISK_PCT").cloned().unwrap_or_default(),
        "db_size_gb": g("DB_SIZE"),
    })
}

// ----------------------------------------------------------------- feeds card

/// python: `_extract_marked_json` (handler.py:688-699) — extract + parse the
/// JSON between two marker lines. Returns (parsed_or_Null, error_str); never
/// fabricates a payload.
pub fn extract_marked_json(stdout: &str, begin: &str, end: &str) -> (Value, String) {
    let mut raw = "";
    if stdout.contains(begin)
        && stdout.contains(end)
        && let Some((_, after)) = stdout.split_once(begin)
    {
        // python `.split(end, 1)[0]` of a string WITHOUT `end` (marker before
        // begin) returns the whole remainder — mirrored by map_or below.
        raw = after.split_once(end).map_or(after, |(mid, _)| mid).trim();
    }
    if raw.is_empty() || raw.contains("TV_CURL_FAILED") {
        return (Value::Null, FEED_API_UNREACHABLE.to_string());
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => (v, String::new()),
        Err(_) => (Value::Null, "app API returned invalid JSON".to_string()),
    }
}

/// python: `_parse_rest_audit` (handler.py:702-723) — parse the REST_AUDIT
/// value (';'-joined `feed,outcome,count` CSV rows) into
/// {feed: {outcome: count_int}}. Charset-validated; malformed fragments are
/// skipped so an empty/absent value yields {} — the card then says "no pulls
/// recorded today" instead of fabricated zeros (audit Rule 11).
pub fn parse_rest_audit(raw: &str) -> Value {
    let mut out = serde_json::Map::new();
    for part in raw.split(';') {
        let parts: Vec<&str> = part
            .trim()
            .split(',')
            .map(|p| p.trim().trim_matches('"'))
            .collect();
        if parts.len() != 3 {
            continue;
        }
        let (feed, outcome, cnt) = (parts[0], parts[1], parts[2]);
        if !is_token(feed) || !is_token(outcome) {
            continue;
        }
        if cnt.is_empty() || !cnt.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(n) = cnt.parse::<i64>() else {
            continue;
        };
        if let Some(m) = out
            .entry(feed.to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
        {
            m.insert(outcome.to_string(), Value::Number(n.into()));
        }
    }
    Value::Object(out)
}

/// python: `_parse_rest_lat_hour` (handler.py:726-755) — parse the
/// REST_LAT_HOUR value (';'-joined `feed,p50,p99` CSV rows, milliseconds)
/// into {feed: {p50_ms, p99_ms}}. Malformed fragments skipped, never
/// fabricated.
pub fn parse_rest_lat_hour(raw: &str) -> Value {
    let mut out = serde_json::Map::new();
    for part in raw.split(';') {
        let parts: Vec<&str> = part
            .trim()
            .split(',')
            .map(|p| p.trim().trim_matches('"'))
            .collect();
        if parts.len() != 3 {
            continue;
        }
        let feed = parts[0];
        if !is_token(feed) {
            continue;
        }
        let (p50, p99) = (parse_num(parts[1]), parse_num(parts[2]));
        if p50.is_none() && p99.is_none() {
            continue;
        }
        out.insert(
            feed.to_string(),
            json!({"p50_ms": p50.map(round1), "p99_ms": p99.map(round1)}),
        );
    }
    Value::Object(out)
}

/// python: `_parse_feeds_view` (handler.py:758-798) — parse the marked
/// feeds-view snapshot. On any failure the matching *_error field carries a
/// structured reason and the payload is Null — the UI renders the error
/// verbatim instead of defaulting to zeros. The REST-lane pulse rides the
/// same snapshot as labeled lines OUTSIDE the marker blocks.
pub fn parse_feeds_view(stdout: &str) -> Value {
    if stdout.trim().is_empty() {
        return json!({
            "feeds": null,
            "feeds_error": BOX_UNREACHABLE,
            "health": null,
            "health_error": BOX_UNREACHABLE,
            "rest_audit": {},
            "rest_lat_hour": {},
        });
    }
    let (feeds, feeds_error) = extract_marked_json(stdout, "FEEDS_BEGIN", "FEEDS_END");
    let (health, health_error) =
        extract_marked_json(stdout, "FEEDS_HEALTH_BEGIN", "FEEDS_HEALTH_END");
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut in_block = false;
    for line in stdout.lines() {
        // Skip the marker-delimited JSON blocks — a JSON body containing '='
        // must never be mistaken for a labeled line.
        if line == "FEEDS_BEGIN" || line == "FEEDS_HEALTH_BEGIN" {
            in_block = true;
            continue;
        }
        if line == "FEEDS_END" || line == "FEEDS_HEALTH_END" {
            in_block = false;
            continue;
        }
        if in_block {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            fields.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    json!({
        "feeds": feeds,
        "feeds_error": feeds_error,
        "health": health,
        "health_error": health_error,
        "rest_audit": parse_rest_audit(fields.get("REST_AUDIT").map_or("", String::as_str)),
        "rest_lat_hour": parse_rest_lat_hour(fields.get("REST_LAT_HOUR").map_or("", String::as_str)),
    })
}

/// python: `_validate_feed_toggle` (handler.py:801-815) — strict validation
/// for the feed-toggle action. Returns "" when valid, else the rejection
/// reason. The feed name is charset-locked (lowercase token — injection-safe
/// for the shell/URL interpolation) but NOT allowlisted, so a future feed #3
/// toggles with zero portal changes; the app itself 400s unknown names.
/// `enabled` must be a JSON boolean — 1/"true"/null are rejected so nothing
/// ambiguous ever reaches the shell.
pub fn validate_feed_toggle(feed: &Value, enabled: &Value) -> String {
    let feed_ok = feed
        .as_str()
        .is_some_and(|s| FEED_NAME_RE.as_ref().is_some_and(|re| re.is_match(s)));
    if !feed_ok {
        return "feed must be a lowercase feed name like 'dhan' or 'groww'".to_string();
    }
    if !enabled.is_boolean() {
        return "enabled must be a JSON boolean (true or false)".to_string();
    }
    String::new()
}

/// python: `_feed_toggle_commands` (handler.py:818-835) — build the SSM curl
/// for POST /api/feeds/{feed}. Only called AFTER [`validate_feed_toggle`]
/// passed, so `feed` is a charset-locked lowercase token and `enabled` is a
/// real bool — the interpolations cannot carry shell metacharacters. The
/// body literal mirrors python `json.dumps({"enabled": enabled})` byte-exact
/// (`{"enabled": true}` — WITH the space; oracle golden).
pub fn feed_toggle_commands(feed: &str, enabled: bool) -> Vec<String> {
    let body = format!("{{\"enabled\": {enabled}}}");
    vec![
        "set +e".to_string(),
        // -w %{http_code} captures the app's status; the body goes to a temp
        // file so status + body are cleanly separable in the labeled output.
        format!(
            "echo \"FEED_TOGGLE_STATUS=$(curl -sS --max-time 8 -o /tmp/tv-feed-toggle.json \
-w %{{http_code}} -X POST -H 'content-type: application/json' -d '{body}' \
http://127.0.0.1:3001/api/feeds/{feed} 2>/dev/null)\""
        ),
        "echo \"FEED_TOGGLE_BODY_BEGIN\"".to_string(),
        "cat /tmp/tv-feed-toggle.json 2>/dev/null; echo".to_string(),
        "echo \"FEED_TOGGLE_BODY_END\"".to_string(),
        "rm -f /tmp/tv-feed-toggle.json".to_string(),
    ]
}

/// python: `_parse_feed_toggle` (handler.py:838-861) — parse the toggle
/// result: the app's HTTP status + its JSON body passed through VERBATIM
/// (incl. the 409 Dhan-guard message). curl prints 000 when it could not
/// connect — reported as unreachable, never success.
pub fn parse_feed_toggle(stdout: &str) -> Value {
    if stdout.trim().is_empty() {
        return json!({"app_status": null, "app_response": null, "error": BOX_UNREACHABLE});
    }
    let mut status: Option<i64> = None;
    for line in stdout.lines() {
        if let Some(raw) = line.strip_prefix("FEED_TOGGLE_STATUS=") {
            let raw = raw.trim();
            if !raw.is_empty()
                && raw.chars().all(|c| c.is_ascii_digit())
                && let Ok(n) = raw.parse::<i64>()
            {
                status = Some(n);
            }
        }
    }
    let mut body_raw = "";
    if stdout.contains("FEED_TOGGLE_BODY_BEGIN")
        && stdout.contains("FEED_TOGGLE_BODY_END")
        && let Some((_, after)) = stdout.split_once("FEED_TOGGLE_BODY_BEGIN")
    {
        body_raw = after
            .split_once("FEED_TOGGLE_BODY_END")
            .map_or(after, |(mid, _)| mid)
            .trim();
    }
    let body: Value = if body_raw.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body_raw)
            .unwrap_or_else(|_| json!({"raw": first_chars(body_raw, 500)}))
    };
    if status.is_none() || status == Some(0) {
        return json!({"app_status": null, "app_response": body, "error": FEED_API_UNREACHABLE});
    }
    json!({"app_status": status, "app_response": body, "error": ""})
}

// --------------------------------------------------------- deploy provenance

/// python: `_safe_provenance_sha` (handler.py:935-945) — hex-validate one
/// provenance sha (7-40 lowercase hex, else "unknown"), so a poisoned SSM
/// param / GitHub response can never smuggle markup into the footer.
pub fn safe_provenance_sha(sha: &Value) -> String {
    match sha.as_str() {
        Some(s) if HEX_SHA_RE.as_ref().is_some_and(|re| re.is_match(s)) => s.to_string(),
        _ => "unknown".to_string(),
    }
}

/// python: `_provenance_line` (handler.py:948-954) — pure formatter: short-7
/// provenance triple; every input hex-validated first.
pub fn provenance_line(binary_sha: &str, portal_sha: &str, main_sha: &str) -> String {
    let b = safe_provenance_sha(&Value::String(binary_sha.to_string()));
    let p = safe_provenance_sha(&Value::String(portal_sha.to_string()));
    let m = safe_provenance_sha(&Value::String(main_sha.to_string()));
    // Validated shas are ASCII (hex or "unknown") — byte slicing is safe.
    format!(
        "binary {} · portal {} · main {}",
        &b[..b.len().min(7)],
        &p[..p.len().min(7)],
        &m[..m.len().min(7)]
    )
}

/// python: `_provenance_footer_html` (handler.py:957-966) — defense in
/// depth: the line is already hex-validated per sha, and the assembled
/// string is ALSO html-escaped before splicing into the page.
pub fn provenance_footer_html(binary_sha: &str, portal_sha: &str, main_sha: &str) -> String {
    let line = html_escape(&provenance_line(binary_sha, portal_sha, main_sha));
    format!(
        "<footer style=\"margin-top:26px;text-align:center;font-size:11px;\
color:var(--mut);opacity:.75\">{line}</footer>"
    )
}

/// Pure half of the python `_main_sha` cache policy (handler.py:898-932):
/// is the cached value fresh enough to skip the GitHub fetch entirely?
pub fn main_sha_cache_fresh(cached: &str, age_secs: f64) -> bool {
    !cached.is_empty() && age_secs <= MAIN_SHA_TTL_SECS
}

/// Pure half of the python `_main_sha` failure arm (handler.py:927-932): on
/// a FAILED refresh, serve the cached value only while it is younger than
/// the 600s hard max-age; a sustained GitHub outage degrades to "unknown"
/// instead of an unboundedly stale sha.
pub fn resolve_main_sha_after_failed_fetch(cached: &str, age_secs: f64) -> String {
    if !cached.is_empty() && age_secs <= MAIN_SHA_MAX_AGE_SECS {
        cached.to_string()
    } else {
        "unknown".to_string()
    }
}

// --------------------------------------------------------------------- responses

/// python: `_resp` (handler.py:970-971). Ledger deviation: python
/// `json.dumps` emits `", "` / `": "` separators in insertion order; serde
/// emits compact separators in sorted key order — semantically identical
/// JSON (every consumer `JSON.parse`s / `json.loads`es the body; the ported
/// tests compare PARSED values, exactly like the python tests).
pub fn resp(status: u16, body: &Value) -> Value {
    json!({
        "statusCode": status,
        "headers": {"content-type": "application/json"},
        "body": body.to_string(),
    })
}

/// python: `_html_resp` (handler.py:974-991) — B9 deploy provenance: inject
/// the footer just before `</body>` so the giant static console template
/// stays untouched. (The python try/except fail-soft around the replace is
/// structurally unnecessary here — the Rust footer render is pure and
/// infallible.)
pub fn html_resp(footer_html: &str) -> Value {
    let html = CONSOLE_HTML.replacen("</body>", &format!("{footer_html}\n</body>"), 1);
    json!({
        "statusCode": 200,
        "headers": {
            "content-type": "text/html; charset=utf-8",
            "cache-control": "no-store",
            "referrer-policy": "no-referrer",
        },
        "body": html,
    })
}

// ------------------------------------------------------------------ the shell

/// Side-effect surface of the router — the python oracle's monkeypatch seams
/// (`_control_secret` / `_is_market_hours` / `_ssm_shell` / `_ssm_shell_sync`
/// / `_client(...)` / env reads) as ONE trait, so [`route`] is a pure
/// function of (event, shell). Tests implement a MockShell mirroring the
/// python monkeypatching; production uses [`AwsShell`].
// APPROVED: handler is generic over the trait (no dyn); futures awaited inline.
#[allow(async_fn_in_trait)]
pub trait OpsShell {
    /// python `_control_secret()` — SSM SecureString, 60s-cached.
    async fn control_secret(&self) -> String;
    /// python `_is_market_hours(datetime.datetime.utcnow())`.
    fn market_hours_now(&self) -> bool;
    /// python `int(time.time())`.
    fn now_epoch(&self) -> i64;
    /// python `os.environ.get(key, "")`.
    fn env(&self, key: &str) -> String;
    /// python `_client("ec2").start_instances(...)`.
    async fn ec2_start(&self) -> Result<(), String>;
    /// python `_client("ec2").stop_instances(...)`.
    async fn ec2_stop(&self) -> Result<(), String>;
    /// python `_client("ec2").reboot_instances(...)`.
    async fn ec2_reboot(&self) -> Result<(), String>;
    /// python `describe_instances(...)["Reservations"][0]["Instances"][0]["State"]["Name"]`.
    async fn instance_state(&self) -> Result<String, String>;
    /// python `_ssm_shell(commands)` → CommandId (Err → the 500 arm).
    async fn ssm_shell(&self, commands: &[String]) -> Result<String, String>;
    /// python `_ssm_shell_sync(commands, timeout)` → stdout+stderr, "" on
    /// any failure (box stopped / SSM offline) — callers degrade to an empty
    /// snapshot, never a 500.
    async fn ssm_shell_sync(&self, commands: &[String], timeout_secs: f64) -> String;
    /// python `get_command_invocation(...)` → (Status, StdOut, StdErr);
    /// Err → the Pending 200 arm (not registered yet / box offline).
    async fn command_invocation(
        &self,
        command_id: &str,
    ) -> Result<(String, String, String), String>;
    /// python `describe_alarms(StateValue="ALARM", MaxRecords=20)` → alarm
    /// names; None on failure (fail-soft — the UI shows null).
    async fn firing_alarms(&self) -> Option<Vec<String>>;
    /// python `_month_to_date_cost()` — "" on any failure.
    async fn month_to_date_cost(&self) -> String;
    /// python `_binary_sha()` — fail-soft to "unknown".
    async fn binary_sha(&self) -> String;
    /// python `_portal_sha()` — env PORTAL_GIT_SHA or "unknown".
    fn portal_sha(&self) -> String;
    /// python `_main_sha()` — GitHub main HEAD, 60s cache / 600s max-age,
    /// fail-soft to "unknown".
    async fn main_sha(&self) -> String;
}

/// python 500 arm: `except Exception: _resp(500, {"error": "action failed",
/// "action": action})`.
fn action_failed(action: &str) -> Value {
    resp(500, &json!({"error": "action failed", "action": action}))
}

/// Merge `extra`'s object fields into `base` (python `{**a, **b}` splat).
fn merged(base: Value, extra: &Value) -> Value {
    let mut m = base.as_object().cloned().unwrap_or_default();
    if let Some(e) = extra.as_object() {
        for (k, v) in e {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

/// python: `lambda_handler` (handler.py:994-1489) — the full router, ported
/// branch-for-branch (same gate order, same response bodies, same status
/// codes). Generic over the shell so the ported routing tests drive it with
/// a mock exactly like the python tests monkeypatched the module globals.
pub async fn route<S: OpsShell>(event: &Value, shell: &S) -> Value {
    if http_method(event) == "GET" {
        let footer = provenance_footer_html(
            &shell.binary_sha().await,
            &shell.portal_sha(),
            &shell.main_sha().await,
        );
        return html_resp(&footer);
    }

    let headers = event.get("headers").cloned().unwrap_or(Value::Null);
    let secret = shell.control_secret().await;
    if !authorized(&headers, &secret) {
        return resp(401, &json!({"error": "unauthorized"}));
    }

    // python: `raw = event.get("body") or "{}"`; dict passthrough, else
    // json.loads (fail → 400). Ledger deviation: a parsed NON-OBJECT payload
    // (e.g. `[1]`) is treated as {} — python would AttributeError → 502
    // (untested edge, no real caller sends one).
    let payload: Value = match event.get("body") {
        Some(Value::Object(o)) => Value::Object(o.clone()),
        Some(Value::String(s)) if !s.is_empty() => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(o)) => Value::Object(o),
            Ok(_) => json!({}),
            Err(_) => return resp(400, &json!({"error": "invalid JSON body"})),
        },
        _ => json!({}),
    };

    let action = payload_str(&payload, "action").to_string();
    let force = truthy(payload.get("force").unwrap_or(&Value::Bool(false)));

    // HARD gate first: data-destructive actions have NO force escape during
    // market hours (audit fix #2 — see DATA_DESTRUCTIVE above). Must run
    // BEFORE the soft gate below because DATA_DESTRUCTIVE ⊂ DESTRUCTIVE.
    if DATA_DESTRUCTIVE.contains(&action.as_str()) && shell.market_hours_now() {
        return resp(
            409,
            &json!({
                "error": DATA_DESTRUCTIVE_LOCK_MSG,
                "action": action,
                "market_hours_locked": true,
            }),
        );
    }
    if DESTRUCTIVE.contains(&action.as_str()) && shell.market_hours_now() && !force {
        return resp(
            409,
            &json!({
                "error": "blocked during market hours (09:15-15:30 IST). Re-send with {\"force\": true} to override.",
                "action": action,
            }),
        );
    }

    match action.as_str() {
        // ---- box control ----
        "start" => match shell.ec2_start().await {
            Ok(()) => resp(200, &json!({"ok": true, "action": action})),
            Err(exc) => {
                tracing::error!(%action, %exc, "operator-portal action failed");
                action_failed(&action)
            }
        },
        "stop" => match shell.ec2_stop().await {
            Ok(()) => resp(200, &json!({"ok": true, "action": action})),
            Err(exc) => {
                tracing::error!(%action, %exc, "operator-portal action failed");
                action_failed(&action)
            }
        },
        "reboot" => match shell.ec2_reboot().await {
            Ok(()) => resp(200, &json!({"ok": true, "action": action})),
            Err(exc) => {
                tracing::error!(%action, %exc, "operator-portal action failed");
                action_failed(&action)
            }
        },
        "restart-app" => {
            match shell
                .ssm_shell(&["systemctl restart tickvault".to_string()])
                .await
            {
                Ok(cid) => resp(
                    200,
                    &json!({"ok": true, "action": action, "command_id": cid}),
                ),
                Err(exc) => {
                    tracing::error!(%action, %exc, "operator-portal action failed");
                    action_failed(&action)
                }
            }
        }
        "stop-app" => {
            let cmds = [
                "systemctl stop tickvault || true".to_string(),
                "systemctl disable tickvault || true".to_string(),
            ];
            match shell.ssm_shell(&cmds).await {
                Ok(cid) => resp(
                    200,
                    &json!({"ok": true, "action": action, "command_id": cid}),
                ),
                Err(exc) => {
                    tracing::error!(%action, %exc, "operator-portal action failed");
                    action_failed(&action)
                }
            }
        }
        "restart-questdb" => {
            // ensure-questdb.sh (create-or-restart) — robust to
            // docker-compose v1/v2 absence + the CORRECT service name +
            // SSM creds for the recreate case (incident 2026-06-08).
            let cmds = ["bash /opt/tickvault/repo/scripts/ensure-questdb.sh".to_string()];
            match shell.ssm_shell(&cmds).await {
                Ok(cid) => resp(
                    200,
                    &json!({"ok": true, "action": action, "command_id": cid}),
                ),
                Err(exc) => {
                    tracing::error!(%action, %exc, "operator-portal action failed");
                    action_failed(&action)
                }
            }
        }
        "command-status" => {
            // READ-ONLY (not in DESTRUCTIVE, allowed off-hours, no force).
            // Lets the UI poll the REAL outcome of an async SSM command
            // (e.g. the docker-reset nuke) instead of showing a fake "done"
            // the instant it is dispatched — no more false-OK.
            let cid = payload_str(&payload, "command_id").to_string();
            if cid.is_empty() {
                return resp(
                    400,
                    &json!({"error": "command-status requires command_id", "action": action}),
                );
            }
            match shell.command_invocation(&cid).await {
                // Not an error: the command may not have propagated to SSM yet.
                Err(_) => resp(
                    200,
                    &json!({"ok": true, "action": action, "status": "Pending", "stdout_tail": ""}),
                ),
                Ok((status, stdout, stderr)) => {
                    let out = format!("{stdout}{stderr}");
                    resp(
                        200,
                        &json!({
                            "ok": true,
                            "action": action,
                            "status": status,
                            "stdout_tail": last_chars(&out, 1500),
                        }),
                    )
                }
            }
        }
        "wipe-questdb" => {
            // DESTRUCTIVE: empties the market-data tables + EVERY feed's
            // capture/replay files for a fresh start (feed-agnostic
            // resurrect-proof rewrite — operator incident 2026-07-02).
            // Requires force=true (even off-hours) + the typed confirm token
            // (PR-5 H-1: a scripted call with a stolen bearer can no longer
            // fire this). Command list: WIPE_QUESTDB_COMMANDS (oracle golden).
            if !force {
                return resp(
                    409,
                    &json!({
                        "error": "wipe is destructive — re-send with {\"force\": true}",
                        "action": action,
                    }),
                );
            }
            if payload_str(&payload, "confirm") != "WIPE" {
                return resp(
                    409,
                    &json!({
                        "error": "wipe-questdb deletes every tick and candle — re-send with {\"confirm\": \"WIPE\"}",
                        "action": action,
                    }),
                );
            }
            let cmds: Vec<String> = crate::operator_control_action_commands::WIPE_QUESTDB_COMMANDS
                .iter()
                .map(|c| (*c).to_string())
                .collect();
            match shell.ssm_shell(&cmds).await {
                Ok(cid) => resp(
                    200,
                    &json!({"ok": true, "action": action, "command_id": cid}),
                ),
                Err(exc) => {
                    tracing::error!(%action, %exc, "operator-portal action failed");
                    action_failed(&action)
                }
            }
        }
        // (wipe-groww removed 2026-07-16 — the groww-only wipe retired with
        // the Groww live feed; a POST now falls through to unknown-action 400.)
        "docker-reset" => {
            // MOST DESTRUCTIVE: the FULL DOCKER NUKE — full Docker teardown +
            // fresh rebuild (operator Option B, 2026-06-04). Unlike
            // wipe-questdb (TRUNCATE rows, keeps audit tables for SEBI),
            // this DELETES the Docker volumes + images entirely — EVERY
            // table incl. the SEBI-retention audit tables. The operator
            // explicitly chose this; the UI asks the operator to TYPE the
            // confirm phrase and spells out the audit-data loss.
            if payload_str(&payload, "confirm") != "NUKE-DOCKER" {
                return resp(
                    409,
                    &json!({
                        "error": "docker-reset is the FULL DOCKER NUKE (deletes containers + volumes + images = ALL data incl. SEBI audit tables, then fresh start) — re-send with {\"confirm\": \"NUKE-DOCKER\"}",
                        "action": action,
                    }),
                );
            }
            let cmds: Vec<String> = crate::operator_control_action_commands::DOCKER_RESET_COMMANDS
                .iter()
                .map(|c| (*c).to_string())
                .collect();
            match shell.ssm_shell(&cmds).await {
                Ok(cid) => {
                    // Audit line → CloudWatch (python `print` → tracing; the
                    // charter bans print macros; same log-group destination).
                    tracing::info!(
                        command_id = cid,
                        force,
                        "operator-portal AUDIT: docker-reset (FULL DOCKER NUKE) dispatched"
                    );
                    resp(
                        200,
                        &json!({"ok": true, "action": action, "command_id": cid}),
                    )
                }
                Err(exc) => {
                    tracing::error!(%action, %exc, "operator-portal action failed");
                    action_failed(&action)
                }
            }
        }
        "docker-nuke-bare" => {
            // TRUE BARE WIPE (operator request 2026-06-12): like deleting all
            // containers + images + volumes by hand — everything GONE and it
            // STAYS gone; no rebuild, no app restart.
            if !force {
                return resp(
                    409,
                    &json!({
                        "error": "docker-nuke-bare deletes ALL containers+images+volumes and LEAVES THE BOX DEAD (no rebuild, no app). re-send with {\"force\": true}",
                        "action": action,
                    }),
                );
            }
            if payload_str(&payload, "confirm") != "ERASE" {
                return resp(
                    409,
                    &json!({
                        "error": "docker-nuke-bare leaves the box BARE and DEAD — re-send with {\"confirm\": \"ERASE\"}",
                        "action": action,
                    }),
                );
            }
            let cmds: Vec<String> =
                crate::operator_control_action_commands::DOCKER_NUKE_BARE_COMMANDS
                    .iter()
                    .map(|c| (*c).to_string())
                    .collect();
            match shell.ssm_shell(&cmds).await {
                Ok(cid) => resp(
                    200,
                    &json!({"ok": true, "action": action, "command_id": cid}),
                ),
                Err(exc) => {
                    tracing::error!(%action, %exc, "operator-portal action failed");
                    action_failed(&action)
                }
            }
        }
        // ---- overview / data ----
        "status" | "view" => {
            let state = match shell.instance_state().await {
                Ok(s) => s,
                Err(exc) => {
                    tracing::error!(%action, %exc, "operator-portal action failed");
                    return action_failed(&action);
                }
            };
            // The on-box snapshot needs the SSM agent, only reachable while
            // the instance is RUNNING; off-hours we skip SSM entirely and
            // return a clean "stopped" view instead of 500ing.
            let stdout = if state == "running" {
                let cmds: Vec<String> = crate::operator_control_commands::VIEW_COMMANDS
                    .iter()
                    .map(|c| (*c).to_string())
                    .collect();
                shell.ssm_shell_sync(&cmds, VIEW_TIMEOUT_SECS).await
            } else {
                String::new()
            };
            let snap = parse_view(&stdout);
            let mh = shell.market_hours_now();
            resp(
                200,
                &merged(
                    json!({"ok": true, "action": "view", "instance_state": state, "market_hours": mh}),
                    &snap,
                ),
            )
        }
        "sql" => {
            // READ-ONLY console query (Data tab box + DB tab). Writes are
            // blocked server-side by is_safe_sql; cap_sql_rows clamps/appends
            // LIMIT; the /exp `limit=` param + `head` bound the output
            // regardless.
            let q = payload_str(&payload, "query").to_string();
            if !is_safe_sql(&q) {
                return resp(
                    400,
                    &json!({"error": "only read-only single-statement SELECT/SHOW/EXPLAIN/WITH queries are allowed (no ';' chaining, no comments)"}),
                );
            }
            let q = cap_sql_rows(&q);
            let enc = url_quote(&q);
            let cmds = [
                "set +e".to_string(),
                format!(
                    "curl -fsS 'http://127.0.0.1:9000/exp?query={enc}&limit={SQL_MAX_ROWS}' 2>/dev/null | head -{} || echo 'query failed'",
                    SQL_MAX_ROWS + 1
                ),
            ];
            let out = shell.ssm_shell_sync(&cmds, VIEW_TIMEOUT_SECS).await;
            resp(200, &json!({"ok": true, "action": "sql", "csv": out}))
        }
        "qdb_console_url" => {
            // READ-ONLY: mint a one-click 90s HMAC link to the B4 QuestDB
            // console (questdb-console-front Lambda). Same control secret
            // signs the token; the console verifies it constant-time and
            // swaps it for a 12h session cookie. Env QDB_CONSOLE_URL is
            // injected by Terraform only when var.enable_questdb_console.
            let base_env = shell.env("QDB_CONSOLE_URL");
            let base = base_env.trim_end_matches('/');
            if base.is_empty() {
                return resp(
                    400,
                    &json!({"error": "console not enabled", "action": action}),
                );
            }
            let tok = mint_qdb_link_token_default_ttl(&secret, shell.now_epoch());
            resp(
                200,
                &json!({"ok": true, "action": action, "url": format!("{base}/open?tok={tok}")}),
            )
        }
        "logs" => {
            // KEPT even though its tab is gone: the tickvault-logs MCP
            // server's cloudwatch_logs tool POSTs {"action":"logs"} here.
            let cmds: Vec<String> = crate::operator_control_action_commands::LOGS_COMMANDS
                .iter()
                .map(|c| (*c).to_string())
                .collect();
            let out = shell.ssm_shell_sync(&cmds, VIEW_TIMEOUT_SECS).await;
            resp(200, &json!({"ok": true, "action": "logs", "raw": out}))
        }
        "latency" => {
            let cmds: Vec<String> = crate::operator_control_commands::LATENCY_COMMANDS
                .iter()
                .map(|c| (*c).to_string())
                .collect();
            let out = shell.ssm_shell_sync(&cmds, LATENCY_TIMEOUT_SECS).await;
            resp(
                200,
                &merged(
                    json!({"ok": true, "action": "latency"}),
                    &parse_latency(&out),
                ),
            )
        }
        // (cross_verify removed 2026-07-16 — the 15:31 IST Dhan live-vs-
        // historical comparer was deleted in PR-C3 2026-07-14; unknown-action
        // 400 now.)
        "feeds-view" => {
            // READ-ONLY: current per-feed enabled/lane state + per-feed
            // health, via SSM-curl of the app's local API (SG keeps :3001
            // closed).
            let cmds: Vec<String> = crate::operator_control_commands::FEEDS_VIEW_COMMANDS
                .iter()
                .map(|c| (*c).to_string())
                .collect();
            let out = shell.ssm_shell_sync(&cmds, FEEDS_TIMEOUT_SECS).await;
            resp(
                200,
                &merged(
                    json!({"ok": true, "action": action}),
                    &parse_feeds_view(&out),
                ),
            )
        }
        "feed-toggle" => {
            // MUTATING but feed-scoped (not box-destructive): flips one
            // feed's runtime flag via the app's POST /api/feeds/{feed}. The
            // dangerous direction (disabling Dhan during live trading) is
            // guarded by the APP's own 409 — passed through verbatim below.
            // Not in DESTRUCTIVE: the app is the authority on when a flip is
            // unsafe.
            let feed = payload.get("feed").cloned().unwrap_or(Value::Null);
            let enabled = payload.get("enabled").cloned().unwrap_or(Value::Null);
            let verr = validate_feed_toggle(&feed, &enabled);
            if !verr.is_empty() {
                return resp(400, &json!({"error": verr, "action": action}));
            }
            // Validated above: feed is a str token, enabled is a bool.
            let feed_str = feed.as_str().unwrap_or_default();
            let enabled_bool = enabled.as_bool().unwrap_or_default();
            let cmds = feed_toggle_commands(feed_str, enabled_bool);
            let out = shell.ssm_shell_sync(&cmds, FEEDS_TIMEOUT_SECS).await;
            let r = parse_feed_toggle(&out);
            let ok = r.get("app_status").and_then(Value::as_i64) == Some(200);
            resp(
                200,
                &merged(
                    json!({"ok": ok, "action": action, "feed": feed, "enabled": enabled}),
                    &r,
                ),
            )
        }
        // ---- aws ----
        "aws_status" => {
            // Each read is independently fail-soft so a throttle on one
            // doesn't 500 the whole tab.
            let firing = match shell.firing_alarms().await {
                Some(names) => json!(names),
                None => Value::Null,
            };
            let cmds: Vec<String> = crate::operator_control_commands::STORAGE_COMMANDS
                .iter()
                .map(|c| (*c).to_string())
                .collect();
            let storage = parse_storage(&shell.ssm_shell_sync(&cmds, VIEW_TIMEOUT_SECS).await);
            resp(
                200,
                &merged(
                    json!({
                        "ok": true,
                        "action": action,
                        "alarms_firing": firing,
                        "cost_mtd_usd": shell.month_to_date_cost().await,
                    }),
                    &storage,
                ),
            )
        }
        _ => resp(
            400,
            &json!({"error": format!("unknown action: '{action}'")}),
        ),
    }
}

// ------------------------------------------------------------------ AWS shell

/// Production [`OpsShell`] backed by the real AWS SDKs — UNPROVEN until
/// deploy (exercised only in a live Lambda invoke; the pure router/parsers
/// it feeds are what the unit tests cover, exactly like the python oracle's
/// boto3 glue).
pub struct AwsShell {
    ec2: aws_sdk_ec2::Client,
    ssm: aws_sdk_ssm::Client,
    cloudwatch: aws_sdk_cloudwatch::Client,
    ce: aws_sdk_costexplorer::Client,
    /// None when the client build failed (HTTP-CLIENT-01 class — logged
    /// loudly at build; provenance then degrades to "unknown", never a
    /// panic-class `Client::new()` fallback).
    http: Option<reqwest::Client>,
    instance_id: String,
    secret_param: String,
    binary_sha_param: String,
    gh_repo: String,
    gh_token_param: String,
    /// python `_cache` — 60s-TTL SSM param cache.
    param_cache: std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
    /// python `_main_sha_cache` — (value, fetched_at).
    main_sha_cache: std::sync::Mutex<(String, Option<std::time::Instant>)>,
}

fn lock_unpoisoned<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl AwsShell {
    /// Build the live shell from the Lambda execution-role environment —
    /// python module-init parity (`INSTANCE_ID` / `_SECRET_PARAM` /
    /// provenance env vars).
    pub async fn new() -> Self {
        let config = crate::clients::sdk_config().await;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3)) // APPROVED: python parity — urllib timeout=3 on the provenance GitHub read.
            .build()
            .map_err(|err| {
                // HTTP-CLIENT-01 class: never a panic-class Client::new()
                // fallback — provenance degrades to "unknown".
                tracing::error!(code = "HTTP-CLIENT-01", error = %err, site = "operator_control_provenance", "reqwest client build failed");
            })
            .ok();
        Self {
            ec2: crate::clients::ec2(&config),
            ssm: crate::clients::ssm(&config),
            cloudwatch: crate::clients::cloudwatch(&config),
            ce: crate::clients::cost_explorer_us_east_1().await,
            http,
            instance_id: std::env::var("TV_INSTANCE_ID").unwrap_or_default(),
            secret_param: std::env::var("OPERATOR_CONTROL_SECRET_PARAM").unwrap_or_default(),
            binary_sha_param: std::env::var("BINARY_SHA_PARAM")
                .unwrap_or_else(|_| "/tickvault/prod/deploy/binary-git-sha".to_string()),
            gh_repo: std::env::var("GH_REPO").unwrap_or_else(|_| "SJParthi/tickvault".to_string()),
            gh_token_param: std::env::var("OPERATOR_GITHUB_TOKEN_PARAM").unwrap_or_default(),
            param_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            main_sha_cache: std::sync::Mutex::new((String::new(), None)),
        }
    }

    /// python `_load_param` — "" on any failure (missing ⇒ deny / disable,
    /// fail closed).
    async fn load_param(&self, param: &str) -> String {
        if param.is_empty() {
            return String::new();
        }
        match self
            .ssm
            .get_parameter()
            .name(param)
            .with_decryption(true)
            .send()
            .await
        {
            Ok(out) => out.parameter.and_then(|p| p.value).unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    /// python `_cached_param` — refetch when absent, empty, or older than
    /// `SECRET_TTL_SECS`.
    async fn cached_param(&self, param: &str) -> String {
        {
            let cache = lock_unpoisoned(&self.param_cache);
            if let Some((value, ts)) = cache.get(param)
                && !value.is_empty()
                && ts.elapsed().as_secs_f64() <= SECRET_TTL_SECS
            {
                return value.clone();
            }
        }
        let value = self.load_param(param).await;
        lock_unpoisoned(&self.param_cache).insert(
            param.to_string(),
            (value.clone(), std::time::Instant::now()),
        );
        value
    }

    /// One bounded GitHub main-HEAD read (python `_main_sha` fetch leg).
    async fn fetch_main_sha(&self) -> Option<String> {
        let http = self.http.as_ref()?;
        let url = format!(
            "https://api.github.com/repos/{}/commits/main", // APPROVED: fixed infrastructure host — python parity (handler.py:911), token-free URL.
            self.gh_repo
        );
        let mut req = http
            .get(url)
            .header("accept", "application/vnd.github+json")
            .header("user-agent", "tickvault-operator-portal");
        let token = self.cached_param(&self.gh_token_param).await;
        if !token.is_empty() {
            req = req.header("authorization", format!("Bearer {token}"));
        }
        let body: Value = req.send().await.ok()?.json().await.ok()?;
        let sha = body.get("sha").and_then(Value::as_str).unwrap_or("").trim();
        if sha.is_empty() {
            None
        } else {
            Some(sha.to_string())
        }
    }
}

impl OpsShell for AwsShell {
    async fn control_secret(&self) -> String {
        self.cached_param(&self.secret_param).await
    }

    fn market_hours_now(&self) -> bool {
        is_market_hours(Utc::now())
    }

    fn now_epoch(&self) -> i64 {
        Utc::now().timestamp()
    }

    fn env(&self, key: &str) -> String {
        std::env::var(key).unwrap_or_default()
    }

    async fn ec2_start(&self) -> Result<(), String> {
        self.ec2
            .start_instances()
            .instance_ids(&self.instance_id)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    }

    async fn ec2_stop(&self) -> Result<(), String> {
        self.ec2
            .stop_instances()
            .instance_ids(&self.instance_id)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    }

    async fn ec2_reboot(&self) -> Result<(), String> {
        self.ec2
            .reboot_instances()
            .instance_ids(&self.instance_id)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    }

    async fn instance_state(&self) -> Result<String, String> {
        let out = self
            .ec2
            .describe_instances()
            .instance_ids(&self.instance_id)
            .send()
            .await
            .map_err(|e| format!("{e:?}"))?;
        // python `["Reservations"][0]["Instances"][0]["State"]["Name"]` —
        // a missing element raised (→ 500); Err mirrors that.
        out.reservations()
            .first()
            .and_then(|r| r.instances().first())
            .and_then(|i| i.state())
            .and_then(|s| s.name())
            .map(|n| n.as_str().to_string())
            .ok_or_else(|| "describe_instances: no instance state".to_string())
    }

    async fn ssm_shell(&self, commands: &[String]) -> Result<String, String> {
        let out = self
            .ssm
            .send_command()
            .instance_ids(&self.instance_id)
            .document_name("AWS-RunShellScript")
            .parameters("commands", commands.to_vec())
            .send()
            .await
            .map_err(|e| format!("{e:?}"))?;
        out.command()
            .and_then(|c| c.command_id())
            .map(ToString::to_string)
            .ok_or_else(|| "send_command: no CommandId".to_string())
    }

    async fn ssm_shell_sync(&self, commands: &[String], timeout_secs: f64) -> String {
        // python `_ssm_shell_sync` (handler.py:301-321): "" on any failure —
        // callers degrade to an empty snapshot, never a 500.
        let cid = match self.ssm_shell(commands).await {
            Ok(cid) => cid,
            Err(exc) => {
                tracing::warn!(
                    exc,
                    "operator-portal ssm send_command failed (box offline?)"
                );
                return String::new();
            }
        };
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs_f64(timeout_secs.max(0.0));
        while std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_secs_f64(VIEW_POLL_SECS)).await;
            let Ok((status, stdout, stderr)) = self.command_invocation(&cid).await else {
                continue; // not registered yet
            };
            if matches!(
                status.as_str(),
                "Success" | "Failed" | "Cancelled" | "TimedOut"
            ) {
                return format!("{stdout}{stderr}");
            }
        }
        String::new()
    }

    async fn command_invocation(
        &self,
        command_id: &str,
    ) -> Result<(String, String, String), String> {
        let inv = self
            .ssm
            .get_command_invocation()
            .command_id(command_id)
            .instance_id(&self.instance_id)
            .send()
            .await
            .map_err(|e| format!("{e:?}"))?;
        Ok((
            inv.status()
                .map(|s| s.as_str().to_string())
                .unwrap_or_default(),
            inv.standard_output_content()
                .unwrap_or_default()
                .to_string(),
            inv.standard_error_content().unwrap_or_default().to_string(),
        ))
    }

    async fn firing_alarms(&self) -> Option<Vec<String>> {
        match self
            .cloudwatch
            .describe_alarms()
            .state_value(aws_sdk_cloudwatch::types::StateValue::Alarm)
            .max_records(20)
            .send()
            .await
        {
            Ok(out) => Some(
                out.metric_alarms()
                    .iter()
                    .filter_map(|a| a.alarm_name().map(ToString::to_string))
                    .collect(),
            ),
            Err(exc) => {
                tracing::warn!(exc = ?exc, "operator-portal describe_alarms failed");
                None
            }
        }
    }

    async fn month_to_date_cost(&self) -> String {
        // python `_month_to_date_cost` (handler.py:1492-1507) — "" on any
        // failure. Lambda env clock is UTC (python date.today() parity).
        let today = Utc::now().date_naive();
        let Some(first) = today.with_day(1) else {
            return String::new();
        };
        let Some(end) = today.succ_opt() else {
            return String::new();
        };
        let Ok(period) = aws_sdk_costexplorer::types::DateInterval::builder()
            .start(first.format("%Y-%m-%d").to_string())
            .end(end.format("%Y-%m-%d").to_string())
            .build()
        else {
            return String::new();
        };
        let Ok(out) = self
            .ce
            .get_cost_and_usage()
            .time_period(period)
            .granularity(aws_sdk_costexplorer::types::Granularity::Monthly)
            .metrics("UnblendedCost")
            .send()
            .await
        else {
            return String::new();
        };
        out.results_by_time()
            .first()
            .and_then(|r| r.total())
            .and_then(|t| t.get("UnblendedCost"))
            .and_then(|m| m.amount())
            .and_then(|a| a.parse::<f64>().ok())
            .map(|f| format!("{f:.2}"))
            .unwrap_or_default()
    }

    async fn binary_sha(&self) -> String {
        let v = self.cached_param(&self.binary_sha_param).await;
        let t = v.trim();
        if t.is_empty() {
            "unknown".to_string()
        } else {
            t.to_string()
        }
    }

    fn portal_sha(&self) -> String {
        let v = std::env::var("PORTAL_GIT_SHA").unwrap_or_default();
        let t = v.trim();
        if t.is_empty() {
            "unknown".to_string()
        } else {
            t.to_string()
        }
    }

    async fn main_sha(&self) -> String {
        let now = std::time::Instant::now();
        {
            let cache = lock_unpoisoned(&self.main_sha_cache);
            let age = cache
                .1
                .map_or(f64::INFINITY, |ts| now.duration_since(ts).as_secs_f64());
            if main_sha_cache_fresh(&cache.0, age) {
                return cache.0.clone();
            }
        }
        if let Some(sha) = self.fetch_main_sha().await {
            *lock_unpoisoned(&self.main_sha_cache) = (sha.clone(), Some(now));
            return sha;
        }
        let cache = lock_unpoisoned(&self.main_sha_cache);
        let age = cache
            .1
            .map_or(f64::INFINITY, |ts| now.duration_since(ts).as_secs_f64());
        resolve_main_sha_after_failed_fetch(&cache.0, age)
    }
}

static AWS_SHELL: tokio::sync::OnceCell<AwsShell> = tokio::sync::OnceCell::const_new();

/// Entry point invoked by the thin `operator-control` bin — builds the live
/// AWS shell once per Lambda container and delegates to the pure router.
pub async fn handle(event: Value) -> Result<Value, lambda_runtime::Error> {
    let shell = AWS_SHELL.get_or_init(AwsShell::new).await;
    Ok(route(&event, shell).await)
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
    fn test_console_shared_corpus_pins() {
        // Cross-language freeze with the console lambdas' Python suites
        // (deploy/aws/lambda/questdb-console-{front,proxy}/test_handler.py):
        // with the Python operator-control oracle retired, those suites pin
        // FROZEN verdicts on a shared corpus — this test pins the same three
        // entries the pre-port oracle comparison covered but the inline
        // SafeSql tests did not.
        assert!(is_safe_sql("select 1 ;")); // space before the ONE stripped ';'
        assert!(is_safe_sql("select 1 union all select 2")); // no mutator word
        assert!(!is_safe_sql("(select 1)")); // leading non-letter → empty first word
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

    // =====================================================================
    // Full 1:1 port of the remaining python test classes
    // (deploy/aws/lambda/operator-control/test_handler.py). Test NAMES match
    // the python names byte-for-byte so the parity check is mechanical.
    // =====================================================================

    use crate::operator_control_action_commands::{
        DOCKER_NUKE_BARE_COMMANDS, DOCKER_RESET_COMMANDS, WIPE_QUESTDB_COMMANDS,
    };
    use crate::operator_control_commands::{
        FEEDS_VIEW_COMMANDS, LATENCY_COMMANDS, REST_LATENCY_SQL, VIEW_COMMANDS,
    };

    // ------------------------------------------------------------ MockShell
    // The python tests monkeypatched module globals (`_control_secret`,
    // `_is_market_hours`, `_ssm_shell`, `_ssm_shell_sync`, `_client`); the
    // Rust router is generic over [`OpsShell`], so ONE mock mirrors every
    // seam. Defaults mirror the python fixtures: secret "s3cret-token",
    // market CLOSED (WipeGate pins the clock off-hours), instance running.
    struct MockShell {
        secret: String,
        market_hours: bool,
        now: i64,
        env: std::collections::HashMap<String, String>,
        ssm_result: Result<String, String>,
        /// `Some(msg)` = the python `self.fail("SSM must not be called…")`
        /// stub — any ssm_shell call panics the test.
        forbid_ssm: Option<&'static str>,
        sync_output: String,
        instance_state: Result<String, String>,
        ec2_ok: bool,
        invocation: Result<(String, String, String), String>,
        alarms: Option<Vec<String>>,
        cost: String,
        captured: std::sync::Mutex<Vec<Vec<String>>>,
        captured_sync: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl Default for MockShell {
        fn default() -> Self {
            Self {
                secret: AUTH_SECRET.to_string(),
                market_hours: false,
                now: 1_780_000_000,
                env: std::collections::HashMap::new(),
                ssm_result: Ok("cid-1".to_string()),
                forbid_ssm: None,
                sync_output: String::new(),
                instance_state: Ok("running".to_string()),
                ec2_ok: true,
                invocation: Err("not registered".to_string()),
                alarms: Some(Vec::new()),
                cost: String::new(),
                captured: std::sync::Mutex::new(Vec::new()),
                captured_sync: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl MockShell {
        fn captured_joined(&self) -> String {
            self.captured
                .lock()
                .unwrap()
                .iter()
                .map(|cmds| cmds.join("\n"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    impl OpsShell for MockShell {
        async fn control_secret(&self) -> String {
            self.secret.clone()
        }
        fn market_hours_now(&self) -> bool {
            self.market_hours
        }
        fn now_epoch(&self) -> i64 {
            self.now
        }
        fn env(&self, key: &str) -> String {
            self.env.get(key).cloned().unwrap_or_default()
        }
        async fn ec2_start(&self) -> Result<(), String> {
            if self.ec2_ok {
                Ok(())
            } else {
                Err("ec2 down".to_string())
            }
        }
        async fn ec2_stop(&self) -> Result<(), String> {
            if self.ec2_ok {
                Ok(())
            } else {
                Err("ec2 down".to_string())
            }
        }
        async fn ec2_reboot(&self) -> Result<(), String> {
            if self.ec2_ok {
                Ok(())
            } else {
                Err("ec2 down".to_string())
            }
        }
        async fn instance_state(&self) -> Result<String, String> {
            self.instance_state.clone()
        }
        async fn ssm_shell(&self, commands: &[String]) -> Result<String, String> {
            if let Some(msg) = self.forbid_ssm {
                panic!("{msg}");
            }
            self.captured.lock().unwrap().push(commands.to_vec());
            self.ssm_result.clone()
        }
        async fn ssm_shell_sync(&self, commands: &[String], _timeout_secs: f64) -> String {
            self.captured_sync.lock().unwrap().push(commands.to_vec());
            self.sync_output.clone()
        }
        async fn command_invocation(
            &self,
            _command_id: &str,
        ) -> Result<(String, String, String), String> {
            self.invocation.clone()
        }
        async fn firing_alarms(&self) -> Option<Vec<String>> {
            self.alarms.clone()
        }
        async fn month_to_date_cost(&self) -> String {
            self.cost.clone()
        }
        async fn binary_sha(&self) -> String {
            "unknown".to_string()
        }
        fn portal_sha(&self) -> String {
            // Same 3-line resolution as AwsShell::portal_sha, fed from the
            // mock env map (python manipulated process env; edition-2024
            // `std::env::set_var` is unsafe + racy under parallel tests).
            let v = self.env("PORTAL_GIT_SHA");
            let t = v.trim();
            if t.is_empty() {
                "unknown".to_string()
            } else {
                t.to_string()
            }
        }
        async fn main_sha(&self) -> String {
            "unknown".to_string()
        }
    }

    // ------------------------------------------------------- event helpers
    fn get_event() -> Value {
        json!({"requestContext": {"http": {"method": "GET"}}})
    }

    fn post_event(body: &Value, token: Option<&str>) -> Value {
        let headers = match token {
            Some(t) => json!({"authorization": format!("Bearer {t}")}),
            None => json!({}),
        };
        json!({
            "requestContext": {"http": {"method": "POST"}},
            "headers": headers,
            "body": body.to_string(),
        })
    }

    async fn post(shell: &MockShell, body: Value) -> Value {
        route(&post_event(&body, Some(AUTH_SECRET)), shell).await
    }

    fn status_of(resp: &Value) -> i64 {
        resp["statusCode"].as_i64().unwrap()
    }

    fn body_of(resp: &Value) -> Value {
        serde_json::from_str(resp["body"].as_str().unwrap()).unwrap()
    }

    // -------------------------------------------------- HTML section helpers
    fn after<'a>(hay: &'a str, start: &str) -> &'a str {
        hay.split_once(start).map(|(_, rest)| rest).unwrap_or("")
    }

    fn between<'a>(hay: &'a str, start: &str, end: &str) -> &'a str {
        let rest = after(hay, start);
        rest.split_once(end).map_or(rest, |(mid, _)| mid)
    }

    // ----------------------------------------- CONSOLE_HTML identity ratchet
    #[test]
    fn console_html_is_byte_identical_to_python_pre_footer_template() {
        // Golden sha256 of the RAW pre-footer python `_console_html()`
        // template, captured by RUNNING the oracle before the python source
        // was deleted in this PR (`sha256(handler._console_html())`). The
        // footer is injected at request time via `replacen("</body>", …)` in
        // BOTH implementations, so the stored template must be byte-equal.
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, CONSOLE_HTML.as_bytes());
        let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "7edc78757c33cf3cd3f17971dd47648b50fbb151ae1a947da2cd06d3a6f430ce"
        );
        assert_eq!(CONSOLE_HTML.len(), 45_612);
    }

    // --------------------------------------------------------- class ParseView
    #[test]
    fn test_parses_labeled_snapshot() {
        // REST-era snapshot (2026-07-16): today's rows in the three LIVE
        // tables, totals + per-feed split, the 4-column dedup-key count.
        let stdout = concat!(
            "APP=active\n",
            "SPOT_TODAY=1125\n",
            "CHAIN_TODAY=42000\n",
            "CONTRACT_TODAY=9000\n",
            "SPOT_BY_FEED=\"dhan\",750;\"groww\",375;\n",
            "CHAIN_BY_FEED=\"dhan\",21000;\"groww\",21000;\n",
            "CONTRACT_BY_FEED=\"groww\",9000;\n",
            "DEDUP_KEYS=4\n",
            "ERRORS_BEGIN\n",
            "Jun 01 11:00 tickvault: WARN something\n",
            "ERRORS_END\n",
        );
        let out = parse_view(stdout);
        assert_eq!(out["app"], "active");
        assert_eq!(
            out["rows_today"],
            json!({"spot": "1125", "chain": "42000", "contracts": "9000"})
        );
        assert_eq!(out["rows_today_total"], "52125");
        assert_eq!(
            out["rows_by_feed"]["spot"],
            json!({"dhan": "750", "groww": "375"})
        );
        assert_eq!(
            out["rows_by_feed"]["chain"],
            json!({"dhan": "21000", "groww": "21000"})
        );
        assert_eq!(out["rows_by_feed"]["contracts"], json!({"groww": "9000"}));
        assert_eq!(out["dedup_key_columns"], "4");
        assert_eq!(out["recent_errors"].as_array().unwrap().len(), 1);
        assert!(
            out["recent_errors"][0]
                .as_str()
                .unwrap()
                .contains("WARN something")
        );
    }

    #[test]
    fn test_empty_stdout_yields_blank_fields() {
        // Box stopped: NO fabricated zeros — the hero shows nothing.
        let out = parse_view("");
        assert_eq!(out["app"], "");
        assert_eq!(out["dedup_key_columns"], "");
        assert_eq!(
            out["rows_today"],
            json!({"spot": "", "chain": "", "contracts": ""})
        );
        assert_eq!(out["rows_today_total"], "");
        assert_eq!(
            out["rows_by_feed"],
            json!({"spot": {}, "chain": {}, "contracts": {}})
        );
        assert_eq!(out["recent_errors"], json!([]));
    }

    #[test]
    fn test_partial_counts_sum_only_parseable() {
        // One table unreachable (empty value) — the total sums the rest,
        // never treating an unreachable count as 0-and-green.
        let out = parse_view("SPOT_TODAY=100\nCHAIN_TODAY=\nCONTRACT_TODAY=23\n");
        assert_eq!(out["rows_today_total"], "123");
    }

    #[test]
    fn test_no_error_lines_between_markers() {
        let out = parse_view("APP=inactive\nERRORS_BEGIN\nERRORS_END\n");
        assert_eq!(out["app"], "inactive");
        assert_eq!(out["recent_errors"], json!([]));
    }

    // --------------------------------------------------- class HonestHeroHtml
    // Review fix M4 (2026-07-16): _sum_counts promises the hero "shows
    // nothing rather than a fabricated 0", but the JS did
    // countUp(..., j.rows_today_total||'0') — a stopped/unreachable box
    // rendered an animated 0. The hero + Data-tab bars must degrade to '—'.
    #[test]
    fn test_hero_never_renders_fabricated_zero() {
        assert!(!CONSOLE_HTML.contains("j.rows_today_total||'0'"));
        assert!(CONSOLE_HTML.contains("$('ticksbig').textContent='—'"));
        // countUp still runs on a REAL total (the count-up animation stays).
        assert!(CONSOLE_HTML.contains("countUp($('ticksbig'), j.rows_today_total)"));
    }

    #[test]
    fn test_bars_render_dash_for_unreadable_counts() {
        // An empty per-table count maps to null (never parseInt→0)…
        assert!(CONSOLE_HTML.contains(".trim()===''?null:"));
        // …and bar() renders '—' for a null count instead of a 0 bar.
        assert!(CONSOLE_HTML.contains("(miss?'—':n.toLocaleString())"));
    }

    // ---------------------------------------------- class GetServesPublicHtml
    #[tokio::test]
    async fn test_get_returns_html_without_token() {
        let resp = route(&get_event(), &MockShell::default()).await;
        assert_eq!(status_of(&resp), 200);
        assert!(
            resp["headers"]["content-type"]
                .as_str()
                .unwrap()
                .contains("text/html")
        );
        assert!(resp["body"].as_str().unwrap().contains("operator portal"));
    }

    #[test]
    fn test_html_contains_no_secret() {
        // The page is a static shell — it must NOT embed any token/secret.
        assert!(!CONSOLE_HTML.contains("Bearer s3cret"));
        assert!(CONSOLE_HTML.contains("localStorage")); // token kept client-side only
    }

    #[test]
    fn test_html_has_exactly_three_tabs() {
        // 2026-07-02 redesign (operator: "webpage looks completely messy, too
        // many buttons"): exactly Overview / Data / Admin — nothing else.
        let re = regex::Regex::new(r#"data-t="(\w+)""#).unwrap();
        let tabs: Vec<&str> = re
            .captures_iter(CONSOLE_HTML)
            .map(|c| c.get(1).unwrap().as_str())
            .collect();
        assert_eq!(tabs, ["overview", "data", "admin"]);
    }

    #[test]
    fn test_html_supports_ready_made_key_link() {
        // A #key=... link must auto-unlock + strip the fragment from the URL.
        assert!(CONSOLE_HTML.contains("location.hash"));
        assert!(CONSOLE_HTML.contains("replaceState"));
        assert!(CONSOLE_HTML.contains("tv_token"));
    }

    #[test]
    fn test_nuke_message_is_honest_not_optimistic() {
        // The nuke runs async for minutes; the UI must NOT claim "fresh
        // containers + empty data" the instant it is dispatched (false-OK).
        assert!(!CONSOLE_HTML.contains("fresh containers + empty data"));
        assert!(CONSOLE_HTML.contains("wiping in background"));
        // It must poll the REAL outcome and surface both success + the hard-gate
        // failure (volume still in-use → data NOT wiped).
        assert!(CONSOLE_HTML.contains("pollNuke"));
        assert!(CONSOLE_HTML.contains("command-status"));
        assert!(CONSOLE_HTML.contains("DOCKER-RESET-FAILED"));
        assert!(CONSOLE_HTML.contains("NUKE FAILED"));
    }

    // ------------------------------------------------------ class ViewCommands
    #[test]
    fn test_dedup_key_query_url_encodes_the_equals() {
        // The dedup-key count query is the only view query with an `=` in its
        // value; it MUST be encoded as %3D or QuestDB /exp returns empty and the
        // "Dedup key columns" panel shows "?". Guard against regression.
        let dedup_cmd = VIEW_COMMANDS
            .iter()
            .find(|c| c.contains("DEDUP_KEYS="))
            .unwrap();
        assert!(dedup_cmd.contains("upsertKey%3Dtrue"));
        assert!(!dedup_cmd.contains("upsertKey=true"));
    }

    #[test]
    fn test_dedup_key_query_targets_spot_1m_rest() {
        // REST-era repoint (2026-07-16): the shield reads spot_1m_rest's
        // 4-column key (ts, security_id, exchange_segment, feed per
        // DEDUP_KEY_SPOT_1M_REST), not the retired ticks 5-column key.
        let dedup_cmd = VIEW_COMMANDS
            .iter()
            .find(|c| c.contains("DEDUP_KEYS="))
            .unwrap();
        assert!(dedup_cmd.contains("table_columns(%27spot_1m_rest%27)"));
        assert!(!dedup_cmd.contains("'ticks'"));
    }

    #[test]
    fn test_view_commands_target_live_rest_tables() {
        let joined = VIEW_COMMANDS.join("\n");
        for live in ["spot_1m_rest", "option_chain_1m", "option_contract_1m_rest"] {
            assert!(joined.contains(live), "{live}");
        }
        // Today windows use the house `ts IN today()` convention.
        assert!(joined.contains("ts%20IN%20today()"));
    }

    #[test]
    fn test_db_console_default_query_targets_live_table() {
        assert!(CONSOLE_HTML.contains("SELECT * FROM spot_1m_rest ORDER BY ts DESC LIMIT 50"));
        assert!(!CONSOLE_HTML.contains("SELECT * FROM ticks ORDER BY ts DESC LIMIT 50"));
    }

    // ------------------------------------------------------- class RestLatency
    // REST-era latency snapshot (2026-07-16 cleanup): per-(feed, leg)
    // prompt-pull percentiles from rest_fetch_audit + the KEPT box-wide
    // probes (QuestDB RTT, clock skew, dormant order-placement histogram).
    #[test]
    fn test_avg_ns() {
        assert_eq!(avg_ns("1000", "10"), Some(100.0));
        assert_eq!(avg_ns("1000", "0"), None);
        assert_eq!(avg_ns("", ""), None);
    }

    #[test]
    fn test_rest_latency_sql_filters_ok_and_sentinel() {
        // outcome='ok' rows only; close_to_data_ms >= 0 drops the -1
        // not-measured sentinel AND satisfies approx_percentile's
        // non-negative-input requirement; today window + per-(feed, leg).
        let sql = REST_LATENCY_SQL;
        assert!(sql.contains("from rest_fetch_audit"));
        assert!(sql.contains("outcome = 'ok'"));
        assert!(sql.contains("close_to_data_ms >= 0"));
        assert!(sql.contains("approx_percentile(close_to_data_ms, 0.5, 3)"));
        assert!(sql.contains("approx_percentile(close_to_data_ms, 0.99, 3)"));
        assert!(sql.contains("ts in today()"));
        assert!(sql.contains("group by feed, leg"));
    }

    #[test]
    fn test_latency_commands_rest_era_bounded() {
        let joined = LATENCY_COMMANDS.join("\n");
        // The audit aggregate is re-emitted as RESTLAT_ROW= labeled lines,
        // every curl is --max-time bounded, and the box-wide probes are kept.
        assert!(joined.contains("RESTLAT_ROW="));
        assert!(joined.contains("rest_fetch_audit"));
        assert!(joined.contains("--max-time"));
        assert!(joined.contains("QDB="));
        assert!(joined.contains("SKEW="));
        assert!(joined.contains("tv_order_placement_duration_ns"));
        // The retired live-feed probes must never be dialed again.
        assert!(!joined.contains("api-feed.dhan.co"));
        assert!(!joined.contains("socket-api.groww.in"));
        assert!(!joined.contains("FROM ticks"));
        assert!(!joined.contains("received_at"));
    }

    #[test]
    fn test_latency_timeout_reduced_with_margin() {
        // No 25s WS-probe fan-out anymore — worst case is one 3s metrics curl
        // + one 3s QDB probe + one 4s audit read + SSM registration.
        assert_eq!(LATENCY_TIMEOUT_SECS, 15.0);
        assert_eq!(REST_LAT_QUERY_MAX_SECS, 4);
    }

    #[test]
    fn test_parse_rest_lat_row_happy() {
        let row = parse_rest_lat_row("\"dhan\",\"spot_1m\",370,1450.04,5200.55").unwrap();
        assert_eq!(
            row,
            json!({
                "feed": "dhan",
                "leg": "spot_1m",
                "ok_rows": 370,
                "p50_ms": 1450.0,
                "p99_ms": 5200.6,
            })
        );
    }

    #[test]
    fn test_parse_rest_lat_row_null_percentiles_degrade_to_none() {
        let row = parse_rest_lat_row("groww,chain_1m,0,null,NaN").unwrap();
        assert_eq!(row["ok_rows"], 0);
        assert_eq!(row["p50_ms"], Value::Null);
        assert_eq!(row["p99_ms"], Value::Null);
    }

    #[test]
    fn test_parse_rest_lat_row_rejects_malformed_and_bad_tokens() {
        // Malformed on-box output yields NOTHING — never a fabricated row.
        for bad in [
            "",
            "a,b",
            "dhan,spot_1m,x,1,2",
            "DH AN,leg,1,2,3",
            "<script>,leg,1,2,3",
            "dhan,<b>leg</b>,1,2,3",
            "dhan,leg,-5,1,2",
            "dhan,leg,1,2,3,4",
        ] {
            assert!(parse_rest_lat_row(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn test_parse_latency_full() {
        let stdout = concat!(
            "METRICS_BEGIN\n",
            "tv_order_placement_duration_ns_sum 5000\n",
            "tv_order_placement_duration_ns_count 10\n",
            "METRICS_END\n",
            "QDB=0.0021\n",
            "SKEW=0.000123\n",
            "RESTLAT_ROW=\"dhan\",\"chain_1m\",374,1300.0,2100.0\n",
            "RESTLAT_ROW=\"groww\",\"spot_1m\",372,900.0,1600.0\n",
        );
        let out = parse_latency(stdout);
        assert_eq!(out["questdb_ms"], "2.1");
        assert_eq!(out["clock_skew_ms"], "0.1");
        assert_eq!(out["order_place_avg_ns"], json!(500.0));
        let rows = out["rest_latency"].as_array().unwrap();
        let mut by_key = std::collections::HashMap::new();
        for r in rows {
            by_key.insert(
                (
                    r["feed"].as_str().unwrap().to_string(),
                    r["leg"].as_str().unwrap().to_string(),
                ),
                r.clone(),
            );
        }
        let keys: std::collections::HashSet<_> = by_key.keys().cloned().collect();
        assert_eq!(
            keys,
            std::collections::HashSet::from([
                ("dhan".to_string(), "chain_1m".to_string()),
                ("groww".to_string(), "spot_1m".to_string()),
            ])
        );
        assert_eq!(
            by_key[&("dhan".to_string(), "chain_1m".to_string())]["ok_rows"],
            374
        );
        assert_eq!(
            by_key[&("dhan".to_string(), "chain_1m".to_string())]["p99_ms"],
            json!(2100.0)
        );
        assert_eq!(
            by_key[&("groww".to_string(), "spot_1m".to_string())]["p50_ms"],
            json!(900.0)
        );
    }

    #[test]
    fn test_parse_latency_empty() {
        // Box stopped -> empty table + blank shields, never fabricated numbers.
        let out = parse_latency("");
        assert_eq!(out["rest_latency"], json!([]));
        assert_eq!(out["questdb_ms"], "");
        assert_eq!(out["clock_skew_ms"], "");
        assert_eq!(out["order_place_avg_ns"], Value::Null);
    }

    #[test]
    fn test_parse_latency_malformed_rest_rows_skipped() {
        let out = parse_latency("RESTLAT_ROW=garbage\nRESTLAT_ROW=dhan,spot_1m,10,1.0,2.0\n");
        assert_eq!(out["rest_latency"].as_array().unwrap().len(), 1);
        assert_eq!(out["rest_latency"][0]["feed"], "dhan");
    }

    // ------------------------------------------------------ class ParseStorage
    #[test]
    fn test_parses_df_du() {
        let out = parse_storage("DISK_USED=6G\nDISK_FREE=24G\nDISK_PCT=20%\nDB_SIZE=3G\n");
        assert_eq!(out["disk_used_gb"], "6");
        assert_eq!(out["disk_free_gb"], "24");
        assert_eq!(out["disk_pct"], "20%");
        assert_eq!(out["db_size_gb"], "3");
    }

    #[test]
    fn test_empty() {
        let out = parse_storage("");
        assert_eq!(out["disk_free_gb"], "");
        assert_eq!(out["db_size_gb"], "");
    }

    // ------------------------------------------------------ class DbConsoleTab
    #[test]
    fn test_db_console_folded_into_data_tab() {
        // 2026-07-02 redesign: the DB console (PR #1326) is no longer its own
        // tab — it lives INSIDE the Data tab. There is no data-t="db" tab, and
        // the console markup sits inside the data <section>.
        assert!(!CONSOLE_HTML.contains(r#"data-t="db""#));
        let data_section = between(CONSOLE_HTML, r#"<section data-tab="data""#, "</section>");
        for needle in [
            r#"id="dbtables""#,
            r#"id="dbsql""#,
            r#"id="dbout""#,
            r#"id="dbcols""#,
            r#"id="bars""#,
        ] {
            assert!(data_section.contains(needle), "{needle}");
        }
    }

    #[test]
    fn test_db_tab_shows_read_only_badge() {
        assert!(CONSOLE_HTML.contains("READ-ONLY — writes are blocked server-side"));
    }

    #[test]
    fn test_db_tab_has_all_controls() {
        for needle in [
            "loadDbTables()",
            "runDbSql()",
            "dbDownloadCsv()",
            "dbPick(",
            r#"id="dbtables""#,
            r#"id="dbsql""#,
            r#"id="dbout""#,
            r#"id="dbcount""#,
            r#"id="dbcols""#,
        ] {
            assert!(CONSOLE_HTML.contains(needle), "{needle}");
        }
    }

    #[test]
    fn test_data_tab_autoloads_tables_on_open() {
        // tab('data') must trigger loadDbTables() without an extra click.
        assert!(
            CONSOLE_HTML.contains("name==='data' && !$('dbtables').dataset.loaded) loadDbTables()")
        );
    }

    #[test]
    fn test_table_pick_is_index_based_no_injection() {
        // The clicked table name comes from dbTables[i] — QuestDB's own
        // tables() output — never from user-typed input.
        assert!(CONSOLE_HTML.contains("dbTables[i]"));
        assert!(CONSOLE_HTML.contains("table_columns("));
        assert!(CONSOLE_HTML.contains("LIMIT 100"));
    }

    #[test]
    fn test_grid_rows_render_through_esc() {
        // Every cell of the shared grid renderer must pass through esc().
        assert!(CONSOLE_HTML.contains("'<th>'+esc(c)+'</th>'"));
        assert!(CONSOLE_HTML.contains("'<td>'+esc(tcell(c))+'</td>'"));
    }

    // -------------------------------------------------- class PostRequiresAuth
    #[tokio::test]
    async fn test_post_without_token_is_401() {
        let shell = MockShell::default();
        let ev = post_event(&json!({"action": "view"}), None);
        let resp = route(&ev, &shell).await;
        assert_eq!(status_of(&resp), 401);
    }

    // ---------------------------------------------------------- class WipeGate
    // python setUp pinned the clock OFF-hours (MockShell default) — since the
    // audit-fix-#2 hard lock, a forced data-destructive action is 409'd
    // in-window regardless of force, so these dispatch tests must be
    // deterministic at any wall-clock time.
    async fn wipe(shell: &MockShell, force: bool, confirm: &str) -> Value {
        post(
            shell,
            json!({"action": "wipe-questdb", "force": force, "confirm": confirm}),
        )
        .await
    }

    async fn docker_reset(shell: &MockShell, force: bool, confirm: &str, token: &str) -> Value {
        route(
            &post_event(
                &json!({"action": "docker-reset", "force": force, "confirm": confirm}),
                Some(token),
            ),
            shell,
        )
        .await
    }

    async fn docker_nuke_bare(shell: &MockShell, force: bool, confirm: &str) -> Value {
        post(
            shell,
            json!({"action": "docker-nuke-bare", "force": force, "confirm": confirm}),
        )
        .await
    }

    #[tokio::test]
    async fn test_wipe_without_force_is_blocked() {
        // Either the destructive market-hours guard (409) or the explicit
        // force-required guard (409) — never reaches boto3.
        let shell = MockShell::default();
        let resp = wipe(&shell, false, "WIPE").await;
        assert_eq!(status_of(&resp), 409);
    }

    #[test]
    fn test_wipe_is_in_destructive_set() {
        assert!(DESTRUCTIVE.contains(&"wipe-questdb"));
    }

    #[tokio::test]
    async fn test_legacy_destructive_actions_require_server_side_tokens() {
        // PR-5 H-1 (2026-07-02 security MEDIUM): a scripted call with a stolen
        // bearer + force=true must be 409'd unless it carries the SAME typed
        // word the portal makes the operator type — verified SERVER-side for
        // all three destructive actions (wipe-questdb / docker-reset /
        // docker-nuke-bare; the wipe-groww action that pioneered this gate
        // was removed 2026-07-16 with the Groww live feed).
        let shell = MockShell::default();
        for resp in [
            wipe(&shell, true, "").await,
            wipe(&shell, true, "wipe").await,
            docker_reset(&shell, true, "", AUTH_SECRET).await,
            docker_reset(&shell, true, "YES", AUTH_SECRET).await,
            docker_nuke_bare(&shell, true, "").await,
            docker_nuke_bare(&shell, true, "erase").await,
        ] {
            assert_eq!(status_of(&resp), 409);
            assert!(
                body_of(&resp)["error"]
                    .as_str()
                    .unwrap()
                    .contains("confirm")
            );
        }
        // And the client JS forwards each token (no dead server gate).
        assert!(CONSOLE_HTML.contains("call('wipe-questdb',{force:true,confirm:'WIPE'})"));
        assert!(
            CONSOLE_HTML
                .contains("call('docker-reset',{force:$('force').checked,confirm:'NUKE-DOCKER'})")
        );
        assert!(CONSOLE_HTML.contains("call('docker-nuke-bare',{force:true,confirm:'ERASE'})"));
    }

    #[test]
    fn test_wipe_questdb_removes_replay_sources_before_truncate_and_disables_unit() {
        // PR-5 H-2 (hostile F5): the resurrection race — an external
        // `systemctl start` between TRUNCATE and the replay-source rm let the
        // booting bridge re-tail the capture file. Two pinned layers:
        //  (a) replay sources are removed BEFORE the TRUNCATE python block;
        //  (b) the unit is DISABLED for the wipe window and re-enabled before
        //      the final start;
        //  (c) prev_day_ohlcv is in the dynamic truncate targets.
        // (python scanned handler source; the Rust command list is the
        // oracle-captured WIPE_QUESTDB_COMMANDS golden — same ordering.)
        let joined = WIPE_QUESTDB_COMMANDS.join("\n");
        let rm_pos = joined.find("feed capture/replay sources removed").unwrap();
        let truncate_pos = joined.find("PYWIPE").unwrap();
        assert!(
            rm_pos < truncate_pos,
            "replay-source rm must precede TRUNCATE"
        );
        let disable_pos = joined.find("systemctl disable tickvault || true").unwrap();
        assert!(
            disable_pos < rm_pos,
            "unit must be disabled before the wipe body"
        );
        assert!(joined.contains("t == 'prev_day_ohlcv'"));
        assert!(joined.contains("systemctl enable tickvault || true"));
    }

    #[test]
    fn test_wipe_is_resurrect_proof_and_dynamic() {
        // Operator incident 2026-07-02: the wipe TRUNCATEd 6 hardcoded tables
        // (of 27) and never touched the feed capture/replay files, so Groww
        // rows RESURRECTED via the bridge's byte-0 re-tail and most candle
        // tables survived for BOTH feeds. Pin the rewrite against the golden:
        let joined = WIPE_QUESTDB_COMMANDS.join("\n");
        // (a) dynamic table discovery — no hardcoded 6-table list.
        assert!(joined.contains("SELECT table_name FROM tables()"));
        assert!(joined.contains("t == 'ticks' or t.startswith('candles_')"));
        // (b) every feed's capture/replay source removed (feed-agnostic).
        for needle in ["/ws_wal", "/groww", "/spill", "/dlq", "live-ticks.ndjson"] {
            assert!(
                joined.contains(needle),
                "wipe must remove the {needle} replay source"
            );
        }
        // (c) the app is stopped before + started after the wipe.
        assert!(joined.contains("systemctl stop tickvault"));
        assert!(joined.contains("systemctl start tickvault"));
        // (d) honest completion marker — never a fake OK.
        assert!(joined.contains("WIPE-COMPLETE"));
        assert!(joined.contains("WIPE-PARTIAL"));
    }

    #[test]
    fn test_wipe_questdb_truncates_live_rest_tables_too() {
        // 2026-07-16 destructive-surface extension: a "fresh start" must also
        // drop today's official minute candles + the fetch log — the four
        // LIVE tables the REST pulls write. rest_fetch_audit is per-fetch
        // forensics, NOT a SEBI never-delete table; the SEBI *_audit family
        // stays preserved by the dynamic filter.
        let joined = WIPE_QUESTDB_COMMANDS.join("\n");
        assert!(joined.contains(
            "{'spot_1m_rest', 'option_chain_1m', 'option_contract_1m_rest', 'rest_fetch_audit'}"
        ));
        assert!(joined.contains("t in live_rest"));
        // Review fix M2 (2026-07-16): honest completion verifies EVERY
        // truncate-target family — the legacy pair AND all FOUR live REST
        // tables.
        for t in [
            "ticks",
            "candles_1m",
            "spot_1m_rest",
            "option_chain_1m",
            "option_contract_1m_rest",
            "rest_fetch_audit",
        ] {
            assert!(joined.contains(&format!("$(qc {t})")), "{t}");
        }
        assert!(joined.contains("spot_1m_rest=${S:-?}"));
        // Review fix M2: a missing/erroring count defaults to 0 (absent
        // table = nothing left = wiped). The old default-to-1 made EVERY
        // post-nuke wipe read WIPE-PARTIAL forever.
        for default in [
            "${T:-0}", "${C:-0}", "${S:-0}", "${O:-0}", "${K:-0}", "${A:-0}",
        ] {
            assert!(joined.contains(default), "{default}");
            assert!(
                !joined.contains(&default.replace(":-0", ":-1")),
                "{default}"
            );
        }
        assert!(joined.contains("TRUNCATE-FAILED"));
    }

    #[test]
    fn test_docker_reset_and_bare_nuke_remove_feed_capture_sources() {
        // The full nuke + bare nuke must ALSO sweep the feed capture/replay
        // dirs — the Groww capture file survived both before 2026-07-02.
        let reset_block = DOCKER_RESET_COMMANDS.join("\n");
        let bare_block = DOCKER_NUKE_BARE_COMMANDS.join("\n");
        for (name, block) in [
            ("docker-reset", &reset_block),
            ("docker-nuke-bare", &bare_block),
        ] {
            assert!(block.contains("/ws_wal"), "{name} must remove the Dhan WAL");
            assert!(
                block.contains("/groww"),
                "{name} must remove the Groww capture dir"
            );
            assert!(
                block.contains("live-ticks.ndjson"),
                "{name} must sweep future feeds' capture files"
            );
        }
    }

    #[tokio::test]
    async fn test_docker_reset_unauthorized_is_401() {
        // Bearer-auth gate fires BEFORE anything else — a wrong token never
        // reaches the confirm/market-hours guards or boto3.
        let shell = MockShell::default();
        let resp = docker_reset(&shell, true, "NUKE-DOCKER", "wrong-token").await;
        assert_eq!(status_of(&resp), 401);
    }

    #[tokio::test]
    async fn test_docker_reset_without_confirm_is_blocked() {
        // Full-docker-nuke typed-confirm guard (operator demand 2026-07-03):
        // even a forced, authenticated call without {"confirm": "NUKE-DOCKER"}
        // is 409 and never reaches SSM.
        let shell = MockShell {
            forbid_ssm: Some("SSM must not be called without the typed confirm"),
            ..MockShell::default()
        };
        for bad in ["", "NUKE", "nuke-docker", "DOCKER-NUKE", "WIPE"] {
            let resp = docker_reset(&shell, true, bad, AUTH_SECRET).await;
            assert_eq!(status_of(&resp), 409, "{bad:?}");
            assert!(
                body_of(&resp)["error"]
                    .as_str()
                    .unwrap()
                    .contains("NUKE-DOCKER")
            );
        }
    }

    #[tokio::test]
    async fn test_docker_reset_blocked_during_market_hours_without_force() {
        // 09:15-15:30 IST guard: with the correct confirm phrase but no force,
        // the nuke is 409-blocked while the market is open.
        let shell = MockShell {
            market_hours: true,
            forbid_ssm: Some("SSM must not be called during market hours without force"),
            ..MockShell::default()
        };
        let resp = docker_reset(&shell, false, "NUKE-DOCKER", AUTH_SECRET).await;
        assert_eq!(status_of(&resp), 409);
        assert!(
            body_of(&resp)["error"]
                .as_str()
                .unwrap()
                .contains("market hours")
        );
    }

    #[test]
    fn test_docker_reset_is_in_destructive_set() {
        // Membership = market-hours-blocked during 09:15-15:30 IST.
        assert!(DESTRUCTIVE.contains(&"docker-reset"));
    }

    #[tokio::test]
    async fn test_docker_reset_forced_is_hardened_full_nuke() {
        // Regression 2026-06-05: "the nuke didn't wipe the data". The forced
        // docker-reset must (a) remove containers by VOLUME, (b) fail LOUD
        // without recreating if the volume survives, and (c) wipe the HOST app
        // caches the Docker nuke can't see (instrument-cache/spill/dlq).
        let shell = MockShell {
            ssm_result: Ok("cmd-123".to_string()),
            ..MockShell::default()
        };
        let resp = docker_reset(&shell, true, "NUKE-DOCKER", AUTH_SECRET).await;
        assert_eq!(status_of(&resp), 200);
        let joined = shell.captured_joined();
        // (a) robust container removal by VOLUME, not just by name
        assert!(joined.contains("--filter volume=tv-questdb-data"));
        // (b) hard fail-loud gate — must NOT recreate if the volume survives
        assert!(joined.contains("DOCKER-RESET-FAILED"));
        // (c) host caches wiped too (the dirs the Docker nuke cannot see)
        assert!(joined.contains("/opt/tickvault/data/instrument-cache"));
        assert!(joined.contains("/opt/tickvault/data/spill"));
        assert!(joined.contains("/opt/tickvault/data/dlq"));
        // (d) the FULL-NUKE sequence: stop app → compose down -v → prune →
        //     fresh rebuild + app restart — operator demand 2026-07-03.
        assert!(joined.contains("systemctl stop tickvault"));
        assert!(joined.contains("docker compose down -v"));
        assert!(joined.contains("docker system prune -af --volumes"));
        assert!(joined.contains("ensure-questdb.sh"));
        assert!(joined.contains("systemctl restart tickvault"));
    }

    #[tokio::test]
    async fn test_docker_nuke_bare_without_force_is_blocked() {
        let shell = MockShell::default();
        assert_eq!(
            status_of(&docker_nuke_bare(&shell, false, "ERASE").await),
            409
        );
    }

    #[test]
    fn test_docker_nuke_bare_is_in_destructive_set() {
        assert!(DESTRUCTIVE.contains(&"docker-nuke-bare"));
    }

    #[tokio::test]
    async fn test_docker_nuke_bare_forced_deletes_all_and_does_not_rebuild() {
        // Operator request 2026-06-12: behave like Docker Desktop "delete all"
        // — remove EVERY container + image + volume and DO NOT rebuild (the
        // box is left bare). This is what distinguishes it from docker-reset.
        let shell = MockShell {
            ssm_result: Ok("cmd-bare".to_string()),
            ..MockShell::default()
        };
        let resp = docker_nuke_bare(&shell, true, "ERASE").await;
        assert_eq!(status_of(&resp), 200);
        let joined = shell.captured_joined();
        // deletes ALL of the three object types
        assert!(joined.contains("docker ps -aq"));
        assert!(joined.contains("docker images -aq"));
        assert!(joined.contains("docker volume ls -q"));
        // truthful verify line
        assert!(joined.contains("BARE-NUKE-RESULT"));
        assert!(joined.contains("bare-nuke-complete"));
        // the WHOLE POINT: it must NOT rebuild / restart the app
        assert!(!joined.contains("ensure-questdb.sh"));
        assert!(!joined.contains("systemctl restart tickvault"));
        assert!(!joined.contains("docker compose up"));
    }

    // ---------------------------------------------------- class HtmlWipeButton
    #[test]
    fn test_html_has_wipe_button() {
        assert!(CONSOLE_HTML.contains("wipeData()"));
        assert!(CONSOLE_HTML.contains("Wipe ALL data"));
    }

    #[test]
    fn test_html_has_docker_reset_button() {
        // Operator demand 2026-07-03 ("entire docker nuke") — the Full Docker
        // Nuke must be a clearly-labeled red action inside the danger zone.
        assert!(CONSOLE_HTML.contains("dockerReset()"));
        assert!(CONSOLE_HTML.contains("Full Docker nuke"));
        assert!(CONSOLE_HTML.contains("fresh start"));
        // The nuke must spell out the SEBI-audit-data loss + require typing
        // the server-verified confirm phrase NUKE-DOCKER.
        assert!(CONSOLE_HTML.contains("NUKE-DOCKER"));
        assert!(CONSOLE_HTML.contains("audit"));
        // The UI must SEND the typed confirm phrase server-side and respect
        // the force checkbox (market-hours guard).
        assert!(
            CONSOLE_HTML
                .contains("call('docker-reset',{force:$('force').checked,confirm:'NUKE-DOCKER'})")
        );
    }

    #[test]
    fn test_html_has_bare_nuke_button() {
        assert!(CONSOLE_HTML.contains("bareNuke()"));
        assert!(CONSOLE_HTML.contains("Bare Nuke"));
        // must require typing ERASE + spell out that the box is left EMPTY/dead.
        assert!(CONSOLE_HTML.contains("ERASE"));
        assert!(CONSOLE_HTML.contains("redeploy"));
    }

    // ------------------------------------------- class RemovedActionsReturn400
    // 2026-07-16 cleanup: the cross-verify card (its producer was deleted in
    // PR-C3 2026-07-14) and the groww-only wipe are REMOVED. Their POST
    // actions fall through to the unknown-action 400 — never a silent success.
    #[tokio::test]
    async fn test_cross_verify_action_removed() {
        let shell = MockShell::default();
        let resp = post(&shell, json!({"action": "cross_verify"})).await;
        assert_eq!(status_of(&resp), 400);
        assert!(
            body_of(&resp)["error"]
                .as_str()
                .unwrap()
                .contains("unknown action")
        );
    }

    #[tokio::test]
    async fn test_wipe_groww_action_removed() {
        // Even the historically-correct payload (force + typed confirm) must
        // 400 — the action no longer exists and never reaches SSM.
        let shell = MockShell {
            forbid_ssm: Some("SSM must not be called for a removed action"),
            ..MockShell::default()
        };
        let resp = post(
            &shell,
            json!({"action": "wipe-groww", "force": true, "confirm": "GROWW"}),
        )
        .await;
        assert_eq!(status_of(&resp), 400);
        assert!(
            body_of(&resp)["error"]
                .as_str()
                .unwrap()
                .contains("unknown action")
        );
    }

    #[test]
    fn test_removed_actions_left_the_destructive_sets() {
        assert!(!DESTRUCTIVE.contains(&"wipe-groww"));
        assert!(!DATA_DESTRUCTIVE.contains(&"wipe-groww"));
        assert!(!DESTRUCTIVE.contains(&"cross_verify"));
    }

    // -------------------------------------------------------- class FeedCounts
    #[test]
    fn test_parse_feed_counts_two_feeds() {
        let got = parse_feed_counts("\"dhan\",152000;\"groww\",340;");
        assert_eq!(got, json!({"dhan": "152000", "groww": "340"}));
    }

    #[test]
    fn test_parse_feed_counts_empty_or_garbage_yields_empty() {
        // No fabricated zeros — an unreachable QuestDB renders NOTHING.
        assert_eq!(parse_feed_counts(""), json!({}));
        assert_eq!(parse_feed_counts(";;"), json!({}));
        assert_eq!(parse_feed_counts("garbage-without-comma"), json!({}));
    }

    #[test]
    fn test_view_commands_query_live_tables_grouped_by_feed() {
        for (label, table) in [
            ("SPOT_BY_FEED=", "spot_1m_rest"),
            ("CHAIN_BY_FEED=", "option_chain_1m"),
            ("CONTRACT_BY_FEED=", "option_contract_1m_rest"),
        ] {
            let cmd = VIEW_COMMANDS.iter().find(|c| c.contains(label)).unwrap();
            assert!(cmd.contains("GROUP%20BY%20feed"));
            assert!(cmd.contains(&format!("FROM%20{table}")));
        }
    }

    // --------------------------------------------------------- class FeedsView
    #[test]
    fn test_parse_feeds_view_full() {
        let stdout = concat!(
            "FEEDS_BEGIN\n",
            "{\"dhan_enabled\": true, \"groww_enabled\": false, ",
            "\"dhan_lane_running\": true, \"groww_lane_running\": false}\n",
            "FEEDS_END\n",
            "FEEDS_HEALTH_BEGIN\n",
            "{\"market_open\": true, \"feeds\": [{\"feed\": \"dhan\", \"verdict\": \"ok\", ",
            "\"reason\": \"streaming\", \"enabled\": true, \"lane_running\": true, ",
            "\"ticks_total\": 152000}]}\n",
            "FEEDS_HEALTH_END\n",
        );
        let out = parse_feeds_view(stdout);
        assert_eq!(out["feeds_error"], "");
        assert_eq!(out["health_error"], "");
        assert_eq!(out["feeds"]["dhan_enabled"], true);
        assert_eq!(out["feeds"]["groww_enabled"], false);
        assert_eq!(out["feeds"]["dhan_lane_running"], true);
        assert_eq!(out["health"]["feeds"][0]["feed"], "dhan");
        assert_eq!(out["health"]["feeds"][0]["verdict"], "ok");
    }

    #[test]
    fn test_parse_feeds_view_curl_failed_is_structured_error_not_zeros() {
        let stdout = concat!(
            "FEEDS_BEGIN\nTV_CURL_FAILED\nFEEDS_END\n",
            "FEEDS_HEALTH_BEGIN\nTV_CURL_FAILED\nFEEDS_HEALTH_END\n",
        );
        let out = parse_feeds_view(stdout);
        assert_eq!(out["feeds"], Value::Null);
        assert_eq!(out["health"], Value::Null);
        assert!(out["feeds_error"].as_str().unwrap().contains("unreachable"));
        assert!(
            out["health_error"]
                .as_str()
                .unwrap()
                .contains("unreachable")
        );
    }

    #[test]
    fn test_parse_feeds_view_empty_stdout_is_box_unreachable() {
        let out = parse_feeds_view("");
        assert_eq!(out["feeds"], Value::Null);
        assert!(
            out["feeds_error"]
                .as_str()
                .unwrap()
                .contains("SSM offline or instance stopped")
        );
        assert!(
            out["health_error"]
                .as_str()
                .unwrap()
                .contains("SSM offline or instance stopped")
        );
    }

    #[test]
    fn test_parse_feeds_view_invalid_json_is_error() {
        let stdout = concat!(
            "FEEDS_BEGIN\n{not json\nFEEDS_END\n",
            "FEEDS_HEALTH_BEGIN\n{}\nFEEDS_HEALTH_END\n",
        );
        let out = parse_feeds_view(stdout);
        assert_eq!(out["feeds"], Value::Null);
        assert!(
            out["feeds_error"]
                .as_str()
                .unwrap()
                .contains("invalid JSON")
        );
        assert_eq!(out["health"], json!({}));
        assert_eq!(out["health_error"], "");
    }

    #[test]
    fn test_parse_feeds_view_rest_lane_fields() {
        // The REST-lane pulse rides the same snapshot as labeled lines
        // OUTSIDE the marker blocks (2026-07-16).
        let stdout = concat!(
            "FEEDS_BEGIN\n{}\nFEEDS_END\n",
            "FEEDS_HEALTH_BEGIN\n{}\nFEEDS_HEALTH_END\n",
            "REST_AUDIT=\"dhan\",\"ok\",370;\"dhan\",\"error\",3;\"groww\",\"ok\",372;\"groww\",\"rate_limited\",2;\n",
            "REST_LAT_HOUR=\"dhan\",1450.04,5200.55;\"groww\",900.0,1600.0;\n",
        );
        let out = parse_feeds_view(stdout);
        assert_eq!(out["rest_audit"]["dhan"], json!({"ok": 370, "error": 3}));
        assert_eq!(
            out["rest_audit"]["groww"],
            json!({"ok": 372, "rate_limited": 2})
        );
        assert_eq!(
            out["rest_lat_hour"]["dhan"],
            json!({"p50_ms": 1450.0, "p99_ms": 5200.6})
        );
        assert_eq!(
            out["rest_lat_hour"]["groww"],
            json!({"p50_ms": 900.0, "p99_ms": 1600.0})
        );
    }

    #[test]
    fn test_parse_feeds_view_rest_lane_empty_or_garbage_is_empty() {
        // Empty/absent/garbage values yield {} — the card then says
        // "no pulls recorded today", never fabricated zeros (Rule 11).
        let out = parse_feeds_view("FEEDS_BEGIN\n{}\nFEEDS_END\n");
        assert_eq!(out["rest_audit"], json!({}));
        assert_eq!(out["rest_lat_hour"], json!({}));
        assert_eq!(
            parse_rest_audit(";;garbage;a,b;<x>,ok,3;dhan,ok,x;"),
            json!({})
        );
        assert_eq!(parse_rest_lat_hour(";;garbage;dhan,null,null;"), json!({}));
    }

    #[test]
    fn test_parse_feeds_view_json_body_never_mistaken_for_labeled_line() {
        // A '=' inside the marker-delimited JSON must not leak into the
        // labeled-line scan.
        let stdout = "FEEDS_BEGIN\n{\"note\": \"REST_AUDIT=fake\"}\nFEEDS_END\n";
        let out = parse_feeds_view(stdout);
        assert_eq!(out["rest_audit"], json!({}));
    }

    // -------------------------------------- class FeedsViewCommandsPinned
    // Review fixes M1 + M5 (2026-07-16): the feeds-card snapshot commands
    // were unpinned and the SSM budget sat BELOW the worst-case curl total,
    // so a slow-but-RUNNING box read as the FALSE "box unreachable".
    #[test]
    fn test_feeds_timeout_budget_exceeds_curl_max_time_sum() {
        // M1: parse every --max-time out of the command strings (drift-proof
        // — adding a curl without raising the budget fails this test).
        let re = regex::Regex::new(r"--max-time\s+(\d+)").unwrap();
        let total: i64 = FEEDS_VIEW_COMMANDS
            .iter()
            .flat_map(|c| re.captures_iter(c))
            .map(|m| m[1].parse::<i64>().unwrap())
            .sum();
        assert_eq!(total, 24); // 2×8s app curls + 2×4s audit curls
        assert!(FEEDS_TIMEOUT_SECS > total as f64);
        assert_eq!(FEEDS_TIMEOUT_SECS, 28.0);
    }

    #[test]
    fn test_rest_audit_curl_targets_todays_fetch_log() {
        let cmd = FEEDS_VIEW_COMMANDS
            .iter()
            .find(|c| c.contains("REST_AUDIT="))
            .unwrap();
        assert!(cmd.contains("from rest_fetch_audit"));
        assert!(cmd.contains("ts in today()"));
        assert!(cmd.contains("group by feed, outcome"));
        assert!(cmd.contains("--data-urlencode"));
    }

    #[test]
    fn test_rest_lat_hour_curl_ist_timebase_ok_filter_and_sentinel() {
        let cmd = FEEDS_VIEW_COMMANDS
            .iter()
            .find(|c| c.contains("REST_LAT_HOUR="))
            .unwrap();
        // IST timebase: ts is IST-shifted while QuestDB now() is UTC — the
        // window compares against dateadd('m', 330, now()) (2026-07-07 lesson).
        assert!(cmd.contains("dateadd('m', 330, now())"));
        assert!(cmd.contains("dateadd('h', -1,"));
        // Successful pulls only + the -1 not-measured sentinel excluded.
        assert!(cmd.contains("outcome = 'ok'"));
        assert!(cmd.contains("close_to_data_ms >= 0"));
        assert!(cmd.contains("from rest_fetch_audit"));
        assert!(cmd.contains("approx_percentile(close_to_data_ms, 0.5, 3)"));
        assert!(cmd.contains("approx_percentile(close_to_data_ms, 0.99, 3)"));
    }

    // ---------------------------------------------- class FeedToggleValidation
    #[test]
    fn test_valid_feed_and_bool_accepted() {
        assert_eq!(validate_feed_toggle(&json!("dhan"), &json!(true)), "");
        assert_eq!(validate_feed_toggle(&json!("groww"), &json!(false)), "");
        // Future feed #3: a well-formed lowercase name passes the Lambda; the
        // APP is the authority that 400s unknown feeds (passed through).
        assert_eq!(validate_feed_toggle(&json!("kite"), &json!(true)), "");
    }

    #[test]
    fn test_bad_feed_name_rejected() {
        for bad in [
            json!(""),
            json!("DHAN"),
            json!("dhan; rm -rf /"),
            json!("a b"),
            json!("../etc"),
            Value::Null,
            json!(7),
            json!(["dhan"]),
        ] {
            assert_ne!(validate_feed_toggle(&bad, &json!(true)), "", "{bad:?}");
        }
    }

    #[test]
    fn test_enabled_must_be_json_bool() {
        for bad in [
            json!(1),
            json!(0),
            json!("true"),
            json!("false"),
            Value::Null,
            json!("yes"),
        ] {
            let msg = validate_feed_toggle(&json!("dhan"), &bad);
            assert!(msg.contains("boolean"), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn test_lambda_rejects_bad_feed_with_400() {
        let shell = MockShell::default();
        let resp = post(
            &shell,
            json!({"action": "feed-toggle", "feed": "x; reboot", "enabled": true}),
        )
        .await;
        assert_eq!(status_of(&resp), 400);
    }

    #[test]
    fn test_toggle_commands_target_correct_url_and_body() {
        let joined = feed_toggle_commands("groww", true).join("\n");
        assert!(joined.contains("http://127.0.0.1:3001/api/feeds/groww"));
        assert!(joined.contains(r#"{"enabled": true}"#));
        assert!(joined.contains("-X POST"));
        let cmds_off = feed_toggle_commands("dhan", false).join("\n");
        assert!(cmds_off.contains("http://127.0.0.1:3001/api/feeds/dhan"));
        assert!(cmds_off.contains(r#"{"enabled": false}"#));
    }

    #[test]
    fn test_feed_toggle_is_not_market_hours_blocked() {
        // The APP owns the safety gate (409 on Dhan-disable during live
        // trading); the Lambda must not double-gate a read-mostly feed flip.
        assert!(!DESTRUCTIVE.contains(&"feed-toggle"));
        assert!(!DESTRUCTIVE.contains(&"feeds-view"));
    }

    // ------------------------------------------- class FeedTogglePassthrough
    const DHAN_GUARD: &str = "refusing to disable the Dhan feed while live trading is active \
(orders/positions open) — Dhan can only be turned off in the no-orders \
data-pull phase, so the system is never blinded mid-trade";

    #[test]
    fn test_parse_feed_toggle_200_success() {
        let stdout = concat!(
            "FEED_TOGGLE_STATUS=200\n",
            "FEED_TOGGLE_BODY_BEGIN\n",
            "{\"dhan_enabled\": true, \"groww_enabled\": true, ",
            "\"dhan_lane_running\": true, \"groww_lane_running\": false}\n",
            "FEED_TOGGLE_BODY_END\n",
        );
        let out = parse_feed_toggle(stdout);
        assert_eq!(out["app_status"], 200);
        assert_eq!(out["error"], "");
        assert_eq!(out["app_response"]["groww_enabled"], true);
    }

    #[test]
    fn test_parse_feed_toggle_409_dhan_guard_message_verbatim() {
        let stdout = format!(
            "FEED_TOGGLE_STATUS=409\nFEED_TOGGLE_BODY_BEGIN\n{}\nFEED_TOGGLE_BODY_END\n",
            json!({"error": DHAN_GUARD, "allowed": ["groww"]})
        );
        let out = parse_feed_toggle(&stdout);
        assert_eq!(out["app_status"], 409);
        // The app's 409 guard message must survive VERBATIM.
        assert_eq!(out["app_response"]["error"], DHAN_GUARD);
    }

    #[test]
    fn test_parse_feed_toggle_curl_000_is_unreachable_not_success() {
        let out = parse_feed_toggle(
            "FEED_TOGGLE_STATUS=000\nFEED_TOGGLE_BODY_BEGIN\nFEED_TOGGLE_BODY_END\n",
        );
        assert_eq!(out["app_status"], Value::Null);
        assert!(out["error"].as_str().unwrap().contains("unreachable"));
    }

    #[test]
    fn test_parse_feed_toggle_empty_stdout_is_box_unreachable() {
        let out = parse_feed_toggle("");
        assert_eq!(out["app_status"], Value::Null);
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("SSM offline or instance stopped")
        );
    }

    #[tokio::test]
    async fn test_lambda_passes_409_through_with_ok_false() {
        let stdout = format!(
            "FEED_TOGGLE_STATUS=409\nFEED_TOGGLE_BODY_BEGIN\n{}\nFEED_TOGGLE_BODY_END\n",
            json!({"error": DHAN_GUARD, "allowed": ["groww"]})
        );
        let shell = MockShell {
            sync_output: stdout,
            ..MockShell::default()
        };
        let resp = post(
            &shell,
            json!({"action": "feed-toggle", "feed": "dhan", "enabled": false}),
        )
        .await;
        assert_eq!(status_of(&resp), 200);
        let body = body_of(&resp);
        assert_eq!(body["ok"], false);
        assert_eq!(body["app_status"], 409);
        assert_eq!(body["app_response"]["error"], DHAN_GUARD);
    }

    // ---------------------------------------------------- class DedupKeyShield
    // The dedup shield reads spot_1m_rest's REAL 4-column upsert key
    // (ts, security_id, exchange_segment, feed per DEDUP_KEY_SPOT_1M_REST in
    // crates/storage/src/spot_1m_rest_persistence.rs) — repointed 2026-07-16
    // from the retired ticks 5-column key.
    #[test]
    fn test_view_comment_names_the_four_real_key_columns() {
        // python scanned handler.py; the Rust twin lives in the commands
        // module's doc comment (the %3D rationale block).
        let src = include_str!("operator_control_commands.rs");
        assert!(src.contains("(ts, security_id, exchange_segment, feed)"));
        assert!(src.contains("DEDUP_KEY_SPOT_1M_REST"));
    }

    #[test]
    fn test_html_distinguishes_ok_disabled_drift_unreachable() {
        // 4 = OK (green): the check compares against 4, not the stale 5.
        assert!(CONSOLE_HTML.contains("dkN===4"));
        assert!(!CONSOLE_HTML.contains("dkN===5"));
        // 0 = DEDUP disabled entirely (RED).
        assert!(CONSOLE_HTML.contains("DEDUP disabled!"));
        assert!(CONSOLE_HTML.contains("'bad'"));
        // other = schema drift (amber).
        assert!(CONSOLE_HTML.contains("schema drift (expected 4)"));
        // fetch failure = "unreachable" (amber), never a fake 0/OLD.
        assert!(CONSOLE_HTML.contains("'unreachable'"));
    }

    // ----------------------------------------------------- class FeedsCardHtml
    #[test]
    fn test_html_has_feeds_card_and_actions() {
        assert!(CONSOLE_HTML.contains("loadFeeds()"));
        assert!(CONSOLE_HTML.contains("feedToggle("));
        assert!(CONSOLE_HTML.contains("feeds-view"));
        assert!(CONSOLE_HTML.contains("feed-toggle"));
        assert!(CONSOLE_HTML.contains(r#"id="feeds""#));
        assert!(CONSOLE_HTML.contains(r#"id="feedsplit""#));
    }

    #[test]
    fn test_feeds_card_iterates_feed_list_no_hardcoded_names() {
        // Future-feeds property: the card renders whatever the app reports —
        // it iterates health.feeds / derives names from *_enabled keys, and
        // must NOT hardcode row('dhan')/row('groww') calls.
        assert!(CONSOLE_HTML.contains("j.health.feeds.map"));
        assert!(CONSOLE_HTML.contains("_enabled'"));
        assert!(!CONSOLE_HTML.contains("row('dhan'"));
        assert!(!CONSOLE_HTML.contains("row('groww'"));
    }

    #[test]
    fn test_disable_asks_for_confirmation() {
        // Review fix M3 (2026-07-16): the confirm dialog speaks REST-lane
        // truth — there is no live feed and no ticks; turning a broker off
        // stops its official-candle pulls.
        assert!(CONSOLE_HTML.contains("confirm("));
        assert!(CONSOLE_HTML.contains(
            "official-candle pulls (spot + option chain) stop until you turn it back on"
        ));
        assert!(!CONSOLE_HTML.contains("live feed? Ticks"));
    }

    #[test]
    fn test_failed_bucket_excludes_never_attempted_minutes() {
        // Review fix L4 (2026-07-16): skipped / boundary_skipped audit rows
        // are minutes the leg never ATTEMPTED — lumping them into "failed"
        // inflated the failure count. no_token IS a real failure and stays
        // counted.
        assert!(CONSOLE_HTML.contains("k!=='skipped'"));
        assert!(CONSOLE_HTML.contains("k!=='boundary_skipped'"));
        // no_token must NOT be excluded from the failed fold.
        assert!(!CONSOLE_HTML.contains("k!=='no_token'"));
    }

    #[test]
    fn test_errors_render_verbatim_through_esc() {
        assert!(CONSOLE_HTML.contains("esc(j.feeds_error"));
        assert!(CONSOLE_HTML.contains("j.app_response.error"));
    }

    #[test]
    fn test_feeds_card_shows_rest_pull_line_not_tick_counters() {
        // 2026-07-16: the per-feed detail line is today's fetch-log pulse
        // (ok/failed/rate-limited + last-hour p50/p99 after minute close),
        // sourced from rest_fetch_audit — the old ticks/subscribed counters
        // read frozen live-feed registries and are GONE.
        assert!(CONSOLE_HTML.contains("j.rest_audit"));
        assert!(CONSOLE_HTML.contains("j.rest_lat_hour"));
        assert!(CONSOLE_HTML.contains("pulls today:"));
        assert!(CONSOLE_HTML.contains("rate-limited"));
        assert!(CONSOLE_HTML.contains("after minute close"));
        assert!(CONSOLE_HTML.contains("no official-candle pulls recorded today"));
        assert!(!CONSOLE_HTML.contains("ticks_total"));
        assert!(!CONSOLE_HTML.contains("subscribed_total"));
        assert!(!CONSOLE_HTML.contains("' · ticks '"));
        assert!(!CONSOLE_HTML.contains("' · subscribed '"));
    }

    // ---------------------------------------------------- class LatencyCardHtml
    // REST-era latency card (2026-07-16): per-(broker, pull type) prompt-pull
    // percentiles from today's fetch log + the kept box-wide shields.
    fn load_latency_js() -> &'static str {
        between(
            CONSOLE_HTML,
            "async function loadLatency()",
            "async function act(",
        )
    }

    #[test]
    fn test_html_has_rest_latency_table() {
        assert!(CONSOLE_HTML.contains(r#"id="latrest""#));
        assert!(CONSOLE_HTML.contains("how fast each official minute candle arrives"));
        assert!(CONSOLE_HTML.contains("pull type"));
        assert!(CONSOLE_HTML.contains("ok pulls today"));
        assert!(CONSOLE_HTML.contains("p50 after close"));
        assert!(CONSOLE_HTML.contains("p99 after close"));
    }

    #[test]
    fn test_latency_card_iterates_rows_no_hardcoded_names() {
        // Future-feeds ratchet: the table renders whatever j.rest_latency
        // carries (feed+leg discovered from the fetch log at measure time) —
        // NO feed/leg name may be hardcoded in the portal JS.
        let js = load_latency_js();
        assert!(js.contains("j.rest_latency"));
        let lower = js.to_lowercase();
        assert!(!lower.contains("dhan"));
        assert!(!lower.contains("groww"));
        assert!(!js.contains("spot_1m"));
        assert!(!js.contains("chain_1m"));
    }

    #[test]
    fn test_empty_table_is_honest_not_fake_zero() {
        assert!(CONSOLE_HTML.contains("no successful pulls recorded today"));
        assert!(CONSOLE_HTML.contains("never fake"));
    }

    #[test]
    fn test_box_wide_cards_labeled_honestly() {
        // QuestDB RTT / clock skew carry NO feed label — the card must say
        // "box-wide"; the dormant order-placement shield is KEPT and reads
        // "—" until live trading returns.
        assert!(CONSOLE_HTML.contains("box-wide (shared by all pulls)"));
        assert!(CONSOLE_HTML.contains("QuestDB round-trip (box-wide)"));
        assert!(CONSOLE_HTML.contains("Clock skew (box-wide)"));
        assert!(CONSOLE_HTML.contains("'Order placement'"));
        assert!(CONSOLE_HTML.contains("dormant"));
    }

    // ------------------------------------ class DataDestructiveMarketHoursLock
    // Audit fix #2 (operator incident 2026-07-02 15:05 IST): the
    // data-destructive actions have NO force escape during market hours. A
    // mid-market forced wipe-ALL + docker-reset deleted ~4.5M rows and 77s of
    // live feed — upstream data in that window is unrecoverable.
    fn market_open_locked_shell() -> MockShell {
        MockShell {
            market_hours: true,
            forbid_ssm: Some("SSM must not be called for a market-hours-locked action"),
            ..MockShell::default()
        }
    }

    #[tokio::test]
    async fn test_all_data_destructive_actions_locked_in_window_even_with_force() {
        // force=true + the CORRECT confirm word — the exact call that wiped
        // the box on 2026-07-02 — must be 409'd for every destructive action.
        let confirms = [
            ("wipe-questdb", "WIPE"),
            ("docker-reset", "NUKE-DOCKER"),
            ("docker-nuke-bare", "ERASE"),
        ];
        let confirm_actions: std::collections::HashSet<&str> =
            confirms.iter().map(|(a, _)| *a).collect();
        let data_destructive: std::collections::HashSet<&str> =
            DATA_DESTRUCTIVE.iter().copied().collect();
        assert_eq!(confirm_actions, data_destructive);
        let shell = market_open_locked_shell();
        for (action, word) in confirms {
            let resp = post(
                &shell,
                json!({"action": action, "force": true, "confirm": word}),
            )
            .await;
            assert_eq!(status_of(&resp), 409, "{action}");
            let body = body_of(&resp);
            assert_eq!(body["market_hours_locked"], true, "{action}");
            let err = body["error"].as_str().unwrap();
            assert!(err.contains("locked during market hours"));
            assert!(err.contains("never be re-fetched"));
            assert!(err.contains("Run after 15:30"));
        }
    }

    #[tokio::test]
    async fn test_lifecycle_actions_keep_force_override_in_window() {
        // Emergencies stay possible: stop/reboot/restart-app/stop-app with
        // force=true must pass BOTH market-hours gates (they then reach the
        // EC2/SSM shell layer, which the mock accepts — reaching it proves
        // not-blocked).
        let shell = MockShell {
            market_hours: true,
            ssm_result: Ok("cmd-lifecycle".to_string()),
            ..MockShell::default()
        };
        for action in ["stop", "reboot", "restart-app", "stop-app"] {
            let resp = post(&shell, json!({"action": action, "force": true})).await;
            assert_eq!(status_of(&resp), 200, "{action}");
        }
    }

    #[tokio::test]
    async fn test_lifecycle_actions_without_force_still_soft_blocked_in_window() {
        // The pre-existing soft gate is unchanged: no force → 409 with the
        // force hint (NOT the hard-lock body).
        let shell = market_open_locked_shell();
        let resp = post(&shell, json!({"action": "stop", "force": false})).await;
        assert_eq!(status_of(&resp), 409);
        let body = body_of(&resp);
        assert!(body.get("market_hours_locked").is_none());
        assert!(body["error"].as_str().unwrap().contains(r#""force": true"#));
    }

    #[test]
    fn test_data_destructive_is_strict_subset_of_destructive() {
        let data: std::collections::HashSet<&str> = DATA_DESTRUCTIVE.iter().copied().collect();
        let all: std::collections::HashSet<&str> = DESTRUCTIVE.iter().copied().collect();
        assert!(data.is_subset(&all) && data.len() < all.len());
        for lifecycle in ["stop", "reboot", "restart-app", "stop-app"] {
            assert!(!DATA_DESTRUCTIVE.contains(&lifecycle));
        }
    }

    #[tokio::test]
    async fn test_out_of_window_forced_wipe_still_dispatches() {
        // After 15:30 IST the operator's forced+confirmed wipe works as before.
        let shell = MockShell {
            market_hours: false,
            ssm_result: Ok("cmd-off-hours".to_string()),
            ..MockShell::default()
        };
        let resp = post(
            &shell,
            json!({"action": "wipe-questdb", "force": true, "confirm": "WIPE"}),
        )
        .await;
        assert_eq!(status_of(&resp), 200);
        assert_eq!(body_of(&resp)["command_id"], "cmd-off-hours");
        assert!(shell.captured_joined().contains("WIPE-COMPLETE"));
    }

    #[test]
    fn test_boundary_minutes_pin_exact_semantics() {
        // Pinned lock window semantics: 09:15:00 ≤ t < 15:30:00 IST Mon-Fri.
        // 2026-06-01 is a Monday; IST = UTC + 05:30.
        let cases = [
            ((3, 44, 59), false), // 09:14:59 IST — open
            ((3, 45, 0), true),   // 09:15:00 IST — locked
            ((9, 59, 59), true),  // 15:29:59 IST — locked
            ((10, 0, 0), false),  // 15:30:00 IST — open
        ];
        for ((h, m, s), locked) in cases {
            let utc = Utc.with_ymd_and_hms(2026, 6, 1, h, m, s).unwrap();
            assert_eq!(is_market_hours(utc), locked, "{utc}");
        }
    }

    #[test]
    fn test_danger_zone_shows_lock_label_when_market_open() {
        // The label exists, starts hidden, and names the lock honestly.
        assert!(CONSOLE_HTML.contains(r#"id="dangerlock""#));
        assert!(CONSOLE_HTML.contains("Locked until 3:30 PM IST"));
        assert!(CONSOLE_HTML.contains("even with force"));
        // loadOverview un-hides it from the server's market_hours flag.
        assert!(CONSOLE_HTML.contains("dl.hidden=!j.market_hours"));
    }

    // ----------------------------------- class LegacyLiveFeedPanelsRemoved
    // 2026-07-16 REST-only cleanup ratchet: the live-feed-era helpers and
    // panels must not resurrect — their producers were deleted with the live
    // feeds (Dhan WS 2026-07-13 PR-C2/C3, Groww WS 2026-07-15).
    const SELF_SRC: &str = include_str!("operator_control.rs");
    const COMMANDS_SRC: &str = include_str!("operator_control_commands.rs");
    const ACTIONS_SRC: &str = include_str!("operator_control_action_commands.rs");

    /// Everything above the test module — the scan surface for banned
    /// identifiers, mirroring python's `hasattr(handler, …)` + source scan
    /// (this test module itself legitimately carries the needles).
    fn production_region(src: &str) -> &str {
        src.split("#[cfg(test)]").next().unwrap_or(src)
    }

    /// Strip `//` line comments (doc comments included), string-aware for
    /// `://` URL scheme separators — the house source-scan convention.
    fn strip_line_comments(src: &str) -> String {
        let mut out = String::with_capacity(src.len());
        for line in src.lines() {
            let bytes = line.as_bytes();
            let mut cut = line.len();
            let mut i = 0;
            while i + 1 < bytes.len() {
                if bytes[i] == b'/' && bytes[i + 1] == b'/' && (i == 0 || bytes[i - 1] != b':') {
                    cut = i;
                    break;
                }
                i += 1;
            }
            out.push_str(&line[..cut]);
            out.push('\n');
        }
        out
    }

    #[test]
    fn test_backend_helpers_gone() {
        // python asserted the module lacks the retired attributes; the Rust
        // twin scans the production regions of all three port modules.
        let prod = format!(
            "{}\n{}\n{}",
            production_region(SELF_SRC),
            production_region(COMMANDS_SRC),
            production_region(ACTIONS_SRC)
        );
        for name in [
            "classify_tick_conservation",
            "parse_conserve_rows",
            "CONSERVATION_ENVELOPE",
            "parse_cross_verify",
            "CROSS_VERIFY_COMMANDS",
            "GROWW_WIPE_PY",
            "wipe_groww_commands",
            "FEED_LIVE_HOSTS",
            "_pctl_",
            "percentile_winners",
            "tv_tick_processing_duration_ns",
            "tv_wire_to_done_duration_ns",
        ] {
            assert!(!prod.contains(name), "{name}");
        }
    }

    #[test]
    fn test_retired_ws_probe_hosts_absent_from_code() {
        // Review fix L1 (2026-07-16): the retired WS probe hosts legitimately
        // appear in dated retirement COMMENTS, so scan the source with
        // comments stripped (string-aware — `://` inside literals survives)
        // and assert the hosts appear NOWHERE in code (strings, URLs,
        // commands).
        let raw = format!(
            "{}\n{}\n{}",
            production_region(SELF_SRC),
            production_region(COMMANDS_SRC),
            production_region(ACTIONS_SRC)
        );
        let code_only = strip_line_comments(&raw);
        for host in ["api-feed.dhan.co", "socket-api.groww.in"] {
            assert!(!code_only.contains(host), "{host}");
            // Sanity: the scan is non-vacuous — the host IS still present in
            // the raw source (inside the dated retirement comments).
            assert!(raw.contains(host), "{host}");
        }
    }

    #[test]
    fn test_html_panels_gone() {
        for dead in [
            "drawSpark",
            r#"id="spark""#,
            r#"id="p_tps""#,
            "peak ticks/sec",
            "loadCrossVerify",
            r#"id="cvshields""#,
            "wipeGroww",
            r#"value="groww""#,
            r#"id="latfeeds""#,
            r#"id="latpctl""#,
            "Tick conservation",
            "Sub-second fix",
            "Peak ticks / second",
            "ticks captured today",
            "GROWW-WIPE-COMPLETE",
        ] {
            assert!(!CONSOLE_HTML.contains(dead), "{dead}");
        }
    }

    #[test]
    fn test_view_sql_targets_no_dead_tables() {
        let joined = VIEW_COMMANDS.join("\n");
        for dead in [
            "FROM%20ticks",
            "candles_1m",
            "tick_conservation_audit",
            "ws_event_audit",
            "MAX_TPS",
            "TICKS_TODAY",
            "CONSERVE",
            "WS_DISC",
        ] {
            assert!(!joined.contains(dead), "{dead}");
        }
    }

    // --------------------------------------------- class RedesignThreeTabs
    // 2026-07-02 portal redesign — Overview / Data / Admin, collapsed danger
    // zone, Logs + GitHub tabs removed. UI reorganization ONLY: every action
    // semantic, guard, and confirm token is unchanged; these tests pin the
    // new structure.
    #[test]
    fn test_logs_and_github_tabs_absent() {
        assert!(!CONSOLE_HTML.contains(r#"data-t="logs""#));
        assert!(!CONSOLE_HTML.contains(r#"data-t="github""#));
        // ...and their now-unused JS is gone too.
        for dead in [
            "loadLogs",
            "loadGithub",
            "ghMerge",
            "ghDeploy",
            "ciBadge",
            "runSql",
        ] {
            assert!(!CONSOLE_HTML.contains(dead), "{dead}");
        }
    }

    #[tokio::test]
    async fn test_logs_route_kept_for_mcp_server() {
        // The tickvault-logs MCP server POSTs {"action":"logs"} to this portal
        // (scripts/mcp-servers/tickvault-logs/server.py) — the API route must
        // survive the tab removal.
        let shell = MockShell {
            sync_output: "ERR_BEGIN\nERR_END\nAPP_BEGIN\nAPP_END\n".to_string(),
            ..MockShell::default()
        };
        let resp = post(&shell, json!({"action": "logs"})).await;
        assert_eq!(status_of(&resp), 200);
        assert!(body_of(&resp).get("raw").is_some());
    }

    #[tokio::test]
    async fn test_gh_actions_are_removed_from_backend() {
        // No caller existed outside the deleted GitHub tab → the routes are
        // fully dead and now 400 as unknown actions.
        let shell = MockShell::default();
        for action in ["gh_prs", "gh_merge", "gh_deploy"] {
            let resp = post(&shell, json!({"action": action})).await;
            assert_eq!(status_of(&resp), 400, "{action}");
            assert!(
                body_of(&resp)["error"]
                    .as_str()
                    .unwrap()
                    .contains("unknown action")
            );
        }
    }

    #[test]
    fn test_danger_zone_collapsed_by_default() {
        // The three destructive wipes live inside a <details> WITHOUT the open
        // attribute — collapsed until the operator deliberately taps it.
        // (The groww-only wipe left with the Groww live feed, 2026-07-16.)
        assert!(CONSOLE_HTML.contains(r#"<details class="fold" id="danger">"#));
        let danger = between(
            CONSOLE_HTML,
            r#"<details class="fold" id="danger">"#,
            "</details>",
        );
        for needle in [
            r#"value="wipe""#,
            r#"value="nuke""#,
            r#"value="erase""#,
            "dangerExecute()",
        ] {
            assert!(danger.contains(needle), "{needle}");
        }
        assert!(!danger.contains(r#"value="groww""#));
        // No <details ... open> anywhere (both folds start collapsed).
        assert!(!CONSOLE_HTML.contains("<details open"));
        assert!(!CONSOLE_HTML.contains(r#"id="danger" open"#));
    }

    #[test]
    fn test_fold_has_visible_disclosure_affordance() {
        // The operator TWICE read the folded danger zone as "the wipes were
        // DELETED" because the fold had no visual cue. Pin: every .fold
        // summary shows a ▸ chevron that flips to ▾ when open (CSS ::after).
        assert!(CONSOLE_HTML.contains("details.fold>summary::after{ content:' ▸'"));
        assert!(CONSOLE_HTML.contains("details.fold[open]>summary::after{ content:' ▾'"));
    }

    #[test]
    fn test_danger_summary_names_its_contents() {
        let summary = between(
            CONSOLE_HTML,
            r#"<details class="fold" id="danger">"#,
            "</summary>",
        );
        assert!(summary.contains("(contains: Wipe ALL · Docker reset · Bare nuke)"));
    }

    #[test]
    fn test_severity_picker_maps_each_choice_to_action_and_token() {
        // One dispatch map, radio value → the UNCHANGED per-action function.
        assert!(CONSOLE_HTML.contains("{wipe:wipeData, nuke:dockerReset, erase:bareNuke}"));
        // Each function still demands its OWN typed confirm token — the picker
        // introduces no token-bypass path.
        assert!(CONSOLE_HTML.contains("Type WIPE to confirm:')!=='WIPE'"));
        assert!(CONSOLE_HTML.contains("Type NUKE-DOCKER to confirm:')!=='NUKE-DOCKER'"));
        assert!(CONSOLE_HTML.contains("Type ERASE to confirm:')!=='ERASE'"));
        // No selection → no dispatch.
        assert!(CONSOLE_HTML.contains("Pick a danger-zone action first"));
    }

    #[test]
    fn test_context_aware_instance_button() {
        // ONE instance button: ▶ Start when stopped, ■ Stop when running —
        // label + action derived from the live instance_state.
        assert!(CONSOLE_HTML.contains(r#"id="instbtn""#));
        assert!(CONSOLE_HTML.contains("■ Stop instance"));
        assert!(CONSOLE_HTML.contains("▶ Start instance"));
        assert!(CONSOLE_HTML.contains("instState==='running'?'stop':'start'"));
        // Its force checkbox sits next to it and feeds through act().
        assert!(CONSOLE_HTML.contains(r#"id="force_inst""#));
        assert!(CONSOLE_HTML.contains("'force_inst'"));
    }

    #[test]
    fn test_stopped_box_banner_and_grey_shields() {
        // One calm banner replaces the per-shield "unreachable" scatter…
        assert!(CONSOLE_HTML.contains(r#"id="stoppedbanner""#));
        assert!(CONSOLE_HTML.contains(
        "Box stopped (auto-stops 16:30 IST, auto-starts 08:30 Mon–Fri) — guarantees resume on start"
    ));
        assert!(CONSOLE_HTML.contains("$('stoppedbanner').hidden=running"));
        // …and the shield greys out as "—" ONLY when the box is not running.
        assert!(CONSOLE_HTML.contains("shieldIdle"));
        assert!(CONSOLE_HTML.contains("if(!running){"));
        // The REAL warning paths for a RUNNING box must still exist (a stopped
        // banner must never hide genuine failures while running).
        assert!(CONSOLE_HTML.contains("'unreachable'"));
        assert!(CONSOLE_HTML.contains("DEDUP disabled!"));
        assert!(CONSOLE_HTML.contains("schema drift (expected 4)"));
    }

    #[test]
    fn test_overview_has_aws_strip_and_latency_card() {
        let overview = between(
            CONSOLE_HTML,
            r#"<section data-tab="overview""#,
            "</section>",
        );
        // Thin AWS strip (spend / alarms / disk) with click-to-expand details.
        assert!(overview.contains(r#"id="awsstrip""#));
        assert!(overview.contains(r#"id="awsdetails""#));
        assert!(overview.contains(r#"id="alarms""#));
        assert!(overview.contains(r#"id="storage""#));
        // Compact latency card reusing the existing endpoint + renderers.
        assert!(overview.contains("loadLatency()"));
        assert!(overview.contains("Measure now"));
        assert!(overview.contains(r#"id="latnet""#));
        assert!(overview.contains(r#"id="latrest""#));
        // Strip is fed by the same aws_status action the old AWS tab used.
        assert!(CONSOLE_HTML.contains("call('aws_status')"));
    }

    #[test]
    fn test_admin_tab_holds_ops_and_lock() {
        let admin = between(CONSOLE_HTML, r#"<section data-tab="admin""#, "</section>");
        for needle in [
            "act('restart-app')",
            "act('restart-questdb')",
            "act('stop-app')",
            r#"id="force""#,
            r#"id="danger""#,
            "lock()",
        ] {
            assert!(admin.contains(needle), "{needle}");
        }
        // The bare start/stop instance buttons left the Admin tab — the ONE
        // context-aware button on Overview owns instance lifecycle now.
        assert!(!admin.contains("act('start')"));
        assert!(!admin.contains("act('stop')"));
    }

    // ---------------------------------------------- class QdbConsoleUrlAction
    // B4: the `qdb_console_url` action mints a 90s one-click HMAC link to the
    // QuestDB console front Lambda (env QDB_CONSOLE_URL, TF-injected).
    #[tokio::test]
    async fn test_qdb_console_url_disabled_returns_error() {
        // Console not deployed (env empty) → honest error, no fake URL.
        let shell = MockShell::default();
        let resp = post(&shell, json!({"action": "qdb_console_url"})).await;
        assert_eq!(status_of(&resp), 400);
        assert!(
            body_of(&resp)["error"]
                .as_str()
                .unwrap()
                .contains("console not enabled")
        );
    }

    #[tokio::test]
    async fn test_qdb_console_url_enabled_returns_signed_link() {
        let shell = MockShell {
            env: std::collections::HashMap::from([(
                "QDB_CONSOLE_URL".to_string(),
                "https://console.example.test/".to_string(),
            )]),
            ..MockShell::default()
        };
        let resp = post(&shell, json!({"action": "qdb_console_url"})).await;
        assert_eq!(status_of(&resp), 200);
        let url = body_of(&resp)["url"].as_str().unwrap().to_string();
        // trailing slash on the env base must not produce '//open'
        assert!(
            url.starts_with("https://console.example.test/open?tok="),
            "{url}"
        );
        let tok = url.split_once("tok=").unwrap().1.to_string();
        let (exp_s, sig) = tok.split_once('.').unwrap();
        // The mock pins now = 1_780_000_000 → exp is exactly now + the 90s
        // TTL, and the signature equals the python-oracle HMAC vector
        // (`hmac.new(b"s3cret-token", b"qdblink|1780000090", sha256)`).
        assert_eq!(
            exp_s.parse::<i64>().unwrap(),
            1_780_000_000 + QDB_LINK_TTL_SECS
        );
        assert_eq!(
            sig,
            "643a1010a7594e457633d6a2ba7f0e31dd84efbb8b0bf4191a1b5bed07caa163"
        );
        assert_eq!(
            tok,
            mint_qdb_link_token_default_ttl(AUTH_SECRET, 1_780_000_000)
        );
    }

    #[test]
    fn test_html_has_qdb_console_button() {
        assert!(CONSOLE_HTML.contains("Open QuestDB Console"));
        assert!(CONSOLE_HTML.contains("qdb_console_url"));
        assert!(CONSOLE_HTML.contains("openQdbConsole"));
    }

    #[test]
    fn test_qdb_console_open_is_popup_safe() {
        // FIX 4: the window must be opened SYNCHRONOUSLY in the click, before
        // the await, then navigated — so the browser popup blocker allows it.
        assert!(CONSOLE_HTML.contains("window.open('','_blank')"));
        assert!(CONSOLE_HTML.contains("w.location=j.url"));
        assert!(CONSOLE_HTML.contains("w.close()"));
    }

    // ------------------------------------------------- class DeployProvenance
    // B9 deploy provenance — pure formatter + fail-soft GET contract.
    #[test]
    fn test_provenance_line_short_shas() {
        let line = provenance_line(
            "abc1234def567890abc1234def567890abc12345",
            "def5678900000000000000000000000000000000",
            "fed4321", // already short hex — must not blow up
        );
        assert_eq!(line, "binary abc1234 · portal def5678 · main fed4321");
    }

    #[test]
    fn test_provenance_line_unknown_values() {
        // "unknown" is not hex — the validator maps it (and any other
        // non-hex value) to the literal "unknown"; never raises.
        let line = provenance_line("unknown", "unknown", "unknown");
        assert_eq!(line, "binary unknown · portal unknown · main unknown");
    }

    #[test]
    fn test_provenance_line_rejects_markup_as_unknown() {
        // 2026-07-03 hardening: a poisoned SSM param / GitHub response
        // carrying markup must render as "unknown", never as markup.
        let line = provenance_line(
            "<script>alert(1)</script>",
            r#""><img src=x onerror=alert(1)>"#,
            "abc1234def567890abc1234def567890abc12345",
        );
        assert_eq!(line, "binary unknown · portal unknown · main abc1234");
    }

    #[test]
    fn test_safe_provenance_sha_validation() {
        let ok = "abc1234def567890abc1234def567890abc12345";
        assert_eq!(safe_provenance_sha(&json!(ok)), ok);
        assert_eq!(safe_provenance_sha(&json!("abc1234")), "abc1234");
        for bad in [
            json!("<script"),
            json!("ABC1234"),      // uppercase — validator is lowercase-only
            json!("abc123"),       // 6 chars — below the 7-char floor
            json!("a".repeat(41)), // above the 40-char ceiling
            json!(""),
            Value::Null,
            json!(1_234_567),
        ] {
            assert_eq!(safe_provenance_sha(&bad), "unknown", "{bad:?}");
        }
    }

    #[test]
    fn test_provenance_footer_html_escapes_line() {
        // Defense in depth: even if the formatter ever regressed, the footer
        // html-escapes the assembled line. Every sha in this fixture is
        // "unknown" (the python test env had no AWS/GitHub) — no angle
        // brackets may appear inside the rendered line.
        let footer = provenance_footer_html("unknown", "unknown", "unknown");
        assert!(footer.contains("<footer"));
        assert!(footer.contains("· portal"));
        let inner = between(after(&footer, ">"), "", "</footer>");
        assert!(!inner.contains('<'));
        assert!(!inner.contains('>'));
    }

    #[test]
    fn test_portal_sha_defaults_to_unknown() {
        // python manipulated process env around handler._portal_sha(); the
        // Rust shell seam reads env through OpsShell, so the mock env map
        // drives the SAME trim/empty→unknown resolution hermetically
        // (edition-2024 `std::env::set_var` is unsafe + racy in parallel
        // tests — deliberate ledger deviation).
        let shell = MockShell::default();
        assert_eq!(shell.portal_sha(), "unknown");
        let shell = MockShell {
            env: std::collections::HashMap::from([(
                "PORTAL_GIT_SHA".to_string(),
                "1234567890abcdef1234567890abcdef12345678".to_string(),
            )]),
            ..MockShell::default()
        };
        assert_eq!(
            shell.portal_sha(),
            "1234567890abcdef1234567890abcdef12345678"
        );
    }

    #[test]
    fn test_main_sha_hard_max_age_degrades_to_unknown() {
        // 2026-07-03 hardening: on sustained GitHub failure the cached main
        // sha is served only up to the 600s hard max-age; beyond that a
        // failed refresh returns "unknown" instead of the stale value.
        // (python patched urlopen + the cache dict; the Rust cache policy is
        // the pure pair main_sha_cache_fresh/resolve_main_sha_after_failed_fetch
        // that AwsShell::main_sha drives.)
        let sha = "abc1234def567890abc1234def567890abc12345";
        // Past the 60s fresh TTL but inside the 600s max-age: the
        // stale-but-bounded cached value is still served after a failed fetch.
        assert!(!main_sha_cache_fresh(sha, 120.0));
        assert_eq!(resolve_main_sha_after_failed_fetch(sha, 120.0), sha);
        // Older than 600s: degrade to "unknown".
        assert_eq!(resolve_main_sha_after_failed_fetch(sha, 601.0), "unknown");
    }

    #[tokio::test]
    async fn test_get_html_carries_provenance_footer() {
        // No AWS creds / no GitHub in the test env — every lookup must
        // fail-soft to "unknown" and the page must still render (the
        // provenance footer can NEVER break the GET).
        let resp = route(&get_event(), &MockShell::default()).await;
        assert_eq!(status_of(&resp), 200);
        let body = resp["body"].as_str().unwrap();
        assert!(body.contains("· portal"));
        assert!(body.contains("· main"));
        assert!(body.contains("<footer"));
    }
}
