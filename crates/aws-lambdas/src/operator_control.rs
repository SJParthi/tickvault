//! Operator portal Lambda — ONE URL to run the whole product (VIEW + CONTROL).
//!
//! Rust port (rust-only phase 2b-3, 2026-07-18) of
//! `deploy/aws/lambda/operator-control/handler.py` — the legacy file is the
//! ORACLE: every constant, string, parser, gate, and response shape below is
//! byte-parity with it, and every deliberate deviation is ledger-commented at
//! the deviating line. The 150 legacy unit tests are ported 1:1 in the test
//! module(s) of this file.
//!
//! Served via BOTH the Lambda Function URL and the API-GW v2 (payload 2.0)
//! route: `GET /` returns the public portal HTML shell (zero secrets);
//! `POST /` requires `Authorization: Bearer <secret>` (constant-time compare
//! against the SSM SecureString control secret).
//!
//! SECURITY MODEL (deliberately strict — this can stop a live trading box):
//! * Destructive box actions (stop/reboot/restart-app/stop-app) are blocked
//!   during market hours (09:15-15:40 IST Mon-Fri) unless `{"force": true}`.
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
// legacy: handler.py:112 — destructive box actions blocked during market
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

// legacy: handler.py:123 — HARD LOCK: the DATA-DESTRUCTIVE subset of
// DESTRUCTIVE — actions that DELETE market data which can NEVER be
// re-fetched from upstream. During market hours these are REFUSED with 409
// EVEN WHEN force=true. Lifecycle actions (stop / reboot / restart-app /
// stop-app) deliberately KEEP the force override so emergencies stay
// possible.
pub const DATA_DESTRUCTIVE: [&str; 3] = ["wipe-questdb", "docker-reset", "docker-nuke-bare"];

/// legacy: handler.py:125-129 (`_DATA_DESTRUCTIVE_LOCK_MSG`) — verbatim.
pub const DATA_DESTRUCTIVE_LOCK_MSG: &str = "Data-destructive actions are locked during market hours \
(09:15-15:40 IST) — a mid-market wipe destroys data that can never \
be re-fetched. Run after 15:40.";

/// legacy: `_MKT_OPEN_SECS = 9 * 3600 + 15 * 60` (09:15 IST, seconds-of-day).
pub const MKT_OPEN_SECS: u32 = 9 * 3600 + 15 * 60;
/// Market close, 15:40 IST (seconds-of-day). Legacy oracle value was
/// `15 * 3600 + 30 * 60` (15:30).
///
/// 2026-08-07: moved 15:30 -> 15:40 to follow the NSE Closing Auction change
/// of 2026-08-03 (canonical: `tickvault_common::constants::MARKET_CLOSE_IST_NANOS`).
/// While it read 15:30, `is_market_hours` reported FALSE during the closing
/// auction, so operator/infra actions gated on "outside market hours" could
/// run against a live session.
///
/// NOTE: unlike the in-workspace sites fixed alongside this one, there is NO
/// `const _: () = assert!(…)` drift-pin here — `aws-lambdas` deliberately does
/// not depend on `tickvault-common` (keeping the lambda binaries small), so
/// the canonical constant is not in scope. This value is therefore pinned by
/// TEST only (`market_close_matches_canonical_ist_close`), not by the
/// compiler. Stated plainly rather than implied.
pub const MKT_CLOSE_SECS: u32 = 15 * 3600 + 40 * 60;
/// legacy: `_IST_OFFSET_SECS = 19800  # +05:30`.
pub const IST_OFFSET_SECS: i64 = 19_800;

/// legacy: `_SECRET_TTL_SECS = 60.0` — control-secret SSM cache TTL.
pub const SECRET_TTL_SECS: f64 = 60.0;
/// legacy: `_VIEW_TIMEOUT_SECS = 6.0` — SSM RunCommand poll budget (view).
pub const VIEW_TIMEOUT_SECS: f64 = 6.0;
/// legacy: `_VIEW_POLL_SECS = 0.4` — SSM get_command_invocation poll cadence.
pub const VIEW_POLL_SECS: f64 = 0.4;
/// SSM RunCommand poll budget for the latency tab.
///
/// RE-DERIVED 2026-09-17, and the old figure is worth recording because it
/// was stale in the DANGEROUS direction. It read "one metrics scrape (3s) +
/// one QuestDB probe (3s) + chronyc + one bounded rest_fetch_audit aggregate
/// (4s); worst case ≈ 10-11s" against a 15.0 budget — but the per-socket
/// `WSLAT_RAW=` metrics curl (another 3s) was added later and never counted,
/// so the real curl sum was 13s and the true margin was 2s, not the 4-5s the
/// docstring implied.
///
/// Measured from the array itself — `LATENCY_COMMANDS` now carries THREE
/// `--max-time 3` curls (order-placement metrics, QuestDB round-trip,
/// per-socket WS lag) after the `rest_fetch_audit` aggregate was removed with
/// the per-minute REST legs (`no-rest-except-live-feed-2026-06-27.md` §12.12,
/// dated correction). `chronyc` is local and unbounded-but-instant.
///
///   3 + 3 + 3 = 9s of curl + SSM registration ≈ 10-11s worst case.
///
/// 12.0 leaves 3s over the curl sum — MORE margin than the 2s the 15.0
/// actually had, while coming down with the work it budgets for. A timeout
/// that outlives its commands is not free: it is how long an operator stares
/// at a spinner when the box is wedged.
pub const LATENCY_TIMEOUT_SECS: f64 = 12.0;

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

/// legacy: `_QDB_LINK_TTL_SECS = 90` — TTL of the one-click console link
/// token minted by the `qdb_console_url` action. The console front Lambda
/// verifies it (same control secret, HMAC over `"qdblink|<exp>"`) and swaps
/// it for a 12h session cookie — so the link in a browser history / Telegram
/// scroll dies in 90s.
pub const QDB_LINK_TTL_SECS: i64 = 90;

// ------------------------------------------------------------- pure functions

/// legacy: `_is_market_hours` (handler.py:193-199) — true during NSE market
/// hours (Mon-Fri 09:15-15:40 IST). Fixed UTC+5:30 offset exactly like the
/// oracle (India has no DST).
pub fn is_market_hours(now_utc: DateTime<Utc>) -> bool {
    let ist = now_utc + TimeDelta::seconds(IST_OFFSET_SECS);
    // legacy weekday(): Monday == 0 .. Sunday == 6; chrono mirror below.
    if ist.weekday().num_days_from_monday() >= 5 {
        return false;
    }
    let sod = ist.hour() * 3600 + ist.minute() * 60 + ist.second();
    (MKT_OPEN_SECS..MKT_CLOSE_SECS).contains(&sod)
}

/// legacy: `_http_method` (handler.py:202-206). Fail closed: a malformed
/// event is treated as an action ("POST" — hits auth), never a free page
/// serve.
pub fn http_method(event: &Value) -> String {
    if let Some(m) = event
        .pointer("/requestContext/http/method")
        .and_then(Value::as_str)
    {
        return m.to_uppercase();
    }
    // Ledger deviation: legacy `str()`-ifies a NON-STRING value found at
    // requestContext.http.method / httpMethod (e.g. str({}) → "{}"); real
    // fn-URL / API-GW v2 events always carry strings, so we fall through to
    // the same "POST" default instead of JSON-stringifying garbage.
    match event.get("httpMethod").and_then(Value::as_str) {
        Some(s) => s.to_uppercase(),
        None => "POST".to_string(),
    }
}

/// legacy: `_authorized` (handler.py:209-216) — constant-time bearer check.
/// Header keys are lowercased by the Function URL. The legacy oracle read
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
    // legacy `hmac.compare_digest` — constant-time for equal lengths, early
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

/// legacy: `_mint_qdb_link_token` (handler.py:281-291).
///
/// hmac token wire contract (byte-exact with the legacy oracle,
/// handler.py:281-291, and with the sibling `questdb-console-front` Lambda
/// which VERIFIES this token constant-time against the SAME SSM control
/// secret):
///   * `exp = now_epoch + ttl` — integer unix seconds, base-10, no padding;
///   * message = `"qdblink|<exp>"` (ASCII, pipe separator, decimal exp);
///   * sig = HMAC-SHA256(key = UTF-8 secret bytes, message) hex-encoded in
///     LOWERCASE (legacy `.hexdigest()`);
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

/// legacy default-arg form: `_mint_qdb_link_token(secret, now_epoch)` with
/// `ttl=_QDB_LINK_TTL_SECS`.
pub fn mint_qdb_link_token_default_ttl(secret: &str, now_epoch: i64) -> String {
    mint_qdb_link_token(secret, now_epoch, QDB_LINK_TTL_SECS)
}

// -------------------------------------------------------------- more constants

// ---- `REST_LAT_QUERY_MAX_SECS` is RETIRED 2026-09-17 ----
//
// The 4-second `--max-time` budget on the ONE `rest_fetch_audit` percentile
// curl in `LATENCY_COMMANDS`. That curl is gone (the ninth retired-table read
// in `operator_control_commands.rs`; full record in
// `no-rest-except-live-feed-2026-06-27.md` §12.12's dated correction), so a
// budget sized for it is a number that no longer bounds anything.
//
// The rule it encoded is KEPT and is why this tombstone exists rather than a
// silent deletion: every on-box curl in a command array carries its own
// `--max-time`, and the tab's `LATENCY_TIMEOUT_SECS` must stay above their sum
// plus SSM registration. Adding a query back means adding a budget back and
// re-deriving that total — never reusing this figure, which was fitted to a
// query nobody will write again.

/// legacy: `_FEED_API_UNREACHABLE` (handler.py:636-638) — verbatim.
pub const FEED_API_UNREACHABLE: &str =
    "app API unreachable on the box (curl to 127.0.0.1:3001 failed — is the app running?)";

/// legacy: `_BOX_UNREACHABLE` (handler.py:639) — verbatim.
pub const BOX_UNREACHABLE: &str = "box unreachable (SSM offline or instance stopped)";

// ---- `REST_AUDIT_UNPARSEABLE` / `REST_AUDIT_ABSENT` are RETIRED 2026-09-17 ----
//
// The two sentences that kept the console's pull line honest, by separating
// THREE outcomes the console had once folded into one: the query ran and
// returned nothing (a real zero), the query answered with something
// unreadable (a shape failure), and the snapshot carried no line at all (a
// malformed answer). Only the first was ever "zero pulls today".
//
// They retire with the pull line itself — `rest_fetch_audit` has no writer
// after the operator's SOCKETS-ONLY narrowing (§12.10; §12.11 names the reads
// REMOVE; the console decision is §12.12).
//
// The THREE-OUTCOME RULE they encoded is NOT retired and is the durable part:
// an absent field, an unparseable field and a genuinely empty one are three
// different facts, and collapsing them into a confident zero is the exact
// false-OK that produced the 2026-09-08 false alarm recorded in
// `parse_feeds_view` below. Any future surface that reports a count read off
// the box owes the same three-way split.

/// SSM RunCommand poll budget for the feeds tab.
///
/// RE-DERIVED 2026-09-17. The superseded 28.0 was correct arithmetic for a
/// four-curl array: 8s + 8s (app `/api/feeds` + `/api/feeds/health`) + 4s + 4s
/// (the two `rest_fetch_audit` reads) = 24s, plus SSM registration → 28s.
/// Both audit reads were removed with the per-minute REST legs
/// (`no-rest-except-live-feed-2026-06-27.md` §12.12), leaving:
///
///   8 + 8 = 16s of curl + SSM registration ≈ 18-20s worst case.
///
/// 20.0 keeps the SAME 4s margin over the curl sum that the 28.0 had over 24.
/// The budget MUST exceed the sum of every `--max-time` in
/// `FEEDS_VIEW_COMMANDS`, and the test parses them rather than copying a
/// number — adding a curl without raising the budget fails the build.
pub const FEEDS_TIMEOUT_SECS: f64 = 20.0;

/// legacy: `_MAIN_SHA_TTL_SECS = 60.0` (handler.py:875).
pub const MAIN_SHA_TTL_SECS: f64 = 60.0;
/// legacy: `_MAIN_SHA_MAX_AGE_SECS = 600.0` (handler.py:879) — on sustained
/// GitHub failure the cached main-HEAD value must not be served forever; past
/// this hard max-age a failed refresh degrades to "unknown".
pub const MAIN_SHA_MAX_AGE_SECS: f64 = 600.0;

/// The single-page operator portal — BYTE-IDENTICAL to the RAW pre-footer
/// legacy `_console_html()` template (handler.py:1510+; extracted verbatim
/// from the running oracle before the legacy source is deleted in this PR —
/// sha256 identity is ratcheted in the test module). The provenance footer is
/// spliced in at serve time by [`html_resp`], mirroring legacy
/// `_html_resp()`'s `html.replace("</body>", footer + "\n</body>", 1)`.
pub const CONSOLE_HTML: &str = include_str!("operator_control_console.html");

// --------------------------------------------------------- legacy-parity glue

/// legacy `bool(x)` truthiness for JSON values (`bool(payload.get("force",
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

