//! QuestDB console FRONT Lambda (B4) — auth + read-only gate, NON-VPC.
//!
//! 1:1 port of `deploy/aws/lambda/questdb-console-front/handler.py`
//! (rust-only phase 2b-3, 2026-07-18). Python is the ORACLE — every test
//! vector below is derived by running python3 on the original handler
//! (`scratchpad/w3-oracle.json`, derived 2026-07-18), never invented.
//!
//! Browser → API-Gateway v2 ($default, payload v2) → THIS Lambda →
//! (lambda:Invoke) → the VPC-attached back Lambda (qdb-console-proxy) →
//! `http://<box>:9000`.
//!
//! WHY TWO LAMBDAS: a VPC Lambda in this VPC has NO internet/AWS-API path
//! (single public subnet, no NAT, no VPC endpoints), so it cannot read the
//! SSM device-key secret at runtime. THIS front Lambda stays outside the VPC
//! and handles ALL secret work (runtime SSM read, 60s cache, fail-closed,
//! never in env vars / TF state); the VPC back Lambda is a dumb, secret-free
//! HTTP relay.
//!
//! AUTH (stateless, HMAC over the SSM control secret, constant-time
//! compares):
//! * One-click link token (≤90s): minted by the operator portal's
//!   `qdb_console_url` action as `<exp>.<hexhmac(secret, "qdblink|<exp>")>`,
//!   consumed via `GET /open?tok=...` → 302 / + session cookie. The MINT
//!   side lives in operator-control (wave 4) — the wire contract here MUST
//!   byte-match it (oracle-pinned vectors below).
//! * Session cookie `qdb_sess = <exp>.<hexhmac(secret, "qdbsess|<exp>")>`
//!   (HttpOnly; Secure; SameSite=Lax; 12h).
//! * Fallback login page: paste the device key → POST /login →
//!   constant-time compare vs the SSM secret → cookie.
//! * `Authorization: Bearer <secret>` accepted directly on any request.
//!
//! The read-only SQL gate, the traversal reject, and the path classifier are
//! the SAME functions the back uses (`crate::qdb_console_proxy`) — the
//! Python sources carried two VERBATIM copies with a byte-parity test; the
//! Rust port makes that parity STRUCTURAL (one shared implementation), and
//! the ported parity tests pin the shared behavior against the
//! operator-control oracle verdicts.
//!
//! Never logs secrets, tokens, or cookie values. Structured JSON log per
//! request (tracing → CloudWatch, replacing the Python `print`).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Map, Value, json};
use tracing::info;

use crate::qdb_console_proxy::{self as back_gate, PathKind, bad_path, classify_path, is_safe_sql};

// ------------------------------------------------------------------ consts

/// Deployed-bytes proof marker — MUST equal the back's marker (the Python
/// BuildMarker test asserted the same value in both handler sources; the
/// Rust test pins equality against `qdb_console_proxy::QDB_CONSOLE_BUILD`).
pub const QDB_CONSOLE_BUILD: &str = "b4-qdb-console-2026-07-06-r3";

/// Session cookie lifetime — Python `12 * 3600`.
pub const SESSION_TTL_SECS: i128 = 12 * 3600;

/// One-click link token lifetime. The front only VERIFIES link tokens (the
/// portal mints them); the const is kept for wire-contract documentation
/// parity with the Python source.
pub const LINK_TOKEN_TTL_SECS: i128 = 90;

/// Shared row cap (Python `_SQL_MAX_ROWS`).
pub const SQL_MAX_ROWS: u64 = 1000;

pub use crate::qdb_console_proxy::{MAX_BODY_BYTES, MAX_SQL_LEN};

/// Query params forwarded on /exec + /exp (the console sends these; anything
/// else is dropped — deny-by-default).
pub const PASSTHROUGH_PARAMS: [&str; 7] = [
    "limit", "count", "nm", "timings", "explain", "src", "version",
];

/// Query params forwarded on STATIC / /chk / console paths. The raw
/// rawQueryString is NEVER forwarded on these paths (a `?query=drop...` on a
/// static path must not reach QuestDB) — only this whitelist passes.
pub const STATIC_ALLOWED_PARAMS: [&str; 6] = ["f", "j", "v", "version", "nm", "src"];

/// Python `_OFFLINE_MSG` — byte-exact (en dash + em dash included).
pub const OFFLINE_MSG: &str = "tickvault box is offline (auto-stopped outside 08:30–16:30 IST) — \
                               try during market hours";

// -------------------------------------------------------- python semantics

