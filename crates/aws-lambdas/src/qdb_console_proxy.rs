//! QuestDB console BACK Lambda (B4) — VPC-attached dumb HTTP relay, ZERO
//! secrets. Rust port of `deploy/aws/lambda/questdb-console-proxy/handler.py`
//! (rust-only phase 2b-3, 2026-07-18). Behavior parity is the contract; the
//! Python source is the oracle and every deviation is documented inline.
//!
//! Invoked ONLY by the front Lambda (qdb-console-front) via
//! lambda:InvokeFunction with a JSON envelope:
//!   `{"method", "path", "rawQuery", "headers": {accept, content-type, ...}}`
//! Relays the request to QuestDB on the box's PRIVATE IP inside the VPC
//! (env `QDB_BASE`, e.g. `http://<vpc-private-ip>:9000` — injected by
//! Terraform from `aws_instance.tv_app.private_ip`; NEVER hardcoded) and
//! returns `{"status", "headers": {content-type, content-encoding,
//! cache-control, location}, "body_b64"}` (`location` forwarded on BOTH the
//! success and error-status arms so the front can relay a body-less 3xx) or
//! `{"err": "box_unreachable" | "too_large" | "bad_path" | "denied" |
//! "denied_sql" | "upstream_timeout"}`.
//!
//! WHY THIS LAMBDA EXISTS + WHY IT IS SECRET-FREE: a VPC Lambda in this VPC
//! has NO internet/AWS-API path (single public subnet, no NAT, no VPC
//! endpoints), so it CANNOT read SSM at runtime. All auth/secret work lives
//! in the non-VPC front Lambda. This hop carries zero secrets and makes ZERO
//! AWS SDK calls at runtime — a plain HTTP client to the box. The box SG
//! opens TCP 9000 to THIS Lambda's SG only; nothing public.
//!
//! DEFENSE-IN-DEPTH (zero trust of the front): this handler re-runs the SAME
//! read-only SQL gate on /exec + /exp and re-applies the SAME GET/HEAD +
//! path whitelist. A compromised or buggy front cannot turn this relay into
//! a writer.
//!
//! B4 r3 shell-hang fix carry-over (2026-07-06, evidence: §9a/§10 of
//! docs/incidents/2026-07-06-questdb-console-shell-hang/repro-evidence.md):
//! QuestDB 9.3.5 answers GET / with an UNFRAMED keep-alive 301 (no
//! Content-Length, no Transfer-Encoding) and NEVER closes the socket — even
//! under request `Connection: close`. The Python fix pair is preserved
//! byte-for-byte in behavior:
//!   (1) GET / is rewritten to /index.html so the unframed 301 is never
//!       elicited (the framed shell answers in ~4ms), and
//!   (2) redirects are NEVER followed and a 3xx is relayed BODY-LESS
//!       (status + Location only; reading an unframed 3xx body would block
//!       until the socket timeout). In Python this was the
//!       `_NoFollowRedirect` opener; here the live client is built with
//!       `reqwest::redirect::Policy::none()` and [`handle_core`]'s 3xx arm
//!       structurally never touches the body reader (bomb-reader ratchet in
//!       the tests).

use base64::Engine as _;
use serde_json::{Map, Value, json};

/// Deployed-bytes proof marker (proof-3 ratchet: `build_marker_present`).
/// Byte-identical to the Python `QDB_CONSOLE_BUILD` and to the front's copy.
pub const QDB_CONSOLE_BUILD: &str = "b4-qdb-console-2026-07-06-r3";

/// Single-timeout tradeoff (Python FIX 8): urllib's `timeout` was the
/// SOCKET-op timeout (it bounded BOTH the connect attempt AND each blocking
/// recv). The Rust live client maps this 1:1 as `connect_timeout` +
/// `read_timeout` (per-read inactivity), deliberately with NO total request
/// timeout — a DRIBBLING body (>=1 byte per <12s recv) resets the per-recv
/// timer each time and is bounded by the back Lambda's 26s timeout instead
/// (questdb-console.tf), exactly the Python honest bound.
pub const TIMEOUT_SECS: u64 = 12;

/// Keep base64+JSON under Lambda's 6 MiB invoke envelope.
pub const MAX_BODY_BYTES: usize = 4_100_000;

/// Incremental read chunk size (Python `_READ_CHUNK`).
pub const READ_CHUNK: usize = 262_144;

// ------------------------------------------------------- read-only SQL gate
// BYTE-IDENTICAL to the qdb-console-front gate (zero trust of the front;
// parity is ratcheted by the tests). This is an INTENTIONAL read-only
// SUPERSET of operator-control's `_is_safe_sql`: the QuestDB 9.3.5 console
// SPA issues bare introspection functions (tables(), columns('t'), ...)
// which the select/show/explain/with first-word allowlist would reject;
// operator-control wraps its OWN queries in SELECT so it never needs them.

/// Allowed read-only first keywords (parity: `_SQL_ALLOWED_PREFIXES` in
/// operator-control/handler.py — asserted by the shared-subset parity test).
pub const SQL_ALLOWED_PREFIXES: [&str; 4] = ["select", "show", "explain", "with"];

/// Banned mutating keywords, whole-word, anywhere (parity: `_SQL_BANNED`).
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

pub const MAX_SQL_LEN: usize = 20_000;

/// Read-only introspection funcs the QuestDB 9.3.5 console calls bare
/// against /exec. The exact set is to be LIVE-VERIFIED against the deployed
/// console.
pub const SQL_ALLOWED_FUNCS: [&str; 12] = [
    "tables",
    "columns",
    "table_columns",
    "materialized_views",
    "wal_tables",
    "table_partitions",
    "functions",
    "hydrate_table_metadata",
    "memory_metrics",
    "reader_pool",
    "query_activity",
    "flush_query_cache",
];

/// Python `\w` word-character test (`re` str patterns are unicode-aware):
/// letters, digits, underscore.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whole-word occurrence scan — the Rust equivalent of
/// `re.search(r"\b" + kw + r"\b", q)` for an all-lowercase-ascii `kw`.
fn contains_word(haystack: &str, word: &str) -> bool {
    let mut start = 0;
    while let Some(idx) = haystack[start..].find(word) {
        let at = start + idx;
        let end = at + word.len();
        let before_ok = haystack[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c));
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !is_word_char(c));
        if before_ok && after_ok {
            return true;
        }
        // Advance one byte past the match start (kw is ascii; the haystack
        // slice at `at` starts an ascii char, so at+1 is a char boundary
        // only if the next byte is — step to the next char boundary safely.
        start = end;
    }
    false
}

