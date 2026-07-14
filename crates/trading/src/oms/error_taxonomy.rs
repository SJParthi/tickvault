//! Pure, total, panic-free Dhan order-path error taxonomy.
//!
//! Cold path — order submission is ~1-100/day. Allocations acceptable
//! (`crates/trading/src/oms` is exempt from the hot-path allocation bans; only
//! the universal Cat-1 bans apply).
//!
//! This module classifies a Dhan REST error (or the transport failure carried
//! in `OmsError`) into a `DhanErrorClass`, then maps that class — per order
//! endpoint — onto an `OrderErrorPolicy` (retry / backoff / halt / stop-all),
//! a coded `ErrorCode`, and a circuit-breaker interaction. Classification is
//! done at CONSUMPTION time over `OmsError` so the ~35 existing api_client
//! construction sites stay byte-identical.
//!
//! Ground truth: `docs/dhan-ref/01-introduction-and-rate-limits.md` +
//! `08-annexure-enums.md`; `.claude/rules/dhan/annexure-enums.md` rules 11-12;
//! `.claude/rules/project/order-readiness-error-codes.md`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use tickvault_common::constants::DHAN_ERROR_BODY_SCAN_CAP_BYTES;
use tickvault_common::error_code::ErrorCode;

use super::types::OmsError;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Which order endpoint the error came from. The endpoint decides how ambiguous
/// (possibly-already-processed) errors fold: Place/Modify never retry them
/// (duplicate-order risk); Cancel gets exactly one transient retry (exposure
/// reducing, double-cancel is a benign terminal error).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderEndpoint {
    /// `POST /orders`.
    Place,
    /// `PUT /orders/{id}`.
    Modify,
    /// `DELETE /orders/{id}`.
    Cancel,
}

/// The classified Dhan order-path error. Shape-gated codes always win over the
/// raw HTTP status (Dhan's own classification is authoritative).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DhanErrorClass {
    /// DH-901 — invalid auth.
    Dh901,
    /// DH-902 — no API access.
    Dh902,
    /// DH-903 — account issue.
    Dh903,
    /// DH-904 — rate limited.
    Dh904,
    /// DH-905 — input exception.
    Dh905,
    /// DH-906 — order error.
    Dh906,
    /// DH-907 — data error.
    Dh907,
    /// DH-908 — internal server error.
    Dh908,
    /// DH-909 — network error.
    Dh909,
    /// DH-910 — other.
    Dh910,
    /// DATA-800 — internal server error.
    Data800,
    /// DATA-804 — instruments exceed limit.
    Data804,
    /// DATA-805 — too many requests/connections (STOP ALL).
    Data805,
    /// DATA-806 — data APIs not subscribed.
    Data806,
    /// DATA-807 — access token expired.
    Data807,
    /// DATA-808 — auth failed.
    Data808,
    /// DATA-809 — token invalid.
    Data809,
    /// DATA-810 — client-id invalid.
    Data810,
    /// DATA-811 — invalid expiry.
    Data811,
    /// DATA-812 — invalid date format.
    Data812,
    /// DATA-813 — invalid SecurityId.
    Data813,
    /// DATA-814 — invalid request.
    Data814,
    /// HTTP 429 with no Dhan shape (the `check_rate_limit` / `DhanRateLimited` path).
    Http429NoCode,
    /// 401/403 with no Dhan shape (WAF class — NEVER emits Dh901, NEVER halts).
    AuthNoCode,
    /// 5xx with no Dhan shape (incl. HTML/WAF/empty bodies).
    ServerErrorNoCode,
    /// Other 4xx with no Dhan shape.
    ClientErrorNoCode,
    /// Dhan shape present, code outside the catalogue.
    UnknownCode,
    /// `OmsError::HttpError` transport failure (ambiguous by default).
    Transport,
}

/// The retry/backoff/halt policy for a `(class, endpoint)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderErrorPolicy {
    /// DH-901 — rotate token, retry once, then HALT.
    RotateRetryOnceThenHalt,
    /// DH-902/903, DATA-810 — halt + alert.
    HaltAndAlert,
    /// DH-904, HTTP 429 — DH-904 backoff ladder (all endpoints).
    BackoffLadder,
    /// Never retry (definitely-processed or definitely-bad-input).
    NeverRetry,
    /// DH-907, DATA-804 — check params, no retry.
    CheckParamsNoRetry,
    /// Cancel-only: 908/909/800/5xx/transport → one transient retry.
    CancelSingleRetry,
    /// DH-910, unknown code — log + alert.
    LogAndAlert,
    /// DATA-805 — engage the stop-all latch, no retry.
    StopAllCooldown,
    /// DATA-807/808/809 — token refresh, retry once, NO halt on the 2nd failure.
    TokenRefreshRetryOnce,
    /// DATA-806 on the order surface — alert only + poison readiness.
    AlertOnlyPoisonReadiness,
}