/// legacy `str(payload.get(key, "")).strip()` for the string fields the
/// router reads (action / confirm / command_id / query). Ledger deviation: a
/// NON-STRING value (e.g. a JSON number) falls to "" instead of the legacy runtime's
/// `str()`-ification ("5" / "True") — untested edge, every real caller sends
/// strings.
fn payload_str<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
}

/// legacy `urllib.parse.quote(s)` with the default `safe='/'`: unreserved
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

/// legacy `round(x, 1)` byte-parity: both operate on the same IEEE double
/// with round-half-to-even of the true decimal expansion (verified vectors:
/// 5200.55 → 5200.6, 1450.04 → 1450.0).
fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

/// legacy `html.escape(s)` (quote=True): `&` first, then `< > " '`.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Last `n` CHARS of a string — legacy `s[-1500:]` slicing semantics
/// (char-boundary safe on UTF-8).
fn last_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// First `n` CHARS — legacy `s[:500]` slicing semantics.
fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// The hex/feed-name gate regexes cannot fail to compile; the `Option` +
// fail-closed arms exist only to satisfy the no-unwrap/no-expect lints.
static FEED_NAME_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_-]{0,31}$").ok());
static HEX_SHA_RE: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{7,40}$").ok());

// ---- `TOKEN_RE` / `is_token` / `parse_num` are RETIRED 2026-09-17 ----
//
// All three existed for `parse_rest_lat_row` and the `rest_fetch_audit`
// parsers above it, which went with the per-minute REST legs
// (`no-rest-except-live-feed-2026-06-27.md` §12.12 and its dated correction).
// `is_token` gated `<feed>`/`<leg>` symbols and `parse_num` turned
// `""`/`null`/`NaN` into `None` so a percentile that the database could not
// compute never became a plausible number.
//
// BOTH rules are still enforced, by the parsers that survive — which is why
// this is a tombstone and not a lost guarantee:
//   * `FEED_NAME_RE` (immediately above) is the SAME charset gate, anchored
//     tighter (must start with a letter), and `is_feed_name` applies it to
//     every feed symbol the console renders.
//   * `parse_ws_lag_rows` drops a malformed Prometheus line rather than
//     defaulting it, and `prom_label` returns `None` instead of falling back
//     to connection 0.
//
// A future parser needing either must re-derive it against its own input.
// Reviving these verbatim would import a charset fitted to a wire format
// (the on-box CSV) that no longer exists.

// ------------------------------------------------------------------ view snap

/// legacy: `_parse_feed_counts` (handler.py:357-372) — parse a *_BY_FEED
/// value (';'-joined QuestDB CSV rows like `"dhan",123;"groww",45;`) into
/// {feed: count_str}. Malformed fragments are skipped; an empty/absent value
/// yields {} (the UI then shows nothing rather than fabricated zeros).
///
/// The `"groww"` in that example and in this module's parser fixtures is NOT
/// stale after the 2026-08-21 feed removal: the `feed` column is part of every
/// persisted DEDUP key, and the historical `feed='groww'` rows are retained
/// (SEBI never-delete), so a GROUP BY feed genuinely still returns them. The
/// parser must keep handling any feed label it is handed, known or retired.
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

/// legacy: `_sum_counts` (handler.py:375-380) — sum the parseable
/// non-negative integer count strings; "" when NONE parsed (box stopped /
/// QuestDB down) — the hero then shows nothing rather than a fabricated 0.
pub fn sum_counts(vals: &[&str]) -> String {
    let mut sum: u128 = 0;
    let mut any = false;
    for v in vals {
        let t = v.trim();
        // legacy `.isdigit()` then arbitrary-precision int(); u128 covers any
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

/// legacy: `_parse_view` (handler.py:383-418) — parse the labeled snapshot
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
    let (ticks, depth) = (get("TICKS_TODAY"), get("DEPTH_TODAY"));
    json!({
        "app": get("APP"),
        // Rows captured today by the LIVE socket lane — the two live tables,
        // per table + per feed. See VIEW_COMMANDS.
        //
        // 2026-09-03: `contracts` is GONE from all three fields, not zeroed.
        // `rest_option_contract_1m` has had no DDL caller and no writer since
        // the 2026-08-21 Groww removal, so a "contracts: 0" bar would have
        // said *the leg captured nothing today* about a leg that no longer
        // exists — and a zero an operator cannot distinguish from a real
        // outage is worse than an absent field. Rationale + the measurement
        // live on VIEW_COMMANDS in operator_control_commands.rs.
        //
        // ⚠ 2026-09-17: these fields are RE-POINTED, not merely renamed. They
        // read `rest_spot_1m` / `rest_option_chain_1m` until the operator's
        // SOCKETS-ONLY narrowing removed both writers
        // (`no-rest-except-live-feed-2026-06-27.md` §12.10; the decision and
        // its alternatives are §12.12). Both tables are RETAINED and hold real
        // history, so these queries did not ERROR — they returned a permanent
        // zero for `today()`, which is the 2026-09-03 paragraph above arriving
        // a second time and in its worse form. The names move WITH the
        // queries: a re-pointed read under its old name is the stale-name trap
        // §12.8(a)/§12.9(a) record, and an operator reading "spot" would
        // believe they were looking at the retired REST leg.
        "rows_today": {"ticks": ticks, "depth": depth},
        "rows_today_total": sum_counts(&[&ticks, &depth]),
        "rows_by_feed": {
            "ticks": parse_feed_counts(&get("TICKS_BY_FEED")),
        },
        // Upsert-key count for `ticks` — FIVE columns
        // (ts, security_id, segment, capture_seq, feed), per
        // `tick_persistence::DEDUP_KEY_TICKS` and pinned by
        // `questdb_init_script_guard.rs`. The expectation moves 4 → 5 with the
        // re-point; keeping the 4 would have shown a red "DEDUP disabled /
        // schema drift" shield on a perfectly healthy box, every day.
        "dedup_key_columns": get("DEDUP_KEYS"),
        "recent_errors": errors,
    })
}

// --------------------------------------------------------------- latency snap

/// legacy: `_avg_ns` (handler.py:486-493) — sum/count → average nanoseconds,
/// or None if no samples.
pub fn avg_ns(sum_v: &str, count_v: &str) -> Option<f64> {
    let s: f64 = sum_v.trim().parse().ok()?;
    let c: f64 = count_v.trim().parse().ok()?;
    if c > 0.0 { Some(s / c) } else { None }
}

// ---- `parse_rest_lat_row` is RETIRED 2026-09-17 ----
//
// It parsed one `<feed>,<leg>,<ok_rows>,<p50>,<p99>` line out of the
// `RESTLAT_ROW=` stream. Its producer — the `rest_fetch_audit` percentile
// curl — was removed with the per-minute REST legs
// (`no-rest-except-live-feed-2026-06-27.md` §12.12, dated correction), so
// nothing emits that prefix any more.
//
// The DISCIPLINE it carried is preserved by its siblings and is restated here
// because it is the durable half: a malformed on-box line yielded `None` and
// the row was DROPPED — never defaulted, never half-parsed into a plausible
// wrong number. `parse_ws_lag_rows` and `parse_feed_counts` still hold that
// line, and any future latency parser must.

/// legacy: `sec_to_ms` inside `_parse_latency` (handler.py:567-571) —
/// `f"{float(s) * 1000:.1f}"`, "" on unparseable.
fn sec_to_ms(s: &str) -> String {
    s.trim()
        .parse::<f64>()
        .map(|f| format!("{:.1}", f * 1000.0))
        .unwrap_or_default()
}

/// Extract a label's value from a Prometheus series line.
///
/// `tv_dhan_ws_lag_ms_bucket{connection="3",le="250"} 17` → `("3")` for
/// `connection`. Returns `None` when the label is absent, which is how a
/// malformed or unexpected line is DROPPED rather than defaulted into
/// connection 0 and silently merged with a real socket's samples.
fn prom_label<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let needle = format!("{label}=\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// The numeric sample at the end of a Prometheus series line.
fn prom_value(line: &str) -> Option<f64> {
    line.rsplit_once(' ')
        .and_then(|(_, v)| v.trim().parse::<f64>().ok())
}

/// Per-connection `{connection, samples, p50_ms, p99_ms}` rows from the raw
/// `tv_dhan_ws_lag_ms` histogram lines.
///
/// # Why the percentiles are computed here and not in shell
///
/// A histogram is exposed as cumulative `_bucket{le=…}` series plus `_sum` and
/// `_count`. Deriving a percentile means finding the first bucket whose
/// cumulative count reaches `q × total`. Doing that in awk on the box would
/// produce a number that LOOKS right when the bucket walk is subtly wrong —
/// precisely the failure this panel exists to end.
///
/// # The resolution limit, which the panel must state
///
/// The value returned is the bucket's UPPER BOUND, so it OVER-estimates within
/// a bucket: a p99 of 250 means "at or below 250 ms", not "250 ms". Combined
/// with the ±1 s floor from Dhan's whole-second timestamps, this is an outage
/// and drift signal, never a precision instrument. `+Inf` is reported as-is
/// rather than as a number, because a p99 in the overflow bucket means the true
/// value is unbounded above and printing the largest finite bound would be a
/// lie in the safe direction.
fn parse_ws_lag_rows(raw: &[String]) -> Vec<Value> {
    use std::collections::BTreeMap;
    /// One socket's raw series, gathered before any arithmetic.
    #[derive(Default)]
    struct Conn {
        buckets: BTreeMap<String, f64>,
        count: f64,
        endpoint: Option<String>,
        instruments: Option<f64>,
        frames: Option<f64>,
        tick_age_secs: Option<f64>,
    }
    let mut per_conn: BTreeMap<u32, Conn> = BTreeMap::new();

    for line in raw {
        let Some(conn) = prom_label(line, "connection") else {
            continue;
        };
        // A numeric slot, never a string key: "10" must sort AFTER "9", and
        // the `unknown` bucket the drain degrades into is not a socket.
        let Ok(slot) = conn.parse::<u32>() else {
            continue;
        };
        let Some(value) = prom_value(line) else {
            continue;
        };
        let entry = per_conn.entry(slot).or_default();
        if let Some(endpoint) = prom_label(line, "endpoint") {
            entry.endpoint = Some(endpoint.to_string());
        }
        if line.starts_with("tv_dhan_ws_lag_ms_bucket") {
            if let Some(le) = prom_label(line, "le") {
                entry.buckets.insert(le.to_string(), value);
            }
        } else if line.starts_with("tv_dhan_ws_lag_ms_count") {
            entry.count = value;
        } else if line.starts_with("tv_dhan_ws_conn_instruments") {
            entry.instruments = Some(value);
        } else if line.starts_with("tv_dhan_ws_conn_frames") {
            entry.frames = Some(value);
        } else if line.starts_with("tv_dhan_ws_conn_tick_age_secs") {
            // The drain publishes -1 for "never delivered"; that is an
            // absence, not a negative age, and must not render as a number.
            entry.tick_age_secs = (value >= 0.0).then_some(value);
        }
    }

    per_conn
        .into_iter()
        // A socket with measured ticks OR published stats gets a row. A bare
        // zero-count histogram with nothing else is a series the exporter
        // pre-registered, not a socket that exists.
        .filter(|(_, c)| {
            c.count > 0.0 || c.instruments.is_some() || c.frames.is_some() || c.endpoint.is_some()
        })
        .map(|(slot, c)| {
            // Sort by numeric bound; `+Inf` sorts last by construction.
            let mut bounds: Vec<(f64, &String, f64)> = c
                .buckets
                .iter()
                .map(|(le, cum)| (le.parse::<f64>().unwrap_or(f64::INFINITY), le, *cum))
                .collect();
            bounds.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

            let count = c.count;
            let quantile = |q: f64| -> Value {
                if count <= 0.0 {
                    // No ticks measured: there is no percentile, and a zero
                    // here would read as "instant delivery".
                    return Value::Null;
                }
                let target = q * count;
                // The bucket's `le` STRING is deliberately unused here — the
                // decision below is made on the PARSED value's finiteness, for
                // the reason spelled out in the comment inside the branch. It
                // stays in the tuple because `bounds` is also the sort key
                // carrier; binding it as `_le` keeps that shape while telling
                // the compiler (and `-D warnings`, which `Build & Verify` runs)
                // that the omission is deliberate rather than an oversight.
                for (bound, _le, cum) in &bounds {
                    if *cum >= target {
                        // NOT `le.parse().is_err()`: Rust parses "+Inf" as a
                        // VALID f64 infinity, so an Err branch here never
                        // fires — and `serde_json` cannot encode infinity, so
                        // the overflow bucket would silently render as `null`
                        // and the panel would show a blank cell for the worst
                        // case it exists to expose. Test the finiteness of the
                        // parsed value instead.
                        return if bound.is_finite() {
                            json!(*bound)
                        } else {
                            // Unbounded above. A string so the page cannot
                            // render it as a finite number.
                            json!("+Inf")
                        };
                    }
                }
                Value::Null
            };

            json!({
                "connection": slot.to_string(),
                "endpoint": c.endpoint,
                "instruments": c.instruments,
                "frames": c.frames,
                "tick_age_secs": c.tick_age_secs,
                "samples": count,
                "p50_ms": quantile(0.50),
                "p99_ms": quantile(0.99),
            })
        })
        .collect()
}

/// legacy: `_parse_latency` (handler.py:535-585) — parse the labeled latency
/// snapshot. WSLAT_RAW lines carry the per-socket live delivery-lag histogram
/// and gauges; the METRICS block carries the dormant order-placement
/// histogram. Empty stdout (box stopped) degrades to an empty table + blank
/// shields — the page says so honestly instead of fabricating numbers.
///
/// 2026-09-17: the `RESTLAT_ROW=` aggregate is no longer parsed — its
/// `rest_fetch_audit` producer went with the per-minute REST legs.
pub fn parse_latency(stdout: &str) -> Value {
    let mut metrics: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut ws_raw: Vec<String> = Vec::new();
    let mut in_metrics = false;
    for line in stdout.lines() {
        // 2026-09-17: the `RESTLAT_ROW=` arm is GONE with its producer. An
        // older Lambda build still emitting the prefix now falls through to
        // the generic `k=v` arm below and lands in `fields`, where nothing
        // reads it — ignored, never rendered, and never surfaced as a
        // latency row sourced from a table with no writer.
        if let Some(raw) = line.strip_prefix("WSLAT_RAW=") {
            ws_raw.push(raw.to_string());
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
        // 2026-09-17: the `rest_latency` field is GONE, not emptied. It
        // carried per-(feed, leg) "how fast after each minute closed"
        // percentiles from `rest_fetch_audit`; with no per-minute fetch left
        // to time, an always-`[]` field would read as "the pulls ran and were
        // never late" — the zero-an-operator-cannot-distinguish class this
        // file records twice above.
        // Per-SOCKET live-feed delivery lag. Empty list = the lane produced no
        // ticks (feed off, box stopped, or /metrics unreachable) — the page
        // says that rather than showing a comforting zero.
        "ws_latency": parse_ws_lag_rows(&ws_raw),
        "questdb_ms": sec_to_ms(fields.get("QDB").map_or("", String::as_str)),
        "clock_skew_ms": sec_to_ms(fields.get("SKEW").map_or("", String::as_str)),
        "order_place_avg_ns": order_avg.map(round1),
    })
}

// --------------------------------------------------------------- storage snap

/// legacy: `_parse_storage` (handler.py:598-614) — parse df/du output into
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

/// legacy: `_extract_marked_json` (handler.py:688-699) — extract + parse the
/// JSON between two marker lines. Returns (parsed_or_Null, error_str); never
/// fabricates a payload.
pub fn extract_marked_json(stdout: &str, begin: &str, end: &str) -> (Value, String) {
    let mut raw = "";
    if stdout.contains(begin)
        && stdout.contains(end)
        && let Some((_, after)) = stdout.split_once(begin)
    {
        // legacy `.split(end, 1)[0]` of a string WITHOUT `end` (marker before
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

// ---- `parse_rest_audit` and `parse_rest_lat_hour` are RETIRED 2026-09-17 ----
//
// They turned the two `rest_fetch_audit` snapshot lines into
// `{feed: {outcome: count}}` and `{feed: {p50_ms, p99_ms}}`. Both sources are
// gone with the per-minute REST legs (§12.10 / §12.11 / §12.12).
//
// Their shared DISCIPLINE is not retired and binds every future snapshot
// parser here: charset-validate each fragment, SKIP anything malformed, and
// let an empty input yield an empty map — never a fabricated zero row. The
// caller, not the parser, decides what an empty map MEANS.
//
// `parse_feed_counts` below is the surviving example of that shape and is
// still in use by the Data tab.

/// legacy: `_parse_feeds_view` (handler.py:758-798) — parse the marked
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
            // Both fields are `null` + a STRUCTURED error, never an empty map.
            //
            // That shape is the 2026-09-08 lesson, kept after the third field
            // that taught it was retired with the REST legs: an empty map
            // renders as a SENTENCE ("no ... recorded today"), so a blind read
            // and a genuinely idle lane produce identical words. The operator
            // read that sentence for a whole morning while the lane was
            // healthy and flushing every minute.
            //
            // A false ALARM manufactured by an unreadable data path costs the
            // same thing a false OK does: trust in the surface. Any field
            // added here owes the same three-way split — see the retired
            // feeds-card tests in this file's test module for the full record.
        });
    }
    let (feeds, feeds_error) = extract_marked_json(stdout, "FEEDS_BEGIN", "FEEDS_END");
    let (health, health_error) =
        extract_marked_json(stdout, "FEEDS_HEALTH_BEGIN", "FEEDS_HEALTH_END");
    // ⚠ 2026-09-17: the labeled-line scan that stood here is REMOVED with the
    // only two lines it ever read — `REST_AUDIT=` and `REST_LAT_HOUR=`, both
    // sourced from `rest_fetch_audit`, whose writer went with the per-minute
    // REST legs (`no-rest-except-live-feed-2026-06-27.md` §12.12). Every
    // command in `FEEDS_VIEW_COMMANDS` now emits marker-delimited JSON and
    // nothing else, so the scan had no input and its output had no reader:
    // a `HashMap` built on every call and dropped unread.
    //
    // TWO RULES it encoded, restated because re-adding a labeled line means
    // re-adding the scan and both are easy to miss the second time:
    //
    //   1. The scan MUST skip between the BEGIN/END markers. A '=' inside a
    //      JSON body is not a labeled line, and a body carrying one would
    //      otherwise register a phantom field.
    //   2. A field read that way owes the three-way ABSENT / UNPARSEABLE /
    //      GENUINELY-EMPTY split — see the unreachable arm above.
    json!({
        "feeds": feeds,
        "feeds_error": feeds_error,
        "health": health,
        "health_error": health_error,
    })
}