/// First token per Python `re.split(r"[^a-z]+", q, maxsplit=1)[0]` — the
/// leading run of ascii `a-z` chars (empty when `q` starts with anything
/// else).
fn first_token(q: &str) -> &str {
    let end = q
        .char_indices()
        .find(|(_, c)| !c.is_ascii_lowercase())
        .map_or(q.len(), |(i, _)| i);
    &q[..end]
}

/// Bare-call match per Python `re.match(r"\s*([a-z_]+)\s*\(", q)` — returns
/// the function name when `q` opens with optional whitespace, a non-empty
/// `[a-z_]+` run, optional whitespace, then `(`.
fn leading_func_name(q: &str) -> Option<&str> {
    let after_ws = q.trim_start();
    let name_end = after_ws
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_lowercase() || *c == '_'))
        .map_or(after_ws.len(), |(i, _)| i);
    if name_end == 0 {
        return None;
    }
    let rest = after_ws[name_end..].trim_start();
    if rest.starts_with('(') {
        Some(&after_ws[..name_end])
    } else {
        None
    }
}

/// Read-only gate (hardened 2026-07-02; introspection superset 2026-07-03).
/// Rules, all fail-closed — Python `_is_safe_sql` parity:
/// 1. SINGLE STATEMENT: one trailing ';' is stripped; ANY remaining ';'
///    rejects (closes the "select 1; <unlisted mutator>" chaining gap).
/// 2. NO SQL COMMENTS ('--' or '/*'): keeps the banned-word scan honest —
///    comments could otherwise hide/split keywords.
/// 3. First TOKEN must be an allowed read-only keyword (select/show/explain/
///    with) OR a bare call to a read-only introspection function in
///    [`SQL_ALLOWED_FUNCS`] (e.g. `tables()`), so the QuestDB console's own
///    introspection works. "explainx ..." / "selector ..." are still
///    rejected.
/// 4. No mutating keyword (whole-word) anywhere — incl. QuestDB mutators
///    (BACKUP/CHECKPOINT/SNAPSHOT/...). False-positive rejects on string
///    literals are accepted (fail-closed).
///
/// Pure function — fully unit-tested.
pub fn is_safe_sql(query: &str) -> bool {
    let lowered = query.trim().to_lowercase();
    let mut q: &str = &lowered;
    if q.is_empty() {
        return false;
    }
    // Rule 1 — single statement: strip ONE trailing ';', reject any other.
    if let Some(stripped) = q.strip_suffix(';') {
        q = stripped.trim_end();
    }
    if q.is_empty() || q.contains(';') {
        return false;
    }
    // Rule 2 — no comments.
    if q.contains("--") || q.contains("/*") {
        return false;
    }
    // Rule 3 — allowed first token: keyword OR read-only introspection func.
    let first = first_token(q);
    let mut allowed_first = SQL_ALLOWED_PREFIXES.contains(&first);
    if !allowed_first
        && let Some(name) = leading_func_name(q)
        && SQL_ALLOWED_FUNCS.contains(&name)
    {
        allowed_first = true;
    }
    if !allowed_first {
        return false;
    }
    // Rule 4 — banned keywords anywhere (still applies to func calls).
    !SQL_BANNED.iter().any(|kw| contains_word(q, kw))
}

// --------------------------------------------------------------- path gating
// SAME whitelist + traversal defense as the front (parity ratcheted by
// tests).

pub const STATIC_EXTS: [&str; 10] = [
    ".html", ".js", ".css", ".svg", ".png", ".woff2", ".ico", ".map", ".json", ".txt",
];

/// The Python `_STATIC_EXTS` tuple carries an 11th entry `.webmanifest`;
/// keeping the array + tail split makes the const arity explicit.
pub const STATIC_EXT_WEBMANIFEST: &str = ".webmanifest";

fn has_static_ext(path_lower: &str) -> bool {
    STATIC_EXTS.iter().any(|e| path_lower.ends_with(e))
        || path_lower.ends_with(STATIC_EXT_WEBMANIFEST)
}

/// Python `urllib.parse.unquote` parity for the traversal check: decode
/// every valid `%XX` hex pair to its byte; leave invalid sequences literal;
/// decode the byte string as UTF-8 with replacement (errors='replace').
fn percent_unquote(raw: &str) -> String {
    percent_encoding::percent_decode_str(raw)
        .decode_utf8_lossy()
        .into_owned()
}

/// Path-confusion / traversal reject (zero-trust re-check of the front).
/// True = reject. Checks the raw path AND its URL-decoded form for `..`,
/// `//`, `\`, encoded separators (%2e/%2f/%5c, any case) and control chars.
pub fn bad_path(raw_path: &str) -> bool {
    let low = raw_path.to_lowercase();
    if low.contains("%2e") || low.contains("%2f") || low.contains("%5c") {
        return true;
    }
    let decoded = percent_unquote(raw_path);
    for f in [raw_path, decoded.as_str()] {
        if f.contains("..") || f.contains("//") || f.contains('\\') {
            return true;
        }
        if f.chars().any(|ch| (ch as u32) < 0x20) {
            return true;
        }
    }
    false
}

/// `_classify_path` verdicts, with the exact Python string labels for the
/// parity ratchets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Deny,
    Method,
    Sql,
    Static,
}

impl PathKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PathKind::Deny => "deny",
            PathKind::Method => "method",
            PathKind::Sql => "sql",
            PathKind::Static => "static",
        }
    }
}

/// Python `_classify_path` parity (`path or "/"` then `.rstrip()`).
pub fn classify_path(method: &str, path: &str) -> PathKind {
    let base = if path.is_empty() { "/" } else { path };
    let p = base.trim_end();
    let allowed_sql = p == "/exec" || p == "/exp";
    let allowed_static = matches!(p, "/" | "/index.html" | "/chk" | "/settings")
        || p.starts_with("/assets/")
        || has_static_ext(&p.to_lowercase());
    if !(allowed_sql || allowed_static) {
        return PathKind::Deny;
    }
    if method != "GET" && method != "HEAD" {
        return PathKind::Method;
    }
    if allowed_sql {
        PathKind::Sql
    } else {
        PathKind::Static
    }
}

/// Python `parse_qs(raw_query).get("query", [""])[0]` parity: '&'-separated
/// pairs, '+' as space, percent-decoded UTF-8 with replacement, BLANK values
/// dropped (parse_qs default `keep_blank_values=False`), first survivor
/// wins.
fn query_param(raw_query: &str, name: &str) -> String {
    for (k, v) in form_urlencoded::parse(raw_query.as_bytes()) {
        if k == name && !v.is_empty() {
            return v.into_owned();
        }
    }
    String::new()
}