// ---------------------------------------------------------------------------
// BrokerCooldownLatch (DATA-805 STOP-ALL)
// ---------------------------------------------------------------------------

/// DATA-805 STOP-ALL latch: monotonic, lock-free, process-local (a field on
/// `OrderApiClient`). `engage()` is extension-only (`fetch_max`), immune to
/// wall-clock/NTP jumps (monotonic `tokio::time::Instant` anchor), and
/// pause-testable. Expiry is passive — no background task.
pub struct BrokerCooldownLatch {
    /// Deadline in milliseconds since `anchor`. 0 = never engaged.
    deadline_ms: AtomicU64,
    /// Monotonic anchor, lazily initialised on first engage.
    anchor: OnceLock<tokio::time::Instant>,
}

impl Default for BrokerCooldownLatch {
    fn default() -> Self {
        Self::new()
    }
}

impl BrokerCooldownLatch {
    /// A fresh, un-engaged latch.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            deadline_ms: AtomicU64::new(0),
            anchor: OnceLock::new(),
        }
    }

    fn now_ms(&self) -> u64 {
        // Monotonic elapsed since the anchor. Milliseconds fit u64 for any
        // realistic uptime; the `as` cast cannot meaningfully truncate.
        let anchor = *self.anchor.get_or_init(tokio::time::Instant::now);
        anchor.elapsed().as_millis() as u64
    }

    /// Engage (or extend) the cooldown for `secs`. Extension-only: a shorter
    /// deadline never shortens a longer one. Returns `true` on the RISING edge
    /// (was clear/expired → now latched) so the caller can alert exactly once.
    pub fn engage(&self, secs: u64) -> bool {
        let now = self.now_ms();
        let new_deadline = now.saturating_add(secs.saturating_mul(1_000));
        let prev = self.deadline_ms.fetch_max(new_deadline, Ordering::SeqCst);
        // Rising edge iff the previous deadline was already reached at `now`
        // (0 = never engaged also qualifies, since now >= 0).
        prev <= now
    }

    /// `None` = clear; `Some(remaining_secs)` = still latched (rounded up).
    #[must_use]
    pub fn remaining_secs(&self) -> Option<u64> {
        let deadline = self.deadline_ms.load(Ordering::SeqCst);
        if deadline == 0 {
            return None;
        }
        let now = self.now_ms();
        if deadline > now {
            Some((deadline - now).div_ceil(1_000))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// Bounded serde shape for a Dhan error body. A successful parse of this shape
/// (extra fields like `errorType`/`errorMessage` are ignored by serde) IS the
/// shape gate; `errorCode` is a `Value` because Dhan sends it as either a JSON
/// string (`"DH-905"`) or a bare number (`805`). A code is trusted only when
/// this parses AND `errorCode` is present — an HTML/WAF page cannot parse.
#[derive(serde::Deserialize)]
struct DhanErrorBody {
    #[serde(rename = "errorCode", default)]
    error_code: Option<serde_json::Value>,
}

/// Char-boundary-safe prefix of `body` capped at `cap` bytes.
fn safe_prefix(body: &str, cap: usize) -> &str {
    if body.len() <= cap {
        return body;
    }
    let mut end = cap;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

fn value_to_code_token(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Map a Dhan error-code token (`"DH-905"`, `"805"`, `"DATA-805"`, any case)
/// onto its class. `None` for anything outside the catalogue.
#[must_use]
pub fn class_for_code_token(raw: &str) -> Option<DhanErrorClass> {
    let up = raw.trim().to_ascii_uppercase();
    if let Some(dh) = dh_class(up.as_str()) {
        return Some(dh);
    }
    let numeric = up.strip_prefix("DATA-").unwrap_or(up.as_str());
    if let Ok(n) = numeric.parse::<u16>() {
        return data_class(n);
    }
    None
}

fn dh_class(up: &str) -> Option<DhanErrorClass> {
    Some(match up {
        "DH-901" => DhanErrorClass::Dh901,
        "DH-902" => DhanErrorClass::Dh902,
        "DH-903" => DhanErrorClass::Dh903,
        "DH-904" => DhanErrorClass::Dh904,
        "DH-905" => DhanErrorClass::Dh905,
        "DH-906" => DhanErrorClass::Dh906,
        "DH-907" => DhanErrorClass::Dh907,
        "DH-908" => DhanErrorClass::Dh908,
        "DH-909" => DhanErrorClass::Dh909,
        "DH-910" => DhanErrorClass::Dh910,
        _ => return None,
    })
}

const fn data_class(n: u16) -> Option<DhanErrorClass> {
    Some(match n {
        800 => DhanErrorClass::Data800,
        804 => DhanErrorClass::Data804,
        805 => DhanErrorClass::Data805,
        806 => DhanErrorClass::Data806,
        807 => DhanErrorClass::Data807,
        808 => DhanErrorClass::Data808,
        809 => DhanErrorClass::Data809,
        810 => DhanErrorClass::Data810,
        811 => DhanErrorClass::Data811,
        812 => DhanErrorClass::Data812,
        813 => DhanErrorClass::Data813,
        814 => DhanErrorClass::Data814,
        _ => return None,
    })
}

const fn classify_status(status: u16) -> DhanErrorClass {
    match status {
        429 => DhanErrorClass::Http429NoCode,
        401 | 403 => DhanErrorClass::AuthNoCode,
        500..=599 => DhanErrorClass::ServerErrorNoCode,
        // Any other 4xx — and any non-error status that somehow reached an
        // error arm — is treated as a client error (never retried on
        // place/modify; a benign single cancel retry).
        _ => DhanErrorClass::ClientErrorNoCode,
    }
}

/// Shape-gated classifier. Truncates `body` to `DHAN_ERROR_BODY_SCAN_CAP_BYTES`
/// on a char boundary, then serde-parses the Dhan 3-field shape. A code is
/// trusted ONLY if the body parsed as that shape AND carried an `errorCode`
/// (so a WAF/HTML page quoting a code in plain text can never steer policy —
/// it will not serde-parse and falls through to the status). Never panics;
/// one bounded serde allocation (cold path).
#[must_use]
pub fn classify_dhan_error(status: u16, body: &str) -> DhanErrorClass {
    let prefix = safe_prefix(body, DHAN_ERROR_BODY_SCAN_CAP_BYTES);
    // The shape gate is the serde parse itself: an `errorCode` is trusted only
    // when the bounded prefix parses as the Dhan 3-field JSON shape AND carries
    // that field. A WAF/HTML page quoting a code in plain text does not parse,
    // so it falls through to the status. A genuine envelope with only
    // type+message (no code) has nothing to classify by → status fallback too.
    if let Ok(parsed) = serde_json::from_str::<DhanErrorBody>(prefix)
        && let Some(token) = parsed.error_code.as_ref().and_then(value_to_code_token)
    {
        return class_for_code_token(&token).unwrap_or(DhanErrorClass::UnknownCode);
    }
    classify_status(status)
}

/// Consumption-side entry point. Maps an `OmsError` onto its Dhan class, or
/// `None` for non-Dhan-surface errors (risk / rate-limiter / not-found / …).
#[must_use]
pub fn classify_oms_error(err: &OmsError) -> Option<DhanErrorClass> {
    match err {
        OmsError::DhanApiError {
            status_code,
            message,
        } => Some(classify_dhan_error(*status_code, message)),
        OmsError::DhanRateLimited => Some(DhanErrorClass::Http429NoCode),
        OmsError::HttpError(_) => Some(DhanErrorClass::Transport),
        OmsError::StopAllCooldown { .. } => Some(DhanErrorClass::Data805),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Policy tables (pure, const)
// ---------------------------------------------------------------------------

const fn ambiguous_policy(endpoint: OrderEndpoint) -> OrderErrorPolicy {
    match endpoint {
        OrderEndpoint::Cancel => OrderErrorPolicy::CancelSingleRetry,
        OrderEndpoint::Place | OrderEndpoint::Modify => OrderErrorPolicy::NeverRetry,
    }
}

/// THE policy table. Place/Modify never retry ambiguous (possibly-processed)
/// classes; Cancel folds them to a single transient retry.
#[must_use]
pub const fn policy_for(class: DhanErrorClass, endpoint: OrderEndpoint) -> OrderErrorPolicy {
    use DhanErrorClass as C;
    use OrderErrorPolicy as P;
    match class {
        C::Dh901 => P::RotateRetryOnceThenHalt,
        C::Dh902 | C::Dh903 | C::Data810 => P::HaltAndAlert,
        C::Dh904 | C::Http429NoCode => P::BackoffLadder,
        C::Dh907 | C::Data804 => P::CheckParamsNoRetry,
        C::Data805 => P::StopAllCooldown,
        C::Data806 => P::AlertOnlyPoisonReadiness,
        C::Data807 | C::Data808 | C::Data809 => P::TokenRefreshRetryOnce,
        C::Dh910 | C::UnknownCode => P::LogAndAlert,
        // Never-retry any endpoint: definitely-processed or definitely-bad-input.
        C::Dh905
        | C::Dh906
        | C::Data811
        | C::Data812
        | C::Data813
        | C::Data814
        | C::AuthNoCode
        | C::ClientErrorNoCode => P::NeverRetry,
        // Ambiguous (may already have been processed) → endpoint fold.
        C::Dh908 | C::Dh909 | C::Data800 | C::ServerErrorNoCode | C::Transport => {
            ambiguous_policy(endpoint)
        }
    }
}

/// The coded `ErrorCode` for a class. AuthNoCode/UnknownCode → `Dh910Other`
/// (NEVER `Dh901InvalidAuth` — preserves the pre-existing DH-901 CloudWatch
/// tripwire precision on a WAF blip).
#[must_use]
pub const fn error_code_for(class: DhanErrorClass) -> ErrorCode {
    use DhanErrorClass as C;
    use ErrorCode as E;
    match class {
        C::Dh901 => E::Dh901InvalidAuth,
        C::Dh902 => E::Dh902NoApiAccess,
        C::Dh903 => E::Dh903AccountIssue,
        C::Dh904 | C::Http429NoCode => E::Dh904RateLimit,
        C::Dh905 | C::ClientErrorNoCode => E::Dh905InputException,
        C::Dh906 => E::Dh906OrderError,
        C::Dh907 => E::Dh907DataError,
        C::Dh908 | C::ServerErrorNoCode => E::Dh908InternalServerError,
        C::Dh909 | C::Transport => E::Dh909NetworkError,
        C::Dh910 | C::AuthNoCode | C::UnknownCode => E::Dh910Other,
        C::Data800 => E::Data800InternalServerError,
        C::Data804 => E::Data804InstrumentsExceedLimit,
        C::Data805 => E::Data805TooManyConnections,
        C::Data806 => E::Data806NotSubscribed,
        C::Data807 => E::Data807TokenExpired,
        C::Data808 => E::Data808AuthFailed,
        C::Data809 => E::Data809TokenInvalid,
        C::Data810 => E::Data810ClientIdInvalid,
        C::Data811 => E::Data811InvalidExpiry,
        C::Data812 => E::Data812InvalidDateFormat,
        C::Data813 => E::Data813InvalidSecurityId,
        C::Data814 => E::Data814InvalidRequest,
    }
}

/// Whether a class trips the OMS circuit breaker. Halt classes
/// (901/902/903/810), rate classes (904/429/805) and refresh/entitlement
/// classes (806/807/808/809) do NOT trip — the breaker's time-based HalfOpen
/// probe would fire a real order into a dead/throttled account; the halt latch
/// owns account-class recovery. Everything else keeps tripping.
#[must_use]
pub const fn trips_circuit_breaker(class: DhanErrorClass) -> bool {
    use DhanErrorClass as C;
    !matches!(
        class,
        C::Dh901
            | C::Dh902
            | C::Dh903
            | C::Data810
            | C::Dh904
            | C::Http429NoCode
            | C::Data805
            | C::Data806
            | C::Data807
            | C::Data808
            | C::Data809
    )
}

// ---------------------------------------------------------------------------
// Extraction / sanitisation (metric-label closed-set + log hygiene)
// ---------------------------------------------------------------------------

/// Borrow-based substring extraction of a quoted `errorCode` token, bounded by
/// `DHAN_ERROR_BODY_SCAN_CAP_BYTES`. Reused by `record_dh_error_metric`'s
/// closed-set label rebase. Numeric (unquoted) codes return `None` here (they
/// were not classified by the legacy substring scan either).
#[must_use]
pub fn extract_dhan_error_code(body: &str) -> Option<&str> {
    let scan = safe_prefix(body, DHAN_ERROR_BODY_SCAN_CAP_BYTES);
    let start = scan.find("\"errorCode\":\"")?;
    let after = &scan[start + 13..]; // skip past `"errorCode":"`
    let end = after.find('"')?;
    Some(&after[..end])
}

/// Shape-gated extraction of the Dhan `errorCode` token as an owned `String`,
/// handling BOTH the quoted (`"errorCode":"DH-905"`) and bare-numeric
/// (`"errorCode":805`) forms. Reused by `record_dh_error_metric` so DATA-8xx
/// numeric codes (Dhan sends `{"errorCode":805}`) get a proper closed-set
/// label instead of `"unknown"`. `None` unless the bounded prefix parses as
/// the Dhan 3-field shape AND carries an `errorCode`.
#[must_use]
pub fn extract_dhan_error_code_token(body: &str) -> Option<String> {
    let prefix = safe_prefix(body, DHAN_ERROR_BODY_SCAN_CAP_BYTES);
    let parsed = serde_json::from_str::<DhanErrorBody>(prefix).ok()?;
    parsed.error_code.as_ref().and_then(value_to_code_token)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- extraction ---

    #[test]
    fn test_extract_dhan_error_code_string_form() {
        assert_eq!(
            extract_dhan_error_code(r#"{"errorCode":"DH-905","errorMessage":"bad"}"#),
            Some("DH-905")
        );
    }

    #[test]
    fn test_extract_dhan_error_code_numeric_form() {
        // Unquoted numeric code is NOT matched by the quoted-substring scan.
        assert_eq!(extract_dhan_error_code(r#"{"errorCode":805}"#), None);
    }

    #[test]
    fn test_extract_dhan_error_code_absent() {
        assert_eq!(extract_dhan_error_code(r#"{"message":"nope"}"#), None);
    }

    #[test]
    fn test_extract_dhan_error_code_empty_body() {
        assert_eq!(extract_dhan_error_code(""), None);
    }

    #[test]
    fn test_extract_dhan_error_code_truncated_at_scan_cap() {
        // A code that begins after the scan cap must not be found.
        let padding = "x".repeat(DHAN_ERROR_BODY_SCAN_CAP_BYTES);
        let body = format!("{padding}\"errorCode\":\"DH-905\"");
        assert_eq!(extract_dhan_error_code(&body), None);
    }

    #[test]
    fn test_extract_dhan_error_code_multibyte_boundary_at_cap() {
        // Multibyte chars straddling the cap must not panic and must truncate
        // on a char boundary.
        let body = "🔷".repeat(DHAN_ERROR_BODY_SCAN_CAP_BYTES);
        assert_eq!(extract_dhan_error_code(&body), None);
    }

    #[test]
    fn test_extract_dhan_error_code_token_quoted_and_numeric() {
        // Quoted string form.
        assert_eq!(
            extract_dhan_error_code_token(r#"{"errorCode":"DH-905","errorMessage":"bad"}"#)
                .as_deref(),
            Some("DH-905")
        );
        // Bare-numeric form (Dhan sends DATA-8xx as `{"errorCode":805}`).
        assert_eq!(
            extract_dhan_error_code_token(r#"{"errorType":"x","errorCode":807}"#).as_deref(),
            Some("807")
        );
        // A WAF/HTML page quoting a code in plain text does not parse → None.
        assert_eq!(
            extract_dhan_error_code_token("<html>DH-905 blocked</html>"),
            None
        );
        assert_eq!(extract_dhan_error_code_token(""), None);
    }

    // --- classification: codes ---

    #[test]
    fn test_class_for_code_token_case_and_prefix_forms() {
        assert_eq!(class_for_code_token("dh-901"), Some(DhanErrorClass::Dh901));
        assert_eq!(
            class_for_code_token(" DH-910 "),
            Some(DhanErrorClass::Dh910)
        );
        assert_eq!(class_for_code_token("805"), Some(DhanErrorClass::Data805));
        assert_eq!(
            class_for_code_token("data-807"),
            Some(DhanErrorClass::Data807)
        );
        assert_eq!(class_for_code_token("DH-999"), None);
        assert_eq!(class_for_code_token("803"), None);
        assert_eq!(class_for_code_token("garbage"), None);
    }

    #[test]
    fn test_classify_all_ten_dh_codes() {
        let cases = [
            ("DH-901", DhanErrorClass::Dh901),
            ("DH-902", DhanErrorClass::Dh902),
            ("DH-903", DhanErrorClass::Dh903),
            ("DH-904", DhanErrorClass::Dh904),
            ("DH-905", DhanErrorClass::Dh905),
            ("DH-906", DhanErrorClass::Dh906),
            ("DH-907", DhanErrorClass::Dh907),
            ("DH-908", DhanErrorClass::Dh908),
            ("DH-909", DhanErrorClass::Dh909),
            ("DH-910", DhanErrorClass::Dh910),
        ];
        for (code, expected) in cases {
            let body = format!("{{\"errorCode\":\"{code}\"}}");
            assert_eq!(classify_dhan_error(400, &body), expected, "code {code}");
        }
    }

    #[test]
    fn test_classify_all_twelve_data_codes() {
        let cases = [
            (800, DhanErrorClass::Data800),
            (804, DhanErrorClass::Data804),
            (805, DhanErrorClass::Data805),
            (806, DhanErrorClass::Data806),
            (807, DhanErrorClass::Data807),
            (808, DhanErrorClass::Data808),
            (809, DhanErrorClass::Data809),
            (810, DhanErrorClass::Data810),
            (811, DhanErrorClass::Data811),
            (812, DhanErrorClass::Data812),
            (813, DhanErrorClass::Data813),
            (814, DhanErrorClass::Data814),
        ];
        for (n, expected) in cases {
            let body = format!("{{\"errorCode\":{n}}}");
            assert_eq!(classify_dhan_error(400, &body), expected, "data {n}");
        }
    }

    #[test]
    fn test_classify_body_code_wins_over_status() {
        // A 200 that reached the error arm but carries a DH-905 body -> Dh905.
        assert_eq!(
            classify_dhan_error(200, r#"{"errorCode":"DH-905"}"#),
            DhanErrorClass::Dh905
        );
        // A 500 that carries a DH-904 body -> Dh904 (code beats status).
        assert_eq!(
            classify_dhan_error(500, r#"{"errorCode":"DH-904"}"#),
            DhanErrorClass::Dh904
        );
    }

    #[test]
    fn test_classify_html_waf_body_quoting_code_never_classifies_by_code() {
        // A WAF/HTML page that quotes DH-905 in plain text must NOT be
        // classified as Dh905 — it does not serde-parse; status wins.
        let html = "<html><body>Blocked. Ref DH-905 errorCode DH-905</body></html>";
        assert_eq!(classify_dhan_error(403, html), DhanErrorClass::AuthNoCode);
        assert_eq!(
            classify_dhan_error(503, html),
            DhanErrorClass::ServerErrorNoCode
        );
    }

    #[test]
    fn test_classify_status_fallbacks_429_401_403_5xx_4xx_no_shape() {
        let empty = "";
        assert_eq!(
            classify_dhan_error(429, empty),
            DhanErrorClass::Http429NoCode
        );
        assert_eq!(classify_dhan_error(401, empty), DhanErrorClass::AuthNoCode);
        assert_eq!(classify_dhan_error(403, empty), DhanErrorClass::AuthNoCode);
        assert_eq!(
            classify_dhan_error(500, empty),
            DhanErrorClass::ServerErrorNoCode
        );
        assert_eq!(
            classify_dhan_error(502, empty),
            DhanErrorClass::ServerErrorNoCode
        );
        assert_eq!(
            classify_dhan_error(422, empty),
            DhanErrorClass::ClientErrorNoCode
        );
    }

    #[test]
    fn test_classify_unknown_dhan_code_sanitized() {
        // Dhan-shaped body, code outside the catalogue -> UnknownCode.
        assert_eq!(
            classify_dhan_error(400, r#"{"errorCode":"DH-999"}"#),
            DhanErrorClass::UnknownCode
        );
    }

    #[test]
    fn test_classify_malformed_json_falls_back_to_status() {
        assert_eq!(
            classify_dhan_error(500, "{not json"),
            DhanErrorClass::ServerErrorNoCode
        );
        // Genuine envelope with type+message but no code -> status fallback.
        assert_eq!(
            classify_dhan_error(400, r#"{"errorType":"x","errorMessage":"y"}"#),
            DhanErrorClass::ClientErrorNoCode
        );
    }

    // --- classify_oms_error ---

    #[test]
    fn test_classify_oms_error_rate_limited_maps_http429() {
        assert_eq!(
            classify_oms_error(&OmsError::DhanRateLimited),
            Some(DhanErrorClass::Http429NoCode)
        );
    }

    #[test]
    fn test_classify_oms_error_http_error_maps_transport() {
        assert_eq!(
            classify_oms_error(&OmsError::HttpError("reset".to_owned())),
            Some(DhanErrorClass::Transport)
        );
    }

    #[test]
    fn test_classify_oms_error_stop_all_cooldown_maps_data805() {
        assert_eq!(
            classify_oms_error(&OmsError::StopAllCooldown { remaining_secs: 12 }),
            Some(DhanErrorClass::Data805)
        );
    }

    #[test]
    fn test_classify_oms_error_dhan_api_error_uses_body() {
        assert_eq!(
            classify_oms_error(&OmsError::DhanApiError {
                status_code: 400,
                message: r#"{"errorCode":"DH-905"}"#.to_owned(),
            }),
            Some(DhanErrorClass::Dh905)
        );
    }

    #[test]
    fn test_classify_oms_error_non_dhan_variants_none() {
        assert_eq!(classify_oms_error(&OmsError::RateLimited), None);
        assert_eq!(classify_oms_error(&OmsError::CircuitBreakerOpen), None);
        assert_eq!(classify_oms_error(&OmsError::NoToken), None);
        assert_eq!(classify_oms_error(&OmsError::TokenExpired), None);
    }

    // --- policy ---

    fn all_classes() -> [DhanErrorClass; 27] {
        use DhanErrorClass as C;
        [
            C::Dh901,
            C::Dh902,
            C::Dh903,
            C::Dh904,
            C::Dh905,
            C::Dh906,
            C::Dh907,
            C::Dh908,
            C::Dh909,
            C::Dh910,
            C::Data800,
            C::Data804,
            C::Data805,
            C::Data806,
            C::Data807,
            C::Data808,
            C::Data809,
            C::Data810,
            C::Data811,
            C::Data812,
            C::Data813,
            C::Data814,
            C::Http429NoCode,
            C::AuthNoCode,
            C::ServerErrorNoCode,
            C::ClientErrorNoCode,
            C::Transport,
        ]
    }

    #[test]
    fn test_policy_for_place_never_retries_ambiguous_908_909_800_5xx_transport() {
        use DhanErrorClass as C;
        for class in [
            C::Dh908,
            C::Dh909,
            C::Data800,
            C::ServerErrorNoCode,
            C::Transport,
        ] {
            assert_eq!(
                policy_for(class, OrderEndpoint::Place),
                OrderErrorPolicy::NeverRetry,
                "place must never retry {class:?}"
            );
        }
    }

    #[test]
    fn test_policy_for_modify_never_retries_ambiguous_classes() {
        use DhanErrorClass as C;
        for class in [
            C::Dh908,
            C::Dh909,
            C::Data800,
            C::ServerErrorNoCode,
            C::Transport,
        ] {
            assert_eq!(
                policy_for(class, OrderEndpoint::Modify),
                OrderErrorPolicy::NeverRetry,
                "modify must never retry {class:?}"
            );
        }
    }

    #[test]
    fn test_policy_for_cancel_single_retry_classes() {
        use DhanErrorClass as C;
        for class in [
            C::Dh908,
            C::Dh909,
            C::Data800,
            C::ServerErrorNoCode,
            C::Transport,
        ] {
            assert_eq!(
                policy_for(class, OrderEndpoint::Cancel),
                OrderErrorPolicy::CancelSingleRetry,
                "cancel one-retries {class:?}"
            );
        }
    }

    #[test]
    fn test_policy_for_all_classes_all_endpoints_total() {
        // Exhaustive cross-product must not panic and each class must yield a
        // consistent policy per endpoint (only the ambiguous set differs).
        for class in all_classes() {
            let place = policy_for(class, OrderEndpoint::Place);
            let modify = policy_for(class, OrderEndpoint::Modify);
            let cancel = policy_for(class, OrderEndpoint::Cancel);
            // Place and Modify always agree (no per-endpoint modify special-case).
            assert_eq!(place, modify, "place/modify diverge for {class:?}");
            // The ONLY class that differs on cancel is the ambiguous fold.
            let ambiguous = matches!(
                class,
                DhanErrorClass::Dh908
                    | DhanErrorClass::Dh909
                    | DhanErrorClass::Data800
                    | DhanErrorClass::ServerErrorNoCode
                    | DhanErrorClass::Transport
            );
            if ambiguous {
                assert_eq!(place, OrderErrorPolicy::NeverRetry);
                assert_eq!(cancel, OrderErrorPolicy::CancelSingleRetry);
            } else {
                assert_eq!(place, cancel, "non-ambiguous class differs on cancel");
            }
        }
    }

    #[test]
    fn test_policy_for_boundary_specific_rows() {
        // Boundary/spot checks for the non-ambiguous policy rows.
        assert_eq!(
            policy_for(DhanErrorClass::Dh901, OrderEndpoint::Place),
            OrderErrorPolicy::RotateRetryOnceThenHalt
        );
        assert_eq!(
            policy_for(DhanErrorClass::Dh904, OrderEndpoint::Cancel),
            OrderErrorPolicy::BackoffLadder
        );
        assert_eq!(
            policy_for(DhanErrorClass::Data805, OrderEndpoint::Place),
            OrderErrorPolicy::StopAllCooldown
        );
        assert_eq!(
            policy_for(DhanErrorClass::Data806, OrderEndpoint::Modify),
            OrderErrorPolicy::AlertOnlyPoisonReadiness
        );
        assert_eq!(
            policy_for(DhanErrorClass::Data807, OrderEndpoint::Place),
            OrderErrorPolicy::TokenRefreshRetryOnce
        );
        assert_eq!(
            policy_for(DhanErrorClass::AuthNoCode, OrderEndpoint::Cancel),
            OrderErrorPolicy::NeverRetry
        );
    }

    // --- error_code_for ---

    #[test]
    fn test_error_code_for_covers_every_class() {
        for class in all_classes() {
            // Must not panic; must return a stable wire code.
            let code = error_code_for(class);
            assert!(!code.code_str().is_empty());
        }
    }

    #[test]
    fn test_error_code_for_auth_no_code_is_dh910_never_dh901() {
        // R15 tripwire pin: a non-Dhan-shaped 401/403 must NEVER emit
        // Dh901InvalidAuth (that would trip the pre-existing DH-901 filter).
        assert_eq!(
            error_code_for(DhanErrorClass::AuthNoCode),
            ErrorCode::Dh910Other
        );
        assert_eq!(
            error_code_for(DhanErrorClass::UnknownCode),
            ErrorCode::Dh910Other
        );
        assert_eq!(
            error_code_for(DhanErrorClass::ServerErrorNoCode),
            ErrorCode::Dh908InternalServerError
        );
        assert_eq!(
            error_code_for(DhanErrorClass::ClientErrorNoCode),
            ErrorCode::Dh905InputException
        );
        assert_eq!(
            error_code_for(DhanErrorClass::Transport),
            ErrorCode::Dh909NetworkError
        );
    }

    // --- circuit breaker ---

    #[test]
    fn test_trips_circuit_breaker_halt_rate_and_refresh_classes_false() {
        use DhanErrorClass as C;
        for class in [
            C::Dh901,
            C::Dh902,
            C::Dh903,
            C::Data810,
            C::Dh904,
            C::Http429NoCode,
            C::Data805,
            C::Data806,
            C::Data807,
            C::Data808,
            C::Data809,
        ] {
            assert!(!trips_circuit_breaker(class), "{class:?} must not trip CB");
        }
        for class in [
            C::Dh905,
            C::Dh906,
            C::Dh907,
            C::Dh908,
            C::Dh909,
            C::Dh910,
            C::Data800,
            C::Data804,
            C::Data811,
            C::Data814,
            C::AuthNoCode,
            C::ServerErrorNoCode,
            C::ClientErrorNoCode,
            C::UnknownCode,
            C::Transport,
        ] {
            assert!(trips_circuit_breaker(class), "{class:?} must trip CB");
        }
    }

    // --- BrokerCooldownLatch ---

    #[tokio::test(start_paused = true)]
    async fn test_cooldown_latch_engage_rising_edge_once() {
        let latch = BrokerCooldownLatch::new();
        assert!(latch.remaining_secs().is_none());
        assert!(latch.engage(60), "first engage is a rising edge");
        assert!(
            !latch.engage(60),
            "re-engage while latched is not a rising edge"
        );
        assert!(latch.remaining_secs().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn test_cooldown_latch_fetch_max_extends_never_shortens() {
        let latch = BrokerCooldownLatch::new();
        latch.engage(60);
        let long = latch.remaining_secs().unwrap_or(0);
        // A shorter engage must not shorten the deadline.
        latch.engage(1);
        let after = latch.remaining_secs().unwrap_or(0);
        assert!(after >= long.saturating_sub(1), "deadline must not shorten");
    }

    #[tokio::test(start_paused = true)]
    async fn test_cooldown_latch_expires_passively_paused_clock() {
        let latch = BrokerCooldownLatch::new();
        latch.engage(60);
        assert!(latch.remaining_secs().is_some());
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        assert!(
            latch.remaining_secs().is_none(),
            "latch must expire passively"
        );
        // Post-expiry, engage is a fresh rising edge again.
        assert!(latch.engage(60));
    }

    // --- proptests ---

    proptest::proptest! {
        #[test]
        fn prop_classify_never_panics_and_is_deterministic_on_arbitrary_status_and_body(
            status in proptest::prelude::any::<u16>(),
            body in ".{0,9000}",
        ) {
            let a = classify_dhan_error(status, &body);
            let b = classify_dhan_error(status, &body);
            proptest::prop_assert_eq!(a, b);
        }

        #[test]
        fn prop_classify_multibyte_body_never_panics(
            status in proptest::prelude::any::<u16>(),
            body in "(\\PC){0,3000}",
        ) {
            let _ = classify_dhan_error(status, &body);
        }

        #[test]
        fn prop_extract_code_output_always_within_caps(body in ".{0,9000}") {
            if let Some(code) = extract_dhan_error_code(&body) {
                proptest::prop_assert!(code.len() < DHAN_ERROR_BODY_SCAN_CAP_BYTES);
            }
            // The shape-gated token extractor never panics on arbitrary input.
            let _ = extract_dhan_error_code_token(&body);
        }

        #[test]
        fn prop_policy_for_total_no_panic(
            idx in 0usize..27,
            endpoint_idx in 0usize..3,
        ) {
            let class = super::tests::all_classes()[idx];
            let endpoint = [OrderEndpoint::Place, OrderEndpoint::Modify, OrderEndpoint::Cancel][endpoint_idx];
            let _ = policy_for(class, endpoint);
            let _ = error_code_for(class);
            let _ = trips_circuit_breaker(class);
        }
    }
}
