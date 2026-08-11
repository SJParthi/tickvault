//! §18-hardened CSV download for the daily instrument pipeline.
//!
//! Two sources, one client contract:
//!
//! * the Dhan detailed instrument master (`DHAN_DETAILED_CSV_URL`)
//! * the NSE India (niftyindices) index constituent lists
//!
//! Rebuilt 2026-08-11 under the operator's dated Q3 reversal. The hardening
//! below is not invented here — it is §18 of
//! `daily-universe-scope-expansion-2026-05-27.md`, locked after a security
//! review, and every row of that table is implemented verbatim.
//!
//! # Why the hardening is not optional
//!
//! This client fetches an unauthenticated file over the public internet and
//! hands the result to a parser whose output decides which instruments we
//! subscribe and trade. Each control below closes a specific way that becomes
//! dangerous:
//!
//! | Control | What it stops |
//! |---|---|
//! | Redirect refusal | a poisoned DNS answer 301-ing us to an attacker host that serves a valid-TLS CSV of its own choosing |
//! | Byte cap, enforced DURING the read | a server streaming gigabytes; a cap checked only after `bytes()` has already bought the whole thing |
//! | Content-type allowlist | a WAF block page or a JSON error body being fed to the CSV parser |
//! | Bounded connect + total timeouts | a black-hole DNS or a stalled socket holding the boot open |
//! | HTTPS only | a downgrade to plaintext |
//! | Never logging the URL | a query string carrying anything sensitive reaching the log sink |
//!
//! # Complexity
//!
//! O(response bytes), bounded by [`tickvault_common::constants::MAX_CSV_BODY_BYTES`].
//! COLD PATH — once per source per trading day. Never on the tick path.

use std::time::Duration;

use tickvault_common::constants::{
    CSV_CONNECT_TIMEOUT_SECS, INDEX_CONSTITUENCY_BASE_URL, INDEX_CONSTITUENCY_USER_AGENT,
    INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS, MAX_CSV_BODY_BYTES,
};

/// Content types a CSV endpoint may legitimately serve.
///
/// A CDN commonly serves a static `.csv` as `application/octet-stream`, and
/// some serve `text/plain`; both are accepted. `text/html` and
/// `application/json` are the two that matter to REJECT — they are what an
/// error page and a broken API respectively return, and both would otherwise
/// reach the parser as "a CSV that happens to fail".
pub const ALLOWED_CSV_CONTENT_TYPES: &[&str] =
    &["text/csv", "application/octet-stream", "text/plain"];

/// Why a download was refused. Every variant is a REFUSAL — there is no
/// partial-body success, because a truncated CSV parses into a plausible
/// smaller universe rather than an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadError {
    /// The client could not be constructed. Carries no URL.
    ClientBuild(String),
    /// Transport failure: DNS, TLS, connect, timeout, or a refused redirect.
    Transport(String),
    /// A non-2xx status. Carries the status only — never the body, which on
    /// an error is attacker- or WAF-controlled text.
    Status(u16),
    /// Content-type was absent-and-hostile or explicitly disallowed.
    ContentType(String),
    /// The body exceeded [`MAX_CSV_BODY_BYTES`], detected DURING the read.
    TooLarge { cap: usize },
    /// The body was not valid UTF-8. A CSV that is not text is not a CSV.
    NotUtf8,
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientBuild(e) => write!(f, "CSV download client build failed: {e}"),
            Self::Transport(e) => write!(f, "CSV download transport failure: {e}"),
            Self::Status(s) => write!(f, "CSV download refused: HTTP {s}"),
            Self::ContentType(ct) => write!(
                f,
                "CSV download refused: disallowed content-type {ct:?} \
                 (an HTML error page or JSON body must never reach the CSV parser)"
            ),
            Self::TooLarge { cap } => write!(
                f,
                "CSV download refused: body exceeded the {cap}-byte cap mid-read"
            ),
            Self::NotUtf8 => write!(f, "CSV download refused: body is not valid UTF-8"),
        }
    }
}