/// Returns `None` when the request may be relayed, else the typed err code
/// (Python `_gate` returned `""` / the code).
pub fn gate(method: &str, path: &str, raw_query: &str) -> Option<&'static str> {
    match classify_path(method, path) {
        PathKind::Deny | PathKind::Method => Some("denied"),
        PathKind::Sql => {
            let q = query_param(raw_query, "query");
            // Python `len(q)` counts CHARS, not bytes.
            if q.is_empty() || q.chars().count() > MAX_SQL_LEN || !is_safe_sql(&q) {
                Some("denied_sql")
            } else {
                None
            }
        }
        PathKind::Static => None,
    }
}

// ------------------------------------------------------------- upstream seam
// The Python handler called `urllib.request.urlopen` directly and the tests
// mock-patched it. The Rust port injects the upstream through this seam so
// the pure core stays fully unit-testable; [`ReqwestUpstream`] is the live
// implementation the bin wires in.

/// One relayed request. `timeout_secs` mirrors the Python
/// `urlopen(req, timeout=_TIMEOUT_SECS)` argument so the fakes can pin it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayRequest {
    pub method: String,
    pub url: String,
    /// Lowercase header names, insertion order. HTTP header names are
    /// case-insensitive on the wire (urllib title-cased them; reqwest
    /// lowercases them — same bytes semantics).
    pub headers: Vec<(String, String)>,
    pub timeout_secs: u64,
}

impl RelayRequest {
    /// Case-insensitive header lookup for tests.
    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_ascii_lowercase() == want)
            .map(|(_, v)| v.as_str())
    }
}

/// Connect/transport failure classification — the Python
/// `except (TimeoutError, socket.timeout)` vs `except URLError/Exception`
/// split (a URLError WRAPPING a socket timeout is still a timeout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    Timeout,
    Unreachable,
}

/// Body-read failure classification. `Timeout` is the unframed-body
/// per-recv timeout (Python fixer round 2 → `upstream_timeout`); `Other`
/// collapses the Python fixer-round-3 class (ConnectionResetError /
/// IncompleteRead / ConnectionAbortedError — peer RST mid body) →
/// `box_unreachable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyReadError {
    Timeout,
    Other,
}

/// Chunked body reader — Python's `fp.read(_READ_CHUNK)` semantics: an
/// empty chunk means EOF.
pub trait BodyRead {
    fn read_chunk(&mut self, max_len: usize) -> Result<Vec<u8>, BodyReadError>;
}

/// The upstream response as the relay sees it — status + lowercase headers
/// + a lazily-read body (the 3xx arm must be able to relay WITHOUT reading).
pub struct UpstreamResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Box<dyn BodyRead>,
}

impl UpstreamResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The urlopen seam. In Python every 3xx ALSO surfaced through this call
/// (as an HTTPError with an unread body, courtesy of `_NoFollowRedirect`);
/// here the live impl disables redirects so a 3xx arrives as a plain
/// response and [`handle_core`] relays it body-less.
pub trait Upstream {
    fn fetch(&mut self, req: &RelayRequest) -> Result<UpstreamResponse, FetchError>;
}

/// Incremental read bounded at [`MAX_BODY_BYTES`]. Returns `(body, over)`
/// where `over=true` means the cap was exceeded (body is truncated to the
/// cap). Used on BOTH the success and the error-status path so neither
/// reads-then-slices (Python `_read_capped`).
pub fn read_capped(fp: &mut dyn BodyRead) -> Result<(Vec<u8>, bool), BodyReadError> {
    let mut out: Vec<u8> = Vec::new();
    let mut total = 0usize;
    loop {
        let chunk = fp.read_chunk(READ_CHUNK)?;
        if chunk.is_empty() {
            return Ok((out, false));
        }
        let room = MAX_BODY_BYTES - total;
        if chunk.len() >= room {
            out.extend_from_slice(&chunk[..room]);
            return Ok((out, true));
        }
        total += chunk.len();
        out.extend_from_slice(&chunk);
    }
}

// ------------------------------------------------------------------- handler

fn err_json(code: &str) -> Value {
    json!({ "err": code })
}