/// legacy: `_validate_feed_toggle` (handler.py:801-815) — strict validation
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
        return "feed must be a lowercase feed name like 'dhan' or 'truedata'".to_string();
    }
    if !enabled.is_boolean() {
        return "enabled must be a JSON boolean (true or false)".to_string();
    }
    String::new()
}

/// legacy: `_feed_toggle_commands` (handler.py:818-835) — build the SSM curl
/// for POST /api/feeds/{feed}. Only called AFTER [`validate_feed_toggle`]
/// passed, so `feed` is a charset-locked lowercase token and `enabled` is a
/// real bool — the interpolations cannot carry shell metacharacters. The
/// body literal mirrors legacy `json.dumps({"enabled": enabled})` byte-exact
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

/// legacy: `_parse_feed_toggle` (handler.py:838-861) — parse the toggle
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

/// legacy: `_safe_provenance_sha` (handler.py:935-945) — hex-validate one
/// provenance sha (7-40 lowercase hex, else "unknown"), so a poisoned SSM
/// param / GitHub response can never smuggle markup into the footer.
pub fn safe_provenance_sha(sha: &Value) -> String {
    match sha.as_str() {
        Some(s) if HEX_SHA_RE.as_ref().is_some_and(|re| re.is_match(s)) => s.to_string(),
        _ => "unknown".to_string(),
    }
}

/// legacy: `_provenance_line` (handler.py:948-954) — pure formatter: short-7
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

/// legacy: `_provenance_footer_html` (handler.py:957-966) — defense in
/// depth: the line is already hex-validated per sha, and the assembled
/// string is ALSO html-escaped before splicing into the page.
pub fn provenance_footer_html(binary_sha: &str, portal_sha: &str, main_sha: &str) -> String {
    let line = html_escape(&provenance_line(binary_sha, portal_sha, main_sha));
    format!(
        "<footer style=\"margin-top:26px;text-align:center;font-size:11px;\
color:var(--mut);opacity:.75\">{line}</footer>"
    )
}

/// Pure half of the legacy `_main_sha` cache policy (handler.py:898-932):
/// is the cached value fresh enough to skip the GitHub fetch entirely?
pub fn main_sha_cache_fresh(cached: &str, age_secs: f64) -> bool {
    !cached.is_empty() && age_secs <= MAIN_SHA_TTL_SECS
}

/// Pure half of the legacy `_main_sha` failure arm (handler.py:927-932): on
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

/// legacy: `_resp` (handler.py:970-971). Ledger deviation: legacy
/// `json.dumps` emits `", "` / `": "` separators in insertion order; serde
/// emits compact separators in sorted key order — semantically identical
/// JSON (every consumer `JSON.parse`s / `json.loads`es the body; the ported
/// tests compare PARSED values, exactly like the legacy tests).
pub fn resp(status: u16, body: &Value) -> Value {
    json!({
        "statusCode": status,
        "headers": {"content-type": "application/json"},
        "body": body.to_string(),
    })
}

/// legacy: `_html_resp` (handler.py:974-991) — B9 deploy provenance: inject
/// the footer just before `</body>` so the giant static console template
/// stays untouched. (The legacy try/except fail-soft around the replace is
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

/// Side-effect surface of the router — the legacy oracle's monkeypatch seams
/// (`_control_secret` / `_is_market_hours` / `_ssm_shell` / `_ssm_shell_sync`
/// / `_client(...)` / env reads) as ONE trait, so [`route`] is a pure
/// function of (event, shell). Tests implement a MockShell mirroring the
/// legacy monkeypatching; production uses [`AwsShell`].
// APPROVED: handler is generic over the trait (no dyn); futures awaited inline.
#[allow(async_fn_in_trait)]
pub trait OpsShell {
    /// legacy `_control_secret()` — SSM SecureString, 60s-cached.
    async fn control_secret(&self) -> String;
    /// legacy `_is_market_hours(datetime.datetime.utcnow())`.
    fn market_hours_now(&self) -> bool;
    /// legacy `int(time.time())`.
    fn now_epoch(&self) -> i64;
    /// legacy `os.environ.get(key, "")`.
    fn env(&self, key: &str) -> String;
    /// legacy `_client("ec2").start_instances(...)`.
    async fn ec2_start(&self) -> Result<(), String>;
    /// legacy `_client("ec2").stop_instances(...)`.
    async fn ec2_stop(&self) -> Result<(), String>;
    /// legacy `_client("ec2").reboot_instances(...)`.
    async fn ec2_reboot(&self) -> Result<(), String>;
    /// legacy `describe_instances(...)["Reservations"][0]["Instances"][0]["State"]["Name"]`.
    async fn instance_state(&self) -> Result<String, String>;
    /// legacy `_ssm_shell(commands)` → CommandId (Err → the 500 arm).
    async fn ssm_shell(&self, commands: &[String]) -> Result<String, String>;
    /// legacy `_ssm_shell_sync(commands, timeout)` → stdout+stderr, "" on
    /// any failure (box stopped / SSM offline) — callers degrade to an empty
    /// snapshot, never a 500.
    async fn ssm_shell_sync(&self, commands: &[String], timeout_secs: f64) -> String;
    /// legacy `get_command_invocation(...)` → (Status, StdOut, StdErr);
    /// Err → the Pending 200 arm (not registered yet / box offline).
    async fn command_invocation(
        &self,
        command_id: &str,
    ) -> Result<(String, String, String), String>;
    /// legacy `describe_alarms(StateValue="ALARM", MaxRecords=20)` → alarm
    /// names; None on failure (fail-soft — the UI shows null).
    async fn firing_alarms(&self) -> Option<Vec<String>>;
    /// legacy `_month_to_date_cost()` — "" on any failure.
    async fn month_to_date_cost(&self) -> String;
    /// legacy `_binary_sha()` — fail-soft to "unknown".
    async fn binary_sha(&self) -> String;
    /// legacy `_portal_sha()` — env PORTAL_GIT_SHA or "unknown".
    fn portal_sha(&self) -> String;
    /// legacy `_main_sha()` — GitHub main HEAD, 60s cache / 600s max-age,
    /// fail-soft to "unknown".
    async fn main_sha(&self) -> String;
}

/// legacy 500 arm: `except Exception: _resp(500, {"error": "action failed",
/// "action": action})`.
fn action_failed(action: &str) -> Value {
    resp(500, &json!({"error": "action failed", "action": action}))
}