impl std::error::Error for DownloadError {}

/// Builds the §18-hardened client.
///
/// # Errors
/// [`DownloadError::ClientBuild`] if `reqwest` refuses the configuration.
pub fn build_hardened_csv_client() -> Result<reqwest::Client, DownloadError> {
    reqwest::ClientBuilder::new()
        // §18 row 1. `Policy::none()` does not error on a 3xx — it stops
        // following and hands back the redirect response, which then fails
        // the status check below. That is the intended shape: a redirect is
        // refused, never silently chased.
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .connect_timeout(Duration::from_secs(CSV_CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(
            INSTRUMENT_FETCH_PER_ATTEMPT_TIMEOUT_SECS,
        ))
        .build()
        .map_err(|e| DownloadError::ClientBuild(e.to_string()))
}

/// Decides whether a `Content-Type` header value is acceptable.
///
/// Pure and separately tested — the header is the last cheap gate before a
/// body reaches the parser, and its logic (parameter stripping, case folding,
/// the absent-header decision) is exactly where a lenient reading lets an
/// HTML error page through.
///
/// An ABSENT header is ACCEPTED. That is a deliberate trade, not an
/// oversight: static CDN objects routinely omit it, and refusing them would
/// take the pipeline down on a vendor CDN change. The byte cap and the
/// parser's own header/row checks remain in force behind it, so an HTML body
/// with no content-type still fails — one gate later, loudly.
#[must_use]
pub fn content_type_is_allowed(header: Option<&str>) -> bool {
    let Some(raw) = header else {
        return true;
    };
    let essence = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if essence.is_empty() {
        return true;
    }
    ALLOWED_CSV_CONTENT_TYPES.contains(&essence.as_str())
}

/// Fetches one CSV with the full §18 contract applied.
///
/// `url` is NEVER logged by this function, per §18's last row. Callers log a
/// stable label (`"dhan_master"`, `"nse_index"`) instead.
///
/// # Errors
/// [`DownloadError`] — every variant refuses the whole body.
pub async fn fetch_csv_hardened(
    client: &reqwest::Client,
    url: &str,
    user_agent: Option<&str>,
) -> Result<String, DownloadError> {
    let mut req = client.get(url);
    if let Some(ua) = user_agent {
        req = req.header(reqwest::header::USER_AGENT, ua);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| DownloadError::Transport(sanitize_transport_error(&e.to_string(), url)))?;

    let status = resp.status();
    if !status.is_success() {
        // A 3xx lands here too, because `Policy::none()` returns the redirect
        // rather than following it. Body deliberately not read: on an error
        // it is WAF- or attacker-controlled text with nothing to learn from.
        return Err(DownloadError::Status(status.as_u16()));
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if !content_type_is_allowed(content_type.as_deref()) {
        return Err(DownloadError::ContentType(content_type.unwrap_or_default()));
    }

    // A declared length over the cap is refused BEFORE a single byte of body
    // is read — the cheapest possible refusal. It is only an optimisation:
    // the header is server-controlled and may lie or be absent, so the
    // streaming check below is the one that actually enforces the cap.
    if resp
        .content_length()
        .is_some_and(|len| len > MAX_CSV_BODY_BYTES as u64)
    {
        return Err(DownloadError::TooLarge {
            cap: MAX_CSV_BODY_BYTES,
        });
    }

    // Streamed, not `resp.bytes()`. `bytes()` buys the ENTIRE body before we
    // can measure it, which makes a post-hoc length check useless against the
    // exact attack §18 names: a server streaming gigabytes. Checking per
    // chunk means we stop paying at the cap.
    let mut body: Vec<u8> = Vec::new();
    let mut resp = resp;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| DownloadError::Transport(sanitize_transport_error(&e.to_string(), url)))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_CSV_BODY_BYTES {
            return Err(DownloadError::TooLarge {
                cap: MAX_CSV_BODY_BYTES,
            });
        }
        body.extend_from_slice(&chunk);
    }

    String::from_utf8(body).map_err(|_| DownloadError::NotUtf8)
}