/// Python `str(event.get(key, default))` parity for the envelope fields.
/// Deliberate documented deviation: a JSON `null` renders as the default
/// (Python rendered `str(None) == "None"`); non-string scalars render via
/// their JSON text (Python `str(5) == "5"` — same for numbers/bools).
fn event_str(event: &Value, key: &str, default: &str) -> String {
    match event.get(key) {
        None | Some(Value::Null) => default.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Python `str(event.get("rawQuery", "") or "")` — any falsy value (null,
/// "", 0, false) collapses to "".
fn event_raw_query(event: &Value) -> String {
    match event.get("rawQuery") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "true".to_string(),
        Some(Value::Number(n)) if n.as_f64() != Some(0.0) => n.to_string(),
        _ => String::new(),
    }
}

fn relay_out_headers(resp: &UpstreamResponse) -> Map<String, Value> {
    let mut out = Map::new();
    for k in [
        "content-type",
        "content-encoding",
        "cache-control",
        "location",
    ] {
        if let Some(v) = resp.header(k)
            && !v.is_empty()
        {
            out.insert(k.to_string(), Value::String(v.to_string()));
        }
    }
    out
}

/// The full handler core — Python `lambda_handler` parity, with the
/// upstream injected. `qdb_base` is the `QDB_BASE` env value (read by the
/// bin per invoke; Lambda env is static per sandbox, so this matches the
/// Python module-import read).
pub fn handle_core(event: &Value, qdb_base: &str, upstream: &mut dyn Upstream) -> Value {
    let method = event_str(event, "method", "GET").to_uppercase();
    let mut path = event_str(event, "path", "/");
    let raw_query = event_raw_query(event);
    let empty = Map::new();
    let in_headers = event
        .get("headers")
        .and_then(Value::as_object)
        .unwrap_or(&empty);

    if qdb_base.is_empty() {
        return err_json("box_unreachable");
    }

    // Zero-trust path-confusion reject BEFORE classification (the front
    // already rejects these, but the VPC hop trusts nothing).
    if bad_path(&path) {
        return err_json("bad_path");
    }

    if let Some(denied) = gate(&method, &path, &raw_query) {
        return err_json(denied);
    }

    // B4 r3 shell-load fix: QuestDB 9.3.5 does NOT serve the console shell
    // at "/" — it answers an UNFRAMED keep-alive 301 -> /index.html and
    // never closes the socket, so any read-until-EOF relay hangs until its
    // socket timeout. Never elicit the 301: fetch the framed shell
    // directly. GET-only on purpose: HEAD / keeps relaying verbatim
    // (QuestDB answers a FRAMED chunked 405 in 1.2ms; HEAD /index.html is
    // live-unverified — don't change untested behavior). The browser URL
    // stays "/"; the shell's RELATIVE asset refs resolve identically.
    if method == "GET" && path == "/" {
        path = "/index.html".to_string();
    }

    let mut url = format!("{}{}", qdb_base.trim_end_matches('/'), path);
    if !raw_query.is_empty() {
        url.push('?');
        url.push_str(&raw_query);
    }

    // Forward only `accept` + `content-type` from the browser.
    // `accept-encoding` is deliberately NOT forwarded: the retained-hygiene
    // pair below keeps the OBSERVED static paths Content-Length'd /
    // uncompressed (repro-evidence.md §6b); /exec stays chunked (§1)
    // regardless. `Connection: close` is retained as defense-in-depth ONLY
    // — the raw-socket proof showed QuestDB 9.3.5 does NOT honor it on the
    // / 301 (repro-evidence.md §3/§10); the actual fixes are the rewrite
    // above + the never-follow body-less 3xx relay below.
    let mut headers: Vec<(String, String)> = Vec::new();
    for k in ["accept", "content-type"] {
        if let Some(v) = in_headers.get(k) {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Null | Value::Bool(false) => String::new(),
                other => other.to_string(),
            };
            if !s.is_empty() {
                headers.push((k.to_string(), s));
            }
        }
    }
    headers.push(("accept-encoding".to_string(), "identity".to_string()));
    headers.push(("connection".to_string(), "close".to_string()));

    let relay_req = RelayRequest {
        method: method.clone(),
        url,
        headers,
        timeout_secs: TIMEOUT_SECS,
    };

    let mut resp = match upstream.fetch(&relay_req) {
        Ok(r) => r,
        // Diagnostic honesty (B4 r2): a connect/read timeout means the box
        // was REACHABLE but slow — NOT a refused connection. Distinct code
        // so the front returns 504, reserving the offline-503 for a genuine
        // refusal (URLError-wrapping-timeout maps here too — see the live
        // classifier).
        Err(FetchError::Timeout) => return err_json("upstream_timeout"),
        Err(FetchError::Unreachable) => return err_json("box_unreachable"),
    };

    let out_headers = relay_out_headers(&resp);
    let status = resp.status;

    // 3xx is special: with redirects disabled it arrives here un-drained,
    // and it may be DELIMITER-LESS on a socket QuestDB never closes (the /
    // 301 class) — reading it would block until the socket timeout. Relay
    // 3xx body-less (status + Location); the front forwards Location and
    // the browser follows.
    if (300..400).contains(&status) {
        return json!({ "status": status, "headers": out_headers, "body_b64": "" });
    }

    // Body-read error mapping is IDENTICAL on the success (<300) and the
    // error-status (>=400) arm in the Python oracle: a per-recv timeout on
    // an unframed body -> upstream_timeout (fixer round 2); any other read
    // failure (peer RST mid body / IncompleteRead / aborted) ->
    // box_unreachable (fixer round 3 + the success path's blanket arm).
    let (body, over) = match read_capped(resp.body.as_mut()) {
        Ok(pair) => pair,
        Err(BodyReadError::Timeout) => return err_json("upstream_timeout"),
        Err(BodyReadError::Other) => return err_json("box_unreachable"),
    };

    // QuestDB error responses (e.g. /exec 400 with JSON body) are VALID
    // console traffic — relay them. Only the success arm enforces the
    // too_large verdict (Python parity: the HTTPError arm ignored `over`
    // and relayed the capped body).
    if status < 300 && over {
        return err_json("too_large"); // → front 502, never 503
    }

    json!({
        "status": status,
        "headers": out_headers,
        "body_b64": base64::engine::general_purpose::STANDARD.encode(&body),
    })
}

// ------------------------------------------------------------ live upstream

/// The live relay client. Redirect policy is NONE (the `_NoFollowRedirect`
/// port — a 3xx must surface to [`handle_core`] un-followed and un-read),
/// with the Python single-timeout semantics mapped to
/// `connect_timeout` + the blocking client's op-level `timeout` (reqwest
/// 0.13's blocking builder exposes no per-recv `read_timeout`; its
/// `timeout` covers connect/read/write ops — a deliberate, documented
/// deviation that is strictly TIGHTER than Python's per-socket-op
/// timeout, never looser).
pub struct ReqwestUpstream {
    client: reqwest::blocking::Client,
}

impl ReqwestUpstream {
    pub fn new() -> Result<Self, reqwest::Error> {
        Self::with_timeouts(
            std::time::Duration::from_secs(TIMEOUT_SECS),
            std::time::Duration::from_secs(TIMEOUT_SECS),
        )
    }

    /// Timeout-injectable constructor so the live tests can exercise the
    /// timeout classification in milliseconds instead of 12s.
    pub fn with_timeouts(
        connect: std::time::Duration,
        read: std::time::Duration,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::blocking::Client::builder()
            // The `_NoFollowRedirect` port: never follow ANY 3xx (QuestDB's
            // / 301 is unframed on a never-closed socket; following would
            // hang draining it).
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect)
            // Op-level timeout (see the struct doc): reqwest 0.13's blocking
            // builder has no `read_timeout`; `timeout` bounds connect/read/
            // write operations — a tighter-than-Python envelope.
            .timeout(read)
            .build()?;
        Ok(Self { client })
    }
}

/// A reqwest transport error is a timeout when any layer of its source
/// chain says so (the Python `URLError(reason=socket.timeout)` unwrap).
fn classify_transport_error(e: &reqwest::Error) -> FetchError {
    if e.is_timeout() {
        return FetchError::Timeout;
    }
    // Walk the source chain for a wrapped io timeout (URLError parity).
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(e);
    while let Some(s) = src {
        if let Some(io) = s.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            )
        {
            return FetchError::Timeout;
        }
        src = s.source();
    }
    FetchError::Unreachable
}

struct BlockingBody {
    resp: reqwest::blocking::Response,
}

impl BodyRead for BlockingBody {
    fn read_chunk(&mut self, max_len: usize) -> Result<Vec<u8>, BodyReadError> {
        use std::io::Read as _;
        let mut buf = vec![0u8; max_len];
        let mut filled = 0usize;
        // io::Read may return short reads; loop until EOF or the chunk is
        // full so the capped reader sees Python-`fp.read(n)`-like chunks.
        while filled < buf.len() {
            match self.resp.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Err(BodyReadError::Timeout);
                }
                Err(_) => return Err(BodyReadError::Other),
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }
}

impl Upstream for ReqwestUpstream {
    fn fetch(&mut self, req: &RelayRequest) -> Result<UpstreamResponse, FetchError> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|_| FetchError::Unreachable)?;
        let mut builder = self.client.request(method, &req.url);
        for (k, v) in &req.headers {
            builder = builder.header(k, v);
        }
        let resp = builder.send().map_err(|e| classify_transport_error(&e))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        Ok(UpstreamResponse {
            status,
            headers,
            body: Box::new(BlockingBody { resp }),
        })
    }
}