/// Merge `extra`'s object fields into `base` (legacy `{**a, **b}` splat).
fn merged(base: Value, extra: &Value) -> Value {
    let mut m = base.as_object().cloned().unwrap_or_default();
    if let Some(e) = extra.as_object() {
        for (k, v) in e {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

/// legacy: `lambda_handler` (handler.py:994-1489) — the full router, ported
/// branch-for-branch (same gate order, same response bodies, same status
/// codes). Generic over the shell so the ported routing tests drive it with
/// a mock exactly like the legacy tests monkeypatched the module globals.
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

    // legacy: `raw = event.get("body") or "{}"`; dict passthrough, else
    // json.loads (fail → 400). Ledger deviation: a parsed NON-OBJECT payload
    // (e.g. `[1]`) is treated as {} — the legacy runtime would AttributeError → 502
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
                "error": "blocked during market hours (09:15-15:40 IST). Re-send with {\"force\": true} to override.",
                "action": action,
            }),
        );
    }

    match action.as_str() {
        // ---- box control ----
        "start" => match shell.ec2_start().await {
            Ok(()) => resp(200, &json!({"ok": true, "action": action})),
            Err(exc) => {
                tracing::error!(code = "LAMBDA-PORTAL-01", %action, %exc, "operator-portal action failed");
                action_failed(&action)
            }
        },
        "stop" => match shell.ec2_stop().await {
            Ok(()) => resp(200, &json!({"ok": true, "action": action})),
            Err(exc) => {
                tracing::error!(code = "LAMBDA-PORTAL-01", %action, %exc, "operator-portal action failed");
                action_failed(&action)
            }
        },
        "reboot" => match shell.ec2_reboot().await {
            Ok(()) => resp(200, &json!({"ok": true, "action": action})),
            Err(exc) => {
                tracing::error!(code = "LAMBDA-PORTAL-01", %action, %exc, "operator-portal action failed");
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
                    tracing::error!(code = "LAMBDA-PORTAL-01", %action, %exc, "operator-portal action failed");
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
                    tracing::error!(code = "LAMBDA-PORTAL-01", %action, %exc, "operator-portal action failed");
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
                    tracing::error!(code = "LAMBDA-PORTAL-01", %action, %exc, "operator-portal action failed");
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
                    tracing::error!(code = "LAMBDA-PORTAL-01", %action, %exc, "operator-portal action failed");
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
                    // Audit line → CloudWatch (legacy `print` → tracing; the
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
                    tracing::error!(code = "LAMBDA-PORTAL-01", %action, %exc, "operator-portal action failed");
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
                    tracing::error!(code = "LAMBDA-PORTAL-01", %action, %exc, "operator-portal action failed");
                    action_failed(&action)
                }
            }
        }
        // ---- overview / data ----
        "status" | "view" => {
            let state = match shell.instance_state().await {
                Ok(s) => s,
                Err(exc) => {
                    tracing::error!(code = "LAMBDA-PORTAL-01", %action, %exc, "operator-portal action failed");
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
/// it feeds are what the unit tests cover, exactly like the legacy oracle's
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
    /// legacy `_cache` — 60s-TTL SSM param cache.
    param_cache: std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
    /// legacy `_main_sha_cache` — (value, fetched_at).
    main_sha_cache: std::sync::Mutex<(String, Option<std::time::Instant>)>,
}

fn lock_unpoisoned<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl AwsShell {
    /// Build the live shell from the Lambda execution-role environment —
    /// legacy module-init parity (`INSTANCE_ID` / `_SECRET_PARAM` /
    /// provenance env vars).
    pub async fn new() -> Self {
        let config = crate::clients::sdk_config().await;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3)) // APPROVED: legacy parity — urllib timeout=3 on the provenance GitHub read.
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

    /// legacy `_load_param` — "" on any failure (missing ⇒ deny / disable,
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

    /// legacy `_cached_param` — refetch when absent, empty, or older than
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

    /// One bounded GitHub main-HEAD read (legacy `_main_sha` fetch leg).
    async fn fetch_main_sha(&self) -> Option<String> {
        let http = self.http.as_ref()?;
        let url = format!(
            "https://api.github.com/repos/{}/commits/main", // APPROVED: fixed infrastructure host — legacy parity (handler.py:911), token-free URL.
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
        // legacy `["Reservations"][0]["Instances"][0]["State"]["Name"]` —
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
        // legacy `_ssm_shell_sync` (handler.py:301-321): "" on any failure —
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
        // legacy `_month_to_date_cost` (handler.py:1492-1507) — "" on any
        // failure. Lambda env clock is UTC (legacy date.today() parity).
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

    /// 2026-08-07 drift pin. `aws-lambdas` deliberately does NOT depend on
    /// `tickvault-common` (lambda binary size), so `MARKET_CLOSE_IST_NANOS` is
    /// out of scope for a `const _: () = assert!(…)`. This test is therefore
    /// the ONLY thing pinning this boundary — it is checked by TEST, not by
    /// the compiler, and that difference is stated plainly rather than implied.
    ///
    /// If the canonical NSE close moves again, this test must move with it.
    #[test]
    fn market_close_matches_canonical_ist_close() {
        // MARKET_CLOSE_IST_NANOS == 56_400_000_000_000 (15:40 IST) since the
        // NSE Closing Auction change of 2026-08-03.
        assert_eq!(
            MKT_CLOSE_SECS, 56_400,
            "operator-control market close drifted from the canonical NSE close"
        );
        assert_eq!(MKT_OPEN_SECS, 33_300, "market open drifted");
    }

    #[test]
    fn test_closing_auction_window_is_market_hours() {
        // 15:35 IST Mon == 10:05 UTC Mon — inside the closing auction, which
        // read as CLOSED while MKT_CLOSE_SECS was stale at 15:30.
        let utc = Utc.with_ymd_and_hms(2026, 6, 1, 10, 5, 0).unwrap();
        assert!(is_market_hours(utc));
    }

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
    // legacy monkeypatched `handler._control_secret`; the Rust core takes the
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
        // Cross-language freeze with the console lambdas' legacy suites
        // (deploy/aws/lambda/questdb-console-{front,proxy}/test_handler.py):
        // with the legacy operator-control oracle retired, those suites pin
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
    // (pinned by the legacy docstring; exercised here so the port cannot
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
        // i64-overflowing positive limit still clamps (legacy bigint parity).
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
    fn mint_qdb_link_token_matches_legacy_hexdigest_vector() {
        // Byte-exact oracle vector: legacy
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
    // Full 1:1 port of the remaining legacy test classes
    // (deploy/aws/lambda/operator-control/test_handler.py). Test NAMES match
    // the legacy names byte-for-byte so the parity check is mechanical.
    // =====================================================================

    use crate::operator_control_action_commands::{
        DOCKER_NUKE_BARE_COMMANDS, DOCKER_RESET_COMMANDS, WIPE_QUESTDB_COMMANDS,
    };
    use crate::operator_control_commands::{FEEDS_VIEW_COMMANDS, LATENCY_COMMANDS, VIEW_COMMANDS};

    // ------------------------------------------------------------ MockShell
    // The legacy tests monkeypatched module globals (`_control_secret`,
    // `_is_market_hours`, `_ssm_shell`, `_ssm_shell_sync`, `_client`); the
    // Rust router is generic over [`OpsShell`], so ONE mock mirrors every
    // seam. Defaults mirror the legacy fixtures: secret "s3cret-token",
    // market CLOSED (WipeGate pins the clock off-hours), instance running.
    struct MockShell {
        secret: String,
        market_hours: bool,
        now: i64,
        env: std::collections::HashMap<String, String>,
        ssm_result: Result<String, String>,
        /// `Some(msg)` = the legacy `self.fail("SSM must not be called…")`
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
            // mock env map (the legacy runtime manipulated process env; edition-2024
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
    fn console_html_content_pin() {
        // ORIGINAL PURPOSE (unchanged in spirit): this pinned the RAW
        // pre-footer legacy `_console_html()` template byte-for-byte, so the
        // port could be proven not to have drifted during migration. That
        // golden was
        // `7edc78757c33cf3cd3f17971dd47648b50fbb151ae1a947da2cd06d3a6f430ce`.
        //
        // 2026-08-07: FIRST INTENTIONAL DIVERGENCE from the legacy template.
        // The danger-zone label read "Locked until 3:30 PM IST" — factually
        // WRONG after the NSE Closing Auction change of 2026-08-03 moved the
        // close to 15:40. The destructive-action lock now genuinely holds
        // until 15:40, so the old label told the operator the lock lifts ten
        // minutes before it does — an operator-facing false signal in exactly
        // the class this repo's guards exist to eliminate. Correcting it is
        // worth more than byte-identity with a retired template.
        //
        // 2026-08-08: SECOND intentional divergence (operator Quote 14). The
        // stopped-box banner read "auto-stops 16:30 IST" — factually wrong once
        // the 9-hour window moved the stop to 17:30. Same class as the 2026-08-07
        // correction above: an operator-facing time that lies by an hour. Note
        // the ratchet caught this edit, which is exactly its job — the banner is
        // the operator's only in-console statement of when the box goes down.
        //
        // 2026-08-14: THIRD divergence, and unlike the first two this one is
        // not a correction — it is a mechanical follow-on. The QuestDB table
        // `spot_1m_rest` was renamed `rest_spot_1m`, and the console embeds
        // that name in two places: the Data-tab default query the operator
        // actually runs, and a dedup-key comment. A stale name in the default
        // query is not cosmetic — it hands the operator a query that errors
        // out on a table that no longer exists, on the one screen he uses to
        // check the box. Byte-length is UNCHANGED (both names are 12 chars),
        // so only the digest moves; that is itself the evidence this edit is
        // a rename and nothing else.
        //
        // 2026-08-14: FOURTH divergence, and the first that ADDS a panel
        // rather than correcting a string. The LATENCY card showed only REST
        // pull rows — the operator noticed and asked where the WebSocket
        // numbers were. The honest answer was that NOTHING on the box measured
        // live-socket lag: `tv_dhan_exchange_lag_p99_seconds` was retired
        // 2026-07-17 and nothing replaced it, so "the live feed is fast" was
        // unfalsifiable and the p99 46 s that retired this feed on 2026-07-13
        // could be neither reproduced nor refuted.
        //
        // The card now renders a per-SOCKET live table ABOVE the REST table,
        // fed from the app's own histogram via /metrics (no table holds live
        // lag). The caption states the ±1 s floor at the point of display,
        // because Dhan sends its timestamp in whole seconds and a reader must
        // not take a sub-second figure as precision. The empty state says "no
        // live ticks measured — this is NOT a zero-latency reading", since a
        // blank latency table is exactly the shape that otherwise reads as
        // "instant".
        //
        // Byte length MOVES this time (45,612 -> 47,467): a panel was added,
        // not a name swapped. Both numbers are updated together so the pin
        // keeps proving what it claims.
        //
        // The pin REMAINS a content ratchet — any further unreviewed edit to
        // the console HTML still fails the build; it simply no longer claims
        // legacy-byte-identity.
        //
        // 2026-09-03 re-bless (47,467 -> 47,435 — SMALLER, which is the point):
        // the Data tab's third bar, "contracts", is REMOVED along with the two
        // VIEW_COMMANDS queries that fed it. `rest_option_contract_1m` has had
        // no DDL caller and no writer since the 2026-08-21 Groww removal —
        // MEASURED on the box the same day, the table does not exist — so that
        // bar could only ever render blank, which an operator cannot tell from
        // a real capture failure.
        //
        // The first two attempts at this re-bless landed at 47,634 and 47,490,
        // and BOTH were caught by `browser_surface_and_toolchain_guard::
        // javascript_volume_inside_script_tags_only_shrinks` (JS lines 339 ->
        // 342, then -> 340). It was right to fail them: that budget is
        // shrink-only, and a change whose whole point is DELETING a bar has no
        // business growing the frontend carve-out to hold a comment about the
        // deletion. The reasoning lives in `parse_view` above — a Rust file
        // under no such budget — and the HTML keeps a one-line pointer to it.
        //
        // RE-BLESSED 2026-09-08 — the false-alarm fix.
        //
        // Three in-line edits, no new JS lines (the shrink-only volume budget
        // is untouched, deliberately — see the two failed attempts recorded
        // above for why that matters):
        //
        //   1. the feed card's `else` now checks `rest_audit_error` first, so
        //      an unreadable read renders the reason verbatim instead of the
        //      sentence "no official-candle pulls recorded today";
        //   2. `rest_audit_error` joins the `errs` strip beside its two
        //      siblings, which have carried their errors since they were
        //      written;
        //   3. the hero's `ticksbig` default changes from the literal `0` to
        //      `—`, matching every pill on the same card. A render that never
        //      reached `countUp` was showing a fabricated zero that an
        //      operator could not tell from a measured one.
        //
        // The cost of not doing this, measured: on 2026-09-08 the console
        // reported no official-candle pulls all morning while the cadence lane
        // was persisting every minute — `tv_rest_1m_fire_heartbeat` read 1.0
        // from 09:15 IST, and that gauge is set only after a flush ACK.
        // RE-BLESSED 2026-09-17 — the SOCKETS-ONLY console re-point.
        //
        // 48,906 -> 46,394 bytes. The console SHRANK by 2,512, which is the
        // shape a removal PR should have: three surfaces whose source tables
        // lost their writers under the operator's narrowing
        // (`no-rest-except-live-feed-2026-06-27.md` §12.12 and its dated
        // correction) are gone, and nothing was added to explain them here —
        // the reasoning lives in the Rust files, which carry no shrink-only
        // budget. The two failed 2026-09-08 re-blesses recorded above are why
        // that distinction is kept.
        //
        // What left the file:
        //   1. the per-(broker, pull type) REST latency table and its caption
        //      (`rest_fetch_audit`);
        //   2. the feed card's "pulls today: N ok · N failed" line and its
        //      `rest_audit_error` strip entry (same source);
        //   3. nothing else — the Data tab was RE-POINTED in place, so its
        //      bars, hero and dedup shield keep their markup and only their
        //      keys, table names and the 4 -> 5 expectation moved.
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, CONSOLE_HTML.as_bytes());
        let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "b364d3dacb24fb069f8374de122f32eb389eb2c35302018a00dd2e375e2bede1"
        );
        assert_eq!(CONSOLE_HTML.len(), 46_394);
    }

    // --------------------------------------------------------- class ParseView
    #[test]
    fn test_parses_labeled_snapshot() {
        // LIVE-LANE snapshot (re-pointed 2026-09-17): today's rows in the two
        // SOCKET tables, the total, the per-feed split, and the FIVE-column
        // dedup-key count for `ticks`.
        //
        // The stdout deliberately still CARRIES the retired REST-era labels
        // (`SPOT_TODAY` / `CHAIN_TODAY` / `CONTRACT_*` / `*_BY_FEED`). A box
        // running an older Lambda build, or a stale in-flight SSM invocation,
        // emits exactly that shape, and the parser must IGNORE an unknown
        // label rather than surface it. That is why the fixture keeps them
        // rather than being tidied to the new names only: the re-point is a
        // rolling deploy, and the first minutes of it look like this.
        let stdout = concat!(
            "APP=active\n",
            "TICKS_TODAY=1125\n",
            "DEPTH_TODAY=42000\n",
            "SPOT_TODAY=7\n",
            "CHAIN_TODAY=9\n",
            "CONTRACT_TODAY=9000\n",
            "TICKS_BY_FEED=\"dhan\",750;\"groww\",375;\n",
            "SPOT_BY_FEED=\"dhan\",4;\n",
            "CHAIN_BY_FEED=\"dhan\",21000;\"groww\",21000;\n",
            "CONTRACT_BY_FEED=\"groww\",9000;\n",
            "DEDUP_KEYS=5\n",
            "ERRORS_BEGIN\n",
            "Jun 01 11:00 tickvault: WARN something\n",
            "ERRORS_END\n",
        );
        let out = parse_view(stdout);
        assert_eq!(out["app"], "active");
        assert_eq!(
            out["rows_today"],
            json!({"ticks": "1125", "depth": "42000"})
        );
        // 43125, NOT 52141: every retired label above is present in the stdout
        // and must contribute nothing — the sum is the two LIVE counts alone.
        assert_eq!(out["rows_today_total"], "43125");
        assert_eq!(
            out["rows_by_feed"]["ticks"],
            json!({"dhan": "750", "groww": "375"})
        );
        // The retired legs render as ABSENT, never as an empty map: a `{}`
        // would read as "the leg ran and captured nothing".
        for gone in ["spot", "chain", "contracts"] {
            assert!(
                out["rows_by_feed"].get(gone).is_none(),
                "rows_by_feed must not carry the retired `{gone}` leg"
            );
            assert!(
                out["rows_today"].get(gone).is_none(),
                "rows_today must not carry the retired `{gone}` leg"
            );
        }
        // FIVE, not four — `DEDUP_KEY_TICKS` is
        // (ts, security_id, segment, capture_seq, feed). The 4 the REST-era
        // console expected would have shown a red schema-drift shield on a
        // perfectly healthy box, every day.
        assert_eq!(out["dedup_key_columns"], "5");
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
        assert_eq!(out["rows_today"], json!({"ticks": "", "depth": ""}));
        assert_eq!(out["rows_today_total"], "");
        assert_eq!(out["rows_by_feed"], json!({"ticks": {}}));
        assert_eq!(out["recent_errors"], json!([]));
    }

    #[test]
    fn test_partial_counts_sum_only_parseable() {
        // One table unreachable (empty value) — the total sums the rest,
        // never treating an unreachable count as 0-and-green. The retired
        // REST-era labels are fed in deliberately and must not be summed.
        let out = parse_view("TICKS_TODAY=100\nDEPTH_TODAY=\nSPOT_TODAY=23\nCONTRACT_TODAY=23\n");
        assert_eq!(out["rows_today_total"], "100");
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
    fn test_dedup_key_query_targets_ticks() {
        // RE-POINTED 2026-09-17 (renamed with its subject). It read
        // `rest_spot_1m`'s 4-column key from 2026-07-16 until the operator's
        // SOCKETS-ONLY narrowing removed that table's writer
        // (`no-rest-except-live-feed-2026-06-27.md` \u00a712.12). The shield now
        // reads `ticks`, whose key is FIVE columns
        // (ts, security_id, segment, capture_seq, feed) per
        // `DEDUP_KEY_TICKS`, pinned by `questdb_init_script_guard.rs`.
        //
        // The column COUNT matters as much as the table: a re-point that kept
        // the 4-column expectation would have shown a red "schema drift"
        // shield on a perfectly healthy box, every day.
        let dedup_cmd = VIEW_COMMANDS
            .iter()
            .find(|c| c.contains("DEDUP_KEYS="))
            .unwrap();
        assert!(dedup_cmd.contains("table_columns(%27ticks%27)"));
        assert!(!dedup_cmd.contains("rest_spot_1m"));
        assert!(dedup_cmd.contains("upsertKey"));
    }

    #[test]
    fn test_view_commands_target_live_socket_tables() {
        // RE-POINTED 2026-09-17 (renamed with its subject). It required
        // `rest_spot_1m` and `rest_option_chain_1m` — the two per-minute REST
        // tables — until the operator's SOCKETS-ONLY narrowing removed both
        // writers (`no-rest-except-live-feed-2026-06-27.md` §12.12).
        let joined = VIEW_COMMANDS.join("\n");
        for live in ["FROM%20ticks", "FROM%20market_depth"] {
            assert!(joined.contains(live), "{live}");
        }
        // The rule this test has always enforced, now pointed the other way:
        // the view must query NOTHING that has no writer. It is the
        // operator's read of "what did we capture today", and a query against
        // a writerless table renders a permanent zero the operator cannot
        // tell from a real capture failure.
        //
        // All three REST tables are RETAINED and hold real history, so these
        // queries did not ERROR — they returned 0 for `today()`, forever,
        // which is the WORSE shape. Re-adding any of them needs its writer
        // back first.
        for dead in [
            "rest_spot_1m",
            "rest_option_chain_1m",
            "rest_option_contract_1m",
            "rest_fetch_audit",
        ] {
            assert!(
                !joined.contains(dead),
                "the view queried `{dead}`, which has no writer"
            );
        }
        // Today windows use the house `ts IN today()` convention.
        assert!(joined.contains("ts%20IN%20today()"));
    }

    #[test]
    fn test_db_console_default_query_targets_live_table() {
        // ⚠ INVERTED 2026-09-17, same sign-flip as
        // `test_view_sql_targets_no_dead_tables`: this test required
        // `rest_spot_1m` and BANNED `ticks` — correct when the live feed was
        // retired, and exactly backwards now that the SOCKETS-ONLY narrowing
        // has removed the REST writers (§12.12).
        //
        // The RULE is unchanged and is why the default query matters at all:
        // it is the first thing an operator sees when the Data tab opens, so
        // it must hit a table that is guaranteed to have a writer. A default
        // that returns a permanent empty result teaches them the button is
        // broken; on a FRESH volume `rest_spot_1m` would not exist at all and
        // the query would error outright.
        assert!(CONSOLE_HTML.contains("SELECT * FROM ticks ORDER BY ts DESC LIMIT 50"));
        assert!(!CONSOLE_HTML.contains("FROM rest_spot_1m"));
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

    // ---- `test_rest_latency_sql_filters_ok_and_sentinel` is RETIRED
    // ---- 2026-09-17 ----
    //
    // It pinned the shape of `REST_LATENCY_SQL`: `outcome = 'ok'` rows only,
    // `close_to_data_ms >= 0` (which BOTH drops the -1 not-measured sentinel
    // AND satisfies `approx_percentile`'s non-negative-input requirement), the
    // `today()` window, and the `(feed, leg)` grouping.
    //
    // The const is gone with the rest of the `rest_fetch_audit` reads
    // (`no-rest-except-live-teed-2026-06-27.md` §12.12, dated correction).
    //
    // WHAT THIS LEAVES UNWATCHED, stated rather than implied: nothing checks
    // that a FUTURE percentile query filters its not-measured sentinel before
    // aggregating. That rule is not obvious — a `-1` sentinel silently drags
    // a p50 negative and `approx_percentile` rejects the input outright — so
    // if any percentile-over-a-sentinel-bearing-column query is ever written,
    // this test is the shape to restore.

    #[test]
    fn test_latency_commands_rest_era_bounded() {
        let joined = LATENCY_COMMANDS.join("\n");
        // NARROWED 2026-09-17: the two `RESTLAT_ROW=` / `rest_fetch_audit`
        // assertions are gone with the curl they pinned. Everything below is
        // UNCHANGED and is why this test was narrowed rather than deleted —
        // none of it ever depended on the REST leg.
        //
        // Every curl stays `--max-time` bounded, and the box-wide probes are
        // kept: an operator asking "is the box slow?" still gets the QuestDB
        // round-trip, the clock skew and the order-placement histogram.
        assert!(joined.contains("--max-time"));
        assert!(joined.contains("QDB="));
        assert!(joined.contains("SKEW="));
        assert!(joined.contains("tv_order_placement_duration_ns"));
        // The per-socket live delivery lag is the tab's primary table now
        // that the REST percentiles are gone — pinned so a future edit
        // cannot leave the tab with no latency measurement at all.
        assert!(joined.contains("WSLAT_RAW="));
        // A retired table must never be read from this array again. The
        // `rest_fetch_audit` needle is the one this test could not have
        // caught before: it was an ASSERTED-PRESENT string until today.
        assert!(!joined.contains("rest_fetch_audit"));
        assert!(!joined.contains("rest_spot_1m"));
        assert!(!joined.contains("rest_option_chain_1m"));
        // The retired live-feed probes must never be dialed again.
        assert!(!joined.contains("api-feed.dhan.co"));
        assert!(!joined.contains("socket-api.groww.in"));
        assert!(!joined.contains("FROM ticks"));
        assert!(!joined.contains("received_at"));
    }

    #[test]
    fn test_latency_timeout_reduced_with_margin() {
        // RE-DERIVED 2026-09-17 with the rest_fetch_audit aggregate removed:
        // three `--max-time 3` curls (order metrics, QuestDB probe, WS lag)
        // = 9s + SSM registration. See the const's own docstring for why the
        // superseded 15.0 was stale in the dangerous direction.
        //
        // The budget is asserted against the ARRAY, not against a copied
        // number: a future curl added without raising the timeout fails here
        // rather than silently eating the margin, which is exactly how the
        // WSLAT curl slipped in under the old figure.
        let curl_budget: f64 = LATENCY_COMMANDS
            .iter()
            .flat_map(|c| c.split("--max-time ").skip(1))
            .filter_map(|rest| {
                rest.split_whitespace()
                    .next()
                    .and_then(|n| n.parse::<f64>().ok())
            })
            .sum();
        assert_eq!(curl_budget, 9.0, "LATENCY_COMMANDS curl budget");
        assert_eq!(LATENCY_TIMEOUT_SECS, 12.0);
        assert!(
            LATENCY_TIMEOUT_SECS > curl_budget,
            "the poll budget must outlast the commands it polls \
             ({LATENCY_TIMEOUT_SECS} vs {curl_budget})"
        );
    }

    // ---- the three `parse_rest_lat_row` tests are RETIRED 2026-09-17 ----
    //
    // `..._happy`, `..._null_percentiles_degrade_to_none` and
    // `..._rejects_malformed_and_bad_tokens` covered the `RESTLAT_ROW=` CSV
    // parser, which went with its `rest_fetch_audit` producer
    // (`no-rest-except-live-feed-2026-06-27.md` §12.12, dated correction).
    //
    // The two rules they enforced are PRESERVED by the socket-lag tests that
    // follow, which is why this is a tombstone and not a lost guarantee:
    //   * a `null`/`NaN` percentile the database could not compute renders as
    //     ABSENT, never as 0 (`..._degrade_to_none` → the `+Inf` and
    //     missing-count arms below);
    //   * a malformed on-box line yields NOTHING rather than a partially
    //     parsed row (`..._rejects_malformed` →
    //     `test_parse_ws_lag_rows_drops_malformed_lines`).
    //
    // The charset gate those tests also exercised (`is_token`) is retired with
    // them; `FEED_NAME_RE` is the surviving, tighter equivalent and is
    // exercised by the feed-name tests.

    /// Two sockets, one healthy and one badly late — the exact shape the panel
    /// exists to make visible, and the one a `{host}`-folded or per-endpoint
    /// metric would average away.
    #[test]
    fn test_parse_ws_lag_rows_reports_each_socket_separately() {
        let raw: Vec<String> = [
            r#"tv_dhan_ws_lag_ms_bucket{connection="0",le="100"} 90"#,
            r#"tv_dhan_ws_lag_ms_bucket{connection="0",le="250"} 100"#,
            r#"tv_dhan_ws_lag_ms_bucket{connection="0",le="+Inf"} 100"#,
            r#"tv_dhan_ws_lag_ms_count{connection="0"} 100"#,
            r#"tv_dhan_ws_lag_ms_bucket{connection="3",le="100"} 0"#,
            r#"tv_dhan_ws_lag_ms_bucket{connection="3",le="60000"} 100"#,
            r#"tv_dhan_ws_lag_ms_bucket{connection="3",le="+Inf"} 100"#,
            r#"tv_dhan_ws_lag_ms_count{connection="3"} 100"#,
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

        let rows = parse_ws_lag_rows(&raw);
        assert_eq!(rows.len(), 2, "one row per socket, never merged");

        assert_eq!(rows[0]["connection"], "0");
        assert_eq!(rows[0]["p50_ms"], 100.0);
        assert_eq!(rows[0]["p99_ms"], 250.0);

        // The sick socket must NOT be smoothed by its healthy sibling.
        assert_eq!(rows[1]["connection"], "3");
        assert_eq!(rows[1]["p50_ms"], 60_000.0);
    }

    #[test]
    fn test_parse_ws_lag_rows_reports_an_overflow_p99_as_inf_not_a_number() {
        // A p99 in the overflow bucket means the true value is unbounded above.
        // Printing the largest finite bound would understate it — a lie in the
        // comfortable direction, which is the one that matters here.
        let raw: Vec<String> = [
            r#"tv_dhan_ws_lag_ms_bucket{connection="1",le="60000"} 50"#,
            r#"tv_dhan_ws_lag_ms_bucket{connection="1",le="+Inf"} 100"#,
            r#"tv_dhan_ws_lag_ms_count{connection="1"} 100"#,
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let rows = parse_ws_lag_rows(&raw);
        assert_eq!(rows[0]["p99_ms"], "+Inf");
    }

    #[test]
    fn test_parse_ws_lag_rows_is_empty_when_the_lane_produced_nothing() {
        // Feed off, box stopped, or /metrics unreachable. An empty table is the
        // honest answer; a zero would read as "instant delivery".
        assert!(parse_ws_lag_rows(&[]).is_empty());
        // A series present but with zero samples must also not render a row.
        let zero: Vec<String> = vec![r#"tv_dhan_ws_lag_ms_count{connection="0"} 0"#.to_string()];
        assert!(parse_ws_lag_rows(&zero).is_empty());
    }

    /// 2026-09-08: the operator's console showed two sockets of sixteen and a
    /// "NaN ms" p99. Every socket the app tracks now gets a row — what it
    /// holds, what it delivered, when — whether or not its lag histogram has
    /// samples, and the numbers ride the same raw lines.
    #[test]
    fn test_parse_ws_lag_rows_carries_every_socket_with_its_held_and_delivered_counts() {
        let raw: Vec<String> = [
            // Socket 0: healthy main-feed socket with measured lag.
            r#"tv_dhan_ws_conn_instruments{connection="0",endpoint="main_feed"} 4600"#,
            r#"tv_dhan_ws_conn_frames{connection="0",endpoint="main_feed"} 120500"#,
            r#"tv_dhan_ws_conn_tick_age_secs{connection="0",endpoint="main_feed"} 1"#,
            r#"tv_dhan_ws_lag_ms_bucket{connection="0",le="100"} 90"#,
            r#"tv_dhan_ws_lag_ms_bucket{connection="0",le="+Inf"} 100"#,
            r#"tv_dhan_ws_lag_ms_count{connection="0"} 100"#,
            // Socket 12: a depth-200 socket holding ONE contract that has
            // never delivered — no lag samples at all. It must still appear,
            // with NO age and NO percentile (never a zero).
            r#"tv_dhan_ws_conn_instruments{connection="12",endpoint="depth_200"} 1"#,
            r#"tv_dhan_ws_conn_frames{connection="12",endpoint="depth_200"} 0"#,
            r#"tv_dhan_ws_conn_tick_age_secs{connection="12",endpoint="depth_200"} -1"#,
            r#"tv_dhan_ws_lag_ms_count{connection="12"} 0"#,
            // The drain's degrade bucket is not a socket and gets no row.
            r#"tv_dhan_ws_lag_ms_count{connection="unknown"} 3"#,
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

        let rows = parse_ws_lag_rows(&raw);
        assert_eq!(rows.len(), 2, "one row per socket seen, none for `unknown`");

        assert_eq!(rows[0]["connection"], "0");
        assert_eq!(rows[0]["endpoint"], "main_feed");
        assert_eq!(rows[0]["instruments"], 4600.0);
        assert_eq!(rows[0]["frames"], 120_500.0);
        assert_eq!(rows[0]["tick_age_secs"], 1.0);
        assert_eq!(rows[0]["p50_ms"], 100.0);
        assert_eq!(rows[0]["p99_ms"], "+Inf");

        assert_eq!(rows[1]["connection"], "12");
        assert_eq!(rows[1]["endpoint"], "depth_200");
        assert_eq!(rows[1]["instruments"], 1.0);
        assert_eq!(rows[1]["frames"], 0.0);
        assert!(
            rows[1]["tick_age_secs"].is_null(),
            "-1 is 'never', not an age"
        );
        assert!(
            rows[1]["p50_ms"].is_null(),
            "no samples, no percentile — never a zero"
        );
        assert!(rows[1]["p99_ms"].is_null());
    }

    /// Sockets sort NUMERICALLY: "10" after "9", not after "1".
    #[test]
    fn test_parse_ws_lag_rows_orders_sockets_numerically() {
        let raw: Vec<String> = [
            r#"tv_dhan_ws_conn_frames{connection="10",endpoint="depth_200"} 5"#,
            r#"tv_dhan_ws_conn_frames{connection="9",endpoint="depth_20"} 5"#,
            r#"tv_dhan_ws_conn_frames{connection="1",endpoint="main_feed"} 5"#,
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let order: Vec<String> = parse_ws_lag_rows(&raw)
            .iter()
            .map(|r| r["connection"].as_str().unwrap_or("").to_string())
            .collect();
        assert_eq!(order, ["1", "9", "10"]);
    }

    #[test]
    fn test_parse_ws_lag_rows_drops_a_line_with_no_connection_label() {
        // Never default a malformed line into connection 0 — that would merge
        // unattributable samples into a real socket's numbers.
        let raw: Vec<String> = vec![
            "tv_dhan_ws_lag_ms_count 500".to_string(),
            r#"tv_dhan_ws_lag_ms_count{connection="2"} 4"#.to_string(),
            r#"tv_dhan_ws_lag_ms_bucket{connection="2",le="50"} 4"#.to_string(),
        ];
        let rows = parse_ws_lag_rows(&raw);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["connection"], "2");
        assert_eq!(rows[0]["samples"], 4.0);
    }

    #[test]
    fn test_parse_latency_full() {
        // NARROWED 2026-09-17: the two `RESTLAT_ROW=` lines and every
        // assertion over `rest_latency` are gone with the `rest_fetch_audit`
        // curl that produced them (`no-rest-except-live-feed-2026-06-27.md`
        // §12.12, dated correction). The box-wide probes below are unchanged
        // and are what this test now pins.
        //
        // One `RESTLAT_ROW=` line is KEPT in the fixture DELIBERATELY: a box
        // running an older Lambda build still emits the prefix, and it must
        // fall through the generic `k=v` arm into `fields` where nothing reads
        // it — ignored, never rendered. The assertion below is that no
        // `rest_latency` key reappears, which is what would happen if someone
        // restored the arm without restoring its source.
        let stdout = concat!(
            "METRICS_BEGIN\n",
            "tv_order_placement_duration_ns_sum 5000\n",
            "tv_order_placement_duration_ns_count 10\n",
            "METRICS_END\n",
            "QDB=0.0021\n",
            "SKEW=0.000123\n",
            "RESTLAT_ROW=\"dhan\",\"chain_1m\",374,1300.0,2100.0\n",
        );
        let out = parse_latency(stdout);
        assert_eq!(out["questdb_ms"], "2.1");
        assert_eq!(out["clock_skew_ms"], "0.1");
        assert_eq!(out["order_place_avg_ns"], json!(500.0));
        assert!(
            out.get("rest_latency").is_none(),
            "a stale RESTLAT_ROW= line must not resurrect the retired field"
        );
    }

    #[test]
    fn test_parse_latency_empty() {
        // Box stopped -> blank shields, never fabricated numbers.
        //
        // 2026-09-17: `rest_latency` is asserted ABSENT rather than `[]`. An
        // always-empty list would have read as "the pulls ran and were never
        // late" — the zero-an-operator-cannot-distinguish class. `ws_latency`
        // keeps the empty-list shape because its producer is live: `[]` there
        // genuinely means the lane reported no sockets, and the console says
        // so in words rather than showing a comforting zero.
        let out = parse_latency("");
        assert!(out.get("rest_latency").is_none());
        assert_eq!(out["ws_latency"], json!([]));
        assert_eq!(out["questdb_ms"], "");
        assert_eq!(out["clock_skew_ms"], "");
        assert_eq!(out["order_place_avg_ns"], Value::Null);
    }

    // ---- `test_parse_latency_malformed_rest_rows_skipped` is RETIRED
    // ---- 2026-09-17 ----
    //
    // It fed `RESTLAT_ROW=garbage` beside one good row and asserted the
    // garbage was DROPPED rather than half-parsed. The rule is intact and is
    // enforced one panel over by
    // `test_parse_ws_lag_rows_drops_malformed_lines`, on the parser that
    // still has a producer.

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
    // legacy setUp pinned the clock OFF-hours (MockShell default) — since the
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
        //  (a) replay sources are removed BEFORE the TRUNCATE legacy block;
        //  (b) the unit is DISABLED for the wipe window and re-enabled before
        //      the final start;
        //  (c) prev_day_ohlcv is in the dynamic truncate targets.
        // (the legacy runtime scanned handler source; the Rust command list is the
        // oracle-captured WIPE_QUESTDB_COMMANDS golden — same ordering.)
        let joined = WIPE_QUESTDB_COMMANDS.join("\n");
        let rm_pos = joined.find("feed capture/replay sources removed").unwrap();
        let truncate_pos = joined.find("WIPE-TARGETS").unwrap();
        assert!(
            rm_pos < truncate_pos,
            "replay-source rm must precede TRUNCATE"
        );
        let disable_pos = joined.find("systemctl disable tickvault || true").unwrap();
        assert!(
            disable_pos < rm_pos,
            "unit must be disabled before the wipe body"
        );
        assert!(joined.contains("$0==\"prev_day_ohlcv\""));
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
        assert!(joined.contains("$0==\"ticks\""));
        assert!(joined.contains("index($0,\"candles_\")==1"));
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

    /// Every equality-arm table in the wipe TARGET predicate must also be
    /// counted by the verification tail, and vice versa.
    ///
    /// # The defect this closes (2026-09-05)
    ///
    /// `market_depth` was in NEITHER half. The action truncated everything
    /// else and printed `WIPE-COMPLETE` with the largest table in the process
    /// untouched — a measured 1,530,651,649 rows/session, 299 GB — which is
    /// why that day's wipe had to drop the table by hand over EC2 Instance
    /// Connect *after* this tool reported success. Quote 21 of the same day
    /// names `market_depth` in the authorized wipe set, so the tool was
    /// failing against the operator's own written scope.
    ///
    /// A symmetry test alone would NOT have caught it — absent from both
    /// halves is symmetric. So this asserts symmetry AND names the two
    /// market-data tables whose omission is the expensive one. Symmetry is
    /// what stops the next addition from landing in one half only; the named
    /// pair is what stops this specific regression.
    #[test]
    fn test_wipe_targets_and_verification_name_the_same_tables() {
        let joined = WIPE_QUESTDB_COMMANDS.join("\n");

        // The two market-data tables that carry essentially all the bytes.
        // `ticks` was always present; `market_depth` was the omission.
        for t in ["ticks", "market_depth"] {
            assert!(
                joined.contains(&format!("$0==\"{t}\"")),
                "wipe TARGET predicate does not name `{t}` — the action would \
                 report WIPE-COMPLETE while leaving it fully populated"
            );
            assert!(
                joined.contains(&format!("$(qc {t})")),
                "wipe VERIFICATION tail does not count `{t}` — WIPE-COMPLETE \
                 would be printed without ever checking it reached zero"
            );
        }

        // Symmetry, discovered rather than listed: every `$0=="X"` equality
        // arm in the predicate must have a `$(qc X)` in the tail, and every
        // `$(qc X)` must have an arm. `candles_` is matched by `index(...)==1`
        // (a prefix, not an equality arm) and is verified via `candles_1m`
        // alone, so it is excluded from both directions deliberately.
        let mut targets: Vec<String> = Vec::new();
        for part in joined.split("$0==\"").skip(1) {
            if let Some(end) = part.find('"') {
                targets.push(part[..end].to_string());
            }
        }
        let mut verified: Vec<String> = Vec::new();
        for part in joined.split("$(qc ").skip(1) {
            if let Some(end) = part.find(')') {
                verified.push(part[..end].to_string());
            }
        }
        assert!(
            targets.len() >= 6 && verified.len() >= 6,
            "wipe predicate/verification parse looks vacuous (targets={}, \
             verified={}) — the scanner, not the commands, is broken",
            targets.len(),
            verified.len()
        );
        for t in &targets {
            assert!(
                verified.contains(t),
                "wipe TARGETS `{t}` but never verifies it reached zero — \
                 WIPE-COMPLETE would be printed on an untested table"
            );
        }
        for v in &verified {
            if v == "candles_1m" {
                continue; // matched by the `index($0,"candles_")==1` prefix arm
            }
            assert!(
                targets.contains(v),
                "wipe VERIFIES `{v}` but never truncates it — the action would \
                 report WIPE-PARTIAL forever on a table it does not touch"
            );
        }
    }

    #[test]
    fn test_wipe_questdb_truncates_live_rest_tables_too() {
        // 2026-07-16 destructive-surface extension: a "fresh start" must also
        // drop today's official minute candles + the fetch log — the four
        // LIVE tables the REST pulls write. rest_fetch_audit is per-fetch
        // forensics, NOT a SEBI never-delete table; the SEBI *_audit family
        // stays preserved by the dynamic filter.
        let joined = WIPE_QUESTDB_COMMANDS.join("\n");
        // 2026-08-01: the target predicate is now an awk expression (the
        // embedded interpreter program was retired). Each of the four LIVE
        // REST tables must still appear as its own equality arm.
        for live in [
            "rest_spot_1m",
            "rest_option_chain_1m",
            "rest_option_contract_1m",
            "rest_fetch_audit",
        ] {
            assert!(
                joined.contains(&format!("$0==\"{live}\"")),
                "wipe target predicate lost the {live} arm"
            );
        }
        // Review fix M2 (2026-07-16): honest completion verifies EVERY
        // truncate-target family — the legacy pair AND all FOUR live REST
        // tables.
        for t in [
            "ticks",
            "candles_1m",
            "rest_spot_1m",
            "rest_option_chain_1m",
            "rest_option_contract_1m",
            "rest_fetch_audit",
        ] {
            assert!(joined.contains(&format!("$(qc {t})")), "{t}");
        }
        assert!(joined.contains("rest_spot_1m=${S:-?}"));
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

    /// AUTO-START GUARANTEE ratchet (2026-08-20 incident).
    ///
    /// A destructive maintenance action that `systemctl disable tickvault`
    /// WITHOUT re-enabling it silently costs the next trading day: a disabled
    /// unit does not auto-start at the 08:30 IST boot, and
    /// `scripts/aws-autopilot.sh` treats a disabled unit as an INTENTIONAL
    /// kill-switch and refuses to self-heal it.
    ///
    /// This is the same class `deploy_no_disable_on_failure_guard.rs` pinned
    /// for `deploy-aws.yml` on 2026-06-09 — that guard never covered these
    /// Lambda action lists, and `DOCKER_NUKE_BARE_COMMANDS` fell through the
    /// hole on 2026-08-20 (box up 08:30:42, app dead all morning).
    ///
    /// The INTENTIONAL kill-switch is the separate `stop-app` action and is
    /// deliberately NOT covered here — it is supposed to leave the unit down.
    #[test]
    fn test_destructive_actions_never_leave_tickvault_disabled() {
        for (name, cmds) in [
            ("wipe-questdb", WIPE_QUESTDB_COMMANDS.as_slice()),
            ("docker-reset", DOCKER_RESET_COMMANDS.as_slice()),
            ("docker-nuke-bare", DOCKER_NUKE_BARE_COMMANDS.as_slice()),
        ] {
            let disable_at = cmds
                .iter()
                .position(|c| c.contains("systemctl disable tickvault"));
            let Some(disable_at) = disable_at else {
                continue; // never disables — nothing to restore
            };
            let enable_at = cmds
                .iter()
                .position(|c| c.contains("systemctl enable tickvault"))
                .unwrap_or_else(|| {
                    panic!(
                        "{name} runs `systemctl disable tickvault` but NEVER re-enables it. \
                         A disabled unit does not auto-start at the next 08:30 IST boot and \
                         aws-autopilot.sh refuses to self-heal it (it reads disabled as an \
                         intentional kill-switch), so this silently costs a trading day. \
                         Append `systemctl enable tickvault || true`. The intentional \
                         kill-switch is the separate `stop-app` action."
                    )
                });
            assert!(
                enable_at > disable_at,
                "{name}: the re-enable must come AFTER the disable (disable at {disable_at}, \
                 enable at {enable_at}) — otherwise the disable wins and the unit stays off"
            );
        }
    }
    /// The set of tables this action MUST export, DERIVED from the
    /// repository's own authoritative never-delete lists rather than restated
    /// here.
    ///
    /// # Why derived, and not a list
    ///
    /// Until 2026-09-05 this guard carried a hardcoded four-name array — the
    /// SAME four the shell block used. It therefore asserted a list against
    /// itself and could not, even in principle, notice that 32 other tables
    /// the repository declares as never-delete were being destroyed with the
    /// volume while the action printed `SEBI-PRESERVED` four times and exited
    /// 0. A guard that restates its subject is a copy, not a check.
    ///
    /// The authority is `partition_manager.rs`: `DAY_PARTITIONED_TABLES` (the
    /// audit and daily-data tables the retention sweep detaches but never
    /// drops) plus `RETENTION_EXEMPT_TABLES` (the pinned-timestamp masters it
    /// never touches at all). `HOUR_PARTITIONED_TABLES` — `ticks` and
    /// `market_depth` — is deliberately EXCLUDED: that is the market data this
    /// action exists to destroy, and exporting it would turn a recovery tool
    /// into an out-of-disk incident.
    ///
    /// Adding a table to either list now makes this test fail until the shell
    /// block exports it too. That is the point.
    fn never_delete_tables() -> Vec<String> {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../storage/src/partition_manager.rs"),
        )
        .expect("partition_manager.rs must be readable — it is the authority for this test");

        let mut out = Vec::new();
        for konst in ["DAY_PARTITIONED_TABLES", "RETENTION_EXEMPT_TABLES"] {
            let start = src
                .find(konst)
                .unwrap_or_else(|| panic!("{konst} must exist — if it was renamed, update this"));
            let body = &src[start..];
            let end = body
                .find("\n];")
                .unwrap_or_else(|| panic!("{konst} must be a terminated slice literal"));
            let mut found = 0usize;
            for line in body[..end].lines() {
                let line = line.trim();
                // A quoted entry at the start of a line is a table name; a
                // quote inside a `//` comment is prose and must not count.
                if line.starts_with("//") {
                    continue;
                }
                if let Some(rest) = line.strip_prefix('"') {
                    if let Some((name, _)) = rest.split_once('"') {
                        out.push(name.to_owned());
                        found += 1;
                    }
                }
            }
            assert!(
                found >= 5,
                "parsed only {found} entries from {konst} — the parser is broken and this \
                 guard would pass while exporting almost nothing"
            );
        }
        out.sort();
        out.dedup();
        out
    }

    /// The 2026-08-25 finding: `wipe-questdb` allowlists only market-data
    /// tables, so the 5-year regulatory tables survive it — while
    /// `docker-reset` and `docker-nuke-bare` delete the whole QuestDB volume
    /// and take them with it, with nothing exported first.
    ///
    /// Two things are asserted, and the second is the one that matters:
    /// the export happens, and it happens BEFORE anything destructive. An
    /// export placed after `docker volume rm` would pass a naive "contains"
    /// check while preserving nothing.
    #[test]
    fn destructive_actions_export_the_sebi_tables_before_destroying_the_volume() {
        let sebi = never_delete_tables();
        assert!(
            sebi.len() >= 30,
            "derived only {} never-delete tables — the derivation is broken",
            sebi.len()
        );
        for (name, cmds) in [
            ("docker-reset", DOCKER_RESET_COMMANDS.as_slice()),
            ("docker-nuke-bare", DOCKER_NUKE_BARE_COMMANDS.as_slice()),
        ] {
            let preserve_at = cmds
                .iter()
                .position(|c| c.contains("SEBI-PRESERVED"))
                .unwrap_or_else(|| {
                    panic!(
                        "{name} destroys the QuestDB volume without exporting the 5-year \
                         regulatory tables first. `wipe-questdb` spares them by allowlist; \
                         this action must export them, because it deletes the volume they \
                         live in."
                    )
                });

            // Every destructive step must come after the export.
            for (i, c) in cmds.iter().enumerate() {
                let destroys = c.contains("docker volume rm")
                    || c.contains("docker volume ls")
                    || c.contains("compose down -v")
                    || c.contains("system prune");
                assert!(
                    !destroys || i > preserve_at,
                    "{name}: step {i} destroys data but runs BEFORE the SEBI export at \
                     step {preserve_at} — an export that runs afterwards preserves nothing"
                );
            }

            let block = cmds.join("\n");
            for t in &sebi {
                assert!(
                    block.contains(t.as_str()),
                    "{name} must export the regulatory table `{t}` before destroying the volume"
                );
            }
            // It must REFUSE when a table exists and cannot be exported —
            // otherwise the export is decoration and the data is still lost.
            assert!(
                block.contains("SEBI-PRESERVE-FAILED") && block.contains("exit 1"),
                "{name}: a failed export of an EXISTING regulatory table must abort the \
                 action, not be logged and stepped over"
            );
            // And it must NOT refuse when QuestDB is simply unreachable —
            // this action is also the remedy for a wedged QuestDB, and a
            // recovery tool that cannot run during the failure it recovers
            // from is not a recovery tool.
            assert!(
                block.contains("SEBI-PRESERVE-UNAVAILABLE"),
                "{name}: an unreachable QuestDB must be reported and stepped over, never \
                 treated as unexported data"
            );
            // The export target must be outside every path this action wipes.
            assert!(
                block.contains("/opt/tickvault/data/sebi-preserve/"),
                "{name}: the export must land outside the volume and outside every rm -rf \
                 path in this action"
            );
            for c in cmds.iter() {
                if c.contains("rm -rf") {
                    assert!(
                        !c.contains("sebi-preserve"),
                        "{name}: an rm -rf step must never sweep the preservation directory"
                    );
                }
            }
        }
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
        // 09:15-15:40 IST guard: with the correct confirm phrase but no force,
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
        // RE-POINTED 2026-09-17. Two by-feed splits became ONE: `CHAIN_BY_FEED`
        // is not re-pointed at `market_depth` because one live by-feed split is
        // the question the card asks (§12.12's mapping table), and the NAME
        // moved with the query — an operator reading "spot" would have believed
        // they were looking at the retired REST leg.
        let cmd = VIEW_COMMANDS
            .iter()
            .find(|c| c.contains("TICKS_BY_FEED="))
            .unwrap();
        assert!(cmd.contains("GROUP%20BY%20feed"));
        assert!(cmd.contains("FROM%20ticks"));
        let joined = VIEW_COMMANDS.join("\n");
        for gone in ["SPOT_BY_FEED=", "CHAIN_BY_FEED=", "CONTRACT_BY_FEED="] {
            assert!(!joined.contains(gone), "retired label `{gone}` is back");
        }
    }

    // --------------------------------------------------------- class FeedsView
    #[test]
    fn test_parse_feeds_view_full() {
        let stdout = concat!(
            "FEEDS_BEGIN\n",
            "{\"dhan_enabled\": true, \"truedata_enabled\": false, ",
            "\"dhan_lane_running\": true, \"truedata_lane_running\": false}\n",
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
        assert_eq!(out["feeds"]["truedata_enabled"], false);
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

    // ---- the REST-lane feeds-card tests are RETIRED 2026-09-17 ----
    //
    // Three tests went together, because they pinned one chain that no longer
    // exists: `test_parse_feeds_view_rest_lane_fields`,
    // `test_parse_feeds_view_distinguishes_absent_unreadable_and_genuinely_zero`,
    // and `test_rest_audit_line_inside_json_body_is_not_parsed`. Their
    // subject was the `REST_AUDIT=` / `REST_LAT_HOUR=` pulse on the feeds
    // card, read from `rest_fetch_audit` — removed with the per-minute REST
    // legs (`no-rest-except-live-feed-2026-06-27.md` §12.12).
    //
    // ONE of them is worth more than the code it tested, so its rule is
    // restated here rather than lost with it. The three-outcome test was
    // REWRITTEN on 2026-09-08 after the console told the operator that no
    // official-candle pulls had been recorded, all morning, while the lane
    // was healthy and flushing every minute. The old version asserted that
    // "absent / unreadable / garbage all yield `{}`" and cited Rule 11 while
    // pinning the exact conflation Rule 11 forbids: `{}` rendered as the
    // SENTENCE "no pulls recorded today", so a blind read and an idle lane
    // produced identical words.
    //
    // THE DURABLE RULE, which binds every future parser on this surface:
    // ABSENT, UNPARSEABLE and GENUINELY-EMPTY are three different facts and
    // must render as three different things. Avoiding a fabricated zero
    // NUMBER while emitting a fabricated zero CLAIM is not honesty.
    //
    // `parse_feeds_view`'s surviving arms still hold that line — an
    // unreachable box yields `Value::Null` + `BOX_UNREACHABLE`, invalid JSON
    // yields `Null` + a structured error, and an empty body yields `{}` — and
    // those three cases keep their own tests above.

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
        // 2026-09-17: 24 → 16. The two 4s `rest_fetch_audit` curls went with
        // the per-minute REST legs; the two 8s app curls are what remain.
        assert_eq!(total, 16); // 2×8s app curls
        assert!(FEEDS_TIMEOUT_SECS > total as f64);
        assert_eq!(FEEDS_TIMEOUT_SECS, 20.0);
    }

    // ---- the two REST-curl shape tests are RETIRED 2026-09-17 ----
    //
    // `test_rest_audit_curl_targets_todays_fetch_log` and
    // `test_rest_lat_hour_curl_ist_timebase_ok_filter_and_sentinel` pinned the
    // two `rest_fetch_audit` curls in `FEEDS_VIEW_COMMANDS`, both removed with
    // the per-minute REST legs (`no-rest-except-live-feed-2026-06-27.md`
    // §12.12).
    //
    // TWO rules they enforced are NOT obvious and are recorded here so a
    // future query does not rediscover them the expensive way:
    //
    //   1. IST TIMEBASE. `ts` on these tables is IST-shifted while QuestDB's
    //      `now()` is UTC, so an hour window must compare against
    //      `dateadd('m', 330, now())`, never bare `now()` (the 2026-07-07
    //      lesson). A query that forgets this is off by 5h30m and returns a
    //      confident empty result.
    //   2. THE -1 SENTINEL. `close_to_data_ms` stores -1 for "not measured",
    //      so any aggregate over it needs `close_to_data_ms >= 0` — which
    //      both drops the sentinel AND satisfies `approx_percentile`'s
    //      non-negative-input requirement.
    //
    // Rule 1 still binds every remaining `today()`/window query on an
    // IST-shifted table; rule 2 has no live consumer, which is why it is
    // written out rather than pinned.

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
        let joined = feed_toggle_commands("truedata", true).join("\n");
        assert!(joined.contains("http://127.0.0.1:3001/api/feeds/truedata"));
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
            "{\"dhan_enabled\": true, \"truedata_enabled\": true, ",
            "\"dhan_lane_running\": true, \"truedata_lane_running\": false}\n",
            "FEED_TOGGLE_BODY_END\n",
        );
        let out = parse_feed_toggle(stdout);
        assert_eq!(out["app_status"], 200);
        assert_eq!(out["error"], "");
        assert_eq!(out["app_response"]["truedata_enabled"], true);
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
    // The dedup shield reads `ticks`'s REAL 5-column upsert key
    // (ts, security_id, segment, capture_seq, feed per DEDUP_KEY_TICKS in
    // crates/storage/src/tick_persistence.rs) — RE-POINTED 2026-09-17 from
    // rest_spot_1m's 4-column key, which it had carried since 2026-07-16.
    //
    // This is the SECOND time this shield has moved, and the direction
    // reversed: 2026-07-16 took it OFF `ticks` when the live feed was
    // retired; today's SOCKETS-ONLY narrowing takes it back.
    #[test]
    fn test_view_comment_names_the_five_real_key_columns() {
        // the legacy runtime scanned handler.py; the Rust twin lives in the commands
        // module's doc comment (the %3D rationale block).
        let src = include_str!("operator_control_commands.rs");
        assert!(src.contains("(ts, security_id, segment, capture_seq, feed)"));
        assert!(src.contains("DEDUP_KEY_TICKS"));
    }

    #[test]
    fn test_html_distinguishes_ok_disabled_drift_unreachable() {
        // 5 = OK (green): RE-POINTED 2026-09-17 with the shield itself. The
        // comment here read "not the stale 5" — `ticks` is the live table
        // again, so 5 is the correct expectation and 4 is now the stale one.
        assert!(CONSOLE_HTML.contains("dkN===5"));
        assert!(!CONSOLE_HTML.contains("dkN===4"));
        // 0 = DEDUP disabled entirely (RED).
        assert!(CONSOLE_HTML.contains("DEDUP disabled!"));
        assert!(CONSOLE_HTML.contains("'bad'"));
        // other = schema drift (amber).
        assert!(CONSOLE_HTML.contains("schema drift (expected 5)"));
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

    // ---- `test_failed_bucket_excludes_never_attempted_minutes` is RETIRED
    // ---- 2026-09-17 ----
    //
    // It pinned the REST-pull line's "failed" fold: `skipped` and
    // `boundary_skipped` audit rows are minutes the leg never ATTEMPTED
    // (trading-day gate, missed boundaries), so folding them into "failed"
    // inflated the count — while `no_token` IS a real failure and stayed
    // counted. Its subject is gone with `rest_fetch_audit`.
    //
    // THE DURABLE RULE, recorded because it is not obvious and cost a review
    // round to find: a NOT-ATTEMPTED outcome and a FAILED outcome are
    // different facts, and an error bucket that silently absorbs the former
    // reports a healthy gated leg as broken. Any future per-outcome rollup on
    // this console owes the same distinction.
    #[test]
    fn test_errors_render_verbatim_through_esc() {
        assert!(CONSOLE_HTML.contains("esc(j.feeds_error"));
        assert!(CONSOLE_HTML.contains("j.app_response.error"));
    }

    #[test]
    fn test_feeds_card_carries_no_writerless_pull_line() {
        // ↺ REPLACES `test_feeds_card_shows_rest_pull_line_not_tick_counters`
        // (2026-09-17). It REQUIRED the per-feed REST-pull line
        // (ok/failed/rate-limited + last-hour p50/p99), sourced from
        // `rest_fetch_audit` — removed with the per-minute legs
        // (`no-rest-except-live-feed-2026-06-27.md` §12.12).
        //
        // Nothing replaces the line: there is no per-minute fetch left to
        // count, and an always-empty "pulls today: 0 ok" would read as a
        // failure rather than as an absence.
        for gone in [
            "j.rest_audit",
            "j.rest_lat_hour",
            "pulls today:",
            "after minute close",
            "no official-candle pulls recorded today",
        ] {
            assert!(!CONSOLE_HTML.contains(gone), "retired pull-line: {gone}");
        }
        // The ORIGINAL half of this test is UNCHANGED and still binds: the
        // frozen live-feed registry counters must never come back either.
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
    fn test_html_latency_card_is_socket_only() {
        // ↺ REPLACES `test_html_has_rest_latency_table` (2026-09-17). That
        // test pinned the per-(broker, pull type) REST table, whose
        // `rest_fetch_audit` source went with the per-minute legs
        // (`no-rest-except-live-feed-2026-06-27.md` §12.12, dated correction).
        //
        // It is REPLACED rather than deleted because the card must not end up
        // with NO latency table at all: the per-socket live delivery lag is
        // now the tab's only measurement, and this is what pins it there.
        assert!(CONSOLE_HTML.contains(r#"id="latws""#));
        assert!(CONSOLE_HTML.contains("p50 exchange→here"));
        assert!(CONSOLE_HTML.contains("p99 exchange→here"));
        assert!(CONSOLE_HTML.contains("instruments on wire"));
        // The retired REST table must not come back without its source.
        assert!(!CONSOLE_HTML.contains(r#"id="latrest""#));
        assert!(!CONSOLE_HTML.contains("ok pulls today"));
        assert!(!CONSOLE_HTML.contains("p50 after close"));
    }

    #[test]
    fn test_latency_card_iterates_rows_no_hardcoded_names() {
        // RE-POINTED 2026-09-17 from `j.rest_latency` to `j.ws_latency`. The
        // RATCHET is unchanged and is the reason this was re-pointed rather
        // than retired: the table renders whatever the server sends (socket
        // index + endpoint discovered at measure time), so a new socket, pool
        // or broker gets its row with ZERO portal changes. No feed, leg or
        // endpoint name may be hardcoded in the portal JS.
        let js = load_latency_js();
        assert!(js.contains("j.ws_latency"));
        assert!(!js.contains("j.rest_latency"));
        let lower = js.to_lowercase();
        assert!(!lower.contains("dhan"));
        assert!(!lower.contains("groww"));
        assert!(!js.contains("spot_1m"));
        assert!(!js.contains("chain_1m"));
    }

    #[test]
    fn test_empty_table_is_honest_not_fake_zero() {
        // NARROWED 2026-09-17: the REST "no successful pulls recorded today"
        // string went with its table. The RULE is unchanged and is now pinned
        // on the surface that still has a producer — an empty socket table
        // must say so in words, never render as a comforting zero.
        // Both surviving empty-state strings, on the two tables that still
        // have producers: the socket table says an empty result is NOT a
        // zero-latency reading, and the card caption says an empty table must
        // never be read as "fast".
        assert!(CONSOLE_HTML.contains("This is NOT a zero-latency reading."));
        assert!(CONSOLE_HTML.contains(r#"never read that as "fast""#));
        assert!(!CONSOLE_HTML.contains("no successful pulls recorded today"));
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
            assert!(err.contains("Run after 15:40"));
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
        // Pinned lock window semantics: 09:15:00 ≤ t < 15:40:00 IST Mon-Fri.
        // 2026-08-07: upper bound moved 15:30 -> 15:40 with the NSE Closing
        // Auction change of 2026-08-03. While it read 15:30, the destructive-
        // action lock LIFTED ten minutes before the market actually closed.
        // 2026-06-01 is a Monday; IST = UTC + 05:30.
        let cases = [
            ((3, 44, 59), false), // 09:14:59 IST — open
            ((3, 45, 0), true),   // 09:15:00 IST — locked
            ((9, 59, 59), true),  // 15:29:59 IST — locked
            ((10, 0, 0), true),   // 15:30:00 IST — STILL locked (was open)
            ((10, 9, 59), true),  // 15:39:59 IST — locked
            ((10, 10, 0), false), // 15:40:00 IST — open
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
        assert!(CONSOLE_HTML.contains("Locked until 3:40 PM IST"));
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
    /// identifiers, mirroring the legacy runtime's `hasattr(handler, …)` + source scan
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
        // the legacy runtime asserted the module lacks the retired attributes; the Rust
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
        // ⚠ INVERTED 2026-09-17 for `ticks`, and that is the whole point of
        // this note. This test was written when the live feed was retired and
        // `ticks` was the DEAD table — it banned `FROM%20ticks` and
        // `TICKS_TODAY` by name. The 2026-08-11 revival made `ticks` the live
        // lane's primary table, and the SOCKETS-ONLY narrowing made the REST
        // tables the dead ones, so a guard left as written would now be
        // asserting the exact reverse of the truth: it would fail the build
        // for querying the one table that is guaranteed to have a writer.
        //
        // The RULE is unchanged and is what survives: the view must query
        // nothing that has no writer. Only the membership of "dead" moved.
        let joined = VIEW_COMMANDS.join("\n");
        for dead in [
            "candles_1m",
            "tick_conservation_audit",
            "ws_event_audit",
            "CONSERVE",
            "WS_DISC",
            // Retired with the per-minute REST legs (§12.12). RETAINED tables,
            // so these return a permanent zero rather than erroring — the
            // shape this guard exists to keep off the operator's console.
            "rest_spot_1m",
            "rest_option_chain_1m",
            "rest_option_contract_1m",
            "rest_fetch_audit",
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
        // (the retired reference implementation, formerly under scripts/mcp-servers/) — the API route must
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
        "Box stopped (auto-stops 17:30 IST, auto-starts 08:30 Mon–Fri) — guarantees resume on start"
    ));
        assert!(CONSOLE_HTML.contains("$('stoppedbanner').hidden=running"));
        // …and the shield greys out as "—" ONLY when the box is not running.
        assert!(CONSOLE_HTML.contains("shieldIdle"));
        assert!(CONSOLE_HTML.contains("if(!running){"));
        // The REAL warning paths for a RUNNING box must still exist (a stopped
        // banner must never hide genuine failures while running).
        assert!(CONSOLE_HTML.contains("'unreachable'"));
        assert!(CONSOLE_HTML.contains("DEDUP disabled!"));
        // 4 -> 5 with the shield's re-point (2026-09-17, §12.12).
        assert!(CONSOLE_HTML.contains("schema drift (expected 5)"));
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
        // 2026-09-17: `latrest` is gone with the REST latency table; `latws`
        // (the per-socket live lag) is the card's measurement now, and is
        // pinned so the overview can never lose its latency table entirely.
        assert!(overview.contains(r#"id="latws""#));
        assert!(!overview.contains(r#"id="latrest""#));
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
        // TTL, and the signature equals the legacy-oracle HMAC vector
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
        // "unknown" (the legacy test env had no AWS/GitHub) — no angle
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
        // the legacy runtime manipulated process env around handler._portal_sha(); the
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
        // (the legacy runtime patched urlopen + the cache dict; the Rust cache policy is
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