/// Strips the URL out of a transport error string.
///
/// `reqwest`'s error `Display` embeds the request URL, and for proxied
/// requests it can embed the PROXY url from `HTTPS_PROXY` — which may carry
/// Basic-Auth userinfo. §18's "never log the URL" rule is therefore not
/// satisfied by simply not logging it ourselves; the error text has to be
/// scrubbed too, or the rule leaks through the one path nobody inspects.
fn sanitize_transport_error(msg: &str, url: &str) -> String {
    let mut out = msg.replace(url, "<csv_url>");
    // Belt and braces: any REMAINING absolute URL (a proxy url, a redirect
    // target) is replaced wholesale rather than parsed. Cheap, and it cannot
    // be defeated by a shape we failed to anticipate.
    while let Some(start) = out.find("://") {
        let scheme_start = out[..start]
            .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '+' && c != '-' && c != '.')
            .map_or(0, |i| i + 1);
        let after = start + 3;
        let end = out[after..]
            .find(|c: char| c.is_whitespace() || c == ')' || c == '"' || c == ',')
            .map_or(out.len(), |i| after + i);
        out.replace_range(scheme_start..end, "<url>");
    }
    out
}

/// Builds the URL for one niftyindices constituent list.
///
/// Pure and tested so the slug→URL shape is pinned: a malformed URL here
/// yields a 404 that reads exactly like "this index has no constituents
/// today", which is the kind of failure that gets shrugged off.
#[must_use]
pub fn nse_index_csv_url(slug: &str) -> String {
    format!("{INDEX_CONSTITUENCY_BASE_URL}{slug}.csv")
}