/// Live entrypoint for the bin: read `QDB_BASE`, run the core on a blocking
/// thread (the blocking reqwest client must not run on the async runtime
/// worker). A join failure (panic) propagates as a Lambda FunctionError —
/// the same escape semantics an unhandled Python exception had.
pub async fn handle(event: Value) -> Result<Value, lambda_runtime::Error> {
    let qdb_base = std::env::var("QDB_BASE").unwrap_or_default();
    let out = tokio::task::spawn_blocking(move || {
        let mut upstream = match ReqwestUpstream::new() {
            Ok(u) => u,
            // No Python analog (urllib had no client-construction step);
            // fail-closed to the unreachable-class typed err.
            Err(_) => return err_json("box_unreachable"),
        };
        handle_core(&event, &qdb_base, &mut upstream)
    })
    .await?;
    Ok(out)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    const FAKE_BASE: &str = "http://10.42.1.99:9000";

    // ---- fakes (the Python FakeResp / mock.patch("urlopen") equivalents)

    struct ChunkBody {
        chunks: Vec<Vec<u8>>,
    }

    impl BodyRead for ChunkBody {
        fn read_chunk(&mut self, _max: usize) -> Result<Vec<u8>, BodyReadError> {
            if self.chunks.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(self.chunks.remove(0))
            }
        }
    }

    struct FailBody {
        err: BodyReadError,
    }

    impl BodyRead for FailBody {
        fn read_chunk(&mut self, _max: usize) -> Result<Vec<u8>, BodyReadError> {
            Err(self.err)
        }
    }

    /// The bomb-fp port: reading the body at all fails the test.
    struct BombBody;

    impl BodyRead for BombBody {
        fn read_chunk(&mut self, _max: usize) -> Result<Vec<u8>, BodyReadError> {
            panic!("3xx body must NEVER be read (unframed; socket never closes)");
        }
    }

    enum FakeReply {
        Response {
            status: u16,
            headers: Vec<(&'static str, &'static str)>,
            body: Box<dyn BodyRead>,
        },
        Fetch(FetchError),
    }

    /// Captures every RelayRequest (the `up.call_args` / `assert_not_called`
    /// equivalents) and pops queued replies.
    struct FakeUpstream {
        calls: Rc<RefCell<Vec<RelayRequest>>>,
        replies: Vec<FakeReply>,
    }

    impl FakeUpstream {
        fn new(replies: Vec<FakeReply>) -> Self {
            Self {
                calls: Rc::new(RefCell::new(Vec::new())),
                replies,
            }
        }

        fn never_called() -> Self {
            Self::new(Vec::new())
        }
    }

    impl Upstream for FakeUpstream {
        fn fetch(&mut self, req: &RelayRequest) -> Result<UpstreamResponse, FetchError> {
            self.calls.borrow_mut().push(req.clone());
            assert!(
                !self.replies.is_empty(),
                "upstream fetch was called but the test queued no reply (assert_not_called parity)"
            );
            match self.replies.remove(0) {
                FakeReply::Fetch(e) => Err(e),
                FakeReply::Response {
                    status,
                    headers,
                    body,
                } => Ok(UpstreamResponse {
                    status,
                    headers: headers
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    body,
                }),
            }
        }
    }

    fn ok_resp(
        chunks: Vec<&[u8]>,
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
    ) -> FakeReply {
        FakeReply::Response {
            status,
            headers,
            body: Box::new(ChunkBody {
                chunks: chunks.into_iter().map(<[u8]>::to_vec).collect(),
            }),
        }
    }

    fn html_resp(body: &[u8]) -> FakeReply {
        ok_resp(vec![body], 200, vec![("content-type", "text/html")])
    }

    fn event(method: &str, path: &str, raw_query: &str) -> Value {
        json!({ "method": method, "path": path, "rawQuery": raw_query })
    }

    fn b64d(v: &Value) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(v["body_b64"].as_str().unwrap())
            .unwrap()
    }

    // ------------------------------------------------------------- ReGate

    #[test]
    fn regate_rejects_mutator_sql() {
        // Zero trust of the front: even if the front is bypassed, the
        // mutator dies HERE, before any socket is opened.
        let mut up = FakeUpstream::never_called();
        let r = handle_core(
            &event("GET", "/exec", "query=drop+table+ticks"),
            FAKE_BASE,
            &mut up,
        );
        assert_eq!(r, json!({"err": "denied_sql"}));
        assert!(up.calls.borrow().is_empty());
    }

    #[test]
    fn regate_rejects_chained_statement() {
        let mut up = FakeUpstream::never_called();
        let r = handle_core(
            &event("GET", "/exp", "query=select+1%3B+drop+table+ticks"),
            FAKE_BASE,
            &mut up,
        );
        assert_eq!(r, json!({"err": "denied_sql"}));
        assert!(up.calls.borrow().is_empty());
    }

    #[test]
    fn regate_rejects_missing_and_overlong_query() {
        let mut up = FakeUpstream::never_called();
        assert_eq!(
            handle_core(&event("GET", "/exec", ""), FAKE_BASE, &mut up),
            json!({"err": "denied_sql"})
        );
        let long_q = format!("query={}", "a".repeat(MAX_SQL_LEN + 1));
        assert_eq!(
            handle_core(&event("GET", "/exec", &long_q), FAKE_BASE, &mut up),
            json!({"err": "denied_sql"})
        );
        assert!(up.calls.borrow().is_empty());
    }

    #[test]
    fn path_whitelist_parity() {
        // Python compared against the front's `_classify_path` (loaded from
        // the sibling handler file). The expected verdicts below are the
        // EXECUTED Python front verdicts for the same 16 cases; the front
        // Rust module lands in the sibling commit and this table pins the
        // shared contract byte-for-byte.
        let cases: [(&str, &str, &str); 16] = [
            ("GET", "/exec", "sql"),
            ("GET", "/exp", "sql"),
            ("GET", "/", "static"),
            ("GET", "/index.html", "static"),
            ("GET", "/chk", "static"),
            ("GET", "/settings", "static"),
            ("GET", "/assets/app.js", "static"),
            ("GET", "/favicon.ico", "static"),
            ("GET", "/imp", "deny"),
            ("GET", "/api/x", "deny"),
            ("POST", "/exec", "method"),
            ("PUT", "/settings", "method"),
            ("POST", "/imp", "deny"),
            ("HEAD", "/", "static"),
            ("DELETE", "/exp", "method"),
            ("GET", "/weird/path", "deny"),
        ];
        for (method, path, want) in cases {
            assert_eq!(
                classify_path(method, path).as_str(),
                want,
                "whitelist parity broken for {method} {path}"
            );
        }
    }

    #[test]
    fn denied_paths_and_methods() {
        let mut up = FakeUpstream::never_called();
        assert_eq!(
            handle_core(
                &json!({"method": "GET", "path": "/imp"}),
                FAKE_BASE,
                &mut up
            ),
            json!({"err": "denied"})
        );
        assert_eq!(
            handle_core(
                &event("POST", "/exec", "query=select+1"),
                FAKE_BASE,
                &mut up
            ),
            json!({"err": "denied"})
        );
        assert!(up.calls.borrow().is_empty());
    }

    #[test]
    fn sql_gate_parity_with_operator_control_shared_subset() {
        // The back gate is a read-only SUPERSET of operator-control (adds
        // bare introspection funcs); on the NON-func corpus they must agree.
        // The Python test loaded operator-control/handler.py live; the
        // expected verdicts below are the EXECUTED operator-control
        // `_is_safe_sql` verdicts for the shared corpus, and the tuple pins
        // mirror `opctl._SQL_ALLOWED_PREFIXES` / `opctl._SQL_BANNED`
        // byte-for-byte (operator-control/handler.py — the Rust port of
        // that lambda re-pins the same literals on its side).
        assert_eq!(SQL_ALLOWED_PREFIXES, ["select", "show", "explain", "with"]);
        assert_eq!(
            SQL_BANNED,
            [
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
            ]
        );
        for (q, want) in [
            ("select 1; drop table ticks", false),
            ("select 1 -- x", false),
            ("select /* x */ 1", false),
            ("backup table ticks", false),
            ("select backup from t", false),
            ("WITH x AS (SELECT 1) SELECT * FROM x", true),
            ("show tables", true),
            ("select 1;", true),
            ("", false),
        ] {
            assert_eq!(is_safe_sql(q), want, "{q:?}");
        }
    }

    #[test]
    fn introspection_funcs_allowed_and_rechecked() {
        // FIX 3: the console's bare introspection funcs pass; unknown funcs
        // + chaining + banned-word-after still reject at the VPC hop.
        for fn_name in SQL_ALLOWED_FUNCS {
            assert!(is_safe_sql(&format!("{fn_name}()")), "{fn_name}");
        }
        assert!(is_safe_sql("columns('ticks')"));
        assert!(!is_safe_sql("evil_func()"));
        assert!(!is_safe_sql("tables(); drop table ticks"));
        assert!(!is_safe_sql("tables() delete"));
        assert!(!is_safe_sql("insert into t values(1)"));
    }

    #[test]
    fn introspection_func_relays_through_gate() {
        let mut up = FakeUpstream::new(vec![ok_resp(
            vec![b"[]"],
            200,
            vec![("content-type", "application/json")],
        )]);
        let r = handle_core(
            &event("GET", "/exec", "query=tables%28%29"),
            FAKE_BASE,
            &mut up,
        );
        assert_eq!(r["status"], 200);
        assert_eq!(up.calls.borrow().len(), 1);
    }

    #[test]
    fn bad_path_rejected_at_vpc_hop() {
        // FIX 1 zero-trust: even if the front is bypassed, traversal dies
        // here.
        let mut up = FakeUpstream::never_called();
        for path in [
            "/assets/../exec",
            "/exec/../imp",
            "/%2e%2e%2fexec",
            "/a//b",
            "/x\\y",
        ] {
            let r = handle_core(
                &event("GET", path, "query=drop+table+ticks"),
                FAKE_BASE,
                &mut up,
            );
            assert_eq!(r, json!({"err": "bad_path"}), "{path}");
        }
        assert!(up.calls.borrow().is_empty());
    }

    #[test]
    fn body_cap_constant_within_6mib_envelope() {
        assert!(MAX_BODY_BYTES <= 4_100_000);
    }

    // -------------------------------------------------------------- Relay

    #[test]
    fn happy_path_relays_status_headers_body() {
        let mut up = FakeUpstream::new(vec![ok_resp(
            vec![b"hello ", b"world"],
            200,
            vec![("content-type", "text/csv"), ("content-encoding", "gzip")],
        )]);
        let ev = json!({
            "method": "GET", "path": "/exp", "rawQuery": "query=show+tables",
            "headers": {"accept": "text/csv"},
        });
        let r = handle_core(&ev, FAKE_BASE, &mut up);
        assert_eq!(r["status"], 200);
        assert_eq!(b64d(&r), b"hello world");
        assert_eq!(r["headers"]["content-type"], "text/csv");
        assert_eq!(r["headers"]["content-encoding"], "gzip");
        let calls = up.calls.borrow();
        let req = &calls[0];
        assert!(req.url.starts_with("http://10.42.1.99:9000/exp?"));
        // Python asserted `urlopen(..., timeout=_TIMEOUT_SECS)`.
        assert_eq!(req.timeout_secs, TIMEOUT_SECS);
    }

    #[test]
    fn shell_get_forces_identity_and_connection_close() {
        // B4 r3 (honest wording — the r2 premise is DISPROVEN): these two
        // headers are RETAINED hygiene, NOT the shell fix. The 2026-07-06
        // raw-socket probe proved QuestDB 9.3.5 IGNORES request
        // `Connection: close` on the / 301 and never closes the socket. The
        // actual fix is the GET / -> /index.html rewrite (asserted below) +
        // the never-follow body-less 3xx relay.
        let mut up = FakeUpstream::new(vec![html_resp(b"<!doctype html><html></html>")]);
        let ev = json!({
            "method": "GET", "path": "/",
            "headers": {"accept": "text/html", "accept-encoding": "gzip, br"},
        });
        let r = handle_core(&ev, FAKE_BASE, &mut up);
        assert_eq!(r["status"], 200);
        let calls = up.calls.borrow();
        let req = &calls[0];
        assert_eq!(req.header("accept-encoding"), Some("identity"));
        assert_eq!(req.header("connection"), Some("close"));
        // browser accept still relayed
        assert_eq!(req.header("accept"), Some("text/html"));
        // B4 r3: the shell request must target the framed /index.html.
        assert_eq!(req.url, "http://10.42.1.99:9000/index.html");
    }

    #[test]
    fn root_get_is_rewritten_to_index_html() {
        // B4 r3 L1: GET / never reaches QuestDB — the back fetches the
        // framed shell directly (the / 301 is unframed + never-closed →
        // 12s hang).
        let mut up = FakeUpstream::new(vec![html_resp(b"<!DOCTYPE html>\n<html>")]);
        let ev = json!({"method": "GET", "path": "/", "headers": {"accept": "text/html"}});
        let r = handle_core(&ev, FAKE_BASE, &mut up);
        assert_eq!(r["status"], 200);
        assert_eq!(
            up.calls.borrow()[0].url,
            "http://10.42.1.99:9000/index.html"
        );
    }

    #[test]
    fn root_rewrite_preserves_whitelisted_query() {
        // The front's whitelisted static params must survive the rewrite.
        let mut up = FakeUpstream::new(vec![html_resp(b"<!DOCTYPE html>")]);
        let r = handle_core(&event("GET", "/", "v=1"), FAKE_BASE, &mut up);
        assert_eq!(r["status"], 200);
        assert_eq!(
            up.calls.borrow()[0].url,
            "http://10.42.1.99:9000/index.html?v=1"
        );
    }

    #[test]
    fn head_root_not_rewritten() {
        // Scope pin (repro §8): HEAD / is a fast FRAMED chunked 405 from
        // QuestDB — deliberately unchanged (HEAD /index.html is
        // live-unverified). The rewrite is GET-gated.
        let mut up = FakeUpstream::new(vec![ok_resp(
            vec![b""],
            405,
            vec![("content-type", "text/plain")],
        )]);
        let _ = handle_core(&json!({"method": "HEAD", "path": "/"}), FAKE_BASE, &mut up);
        assert_eq!(up.calls.borrow()[0].url, "http://10.42.1.99:9000/");
    }

    #[test]
    fn non_root_paths_not_rewritten() {
        // Guards an over-eager rewrite: ONLY exactly GET "/" is rewritten.
        for p in ["/index.html", "/chk", "/settings", "/assets/app.js"] {
            let mut up = FakeUpstream::new(vec![html_resp(b"x")]);
            let _ = handle_core(&json!({"method": "GET", "path": p}), FAKE_BASE, &mut up);
            let url = up.calls.borrow()[0].url.clone();
            assert!(url.ends_with(p), "{p} was unexpectedly rewritten to {url}");
        }
        let mut up = FakeUpstream::new(vec![ok_resp(
            vec![b"[]"],
            200,
            vec![("content-type", "application/json")],
        )]);
        let _ = handle_core(&event("GET", "/exec", "query=select+1"), FAKE_BASE, &mut up);
        assert!(
            up.calls.borrow()[0]
                .url
                .starts_with("http://10.42.1.99:9000/exec?")
        );
    }

    #[test]
    fn root_rewrite_kills_the_unframed_301_timeout_class() {
        // Behavioral proof of the fix: a fake box that HANGS (fetch
        // timeout, exactly what the unframed keep-alive 301 read-until-EOF
        // produced) for every path EXCEPT /index.html. On r2 bytes GET /
        // returned {"err": "upstream_timeout"} (the prod 504); on r3 it
        // returns the shell with status 200 because the 301 is never
        // elicited.
        struct RewriteProbe;
        impl Upstream for RewriteProbe {
            fn fetch(&mut self, req: &RelayRequest) -> Result<UpstreamResponse, FetchError> {
                if req.url.trim_end_matches('?').ends_with("/index.html") {
                    Ok(UpstreamResponse {
                        status: 200,
                        headers: vec![("content-type".to_string(), "text/html".to_string())],
                        body: Box::new(ChunkBody {
                            chunks: vec![b"<!DOCTYPE html>".to_vec()],
                        }),
                    })
                } else {
                    Err(FetchError::Timeout)
                }
            }
        }
        let r = handle_core(
            &json!({"method": "GET", "path": "/"}),
            FAKE_BASE,
            &mut RewriteProbe,
        );
        assert_eq!(r["status"], 200);
        let body = b64d(&r);
        assert!(
            body.windows(b"<!DOCTYPE html>".len())
                .any(|w| w == b"<!DOCTYPE html>")
        );
    }

    #[test]
    fn redirect_relayed_bodyless_never_reads_body() {
        // B4 r3 L2 (non-vacuous hang guard): a 3xx body may be
        // DELIMITER-LESS on a socket QuestDB never closes — reading it
        // blocks until the 12s timeout. The bomb body proves the 3xx arm
        // NEVER touches the body. /settings is NOT rewritten, so this
        // exercises the 3xx relay arm.
        let mut up = FakeUpstream::new(vec![FakeReply::Response {
            status: 301,
            headers: vec![("location", "/index.html")],
            body: Box::new(BombBody),
        }]);
        let r = handle_core(
            &json!({"method": "GET", "path": "/settings"}),
            FAKE_BASE,
            &mut up,
        );
        assert_eq!(
            r,
            json!({"status": 301, "headers": {"location": "/index.html"}, "body_b64": ""})
        );
    }

    #[test]
    fn no_follow_redirect_client_used_live() {
        // The belt layer, ported behaviorally (Python asserted the LIVE
        // installed urllib opener object, not source text — a source grep
        // stays green with the install call moved into dead code). Here a
        // REAL loopback server answers a framed 301; the LIVE
        // ReqwestUpstream must surface it as status 301 WITHOUT following
        // (a follow would produce a second connection — counted).
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_srv = hits.clone();
        let srv = std::thread::spawn(move || {
            // Serve at most 2 connections so a buggy follow is observable.
            for _ in 0..2 {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                hits_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(
                    b"HTTP/1.1 301 Moved Permanently\r\nLocation: /index.html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        let mut up = ReqwestUpstream::new().unwrap();
        let resp = up
            .fetch(&RelayRequest {
                method: "GET".to_string(),
                url: format!("http://{addr}/settings"),
                headers: vec![],
                timeout_secs: TIMEOUT_SECS,
            })
            .unwrap();
        assert_eq!(resp.status, 301, "3xx must surface un-followed");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "redirect was FOLLOWED — the no-follow policy is inert"
        );
        drop(resp);
        drop(up);
        // Unblock the server thread's second accept.
        let _ = std::net::TcpStream::connect(addr);
        let _ = srv.join();
    }

    #[test]
    fn disproven_r2_close_claim_removed() {
        // Source ratchet: the factually-wrong r2 claim (that QuestDB
        // "closes the socket <AFTER> the body") can never return — the
        // 2026-07-06 raw-socket probe disproved it verbatim. The needle is
        // assembled at runtime so this test's own literal never matches.
        let src = include_str!("qdb_console_proxy.rs");
        let needle = format!("closes the socket {} the body", "AFTER");
        assert!(!src.contains(&needle));
    }

    #[test]
    fn size_cap_returns_too_large() {
        let chunk = vec![b'x'; READ_CHUNK];
        let n = MAX_BODY_BYTES / chunk.len() + 2;
        let mut up = FakeUpstream::new(vec![FakeReply::Response {
            status: 200,
            headers: vec![("content-type", "text/html")],
            body: Box::new(ChunkBody {
                chunks: vec![chunk; n],
            }),
        }]);
        let r = handle_core(
            &json!({"method": "GET", "path": "/index.html"}),
            FAKE_BASE,
            &mut up,
        );
        assert_eq!(r, json!({"err": "too_large"}));
    }

    #[test]
    fn urlerror_refused_maps_to_box_unreachable() {
        // A genuine connection refusal / reset stays the offline-503 path.
        let mut up = FakeUpstream::new(vec![FakeReply::Fetch(FetchError::Unreachable)]);
        let r = handle_core(&json!({"method": "GET", "path": "/"}), FAKE_BASE, &mut up);
        assert_eq!(r, json!({"err": "box_unreachable"}));
    }

    #[test]
    fn socket_timeout_maps_to_upstream_timeout() {
        // B4 r2 diagnostic honesty: a read/connect timeout is a
        // REACHABLE-but-slow box, NOT a refused connection — distinct code
        // so the front returns 504, never a false 503 "offline".
        let mut up = FakeUpstream::new(vec![FakeReply::Fetch(FetchError::Timeout)]);
        let r = handle_core(&json!({"method": "GET", "path": "/"}), FAKE_BASE, &mut up);
        assert_eq!(r, json!({"err": "upstream_timeout"}));
    }

    #[test]
    fn urlerror_wrapping_timeout_maps_to_upstream_timeout() {
        // Python: urllib wrapped a socket timeout inside URLError — still
        // an upstream timeout. Rust equivalent: reqwest wraps the per-recv
        // io timeout inside its transport error; the LIVE classifier must
        // yield FetchError::Timeout (which handle_core maps to
        // upstream_timeout — pinned by socket_timeout_maps_to_upstream_
        // timeout above). Proven against a REAL loopback socket that
        // accepts and then stays silent past the read timeout.
        use std::io::Read as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                // Read the request, answer NOTHING, hold the socket open
                // past the client's read timeout.
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf);
                std::thread::sleep(std::time::Duration::from_millis(1500));
            }
        });
        let mut up = ReqwestUpstream::with_timeouts(
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(200),
        )
        .unwrap();
        let out = up.fetch(&RelayRequest {
            method: "GET".to_string(),
            url: format!("http://{addr}/"),
            headers: vec![],
            timeout_secs: TIMEOUT_SECS,
        });
        match out {
            Err(FetchError::Timeout) => {}
            Err(FetchError::Unreachable) => panic!("wrapped timeout misclassified as unreachable"),
            Ok(_) => panic!("silent server produced a response"),
        }
        let _ = srv.join();
    }

    #[test]
    fn unframed_non_3xx_body_timeout_maps_to_upstream_timeout() {
        // Fixer round 2 (2026-07-06): a per-recv timeout raised while
        // READING an unframed NON-3xx error body must map to the honest
        // upstream_timeout -> front 504 (in Python it originally ESCAPED
        // lambda_handler as a FunctionError -> dishonest offline-503).
        // /settings is NOT rewritten, so this exercises the non-3xx body
        // read (the 3xx arm never reads — see the bomb-body test above).
        let mut up = FakeUpstream::new(vec![FakeReply::Response {
            status: 500,
            headers: vec![],
            body: Box::new(FailBody {
                err: BodyReadError::Timeout,
            }),
        }]);
        let r = handle_core(
            &json!({"method": "GET", "path": "/settings"}),
            FAKE_BASE,
            &mut up,
        );
        assert_eq!(r, json!({"err": "upstream_timeout"}));
    }

    #[test]
    fn non_timeout_error_body_read_failure_maps_to_box_unreachable() {
        // Fixer round 3 (2026-07-06): a NON-timeout failure of the same
        // non-3xx error-body read — the Python escape class was
        // ConnectionResetError (peer RST mid error body, e.g. the QuestDB
        // container restart on every box deploy), http.client
        // IncompleteRead, ConnectionAbortedError — must map to the typed
        // box_unreachable (a reset/aborted peer IS unreachable-class). The
        // three Python exception types collapse into BodyReadError::Other
        // at the seam by design.
        let mut up = FakeUpstream::new(vec![FakeReply::Response {
            status: 500,
            headers: vec![],
            body: Box::new(FailBody {
                err: BodyReadError::Other,
            }),
        }]);
        let r = handle_core(
            &json!({"method": "GET", "path": "/settings"}),
            FAKE_BASE,
            &mut up,
        );
        assert_eq!(r, json!({"err": "box_unreachable"}));
    }

    #[test]
    fn http_error_is_relayed_not_swallowed() {
        // QuestDB's own 400 (bad column etc.) must reach the console UI.
        let mut up = FakeUpstream::new(vec![ok_resp(
            vec![br#"{"error":"bad column"}"#],
            400,
            vec![("content-type", "application/json")],
        )]);
        let r = handle_core(
            &event("GET", "/exec", "query=select+bad+from+t"),
            FAKE_BASE,
            &mut up,
        );
        assert_eq!(r["status"], 400);
        assert_eq!(b64d(&r), br#"{"error":"bad column"}"#);
    }

    #[test]
    fn empty_qdb_base_is_box_unreachable() {
        let mut up = FakeUpstream::never_called();
        let r = handle_core(&json!({"method": "GET", "path": "/"}), "", &mut up);
        assert_eq!(r, json!({"err": "box_unreachable"}));
        assert!(up.calls.borrow().is_empty());
    }

    // ------------------------------------------------------------ Hygiene

    /// Everything above the first column-0 `#[cfg(test)]` line — the house
    /// production-region split (the Python hygiene tests scanned handler.py
    /// while the fixtures lived in test_handler.py; in Rust both share this
    /// file, so the scan excises the test module).
    fn production_region() -> &'static str {
        let src = include_str!("qdb_console_proxy.rs");
        let marker = concat!("#[cfg(", "test)]");
        src.split(marker).next().unwrap_or(src)
    }

    #[test]
    fn no_aws_sdk_import() {
        // The VPC hop is secret-free and SDK-free by contract (it has no
        // AWS-API network path anyway — no NAT, no VPC endpoints). Python
        // asserted no `boto3`; the Rust equivalent is no aws_sdk_* /
        // aws_config use in the production region. Needles are assembled at
        // runtime so these literals never self-match.
        let prod = production_region();
        assert!(!prod.contains(&format!("aws_{}", "sdk_")));
        assert!(!prod.contains(&format!("aws_{}", "config")));
        assert!(!prod.contains(&format!("local{}", "host")));
        assert!(!prod.contains(&format!("127.0.{}", "0.1")));
    }

    #[test]
    fn no_hardcoded_private_ip() {
        // IP comes ONLY from the TF-injected env (the fakes' 10.42.1.99
        // lives in the test region, which is excised from the scan).
        let prod = production_region();
        assert!(!prod.contains(&format!("10.{}", "42.")));
    }

    #[test]
    fn build_marker_present() {
        assert!(QDB_CONSOLE_BUILD.starts_with("b4-qdb-console-"));
        // Python also asserted the front handler carries the same marker;
        // the front-side cross-pin lands with the qdb_console_front module
        // (its `build_marker_present` asserts equality against this const).
    }
}