/// Python truthiness for JSON-decoded values (`or` chains, `if v:`).
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str(v)` for JSON-decoded scalars. Deliberate documented
/// deviation for composites: Python rendered dict/list via repr (single
/// quotes); here they render as their JSON text — no test or wire consumer
/// touches that arm (API-GW v2 query/header values are strings).
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Python `int(str)` — strips (unicode) whitespace, one optional sign,
/// ascii digits with single underscores BETWEEN digits. Deviations, both
/// fail-closed and documented: unicode digits are rejected (Python accepted
/// them), and integers beyond i128 fail (Python was arbitrary-precision) —
/// every caller maps `None` onto the Python `ValueError` path.
fn py_int(s: &str) -> Option<i128> {
    let t = s.trim();
    let (neg, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty() {
        return None;
    }
    let mut last_was_digit = false;
    for c in digits.chars() {
        if c.is_ascii_digit() {
            last_was_digit = true;
        } else if c == '_' {
            if !last_was_digit {
                return None;
            }
            last_was_digit = false;
        } else {
            return None;
        }
    }
    if !last_was_digit {
        return None; // trailing underscore
    }
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    let n: i128 = cleaned.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// Python `urllib.parse.quote_plus` — percent-encode everything outside
/// `[A-Za-z0-9_.\-~]`, space → `+`, non-ASCII via UTF-8 %XX (uppercase hex).
fn py_quote_plus(s: &str) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
    // NON_ALPHANUMERIC minus the Python-safe chars, minus space (handled
    // by the literal `+` substitution below).
    const QUOTE_PLUS: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'_')
        .remove(b'.')
        .remove(b'-')
        .remove(b'~')
        .remove(b' ');
    utf8_percent_encode(s, QUOTE_PLUS)
        .to_string()
        .replace(' ', "+")
}

/// Python `urllib.parse.urlencode(dict)` — `k=v` pairs joined with `&`,
/// quote_plus on both sides, INSERTION order preserved (the callers build
/// their pair lists in the exact Python dict-insertion order).
fn py_urlencode(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", py_quote_plus(k), py_quote_plus(v)))
        .collect::<Vec<_>>()
        .join("&")
}

// ------------------------------------------------------- read-only SQL gate
// The gate itself (`is_safe_sql`, the banned/prefix/func constants) is the
// SHARED back implementation — see the module doc. Only the row-cap helpers
// live here (the back never caps; the front does, Python parity).

/// Trailing-`LIMIT` clause match — Python
/// `re.search(r"(?i)\blimit\s+(-?\d+)(\s*,\s*-?\d+)?\s*$", q)`.
/// Returns `(byte_start_of_limit, lo_digits, hi_present)`.
fn parse_trailing_limit(q: &str) -> Option<(usize, String, bool)> {
    let chars: Vec<(usize, char)> = q.char_indices().collect();
    'outer: for (pos, &(byte_at, c)) in chars.iter().enumerate() {
        // Case-insensitive ascii "limit" window.
        if !c.eq_ignore_ascii_case(&'l') || pos + 5 > chars.len() {
            continue;
        }
        let word: String = chars[pos..pos + 5].iter().map(|&(_, ch)| ch).collect();
        if !word.eq_ignore_ascii_case("limit") {
            continue;
        }
        // \b before: start-of-string or a non-word char (see
        // `qdb_console_proxy::is_word_char` for the Python `\w` mapping).
        if pos > 0 && back_gate::is_word_char(chars[pos - 1].1) {
            continue;
        }
        // \s+ after the keyword.
        let mut i = pos + 5;
        let ws_start = i;
        while i < chars.len() && chars[i].1.is_whitespace() {
            i += 1;
        }
        if i == ws_start {
            continue; // "limitx" — no whitespace, not this occurrence
        }
        // (-?\d+)
        let mut lo = String::new();
        if i < chars.len() && chars[i].1 == '-' {
            lo.push('-');
            i += 1;
        }
        let digit_start = i;
        while i < chars.len() && chars[i].1.is_ascii_digit() {
            lo.push(chars[i].1);
            i += 1;
        }
        if i == digit_start {
            continue;
        }
        // (\s*,\s*-?\d+)?
        let mut j = i;
        while j < chars.len() && chars[j].1.is_whitespace() {
            j += 1;
        }
        let mut hi_present = false;
        if j < chars.len() && chars[j].1 == ',' {
            j += 1;
            while j < chars.len() && chars[j].1.is_whitespace() {
                j += 1;
            }
            if j < chars.len() && chars[j].1 == '-' {
                j += 1;
            }
            let hi_start = j;
            while j < chars.len() && chars[j].1.is_ascii_digit() {
                j += 1;
            }
            if j > hi_start {
                hi_present = true;
                i = j;
            }
            // A dangling "," with no digits: the optional group fails to
            // match; fall through with the lo-only parse (regex backtrack).
        }
        // \s*$
        let mut k = i;
        while k < chars.len() && chars[k].1.is_whitespace() {
            k += 1;
        }
        if k != chars.len() {
            continue 'outer;
        }
        return Some((byte_at, lo, hi_present));
    }
    None
}

/// `lo > cap` for a non-negative digit string, with Python's
/// arbitrary-precision semantics preserved fail-closed: an over-u128 value
/// is certainly above the cap.
fn limit_exceeds_cap(lo_digits: &str, cap: u64) -> bool {
    lo_digits
        .parse::<u128>()
        .map(|n| n > u128::from(cap))
        .unwrap_or(true)
}

/// Python `re.split(r"[^a-z]+", q, maxsplit=1)[0]` on an already-stripped
/// lowered string — the leading ascii `a-z` run.
fn first_token_lower(q: &str) -> String {
    q.to_lowercase()
        .chars()
        .take_while(char::is_ascii_lowercase)
        .collect()
}

/// Enforce a server-side row cap on an already-validated read-only query —
/// Python `_cap_sql_rows` parity. A trailing `LIMIT n` above the cap is
/// clamped; a SELECT/WITH query with no trailing LIMIT gets `LIMIT <cap>`
/// appended; SHOW/EXPLAIN are left untouched; QuestDB range/negative forms
/// (`LIMIT lo,hi` / `LIMIT -n`) are left as-is.
pub fn cap_sql_rows(query: &str) -> String {
    let cap = SQL_MAX_ROWS;
    let stripped = query.trim();
    let q: String = if let Some(no_semi) = stripped.strip_suffix(';') {
        no_semi.trim_end().to_string()
    } else {
        stripped.to_string()
    };
    if let Some((start, lo, hi_present)) = parse_trailing_limit(&q) {
        if !hi_present && !lo.starts_with('-') && limit_exceeds_cap(&lo, cap) {
            return format!("{}LIMIT {cap}", &q[..start]);
        }
        return q;
    }
    let first = first_token_lower(&q);
    if first == "select" || first == "with" {
        format!("{q} LIMIT {cap}")
    } else {
        q
    }
}

/// Clamp the console's `limit` pagination param (`n` or `lo,hi`) so a
/// single page can never exceed the row cap. Malformed → the cap.
/// Python `_clamp_limit_param` parity (`int()` semantics via [`py_int`]).
pub fn clamp_limit_param(raw: &str) -> String {
    let cap = i128::from(SQL_MAX_ROWS);
    if let Some((lo_s, hi_s)) = raw.split_once(',') {
        return match (py_int(lo_s), py_int(hi_s)) {
            (Some(lo), Some(mut hi)) => {
                if hi - lo > cap {
                    hi = lo + cap;
                }
                format!("{lo},{hi}")
            }
            _ => cap.to_string(),
        };
    }
    match py_int(raw) {
        Some(n) if n >= 0 => n.min(cap).to_string(),
        Some(_) => raw.to_string(), // negative → the ORIGINAL raw string
        None => cap.to_string(),
    }
}

// ------------------------------------------------------------------- signing

/// Lowercase hex of a byte slice (Python `hexdigest()`).
fn hex_lower(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(TABLE[usize::from(b >> 4)] as char);
        out.push(TABLE[usize::from(b & 0x0f)] as char);
    }
    out
}

/// Python `hmac.new(secret, msg, sha256).hexdigest()`.
fn hmac_hex(secret: &str, msg: &str) -> String {
    let key = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, secret.as_bytes());
    let tag = aws_lc_rs::hmac::sign(&key, msg.as_bytes());
    hex_lower(tag.as_ref())
}

/// Constant-time equality (Python `hmac.compare_digest`). Deliberate
/// documented deviation: Python's str form RAISED TypeError on non-ascii
/// inputs (a 500); this byte compare fails closed to `false` instead.
fn ct_eq(a: &str, b: &str) -> bool {
    aws_lc_rs::constant_time::verify_slices_are_equal(a.as_bytes(), b.as_bytes()).is_ok()
}

/// `<exp>.<hexhmac(secret, "<prefix>|<exp>")>` — the shared token/cookie
/// shape. The portal's `qdb_console_url` action mints link tokens with
/// prefix="qdblink"; sessions here use prefix="qdbsess".
pub fn mint_signed(secret: &str, prefix: &str, exp_epoch: i128) -> String {
    format!(
        "{exp_epoch}.{}",
        hmac_hex(secret, &format!("{prefix}|{exp_epoch}"))
    )
}

/// Constant-time verify of an `<exp>.<hexhmac>` value. Fail-closed on any
/// malformation or expiry. Never raises.
pub fn verify_signed(secret: &str, prefix: &str, value: &str, now_epoch: i128) -> bool {
    if secret.is_empty() || value.is_empty() || !value.contains('.') {
        return false;
    }
    let (exp_s, sig) = match value.split_once('.') {
        Some(parts) => parts,
        None => return false,
    };
    let exp = match py_int(exp_s) {
        Some(e) => e,
        None => return false,
    };
    if exp <= now_epoch {
        return false;
    }
    let expected = hmac_hex(secret, &format!("{prefix}|{exp}"));
    ct_eq(sig, &expected)
}

// ------------------------------------------------------------- event helpers

/// Python `_http_method` — `requestContext.http.method` upper-cased, else
/// `event.get("httpMethod", "POST")`.
fn http_method(event: &Value) -> String {
    let nested = event
        .get("requestContext")
        .and_then(|rc| rc.get("http"))
        .and_then(|h| h.get("method"));
    match nested {
        Some(m) => py_str(m).to_uppercase(),
        None => match event.get("httpMethod") {
            Some(m) => py_str(m).to_uppercase(),
            None => "POST".to_string(),
        },
    }
}

/// Python `_path` — `event.get("rawPath") or event.get("path") or "/"`
/// (falsy chain), then `str()`.
fn path_of(event: &Value) -> String {
    for key in ["rawPath", "path"] {
        if let Some(v) = event.get(key)
            && truthy(v)
        {
            return py_str(v);
        }
    }
    "/".to_string()
}

/// Python `_cookies(event).get(name, "")` — API-GW v2 delivers cookies as a
/// list of `k=v` strings; keys are `.strip()`ed; later duplicates overwrite
/// (last occurrence wins).
fn cookie_get(event: &Value, name: &str) -> String {
    let mut out = String::new();
    if let Some(Value::Array(cookies)) = event.get("cookies") {
        for c in cookies {
            let s = py_str(c);
            let (k, v) = s.split_once('=').unwrap_or((s.as_str(), ""));
            if !k.is_empty() && k.trim() == name {
                out = v.to_string();
            }
        }
    }
    out
}

/// The event's `headers` object (Python `event.get("headers") or {}`).
fn headers_of(event: &Value) -> Map<String, Value> {
    match event.get("headers") {
        Some(Value::Object(o)) => o.clone(),
        _ => Map::new(),
    }
}

/// Python `_authenticated` — Bearer header (constant-time) OR a valid
/// `qdb_sess` cookie. Structural deviation, documented: the Python version
/// re-read `_control_secret()` itself; here the caller passes the secret it
/// already fetched (same cached value — the cache TTL is 60s and both reads
/// happen inside one invoke).
fn authenticated(event: &Value, now_epoch: i128, secret: &str) -> bool {
    if secret.is_empty() {
        return false; // fail closed — no secret => nobody gets in
    }
    let headers = headers_of(event);
    let auth = headers.get("authorization").map(py_str).unwrap_or_default();
    if let Some(rest) = auth.strip_prefix("Bearer ")
        && ct_eq(rest, secret)
    {
        return true;
    }
    verify_signed(secret, "qdbsess", &cookie_get(event, "qdb_sess"), now_epoch)
}

// ------------------------------------------------------------------ responses

/// Python `_json_resp` — API-GW v2 response with a dumped-JSON string body.
/// Documented deviations (no assertion or consumer reads either): serde
/// serializes object keys alphabetically (Python kept insertion order) and
/// compactly (Python `json.dumps` used `", "` / `": "` separators).
fn json_resp(status: i64, body: &Value) -> Value {
    json!({
        "statusCode": status,
        "headers": {"content-type": "application/json", "cache-control": "no-store"},
        "body": serde_json::to_string(body).unwrap_or_default(),
    })
}

/// Mimic QuestDB's /exec error JSON so the console UI renders the message
/// in its own error surface instead of a blank grid.
fn sql_reject_resp(query: &str, msg: &str) -> Value {
    json_resp(400, &json!({"query": query, "error": msg, "position": 0}))
}

/// The Python `_LOGIN_HTML` template, byte-identical (oracle sha256
/// `0277780b…` pinned in the tests).
const LOGIN_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>tickvault · QuestDB console</title>
<style>
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;
background:#0b0f14;color:#dbe6ee;font:15px/1.5 -apple-system,system-ui,sans-serif}
.card{background:#121821;border:1px solid #1f2a36;border-radius:12px;padding:28px;max-width:360px;width:90%}
h1{font-size:17px;margin:0 0 6px}p{color:#7d8b98;font-size:13px;margin:0 0 16px}
input{width:100%;box-sizing:border-box;background:#0b0f14;border:1px solid #2a3947;color:#dbe6ee;
border-radius:8px;padding:10px;font-size:14px}
button{margin-top:12px;width:100%;background:#1667d9;border:0;color:#fff;border-radius:8px;
padding:10px;font-size:14px;cursor:pointer}
.err{color:#ff7a7a;font-size:13px;margin-top:10px}
</style></head><body>
<div class="card"><h1>🗄 QuestDB console</h1>
<p>Read-only. Paste your operator device key to unlock this browser for 12 hours.</p>
<form method="POST" action="/login">
<input type="password" name="key" placeholder="device key" autofocus autocomplete="off">
<button type="submit">Unlock</button></form>
__ERR__
</div></body></html>"#;

/// Python `_login_page` — the login shell with an optional error line. The
/// `err` values are FIXED internal literals (never user input), exactly as
/// in Python — no HTML escaping, parity-preserved.
fn login_page(status: i64, err: &str) -> Value {
    let err_html = if err.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="err">{err}</div>"#)
    };
    let body = LOGIN_HTML.replace("__ERR__", &err_html);
    json!({
        "statusCode": status,
        "headers": {
            "content-type": "text/html; charset=utf-8",
            "cache-control": "no-store",
            "referrer-policy": "no-referrer",
        },
        "body": body,
    })
}

/// Python `_session_redirect` — 302 / with the fresh `qdb_sess` cookie.
fn session_redirect(secret: &str, now_epoch: i128) -> Value {
    let exp = now_epoch + SESSION_TTL_SECS;
    let cookie = format!(
        "qdb_sess={}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age={SESSION_TTL_SECS}",
        mint_signed(secret, "qdbsess", exp)
    );
    json!({
        "statusCode": 302,
        "headers": {"location": "/", "cache-control": "no-store"},
        "cookies": [cookie],
        "body": "",
    })
}

// --------------------------------------------------------------- path gating
// `bad_path` + `classify_path` (Deny/Method/Sql/Static) are the SHARED back
// implementations — Python carried verbatim copies in both handlers.

/// Validate + rebuild the query string for /exec | /exp — Python
/// `_build_sql_raw_query`. `Ok((capped_query, raw_query_string))` on
/// success, `Err(api-gw response)` on rejection.
pub fn build_sql_raw_query(
    path: &str,
    params: &Map<String, Value>,
) -> Result<(String, String), Value> {
    // API-GW v2 already URL-decodes queryStringParameters values.
    let q = match params.get("query") {
        Some(v) if truthy(v) => py_str(v),
        _ => String::new(),
    };
    if q.trim().is_empty() {
        return Err(sql_reject_resp("", "missing query parameter"));
    }
    // Python `len(q)` counts CHARACTERS (and `q[:200]` slices them).
    if q.chars().count() > MAX_SQL_LEN {
        return Err(sql_reject_resp(
            &q.chars().take(200).collect::<String>(),
            &format!("query too long (max {MAX_SQL_LEN} chars)"),
        ));
    }
    if !is_safe_sql(&q) {
        return Err(sql_reject_resp(
            &q,
            "read-only console: only single-statement SELECT/SHOW/EXPLAIN/WITH",
        ));
    }
    let capped = cap_sql_rows(&q);
    let mut fwd: Vec<(String, String)> = vec![("query".to_string(), capped.clone())];
    for k in PASSTHROUGH_PARAMS {
        // Python `if k in params and params[k] is not None` — ONLY null is
        // excluded (empty strings forward as `k=`).
        if let Some(v) = params.get(k)
            && !v.is_null()
        {
            let s = py_str(v);
            let s = if k == "limit" {
                clamp_limit_param(&s)
            } else {
                s
            };
            fwd.push((k.to_string(), s));
        }
    }
    if path == "/exp" && !fwd.iter().any(|(k, _)| k == "limit") {
        // Same belt-and-braces operator-control uses.
        fwd.push(("limit".to_string(), SQL_MAX_ROWS.to_string()));
    }
    Ok((capped, py_urlencode(&fwd)))
}

/// Forward ONLY a whitelisted param set on static / /chk / console paths —
/// NEVER the raw rawQueryString (Python `_build_static_query`).
pub fn build_static_query(params: &Map<String, Value>) -> String {
    let fwd: Vec<(String, String)> = STATIC_ALLOWED_PARAMS
        .iter()
        .filter_map(|&k| {
            params
                .get(k)
                .filter(|v| !v.is_null())
                .map(|v| (k.to_string(), py_str(v)))
        })
        .collect();
    py_urlencode(&fwd)
}

// ------------------------------------------------------------------ back relay

/// Injected dependencies — the Python monkeypatch seams (`_control_secret`
/// and `_invoke_back`) made explicit. The live impl is [`LiveDeps`];
/// tests inject fakes.
#[allow(async_fn_in_trait)] // Send-ness resolves at the concrete call sites
pub trait FrontDeps {
    /// Python `_control_secret()` — SSM runtime read, 60s cache,
    /// fail-closed to `""` on any error.
    async fn control_secret(&mut self) -> String;
    /// Python `_invoke_back(payload)` — lambda:InvokeFunction
    /// (RequestResponse) to the VPC back Lambda; `{"err": "..."}` on any
    /// invoke failure.
    async fn invoke_back(&mut self, payload: Value) -> Value;
}

/// Python `_relay` — forward to the back, translate its typed errors,
/// pass the base64 body through.
async fn relay<D: FrontDeps>(
    deps: &mut D,
    method: &str,
    path: &str,
    raw_query: &str,
    headers: &Map<String, Value>,
) -> Value {
    let mut fwd_headers = Map::new();
    for (k, v) in headers {
        if matches!(
            k.to_lowercase().as_str(),
            "accept" | "accept-encoding" | "content-type"
        ) {
            fwd_headers.insert(k.clone(), v.clone());
        }
    }
    let back = deps
        .invoke_back(json!({
            "method": method,
            "path": path,
            "rawQuery": raw_query,
            "headers": fwd_headers,
        }))
        .await;
    let err = back.get("err").cloned().unwrap_or(Value::Null);
    if err == json!("too_large") {
        return json_resp(
            502,
            &json!({"error": "response too large (>4.1MB) — narrow the query"}),
        );
    }
    if err == json!("upstream_timeout") {
        // B4 r2 diagnostic honesty: the box was reachable but the request
        // timed out (slow QuestDB or an unframed response) — a 504 gateway
        // timeout, NOT the 503 "box offline" path (which is reserved for a
        // genuine connection refusal / stopped box).
        return json_resp(
            504,
            &json!({"error": "tickvault box is slow to respond — try again in a moment"}),
        );
    }
    if truthy(&err) {
        return json_resp(503, &json!({"error": OFFLINE_MSG}));
    }
    let mut resp_headers = Map::new();
    resp_headers.insert("cache-control".to_string(), json!("no-store"));
    let back_headers = match back.get("headers") {
        Some(Value::Object(o)) => o.clone(),
        _ => Map::new(),
    };
    for k in [
        "content-type",
        "content-encoding",
        "cache-control",
        "location",
    ] {
        if let Some(v) = back_headers.get(k)
            && truthy(v)
        {
            resp_headers.insert(k.to_string(), v.clone());
        }
    }
    let body_b64 = match back.get("body_b64") {
        Some(v) if truthy(v) => py_str(v),
        _ => String::new(),
    };
    if body_b64.len() > MAX_BODY_BYTES * 4 / 3 + 8 {
        return json_resp(
            502,
            &json!({"error": "response too large (>4.1MB) — narrow the query"}),
        );
    }
    // Python `int(back.get("status", 200))` — numbers truncate toward zero,
    // numeric strings parse; a non-numeric value would have RAISED in
    // Python (a 500) — documented deviation: default 200 (the back's typed
    // envelope always carries an int).
    let status = match back.get("status") {
        None => 200,
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(200),
        Some(Value::String(s)) => py_int(s).map(|n| n as i64).unwrap_or(200),
        Some(_) => 200,
    };
    json!({
        "statusCode": status,
        "headers": resp_headers,
        "body": body_b64,
        "isBase64Encoded": true,
    })
}

// ---------------------------------------------------------------------- log

/// Python `_log` — one structured JSON line per request. NEVER logs
/// secrets, tokens, or cookies. (`print` → `tracing::info!`; the bin's JSON
/// subscriber renders the same CloudWatch-ingestable shape.)
fn log_req(path: &str, method: &str, outcome: &str, status: i64, sql_head: &str) {
    let head: String = sql_head.chars().take(200).collect();
    info!(
        evt = "qdb_console",
        path,
        method,
        outcome,
        sql_head = %head,
        status,
        "qdb_console request"
    );
}

// ------------------------------------------------------------------- handler

/// Python `_body_text` — raw body, base64-decoded (UTF-8 with replacement)
/// when flagged. Documented deviation: Python's `b64decode` silently
/// DISCARDED non-alphabet characters before the padding check; the strict
/// Rust decoder rejects them — both land on the same fail-closed `""`.
fn body_text(event: &Value) -> String {
    let raw = match event.get("body") {
        Some(v) if truthy(v) => py_str(v),
        _ => String::new(),
    };
    let is_b64 = event.get("isBase64Encoded").map(truthy).unwrap_or(false);
    if is_b64 {
        match base64::engine::general_purpose::STANDARD.decode(raw.as_bytes()) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => String::new(), // malformed body → empty (fail closed)
        }
    } else {
        raw
    }
}

/// Extract the login key from a POST /login body — Python inline logic.
fn login_key(body: &str, ctype: &str) -> String {
    if ctype.contains("json") {
        let text = if body.is_empty() { "{}" } else { body };
        match serde_json::from_str::<Value>(text) {
            // Python `.get("key", "")` then `str(...)`: a missing key is
            // `""`; a present non-string renders via `str` (null → "None").
            Ok(Value::Object(o)) => o.get("key").map(py_str).unwrap_or_default(),
            // Non-object JSON RAISED in Python (a 500) — documented
            // deviation: fail closed to "" (a 401, never a crash).
            Ok(_) => String::new(),
            Err(_) => String::new(),
        }
    } else {
        // The login <form> posts x-www-form-urlencoded — Python
        // `parse_qs(body).get("key", [""])[0]` (first NON-BLANK value).
        back_gate::query_param(body, "key")
    }
}

/// The full handler core — Python `lambda_handler` parity, with the SSM
/// secret + back-invoke seams injected.
pub async fn handle_core<D: FrontDeps>(event: &Value, now_epoch: i128, deps: &mut D) -> Value {
    let method = http_method(event);
    let path = path_of(event);
    let params = match event.get("queryStringParameters") {
        Some(Value::Object(o)) => o.clone(),
        _ => Map::new(),
    };
    let secret = deps.control_secret().await;

    // ---- path-confusion / traversal reject FIRST (before /open, /login,
    // auth, and any classification) — deny-by-default, leaks nothing.
    if bad_path(&path) {
        log_req(&path, &method, "denied_path", 403, "");
        return json_resp(
            403,
            &json!({"error": "path not allowed on the read-only console"}),
        );
    }

    // ---- unauthenticated endpoints: /open (link token) + /login (paste key)
    if method == "GET" && path == "/open" {
        let tok = match params.get("tok") {
            Some(v) if truthy(v) => py_str(v),
            _ => String::new(),
        };
        if !secret.is_empty() && verify_signed(&secret, "qdblink", &tok, now_epoch) {
            log_req(&path, &method, "ok", 302, "");
            return session_redirect(&secret, now_epoch);
        }
        log_req(&path, &method, "denied_auth", 401, "");
        return login_page(401, "link expired — paste your device key instead");
    }

    if method == "POST" && path == "/login" {
        let body = body_text(event);
        let ctype = headers_of(event)
            .get("content-type")
            .filter(|v| truthy(v))
            .map(py_str)
            .unwrap_or_default()
            .to_lowercase();
        let key = login_key(&body, &ctype);
        if !secret.is_empty() && !key.is_empty() && ct_eq(&key, &secret) {
            log_req(&path, &method, "ok", 302, "");
            return session_redirect(&secret, now_epoch);
        }
        log_req(&path, &method, "denied_auth", 401, "");
        return login_page(401, "wrong key");
    }

    // ---- everything else requires auth (cookie or Bearer) — fail closed
    if !authenticated(event, now_epoch, &secret) {
        log_req(&path, &method, "denied_auth", 401, "");
        return login_page(401, "");
    }

    let kind = classify_path(&method, &path);
    if kind == PathKind::Deny {
        log_req(&path, &method, "denied_path", 403, "");
        return json_resp(
            403,
            &json!({"error": "path not allowed on the read-only console"}),
        );
    }
    if kind == PathKind::Method {
        log_req(&path, &method, "denied_path", 405, "");
        return json_resp(405, &json!({"error": "read-only console: GET/HEAD only"}));
    }

    let headers = headers_of(event);
    if kind == PathKind::Sql {
        let (capped_query, raw_query) = match build_sql_raw_query(&path, &params) {
            Ok(pair) => pair,
            Err(rejection) => {
                let status = rejection
                    .get("statusCode")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                // Python `str(params.get("query", ""))[:200]` — a present
                // null renders "None" (py_str), a missing key "".
                let head = params.get("query").map(py_str).unwrap_or_default();
                log_req(&path, &method, "denied_sql", status, &head);
                return rejection;
            }
        };
        let resp = relay(deps, &method, &path, &raw_query, &headers).await;
        let status = resp.get("statusCode").and_then(Value::as_i64).unwrap_or(0);
        let outcome = if status >= 502 { "back_error" } else { "ok" };
        log_req(&path, &method, outcome, status, &capped_query);
        return resp;
    }

    // static shell/assets/chk/settings(read) passthrough — forward ONLY a
    // whitelisted param set, NEVER the raw rawQueryString (so
    // `?query=drop...` smuggled onto a static path cannot reach QuestDB).
    let raw_query = build_static_query(&params);
    let resp = relay(deps, &method, &path, &raw_query, &headers).await;
    let status = resp.get("statusCode").and_then(Value::as_i64).unwrap_or(0);
    let outcome = if status >= 502 { "back_error" } else { "ok" };
    log_req(&path, &method, outcome, status, "");
    resp
}

// ----------------------------------------------------------------- live deps

/// SSM secret cache — Python module-global `_cache` parity (persists across
/// warm invokes; `time.monotonic` → `Instant`). Failures (empty values)
/// are never cached: the next read retries.
static SECRET_CACHE: Mutex<Option<(String, Instant)>> = Mutex::new(None);

/// Python `_SECRET_TTL_SECS = 60.0`.
const SECRET_TTL: Duration = Duration::from_secs(60);

/// The live dependency set — real SSM + Lambda clients. UNPROVEN until
/// deploy: both legs run only in a live Lambda invoke; the pure logic they
/// feed is what the unit tests cover.
pub struct LiveDeps {
    ssm: aws_sdk_ssm::Client,
    lambda: aws_sdk_lambda::Client,
    back_fn_arn: String,
    control_secret_param: String,
}

impl LiveDeps {
    pub async fn new() -> Self {
        let config = crate::clients::sdk_config().await;
        Self {
            ssm: crate::clients::ssm(&config),
            lambda: crate::clients::lambda(&config),
            back_fn_arn: std::env::var("BACK_FN_ARN").unwrap_or_default(),
            control_secret_param: std::env::var("CONTROL_SECRET_PARAM")
                .unwrap_or_else(|_| "/tickvault/prod/operator/control-secret".to_string()),
        }
    }

    /// Python `_load_param` — missing/any-error => `""` (fail closed).
    async fn load_param(&self) -> String {
        if self.control_secret_param.is_empty() {
            return String::new();
        }
        match self
            .ssm
            .get_parameter()
            .name(&self.control_secret_param)
            .with_decryption(true)
            .send()
            .await
        {
            Ok(out) => out
                .parameter()
                .and_then(|p| p.value())
                .unwrap_or_default()
                .to_string(),
            Err(_) => String::new(),
        }
    }
}

impl FrontDeps for LiveDeps {
    async fn control_secret(&mut self) -> String {
        {
            let guard = SECRET_CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((value, at)) = guard.as_ref()
                && !value.is_empty()
                && at.elapsed() <= SECRET_TTL
            {
                return value.clone();
            }
        }
        let fresh = self.load_param().await;
        let mut guard = SECRET_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some((fresh.clone(), Instant::now()));
        fresh
    }

    async fn invoke_back(&mut self, payload: Value) -> Value {
        if self.back_fn_arn.is_empty() {
            return json!({"err": "back_not_configured"});
        }
        let bytes = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(_) => return json!({"err": "back_invoke_failed"}),
        };
        let out = match self
            .lambda
            .invoke()
            .function_name(&self.back_fn_arn)
            .invocation_type(aws_sdk_lambda::types::InvocationType::RequestResponse)
            .payload(aws_sdk_lambda::primitives::Blob::new(bytes))
            .send()
            .await
        {
            Ok(o) => o,
            Err(_) => return json!({"err": "back_invoke_failed"}),
        };
        if out.function_error().is_some() {
            return json!({"err": "back_function_error"});
        }
        let body = out
            .payload()
            .map(|b| b.as_ref().to_vec())
            .unwrap_or_default();
        match serde_json::from_slice::<Value>(&body) {
            Ok(v) => v,
            Err(_) => json!({"err": "back_invoke_failed"}),
        }
    }
}

/// Live entrypoint for the bin. Python `lambda_handler` used
/// `int(time.time())` for `now`.
pub async fn handle(event: Value) -> Result<Value, lambda_runtime::Error> {
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i128::from(d.as_secs()));
    let mut deps = LiveDeps::new().await;
    Ok(handle_core(&event, now_epoch, &mut deps).await)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qdb_console_proxy as back;

    const SECRET: &str = "test-device-key-0123456789";

    fn now_epoch() -> i128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i128::from(d.as_secs()))
    }

    // ---- fakes (the Python WithSecret monkeypatch seam)

    struct FakeDeps {
        secret: String,
        back_calls: Vec<Value>,
        back: Box<dyn FnMut(&Value) -> Value + Send>,
    }

    impl FakeDeps {
        fn new() -> Self {
            Self {
                secret: SECRET.to_string(),
                back_calls: Vec::new(),
                back: Box::new(|_payload| {
                    json!({
                        "status": 200,
                        "headers": {"content-type": "text/html"},
                        "body_b64": base64::engine::general_purpose::STANDARD.encode(b"ok"),
                    })
                }),
            }
        }

        fn with_back(back: impl FnMut(&Value) -> Value + Send + 'static) -> Self {
            let mut d = Self::new();
            d.back = Box::new(back);
            d
        }
    }

    impl FrontDeps for FakeDeps {
        async fn control_secret(&mut self) -> String {
            self.secret.clone()
        }

        async fn invoke_back(&mut self, payload: Value) -> Value {
            self.back_calls.push(payload.clone());
            (self.back)(&payload)
        }
    }

    // ---- the Python `_event` builder

    #[allow(clippy::too_many_arguments)]
    fn event_full(
        method: &str,
        path: &str,
        qs: Option<&[(&str, &str)]>,
        headers: Option<&[(&str, &str)]>,
        cookies: Option<&[&str]>,
        body: Option<&str>,
        b64: bool,
    ) -> Value {
        let qs_pairs: Vec<(String, String)> = qs
            .unwrap_or(&[])
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let qsp: Value = match qs {
            Some(pairs) => {
                let mut m = Map::new();
                for (k, v) in pairs {
                    m.insert(k.to_string(), json!(v));
                }
                Value::Object(m)
            }
            None => Value::Null,
        };
        let hdrs: Map<String, Value> = headers
            .unwrap_or(&[])
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect();
        json!({
            "requestContext": {"http": {"method": method}},
            "rawPath": path,
            "rawQueryString": py_urlencode(&qs_pairs),
            "queryStringParameters": qsp,
            "headers": hdrs,
            "cookies": cookies.unwrap_or(&[]),
            "body": body,
            "isBase64Encoded": b64,
        })
    }

    fn event(method: &str, path: &str, qs: Option<&[(&str, &str)]>) -> Value {
        event_full(method, path, qs, None, None, None, false)
    }

    fn bearer() -> Vec<(&'static str, &'static str)> {
        vec![("authorization", "Bearer test-device-key-0123456789")]
    }

    fn body_str(resp: &Value) -> String {
        resp.get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    fn status_of(resp: &Value) -> i64 {
        resp.get("statusCode").and_then(Value::as_i64).unwrap_or(-1)
    }

    async fn run(event: &Value, deps: &mut FakeDeps) -> Value {
        handle_core(event, now_epoch(), deps).await
    }

    // The adversarial corpus — the Python `_SQL_CORPUS` base entries, with
    // the operator-control gate's verdict per entry DERIVED FROM THE PYTHON
    // ORACLE (scratchpad/w3-oracle.json run of
    // deploy/aws/lambda/operator-control/handler.py, 2026-07-18). On the
    // base corpus front == opctl on every entry (the introspection-func
    // superset diverges only on entries outside this list).
    const SQL_CORPUS_BASE: [(&str, bool); 23] = [
        // multi-statement / chaining
        ("select 1; drop table ticks", false),
        ("select 1;;", false),
        ("select 1 ; select 2", false),
        ("select 1;", true), // single trailing ';' is OK
        ("select 1 ;", true),
        // comments
        ("select 1 -- drop table ticks", false),
        ("select /* drop */ 1", false),
        ("--select 1", false),
        // first-word gate
        ("selector 1", false),
        ("explainx select 1", false),
        ("(select 1)", false),
        ("insert into t values(1)", false),
        // banned words embedded, any case/position
        ("select * from t where x = 'UPDATE'", false),
        ("SELECT INSERT", false),
        ("with x as (delete from t) select 1", false),
        ("select 1 union all select 2", true), // allowed — no mutator
        ("WITH x AS (SELECT 1) SELECT * FROM x", true),
        // leading whitespace / newlines / tabs / uppercase
        ("  \n\t SELECT 1", true),
        ("\nshow tables", true),
        ("explain select * from ticks", true),
        // empty / whitespace-only
        ("", false),
        ("   ", false),
        (";", false),
    ];

    /// The full corpus: base entries + every banned keyword exercised
    /// individually (the Python `_SQL_CORPUS +=` loops; oracle verdict for
    /// every generated entry is `false`).
    fn sql_corpus() -> Vec<(String, bool)> {
        let mut corpus: Vec<(String, bool)> = SQL_CORPUS_BASE
            .iter()
            .map(|(q, ok)| (q.to_string(), *ok))
            .collect();
        for kw in back::SQL_BANNED {
            corpus.push((format!("select {kw} from t"), false));
            corpus.push((
                format!("select * from t where a = {}", kw.to_uppercase()),
                false,
            ));
        }
        corpus
    }

    // ---- SqlGateParity (Python: 8 tests) ----

    #[test]
    fn test_sql_gate_parity_with_operator_control() {
        // The mirrored constants must be IDENTICAL tuples — the
        // operator-control values here are the ORACLE dump of its
        // _SQL_ALLOWED_PREFIXES / _SQL_BANNED (w3-oracle.json:
        // prefixes_equal=true, banned_equal=true).
        assert_eq!(
            back::SQL_ALLOWED_PREFIXES,
            ["select", "show", "explain", "with"]
        );
        assert_eq!(back::SQL_BANNED.len(), 25);
        for (q, opctl_verdict) in sql_corpus() {
            assert_eq!(
                is_safe_sql(&q),
                opctl_verdict,
                "gate parity broken for query: {q:?}"
            );
        }
    }

    #[test]
    fn test_cap_sql_rows_parity_with_operator_control() {
        // Expected outputs are the ORACLE run of operator-control's
        // _cap_sql_rows (w3-oracle.json `cap`, front == opctl on all 8).
        for (q, expected) in [
            ("select * from ticks", "select * from ticks LIMIT 1000"),
            (
                "select * from ticks LIMIT 5000",
                "select * from ticks LIMIT 1000",
            ),
            (
                "select * from ticks limit 10",
                "select * from ticks limit 10",
            ),
            (
                "select * from ticks limit 0,5000",
                "select * from ticks limit 0,5000",
            ),
            (
                "select * from ticks limit -5",
                "select * from ticks limit -5",
            ),
            ("show tables", "show tables"),
            ("explain select 1", "explain select 1"),
            (
                "with x as (select 1) select * from x;",
                "with x as (select 1) select * from x LIMIT 1000",
            ),
        ] {
            assert_eq!(cap_sql_rows(q), expected, "{q}");
        }
    }

    #[test]
    fn test_multi_statement_rejected() {
        assert!(!is_safe_sql("select 1; drop table ticks"));
        assert!(!is_safe_sql("select 1; select 2"));
        assert!(is_safe_sql("select 1;")); // one trailing ';' OK
    }

    #[test]
    fn test_comment_smuggling_rejected() {
        assert!(!is_safe_sql("select 1 -- drop table ticks"));
        assert!(!is_safe_sql("select /* hidden */ 1"));
    }

    #[test]
    fn test_banned_words_rejected_any_case() {
        for kw in back::SQL_BANNED {
            assert!(!is_safe_sql(&format!("select {kw} from t")), "{kw}");
            assert!(
                !is_safe_sql(&format!("select * from t where {}=1", kw.to_uppercase())),
                "{kw}"
            );
        }
    }

    #[test]
    fn test_cte_hiding_mutator_rejected() {
        assert!(!is_safe_sql("with x as (insert into t values(1)) select 1"));
        assert!(is_safe_sql("WITH x AS (SELECT 1) SELECT * FROM x"));
    }

    #[test]
    fn test_empty_whitespace_and_leading_ws() {
        assert!(!is_safe_sql(""));
        assert!(!is_safe_sql("   "));
        assert!(!is_safe_sql(";"));
        assert!(is_safe_sql("  \n\t SELECT 1"));
    }

    #[test]
    fn test_parenthesised_select_rejected_like_operator_control() {
        // The first-word split sees a leading non-letter → "" → reject.
        // Oracle: opctl._is_safe_sql("(select 1)") == False — fail-closed
        // parity.
        assert!(!is_safe_sql("(select 1)"));
    }

    // ---- SqlIntrospectionSuperset (Python: 6 tests) ----

    #[test]
    fn test_each_allowed_func_passes_bare_and_with_args() {
        for fun in back::SQL_ALLOWED_FUNCS {
            assert!(is_safe_sql(&format!("{fun}()")), "{fun}");
            assert!(is_safe_sql(&format!("{}()", fun.to_uppercase())), "{fun}");
        }
        assert!(is_safe_sql("columns('ticks')"));
        assert!(is_safe_sql("table_columns('ticks')"));
        assert!(is_safe_sql("  tables()  "));
    }

    #[test]
    fn test_unknown_func_still_rejected() {
        assert!(!is_safe_sql("evil_func()"));
        assert!(!is_safe_sql("pg_sleep(10)"));
    }

    #[test]
    fn test_func_chaining_rejected() {
        assert!(!is_safe_sql("tables(); drop table ticks"));
        assert!(!is_safe_sql("tables() ; select 1"));
    }

    #[test]
    fn test_func_with_banned_word_rejected() {
        // A banned mutator after an allowed func still trips the whole-word
        // scan.
        assert!(!is_safe_sql("tables() delete"));
        assert!(!is_safe_sql("columns('t') drop"));
    }

    #[test]
    fn test_insert_still_rejected() {
        assert!(!is_safe_sql("insert into t values(1)"));
    }

    #[test]
    fn test_front_back_gate_byte_identical() {
        // The Rust port makes the Python front/back byte-parity STRUCTURAL:
        // the front calls the back's `is_safe_sql` directly. This ported
        // test keeps the corpus exercised through both call paths and pins
        // the shared verdicts against the oracle (front == back on ALL 89
        // oracle corpus entries, w3-oracle.json).
        assert_eq!(back::SQL_ALLOWED_FUNCS.len(), 12);
        let mut corpus: Vec<(String, bool)> = sql_corpus();
        for fun in back::SQL_ALLOWED_FUNCS {
            corpus.push((format!("{fun}()"), true));
        }
        corpus.push(("columns('ticks')".to_string(), true));
        corpus.push(("tables(); drop table ticks".to_string(), false));
        corpus.push(("evil_func()".to_string(), false));
        corpus.push(("tables() delete".to_string(), false));
        for (q, expected) in corpus {
            assert_eq!(is_safe_sql(&q), back::is_safe_sql(&q), "{q}");
            assert_eq!(is_safe_sql(&q), expected, "{q}");
        }
    }

    // ---- Signing (Python: 7 tests) ----

    #[test]
    fn test_mint_verify_roundtrip() {
        let now = now_epoch();
        let tok = mint_signed(SECRET, "qdblink", now + 90);
        assert!(verify_signed(SECRET, "qdblink", &tok, now));
        // Wire-contract vectors DERIVED FROM THE PYTHON ORACLE
        // (w3-oracle.json `hmac`, python3 run of the original handler —
        // the operator-control MINT side must byte-match these).
        assert_eq!(
            mint_signed(SECRET, "qdblink", 1_780_000_090),
            "1780000090.1423f6c5aaf7e2cda2d63d92fd2562294af392c19d9f641f2217ad3dd43b1bf4"
        );
        assert_eq!(
            mint_signed(SECRET, "qdbsess", 1_780_000_090),
            "1780000090.171b2270b189ea49c7b193770a29e53060875890913811aa5d478ebc2506c77e"
        );
        assert_eq!(
            mint_signed("s3cret-token", "qdblink", 1_780_000_090),
            "1780000090.643a1010a7594e457633d6a2ba7f0e31dd84efbb8b0bf4191a1b5bed07caa163"
        );
    }

    #[test]
    fn test_expired_token_rejected() {
        let now = now_epoch();
        let tok = mint_signed(SECRET, "qdblink", now - 1);
        assert!(!verify_signed(SECRET, "qdblink", &tok, now));
    }

    #[test]
    fn test_forged_hmac_rejected() {
        let now = now_epoch();
        let tok = mint_signed("some-other-secret", "qdblink", now + 90);
        assert!(!verify_signed(SECRET, "qdblink", &tok, now));
    }

    #[test]
    fn test_wrong_prefix_rejected() {
        // A session cookie can never be replayed as a link token (domain
        // separation).
        let now = now_epoch();
        let tok = mint_signed(SECRET, "qdbsess", now + 90);
        assert!(!verify_signed(SECRET, "qdblink", &tok, now));
    }

    #[test]
    fn test_garbage_and_empty_rejected() {
        let now = now_epoch();
        for bad in [
            "",
            "garbage",
            "abc.def",
            "12345",
            "99999999999999999999.x",
            ".",
            "1e9.aa",
        ] {
            assert!(!verify_signed(SECRET, "qdblink", bad, now), "{bad}");
        }
    }

    #[test]
    fn test_no_secret_rejects_everything() {
        let now = now_epoch();
        let tok = mint_signed(SECRET, "qdblink", now + 90);
        assert!(!verify_signed("", "qdblink", &tok, now));
    }

    #[test]
    fn test_constant_time_compare_used() {
        // The ratchet the brief demands: every secret compare goes through
        // the constant-time helper — token/cookie verify AND the two direct
        // key compares (Bearer + login POST). Python asserted
        // `hmac.compare_digest` in the source of _verify_signed /
        // _authenticated / lambda_handler; here the source-scan pins
        // `ct_eq(` at all three sites (production region only) and the
        // helper's constant-time core.
        let src = include_str!("qdb_console_front.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or("");
        assert!(prod.contains("verify_slices_are_equal"), "ct core missing");
        let ct_call_sites = prod.matches("ct_eq(").count();
        // 1 definition + verify_signed + authenticated + login POST.
        assert!(
            ct_call_sites >= 4,
            "expected >=4 ct_eq occurrences in production region, got {ct_call_sites}"
        );
    }

    // ---- AuthFlow (Python: 14 tests) ----

    #[tokio::test]
    async fn test_unauthenticated_get_returns_login_page() {
        let mut deps = FakeDeps::new();
        let r = run(&event("GET", "/", None), &mut deps).await;
        assert_eq!(status_of(&r), 401);
        assert!(body_str(&r).contains("device key"));
        assert!(!body_str(&r).contains(SECRET)); // never echo the secret
    }

    #[tokio::test]
    async fn test_open_with_valid_token_sets_cookie_and_redirects() {
        let mut deps = FakeDeps::new();
        let tok = mint_signed(SECRET, "qdblink", now_epoch() + 90);
        let r = run(
            &event("GET", "/open", Some(&[("tok", tok.as_str())])),
            &mut deps,
        )
        .await;
        assert_eq!(status_of(&r), 302);
        assert_eq!(r["headers"]["location"], json!("/"));
        let cookie = r["cookies"][0].as_str().unwrap_or("");
        assert!(cookie.contains("qdb_sess="));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains(&format!("Max-Age={SESSION_TTL_SECS}")));
    }

    #[tokio::test]
    async fn test_open_with_expired_token_401() {
        let mut deps = FakeDeps::new();
        let tok = mint_signed(SECRET, "qdblink", now_epoch() - 5);
        let r = run(
            &event("GET", "/open", Some(&[("tok", tok.as_str())])),
            &mut deps,
        )
        .await;
        assert_eq!(status_of(&r), 401);
    }

    #[tokio::test]
    async fn test_open_with_forged_token_401() {
        let mut deps = FakeDeps::new();
        let tok = mint_signed("attacker-secret", "qdblink", now_epoch() + 90);
        let r = run(
            &event("GET", "/open", Some(&[("tok", tok.as_str())])),
            &mut deps,
        )
        .await;
        assert_eq!(status_of(&r), 401);
    }

    #[tokio::test]
    async fn test_open_with_garbage_token_401() {
        let mut deps = FakeDeps::new();
        let r = run(&event("GET", "/open", Some(&[("tok", "zzz")])), &mut deps).await;
        assert_eq!(status_of(&r), 401);
    }

    #[tokio::test]
    async fn test_valid_session_cookie_authenticates() {
        let mut deps = FakeDeps::new();
        let sess = mint_signed(SECRET, "qdbsess", now_epoch() + 3600);
        let cookie = format!("qdb_sess={sess}");
        let ev = event_full(
            "GET",
            "/",
            None,
            None,
            Some(&[cookie.as_str()]),
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 200); // forwarded to (fake) back
    }

    #[tokio::test]
    async fn test_expired_cookie_rejected() {
        let mut deps = FakeDeps::new();
        let sess = mint_signed(SECRET, "qdbsess", now_epoch() - 5);
        let cookie = format!("qdb_sess={sess}");
        let ev = event_full(
            "GET",
            "/",
            None,
            None,
            Some(&[cookie.as_str()]),
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 401);
    }

    #[tokio::test]
    async fn test_forged_cookie_rejected() {
        let mut deps = FakeDeps::new();
        let sess = mint_signed("attacker", "qdbsess", now_epoch() + 3600);
        let cookie = format!("qdb_sess={sess}");
        let ev = event_full(
            "GET",
            "/",
            None,
            None,
            Some(&[cookie.as_str()]),
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 401);
    }

    #[tokio::test]
    async fn test_bearer_right_key_authorized() {
        let mut deps = FakeDeps::new();
        let ev = event_full("GET", "/", None, Some(&bearer()), None, None, false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 200);
    }

    #[tokio::test]
    async fn test_bearer_wrong_key_rejected() {
        let mut deps = FakeDeps::new();
        let ev = event_full(
            "GET",
            "/",
            None,
            Some(&[("authorization", "Bearer nope")]),
            None,
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 401);
    }

    #[tokio::test]
    async fn test_login_post_right_key_sets_cookie() {
        let mut deps = FakeDeps::new();
        let body = py_urlencode(&[("key".to_string(), SECRET.to_string())]);
        let ev = event_full(
            "POST",
            "/login",
            None,
            Some(&[("content-type", "application/x-www-form-urlencoded")]),
            None,
            Some(&body),
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 302);
        assert!(r["cookies"][0].as_str().unwrap_or("").contains("qdb_sess="));
    }

    #[tokio::test]
    async fn test_login_post_wrong_key_401() {
        let mut deps = FakeDeps::new();
        let body = py_urlencode(&[("key".to_string(), "wrong".to_string())]);
        let ev = event_full("POST", "/login", None, None, None, Some(&body), false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 401);
        assert!(body_str(&r).contains("wrong key"));
    }

    #[tokio::test]
    async fn test_login_post_base64_body_json() {
        let mut deps = FakeDeps::new();
        let json_body = serde_json::to_string(&json!({"key": SECRET})).unwrap_or_default();
        let b64 = base64::engine::general_purpose::STANDARD.encode(json_body.as_bytes());
        let ev = event_full(
            "POST",
            "/login",
            None,
            Some(&[("content-type", "application/json")]),
            None,
            Some(&b64),
            true,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 302);
    }

    #[tokio::test]
    async fn test_no_secret_configured_denies_all() {
        let mut deps = FakeDeps::new();
        deps.secret = String::new();
        let ev = event_full("GET", "/", None, Some(&bearer()), None, None, false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 401);
        // Even a "valid-looking" open link dies without a secret
        // (fail closed).
        let r = run(&event("GET", "/open", Some(&[("tok", "1.a")])), &mut deps).await;
        assert_eq!(status_of(&r), 401);
    }

    // ---- PathMethodGate (Python: 16 tests) ----

    #[tokio::test]
    async fn test_imp_path_403() {
        let mut deps = FakeDeps::new();
        let ev = event_full("GET", "/imp", None, Some(&bearer()), None, None, false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 403);
        assert!(deps.back_calls.is_empty()); // never forwarded
    }

    #[tokio::test]
    async fn test_path_traversal_confusion_denied() {
        // FIX 1: none of these may reach the classifier / SQL gate / back.
        type Case<'a> = (&'a str, Option<&'a [(&'a str, &'a str)]>);
        let cases: [Case<'_>; 5] = [
            ("/assets/../exec", Some(&[("query", "drop table ticks")])),
            ("/exec/../imp", None),
            ("/index.html/../exec", Some(&[("query", "select 1")])),
            ("/assets//app.js", None),
            ("/assets/\\..\\exec", None),
        ];
        for (path, qs) in cases {
            let mut deps = FakeDeps::new();
            let ev = event_full("GET", path, qs, Some(&bearer()), None, None, false);
            let r = run(&ev, &mut deps).await;
            assert_eq!(status_of(&r), 403, "{path}");
            assert!(deps.back_calls.is_empty(), "{path}");
        }
    }

    #[tokio::test]
    async fn test_encoded_traversal_denied() {
        // %2e%2e%2fexec, %2f, %5c encoded separators — rejected on the raw
        // form.
        for path in [
            "/%2e%2e%2fexec",
            "/assets/%2e%2e/exec",
            "/%2fexec",
            "/a%5c..%5cexec",
        ] {
            let mut deps = FakeDeps::new();
            let ev = event_full(
                "GET",
                path,
                Some(&[("query", "drop table ticks")]),
                Some(&bearer()),
                None,
                None,
                false,
            );
            let r = run(&ev, &mut deps).await;
            assert_eq!(status_of(&r), 403, "{path}");
            assert!(deps.back_calls.is_empty(), "{path}");
        }
    }

    #[tokio::test]
    async fn test_static_path_does_not_execute_sql() {
        // FIX 1: `?query=...` on a static path must NOT be forwarded to
        // QuestDB.
        let mut deps = FakeDeps::new();
        let ev = event_full(
            "GET",
            "/index.html",
            Some(&[("query", "drop table ticks"), ("v", "9")]),
            Some(&bearer()),
            None,
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 200);
        assert_eq!(deps.back_calls[0]["path"], json!("/index.html"));
        let raw = deps.back_calls[0]["rawQuery"].as_str().unwrap_or("");
        let fwd: Vec<(String, String)> = form_urlencoded::parse(raw.as_bytes())
            .into_owned()
            .collect();
        assert!(!fwd.iter().any(|(k, _)| k == "query")); // never smuggled
        assert!(fwd.contains(&("v".to_string(), "9".to_string()))); // whitelisted survives
    }

    #[tokio::test]
    async fn test_chk_forwards_only_whitelisted_params() {
        let mut deps = FakeDeps::new();
        let ev = event_full(
            "GET",
            "/chk",
            Some(&[("f", "json"), ("j", "ticks"), ("query", "drop table ticks")]),
            Some(&bearer()),
            None,
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 200);
        let raw = deps.back_calls[0]["rawQuery"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let fwd: Vec<(String, String)> = form_urlencoded::parse(raw.as_bytes())
            .into_owned()
            .collect();
        assert!(fwd.contains(&("f".to_string(), "json".to_string())));
        assert!(fwd.contains(&("j".to_string(), "ticks".to_string())));
        assert!(!fwd.iter().any(|(k, _)| k == "query"));
        // Oracle byte-vector for this exact whitelist forward
        // (w3-oracle.json `static_q`).
        assert_eq!(
            build_static_query(&{
                let mut m = Map::new();
                m.insert("f".to_string(), json!("json"));
                m.insert("j".to_string(), json!("ticks"));
                m.insert("v".to_string(), json!("9"));
                m.insert("query".to_string(), json!("drop table ticks"));
                m
            }),
            "f=json&j=ticks&v=9"
        );
    }

    #[tokio::test]
    async fn test_post_exec_405() {
        let mut deps = FakeDeps::new();
        let ev = event_full(
            "POST",
            "/exec",
            Some(&[("query", "select 1")]),
            Some(&bearer()),
            None,
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 405);
        assert!(deps.back_calls.is_empty());
    }

    #[tokio::test]
    async fn test_unknown_path_403() {
        let mut deps = FakeDeps::new();
        let ev = event_full(
            "GET",
            "/api/thing",
            None,
            Some(&bearer()),
            None,
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 403);
    }

    #[tokio::test]
    async fn test_settings_write_405() {
        let mut deps = FakeDeps::new();
        let ev = event_full("PUT", "/settings", None, Some(&bearer()), None, None, false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 405);
    }

    #[tokio::test]
    async fn test_assets_forwarded() {
        let mut deps = FakeDeps::new();
        let ev = event_full(
            "GET",
            "/assets/app.js",
            None,
            Some(&bearer()),
            None,
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 200);
        assert_eq!(deps.back_calls[0]["path"], json!("/assets/app.js"));
    }

    #[tokio::test]
    async fn test_chk_forwarded() {
        let mut deps = FakeDeps::new();
        let ev = event_full(
            "GET",
            "/chk",
            Some(&[("f", "json"), ("j", "ticks")]),
            Some(&bearer()),
            None,
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 200);
        assert_eq!(deps.back_calls[0]["path"], json!("/chk"));
    }

    #[tokio::test]
    async fn test_exec_without_query_400() {
        let mut deps = FakeDeps::new();
        let ev = event_full("GET", "/exec", None, Some(&bearer()), None, None, false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 400);
        assert!(body_str(&r).contains("missing query"));
    }

    #[tokio::test]
    async fn test_exec_mutator_400_questdb_shaped() {
        let mut deps = FakeDeps::new();
        let ev = event_full(
            "GET",
            "/exec",
            Some(&[("query", "drop table ticks")]),
            Some(&bearer()),
            None,
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 400);
        let body: Value = serde_json::from_str(&body_str(&r)).unwrap_or_default();
        // QuestDB /exec error JSON shape so the console UI renders it.
        let obj = body.as_object().cloned().unwrap_or_default();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["error", "position", "query"]);
        assert!(
            obj["error"]
                .as_str()
                .unwrap_or("")
                .contains("read-only console")
        );
        assert!(deps.back_calls.is_empty());
    }

    #[tokio::test]
    async fn test_exec_overlength_query_400() {
        let mut deps = FakeDeps::new();
        let q = format!("select {}", "1,".repeat(MAX_SQL_LEN));
        let ev = event_full(
            "GET",
            "/exec",
            Some(&[("query", q.as_str())]),
            Some(&bearer()),
            None,
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 400);
        let body: Value = serde_json::from_str(&body_str(&r)).unwrap_or_default();
        assert!(body["error"].as_str().unwrap_or("").contains("too long"));
    }

    #[tokio::test]
    async fn test_exec_forwards_capped_query_and_whitelisted_params_only() {
        let mut deps = FakeDeps::new();
        let ev = event_full(
            "GET",
            "/exec",
            Some(&[
                ("query", "select * from ticks"),
                ("count", "true"),
                ("evil", "1"),
            ]),
            Some(&bearer()),
            None,
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 200);
        let raw = deps.back_calls[0]["rawQuery"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let fwd: Vec<(String, String)> = form_urlencoded::parse(raw.as_bytes())
            .into_owned()
            .collect();
        assert!(fwd.contains(&(
            "query".to_string(),
            "select * from ticks LIMIT 1000".to_string()
        )));
        assert!(fwd.contains(&("count".to_string(), "true".to_string())));
        assert!(!fwd.iter().any(|(k, _)| k == "evil"));
        // Oracle byte-vector (w3-oracle.json `build_sql.exec_count` — the
        // Python quote_plus wire form, '*' percent-encoded).
        assert_eq!(raw, "query=select+%2A+from+ticks+LIMIT+1000&count=true");
    }

    #[tokio::test]
    async fn test_exec_limit_param_clamped() {
        let mut deps = FakeDeps::new();
        let ev = event_full(
            "GET",
            "/exec",
            Some(&[("query", "show tables"), ("limit", "0,999999")]),
            Some(&bearer()),
            None,
            None,
            false,
        );
        run(&ev, &mut deps).await;
        let raw = deps.back_calls[0]["rawQuery"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let fwd: Vec<(String, String)> = form_urlencoded::parse(raw.as_bytes())
            .into_owned()
            .collect();
        assert!(fwd.contains(&("limit".to_string(), "0,1000".to_string())));
        // Oracle byte-vector (w3-oracle.json `build_sql.exec_clamped`).
        assert_eq!(raw, "query=show+tables&limit=0%2C1000");
    }

    #[tokio::test]
    async fn test_exp_gets_belt_and_braces_limit() {
        let mut deps = FakeDeps::new();
        let ev = event_full(
            "GET",
            "/exp",
            Some(&[("query", "show tables")]),
            Some(&bearer()),
            None,
            None,
            false,
        );
        run(&ev, &mut deps).await;
        let raw = deps.back_calls[0]["rawQuery"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let fwd: Vec<(String, String)> = form_urlencoded::parse(raw.as_bytes())
            .into_owned()
            .collect();
        assert!(fwd.contains(&("limit".to_string(), "1000".to_string())));
        // Oracle byte-vector (w3-oracle.json `build_sql.exp_default_limit`).
        assert_eq!(raw, "query=show+tables&limit=1000");
    }

    // ---- RelayPlumbing (Python: 9 tests) ----

    #[tokio::test]
    async fn test_binary_base64_passthrough_preserves_headers() {
        let blob = base64::engine::general_purpose::STANDARD.encode(b"\x1f\x8b\x00binary");
        let blob_out = blob.clone();
        let mut deps = FakeDeps::with_back(move |_p| {
            json!({
                "status": 200,
                "headers": {
                    "content-type": "font/woff2",
                    "content-encoding": "gzip",
                    "cache-control": "max-age=60",
                },
                "body_b64": blob_out,
            })
        });
        let ev = event_full(
            "GET",
            "/assets/font.woff2",
            None,
            Some(&bearer()),
            None,
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 200);
        assert_eq!(r["isBase64Encoded"], json!(true));
        assert_eq!(r["body"], json!(blob));
        assert_eq!(r["headers"]["content-type"], json!("font/woff2"));
        assert_eq!(r["headers"]["content-encoding"], json!("gzip"));
        assert_eq!(r["headers"]["cache-control"], json!("max-age=60"));
    }

    #[tokio::test]
    async fn test_oversize_body_502() {
        let mut deps = FakeDeps::with_back(|_p| json!({"err": "too_large"}));
        let ev = event_full("GET", "/", None, Some(&bearer()), None, None, false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 502);
        let body: Value = serde_json::from_str(&body_str(&r)).unwrap_or_default();
        assert!(body["error"].as_str().unwrap_or("").contains("too large"));
    }

    #[tokio::test]
    async fn test_box_unreachable_503_honest_offline_message() {
        let mut deps = FakeDeps::with_back(|_p| json!({"err": "box_unreachable"}));
        let ev = event_full("GET", "/", None, Some(&bearer()), None, None, false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 503);
        let body: Value = serde_json::from_str(&body_str(&r)).unwrap_or_default();
        let msg = body["error"].as_str().unwrap_or("");
        assert!(msg.contains("offline"));
        assert!(msg.contains("08:30"));
        // Stronger than the Python substring asserts: the full oracle
        // message (w3-oracle.json `offline_msg`, en/em dashes included).
        assert_eq!(msg, OFFLINE_MSG);
    }

    #[tokio::test]
    async fn test_shell_get_authed_forwards_to_back_root() {
        // B4: the console SHELL (GET /) reaches the back with path "/" so
        // the back can apply the identity + Connection:close framing fix.
        let mut deps = FakeDeps::new();
        let ev = event_full("GET", "/", None, Some(&bearer()), None, None, false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 200);
        assert_eq!(deps.back_calls[0]["path"], json!("/"));
    }

    #[tokio::test]
    async fn test_upstream_timeout_maps_to_504_not_503() {
        // B4 r2 diagnostic honesty: a reachable-but-slow box is a 504
        // gateway timeout, distinct from the 503 offline path (genuine
        // refusal).
        let mut deps = FakeDeps::with_back(|_p| json!({"err": "upstream_timeout"}));
        let ev = event_full("GET", "/", None, Some(&bearer()), None, None, false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 504);
        let body: Value = serde_json::from_str(&body_str(&r)).unwrap_or_default();
        let msg = body["error"].as_str().unwrap_or("");
        assert!(msg.contains("slow"));
        assert!(!msg.contains("offline"));
    }

    #[test]
    // The python test pins the CONSTANT itself — a constant assertion is
    // the point (ratchet on the cap value), so the lint is waived here.
    #[allow(clippy::assertions_on_constants)]
    fn test_body_cap_constant_within_6mib_envelope() {
        // FIX 2: base64(cap) + JSON must stay under Lambda's 6 MiB
        // response.
        assert!(MAX_BODY_BYTES <= 4_100_000);
    }

    #[tokio::test]
    async fn test_over_cap_back_body_maps_to_502_not_503() {
        // A too_large from the back is a 502, never the 503 offline path.
        let mut deps = FakeDeps::with_back(|_p| json!({"err": "too_large"}));
        let ev = event_full("GET", "/", None, Some(&bearer()), None, None, false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 502);
        // And an over-sized base64 body_b64 from a (mis)reporting back →
        // 502.
        let big = "A".repeat(MAX_BODY_BYTES * 4 / 3 + 100);
        let mut deps =
            FakeDeps::with_back(move |_p| json!({"status": 200, "headers": {}, "body_b64": big}));
        let ev = event_full("GET", "/", None, Some(&bearer()), None, None, false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 502);
    }

    #[tokio::test]
    async fn test_relay_forwards_location_on_3xx() {
        // B4 r3 defense-in-depth: the back relays a 3xx BODY-LESS (status +
        // Location) — the front must forward `location` so the browser can
        // follow to /index.html. FAILED on the Python r2 bytes (the _relay
        // header tuple dropped `location`).
        let mut deps = FakeDeps::with_back(|_p| {
            json!({
                "status": 301,
                "headers": {"location": "/index.html"},
                "body_b64": "",
            })
        });
        let ev = event_full("GET", "/", None, Some(&bearer()), None, None, false);
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 301);
        assert_eq!(r["headers"]["location"], json!("/index.html"));
        assert_eq!(r["isBase64Encoded"], json!(true));
    }

    #[tokio::test]
    async fn test_back_status_relayed() {
        let mut deps = FakeDeps::with_back(|_p| {
            json!({
                "status": 400,
                "headers": {"content-type": "application/json"},
                "body_b64": base64::engine::general_purpose::STANDARD.encode(b"{\"error\":\"x\"}"),
            })
        });
        let ev = event_full(
            "GET",
            "/exec",
            Some(&[("query", "select bad_col from ticks")]),
            Some(&bearer()),
            None,
            None,
            false,
        );
        let r = run(&ev, &mut deps).await;
        assert_eq!(status_of(&r), 400);
    }

    // ---- BuildMarker (Python: 1 test) ----

    #[test]
    fn test_build_marker_present() {
        // Proof-3 ratchet: the deployed-bytes marker exists in BOTH halves
        // of the console pair with the SAME value (Python asserted the
        // front marker appears in the back source).
        assert!(QDB_CONSOLE_BUILD.starts_with("b4-qdb-console-"));
        assert_eq!(QDB_CONSOLE_BUILD, back::QDB_CONSOLE_BUILD);
    }

    // ---- Extra (beyond the 61 Python tests): oracle byte-vectors ----

    #[test]
    fn test_python_oracle_vectors() {
        // Byte-equivalence law: every value below was produced by running
        // python3 on the original handler.py (w3-oracle.json, 2026-07-18).
        fn sha256_hex(s: &str) -> String {
            hex_lower(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, s.as_bytes()).as_ref())
        }
        // The login template + its three rendered forms.
        assert_eq!(
            sha256_hex(LOGIN_HTML),
            "0277780beac0244207435450edcc62185bfef614c6854e1b256fd08b62478830"
        );
        assert_eq!(
            sha256_hex(
                login_page(401, "")
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ),
            "39ec167960b713f43ad83f43228fdd0c68fd939b10590ecf8a0453a214932767"
        );
        assert_eq!(
            sha256_hex(
                login_page(401, "wrong key")
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ),
            "69c9b48730a2dfa72cbd1e1cbd1bb072de8f8d35881e43691f17083a37094bbe"
        );
        assert_eq!(
            sha256_hex(
                login_page(401, "link expired — paste your device key instead")
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ),
            "63360429ed7592528dcc8733799df59f408ce9c281ca0f1ceff13f4469f109a0"
        );
        // The session cookie wire string at exp = 1780000090.
        let redirect = session_redirect(SECRET, 1_780_000_090 - SESSION_TTL_SECS);
        assert_eq!(
            redirect["cookies"][0],
            json!(
                "qdb_sess=1780000090.171b2270b189ea49c7b193770a29e53060875890913811aa5d478ebc2506c77e; \
                 HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=43200"
            )
        );
        // The limit-clamp vectors (w3-oracle.json `clamp`).
        for (raw, expected) in [
            ("0,999999", "0,1000"),
            ("5000", "1000"),
            ("10", "10"),
            ("-5", "-5"),
            ("abc", "1000"),
            ("3,7", "3,7"),
            ("", "1000"),
        ] {
            assert_eq!(clamp_limit_param(raw), expected, "{raw:?}");
        }
    }
}