/// The browser user-agent niftyindices requires.
///
/// Not cosmetic: the site answers **403** to a default client UA. Without
/// this, every index list fails identically and the join sees zero
/// constituents — which the tolerance gate correctly rejects, but the
/// operator would be hunting a phantom data problem rather than a header.
#[must_use]
pub const fn nse_user_agent() -> &'static str {
    INDEX_CONSTITUENCY_USER_AGENT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hardened_client_builds() {
        assert!(
            build_hardened_csv_client().is_ok(),
            "the §18 client configuration must be constructible"
        );
    }

    #[test]
    fn test_content_type_allows_the_three_csv_shapes() {
        for ct in ALLOWED_CSV_CONTENT_TYPES {
            assert!(
                content_type_is_allowed(Some(ct)),
                "{ct} is an allowed CSV content type"
            );
        }
    }

    #[test]
    fn test_content_type_rejects_html_and_json() {
        // The two that matter: an HTML body is a WAF block page, a JSON body
        // is a broken API. Both would otherwise reach the CSV parser and
        // present as "a CSV that happens to fail".
        assert!(!content_type_is_allowed(Some("text/html")));
        assert!(!content_type_is_allowed(Some("application/json")));
        assert!(!content_type_is_allowed(Some("application/xml")));
    }

    #[test]
    fn test_content_type_strips_parameters_and_folds_case() {
        // A real server sends `text/csv; charset=utf-8`, and case is not
        // guaranteed. A naive equality check rejects both — taking the
        // pipeline down on a correct response, which is the worst failure
        // mode a security control can have.
        assert!(content_type_is_allowed(Some("text/csv; charset=utf-8")));
        assert!(content_type_is_allowed(Some("TEXT/CSV")));
        assert!(content_type_is_allowed(Some("  text/csv  ")));
        // ...and the stripping must not become a way IN for a bad essence.
        assert!(!content_type_is_allowed(Some("text/html; charset=utf-8")));
    }

    #[test]
    fn test_content_type_absent_is_accepted_but_empty_essence_too() {
        // Documented trade: static CDN objects omit the header, and refusing
        // them would break on a vendor CDN change. The byte cap and the
        // parser's own checks still stand behind this.
        assert!(content_type_is_allowed(None));
        assert!(content_type_is_allowed(Some("")));
        assert!(content_type_is_allowed(Some("   ")));
    }

    #[test]
    fn test_content_type_rejects_a_prefix_lookalike() {
        // `text/csvx` must not pass on a `starts_with`-style reading, and
        // `application/octet-stream-x` likewise.
        assert!(!content_type_is_allowed(Some("text/csvx")));
        assert!(!content_type_is_allowed(Some("application/octet-stream-x")));
        assert!(
            !content_type_is_allowed(Some("x-text/csv")),
            "a suffix match must not admit a foreign type either"
        );
    }

    #[test]
    fn test_nse_index_url_shape_is_pinned() {
        let url = nse_index_csv_url("ind_nifty50list");
        assert_eq!(
            url,
            "https://www.niftyindices.com/IndexConstituent/ind_nifty50list.csv"
        );
        assert!(url.starts_with("https://"), "must never be plaintext http");
    }

    #[test]
    fn test_nse_user_agent_is_a_browser_string() {
        // niftyindices answers 403 to a default client UA. If this ever
        // degrades to something non-browser, every index list fails
        // identically and the failure reads like missing data rather than a
        // rejected header.
        let ua = nse_user_agent();
        assert!(
            ua.contains("Mozilla"),
            "the niftyindices UA must remain a browser string, got {ua:?}"
        );
    }

    #[test]
    fn test_sanitize_transport_error_removes_the_request_url() {
        let url = "https://images.dhan.co/api-data/api-scrip-master-detailed.csv";
        let msg = format!("error sending request for url ({url}): connection closed");
        let out = sanitize_transport_error(&msg, url);
        assert!(
            !out.contains("dhan.co"),
            "the request URL must not survive into a log line: {out}"
        );
    }

    #[test]
    fn test_sanitize_transport_error_removes_proxy_credentials() {
        // THE case this exists for. reqwest embeds the proxy URL from
        // HTTPS_PROXY in its error Display, and that URL can carry
        // Basic-Auth userinfo. Not logging the URL ourselves is not enough —
        // it leaks through the error text, the one path nobody inspects.
        let msg = "builder error: proxy https://user:sup3rsecret@proxy.internal:3128 rejected";
        let out = sanitize_transport_error(msg, "https://example.invalid/x.csv");
        assert!(
            !out.contains("sup3rsecret"),
            "proxy credentials leaked: {out}"
        );
        assert!(!out.contains("proxy.internal"), "proxy host leaked: {out}");
    }

    #[test]
    fn test_sanitize_transport_error_terminates_on_every_input() {
        // The scrubber loops while it finds "://" and must always shrink or
        // advance — a replacement that reintroduced "://" would spin forever
        // on the boot path. Fed inputs designed to trip that.
        for msg in [
            "https://a://b://c broken",
            "://",
            "no urls here at all",
            "https://x https://y https://z",
            "(https://a.b/c) and \"https://d.e/f\", plus https://g.h/i",
        ] {
            let out = sanitize_transport_error(msg, "https://unrelated.invalid/");
            assert!(
                !out.contains("://"),
                "scrubber left a URL behind in {msg:?} -> {out:?}"
            );
        }
    }

    #[test]
    fn test_download_error_display_never_carries_a_body() {
        // Error text reaches the log sink. A status error must carry the
        // status and nothing else: on a failure the body is WAF- or
        // attacker-controlled and there is nothing to learn from it.
        let e = DownloadError::Status(403);
        let s = e.to_string();
        assert!(s.contains("403"));
        assert!(
            !s.contains("http"),
            "no URL fragment may appear in the rendered error: {s}"
        );
        let e = DownloadError::TooLarge {
            cap: MAX_CSV_BODY_BYTES,
        };
        assert!(e.to_string().contains(&MAX_CSV_BODY_BYTES.to_string()));
    }
}
